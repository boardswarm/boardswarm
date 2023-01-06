use std::{os::unix::prelude::AsRawFd, task::Poll};

use boardswarm_protocol::{
    device_client::DeviceClient, device_input_request::TargetOrData, DeviceInputRequest,
    DeviceModeRequest, DeviceTarget,
};
use bytes::Bytes;
use crossterm::{
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen},
};
use tokio_util::sync::ReusableBoxFuture;
use tui::{
    backend::CrosstermBackend,
    layout::Rect,
    widgets::{Block, Borders},
    Terminal as TuiTerminal,
};

use futures::{ready, Stream, StreamExt};
use tokio::io::{AsyncRead, AsyncReadExt};

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
        let term = crate::ui_term::UiTerm::new(screen);
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
}

async fn process_input<R>(mut input: R) -> (Bytes, R)
where
    R: AsyncRead + Unpin,
{
    let mut buffer = [0; 4096];
    let read = input.read(&mut buffer).await.unwrap();
    (Bytes::copy_from_slice(&buffer[0..read]), input)
}

enum Input {
    PowerOn,
    PowerOff,
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

pub async fn run_ui(url: String, device: String, console: Option<String>) -> anyhow::Result<()> {
    let mut client = DeviceClient::connect(url).await?;
    let request = tonic::Request::new(DeviceTarget {
        device: device.clone(),
        console: console.clone(),
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

    let stdin = tokio::io::stdin();
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
            match output.unwrap().data_or_state.unwrap() {
                boardswarm_protocol::console_output::DataOrState::Data(data) => {
                    terminal.process(&data);
                    terminal.update().await;
                }

                _ => (),
            }
        }
    });

    let reader = tokio::spawn(async move {
        let input = InputStream::new(stdin);
        let mut s_client = client.clone();
        let target = DeviceInputRequest {
            target_or_data: Some(TargetOrData::Target(DeviceTarget {
                device: device.clone(),
                console: console.clone(),
            })),
        };
        s_client
            .stream_input(
                futures::stream::once(async { target }).chain(input.filter_map(move |i| {
                    let device = device.clone();
                    let mut client = client.clone();
                    async move {
                        match i {
                            Input::PowerOn => {
                                client
                                    .change_device_mode(DeviceModeRequest {
                                        device,
                                        mode: "on".to_string(),
                                    })
                                    .await
                                    .unwrap();
                                None
                            }
                            Input::PowerOff => {
                                client
                                    .change_device_mode(DeviceModeRequest {
                                        device,
                                        mode: "off".to_string(),
                                    })
                                    .await
                                    .unwrap();
                                None
                            }
                            Input::Bytes(data) => Some(DeviceInputRequest {
                                target_or_data: Some(TargetOrData::Data(data)),
                            }),
                        }
                    }
                })),
            )
            .await
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
