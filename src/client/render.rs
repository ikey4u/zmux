use std::io::{self, Write};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use ratatui::{
    buffer::CellDiffOption,
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
    Frame,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FrameData {
    #[serde(rename = "type")]
    pub frame_type: String,
    pub layout: LayoutJson,
    #[serde(default)]
    pub status: Option<StatusJson>,
    /// Base64-encoded server-rendered pane ANSI (Zellij-style direct output).
    #[serde(default)]
    pub ansi: Option<String>,
    #[serde(default)]
    pub exit: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yank_text: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum LayoutJson {
    Split {
        direction: String,
        sizes: Vec<u16>,
        children: Vec<LayoutJson>,
    },
    Leaf {
        id: usize,
        rows: u16,
        cols: u16,
        cursor_row: u16,
        cursor_col: u16,
        #[serde(default)]
        hide_cursor: bool,
        #[serde(default)]
        alternate_screen: bool,
        #[serde(default)]
        mouse_mode: u8,
        #[serde(default)]
        in_copy_mode: bool,
        #[serde(default)]
        scroll_ratio: Option<f32>,
        #[serde(default)]
        cursor_shape: u8,
        #[serde(default)]
        active: bool,
        #[serde(default)]
        rows_v2: Vec<RowRunsJson>,
        #[serde(default)]
        title: Option<String>,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RowRunsJson {
    pub runs: Vec<CellRunJson>,
    #[serde(default)]
    pub line: Option<usize>,
    #[serde(default)]
    pub start_col: usize,
    #[serde(default)]
    pub end_col: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CellRunJson {
    pub text: String,
    pub fg: String,
    pub bg: String,
    pub flags: u8,
    pub width: u16,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StatusJson {
    pub left: String,
    pub right: String,
    pub windows: Vec<WindowTabJson>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WindowTabJson {
    pub index: usize,
    pub name: String,
    pub active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientTabState {
    Active,
    Inactive,
    Dead,
}

#[derive(Debug, Clone)]
pub struct ClientTabView {
    pub code: String,
    pub title: String,
    pub state: ClientTabState,
    pub socket_name: String,
    pub visible: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientTabBarHit {
    Tab(usize),
    OverflowStart,
    OverflowEnd,
}

pub fn render_frame(f: &mut Frame, fd: &FrameData, in_prefix: bool) {
    render_frame_ex(f, fd, in_prefix, false, false);
}

pub fn active_cursor_shape(fd: &FrameData) -> Option<u8> {
    active_cursor_shape_in_layout(&fd.layout)
}

pub fn active_mouse_mode(fd: &FrameData) -> u8 {
    active_mouse_mode_in_layout(&fd.layout)
}

pub fn active_in_copy_mode(fd: &FrameData) -> bool {
    active_in_copy_mode_in_layout(&fd.layout)
}

pub fn active_scroll_ratio(fd: &FrameData) -> Option<f32> {
    active_scroll_ratio_in_layout(&fd.layout)
}

/// Write server-rendered pane ANSI directly to the terminal.
/// Mark the pane layout region as skipped in ratatui's diff so stale buffer
/// cells (e.g. after mouse selection) are not written over server ANSI output.
pub fn skip_pane_area_for_ansi(
    f: &mut Frame,
    fd: &FrameData,
    hide_borders: bool,
) {
    if fd.ansi.is_none() {
        return;
    }
    let area = f.area();
    if area.height <= 1 {
        return;
    }
    let frame_area = Rect {
        x: area.x,
        y: area.y + 1,
        width: area.width,
        height: area.height - 1,
    };
    if frame_area.height < 2 {
        return;
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(frame_area);
    mark_layout_area_skip(f, &fd.layout, chunks[0], hide_borders);
}

/// Repaint a screen rectangle from layout rows_v2 (used to undo selection overlay).
pub fn write_rows_v2_rect_ansi<W: io::Write>(
    writer: &mut W,
    rows_v2: &[RowRunsJson],
    content_area: Rect,
    start_row: u16,
    start_col: u16,
    end_row: u16,
    end_col: u16,
) -> io::Result<()> {
    use crate::output::{
        vte_goto, write_style_diff, CharacterStyles, DEFAULT_STYLES,
    };

    if start_row > end_row || (start_row == end_row && start_col >= end_col) {
        return Ok(());
    }

    let sr = start_row
        .max(content_area.y)
        .min(content_area.y + content_area.height.saturating_sub(1));
    let sc = start_col
        .max(content_area.x)
        .min(content_area.x + content_area.width.saturating_sub(1));
    let er = end_row
        .max(content_area.y)
        .min(content_area.y + content_area.height.saturating_sub(1));
    let ec = end_col
        .max(content_area.x)
        .min(content_area.x + content_area.width);
    if sr == er && sc >= ec {
        return Ok(());
    }

    let mut buf = String::new();
    let mut current = DEFAULT_STYLES;
    let max_cols = content_area.width as usize;

    for row in sr..=er {
        let col_begin = if row == sr { sc } else { content_area.x };
        let col_end = if row == er {
            ec
        } else {
            content_area.x + content_area.width
        };
        let pane_row = (row - content_area.y) as usize;
        let Some(row_data) = rows_v2.get(pane_row) else {
            continue;
        };
        let pane_col_begin =
            (col_begin.saturating_sub(content_area.x)) as usize;
        let pane_col_end =
            ((col_end.saturating_sub(content_area.x)) as usize).min(max_cols);
        if pane_col_begin >= pane_col_end {
            continue;
        }

        let y = row;
        let mut col = 0usize;
        for run in &row_data.runs {
            if col >= max_cols {
                break;
            }
            let style =
                CharacterStyles::from_layout_run(&run.fg, &run.bg, run.flags);
            for ch in run.text.chars() {
                if col >= max_cols {
                    break;
                }
                let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
                if col >= pane_col_end {
                    break;
                }
                if col + w > pane_col_begin {
                    vte_goto(content_area.x + col as u16, y, &mut buf);
                    write_style_diff(&mut current, style, &mut buf);
                    buf.push(ch);
                }
                col += w;
            }
        }
    }

    if !buf.is_empty() {
        writer.write_all(buf.as_bytes())?;
        writer.flush()?;
    }
    Ok(())
}

pub fn restore_mouse_selection_ansi<W: io::Write>(
    writer: &mut W,
    fd: &FrameData,
    start_row: u16,
    start_col: u16,
    end_row: u16,
    end_col: u16,
    layout_area: Rect,
    hide_borders: bool,
) -> io::Result<()> {
    let Some((rows_v2, content_area)) =
        active_pane_rows_v2(&fd.layout, layout_area, hide_borders)
    else {
        return Ok(());
    };
    write_rows_v2_rect_ansi(
        writer,
        rows_v2,
        content_area,
        start_row,
        start_col,
        end_row,
        end_col,
    )
}

/// Repaint the active pane from layout JSON before drawing a mouse selection overlay.
/// Server-side ANSI skips unchanged panes, so the client must restore the active pane
/// when only the selection bounds change.
pub fn write_active_pane_layout_ansi<W: io::Write>(
    writer: &mut W,
    fd: &FrameData,
    layout_area: Rect,
    hide_borders: bool,
) -> io::Result<()> {
    let Some((rows_v2, content_area)) =
        active_pane_rows_v2(&fd.layout, layout_area, hide_borders)
    else {
        return Ok(());
    };
    let mut buf = String::new();
    let area = crate::types::Rect::new(
        content_area.x,
        content_area.y,
        content_area.width,
        content_area.height,
    );
    use crate::output::{
        vte_goto, write_erase_rect, write_style_diff, CharacterStyles,
        DEFAULT_STYLES,
    };
    write_erase_rect(area, &mut buf);
    let mut current = DEFAULT_STYLES;
    let max_rows = content_area.height as usize;
    let max_cols = content_area.width as usize;
    for (row_idx, row_data) in rows_v2.iter().enumerate().take(max_rows) {
        let y = content_area.y + row_idx as u16;
        let mut col = 0usize;
        for run in &row_data.runs {
            if col >= max_cols {
                break;
            }
            vte_goto(content_area.x + col as u16, y, &mut buf);
            let style =
                CharacterStyles::from_layout_run(&run.fg, &run.bg, run.flags);
            write_style_diff(&mut current, style, &mut buf);
            let available = max_cols - col;
            let text = truncate_to_width(&run.text, available);
            if text.is_empty() {
                break;
            }
            buf.push_str(&text);
            col += unicode_display_width(&text);
        }
    }
    writer.write_all(buf.as_bytes())?;
    writer.flush()
}

fn active_pane_rows_v2<'a>(
    layout: &'a LayoutJson,
    area: Rect,
    hide_borders: bool,
) -> Option<(&'a [RowRunsJson], Rect)> {
    match layout {
        LayoutJson::Leaf {
            active: true,
            rows_v2,
            ..
        } => {
            let content_area =
                if !hide_borders && area.width > 2 && area.height > 2 {
                    Rect {
                        x: area.x + 1,
                        y: area.y + 1,
                        width: area.width - 2,
                        height: area.height - 2,
                    }
                } else {
                    area
                };
            Some((rows_v2.as_slice(), content_area))
        }
        LayoutJson::Split {
            direction,
            sizes,
            children,
        } => {
            let chunks = split_layout_rects(
                area,
                direction,
                sizes,
                children.len(),
                hide_borders,
            );
            for (child, chunk) in children.iter().zip(chunks.iter()) {
                if matches!(child, LayoutJson::Leaf { active: true, .. }) {
                    if let Some(found) =
                        active_pane_rows_v2(child, *chunk, hide_borders)
                    {
                        return Some(found);
                    }
                }
            }
            for (child, chunk) in children.iter().zip(chunks.into_iter()) {
                if let Some(found) =
                    active_pane_rows_v2(child, chunk, hide_borders)
                {
                    return Some(found);
                }
            }
            None
        }
        LayoutJson::Leaf { .. } => None,
    }
}

fn mark_layout_area_skip(
    f: &mut Frame,
    layout: &LayoutJson,
    area: Rect,
    hide_borders: bool,
) {
    match layout {
        LayoutJson::Split {
            direction,
            sizes,
            children,
        } => {
            let chunks = split_layout_rects(
                area,
                direction,
                sizes,
                children.len(),
                hide_borders,
            );
            for (child, chunk) in children.iter().zip(chunks.iter()) {
                mark_layout_area_skip(f, child, *chunk, hide_borders);
            }
            if !hide_borders {
                mark_split_gaps_skip(f, direction, &chunks);
            }
        }
        LayoutJson::Leaf { .. } => {
            mark_rect_skip(f, area);
        }
    }
}

fn mark_split_gaps_skip(f: &mut Frame, direction: &str, chunks: &[Rect]) {
    if chunks.len() < 2 {
        return;
    }
    let horizontal = direction == "horizontal";
    for pair in chunks.windows(2) {
        let gap = if horizontal {
            Rect {
                x: pair[0].x.saturating_add(pair[0].width),
                y: pair[0].y,
                width: pair[1]
                    .x
                    .saturating_sub(pair[0].x.saturating_add(pair[0].width)),
                height: pair[0].height,
            }
        } else {
            Rect {
                x: pair[0].x,
                y: pair[0].y.saturating_add(pair[0].height),
                width: pair[0].width,
                height: pair[1]
                    .y
                    .saturating_sub(pair[0].y.saturating_add(pair[0].height)),
            }
        };
        mark_rect_skip(f, gap);
    }
}

fn mark_rect_skip(f: &mut Frame, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let buf = f.buffer_mut();
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            buf[(x, y)].set_diff_option(CellDiffOption::Skip);
        }
    }
}

pub fn write_server_ansi<W: Write>(
    writer: &mut W,
    ansi_b64: &str,
) -> io::Result<()> {
    let bytes = STANDARD
        .decode(ansi_b64.trim())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    writer.write_all(&bytes)?;
    writer.flush()
}

pub fn active_cursor_screen_position(
    fd: &FrameData,
    frame_area: Rect,
    hide_borders: bool,
) -> Option<Position> {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(frame_area);
    let layout_area = if fd.status.is_some() {
        chunks[0]
    } else {
        frame_area
    };
    cursor_position_in_layout(&fd.layout, layout_area, hide_borders)
}

fn cursor_position_in_layout(
    layout: &LayoutJson,
    area: Rect,
    hide_borders: bool,
) -> Option<Position> {
    match layout {
        LayoutJson::Split {
            direction,
            sizes,
            children,
        } => {
            let chunks = split_layout_rects(
                area,
                direction,
                sizes,
                children.len(),
                hide_borders,
            );
            children
                .iter()
                .zip(chunks.iter())
                .find_map(|(child, chunk)| {
                    cursor_position_in_layout(child, *chunk, hide_borders)
                })
        }
        LayoutJson::Leaf {
            active: true,
            cursor_row,
            cursor_col,
            hide_cursor,
            in_copy_mode,
            scroll_ratio,
            ..
        } => {
            if *hide_cursor {
                return None;
            }
            if !*in_copy_mode && scroll_ratio.is_some() {
                return None;
            }
            let content = pane_content_rect(area, hide_borders);
            Some(Position {
                x: content
                    .x
                    .saturating_add(*cursor_col)
                    .min(content.x + content.width.saturating_sub(1)),
                y: content
                    .y
                    .saturating_add(*cursor_row)
                    .min(content.y + content.height.saturating_sub(1)),
            })
        }
        LayoutJson::Leaf { .. } => None,
    }
}

fn pane_content_rect(area: Rect, hide_borders: bool) -> Rect {
    if !hide_borders && area.width > 2 && area.height > 2 {
        Rect {
            x: area.x + 1,
            y: area.y + 1,
            width: area.width - 2,
            height: area.height - 2,
        }
    } else {
        area
    }
}

pub fn active_window_index(fd: &FrameData) -> Option<usize> {
    fd.status.as_ref().and_then(|status| {
        status.windows.iter().find(|w| w.active).map(|w| w.index)
    })
}

/// Geometry fingerprint for every pane in the layout.
pub fn layout_geometry_fingerprint(
    layout: &LayoutJson,
) -> Vec<(usize, u16, u16)> {
    let mut panes = Vec::new();
    collect_pane_geometry(layout, &mut panes);
    panes.sort_by_key(|(id, _, _)| *id);
    panes
}

fn collect_pane_geometry(
    layout: &LayoutJson,
    out: &mut Vec<(usize, u16, u16)>,
) {
    match layout {
        LayoutJson::Split { children, .. } => {
            for child in children {
                collect_pane_geometry(child, out);
            }
        }
        LayoutJson::Leaf { id, rows, cols, .. } => {
            out.push((*id, *rows, *cols));
        }
    }
}

fn active_scroll_ratio_in_layout(layout: &LayoutJson) -> Option<f32> {
    match layout {
        LayoutJson::Split { children, .. } => {
            children.iter().find_map(active_scroll_ratio_in_layout)
        }
        LayoutJson::Leaf {
            active,
            scroll_ratio,
            ..
        } if *active => *scroll_ratio,
        LayoutJson::Leaf { .. } => None,
    }
}

fn active_in_copy_mode_in_layout(layout: &LayoutJson) -> bool {
    match layout {
        LayoutJson::Split { children, .. } => {
            children.iter().any(active_in_copy_mode_in_layout)
        }
        LayoutJson::Leaf {
            active,
            in_copy_mode,
            ..
        } if *active => *in_copy_mode,
        LayoutJson::Leaf { .. } => false,
    }
}

fn active_mouse_mode_in_layout(layout: &LayoutJson) -> u8 {
    match layout {
        LayoutJson::Split { children, .. } => children
            .iter()
            .map(active_mouse_mode_in_layout)
            .find(|&m| m != 0)
            .unwrap_or(0),
        LayoutJson::Leaf {
            active, mouse_mode, ..
        } if *active => *mouse_mode,
        LayoutJson::Leaf { .. } => 0,
    }
}

fn active_cursor_shape_in_layout(layout: &LayoutJson) -> Option<u8> {
    match layout {
        LayoutJson::Split { children, .. } => {
            children.iter().find_map(active_cursor_shape_in_layout)
        }
        LayoutJson::Leaf {
            active,
            hide_cursor,
            cursor_shape,
            ..
        } if *active && !*hide_cursor => Some(*cursor_shape),
        LayoutJson::Leaf { .. } => None,
    }
}

pub fn render_frame_ex(
    f: &mut Frame,
    fd: &FrameData,
    in_prefix: bool,
    hide_status: bool,
    hide_borders: bool,
) {
    render_frame_area_ex(f, fd, in_prefix, hide_status, hide_borders, f.area());
}

pub fn render_tabbed_frame(
    f: &mut Frame,
    fd: &FrameData,
    tabs: &[ClientTabView],
    tab_bar_offset: usize,
    in_prefix: bool,
    hide_status: bool,
    hide_borders: bool,
) {
    let area = f.area();
    if area.height == 0 {
        return;
    }
    let tab_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: 1,
    };
    f.render_widget(Clear, tab_area);
    render_tab_bar(f, tabs, tab_area, tab_bar_offset);
    if area.height <= 1 {
        return;
    }
    let frame_area = Rect {
        x: area.x,
        y: area.y + 1,
        width: area.width,
        height: area.height - 1,
    };
    render_frame_area_ex(
        f,
        fd,
        in_prefix,
        hide_status,
        hide_borders,
        frame_area,
    );
}

fn render_frame_area_ex(
    f: &mut Frame,
    fd: &FrameData,
    in_prefix: bool,
    hide_status: bool,
    hide_borders: bool,
    area: Rect,
) {
    if area.height < 2 {
        return;
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);

    if fd.ansi.is_none() {
        fill_area_terminal_bg(f, chunks[0], " ");
        render_layout_node(f, &fd.layout, chunks[0], hide_borders);
    }
    if !hide_status {
        fill_area_terminal_bg(f, chunks[1], " ");
        render_status_bar(f, &fd.status, chunks[1], in_prefix);
    }
}

fn render_layout_node(
    f: &mut Frame,
    layout: &LayoutJson,
    area: Rect,
    hide_borders: bool,
) {
    match layout {
        LayoutJson::Split {
            direction,
            sizes,
            children,
        } => {
            if children.is_empty() || area.width == 0 || area.height == 0 {
                return;
            }
            let chunks = split_layout_rects(
                area,
                direction,
                sizes,
                children.len(),
                hide_borders,
            );
            for (child, chunk) in children.iter().zip(chunks.iter()) {
                render_layout_node(f, child, *chunk, hide_borders);
            }
            if !hide_borders {
                fill_split_gaps(f, direction, &chunks, " ");
            }
        }
        LayoutJson::Leaf {
            rows_v2,
            active,
            cursor_row,
            cursor_col,
            hide_cursor,
            in_copy_mode,
            scroll_ratio,
            ..
        } => {
            render_pane_content(
                f,
                rows_v2,
                *active,
                *cursor_row,
                *cursor_col,
                *hide_cursor,
                *in_copy_mode,
                *scroll_ratio,
                area,
                hide_borders,
            );
        }
    }
}

fn split_layout_rects(
    area: Rect,
    direction: &str,
    sizes: &[u16],
    count: usize,
    hide_borders: bool,
) -> Vec<Rect> {
    if count == 0 {
        return Vec::new();
    }
    let horizontal = direction == "horizontal";
    let total_dim = if horizontal { area.width } else { area.height };
    let gap: u16 = if hide_borders { 0 } else { 1 };
    let borders = count.saturating_sub(1) as u16 * gap;
    let available = total_dim.saturating_sub(borders);
    let total_pct = sizes.iter().copied().sum::<u16>().max(1);
    let mut rects = Vec::with_capacity(count);
    let mut offset = 0u16;
    for (index, &pct) in sizes.iter().enumerate().take(count) {
        let dim = if index + 1 == count {
            available.saturating_sub(offset)
        } else {
            (available as u32 * pct as u32 / total_pct as u32) as u16
        };
        rects.push(if horizontal {
            Rect {
                x: area.x + offset,
                y: area.y,
                width: dim,
                height: area.height,
            }
        } else {
            Rect {
                x: area.x,
                y: area.y + offset,
                width: area.width,
                height: dim,
            }
        });
        offset += dim + gap;
    }
    rects
}

fn render_pane_content(
    f: &mut Frame,
    rows_v2: &[RowRunsJson],
    is_active: bool,
    cursor_row: u16,
    cursor_col: u16,
    hide_cursor: bool,
    in_copy_mode: bool,
    scroll_ratio: Option<f32>,
    area: Rect,
    hide_borders: bool,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let border_color = if is_active {
        Color::Green
    } else {
        Color::DarkGray
    };

    let (content_area, has_border) =
        if !hide_borders && area.width > 2 && area.height > 2 {
            let inner = Rect {
                x: area.x + 1,
                y: area.y + 1,
                width: area.width - 2,
                height: area.height - 2,
            };
            (inner, true)
        } else {
            (area, false)
        };

    // Scrub stale SGR backgrounds before drawing, using the terminal's own
    // default background (Color::Reset / \x1b[49m).
    fill_area_terminal_bg(f, content_area, " ");
    if has_border {
        draw_border(f, area, border_color);
    }

    let max_rows = content_area.height as usize;
    let max_cols = content_area.width as usize;

    let pad_style = Style::default();
    for row_idx in 0..max_rows {
        let y = content_area.y + row_idx as u16;
        let row_rect = Rect {
            x: content_area.x,
            y,
            width: max_cols as u16,
            height: 1,
        };

        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut col_used = 0usize;

        if let Some(row_data) = rows_v2.get(row_idx) {
            for run in &row_data.runs {
                if col_used >= max_cols {
                    break;
                }
                let available = max_cols - col_used;
                let text = truncate_to_width(&run.text, available);
                if text.is_empty() {
                    break;
                }
                let actual_width = unicode_display_width(&text);

                let fg = parse_color_str(&run.fg);
                let bg = parse_bg_color_str(&run.bg);
                let mut style = Style::default().fg(fg).bg(bg);
                if run.flags & 2 != 0 {
                    style = style.add_modifier(Modifier::BOLD);
                }
                if run.flags & 1 != 0 {
                    style = style.add_modifier(Modifier::DIM);
                }
                if run.flags & 4 != 0 {
                    style = style.add_modifier(Modifier::ITALIC);
                }
                if run.flags & 8 != 0 {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                if run.flags & 16 != 0 {
                    style = style.add_modifier(Modifier::REVERSED);
                }

                spans.push(Span::styled(text, style));
                col_used += actual_width;
            }
        }

        if col_used < max_cols {
            spans
                .push(Span::styled(" ".repeat(max_cols - col_used), pad_style));
        }
        let line = if spans.is_empty() {
            Line::from(Span::styled(" ".repeat(max_cols), pad_style))
        } else {
            Line::from(spans)
        };
        f.render_widget(Paragraph::new(line), row_rect);
    }

    if let Some(ratio) = scroll_ratio {
        draw_scrollbar(f, content_area, ratio);
    }

    if is_active && !hide_cursor && (in_copy_mode || scroll_ratio.is_none()) {
        let cx = content_area
            .x
            .saturating_add(cursor_col)
            .min(content_area.x + content_area.width.saturating_sub(1));
        let cy = content_area
            .y
            .saturating_add(cursor_row)
            .min(content_area.y + content_area.height.saturating_sub(1));
        f.set_cursor_position((cx, cy));
    }
}

fn draw_scrollbar(f: &mut Frame, content_area: Rect, ratio: f32) {
    let height = content_area.height as usize;
    if height < 3 {
        return;
    }
    let thumb_height = (height / 4).max(1);
    let track_range = height.saturating_sub(thumb_height);
    let thumb_top = (ratio.clamp(0.0, 1.0) * track_range as f32) as usize;
    let col = content_area.x + content_area.width.saturating_sub(1);
    for row in 0..height {
        let y = content_area.y + row as u16;
        let in_thumb = row >= thumb_top && row < thumb_top + thumb_height;
        let (ch, style) = if in_thumb {
            ("┃", Style::default().fg(Color::White).bg(Color::DarkGray))
        } else {
            ("│", Style::default().fg(Color::Rgb(60, 60, 60)))
        };
        let para = Paragraph::new(ch).style(style);
        f.render_widget(
            para,
            Rect {
                x: col,
                y,
                width: 1,
                height: 1,
            },
        );
    }
}

fn draw_border(f: &mut Frame, area: Rect, color: Color) {
    use ratatui::widgets::BorderType;

    if area.width < 2 || area.height < 2 {
        return;
    }

    // Draw only the perimeter cells.  The Block widget also calls
    // `buf.set_style` on the full rect, which can leave colored cells inside
    // the pane when row updates do not repaint every column.
    let style = Style::default().fg(color);
    let set = BorderType::Plain.to_border_set();
    let buf = f.buffer_mut();
    let left = area.left();
    let top = area.top();
    let right = area.right().saturating_sub(1);
    let bottom = area.bottom().saturating_sub(1);

    buf[(left, top)].set_symbol(set.top_left).set_style(style);
    buf[(right, top)].set_symbol(set.top_right).set_style(style);
    buf[(left, bottom)]
        .set_symbol(set.bottom_left)
        .set_style(style);
    buf[(right, bottom)]
        .set_symbol(set.bottom_right)
        .set_style(style);

    for x in (left + 1)..right {
        buf[(x, top)]
            .set_symbol(set.horizontal_top)
            .set_style(style);
        buf[(x, bottom)]
            .set_symbol(set.horizontal_bottom)
            .set_style(style);
    }
    for y in (top + 1)..bottom {
        buf[(left, y)]
            .set_symbol(set.vertical_left)
            .set_style(style);
        buf[(right, y)]
            .set_symbol(set.vertical_right)
            .set_style(style);
    }
}

pub fn render_tabbed_loading(
    f: &mut Frame,
    tabs: &[ClientTabView],
    tab_bar_offset: usize,
) {
    let area = f.area();
    if area.height == 0 {
        return;
    }
    let tab_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: 1,
    };
    render_tab_bar(f, tabs, tab_area, tab_bar_offset);
    if area.height <= 1 {
        return;
    }
    let body = Rect {
        x: area.x,
        y: area.y + 1,
        width: area.width,
        height: area.height - 1,
    };
    let para = Paragraph::new(" Starting zmux...")
        .style(Style::default().fg(Color::Yellow));
    f.render_widget(para, body);
}

pub fn status_bar_screen_row(
    screen_rows: u16,
    hide_status: bool,
) -> Option<u16> {
    if hide_status || screen_rows < 2 {
        None
    } else {
        Some(screen_rows - 1)
    }
}

pub fn status_window_tab_hit(
    status: &StatusJson,
    _width: u16,
    col: u16,
) -> Option<usize> {
    let col = col as usize;
    let mut used = 0usize;

    let left = format!(" {} ", status.left);
    used += unicode_display_width(&left);

    for w in &status.windows {
        let label = if w.active {
            format!(" *[{}] {}* ", w.index, w.name)
        } else {
            format!("  [{}] {}  ", w.index, w.name)
        };
        let label_width = unicode_display_width(&label);
        if col >= used && col < used + label_width {
            return Some(w.index);
        }
        used += label_width;
    }
    None
}

const TAB_BAR_ELLIPSIS: &str = " ... ";

pub fn tab_bar_visible_range(
    tabs: &[ClientTabView],
    width: u16,
    offset: usize,
) -> std::ops::Range<usize> {
    if tabs.is_empty() || offset >= tabs.len() {
        return 0..0;
    }
    let max_width = width as usize;
    let ellipsis_width = unicode_display_width(TAB_BAR_ELLIPSIS);
    let mut used = if offset > 0 { ellipsis_width } else { 0 };
    let mut end = offset;
    while end < tabs.len() {
        let last = end + 1 == tabs.len();
        let segment_width = tab_segment_width(&tabs[end]);
        if last {
            if used + segment_width <= max_width {
                return offset..tabs.len();
            }
            break;
        }
        if used + segment_width + ellipsis_width > max_width {
            break;
        }
        used += segment_width;
        end += 1;
    }
    if end == offset && offset < tabs.len() {
        return offset..(offset + 1).min(tabs.len());
    }
    offset..end
}

pub fn tab_bar_hit(
    tabs: &[ClientTabView],
    width: u16,
    col: u16,
    offset: usize,
) -> Option<ClientTabBarHit> {
    if tabs.is_empty() {
        return None;
    }
    let mut used = 0usize;
    let max_width = width as usize;
    let col = col as usize;
    let ellipsis_width = unicode_display_width(TAB_BAR_ELLIPSIS);
    let visible = tab_bar_visible_range(tabs, width, offset);
    let has_trailing = visible.end < tabs.len();

    if offset > 0 {
        if used + ellipsis_width > max_width {
            return None;
        }
        if col < used + ellipsis_width {
            return Some(ClientTabBarHit::OverflowStart);
        }
        used += ellipsis_width;
    }

    for index in visible.start..visible.end {
        let segment_width = tab_segment_width(&tabs[index]);
        if col >= used && col < used + segment_width {
            return Some(ClientTabBarHit::Tab(index));
        }
        used += segment_width;
    }

    if has_trailing && used + ellipsis_width <= max_width {
        (col >= used && col < used + ellipsis_width)
            .then_some(ClientTabBarHit::OverflowEnd)
    } else {
        None
    }
}

pub fn last_visible_tab_index(
    tabs: &[ClientTabView],
    width: u16,
    offset: usize,
) -> Option<usize> {
    let range = tab_bar_visible_range(tabs, width, offset);
    range.end.checked_sub(1)
}

fn render_tab_bar(
    f: &mut Frame,
    tabs: &[ClientTabView],
    area: Rect,
    offset: usize,
) {
    if area.height == 0 {
        return;
    }
    let bg = Style::default().fg(Color::White).bg(Color::Rgb(28, 28, 28));
    f.render_widget(
        Paragraph::new(" ".repeat(area.width as usize)).style(bg),
        area,
    );
    if tabs.is_empty() {
        return;
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    let max_width = area.width as usize;
    let ellipsis_width = unicode_display_width(TAB_BAR_ELLIPSIS);
    let visible = tab_bar_visible_range(tabs, area.width, offset);
    let has_trailing = visible.end < tabs.len();

    if offset > 0 && used + ellipsis_width <= max_width {
        spans.push(Span::styled(TAB_BAR_ELLIPSIS.to_string(), bg));
        used += ellipsis_width;
    }

    for index in visible.start..visible.end {
        push_tab_spans(&mut spans, &tabs[index]);
        used += tab_segment_width(&tabs[index]);
    }

    if has_trailing && used + ellipsis_width <= max_width {
        spans.push(Span::styled(TAB_BAR_ELLIPSIS.to_string(), bg));
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

const PANEL_BG: Color = Color::Rgb(40, 40, 40);

/// Server ANSI marks pane cells as Skip; overlays must opt back into drawing.
fn prepare_overlay_draw(f: &mut Frame, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let buf = f.buffer_mut();
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            buf[(x, y)].set_diff_option(CellDiffOption::AlwaysUpdate);
        }
    }
}

fn fill_rect_background(f: &mut Frame, area: Rect, bg: Color) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let style = Style::default().fg(bg).bg(bg);
    let line = " ".repeat(area.width as usize);
    for row in 0..area.height {
        f.render_widget(
            Paragraph::new(line.clone()).style(style),
            Rect {
                x: area.x,
                y: area.y + row,
                width: area.width,
                height: 1,
            },
        );
    }
}

fn begin_floating_panel(f: &mut Frame, panel: Rect) {
    prepare_overlay_draw(f, panel);
    fill_rect_background(f, panel, PANEL_BG);
}

/// Erase a ratatui-drawn floating overlay region.
///
/// Do not call this after `write_server_ansi` in the same frame: it paints spaces
/// over pane borders/split gaps. Overlay transitions should rely on
/// `last_drawn_counter = 0` to refresh server ANSI instead.
#[allow(dead_code)]
pub fn clear_floating_overlay_rect(f: &mut Frame, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    prepare_overlay_draw(f, area);
    let line = " ".repeat(area.width as usize);
    let style = Style::default();
    for row in 0..area.height {
        f.render_widget(
            Paragraph::new(line.clone()).style(style),
            Rect {
                x: area.x,
                y: area.y + row,
                width: area.width,
                height: 1,
            },
        );
    }
}

/// Centered overlay rect with stable dimensions so expand/collapse does not move
/// the panel (moving leaves stale ratatui cells on ANSI-backed panes).
pub fn centered_floating_panel(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.max(1).min(area.width);
    let height = height.max(1).min(area.height);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect {
        x,
        y,
        width,
        height,
    }
}

/// Shared overlay size for tab/session choosers: ~75% width, capped height, stable
/// while open (must not depend on list length or tree expand state).
pub fn chooser_overlay_panel(area: Rect) -> Rect {
    if area.width == 0 || area.height == 0 {
        return area;
    }
    let width = area
        .width
        .saturating_mul(3)
        .saturating_div(4)
        .max(36)
        .min(area.width);
    let height = area
        .height
        .saturating_sub(2)
        .max(8)
        .min(24)
        .min(area.height);
    centered_floating_panel(area, width, height)
}

pub fn rename_tab_panel_rect(area: Rect) -> Rect {
    if area.width == 0 || area.height == 0 {
        return area;
    }
    let width = area
        .width
        .saturating_mul(2)
        .saturating_div(3)
        .max(44)
        .min(area.width);
    let height = 9.min(area.height).max(1);
    centered_floating_panel(area, width, height)
}

pub fn tab_quick_switch_panel_rect(area: Rect) -> Rect {
    if area.width == 0 || area.height == 0 {
        return area;
    }
    let width = area
        .width
        .saturating_mul(2)
        .saturating_div(5)
        .max(36)
        .min(area.width);
    let height = 7.min(area.height).max(1);
    centered_floating_panel(area, width, height)
}

pub fn options_panel_rect(area: Rect) -> Rect {
    if area.width == 0 || area.height == 0 {
        return area;
    }
    let width = area
        .width
        .saturating_mul(2)
        .saturating_div(3)
        .max(64)
        .min(area.width);
    let height = 7.min(area.height).max(1);
    centered_floating_panel(area, width, height)
}

fn fill_panel_row(f: &mut Frame, x: u16, y: u16, width: u16) {
    if width == 0 {
        return;
    }
    let padded = " ".repeat(width as usize);
    f.render_widget(
        Paragraph::new(padded).style(Style::default().bg(PANEL_BG)),
        Rect {
            x,
            y,
            width,
            height: 1,
        },
    );
}

fn tab_segment_width(tab: &ClientTabView) -> usize {
    let title_width = if tab.title.is_empty() {
        0
    } else {
        1 + unicode_display_width(&tab.title)
    };
    2 + 1 + title_width + 1
}

fn push_tab_spans(spans: &mut Vec<Span<'static>>, tab: &ClientTabView) {
    let active = tab.state == ClientTabState::Active;
    let base_bg = if active {
        Color::Rgb(42, 72, 120)
    } else {
        Color::Rgb(48, 48, 48)
    };
    let code_style = Style::default()
        .fg(Color::White)
        .bg(base_bg)
        .add_modifier(Modifier::BOLD);
    let text_style = Style::default().fg(Color::White).bg(base_bg);
    let (status, status_style) = match tab.state {
        ClientTabState::Active => (
            "*",
            Style::default()
                .fg(Color::Green)
                .bg(base_bg)
                .add_modifier(Modifier::BOLD),
        ),
        ClientTabState::Inactive => {
            ("-", Style::default().fg(Color::DarkGray).bg(base_bg))
        }
        ClientTabState::Dead => (
            "!",
            Style::default()
                .fg(Color::Red)
                .bg(base_bg)
                .add_modifier(Modifier::BOLD),
        ),
    };
    spans.push(Span::styled(tab.code.clone(), code_style));
    spans.push(Span::styled(status.to_string(), status_style));
    if !tab.title.is_empty() {
        spans.push(Span::styled(format!(" {}", tab.title), text_style));
    }
    spans.push(Span::styled(
        " ".to_string(),
        Style::default().bg(Color::Rgb(28, 28, 28)),
    ));
}

pub fn render_tab_chooser(
    f: &mut Frame,
    tabs: &[ClientTabView],
    query: &str,
    selected: usize,
    search_active: bool,
) {
    use ratatui::widgets::{Block, Borders};

    let area = f.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let panel = chooser_overlay_panel(area);
    begin_floating_panel(f, panel);
    let block = Block::default()
        .title(" Tabs  Space=show/hide  K=kill  R=rename  Enter=switch  /=search  Esc/q=close ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .style(Style::default().bg(PANEL_BG));
    let inner = block.inner(panel);
    f.render_widget(block, panel);
    if inner.height == 0 {
        return;
    }

    let search = if search_active {
        format!(" search: {}", query)
    } else if query.is_empty() {
        " search: <press / or ?>".to_string()
    } else {
        format!(" search: {}", query)
    };
    let search_style = if search_active {
        Style::default()
            .fg(Color::Yellow)
            .bg(PANEL_BG)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray).bg(PANEL_BG)
    };
    f.render_widget(
        Paragraph::new(truncate_to_width(&search, inner.width as usize))
            .style(search_style),
        Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: 1,
        },
    );

    let matches: Vec<(usize, &ClientTabView)> = tabs
        .iter()
        .enumerate()
        .filter(|(_, tab)| tab_matches(tab, query))
        .collect();
    if inner.height <= 1 {
        return;
    }
    if matches.is_empty() {
        f.render_widget(
            Paragraph::new(" no matching tabs")
                .style(Style::default().fg(Color::DarkGray).bg(PANEL_BG)),
            Rect {
                x: inner.x,
                y: inner.y + 1,
                width: inner.width,
                height: 1,
            },
        );
        return;
    }

    let list_height = inner.height.saturating_sub(1) as usize;
    let selected = selected.min(matches.len().saturating_sub(1));
    let scroll = selected.saturating_sub(list_height.saturating_sub(1));
    for (row, (_, tab)) in
        matches.iter().skip(scroll).take(list_height).enumerate()
    {
        let y = inner.y + 1 + row as u16;
        let is_selected = scroll + row == selected;
        let state = match tab.state {
            ClientTabState::Active => "*",
            ClientTabState::Inactive => "-",
            ClientTabState::Dead => "!",
        };
        let title = if tab.title.is_empty() {
            "(untitled)"
        } else {
            &tab.title
        };
        let checkbox = if tab.visible { "[x]" } else { "[ ]" };
        let label = format!(" {} {}{} {}", checkbox, tab.code, state, title);
        let style = if is_selected {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White).bg(PANEL_BG)
        };
        let text = format!(
            "{:<width$}",
            truncate_to_width(&label, inner.width as usize),
            width = inner.width as usize
        );
        f.render_widget(
            Paragraph::new(text).style(style),
            Rect {
                x: inner.x,
                y,
                width: inner.width,
                height: 1,
            },
        );
    }
}

fn tab_matches(tab: &ClientTabView, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    query.is_empty()
        || tab.code.to_lowercase().contains(&query)
        || tab.title.to_lowercase().contains(&query)
        || tab.socket_name.to_lowercase().contains(&query)
}

pub fn render_rename_tab_panel(
    f: &mut Frame,
    code: &str,
    title: &str,
    editing_code: bool,
    error: Option<&str>,
) {
    use ratatui::widgets::{Block, Borders};

    let area = f.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let panel = rename_tab_panel_rect(area);
    begin_floating_panel(f, panel);
    let block = Block::default()
        .title(" Rename Tab  (Tab=switch field  Enter=next/save  Esc=cancel) ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .style(Style::default().bg(PANEL_BG));
    let inner = block.inner(panel);
    f.render_widget(block, panel);
    if inner.height == 0 {
        return;
    }

    render_rename_field(f, inner, 0, "Code", code, editing_code);
    if inner.height > 2 {
        render_rename_field(f, inner, 2, "Title", title, !editing_code);
    }
    if let Some(error) = error {
        if inner.height > 4 {
            f.render_widget(
                Paragraph::new(truncate_to_width(error, inner.width as usize))
                    .style(Style::default().fg(Color::Red)),
                Rect {
                    x: inner.x,
                    y: inner.y + 4,
                    width: inner.width,
                    height: 1,
                },
            );
        }
    }
    if inner.height > 6 {
        let help = "Code must be two unique uppercase letters.";
        f.render_widget(
            Paragraph::new(truncate_to_width(help, inner.width as usize))
                .style(Style::default().fg(Color::DarkGray)),
            Rect {
                x: inner.x,
                y: inner.y + 6,
                width: inner.width,
                height: 1,
            },
        );
    }
}

pub fn render_tab_quick_switch_panel(
    f: &mut Frame,
    code: &str,
    error: Option<&str>,
) {
    use ratatui::widgets::{Block, Borders};

    let area = f.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let panel = tab_quick_switch_panel_rect(area);
    begin_floating_panel(f, panel);
    let block = Block::default()
        .title(" Switch Tab  (Enter=switch  Esc=cancel) ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .style(Style::default().bg(PANEL_BG));
    let inner = block.inner(panel);
    f.render_widget(block, panel);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let field = format!(" Code: {}", code);
    let field_text = format!(
        "{:<width$}",
        truncate_to_width(&field, inner.width as usize),
        width = inner.width as usize
    );
    f.render_widget(
        Paragraph::new(field_text).style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: 1,
        },
    );
    let cursor_x = inner
        .x
        .saturating_add(7)
        .saturating_add(code.chars().count() as u16)
        .min(inner.x + inner.width.saturating_sub(1));
    f.set_cursor_position((cursor_x, inner.y));

    if let Some(error) = error {
        if inner.height > 2 {
            f.render_widget(
                Paragraph::new(truncate_to_width(error, inner.width as usize))
                    .style(Style::default().fg(Color::Red)),
                Rect {
                    x: inner.x,
                    y: inner.y + 2,
                    width: inner.width,
                    height: 1,
                },
            );
        }
    }

    if inner.height > 4 {
        let help = "Enter the two-letter code shown in the tab bar.";
        f.render_widget(
            Paragraph::new(truncate_to_width(help, inner.width as usize))
                .style(Style::default().fg(Color::DarkGray)),
            Rect {
                x: inner.x,
                y: inner.y + 4,
                width: inner.width,
                height: 1,
            },
        );
    }
}

fn render_rename_field(
    f: &mut Frame,
    area: Rect,
    row: u16,
    label: &str,
    value: &str,
    active: bool,
) {
    if row >= area.height {
        return;
    }
    let text = format!(" {}: {}", label, value);
    let style = if active {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    let padded = format!(
        "{:<width$}",
        truncate_to_width(&text, area.width as usize),
        width = area.width as usize
    );
    f.render_widget(
        Paragraph::new(padded).style(style),
        Rect {
            x: area.x,
            y: area.y + row,
            width: area.width,
            height: 1,
        },
    );
}

fn render_status_bar(
    f: &mut Frame,
    status: &Option<StatusJson>,
    area: Rect,
    in_prefix: bool,
) {
    if area.height == 0 {
        return;
    }

    let bg_style = Style::default().fg(Color::White).bg(Color::Rgb(40, 40, 40));
    let blank = Paragraph::new(" ".repeat(area.width as usize)).style(bg_style);
    f.render_widget(blank, area);

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    let prefix_label = " PREFIX ";
    let prefix_width = if in_prefix {
        unicode_display_width(prefix_label)
    } else {
        0
    };

    if let Some(s) = status {
        let left = format!(" {} ", s.left);
        used += unicode_display_width(&left);
        spans.push(Span::styled(
            left,
            Style::default()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ));

        for w in &s.windows {
            let label = if w.active {
                format!(" *[{}] {}* ", w.index, w.name)
            } else {
                format!("  [{}] {}  ", w.index, w.name)
            };
            used += unicode_display_width(&label);
            let tab_style = if w.active {
                Style::default()
                    .fg(Color::White)
                    .bg(Color::Blue)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White).bg(Color::Rgb(60, 60, 60))
            };
            spans.push(Span::styled(label, tab_style));
        }

        let mut suffix: Vec<Span<'static>> = Vec::new();
        let mut suffix_width = prefix_width;
        if !s.right.trim().is_empty() {
            let available =
                (area.width as usize).saturating_sub(used + prefix_width);
            let text = truncate_left_to_width(
                s.right.trim(),
                available.saturating_sub(2),
            );
            if !text.is_empty() {
                let right = format!(" {} ", text);
                suffix_width += unicode_display_width(&right);
                suffix.push(Span::styled(
                    right,
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ));
            }
        }
        if in_prefix {
            suffix.push(Span::styled(
                prefix_label.to_string(),
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        let pad = (area.width as usize).saturating_sub(used + suffix_width);
        if pad > 0 {
            spans.push(Span::styled(
                " ".repeat(pad),
                Style::default().bg(Color::Rgb(40, 40, 40)),
            ));
        }
        spans.extend(suffix);
    } else {
        let label = " [zmux] ".to_string();
        used += unicode_display_width(&label);
        spans.push(Span::styled(
            label,
            Style::default()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ));
        if in_prefix {
            let pad = (area.width as usize).saturating_sub(used + prefix_width);
            if pad > 0 {
                spans.push(Span::styled(
                    " ".repeat(pad),
                    Style::default().bg(Color::Rgb(40, 40, 40)),
                ));
            }
            spans.push(Span::styled(
                prefix_label.to_string(),
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));
        }
    }

    let para = Paragraph::new(Line::from(spans));
    f.render_widget(para, area);
}

fn truncate_to_width(s: &str, max_display_cols: usize) -> String {
    if max_display_cols == 0 {
        return String::new();
    }
    let mut result = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let w = unicode_char_width(ch);
        if used + w > max_display_cols {
            break;
        }
        result.push(ch);
        used += w;
    }
    result
}

fn truncate_left_to_width(s: &str, max_display_cols: usize) -> String {
    if max_display_cols == 0 {
        return String::new();
    }
    if unicode_display_width(s) <= max_display_cols {
        return s.to_string();
    }
    if max_display_cols == 1 {
        return "…".to_string();
    }
    let mut tail = String::new();
    let mut used = 1usize;
    for ch in s.chars().rev() {
        let w = unicode_char_width(ch);
        if used + w > max_display_cols {
            break;
        }
        tail.insert(0, ch);
        used += w;
    }
    format!("…{}", tail)
}

fn unicode_display_width(s: &str) -> usize {
    s.chars().map(unicode_char_width).sum()
}

fn unicode_char_width(c: char) -> usize {
    use unicode_width::UnicodeWidthChar;
    c.width().unwrap_or(1)
}

fn fill_area_terminal_bg(f: &mut Frame, area: Rect, pad_fill: &str) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let style = Style::default();
    let line_text = pad_fill.repeat(area.width as usize);
    for row in 0..area.height {
        let row_rect = Rect {
            x: area.x,
            y: area.y + row,
            width: area.width,
            height: 1,
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(line_text.clone(), style))),
            row_rect,
        );
    }
}

fn fill_split_gaps(
    f: &mut Frame,
    direction: &str,
    chunks: &[Rect],
    pad_fill: &str,
) {
    if chunks.len() < 2 {
        return;
    }
    let horizontal = direction == "horizontal";
    for pair in chunks.windows(2) {
        let gap = if horizontal {
            Rect {
                x: pair[0].x.saturating_add(pair[0].width),
                y: pair[0].y,
                width: pair[1]
                    .x
                    .saturating_sub(pair[0].x.saturating_add(pair[0].width)),
                height: pair[0].height,
            }
        } else {
            Rect {
                x: pair[0].x,
                y: pair[0].y.saturating_add(pair[0].height),
                width: pair[0].width,
                height: pair[1]
                    .y
                    .saturating_sub(pair[0].y.saturating_add(pair[0].height)),
            }
        };
        fill_area_terminal_bg(f, gap, pad_fill);
    }
}

fn parse_bg_color_str(s: &str) -> Color {
    match s {
        "default" | "" => Color::Reset,
        s if s.starts_with("idx:") => s[4..]
            .parse::<u8>()
            .map(Color::Indexed)
            .unwrap_or(Color::Reset),
        s if s.starts_with("rgb:") => {
            let parts: Vec<u8> = s[4..]
                .splitn(3, ',')
                .filter_map(|x| x.parse().ok())
                .collect();
            if parts.len() == 3 {
                Color::Rgb(parts[0], parts[1], parts[2])
            } else {
                Color::Reset
            }
        }
        s => crate::style::parse_color(s),
    }
}

fn parse_color_str(s: &str) -> Color {
    match s {
        "default" | "" => Color::Reset,
        s if s.starts_with("idx:") => s[4..]
            .parse::<u8>()
            .map(Color::Indexed)
            .unwrap_or(Color::Reset),
        s if s.starts_with("rgb:") => {
            let parts: Vec<u8> = s[4..]
                .splitn(3, ',')
                .filter_map(|x| x.parse().ok())
                .collect();
            if parts.len() == 3 {
                Color::Rgb(parts[0], parts[1], parts[2])
            } else {
                Color::Reset
            }
        }
        s => crate::style::parse_color(s),
    }
}

pub fn render_loading(f: &mut Frame) {
    let area = f.area();
    let para = Paragraph::new(" Starting zmux...")
        .style(Style::default().fg(Color::Yellow));
    f.render_widget(para, area);
}

pub fn render_prompt(f: &mut Frame, label: &str, buf: &str) {
    let area = f.area();
    if area.height < 1 {
        return;
    }

    let prompt_area = Rect {
        x: area.x,
        y: area.y + area.height - 1,
        width: area.width,
        height: 1,
    };

    let content = format!("{}{}", label, buf);
    let para = Paragraph::new(content.clone())
        .style(Style::default().fg(Color::Black).bg(Color::Yellow));
    f.render_widget(para, prompt_area);

    let cursor_x =
        (prompt_area.x + label.len() as u16 + buf.chars().count() as u16)
            .min(prompt_area.x + prompt_area.width - 1);
    f.set_cursor_position((cursor_x, prompt_area.y));
}

pub fn render_options_panel(
    f: &mut Frame,
    selected: usize,
    scroll_on_erase_in_display: bool,
) {
    use ratatui::widgets::{Block, Borders};

    let area = f.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let panel_area = options_panel_rect(area);

    begin_floating_panel(f, panel_area);
    let block = Block::default()
        .title(" Options  (Space/Enter=toggle  q/Esc=close) ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .style(Style::default().bg(PANEL_BG));
    let inner = block.inner(panel_area);
    f.render_widget(block, panel_area);

    let state = if scroll_on_erase_in_display {
        "on"
    } else {
        "off"
    };
    let mark = if scroll_on_erase_in_display {
        "[x]"
    } else {
        "[ ]"
    };
    let label = format!(
        " {} scrollOnEraseInDisplay compatibility mode: {}",
        mark, state
    );
    let style = if selected == 0 {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    let padded = format!("{:<width$}", label, width = inner.width as usize);
    f.render_widget(
        Paragraph::new(padded).style(style),
        Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: 1,
        },
    );

    if inner.height > 2 {
        let help = " Normal buffer CSI 2J scrolls content into history instead of cutting the screen.";
        let help = truncate_to_width(help, inner.width as usize);
        f.render_widget(
            Paragraph::new(help).style(Style::default().fg(Color::DarkGray)),
            Rect {
                x: inner.x,
                y: inner.y + 2,
                width: inner.width,
                height: 1,
            },
        );
    }
}

pub fn render_session_chooser(
    f: &mut Frame,
    entries: &[crate::server::SessionTreeEntry],
    selected: usize,
    collapsed: &std::collections::HashSet<String>,
    collapsed_windows: &std::collections::HashSet<(String, usize)>,
) {
    use ratatui::widgets::{Block, Borders};

    use crate::server::SessionTreeEntry;

    let visible: Vec<&SessionTreeEntry> = entries
        .iter()
        .filter(|e| match e {
            SessionTreeEntry::Session { .. } => true,
            SessionTreeEntry::Window { session_name, .. } => {
                !collapsed.contains(session_name)
            }
            SessionTreeEntry::Pane {
                session_name,
                window_index,
                ..
            } => {
                !collapsed.contains(session_name)
                    && !collapsed_windows
                        .contains(&(session_name.clone(), *window_index))
            }
        })
        .collect();

    let area = f.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let chooser_area = chooser_overlay_panel(area);

    begin_floating_panel(f, chooser_area);
    let block = Block::default()
        .title(" Sessions  (Enter=select  q/Esc=close  j/k=nav  l=expand  h=collapse) ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .style(Style::default().bg(PANEL_BG));
    let inner = block.inner(chooser_area);
    f.render_widget(block, chooser_area);

    let view_height = inner.height as usize;
    let scroll = if view_height == 0 {
        0
    } else if selected >= view_height {
        selected + 1 - view_height
    } else {
        0
    }
    .min(visible.len().saturating_sub(view_height));

    let mut drawn_rows = 0usize;
    for row in 0..view_height {
        let i = scroll + row;
        let Some(entry) = visible.get(i) else {
            break;
        };
        drawn_rows = row + 1;
        let row_y = inner.y + row as u16;
        let is_sel = i == selected;

        let (label, style) = match entry {
            SessionTreeEntry::Session {
                name,
                window_count,
                is_active,
            } => {
                let active_mark = if *is_active { "*" } else { " " };
                let expand_mark = if collapsed.contains(name) {
                    "▶"
                } else {
                    "▼"
                };
                let text = format!(
                    " {} {} [{}]  {} windows",
                    active_mark, expand_mark, name, window_count
                );
                let s = if is_sel {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else if *is_active {
                    Style::default()
                        .fg(Color::Green)
                        .bg(PANEL_BG)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White).bg(PANEL_BG)
                };
                (text, s)
            }
            SessionTreeEntry::Window {
                session_name,
                index,
                name,
                pane_count,
                is_active,
            } => {
                let active_mark = if *is_active { ">" } else { " " };
                let key_w = (session_name.clone(), *index);
                let expand_mark = if collapsed_windows.contains(&key_w) {
                    "▶"
                } else {
                    "▼"
                };
                let text = format!(
                    "     {} {} [{}] {}  ({} panes)",
                    active_mark, expand_mark, index, name, pane_count
                );
                let s = if is_sel {
                    Style::default().fg(Color::Black).bg(Color::Cyan)
                } else if *is_active {
                    Style::default().fg(Color::Blue).bg(PANEL_BG)
                } else {
                    Style::default().fg(Color::White).bg(PANEL_BG)
                };
                (text, s)
            }
            SessionTreeEntry::Pane {
                index, is_active, ..
            } => {
                let active_mark = if *is_active { "●" } else { "○" };
                let text = format!("           {} pane {}", active_mark, index);
                let s = if is_sel {
                    Style::default().fg(Color::Black).bg(Color::Cyan)
                } else if *is_active {
                    Style::default().fg(Color::Yellow).bg(PANEL_BG)
                } else {
                    Style::default().fg(Color::DarkGray).bg(PANEL_BG)
                };
                (text, s)
            }
        };

        let padded = format!("{:<width$}", label, width = inner.width as usize);
        let para = Paragraph::new(padded).style(style);
        f.render_widget(
            para,
            Rect {
                x: inner.x,
                y: row_y,
                width: inner.width,
                height: 1,
            },
        );
    }
    for row in drawn_rows..view_height {
        fill_panel_row(f, inner.x, inner.y + row as u16, inner.width);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chooser_overlay_panel_is_stable_for_same_terminal_size() {
        let area = Rect::new(0, 1, 120, 20);
        let first = chooser_overlay_panel(area);
        let second = chooser_overlay_panel(area);
        assert_eq!(first, second);
        assert_eq!(first.width, 90);
        assert_eq!(first.height, 18);
    }

    #[test]
    fn rename_panel_is_smaller_than_tab_chooser_panel() {
        let area = Rect::new(0, 0, 120, 24);
        let chooser = chooser_overlay_panel(area);
        let rename = rename_tab_panel_rect(area);
        assert!(rename.width <= chooser.width);
        assert!(rename.height < chooser.height);
        assert_ne!(chooser, rename);
    }

    #[test]
    fn status_window_tab_hit_matches_rendered_labels() {
        let status = StatusJson {
            left: "[main]".to_string(),
            right: String::new(),
            windows: vec![
                WindowTabJson {
                    index: 0,
                    name: "zsh".to_string(),
                    active: true,
                },
                WindowTabJson {
                    index: 1,
                    name: "vim".to_string(),
                    active: false,
                },
            ],
        };
        let left_width = unicode_display_width(" [main] ");
        let first_width = unicode_display_width(" *[0] zsh* ");
        assert_eq!(
            status_window_tab_hit(&status, 120, left_width as u16),
            Some(0)
        );
        assert_eq!(
            status_window_tab_hit(
                &status,
                120,
                (left_width + first_width + 1) as u16
            ),
            Some(1)
        );
    }

    #[test]
    fn split_layout_rects_keeps_server_gap_rules() {
        let rects = split_layout_rects(
            Rect {
                x: 0,
                y: 0,
                width: 80,
                height: 23,
            },
            "vertical",
            &[50, 50],
            2,
            false,
        );
        assert_eq!(
            rects,
            vec![
                Rect {
                    x: 0,
                    y: 0,
                    width: 80,
                    height: 11,
                },
                Rect {
                    x: 0,
                    y: 12,
                    width: 80,
                    height: 10,
                },
            ]
        );
    }

    #[test]
    fn active_cursor_shape_returns_active_leaf_shape() {
        let fd = FrameData {
            frame_type: "frame".to_string(),
            layout: LayoutJson::Split {
                direction: "horizontal".to_string(),
                sizes: vec![50, 50],
                children: vec![
                    LayoutJson::Leaf {
                        id: 1,
                        rows: 10,
                        cols: 10,
                        cursor_row: 0,
                        cursor_col: 0,
                        hide_cursor: false,
                        alternate_screen: false,
                        mouse_mode: 0,
                        in_copy_mode: false,
                        scroll_ratio: None,
                        cursor_shape: 2,
                        active: false,
                        rows_v2: Vec::new(),
                        title: None,
                    },
                    LayoutJson::Leaf {
                        id: 2,
                        rows: 10,
                        cols: 10,
                        cursor_row: 0,
                        cursor_col: 0,
                        hide_cursor: false,
                        alternate_screen: false,
                        mouse_mode: 0,
                        in_copy_mode: false,
                        scroll_ratio: None,
                        cursor_shape: 4,
                        active: true,
                        rows_v2: Vec::new(),
                        title: None,
                    },
                ],
            },
            status: None,
            ansi: None,
            exit: false,
            yank_text: None,
        };

        assert_eq!(active_cursor_shape(&fd), Some(4));
    }

    #[test]
    fn layout_geometry_fingerprint_tracks_all_panes() {
        let layout = LayoutJson::Split {
            direction: "horizontal".to_string(),
            sizes: vec![50, 50],
            children: vec![
                LayoutJson::Leaf {
                    id: 1,
                    rows: 20,
                    cols: 40,
                    cursor_row: 0,
                    cursor_col: 0,
                    hide_cursor: false,
                    alternate_screen: false,
                    mouse_mode: 0,
                    in_copy_mode: false,
                    scroll_ratio: None,
                    cursor_shape: 0,
                    active: true,
                    rows_v2: Vec::new(),
                    title: None,
                },
                LayoutJson::Leaf {
                    id: 2,
                    rows: 20,
                    cols: 39,
                    cursor_row: 0,
                    cursor_col: 0,
                    hide_cursor: false,
                    alternate_screen: false,
                    mouse_mode: 0,
                    in_copy_mode: false,
                    scroll_ratio: None,
                    cursor_shape: 0,
                    active: false,
                    rows_v2: Vec::new(),
                    title: None,
                },
            ],
        };
        assert_eq!(
            layout_geometry_fingerprint(&layout),
            vec![(1, 20, 40), (2, 20, 39)]
        );
    }

    #[test]
    fn parse_bg_color_str_maps_default_to_terminal_reset() {
        assert_eq!(parse_bg_color_str("default"), Color::Reset);
        assert_eq!(parse_bg_color_str(""), Color::Reset);
        assert_eq!(parse_bg_color_str("green"), Color::Green);
    }

    #[test]
    fn active_window_index_returns_active_tab() {
        let fd = FrameData {
            frame_type: "frame".to_string(),
            layout: LayoutJson::Leaf {
                id: 1,
                rows: 10,
                cols: 10,
                cursor_row: 0,
                cursor_col: 0,
                hide_cursor: false,
                alternate_screen: false,
                mouse_mode: 0,
                in_copy_mode: false,
                scroll_ratio: None,
                cursor_shape: 0,
                active: true,
                rows_v2: Vec::new(),
                title: None,
            },
            status: Some(StatusJson {
                left: "[main]".to_string(),
                right: String::new(),
                windows: vec![
                    WindowTabJson {
                        index: 0,
                        name: "zsh".to_string(),
                        active: false,
                    },
                    WindowTabJson {
                        index: 1,
                        name: "shell".to_string(),
                        active: true,
                    },
                ],
            }),
            ansi: None,
            exit: false,
            yank_text: None,
        };

        assert_eq!(active_window_index(&fd), Some(1));
    }

    #[test]
    fn active_cursor_shape_ignores_hidden_cursor() {
        let fd = FrameData {
            frame_type: "frame".to_string(),
            layout: LayoutJson::Leaf {
                id: 1,
                rows: 10,
                cols: 10,
                cursor_row: 0,
                cursor_col: 0,
                hide_cursor: true,
                alternate_screen: false,
                mouse_mode: 0,
                in_copy_mode: false,
                scroll_ratio: None,
                cursor_shape: 6,
                active: true,
                rows_v2: Vec::new(),
                title: None,
            },
            status: None,
            ansi: None,
            exit: false,
            yank_text: None,
        };

        assert_eq!(active_cursor_shape(&fd), None);
    }
}
