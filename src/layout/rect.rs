use std::collections::HashMap;

use crate::types::{LayoutNode, PaneId, Rect, SplitDirection};

pub const BORDER_SIZE: u16 = 1;

pub fn compute_rects(
    node: &LayoutNode,
    area: Rect,
    border_size: u16,
) -> HashMap<PaneId, Rect> {
    let mut map = HashMap::new();
    fill_rects(node, area, border_size, &mut map);
    map
}

fn fill_rects(
    node: &LayoutNode,
    area: Rect,
    border_size: u16,
    map: &mut HashMap<PaneId, Rect>,
) {
    match node {
        LayoutNode::Leaf(p) => {
            map.insert(p.id, area);
        }
        LayoutNode::Split {
            direction,
            sizes,
            children,
        } => {
            if children.is_empty() {
                return;
            }
            let rects = split_rects(
                area,
                direction,
                sizes,
                children.len(),
                border_size,
            );
            for (child, rect) in children.iter().zip(rects) {
                fill_rects(child, rect, border_size, map);
            }
        }
    }
}

fn split_rects(
    area: Rect,
    direction: &SplitDirection,
    sizes: &[u16],
    count: usize,
    border_size: u16,
) -> Vec<Rect> {
    if count == 0 {
        return vec![];
    }

    let total_dim = match direction {
        SplitDirection::Horizontal => area.width,
        SplitDirection::Vertical => area.height,
    };

    let borders = (count.saturating_sub(1)) as u16 * border_size;
    let available = total_dim.saturating_sub(borders);

    let total_pct: u16 = sizes.iter().copied().sum::<u16>().max(1);
    let mut rects = Vec::with_capacity(count);
    let mut offset = 0u16;

    for (i, &pct) in sizes.iter().enumerate().take(count) {
        let dim = if i == count - 1 {
            available.saturating_sub(offset)
        } else {
            (available as u32 * pct as u32 / total_pct as u32) as u16
        };

        let rect = match direction {
            SplitDirection::Horizontal => {
                Rect::new(area.x + offset, area.y, dim, area.height)
            }
            SplitDirection::Vertical => {
                Rect::new(area.x, area.y + offset, area.width, dim)
            }
        };

        rects.push(rect);
        offset += dim + border_size;
    }

    rects
}

pub fn compute_split_borders(node: &LayoutNode, area: Rect) -> Vec<BorderInfo> {
    let mut borders = Vec::new();
    collect_borders(node, area, &mut borders);
    borders
}

#[derive(Debug, Clone)]
pub struct BorderInfo {
    pub x: u16,
    pub y: u16,
    pub length: u16,
    pub horizontal: bool,
    pub path: Vec<usize>,
    pub index: usize,
}

fn collect_borders(
    node: &LayoutNode,
    area: Rect,
    borders: &mut Vec<BorderInfo>,
) {
    match node {
        LayoutNode::Leaf(_) => {}
        LayoutNode::Split {
            direction,
            sizes,
            children,
        } => {
            if children.is_empty() {
                return;
            }
            let rects = split_rects(
                area,
                direction,
                sizes,
                children.len(),
                BORDER_SIZE,
            );
            for (i, (child, rect)) in
                children.iter().zip(rects.iter()).enumerate()
            {
                collect_borders(child, *rect, borders);
                if i + 1 < children.len() {
                    let bx;
                    let by;
                    let length;
                    let horiz;
                    match direction {
                        SplitDirection::Horizontal => {
                            bx = rect.x + rect.width;
                            by = area.y;
                            length = area.height;
                            horiz = false;
                        }
                        SplitDirection::Vertical => {
                            bx = area.x;
                            by = rect.y + rect.height;
                            length = area.width;
                            horiz = true;
                        }
                    }
                    borders.push(BorderInfo {
                        x: bx,
                        y: by,
                        length,
                        horizontal: horiz,
                        path: vec![],
                        index: i,
                    });
                }
            }
        }
    }
}
