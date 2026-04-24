use unicode_width::UnicodeWidthChar;

use crate::types::{
    CopyModeState, CopyPoint, CopySnapshotSource, Pane, PaneTextSnapshot,
    SearchMatch, SelectionMode, SnapshotLine, WrappedRow, WrappedSnapshot,
};

#[derive(Clone)]
pub struct CopyRenderRun {
    pub text: String,
    pub fg: String,
    pub bg: String,
    pub flags: u8,
    pub width: u16,
}

#[derive(Clone)]
pub struct CopyRenderRow {
    pub runs: Vec<CopyRenderRun>,
}

#[derive(Clone)]
pub struct CopyRenderView {
    pub rows: Vec<CopyRenderRow>,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub scroll_ratio: Option<f32>,
}

pub fn enter(pane: &mut Pane) -> bool {
    let (snapshot_source, snapshot) = match snapshot_for_copy_mode(pane, None) {
        Some(snapshot) => snapshot,
        None => return false,
    };
    let mut state = CopyModeState::new_with_source(snapshot, snapshot_source);
    let height = pane.last_rows.max(1) as usize;
    rebuild_wrapped(&mut state, pane.last_cols as usize, height);
    state.scroll_top = state.wrapped.rows.len().saturating_sub(height);
    state.preferred_column = current_display_position(&state).1;
    pane.copy_state = Some(state);
    true
}

fn snapshot_for_copy_mode(
    pane: &Pane,
    preferred_source: Option<CopySnapshotSource>,
) -> Option<(CopySnapshotSource, PaneTextSnapshot)> {
    let buffer_snapshot = pane
        .text_buffer
        .lock()
        .ok()
        .map(|buffer| (buffer.reflow_enabled(), buffer.snapshot()));
    let mut parser = pane.parser.lock().ok();
    let alternate_screen = parser
        .as_ref()
        .map(|p| p.screen().alternate_screen())
        .unwrap_or(false);
    let parser_snapshot = if alternate_screen {
        parser.as_mut().map(|p| snapshot_from_parser(p))
    } else {
        snapshot_from_output_ring(pane)
            .or_else(|| parser.as_mut().map(|p| snapshot_from_parser(p)))
    };
    select_snapshot_for_copy_mode(
        buffer_snapshot,
        parser_snapshot,
        alternate_screen,
        preferred_source,
    )
}

fn snapshot_from_output_ring(pane: &Pane) -> Option<PaneTextSnapshot> {
    let ring_data: Vec<u8> = pane
        .output_ring
        .lock()
        .ok()
        .map(|ring| ring.iter().copied().collect())?;
    if ring_data.is_empty() {
        return None;
    }
    let wide_cols = 500u16;
    let rows = pane.last_rows.max(24);
    let mut tmp_parser = vt100::Parser::new(rows, wide_cols, 2000);
    tmp_parser.process(&ring_data);
    Some(snapshot_from_parser(&mut tmp_parser))
}

fn select_snapshot_for_copy_mode(
    buffer_snapshot: Option<(bool, PaneTextSnapshot)>,
    parser_snapshot: Option<PaneTextSnapshot>,
    alternate_screen: bool,
    _preferred_source: Option<CopySnapshotSource>,
) -> Option<(CopySnapshotSource, PaneTextSnapshot)> {
    if alternate_screen {
        return parser_snapshot
            .map(|snapshot| (CopySnapshotSource::Parser, snapshot))
            .or_else(|| {
                buffer_snapshot
                    .map(|(_, snapshot)| (CopySnapshotSource::Buffer, snapshot))
            });
    }
    if let Some(snapshot) = parser_snapshot {
        return Some((CopySnapshotSource::Parser, snapshot));
    }
    buffer_snapshot.map(|(_, snapshot)| (CopySnapshotSource::Buffer, snapshot))
}

fn snapshot_from_parser(parser: &mut vt100::Parser) -> PaneTextSnapshot {
    let screen = parser.screen_mut();
    let original_scrollback = screen.scrollback();
    screen.set_scrollback(usize::MAX);
    let max_scrollback = screen.scrollback();
    screen.set_scrollback(0);
    let (rows, cols) = screen.size();
    let rows = rows.max(1);
    let cols = cols.max(1);
    let (cursor_row, cursor_col) = screen.cursor_position();
    let cursor_physical_row = max_scrollback + cursor_row as usize;
    let mut physical_rows = Vec::with_capacity(max_scrollback + rows as usize);

    for offset in (1..=max_scrollback).rev() {
        screen.set_scrollback(offset);
        if let Some(row) = snapshot_visible_row(screen, 0, cols, 0) {
            physical_rows.push(row);
        }
    }

    screen.set_scrollback(0);
    for row_idx in 0..rows {
        let min_width = if row_idx == cursor_row {
            cursor_col as usize
        } else {
            0
        };
        if let Some(row) =
            snapshot_visible_row(screen, row_idx, cols, min_width)
        {
            physical_rows.push(row);
        }
    }
    trim_viewport_blank_tail(&mut physical_rows, cursor_physical_row);

    screen.set_scrollback(original_scrollback);
    snapshot_from_physical_rows(
        &physical_rows,
        cursor_physical_row,
        cursor_col as usize,
    )
}

fn trim_viewport_blank_tail(
    rows: &mut Vec<ScreenSnapshotRow>,
    cursor_physical_row: usize,
) {
    while rows.len() > cursor_physical_row + 1 {
        let Some(last_row) = rows.last() else {
            break;
        };
        if last_row.wrapped || !last_row.text.is_empty() {
            break;
        }
        rows.pop();
    }
}

#[derive(Clone)]
struct ScreenSnapshotRow {
    text: String,
    wrapped: bool,
}

fn snapshot_visible_row(
    screen: &vt100::Screen,
    row_idx: u16,
    cols: u16,
    min_width: usize,
) -> Option<ScreenSnapshotRow> {
    let row_text = screen.rows(0, cols).nth(row_idx as usize)?;
    Some(ScreenSnapshotRow {
        text: trim_screen_row(&row_text, min_width),
        wrapped: screen.row_wrapped(row_idx),
    })
}

