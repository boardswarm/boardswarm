use ratatui::{
    layout::Rect,
    style::{Color, Style},
    widgets::{Block, Paragraph, Widget},
};

#[derive(Debug, Clone)]
pub enum UiStatusMode {
    Normal,
    Error(String),
}

pub struct UiStatus {
    mode: UiStatusMode,
}

impl UiStatus {
    pub fn new(mode: UiStatusMode) -> Self {
        UiStatus { mode }
    }

    fn status_style(&self) -> Style {
        let (fg, bg) = match &self.mode {
            UiStatusMode::Normal => (Color::Black, Color::Gray),
            UiStatusMode::Error(_) => (Color::Black, Color::LightRed),
        };
        Style::default().fg(fg).bg(bg)
    }

    fn status_text(&self) -> String {
        match &self.mode {
            UiStatusMode::Normal => "Normal".to_string(),
            UiStatusMode::Error(msg) => format!("Error: {}", msg),
        }
    }
}

impl Widget for UiStatus {
    fn render(self, area: Rect, buf: &mut ratatui::buffer::Buffer) {
        let paragraph = Paragraph::new(self.status_text()).block(
            Block::default().style(self.status_style()));
        paragraph.render(area, buf);
    }
}
