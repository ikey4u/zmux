use std::{
    collections::BTreeMap,
    hash::{Hash, Hasher},
    time::{Duration, Instant},
};

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

pub mod osc_colors;
mod output_buffer;

pub use output_buffer::OutputBuffer;

// Process large reads and synchronized updates incrementally. This bounds the
// number of grid rows which can materialize before they are compressed into
// cold history; reserving for vte's entire 2 MiB sync buffer can otherwise turn
// newline-heavy output into millions of full-width terminal cells.
const PROCESS_CHUNK_BYTES: usize = 4 * 1024;
const SYNC_OUTPUT_HISTORY_RESERVE: usize = PROCESS_CHUNK_BYTES;
const PTY_BATCH_HISTORY_RESERVE: usize = PROCESS_CHUNK_BYTES;

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
    config: Config,
    parser: Processor,
    rows: u16,
    cols: u16,
    scrollback_limit: usize,
    scroll_on_erase_in_display: bool,
    scroll_on_erase_history: bool,
    suppress_next_scroll_on_erase: bool,
    pending_scroll_erase_escape: Vec<u8>,
    pane_default_fg: Option<(u8, u8, u8)>,
    pane_default_bg: Option<(u8, u8, u8)>,
    output_buffer: OutputBuffer,
    row_hashes: Vec<u64>,
    last_display_offset: usize,
    sync_started_at: Option<Instant>,
    forced_sync_update: bool,
    sync_display_snapshot: Option<TerminalFrameSnapshot>,
    pending_history_rows: Vec<TerminalHistoryRow>,
    history_clear_requested: bool,
    history_control: HistoryControlTracker,
}

#[derive(Clone)]
pub struct TerminalCell {
    pub text: String,
    pub fg: Color,
    pub bg: Color,
    pub flags: Flags,
    pub width: u16,
}

#[derive(Clone)]
pub(crate) struct TerminalHistoryRow {
    pub(crate) cells: Vec<Option<TerminalCell>>,
}

#[derive(Clone)]
pub(crate) struct TerminalFrameSnapshot {
    pub(crate) rows: Vec<Vec<Option<TerminalCell>>>,
    pub(crate) cursor_row: u16,
    pub(crate) cursor_col: u16,
    pub(crate) hide_cursor: bool,
    pub(crate) alternate_screen: bool,
    pub(crate) mouse_mode: u8,
}

#[derive(Default)]
struct HistoryControlTracker {
    state: HistoryControlState,
    csi_params: Vec<u8>,
    alternate_screen: bool,
    alternate_enter_requested: bool,
    sync_update: bool,
    clear_requested: bool,
}

#[derive(Clone, Copy, Default)]
enum HistoryControlState {
    #[default]
    Ground,
    Escape,
    Csi,
    String,
    StringEscape,
}

impl HistoryControlTracker {
    fn process_byte(&mut self, byte: u8) {
        match self.state {
            HistoryControlState::Ground => {
                if byte == 0x1b {
                    self.state = HistoryControlState::Escape;
                }
            }
            HistoryControlState::Escape => match byte {
                b'[' => {
                    self.csi_params.clear();
                    self.state = HistoryControlState::Csi;
                }
                b']' | b'P' | b'^' | b'_' => {
                    self.state = HistoryControlState::String;
                }
                b'c' => {
                    self.alternate_screen = false;
                    self.clear_requested = true;
                    self.state = HistoryControlState::Ground;
                }
                0x1b => {}
                _ => self.state = HistoryControlState::Ground,
            },
            HistoryControlState::Csi => {
                if (0x40..=0x7e).contains(&byte) {
                    self.finish_csi(byte);
                    self.state = HistoryControlState::Ground;
                } else if (0x20..=0x3f).contains(&byte) {
                    if self.csi_params.len() < 32 {
                        self.csi_params.push(byte);
                    }
                } else if byte == 0x1b {
                    self.state = HistoryControlState::Escape;
                }
            }
            HistoryControlState::String => match byte {
                0x07 => self.state = HistoryControlState::Ground,
                0x1b => self.state = HistoryControlState::StringEscape,
                _ => {}
            },
            HistoryControlState::StringEscape => match byte {
                b'\\' => self.state = HistoryControlState::Ground,
                0x1b => {}
                _ => self.state = HistoryControlState::String,
            },
        }
    }

    fn finish_csi(&mut self, final_byte: u8) {
        if final_byte == b'J'
            && first_csi_param(&self.csi_params) == Some(3)
            && !self.alternate_screen
        {
            self.clear_requested = true;
        } else if matches!(final_byte, b'h' | b'l')
            && self.csi_params.first() == Some(&b'?')
        {
            let enabled = final_byte == b'h';
            for parameter in self.csi_params[1..].split(|byte| *byte == b';') {
                match decimal_parameter(parameter) {
                    // Mirror only modes implemented by alacritty_terminal.
                    Some(1049) => {
                        if enabled && !self.alternate_screen {
                            self.alternate_enter_requested = true;
                        }
                        self.alternate_screen = enabled;
                    }
                    Some(2026) => self.sync_update = enabled,
                    _ => {}
                }
            }
        }
    }

    fn take_clear_requested(&mut self) -> bool {
        std::mem::take(&mut self.clear_requested)
    }

    fn take_alternate_enter_requested(&mut self) -> bool {
        std::mem::take(&mut self.alternate_enter_requested)
    }

    fn end_sync_update(&mut self) {
        self.sync_update = false;
    }
}

fn first_csi_param(parameters: &[u8]) -> Option<u16> {
    if parameters.first() == Some(&b'?') {
        return None;
    }
    decimal_parameter(
        parameters
            .split(|byte| *byte == b';')
            .next()
            .unwrap_or_default(),
    )
}

fn decimal_parameter(parameter: &[u8]) -> Option<u16> {
    if parameter.is_empty() {
        return Some(0);
    }
    parameter.iter().try_fold(0u16, |value, byte| {
        let digit = byte.checked_sub(b'0')?;
        if digit > 9 {
            return None;
        }
        value.checked_mul(10)?.checked_add(u16::from(digit))
    })
}

