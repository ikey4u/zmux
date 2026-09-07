use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicU8},
        Arc, Mutex,
    },
    time::Instant,
};

use chrono::{DateTime, Local};
use portable_pty::MasterPty;

use super::{
    layout::LayoutNode,
    mode::{CopyModeState, Mode},
    options::{GlobalOptions, SessionOptions, WindowOptions},
};
use crate::{
    history_store::{PaneHistory, PaneHistoryWriter},
    terminal::AlacrittyTermState,
};

pub type SessionId = usize;
pub type WindowId = usize;
pub type PaneId = usize;
pub type ClientId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    pub rows: u16,
    pub cols: u16,
    /// Absolute client-screen origin used by server-rendered ANSI.
    pub x: u16,
    pub y: u16,
}

impl Size {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            rows,
            cols,
            x: 0,
            y: 0,
        }
    }

    pub fn viewport(rows: u16, cols: u16, x: u16, y: u16) -> Self {
        Self { rows, cols, x, y }
    }
}

pub struct Pane {
    pub id: PaneId,
    pub master: Box<dyn MasterPty>,
    pub writer: crate::pty::PtyWriter,
    pub child: Box<dyn portable_pty::Child>,
    pub parser: Arc<Mutex<AlacrittyTermState>>,
    /// Disk-backed history older than the parser's in-memory scrollback.
    pub history: Arc<Mutex<PaneHistory>>,
    /// Bounded write-behind worker for cold history.
    pub history_writer: PaneHistoryWriter,
    /// Serializes cold-history drains so concurrent resize/render paths cannot
    /// append newer rows ahead of an older PTY batch.
    pub history_serial: Arc<Mutex<()>>,
    pub last_rows: u16,
    pub last_cols: u16,
    pub title: String,
    pub title_locked: bool,
    pub child_pid: Option<u32>,
    pub data_version: Arc<AtomicU64>,
    pub last_title_check: Instant,
    pub dead: Arc<AtomicBool>,
    pub cursor_shape: Arc<AtomicU8>,
    pub bell_pending: Arc<AtomicBool>,
    pub copy_state: Option<CopyModeState>,
    /// Complete OSC 52 sequences emitted by the pane, waiting to be relayed to
    /// the attached terminal.
    pub pending_osc52: Arc<Mutex<VecDeque<Vec<u8>>>>,
    pub reported_cwd: Arc<Mutex<Option<String>>>,
    pub start_dir: Option<String>,
    /// Set when PTY output arrives; cleared when server ANSI repaints this pane.
    pub render_dirty: Arc<AtomicBool>,
}

impl Pane {
    pub fn mark_render_dirty(&self) {
        self.render_dirty
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.dead.store(true, std::sync::atomic::Ordering::Relaxed);
        let _history_guard = self
            .history_serial
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.history_writer.shutdown_and_discard();
    }
}

pub struct Window {
    pub id: WindowId,
    pub name: String,
    pub root: LayoutNode,
    pub active_pane_path: Vec<usize>,
    pub options: WindowOptions,
    pub pane_mru: Vec<PaneId>,
    pub zoom_state: Option<ZoomState>,
    pub flags: WindowFlags,
    pub layout_index: usize,
    pub last_output_time: Instant,
    pub last_seen_version: u64,
    pub default_start_dir: Option<String>,
}

#[derive(Default)]
pub struct WindowFlags {
    pub activity: bool,
    pub bell: bool,
    pub silence: bool,
    pub last: bool,
}

pub struct ZoomState {
    pub saved_sizes: Vec<(Vec<usize>, Vec<u16>)>,
    pub zoomed_pane_id: PaneId,
}

pub struct Session {
    pub id: SessionId,
    pub name: String,
    pub windows: Vec<Window>,
    pub active_window_idx: usize,
    pub last_window_idx: usize,
    pub options: SessionOptions,
    pub created_at: DateTime<Local>,
    pub last_attached: Option<DateTime<Local>>,
    pub next_window_id: WindowId,
    pub next_pane_id: PaneId,
    pub group: Option<String>,
}

