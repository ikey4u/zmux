use std::{collections::HashMap, fmt::Write as _};

use super::{
    split_layout_rects, CellRunJson, FrameData, LayoutJson, RowRunsJson,
    StatusJson,
};
use crate::{
    layout::NavDir,
    output::{vte_goto, CharacterStyles, DEFAULT_STYLES},
    types::session::Size,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VisualTarget {
    Local { pane_id: usize },
    Remote { slot_id: u64, pane_id: usize },
    Placeholder { slot_id: u64 },
}

#[derive(Debug, Clone)]
pub struct VisualHit {
    pub target: VisualTarget,
    pub rect: ratatui::layout::Rect,
    pub active: bool,
}

pub fn slot_id_for_host(layout: &LayoutJson, host: &str) -> Option<u64> {
    match layout {
        LayoutJson::External {
            slot_id, host: h, ..
        } if h == host => Some(*slot_id),
        LayoutJson::Split { children, .. } => {
            children.iter().find_map(|c| slot_id_for_host(c, host))
        }
        LayoutJson::External { graft: Some(g), .. } => {
            slot_id_for_host(g, host)
        }
        _ => None,
    }
}

pub fn external_local_id(layout: &LayoutJson, slot_id: u64) -> Option<usize> {
    match layout {
        LayoutJson::External {
            id, slot_id: sid, ..
        } if *sid == slot_id => Some(*id),
        LayoutJson::Split { children, .. } => {
            children.iter().find_map(|c| external_local_id(c, slot_id))
        }
        _ => None,
    }
}

pub fn compose_layout(
    local: &LayoutJson,
    grafts: &HashMap<u64, FrameData>,
) -> LayoutJson {
    match local {
        LayoutJson::Split {
            direction,
            sizes,
            children,
        } => LayoutJson::Split {
            direction: direction.clone(),
            sizes: sizes.clone(),
            children: children
                .iter()
                .map(|c| compose_layout(c, grafts))
                .collect(),
        },
        LayoutJson::External {
            id,
            slot_id,
            host,
            remote_socket,
            state,
            generation,
            rows,
            cols,
            active,
            ..
        } => {
            let graft =
                grafts.get(slot_id).map(|fd| Box::new(fd.layout.clone()));
            LayoutJson::External {
                id: *id,
                slot_id: *slot_id,
                host: host.clone(),
                remote_socket: remote_socket.clone(),
                state: state.clone(),
                generation: *generation,
                rows: *rows,
                cols: *cols,
                active: *active,
                graft,
            }
        }
        leaf => leaf.clone(),
    }
}

pub fn collect_visual_hits(
    layout: &LayoutJson,
    area: ratatui::layout::Rect,
    hide_borders: bool,
    slot: Option<u64>,
) -> Vec<VisualHit> {
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
                .zip(chunks.into_iter())
                .flat_map(|(child, chunk)| {
                    collect_visual_hits(child, chunk, hide_borders, slot)
                })
                .collect()
        }
        LayoutJson::Leaf { id, active, .. } => {
            let target = match slot {
                Some(slot_id) => VisualTarget::Remote {
                    slot_id,
                    pane_id: *id,
                },
                None => VisualTarget::Local { pane_id: *id },
            };
            vec![VisualHit {
                target,
                rect: area,
                active: *active,
            }]
        }
        LayoutJson::External {
            slot_id,
            graft: Some(g),
            ..
        } => collect_visual_hits(g, area, false, Some(*slot_id)),
        LayoutJson::External {
            slot_id, active, ..
        } => vec![VisualHit {
            target: VisualTarget::Placeholder { slot_id: *slot_id },
            rect: area,
            active: *active,
        }],
    }
}

pub fn neighbor_in_dir(
    hits: &[VisualHit],
    current: &VisualTarget,
    dir: NavDir,
) -> Option<VisualTarget> {
    let cr = hits.iter().find(|h| &h.target == current)?.rect;
    let cur_left = cr.x as i32;
    let cur_right = cr.x as i32 + cr.width as i32 - 1;
    let cur_top = cr.y as i32;
    let cur_bottom = cr.y as i32 + cr.height as i32 - 1;
    let mut best: Option<(i32, VisualTarget)> = None;
    for hit in hits {
        if &hit.target == current {
            continue;
        }
        let r = hit.rect;
        let left = r.x as i32;
        let right = r.x as i32 + r.width as i32 - 1;
        let top = r.y as i32;
        let bottom = r.y as i32 + r.height as i32 - 1;
        let overlap_v = cur_bottom >= top && bottom >= cur_top;
        let overlap_h = cur_right >= left && right >= cur_left;
        let (ok, dist) = match dir {
            NavDir::Left => (right < cur_left && overlap_v, cur_left - right),
            NavDir::Right => (left > cur_right && overlap_v, left - cur_right),
            NavDir::Up => (bottom < cur_top && overlap_h, cur_top - bottom),
            NavDir::Down => (top > cur_bottom && overlap_h, top - cur_bottom),
        };
        if !ok {
            continue;
        }
        if best.as_ref().is_none_or(|(best_dist, _)| dist < *best_dist) {
            best = Some((dist, hit.target.clone()));
        }
    }
    best.map(|(_, t)| t)
}

