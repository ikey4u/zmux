use std::{fmt::Write as FmtWrite, sync::atomic::Ordering};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    copy_mode::{
        build_wrapped_snapshot, CopyRenderRow, CopyRenderRun, CopyRenderView,
    },
    types::{
        LayoutNode, Pane, PaneTextSnapshot, Rect, SplitDirection, Window,
        WrappedRow, WrappedSnapshot,
    },
};

pub fn serialize_frame(win: &Window, area: Rect) -> String {
    let mut out = String::with_capacity(65536);
    out.push_str("{\"type\":\"frame\",\"layout\":");
    if let Some(zoom) = &win.zoom_state {
        let zoomed_id = zoom.zoomed_pane_id;
        if let Some(pane) = crate::layout::find_pane_by_id(&win.root, zoomed_id)
        {
            write_leaf(pane, true, &mut out);
        } else {
            write_node(&win.root, &win.active_pane_path, &mut out, area);
        }
    } else {
        write_node(&win.root, &win.active_pane_path, &mut out, area);
    }
    out.push('}');
    out
}

fn write_node(
    node: &LayoutNode,
    active_path: &[usize],
    out: &mut String,
    area: Rect,
) {
    match node {
        LayoutNode::Split {
            direction,
            sizes,
            children,
        } => {
            let dir_str = match direction {
                SplitDirection::Horizontal => "horizontal",
                SplitDirection::Vertical => "vertical",
            };
            let _ = write!(
                out,
                "{{\"type\":\"split\",\"direction\":\"{}\",\"sizes\":[",
                dir_str
            );
            for (i, s) in sizes.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                let _ = write!(out, "{}", s);
            }
            out.push_str("],\"children\":[");
            for (i, child) in children.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                let child_active = active_path.first() == Some(&i);
                let child_inner_path = if active_path.first() == Some(&i) {
                    &active_path[1..]
                } else {
                    &[]
                };
                let child_area =
                    child_rect(area, direction, sizes, children.len(), i);
                write_child_node(
                    child,
                    child_inner_path,
                    child_active,
                    out,
                    child_area,
                );
            }
            out.push_str("]}");
        }
        LayoutNode::Leaf(p) => {
            write_leaf(p, true, out);
        }
    }
}

fn write_child_node(
    node: &LayoutNode,
    relative_path: &[usize],
    is_active_branch: bool,
    out: &mut String,
    area: Rect,
) {
    match node {
        LayoutNode::Split {
            direction,
            sizes,
            children,
        } => {
            let dir_str = match direction {
                SplitDirection::Horizontal => "horizontal",
                SplitDirection::Vertical => "vertical",
            };
            let _ = write!(
                out,
                "{{\"type\":\"split\",\"direction\":\"{}\",\"sizes\":[",
                dir_str
            );
            for (i, s) in sizes.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                let _ = write!(out, "{}", s);
            }
            out.push_str("],\"children\":[");
            for (i, child) in children.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                let child_is_active =
                    is_active_branch && relative_path.first() == Some(&i);
                let child_rel = if relative_path.first() == Some(&i) {
                    &relative_path[1..]
                } else {
                    &[]
                };
                let child_area =
                    child_rect(area, direction, sizes, children.len(), i);
                write_child_node(
                    child,
                    child_rel,
                    child_is_active,
                    out,
                    child_area,
                );
            }
            out.push_str("]}");
        }
        LayoutNode::Leaf(p) => {
            let is_active = is_active_branch && relative_path.is_empty();
            write_leaf(p, is_active, out);
        }
    }
}

fn child_rect(
    area: Rect,
    direction: &SplitDirection,
    sizes: &[u16],
    count: usize,
    index: usize,
) -> Rect {
    use crate::layout::rect::BORDER_SIZE;
    let total_dim = match direction {
        SplitDirection::Horizontal => area.width,
        SplitDirection::Vertical => area.height,
    };
    let borders = (count.saturating_sub(1)) as u16 * BORDER_SIZE;
    let available = total_dim.saturating_sub(borders);
    let total_pct: u16 = sizes.iter().copied().sum::<u16>().max(1);
    let mut offset = 0u16;
    for (i, &pct) in sizes.iter().enumerate().take(count) {
        let dim = if i == count - 1 {
            available.saturating_sub(offset)
        } else {
            (available as u32 * pct as u32 / total_pct as u32) as u16
        };
        if i == index {
            return match direction {
                SplitDirection::Horizontal => {
                    Rect::new(area.x + offset, area.y, dim, area.height)
                }
                SplitDirection::Vertical => {
                    Rect::new(area.x, area.y + offset, area.width, dim)
                }
            };
        }
        offset += dim + BORDER_SIZE;
    }
    area
}

