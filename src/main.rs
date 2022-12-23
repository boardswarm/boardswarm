use std::{os::unix::prelude::AsRawFd, path::PathBuf};

use clap::Parser;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_serial::SerialPortBuilderExt;

const INIT: &[u8] = b"\x1b7\x1b[?47h\x1b[2J\x1b[H\x1b[?25h";
const DEINIT: &[u8] = b"\x1b[?47l\x1b8\x1b[?25h";

struct Terminal {
    parser: vt100::Parser,
    stdout: tokio::io::Stdout,
}

impl Terminal {
    async fn new(width: u16, height: u16, mut stdout: tokio::io::Stdout) -> Self {
        let parser = vt100::Parser::new(height, width, 0);
        stdout.write_all(INIT).await.unwrap();
        stdout.flush().await.unwrap();
        Self { parser, stdout }
    }

    fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    async fn update(&mut self) {
        let screen = self.parser.screen();
        self.stdout
            .write_all(b"\x1b[?25l\x1b[1;1H\x1b[K TEST 1234\n")
            .await
            .unwrap();
        let rows = screen.rows_formatted(0, 80).enumerate();
        for (i, row) in rows {
            self.stdout
                .write_all(format!("\x1b[{};1H\x1b[K\x1b[m", i + 3).as_bytes())
                .await
                .unwrap();
            self.stdout.write_all(&row).await.unwrap();
        }
        let (row, column) = screen.cursor_position();
        self.stdout
            .write_all(format!("\x1b[{};{}H\x1b[?25h", row + 3, column + 1).as_bytes())
            .await
            .unwrap();
        self.stdout.flush().await.unwrap();
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
async fn main() {
    let opt = Opts::parse();

    let serial = tokio_serial::new(&opt.path, opt.rate)
        .open_native_async()
        .unwrap();

    let (mut rx, mut tx) = tokio::io::split(serial);

    let mut stdout = tokio::io::stdout();
    let mut stdin = tokio::io::stdin();

    let stdin_fd = stdin.as_raw_fd();
    let _stdout_fd = stdout.as_raw_fd();

    let stdin_termios = nix::sys::termios::tcgetattr(stdin_fd).unwrap();

    let mut stdin_termios_mod = stdin_termios.clone();
    nix::sys::termios::cfmakeraw(&mut stdin_termios_mod);
    nix::sys::termios::tcsetattr(
        stdin_fd,
        nix::sys::termios::SetArg::TCSANOW,
        &stdin_termios_mod,
    )
    .unwrap();

    let _writer = tokio::spawn(async move {
        let mut buffer = [0; 4096];
        let mut terminal = Terminal::new(80, 24, stdout).await;
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
}
