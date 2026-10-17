//! Renderer-independent frame contract shared by headless hosts and terminal clients.
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