fn snapshot_from_physical_rows(
    rows: &[ScreenSnapshotRow],
    cursor_row: usize,
    cursor_col: usize,
) -> PaneTextSnapshot {
    let mut lines = Vec::new();
    let mut logical_line = String::new();
    let mut cursor_line = 0usize;
    let mut cursor_col_chars = 0usize;
    let mut cursor_mapped = false;

    for (row_idx, row) in rows.iter().enumerate() {
        if row_idx == cursor_row {
            cursor_line = lines.len();
            cursor_col_chars = logical_line.chars().count()
                + char_index_for_display_col(&row.text, cursor_col);
            cursor_mapped = true;
        }
        logical_line.push_str(&row.text);
        if !row.wrapped {
            lines.push(SnapshotLine {
                text: std::mem::take(&mut logical_line),
                terminated: row_idx + 1 < rows.len(),
            });
        }
    }

    if !logical_line.is_empty() || lines.is_empty() {
        if !cursor_mapped {
            cursor_line = lines.len();
            cursor_col_chars = logical_line.chars().count();
        }
        lines.push(SnapshotLine {
            text: logical_line,
            terminated: false,
        });
    }

    PaneTextSnapshot {
        lines,
        cursor_line,
        cursor_col: cursor_col_chars,
    }
}

fn trim_screen_row(row: &str, min_width: usize) -> String {
    let mut chars: Vec<char> = row.chars().collect();
    let mut width: usize = chars.iter().map(|ch| char_width(*ch)).sum();
    while matches!(chars.last(), Some(' ')) && width > min_width {
        chars.pop();
        width = width.saturating_sub(1);
    }
    chars.into_iter().collect()
}

fn char_index_for_display_col(text: &str, target_width: usize) -> usize {
    let mut used = 0usize;
    let mut idx = 0usize;
    for ch in text.chars() {
        let ch_width = char_width(ch);
        if used + ch_width > target_width {
            break;
        }
        used += ch_width;
        idx += 1;
    }
    idx
}

pub fn exit(pane: &mut Pane) {
    pane.copy_state = None;
}

pub fn refresh_layout(pane: &mut Pane) {
    let Some(preferred_source) =
        pane.copy_state.as_ref().map(|state| state.snapshot_source)
    else {
        return;
    };
    let width = pane.last_cols.max(1) as usize;
    let height = pane.last_rows.max(1) as usize;
    let snapshot = snapshot_for_copy_mode(pane, Some(preferred_source));
    let Some(state) = pane.copy_state.as_mut() else {
        return;
    };
    if let Some((snapshot_source, snapshot)) = snapshot {
        state.snapshot_source = snapshot_source;
        refresh_snapshot(state, snapshot, width, height);
    } else {
        rebuild_wrapped(state, width, height);
        ensure_cursor_visible(state, height);
    }
}

pub fn move_left(pane: &mut Pane) {
    with_state(pane, |state, width, height| {
        if state.cursor.col > 0 {
            state.cursor.col -= 1;
        } else if state.cursor.line > 0 {
            state.cursor.line -= 1;
            state.cursor.col =
                line_char_len(&state.snapshot, state.cursor.line);
        }
        rebuild_wrapped(state, width, height);
        state.preferred_column = current_display_position(state).1;
        ensure_cursor_visible(state, height);
    });
}

pub fn move_right(pane: &mut Pane) {
    with_state(pane, |state, width, height| {
        let line_len = line_char_len(&state.snapshot, state.cursor.line);
        if state.cursor.col < line_len {
            state.cursor.col += 1;
        } else if state.cursor.line + 1 < state.snapshot.lines.len() {
            state.cursor.line += 1;
            state.cursor.col = 0;
        }
        rebuild_wrapped(state, width, height);
        state.preferred_column = current_display_position(state).1;
        ensure_cursor_visible(state, height);
    });
}

pub fn move_up(pane: &mut Pane) {
    with_state(pane, |state, width, height| {
        rebuild_wrapped(state, width, height);
        let (row_idx, _) = current_display_position(state);
        if row_idx > 0 {
            state.cursor = point_from_display_position(
                state,
                row_idx - 1,
                state.preferred_column,
            );
        }
        ensure_cursor_visible(state, height);
    });
}

pub fn move_down(pane: &mut Pane) {
    with_state(pane, |state, width, height| {
        rebuild_wrapped(state, width, height);
        let (row_idx, _) = current_display_position(state);
        if row_idx + 1 < state.wrapped.rows.len() {
            state.cursor = point_from_display_position(
                state,
                row_idx + 1,
                state.preferred_column,
            );
        }
        ensure_cursor_visible(state, height);
    });
}

pub fn page_up(pane: &mut Pane) {
    with_state(pane, |state, width, height| {
        rebuild_wrapped(state, width, height);
        let (row_idx, _) = current_display_position(state);
        let target = row_idx.saturating_sub(height.max(1));
        state.cursor =
            point_from_display_position(state, target, state.preferred_column);
        ensure_cursor_visible(state, height);
    });
}

pub fn page_down(pane: &mut Pane) {
    with_state(pane, |state, width, height| {
        rebuild_wrapped(state, width, height);
        let (row_idx, _) = current_display_position(state);
        let max_row = state.wrapped.rows.len().saturating_sub(1);
        let target = (row_idx + height.max(1)).min(max_row);
        state.cursor =
            point_from_display_position(state, target, state.preferred_column);
        ensure_cursor_visible(state, height);
    });
}

pub fn scroll_up(pane: &mut Pane, lines: usize) -> bool {
    if pane.copy_state.is_none() {
        if !enter(pane) {
            return false;
        }
    }
    let width = pane.last_cols.max(1) as usize;
    let height = pane.last_rows.max(1) as usize;
    let Some(state) = pane.copy_state.as_mut() else {
        return false;
    };
    rebuild_wrapped(state, width, height);
    state.scroll_top = state.scroll_top.saturating_sub(lines);
    true
}

