use std::task::Poll;

use anyhow::anyhow;
use bytes::Bytes;
use futures::{pin_mut, ready, Stream, StreamExt};
use ratatui::{
    backend::CrosstermBackend,
    layout::Rect,
    widgets::{Block, Borders},
    DefaultTerminal, Terminal as TuiTerminal,
};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio_util::sync::ReusableBoxFuture;

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
    UIMessage(String),
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

    let stdin = tokio::io::stdin();
    let mut stdin_termios = nix::sys::termios::tcgetattr(&stdin)?;

    nix::sys::termios::cfmakeraw(&mut stdin_termios);
    nix::sys::termios::tcsetattr(&stdin, nix::sys::termios::SetArg::TCSANOW, &stdin_termios)?;

    let mut terminal = Terminal::new(80, 24, tui_terminal).await;

    let output = console
        .clone()
        .stream_output()
        .await
        .map_err(|e| anyhow!("Console stream opening failed: '{}'", e.message()))?;

    let (input_tx, input_rx) = tokio::sync::mpsc::channel(16);
    let _writer = tokio::spawn(async move {
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
                        _ => (),
                    }
                }
            }
            terminal.update().await;
        }
    });

    let reader = tokio::spawn(async move {
        let input = InputStream::new(stdin);
        console
            .stream_input(input.filter_map(move |i| {
                let device = device.clone();
                let input_tx = input_tx.clone();
                async move {
                    match i {
                        Input::PowerOn => {
                            // Perform power on action
                            if let Err(err) = device.change_mode("on").await {
                                let err_msg: String =
                                    format!("Change mode 'on' action failed: {}\r\n", err.code());
                                input_tx.send(Input::UIMessage(err_msg)).await.unwrap();
                            }
                            None
                        }
                        Input::PowerOff => {
                            // Perform power off action
                            if let Err(err) = device.change_mode("off").await {
                                let err_msg: String =
                                    format!("Change mode 'off' action failed: {}\r\n", err.code());
                                let _ = input_tx.send(Input::UIMessage(err_msg)).await;
                            };
                            None
                        }
                        Input::PowerReset => {
                            // Perform power off action
                            if let Err(err) = device.change_mode("off").await {
                                let err_msg: String =
                                    format!("Change mode 'off' action failed: {}\r\n", err.code());
                                let _ = input_tx.send(Input::UIMessage(err_msg)).await;
                            };
                            // Perform power on action
                            if let Err(err) = device.change_mode("on").await {
                                let err_msg: String =
                                    format!("Change mode 'on' action failed: {}\r\n", err.code());
                                let _ = input_tx.send(Input::UIMessage(err_msg)).await;
                            }
                            None
                        }
                        Input::Up | Input::Down | Input::ScrollReset => {
                            let _ = input_tx.send(i).await;
                            None
                        }
                        Input::Bytes(data) => Some(data),
                        Input::UIMessage(_) => None,
                    }
                }
            }))
            .await
    });

    match reader.await {
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

    let result = run_ui_internal(ratatui::init(), device, console).await;
    ratatui::restore();
    result
}
