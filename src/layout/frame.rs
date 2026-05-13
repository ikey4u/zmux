use std::{fmt::Write as FmtWrite, sync::atomic::Ordering};

use alacritty_terminal::{term::cell::Flags, vte::ansi::Color};

use crate::{
    copy_mode::CopyRenderRow,
    layout::rect::BORDER_SIZE,
    terminal::{color_name, AlacrittyTermState},
    types::{LayoutNode, Pane, Rect, SplitDirection, Window},
};

pub fn serialize_frame(win: &Window, area: Rect, hide_borders: bool) -> String {
    let mut out = String::with_capacity(65536);
    out.push_str("{\"type\":\"frame\",\"layout\":");
    if let Some(zoom) = &win.zoom_state {
        let zoomed_id = zoom.zoomed_pane_id;
        if let Some(pane) = crate::layout::find_pane_by_id(&win.root, zoomed_id)
        {
            write_leaf(pane, true, &mut out);
        } else {
            write_node(
                &win.root,
                &win.active_pane_path,
                &mut out,
                area,
                hide_borders,
            );
        }
    } else {
        write_node(
            &win.root,
            &win.active_pane_path,
            &mut out,
            area,
            hide_borders,
        );
    }
    out.push('}');
    out
}

fn write_node(
    node: &LayoutNode,
    active_path: &[usize],
    out: &mut String,
    area: Rect,
    hide_borders: bool,
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
                let child_area = child_rect(
                    area,
                    direction,
                    sizes,
                    children.len(),
                    i,
                    hide_borders,
                );
                write_child_node(
                    child,
                    child_inner_path,
                    child_active,
                    out,
                    child_area,
                    hide_borders,
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
    hide_borders: bool,
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
                let child_area = child_rect(
                    area,
                    direction,
                    sizes,
                    children.len(),
                    i,
                    hide_borders,
                );
                write_child_node(
                    child,
                    child_rel,
                    child_is_active,
                    out,
                    child_area,
                    hide_borders,
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
    hide_borders: bool,
) -> Rect {
    let border_size: u16 = if hide_borders { 0 } else { BORDER_SIZE };
    let total_dim = match direction {
        SplitDirection::Horizontal => area.width,
        SplitDirection::Vertical => area.height,
    };
    let borders = (count.saturating_sub(1)) as u16 * border_size;
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
        offset += dim + border_size;
    }
    area
}

fn write_leaf(pane: &Pane, is_active: bool, out: &mut String) {
    let cs = pane.cursor_shape.load(Ordering::Relaxed);
    if let Some(copy_view) = crate::copy_mode::render_view(pane) {
        let hide_cursor = false;
        let _ = write!(
            out,
            "{{\"type\":\"leaf\",\"id\":{},\"rows\":{},\"cols\":{},\
             \"cursor_row\":{},\"cursor_col\":{},\
             \"hide_cursor\":{},\"alternate_screen\":false,\
             \"mouse_mode\":0,\"in_copy_mode\":true,\
             \"cursor_shape\":{},\"active\":{},",
            pane.id,
            pane.last_rows,
            pane.last_cols,
            copy_view.cursor_row,
            copy_view.cursor_col,
            hide_cursor,
            cs,
            is_active,
        );
        if let Some(ratio) = copy_view.scroll_ratio {
            let _ = write!(out, "\"scroll_ratio\":{:.4},", ratio);
        }
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
    let (cr, cc) = parser.cursor_position();
    let hide_cursor = parser.hide_cursor();
    let alt = parser.alternate_screen();
    let mouse_mode = parser.mouse_mode();

    let _ = write!(
        out,
        "{{\"type\":\"leaf\",\"id\":{},\"rows\":{},\"cols\":{},\
         \"cursor_row\":{},\"cursor_col\":{},\
         \"hide_cursor\":{},\"alternate_screen\":{},\
         \"mouse_mode\":{},\
         \"cursor_shape\":{},\"active\":{},",
        pane.id,
        pane.last_rows,
        pane.last_cols,
        cr,
        cc,
        hide_cursor,
        alt,
        mouse_mode,
        cs,
        is_active,
    );

    if !pane.title.is_empty() {
        out.push_str("\"title\":\"");
        json_escape(&pane.title, out);
        out.push_str("\",");
    }

    out.push_str("\"rows_v2\":");
    write_rows_v2(&parser, pane.last_rows, pane.last_cols, out);
    out.push('}');
}

struct Run {
    text: String,
    fg: Color,
    bg: Color,
    flags: u8,
    width: u16,
}

fn write_rows_v2(
    term: &AlacrittyTermState,
    rows: u16,
    cols: u16,
    out: &mut String,
) {
    const FLAG_DIM: u8 = 1;
    const FLAG_BOLD: u8 = 2;
    const FLAG_ITALIC: u8 = 4;
    const FLAG_UNDERLINE: u8 = 8;
    const FLAG_INVERSE: u8 = 16;

    let visible_rows = term.visible_rows();
    let mut logical_line = 0usize;
    let mut logical_col = 0usize;
    out.push('[');
    for r in 0..rows {
        if r > 0 {
            out.push(',');
        }
        out.push_str("{\"runs\":[");

        let row_start_col = logical_col;
        let mut row_wrapped = false;
        let mut runs: Vec<Run> = Vec::new();
        let mut c = 0u16;

        while c < cols {
            let (text, fg, bg, flags, w) = if let Some(cell) = visible_rows
                .get(r as usize)
                .and_then(|cells| cells.get(c as usize))
                .and_then(|cell| cell.as_ref())
            {
                row_wrapped |= cell.flags.contains(Flags::WRAPLINE);
                let mut fl = 0u8;
                if cell.flags.contains(Flags::DIM) {
                    fl |= FLAG_DIM;
                }
                if cell.flags.contains(Flags::BOLD) {
                    fl |= FLAG_BOLD;
                }
                if cell.flags.contains(Flags::ITALIC) {
                    fl |= FLAG_ITALIC;
                }
                if cell.flags.intersects(Flags::ALL_UNDERLINES) {
                    fl |= FLAG_UNDERLINE;
                }
                if cell.flags.contains(Flags::INVERSE) {
                    fl |= FLAG_INVERSE;
                }
                (cell.text.clone(), cell.fg, cell.bg, fl, cell.width)
            } else {
                (
                    " ".to_string(),
                    Color::Named(
                        alacritty_terminal::vte::ansi::NamedColor::Foreground,
                    ),
                    Color::Named(
                        alacritty_terminal::vte::ansi::NamedColor::Background,
                    ),
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

        let row_end_col = row_start_col + cols as usize;
        let _ = write!(
            out,
            "],\"line\":{},\"start_col\":{},\"end_col\":{}}}",
            logical_line, row_start_col, row_end_col
        );
        if row_wrapped {
            logical_col = row_end_col;
        } else {
            logical_line += 1;
            logical_col = 0;
        }
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
        out.push(']');
        if let Some(line) = row.line {
            let _ = write!(
                out,
                ",\"line\":{},\"start_col\":{},\"end_col\":{}",
                line, row.start_col, row.end_col
            );
        }
        out.push('}');
    }
    out.push(']');
}

fn push_color(c: Color, out: &mut String) {
    out.push_str(&color_name(c));
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
    use serde_json::Value;

    use super::*;

    #[test]
    fn terminal_rows_tag_soft_wraps_as_same_logical_line() {
        let rows = serialized_rows_for(b"abcdef\nXYZ", 4, 5);

        assert_eq!(rows[0]["line"], Value::from(0));
        assert_eq!(rows[1]["line"], Value::from(0));
        assert_eq!(rows[2]["line"], Value::from(1));
    }

    #[test]
    fn terminal_rows_keep_hard_newlines_as_separate_logical_lines() {
        let rows = serialized_rows_for(b"abc\ndef", 4, 5);

        assert_eq!(rows[0]["line"], Value::from(0));
        assert_eq!(rows[1]["line"], Value::from(1));
    }

    #[test]
    fn terminal_rows_tag_wide_char_soft_wraps_as_same_logical_line() {
        let rows = serialized_rows_for("abc中x".as_bytes(), 3, 5);

        assert_eq!(rows[0]["line"], Value::from(0));
        assert_eq!(rows[1]["line"], Value::from(0));
    }

    fn serialized_rows_for(input: &[u8], rows: u16, cols: u16) -> Vec<Value> {
        let mut term = AlacrittyTermState::new(rows, cols, 100);
        term.process(input);
        let mut out = String::new();
        write_rows_v2(&term, rows, cols, &mut out);
        serde_json::from_str(&out).unwrap()
    }
}
