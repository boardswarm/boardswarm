use std::task::Poll;

use boardswarm_client::device::Device;
use bytes::Bytes;
use futures::{pin_mut, ready, Stream, StreamExt};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint::Length, Layout, Rect},
    widgets::{Block, Borders},
    Terminal as TuiTerminal,
};
use tokio::{io::{AsyncRead, AsyncReadExt}, sync::mpsc::Sender};
use tokio_util::sync::ReusableBoxFuture;

use crate::ui_status::{UiStatus, UiStatusMode};
use crate::ui_term;

struct Terminal {
    parser: vt100::Parser,
    tui: TuiTerminal<CrosstermBackend<std::io::Stdout>>,
    mode: UiStatusMode,
}

impl Terminal {
    async fn new(
        width: u16,
        height: u16,
        tui: TuiTerminal<CrosstermBackend<std::io::Stdout>>,
    ) -> Self {
        let parser = vt100::Parser::new(height, width, height as usize * 128);
        let mode = UiStatusMode::Normal;
        let mut this = Self { parser, tui, mode };
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

    fn status_set(&mut self, mode: UiStatusMode) {
        self.mode = mode;
    }

    fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    async fn update(&mut self) {
        let screen = self.parser.screen();
        let term = ui_term::UiTerm::new(screen);
        let status = UiStatus::new(self.mode.clone());

        self.tui
            .draw(|f| {
                // Terminal area is currently fixed at 80x24.
                // Add an extra row at the bottom for the status bar.
                let area = Rect::new(0, 0, 80, 25).clamp(f.area());
                let [term_area, status_area] =
                    Layout::vertical([Length(24), Length(1)]).areas::<2>(area);

                f.render_widget(term, term_area);
                f.render_widget(status, status_area);

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
    Error(String),
    ClearStatus,
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
    let mut terminal = ratatui::init();

    terminal.draw(|f| {
        let area = f.area();
        let block = Block::default().title("Block").borders(Borders::ALL);
        f.render_widget(block, area);
    })?;

    let stdin = tokio::io::stdin();
    let stdin_termios = nix::sys::termios::tcgetattr(&stdin).unwrap();

    let mut stdin_termios_mod = stdin_termios.clone();
    nix::sys::termios::cfmakeraw(&mut stdin_termios_mod);
    nix::sys::termios::tcsetattr(
        &stdin,
        nix::sys::termios::SetArg::TCSANOW,
        &stdin_termios_mod,
    )
    .unwrap();

    let mut terminal = Terminal::new(80, 24, terminal).await;
    let mut console = match console {
        Some(console) => device.console_by_name(&console),
        None => device.console(),
    }
    .ok_or_else(|| anyhow::anyhow!("Console not available"))?;

    let mut output_console = console.clone();
    let output = output_console.stream_output().await?;

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
                        Input::Error(msg) => { terminal.status_set(UiStatusMode::Error(msg)) },
                        Input::ClearStatus => { terminal.status_set(UiStatusMode::Normal) },
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
                    async fn do_change_mode(device: &Device, channel: &Sender<Input>, mode: &str) {
                        if let Err(err) = device.change_mode(mode).await {
                            let msg = err.code().to_string();
                            channel.send(Input::Error(msg)).await.unwrap();
                        }
                    }
                    match i {
                        Input::PowerOn => {
                            do_change_mode(&device, &input_tx, "on").await;
                            None
                        }
                        Input::PowerOff => {
                            do_change_mode(&device, &input_tx, "off").await;
                            None
                        }
                        Input::PowerReset => {
                            do_change_mode(&device, &input_tx, "off").await;
                            do_change_mode(&device, &input_tx, "on").await;
                            None
                        }
                        Input::Bytes(data) => {
                            input_tx.send(Input::ClearStatus).await.unwrap();
                            Some(data)
                        }
                        _ => {
                            input_tx.send(i).await.unwrap();
                            None
                        }
                    }
                }
            }))
            .await
    });

    let r = reader.await;

    ratatui::restore();
    match r {
        Ok(_) => Ok(()),
        //Ok(Ok(_)) => Ok(()),
        //Ok(Err(e)) => Err(e.into()),
        Err(e) => Err(e.into()),
    }
}
