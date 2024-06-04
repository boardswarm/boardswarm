use std::{os::fd::AsFd, task::Poll};

use bytes::Bytes;
use crossterm::{
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen},
};
use futures::{pin_mut, ready, Stream, StreamExt, TryStreamExt};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Layout, Rect},
    prelude::{Constraint, Direction},
    style::{Color, Style},
    widgets::{Block, Borders, Paragraph},
    Terminal as TuiTerminal,
};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio_util::sync::ReusableBoxFuture;

use crate::ui_term;

const COLUMNS: u16 = 80u16;
const ROWS: u16 = 24u16;

#[derive(Clone)]
pub enum TerminalMode {
    NonFunctioning(String),
    Functioning(String),
    Alternate(String),
}

struct Terminal {
    parser: vt100::Parser,
    tui: TuiTerminal<CrosstermBackend<std::io::Stdout>>,
    terminal_mode: Option<TerminalMode>,
}

impl Terminal {
    async fn new(
        width: u16,
        height: u16,
        tui: TuiTerminal<CrosstermBackend<std::io::Stdout>>,
    ) -> Self {
        let parser = vt100::Parser::new(height, width, height as usize * 128);
        let mut this = Self {
            parser,
            tui,
            terminal_mode: None,
        };
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

    fn set_functioning(&mut self, str: String) {
        self.terminal_mode = Some(TerminalMode::Functioning(str));
    }

    fn set_non_functioning(&mut self, str: String) {
        self.terminal_mode = Some(TerminalMode::NonFunctioning(str));
    }

    fn set_alternate(&mut self, str: String) {
        self.terminal_mode = Some(TerminalMode::Alternate(str));
    }

    fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    async fn update(&mut self) {
        let screen = self.parser.screen();
        let term = ui_term::UiTerm::new(screen);
        let terminal_mode = self.terminal_mode.clone();
        self.tui
            .draw(|f| {
                let size = f.size();
                let term_area = Rect::new(0, 0, COLUMNS.min(size.width), ROWS.min(size.height));
                let area_chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Min(1), Constraint::Length(1)])
                    .split(term_area);

                f.render_widget(term, area_chunks[0]);
                if !screen.hide_cursor() && screen.scrollback() == 0 {
                    let cursor = screen.cursor_position();
                    f.set_cursor(cursor.1 + area_chunks[0].x, cursor.0 + area_chunks[0].y);
                }

                let (style, text) = match terminal_mode {
                    Some(terminal_mode) => match terminal_mode {
                        TerminalMode::NonFunctioning(s) => {
                            (Style::reset().fg(Color::Black).bg(Color::LightRed), s)
                        }
                        TerminalMode::Functioning(s) => {
                            (Style::reset().fg(Color::Black).bg(Color::LightGreen), s)
                        }
                        TerminalMode::Alternate(s) => {
                            (Style::reset().fg(Color::Black).bg(Color::LightMagenta), s)
                        }
                    },
                    None => (Style::reset(), "UNKNOWN".to_string()),
                };
                let mode_block = Block::default().style(style);
                let mode = Paragraph::new(text).block(mode_block);

                let status_chunks = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Ratio(1, 8), Constraint::Ratio(7, 8)])
                    .split(area_chunks[1]);

                f.render_widget(mode, status_chunks[0]);
            })
            .unwrap();
    }
}

async fn process_input<R>(mut input: R) -> (Bytes, R)
where
    R: AsyncRead + Unpin,
{
    let mut buffer = [0; 4096];
    let read = input.read(&mut buffer).await.unwrap();
    (Bytes::copy_from_slice(&buffer[0..read]), input)
}

#[derive(Debug, Clone)]
enum Input {
    PowerOn,
    PowerOff,
    PowerReset,
    Up,
    Down,
    ScrollReset,
    Bytes(Bytes),
}

struct InputStream<R> {
    saw_escape: bool,
    future: tokio_util::sync::ReusableBoxFuture<'static, (Bytes, R)>,
}

impl<R> InputStream<R>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    fn new(rx: R) -> Self {
        let future = ReusableBoxFuture::new(process_input(rx));
        Self {
            saw_escape: false,
            future,
        }
    }

    fn check_input(&mut self, input: Bytes) -> Option<Input> {
        for i in &input {
            match i {
                0x1 => self.saw_escape = true, /* ^a */
                b'q' if self.saw_escape => return None,
                b'o' if self.saw_escape => return Some(Input::PowerOn),
                b'f' if self.saw_escape => return Some(Input::PowerOff),
                b'r' if self.saw_escape => return Some(Input::PowerReset),
                b'k' if self.saw_escape => return Some(Input::Up),
                b'j' if self.saw_escape => return Some(Input::Down),
                // FIXME enter doesn't work
                b'\n' if self.saw_escape => return Some(Input::ScrollReset),
                b'0' if self.saw_escape => return Some(Input::ScrollReset),
                _ => self.saw_escape = false,
            }
        }
        Some(Input::Bytes(input))
    }
}

