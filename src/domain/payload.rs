use serde::{Deserialize, Serialize};

use super::ids::PaneRef;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachPayload {
    pub rows: u16,
    pub cols: u16,
    #[serde(default)]
    pub pane: Option<PaneRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputPayload {
    pub bytes_b64: String,
    #[serde(default)]
    pub pane: Option<PaneRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PastePayload {
    pub text: String,
    #[serde(default)]
    pub pane: Option<PaneRef>,
    #[serde(default)]
    pub raw: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CmdPayload {
    pub id: u64,
    pub cmd: String,
    #[serde(default)]
    pub want_output: bool,
    #[serde(default)]
    pub pane: Option<PaneRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RespPayload {
    pub id: u64,
    pub ok: bool,
    #[serde(default)]
    pub output: String,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResizePayload {
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DomainFramePayload {
    pub server_instance_id: String,
    pub sequence: u64,
    pub base_sequence: u64,
    pub full: bool,
    pub layout_revision: u64,
    pub frame: crate::client::FrameData,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowUpdatePayload {
    pub stream_id: u32,
    pub credit: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipPayload {
    pub id: String,
    pub kind: String,
    pub mime: String,
    pub size: u64,
    pub name: String,
    #[serde(default)]
    pub sha256: String,
    #[serde(default)]
    pub pane: Option<PaneRef>,
    #[serde(default)]
    pub generation: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlobPayload {
    pub id: String,
    pub offset: u64,
    pub last: bool,
    pub data_b64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DropOkPayload {
    pub id: String,
    pub path: String,
    pub bytes: u64,
    #[serde(default)]
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelPayload {
    pub id: String,
}