fn write_leaf(pane: &Pane, is_active: bool, out: &mut String) {
    let cs = pane.cursor_shape.load(Ordering::Relaxed);
    if let Some(copy_view) = crate::copy_mode::render_view(pane) {
        let _ = write!(
            out,
            "{{\"type\":\"leaf\",\"id\":{},\"rows\":{},\"cols\":{},\
             \"cursor_row\":{},\"cursor_col\":{},\
             \"hide_cursor\":false,\"alternate_screen\":false,\
             \"cursor_shape\":{},\"active\":{},",
            pane.id,
            pane.last_rows,
            pane.last_cols,
            copy_view.cursor_row,
            copy_view.cursor_col,
            cs,
            is_active,
        );
        if !pane.title.is_empty() {
            out.push_str("\"title\":\"");
            json_escape(&pane.title, out);
            out.push_str("\",");
        }
        out.push_str("\"rows_v2\":");
        write_copy_rows(&copy_view.rows, out);
        out.push('}');
        return;
    }
    let Ok(parser) = pane.parser.lock() else {
        let _ = write!(
            out,
            "{{\"type\":\"leaf\",\"id\":{},\"rows\":{},\"cols\":{},\
             \"cursor_row\":0,\"cursor_col\":0,\"active\":{},\
             \"cursor_shape\":{},\"rows_v2\":[]}}",
            pane.id, pane.last_rows, pane.last_cols, is_active, cs
        );
        return;
    };
    let screen = parser.screen();
    let (cr, cc) = screen.cursor_position();
    let hide_cursor = screen.hide_cursor();
    let alt = screen.alternate_screen();

    if !alt {
        if let Some(reflow_view) = logical_reflow_view(pane, screen) {
            let _ = write!(
                out,
                "{{\"type\":\"leaf\",\"id\":{},\"rows\":{},\"cols\":{},\
                 \"cursor_row\":{},\"cursor_col\":{},\
                 \"hide_cursor\":{},\"alternate_screen\":false,\
                 \"cursor_shape\":{},\"active\":{},",
                pane.id,
                pane.last_rows,
                pane.last_cols,
                reflow_view.cursor_row,
                reflow_view.cursor_col,
                hide_cursor,
                cs,
                is_active,
            );
            if !pane.title.is_empty() {
                out.push_str("\"title\":\"");
                json_escape(&pane.title, out);
                out.push_str("\",");
            }
            out.push_str("\"rows_v2\":");
            write_copy_rows(&reflow_view.rows, out);
            out.push('}');
            return;
        }
    }

    let _ = write!(
        out,
        "{{\"type\":\"leaf\",\"id\":{},\"rows\":{},\"cols\":{},\
         \"cursor_row\":{},\"cursor_col\":{},\
         \"hide_cursor\":{},\"alternate_screen\":{},\
         \"cursor_shape\":{},\"active\":{},",
        pane.id,
        pane.last_rows,
        pane.last_cols,
        cr,
        cc,
        hide_cursor,
        alt,
        cs,
        is_active,
    );

    if !pane.title.is_empty() {
        out.push_str("\"title\":\"");
        json_escape(&pane.title, out);
        out.push_str("\",");
    }

    out.push_str("\"rows_v2\":");
    write_rows_v2(screen, pane.last_rows, pane.last_cols, out);
    out.push('}');
}

fn logical_reflow_view(
    pane: &Pane,
    screen: &vt100::Screen,
) -> Option<CopyRenderView> {
    let buffer = pane.text_buffer.lock().ok()?;
    if !can_use_logical_reflow(
        screen,
        pane.last_rows,
        pane.last_cols,
        buffer.reflow_enabled(),
    ) {
        return None;
    }
    let snapshot = buffer.snapshot();
    Some(build_logical_reflow_view(
        &snapshot,
        pane.last_rows as usize,
        pane.last_cols as usize,
    ))
}

fn can_use_logical_reflow(
    screen: &vt100::Screen,
    rows: u16,
    cols: u16,
    reflow_enabled: bool,
) -> bool {
    let (cursor_row, _) = screen.cursor_position();
    reflow_enabled
        && !screen.alternate_screen()
        && !screen_has_non_default_formatting(screen, rows, cols, cursor_row)
}

fn build_logical_reflow_view(
    snapshot: &PaneTextSnapshot,
    height: usize,
    width: usize,
) -> CopyRenderView {
    let width = width.max(1);
    let height = height.max(1);
    let wrapped = build_wrapped_snapshot(snapshot, width);
    let (cursor_row_idx, cursor_col) =
        logical_cursor_display_position(snapshot, &wrapped);
    let scroll_top = wrapped.rows.len().saturating_sub(height);
    let mut rows = Vec::with_capacity(height);
    for visible_row in 0..height {
        let absolute_row = scroll_top + visible_row;
        rows.push(render_logical_row(wrapped.rows.get(absolute_row)));
    }
    CopyRenderView {
        rows,
        cursor_row: cursor_row_idx.saturating_sub(scroll_top) as u16,
        cursor_col: cursor_col as u16,
    }
}

