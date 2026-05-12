use std::collections::BTreeMap;

use alacritty_terminal::{
    event::VoidListener,
    grid::{Dimensions, GridCell},
    index::{Line, Point},
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
    scroll_on_erase_in_display: bool,
    scroll_on_erase_history: bool,
    suppress_next_scroll_on_erase: bool,
    pending_scroll_erase_escape: Vec<u8>,
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
            scroll_on_erase_in_display: false,
            scroll_on_erase_history: false,
            suppress_next_scroll_on_erase: false,
            pending_scroll_erase_escape: Vec::new(),
        }
    }

    pub fn set_scroll_on_erase_in_display(&mut self, enabled: bool) {
        self.scroll_on_erase_in_display = enabled;
        if !enabled {
            self.suppress_next_scroll_on_erase = false;
        }
    }

    pub fn suppress_next_scroll_on_erase_in_display(&mut self) {
        self.suppress_next_scroll_on_erase = true;
    }

    pub fn scroll_on_erase_in_display(&self) -> bool {
        self.scroll_on_erase_in_display
    }

    pub fn scroll_on_erase_history(&self) -> bool {
        self.scroll_on_erase_history
    }

    pub fn process(&mut self, data: &[u8]) -> bool {
        if self.scroll_on_erase_in_display {
            self.process_with_scroll_on_erase(data);
        } else if self.pending_scroll_erase_escape.is_empty() {
            self.parser.advance(&mut self.term, data);
        } else {
            let mut bytes =
                std::mem::take(&mut self.pending_scroll_erase_escape);
            bytes.extend_from_slice(data);
            self.parser.advance(&mut self.term, &bytes);
        }
        !data.is_empty() && !self.sync_update_active()
    }

    fn process_with_scroll_on_erase(&mut self, data: &[u8]) {
        let mut bytes = std::mem::take(&mut self.pending_scroll_erase_escape);
        bytes.extend_from_slice(data);
        let mut segment_start = 0usize;
        let mut i = 0usize;
        while i < bytes.len() {
            if bytes[i] != 0x1b {
                i += 1;
                continue;
            }
            if i + 1 >= bytes.len() {
                self.parser
                    .advance(&mut self.term, &bytes[segment_start..i]);
                self.pending_scroll_erase_escape
                    .extend_from_slice(&bytes[i..]);
                return;
            }
            if bytes[i + 1] != b'[' {
                i += 1;
                continue;
            }
            let params_start = i + 2;
            let Some(rel_end) = bytes[params_start..]
                .iter()
                .position(|&b| (0x40..=0x7e).contains(&b))
            else {
                self.parser
                    .advance(&mut self.term, &bytes[segment_start..i]);
                self.pending_scroll_erase_escape
                    .extend_from_slice(&bytes[i..]);
                return;
            };
            let final_index = params_start + rel_end;
            let final_byte = bytes[final_index];
            let params = &bytes[params_start..final_index];
            if final_byte == b'J'
                && is_erase_display_all(params)
                && !self.alternate_screen()
            {
                self.parser
                    .advance(&mut self.term, &bytes[segment_start..i]);
                if self.suppress_next_scroll_on_erase {
                    self.suppress_next_scroll_on_erase = false;
                    self.parser
                        .advance(&mut self.term, &bytes[i..=final_index]);
                } else if self.should_scroll_erase_into_history() {
                    self.scroll_full_viewport_into_history();
                    self.scroll_on_erase_history = true;
                } else {
                    self.clear_viewport_without_history();
                }
                segment_start = final_index + 1;
            }
            i = final_index + 1;
        }
        self.parser.advance(&mut self.term, &bytes[segment_start..]);
    }

    fn should_scroll_erase_into_history(&self) -> bool {
        let grid = self.term.grid();
        let rows = self.rows.max(1) as i32;
        let mut first_non_empty: Option<i32> = None;
        let mut non_empty_rows = 0usize;

        for line in 0..rows {
            let row_has_content =
                (&grid[Line(line)]).into_iter().any(|cell| !cell.is_empty());
            if row_has_content {
                first_non_empty.get_or_insert(line);
                non_empty_rows += 1;
            }
        }

        let Some(first_non_empty) = first_non_empty else {
            return false;
        };
        let leading_blank_rows = first_non_empty.max(0) as usize;
        let sparse_content_limit = (self.rows as usize / 5).max(3);
        if leading_blank_rows > self.rows as usize / 3
            && non_empty_rows <= sparse_content_limit
        {
            return false;
        }
        true
    }

    fn scroll_full_viewport_into_history(&mut self) {
        let rows = self.rows.max(1) as usize;
        let region = Line(0)..Line(self.rows.max(1) as i32);
        self.term.grid_mut().scroll_up::<Color>(&region, rows);
    }

    fn clear_viewport_without_history(&mut self) {
        self.term.grid_mut().reset_region::<Color, _>(..);
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

    pub fn scrollback_top(&mut self) {
        self.term
            .grid_mut()
            .scroll_display(alacritty_terminal::grid::Scroll::Top);
    }

    pub fn visible_rows(&self) -> Vec<Vec<Option<TerminalCell>>> {
        let grid = self.term.grid();
        let top_line = -(grid.display_offset() as i32);
        let mut rows = vec![vec![None; self.cols as usize]; self.rows as usize];
        for indexed in grid.display_iter() {
            let relative_line = indexed.point.line.0 - top_line;
            if relative_line < 0 || relative_line >= rows.len() as i32 {
                continue;
            }
            let row_idx = relative_line as usize;
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

fn is_erase_display_all(params: &[u8]) -> bool {
    !params.is_empty() && params.iter().all(|&b| b.is_ascii_digit()) && {
        let trimmed = params
            .iter()
            .position(|&b| b != b'0')
            .map(|index| &params[index..])
            .unwrap_or(b"0".as_slice());
        trimmed == b"2"
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

    fn screen_text(term: &AlacrittyTermState) -> String {
        term.visible_rows()
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .filter_map(|cell| cell.map(|cell| cell.text))
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn scroll_on_erase_in_display_handles_split_ed2() {
        let mut term = AlacrittyTermState::new(3, 20, 2000);
        term.set_scroll_on_erase_in_display(true);
        term.process(b"abc");
        term.process(b"\x1b[2");
        term.process(b"Jxyz");
        let text = screen_text(&term);
        assert!(text.contains("xyz"));
        assert!(!text.contains("Jxyz"));
    }

    #[test]
    fn suppressed_scroll_on_erase_uses_real_ed2_once() {
        let mut term = AlacrittyTermState::new(3, 20, 2000);
        term.set_scroll_on_erase_in_display(true);
        term.suppress_next_scroll_on_erase_in_display();
        term.process(b"abc\x1b[H\x1b[2Jprompt");
        assert_eq!(first_row_text(&term), "prompt");
        assert!(!term.scroll_on_erase_history());
    }

    #[test]
    fn sparse_bottom_content_ed2_does_not_enter_history() {
        let mut term = AlacrittyTermState::new(6, 20, 2000);
        term.set_scroll_on_erase_in_display(true);
        term.process(b"\x1b[6;1Hbottom\x1b[H\x1b[2Jprompt");
        assert_eq!(first_row_text(&term), "prompt");
        assert!(!term.scroll_on_erase_history());
    }

    #[test]
    fn dense_content_ed2_scrolls_into_history() {
        let mut term = AlacrittyTermState::new(6, 20, 2000);
        term.set_scroll_on_erase_in_display(true);
        term.process(b"alpha\r\nbeta\r\ngamma\x1b[H\x1b[2Jprompt");
        assert!(term.scroll_on_erase_history());
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
