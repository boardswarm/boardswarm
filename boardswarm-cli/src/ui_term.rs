use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::Span,
    widgets::Widget,
};

pub struct UiTerm<'a> {
    screen: &'a vt100::Screen,
    // Per-row high-water mark: the number of columns that were painted (i.e. not
    // skipped) in the previous frame. Used to clear cells when a line shrinks
    // while leaving never-painted trailing cells untouched, so copied lines
    // don't carry trailing whitespace.
    painted_cols: &'a mut Vec<u16>,
}

impl<'a> UiTerm<'a> {
    pub fn new(screen: &'a vt100::Screen, painted_cols: &'a mut Vec<u16>) -> Self {
        UiTerm {
            screen,
            painted_cols,
        }
    }
}

impl Widget for UiTerm<'_> {
    fn render(self, area: Rect, buf: &mut ratatui::buffer::Buffer) {
        let screen = self.screen;
        // New rows start with no painted columns, so trailing cells are left
        // untouched rather than filled with spaces. The screen is cleared before
        // the first frame, so there is no pre-existing content to clear.
        self.painted_cols.resize(area.height as usize, 0);

        for row in 0..area.height {
            // Column past the last cell that has content on this row.
            let mut content_end = 0;
            for col in 0..area.width {
                if screen.cell(row, col).is_some_and(|c| c.has_contents()) {
                    content_end = col + 1;
                }
            }

            // Clear up to whichever is further right: the current content or the
            // cells that were painted last frame. This removes stale glyphs left
            // behind when a line shrinks.
            let clear_end = content_end.max(self.painted_cols[row as usize]);

            for col in 0..area.width {
                let to_cell = &mut buf[(area.x + col, area.y + row)];
                if col < content_end {
                    if let Some(cell) = screen.cell(row, col).filter(|c| c.has_contents()) {
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
                        to_cell.set_symbol(cell.contents());
                    } else {
                        // Blank cell in the middle of a line.
                        to_cell.set_char(' ');
                    }
                } else if col < clear_end {
                    // Trailing cell that held content last frame: clear it.
                    to_cell.set_char(' ');
                } else {
                    // Trailing cell that was never painted: leave the host
                    // terminal untouched so copied lines have no trailing spaces.
                    to_cell.set_skip(true);
                }
            }

            self.painted_cols[row as usize] = content_end;
        }

        let scrollback = screen.scrollback();
        if scrollback > 0 {
            let str = format!(" -{} ", scrollback);
            let width = str.len() as u16;
            let span = Span::styled(str, Style::reset().bg(Color::LightYellow).fg(Color::Black));
            let x = area.x + area.width - width;
            let y = area.y;
            buf.set_span(x, y, &span, width);
            // The overlay paints into the trailing region of the top row; make
            // sure those cells get cleared once scrollback returns to zero.
            self.painted_cols[0] = area.width;
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

#[cfg(test)]
mod test {
    use super::*;
    use ratatui::buffer::Buffer;

    fn render(parser: &vt100::Parser, painted_cols: &mut Vec<u16>) -> Buffer {
        let area = Rect::new(0, 0, parser.screen().size().1, parser.screen().size().0);
        let mut buf = Buffer::empty(area);
        UiTerm::new(parser.screen(), painted_cols).render(area, &mut buf);
        buf
    }

    #[test]
    fn trailing_cells_are_skipped() {
        let mut parser = vt100::Parser::new(2, 20, 0);
        parser.process(b"hello");

        let mut painted_cols = Vec::new();
        let buf = render(&parser, &mut painted_cols);

        // The cells holding "hello" are painted.
        for col in 0..5 {
            assert!(!buf[(col, 0)].skip, "col {col} should be painted");
        }
        // Everything past the content on both rows is left untouched so copied
        // lines carry no trailing whitespace.
        for col in 5..20 {
            assert!(buf[(col, 0)].skip, "col {col} should be skipped");
        }
        for col in 0..20 {
            assert!(buf[(col, 1)].skip, "empty row col {col} should be skipped");
        }
    }

    #[test]
    fn shrinking_line_clears_stale_cells() {
        let mut parser = vt100::Parser::new(1, 20, 0);
        parser.process(b"hello world");

        let mut painted_cols = Vec::new();
        render(&parser, &mut painted_cols);

        // Redraw the line as something shorter.
        parser.process(b"\r\x1b[Khi");
        let buf = render(&parser, &mut painted_cols);

        assert_eq!(buf[(0, 0)].symbol(), "h");
        assert_eq!(buf[(1, 0)].symbol(), "i");
        // Cells that previously held "llo world" are cleared (not skipped) so no
        // stale glyphs remain on the terminal.
        for col in 2..11 {
            assert!(!buf[(col, 0)].skip, "stale col {col} should be cleared");
            assert_eq!(buf[(col, 0)].symbol(), " ");
        }
        // Cells that were never painted stay untouched.
        for col in 11..20 {
            assert!(buf[(col, 0)].skip, "col {col} should be skipped");
        }
    }
}
