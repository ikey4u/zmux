//! Server-side ANSI rendering for pane content (Zellij-style direct terminal output).

mod styles;

use std::{
    fmt::Write as FmtWrite,
    hash::{Hash, Hasher},
    sync::atomic::Ordering,
};

use alacritty_terminal::vte::ansi::{Color, NamedColor};
pub use styles::{
    adjust_styles_for_custom_bg_fg, color_to_ansi, color_to_ansi_or_reset,
    pane_default_ansi, row_trailing_bg, vte_cup, vte_goto, write_style_diff,
    AnsiCode, CharacterStyles, DEFAULT_STYLES, RESET_STYLES,
};

pub use crate::terminal::OutputBuffer;
use crate::{
    copy_mode::CopyRenderRow,
    layout::BORDER_SIZE,
    terminal::{color_is_default, TerminalCell},
    types::{LayoutNode, Pane, Rect, SplitDirection, Window},
};

#[derive(Clone, Copy, Debug, Default)]
pub struct FrameAnsiOptions {
    /// Destructively clear the whole pane area before painting. Needed only when the
    /// layout geometry changed (panes added/removed/resized, active pane switched),
    /// since otherwise per-pane repaints fully overwrite their own cells. Doing this
    /// every frame causes a full-screen blank flash (flicker).
    pub clear_display: bool,
    /// Repaint every pane regardless of its dirty flag. Used for attach, resize,
    /// and overlay restoration where the client needs a complete snapshot without
    /// relying on incremental dirty state.
    pub force_repaint: bool,
}

fn border_styles(active: bool) -> CharacterStyles {
    CharacterStyles {
        foreground: Some(if active {
            AnsiCode::Named(NamedColor::Green)
        } else {
            AnsiCode::Named(NamedColor::BrightBlack)
        }),
        background: Some(AnsiCode::Reset),
        ..DEFAULT_STYLES
    }
}

fn write_border(area: Rect, active: bool, out: &mut String) {
    if area.width < 2 || area.height < 2 {
        return;
    }
    let left = area.x;
    let top = area.y;
    let right = area.x + area.width - 1;
    let bottom = area.y + area.height - 1;
    let style = border_styles(active);
    // Stream each edge from a single CUP. Per-cell cups used to race the
    // visible cursor around the frame and looked like white flecks.
    vte_goto(left, top, out);
    let _ = write!(out, "{style}┌");
    for _ in (left + 1)..right {
        let _ = write!(out, "{style}─");
    }
    let _ = write!(out, "{style}┐");
    // `┐` often sits on the physical last column. Park the cursor so a host
    // with autowrap still on cannot wrap it onto the next row.
    vte_cup(left, top, out);
    vte_goto(left, bottom, out);
    let _ = write!(out, "{style}└");
    for _ in (left + 1)..right {
        let _ = write!(out, "{style}─");
    }
    let _ = write!(out, "{style}┘");
    vte_cup(left, bottom, out);
    write_vertical_borders(area, active, out);
}

/// Restore left/right `│` columns. Incremental dirty paints used to skip
/// borders entirely, so a glyph the host draws wider than the grid (or a
/// last-column wrap) left stray cells just past the right edge forever.
fn write_vertical_borders(area: Rect, active: bool, out: &mut String) {
    if area.width < 2 || area.height < 2 {
        return;
    }
    let left = area.x;
    let right = area.x + area.width - 1;
    let top = area.y;
    let bottom = area.y + area.height - 1;
    let style = border_styles(active);
    for y in (top + 1)..bottom {
        vte_goto(left, y, out);
        let _ = write!(out, "{style}│");
        vte_goto(right, y, out);
        let _ = write!(out, "{style}│");
        vte_cup(left, y, out);
    }
}

fn write_pane_borders(
    area: Rect,
    is_active: bool,
    has_border: bool,
    redraw_full: bool,
    out: &mut String,
) {
    if !has_border {
        return;
    }
    if redraw_full {
        write_border(area, is_active, out);
    } else {
        write_vertical_borders(area, is_active, out);
    }
}

fn content_area(area: Rect, has_border: bool) -> Rect {
    if has_border && area.width > 2 && area.height > 2 {
        Rect::new(area.x + 1, area.y + 1, area.width - 2, area.height - 2)
    } else {
        area
    }
}

/// Erase one pane's content rectangle (not borders/gaps) before repainting a row.
/// Must not use `\x1b[K` — that clears to the physical line end and wipes pane
/// right borders and split gaps on the same row.
pub fn write_erase_rect(area: Rect, out: &mut String) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let pad = adjust_styles_for_custom_bg_fg(DEFAULT_STYLES, None, None);
    for row in 0..area.height {
        vte_goto(area.x, area.y + row, out);
        let mut current = DEFAULT_STYLES;
        write_style_diff(&mut current, pad, out);
        for _ in 0..area.width {
            out.push(' ');
        }
    }
}

fn pane_default_codes(
    pane_default_fg: Option<(u8, u8, u8)>,
    pane_default_bg: Option<(u8, u8, u8)>,
) -> (Option<AnsiCode>, Option<AnsiCode>) {
    (
        pane_default_fg.map(pane_default_ansi),
        pane_default_bg.map(pane_default_ansi),
    )
}

/// Last column with drawable content: non-space text or styled cells (e.g. EL `\x1b[K`
/// green fill). Mirrors Zellij `dump_screen_with_ansi` drawable extent.
fn row_styled_end(cells: &[Option<TerminalCell>], cols: u16) -> Option<u16> {
    for col in (0..cols.min(cells.len() as u16)).rev() {
        let Some(cell) = cells.get(col as usize).and_then(|c| c.as_ref())
        else {
            continue;
        };
        let is_space = cell.text.chars().all(|c| c == ' ');
        let styled = !color_is_default(cell.bg)
            || !color_is_default(cell.fg)
            || !cell.flags.is_empty();
        if !is_space || styled {
            return Some(col.saturating_add(cell.width.saturating_sub(1)));
        }
    }
    None
}

