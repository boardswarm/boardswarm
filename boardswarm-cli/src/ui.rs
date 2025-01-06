use anyhow::anyhow;
use bytes::Bytes;
use futures::{pin_mut, FutureExt, StreamExt};
use ratatui::{
    backend::CrosstermBackend,
    crossterm::{
        event::{
            DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent,
            KeyEventKind, KeyModifiers, MouseEventKind,
        },
        execute, ExecutableCommand,
    },
    layout::Rect,
    widgets::{Block, Borders},
    DefaultTerminal, Terminal as TuiTerminal,
};
use std::io::stdout;

use boardswarm_client::device::{Device, DeviceConsole};

use crate::ui_term;

struct Terminal {
    parser: vt100::Parser,
    tui: TuiTerminal<CrosstermBackend<std::io::Stdout>>,
}

impl Terminal {
    async fn new(
        width: u16,
        height: u16,
        tui: TuiTerminal<CrosstermBackend<std::io::Stdout>>,
    ) -> Self {
        let parser = vt100::Parser::new(height, width, height as usize * 128);
        let mut this = Self { parser, tui };
        this.update().await;
        this
    }

    fn scroll_up(&mut self) {
        let offset = self.parser.screen().scrollback();
        self.parser.set_scrollback(offset + 1)
    }

    fn scroll_down(&mut self) {
        let offset = self.parser.screen().scrollback();
        if offset > 0 {
            self.parser.set_scrollback(offset - 1)
        }
    }

    fn scroll_reset(&mut self) {
        self.parser.set_scrollback(0)
    }

    fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    async fn update(&mut self) {
        let screen = self.parser.screen();
        let term = ui_term::UiTerm::new(screen);
        self.tui
            .draw(|f| {
                let area = f.area();
                let term_area = Rect::new(0, 0, 80.min(area.width), 24.min(area.height));
                f.render_widget(term, term_area);
                if !screen.hide_cursor() && screen.scrollback() == 0 {
                    let cursor = screen.cursor_position();
                    f.set_cursor_position((cursor.1 + term_area.x, cursor.0 + term_area.y));
                }
            })
            .unwrap();
    }
}

#[derive(Debug, Clone)]
enum Input {
    Up,
    Down,
    ScrollReset,
    UIMessage(String),
}

