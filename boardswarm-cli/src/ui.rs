use std::task::Poll;
use std::{num::ParseIntError, str::FromStr};

use bytes::Bytes;
use futures::{pin_mut, ready, Stream, StreamExt};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Rect, Size},
    widgets::{Block, Borders},
    Terminal as TuiTerminal,
};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio_util::sync::ReusableBoxFuture;
use tracing::warn;

use crate::ui_term;

struct Terminal {
    parser: vt100::Parser,
    tui: TuiTerminal<CrosstermBackend<std::io::Stdout>>,
    size_setting: TerminalSizeSetting,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TerminalSizeSetting {
    Fixed(Size),
    Auto,
}

#[derive(Debug, PartialEq, Error)]
pub enum TerminalSizeSettingError {
    #[error("unable to parse terminal setting format")]
    ParseError,
    #[error("invalid size, width and height should greater than 0")]
    InvalidSizeError,
}

impl From<ParseIntError> for TerminalSizeSettingError {
    fn from(_: ParseIntError) -> Self {
        TerminalSizeSettingError::ParseError
    }
}

impl FromStr for TerminalSizeSetting {
    type Err = TerminalSizeSettingError;

    fn from_str(fmt: &str) -> Result<Self, Self::Err> {
        if fmt == "auto" {
            return Ok(TerminalSizeSetting::Auto);
        }

        // Try to parse terminal size format to get width and height
        let (w, h) = fmt
            .split_once('x')
            .ok_or(TerminalSizeSettingError::ParseError)?;

        // Try to convert the tuple values into u16
        let width = w.parse::<u16>()?;
        let height = h.parse::<u16>()?;

        if width == 0 || height == 0 {
            return Err(TerminalSizeSettingError::InvalidSizeError);
        }

        Ok(TerminalSizeSetting::Fixed(Size { width, height }))
    }
}

impl Terminal {
    async fn new(
        size_setting: TerminalSizeSetting,
        scrollback_lines: usize,
        tui: TuiTerminal<CrosstermBackend<std::io::Stdout>>,
    ) -> Self {
        let size = match size_setting {
            TerminalSizeSetting::Fixed(s) => s,
            TerminalSizeSetting::Auto => match tui.size() {
                Ok(s) => s,
                Err(_) => {
                    warn!("Unable to retrieve terminal size, use default value ('80x24')");
                    Size {
                        width: 80,
                        height: 24,
                    }
                }
            },
        };

        let parser = vt100::Parser::new(size.height, size.width, scrollback_lines);
        let mut this = Self {
            parser,
            tui,
            size_setting,
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

    fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    async fn update(&mut self) {
        let term_size = match self.size_setting {
            TerminalSizeSetting::Fixed(s) => s,
            TerminalSizeSetting::Auto => {
                // Try to auto resize terminal and parser
                if self.tui.autoresize().is_ok() {
                    match self.tui.size() {
                        Ok(s) => s,
                        Err(_) => Size {
                            width: 80,
                            height: 24,
                        },
                    }
                } else {
                    // Fallback
                    Size {
                        width: 80,
                        height: 24,
                    }
                }
            }
        };
        self.parser.set_size(term_size.height, term_size.width);

        let screen = self.parser.screen();
        let term = ui_term::UiTerm::new(screen);
        self.tui
            .draw(|f| {
                let area = f.area();
                let term_area = Rect::new(
                    0,
                    0,
                    term_size.width.min(area.width),
                    term_size.height.min(area.height),
                );
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
    terminal_size_setting: TerminalSizeSetting,
    scrollback_lines: usize,
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

    let mut terminal = Terminal::new(terminal_size_setting, scrollback_lines, terminal).await;
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

    let r = reader.await;

    ratatui::restore();
    match r {
        Ok(_) => Ok(()),
        //Ok(Ok(_)) => Ok(()),
        //Ok(Err(e)) => Err(e.into()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn terminal_size_setting() {
        // Nominal

        // auto string
        let size = TerminalSizeSetting::from_str("auto");
        assert_eq!(size, Ok(TerminalSizeSetting::Auto));

        // Fixed string
        let size = TerminalSizeSetting::from_str("166x58");
        assert_eq!(
            size,
            Ok(TerminalSizeSetting::Fixed(Size {
                width: 166,
                height: 58
            }))
        );

        // Errors

        // Invalid string
        let size = TerminalSizeSetting::from_str("random");
        assert_eq!(size, Err(TerminalSizeSettingError::ParseError));

        // Only a width
        let size = TerminalSizeSetting::from_str("80");
        assert_eq!(size, Err(TerminalSizeSettingError::ParseError));

        // No height
        let size = TerminalSizeSetting::from_str("80x");
        assert_eq!(size, Err(TerminalSizeSettingError::ParseError));

        // Invalid width format
        let size = TerminalSizeSetting::from_str("80xA");
        assert_eq!(size, Err(TerminalSizeSettingError::ParseError));

        // Invalid height format
        let size = TerminalSizeSetting::from_str("Ax24");
        assert_eq!(size, Err(TerminalSizeSettingError::ParseError));

        // Null width value
        let size = TerminalSizeSetting::from_str("0x24");
        assert_eq!(size, Err(TerminalSizeSettingError::InvalidSizeError));

        // Null height value
        let size = TerminalSizeSetting::from_str("80x0");
        assert_eq!(size, Err(TerminalSizeSettingError::InvalidSizeError));
    }
}