impl<R> Stream for InputStream<R>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    type Item = Input;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let (data, rx) = ready!(self.future.poll(cx));
        self.future.set(process_input(rx));

        Poll::Ready(self.check_input(data))
    }
}

pub async fn run_ui(
    device: boardswarm_client::device::Device,
    console: Option<String>,
) -> anyhow::Result<()> {
    enable_raw_mode()?;

    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = TuiTerminal::new(backend).unwrap();

    terminal.draw(|f| {
        let size = f.size();
        let block = Block::default().title("Block").borders(Borders::ALL);
        f.render_widget(block, size);
    })?;

    let stdin = tokio::io::stdin();
    let stdin_fd = stdin
        .as_fd()
        .try_clone_to_owned()
        .expect("Couldn't clone stdin fd");
    let stdin_termios = nix::sys::termios::tcgetattr(&stdin).unwrap();

    let mut stdin_termios_mod = stdin_termios.clone();
    nix::sys::termios::cfmakeraw(&mut stdin_termios_mod);
    nix::sys::termios::tcsetattr(
        &stdin,
        nix::sys::termios::SetArg::TCSANOW,
        &stdin_termios_mod,
    )
    .unwrap();

    /* 1 row for status line at the bottom */
    let mut terminal = Terminal::new(COLUMNS, ROWS - 1, terminal).await;
    let mut console = match console {
        Some(console) => device.console_by_name(&console),
        None => device.console(),
    }
    .ok_or_else(|| anyhow::anyhow!("Console not available"))?;

    let mut output_console = console.clone();
    let output = output_console.stream_output().await?;

    let (input_tx, input_rx) = tokio::sync::mpsc::channel(16);
    let (state_tx, state_rx) = tokio::sync::mpsc::channel(16);
    let _writer = tokio::spawn(async move {
        pin_mut!(output);
        pin_mut!(input_rx);
        pin_mut!(state_rx);
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
                        _ => (),
                    }
                }
                Some(new_state) = state_rx.recv() => {
                    let state: String = new_state;
                    match state.as_str() {
                        "on" => terminal.set_functioning("ON".to_string()),
                        "off" => terminal.set_non_functioning("OFF".to_string()),
                        other => terminal.set_alternate(other.to_uppercase()),
                    }
                }
            }
            terminal.update().await;
        }
    });

    let dev_clone = device.clone();
    let reader = tokio::spawn(async move {
        let input = InputStream::new(stdin);
        console
            .stream_input(input.filter_map(move |i| {
                let device = device.clone();
                let input_tx = input_tx.clone();
                async move {
                    match i {
                        Input::PowerOn => {
                            device.change_mode("on").await.unwrap();
                            None
                        }
                        Input::PowerOff => {
                            device.change_mode("off").await.unwrap();
                            None
                        }
                        Input::PowerReset => {
                            device.change_mode("off").await.unwrap();
                            device.change_mode("on").await.unwrap();
                            None
                        }
                        Input::Up | Input::Down | Input::ScrollReset => {
                            input_tx.send(i).await.unwrap();
                            None
                        }
                        Input::Bytes(data) => Some(data),
                    }
                }
            }))
            .await
    });

    let _device_state_watcher = tokio::spawn(async move {
        let mut d = dev_clone.device_info().await.unwrap();
        let mut current_mode = None;

        while let Some(device) = d.try_next().await.unwrap() {
            if device.current_mode != current_mode {
                current_mode = device.current_mode;
                state_tx.send(current_mode.clone().unwrap()).await.unwrap();
            }
        }
    });

    let r = reader.await;
    nix::sys::termios::tcsetattr(
        stdin_fd,
        nix::sys::termios::SetArg::TCSAFLUSH,
        &stdin_termios,
    )
    .unwrap();

    // restore terminal
    disable_raw_mode()?;
    //let mut terminal = terminal.inner();
    //execute!(terminal.backend_mut(), LeaveAlternateScreen,)?;
    //terminal.show_cursor()?;
    //
    match r {
        Ok(_) => Ok(()),
        //Ok(Ok(_)) => Ok(()),
        //Ok(Err(e)) => Err(e.into()),
        Err(e) => Err(e.into()),
    }
}
