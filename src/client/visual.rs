use super::{split_layout_rects, LayoutJson};
use crate::layout::NavDir;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VisualTarget {
    Local { pane_id: usize },
}

#[derive(Debug, Clone)]
pub struct VisualHit {
    pub target: VisualTarget,
    pub rect: ratatui::layout::Rect,
    pub active: bool,
}

pub fn collect_visual_hits(
    layout: &LayoutJson,
    area: ratatui::layout::Rect,
    hide_borders: bool,
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
                    collect_visual_hits(child, chunk, hide_borders)
                })
                .collect()
        }
        LayoutJson::Leaf { id, active, .. } => vec![VisualHit {
            target: VisualTarget::Local { pane_id: *id },
            rect: area,
            active: *active,
        }],
    }
}

pub fn current_from_hits(
    stored: Option<&VisualTarget>,
    hits: &[VisualHit],
) -> Option<VisualTarget> {
    if let Some(target) = stored {
        if hits.iter().any(|hit| &hit.target == target) {
            return Some(target.clone());
        }
    }
    hits.iter()
        .find(|hit| hit.active)
        .or_else(|| hits.first())
        .map(|hit| hit.target.clone())
}

pub fn neighbor_in_dir(
    hits: &[VisualHit],
    current: &VisualTarget,
    dir: NavDir,
) -> Option<VisualTarget> {
    let cr = hits.iter().find(|h| &h.target == current)?.rect;
    let cur_left = cr.x as i32;
    let cur_right = cr.x as i32 + cr.width.max(1) as i32 - 1;
    let cur_top = cr.y as i32;
    let cur_bottom = cr.y as i32 + cr.height.max(1) as i32 - 1;
    let cur_mid_x = (cur_left + cur_right) / 2;
    let cur_mid_y = (cur_top + cur_bottom) / 2;
    let mut best: Option<(u8, i32, i32, VisualTarget)> = None;
    for hit in hits {
        if &hit.target == current {
            continue;
        }
        if hit.rect.width == 0 || hit.rect.height == 0 {
            continue;
        }
        let r = hit.rect;
        let left = r.x as i32;
        let right = r.x as i32 + r.width as i32 - 1;
        let top = r.y as i32;
        let bottom = r.y as i32 + r.height as i32 - 1;
        let overlap_v = (cur_bottom.min(bottom) - cur_top.max(top) + 1).max(0);
        let overlap_h = (cur_right.min(right) - cur_left.max(left) + 1).max(0);
        let (ok, dist, overlap) = match dir {
            NavDir::Left => (right < cur_left, cur_left - right, overlap_v),
            NavDir::Right => (left > cur_right, left - cur_right, overlap_v),
            NavDir::Up => (bottom < cur_top, cur_top - bottom, overlap_h),
            NavDir::Down => (top > cur_bottom, top - cur_bottom, overlap_h),
        };
        if !ok {
            continue;
        }
        let center_dist = match dir {
            NavDir::Left | NavDir::Right => {
                ((top + bottom) / 2 - cur_mid_y).abs()
            }
            NavDir::Up | NavDir::Down => ((left + right) / 2 - cur_mid_x).abs(),
        };
        let score = (u8::from(overlap == 0), dist, center_dist);
        if best.as_ref().is_none_or(|(best0, best1, best2, _)| {
            score < (*best0, *best1, *best2)
        }) {
            best = Some((score.0, score.1, score.2, hit.target.clone()));
        }
    }
    best.map(|(_, _, _, t)| t)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(
        target: VisualTarget,
        x: u16,
        y: u16,
        w: u16,
        h: u16,
        active: bool,
    ) -> VisualHit {
        VisualHit {
            target,
            rect: ratatui::layout::Rect {
                x,
                y,
                width: w,
                height: h,
            },
            active,
        }
    }

    #[test]
    fn current_from_hits_prefers_active_when_stored_is_missing() {
        let hits = vec![
            hit(VisualTarget::Local { pane_id: 1 }, 0, 0, 40, 10, false),
            hit(VisualTarget::Local { pane_id: 2 }, 41, 0, 40, 10, true),
        ];
        assert_eq!(
            current_from_hits(None, &hits),
            Some(VisualTarget::Local { pane_id: 2 })
        );
        assert_eq!(
            current_from_hits(Some(&VisualTarget::Local { pane_id: 1 }), &hits),
            Some(VisualTarget::Local { pane_id: 1 })
        );
        assert_eq!(
            current_from_hits(Some(&VisualTarget::Local { pane_id: 9 }), &hits),
            Some(VisualTarget::Local { pane_id: 2 })
        );
    }

    #[test]
    fn neighbor_in_dir_reaches_non_overlapping_pane() {
        let hits = vec![
            hit(VisualTarget::Local { pane_id: 1 }, 0, 0, 20, 10, true),
            hit(VisualTarget::Local { pane_id: 2 }, 30, 12, 20, 10, false),
        ];
        let from = VisualTarget::Local { pane_id: 1 };
        assert_eq!(
            neighbor_in_dir(&hits, &from, NavDir::Down),
            Some(VisualTarget::Local { pane_id: 2 })
        );
        assert_eq!(
            neighbor_in_dir(
                &hits,
                &VisualTarget::Local { pane_id: 2 },
                NavDir::Up
            ),
            Some(VisualTarget::Local { pane_id: 1 })
        );
    }
}
