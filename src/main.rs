use std::{os::unix::prelude::AsRawFd, thread};

use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use tui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    text::{Span, Spans, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
    Terminal as TuiTerminal,
};

use clap::Parser;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_serial::SerialPortBuilderExt;

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
                let outer = Rect::new(0, 0, 82, 26);
                let inner = block.inner(outer);
                f.render_widget(block, outer);
                f.render_widget(term, inner);
                if !screen.hide_cursor() {
                    let cursor = screen.cursor_position();
                    if cursor.1 < 80 && cursor.0 < 24 {
                        f.set_cursor(cursor.1 + inner.x, cursor.0 + inner.y);
                    }
                }

                let chunks = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([
                        Constraint::Length(80),
                        Constraint::Min(10),
                        Constraint::Min(10),
                    ])
                    .split(inner);

                let text: Text = chunks
                    .iter()
                    .map(|c| Spans::from(format!("-> {:?} - \n\n b", c)))
                    .collect::<Vec<_>>()
                    .into();

                let cursor = screen.cursor_position();
                let p = Paragraph::new(text).wrap(Wrap { trim: false });
                f.render_widget(p, Rect::new(0, 27, size.width, size.height - 27));
            })
            .unwrap();
    }

    fn inner(self) -> TuiTerminal<CrosstermBackend<std::io::Stdout>> {
        self.tui
    }
}

struct InputMonitor {
    saw_escape: bool,
}

impl InputMonitor {
    fn new() -> Self {
        Self { saw_escape: false }
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

async fn input<R, W>(mut input: R, mut output: W)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buffer = [0; 4096];
    let mut monitor = InputMonitor::new();
    loop {
        let read = input.read(&mut buffer).await.unwrap();
        if monitor.check_input(&buffer[0..read]) {
            return;
        }
        output.write_all(&buffer[0..read]).await.unwrap();
        output.flush().await.unwrap();
    }
}

#[derive(clap::Parser)]
struct Opts {
    path: String,
    rate: u32,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let opt = Opts::parse();

    enable_raw_mode()?;

    let serial = tokio_serial::new(&opt.path, opt.rate)
        .open_native_async()
        .unwrap();

    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = TuiTerminal::new(backend).unwrap();

    terminal.draw(|f| {
        let size = f.size();
        let block = Block::default().title("Block").borders(Borders::ALL);
        f.render_widget(block, size);
    })?;

    let (mut rx, mut tx) = tokio::io::split(serial);
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
        let mut buffer = [0; 4096];
        loop {
            let read = rx.read(&mut buffer).await.unwrap();
            terminal.process(&buffer[0..read]);
            terminal.update().await;
        }
    });

    let reader = tokio::spawn(input(stdin, tx));

    reader.await.unwrap();
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

    Ok(())
}