pub fn hit_at(hits: &[VisualHit], col: u16, row: u16) -> Option<&VisualHit> {
    hits.iter().find(|h| {
        col >= h.rect.x
            && col < h.rect.x.saturating_add(h.rect.width)
            && row >= h.rect.y
            && row < h.rect.y.saturating_add(h.rect.height)
    })
}

pub fn content_rect(
    area: ratatui::layout::Rect,
    hide_borders: bool,
) -> ratatui::layout::Rect {
    if !hide_borders && area.width > 2 && area.height > 2 {
        ratatui::layout::Rect {
            x: area.x + 1,
            y: area.y + 1,
            width: area.width - 2,
            height: area.height - 2,
        }
    } else {
        area
    }
}

pub fn leaf_count(layout: &LayoutJson) -> usize {
    match layout {
        LayoutJson::Split { children, .. } => {
            children.iter().map(leaf_count).sum()
        }
        LayoutJson::Leaf { .. } => 1,
        LayoutJson::External { graft: Some(g), .. } => leaf_count(g),
        LayoutJson::External { .. } => 1,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeOwner {
    Local,
    Remote { slot_id: u64 },
    None,
}

pub fn resize_owner(
    hits: &[VisualHit],
    current: &VisualTarget,
    dir: NavDir,
) -> ResizeOwner {
    let Some(next) = neighbor_in_dir(hits, current, dir) else {
        return ResizeOwner::None;
    };
    match (current, next) {
        (
            VisualTarget::Remote { slot_id: a, .. },
            VisualTarget::Remote { slot_id: b, .. },
        ) if *a == b => ResizeOwner::Remote { slot_id: *a },
        _ => ResizeOwner::Local,
    }
}

pub fn slot_rect(
    layout: &LayoutJson,
    area: ratatui::layout::Rect,
    hide_borders: bool,
    slot_id: u64,
) -> Option<ratatui::layout::Rect> {
    match layout {
        LayoutJson::External { slot_id: id, .. } if *id == slot_id => {
            Some(area)
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
            children.iter().zip(chunks.into_iter()).find_map(
                |(child, chunk)| slot_rect(child, chunk, hide_borders, slot_id),
            )
        }
        _ => None,
    }
}

pub fn graft_size(rect: ratatui::layout::Rect) -> Size {
    Size::new(rect.height.saturating_add(1).max(2), rect.width.max(1))
}

pub fn paint_graft_ansi(
    layout: &LayoutJson,
    area: ratatui::layout::Rect,
    hide_borders: bool,
) -> String {
    let mut out = String::new();
    paint_node(layout, area, hide_borders, &mut out);
    out
}

fn paint_node(
    layout: &LayoutJson,
    area: ratatui::layout::Rect,
    hide_borders: bool,
    out: &mut String,
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
            for (child, chunk) in children.iter().zip(chunks.into_iter()) {
                paint_node(child, chunk, hide_borders, out);
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
            let has_border = !hide_borders && area.width > 2 && area.height > 2;
            if has_border {
                paint_border(area, *active, out);
            }
            let inner = if has_border {
                ratatui::layout::Rect {
                    x: area.x + 1,
                    y: area.y + 1,
                    width: area.width - 2,
                    height: area.height - 2,
                }
            } else {
                area
            };
            paint_rows(rows_v2, inner, out);
            if *active && !*hide_cursor {
                let x = inner
                    .x
                    .saturating_add(*cursor_col)
                    .min(inner.x.saturating_add(inner.width.saturating_sub(1)));
                let y = inner.y.saturating_add(*cursor_row).min(
                    inner.y.saturating_add(inner.height.saturating_sub(1)),
                );
                vte_goto(x, y, out);
            }
        }
        LayoutJson::External { graft: Some(g), .. } => {
            paint_node(g, area, false, out);
        }
        LayoutJson::External { .. } => {}
    }
}

fn paint_border(area: ratatui::layout::Rect, active: bool, out: &mut String) {
    if area.width < 2 || area.height < 2 {
        return;
    }
    let color = if active { "\x1b[32m" } else { "\x1b[90m" };
    let left = area.x;
    let top = area.y;
    let right = area.x + area.width - 1;
    let bottom = area.y + area.height - 1;
    vte_goto(left, top, out);
    out.push_str(color);
    out.push('┌');
    for _ in (left + 1)..right {
        out.push('─');
    }
    out.push('┐');
    vte_goto(left, bottom, out);
    out.push_str(color);
    out.push('└');
    for _ in (left + 1)..right {
        out.push('─');
    }
    out.push('┘');
    for y in (top + 1)..bottom {
        vte_goto(left, y, out);
        out.push_str(color);
        out.push('│');
        vte_goto(right, y, out);
        out.push_str(color);
        out.push('│');
    }
    out.push_str("\x1b[0m");
}

fn paint_rows(
    rows: &[RowRunsJson],
    area: ratatui::layout::Rect,
    out: &mut String,
) {
    for (i, row) in rows.iter().enumerate() {
        if i as u16 >= area.height {
            break;
        }
        let y = area.y + i as u16;
        vte_goto(area.x, y, out);
        let mut styles = DEFAULT_STYLES;
        let mut col = 0u16;
        for run in &row.runs {
            if col >= area.width {
                break;
            }
            paint_run(run, &mut styles, out);
            col = col.saturating_add(run.width.max(1));
        }
    }
    out.push_str("\x1b[0m");
}

fn paint_run(
    run: &CellRunJson,
    styles: &mut CharacterStyles,
    out: &mut String,
) {
    let next = CharacterStyles::from_layout_run(&run.fg, &run.bg, run.flags);
    if let Some(diff) = styles.update_and_return_diff(&next) {
        let _ = write!(out, "{diff}");
    }
    *styles = next;
    out.push_str(&run.text);
}

pub fn merge_status(
    local: Option<&StatusJson>,
    remote: Option<&StatusJson>,
    focused_remote: bool,
    blob_notice: Option<&str>,
) -> Option<StatusJson> {
    let mut status = if focused_remote {
        remote.cloned().or_else(|| local.cloned())
    } else {
        local.cloned()
    }?;
    if focused_remote {
        if let Some(remote) = remote {
            status.windows = remote.windows.clone();
        }
    }
    if let Some(notice) = blob_notice {
        if status.right.is_empty() {
            status.right = notice.to_string();
        } else {
            status.right = format!("{} | {}", notice, status.right);
        }
    }
    Some(status)
}

pub fn hits_escape_slot(
    slot: ratatui::layout::Rect,
    hits: &[VisualHit],
) -> bool {
    hits.iter().any(|h| {
        h.rect.x < slot.x
            || h.rect.y < slot.y
            || h.rect.x.saturating_add(h.rect.width)
                > slot.x.saturating_add(slot.width)
            || h.rect.y.saturating_add(h.rect.height)
                > slot.y.saturating_add(slot.height)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(id: usize, active: bool) -> LayoutJson {
        LayoutJson::Leaf {
            id,
            rows: 10,
            cols: 20,
            cursor_row: 0,
            cursor_col: 0,
            hide_cursor: false,
            alternate_screen: false,
            mouse_mode: 0,
            in_copy_mode: false,
            scroll_ratio: None,
            cursor_shape: 0,
            active,
            rows_v2: Vec::new(),
            title: None,
        }
    }

    fn split_h(left: LayoutJson, right: LayoutJson) -> LayoutJson {
        LayoutJson::Split {
            direction: "horizontal".to_string(),
            sizes: vec![50, 50],
            children: vec![left, right],
        }
    }

    #[test]
    fn composed_hits_stay_inside_slot() {
        let remote = split_h(leaf(10, true), leaf(11, false));
        let mut grafts = HashMap::new();
        grafts.insert(
            7,
            FrameData {
                frame_type: "frame".into(),
                layout: remote,
                status: None,
                ansi: None,
                exit: false,
                yank_text: None,
                client_requests: Vec::new(),
            },
        );
        let local = split_h(
            leaf(1, false),
            LayoutJson::External {
                id: 2,
                slot_id: 7,
                host: "linux".into(),
                remote_socket: "default".into(),
                state: "bound".into(),
                generation: 1,
                rows: 22,
                cols: 40,
                active: true,
                graft: None,
            },
        );
        let composed = compose_layout(&local, &grafts);
        let area = ratatui::layout::Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        };
        let hits = collect_visual_hits(&composed, area, true, None);
        assert_eq!(hits.len(), 3);
        let slot = slot_rect(&composed, area, true, 7).unwrap();
        let remote_hits: Vec<_> = hits
            .iter()
            .filter(|h| {
                matches!(h.target, VisualTarget::Remote { slot_id: 7, .. })
            })
            .cloned()
            .collect();
        assert_eq!(remote_hits.len(), 2);
        assert!(!hits_escape_slot(slot, &remote_hits));
        let from = VisualTarget::Remote {
            slot_id: 7,
            pane_id: 10,
        };
        let left = neighbor_in_dir(&hits, &from, NavDir::Left).unwrap();
        assert_eq!(left, VisualTarget::Local { pane_id: 1 });
        let right = neighbor_in_dir(
            &hits,
            &VisualTarget::Local { pane_id: 1 },
            NavDir::Right,
        )
        .unwrap();
        assert_eq!(
            right,
            VisualTarget::Remote {
                slot_id: 7,
                pane_id: 10
            }
        );
        assert_eq!(leaf_count(&composed), 3);
        let inner = resize_owner(&hits, &from, NavDir::Right);
        assert_eq!(inner, ResizeOwner::Remote { slot_id: 7 });
        let across = resize_owner(&hits, &from, NavDir::Left);
        assert_eq!(across, ResizeOwner::Local);
        assert_eq!(resize_owner(&hits, &from, NavDir::Up), ResizeOwner::None);
    }
}
