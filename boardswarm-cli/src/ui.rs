use std::{num::ParseIntError, pin::Pin, str::FromStr};

use boardswarm_client::client::Boardswarm;
use boardswarm_protocol::ItemType;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use ratatui::{
    crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::{Rect, Size},
    style::Style,
    widgets::{Block, Borders, List, ListItem, ListState},
};
use thiserror::Error;

use crate::ui_term;

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

struct DeviceSelector {
    items: Vec<boardswarm_protocol::Item>,
    list_state: ListState,
}

enum SelectAction {
    Select(u64),
    Quit,
}

impl DeviceSelector {
    fn new(items: Vec<boardswarm_protocol::Item>) -> Self {
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        Self { items, list_state }
    }

    fn render(&mut self, frame: &mut ratatui::Frame) {
        let area = frame.area();
        let list_items: Vec<ListItem> = self
            .items
            .iter()
            .map(|i| ListItem::new(i.name.clone()))
            .collect();
        let list = List::new(list_items)
            .block(
                Block::default()
                    .title(" Select a device (↑/↓ or j/k to navigate, Enter to select, q to quit) ")
                    .borders(Borders::ALL),
            )
            .highlight_style(Style::new().reversed())
            .highlight_symbol("> ");
        frame.render_stateful_widget(list, area, &mut self.list_state);
    }

    fn handle_key(&mut self, key: KeyEvent) -> Option<SelectAction> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                let i = self.list_state.selected().unwrap_or(0);
                self.list_state
                    .select(Some((i + 1).min(self.items.len() - 1)));
                None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let i = self.list_state.selected().unwrap_or(0);
                self.list_state.select(Some(i.saturating_sub(1)));
                None
            }
            KeyCode::Enter => self
                .list_state
                .selected()
                .map(|i| SelectAction::Select(self.items[i].id)),
            KeyCode::Char('q') | KeyCode::Esc => Some(SelectAction::Quit),
            _ => None,
        }
    }
}

struct RunningState {
    parser: vt100::Parser,
    size_setting: TerminalSizeSetting,
    output: Pin<Box<dyn Stream<Item = Bytes> + Send>>,
    input_tx: futures::channel::mpsc::Sender<Bytes>,
    device: boardswarm_client::device::Device,
    saw_escape: bool,
}

enum RunAction {
    Exit,
}

impl RunningState {
    async fn new(
        device: boardswarm_client::device::Device,
        console_name: Option<&str>,
        size_setting: TerminalSizeSetting,
        scrollback_lines: usize,
        tui_size: Size,
    ) -> anyhow::Result<Self> {
        let size = match size_setting {
            TerminalSizeSetting::Fixed(s) => s,
            TerminalSizeSetting::Auto => tui_size,
        };

        let mut console = match console_name {
            Some(name) => device.console_by_name(name),
            None => device.console(),
        }
        .ok_or_else(|| anyhow::anyhow!("Console not available"))?;

        let output = console.stream_output().await?;

        let (input_tx, input_rx) = futures::channel::mpsc::channel::<Bytes>(16);
        tokio::spawn(async move {
            let _ = console.stream_input(input_rx).await;
        });

        let parser = vt100::Parser::new(size.height, size.width, scrollback_lines);

        Ok(Self {
            parser,
            size_setting,
            output: Box::pin(output),
            input_tx,
            device,
            saw_escape: false,
        })
    }

    fn render(&self, frame: &mut ratatui::Frame) {
        let area = frame.area();
        let term_size = match self.size_setting {
            TerminalSizeSetting::Fixed(s) => s,
            TerminalSizeSetting::Auto => Size {
                width: area.width,
                height: area.height,
            },
        };
        let screen = self.parser.screen();
        let term = ui_term::UiTerm::new(screen);
        let term_area = Rect::new(
            0,
            0,
            term_size.width.min(area.width),
            term_size.height.min(area.height),
        );
        frame.render_widget(term, term_area);
        if !screen.hide_cursor() && screen.scrollback() == 0 {
            let cursor = screen.cursor_position();
            frame.set_cursor_position((cursor.1 + term_area.x, cursor.0 + term_area.y));
        }
    }

