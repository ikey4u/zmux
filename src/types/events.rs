use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Condvar, Mutex,
};

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

pub static PTY_DATA_READY: AtomicBool = AtomicBool::new(false);

static RENDER_CONDVAR: (Mutex<bool>, Condvar) =
    (Mutex::new(false), Condvar::new());

pub fn mark_data_ready() {
    PTY_DATA_READY.store(true, Ordering::Relaxed);
    notify_render();
}

pub fn notify_render() {
    if let Ok(mut flag) = RENDER_CONDVAR.0.lock() {
        *flag = true;
    }
    RENDER_CONDVAR.1.notify_one();
}

pub fn wait_render(timeout: std::time::Duration) {
    if let Ok(flag) = RENDER_CONDVAR.0.lock() {
        let result =
            RENDER_CONDVAR
                .1
                .wait_timeout_while(flag, timeout, |ready| !*ready);
        if let Ok((mut guard, _)) = result {
            *guard = false;
        }
    }
}
