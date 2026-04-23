use crate::types::{LayoutNode, Pane, SplitDirection};

pub fn layout_checksum(s: &str) -> u16 {
    let mut csum: u16 = 0;
    for b in s.bytes() {
        csum = (csum >> 1) | ((csum & 1) << 15);
        csum = csum.wrapping_add(b as u16);
    }
    csum
}

pub fn serialize_layout(
    node: &LayoutNode,
    total_width: u16,
    total_height: u16,
) -> String {
    let mut body = String::new();
    write_layout_node(node, 0, 0, total_width, total_height, &mut body);
    let csum = layout_checksum(&body);
    format!("{:04x},{}", csum, body)
}

fn write_layout_node(
    node: &LayoutNode,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    out: &mut String,
) {
    use std::fmt::Write;
    match node {
        LayoutNode::Leaf(p) => {
            let _ = write!(out, "{}x{},{},{},{}", width, height, x, y, p.id);
        }
        LayoutNode::Split {
            direction,
            sizes,
            children,
        } => {
            let open = match direction {
                SplitDirection::Horizontal => '{',
                SplitDirection::Vertical => '[',
            };
            let close = match direction {
                SplitDirection::Horizontal => '}',
                SplitDirection::Vertical => ']',
            };
            let _ = write!(out, "{}x{},{},{}", width, height, x, y);
            out.push(open);
            let n = children.len();
            let total_pct: u16 = sizes.iter().copied().sum::<u16>().max(1);
            let mut offset = 0u16;
            for (i, (child, &pct)) in
                children.iter().zip(sizes.iter()).enumerate()
            {
                if i > 0 {
                    out.push(',');
                }
                let dim = if i == n - 1 {
                    match direction {
                        SplitDirection::Horizontal => {
                            width.saturating_sub(offset)
                        }
                        SplitDirection::Vertical => {
                            height.saturating_sub(offset)
                        }
                    }
                } else {
                    let total_dim = match direction {
                        SplitDirection::Horizontal => width,
                        SplitDirection::Vertical => height,
                    };
                    (total_dim as u32 * pct as u32 / total_pct as u32) as u16
                };
                let (cx, cy, cw, ch) = match direction {
                    SplitDirection::Horizontal => (x + offset, y, dim, height),
                    SplitDirection::Vertical => (x, y + offset, width, dim),
                };
                write_layout_node(child, cx, cy, cw, ch, out);
                offset += dim + 1;
            }
            out.push(close);
        }
    }
}

pub fn parse_layout_string(s: &str) -> Option<ParsedLayout> {
    let s = s.trim();
    if s.len() < 5 {
        return None;
    }
    let bytes = s.as_bytes();
    if bytes[4] != b',' {
        return None;
    }
    for &b in &bytes[..4] {
        if !b.is_ascii_hexdigit() {
            return None;
        }
    }
    let body = &s[5..];
    let (node, _) = parse_node(body)?;
    Some(node)
}

#[derive(Debug, Clone)]
pub enum ParsedLayout {
    Leaf {
        w: u16,
        h: u16,
        x: u16,
        y: u16,
        pane_id: Option<usize>,
    },
    Split {
        direction: SplitDirection,
        w: u16,
        h: u16,
        x: u16,
        y: u16,
        children: Vec<ParsedLayout>,
    },
}

fn parse_node(s: &str) -> Option<(ParsedLayout, usize)> {
    let (w, h, x, y, consumed) = parse_dims(s)?;
    let rest = &s[consumed..];

    if rest.starts_with('{') {
        let (children, used) = parse_children(&rest[1..], '}')?;
        Some((
            ParsedLayout::Split {
                direction: SplitDirection::Horizontal,
                w,
                h,
                x,
                y,
                children,
            },
            consumed + 1 + used,
        ))
    } else if rest.starts_with('[') {
        let (children, used) = parse_children(&rest[1..], ']')?;
        Some((
            ParsedLayout::Split {
                direction: SplitDirection::Vertical,
                w,
                h,
                x,
                y,
                children,
            },
            consumed + 1 + used,
        ))
    } else {
        let mut extra = 0;
        let mut pane_id = None;
        if rest.starts_with(',') {
            let id_str = &rest[1..];
            let end = id_str
                .find(|c: char| matches!(c, ',' | '{' | '[' | '}' | ']'))
                .unwrap_or(id_str.len());
            pane_id = id_str[..end].parse().ok();
            extra = 1 + end;
        }
        Some((
            ParsedLayout::Leaf {
                w,
                h,
                x,
                y,
                pane_id,
            },
            consumed + extra,
        ))
    }
}

fn parse_dims(s: &str) -> Option<(u16, u16, u16, u16, usize)> {
    let xp = s.find('x')?;
    let w: u16 = s[..xp].parse().ok()?;
    let after_x = &s[xp + 1..];
    let c1 = after_x.find(',')?;
    let h: u16 = after_x[..c1].parse().ok()?;
    let after_h = &after_x[c1 + 1..];
    let c2 = after_h.find(',')?;
    let x: u16 = after_h[..c2].parse().ok()?;
    let after_x2 = &after_h[c2 + 1..];
    let y_end = after_x2
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(after_x2.len());
    let y: u16 = after_x2[..y_end].parse().ok()?;
    Some((w, h, x, y, xp + 1 + c1 + 1 + c2 + 1 + y_end))
}

fn parse_children(
    s: &str,
    closing: char,
) -> Option<(Vec<ParsedLayout>, usize)> {
    let mut children = Vec::new();
    let mut pos = 0;
    loop {
        if pos >= s.len() {
            return None;
        }
        if s.as_bytes()[pos] == closing as u8 {
            return Some((children, pos + 1));
        }
        if !children.is_empty() && s.as_bytes().get(pos).copied() == Some(b',')
        {
            pos += 1;
        }
        let (node, used) = parse_node(&s[pos..])?;
        children.push(node);
        pos += used;
    }
}