pub fn scroll_down(pane: &mut Pane, lines: usize) -> bool {
    if pane.copy_state.is_none() {
        return false;
    }
    let width = pane.last_cols.max(1) as usize;
    let height = pane.last_rows.max(1) as usize;
    let Some(state) = pane.copy_state.as_mut() else {
        return false;
    };
    rebuild_wrapped(state, width, height);
    let max_scroll = state.wrapped.rows.len().saturating_sub(height);
    state.scroll_top = (state.scroll_top + lines).min(max_scroll);
    if state.scroll_top >= max_scroll {
        exit(pane);
        return false;
    }
    true
}

pub fn scroll_ratio(pane: &Pane) -> Option<f32> {
    let state = pane.copy_state.as_ref()?;
    let height = pane.last_rows.max(1) as usize;
    let total = state.wrapped.rows.len();
    if total <= height {
        return None;
    }
    let max_scroll = total - height;
    Some(state.scroll_top as f32 / max_scroll as f32)
}

pub fn move_to_top(pane: &mut Pane) {
    with_state(pane, |state, width, height| {
        state.cursor = CopyPoint { line: 0, col: 0 };
        rebuild_wrapped(state, width, height);
        state.scroll_top = 0;
        state.preferred_column = 0;
        ensure_cursor_visible(state, height);
    });
}

pub fn move_to_bottom(pane: &mut Pane) {
    with_state(pane, |state, width, height| {
        let last_line = state.snapshot.lines.len().saturating_sub(1);
        state.cursor = CopyPoint {
            line: last_line,
            col: line_char_len(&state.snapshot, last_line),
        };
        rebuild_wrapped(state, width, height);
        state.preferred_column = current_display_position(state).1;
        ensure_cursor_visible(state, height);
    });
}

pub fn move_to_line_start(pane: &mut Pane) {
    with_state(pane, |state, width, height| {
        state.cursor.col = 0;
        rebuild_wrapped(state, width, height);
        state.preferred_column = 0;
        ensure_cursor_visible(state, height);
    });
}

pub fn move_to_line_end(pane: &mut Pane) {
    with_state(pane, |state, width, height| {
        state.cursor.col = line_char_len(&state.snapshot, state.cursor.line);
        rebuild_wrapped(state, width, height);
        state.preferred_column = current_display_position(state).1;
        ensure_cursor_visible(state, height);
    });
}

pub fn move_word_backward(pane: &mut Pane) {
    with_state(pane, |state, width, height| {
        state.cursor = find_prev_word_start(&state.snapshot, state.cursor)
            .unwrap_or(CopyPoint { line: 0, col: 0 });
        rebuild_wrapped(state, width, height);
        state.preferred_column = current_display_position(state).1;
        ensure_cursor_visible(state, height);
    });
}

pub fn move_word_forward(pane: &mut Pane) {
    with_state(pane, |state, width, height| {
        state.cursor = find_next_word_start(&state.snapshot, state.cursor)
            .unwrap_or_else(|| last_cursor_point(&state.snapshot));
        rebuild_wrapped(state, width, height);
        state.preferred_column = current_display_position(state).1;
        ensure_cursor_visible(state, height);
    });
}

pub fn move_word_end(pane: &mut Pane) {
    with_state(pane, |state, width, height| {
        state.cursor = find_word_end(&state.snapshot, state.cursor)
            .unwrap_or(state.cursor);
        rebuild_wrapped(state, width, height);
        state.preferred_column = current_display_position(state).1;
        ensure_cursor_visible(state, height);
    });
}

pub fn start_selection(pane: &mut Pane, mode: SelectionMode) {
    with_state(pane, |state, width, height| {
        state.anchor = Some(state.cursor);
        state.selection_mode = mode;
        rebuild_wrapped(state, width, height);
        ensure_cursor_visible(state, height);
    });
}

pub fn clear_selection(pane: &mut Pane) {
    if let Some(state) = pane.copy_state.as_mut() {
        state.anchor = None;
    }
}

pub fn search(pane: &mut Pane, query: String, forward: bool) -> bool {
    let width = pane.last_cols.max(1) as usize;
    let height = pane.last_rows.max(1) as usize;
    let Some(state) = pane.copy_state.as_mut() else {
        return false;
    };
    state.search_query = query;
    state.search_forward = forward;
    state.search_matches =
        build_search_matches(&state.snapshot, &state.search_query);
    if state.search_matches.is_empty() {
        state.search_idx = 0;
        return false;
    }
    state.search_idx =
        find_match_index(&state.search_matches, state.cursor, forward);
    jump_to_match(state, width, height);
    true
}

pub fn search_next(pane: &mut Pane) -> bool {
    step_search(pane, true)
}

pub fn search_prev(pane: &mut Pane) -> bool {
    step_search(pane, false)
}

pub fn yank_selection(pane: &mut Pane) -> String {
    let Some(state) = pane.copy_state.as_ref() else {
        return String::new();
    };
    selection_text(state)
}

pub fn render_view(pane: &Pane) -> Option<CopyRenderView> {
    let state = pane.copy_state.as_ref()?;
    let height = pane.last_rows.max(1) as usize;
    let width = pane.last_cols.max(1) as usize;
    if state.wrapped.width != width {
        return None;
    }
    let (cursor_row_idx, cursor_col) = current_display_position(state);
    let mut rows = Vec::with_capacity(height);
    let active_match = state.search_matches.get(state.search_idx);
    for visible_row in 0..height {
        let absolute_row = state.scroll_top + visible_row;
        let row = state.wrapped.rows.get(absolute_row);
        rows.push(render_row(state, row, active_match));
    }
    let total_rows = state.wrapped.rows.len();
    let scroll_ratio = if total_rows > height {
        let max_scroll = total_rows - height;
        Some(state.scroll_top as f32 / max_scroll as f32)
    } else {
        None
    };
    Some(CopyRenderView {
        rows,
        cursor_row: cursor_row_idx.saturating_sub(state.scroll_top) as u16,
        cursor_col: cursor_col as u16,
        scroll_ratio,
    })
}

fn with_state(
    pane: &mut Pane,
    f: impl FnOnce(&mut CopyModeState, usize, usize),
) {
    let width = pane.last_cols.max(1) as usize;
    let height = pane.last_rows.max(1) as usize;
    let Some(state) = pane.copy_state.as_mut() else {
        return;
    };
    rebuild_wrapped(state, width, height);
    f(state, width, height);
}

