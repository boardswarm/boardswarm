use std::{os::unix::prelude::AsRawFd, task::Poll, thread};

use bytes::Bytes;
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use protocol::protocol::{serial_client::SerialClient, InputRequest, OutputRequest};
use tokio_util::sync::ReusableBoxFuture;
use tui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    text::{Span, Spans, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
    Terminal as TuiTerminal,
};

use clap::Parser;
use futures::{ready, Stream, StreamExt};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

mod ui_term;

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
        let parser = vt100::Parser::new(height, width, 0);
        let mut this = Self { parser, tui };
        this.update().await;
        this
    }

    fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    async fn update(&mut self) {
        let screen = self.parser.screen();
        let term = ui_term::UiTerm::new(&screen);
        self.tui
            .draw(|f| {
                let size = f.size();
                let block = Block::default().title(screen.title()).borders(Borders::ALL);

                let outer = Rect::new(0, 0, 82.min(size.width), 26.min(size.height));
                let inner = block.inner(outer);
                f.render_widget(block, outer);
                f.render_widget(term, inner);
                if !screen.hide_cursor() {
                    let cursor = screen.cursor_position();
                    f.set_cursor(cursor.1 + inner.x, cursor.0 + inner.y);
                }
            })
            .unwrap();
    }

    fn inner(self) -> TuiTerminal<CrosstermBackend<std::io::Stdout>> {
        self.tui
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

struct InputStream<R> {
    name: Option<String>,
    saw_escape: bool,
    future: tokio_util::sync::ReusableBoxFuture<'static, (Bytes, R)>,
}

impl<R> InputStream<R>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    fn new(name: String, rx: R) -> Self {
        let future = ReusableBoxFuture::new(process_input(rx));
        Self {
            name: Some(name),
            saw_escape: false,
            future,
        }
    }

    fn check_input(&mut self, input: &[u8]) -> bool {
        for i in input {
            match i {
                0x1 => self.saw_escape = true, /* ^a */
                b'q' if self.saw_escape => return true,
                _ => self.saw_escape = false,
            }
        }
        false
    }
}

impl<R> Stream for InputStream<R>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    type Item = InputRequest;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let (data, rx) = ready!(self.future.poll(cx));
        self.future.set(process_input(rx));

        if self.check_input(&data) {
            Poll::Ready(None)
        } else {
            Poll::Ready(Some(InputRequest {
                name: self.name.take(),
                data,
            }))
        }
    }
}

#[derive(clap::Parser)]
struct Opts {
    name: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let opt = Opts::parse();

    let mut client = SerialClient::connect("http://[::1]:50051").await?;
    let request = tonic::Request::new(OutputRequest {
        name: opt.name.clone(),
    });
    let mut response = client.stream_output(request).await?;

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

    let mut stdin = tokio::io::stdin();
    let stdin_fd = stdin.as_raw_fd();
    let stdin_termios = nix::sys::termios::tcgetattr(stdin_fd).unwrap();

    let mut stdin_termios_mod = stdin_termios.clone();
    nix::sys::termios::cfmakeraw(&mut stdin_termios_mod);
    nix::sys::termios::tcsetattr(
        stdin_fd,
        nix::sys::termios::SetArg::TCSANOW,
        &stdin_termios_mod,
    )
    .unwrap();

    let mut terminal = Terminal::new(80, 24, terminal).await;
    let _writer = tokio::spawn(async move {
        while let Some(output) = response.get_mut().next().await {
            let data = output.unwrap().data;
            terminal.process(&data);
            terminal.update().await;
        }
    });

    let reader = tokio::spawn(async move {
        let input = InputStream::new(opt.name, stdin);
        client.stream_input(input).await
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
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(e.into()),
        Err(e) => Err(e.into()),
    }
}