fn logical_cursor_display_position(
    snapshot: &PaneTextSnapshot,
    wrapped: &WrappedSnapshot,
) -> (usize, usize) {
    if snapshot.cursor_line >= wrapped.line_ranges.len() {
        return (wrapped.rows.len().saturating_sub(1), 0);
    }
    let (start, end) = wrapped.line_ranges[snapshot.cursor_line];
    let mut row_idx = start;
    for idx in start..end {
        let row = &wrapped.rows[idx];
        if snapshot.cursor_col <= row.end_col || idx + 1 == end {
            row_idx = idx;
            break;
        }
    }
    let row = &wrapped.rows[row_idx];
    let col = display_width_between(
        &snapshot.lines[row.line].text,
        row.start_col,
        snapshot.cursor_col.min(row.end_col),
    );
    (row_idx, col)
}

fn render_logical_row(row: Option<&WrappedRow>) -> CopyRenderRow {
    let Some(row) = row else {
        return CopyRenderRow { runs: Vec::new() };
    };
    if row.text.is_empty() {
        return CopyRenderRow { runs: Vec::new() };
    }
    CopyRenderRow {
        runs: vec![CopyRenderRun {
            text: row.text.clone(),
            fg: "default".to_string(),
            bg: "default".to_string(),
            flags: 0,
            width: UnicodeWidthStr::width(row.text.as_str())
                .min(u16::MAX as usize) as u16,
        }],
    }
}

fn screen_has_non_default_formatting(
    screen: &vt100::Screen,
    rows: u16,
    cols: u16,
    cursor_row: u16,
) -> bool {
    for row in 0..rows {
        if row == cursor_row {
            continue;
        }
        for col in 0..cols {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            if cell.fgcolor() != vt100::Color::Default
                || cell.bgcolor() != vt100::Color::Default
                || cell.dim()
                || cell.bold()
                || cell.italic()
                || cell.underline()
                || cell.inverse()
            {
                return true;
            }
        }
    }
    false
}

struct Run {
    text: String,
    fg: vt100::Color,
    bg: vt100::Color,
    flags: u8,
    width: u16,
}

fn write_rows_v2(
    screen: &vt100::Screen,
    rows: u16,
    cols: u16,
    out: &mut String,
) {
    const FLAG_DIM: u8 = 1;
    const FLAG_BOLD: u8 = 2;
    const FLAG_ITALIC: u8 = 4;
    const FLAG_UNDERLINE: u8 = 8;
    const FLAG_INVERSE: u8 = 16;

    out.push('[');
    for r in 0..rows {
        if r > 0 {
            out.push(',');
        }
        out.push_str("{\"runs\":[");

        let mut runs: Vec<Run> = Vec::new();
        let mut c = 0u16;

        while c < cols {
            let (text, fg, bg, flags, w) = if let Some(cell) = screen.cell(r, c)
            {
                let t = cell.contents();
                let t = if t.is_empty() { " " } else { t };
                let fg = cell.fgcolor();
                let bg = cell.bgcolor();
                let mut fl = 0u8;
                if cell.dim() {
                    fl |= FLAG_DIM;
                }
                if cell.bold() {
                    fl |= FLAG_BOLD;
                }
                if cell.italic() {
                    fl |= FLAG_ITALIC;
                }
                if cell.underline() {
                    fl |= FLAG_UNDERLINE;
                }
                if cell.inverse() {
                    fl |= FLAG_INVERSE;
                }
                let w = UnicodeWidthStr::width(t).max(1) as u16;
                (t.to_string(), fg, bg, fl, w)
            } else {
                (
                    " ".to_string(),
                    vt100::Color::Default,
                    vt100::Color::Default,
                    0u8,
                    1u16,
                )
            };

            if let Some(last) = runs.last_mut() {
                if last.fg == fg && last.bg == bg && last.flags == flags {
                    last.text.push_str(&text);
                    last.width += w;
                    c += w.max(1);
                    continue;
                }
            }
            runs.push(Run {
                text,
                fg,
                bg,
                flags,
                width: w,
            });
            c += w.max(1);
        }

        for (i, run) in runs.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str("{\"text\":\"");
            json_escape(&run.text, out);
            out.push_str("\",\"fg\":\"");
            push_color(run.fg, out);
            out.push_str("\",\"bg\":\"");
            push_color(run.bg, out);
            let _ = write!(
                out,
                "\",\"flags\":{},\"width\":{}}}",
                run.flags, run.width
            );
        }

        out.push_str("]}");
    }
    out.push(']');
}