impl AlacrittyTermState {
    pub fn new(rows: u16, cols: u16, scrollback: usize) -> Self {
        let config = Config {
            scrolling_history: internal_scrollback_limit(scrollback),
            ..Config::default()
        };
        let size = TermSize::new(rows, cols);
        Self {
            term: Term::new(config.clone(), &size, VoidListener),
            config,
            parser: Processor::new(),
            rows: size.screen_lines() as u16,
            cols: size.columns() as u16,
            scrollback_limit: scrollback,
            scroll_on_erase_in_display: false,
            scroll_on_erase_history: false,
            suppress_next_scroll_on_erase: false,
            pending_scroll_erase_escape: Vec::new(),
            pane_default_fg: None,
            pane_default_bg: None,
            output_buffer: OutputBuffer::default(),
            row_hashes: Vec::new(),
            last_display_offset: 0,
            sync_started_at: None,
            forced_sync_update: false,
            sync_display_snapshot: None,
            pending_history_rows: Vec::new(),
            history_clear_requested: false,
            history_control: HistoryControlTracker::default(),
        }
    }

    pub fn scrollback_limit(&self) -> usize {
        self.scrollback_limit
    }

    pub fn set_scrollback_limit(&mut self, scrollback: usize) {
        let changed = scrollback != self.scrollback_limit;
        self.scrollback_limit = scrollback;

        if self.alternate_screen() {
            // `Term::set_options` updates the inactive primary grid while the
            // alternate screen is active. Growing it is safe, but defer a
            // reduction until the primary grid is visible and its oldest rows
            // can be captured first.
            let internal_limit = internal_scrollback_limit(scrollback);
            if internal_limit > self.config.scrolling_history {
                self.config.scrolling_history = internal_limit;
                self.term.set_options(self.config.clone());
            }
        } else {
            self.capture_history_rows();
        }

        if changed {
            self.output_buffer.update_all_lines();
        }
    }

    pub(crate) fn take_history_rows(&mut self) -> Vec<TerminalHistoryRow> {
        std::mem::take(&mut self.pending_history_rows)
    }

    pub(crate) fn take_history_clear_requested(&mut self) -> bool {
        std::mem::take(&mut self.history_clear_requested)
    }

    fn capture_history_rows(&mut self) {
        self.capture_history_rows_above(self.scrollback_limit);
    }

    /// Move complete logical-line prefixes above `retained_limit` into the
    /// pending cold-history queue. An unfinished logical line is never split,
    /// but the terminal's fixed internal limit still bounds its physical tail.
    fn capture_history_rows_above(&mut self, retained_limit: usize) {
        if self.alternate_screen() {
            return;
        }

        let history_size = self.term.grid().history_size();
        let overflow = history_size.saturating_sub(retained_limit);
        let capture_count = if overflow == 0 {
            0
        } else {
            let grid = self.term.grid();
            let top_line = grid.topmost_line().0;
            let mut complete_prefix = 0;

            // Capture through the first hard boundary at or beyond the target
            // watermark. Stopping the scan at `overflow` would strand a
            // completed long line which crosses that boundary; its prefix
            // could then be truncated when the fixed physical cap is reached.
            for offset in 0..history_size {
                let row = &grid[Line(top_line + offset as i32)];
                let wrapped = row
                    .last()
                    .is_some_and(|cell| cell.flags.contains(Flags::WRAPLINE));
                if !wrapped {
                    complete_prefix = offset + 1;
                    if complete_prefix >= overflow {
                        break;
                    }
                }
            }

            complete_prefix
        };

        if capture_count != 0 {
            let grid = self.term.grid();
            let top_line = grid.topmost_line().0;
            let captured = (0..capture_count).map(|offset| {
                let row = &grid[Line(top_line + offset as i32)];
                let mut cells = Vec::new();
                for (column, cell) in row.into_iter().enumerate() {
                    if cell.is_empty() {
                        continue;
                    }
                    while cells.len() < column {
                        cells.push(Some(default_blank_cell()));
                    }
                    cells.push(None);
                    write_cell_to_view(&mut cells, column, cell);
                }
                TerminalHistoryRow { cells }
            });
            self.pending_history_rows.extend(captured);

            // Lowering the history size removes exactly the oldest rows. Raise
            // it again immediately; this changes only the lazy limit and does
            // not allocate the reserve.
            let retained = history_size - capture_count;
            self.term.grid_mut().update_history(retained);
        }

        // This is also the hard physical-row bound for a single unfinished
        // logical line. Do not raise it to match `retained`: doing that for a
        // program which never emits a newline would make memory unbounded.
        let internal_limit = internal_scrollback_limit(self.scrollback_limit);
        self.term.grid_mut().update_history(internal_limit);

        if self.config.scrolling_history != internal_limit {
            self.config.scrolling_history = internal_limit;
            self.term.set_options(self.config.clone());
        }
    }

    pub fn set_pane_default_fg(&mut self, rgb: (u8, u8, u8)) {
        self.pane_default_fg = Some(rgb);
        self.output_buffer.update_all_lines();
    }

    pub fn set_pane_default_bg(&mut self, rgb: (u8, u8, u8)) {
        self.pane_default_bg = Some(rgb);
        self.output_buffer.update_all_lines();
    }

    pub fn reset_pane_default_fg(&mut self) {
        self.pane_default_fg = None;
        self.output_buffer.update_all_lines();
    }

    pub fn reset_pane_default_bg(&mut self) {
        self.pane_default_bg = None;
        self.output_buffer.update_all_lines();
    }

    pub fn pane_default_fg(&self) -> Option<(u8, u8, u8)> {
        self.pane_default_fg
    }

    pub fn pane_default_bg(&self) -> Option<(u8, u8, u8)> {
        self.pane_default_bg
    }