fn row_has_non_space_content(
    cells: &[Option<TerminalCell>],
    cols: u16,
) -> bool {
    cells.iter().take(cols as usize).any(|cell| {
        cell.as_ref()
            .is_some_and(|c| !c.text.chars().all(|ch| ch == ' '))
    })
}

/// How far across the row to paint. Diff/highlight lines extend colored backgrounds to
/// EOL; blank rows do not (Zellij `extract_characters_from_row` + compact storage).
fn row_paint_end(cells: &[Option<TerminalCell>], cols: u16) -> Option<u16> {
    let styled_end = row_styled_end(cells, cols)?;
    let trailing_bg = row_trailing_bg(cells);
    if row_has_non_space_content(cells, cols) && trailing_bg != AnsiCode::Reset
    {
        Some(cols.saturating_sub(1))
    } else {
        Some(styled_end)
    }
}

fn reset_pad_styles() -> CharacterStyles {
    CharacterStyles {
        foreground: Some(AnsiCode::Reset),
        background: Some(AnsiCode::Reset),
        bold: Some(AnsiCode::Reset),
        dim: Some(AnsiCode::Reset),
        italic: Some(AnsiCode::Reset),
        underline: Some(AnsiCode::Reset),
        reverse: Some(AnsiCode::Reset),
    }
}

fn pad_styles_for_active_bg(active_bg: AnsiCode) -> CharacterStyles {
    CharacterStyles {
        foreground: Some(AnsiCode::Reset),
        background: Some(active_bg),
        bold: Some(AnsiCode::Reset),
        dim: Some(AnsiCode::Reset),
        italic: Some(AnsiCode::Reset),
        underline: Some(AnsiCode::Reset),
        reverse: Some(AnsiCode::Reset),
    }
}

/// Width used to walk the alacritty grid. Always trust `cell.width` — overstating
/// it (e.g. forcing Nerd Font PUA to 2) skips the next grid cell and shears
/// columnar output like `ls`.
fn grid_advance_width(cell_width: u16) -> u16 {
    cell_width.max(1)
}

/// Conservative display width for edge clipping only. May be wider than the grid
/// cell when Unicode/PUA disagrees; never used to skip grid columns.
fn glyph_clip_width(ch: char, cell_width: u16) -> u16 {
    let unicode_w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
    let mut w = unicode_w.max(cell_width as usize).max(1);
    if w == 1 && is_ambiguous_double_width(ch) {
        w = 2;
    }
    w as u16
}

fn is_ambiguous_double_width(ch: char) -> bool {
    matches!(
        ch as u32,
        0xE000..=0xF8FF // BMP Private Use (Nerd Fonts, Powerline)
            | 0xF0000..=0xFFFFD // Supplementary PUA-A
            | 0x100000..=0x10FFFD // Supplementary PUA-B
    )
}

fn write_terminal_row(
    cells: &[Option<TerminalCell>],
    cols: u16,
    x: u16,
    y: u16,
    pane_default_fg: Option<AnsiCode>,
    pane_default_bg: Option<AnsiCode>,
    anchor_each_cell: bool,
    out: &mut String,
) {
    // Ordinary shell output is anchored at every grid column so host-side
    // width disagreement or automatic wrapping cannot carry text across a
    // pane boundary. Alternate-screen TUIs stream matching-width cells to
    // avoid turning every nvim redraw into thousands of CUP sequences; they
    // re-anchor after a glyph whose host display width may disagree with the
    // Alacritty grid (most commonly a Nerd Font PUA glyph).
    vte_goto(x, y, out);
    let mut current_styles = DEFAULT_STYLES;
    let paint_end = row_paint_end(cells, cols);
    let reset_pad = adjust_styles_for_custom_bg_fg(
        reset_pad_styles(),
        pane_default_fg,
        pane_default_bg,
    );
    let mut active_bg = AnsiCode::Reset;
    let mut col = 0u16;
    let mut resync_next_cell = false;
    while col < cols {
        if col > 0 && (anchor_each_cell || resync_next_cell) {
            vte_cup(x + col, y, out);
            resync_next_cell = false;
        }
        if paint_end.is_none_or(|end| col > end) {
            write_style_diff(&mut current_styles, reset_pad, out);
            out.push(' ');
            col += 1;
            continue;
        }
        if let Some(cell) = cells.get(col as usize).and_then(|c| c.as_ref()) {
            active_bg = color_to_ansi_or_reset(cell.bg);
            let ch = cell.text.chars().next().unwrap_or(' ');
            let advance = grid_advance_width(cell.width);
            let clip_w = glyph_clip_width(ch, cell.width);
            if col + clip_w > cols {
                let new_styles = adjust_styles_for_custom_bg_fg(
                    pad_styles_for_active_bg(active_bg),
                    pane_default_fg,
                    pane_default_bg,
                );
                write_style_diff(&mut current_styles, new_styles, out);
                out.push(' ');
                col += 1;
                continue;
            }
            let new_styles = adjust_styles_for_custom_bg_fg(
                CharacterStyles::from_cell(cell),
                pane_default_fg,
                pane_default_bg,
            );
            write_style_diff(&mut current_styles, new_styles, out);
            out.push_str(&cell.text);
            resync_next_cell = clip_w != advance;
            col = col.saturating_add(advance);
        } else {
            let new_styles = adjust_styles_for_custom_bg_fg(
                pad_styles_for_active_bg(active_bg),
                pane_default_fg,
                pane_default_bg,
            );
            write_style_diff(&mut current_styles, new_styles, out);
            out.push(' ');
            col += 1;
        }
    }
    // If the last cell sat on the physical last column, cancel wrap-pending
    // before the next row (or the pane border) is painted.
    if cols > 0 {
        vte_cup(x, y, out);
    }
}

