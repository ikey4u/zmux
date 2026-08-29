use super::session::Pane;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

pub enum LayoutNode {
    Leaf(Pane),
    Split {
        direction: SplitDirection,
        sizes: Vec<u16>,
        children: Vec<LayoutNode>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl Rect {
    pub fn new(x: u16, y: u16, width: u16, height: u16) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn area(&self) -> u32 {
        self.width as u32 * self.height as u32
    }
}
