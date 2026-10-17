use std::sync::mpsc;

use super::session::{ClientId, PaneId, SessionId, Size};

pub enum ServerMsg {
    ClientConnect {
        client_id: ClientId,
        size: Size,
        resp_tx: mpsc::Sender<ServerResp>,
    },
    ClientDisconnect {
        client_id: ClientId,
    },
    ClientResize {
        client_id: ClientId,
        size: Size,
    },
    PtyOutput {
        pane_id: PaneId,
    },
    Command {
        client_id: ClientId,
        raw: String,
        resp_tx: Option<mpsc::Sender<String>>,
    },
    AttachSession {
        client_id: ClientId,
        session_name: String,
        resp_tx: mpsc::Sender<ServerResp>,
    },
    Tick,
    Shutdown,
}

pub enum ServerResp {
    Ok,
    OkData(String),
    Error(String),
    Frame(String),
    Redirect { session_id: SessionId },
}

#[derive(Debug, Clone)]
pub struct FramePush {
    pub session_id: SessionId,
    pub json: String,
}

pub fn mark_data_ready() {
    crate::environment::current().notify();
}
pub fn notify_render() {
    mark_data_ready();
}
pub fn wait_render_since(after: u64, timeout: std::time::Duration) -> u64 {
    crate::environment::current().wait(after, timeout)
}