fn jump_to_match(state: &mut CopyModeState, width: usize, height: usize) {
    rebuild_wrapped(state, width, height);
    if let Some(target) = state.search_matches.get(state.search_idx) {
        state.cursor = CopyPoint {
            line: target.line,
            col: target.start,
        };
        state.preferred_column = current_display_position(state).1;
        ensure_cursor_visible(state, height);
    }
}

fn step_search(pane: &mut Pane, forward: bool) -> bool {
    let width = pane.last_cols.max(1) as usize;
    let height = pane.last_rows.max(1) as usize;
    let Some(state) = pane.copy_state.as_mut() else {
        return false;
    };
    if state.search_matches.is_empty() {
        return false;
    }
    if forward {
        if state.search_forward {
            state.search_idx =
                (state.search_idx + 1) % state.search_matches.len();
        } else {
            state.search_idx = if state.search_idx == 0 {
                state.search_matches.len() - 1
            } else {
                state.search_idx - 1
            };
        }
    } else if state.search_forward {
        state.search_idx = if state.search_idx == 0 {
            state.search_matches.len() - 1
        } else {
            state.search_idx - 1
        };
    } else {
        state.search_idx = (state.search_idx + 1) % state.search_matches.len();
    }
    jump_to_match(state, width, height);
    true
}

fn refresh_snapshot(
    state: &mut CopyModeState,
    snapshot: PaneTextSnapshot,
    width: usize,
    height: usize,
) {
    let follow_bottom = state.anchor.is_none()
        && state.cursor == last_cursor_point(&state.snapshot);
    let preserved_cursor = state.cursor;
    let preserved_anchor = state.anchor;
    let preserved_scroll_top = state.scroll_top;
    state.snapshot = snapshot;
    state.cursor = if follow_bottom {
        last_cursor_point(&state.snapshot)
    } else {
        clamp_point(&state.snapshot, preserved_cursor)
    };
    state.anchor =
        preserved_anchor.map(|anchor| clamp_point(&state.snapshot, anchor));
    state.scroll_top = preserved_scroll_top;
    if state.search_query.is_empty() {
        state.search_matches.clear();
        state.search_idx = 0;
    } else {
        state.search_matches =
            build_search_matches(&state.snapshot, &state.search_query);
        state.search_idx = if state.search_matches.is_empty() {
            0
        } else {
            find_match_index(
                &state.search_matches,
                state.cursor,
                state.search_forward,
            )
        };
    }
    rebuild_wrapped(state, width, height);
    if follow_bottom {
        state.scroll_top =
            state.wrapped.rows.len().saturating_sub(height.max(1));
    }
    state.preferred_column = current_display_position(state).1;
    ensure_cursor_visible(state, height);
}

fn rebuild_wrapped(state: &mut CopyModeState, width: usize, height: usize) {
    let width = width.max(1);
    let height = height.max(1);
    if state.wrapped.width != width || state.wrapped.rows.is_empty() {
        state.wrapped = build_wrapped_snapshot(&state.snapshot, width);
    }
    clamp_cursor(state);
    let max_scroll_top = state.wrapped.rows.len().saturating_sub(height);
    state.scroll_top = state.scroll_top.min(max_scroll_top);
}

pub(crate) fn build_wrapped_snapshot(
    snapshot: &PaneTextSnapshot,
    width: usize,
) -> WrappedSnapshot {
    let mut rows = Vec::new();
    let mut line_ranges = Vec::with_capacity(snapshot.lines.len());
    for (line_idx, line) in snapshot.lines.iter().enumerate() {
        let start = rows.len();
        let chars: Vec<char> = line.text.chars().collect();
        if chars.is_empty() {
            rows.push(WrappedRow {
                line: line_idx,
                start_col: 0,
                end_col: 0,
                text: String::new(),
            });
        } else {
            let mut col = 0usize;
            while col < chars.len() {
                let row_start = col;
                let mut used = 0usize;
                while col < chars.len() {
                    let ch_width = char_width(chars[col]);
                    if col > row_start && used + ch_width > width {
                        break;
                    }
                    used += ch_width;
                    col += 1;
                    if used >= width {
                        break;
                    }
                }
                if col == row_start {
                    col += 1;
                }
                rows.push(WrappedRow {
                    line: line_idx,
                    start_col: row_start,
                    end_col: col,
                    text: chars[row_start..col].iter().collect(),
                });
            }
        }
        let end = rows.len();
        line_ranges.push((start, end.max(start + 1)));
    }
    if snapshot.cursor_line == snapshot.lines.len() {
        rows.push(WrappedRow {
            line: snapshot.cursor_line,
            start_col: 0,
            end_col: 0,
            text: String::new(),
        });
    }
    if rows.is_empty() {
        rows.push(WrappedRow {
            line: 0,
            start_col: 0,
            end_col: 0,
            text: String::new(),
        });
        line_ranges.push((0, 1));
    }
    WrappedSnapshot {
        width,
        rows,
        line_ranges,
    }
}

fn clamp_cursor(state: &mut CopyModeState) {
    state.cursor = clamp_point(&state.snapshot, state.cursor);
}

fn clamp_point(snapshot: &PaneTextSnapshot, point: CopyPoint) -> CopyPoint {
    if snapshot.lines.is_empty() {
        return CopyPoint { line: 0, col: 0 };
    }
    let line = point.line.min(snapshot.lines.len() - 1);
    CopyPoint {
        line,
        col: point.col.min(line_char_len(snapshot, line)),
    }
}

fn ensure_cursor_visible(state: &mut CopyModeState, height: usize) {
    let (row, _) = current_display_position(state);
    if row < state.scroll_top {
        state.scroll_top = row;
    } else if row >= state.scroll_top + height.max(1) {
        state.scroll_top = row + 1 - height.max(1);
    }
}

fn current_display_position(state: &CopyModeState) -> (usize, usize) {
    point_display_position(state, state.cursor)
}