fn write_copy_row(
    row: &CopyRenderRow,
    cols: u16,
    x: u16,
    y: u16,
    pane_default_fg: Option<AnsiCode>,
    pane_default_bg: Option<AnsiCode>,
    out: &mut String,
) {
    vte_goto(x, y, out);
    let mut current_styles = DEFAULT_STYLES;
    let mut col = 0u16;
    for run in &row.runs {
        let fg = parse_run_color(&run.fg, true);
        let bg = parse_run_color(&run.bg, false);
        let run_styles = adjust_styles_for_custom_bg_fg(
            CharacterStyles::from_copy_run(fg, bg, run.flags),
            pane_default_fg,
            pane_default_bg,
        );
        for ch in run.text.chars() {
            if col >= cols {
                break;
            }
            if col > 0 {
                vte_cup(x + col, y, out);
            }
            let w = glyph_clip_width(ch, 1);
            if col + w > cols {
                write_style_diff(&mut current_styles, run_styles, out);
                out.push(' ');
                col += 1;
                continue;
            }
            write_style_diff(&mut current_styles, run_styles, out);
            out.push(ch);
            col = col.saturating_add(w);
        }
        if col >= cols {
            break;
        }
    }
    while col < cols {
        let pad = adjust_styles_for_custom_bg_fg(
            reset_pad_styles(),
            pane_default_fg,
            pane_default_bg,
        );
        if col > 0 {
            vte_cup(x + col, y, out);
        }
        write_style_diff(&mut current_styles, pad, out);
        out.push(' ');
        col += 1;
    }
    if cols > 0 {
        vte_cup(x, y, out);
    }
}

fn parse_run_color(name: &str, fg: bool) -> Color {
    layout_color_str(name, fg)
}

fn layout_color_str(name: &str, fg: bool) -> Color {
    use ratatui::style::Color as RtColor;

    use crate::style::parse_color;
    match parse_color(name) {
        RtColor::Reset => {
            if fg {
                Color::Named(NamedColor::Foreground)
            } else {
                Color::Named(NamedColor::Background)
            }
        }
        RtColor::Black => Color::Named(NamedColor::Black),
        RtColor::Red => Color::Named(NamedColor::Red),
        RtColor::Green => Color::Named(NamedColor::Green),
        RtColor::Yellow => Color::Named(NamedColor::Yellow),
        RtColor::Blue => Color::Named(NamedColor::Blue),
        RtColor::Magenta => Color::Named(NamedColor::Magenta),
        RtColor::Cyan => Color::Named(NamedColor::Cyan),
        RtColor::White | RtColor::Gray => Color::Named(NamedColor::White),
        RtColor::DarkGray => Color::Named(NamedColor::BrightBlack),
        RtColor::LightRed => Color::Named(NamedColor::BrightRed),
        RtColor::LightGreen => Color::Named(NamedColor::BrightGreen),
        RtColor::LightYellow => Color::Named(NamedColor::BrightYellow),
        RtColor::LightBlue => Color::Named(NamedColor::BrightBlue),
        RtColor::LightMagenta => Color::Named(NamedColor::BrightMagenta),
        RtColor::LightCyan => Color::Named(NamedColor::BrightCyan),
        RtColor::Indexed(i) => Color::Indexed(i),
        RtColor::Rgb(r, g, b) => {
            Color::Spec(alacritty_terminal::vte::ansi::Rgb { r, g, b })
        }
    }
}

fn claim_render_dirty(
    dirty: &std::sync::atomic::AtomicBool,
    force_repaint: bool,
) -> bool {
    // Claim the current dirty generation before taking the parser snapshot.
    // PTY output that arrives while this paint is in progress sets the flag
    // again and must remain pending for the next render loop.
    let was_dirty = dirty.swap(false, Ordering::AcqRel);
    force_repaint || was_dirty
}

fn pane_paint_pending_with_mode(
    pane: &Pane,
    opts: &FrameAnsiOptions,
    consume_dirty: bool,
) -> bool {
    let sync_still_active =
        pane.parser.lock().ok().is_some_and(|mut parser| {
            if parser.sync_update_active() {
                parser.flush_sync_for_display();
            }
            parser.sync_update_active()
        });
    if sync_still_active {
        // Keep render_dirty set so ESU (or the timeout) paints the completed
        // frame. Incremental sync replay may already have updated the internal
        // grid, but exposing it here would reintroduce TUI flicker.
        return false;
    }
    if !consume_dirty {
        return opts.clear_display
            || opts.force_repaint
            || pane.render_dirty.load(Ordering::Acquire);
    }
    claim_render_dirty(
        &pane.render_dirty,
        opts.clear_display || opts.force_repaint,
    )
}

fn write_scrollbar(content: Rect, ratio: f32, out: &mut String) {
    let height = content.height as usize;
    if height < 3 {
        return;
    }
    let thumb_height = (height / 4).max(1);
    let track_range = height.saturating_sub(thumb_height);
    let thumb_top = (ratio.clamp(0.0, 1.0) * track_range as f32) as usize;
    let col = content.x + content.width.saturating_sub(1);
    for row in 0..height {
        let y = content.y + row as u16;
        let in_thumb = row >= thumb_top && row < thumb_top + thumb_height;
        vte_goto(col, y, out);
        if in_thumb {
            out.push_str("\x1b[37;49m┃");
        } else {
            out.push_str("\x1b[38;2;60;60;60;49m│");
        }
    }
}

