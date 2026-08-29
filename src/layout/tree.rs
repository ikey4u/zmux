use crate::types::{LayoutNode, Pane, PaneId, SplitDirection};

pub fn first_leaf_path(node: &LayoutNode) -> Vec<usize> {
    match node {
        LayoutNode::Leaf(_) => vec![],
        LayoutNode::Split { children, .. } => {
            let mut path = vec![0];
            path.extend(first_leaf_path(&children[0]));
            path
        }
    }
}

pub fn node_id(node: &LayoutNode) -> Option<PaneId> {
    match node {
        LayoutNode::Leaf(p) => Some(p.id),
        LayoutNode::Split { .. } => None,
    }
}

pub fn active_node_id(node: &LayoutNode, path: &[usize]) -> Option<PaneId> {
    match (node, path) {
        (LayoutNode::Leaf(p), []) => Some(p.id),
        (LayoutNode::Split { children, .. }, [idx, rest @ ..]) => {
            children.get(*idx).and_then(|c| active_node_id(c, rest))
        }
        _ => None,
    }
}

pub fn active_pane<'a>(
    node: &'a LayoutNode,
    path: &[usize],
) -> Option<&'a Pane> {
    match (node, path) {
        (LayoutNode::Leaf(p), []) => Some(p),
        (LayoutNode::Split { children, .. }, [idx, rest @ ..]) => {
            children.get(*idx).and_then(|c| active_pane(c, rest))
        }
        _ => None,
    }
}

pub fn active_pane_mut<'a>(
    node: &'a mut LayoutNode,
    path: &[usize],
) -> Option<&'a mut Pane> {
    match (node, path) {
        (LayoutNode::Leaf(p), []) => Some(p),
        (LayoutNode::Split { children, .. }, [idx, rest @ ..]) => children
            .get_mut(*idx)
            .and_then(|c| active_pane_mut(c, rest)),
        _ => None,
    }
}

pub fn find_pane_by_id(node: &LayoutNode, id: PaneId) -> Option<&Pane> {
    match node {
        LayoutNode::Leaf(p) => {
            if p.id == id {
                Some(p)
            } else {
                None
            }
        }
        LayoutNode::Split { children, .. } => {
            children.iter().find_map(|c| find_pane_by_id(c, id))
        }
    }
}

pub fn find_pane_by_id_mut(
    node: &mut LayoutNode,
    id: PaneId,
) -> Option<&mut Pane> {
    match node {
        LayoutNode::Leaf(p) => {
            if p.id == id {
                Some(p)
            } else {
                None
            }
        }
        LayoutNode::Split { children, .. } => {
            children.iter_mut().find_map(|c| find_pane_by_id_mut(c, id))
        }
    }
}

pub fn find_pane_path(node: &LayoutNode, id: PaneId) -> Option<Vec<usize>> {
    match node {
        LayoutNode::Leaf(p) => {
            if p.id == id {
                Some(vec![])
            } else {
                None
            }
        }
        LayoutNode::Split { children, .. } => {
            for (i, child) in children.iter().enumerate() {
                if let Some(mut path) = find_pane_path(child, id) {
                    path.insert(0, i);
                    return Some(path);
                }
            }
            None
        }
    }
}

pub fn collect_pane_ids(node: &LayoutNode) -> Vec<PaneId> {
    match node {
        LayoutNode::Leaf(p) => vec![p.id],
        LayoutNode::Split { children, .. } => {
            children.iter().flat_map(|c| collect_pane_ids(c)).collect()
        }
    }
}

pub fn leaf_count(node: &LayoutNode) -> usize {
    match node {
        LayoutNode::Leaf(_) => 1,
        LayoutNode::Split { children, .. } => {
            children.iter().map(leaf_count).sum()
        }
    }
}

pub fn split_node(
    root: LayoutNode,
    path: &[usize],
    direction: SplitDirection,
    new_pane: Pane,
    new_first: bool,
) -> LayoutNode {
    replace_leaf(root, path, |leaf| {
        let (a, b) = if new_first {
            (LayoutNode::Leaf(new_pane), leaf)
        } else {
            (leaf, LayoutNode::Leaf(new_pane))
        };
        LayoutNode::Split {
            direction,
            sizes: vec![50, 50],
            children: vec![a, b],
        }
    })
}