fn write_copy_rows(rows: &[CopyRenderRow], out: &mut String) {
    out.push('[');
    for (row_idx, row) in rows.iter().enumerate() {
        if row_idx > 0 {
            out.push(',');
        }
        out.push_str("{\"runs\":[");
        for (run_idx, run) in row.runs.iter().enumerate() {
            if run_idx > 0 {
                out.push(',');
            }
            out.push_str("{\"text\":\"");
            json_escape(&run.text, out);
            out.push_str("\",\"fg\":\"");
            json_escape(&run.fg, out);
            out.push_str("\",\"bg\":\"");
            json_escape(&run.bg, out);
            let _ = write!(
                out,
                "\",\"flags\":{},\"width\":{}}}",
                run.flags, run.width
            );
        }
        out.push_str("]}");
    }
    out.push(']');
}

fn display_width_between(text: &str, start: usize, end: usize) -> usize {
    text.chars()
        .skip(start)
        .take(end.saturating_sub(start))
        .map(char_width)
        .sum()
}

fn char_width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(1).max(1)
}

fn push_color(c: vt100::Color, out: &mut String) {
    match c {
        vt100::Color::Default => out.push_str("default"),
        vt100::Color::Idx(i) => {
            let _ = write!(out, "idx:{}", i);
        }
        vt100::Color::Rgb(r, g, b) => {
            let _ = write!(out, "rgb:{},{},{}", r, g, b);
        }
    }
}

fn json_escape(s: &str, out: &mut String) {
    if !s.bytes().any(|b| b == b'"' || b == b'\\' || b < 0x20) {
        out.push_str(s);
        return;
    }
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_logical_reflow_view, can_use_logical_reflow,
        screen_has_non_default_formatting,
    };
    use crate::types::{PaneTextSnapshot, SnapshotLine};

    fn plain_row_text(
        snapshot: &PaneTextSnapshot,
        height: usize,
        width: usize,
    ) -> Vec<String> {
        build_logical_reflow_view(snapshot, height, width)
            .rows
            .into_iter()
            .map(|row| row.runs.into_iter().map(|run| run.text).collect())
            .collect()
    }

    #[test]
    fn logical_reflow_wraps_long_lines_to_new_width() {
        let snapshot = PaneTextSnapshot {
            lines: vec![SnapshotLine {
                text: "abcdefghij".to_string(),
                terminated: false,
            }],
            cursor_line: 0,
            cursor_col: 10,
        };
        let view = build_logical_reflow_view(&snapshot, 3, 5);
        let rows = plain_row_text(&snapshot, 3, 5);
        assert_eq!(rows, vec!["abcde", "fghij", ""]);
        assert_eq!(view.cursor_row, 1);
        assert_eq!(view.cursor_col, 5);
    }

    #[test]
    fn logical_reflow_keeps_virtual_blank_cursor_line_after_newline() {
        let snapshot = PaneTextSnapshot {
            lines: vec![SnapshotLine {
                text: "hello".to_string(),
                terminated: true,
            }],
            cursor_line: 1,
            cursor_col: 0,
        };
        let view = build_logical_reflow_view(&snapshot, 3, 10);
        let rows = plain_row_text(&snapshot, 3, 10);
        assert_eq!(rows, vec!["hello", "", ""]);
        assert_eq!(view.cursor_row, 1);
        assert_eq!(view.cursor_col, 0);
    }

    #[test]
    fn prompt_line_styling_does_not_disable_logical_reflow() {
        let mut parser = vt100::Parser::new(3, 10, 0);
        parser.process(b"\x1b[31mred");
        assert!(!screen_has_non_default_formatting(
            parser.screen(),
            3,
            10,
            0
        ));
        assert!(can_use_logical_reflow(parser.screen(), 3, 10, true));
    }

    #[test]
    fn styling_above_cursor_still_disables_logical_reflow() {
        let mut parser = vt100::Parser::new(3, 10, 0);
        parser.process(b"\x1b[31mred\n\x1b[0mplain");
        assert!(screen_has_non_default_formatting(parser.screen(), 3, 10, 1));
        assert!(!can_use_logical_reflow(parser.screen(), 3, 10, true));
    }

    #[test]
    fn filled_last_row_does_not_disable_logical_reflow() {
        let mut parser = vt100::Parser::new(2, 5, 0);
        parser.process(b"12345\n67890");
        assert!(can_use_logical_reflow(parser.screen(), 2, 5, true));
    }

    #[test]
    fn actual_alternate_screen_still_disables_logical_reflow() {
        let mut parser = vt100::Parser::new(3, 10, 0);
        parser.process(b"\x1b[?1049h");
        assert!(!can_use_logical_reflow(parser.screen(), 3, 10, true));
    }
}