fn write_pane(
    pane: &Pane,
    is_active: bool,
    area: Rect,
    hide_borders: bool,
    out: &mut String,
    opts: &FrameAnsiOptions,
    consume_dirty: bool,
) {
    if !pane_paint_pending_with_mode(pane, opts, consume_dirty) {
        return;
    }
    if area.width == 0 || area.height == 0 {
        return;
    }
    let has_border = !hide_borders && area.width > 2 && area.height > 2;
    let inner = content_area(area, has_border);
    // Paint content in place (each row covers the full inner width). Do not
    // `write_erase_rect` first — that flashes blank cells on terminals with weak
    // synchronized-update support. Borders are static on dirty typing frames;
    // only redraw them on full clear/repaint.
    let redraw_border =
        has_border && (opts.clear_display || opts.force_repaint);
    if pane.copy_state.is_some() {
        let pane_defaults = pane
            .parser
            .lock()
            .ok()
            .map(|parser| {
                pane_default_codes(
                    parser.pane_default_fg(),
                    parser.pane_default_bg(),
                )
            })
            .unwrap_or((None, None));
        if let Some(copy_view) = crate::copy_mode::render_view(pane) {
            for (row_idx, row) in copy_view.rows.iter().enumerate() {
                if row_idx >= inner.height as usize {
                    break;
                }
                write_copy_row(
                    row,
                    inner.width,
                    inner.x,
                    inner.y + row_idx as u16,
                    pane_defaults.0,
                    pane_defaults.1,
                    out,
                );
            }
            if let Some(ratio) = copy_view.scroll_ratio {
                write_scrollbar(inner, ratio, out);
            }
            write_pane_borders(area, is_active, has_border, redraw_border, out);
            return;
        }
        write_pane_borders(area, is_active, has_border, redraw_border, out);
        return;
    }
    let snapshot = match pane.parser.lock() {
        Ok(mut parser) => {
            parser.flush_sync_for_display();
            Some((
                pane_default_codes(
                    parser.pane_default_fg(),
                    parser.pane_default_bg(),
                ),
                parser.alternate_screen(),
                parser.visible_rows(),
            ))
        }
        Err(_) => None,
    };
    let Some(((pane_fg, pane_bg), alternate_screen, rows)) = snapshot else {
        write_pane_borders(area, is_active, has_border, redraw_border, out);
        return;
    };
    for row_idx in 0..inner.height as usize {
        let cells = rows.get(row_idx).cloned().unwrap_or_default();
        write_terminal_row(
            &cells,
            inner.width,
            inner.x,
            inner.y + row_idx as u16,
            pane_fg,
            pane_bg,
            !alternate_screen,
            out,
        );
    }
    write_pane_borders(area, is_active, has_border, redraw_border, out);
}

fn fill_gap(gap: Rect, out: &mut String) {
    if gap.width == 0 || gap.height == 0 {
        return;
    }
    let pad = adjust_styles_for_custom_bg_fg(DEFAULT_STYLES, None, None);
    for row in 0..gap.height {
        vte_goto(gap.x, gap.y + row, out);
        let mut current = DEFAULT_STYLES;
        write_style_diff(&mut current, pad, out);
        for _ in 0..gap.width {
            out.push(' ');
        }
    }
}

fn write_split_gaps(
    direction: &SplitDirection,
    chunks: &[Rect],
    out: &mut String,
) {
    if chunks.len() < 2 {
        return;
    }
    for pair in chunks.windows(2) {
        let gap = match direction {
            SplitDirection::Horizontal => Rect::new(
                pair[0].x + pair[0].width,
                pair[0].y,
                pair[1].x.saturating_sub(pair[0].x + pair[0].width),
                pair[0].height,
            ),
            SplitDirection::Vertical => Rect::new(
                pair[0].x,
                pair[0].y + pair[0].height,
                pair[0].width,
                pair[1].y.saturating_sub(pair[0].y + pair[0].height),
            ),
        };
        fill_gap(gap, out);
    }
}

fn split_rects(
    area: Rect,
    direction: &SplitDirection,
    sizes: &[u16],
    count: usize,
    hide_borders: bool,
) -> Vec<Rect> {
    if count == 0 {
        return Vec::new();
    }
    let gap = if hide_borders { 0 } else { BORDER_SIZE };
    let horizontal = matches!(direction, SplitDirection::Horizontal);
    let total_dim = if horizontal { area.width } else { area.height };
    let borders = count.saturating_sub(1) as u16 * gap;
    let available = total_dim.saturating_sub(borders);
    let total_pct = sizes.iter().copied().sum::<u16>().max(1);
    let mut rects = Vec::with_capacity(count);
    let mut offset = 0u16;
    for (index, &pct) in sizes.iter().enumerate().take(count) {
        let dim = if index + 1 == count {
            total_dim.saturating_sub(offset)
        } else {
            (available as u32 * pct as u32 / total_pct as u32) as u16
        };
        rects.push(if horizontal {
            Rect::new(area.x + offset, area.y, dim, area.height)
        } else {
            Rect::new(area.x, area.y + offset, area.width, dim)
        });
        offset += dim + gap;
    }
    rects
}