fn replace_leaf<F>(node: LayoutNode, path: &[usize], f: F) -> LayoutNode
where
    F: FnOnce(LayoutNode) -> LayoutNode,
{
    match (node, path) {
        (leaf @ LayoutNode::Leaf(_), []) => f(leaf),
        (
            LayoutNode::Split {
                direction,
                sizes,
                mut children,
            },
            [idx, rest @ ..],
        ) => {
            if *idx < children.len() {
                let old = children.remove(*idx);
                let new = replace_leaf(old, rest, f);
                children.insert(*idx, new);
            }
            LayoutNode::Split {
                direction,
                sizes,
                children,
            }
        }
        (node, _) => node,
    }
}

pub fn kill_pane_at_path(
    root: LayoutNode,
    path: &[usize],
) -> Option<LayoutNode> {
    if path.is_empty() {
        return None;
    }
    Some(remove_at(root, path))
}

fn remove_at(node: LayoutNode, path: &[usize]) -> LayoutNode {
    match (node, path) {
        (
            LayoutNode::Split {
                direction,
                mut sizes,
                mut children,
            },
            [idx, rest @ ..],
        ) => {
            if *idx >= children.len() {
                return normalize_split(direction, sizes, children);
            }
            if rest.is_empty() {
                children.remove(*idx);
                if *idx < sizes.len() {
                    sizes.remove(*idx);
                }
            } else {
                let child = children.remove(*idx);
                let new_child = remove_at(child, rest);
                children.insert(*idx, new_child);
            }
            normalize_split(direction, sizes, children)
        }
        (node, _) => node,
    }
}

fn normalize_split(
    direction: SplitDirection,
    sizes: Vec<u16>,
    children: Vec<LayoutNode>,
) -> LayoutNode {
    let mut children: Vec<LayoutNode> =
        children.into_iter().map(normalize_layout).collect();
    if children.len() == 1 {
        return children.remove(0);
    }
    let sizes = normalize_sizes(sizes, children.len());
    LayoutNode::Split {
        direction,
        sizes,
        children,
    }
}

fn normalize_layout(node: LayoutNode) -> LayoutNode {
    match node {
        LayoutNode::Split {
            direction,
            sizes,
            children,
        } => normalize_split(direction, sizes, children),
        node => node,
    }
}

fn normalize_sizes(sizes: Vec<u16>, count: usize) -> Vec<u16> {
    if count == 0 {
        return vec![];
    }
    if sizes.len() == count && sizes.iter().any(|&size| size > 0) {
        sizes
    } else {
        equal_sizes(count)
    }
}

pub fn equal_sizes(n: usize) -> Vec<u16> {
    if n == 0 {
        return vec![];
    }
    let base = 100 / n as u16;
    let rem = 100 - base * n as u16;
    let mut sizes = vec![base; n];
    if let Some(last) = sizes.last_mut() {
        *last += rem;
    }
    sizes
}

pub fn next_pane_path(node: &LayoutNode, current: &[usize]) -> Vec<usize> {
    cycle_pane_path(node, current, 1)
}

pub fn prev_pane_path(node: &LayoutNode, current: &[usize]) -> Vec<usize> {
    cycle_pane_path(node, current, -1)
}

