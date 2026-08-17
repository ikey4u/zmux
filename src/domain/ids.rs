use serde::{Deserialize, Serialize};

use crate::client::{FrameData, LayoutJson};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DomainId {
    pub transport: String,
    pub host_alias: String,
    pub remote_socket: String,
    pub server_instance_id: String,
}

impl DomainId {
    pub fn local(socket: &str, instance: &str) -> Self {
        Self {
            transport: "unix".to_string(),
            host_alias: "local".to_string(),
            remote_socket: socket.to_string(),
            server_instance_id: instance.to_string(),
        }
    }

    pub fn ssh(host: &str, socket: &str, instance: &str) -> Self {
        Self {
            transport: "ssh".to_string(),
            host_alias: host.to_string(),
            remote_socket: socket.to_string(),
            server_instance_id: instance.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PaneRef {
    pub domain_id: DomainId,
    pub session_id: u64,
    pub window_id: u64,
    pub pane_id: u64,
    pub pane_generation: u64,
}

pub fn pane_ref_from_frame(
    domain: &DomainId,
    frame: &FrameData,
) -> Option<PaneRef> {
    let pane_id = active_leaf_id(&frame.layout)?;
    let window_id = frame
        .status
        .as_ref()
        .and_then(|status| status.windows.iter().find(|w| w.active))
        .map(|w| w.index as u64)
        .unwrap_or(0);
    Some(PaneRef {
        domain_id: domain.clone(),
        session_id: 0,
        window_id,
        pane_id,
        pane_generation: 1,
    })
}

fn active_leaf_id(layout: &LayoutJson) -> Option<u64> {
    match layout {
        LayoutJson::Leaf {
            id, active: true, ..
        } => Some(*id as u64),
        LayoutJson::Split { children, .. } => {
            children.iter().find_map(active_leaf_id)
        }
        LayoutJson::Leaf { .. } => None,
        LayoutJson::External { graft: Some(g), .. } => active_leaf_id(g),
        LayoutJson::External { .. } => None,
    }
}

pub fn new_instance_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}-{}", std::process::id())
}