fn write_node(
    node: &LayoutNode,
    active_path: &[usize],
    area: Rect,
    hide_borders: bool,
    out: &mut String,
    opts: &FrameAnsiOptions,
    consume_dirty: bool,
) {
    match node {
        LayoutNode::Split {
            direction,
            sizes,
            children,
        } => {
            let chunks = split_rects(
                area,
                direction,
                sizes,
                children.len(),
                hide_borders,
            );
            for (i, child) in children.iter().enumerate().rev() {
                let child_active = active_path.first() == Some(&i);
                let child_path = if active_path.first() == Some(&i) {
                    &active_path[1..]
                } else {
                    &[]
                };
                write_child_node(
                    child,
                    child_path,
                    child_active,
                    chunks[i],
                    hide_borders,
                    out,
                    opts,
                    consume_dirty,
                );
            }
            // Only refresh gaps when the frame is doing a full clear/repaint.
            // Rewriting the gap on every dirty-pane keystroke flashes a blank
            // column under weak synchronized-update support.
            if !hide_borders && (opts.clear_display || opts.force_repaint) {
                write_split_gaps(direction, &chunks, out);
            }
        }
        LayoutNode::Leaf(pane) => {
            write_pane(
                pane,
                true,
                area,
                hide_borders,
                out,
                opts,
                consume_dirty,
            );
        }
    }
}

fn write_child_node(
    node: &LayoutNode,
    relative_path: &[usize],
    is_active_branch: bool,
    area: Rect,
    hide_borders: bool,
    out: &mut String,
    opts: &FrameAnsiOptions,
    consume_dirty: bool,
) {
    match node {
        LayoutNode::Split {
            direction,
            sizes,
            children,
        } => {
            let chunks = split_rects(
                area,
                direction,
                sizes,
                children.len(),
                hide_borders,
            );
            for (i, child) in children.iter().enumerate().rev() {
                let child_active =
                    is_active_branch && relative_path.first() == Some(&i);
                let child_rel = if relative_path.first() == Some(&i) {
                    &relative_path[1..]
                } else {
                    &[]
                };
                write_child_node(
                    child,
                    child_rel,
                    child_active,
                    chunks[i],
                    hide_borders,
                    out,
                    opts,
                    consume_dirty,
                );
            }
            // Only refresh gaps on full clear/repaint (see write_node).
            if !hide_borders && (opts.clear_display || opts.force_repaint) {
                write_split_gaps(direction, &chunks, out);
            }
        }
        LayoutNode::Leaf(pane) => {
            let is_active = is_active_branch && relative_path.is_empty();
            write_pane(
                pane,
                is_active,
                area,
                hide_borders,
                out,
                opts,
                consume_dirty,
            );
        }
    }
}

fn relay_pending_osc52(node: &LayoutNode, out: &mut String) {
    match node {
        LayoutNode::Split { children, .. } => {
            for child in children {
                relay_pending_osc52(child, out);
            }
        }
        LayoutNode::Leaf(pane) => {
            let Ok(mut pending) = pane.pending_osc52.lock() else {
                return;
            };
            while let Some(sequence) = pending.pop_front() {
                // OSC 52 is ASCII control data. The tracker only enqueues
                // validated sequences, but discard anything unexpected.
                if let Ok(sequence) = std::str::from_utf8(&sequence) {
                    out.push_str(sequence);
                }
            }
        }
    }
}

pub fn layout_fingerprint(win: &Window, area: Rect, hide_borders: bool) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    let mut hasher = DefaultHasher::new();
    area.width.hash(&mut hasher);
    area.height.hash(&mut hasher);
    hide_borders.hash(&mut hasher);
    win.active_pane_path.hash(&mut hasher);
    if let Some(zoom) = &win.zoom_state {
        zoom.zoomed_pane_id.hash(&mut hasher);
    }
    hash_layout_node(&win.root, area, hide_borders, &mut hasher);
    hasher.finish()
}

fn hash_layout_node(
    node: &LayoutNode,
    area: Rect,
    hide_borders: bool,
    hasher: &mut impl Hasher,
) {
    match node {
        LayoutNode::Split {
            direction,
            sizes,
            children,
        } => {
            match direction {
                SplitDirection::Horizontal => 0u8.hash(hasher),
                SplitDirection::Vertical => 1u8.hash(hasher),
            }
            for size in sizes {
                size.hash(hasher);
            }
            let chunks = split_rects(
                area,
                direction,
                sizes,
                children.len(),
                hide_borders,
            );
            for (child, rect) in children.iter().zip(chunks.iter()) {
                hash_layout_node(child, *rect, hide_borders, hasher);
            }
        }
        LayoutNode::Leaf(pane) => {
            pane.id.hash(hasher);
            area.x.hash(hasher);
            area.y.hash(hasher);
            area.width.hash(hasher);
            area.height.hash(hasher);
        }
    }
}

/// Clear only the pane region, preserving the client tab bar (row 0) and status bar.
///
/// Must not use `\x1b[K` (EL): that clears to the physical line end and would wipe
/// every pane on the row. Space-fill the rect instead (same as per-pane erase).
pub fn write_clear_pane_area(out: &mut String, area: Rect) {
    write_erase_rect(area, out);
}

/// Serialize pane layout to ANSI. Coordinates are absolute screen positions.
pub fn serialize_frame_ansi(
    win: &Window,
    area: Rect,
    hide_borders: bool,
    opts: FrameAnsiOptions,
) -> String {
    serialize_frame_ansi_with_mode(win, area, hide_borders, opts, true)
}

/// Build an authoritative full paint without claiming the normal renderer's
/// dirty generation or draining one-shot terminal side effects. This is used
/// to resynchronize one lagging frame connection; publishing it globally would
/// make healthy clients replay a recovery frame they did not request.
pub(crate) fn serialize_frame_ansi_snapshot(
    win: &Window,
    area: Rect,
    hide_borders: bool,
    opts: FrameAnsiOptions,
) -> String {
    serialize_frame_ansi_with_mode(win, area, hide_borders, opts, false)
}