    fn process_output(&mut self, data: Bytes) {
        self.parser.process(&data);
    }

    fn handle_resize(&mut self, width: u16, height: u16) {
        if self.size_setting == TerminalSizeSetting::Auto {
            self.parser.screen_mut().set_size(height, width);
        }
    }

    async fn handle_key(&mut self, key: KeyEvent) -> Option<RunAction> {
        if key.kind != KeyEventKind::Press {
            return None;
        }

        if self.saw_escape {
            self.saw_escape = false;
            match key.code {
                KeyCode::Char('q') => return Some(RunAction::Exit),
                KeyCode::Char('o') => {
                    let _ = self.device.change_mode("on").await;
                }
                KeyCode::Char('f') => {
                    let _ = self.device.change_mode("off").await;
                }
                KeyCode::Char('r') => {
                    let _ = self.device.change_mode("off").await;
                    let _ = self.device.change_mode("on").await;
                }
                KeyCode::Char('k') => {
                    let offset = self.parser.screen().scrollback();
                    self.parser.screen_mut().set_scrollback(offset + 1);
                }
                KeyCode::Char('j') => {
                    let offset = self.parser.screen().scrollback();
                    if offset > 0 {
                        self.parser.screen_mut().set_scrollback(offset - 1);
                    }
                }
                KeyCode::Enter | KeyCode::Char('0') => {
                    self.parser.screen_mut().set_scrollback(0);
                }
                _ => {}
            }
            return None;
        }

        if key.code == KeyCode::Char('a') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.saw_escape = true;
            return None;
        }

        if let Some(bytes) = key_to_bytes(key) {
            let _ = self.input_tx.try_send(bytes);
        }
        None
    }
}

fn key_to_bytes(key: KeyEvent) -> Option<Bytes> {
    match key.code {
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                let byte = (c as u8)
                    .to_ascii_lowercase()
                    .wrapping_sub(b'a')
                    .wrapping_add(1);
                return Some(Bytes::from(vec![byte]));
            }
            let mut buf = [0u8; 4];
            let s = c.encode_utf8(&mut buf);
            Some(Bytes::copy_from_slice(s.as_bytes()))
        }
        KeyCode::Enter => Some(Bytes::from_static(b"\r")),
        KeyCode::Backspace => Some(Bytes::from_static(b"\x7f")),
        KeyCode::Tab => Some(Bytes::from_static(b"\t")),
        KeyCode::BackTab => Some(Bytes::from_static(b"\x1b[Z")),
        KeyCode::Esc => Some(Bytes::from_static(b"\x1b")),
        KeyCode::Up => Some(Bytes::from_static(b"\x1b[A")),
        KeyCode::Down => Some(Bytes::from_static(b"\x1b[B")),
        KeyCode::Right => Some(Bytes::from_static(b"\x1b[C")),
        KeyCode::Left => Some(Bytes::from_static(b"\x1b[D")),
        KeyCode::Home => Some(Bytes::from_static(b"\x1b[H")),
        KeyCode::End => Some(Bytes::from_static(b"\x1b[F")),
        KeyCode::Insert => Some(Bytes::from_static(b"\x1b[2~")),
        KeyCode::Delete => Some(Bytes::from_static(b"\x1b[3~")),
        KeyCode::PageUp => Some(Bytes::from_static(b"\x1b[5~")),
        KeyCode::PageDown => Some(Bytes::from_static(b"\x1b[6~")),
        KeyCode::F(1) => Some(Bytes::from_static(b"\x1bOP")),
        KeyCode::F(2) => Some(Bytes::from_static(b"\x1bOQ")),
        KeyCode::F(3) => Some(Bytes::from_static(b"\x1bOR")),
        KeyCode::F(4) => Some(Bytes::from_static(b"\x1bOS")),
        KeyCode::F(5) => Some(Bytes::from_static(b"\x1b[15~")),
        KeyCode::F(6) => Some(Bytes::from_static(b"\x1b[17~")),
        KeyCode::F(7) => Some(Bytes::from_static(b"\x1b[18~")),
        KeyCode::F(8) => Some(Bytes::from_static(b"\x1b[19~")),
        KeyCode::F(9) => Some(Bytes::from_static(b"\x1b[20~")),
        KeyCode::F(10) => Some(Bytes::from_static(b"\x1b[21~")),
        KeyCode::F(11) => Some(Bytes::from_static(b"\x1b[23~")),
        KeyCode::F(12) => Some(Bytes::from_static(b"\x1b[24~")),
        _ => None,
    }
}