fn convert_keycode_to_term_code(code: KeyCode, modifiers: KeyModifiers) -> Option<Vec<u8>> {
    let val = match code {
        KeyCode::Esc => vec![0x1B_u8],
        KeyCode::Enter => vec![b'\n'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::Backspace => vec![0x8_u8],
        KeyCode::Insert => b"\x1B[2~".to_vec(),
        KeyCode::Delete => b"\x1B[3~".to_vec(),
        KeyCode::PageUp => b"\x1B[5~".to_vec(),
        KeyCode::PageDown => b"\x1B[6~".to_vec(),
        KeyCode::Home => b"\x1B[1~".to_vec(),
        KeyCode::End => b"\x1B[4~".to_vec(),
        KeyCode::Up => b"\x1B[A".to_vec(),    // CUU
        KeyCode::Down => b"\x1B[B".to_vec(),  // CUD
        KeyCode::Left => b"\x1B[D".to_vec(),  // CUB
        KeyCode::Right => b"\x1B[C".to_vec(), // CUF
        KeyCode::Char('c') if modifiers == KeyModifiers::CONTROL => vec![0x3_u8], // ^C ETX
        KeyCode::Char('d') if modifiers == KeyModifiers::CONTROL => vec![0x4_u8], // ^D EOT
        KeyCode::Char(c) => {
            let mut b = [0; 4];
            let result = c.encode_utf8(&mut b);
            result.to_string().into_bytes()
        }
        _ => return None,
    };
    Some(val)
}

async fn run_ui_internal(
    mut tui_terminal: DefaultTerminal,
    device: Device,
    mut console: DeviceConsole,
) -> anyhow::Result<()> {
    tui_terminal.draw(|f| {
        let area = f.area();
        let block = Block::default().title("Block").borders(Borders::ALL);
        f.render_widget(block, area);
    })?;

    let mut terminal = Terminal::new(80, 24, tui_terminal).await;

    let output = console
        .clone()
        .stream_output()
        .await
        .map_err(|e| anyhow!("Console stream opening failed: '{}'", e.message()))?;

    let (input_tx, input_rx) = tokio::sync::mpsc::channel(16);

    // Handle device console stdout
    let _output_writer = tokio::spawn(async move {
        pin_mut!(output);
        pin_mut!(input_rx);
        loop {
            tokio::select! {
                data = output.next() => {
                    if let Some(data) = &data {
                        terminal.process(data);
                    } else {
                        break
                    }
                }
                Some(input) = input_rx.recv() => {
                    match input {
                        Input::Up => { terminal.scroll_up(); },
                        Input::Down => { terminal.scroll_down(); },
                        Input::ScrollReset => { terminal.scroll_reset(); },
                        Input::UIMessage(msg) => {terminal.process(msg.as_bytes())}
                    }
                }
            }
            terminal.update().await;
        }
    });

    // Handle keyboard and mouse events from local console interface
    let event_listener = tokio::spawn(async move {
        let mut event_stream = EventStream::new();
        let mut saw_escape: bool = false;
        loop {
            let device = device.clone();
            let input_tx = input_tx.clone();
            let event = event_stream.next().fuse();

            tokio::select! {
                maybe_event = event => {
                    match maybe_event {
                        Some(Ok(event)) => match event {
                            Event::Key(KeyEvent {
                                code: key_code,
                                modifiers,
                                kind: KeyEventKind::Press,
                                ..
                            }) => {
                                match key_code {
                                    KeyCode::Char('a') if modifiers == KeyModifiers::CONTROL => {
                                        saw_escape = true;
                                    }

                                    KeyCode::Char('q') if saw_escape => {
                                        // Quit
                                        break;
                                    }
                                    KeyCode::Char('k') if saw_escape => {
                                        let _ = input_tx.send(Input::Up).await;
                                    }
                                    KeyCode::Char('j') if saw_escape => {
                                        let _ = input_tx.send(Input::Down).await;
                                    }
                                    KeyCode::Char('o') if saw_escape => {
                                        // Perform power on action
                                        if let Err(err) = device.change_mode("on").await {
                                            let err_msg: String =
                                                format!("Change mode 'on' action failed: {}\r\n", err.code());
                                                let _ = input_tx.send(Input::UIMessage(err_msg)).await;
                                        }
                                    }
                                    KeyCode::Char('f') if saw_escape => {
                                        // Perform power off action
                                        if let Err(err) = device.change_mode("off").await {
                                            let err_msg: String =
                                                format!("Change mode 'off' action failed: {}\r\n", err.code());
                                                let _ = input_tx.send(Input::UIMessage(err_msg)).await;
                                        }
                                    }
                                    KeyCode::Char('r') if saw_escape => {
                                        // Perform power off action
                                        if let Err(err) = device.change_mode("off").await {
                                            let err_msg: String =
                                                format!("Change mode 'off' action failed: {}\r\n", err.code());
                                                let _ = input_tx.send(Input::UIMessage(err_msg)).await;
                                        }
                                        // Perform power on action
                                        if let Err(err) = device.change_mode("on").await {
                                            let err_msg: String =
                                                format!("Change mode 'on' action failed: {}\r\n", err.code());
                                                let _ = input_tx.send(Input::UIMessage(err_msg)).await;
                                        }
                                    }

                                    _ => {
                                        // Reset escape code
                                        saw_escape = false;

                                        // Handle Enter key for output console
                                        if key_code == KeyCode::Enter {
                                            // Send scroll reset on output console
                                            let _ = input_tx.send(Input::ScrollReset).await;
                                        }

                                        // Convert keycode to an equivalent value (ANSI or VT) for input console
                                        if let Some(val) = convert_keycode_to_term_code(key_code, modifiers) {
                                            let stream = futures::stream::once(async move { Bytes::from(val) } );
                                            let _ = console.stream_input(stream).await;
                                        }
                                    },
                                }
                            }
                            Event::Mouse(mouse_event) => match mouse_event.kind {
                                MouseEventKind::ScrollDown => {
                                    let _ = input_tx.send(Input::Down).await;
                                }

                                MouseEventKind::ScrollUp => {
                                    let _ = input_tx.send(Input::Up).await;
                                }
                                _ =>{},
                            },
                            _ => {},
                        },
                        None => {},
                       Some(Err(_)) => panic!(),
                    }
                }
            };
        }
    });

    match event_listener.await {
        Ok(_) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

pub async fn run_ui(device: Device, console_name: Option<String>) -> anyhow::Result<()> {
    let console = match console_name {
        Some(name) => device.console_by_name(&name),
        None => device.console(),
    }
    .ok_or_else(|| anyhow::anyhow!("Console not available"))?;

    execute!(std::io::stdout(), EnableMouseCapture)?;
    let tui_terminal = ratatui::init();

    let result = run_ui_internal(tui_terminal, device, console).await;
    ratatui::restore();
    stdout().execute(DisableMouseCapture)?;
    result
}