fn cycle_pane_path(
    node: &LayoutNode,
    current: &[usize],
    delta: isize,
) -> Vec<usize> {
    let ids = collect_pane_ids(node);
    if ids.len() <= 1 {
        return current.to_vec();
    }
    let Some(cur_id) = active_node_id(node, current) else {
        return current.to_vec();
    };
    let Some(pos) = ids.iter().position(|&id| id == cur_id) else {
        return current.to_vec();
    };
    let len = ids.len() as isize;
    let next = (pos as isize + delta).rem_euclid(len) as usize;
    find_pane_path(node, ids[next]).unwrap_or_else(|| current.to_vec())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavDir {
    Left,
    Right,
    Up,
    Down,
}

pub fn pane_path_in_direction(
    node: &LayoutNode,
    current: &[usize],
    dir: NavDir,
    total_area: crate::types::Rect,
    border_size: u16,
    pane_mru: &[PaneId],
) -> Vec<usize> {
    use crate::layout::rect::compute_rects;

    let rects = compute_rects(node, total_area, border_size);
    let cur_id = match active_node_id(node, current) {
        Some(id) => id,
        None => return current.to_vec(),
    };
    let cr = match rects.get(&cur_id) {
        Some(r) => *r,
        None => return current.to_vec(),
    };

    // 当前 pane 的边界（含）
    let cur_left = cr.x as i32;
    let cur_right = cr.x as i32 + cr.width as i32 - 1;
    let cur_top = cr.y as i32;
    let cur_bottom = cr.y as i32 + cr.height as i32 - 1;

    let ids = collect_pane_ids(node);
    let mut best_id: Option<crate::types::PaneId> = None;
    // Stable spatial score, smallest first:
    // 1. nearest pane on the requested axis
    // 2. candidates overlapping the current pane on the other axis
    // 3. most recently focused pane (makes crossing a one-to-many split reversible)
    // 4. closest center, then greatest overlap and screen order
    let mut best_score: Option<(i32, u8, usize, i32, i32, i32, PaneId)> = None;

    for &id in &ids {
        if id == cur_id {
            continue;
        }
        let r = match rects.get(&id) {
            Some(r) => *r,
            None => continue,
        };

        let r_left = r.x as i32;
        let r_right = r.x as i32 + r.width as i32 - 1;
        let r_top = r.y as i32;
        let r_bottom = r.y as i32 + r.height as i32 - 1;

        // 严格方向判断：候选 pane 的近端边界必须超过当前 pane 的远端边界
        let (primary_dist, overlap, center_dist, screen_order) = match dir {
            NavDir::Right => {
                if r_left <= cur_right {
                    continue;
                }
                let dist = r_left - cur_right;
                // 垂直方向重叠
                let ov = overlap_1d(cur_top, cur_bottom, r_top, r_bottom);
                (
                    dist,
                    ov,
                    ((cur_top + cur_bottom) - (r_top + r_bottom)).abs(),
                    r_top,
                )
            }
            NavDir::Left => {
                if r_right >= cur_left {
                    continue;
                }
                let dist = cur_left - r_right;
                let ov = overlap_1d(cur_top, cur_bottom, r_top, r_bottom);
                (
                    dist,
                    ov,
                    ((cur_top + cur_bottom) - (r_top + r_bottom)).abs(),
                    r_top,
                )
            }
            NavDir::Down => {
                if r_top <= cur_bottom {
                    continue;
                }
                let dist = r_top - cur_bottom;
                let ov = overlap_1d(cur_left, cur_right, r_left, r_right);
                (
                    dist,
                    ov,
                    ((cur_left + cur_right) - (r_left + r_right)).abs(),
                    r_left,
                )
            }
            NavDir::Up => {
                if r_bottom >= cur_top {
                    continue;
                }
                let dist = cur_top - r_bottom;
                let ov = overlap_1d(cur_left, cur_right, r_left, r_right);
                (
                    dist,
                    ov,
                    ((cur_left + cur_right) - (r_left + r_right)).abs(),
                    r_left,
                )
            }
        };

        let mru_rank = pane_mru
            .iter()
            .position(|pane_id| *pane_id == id)
            .unwrap_or(usize::MAX);
        let score = (
            primary_dist,
            u8::from(overlap == 0),
            mru_rank,
            center_dist,
            -overlap,
            screen_order,
            id,
        );
        if best_score.is_none_or(|best| score < best) {
            best_score = Some(score);
            best_id = Some(id);
        }
    }

    match best_id {
        Some(id) => {
            find_pane_path(node, id).unwrap_or_else(|| current.to_vec())
        }
        None => current.to_vec(),
    }
}

/// 计算两个区间 [a0,a1] 和 [b0,b1] 的重叠长度（无重叠返回 0）
fn overlap_1d(a0: i32, a1: i32, b0: i32, b1: i32) -> i32 {
    let lo = a0.max(b0);
    let hi = a1.min(b1);
    (hi - lo + 1).max(0)
}