enum AppState {
    SelectingDevice(DeviceSelector),
    Running(Box<RunningState>),
}

impl AppState {
    fn render(&mut self, frame: &mut ratatui::Frame) {
        match self {
            AppState::SelectingDevice(s) => s.render(frame),
            AppState::Running(r) => r.render(frame),
        }
    }
}

pub async fn run_ui(
    device: Option<boardswarm_client::device::Device>,
    boardswarm: Boardswarm,
    mut console: Option<boardswarm_client::device::DeviceConsole>,
    terminal_size_setting: TerminalSizeSetting,
    scrollback_lines: usize,
) -> anyhow::Result<()> {
    let mut tui = ratatui::init();
    let result = run_app(
        &mut tui,
        device,
        boardswarm,
        console.as_deref(),
        terminal_size_setting,
        scrollback_lines,
    )
    .await;
    ratatui::restore();
    result
}

async fn run_app(
    tui: &mut ratatui::DefaultTerminal,
    device: Option<boardswarm_client::device::Device>,
    mut boardswarm: Boardswarm,
    console_name: Option<&str>,
    terminal_size_setting: TerminalSizeSetting,
    scrollback_lines: usize,
) -> anyhow::Result<()> {
    let tui_size = tui.size().unwrap_or(Size {
        width: 80,
        height: 24,
    });

    let mut state = if let Some(d) = device {
        AppState::Running(Box::new(
            RunningState::new(
                d,
                console_name,
                terminal_size_setting.clone(),
                scrollback_lines,
                tui_size,
            )
            .await?,
        ))
    } else {
        let items = boardswarm.list(ItemType::Device).await?;
        if items.is_empty() {
            anyhow::bail!("No devices available on the server");
        }
        AppState::SelectingDevice(DeviceSelector::new(items))
    };

    let mut event_stream = EventStream::new();
    tui.draw(|f| state.render(f))?;

    loop {
        let mut next_state: Option<AppState> = None;

        match &mut state {
            AppState::SelectingDevice(selector) => {
                let Some(Ok(event)) = event_stream.next().await else {
                    break;
                };
                if let Event::Key(key) = event {
                    match selector.handle_key(key) {
                        Some(SelectAction::Select(id)) => {
                            let tui_size = tui.size().unwrap_or(Size {
                                width: 80,
                                height: 24,
                            });
                            let device = boardswarm_client::device::DeviceBuilder::from_client(
                                boardswarm.clone(),
                            )
                            .by_id(id)
                            .await?;
                            next_state = Some(AppState::Running(Box::new(
                                RunningState::new(
                                    device,
                                    console_name,
                                    terminal_size_setting.clone(),
                                    scrollback_lines,
                                    tui_size,
                                )
                                .await?,
                            )));
                        }
                        Some(SelectAction::Quit) => break,
                        None => {}
                    }
                }
            }
            AppState::Running(runner) => {
                tokio::select! {
                    event = event_stream.next() => {
                        let Some(Ok(event)) = event else { break; };
                        match event {
                            Event::Key(key) => {
                                if let Some(RunAction::Exit) = runner.handle_key(key).await {
                                    break;
                                }
                            }
                            Event::Resize(w, h) => runner.handle_resize(w, h),
                            _ => {}
                        }
                    }
                    data = runner.output.next() => {
                        match data {
                            Some(data) => runner.process_output(data),
                            None => break,
                        }
                    }
                }
            }
        }

        if let Some(new_state) = next_state {
            state = new_state;
        }

        tui.draw(|f| state.render(f))?;
    }

    Ok(())
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