fn serialize_frame_ansi_with_mode(
    win: &Window,
    area: Rect,
    hide_borders: bool,
    opts: FrameAnsiOptions,
    consume_render_state: bool,
) -> String {
    let mut out = String::with_capacity(65536);
    if opts.clear_display {
        write_clear_pane_area(&mut out, area);
    }
    if let Some(zoom) = &win.zoom_state {
        if let Some(pane) =
            crate::layout::find_pane_by_id(&win.root, zoom.zoomed_pane_id)
        {
            write_pane(
                pane,
                true,
                area,
                hide_borders,
                &mut out,
                &opts,
                consume_render_state,
            );
            if consume_render_state {
                relay_pending_osc52(&win.root, &mut out);
            }
            return out;
        }
    }
    write_node(
        &win.root,
        &win.active_pane_path,
        area,
        hide_borders,
        &mut out,
        &opts,
        consume_render_state,
    );
    if consume_render_state {
        relay_pending_osc52(&win.root, &mut out);
    }
    out
}

/// Pane-only ANSI area on the client screen (below tab bar, above status bar).
pub fn frame_ansi_area(size: crate::types::session::Size) -> Rect {
    Rect::new(0, 1, size.cols.max(1), size.rows.saturating_sub(1).max(1))
}

pub fn encode_ansi_base64(ansi: &str) -> String {
    base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        ansi.as_bytes(),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use alacritty_terminal::{
        term::cell::Flags,
        vte::ansi::{Color, NamedColor},
    };

    use super::*;
    use crate::terminal::AlacrittyTermState;

    #[test]
    fn default_background_after_colored_emits_reset() {
        let mut term = AlacrittyTermState::new(1, 4, 100);
        term.process(b"\x1b[42mABC\x1b[49m   ");
        let mut out = String::new();
        let cells = term.visible_rows().into_iter().next().unwrap_or_default();
        write_terminal_row(&cells, 4, 0, 0, None, None, false, &mut out);
        assert!(
            out.starts_with("\x1b[1;1H\x1b[m"),
            "line should reset SGR at start, got: {out:?}"
        );
    }

    #[test]
    fn goto_resets_sgr_at_line_start() {
        let mut out = String::new();
        vte_goto(5, 3, &mut out);
        assert!(out.starts_with("\x1b[4;6H\x1b[m"));
    }

    #[test]
    fn write_erase_rect_does_not_clear_to_line_end() {
        let mut out = String::new();
        write_erase_rect(Rect::new(10, 5, 20, 2), &mut out);
        assert!(
            !out.contains("\x1b[K"),
            "must not EL to line end (erases pane borders and split gaps), got {out:?}"
        );
    }

    #[test]
    fn clear_display_preserves_chrome_rows() {
        let pane = Rect::new(0, 1, 80, 22);
        let mut out = String::new();
        write_clear_pane_area(&mut out, pane);
        assert!(
            !out.contains("\x1b[2J"),
            "must not full-screen clear, got {out:?}"
        );
        assert!(
            !out.contains("\x1b[0J"),
            "must not clear to end of screen (status bar would be erased), got {out:?}"
        );
        assert!(
            !out.contains("\x1b[K"),
            "must not EL to line end (would wipe sibling panes), got {out:?}"
        );
        assert!(
            out.contains("\x1b[2;1H\x1b[m"),
            "must clear pane lines by space-fill from the area origin, got {out:?}"
        );
        assert!(
            !out.contains("\x1b[1;"),
            "tab bar row must be preserved, got {out:?}"
        );
        assert!(
            !out.contains("\x1b[24;"),
            "status bar row must be preserved, got {out:?}"
        );
    }

    #[test]
    fn colored_diff_line_extends_background_to_eol() {
        let mut out = String::new();
        let cells = vec![
            Some(TerminalCell {
                text: "+".to_string(),
                fg: Color::Named(NamedColor::Foreground),
                bg: Color::Named(NamedColor::Green),
                flags: Flags::empty(),
                width: 1,
            }),
            Some(TerminalCell {
                text: "x".to_string(),
                fg: Color::Named(NamedColor::Foreground),
                bg: Color::Named(NamedColor::Green),
                flags: Flags::empty(),
                width: 1,
            }),
            None,
        ];
        write_terminal_row(&cells, 3, 0, 0, None, None, false, &mut out);
        assert_eq!(row_paint_end(&cells, 3), Some(2));
        assert!(
            out.contains("\x1b[42m"),
            "diff line should extend green background through EOL, got: {out:?}"
        );
    }

    #[test]
    fn diff_style_line_extends_colored_background_to_eol() {
        let mut term = AlacrittyTermState::new(5, 40, 1000);
        term.process(b"\x1b[48;5;22m+ # Teamspace title\n");
        term.process(b"\x1b[48;5;22m+ > **Status:** open\n");
        let rows = term.visible_rows();
        let mut out = String::new();
        for (idx, cells) in rows.iter().take(2).enumerate() {
            write_terminal_row(
                cells, 40, 0, idx as u16, None, None, false, &mut out,
            );
        }
        assert_eq!(row_paint_end(&rows[0], 40), Some(39));
        let green_count = out.matches("\x1b[48;5;22m").count();
        assert!(
            green_count >= 2,
            "colored diff lines should paint green through EOL, got: {out:?}"
        );
    }

    #[test]
    fn erase_in_line_green_fill_paints_to_eol() {
        let mut term = AlacrittyTermState::new(2, 20, 1000);
        term.process(b"\x1b[48;5;22m+ title\x1b[K");
        let cells = term.visible_rows().into_iter().next().unwrap_or_default();
        let text: String = cells
            .iter()
            .filter_map(|c| c.as_ref())
            .map(|c| c.text.as_str())
            .collect();
        assert!(
            text.contains('+'),
            "expected diff prefix in row, got cells: {text:?}"
        );
        assert_eq!(row_paint_end(&cells, 20), Some(19));
        let mut out = String::new();
        write_terminal_row(&cells, 20, 0, 0, None, None, false, &mut out);
        assert!(
            out.contains("\x1b[48;5;22m"),
            "EL fill should paint green through EOL, got: {out:?}"
        );
        assert!(
            out.contains(' '),
            "row should be padded to pane width, got: {out:?}"
        );
    }

    #[test]
    fn blank_line_after_colored_line_is_not_green() {
        let mut term = AlacrittyTermState::new(3, 30, 1000);
        term.process(b"\x1b[48;5;22m+ line one\x1b[m\n\n");
        let rows = term.visible_rows();
        let mut out = String::new();
        write_terminal_row(&rows[1], 30, 0, 1, None, None, false, &mut out);
        assert!(
            !out.contains("\x1b[48;5;22m"),
            "blank line should not inherit diff green, got: {out:?}"
        );
    }

    #[test]
    fn row_gap_inherits_active_background() {
        let mut out = String::new();
        let cells = vec![
            Some(TerminalCell {
                text: "+".to_string(),
                fg: Color::Named(NamedColor::Foreground),
                bg: Color::Named(NamedColor::Green),
                flags: Flags::empty(),
                width: 1,
            }),
            None,
            Some(TerminalCell {
                text: "X".to_string(),
                fg: Color::Named(NamedColor::Foreground),
                bg: Color::Named(NamedColor::Green),
                flags: Flags::empty(),
                width: 1,
            }),
        ];
        write_terminal_row(&cells, 3, 0, 0, None, None, false, &mut out);
        let green_start = out.find("\x1b[42m").expect("green bg");
        let x_pos = out.find('X').expect("X");
        let plus_pos = out.find('+').expect("plus");
        // Gap cell between '+' and 'X' must be painted after green is active.
        assert!(plus_pos > green_start);
        assert!(x_pos > plus_pos);
        assert!(
            out[plus_pos..x_pos].contains(' '),
            "gap between + and X should be a space carrying active bg, got {out:?}"
        );
    }

    #[test]
    fn clear_display_paints_even_when_render_dirty_is_false() {
        let dirty = AtomicBool::new(false);
        assert!(claim_render_dirty(&dirty, true));
        assert!(!dirty.load(Ordering::Relaxed));
    }

    #[test]
    fn dirty_output_arriving_during_paint_remains_pending() {
        let dirty = AtomicBool::new(false);
        assert!(!claim_render_dirty(&dirty, false));

        dirty.store(true, Ordering::Relaxed);
        assert!(claim_render_dirty(&dirty, false));
        assert!(!dirty.load(Ordering::Relaxed));

        // Simulate PTY output arriving after the renderer claimed the previous
        // dirty generation but before it finished writing the ANSI payload.
        dirty.store(true, Ordering::Release);
        assert!(dirty.load(Ordering::Acquire));
        assert!(claim_render_dirty(&dirty, false));
    }

    /// A wide character (CJK) sitting in the last content column must not be
    /// painted past the pane's right edge. `write_terminal_row` advances by
    /// display width, so a width-2 cell at the final column used to emit the
    /// glyph at `inner.x + (cols-1)` — occupying the border/gap cell to the
    /// right and bleeding pane content into the neighbouring pane.
    #[test]
    fn wide_char_in_last_column_does_not_overflow_pane_width() {
        let cells = vec![
            Some(TerminalCell {
                text: "a".to_string(),
                fg: Color::Named(NamedColor::Foreground),
                bg: Color::Named(NamedColor::Background),
                flags: Flags::empty(),
                width: 1,
            }),
            Some(TerminalCell {
                // U+4E2D 中 — display width 2, placed in the last column.
                text: "中".to_string(),
                fg: Color::Named(NamedColor::Foreground),
                bg: Color::Named(NamedColor::Background),
                flags: Flags::empty(),
                width: 2,
            }),
        ];
        // inner.x = 5, cols = 2 -> drawable columns are screen x = 5,6. A width-2
        // glyph at x = 6 would spill into x = 7 (the pane's right border / gap),
        // so the row must not contain the wide glyph at all.
        let mut out = String::new();
        write_terminal_row(&cells, 2, 5, 0, None, None, false, &mut out);
        assert!(
            !out.contains('中'),
            "wide char painted in last column overflows pane width: {out:?}"
        );
    }

    /// Normal CJK width agrees with the terminal grid, so it can stream without
    /// a per-cell CUP. This keeps full-screen applications fast.
    #[test]
    fn write_terminal_row_streams_when_grid_width_matches_display_width() {
        let cells = vec![
            Some(TerminalCell {
                text: "中".to_string(),
                fg: Color::Named(NamedColor::Foreground),
                bg: Color::Named(NamedColor::Background),
                flags: Flags::empty(),
                width: 2,
            }),
            None, // spacer
            Some(TerminalCell {
                text: "A".to_string(),
                fg: Color::Named(NamedColor::Foreground),
                bg: Color::Named(NamedColor::Background),
                flags: Flags::empty(),
                width: 1,
            }),
        ];
        let mut out = String::new();
        write_terminal_row(&cells, 3, 10, 5, None, None, false, &mut out);
        assert!(
            out.contains("\x1b[6;11H\x1b[m"),
            "row start cup+reset missing: {out:?}"
        );
        assert!(
            out.contains('中') && out.contains('A'),
            "both glyphs must paint: {out:?}"
        );
        assert!(
            !out.contains("\x1b[6;13H"),
            "matching-width cells should stream without a per-cell CUP: {out:?}"
        );
    }

    /// Shell rows are re-anchored at every grid column. This prevents a glyph
    /// width mismatch or host-side automatic wrap in one pane from carrying
    /// later `ls` output across the split into its neighbour.
    #[test]
    fn write_terminal_row_anchors_every_shell_grid_column() {
        let cells = vec![
            Some(TerminalCell {
                text: "A".to_string(),
                fg: Color::Named(NamedColor::Foreground),
                bg: Color::Named(NamedColor::Background),
                flags: Flags::empty(),
                width: 1,
            }),
            Some(TerminalCell {
                text: "B".to_string(),
                fg: Color::Named(NamedColor::Foreground),
                bg: Color::Named(NamedColor::Background),
                flags: Flags::empty(),
                width: 1,
            }),
        ];
        let mut out = String::new();
        write_terminal_row(&cells, 3, 10, 5, None, None, true, &mut out);
        assert!(
            out.contains("\x1b[6;12H"),
            "second shell cell must have an absolute position: {out:?}"
        );
        assert!(
            out.contains("\x1b[6;13H"),
            "trailing shell padding must stay inside the pane: {out:?}"
        );
    }

    /// PUA icons often claim cell.width=1 while the host draws them double-wide.
    /// Re-CUP only after such a glyph so later cells stay aligned.
    #[test]
    fn write_terminal_row_resyncs_cup_after_underreported_pua() {
        let icon = char::from_u32(0xE0A0).unwrap();
        let cells = vec![
            Some(TerminalCell {
                text: icon.to_string(),
                fg: Color::Named(NamedColor::Foreground),
                bg: Color::Named(NamedColor::Background),
                flags: Flags::empty(),
                width: 1,
            }),
            Some(TerminalCell {
                text: "B".to_string(),
                fg: Color::Named(NamedColor::Foreground),
                bg: Color::Named(NamedColor::Background),
                flags: Flags::empty(),
                width: 1,
            }),
            Some(TerminalCell {
                text: "C".to_string(),
                fg: Color::Named(NamedColor::Foreground),
                bg: Color::Named(NamedColor::Background),
                flags: Flags::empty(),
                width: 1,
            }),
        ];
        let mut out = String::new();
        write_terminal_row(&cells, 3, 10, 5, None, None, false, &mut out);
        // After icon at x=10 (advance 1), next grid col is 11 → 1-based CUP 12.
        assert!(
            out.contains("\x1b[6;12H"),
            "expected resync CUP after under-reported PUA, got {out:?}"
        );
        assert!(
            out.contains('B') && out.contains('C'),
            "following cells must still paint: {out:?}"
        );
    }

    /// Even when `cell.width` under-reports a wide glyph, clipping must use the
    /// real unicode width so the glyph cannot spill into the next pane.
    #[test]
    fn underestimated_glyph_width_does_not_wrap_past_pane() {
        let cells = vec![
            Some(TerminalCell {
                text: "a".to_string(),
                fg: Color::Named(NamedColor::Foreground),
                bg: Color::Named(NamedColor::Background),
                flags: Flags::empty(),
                width: 1,
            }),
            Some(TerminalCell {
                text: "中".to_string(),
                fg: Color::Named(NamedColor::Foreground),
                bg: Color::Named(NamedColor::Background),
                flags: Flags::empty(),
                width: 1, // under-report; real width is 2
            }),
        ];
        let mut out = String::new();
        write_terminal_row(&cells, 2, 10, 3, None, None, false, &mut out);
        assert!(
            !out.contains('中'),
            "under-reported wide glyph at last column must be clipped: {out:?}"
        );
        assert!(
            out.contains('a'),
            "leading narrow glyph must still be painted: {out:?}"
        );
    }

    /// Advancing by an overstated PUA width must not skip the next grid cell —
    /// that was shearing `ls` columns. Grid advance stays 1 when cell.width is 1.
    #[test]
    fn nerd_font_pua_does_not_skip_following_grid_cell() {
        let icon = char::from_u32(0xE0A0).unwrap();
        assert_eq!(grid_advance_width(1), 1);
        assert_eq!(glyph_clip_width(icon, 1), 2);
        let cells = vec![
            Some(TerminalCell {
                text: icon.to_string(),
                fg: Color::Named(NamedColor::Foreground),
                bg: Color::Named(NamedColor::Background),
                flags: Flags::empty(),
                width: 1,
            }),
            Some(TerminalCell {
                text: "B".to_string(),
                fg: Color::Named(NamedColor::Foreground),
                bg: Color::Named(NamedColor::Background),
                flags: Flags::empty(),
                width: 1,
            }),
        ];
        let mut out = String::new();
        write_terminal_row(&cells, 2, 0, 0, None, None, false, &mut out);
        assert!(
            out.contains('B'),
            "PUA must not consume the next grid cell: {out:?}"
        );
        assert!(out.contains(icon), "PUA icon should still paint: {out:?}");
    }

    #[test]
    fn write_terminal_row_parks_cursor_after_last_cell() {
        let cells = vec![Some(TerminalCell {
            text: "A".to_string(),
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            flags: Flags::empty(),
            width: 1,
        })];
        let mut out = String::new();
        write_terminal_row(&cells, 1, 79, 5, None, None, false, &mut out);
        assert!(
            out.ends_with("\x1b[6;80H"),
            "must CUP away from a last-column write so autowrap cannot spill, got {out:?}"
        );
    }

    #[test]
    fn write_border_parks_cursor_after_right_edge_glyphs() {
        let mut out = String::new();
        write_border(Rect::new(0, 0, 80, 5), true, &mut out);
        assert!(
            out.contains("┐\x1b[1;1H"),
            "top-right corner must not leave wrap-pending on the last column: {out:?}"
        );
        assert!(
            out.contains("┘\x1b[5;1H"),
            "bottom-right corner must not leave wrap-pending on the last column: {out:?}"
        );
        assert!(
            out.contains("│\x1b[2;1H"),
            "right vertical must park after the last-column glyph: {out:?}"
        );
    }
}