fn point_display_position(
    state: &CopyModeState,
    point: CopyPoint,
) -> (usize, usize) {
    let line = point
        .line
        .min(state.wrapped.line_ranges.len().saturating_sub(1));
    let (start, end) = state.wrapped.line_ranges[line];
    let mut row_idx = start;
    for idx in start..end {
        let row = &state.wrapped.rows[idx];
        if point.col <= row.end_col || idx + 1 == end {
            row_idx = idx;
            break;
        }
    }
    let row = &state.wrapped.rows[row_idx];
    let col = display_width_between(
        &state.snapshot.lines[row.line],
        row.start_col,
        point.col.min(row.end_col),
    );
    (row_idx, col)
}

fn point_from_display_position(
    state: &CopyModeState,
    row_idx: usize,
    target_col: usize,
) -> CopyPoint {
    let row_idx = row_idx.min(state.wrapped.rows.len().saturating_sub(1));
    let row = &state.wrapped.rows[row_idx];
    let line = &state.snapshot.lines[row.line];
    let mut col = row.start_col;
    let mut used = 0usize;
    for ch in line
        .text
        .chars()
        .skip(row.start_col)
        .take(row.end_col.saturating_sub(row.start_col))
    {
        let ch_width = char_width(ch);
        if used + ch_width > target_col {
            break;
        }
        used += ch_width;
        col += 1;
    }
    CopyPoint {
        line: row.line,
        col,
    }
}

fn last_cursor_point(snapshot: &PaneTextSnapshot) -> CopyPoint {
    let line = snapshot.lines.len().saturating_sub(1);
    CopyPoint {
        line,
        col: line_char_len(snapshot, line),
    }
}

fn char_at(snapshot: &PaneTextSnapshot, point: CopyPoint) -> Option<char> {
    snapshot.lines.get(point.line)?.text.chars().nth(point.col)
}

fn char_class_at(
    snapshot: &PaneTextSnapshot,
    point: CopyPoint,
) -> Option<WordClass> {
    Some(classify_char(char_at(snapshot, point)?))
}

fn next_char_after(
    snapshot: &PaneTextSnapshot,
    point: CopyPoint,
) -> Option<CopyPoint> {
    if point.line >= snapshot.lines.len() {
        return None;
    }
    let line_len = line_char_len(snapshot, point.line);
    if point.col + 1 < line_len {
        return Some(CopyPoint {
            line: point.line,
            col: point.col + 1,
        });
    }
    let mut line = point.line + 1;
    while line < snapshot.lines.len() {
        if line_char_len(snapshot, line) > 0 {
            return Some(CopyPoint { line, col: 0 });
        }
        line += 1;
    }
    None
}

fn prev_char_before(
    snapshot: &PaneTextSnapshot,
    point: CopyPoint,
) -> Option<CopyPoint> {
    if point.line >= snapshot.lines.len() {
        return None;
    }
    if point.col > 0 {
        return Some(CopyPoint {
            line: point.line,
            col: point.col - 1,
        });
    }
    let mut line = point.line;
    while line > 0 {
        line -= 1;
        let line_len = line_char_len(snapshot, line);
        if line_len > 0 {
            return Some(CopyPoint {
                line,
                col: line_len - 1,
            });
        }
    }
    None
}

fn first_char_at_or_after(
    snapshot: &PaneTextSnapshot,
    point: CopyPoint,
) -> Option<CopyPoint> {
    if char_at(snapshot, point).is_some() {
        Some(point)
    } else {
        next_char_after(snapshot, point)
    }
}

fn skip_forward_non_space(
    snapshot: &PaneTextSnapshot,
    mut point: CopyPoint,
) -> Option<CopyPoint> {
    loop {
        if char_class_at(snapshot, point)? != WordClass::Space {
            return Some(point);
        }
        point = next_char_after(snapshot, point)?;
    }
}

fn is_token_start(snapshot: &PaneTextSnapshot, point: CopyPoint) -> bool {
    let Some(class) = char_class_at(snapshot, point) else {
        return false;
    };
    prev_char_before(snapshot, point)
        .and_then(|prev| char_class_at(snapshot, prev))
        != Some(class)
}

fn is_token_end(snapshot: &PaneTextSnapshot, point: CopyPoint) -> bool {
    let Some(class) = char_class_at(snapshot, point) else {
        return false;
    };
    next_char_after(snapshot, point)
        .and_then(|next| char_class_at(snapshot, next))
        != Some(class)
}

fn find_next_word_start(
    snapshot: &PaneTextSnapshot,
    cursor: CopyPoint,
) -> Option<CopyPoint> {
    let mut point = first_char_at_or_after(snapshot, cursor)?;
    let class = char_class_at(snapshot, point)?;
    if class == WordClass::Space {
        return skip_forward_non_space(snapshot, point);
    }
    while let Some(next) = next_char_after(snapshot, point) {
        let next_class = char_class_at(snapshot, next)?;
        if next.line != point.line {
            return Some(next);
        }
        point = next;
        if next_class == WordClass::Space {
            return skip_forward_non_space(snapshot, point);
        }
        if next_class != class {
            return Some(point);
        }
    }
    None
}

fn find_prev_word_start(
    snapshot: &PaneTextSnapshot,
    cursor: CopyPoint,
) -> Option<CopyPoint> {
    let mut point = match char_class_at(snapshot, cursor) {
        Some(WordClass::Space) => cursor,
        Some(_) if is_token_start(snapshot, cursor) => {
            prev_char_before(snapshot, cursor)?
        }
        Some(_) => cursor,
        None => prev_char_before(snapshot, cursor)?,
    };
    while char_class_at(snapshot, point)? == WordClass::Space {
        point = prev_char_before(snapshot, point)?;
    }
    let class = char_class_at(snapshot, point)?;
    while let Some(prev) = prev_char_before(snapshot, point) {
        if char_class_at(snapshot, prev) != Some(class) {
            break;
        }
        point = prev;
    }
    Some(point)
}