    pub fn output_buffer(&self) -> &OutputBuffer {
        &self.output_buffer
    }

    pub fn clear_output_buffer(&mut self) {
        self.output_buffer.clear();
    }

    pub fn force_output_full_repaint(&mut self) {
        self.output_buffer.update_all_lines();
    }

    pub fn after_pty_process(&mut self, changed: bool) {
        if !changed {
            return;
        }
        let display_offset = self.term.grid().display_offset();
        if display_offset != self.last_display_offset {
            self.output_buffer.update_all_lines();
            self.last_display_offset = display_offset;
            self.rehash_all_rows();
            return;
        }
        self.diff_visible_rows();
    }

    fn diff_visible_rows(&mut self) {
        let rows = self.visible_rows();
        if self.row_hashes.len() != rows.len() {
            self.row_hashes.resize(rows.len(), 0);
            self.output_buffer.update_all_lines();
        }
        for (i, row) in rows.iter().enumerate() {
            let hash = hash_row(row);
            if self.row_hashes.get(i) != Some(&hash) {
                self.output_buffer.update_line(i);
                if let Some(slot) = self.row_hashes.get_mut(i) {
                    *slot = hash;
                }
            }
        }
    }

    fn rehash_all_rows(&mut self) {
        let rows = self.visible_rows();
        self.row_hashes = rows.iter().map(|row| hash_row(row)).collect();
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
        let flushed_before = self.flush_sync_if_timed_out();
        for chunk in data.chunks(PROCESS_CHUNK_BYTES) {
            let mut segment_start = 0;
            for (offset, &byte) in chunk.iter().enumerate() {
                self.history_control.process_byte(byte);
                if self.history_control.take_clear_requested() {
                    self.process_terminal_segment(
                        &chunk[segment_start..=offset],
                    );
                    // Everything captured before this exact clear boundary is
                    // part of the saved history the application discarded.
                    self.pending_history_rows.clear();
                    self.history_clear_requested = true;
                    segment_start = offset + 1;
                } else if self.history_control.take_alternate_enter_requested()
                {
                    // Feed the CSI prefix while the primary grid is still
                    // active, archive its complete scrollback, and only then
                    // feed the final `h` which swaps to alternate screen. The
                    // inactive primary can now reflow without losing ordinary
                    // history to the fixed physical safety cap.
                    if segment_start < offset {
                        self.process_terminal_segment(
                            &chunk[segment_start..offset],
                        );
                    }
                    self.flush_sync_before_alternate_enter();
                    self.capture_history_rows_above(0);
                    self.process_terminal_segment(&chunk[offset..=offset]);
                    segment_start = offset + 1;
                }
            }
            if segment_start < chunk.len() {
                self.process_terminal_segment(&chunk[segment_start..]);
            }
        }
        self.track_sync_session();
        let flushed_after = self.flush_sync_if_timed_out();
        let changed = (!data.is_empty() && !self.sync_update_active())
            || flushed_before
            || flushed_after;
        self.capture_history_rows();
        changed
    }

    fn process_terminal_segment(&mut self, segment: &[u8]) {
        if segment.is_empty() {
            return;
        }

        let hard_limit = internal_scrollback_limit(self.scrollback_limit);
        let near_physical_limit = !self.alternate_screen()
            && self
                .term
                .grid()
                .history_size()
                .saturating_add(segment.len())
                >= hard_limit;
        if !near_physical_limit {
            self.process_terminal_chunk(segment);
            return;
        }

        // Once an unfinished line is near the hard cap, process through each
        // hard line-feed boundary separately. VTE treats LF, VT, and FF as
        // line feeds. Giving each one its own processing boundary lets a
        // just-completed long line move to cold storage before later bytes in
        // the same PTY batch can evict its oldest physical rows.
        let mut start = 0;
        for (offset, byte) in segment.iter().enumerate() {
            if matches!(*byte, b'\n' | 0x0b | 0x0c) {
                self.process_terminal_chunk(&segment[start..=offset]);
                start = offset + 1;
            }
        }
        if start < segment.len() {
            self.process_terminal_chunk(&segment[start..]);
        }
    }

    fn process_terminal_chunk(&mut self, chunk: &[u8]) {
        if self.scroll_on_erase_in_display {
            self.process_with_scroll_on_erase(chunk);
        } else if self.pending_scroll_erase_escape.is_empty() {
            self.parser.advance(&mut self.term, chunk);
        } else {
            let mut bytes =
                std::mem::take(&mut self.pending_scroll_erase_escape);
            bytes.extend_from_slice(chunk);
            self.parser.advance(&mut self.term, &bytes);
        }

        // Replay oversized synchronized updates in bounded internal chunks,
        // while continuing to suppress painting until ESU. This keeps atomic
        // display semantics without materializing vte's full 2 MiB buffer as
        // terminal rows at once.
        if self.parser.sync_bytes_count() >= PROCESS_CHUNK_BYTES {
            if self.sync_display_snapshot.is_none() {
                self.sync_display_snapshot =
                    Some(self.current_frame_snapshot());
            }
            self.parser.stop_sync(&mut self.term);
            self.forced_sync_update = self.history_control.sync_update;
        }
        if !self.history_control.sync_update {
            self.forced_sync_update = false;
            self.sync_display_snapshot = None;
        }
        self.capture_history_rows();
    }

    fn track_sync_session(&mut self) {
        if self.sync_update_active() && self.sync_started_at.is_none() {
            self.sync_started_at = Some(Instant::now());
        } else if !self.sync_update_active() && !self.sync_timeout_active() {
            self.sync_started_at = None;
        }
    }

    fn sync_timeout_active(&self) -> bool {
        self.parser.sync_timeout().sync_timeout().is_some()
    }

    fn sync_timeout_expired(&self) -> bool {
        self.parser
            .sync_timeout()
            .sync_timeout()
            .is_some_and(|deadline| Instant::now() >= deadline)
    }