impl Session {
    pub fn new(id: SessionId, name: String) -> Self {
        Self {
            id,
            name,
            windows: Vec::new(),
            active_window_idx: 0,
            last_window_idx: 0,
            options: SessionOptions::default(),
            created_at: Local::now(),
            last_attached: None,
            next_window_id: 1,
            next_pane_id: 1,
            group: None,
        }
    }

    pub fn active_window(&self) -> Option<&Window> {
        self.windows.get(self.active_window_idx)
    }

    pub fn active_window_mut(&mut self) -> Option<&mut Window> {
        self.windows.get_mut(self.active_window_idx)
    }

    pub fn alloc_pane_id(&mut self) -> PaneId {
        let id = self.next_pane_id;
        self.next_pane_id += 1;
        id
    }

    pub fn alloc_window_id(&mut self) -> WindowId {
        let id = self.next_window_id;
        self.next_window_id += 1;
        id
    }
}

#[derive(Debug, Clone)]
pub struct ClientInfo {
    pub id: ClientId,
    pub size: Size,
    pub connected_at: Instant,
    pub last_activity: Instant,
    pub attached_session: Option<SessionId>,
    pub is_control: bool,
}

impl ClientInfo {
    pub fn new(id: ClientId, size: Size) -> Self {
        let now = Instant::now();
        Self {
            id,
            size,
            connected_at: now,
            last_activity: now,
            attached_session: None,
            is_control: false,
        }
    }
}

pub struct PasteBuffer {
    pub name: String,
    pub data: String,
    pub created_at: DateTime<Local>,
}

pub struct Server {
    pub sessions: Vec<Session>,
    pub active_session_idx: usize,
    pub clients: HashMap<ClientId, ClientInfo>,
    pub paste_buffers: Vec<PasteBuffer>,
    pub options: GlobalOptions,
    pub mode: Mode,
    pub next_session_id: SessionId,
    pub next_client_id: ClientId,
    pub hide_borders: bool,
    /// Next render should clear the pane area before painting (set on resize/layout churn).
    pub force_clear_display: bool,
    pub instance_id: String,
    pub socket_name: Option<String>,
    /// Creation directory shared only by this workspace/server instance.
    pub workspace_home: Option<String>,
}

impl Server {
    pub fn new() -> Self {
        Self {
            sessions: Vec::new(),
            active_session_idx: 0,
            clients: HashMap::new(),
            paste_buffers: Vec::new(),
            options: GlobalOptions::default(),
            mode: Mode::Passthrough,
            next_session_id: 0,
            next_client_id: 1,
            hide_borders: false,
            force_clear_display: false,
            instance_id: crate::domain::ids::new_instance_id(),
            socket_name: None,
            workspace_home: None,
        }
    }

    pub fn alloc_session_id(&mut self) -> SessionId {
        let id = self.next_session_id;
        self.next_session_id += 1;
        id
    }

    pub fn alloc_client_id(&mut self) -> ClientId {
        let id = self.next_client_id;
        self.next_client_id += 1;
        id
    }

    pub fn active_session(&self) -> Option<&Session> {
        self.sessions.get(self.active_session_idx)
    }

    pub fn active_session_mut(&mut self) -> Option<&mut Session> {
        self.sessions.get_mut(self.active_session_idx)
    }

    pub fn find_session_idx(&self, name: &str) -> Option<usize> {
        self.sessions.iter().position(|s| s.name == name)
    }

    pub fn find_session(&self, name: &str) -> Option<&Session> {
        self.sessions.iter().find(|s| s.name == name)
    }

    pub fn find_session_mut(&mut self, name: &str) -> Option<&mut Session> {
        self.sessions.iter_mut().find(|s| s.name == name)
    }

    pub fn find_session_by_id(&self, id: SessionId) -> Option<&Session> {
        self.sessions.iter().find(|s| s.id == id)
    }

    pub fn find_session_by_id_mut(
        &mut self,
        id: SessionId,
    ) -> Option<&mut Session> {
        self.sessions.iter_mut().find(|s| s.id == id)
    }
}
