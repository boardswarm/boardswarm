use std::os::unix::prelude::AsRawFd;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_serial::SerialPortBuilderExt;

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

#[tokio::main]
async fn main() {
    let serial = tokio_serial::new("/dev/ttyUSB1", 1_500_000)
        .open_native_async()
        .unwrap();

    let (mut rx, mut tx) = tokio::io::split(serial);

    let mut stdout = tokio::io::stdout();
    let mut stdin = tokio::io::stdin();

    let stdin_fd = stdin.as_raw_fd();
    let _stdout_fd = stdout.as_raw_fd();

    let stdin_termios = nix::sys::termios::tcgetattr(stdin_fd).unwrap();

    let mut stdin_termios_mod = stdin_termios.clone();
    stdin_termios_mod
        .local_flags
        .remove(nix::sys::termios::LocalFlags::ECHO);
    stdin_termios_mod
        .local_flags
        .remove(nix::sys::termios::LocalFlags::ICANON);
    stdin_termios_mod
        .local_flags
        .remove(nix::sys::termios::LocalFlags::ISIG);

    stdin_termios_mod
        .input_flags
        .remove(nix::sys::termios::InputFlags::IXON);
    stdin_termios_mod
        .input_flags
        .remove(nix::sys::termios::InputFlags::ICRNL);

    stdin_termios_mod
        .output_flags
        .remove(nix::sys::termios::OutputFlags::OPOST);

    nix::sys::termios::tcsetattr(
        stdin_fd,
        nix::sys::termios::SetArg::TCSAFLUSH,
        &stdin_termios_mod,
    )
    .unwrap();

    let _writer = tokio::spawn(async move {
        let mut buffer = [0; 4096];
        loop {
            let read = rx.read(&mut buffer).await.unwrap();
            stdout.write_all(&buffer[0..read]).await.unwrap();
            stdout.flush().await.unwrap();
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