    fn sync_session_aged_out(&self) -> bool {
        self.sync_started_at.is_some_and(|started| {
            started.elapsed() >= Duration::from_millis(50)
        })
    }

    /// Apply buffered synchronized-output bytes once their deadline passes.
    /// Alacritty does this in its event loop; without it PSReadLine prompt
    /// redraws stay invisible until `\x1b[?2026l` arrives.
    pub fn flush_sync_if_timed_out(&mut self) -> bool {
        self.flush_sync_for_display()
    }

    /// Paint buffered synchronized output when it is safe to do so.
    /// Flushing an empty BSU session breaks CPR responses and PSReadLine input.
    pub fn flush_sync_for_display(&mut self) -> bool {
        if self.parser.sync_bytes_count() == 0 {
            if (self.forced_sync_update || self.history_control.sync_update)
                && self.sync_session_aged_out()
            {
                self.forced_sync_update = false;
                self.history_control.end_sync_update();
                self.sync_display_snapshot = None;
                self.sync_started_at = None;
                self.capture_history_rows();
                return true;
            }
            return false;
        }
        if !(self.sync_timeout_expired() || self.sync_session_aged_out()) {
            return false;
        }
        self.apply_pending_sync();
        true
    }

    /// Apply buffered sync bytes before answering a CPR query from the shell.
    pub fn flush_sync_before_cpr(&mut self) -> bool {
        if self.parser.sync_bytes_count() == 0 {
            if self.forced_sync_update || self.history_control.sync_update {
                self.forced_sync_update = false;
                self.history_control.end_sync_update();
                self.sync_display_snapshot = None;
                self.sync_started_at = None;
                self.capture_history_rows();
                return true;
            }
            return false;
        }
        self.apply_pending_sync();
        true
    }

    fn apply_pending_sync(&mut self) {
        self.parser.stop_sync(&mut self.term);
        self.forced_sync_update = false;
        self.history_control.end_sync_update();
        self.sync_display_snapshot = None;
        self.sync_started_at = None;
        self.capture_history_rows();
    }

