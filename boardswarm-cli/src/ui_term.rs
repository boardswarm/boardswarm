use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::Span,
    widgets::Widget,
};

pub struct UiTerm<'a> {
    screen: &'a vt100::Screen,
}

impl<'a> UiTerm<'a> {
    pub fn new(screen: &'a vt100::Screen) -> Self {
        UiTerm { screen }
    }
}

impl Widget for UiTerm<'_> {
    fn render(self, area: Rect, buf: &mut ratatui::buffer::Buffer) {
        let screen = self.screen;

        for row in 0..area.height {
            for col in 0..area.width {
                let to_cell = buf.get_mut(area.x + col, area.y + row);
                if let Some(cell) = screen.cell(row, col) {
                    if cell.has_contents() {
                        let mut mods = Modifier::empty();
                        mods.set(Modifier::BOLD, cell.bold());
                        mods.set(Modifier::ITALIC, cell.italic());
                        mods.set(Modifier::REVERSED, cell.inverse());
                        mods.set(Modifier::UNDERLINED, cell.underline());

                        let style = Style {
                            fg: conv_color(cell.fgcolor()),
                            bg: conv_color(cell.bgcolor()),
                            underline_color: None,
                            add_modifier: mods,
                            sub_modifier: Modifier::empty(),
                        };
                        to_cell.set_style(style);
                        to_cell.set_symbol(&cell.contents());
                    } else {
                        // Cell doesn't have content.
                        to_cell.set_char(' ');
                    }
                }
            }
        }

        let scrollback = screen.scrollback();
        if scrollback > 0 {
            let str = format!(" -{} ", scrollback);
            let width = str.len() as u16;
            let span = Span::styled(str, Style::reset().bg(Color::LightYellow).fg(Color::Black));
            let x = area.x + area.width - width;
            let y = area.y;
            buf.set_span(x, y, &span, width);
        }
    }
}

fn conv_color(color: vt100::Color) -> Option<ratatui::style::Color> {
    match color {
        vt100::Color::Default => None,
        vt100::Color::Idx(index) => Some(ratatui::style::Color::Indexed(index)),
        vt100::Color::Rgb(r, g, b) => Some(ratatui::style::Color::Rgb(r, g, b)),
    }
}
