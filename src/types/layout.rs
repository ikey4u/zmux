use super::session::{Pane, PaneId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalState {
    Connecting,
    Bound,
    Reconnecting,
    Exited,
}

impl ExternalState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::Bound => "bound",
            Self::Reconnecting => "reconnecting",
            Self::Exited => "exited",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "bound" => Self::Bound,
            "reconnecting" => Self::Reconnecting,
            "exited" => Self::Exited,
            _ => Self::Connecting,
        }
    }
}

pub struct ExternalSlot {
    pub id: PaneId,
    pub slot_id: u64,
    pub host_alias: String,
    pub remote_socket: String,
    pub state: ExternalState,
    pub generation: u64,
    pub last_rows: u16,
    pub last_cols: u16,
}

pub enum LayoutNode {
    Leaf(Pane),
    External(ExternalSlot),
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