    /// Replay a small synchronized update before its final alternate-screen
    /// `h` is applied. The display snapshot and history-control state stay
    /// active, so the client still observes one atomic update, while primary
    /// scrollback produced inside the update can be archived before the grid
    /// becomes inactive and is resized.
    fn flush_sync_before_alternate_enter(&mut self) {
        if self.parser.sync_bytes_count() == 0 {
            return;
        }
        if self.sync_display_snapshot.is_none() {
            self.sync_display_snapshot = Some(self.current_frame_snapshot());
        }
        self.parser.stop_sync(&mut self.term);
        self.forced_sync_update = self.history_control.sync_update;
        self.capture_history_rows();
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

    pub fn sync_update_active(&self) -> bool {
        self.parser.sync_bytes_count() > 0
            || self.parser.sync_timeout().sync_timeout().is_some()
            || self.forced_sync_update
            || self.history_control.sync_update
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        let size = TermSize::new(rows, cols);
        let alternate_screen = self.alternate_screen();

        let hard_limit = internal_scrollback_limit(self.scrollback_limit);
        if !alternate_screen {
            // A narrower width can turn the current hot rows into many more
            // physical rows. Archive as many *complete* oldest lines as needed
            // before reflow so ordinary history reaches SQLite instead of
            // being truncated by the fixed safety limit.
            self.capture_history_rows();
            let old_cols = self.cols.max(1) as usize;
            let old_screen = self.rows.max(1) as usize;
            let target_cols = size.columns().max(1);
            let target_total_rows =
                hard_limit.saturating_add(size.screen_lines());
            let max_old_total_rows =
                target_total_rows.saturating_mul(target_cols) / old_cols;
            let target_history = max_old_total_rows.saturating_sub(old_screen);
            if self.term.grid().history_size() > target_history {
                self.capture_history_rows_above(target_history);
            }
            self.term.grid_mut().update_history(hard_limit);
        }

        // When history-limit was lowered while alt-screen was active, keep the
        // old inactive-primary cap until that grid is visible and capturable.
        let resize_limit = if alternate_screen {
            self.config.scrolling_history.max(hard_limit)
        } else {
            hard_limit
        };
        if self.config.scrolling_history != resize_limit {
            self.config.scrolling_history = resize_limit;
            self.term.set_options(self.config.clone());
        }
        self.term.resize(size);
        self.rows = size.screen_lines() as u16;
        self.cols = size.columns() as u16;
        self.capture_history_rows();
        self.output_buffer.update_all_lines();
        self.row_hashes.clear();
        self.last_display_offset = self.term.grid().display_offset();
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

    fn current_frame_snapshot(&self) -> TerminalFrameSnapshot {
        let (cursor_row, cursor_col) = self.cursor_position();
        TerminalFrameSnapshot {
            rows: self.visible_rows(),
            cursor_row,
            cursor_col,
            hide_cursor: self.hide_cursor(),
            alternate_screen: self.alternate_screen(),
            mouse_mode: self.mouse_mode(),
        }
    }

    /// Keep frame metadata at the last complete synchronized-update boundary
    /// while an oversized update is replayed incrementally into the live grid.
    pub(crate) fn frame_snapshot(&self) -> TerminalFrameSnapshot {
        self.sync_display_snapshot
            .clone()
            .unwrap_or_else(|| self.current_frame_snapshot())
    }

    pub fn scrollback_bottom(&mut self) {
        self.term
            .grid_mut()
            .scroll_display(alacritty_terminal::grid::Scroll::Bottom);
        self.after_display_scroll();
    }

    pub fn scrollback_top(&mut self) {
        self.term
            .grid_mut()
            .scroll_display(alacritty_terminal::grid::Scroll::Top);
        self.after_display_scroll();
    }

    /// Scroll the visible viewport through scrollback. Positive delta moves up
    /// into history; negative delta moves back toward the live bottom.
    pub fn scroll_display_delta(&mut self, delta: i32) -> bool {
        if delta == 0 || self.alternate_screen() {
            return false;
        }
        self.term
            .grid_mut()
            .scroll_display(alacritty_terminal::grid::Scroll::Delta(delta));
        self.after_display_scroll();
        true
    }

    fn after_display_scroll(&mut self) {
        self.output_buffer.update_all_lines();
        self.last_display_offset = self.term.grid().display_offset();
        self.rehash_all_rows();
    }

    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
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
            write_cell_to_view(&mut rows[row_idx], col_idx, indexed.cell);
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
                write_cell_to_view(row, col, indexed.cell);
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

fn internal_scrollback_limit(hot_limit: usize) -> usize {
    hot_limit
        .saturating_add(SYNC_OUTPUT_HISTORY_RESERVE)
        .saturating_add(PTY_BATCH_HISTORY_RESERVE)
}

fn hash_row(cells: &[Option<TerminalCell>]) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    let mut hasher = DefaultHasher::new();
    for cell in cells {
        match cell {
            None => 0u8.hash(&mut hasher),
            Some(c) => {
                1u8.hash(&mut hasher);
                c.text.hash(&mut hasher);
                color_hash(c.fg).hash(&mut hasher);
                color_hash(c.bg).hash(&mut hasher);
                c.flags.bits().hash(&mut hasher);
                c.width.hash(&mut hasher);
            }
        }
    }
    hasher.finish()
}

fn color_hash(color: Color) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    let mut hasher = DefaultHasher::new();
    match color {
        Color::Named(named) => {
            0u8.hash(&mut hasher);
            format!("{named:?}").hash(&mut hasher);
        }
        Color::Indexed(i) => {
            1u8.hash(&mut hasher);
            i.hash(&mut hasher);
        }
        Color::Spec(rgb) => {
            2u8.hash(&mut hasher);
            rgb.r.hash(&mut hasher);
            rgb.g.hash(&mut hasher);
            rgb.b.hash(&mut hasher);
        }
    }
    hasher.finish()
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

fn write_cell_to_view(
    row: &mut [Option<TerminalCell>],
    col: usize,
    cell: &Cell,
) {
    if let Some(view) = cell_to_view(cell) {
        row[col] = Some(view);
    } else if cell.flags.contains(Flags::WRAPLINE) {
        if let Some(prev) = row[..col].iter_mut().rev().find_map(|c| c.as_mut())
        {
            prev.flags.insert(Flags::WRAPLINE);
        }
    }
}

fn default_blank_cell() -> TerminalCell {
    TerminalCell {
        text: " ".to_string(),
        fg: Color::Named(vte::ansi::NamedColor::Foreground),
        bg: Color::Named(vte::ansi::NamedColor::Background),
        flags: Flags::empty(),
        width: 1,
    }
}

fn cell_to_view(cell: &Cell) -> Option<TerminalCell> {
    if cell.flags.contains(Flags::WIDE_CHAR_SPACER)
        || cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)
    {
        return None;
    }
    let mut text = String::new();
    // The terminal grid can retain a horizontal tab in the cell where the tab
    // originated. Re-emitting that C0 control character moves the host cursor
    // to the next tab stop without overwriting the skipped columns, leaving
    // stale characters beside tab-indented output such as `git status`.
    //
    // Grid snapshots must contain drawable cells only. The parser has already
    // represented the tab's cursor movement with the following blank cells, so
    // render its origin as a normal space and never replay terminal controls.
    let base = if cell.c.is_control() { ' ' } else { cell.c };
    text.push(base);
    if let Some(chars) = cell.zerowidth() {
        text.extend(chars.iter().copied());
    }
    if text.is_empty() || text == "\0" {
        text.clear();
        text.push(' ');
    }
    let width = UnicodeWidthStr::width(text.as_str())
        .max(base.width().unwrap_or(1))
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
    use alacritty_terminal::grid::Dimensions;

    use super::{
        AlacrittyTermState, Flags, TerminalHistoryRow, PROCESS_CHUNK_BYTES,
    };

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

    fn history_row_text(row: &TerminalHistoryRow) -> String {
        row.cells
            .iter()
            .filter_map(|cell| cell.as_ref().map(|cell| cell.text.as_str()))
            .collect::<String>()
            .trim_end()
            .to_string()
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
    fn scroll_display_delta_moves_through_scrollback() {
        let mut term = AlacrittyTermState::new(3, 20, 2000);
        term.process(b"line-one\r\nline-two\r\nline-three\r\n");
        assert_eq!(term.display_offset(), 0);
        assert!(term.scroll_display_delta(1));
        assert_eq!(term.display_offset(), 1);
        assert!(term.scroll_display_delta(-1));
        assert_eq!(term.display_offset(), 0);
    }

    #[test]
    fn scrollback_limit_can_change_without_resetting_the_screen() {
        let mut term = AlacrittyTermState::new(2, 20, 3);
        term.process(b"one\r\ntwo\r\nthree\r\nfour");
        assert_eq!(term.scrollback_limit(), 3);

        term.set_scrollback_limit(8);
        assert_eq!(term.scrollback_limit(), 8);
        assert!(screen_text(&term).contains("four"));

        term.set_scrollback_limit(1);
        assert_eq!(term.scrollback_limit(), 1);
        assert!(screen_text(&term).contains("four"));
        assert!(term.snapshot_rows().0.len() <= 3);
    }

    #[test]
    fn lowering_hot_limit_moves_oldest_complete_lines_to_cold_queue() {
        let mut term = AlacrittyTermState::new(2, 20, 6);
        term.process(
            b"line-0\r\nline-1\r\nline-2\r\nline-3\r\nline-4\r\nline-5\r\nline-6\r\nline-7\r\nline-8\r\nline-9",
        );
        term.set_scrollback_limit(2);

        let pending = term.take_history_rows();
        let texts: Vec<_> = pending.iter().map(history_row_text).collect();
        assert_eq!(
            texts,
            vec!["line-0", "line-1", "line-2", "line-3", "line-4", "line-5"]
        );
        assert_eq!(term.term.grid().history_size(), 2);
        let visible = screen_text(&term);
        assert!(visible.contains("line-8"));
        assert!(visible.contains("line-9"));
    }

    #[test]
    fn scrollback_limit_change_preserves_alternate_screen_state() {
        let mut term = AlacrittyTermState::new(3, 20, 3);
        term.process(b"primary");
        term.process(b"\x1b[?1049halternate");
        assert!(term.alternate_screen());

        term.set_scrollback_limit(8);
        assert!(term.alternate_screen());

        term.process(b"\x1b[?1049l");
        assert!(!term.alternate_screen());
        assert!(screen_text(&term).contains("primary"));
        assert!(!screen_text(&term).contains("alternate"));
    }

    #[test]
    fn alternate_screen_resize_bounds_unfinished_primary_history() {
        let mut term = AlacrittyTermState::new(3, 80, 1);
        let mut primary_line = String::from("PRIMARY-START-");
        primary_line.extend(std::iter::repeat_n('x', 85_000));
        term.process(primary_line.as_bytes());

        term.process(b"\x1b[?1049halt-screen");
        assert!(term.alternate_screen());
        term.resize(3, 10);
        term.process(b"\x1b[?1049l");
        assert!(
            term.term.grid().history_size()
                <= super::internal_scrollback_limit(1)
        );
        term.process(b"\r\nnext\r\ntail\r\nend\r\nfinal\r\nlast");

        assert!(!term.alternate_screen());
        let pending = term.take_history_rows();
        assert!(!pending.is_empty());
        let mut restored = String::new();
        for row in &pending {
            restored.push_str(&history_row_text(row));
            let wrapped = row
                .cells
                .iter()
                .flatten()
                .any(|cell| cell.flags.contains(Flags::WRAPLINE));
            if !wrapped {
                break;
            }
        }
        assert!(restored.len() < primary_line.len());
        assert!(!restored.starts_with("PRIMARY-START-"));
        assert!(restored.chars().all(|character| character == 'x'));
    }

    #[test]
    fn alternate_screen_entry_archives_complete_primary_history_before_resize()
    {
        let mut term = AlacrittyTermState::new(2, 80, 2_000);
        let mut output = String::new();
        for index in 0..2_002 {
            if index != 0 {
                output.push_str("\r\n");
            }
            output.push_str(&format!("L{index:04}{}", "x".repeat(74)));
        }
        term.process(output.as_bytes());
        assert!(term.take_history_rows().is_empty());

        term.process(b"\x1b[?1049halt-screen");
        let archived = crate::copy_mode::snapshot_lines_from_history_rows(
            &term.take_history_rows(),
        );
        assert_eq!(archived.len(), 2_000);
        assert!(archived[0].text.starts_with("L0000"));
        assert!(archived[1_999].text.starts_with("L1999"));

        term.resize(2, 1);
        term.process(b"\x1b[?1049l");
        let retained = term
            .snapshot_rows()
            .0
            .into_iter()
            .flat_map(|row| row.into_iter().flatten())
            .map(|cell| cell.text)
            .collect::<String>();
        assert!(retained.contains("L2000"));
        assert!(retained.contains("L2001"));
    }

    #[test]
    fn alternate_screen_entry_inside_sync_archives_primary_history() {
        let mut term = AlacrittyTermState::new(2, 80, 2_000);
        let mut output = String::from("\x1b[?2026h");
        for index in 0..2_002 {
            if index != 0 {
                output.push_str("\r\n");
            }
            output.push_str(&format!("S{index:04}{}", "x".repeat(74)));
        }
        output.push_str("\x1b[?1049halt-screen\x1b[?2026l");

        term.process(output.as_bytes());

        assert!(term.alternate_screen());
        let archived = crate::copy_mode::snapshot_lines_from_history_rows(
            &term.take_history_rows(),
        );
        assert_eq!(archived.len(), 2_000);
        assert!(archived[0].text.starts_with("S0000"));
        assert!(archived[1_999].text.starts_with("S1999"));
        let rendered = screen_text(&term);
        assert_eq!(
            rendered.split_whitespace().collect::<String>(),
            "alt-screen"
        );
    }

    #[test]
    fn narrow_resize_archives_complete_hot_lines_before_reflow() {
        let mut term = AlacrittyTermState::new(2, 20, 500);
        let mut output = String::new();
        for index in 0..500 {
            if index != 0 {
                output.push_str("\r\n");
            }
            output.push_str(&format!("L{index:04}xxxxxxxxxxxxxxx"));
        }
        term.process(output.as_bytes());
        assert!(term.take_history_rows().is_empty());

        term.resize(2, 1);

        let archived = crate::copy_mode::snapshot_lines_from_history_rows(
            &term.take_history_rows(),
        );
        assert!(!archived.is_empty());
        assert_eq!(archived[0].text, "L0000xxxxxxxxxxxxxxx");
        assert!(
            term.term.grid().history_size() <= term.scrollback_limit() + 20
        );
    }

    #[test]
    fn completed_overflow_moves_to_pending_history() {
        let mut term = AlacrittyTermState::new(2, 20, 2);
        term.process(b"zero\r\none\r\ntwo\r\nthree\r\nfour");

        assert_eq!(term.term.grid().history_size(), 2);
        let pending = term.take_history_rows();
        assert_eq!(pending.len(), 1);
        assert_eq!(history_row_text(&pending[0]), "zero");
    }

    #[test]
    fn zero_hot_limit_moves_every_completed_history_row_to_cold_queue() {
        let mut term = AlacrittyTermState::new(2, 20, 0);
        term.process(b"zero\r\none\r\ntwo\r\nthree");

        assert_eq!(term.term.grid().history_size(), 0);
        assert_eq!(
            term.take_history_rows()
                .iter()
                .map(history_row_text)
                .collect::<Vec<_>>(),
            vec!["zero", "one"]
        );
        let visible = screen_text(&term);
        assert!(visible.contains("two"));
        assert!(visible.contains("three"));
    }

    #[test]
    fn alternate_screen_rows_never_enter_pending_history() {
        let mut term = AlacrittyTermState::new(2, 20, 1);
        term.process(
            b"primary-zero\r\nprimary-one\r\nprimary-two\r\nprimary-three",
        );
        let primary = term.take_history_rows();
        assert_eq!(primary.len(), 1);
        assert_eq!(history_row_text(&primary[0]), "primary-zero");

        term.process(b"\x1b[?1049halt-zero\r\nalt-one\r\nalt-two\r\nalt-three");
        assert!(term.alternate_screen());
        let archived_on_entry = term.take_history_rows();
        assert_eq!(archived_on_entry.len(), 1);
        assert_eq!(history_row_text(&archived_on_entry[0]), "primary-one");

        term.process(b"\x1b[?1049l");
        assert!(!term.alternate_screen());
        assert!(term.take_history_rows().is_empty());
    }

    #[test]
    fn saved_history_clear_is_reported_across_fragmented_input() {
        let mut term = AlacrittyTermState::new(2, 20, 2);
        term.process(b"\x1b[3");
        assert!(!term.take_history_clear_requested());
        term.process(b"J");
        assert!(term.take_history_clear_requested());
        assert!(!term.take_history_clear_requested());
    }

    #[test]
    fn saved_history_clear_inside_alternate_screen_is_not_reported() {
        let mut term = AlacrittyTermState::new(2, 20, 2);
        term.process(b"\x1b[?1049;25h\x1b[3J\x1b[?1049;25l");
        assert!(!term.take_history_clear_requested());
        term.process(b"\x1b[03;0J");
        assert!(term.take_history_clear_requested());
    }

    #[test]
    fn terminal_reset_discards_only_rows_before_its_exact_boundary() {
        let mut term = AlacrittyTermState::new(2, 20, 1);
        term.process(b"old-0\r\nold-1\r\nold-2\r\nold-3");
        assert!(!term.take_history_rows().is_empty());
        term.process(b"before-reset\r\n\x1bcnew-0\r\nnew-1\r\nnew-2\r\nnew-3");

        assert!(term.take_history_clear_requested());
        let rows = term.take_history_rows();
        let texts: Vec<_> = rows.iter().map(history_row_text).collect();
        assert!(texts.iter().all(|text| !text.contains("before-reset")));
        assert!(texts.iter().any(|text| text == "new-0"));
    }

    #[test]
    fn escape_text_inside_osc_does_not_clear_saved_history() {
        let mut term = AlacrittyTermState::new(2, 20, 2);
        term.process(b"\x1b]0;literal \x1b[3J title\x07");
        assert!(!term.take_history_clear_requested());
    }

    #[test]
    fn wrapped_overflow_waits_for_a_complete_logical_line() {
        let mut term = AlacrittyTermState::new(2, 4, 1);
        term.process(b"abcdefghijklmnop");

        assert!(term.term.grid().history_size() > term.scrollback_limit());
        assert!(term.take_history_rows().is_empty());

        term.process(b"\r\nZ\r\nQ\r\n");
        let pending = term.take_history_rows();
        assert_eq!(pending.len(), 4);
        assert_eq!(
            pending.iter().map(history_row_text).collect::<String>(),
            "abcdefghijklmnop"
        );
        assert!(pending[..3].iter().all(|row| row
            .cells
            .iter()
            .flatten()
            .any(|cell| cell.flags.contains(Flags::WRAPLINE))));
        assert!(!pending[3]
            .cells
            .iter()
            .flatten()
            .any(|cell| cell.flags.contains(Flags::WRAPLINE)));
        assert!(term.term.grid().history_size() <= term.scrollback_limit());
    }

    #[test]
    fn completed_long_line_crossing_watermark_is_archived_before_later_output()
    {
        let mut term = AlacrittyTermState::new(2, 1, 2_000);
        let mut long_line = String::from("M");
        long_line.extend(std::iter::repeat_n('x', 8_999));
        let mut output = long_line.clone();
        output.push_str("\r\n");
        for _ in 0..1_300 {
            output.push_str("q\r\n");
        }

        term.process(output.as_bytes());

        let archived = crate::copy_mode::snapshot_lines_from_history_rows(
            &term.take_history_rows(),
        );
        assert!(!archived.is_empty());
        assert_eq!(archived[0].text, long_line);
        assert!(term.term.grid().history_size() <= term.scrollback_limit());
    }

    #[test]
    fn vertical_tab_and_form_feed_archive_completed_long_lines() {
        for separator in [0x0b, 0x0c] {
            let mut term = AlacrittyTermState::new(2, 1, 2_000);
            let mut long_line = String::from("M");
            long_line.extend(std::iter::repeat_n('x', 8_995));
            let mut output = long_line.clone().into_bytes();
            output.push(separator);
            for _ in 0..3_000 {
                output.push(b'y');
                output.push(separator);
            }

            term.process(&output);

            let archived = crate::copy_mode::snapshot_lines_from_history_rows(
                &term.take_history_rows(),
            );
            assert!(!archived.is_empty(), "separator {separator:#04x}");
            assert_eq!(
                archived[0].text, long_line,
                "separator {separator:#04x}"
            );
            assert!(
                term.term.grid().history_size() <= term.scrollback_limit(),
                "separator {separator:#04x}"
            );
        }
    }

    #[test]
    fn unterminated_output_has_a_fixed_physical_history_bound() {
        let hot_limit = 2_000;
        let hard_limit = super::internal_scrollback_limit(hot_limit);
        let mut term = AlacrittyTermState::new(2, 1, hot_limit);
        let mut output = String::from("START");
        output.extend(std::iter::repeat_n('x', hard_limit + 5_000));

        term.process(output.as_bytes());

        assert_eq!(term.term.grid().history_size(), hard_limit);
        assert!(term.take_history_rows().is_empty());
        let (rows, _, _) = term.snapshot_rows();
        let retained = rows
            .into_iter()
            .flat_map(|row| row.into_iter().flatten())
            .map(|cell| cell.text)
            .collect::<String>();
        assert!(!retained.contains("START"));
        assert!(retained.chars().all(|character| character == 'x'));
    }

    #[test]
    fn synchronized_output_is_captured_after_replay() {
        let mut term = AlacrittyTermState::new(2, 20, 1);

        assert!(!term.process(b"\x1b[?2026hzero\r\none\r\ntwo\r\nthree"));
        assert!(term.take_history_rows().is_empty());

        assert!(term.process(b"\x1b[?2026l"));
        assert_eq!(term.term.grid().history_size(), 1);
        let pending = term.take_history_rows();
        assert_eq!(pending.len(), 1);
        assert_eq!(history_row_text(&pending[0]), "zero");
    }

    #[test]
    fn newline_heavy_pty_batch_is_captured_incrementally_and_sparsely() {
        let mut term = AlacrittyTermState::new(2, 80, 2);
        let output = vec![b'\n'; 64 * 1024];

        assert!(term.process(&output));

        let pending = term.take_history_rows();
        assert!(pending.len() > 60_000);
        assert!(pending.iter().all(|row| row.cells.is_empty()));
        assert!(term.term.grid().history_size() <= term.scrollback_limit());
    }

    #[test]
    fn large_synchronized_output_replays_in_bounded_chunks() {
        let mut term = AlacrittyTermState::new(2, 80, 2);
        let mut output = b"\x1b[?2026h".to_vec();
        output.resize(output.len() + 128 * 1024, b'\n');
        output.extend_from_slice(b"\x1b[?2026l");

        assert!(term.process(&output));

        let pending = term.take_history_rows();
        assert!(pending.len() > 120_000);
        assert!(pending.iter().all(|row| row.cells.is_empty()));
        assert!(term.term.grid().history_size() <= term.scrollback_limit());
    }

    #[test]
    fn scroll_display_delta_is_noop_on_alternate_screen() {
        let mut term = AlacrittyTermState::new(3, 20, 2000);
        term.process(b"\x1b[?1049h");
        assert!(!term.scroll_display_delta(1));
    }

    #[test]
    fn row_wrapped_survives_wide_char_spacer_at_wrap_boundary() {
        let mut term = AlacrittyTermState::new(3, 5, 100);
        term.process("abc中x".as_bytes());

        assert_eq!(first_row_text(&term), "abc中");
        assert!(term.row_wrapped(0));
    }

    #[test]
    fn git_status_scrolling_keeps_tab_indentation_blank() {
        let mut term = AlacrittyTermState::new(8, 100, 1000);
        let output = concat!(
            "On branch main\r\n",
            "Changes not staged for commit:\r\n",
            "  (use \"git add <file>...\" to update)\r\n",
            "  (use \"git restore <file>...\" to discard)\r\n",
            "\t\x1b[31mmodified:   one\x1b[m\r\n",
            "\t\x1b[31mmodified:   two\x1b[m\r\n",
            "\t\x1b[31mmodified:   three\x1b[m\r\n",
            "\r\n",
            "Untracked files:\r\n",
            "  (use \"git add <file>...\" to include)\r\n",
            "\t\x1b[31muntracked/\x1b[m\r\n",
            "\r\n",
            "no changes added to commit\r\n",
        );
        term.process(output.as_bytes());

        let mut checked_tabbed_row = false;
        for row in term.visible_rows() {
            let text: String = row
                .into_iter()
                .map(|cell| cell.map(|cell| cell.text).unwrap_or(" ".into()))
                .collect();
            if text.contains("modified:") || text.contains("untracked/") {
                checked_tabbed_row = true;
                assert_eq!(
                    &text[..8],
                    "        ",
                    "a tab-indented git status row retained old cells: {text:?}"
                );
            }
        }
        assert!(
            checked_tabbed_row,
            "test output must leave a tab-indented status row visible"
        );
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

    #[test]
    fn synchronized_output_flushes_after_timeout() {
        let mut term = AlacrittyTermState::new(3, 20, 2000);

        assert!(!term.process(b"\x1b[?2026hhello"));
        assert_eq!(first_row_text(&term), "");
        std::thread::sleep(std::time::Duration::from_millis(160));
        assert!(term.flush_sync_if_timed_out());
        assert_eq!(first_row_text(&term), "hello");
    }

    #[test]
    fn synchronized_output_waits_for_display_deadline() {
        let mut term = AlacrittyTermState::new(3, 20, 2000);

        assert!(!term.process(b"\x1b[?2026hhello"));
        assert_eq!(first_row_text(&term), "");
        assert!(!term.flush_sync_for_display());
        std::thread::sleep(std::time::Duration::from_millis(160));
        assert!(term.flush_sync_for_display());
        assert_eq!(first_row_text(&term), "hello");
    }

    #[test]
    fn synchronized_output_cpr_flush_applies_pending_bytes() {
        let mut term = AlacrittyTermState::new(3, 20, 2000);

        assert!(!term.process(b"\x1b[?2026hhello"));
        assert!(!term.flush_sync_for_display());
        assert!(term.flush_sync_before_cpr());
        assert_eq!(first_row_text(&term), "hello");
    }

    #[test]
    fn cpr_finishes_forced_large_synchronized_replay() {
        let mut term = AlacrittyTermState::new(3, 20, 2000);
        let mut output = b"\x1b[?2026h".to_vec();
        output.resize(output.len() + PROCESS_CHUNK_BYTES * 2, b'x');

        assert!(!term.process(&output));
        assert!(term.sync_update_active());
        assert!(term.flush_sync_before_cpr());
        assert!(!term.sync_update_active());
        assert!(first_row_text(&term).contains('x'));
    }
}
