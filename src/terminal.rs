use std::collections::BTreeMap;

use alacritty_terminal::{
    event::VoidListener,
    grid::Dimensions,
    index::Point,
    term::{
        cell::{Cell, Flags},
        Config, Term, TermMode,
    },
    vte::{
        self,
        ansi::{Color, Processor},
    },
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Clone, Copy)]
pub struct TermSize {
    rows: usize,
    cols: usize,
}

impl TermSize {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            rows: rows.max(1) as usize,
            cols: cols.max(1) as usize,
        }
    }
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

pub struct AlacrittyTermState {
    term: Term<VoidListener>,
    parser: Processor,
    rows: u16,
    cols: u16,
}

#[derive(Clone)]
pub struct TerminalCell {
    pub text: String,
    pub fg: Color,
    pub bg: Color,
    pub flags: Flags,
    pub width: u16,
}

impl AlacrittyTermState {
    pub fn new(rows: u16, cols: u16, scrollback: usize) -> Self {
        let mut config = Config::default();
        config.scrolling_history = scrollback;
        let size = TermSize::new(rows, cols);
        Self {
            term: Term::new(config, &size, VoidListener),
            parser: Processor::new(),
            rows: size.screen_lines() as u16,
            cols: size.columns() as u16,
        }
    }

    pub fn process(&mut self, data: &[u8]) -> bool {
        self.parser.advance(&mut self.term, data);
        !data.is_empty() && !self.sync_update_active()
    }

    fn sync_update_active(&self) -> bool {
        self.parser.sync_bytes_count() > 0
            || self.parser.sync_timeout().sync_timeout().is_some()
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        let size = TermSize::new(rows, cols);
        self.term.resize(size);
        self.rows = size.screen_lines() as u16;
        self.cols = size.columns() as u16;
    }

    pub fn size(&self) -> (u16, u16) {
        (self.rows, self.cols)
    }

    pub fn cursor_position(&self) -> (u16, u16) {
        let point = self.term.grid().cursor.point;
        let row = point.line.0.max(0) as u16;
        let col = point.column.0 as u16;
        (
            row.min(self.rows.saturating_sub(1)),
            col.min(self.cols.saturating_sub(1)),
        )
    }

    pub fn hide_cursor(&self) -> bool {
        !self.term.mode().contains(TermMode::SHOW_CURSOR)
    }

    pub fn alternate_screen(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    pub fn mouse_mode(&self) -> u8 {
        let mode = self.term.mode();
        if mode.contains(TermMode::MOUSE_MOTION) {
            4
        } else if mode.contains(TermMode::MOUSE_DRAG) {
            3
        } else if mode.contains(TermMode::MOUSE_REPORT_CLICK) {
            2
        } else if mode.contains(TermMode::MOUSE_MODE) {
            1
        } else {
            0
        }
    }

    pub fn scrollback_bottom(&mut self) {
        self.term
            .grid_mut()
            .scroll_display(alacritty_terminal::grid::Scroll::Bottom);
    }

    pub fn visible_rows(&self) -> Vec<Vec<Option<TerminalCell>>> {
        let mut rows = vec![vec![None; self.cols as usize]; self.rows as usize];
        let mut line_map = BTreeMap::new();
        for indexed in self.term.grid().display_iter() {
            let next = line_map.len();
            let row_idx = *line_map.entry(indexed.point.line.0).or_insert(next);
            if row_idx >= rows.len() {
                continue;
            }
            let col_idx = indexed.point.column.0;
            if col_idx >= rows[row_idx].len() {
                continue;
            }
            rows[row_idx][col_idx] = cell_to_view(indexed.cell);
        }
        rows
    }

    pub fn cell_at(&self, row: u16, col: u16) -> Option<TerminalCell> {
        self.visible_rows()
            .get(row as usize)
            .and_then(|r| r.get(col as usize))
            .and_then(Clone::clone)
    }

    pub fn row_wrapped(&self, row: u16) -> bool {
        self.visible_rows()
            .get(row as usize)
            .and_then(|r| r.iter().rev().find_map(|c| c.as_ref()))
            .is_some_and(|c| c.flags.contains(Flags::WRAPLINE))
    }

    pub fn snapshot_rows(
        &self,
    ) -> (Vec<Vec<Option<TerminalCell>>>, usize, usize) {
        let grid = self.term.grid();
        let start = Point::new(grid.topmost_line() - 1, grid.last_column());
        let cursor = grid.cursor.point;
        let mut rows_by_line: BTreeMap<i32, Vec<Option<TerminalCell>>> =
            BTreeMap::new();
        for indexed in grid.iter_from(start) {
            let row = rows_by_line
                .entry(indexed.point.line.0)
                .or_insert_with(|| vec![None; self.cols as usize]);
            let col = indexed.point.column.0;
            if col < row.len() {
                row[col] = cell_to_view(indexed.cell);
            }
        }
        let cursor_line = cursor.line.0;
        let cursor_row = rows_by_line
            .keys()
            .position(|line| *line == cursor_line)
            .unwrap_or_else(|| rows_by_line.len().saturating_sub(1));
        (
            rows_by_line.into_values().collect(),
            cursor_row,
            cursor.column.0,
        )
    }
}

fn cell_to_view(cell: &Cell) -> Option<TerminalCell> {
    if cell.flags.contains(Flags::WIDE_CHAR_SPACER)
        || cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)
    {
        return None;
    }
    let mut text = String::new();
    text.push(cell.c);
    if let Some(chars) = cell.zerowidth() {
        text.extend(chars.iter().copied());
    }
    if text.is_empty() || text == "\0" {
        text.clear();
        text.push(' ');
    }
    let width = UnicodeWidthStr::width(text.as_str())
        .max(cell.c.width().unwrap_or(1))
        .max(1) as u16;
    Some(TerminalCell {
        text,
        fg: cell.fg,
        bg: cell.bg,
        flags: cell.flags,
        width,
    })
}

pub fn color_name(color: Color) -> String {
    match color {
        Color::Named(
            vte::ansi::NamedColor::Foreground
            | vte::ansi::NamedColor::Background,
        ) => "default".to_string(),
        Color::Named(named) => format!("{:?}", named).to_lowercase(),
        Color::Indexed(index) => format!("colour{}", index),
        Color::Spec(rgb) => format!("#{:02x}{:02x}{:02x}", rgb.r, rgb.g, rgb.b),
    }
}

pub fn color_is_default(color: Color) -> bool {
    matches!(
        color,
        Color::Named(
            vte::ansi::NamedColor::Foreground
                | vte::ansi::NamedColor::Background
        )
    )
}

#[cfg(test)]
mod tests {
    use super::AlacrittyTermState;

    fn first_row_text(term: &AlacrittyTermState) -> String {
        term.visible_rows()
            .into_iter()
            .next()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|cell| cell.map(|cell| cell.text))
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn synchronized_output_suppresses_intermediate_render() {
        let mut term = AlacrittyTermState::new(3, 20, 2000);

        assert!(!term.process(b"\x1b[?2026hhello"));
        assert_eq!(first_row_text(&term), "");
        assert!(!term.process(b" world"));
        assert_eq!(first_row_text(&term), "");
        assert!(term.process(b"\x1b[?2026l"));
        assert_eq!(first_row_text(&term), "hello world");
    }
}