fn find_word_end(
    snapshot: &PaneTextSnapshot,
    cursor: CopyPoint,
) -> Option<CopyPoint> {
    let mut point = match char_class_at(snapshot, cursor) {
        Some(WordClass::Space) => skip_forward_non_space(snapshot, cursor)?,
        Some(_) if is_token_end(snapshot, cursor) => {
            let next = next_char_after(snapshot, cursor)?;
            skip_forward_non_space(snapshot, next)?
        }
        Some(_) => cursor,
        None => {
            let next = next_char_after(snapshot, cursor)?;
            skip_forward_non_space(snapshot, next)?
        }
    };
    let class = char_class_at(snapshot, point)?;
    while let Some(next) = next_char_after(snapshot, point) {
        if next.line != point.line
            || char_class_at(snapshot, next) != Some(class)
        {
            break;
        }
        point = next;
    }
    Some(point)
}

fn build_search_matches(
    snapshot: &PaneTextSnapshot,
    query: &str,
) -> Vec<SearchMatch> {
    if query.is_empty() {
        return Vec::new();
    }
    let mut matches = Vec::new();
    for (line_idx, line) in snapshot.lines.iter().enumerate() {
        let mut offset = 0usize;
        while offset <= line.text.len() {
            let slice = &line.text[offset..];
            let Some(found) = slice.find(query) else {
                break;
            };
            let start_byte = offset + found;
            let end_byte = start_byte + query.len();
            matches.push(SearchMatch {
                line: line_idx,
                start: char_index_at_byte(&line.text, start_byte),
                end: char_index_at_byte(&line.text, end_byte),
            });
            offset = end_byte.max(start_byte + 1);
        }
    }
    matches
}

fn find_match_index(
    matches: &[SearchMatch],
    cursor: CopyPoint,
    forward: bool,
) -> usize {
    if forward {
        matches
            .iter()
            .position(|m| {
                m.line > cursor.line
                    || (m.line == cursor.line && m.start >= cursor.col)
            })
            .unwrap_or(0)
    } else {
        matches
            .iter()
            .rposition(|m| {
                m.line < cursor.line
                    || (m.line == cursor.line && m.start <= cursor.col)
            })
            .unwrap_or(matches.len().saturating_sub(1))
    }
}

fn selection_text(state: &CopyModeState) -> String {
    let Some(anchor) = state.anchor else {
        return String::new();
    };
    match state.selection_mode {
        SelectionMode::Char => {
            yank_char_selection(&state.snapshot, anchor, state.cursor)
        }
        SelectionMode::Line => {
            yank_line_selection(&state.snapshot, anchor.line, state.cursor.line)
        }
        SelectionMode::Rect => {
            yank_rect_selection(&state.snapshot, anchor, state.cursor)
        }
    }
}

