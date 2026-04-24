use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
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

pub fn render_frame(f: &mut Frame, fd: &FrameData, in_prefix: bool) {
    render_frame_ex(f, fd, in_prefix, false, false);
}

pub fn active_cursor_shape(fd: &FrameData) -> Option<u8> {
    active_cursor_shape_in_layout(&fd.layout)
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
    let area = f.area();
    if area.height < 2 {
        return;
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);

    render_layout_node(f, &fd.layout, chunks[0], hide_borders);
    if !hide_status {
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
            for (child, chunk) in children.iter().zip(chunks.into_iter()) {
                render_layout_node(f, child, chunk, hide_borders);
            }
        }
        LayoutJson::Leaf {
            rows_v2,
            active,
            cursor_row,
            cursor_col,
            hide_cursor,
            ..
        } => {
            render_pane_content(
                f,
                rows_v2,
                *active,
                *cursor_row,
                *cursor_col,
                *hide_cursor,
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

    if has_border {
        draw_border(f, area, border_color);
    }

    let max_rows = content_area.height as usize;
    let max_cols = content_area.width as usize;

    for (row_idx, row_data) in rows_v2.iter().enumerate().take(max_rows) {
        let y = content_area.y + row_idx as u16;
        if y >= content_area.y + content_area.height {
            break;
        }

        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut col_used = 0usize;

        for run in &row_data.runs {
            if col_used >= max_cols {
                break;
            }
            let available = max_cols - col_used;
            let text = truncate_to_width(&run.text, available);
            let actual_width = unicode_display_width(&text);
            if text.is_empty() {
                break;
            }

            let fg = parse_color_str(&run.fg);
            let bg = parse_color_str(&run.bg);
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

        if !spans.is_empty() {
            let line = Line::from(spans);
            let para = Paragraph::new(line);
            f.render_widget(
                para,
                Rect {
                    x: content_area.x,
                    y,
                    width: content_area.width,
                    height: 1,
                },
            );
        }
    }

    if is_active && !hide_cursor {
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

fn draw_border(f: &mut Frame, area: Rect, color: Color) {
    use ratatui::widgets::{Block, Borders};
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color));
    f.render_widget(block, area);
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

pub fn render_session_chooser(
    f: &mut Frame,
    entries: &[crate::server::SessionTreeEntry],
    selected: usize,
    collapsed: &std::collections::HashSet<String>,
    collapsed_windows: &std::collections::HashSet<(String, usize)>,
) {
    use ratatui::widgets::{Block, Borders, Clear};

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
    let w = (area.width * 2 / 3).max(50).min(area.width);
    let h = ((visible.len() + 4) as u16)
        .min(area.height.saturating_sub(2))
        .max(5);
    let x = (area.width.saturating_sub(w)) / 2;
    let y = (area.height.saturating_sub(h)) / 2;
    let chooser_area = Rect {
        x,
        y,
        width: w,
        height: h,
    };

    f.render_widget(Clear, chooser_area);
    let block = Block::default()
        .title(" Sessions  (Enter=select  q/Esc=close  j/k=nav  l=expand  h=collapse) ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(chooser_area);
    f.render_widget(block, chooser_area);

    for (i, entry) in visible.iter().enumerate() {
        if i >= inner.height as usize {
            break;
        }
        let row_y = inner.y + i as u16;
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
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
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
                    Style::default().fg(Color::Blue)
                } else {
                    Style::default().fg(Color::White)
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
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::DarkGray)
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
                        cursor_shape: 4,
                        active: true,
                        rows_v2: Vec::new(),
                        title: None,
                    },
                ],
            },
            status: None,
            exit: false,
            yank_text: None,
        };

        assert_eq!(active_cursor_shape(&fd), Some(4));
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
                cursor_shape: 6,
                active: true,
                rows_v2: Vec::new(),
                title: None,
            },
            status: None,
            exit: false,
            yank_text: None,
        };

        assert_eq!(active_cursor_shape(&fd), None);
    }
}