fn yank_char_selection(
    snapshot: &PaneTextSnapshot,
    a: CopyPoint,
    b: CopyPoint,
) -> String {
    let (start, end) = ordered_points(a, b);
    if start == end {
        return String::new();
    }
    let mut out = String::new();
    for line_idx in start.line..=end.line {
        let line = &snapshot.lines[line_idx].text;
        if line_idx == start.line && line_idx == end.line {
            out.push_str(&slice_chars(line, start.col, end.col));
        } else if line_idx == start.line {
            out.push_str(&slice_chars(line, start.col, line.chars().count()));
            out.push('\n');
        } else if line_idx == end.line {
            out.push_str(&slice_chars(line, 0, end.col));
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn yank_line_selection(
    snapshot: &PaneTextSnapshot,
    a: usize,
    b: usize,
) -> String {
    let start = a.min(b);
    let end = a.max(b);
    let mut out = String::new();
    for line_idx in start..=end {
        out.push_str(&snapshot.lines[line_idx].text);
        if line_idx < end {
            out.push('\n');
        }
    }
    out
}

fn yank_rect_selection(
    snapshot: &PaneTextSnapshot,
    a: CopyPoint,
    b: CopyPoint,
) -> String {
    let start_line = a.line.min(b.line);
    let end_line = a.line.max(b.line);
    let start_col = a.col.min(b.col);
    let end_col = a.col.max(b.col);
    let mut out = String::new();
    for line_idx in start_line..=end_line {
        out.push_str(&slice_chars(
            &snapshot.lines[line_idx].text,
            start_col,
            end_col,
        ));
        if line_idx < end_line {
            out.push('\n');
        }
    }
    out
}

fn render_row(
    state: &CopyModeState,
    row: Option<&WrappedRow>,
    active_match: Option<&SearchMatch>,
) -> CopyRenderRow {
    let Some(row) = row else {
        return CopyRenderRow { runs: Vec::new() };
    };
    if row.text.is_empty() {
        return CopyRenderRow { runs: Vec::new() };
    }
    let selected = selection_range_for_line(state, row.line);
    let matches: Vec<(usize, usize)> = state
        .search_matches
        .iter()
        .filter(|m| m.line == row.line)
        .map(|m| (m.start, m.end))
        .collect();
    let active = active_match
        .filter(|m| m.line == row.line)
        .map(|m| (m.start, m.end));
    let mut runs = Vec::new();
    let mut current_style = StyleKind::Plain;
    let mut current_text = String::new();
    let mut current_width = 0usize;
    for (offset, ch) in row.text.chars().enumerate() {
        let col = row.start_col + offset;
        let style = style_for_col(col, selected, active, &matches);
        if !current_text.is_empty() && style != current_style {
            runs.push(style_run(
                current_style,
                std::mem::take(&mut current_text),
                current_width,
            ));
            current_width = 0;
        }
        current_style = style;
        current_text.push(ch);
        current_width += char_width(ch);
    }
    if !current_text.is_empty() {
        runs.push(style_run(current_style, current_text, current_width));
    }
    CopyRenderRow { runs }
}

fn style_for_col(
    col: usize,
    selected: Option<(usize, usize)>,
    active: Option<(usize, usize)>,
    matches: &[(usize, usize)],
) -> StyleKind {
    if selected
        .is_some_and(|(start, end)| start < end && col >= start && col < end)
    {
        return StyleKind::Selected;
    }
    if active
        .is_some_and(|(start, end)| start < end && col >= start && col < end)
    {
        return StyleKind::ActiveSearch;
    }
    if matches
        .iter()
        .any(|(start, end)| *start < *end && col >= *start && col < *end)
    {
        return StyleKind::Search;
    }
    StyleKind::Plain
}

fn selection_range_for_line(
    state: &CopyModeState,
    line_idx: usize,
) -> Option<(usize, usize)> {
    let anchor = state.anchor?;
    match state.selection_mode {
        SelectionMode::Char => {
            let (start, end) = ordered_points(anchor, state.cursor);
            if start == end || line_idx < start.line || line_idx > end.line {
                return None;
            }
            if start.line == end.line {
                Some((start.col, end.col))
            } else if line_idx == start.line {
                Some((start.col, line_char_len(&state.snapshot, line_idx)))
            } else if line_idx == end.line {
                Some((0, end.col))
            } else {
                Some((0, line_char_len(&state.snapshot, line_idx)))
            }
        }
        SelectionMode::Line => {
            let start = anchor.line.min(state.cursor.line);
            let end = anchor.line.max(state.cursor.line);
            if line_idx < start || line_idx > end {
                None
            } else {
                Some((0, line_char_len(&state.snapshot, line_idx)))
            }
        }
        SelectionMode::Rect => {
            let start_line = anchor.line.min(state.cursor.line);
            let end_line = anchor.line.max(state.cursor.line);
            if line_idx < start_line || line_idx > end_line {
                None
            } else {
                Some((
                    anchor.col.min(state.cursor.col),
                    anchor.col.max(state.cursor.col),
                ))
            }
        }
    }
}

fn ordered_points(a: CopyPoint, b: CopyPoint) -> (CopyPoint, CopyPoint) {
    if (a.line, a.col) <= (b.line, b.col) {
        (a, b)
    } else {
        (b, a)
    }
}

fn style_run(style: StyleKind, text: String, width: usize) -> CopyRenderRun {
    let (fg, bg, flags) = match style {
        StyleKind::Plain => ("default", "default", 0),
        StyleKind::Search => ("black", "yellow", 0),
        StyleKind::ActiveSearch => ("white", "blue", 2),
        StyleKind::Selected => ("black", "cyan", 2),
    };
    CopyRenderRun {
        text,
        fg: fg.to_string(),
        bg: bg.to_string(),
        flags,
        width: width.min(u16::MAX as usize) as u16,
    }
}

fn line_char_len(snapshot: &PaneTextSnapshot, line_idx: usize) -> usize {
    snapshot
        .lines
        .get(line_idx)
        .map(|line| line.text.chars().count())
        .unwrap_or(0)
}

fn display_width_between(
    line: &SnapshotLine,
    start: usize,
    end: usize,
) -> usize {
    line.text
        .chars()
        .skip(start)
        .take(end.saturating_sub(start))
        .map(char_width)
        .sum()
}

fn slice_chars(text: &str, start: usize, end: usize) -> String {
    text.chars()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect()
}

fn char_index_at_byte(text: &str, byte_idx: usize) -> usize {
    text[..byte_idx].chars().count()
}

fn char_width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(1)
}

fn classify_char(ch: char) -> WordClass {
    if ch.is_whitespace() {
        WordClass::Space
    } else if ch == '_' || ch.is_alphanumeric() {
        WordClass::Keyword
    } else {
        WordClass::Punct
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WordClass {
    Space,
    Keyword,
    Punct,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StyleKind {
    Plain,
    Search,
    ActiveSearch,
    Selected,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::history::{PaneTextSnapshot, SnapshotLine};

    fn snapshot(lines: &[&str]) -> PaneTextSnapshot {
        PaneTextSnapshot {
            lines: lines
                .iter()
                .map(|line| SnapshotLine {
                    text: (*line).to_string(),
                    terminated: true,
                })
                .collect(),
            cursor_line: lines.len().saturating_sub(1),
            cursor_col: lines
                .last()
                .map(|line| line.chars().count())
                .unwrap_or(0),
        }
    }

    #[test]
    fn next_word_start_skips_spaces_and_blank_lines() {
        let snapshot = snapshot(&["alpha  beta", "/usr/bin", "", "tail"]);
        assert_eq!(
            find_next_word_start(&snapshot, CopyPoint { line: 0, col: 2 }),
            Some(CopyPoint { line: 0, col: 7 })
        );
        assert_eq!(
            find_next_word_start(&snapshot, CopyPoint { line: 1, col: 7 }),
            Some(CopyPoint { line: 3, col: 0 })
        );
    }

    #[test]
    fn prev_word_start_matches_vim_style_backtracking() {
        let snapshot = snapshot(&["alpha  beta", "/usr/bin", "", "tail"]);
        assert_eq!(
            find_prev_word_start(&snapshot, CopyPoint { line: 0, col: 3 }),
            Some(CopyPoint { line: 0, col: 0 })
        );
        assert_eq!(
            find_prev_word_start(&snapshot, CopyPoint { line: 0, col: 7 }),
            Some(CopyPoint { line: 0, col: 0 })
        );
        assert_eq!(
            find_prev_word_start(&snapshot, CopyPoint { line: 3, col: 0 }),
            Some(CopyPoint { line: 1, col: 5 })
        );
    }

    #[test]
    fn word_end_handles_punctuation_and_whitespace() {
        let snapshot = snapshot(&["alpha  beta", "/usr/bin", "", "tail"]);
        assert_eq!(
            find_word_end(&snapshot, CopyPoint { line: 0, col: 1 }),
            Some(CopyPoint { line: 0, col: 4 })
        );
        assert_eq!(
            find_word_end(&snapshot, CopyPoint { line: 0, col: 5 }),
            Some(CopyPoint { line: 0, col: 10 })
        );
        assert_eq!(
            find_word_end(&snapshot, CopyPoint { line: 1, col: 0 }),
            Some(CopyPoint { line: 1, col: 3 })
        );
    }

    #[test]
    fn parser_snapshot_tracks_wrapped_prompt_cursor() {
        let mut parser = vt100::Parser::new(3, 5, 0);
        parser.process(b"hello world");
        let snapshot = snapshot_from_parser(&mut parser);
        assert_eq!(snapshot.lines.len(), 1);
        assert_eq!(snapshot.lines[0].text, "hello world");
        assert_eq!(snapshot.cursor_line, 0);
        assert_eq!(snapshot.cursor_col, 11);
    }

    #[test]
    fn parser_snapshot_preserves_scrollback_history() {
        let mut parser = vt100::Parser::new(2, 8, 10);
        parser.process(b"older\r\nprompt\r\n$");
        let snapshot = snapshot_from_parser(&mut parser);
        assert_eq!(snapshot.lines.len(), 3);
        assert_eq!(snapshot.lines[0].text, "older");
        assert_eq!(snapshot.lines[1].text, "prompt");
        assert_eq!(snapshot.lines[2].text, "$");
        assert_eq!(snapshot.cursor_line, 2);
        assert_eq!(snapshot.cursor_col, 1);
    }

    #[test]
    fn parser_snapshot_trims_blank_tail_below_cursor() {
        let mut parser = vt100::Parser::new(5, 20, 10);
        parser.process(b"prompt\r\n$");
        let snapshot = snapshot_from_parser(&mut parser);
        assert_eq!(snapshot.lines.len(), 2);
        assert_eq!(snapshot.lines[0].text, "prompt");
        assert_eq!(snapshot.lines[1].text, "$");
        assert_eq!(snapshot.cursor_line, 1);
        assert_eq!(snapshot.cursor_col, 1);
    }

    #[test]
    fn parser_height_shrink_can_leave_only_first_visible_rows() {
        let mut parser = vt100::Parser::new(60, 80, 2000);
        let mut output = String::new();
        for i in 0..=50 {
            output.push_str(&i.to_string());
            output.push_str("\r\n");
        }
        parser.process(output.as_bytes());
        parser.screen_mut().set_size(26, 80);
        let snapshot = snapshot_from_parser(&mut parser);
        assert_eq!(snapshot.lines.len(), 26);
        assert_eq!(snapshot.lines[0].text, "0");
        assert_eq!(snapshot.lines[25].text, "25");
    }

    #[test]
    fn copy_mode_prefers_parser_snapshot_on_normal_screen() {
        let parser_snapshot = PaneTextSnapshot {
            lines: vec![
                SnapshotLine {
                    text: "older".to_string(),
                    terminated: true,
                },
                SnapshotLine {
                    text: "prompt".to_string(),
                    terminated: true,
                },
                SnapshotLine {
                    text: "$".to_string(),
                    terminated: false,
                },
            ],
            cursor_line: 2,
            cursor_col: 1,
        };
        let buffer_snapshot = PaneTextSnapshot {
            lines: vec![
                SnapshotLine {
                    text: "older".to_string(),
                    terminated: true,
                },
                SnapshotLine {
                    text: "prompt".to_string(),
                    terminated: true,
                },
                SnapshotLine {
                    text: "$".to_string(),
                    terminated: true,
                },
                SnapshotLine {
                    text: "prompt".to_string(),
                    terminated: true,
                },
                SnapshotLine {
                    text: "$".to_string(),
                    terminated: false,
                },
            ],
            cursor_line: 4,
            cursor_col: 1,
        };
        let (source, snapshot) = select_snapshot_for_copy_mode(
            Some((true, buffer_snapshot)),
            Some(parser_snapshot),
            false,
            None,
        )
        .unwrap();
        assert_eq!(source, CopySnapshotSource::Parser);
        assert_eq!(snapshot.lines.len(), 3);
        assert_eq!(snapshot.lines[0].text, "older");
        assert_eq!(snapshot.lines[1].text, "prompt");
        assert_eq!(snapshot.lines[2].text, "$");
    }

    #[test]
    fn copy_mode_falls_back_to_buffer_when_parser_unavailable() {
        let buffer_snapshot = PaneTextSnapshot {
            lines: (0..=50)
                .map(|i| SnapshotLine {
                    text: i.to_string(),
                    terminated: i != 50,
                })
                .collect(),
            cursor_line: 50,
            cursor_col: 2,
        };
        let (source, snapshot) = select_snapshot_for_copy_mode(
            Some((false, buffer_snapshot)),
            None,
            false,
            None,
        )
        .unwrap();
        assert_eq!(source, CopySnapshotSource::Buffer);
        assert_eq!(snapshot.lines.len(), 51);
    }

    #[test]
    fn rebuild_wrapped_clamps_scroll_top_to_last_visible_page() {
        let mut state = CopyModeState::new(snapshot(&[
            "0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11",
        ]));
        rebuild_wrapped(&mut state, 10, 5);
        state.scroll_top = state.wrapped.rows.len().saturating_sub(5);
        rebuild_wrapped(&mut state, 10, 8);
        assert_eq!(
            state.scroll_top,
            state.wrapped.rows.len().saturating_sub(8)
        );
    }

    #[test]
    fn refresh_snapshot_keeps_following_bottom_when_snapshot_grows() {
        let mut state = CopyModeState::new(snapshot(&["older", "prompt", "$"]));
        rebuild_wrapped(&mut state, 10, 2);
        state.scroll_top = state.wrapped.rows.len().saturating_sub(2);
        refresh_snapshot(
            &mut state,
            snapshot(&["older", "prompt", "$", "next", "tail"]),
            10,
            2,
        );
        assert_eq!(state.cursor, CopyPoint { line: 4, col: 4 });
        assert_eq!(state.snapshot.lines[4].text, "tail");
        assert_eq!(
            state.scroll_top,
            state.wrapped.rows.len().saturating_sub(2)
        );
    }

    #[test]
    fn refresh_snapshot_preserves_cursor_when_not_following_bottom() {
        let mut state = CopyModeState::new(snapshot(&["0", "1", "2", "3"]));
        rebuild_wrapped(&mut state, 10, 2);
        state.cursor = CopyPoint { line: 1, col: 0 };
        state.scroll_top = 1;
        refresh_snapshot(
            &mut state,
            snapshot(&["0", "1", "2", "3", "4", "5"]),
            10,
            2,
        );
        assert_eq!(state.cursor, CopyPoint { line: 1, col: 0 });
        assert_eq!(state.scroll_top, 1);
    }
}
