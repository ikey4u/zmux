use std::{
    collections::HashMap,
    io::{self, Write},
    sync::mpsc,
    time::{Duration, Instant},
};

use arboard::Clipboard;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use crossterm::{
    cursor::{self, SetCursorStyle},
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste,
        EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, KeyboardEnhancementFlags, ModifierKeyCode, MouseButton,
        MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{
        self, DisableLineWrap, EnterAlternateScreen, LeaveAlternateScreen,
    },
};
use ratatui::Terminal;

mod backend;
use backend::TerminalBackend;
use serde::{Deserialize, Serialize};

mod render;
pub(crate) mod socket;
mod visual;
use regex::Regex;
pub use render::*;
pub use socket::SocketClient;

use crate::{
    commands::ParsedCommand,
    domain::DomainHandle,
    server::SessionTreeEntry,
    types::{session::Size, SelectionMode},
};

pub struct ClientApp {
    pub socket_name: String,
    pub session_name: Option<String>,
    pub clean: bool,
    pub start_dir: Option<String>,
    pub initial_tab_title: Option<String>,
    pub attach_all: bool,
    pub ssh_host: Option<String>,
}

#[derive(Clone, PartialEq)]
enum InputMode {
    Normal,
    Prefix,
    Resize,
    CopyMode,
    CopySearch {
        buf: String,
        cursor: usize,
        forward: bool,
    },
    RenameWindow {
        buf: String,
        cursor: usize,
    },
    RenameSession {
        buf: String,
        cursor: usize,
    },
    RenameTab {
        code: String,
        code_cursor: usize,
        title: String,
        title_cursor: usize,
        editing_code: bool,
        error: Option<String>,
        return_to_tab_chooser: bool,
    },
    ConfirmKillTab {
        socket_name: String,
        label: String,
        return_to_tab_chooser: bool,
    },
    Command {
        buf: String,
        cursor: usize,
    },
    SessionChooser {
        entries: Vec<SessionTreeEntry>,
        selected: usize,
        collapsed: std::collections::HashSet<String>,
        collapsed_windows: std::collections::HashSet<(String, usize)>,
    },
    OptionPanel {
        selected: usize,
        scroll_on_erase_in_display: bool,
    },
    TabChooser {
        query: String,
        cursor: usize,
        selected: usize,
        search_active: bool,
    },
    TabQuickSwitch {
        code: String,
        error: Option<String>,
    },
}

const RESIZE_IDLE_TIMEOUT: Duration = Duration::from_millis(500);
const SCROLL_LINES: usize = 3;
const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(400);
const RECONNECT_BACKOFF: Duration = Duration::from_millis(750);

struct LastMouseClick {
    row: u16,
    col: u16,
    at: Instant,
}

#[derive(Clone, Copy)]
struct MouseDragOrigin {
    row: u16,
    col: u16,
}

struct ClientTab {
    code: String,
    title: String,
    socket_name: String,
    client: Box<dyn DomainHandle>,
    grafts: HashMap<u64, Graft>,
    visual_focus: VisualFocus,
    pending_attach: Option<PendingAttach>,
    pending_reconnect: HashMap<u64, PendingReconnect>,
}

struct Graft {
    host: String,
    #[allow(dead_code)]
    remote_socket: String,
    client: Box<dyn DomainHandle>,
    generation: u64,
    last_size: Option<Size>,
    last_reconnect_at: Option<Instant>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum VisualFocus {
    Local { pane_id: Option<usize> },
    Remote { slot_id: u64, pane_id: usize },
}

struct PendingAttach {
    request_id: String,
    host: String,
    #[allow(dead_code)]
    pane_id: usize,
    rx: mpsc::Receiver<Result<crate::domain::cloud::CloudClient, String>>,
    ready: Option<Box<dyn DomainHandle>>,
}

struct PendingReconnect {
    rx: mpsc::Receiver<Result<crate::domain::cloud::CloudClient, String>>,
}

#[derive(Clone, Default, Deserialize, Serialize)]
struct StoredTabMetadata {
    code: Option<String>,
    title: Option<String>,
    visible: Option<bool>,
}

struct TabManager {
    tabs: Vec<ClientTab>,
    active: usize,
    tab_bar_offset: usize,
    base_socket: String,
    start_dir: Option<String>,
    next_id: usize,
    killed_sockets: std::collections::HashSet<String>,
}

impl TabManager {
    fn new(
        base_socket: &str,
        session_name: &str,
        size: Size,
        clean: bool,
        start_dir: Option<String>,
    ) -> io::Result<Self> {
        let (client, existing_server) = ensure_server_and_connect(
            base_socket,
            session_name,
            size,
            clean,
            start_dir.as_deref(),
        )?;
        let stored = if existing_server {
            load_tab_metadata().remove(base_socket)
        } else {
            remove_tab_metadata_for_socket_family(base_socket);
            None
        };
        let code = stored
            .as_ref()
            .and_then(|meta| meta.code.as_deref())
            .and_then(|code| normalize_tab_code(code).ok())
            .unwrap_or_else(|| tab_code(0));
        let title = stored.and_then(|meta| meta.title).unwrap_or_default();
        Ok(Self {
            tabs: vec![ClientTab {
                code,
                title,
                socket_name: base_socket.to_string(),
                client: Box::new(client),
                grafts: HashMap::new(),
                visual_focus: VisualFocus::Local { pane_id: None },
                pending_attach: None,
                pending_reconnect: HashMap::new(),
            }],
            active: 0,
            tab_bar_offset: 0,
            base_socket: base_socket.to_string(),
            start_dir,
            next_id: 1,
            killed_sockets: std::collections::HashSet::new(),
        })
    }

    fn from_existing_sockets(
        base_socket: &str,
        socket_names: Vec<String>,
        target_session: Option<&str>,
        size: Size,
        start_dir: Option<String>,
    ) -> io::Result<Self> {
        let metadata = load_tab_metadata();
        let mut tabs = Vec::new();
        for socket_name in socket_names {
            match SocketClient::connect(&socket_name, size) {
                Ok(client) => {
                    if let Some(target) = target_session {
                        client.run_command(&format!(
                            "switch-client -t {}",
                            shell_quote(target)
                        ));
                    }
                    let stored = metadata.get(&socket_name);
                    if stored.and_then(|meta| meta.visible) == Some(false) {
                        continue;
                    }
                    let code = stored
                        .and_then(|meta| meta.code.as_deref())
                        .and_then(|code| normalize_tab_code(code).ok())
                        .filter(|code| {
                            !tabs
                                .iter()
                                .any(|tab: &ClientTab| tab.code == *code)
                        })
                        .unwrap_or_else(|| {
                            next_available_tab_code(&tabs, tabs.len())
                        });
                    let title = stored
                        .and_then(|meta| meta.title.clone())
                        .unwrap_or_else(|| {
                            attach_tab_title(base_socket, &socket_name)
                        });
                    tabs.push(ClientTab {
                        code,
                        title,
                        socket_name,
                        client: Box::new(client),
                        grafts: HashMap::new(),
                        visual_focus: VisualFocus::Local { pane_id: None },
                        pending_attach: None,
                        pending_reconnect: HashMap::new(),
                    });
                }
                Err(e) => {
                    log_client(&format!(
                        "attach-all skipped socket '{}': {}",
                        socket_name, e
                    ));
                    cleanup_stale_socket(&socket_name, &e);
                }
            }
        }
        if tabs.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no attachable zmux servers found",
            ));
        }
        Ok(Self {
            active: 0,
            tab_bar_offset: 0,
            base_socket: base_socket.to_string(),
            start_dir,
            next_id: tabs.len().max(1),
            tabs,
            killed_sockets: std::collections::HashSet::new(),
        })
    }

    fn from_ssh(
        host: &str,
        local_socket: &str,
        size: Size,
        start_dir: Option<String>,
    ) -> io::Result<Self> {
        let client = crate::domain::connect_ssh(host, size)?;
        Ok(Self {
            tabs: vec![ClientTab {
                code: tab_code(0),
                title: host.to_string(),
                socket_name: format!("ssh:{host}"),
                client: Box::new(client),
                grafts: HashMap::new(),
                visual_focus: VisualFocus::Local { pane_id: None },
                pending_attach: None,
                pending_reconnect: HashMap::new(),
            }],
            active: 0,
            tab_bar_offset: 0,
            base_socket: local_socket.to_string(),
            start_dir,
            next_id: 1,
            killed_sockets: std::collections::HashSet::new(),
        })
    }

    fn attach_ssh(&mut self, host: &str, size: Size) -> io::Result<String> {
        let socket_name = format!("ssh:{host}");
        if let Some(index) = self
            .tabs
            .iter()
            .position(|tab| tab.socket_name == socket_name)
        {
            self.active = index;
            return Ok(format!("attached {host}"));
        }
        let client = crate::domain::connect_ssh(host, size)?;
        let code = next_available_tab_code(&self.tabs, self.tabs.len());
        self.tabs.push(ClientTab {
            code,
            title: host.to_string(),
            socket_name,
            client: Box::new(client),
            grafts: HashMap::new(),
            visual_focus: VisualFocus::Local { pane_id: None },
            pending_attach: None,
            pending_reconnect: HashMap::new(),
        });
        self.active = self.tabs.len() - 1;
        Ok(format!("ssh {host}"))
    }

    fn active_client(&self) -> &dyn DomainHandle {
        self.tabs[self.active].client.as_ref()
    }

    fn focused_client(&self) -> &dyn DomainHandle {
        let tab = &self.tabs[self.active];
        match tab.visual_focus {
            VisualFocus::Remote { slot_id, .. } => tab
                .grafts
                .get(&slot_id)
                .map(|g| g.client.as_ref())
                .unwrap_or(tab.client.as_ref()),
            VisualFocus::Local { .. } => tab.client.as_ref(),
        }
    }

    fn display_snapshot(&mut self) -> (Option<FrameData>, u64) {
        self.tick_cloud();
        let tab = &mut self.tabs[self.active];
        let (mut frame, mut counter) = tab.client.frame_snapshot();
        if let Some(fd) = frame.as_mut() {
            let mut grafts = HashMap::new();
            for (slot_id, graft) in &tab.grafts {
                let (gframe, gcounter) = graft.client.frame_snapshot();
                counter = counter.wrapping_add(gcounter);
                if let Some(gframe) = gframe {
                    if !gframe.exit {
                        grafts.insert(*slot_id, gframe);
                    }
                }
            }
            fd.layout = visual::compose_layout(&fd.layout, &grafts);
            let focused_remote =
                matches!(tab.visual_focus, VisualFocus::Remote { .. });
            let remote_status = match &tab.visual_focus {
                VisualFocus::Remote { slot_id, .. } => {
                    grafts.get(slot_id).and_then(|g| g.status.clone())
                }
                VisualFocus::Local { .. } => None,
            };
            let blob = tab.grafts.values().find_map(|g| g.client.blob_notice());
            fd.status = visual::merge_status(
                fd.status.as_ref(),
                remote_status.as_ref(),
                focused_remote,
                blob.as_deref(),
            );
            if !tab.grafts.is_empty() {
                if let Some(status) = fd.status.as_mut() {
                    if !focused_remote && !status.left.starts_with("[local]") {
                        status.left = format!("[local] {}", status.left);
                    }
                }
            }
        }
        (frame, counter)
    }

    fn tick_cloud(&mut self) {
        let tab = &mut self.tabs[self.active];
        if let Some(fd) = tab.client.latest_frame() {
            for req in &fd.client_requests {
                match req {
                    ClientRequest::DomainAttach {
                        request_id,
                        host,
                        pane_id,
                    } => {
                        if tab
                            .pending_attach
                            .as_ref()
                            .is_some_and(|p| p.request_id == *request_id)
                        {
                            continue;
                        }
                        if tab.grafts.values().any(|g| g.host == *host) {
                            let ok = crate::domain::attach::DomainBindOk {
                                request_id: request_id.clone(),
                                host: host.clone(),
                                remote_socket: "default".into(),
                                generation: 1,
                            };
                            if let Ok(json) = serde_json::to_string(&ok) {
                                tab.client.send_control_line(&format!(
                                    "DOMAIN_BIND_OK {json}"
                                ));
                            }
                            continue;
                        }
                        let host_c = host.clone();
                        tab.pending_attach = Some(PendingAttach {
                            request_id: request_id.clone(),
                            host: host.clone(),
                            pane_id: *pane_id,
                            rx: spawn_ssh_connect(host_c),
                            ready: None,
                        });
                    }
                }
            }
        }
        if let Some(mut pending) = tab.pending_attach.take() {
            let mut aborted = false;
            if pending.ready.is_none() {
                match pending.rx.try_recv() {
                    Ok(Ok(client)) => {
                        let ok = crate::domain::attach::DomainBindOk {
                            request_id: pending.request_id.clone(),
                            host: pending.host.clone(),
                            remote_socket: "default".into(),
                            generation: 1,
                        };
                        if let Ok(json) = serde_json::to_string(&ok) {
                            tab.client.send_control_line(&format!(
                                "DOMAIN_BIND_OK {json}"
                            ));
                        }
                        pending.ready = Some(Box::new(client));
                    }
                    Ok(Err(err)) => {
                        let fail = crate::domain::attach::DomainBindFail {
                            request_id: pending.request_id.clone(),
                            error: err,
                        };
                        if let Ok(json) = serde_json::to_string(&fail) {
                            tab.client.send_control_line(&format!(
                                "DOMAIN_BIND_FAIL {json}"
                            ));
                        }
                        aborted = true;
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                    Err(mpsc::TryRecvError::Disconnected) => {
                        let fail = crate::domain::attach::DomainBindFail {
                            request_id: pending.request_id.clone(),
                            error: "ssh attach thread exited".into(),
                        };
                        if let Ok(json) = serde_json::to_string(&fail) {
                            tab.client.send_control_line(&format!(
                                "DOMAIN_BIND_FAIL {json}"
                            ));
                        }
                        aborted = true;
                    }
                }
            }
            if !aborted {
                if let Some(client) = pending.ready.take() {
                    if let Some(fd) = tab.client.latest_frame() {
                        if let Some(slot_id) =
                            visual::slot_id_for_host(&fd.layout, &pending.host)
                        {
                            tab.visual_focus = VisualFocus::Remote {
                                slot_id,
                                pane_id: 0,
                            };
                            tab.grafts.insert(
                                slot_id,
                                Graft {
                                    host: pending.host.clone(),
                                    remote_socket: "default".into(),
                                    client,
                                    generation: 1,
                                    last_size: None,
                                    last_reconnect_at: None,
                                },
                            );
                        } else {
                            pending.ready = Some(client);
                            tab.pending_attach = Some(pending);
                        }
                    } else {
                        pending.ready = Some(client);
                        tab.pending_attach = Some(pending);
                    }
                } else {
                    tab.pending_attach = Some(pending);
                }
            }
        }
        self.poll_reconnects();
        self.sync_graft_sizes();
    }

    fn poll_reconnects(&mut self) {
        let tab = &mut self.tabs[self.active];
        let pending_ids: Vec<u64> =
            tab.pending_reconnect.keys().copied().collect();
        for slot_id in pending_ids {
            let Some(pending) = tab.pending_reconnect.remove(&slot_id) else {
                continue;
            };
            match pending.rx.try_recv() {
                Ok(Ok(client)) => {
                    if let Some(graft) = tab.grafts.get_mut(&slot_id) {
                        graft.generation = graft.generation.saturating_add(1);
                        graft.client = Box::new(client);
                        graft.last_size = None;
                        graft.last_reconnect_at = None;
                        let generation = graft.generation;
                        send_slot_state(
                            tab.client.as_ref(),
                            slot_id,
                            "bound",
                            generation,
                        );
                    }
                }
                Ok(Err(_)) | Err(mpsc::TryRecvError::Disconnected) => {
                    if let Some(graft) = tab.grafts.get_mut(&slot_id) {
                        graft.last_reconnect_at = Some(Instant::now());
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {
                    tab.pending_reconnect.insert(slot_id, pending);
                }
            }
        }
        let mut start = Vec::new();
        for (slot_id, graft) in &tab.grafts {
            if !graft.client.disconnected() {
                continue;
            }
            if tab.pending_reconnect.contains_key(slot_id) {
                continue;
            }
            let due = graft
                .last_reconnect_at
                .is_none_or(|at| at.elapsed() >= RECONNECT_BACKOFF);
            if due {
                start.push((
                    *slot_id,
                    graft.host.clone(),
                    graft.generation.saturating_add(1),
                ));
            }
        }
        for (slot_id, host, generation) in start {
            send_slot_state(
                tab.client.as_ref(),
                slot_id,
                "reconnecting",
                generation,
            );
            tab.pending_reconnect.insert(
                slot_id,
                PendingReconnect {
                    rx: spawn_ssh_connect(host),
                },
            );
        }
    }

    fn sync_graft_sizes(&mut self) {
        let tab = &mut self.tabs[self.active];
        let Some(fd) = tab.client.latest_frame() else {
            return;
        };
        let (cols, rows) = terminal::size().unwrap_or((80, 24));
        let area = server_layout_area(cols, rows);
        for (slot_id, graft) in tab.grafts.iter_mut() {
            let Some(rect) =
                visual::slot_rect(&fd.layout, area, false, *slot_id)
            else {
                continue;
            };
            let size = visual::graft_size(rect);
            if graft.last_size != Some(size) {
                graft.client.resize(size);
                graft.last_size = Some(size);
            }
        }
    }

    fn visual_move(&mut self, dir: crate::layout::NavDir, hide_borders: bool) {
        let (cols, rows) = terminal::size().unwrap_or((80, 24));
        let area = server_layout_area(cols, rows);
        let tab = &mut self.tabs[self.active];
        let Some(fd) = tab.client.latest_frame() else {
            return;
        };
        let mut grafts = HashMap::new();
        for (id, graft) in &tab.grafts {
            if let Some(frame) = graft.client.latest_frame() {
                if !frame.exit {
                    grafts.insert(*id, frame);
                }
            }
        }
        let composed = visual::compose_layout(&fd.layout, &grafts);
        let hits =
            visual::collect_visual_hits(&composed, area, hide_borders, None);
        let current = match &tab.visual_focus {
            VisualFocus::Remote { slot_id, pane_id } => {
                visual::VisualTarget::Remote {
                    slot_id: *slot_id,
                    pane_id: *pane_id,
                }
            }
            VisualFocus::Local { pane_id } => visual::VisualTarget::Local {
                pane_id: pane_id
                    .or_else(|| {
                        visual::collect_visual_hits(
                            &composed,
                            area,
                            hide_borders,
                            None,
                        )
                        .iter()
                        .find_map(|h| {
                            if let visual::VisualTarget::Local { pane_id } =
                                h.target
                            {
                                Some(pane_id)
                            } else {
                                None
                            }
                        })
                    })
                    .unwrap_or(0),
            },
        };
        let Some(next) = visual::neighbor_in_dir(&hits, &current, dir) else {
            return;
        };
        match next {
            visual::VisualTarget::Local { pane_id } => {
                tab.client
                    .run_command(&format!("select-pane -t %{pane_id}"));
                tab.visual_focus = VisualFocus::Local {
                    pane_id: Some(pane_id),
                };
            }
            visual::VisualTarget::Remote { slot_id, pane_id } => {
                if let Some(slot_pane) =
                    visual::external_local_id(&composed, slot_id)
                {
                    tab.client
                        .run_command(&format!("select-pane -t %{slot_pane}"));
                }
                if let Some(graft) = tab.grafts.get(&slot_id) {
                    graft
                        .client
                        .run_command(&format!("select-pane -t %{pane_id}"));
                }
                tab.visual_focus = VisualFocus::Remote { slot_id, pane_id };
            }
            visual::VisualTarget::Placeholder { slot_id } => {
                if let Some(slot_pane) =
                    visual::external_local_id(&composed, slot_id)
                {
                    tab.client
                        .run_command(&format!("select-pane -t %{slot_pane}"));
                }
                tab.visual_focus = VisualFocus::Local { pane_id: None };
            }
        }
    }

    fn paint_grafts(&self, hide_borders: bool) -> String {
        let tab = &self.tabs[self.active];
        let Some(fd) = tab.client.latest_frame() else {
            return String::new();
        };
        let (cols, rows) = terminal::size().unwrap_or((80, 24));
        let area = server_layout_area(cols, rows);
        let mut out = String::new();
        for (slot_id, graft) in &tab.grafts {
            let Some(rect) =
                visual::slot_rect(&fd.layout, area, hide_borders, *slot_id)
            else {
                continue;
            };
            let Some(gframe) = graft.client.latest_frame() else {
                continue;
            };
            if gframe.exit {
                continue;
            }
            out.push_str(&visual::paint_graft_ansi(
                &gframe.layout,
                rect,
                false,
            ));
        }
        out
    }

    fn resize_visual(&self, dir: crate::layout::NavDir) {
        let cmd = match dir {
            crate::layout::NavDir::Left => "resize-pane -L",
            crate::layout::NavDir::Right => "resize-pane -R",
            crate::layout::NavDir::Up => "resize-pane -U",
            crate::layout::NavDir::Down => "resize-pane -D",
        };
        let hits = self.composed_hits(false);
        let Some(current) = self.current_visual_target(&hits) else {
            self.active_client().run_command(cmd);
            return;
        };
        match visual::resize_owner(&hits, &current, dir) {
            visual::ResizeOwner::Remote { slot_id } => {
                if let Some(graft) = self.tabs[self.active].grafts.get(&slot_id)
                {
                    graft.client.run_command(cmd);
                }
            }
            visual::ResizeOwner::Local => {
                self.active_client().run_command(cmd);
            }
            visual::ResizeOwner::None => {}
        }
    }

    fn composed_hits(&self, hide_borders: bool) -> Vec<visual::VisualHit> {
        let (cols, rows) = terminal::size().unwrap_or((80, 24));
        let area = server_layout_area(cols, rows);
        let tab = &self.tabs[self.active];
        let Some(fd) = tab.client.latest_frame() else {
            return Vec::new();
        };
        let mut grafts = HashMap::new();
        for (id, graft) in &tab.grafts {
            if let Some(frame) = graft.client.latest_frame() {
                if !frame.exit {
                    grafts.insert(*id, frame);
                }
            }
        }
        let composed = visual::compose_layout(&fd.layout, &grafts);
        visual::collect_visual_hits(&composed, area, hide_borders, None)
    }

    fn current_visual_target(
        &self,
        hits: &[visual::VisualHit],
    ) -> Option<visual::VisualTarget> {
        match self.tabs[self.active].visual_focus {
            VisualFocus::Remote { slot_id, pane_id } => {
                Some(visual::VisualTarget::Remote { slot_id, pane_id })
            }
            VisualFocus::Local {
                pane_id: Some(pane_id),
            } => Some(visual::VisualTarget::Local { pane_id }),
            VisualFocus::Local { pane_id: None } => {
                hits.iter().find_map(|h| match h.target {
                    visual::VisualTarget::Local { pane_id } => {
                        Some(visual::VisualTarget::Local { pane_id })
                    }
                    _ => None,
                })
            }
        }
    }

    fn apply_visual_target(&mut self, target: visual::VisualTarget) {
        let tab = &mut self.tabs[self.active];
        let Some(fd) = tab.client.latest_frame() else {
            return;
        };
        match target {
            visual::VisualTarget::Local { pane_id } => {
                tab.client
                    .run_command(&format!("select-pane -t %{pane_id}"));
                tab.visual_focus = VisualFocus::Local {
                    pane_id: Some(pane_id),
                };
            }
            visual::VisualTarget::Remote { slot_id, pane_id } => {
                if let Some(slot_pane) =
                    visual::external_local_id(&fd.layout, slot_id)
                {
                    tab.client
                        .run_command(&format!("select-pane -t %{slot_pane}"));
                }
                if let Some(graft) = tab.grafts.get(&slot_id) {
                    graft
                        .client
                        .run_command(&format!("select-pane -t %{pane_id}"));
                }
                tab.visual_focus = VisualFocus::Remote { slot_id, pane_id };
            }
            visual::VisualTarget::Placeholder { slot_id } => {
                if let Some(slot_pane) =
                    visual::external_local_id(&fd.layout, slot_id)
                {
                    tab.client
                        .run_command(&format!("select-pane -t %{slot_pane}"));
                    tab.visual_focus = VisualFocus::Local {
                        pane_id: Some(slot_pane),
                    };
                } else {
                    tab.visual_focus = VisualFocus::Local { pane_id: None };
                }
            }
        }
    }

    fn focus_at(&mut self, col: u16, row: u16, hide_borders: bool) -> bool {
        let hits = self.composed_hits(hide_borders);
        let Some(hit) = visual::hit_at(&hits, col, row) else {
            return false;
        };
        if self.current_visual_target(&hits).as_ref() == Some(&hit.target) {
            return false;
        }
        let target = hit.target.clone();
        self.apply_visual_target(target);
        true
    }

    fn scroll_at(
        &mut self,
        mouse: MouseEvent,
        hide_borders: bool,
        direction: &str,
    ) {
        let hits = self.composed_hits(hide_borders);
        if let Some(hit) = visual::hit_at(&hits, mouse.column, mouse.row) {
            let pane_id = match hit.target {
                visual::VisualTarget::Local { pane_id } => Some(pane_id),
                visual::VisualTarget::Remote { pane_id, .. } => Some(pane_id),
                visual::VisualTarget::Placeholder { .. } => None,
            };
            let target = hit.target.clone();
            self.apply_visual_target(target);
            if let Some(pane_id) = pane_id {
                self.focused_client().scroll_pane(
                    pane_id,
                    direction,
                    SCROLL_LINES,
                );
            }
            return;
        }
        if direction == "up" {
            self.focused_client().scroll_up(SCROLL_LINES);
        } else {
            self.focused_client().scroll_down(SCROLL_LINES);
        }
    }

    fn send_mouse_at(&self, mouse: MouseEvent, hide_borders: bool) -> bool {
        let hits = self.composed_hits(hide_borders);
        let Some(hit) = visual::hit_at(&hits, mouse.column, mouse.row) else {
            return false;
        };
        let hide = match hit.target {
            visual::VisualTarget::Remote { .. } => false,
            _ => hide_borders,
        };
        let inner = visual::content_rect(hit.rect, hide);
        if mouse.column < inner.x
            || mouse.column >= inner.x.saturating_add(inner.width)
            || mouse.row < inner.y
            || mouse.row >= inner.y.saturating_add(inner.height)
        {
            return false;
        }
        let mut mapped = mouse;
        mapped.column = mouse.column.saturating_sub(inner.x);
        mapped.row = mouse.row.saturating_sub(inner.y);
        let bytes = mouse_to_bytes(mapped);
        if bytes.is_empty() {
            return false;
        }
        match hit.target {
            visual::VisualTarget::Remote { slot_id, .. } => {
                if let Some(graft) = self.tabs[self.active].grafts.get(&slot_id)
                {
                    graft.client.send_input(&bytes);
                    true
                } else {
                    false
                }
            }
            visual::VisualTarget::Local { .. } => {
                self.active_client().send_input(&bytes);
                true
            }
            visual::VisualTarget::Placeholder { .. } => false,
        }
    }

    fn kill_visual_pane(&mut self) {
        match self.tabs[self.active].visual_focus {
            VisualFocus::Remote { slot_id, .. } => {
                let last = self.tabs[self.active]
                    .grafts
                    .get(&slot_id)
                    .and_then(|g| g.client.latest_frame())
                    .map(|f| visual::leaf_count(&f.layout) <= 1)
                    .unwrap_or(true);
                if last {
                    self.exit_slot(slot_id);
                } else if let Some(graft) =
                    self.tabs[self.active].grafts.get(&slot_id)
                {
                    graft.client.run_command("kill-pane");
                }
            }
            VisualFocus::Local { .. } => {
                self.active_client().run_command("kill-pane");
            }
        }
    }

    fn exit_slot(&mut self, slot_id: u64) {
        let tab = &mut self.tabs[self.active];
        tab.pending_reconnect.remove(&slot_id);
        let generation =
            tab.grafts.get(&slot_id).map(|g| g.generation).unwrap_or(1);
        tab.grafts.remove(&slot_id);
        send_slot_state(tab.client.as_ref(), slot_id, "exited", generation);
        let local_id = tab
            .client
            .latest_frame()
            .and_then(|fd| visual::external_local_id(&fd.layout, slot_id));
        tab.visual_focus = VisualFocus::Local { pane_id: local_id };
        if let Some(id) = local_id {
            tab.client.run_command(&format!("select-pane -t %{id}"));
        }
    }

    fn active_index(&self) -> usize {
        self.active
    }

    fn active_code(&self) -> String {
        self.tabs[self.active].code.clone()
    }

    fn active_title(&self) -> String {
        self.tabs[self.active].title.clone()
    }

    fn active_socket_name(&self) -> String {
        self.tabs[self.active].socket_name.clone()
    }

    fn active_title_for_confirm(&self) -> String {
        if self.tabs[self.active].title.is_empty() {
            self.tabs[self.active].socket_name.clone()
        } else {
            self.tabs[self.active].title.clone()
        }
    }

    fn select(&mut self, index: usize) -> bool {
        if index >= self.tabs.len() {
            return false;
        }
        self.active = index;
        if index < self.tab_bar_offset {
            self.tab_bar_offset = index;
        }
        true
    }

    fn select_socket(&mut self, socket_name: &str) -> bool {
        if let Some(index) = self
            .tabs
            .iter()
            .position(|tab| tab.socket_name == socket_name)
        {
            self.active = index;
            true
        } else {
            false
        }
    }

    fn set_active_title(&mut self, title: String) {
        self.tabs[self.active].title = title;
        self.persist_active_metadata();
    }

    fn persist_active_metadata(&self) {
        let tab = &self.tabs[self.active];
        if let Err(e) =
            store_tab_metadata(&tab.socket_name, &tab.code, &tab.title)
        {
            log_client(&format!(
                "failed to store tab metadata for '{}': {}",
                tab.socket_name, e
            ));
        }
    }

    fn set_active_metadata(
        &mut self,
        code: &str,
        title: String,
    ) -> Result<(), String> {
        let code = normalize_tab_code(code)?;
        if self
            .tabs
            .iter()
            .enumerate()
            .any(|(index, tab)| index != self.active && tab.code == code)
        {
            return Err(format!("tab code {} already exists", code));
        }
        self.tabs[self.active].code = code;
        self.tabs[self.active].title = title;
        self.persist_active_metadata();
        Ok(())
    }

    fn create_tab(&mut self, size: Size) -> io::Result<()> {
        let id = self.next_id;
        self.next_id += 1;
        let socket_name =
            format!("{}.tab.{}.{}", self.base_socket, std::process::id(), id);
        let (client, _) = ensure_server_and_connect(
            &socket_name,
            "0",
            size,
            true,
            self.start_dir.as_deref(),
        )?;
        let code = next_available_tab_code(&self.tabs, id);
        self.tabs.push(ClientTab {
            code,
            title: String::new(),
            socket_name,
            client: Box::new(client),
            grafts: HashMap::new(),
            visual_focus: VisualFocus::Local { pane_id: None },
            pending_attach: None,
            pending_reconnect: HashMap::new(),
        });
        self.active = self.tabs.len() - 1;
        Ok(())
    }

    fn next_tab(&mut self) -> bool {
        if self.tabs.len() <= 1 {
            return false;
        }
        self.active = (self.active + 1) % self.tabs.len();
        true
    }

    fn prev_tab(&mut self) -> bool {
        if self.tabs.len() <= 1 {
            return false;
        }
        self.active = (self.active + self.tabs.len() - 1) % self.tabs.len();
        true
    }

    fn ensure_active_visible(&mut self, width: u16) -> bool {
        if self.tabs.is_empty() {
            return false;
        }
        let views = self.tab_views();
        let visible = tab_bar_visible_range(&views, width, self.tab_bar_offset);
        if self.active >= visible.start && self.active < visible.end {
            return false;
        }
        for offset in 0..=self.active.min(self.tabs.len().saturating_sub(1)) {
            let range = tab_bar_visible_range(&views, width, offset);
            if self.active >= range.start && self.active < range.end {
                if offset != self.tab_bar_offset {
                    self.tab_bar_offset = offset;
                    return true;
                }
                return false;
            }
        }
        false
    }

    fn scroll_tab_bar_back(&mut self) -> bool {
        if self.tab_bar_offset == 0 {
            return false;
        }
        self.tab_bar_offset -= 1;
        true
    }

    fn close_active(&mut self) -> bool {
        self.set_active_visibility(false);
        self.tabs[self.active].client.detach();
        self.remove_active()
    }

    fn close_dead_active(&mut self) -> bool {
        self.set_active_visibility(false);
        self.remove_active()
    }

    fn set_active_visibility(&self, visible: bool) {
        let tab = &self.tabs[self.active];
        if let Err(e) =
            set_tab_visibility(&tab.socket_name, &tab.code, &tab.title, visible)
        {
            log_client(&format!(
                "failed to store tab visibility for '{}': {}",
                tab.socket_name, e
            ));
        }
    }

    fn remove_active(&mut self) -> bool {
        self.tabs.remove(self.active);
        if self.tabs.is_empty() {
            return true;
        }
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        }
        false
    }

    fn show_socket(&mut self, socket_name: &str, size: Size) -> io::Result<()> {
        self.show_socket_with_code(socket_name, size, None)
    }

    fn show_socket_with_code(
        &mut self,
        socket_name: &str,
        size: Size,
        preferred_code: Option<&str>,
    ) -> io::Result<()> {
        if let Some(index) = self
            .tabs
            .iter()
            .position(|tab| tab.socket_name == socket_name)
        {
            let tab = &self.tabs[index];
            set_tab_visibility(socket_name, &tab.code, &tab.title, true)?;
            self.active = index;
            return Ok(());
        }
        let metadata = load_tab_metadata();
        let stored = metadata.get(socket_name);
        let preferred_code = preferred_code
            .and_then(|code| normalize_tab_code(code).ok())
            .filter(|code| !self.tabs.iter().any(|tab| tab.code == *code));
        let code = stored
            .and_then(|meta| meta.code.as_deref())
            .and_then(|code| normalize_tab_code(code).ok())
            .filter(|code| !self.tabs.iter().any(|tab| tab.code == *code))
            .or(preferred_code)
            .unwrap_or_else(|| {
                next_available_tab_code(&self.tabs, self.tabs.len())
            });
        let title =
            stored
                .and_then(|meta| meta.title.clone())
                .unwrap_or_else(|| {
                    attach_tab_title(&self.base_socket, socket_name)
                });
        let client = SocketClient::connect(socket_name, size)?;
        set_tab_visibility(socket_name, &code, &title, true)?;
        self.tabs.push(ClientTab {
            code,
            title,
            socket_name: socket_name.to_string(),
            client: Box::new(client),
            grafts: HashMap::new(),
            visual_focus: VisualFocus::Local { pane_id: None },
            pending_attach: None,
            pending_reconnect: HashMap::new(),
        });
        self.active = self.tabs.len() - 1;
        Ok(())
    }

    fn hide_socket(&mut self, socket_name: &str) -> Result<(), String> {
        if self.tabs.len() <= 1 {
            return Err("cannot hide the last visible tab".to_string());
        }
        let Some(index) = self
            .tabs
            .iter()
            .position(|tab| tab.socket_name == socket_name)
        else {
            return Ok(());
        };
        if let Err(e) = set_tab_visibility(
            socket_name,
            &self.tabs[index].code,
            &self.tabs[index].title,
            false,
        ) {
            log_client(&format!(
                "failed to store tab visibility for '{}': {}",
                socket_name, e
            ));
        }
        self.tabs[index].client.detach();
        self.tabs.remove(index);
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        } else if index < self.active {
            self.active -= 1;
        }
        if self.tab_bar_offset >= self.tabs.len() {
            self.tab_bar_offset = self.tabs.len().saturating_sub(1);
        }
        Ok(())
    }

    fn kill_socket(&mut self, socket_name: &str) -> io::Result<bool> {
        match SocketClient::kill_server_socket(socket_name) {
            Ok(()) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                ) => {}
            Err(e) => return Err(e),
        }
        self.killed_sockets.insert(socket_name.to_string());
        if let Err(e) = remove_tab_metadata(socket_name) {
            log_client(&format!(
                "failed to remove tab metadata for '{}': {}",
                socket_name, e
            ));
        }
        cleanup_killed_socket(socket_name);
        if let Some(index) = self
            .tabs
            .iter()
            .position(|tab| tab.socket_name == socket_name)
        {
            self.tabs[index].client.shutdown();
            self.tabs.remove(index);
            if self.tabs.is_empty() {
                return Ok(true);
            }
            if self.active >= self.tabs.len() {
                self.active = self.tabs.len() - 1;
            } else if index < self.active {
                self.active -= 1;
            }
        }
        Ok(self.tabs.is_empty())
    }

    fn remove_dead_inactive(&mut self) -> usize {
        let mut removed = 0;
        let mut index = 0;
        while index < self.tabs.len() {
            let dead = index != self.active
                && self.tabs[index]
                    .client
                    .latest_frame()
                    .as_ref()
                    .is_some_and(|frame| frame.exit);
            if dead {
                self.tabs[index].client.shutdown();
                self.tabs.remove(index);
                removed += 1;
                if index < self.active {
                    self.active -= 1;
                }
            } else {
                index += 1;
            }
        }
        removed
    }

    fn detach_all(&self) {
        for tab in &self.tabs {
            tab.client.detach();
        }
    }

    fn resize_all(&self, size: Size) {
        for tab in &self.tabs {
            tab.client.resize(size);
        }
    }

    fn tab_views(&self) -> Vec<ClientTabView> {
        self.tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| self.tab_view_for(index, tab))
            .collect()
    }

    fn tab_chooser_views(&self) -> Vec<ClientTabView> {
        let mut views = self.tab_views();
        let metadata = load_tab_metadata();
        let socket_names = discover_all_socket_names(&self.base_socket)
            .unwrap_or_else(|e| {
                log_client(&format!("failed to discover sockets: {}", e));
                Vec::new()
            });
        for socket_name in socket_names {
            if self.killed_sockets.contains(&socket_name) {
                continue;
            }
            if views.iter().any(|tab| tab.socket_name == socket_name) {
                continue;
            }
            let stored = metadata.get(&socket_name);
            let code =
                stored.and_then(|meta| meta.code.clone()).unwrap_or_else(
                    || next_available_view_code(&views, views.len()),
                );
            let title =
                stored.and_then(|meta| meta.title.clone()).unwrap_or_else(
                    || attach_tab_title(&self.base_socket, &socket_name),
                );
            views.push(ClientTabView {
                code,
                title,
                state: ClientTabState::Inactive,
                socket_name,
                visible: false,
            });
        }
        views
    }

    fn tab_view_for(&self, index: usize, tab: &ClientTab) -> ClientTabView {
        let dead = tab
            .client
            .latest_frame()
            .as_ref()
            .is_some_and(|frame| frame.exit);
        let state = if dead {
            ClientTabState::Dead
        } else if index == self.active {
            ClientTabState::Active
        } else {
            ClientTabState::Inactive
        };
        ClientTabView {
            code: tab.code.clone(),
            title: tab.title.clone(),
            state,
            socket_name: tab.socket_name.clone(),
            visible: true,
        }
    }
}

fn tab_code(index: usize) -> String {
    let index = index % (26 * 26);
    let first = (b'A' + (index / 26) as u8) as char;
    let second = (b'A' + (index % 26) as u8) as char;
    format!("{}{}", first, second)
}

fn attach_tab_title(base_socket: &str, socket_name: &str) -> String {
    let tab_prefix = format!("{}.tab.", base_socket);
    if socket_name == base_socket {
        return socket_name.to_string();
    }
    if let Some(rest) = socket_name.strip_prefix(&tab_prefix) {
        if let Some((_, id)) = rest.rsplit_once('.') {
            return format!("{}:{}", base_socket, id);
        }
    }
    socket_name.to_string()
}

type TabMetadataMap = std::collections::BTreeMap<String, StoredTabMetadata>;

struct TabMetadataLock {
    file: std::fs::File,
}

impl TabMetadataLock {
    fn acquire(data_path: &std::path::Path) -> io::Result<Self> {
        let lock_path = data_path.with_file_name(format!(
            "{}.lock",
            data_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("tab-metadata.json")
        ));
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&lock_path)?;
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { file }),
                Err(std::fs::TryLockError::WouldBlock)
                    if Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}

impl Drop for TabMetadataLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn read_tab_metadata_file(path: &std::path::Path) -> Option<TabMetadataMap> {
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

fn load_tab_metadata() -> TabMetadataMap {
    if let Ok(path) = tab_metadata_path() {
        let metadata = {
            let _lock = TabMetadataLock::acquire(&path).ok();
            read_tab_metadata_file(&path)
        };
        if let Some(metadata) = metadata {
            if !metadata.is_empty() {
                return metadata;
            }
        }
    }
    if let Ok(path) = legacy_tab_metadata_path() {
        if let Some(metadata) = read_tab_metadata_file(&path) {
            if !metadata.is_empty() {
                if let Err(e) = write_tab_metadata(&metadata) {
                    log_client(&format!(
                        "failed to migrate tab metadata: {}",
                        e
                    ));
                }
            }
            return metadata;
        }
    }
    TabMetadataMap::new()
}

fn store_tab_metadata(
    socket_name: &str,
    code: &str,
    title: &str,
) -> io::Result<()> {
    let path = tab_metadata_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _lock = TabMetadataLock::acquire(&path)?;
    let mut metadata = read_tab_metadata_file(&path)
        .or_else(|| {
            legacy_tab_metadata_path()
                .ok()
                .and_then(|path| read_tab_metadata_file(&path))
        })
        .unwrap_or_default();
    metadata.insert(
        socket_name.to_string(),
        StoredTabMetadata {
            code: Some(code.to_string()),
            title: Some(title.to_string()),
            visible: metadata
                .get(socket_name)
                .and_then(|meta| meta.visible)
                .or(Some(true)),
        },
    );
    write_tab_metadata_atomic(&path, &metadata)
}

fn set_tab_visibility(
    socket_name: &str,
    code: &str,
    title: &str,
    visible: bool,
) -> io::Result<()> {
    let path = tab_metadata_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _lock = TabMetadataLock::acquire(&path)?;
    let mut metadata = read_tab_metadata_file(&path).unwrap_or_default();
    metadata.insert(
        socket_name.to_string(),
        StoredTabMetadata {
            code: Some(code.to_string()),
            title: Some(title.to_string()),
            visible: Some(visible),
        },
    );
    write_tab_metadata_atomic(&path, &metadata)
}

fn remove_tab_metadata(socket_name: &str) -> io::Result<()> {
    let path = tab_metadata_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _lock = TabMetadataLock::acquire(&path)?;
    let mut metadata = read_tab_metadata_file(&path).unwrap_or_default();
    metadata.remove(socket_name);
    write_tab_metadata_atomic(&path, &metadata)
}

fn remove_tab_metadata_for_socket_family(socket_name: &str) {
    if let Err(e) = remove_tab_metadata_for_socket_family_inner(socket_name) {
        log_client(&format!(
            "failed to remove stale tab metadata for '{}': {}",
            socket_name, e
        ));
    }
}

fn remove_tab_metadata_for_socket_family_inner(
    socket_name: &str,
) -> io::Result<()> {
    let path = tab_metadata_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _lock = TabMetadataLock::acquire(&path)?;
    let mut metadata = read_tab_metadata_file(&path).unwrap_or_default();
    let tab_prefix = format!("{}.tab.", socket_name);
    metadata.retain(|name, _| {
        name != socket_name && !name.starts_with(&tab_prefix)
    });
    write_tab_metadata_atomic(&path, &metadata)
}

fn write_tab_metadata(metadata: &TabMetadataMap) -> io::Result<()> {
    let path = tab_metadata_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _lock = TabMetadataLock::acquire(&path)?;
    write_tab_metadata_atomic(&path, metadata)
}

fn write_tab_metadata_atomic(
    path: &std::path::Path,
    metadata: &TabMetadataMap,
) -> io::Result<()> {
    let data = serde_json::to_string_pretty(metadata)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let tmp_path = path.with_file_name(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("tab-metadata.json"),
        std::process::id()
    ));
    std::fs::write(&tmp_path, data)?;
    replace_metadata_file(&tmp_path, path)
}

#[cfg(not(windows))]
fn replace_metadata_file(
    tmp_path: &std::path::Path,
    path: &std::path::Path,
) -> io::Result<()> {
    std::fs::rename(tmp_path, path)
}

#[cfg(windows)]
fn replace_metadata_file(
    tmp_path: &std::path::Path,
    path: &std::path::Path,
) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let from: Vec<u16> = tmp_path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let to: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let ok = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn tab_metadata_path() -> io::Result<std::path::PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "HOME not set")
    })?;
    Ok(std::path::PathBuf::from(home)
        .join("Library")
        .join("Application Support")
        .join("zmux")
        .join("tab-metadata.json"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn tab_metadata_path() -> io::Result<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_STATE_HOME") {
        return Ok(std::path::PathBuf::from(dir)
            .join("zmux")
            .join("tab-metadata.json"));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "HOME not set")
    })?;
    Ok(std::path::PathBuf::from(home)
        .join(".local")
        .join("state")
        .join("zmux")
        .join("tab-metadata.json"))
}

#[cfg(windows)]
fn tab_metadata_path() -> io::Result<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("LOCALAPPDATA") {
        return Ok(std::path::PathBuf::from(dir)
            .join("zmux")
            .join("tab-metadata.json"));
    }
    if let Some(dir) = std::env::var_os("APPDATA") {
        return Ok(std::path::PathBuf::from(dir)
            .join("zmux")
            .join("tab-metadata.json"));
    }
    legacy_tab_metadata_path()
}

#[cfg(unix)]
fn legacy_tab_metadata_path() -> io::Result<std::path::PathBuf> {
    crate::ipc::socket_path("tab-metadata.json")
}

#[cfg(windows)]
fn legacy_tab_metadata_path() -> io::Result<std::path::PathBuf> {
    Ok(std::env::temp_dir().join("zmux-tab-metadata.json"))
}

fn next_available_tab_code(tabs: &[ClientTab], start: usize) -> String {
    for offset in 0..(26 * 26) {
        let code = tab_code(start + offset);
        if !tabs.iter().any(|tab| tab.code == code) {
            return code;
        }
    }
    "ZZ".to_string()
}

fn next_available_view_code(tabs: &[ClientTabView], start: usize) -> String {
    for offset in 0..(26 * 26) {
        let code = tab_code(start + offset);
        if !tabs.iter().any(|tab| tab.code == code) {
            return code;
        }
    }
    "ZZ".to_string()
}

fn normalize_tab_code(code: &str) -> Result<String, String> {
    let code = code.trim().to_ascii_uppercase();
    if code.len() != 2 || !code.bytes().all(|b| b.is_ascii_uppercase()) {
        return Err("tab code must be two uppercase letters".to_string());
    }
    Ok(code)
}

fn default_tab_chooser_mode(tabs: &TabManager) -> InputMode {
    InputMode::TabChooser {
        query: String::new(),
        cursor: 0,
        selected: tabs.active_index(),
        search_active: false,
    }
}

fn tab_quick_switch_mode() -> InputMode {
    InputMode::TabQuickSwitch {
        code: String::new(),
        error: None,
    }
}

fn select_tab_by_code(
    tabs: &mut TabManager,
    code: &str,
    size: Size,
) -> Result<(), String> {
    let code = normalize_tab_code(code)?;
    if let Some(index) = tabs.tabs.iter().position(|tab| tab.code == code) {
        tabs.select(index);
        tabs.active_client().resize(size);
        return Ok(());
    }

    let Some(tab) = tabs.tab_chooser_views().into_iter().find(|tab| {
        !tab.visible
            && normalize_tab_code(&tab.code)
                .is_ok_and(|tab_code| tab_code == code)
    }) else {
        return Err(format!("tab not found: {}", code));
    };

    tabs.show_socket_with_code(&tab.socket_name, size, Some(&code))
        .map_err(|e| format!("show tab failed: {}", e))?;
    if !tabs.select_socket(&tab.socket_name) {
        return Err(format!("tab not found: {}", code));
    }
    tabs.active_client().resize(size);
    Ok(())
}

fn floating_overlay_rect(
    mode: &InputMode,
    area: ratatui::layout::Rect,
) -> Option<ratatui::layout::Rect> {
    if area.width == 0 || area.height == 0 {
        return None;
    }
    match mode {
        InputMode::TabChooser { .. } | InputMode::SessionChooser { .. } => {
            Some(chooser_overlay_panel(area))
        }
        InputMode::RenameTab { .. } => Some(rename_tab_panel_rect(area)),
        InputMode::OptionPanel { .. } => Some(options_panel_rect(area)),
        InputMode::TabQuickSwitch { .. } => {
            Some(tab_quick_switch_panel_rect(area))
        }
        _ => None,
    }
}

fn rename_tab_mode_for_active(
    tabs: &TabManager,
    return_to_tab_chooser: bool,
) -> InputMode {
    let code = tabs.active_code();
    let title = tabs.active_title();
    InputMode::RenameTab {
        code_cursor: code.chars().count(),
        title_cursor: title.chars().count(),
        code,
        title,
        editing_code: true,
        error: None,
        return_to_tab_chooser,
    }
}

fn tab_label(tab: &ClientTabView) -> String {
    if tab.title.is_empty() {
        tab.socket_name.clone()
    } else {
        tab.title.clone()
    }
}

fn matching_tab_indices(tabs: &[ClientTabView], query: &str) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    tabs.iter()
        .enumerate()
        .filter_map(|(index, tab)| {
            if query.is_empty()
                || tab.code.to_lowercase().contains(&query)
                || tab.title.to_lowercase().contains(&query)
                || tab.socket_name.to_lowercase().contains(&query)
            {
                Some(index)
            } else {
                None
            }
        })
        .collect()
}

fn tab_target_index(tabs: &TabManager, target: &str) -> Option<usize> {
    let target = target.trim();
    if target.is_empty() {
        return None;
    }
    if let Ok(index) = target.parse::<usize>() {
        if index < tabs.tabs.len() {
            return Some(index);
        }
    }
    let target_lower = target.to_lowercase();
    tabs.tabs.iter().position(|tab| {
        tab.code.to_lowercase() == target_lower
            || tab.title.to_lowercase() == target_lower
    })
}

fn tab_summary(tabs: &TabManager) -> String {
    tabs.tabs
        .iter()
        .enumerate()
        .map(|(index, tab)| {
            let active = if index == tabs.active { "*" } else { "-" };
            let title = if tab.title.is_empty() {
                "(untitled)".to_string()
            } else {
                tab.title.clone()
            };
            format!("{}{} {}", tab.code, active, title)
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

enum ClientTabCommandResult {
    Handled(Option<String>),
    NotHandled,
}

impl ClientApp {
    pub fn new(
        socket_name: &str,
        session_name: Option<String>,
        clean: bool,
        start_dir: Option<String>,
    ) -> Self {
        Self::new_with_initial_tab_title(
            socket_name,
            session_name,
            clean,
            start_dir,
            None,
        )
    }

    pub fn new_with_initial_tab_title(
        socket_name: &str,
        session_name: Option<String>,
        clean: bool,
        start_dir: Option<String>,
        initial_tab_title: Option<String>,
    ) -> Self {
        Self {
            socket_name: socket_name.to_string(),
            session_name,
            clean,
            start_dir,
            initial_tab_title,
            attach_all: false,
            ssh_host: None,
        }
    }

    pub fn new_attach_all(
        socket_name: &str,
        session_name: Option<String>,
        clean: bool,
        start_dir: Option<String>,
    ) -> Self {
        Self {
            socket_name: socket_name.to_string(),
            session_name,
            clean,
            start_dir,
            initial_tab_title: None,
            attach_all: true,
            ssh_host: None,
        }
    }

    pub fn new_ssh(
        host: String,
        socket_name: &str,
        start_dir: Option<String>,
    ) -> Self {
        Self {
            socket_name: socket_name.to_string(),
            session_name: None,
            clean: false,
            start_dir,
            initial_tab_title: Some(host.clone()),
            attach_all: false,
            ssh_host: Some(host),
        }
    }

    pub fn run(&self) -> io::Result<()> {
        install_client_panic_hook();
        let (cols, rows) = terminal::size().unwrap_or((80, 24));
        let size = server_content_size(cols, rows);
        let session_name =
            self.session_name.clone().unwrap_or_else(|| "0".to_string());

        #[cfg(unix)]
        crate::pty::remember_host_termios();

        let mut tabs = if let Some(host) = &self.ssh_host {
            TabManager::from_ssh(
                host,
                &self.socket_name,
                size,
                self.start_dir.clone(),
            )?
        } else if self.attach_all {
            let socket_names = discover_all_socket_names(&self.socket_name)?;
            match TabManager::from_existing_sockets(
                &self.socket_name,
                socket_names,
                self.session_name.as_deref(),
                size,
                self.start_dir.clone(),
            ) {
                Ok(tabs) => tabs,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    TabManager::new(
                        &self.socket_name,
                        &session_name,
                        size,
                        self.clean,
                        self.start_dir.clone(),
                    )?
                }
                Err(e) => return Err(e),
            }
        } else {
            TabManager::new(
                &self.socket_name,
                &session_name,
                size,
                self.clean,
                self.start_dir.clone(),
            )?
        };
        if let Some(title) = &self.initial_tab_title {
            tabs.set_active_title(title.clone());
        }

        terminal::enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            DisableLineWrap,
            cursor::Hide,
            EnableBracketedPaste,
            EnableMouseCapture
        )?;
        let keyboard_enhancement_enabled = match execute!(
            stdout,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES,
            )
        ) {
            Ok(()) => true,
            Err(e) if e.kind() == io::ErrorKind::Unsupported => false,
            Err(e) => return Err(e),
        };
        let backend = TerminalBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        let mut mouse_select: Option<MouseSelection> = None;
        let mut mouse_drag_origin: Option<MouseDragOrigin> = None;
        let mut last_mouse_click: Option<LastMouseClick> = None;

        let prefix_key = (KeyCode::Char('a'), KeyModifiers::CONTROL);
        let mut mode = InputMode::Normal;
        let mut copy_mode_confirmed = false;
        let mut prefix_from_copy_mode = false;
        let mut copy_mode_sync_suppress_frame: Option<u64> = None;
        let mut copy_mode_exit_pending = false;
        let mut display_scrolled = false;
        let mut resize_deadline: Option<Instant> = None;
        let mut status_notice: Option<(String, Instant)> = None;
        let mut hide_borders = false;
        let mut applied_cursor_style: Option<SetCursorStyle> = None;
        let mut applied_mouse_pointer: Option<MousePointerShape> = None;
        let mut last_mouse_pos: Option<(u16, u16)> = None;
        let mut last_drawn_mouse_select: Option<MouseSelection> = None;
        let mut last_drawn_counter: u64 = 0;
        let mut last_ansi_frame: Option<(String, u64)> = None;
        let mut last_overlay_rect: Option<ratatui::layout::Rect> = None;
        let run_result: io::Result<()> = (|| {
            loop {
                let (frame, current_counter) = tabs.display_snapshot();
                let active_socket_name = tabs.active_socket_name();
                if matches!(
                    copy_mode_sync_suppress_frame,
                    Some(counter) if counter != current_counter
                ) {
                    copy_mode_sync_suppress_frame = None;
                }
                if let Some(ref fd) = frame {
                    if fd.exit {
                        log_client("received exit frame for active tab");
                        if tabs.close_dead_active() {
                            break;
                        }
                        mode = InputMode::Normal;
                        copy_mode_confirmed = false;
                        mouse_select = None;
                        last_drawn_counter = 0;
                        status_notice = Some((
                            "tab closed".to_string(),
                            Instant::now() + Duration::from_secs(3),
                        ));
                        continue;
                    }
                    let removed_dead_tabs = tabs.remove_dead_inactive();
                    if removed_dead_tabs > 0 {
                        last_drawn_counter = 0;
                        status_notice = Some((
                            if removed_dead_tabs == 1 {
                                "closed 1 dead tab".to_string()
                            } else {
                                format!(
                                    "closed {} dead tabs",
                                    removed_dead_tabs
                                )
                            },
                            Instant::now() + Duration::from_secs(3),
                        ));
                    }
                    let (cols, _) = terminal::size().unwrap_or((80, 24));
                    if tabs.ensure_active_visible(cols) {
                        last_drawn_counter = 0;
                    }
                    if !active_in_copy_mode(fd) {
                        copy_mode_exit_pending = false;
                    }
                    if mode == InputMode::CopyMode {
                        if active_in_copy_mode(fd) {
                            copy_mode_confirmed = true;
                        } else if copy_mode_confirmed {
                            mode = InputMode::Normal;
                            copy_mode_confirmed = false;
                        }
                    } else if mode == InputMode::Normal
                        && active_in_copy_mode(fd)
                        && !copy_mode_exit_pending
                        && !matches!(
                            copy_mode_sync_suppress_frame,
                            Some(suppressed) if suppressed == current_counter
                        )
                    {
                        // The server entered copy mode on its own (e.g. via
                        // mouse-scroll auto-enter).  Sync the client so that
                        // copy-mode key bindings (including 'q' to exit) work
                        // correctly instead of forwarding keys to the shell.
                        mode = InputMode::CopyMode;
                        copy_mode_confirmed = true;
                    }
                }
                let desired_cursor_style = cursor_style_for_shape(
                    frame.as_ref().and_then(active_cursor_shape),
                );
                if applied_cursor_style != Some(desired_cursor_style) {
                    execute!(terminal.backend_mut(), desired_cursor_style)?;
                    applied_cursor_style = Some(desired_cursor_style);
                }
                let now = Instant::now();
                if mode == InputMode::Resize
                    && matches!(resize_deadline, Some(expires_at) if now >= expires_at)
                {
                    mode = InputMode::Normal;
                    resize_deadline = None;
                    last_drawn_counter = 0;
                }
                if mode != InputMode::Resize {
                    resize_deadline = None;
                }
                if matches!(status_notice.as_ref(), Some((_, expires_at)) if now >= *expires_at)
                {
                    status_notice = None;
                    last_drawn_counter = 0;
                }
                let status_notice_text =
                    status_notice.as_ref().map(|(text, _)| text.clone());
                let status_banner = status_banner_for_mode(
                    &mode,
                    status_notice_text.as_deref(),
                );
                let has_prompt = matches!(
                    mode,
                    InputMode::CopySearch { .. }
                        | InputMode::RenameWindow { .. }
                        | InputMode::RenameSession { .. }
                        | InputMode::Command { .. }
                        | InputMode::ConfirmKillTab { .. }
                );
                let has_overlay = matches!(
                    mode,
                    InputMode::SessionChooser { .. }
                        | InputMode::OptionPanel { .. }
                        | InputMode::TabChooser { .. }
                        | InputMode::RenameTab { .. }
                        | InputMode::TabQuickSwitch { .. }
                );
                let hide_status = has_prompt;

                let (cols, rows) = terminal::size().unwrap_or((80, 24));
                let mut drew_terminal_output = false;
                let terminal_area =
                    ratatui::layout::Rect::new(0, 0, cols, rows);
                let current_overlay_rect =
                    floating_overlay_rect(&mode, terminal_area);
                let stale_overlay_rect =
                    if last_overlay_rect != current_overlay_rect {
                        last_overlay_rect
                    } else {
                        None
                    };
                if stale_overlay_rect.is_some() {
                    // The old overlay was drawn directly over server ANSI. Ask the
                    // server for a complete pane repaint before drawing again;
                    // replaying the latest incremental frame would leave the
                    // covered portion of an inactive pane blank.
                    tabs.active_client().refresh_display();
                    last_drawn_counter = current_counter;
                    last_overlay_rect = current_overlay_rect;
                }

                let redraw_needed = current_counter != last_drawn_counter;
                let server_frame_new = last_ansi_frame.as_ref().is_none_or(
                    |(socket_name, counter)| {
                        socket_name != &active_socket_name
                            || *counter != current_counter
                    },
                );
                if redraw_needed {
                    drew_terminal_output = true;
                    let mut server_ansi_update_open = false;
                    if server_frame_new {
                        if let Some(ref fd) = frame {
                            if should_write_server_ansi(has_overlay, has_prompt)
                            {
                                if let Some(ref ansi) = fd.ansi {
                                    if !ansi.trim().is_empty() {
                                        match begin_server_ansi_update(
                                            terminal.backend_mut(),
                                        ) {
                                            Ok(()) => {
                                                server_ansi_update_open = true;
                                                if let Err(err) =
                                                    write_server_ansi_payload(
                                                        terminal.backend_mut(),
                                                        ansi,
                                                    )
                                                {
                                                    log_client(&format!(
                                                        "failed to write pane ansi: {err}"
                                                    ));
                                                }
                                                let graft_ansi =
                                                    tabs.paint_grafts(
                                                        hide_borders,
                                                    );
                                                if !graft_ansi.is_empty() {
                                                    if let Err(err) =
                                                        terminal
                                                            .backend_mut()
                                                            .write_all(
                                                                graft_ansi
                                                                    .as_bytes(),
                                                            )
                                                    {
                                                        log_client(&format!(
                                                            "failed to write graft ansi: {err}"
                                                        ));
                                                    }
                                                }
                                            }
                                            Err(err) => log_client(&format!(
                                                "failed to begin pane ANSI paint: {err}"
                                            )),
                                        }
                                    }
                                    if let Err(err) = refresh_mouse_pointer(
                                        terminal.backend_mut(),
                                        &mut applied_mouse_pointer,
                                        last_mouse_pos,
                                        frame.as_ref(),
                                        cols,
                                        rows,
                                        hide_borders,
                                        hide_status,
                                        has_overlay || has_prompt,
                                        true,
                                    ) {
                                        log_client(&format!(
                                            "failed to set mouse pointer after ansi: {err}"
                                        ));
                                    }
                                    // An incremental ANSI frame can update a sibling
                                    // pane only. Keep the last highlighted bounds
                                    // while the selection has just been cleared so
                                    // the post-draw restore below can repaint its
                                    // underlying cells. While a selection remains
                                    // active, invalidate it so an ANSI update cannot
                                    // erase the highlight permanently.
                                    if mouse_select.is_some() {
                                        last_drawn_mouse_select = None;
                                    }
                                }
                            }
                        }
                    }
                    let draw_ansi_selection = mouse_select
                        .as_ref()
                        .is_some_and(|sel| !selection_is_empty(sel))
                        && frame.as_ref().is_some_and(|fd| fd.ansi.is_some());
                    if let Err(err) = terminal.draw(|f| {
                        let in_prefix = mode == InputMode::Prefix;
                        if let Some(ref fd) = frame {
                            skip_pane_area_for_ansi(f, fd, hide_borders);
                            let mut display_frame = fd.clone();
                            if let Some(ref message) = status_banner {
                                if let Some(status) =
                                    display_frame.status.as_mut()
                                {
                                    status.right = message.clone();
                                }
                            }
                            let tab_views = tabs.tab_views();
                            render_tabbed_frame(
                                f,
                                &display_frame,
                                &tab_views,
                                tabs.tab_bar_offset,
                                in_prefix,
                                hide_status,
                                hide_borders,
                            );
                        } else {
                            let tab_views = tabs.tab_views();
                            render_tabbed_loading(
                                f,
                                &tab_views,
                                tabs.tab_bar_offset,
                            );
                        }
                        match &mode {
                            InputMode::CopySearch { buf, forward, .. } => {
                                render_prompt(
                                    f,
                                    if *forward { "/" } else { "?" },
                                    buf,
                                )
                            }
                            InputMode::RenameWindow { buf, .. } => {
                                render_prompt(f, "Rename window: ", buf)
                            }
                            InputMode::RenameSession { buf, .. } => {
                                render_prompt(f, "Rename session: ", buf)
                            }
                            InputMode::RenameTab {
                                code,
                                title,
                                editing_code,
                                error,
                                ..
                            } => render_rename_tab_panel(
                                f,
                                code,
                                title,
                                *editing_code,
                                error.as_deref(),
                            ),
                            InputMode::Command { buf, .. } => {
                                render_prompt(f, ":", buf)
                            }
                            InputMode::TabQuickSwitch { code, error } => {
                                render_tab_quick_switch_panel(
                                    f,
                                    code,
                                    error.as_deref(),
                                )
                            }
                            InputMode::ConfirmKillTab { label, .. } => {
                                render_prompt(
                                    f,
                                    &format!(
                                        "Kill tab '{}' and its server? [y/N] ",
                                        label
                                    ),
                                    "",
                                )
                            }
                            InputMode::SessionChooser {
                                entries,
                                selected,
                                collapsed,
                                collapsed_windows,
                            } => render_session_chooser(
                                f,
                                entries,
                                *selected,
                                collapsed,
                                collapsed_windows,
                            ),
                            InputMode::OptionPanel {
                                selected,
                                scroll_on_erase_in_display,
                            } => render_options_panel(
                                f,
                                *selected,
                                *scroll_on_erase_in_display,
                            ),
                            InputMode::TabChooser {
                                query,
                                selected,
                                search_active,
                                ..
                            } => {
                                let tab_views = tabs.tab_chooser_views();
                                render_tab_chooser(
                                    f,
                                    &tab_views,
                                    query,
                                    *selected,
                                    *search_active,
                                );
                            }

                            _ => {}
                        }
                        if let Some(ref sel) = mouse_select {
                            if let Some(ref fd) = frame {
                                if fd.ansi.is_none() {
                                    render_mouse_selection(
                                        f,
                                        sel,
                                        fd,
                                        hide_borders,
                                    );
                                }
                            }
                        }
                    }) {
                        if server_ansi_update_open {
                            let _ =
                                end_server_ansi_update(terminal.backend_mut());
                        }
                        return Err(err);
                    }
                    if frame.as_ref().is_some_and(|fd| fd.ansi.is_some()) {
                        if let Some(prev) = last_drawn_mouse_select.take() {
                            if mouse_select.is_none() {
                                let (sr, sc, er, ec) = prev.normalized_bounds();
                                if let Some(ref fd) = frame {
                                    let layout_area =
                                        server_layout_area(cols, rows);
                                    if let Err(err) =
                                        restore_mouse_selection_ansi(
                                            terminal.backend_mut(),
                                            fd,
                                            sr,
                                            sc,
                                            er,
                                            ec,
                                            layout_area,
                                            hide_borders,
                                        )
                                    {
                                        log_client(&format!(
                                            "failed to restore after selection: {err}"
                                        ));
                                    }
                                }
                            } else {
                                last_drawn_mouse_select = Some(prev);
                            }
                        }
                    }
                    if draw_ansi_selection {
                        if let (Some(ref fd), Some(sel)) =
                            (frame.as_ref(), mouse_select.as_ref())
                        {
                            let layout_area = server_layout_area(cols, rows);
                            let selection_changed = last_drawn_mouse_select
                                .is_none_or(|prev| {
                                    !mouse_selection_bounds_eq(&prev, sel)
                                });
                            if selection_changed {
                                let (sr, sc, er, ec) = sel.normalized_bounds();
                                let mut repaint_start = sr;
                                let mut repaint_end = er;
                                if let Some(prev) = last_drawn_mouse_select {
                                    let (prev_sr, _, prev_er, _) =
                                        prev.normalized_bounds();
                                    repaint_start = repaint_start.min(prev_sr);
                                    repaint_end = repaint_end.max(prev_er);
                                }
                                if let Err(err) =
                                    write_active_pane_selection_ansi(
                                        terminal.backend_mut(),
                                        fd,
                                        layout_area,
                                        hide_borders,
                                        repaint_start,
                                        repaint_end,
                                        sr,
                                        sc,
                                        er,
                                        ec,
                                    )
                                {
                                    log_client(&format!(
                                        "failed to draw selection overlay: {err}"
                                    ));
                                }
                                last_drawn_mouse_select = Some(*sel);
                            }
                        }
                    }
                    if let Some(ref fd) = frame {
                        if fd.ansi.is_some() {
                            if has_overlay || has_prompt {
                                terminal.hide_cursor()?;
                            } else {
                                let (cols, rows) =
                                    terminal::size().unwrap_or((80, 24));
                                let frame_area = server_frame_area(cols, rows);
                                if let Some(pos) = active_cursor_screen_position(
                                    fd,
                                    frame_area,
                                    hide_borders,
                                ) {
                                    terminal.set_cursor_position(pos)?;
                                    terminal.show_cursor()?;
                                } else {
                                    terminal.hide_cursor()?;
                                }
                            }
                        }
                    }
                    if server_ansi_update_open {
                        end_server_ansi_update(terminal.backend_mut())?;
                    }
                    last_drawn_counter = current_counter;
                    if server_frame_new {
                        last_ansi_frame =
                            Some((active_socket_name, current_counter));
                    }
                    last_overlay_rect = current_overlay_rect;
                }

                if let Err(err) = refresh_mouse_pointer(
                    terminal.backend_mut(),
                    &mut applied_mouse_pointer,
                    last_mouse_pos,
                    frame.as_ref(),
                    cols,
                    rows,
                    hide_borders,
                    hide_status,
                    has_overlay || has_prompt,
                    drew_terminal_output,
                ) {
                    log_client(&format!("failed to set mouse pointer: {err}"));
                }

                if event::poll(Duration::from_millis(8))? {
                    match event::read()? {
                        Event::Key(key)
                            if key.kind == KeyEventKind::Press
                                || key.kind == KeyEventKind::Repeat =>
                        {
                            match mode.clone() {
                                InputMode::Normal => {
                                    if (key.code, key.modifiers) == prefix_key {
                                        mode = InputMode::Prefix;
                                    } else if matches!(
                                        (key.code, key.modifiers),
                                        (KeyCode::Esc, _)
                                            | (
                                                KeyCode::Char('q'),
                                                KeyModifiers::NONE,
                                            )
                                    ) && display_scrolled
                                    {
                                        tabs.focused_client()
                                            .scroll_display_bottom();
                                        display_scrolled = false;
                                        last_drawn_counter = 0;
                                    } else {
                                        if frame
                                            .as_ref()
                                            .is_some_and(active_in_copy_mode)
                                            && copy_mode_exit_pending
                                        {
                                            leave_copy_mode_client(
                                                tabs.focused_client(),
                                                &mut mode,
                                                &mut copy_mode_confirmed,
                                                &mut copy_mode_exit_pending,
                                                &mut display_scrolled,
                                            );
                                        }
                                        let bytes = key_to_bytes(key);
                                        if !bytes.is_empty() {
                                            tabs.focused_client()
                                                .send_input(&bytes);
                                        }
                                    }
                                }

                                InputMode::Prefix => {
                                    mode = InputMode::Normal;
                                    let prefix_started_from_copy_mode =
                                        prefix_from_copy_mode;
                                    prefix_from_copy_mode = false;
                                    if (key.code, key.modifiers) == prefix_key {
                                        let bytes = key_to_bytes(key);
                                        if !bytes.is_empty() {
                                            tabs.focused_client()
                                                .send_input(&bytes);
                                        }
                                        continue;
                                    }
                                    if prefix_started_from_copy_mode {
                                        suppress_copy_mode_client_sync(
                                            &mut copy_mode_confirmed,
                                            &mut copy_mode_sync_suppress_frame,
                                            current_counter,
                                        );
                                    }
                                    if is_resize_modifier_key(key) {
                                        mode = InputMode::Resize;
                                        resize_deadline = Some(
                                            Instant::now()
                                                + RESIZE_IDLE_TIMEOUT,
                                        );
                                        continue;
                                    }
                                    if let Some(cmd) =
                                        resize_command_for_key(key)
                                    {
                                        let dir = match cmd {
                                            "resize-pane -L" => {
                                                crate::layout::NavDir::Left
                                            }
                                            "resize-pane -R" => {
                                                crate::layout::NavDir::Right
                                            }
                                            "resize-pane -U" => {
                                                crate::layout::NavDir::Up
                                            }
                                            _ => crate::layout::NavDir::Down,
                                        };
                                        tabs.resize_visual(dir);
                                        last_drawn_counter = 0;
                                        mode = InputMode::Resize;
                                        resize_deadline = Some(
                                            Instant::now()
                                                + RESIZE_IDLE_TIMEOUT,
                                        );
                                        continue;
                                    }
                                    match (key.code, key.modifiers) {
                                        (
                                            KeyCode::Char('d'),
                                            KeyModifiers::NONE,
                                        ) => {
                                            tabs.detach_all();
                                            break;
                                        }
                                        (
                                            KeyCode::Char('t'),
                                            KeyModifiers::NONE,
                                        ) => {
                                            if prefix_started_from_copy_mode
                                                || frame.as_ref().is_some_and(
                                                    active_in_copy_mode,
                                                )
                                            {
                                                tabs.focused_client()
                                                    .exit_copy_mode();
                                                copy_mode_confirmed = false;
                                            }
                                            mode =
                                                default_tab_chooser_mode(&tabs);
                                            last_drawn_counter = 0;
                                        }
                                        (KeyCode::Char('/'), _) => {
                                            mode = tab_quick_switch_mode();
                                        }
                                        (KeyCode::Char('T'), _) => {
                                            mode = rename_tab_mode_for_active(
                                                &tabs, false,
                                            );
                                        }
                                        (KeyCode::Tab, _) => {
                                            if tabs.next_tab() {
                                                let (cols, rows) =
                                                    terminal::size()
                                                        .unwrap_or((80, 24));
                                                tabs.active_client().resize(
                                                    server_content_size(
                                                        cols, rows,
                                                    ),
                                                );
                                                mode = InputMode::Normal;
                                                copy_mode_confirmed = false;
                                                mouse_select = None;
                                                last_drawn_counter = 0;
                                            }
                                        }
                                        (KeyCode::BackTab, _) => {
                                            if tabs.prev_tab() {
                                                let (cols, rows) =
                                                    terminal::size()
                                                        .unwrap_or((80, 24));
                                                tabs.active_client().resize(
                                                    server_content_size(
                                                        cols, rows,
                                                    ),
                                                );
                                                mode = InputMode::Normal;
                                                copy_mode_confirmed = false;
                                                mouse_select = None;
                                                last_drawn_counter = 0;
                                            }
                                        }
                                        (
                                            KeyCode::Char('w'),
                                            KeyModifiers::NONE,
                                        ) => {
                                            if tabs.close_active() {
                                                break;
                                            }
                                            mode = InputMode::Normal;
                                            copy_mode_confirmed = false;
                                            mouse_select = None;
                                            last_drawn_counter = 0;
                                        }
                                        (KeyCode::Char('W'), _) => {
                                            mode = InputMode::ConfirmKillTab {
                                                socket_name: tabs
                                                    .active_socket_name(),
                                                label: tabs
                                                    .active_title_for_confirm(),
                                                return_to_tab_chooser: false,
                                            };
                                        }
                                        (KeyCode::Char(','), _) => {
                                            let cur = tabs
                                                .active_client()
                                                .active_window_name();
                                            let len = cur.len();
                                            mode = InputMode::RenameWindow {
                                                buf: cur,
                                                cursor: len,
                                            };
                                        }
                                        (KeyCode::Char('$'), _) => {
                                            let cur = tabs
                                                .active_client()
                                                .session_name();
                                            let len = cur.len();
                                            mode = InputMode::RenameSession {
                                                buf: cur,
                                                cursor: len,
                                            };
                                        }
                                        (KeyCode::Char(':'), _) => {
                                            mode = InputMode::Command {
                                                buf: String::new(),
                                                cursor: 0,
                                            };
                                        }
                                        (KeyCode::Char('O'), _) => {
                                            mode = InputMode::OptionPanel {
                                                selected: 0,
                                                scroll_on_erase_in_display: tabs.active_client()
                                                    .scroll_on_erase_in_display(),
                                            };
                                        }
                                        (KeyCode::Char('['), _) => {
                                            if tabs
                                                .focused_client()
                                                .enter_copy_mode()
                                            {
                                                mode = InputMode::CopyMode;
                                                copy_mode_confirmed = false;
                                                copy_mode_exit_pending = false;
                                                display_scrolled = false;
                                            } else {
                                                status_notice = Some((
                                                    "copy mode unavailable"
                                                        .to_string(),
                                                    Instant::now()
                                                        + Duration::from_secs(
                                                            3,
                                                        ),
                                                ));
                                            }
                                        }
                                        (
                                            KeyCode::Char('s'),
                                            KeyModifiers::NONE,
                                        ) => {
                                            let entries = tabs
                                                .active_client()
                                                .session_tree();
                                            let focus = frame
                                                .as_ref()
                                                .and_then(
                                                active_session_focus_from_frame,
                                            );
                                            let (collapsed, collapsed_windows, sel) =
                                                build_initial_session_chooser_state(
                                                    &entries, focus,
                                                );
                                            mode = InputMode::SessionChooser {
                                                entries,
                                                selected: sel,
                                                collapsed,
                                                collapsed_windows,
                                            };
                                            last_drawn_counter = 0;
                                        }
                                        (KeyCode::Char('('), _) => {
                                            tabs.active_client()
                                                .run_command("prev-session");
                                        }
                                        (KeyCode::Char(')'), _) => {
                                            tabs.active_client()
                                                .run_command("next-session");
                                        }
                                        (
                                            KeyCode::Char('b'),
                                            KeyModifiers::NONE,
                                        ) => {
                                            hide_borders = !hide_borders;
                                            tabs.active_client()
                                                .set_hide_borders(hide_borders);
                                        }
                                        _ if is_shifted_letter(key, 'H') => {
                                            if let Some(message) =
                                                set_tab_start_dir(&mut tabs)
                                            {
                                                status_notice = Some((
                                                    message,
                                                    Instant::now()
                                                        + Duration::from_secs(
                                                            3,
                                                        ),
                                                ));
                                            }
                                        }
                                        (
                                            KeyCode::Char(']'),
                                            KeyModifiers::NONE,
                                        ) => {
                                            match tabs
                                                .focused_client()
                                                .paste_cloud()
                                            {
                                                Ok(message) => {
                                                    status_notice = Some((
                                                        message,
                                                        Instant::now()
                                                            + Duration::from_secs(
                                                                3,
                                                            ),
                                                    ));
                                                }
                                                Err(message) => {
                                                    status_notice = Some((
                                                        message,
                                                        Instant::now()
                                                            + Duration::from_secs(
                                                                3,
                                                            ),
                                                    ));
                                                }
                                            }
                                        }
                                        _ => {
                                            if let Some(dir) =
                                                prefix_nav_dir(key)
                                            {
                                                tabs.visual_move(
                                                    dir,
                                                    hide_borders,
                                                );
                                            } else if matches!(
                                                (key.code, key.modifiers),
                                                (
                                                    KeyCode::Char('x'),
                                                    KeyModifiers::NONE
                                                )
                                            ) {
                                                tabs.kill_visual_pane();
                                            } else if let Some(message) =
                                                handle_prefix_key(
                                                    tabs.focused_client(),
                                                    key,
                                                )
                                            {
                                                status_notice = Some((
                                                    message,
                                                    Instant::now()
                                                        + Duration::from_secs(
                                                            3,
                                                        ),
                                                ));
                                            }
                                        }
                                    }
                                }

                                InputMode::Resize => {
                                    if is_resize_modifier_key(key) {
                                        resize_deadline = Some(
                                            Instant::now()
                                                + RESIZE_IDLE_TIMEOUT,
                                        );
                                        continue;
                                    }
                                    if let Some(cmd) =
                                        resize_command_for_key(key)
                                    {
                                        let dir = match cmd {
                                            "resize-pane -L" => {
                                                crate::layout::NavDir::Left
                                            }
                                            "resize-pane -R" => {
                                                crate::layout::NavDir::Right
                                            }
                                            "resize-pane -U" => {
                                                crate::layout::NavDir::Up
                                            }
                                            _ => crate::layout::NavDir::Down,
                                        };
                                        tabs.resize_visual(dir);
                                        last_drawn_counter = 0;
                                        resize_deadline = Some(
                                            Instant::now()
                                                + RESIZE_IDLE_TIMEOUT,
                                        );
                                        continue;
                                    }
                                    if key.code == KeyCode::Esc {
                                        mode = InputMode::Normal;
                                        resize_deadline = None;
                                        continue;
                                    }
                                    if (key.code, key.modifiers) == prefix_key {
                                        mode = InputMode::Prefix;
                                        resize_deadline = None;
                                        continue;
                                    }
                                    mode = InputMode::Normal;
                                    resize_deadline = None;
                                }

                                InputMode::Command {
                                    mut buf,
                                    mut cursor,
                                } => match key.code {
                                    KeyCode::Enter => {
                                        let trimmed = buf.trim().to_string();
                                        mode = InputMode::Normal;
                                        if !trimmed.is_empty() {
                                            let (cols, rows) = terminal::size()
                                                .unwrap_or((80, 24));
                                            match run_client_tab_command(
                                                &mut tabs,
                                                &trimmed,
                                                server_content_size(cols, rows),
                                            ) {
                                                ClientTabCommandResult::Handled(message) => {
                                                    copy_mode_confirmed = false;
                                                    mouse_select = None;
                                                    last_drawn_counter = 0;
                                                    if let Some(message) = message {
                                                        status_notice = Some((
                                                            message,
                                                            Instant::now() + Duration::from_secs(3),
                                                        ));
                                                    }
                                                }
                                                ClientTabCommandResult::NotHandled => {
                                                    if let Some(message) = run_command_notice(
                                                        tabs.focused_client(),
                                                        &trimmed,
                                                    ) {
                                                        status_notice = Some((
                                                            message,
                                                            Instant::now() + Duration::from_secs(3),
                                                        ));
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    KeyCode::Esc => {
                                        mode = InputMode::Normal;
                                    }
                                    KeyCode::Backspace => {
                                        if cursor > 0 {
                                            let bp =
                                                char_byte_pos(&buf, cursor - 1);
                                            let ep =
                                                char_byte_pos(&buf, cursor);
                                            buf.drain(bp..ep);
                                            cursor -= 1;
                                        }
                                        mode =
                                            InputMode::Command { buf, cursor };
                                    }
                                    KeyCode::Left => {
                                        if cursor > 0 {
                                            cursor -= 1;
                                        }
                                        mode =
                                            InputMode::Command { buf, cursor };
                                    }
                                    KeyCode::Right => {
                                        let m = buf.chars().count();
                                        if cursor < m {
                                            cursor += 1;
                                        }
                                        mode =
                                            InputMode::Command { buf, cursor };
                                    }
                                    KeyCode::Char(c)
                                        if key.modifiers
                                            == KeyModifiers::NONE
                                            || key.modifiers
                                                == KeyModifiers::SHIFT =>
                                    {
                                        let bp = char_byte_pos(&buf, cursor);
                                        buf.insert(bp, c);
                                        cursor += 1;
                                        mode =
                                            InputMode::Command { buf, cursor };
                                    }
                                    _ => {
                                        mode =
                                            InputMode::Command { buf, cursor };
                                    }
                                },

                                InputMode::CopyMode => {
                                    if (key.code, key.modifiers) == prefix_key {
                                        mode = InputMode::Prefix;
                                        prefix_from_copy_mode = true;
                                        continue;
                                    }
                                    match (key.code, key.modifiers) {
                                        (KeyCode::Esc, _)
                                        | (
                                            KeyCode::Char('q'),
                                            KeyModifiers::NONE,
                                        ) => {
                                            leave_copy_mode_client(
                                                tabs.focused_client(),
                                                &mut mode,
                                                &mut copy_mode_confirmed,
                                                &mut copy_mode_exit_pending,
                                                &mut display_scrolled,
                                            );
                                            last_drawn_counter = 0;
                                        }
                                        (KeyCode::Char('/'), mods)
                                            if is_copy_plain_key(mods) =>
                                        {
                                            mode = InputMode::CopySearch {
                                                buf: String::new(),
                                                cursor: 0,
                                                forward: true,
                                            };
                                        }
                                        (KeyCode::Char('?'), mods)
                                            if is_copy_plain_key(mods) =>
                                        {
                                            mode = InputMode::CopySearch {
                                                buf: String::new(),
                                                cursor: 0,
                                                forward: false,
                                            };
                                        }
                                        (
                                            KeyCode::Char('h'),
                                            KeyModifiers::NONE,
                                        )
                                        | (KeyCode::Left, KeyModifiers::NONE) =>
                                        {
                                            tabs.focused_client()
                                                .copy_move_left();
                                            mode = InputMode::CopyMode;
                                        }
                                        (
                                            KeyCode::Char('l'),
                                            KeyModifiers::NONE,
                                        )
                                        | (
                                            KeyCode::Right,
                                            KeyModifiers::NONE,
                                        ) => {
                                            tabs.focused_client()
                                                .copy_move_right();
                                            mode = InputMode::CopyMode;
                                        }
                                        (
                                            KeyCode::Char('k'),
                                            KeyModifiers::NONE,
                                        )
                                        | (KeyCode::Up, KeyModifiers::NONE) => {
                                            tabs.focused_client()
                                                .copy_move_up();
                                            mode = InputMode::CopyMode;
                                        }
                                        (
                                            KeyCode::Char('j'),
                                            KeyModifiers::NONE,
                                        )
                                        | (KeyCode::Down, KeyModifiers::NONE) =>
                                        {
                                            tabs.focused_client()
                                                .copy_move_down();
                                            mode = InputMode::CopyMode;
                                        }
                                        (
                                            KeyCode::Char('b'),
                                            KeyModifiers::NONE,
                                        ) => {
                                            tabs.focused_client()
                                                .copy_move_word_backward();
                                            mode = InputMode::CopyMode;
                                        }
                                        (
                                            KeyCode::Char('w'),
                                            KeyModifiers::NONE,
                                        ) => {
                                            tabs.focused_client()
                                                .copy_move_word_forward();
                                            mode = InputMode::CopyMode;
                                        }
                                        (
                                            KeyCode::Char('e'),
                                            KeyModifiers::NONE,
                                        ) => {
                                            tabs.focused_client()
                                                .copy_move_word_end();
                                            mode = InputMode::CopyMode;
                                        }
                                        (
                                            KeyCode::PageUp,
                                            KeyModifiers::NONE,
                                        )
                                        | (
                                            KeyCode::Char('b'),
                                            KeyModifiers::CONTROL,
                                        ) => {
                                            tabs.focused_client()
                                                .copy_page_up();
                                            mode = InputMode::CopyMode;
                                        }
                                        (
                                            KeyCode::PageDown,
                                            KeyModifiers::NONE,
                                        )
                                        | (
                                            KeyCode::Char('f'),
                                            KeyModifiers::CONTROL,
                                        ) => {
                                            tabs.focused_client()
                                                .copy_page_down();
                                            mode = InputMode::CopyMode;
                                        }
                                        (
                                            KeyCode::Char('g'),
                                            KeyModifiers::NONE,
                                        ) => {
                                            tabs.focused_client()
                                                .copy_move_to_top();
                                            mode = InputMode::CopyMode;
                                        }
                                        (KeyCode::Char('G'), mods)
                                            if is_copy_plain_key(mods) =>
                                        {
                                            tabs.focused_client()
                                                .copy_move_to_bottom();
                                            mode = InputMode::CopyMode;
                                        }
                                        _ if is_copy_line_start_key(key) => {
                                            tabs.focused_client()
                                                .copy_move_to_line_start();
                                            mode = InputMode::CopyMode;
                                        }
                                        _ if is_copy_line_end_key(key) => {
                                            tabs.focused_client()
                                                .copy_move_to_line_end();
                                            mode = InputMode::CopyMode;
                                        }
                                        (
                                            KeyCode::Char('v'),
                                            KeyModifiers::NONE,
                                        )
                                        | (
                                            KeyCode::Char(' '),
                                            KeyModifiers::NONE,
                                        ) => {
                                            tabs.focused_client()
                                                .copy_start_selection(
                                                    SelectionMode::Char,
                                                );
                                            mode = InputMode::CopyMode;
                                        }
                                        (KeyCode::Char('V'), mods)
                                            if is_copy_plain_key(mods) =>
                                        {
                                            tabs.focused_client()
                                                .copy_start_selection(
                                                    SelectionMode::Line,
                                                );
                                            mode = InputMode::CopyMode;
                                        }
                                        (
                                            KeyCode::Char('v'),
                                            KeyModifiers::CONTROL,
                                        ) => {
                                            tabs.focused_client()
                                                .copy_start_selection(
                                                    SelectionMode::Rect,
                                                );
                                            mode = InputMode::CopyMode;
                                        }
                                        (
                                            KeyCode::Char('n'),
                                            KeyModifiers::NONE,
                                        ) => {
                                            tabs.focused_client()
                                                .copy_search_next();
                                            mode = InputMode::CopyMode;
                                        }
                                        (KeyCode::Char('N'), mods)
                                            if is_copy_plain_key(mods) =>
                                        {
                                            tabs.focused_client()
                                                .copy_search_prev();
                                            mode = InputMode::CopyMode;
                                        }
                                        (
                                            KeyCode::Char('y'),
                                            KeyModifiers::NONE,
                                        )
                                        | (KeyCode::Enter, _) => {
                                            let text = tabs
                                                .active_client()
                                                .copy_yank_selection();
                                            if text.is_empty() {
                                                status_notice = Some((
                                                    "no selection".to_string(),
                                                    Instant::now()
                                                        + Duration::from_secs(
                                                            3,
                                                        ),
                                                ));
                                                mode = InputMode::CopyMode;
                                            } else {
                                                leave_copy_mode_client(
                                                    tabs.focused_client(),
                                                    &mut mode,
                                                    &mut copy_mode_confirmed,
                                                    &mut copy_mode_exit_pending,
                                                    &mut display_scrolled,
                                                );
                                                let copy_result =
                                                    copy_to_clipboard(&text);
                                                status_notice = Some((
                                                    match copy_result {
                                                        ClipboardCopyResult::System => format!(
                                                            "copied {} chars",
                                                            text.chars()
                                                                .count()
                                                        ),
                                                        ClipboardCopyResult::Osc52 => format!(
                                                            "sent {} chars via OSC 52",
                                                            text.chars()
                                                                .count()
                                                        ),
                                                        ClipboardCopyResult::Unavailable => format!(
                                                            "yanked {} chars",
                                                            text.chars()
                                                                .count()
                                                        ),
                                                    },
                                                    Instant::now()
                                                        + Duration::from_secs(
                                                            3,
                                                        ),
                                                ));
                                            }
                                        }
                                        _ => {
                                            mode = InputMode::CopyMode;
                                        }
                                    }
                                }

                                InputMode::CopySearch {
                                    mut buf,
                                    mut cursor,
                                    forward,
                                } => {
                                    if (key.code, key.modifiers) == prefix_key {
                                        mode = InputMode::Prefix;
                                        prefix_from_copy_mode = true;
                                        continue;
                                    }
                                    match key.code {
                                        KeyCode::Enter => {
                                            if !buf.is_empty() {
                                                let found = tabs
                                                    .active_client()
                                                    .copy_search(
                                                        buf.clone(),
                                                        forward,
                                                    );
                                                if !found {
                                                    status_notice = Some((
                                                    format!("not found: {}", buf),
                                                    Instant::now()
                                                        + Duration::from_secs(3),
                                                ));
                                                }
                                            }
                                            mode = InputMode::CopyMode;
                                        }
                                        KeyCode::Esc => {
                                            mode = InputMode::CopyMode;
                                        }
                                        KeyCode::Char('q')
                                            if key.modifiers
                                                == KeyModifiers::NONE =>
                                        {
                                            leave_copy_mode_client(
                                                tabs.focused_client(),
                                                &mut mode,
                                                &mut copy_mode_confirmed,
                                                &mut copy_mode_exit_pending,
                                                &mut display_scrolled,
                                            );
                                            last_drawn_counter = 0;
                                        }
                                        KeyCode::Backspace => {
                                            if cursor > 0 {
                                                let bp = char_byte_pos(
                                                    &buf,
                                                    cursor - 1,
                                                );
                                                let ep =
                                                    char_byte_pos(&buf, cursor);
                                                buf.drain(bp..ep);
                                                cursor -= 1;
                                            }
                                            mode = InputMode::CopySearch {
                                                buf,
                                                cursor,
                                                forward,
                                            };
                                        }
                                        KeyCode::Left => {
                                            if cursor > 0 {
                                                cursor -= 1;
                                            }
                                            mode = InputMode::CopySearch {
                                                buf,
                                                cursor,
                                                forward,
                                            };
                                        }
                                        KeyCode::Right => {
                                            let m = buf.chars().count();
                                            if cursor < m {
                                                cursor += 1;
                                            }
                                            mode = InputMode::CopySearch {
                                                buf,
                                                cursor,
                                                forward,
                                            };
                                        }
                                        KeyCode::Char(c)
                                            if key.modifiers
                                                == KeyModifiers::NONE
                                                || key.modifiers
                                                    == KeyModifiers::SHIFT =>
                                        {
                                            let bp =
                                                char_byte_pos(&buf, cursor);
                                            buf.insert(bp, c);
                                            cursor += 1;
                                            mode = InputMode::CopySearch {
                                                buf,
                                                cursor,
                                                forward,
                                            };
                                        }
                                        _ => {
                                            mode = InputMode::CopySearch {
                                                buf,
                                                cursor,
                                                forward,
                                            };
                                        }
                                    }
                                }

                                InputMode::OptionPanel {
                                    selected,
                                    scroll_on_erase_in_display,
                                } => match key.code {
                                    KeyCode::Esc | KeyCode::Char('q') => {
                                        mode = InputMode::Normal;
                                        last_drawn_counter = 0;
                                    }
                                    KeyCode::Enter | KeyCode::Char(' ') => {
                                        let enabled =
                                            !scroll_on_erase_in_display;
                                        tabs.active_client()
                                            .set_scroll_on_erase_in_display(
                                                enabled,
                                            );
                                        mode = InputMode::OptionPanel {
                                            selected,
                                            scroll_on_erase_in_display: enabled,
                                        };
                                    }
                                    _ => {
                                        mode = InputMode::OptionPanel {
                                            selected,
                                            scroll_on_erase_in_display,
                                        };
                                    }
                                },

                                InputMode::SessionChooser {
                                    entries,
                                    mut selected,
                                    mut collapsed,
                                    mut collapsed_windows,
                                } => {
                                    let visible = visible_entries_full(
                                        &entries,
                                        &collapsed,
                                        &collapsed_windows,
                                    );
                                    match key.code {
                                        KeyCode::Esc | KeyCode::Char('q') => {
                                            mode = InputMode::Normal;
                                            last_drawn_counter = 0;
                                        }
                                        KeyCode::Up | KeyCode::Char('k') => {
                                            if selected > 0 {
                                                selected -= 1;
                                            }
                                            mode = InputMode::SessionChooser {
                                                entries,
                                                selected,
                                                collapsed,
                                                collapsed_windows,
                                            };
                                        }
                                        KeyCode::Down | KeyCode::Char('j') => {
                                            if selected + 1 < visible.len() {
                                                selected += 1;
                                            }
                                            mode = InputMode::SessionChooser {
                                                entries,
                                                selected,
                                                collapsed,
                                                collapsed_windows,
                                            };
                                        }
                                        KeyCode::Char('l') => {
                                            let new_sel = if let Some(entry) =
                                                visible.get(selected)
                                            {
                                                match entry {
                                                    SessionTreeEntry::Session { name, .. } => {
                                                        if collapsed.contains(name) {
                                                            collapsed.remove(name);
                                                            selected
                                                        } else {
                                                            // 已展开 → 跳到第一个 window
                                                            let n = name.clone();
                                                            let v2 = visible_entries_full(&entries, &collapsed, &collapsed_windows);
                                                            v2.iter().position(|e| matches!(e,
                                                                SessionTreeEntry::Window { session_name, index, .. }
                                                                if *session_name == n && *index == 0
                                                            )).unwrap_or(selected)
                                                        }
                                                    }
                                                    SessionTreeEntry::Window { session_name, index, .. } => {
                                                        let key_w = (session_name.clone(), *index);
                                                        if collapsed_windows.contains(&key_w) {
                                                            collapsed_windows.remove(&key_w);
                                                            selected
                                                        } else {
                                                            // 已展开 → 跳到第一个 pane
                                                            let sn = session_name.clone();
                                                            let wi = *index;
                                                            let v2 = visible_entries_full(&entries, &collapsed, &collapsed_windows);
                                                            v2.iter().position(|e| matches!(e,
                                                                SessionTreeEntry::Pane { session_name, window_index, index, .. }
                                                                if *session_name == sn && *window_index == wi && *index == 0
                                                            )).unwrap_or(selected)
                                                        }
                                                    }
                                                    SessionTreeEntry::Pane { .. } => selected,
                                                }
                                            } else {
                                                selected
                                            };
                                            let visible_len =
                                                visible_entries_full(
                                                    &entries,
                                                    &collapsed,
                                                    &collapsed_windows,
                                                )
                                                .len();
                                            mode = InputMode::SessionChooser {
                                                entries,
                                                selected: clamp_session_chooser_selected(
                                                    new_sel,
                                                    visible_len,
                                                ),
                                                collapsed,
                                                collapsed_windows,
                                            };
                                        }
                                        KeyCode::Char('h') => {
                                            if let Some(entry) =
                                                visible.get(selected)
                                            {
                                                match entry {
                                                    SessionTreeEntry::Session { name, .. } => {
                                                        collapsed.insert(name.clone());
                                                    }
                                                    SessionTreeEntry::Window { session_name, index, .. } => {
                                                        let key_w = (session_name.clone(), *index);
                                                        if collapsed_windows.contains(&key_w) {
                                                            // 已折叠 → 跳回父 session 并折叠 session
                                                            collapsed.insert(session_name.clone());
                                                            let sn = session_name.clone();
                                                            let v2 = visible_entries_full(&entries, &collapsed, &collapsed_windows);
                                                            selected = v2.iter().position(|e| matches!(e,
                                                                SessionTreeEntry::Session { name, .. } if *name == sn
                                                            )).unwrap_or(0);
                                                        } else {
                                                            // 展开 → 折叠 window
                                                            collapsed_windows.insert(key_w);
                                                        }
                                                    }
                                                    SessionTreeEntry::Pane { session_name, window_index, .. } => {
                                                        // 跳回父 window 行
                                                        let sn = session_name.clone();
                                                        let wi = *window_index;
                                                        let v2 = visible_entries_full(&entries, &collapsed, &collapsed_windows);
                                                        selected = v2.iter().position(|e| matches!(e,
                                                            SessionTreeEntry::Window { session_name, index, .. }
                                                            if *session_name == sn && *index == wi
                                                        )).unwrap_or(selected);
                                                    }
                                                }
                                            }
                                            let visible_len =
                                                visible_entries_full(
                                                    &entries,
                                                    &collapsed,
                                                    &collapsed_windows,
                                                )
                                                .len();
                                            mode = InputMode::SessionChooser {
                                                entries,
                                                selected: clamp_session_chooser_selected(
                                                    selected,
                                                    visible_len,
                                                ),
                                                collapsed,
                                                collapsed_windows,
                                            };
                                        }
                                        KeyCode::Enter
                                        | KeyCode::Char('\r')
                                        | KeyCode::Char('\n') => {
                                            if let Some(entry) =
                                                visible.get(selected)
                                            {
                                                let cmd = match entry {
                                                    SessionTreeEntry::Session { name, .. } =>
                                                        format!("switch-client -t {}", name),
                                                    SessionTreeEntry::Window { session_name, index, .. } =>
                                                        format!("switch-client -t {}; select-window -t {}", session_name, index),
                                                    SessionTreeEntry::Pane { session_name, window_index, pane_id, .. } =>
                                                        format!("switch-client -t {}; select-window -t {}; select-pane -t %{}", session_name, window_index, pane_id),
                                                };
                                                tabs.active_client()
                                                    .run_command(&cmd);
                                            }
                                            mode = InputMode::Normal;
                                            last_drawn_counter = 0;
                                        }
                                        _ => {
                                            mode = InputMode::SessionChooser {
                                                entries,
                                                selected,
                                                collapsed,
                                                collapsed_windows,
                                            };
                                        }
                                    }
                                }

                                InputMode::TabQuickSwitch {
                                    mut code,
                                    error,
                                } => match (key.code, key.modifiers) {
                                    (KeyCode::Esc, _) => {
                                        mode = InputMode::Normal;
                                        last_drawn_counter = 0;
                                    }
                                    (KeyCode::Backspace, _) => {
                                        code.pop();
                                        mode = InputMode::TabQuickSwitch {
                                            code,
                                            error: None,
                                        };
                                    }
                                    (KeyCode::Enter, _) => {
                                        let (cols, rows) = terminal::size()
                                            .unwrap_or((80, 24));
                                        match select_tab_by_code(
                                            &mut tabs,
                                            &code,
                                            server_content_size(cols, rows),
                                        ) {
                                            Ok(()) => {
                                                mode = InputMode::Normal;
                                                copy_mode_confirmed = false;
                                                mouse_select = None;
                                                last_drawn_counter = 0;
                                            }
                                            Err(e) => {
                                                mode =
                                                    InputMode::TabQuickSwitch {
                                                        code: String::new(),
                                                        error: Some(e),
                                                    };
                                            }
                                        }
                                    }
                                    (KeyCode::Char(c), modifiers)
                                        if !modifiers.intersects(
                                            KeyModifiers::CONTROL
                                                | KeyModifiers::ALT,
                                        ) =>
                                    {
                                        if code.len() < 2
                                            && c.is_ascii_alphabetic()
                                        {
                                            code.push(c.to_ascii_uppercase());
                                        }
                                        mode = InputMode::TabQuickSwitch {
                                            code,
                                            error: None,
                                        };
                                    }
                                    _ => {
                                        mode = InputMode::TabQuickSwitch {
                                            code,
                                            error,
                                        };
                                    }
                                },

                                InputMode::TabChooser {
                                    mut query,
                                    mut cursor,
                                    mut selected,
                                    mut search_active,
                                } => match (key.code, key.modifiers) {
                                    (KeyCode::Esc, _) if search_active => {
                                        mode = default_tab_chooser_mode(&tabs);
                                    }
                                    (KeyCode::Esc, _)
                                    | (
                                        KeyCode::Char('q'),
                                        KeyModifiers::NONE,
                                    ) => {
                                        mode = InputMode::Normal;
                                        last_drawn_counter = 0;
                                    }
                                    (KeyCode::Char('/'), _)
                                    | (KeyCode::Char('?'), _) => {
                                        query.clear();
                                        cursor = 0;
                                        selected = 0;
                                        search_active = true;
                                        mode = InputMode::TabChooser {
                                            query,
                                            cursor,
                                            selected,
                                            search_active,
                                        };
                                    }
                                    (KeyCode::Enter, _) => {
                                        let tab_views =
                                            tabs.tab_chooser_views();
                                        let matches = matching_tab_indices(
                                            &tab_views, &query,
                                        );
                                        if let Some(&tab_index) =
                                            matches.get(selected)
                                        {
                                            let tab =
                                                tab_views[tab_index].clone();
                                            let (cols, rows) = terminal::size()
                                                .unwrap_or((80, 24));
                                            let size =
                                                server_content_size(cols, rows);
                                            let switch_result: Result<
                                                (),
                                                String,
                                            > = if tab.visible {
                                                if tabs.select_socket(
                                                    &tab.socket_name,
                                                ) {
                                                    Ok(())
                                                } else {
                                                    Err("failed to switch tab"
                                                        .to_string())
                                                }
                                            } else {
                                                tabs.show_socket_with_code(
                                                        &tab.socket_name,
                                                        size,
                                                        Some(&tab.code),
                                                    )
                                                    .map_err(|e| {
                                                        format!(
                                                            "show tab failed: {}",
                                                            e
                                                        )
                                                    })
                                                    .and_then(|()| {
                                                        if tabs.select_socket(
                                                            &tab.socket_name,
                                                        ) {
                                                            Ok(())
                                                        } else {
                                                            Err(
                                                                "tab not found after show"
                                                                    .to_string(),
                                                            )
                                                        }
                                                    })
                                            };
                                            match switch_result {
                                                Ok(()) => {
                                                    tabs.active_client()
                                                        .resize(size);
                                                    mode = InputMode::Normal;
                                                    copy_mode_confirmed = false;
                                                    mouse_select = None;
                                                    last_drawn_counter = 0;
                                                }
                                                Err(message) => {
                                                    status_notice = Some((
                                                        message,
                                                        Instant::now()
                                                            + Duration::from_secs(
                                                                3,
                                                            ),
                                                    ));
                                                    mode =
                                                        InputMode::TabChooser {
                                                            query,
                                                            cursor,
                                                            selected,
                                                            search_active,
                                                        };
                                                }
                                            }
                                        } else {
                                            mode = InputMode::TabChooser {
                                                query,
                                                cursor,
                                                selected,
                                                search_active,
                                            };
                                        }
                                    }
                                    (KeyCode::Char('R'), _) => {
                                        let tab_views =
                                            tabs.tab_chooser_views();
                                        let matches = matching_tab_indices(
                                            &tab_views, &query,
                                        );
                                        if let Some(&tab_index) =
                                            matches.get(selected)
                                        {
                                            let tab = &tab_views[tab_index];
                                            if tab.visible
                                                && tabs.select_socket(
                                                    &tab.socket_name,
                                                )
                                            {
                                                mode =
                                                    rename_tab_mode_for_active(
                                                        &tabs, true,
                                                    );
                                            } else {
                                                status_notice = Some((
                                                    "show the tab before renaming".to_string(),
                                                    Instant::now() + Duration::from_secs(3),
                                                ));
                                                mode = InputMode::TabChooser {
                                                    query,
                                                    cursor,
                                                    selected,
                                                    search_active,
                                                };
                                            }
                                        } else {
                                            mode = InputMode::TabChooser {
                                                query,
                                                cursor,
                                                selected,
                                                search_active,
                                            };
                                        }
                                    }
                                    (KeyCode::Char('K'), _) => {
                                        let tab_views =
                                            tabs.tab_chooser_views();
                                        let matches = matching_tab_indices(
                                            &tab_views, &query,
                                        );
                                        if let Some(&tab_index) =
                                            matches.get(selected)
                                        {
                                            let tab = &tab_views[tab_index];
                                            mode = InputMode::ConfirmKillTab {
                                                socket_name: tab
                                                    .socket_name
                                                    .clone(),
                                                label: tab_label(tab),
                                                return_to_tab_chooser: true,
                                            };
                                        } else {
                                            mode = InputMode::TabChooser {
                                                query,
                                                cursor,
                                                selected,
                                                search_active,
                                            };
                                        }
                                    }
                                    (KeyCode::Char(' '), _) => {
                                        let tab_views =
                                            tabs.tab_chooser_views();
                                        let matches = matching_tab_indices(
                                            &tab_views, &query,
                                        );
                                        if let Some(&tab_index) =
                                            matches.get(selected)
                                        {
                                            let tab =
                                                tab_views[tab_index].clone();
                                            let (cols, rows) = terminal::size()
                                                .unwrap_or((80, 24));
                                            if tab.visible {
                                                match tabs.hide_socket(
                                                    &tab.socket_name,
                                                ) {
                                                    Ok(()) => {
                                                        status_notice = Some((
                                                            format!("hidden {}", tab_label(&tab)),
                                                            Instant::now() + Duration::from_secs(3),
                                                        ));
                                                    }
                                                    Err(e) => {
                                                        status_notice = Some((
                                                            e,
                                                            Instant::now() + Duration::from_secs(3),
                                                        ));
                                                    }
                                                }
                                            } else {
                                                match tabs.show_socket(
                                                    &tab.socket_name,
                                                    server_content_size(
                                                        cols, rows,
                                                    ),
                                                ) {
                                                    Ok(()) => {
                                                        status_notice = Some((
                                                            format!("shown {}", tab_label(&tab)),
                                                            Instant::now() + Duration::from_secs(3),
                                                        ));
                                                    }
                                                    Err(e) => {
                                                        status_notice = Some((
                                                            format!("show tab failed: {}", e),
                                                            Instant::now() + Duration::from_secs(3),
                                                        ));
                                                    }
                                                }
                                            }
                                            last_drawn_counter = 0;
                                        }
                                        let len = matching_tab_indices(
                                            &tabs.tab_chooser_views(),
                                            &query,
                                        )
                                        .len();
                                        if selected >= len {
                                            selected = len.saturating_sub(1);
                                        }
                                        mode = InputMode::TabChooser {
                                            query,
                                            cursor,
                                            selected,
                                            search_active,
                                        };
                                    }
                                    (KeyCode::Up, _) => {
                                        selected = selected.saturating_sub(1);
                                        mode = InputMode::TabChooser {
                                            query,
                                            cursor,
                                            selected,
                                            search_active,
                                        };
                                    }
                                    (
                                        KeyCode::Char('k'),
                                        KeyModifiers::NONE,
                                    ) if !search_active => {
                                        selected = selected.saturating_sub(1);
                                        mode = InputMode::TabChooser {
                                            query,
                                            cursor,
                                            selected,
                                            search_active,
                                        };
                                    }
                                    (KeyCode::Char('k'), m)
                                        if search_active
                                            && m.contains(
                                                KeyModifiers::CONTROL,
                                            ) =>
                                    {
                                        selected = selected.saturating_sub(1);
                                        mode = InputMode::TabChooser {
                                            query,
                                            cursor,
                                            selected,
                                            search_active,
                                        };
                                    }
                                    (KeyCode::Down, _) => {
                                        let tab_views =
                                            tabs.tab_chooser_views();
                                        let len = matching_tab_indices(
                                            &tab_views, &query,
                                        )
                                        .len();
                                        if selected + 1 < len {
                                            selected += 1;
                                        }
                                        mode = InputMode::TabChooser {
                                            query,
                                            cursor,
                                            selected,
                                            search_active,
                                        };
                                    }
                                    (
                                        KeyCode::Char('j'),
                                        KeyModifiers::NONE,
                                    ) if !search_active => {
                                        let tab_views =
                                            tabs.tab_chooser_views();
                                        let len = matching_tab_indices(
                                            &tab_views, &query,
                                        )
                                        .len();
                                        if selected + 1 < len {
                                            selected += 1;
                                        }
                                        mode = InputMode::TabChooser {
                                            query,
                                            cursor,
                                            selected,
                                            search_active,
                                        };
                                    }
                                    (KeyCode::Char('j'), m)
                                        if search_active
                                            && m.contains(
                                                KeyModifiers::CONTROL,
                                            ) =>
                                    {
                                        let tab_views =
                                            tabs.tab_chooser_views();
                                        let len = matching_tab_indices(
                                            &tab_views, &query,
                                        )
                                        .len();
                                        if selected + 1 < len {
                                            selected += 1;
                                        }
                                        mode = InputMode::TabChooser {
                                            query,
                                            cursor,
                                            selected,
                                            search_active,
                                        };
                                    }
                                    (KeyCode::Backspace, _)
                                        if search_active =>
                                    {
                                        if cursor > 0 {
                                            let bp = char_byte_pos(
                                                &query,
                                                cursor - 1,
                                            );
                                            let ep =
                                                char_byte_pos(&query, cursor);
                                            query.drain(bp..ep);
                                            cursor -= 1;
                                            selected = 0;
                                        }
                                        mode = InputMode::TabChooser {
                                            query,
                                            cursor,
                                            selected,
                                            search_active,
                                        };
                                    }
                                    (KeyCode::Left, _) if search_active => {
                                        if cursor > 0 {
                                            cursor -= 1;
                                        }
                                        mode = InputMode::TabChooser {
                                            query,
                                            cursor,
                                            selected,
                                            search_active,
                                        };
                                    }
                                    (KeyCode::Right, _) if search_active => {
                                        let m = query.chars().count();
                                        if cursor < m {
                                            cursor += 1;
                                        }
                                        mode = InputMode::TabChooser {
                                            query,
                                            cursor,
                                            selected,
                                            search_active,
                                        };
                                    }
                                    (
                                        KeyCode::Char(c),
                                        KeyModifiers::NONE
                                        | KeyModifiers::SHIFT,
                                    ) if search_active => {
                                        let bp = char_byte_pos(&query, cursor);
                                        query.insert(bp, c);
                                        cursor += 1;
                                        mode = InputMode::TabChooser {
                                            query,
                                            cursor,
                                            selected: 0,
                                            search_active,
                                        };
                                    }
                                    _ => {
                                        mode = InputMode::TabChooser {
                                            query,
                                            cursor,
                                            selected,
                                            search_active,
                                        };
                                    }
                                },

                                InputMode::ConfirmKillTab {
                                    socket_name,
                                    label,
                                    return_to_tab_chooser,
                                } => {
                                    match key.code {
                                        KeyCode::Char('y')
                                        | KeyCode::Char('Y') => {
                                            match tabs.kill_socket(&socket_name)
                                            {
                                                Ok(empty) => {
                                                    if empty {
                                                        break;
                                                    }
                                                    mode =
                                                        if return_to_tab_chooser
                                                        {
                                                            default_tab_chooser_mode(&tabs)
                                                        } else {
                                                            InputMode::Normal
                                                        };
                                                    copy_mode_confirmed = false;
                                                    mouse_select = None;
                                                    last_drawn_counter = 0;
                                                    status_notice = Some((
                                                    format!("killed {}", label),
                                                    Instant::now() + Duration::from_secs(3),
                                                ));
                                                }
                                                Err(e) => {
                                                    mode =
                                                        if return_to_tab_chooser
                                                        {
                                                            default_tab_chooser_mode(&tabs)
                                                        } else {
                                                            InputMode::Normal
                                                        };
                                                    status_notice = Some((
                                                    format!("kill tab failed: {}", e),
                                                    Instant::now() + Duration::from_secs(3),
                                                ));
                                                }
                                            }
                                        }
                                        KeyCode::Esc
                                        | KeyCode::Char('n')
                                        | KeyCode::Char('N')
                                        | KeyCode::Enter => {
                                            mode = if return_to_tab_chooser {
                                                default_tab_chooser_mode(&tabs)
                                            } else {
                                                InputMode::Normal
                                            };
                                            last_drawn_counter = 0;
                                        }
                                        _ => {
                                            mode = InputMode::ConfirmKillTab {
                                                socket_name,
                                                label,
                                                return_to_tab_chooser,
                                            };
                                        }
                                    }
                                }

                                InputMode::RenameWindow {
                                    mut buf,
                                    mut cursor,
                                } => match key.code {
                                    KeyCode::Enter => {
                                        if !buf.is_empty() {
                                            tabs.active_client().run_command(
                                                &format!(
                                                    "rename-window {}",
                                                    shell_quote(&buf)
                                                ),
                                            );
                                        }
                                        mode = InputMode::Normal;
                                    }
                                    KeyCode::Esc => {
                                        mode = InputMode::Normal;
                                    }
                                    KeyCode::Backspace => {
                                        if cursor > 0 {
                                            let bp =
                                                char_byte_pos(&buf, cursor - 1);
                                            let ep =
                                                char_byte_pos(&buf, cursor);
                                            buf.drain(bp..ep);
                                            cursor -= 1;
                                        }
                                        mode = InputMode::RenameWindow {
                                            buf,
                                            cursor,
                                        };
                                    }
                                    KeyCode::Left => {
                                        if cursor > 0 {
                                            cursor -= 1;
                                        }
                                        mode = InputMode::RenameWindow {
                                            buf,
                                            cursor,
                                        };
                                    }
                                    KeyCode::Right => {
                                        let m = buf.chars().count();
                                        if cursor < m {
                                            cursor += 1;
                                        }
                                        mode = InputMode::RenameWindow {
                                            buf,
                                            cursor,
                                        };
                                    }
                                    KeyCode::Char(c)
                                        if key.modifiers
                                            == KeyModifiers::NONE
                                            || key.modifiers
                                                == KeyModifiers::SHIFT =>
                                    {
                                        let bp = char_byte_pos(&buf, cursor);
                                        buf.insert(bp, c);
                                        cursor += 1;
                                        mode = InputMode::RenameWindow {
                                            buf,
                                            cursor,
                                        };
                                    }
                                    _ => {
                                        mode = InputMode::RenameWindow {
                                            buf,
                                            cursor,
                                        };
                                    }
                                },

                                InputMode::RenameSession {
                                    mut buf,
                                    mut cursor,
                                } => match key.code {
                                    KeyCode::Enter => {
                                        if !buf.is_empty() {
                                            tabs.active_client().run_command(
                                                &format!(
                                                    "rename-session {}",
                                                    shell_quote(&buf)
                                                ),
                                            );
                                        }
                                        mode = InputMode::Normal;
                                    }
                                    KeyCode::Esc => {
                                        mode = InputMode::Normal;
                                    }
                                    KeyCode::Backspace => {
                                        if cursor > 0 {
                                            let bp =
                                                char_byte_pos(&buf, cursor - 1);
                                            let ep =
                                                char_byte_pos(&buf, cursor);
                                            buf.drain(bp..ep);
                                            cursor -= 1;
                                        }
                                        mode = InputMode::RenameSession {
                                            buf,
                                            cursor,
                                        };
                                    }
                                    KeyCode::Left => {
                                        if cursor > 0 {
                                            cursor -= 1;
                                        }
                                        mode = InputMode::RenameSession {
                                            buf,
                                            cursor,
                                        };
                                    }
                                    KeyCode::Right => {
                                        let m = buf.chars().count();
                                        if cursor < m {
                                            cursor += 1;
                                        }
                                        mode = InputMode::RenameSession {
                                            buf,
                                            cursor,
                                        };
                                    }
                                    KeyCode::Char(c)
                                        if key.modifiers
                                            == KeyModifiers::NONE
                                            || key.modifiers
                                                == KeyModifiers::SHIFT =>
                                    {
                                        let bp = char_byte_pos(&buf, cursor);
                                        buf.insert(bp, c);
                                        cursor += 1;
                                        mode = InputMode::RenameSession {
                                            buf,
                                            cursor,
                                        };
                                    }
                                    _ => {
                                        mode = InputMode::RenameSession {
                                            buf,
                                            cursor,
                                        };
                                    }
                                },

                                InputMode::RenameTab {
                                    mut code,
                                    mut code_cursor,
                                    mut title,
                                    mut title_cursor,
                                    mut editing_code,
                                    return_to_tab_chooser,
                                    ..
                                } => {
                                    match key.code {
                                        KeyCode::Enter => {
                                            if editing_code {
                                                editing_code = false;
                                                mode = InputMode::RenameTab {
                                                    code,
                                                    code_cursor,
                                                    title,
                                                    title_cursor,
                                                    editing_code,
                                                    error: None,
                                                    return_to_tab_chooser,
                                                };
                                            } else {
                                                match tabs.set_active_metadata(
                                                    &code,
                                                    title.trim().to_string(),
                                                ) {
                                                    Ok(()) => {
                                                        mode = if return_to_tab_chooser {
                                                        default_tab_chooser_mode(
                                                            &tabs,
                                                        )
                                                    } else {
                                                        InputMode::Normal
                                                    };
                                                        last_drawn_counter = 0;
                                                    }
                                                    Err(error) => {
                                                        mode =
                                                        InputMode::RenameTab {
                                                            code,
                                                            code_cursor,
                                                            title,
                                                            title_cursor,
                                                            editing_code: true,
                                                            error: Some(error),
                                                            return_to_tab_chooser,
                                                        };
                                                    }
                                                }
                                            }
                                        }
                                        KeyCode::Esc => {
                                            mode = if return_to_tab_chooser {
                                                default_tab_chooser_mode(&tabs)
                                            } else {
                                                InputMode::Normal
                                            };
                                            last_drawn_counter = 0;
                                        }
                                        KeyCode::Tab | KeyCode::BackTab => {
                                            editing_code = !editing_code;
                                            mode = InputMode::RenameTab {
                                                code,
                                                code_cursor,
                                                title,
                                                title_cursor,
                                                editing_code,
                                                error: None,
                                                return_to_tab_chooser,
                                            };
                                        }
                                        KeyCode::Backspace => {
                                            if editing_code {
                                                if code_cursor > 0 {
                                                    let bp = char_byte_pos(
                                                        &code,
                                                        code_cursor - 1,
                                                    );
                                                    let ep = char_byte_pos(
                                                        &code,
                                                        code_cursor,
                                                    );
                                                    code.drain(bp..ep);
                                                    code_cursor -= 1;
                                                }
                                            } else if title_cursor > 0 {
                                                let bp = char_byte_pos(
                                                    &title,
                                                    title_cursor - 1,
                                                );
                                                let ep = char_byte_pos(
                                                    &title,
                                                    title_cursor,
                                                );
                                                title.drain(bp..ep);
                                                title_cursor -= 1;
                                            }
                                            mode = InputMode::RenameTab {
                                                code,
                                                code_cursor,
                                                title,
                                                title_cursor,
                                                editing_code,
                                                error: None,
                                                return_to_tab_chooser,
                                            };
                                        }
                                        KeyCode::Left => {
                                            if editing_code {
                                                code_cursor = code_cursor
                                                    .saturating_sub(1);
                                            } else {
                                                title_cursor = title_cursor
                                                    .saturating_sub(1);
                                            }
                                            mode = InputMode::RenameTab {
                                                code,
                                                code_cursor,
                                                title,
                                                title_cursor,
                                                editing_code,
                                                error: None,
                                                return_to_tab_chooser,
                                            };
                                        }
                                        KeyCode::Right => {
                                            if editing_code {
                                                let m = code.chars().count();
                                                if code_cursor < m {
                                                    code_cursor += 1;
                                                }
                                            } else {
                                                let m = title.chars().count();
                                                if title_cursor < m {
                                                    title_cursor += 1;
                                                }
                                            }
                                            mode = InputMode::RenameTab {
                                                code,
                                                code_cursor,
                                                title,
                                                title_cursor,
                                                editing_code,
                                                error: None,
                                                return_to_tab_chooser,
                                            };
                                        }
                                        KeyCode::Char(c)
                                            if key.modifiers
                                                == KeyModifiers::NONE
                                                || key.modifiers
                                                    == KeyModifiers::SHIFT =>
                                        {
                                            if editing_code {
                                                let c = c.to_ascii_uppercase();
                                                let bp = char_byte_pos(
                                                    &code,
                                                    code_cursor,
                                                );
                                                code.insert(bp, c);
                                                code_cursor += 1;
                                            } else {
                                                let bp = char_byte_pos(
                                                    &title,
                                                    title_cursor,
                                                );
                                                title.insert(bp, c);
                                                title_cursor += 1;
                                            }
                                            mode = InputMode::RenameTab {
                                                code,
                                                code_cursor,
                                                title,
                                                title_cursor,
                                                editing_code,
                                                error: None,
                                                return_to_tab_chooser,
                                            };
                                        }
                                        _ => {
                                            mode = InputMode::RenameTab {
                                                code,
                                                code_cursor,
                                                title,
                                                title_cursor,
                                                editing_code,
                                                error: None,
                                                return_to_tab_chooser,
                                            };
                                        }
                                    }
                                }
                            }
                        }
                        Event::Mouse(mouse) => {
                            last_mouse_pos = Some((mouse.column, mouse.row));
                            if let Err(err) = refresh_mouse_pointer(
                                terminal.backend_mut(),
                                &mut applied_mouse_pointer,
                                last_mouse_pos,
                                frame.as_ref(),
                                cols,
                                rows,
                                hide_borders,
                                hide_status,
                                has_overlay || has_prompt,
                                true,
                            ) {
                                log_client(&format!(
                                    "failed to set mouse pointer on move: {err}"
                                ));
                            }
                            if mode == InputMode::Prefix {
                                prefix_from_copy_mode = false;
                            }
                            let (cols, rows) =
                                terminal::size().unwrap_or((80, 24));
                            if mouse.row == 0
                                && !matches!(mode, InputMode::TabChooser { .. })
                            {
                                mouse_select = None;
                                mouse_drag_origin = None;
                                last_mouse_click = None;
                                if matches!(
                                    mouse.kind,
                                    MouseEventKind::Down(MouseButton::Left)
                                ) {
                                    let tab_views = tabs.tab_views();
                                    match tab_bar_hit(
                                        &tab_views,
                                        cols,
                                        mouse.column,
                                        tabs.tab_bar_offset,
                                    ) {
                                        Some(ClientTabBarHit::Tab(index)) => {
                                            if tabs.select(index) {
                                                tabs.active_client().resize(
                                                    server_content_size(
                                                        cols, rows,
                                                    ),
                                                );
                                                mode = InputMode::Normal;
                                                copy_mode_confirmed = false;
                                                last_drawn_counter = 0;
                                            }
                                        }
                                        Some(
                                            ClientTabBarHit::OverflowStart,
                                        ) => {
                                            if tabs.scroll_tab_bar_back() {
                                                last_drawn_counter = 0;
                                            }
                                        }
                                        Some(ClientTabBarHit::OverflowEnd) => {
                                            mode = InputMode::TabChooser {
                                                query: String::new(),
                                                cursor: 0,
                                                selected: tabs.active_index(),
                                                search_active: false,
                                            };
                                            last_drawn_counter = 0;
                                        }
                                        None => {}
                                    }
                                }
                                continue;
                            }
                            if matches!(
                                mouse.kind,
                                MouseEventKind::Down(MouseButton::Left)
                            ) && matches!(
                                mode,
                                InputMode::Normal
                                    | InputMode::Prefix
                                    | InputMode::Resize
                            ) {
                                if let Some(status_row) =
                                    status_bar_screen_row(rows, hide_status)
                                {
                                    if mouse.row == status_row {
                                        if let Some(ref fd) = frame {
                                            if let Some(status) = &fd.status {
                                                if let Some(win_index) =
                                                    status_window_tab_hit(
                                                        status,
                                                        cols,
                                                        mouse.column,
                                                    )
                                                {
                                                    tabs.focused_client()
                                                        .run_command(&format!(
                                                        "select-window -t {}",
                                                        win_index
                                                    ));
                                                    mode = InputMode::Normal;
                                                    copy_mode_confirmed = false;
                                                    last_drawn_counter = 0;
                                                    continue;
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            match mode {
                                InputMode::Normal
                                | InputMode::Prefix
                                | InputMode::Resize => {
                                    if mode != InputMode::Normal {
                                        mode = InputMode::Normal;
                                    }
                                    let mouse_mode = frame
                                        .as_ref()
                                        .map(active_mouse_mode)
                                        .unwrap_or(0);
                                    let shift_held = mouse
                                        .modifiers
                                        .contains(KeyModifiers::SHIFT);
                                    let shift_overrides = shift_held
                                        && matches!(
                                            mouse.kind,
                                            MouseEventKind::Down(
                                                MouseButton::Left
                                            ) | MouseEventKind::Drag(
                                                MouseButton::Left
                                            ) | MouseEventKind::Up(
                                                MouseButton::Left
                                            )
                                        );
                                    if mouse_mode != 0 && !shift_overrides {
                                        mouse_select = None;
                                        let focused_other_pane = matches!(
                                            mouse.kind,
                                            MouseEventKind::Down(
                                                MouseButton::Left
                                            )
                                        ) && tabs
                                            .focus_at(
                                                mouse.column,
                                                mouse.row,
                                                hide_borders,
                                            );
                                        if !focused_other_pane {
                                            tabs.send_mouse_at(
                                                mouse,
                                                hide_borders,
                                            );
                                        }
                                    } else {
                                        match mouse.kind {
                                            MouseEventKind::ScrollUp => {
                                                tabs.scroll_at(
                                                    mouse,
                                                    hide_borders,
                                                    "up",
                                                );
                                                mode = InputMode::CopyMode;
                                                copy_mode_confirmed = false;
                                                copy_mode_exit_pending = false;
                                                display_scrolled = false;
                                                last_drawn_counter = 0;
                                            }
                                            MouseEventKind::ScrollDown => {
                                                tabs.scroll_at(
                                                    mouse,
                                                    hide_borders,
                                                    "down",
                                                );
                                                last_drawn_counter = 0;
                                            }
                                            MouseEventKind::Down(
                                                MouseButton::Left,
                                            ) => {
                                                if let Some(ref fd) = frame {
                                                    let (cols, rows) =
                                                        terminal::size()
                                                            .unwrap_or((
                                                                80, 24,
                                                            ));
                                                    let fa = server_frame_area(
                                                        cols, rows,
                                                    );
                                                    let pa = active_pane_content_rect(fd, fa, hide_borders);
                                                    if mouse.column >= pa.x
                                                        && mouse.column
                                                            < pa.x + pa.width
                                                        && mouse.row >= pa.y
                                                        && mouse.row
                                                            < pa.y + pa.height
                                                    {
                                                        if let Some(sel) =
                                                            try_word_select_on_double_click(
                                                                &mut last_mouse_click,
                                                                fd,
                                                                mouse.row,
                                                                mouse.column,
                                                                hide_borders,
                                                            )
                                                        {
                                                            mouse_select =
                                                                Some(sel);
                                                            mouse_drag_origin =
                                                                None;
                                                        } else {
                                                            // 不在 Down 时清除已有选区，
                                                            // 等到真正开始拖拽（Drag）再建立新选区
                                                            mouse_drag_origin =
                                                                Some(MouseDragOrigin {
                                                                    row: mouse.row,
                                                                    col: mouse.column,
                                                                });
                                                        }
                                                    } else {
                                                        let _ = tabs.focus_at(
                                                            mouse.column,
                                                            mouse.row,
                                                            hide_borders,
                                                        );
                                                    }
                                                }
                                            }
                                            MouseEventKind::Drag(
                                                MouseButton::Left,
                                            ) => {
                                                if let Some(ref mut sel) =
                                                    mouse_select
                                                {
                                                    if let Some(ref fd) = frame
                                                    {
                                                        let (cols, rows) =
                                                            terminal::size()
                                                                .unwrap_or((
                                                                    80, 24,
                                                                ));
                                                        let fa =
                                                            server_frame_area(
                                                                cols, rows,
                                                            );
                                                        let pa = active_pane_content_rect(fd, fa, hide_borders);
                                                        sel.end_col = mouse
                                                            .column
                                                            .saturating_add(1)
                                                            .max(pa.x)
                                                            .min(
                                                                pa.x + pa.width,
                                                            );
                                                        sel.end_row = mouse.row
                                                            .max(pa.y)
                                                            .min(pa.y + pa.height.saturating_sub(1));
                                                    } else {
                                                        sel.end_col =
                                                            mouse.column;
                                                        sel.end_row = mouse.row;
                                                    }
                                                } else if let Some(origin) =
                                                    mouse_drag_origin
                                                {
                                                    if let Some(ref fd) = frame
                                                    {
                                                        mouse_select = Some(
                                                            mouse_selection_from_drag(
                                                                origin,
                                                                mouse,
                                                                fd,
                                                                hide_borders,
                                                            ),
                                                        );
                                                    }
                                                }
                                            }
                                            MouseEventKind::Up(
                                                MouseButton::Left,
                                            ) => {
                                                let refresh_ansi =
                                                    frame.as_ref().is_some_and(
                                                        |fd| fd.ansi.is_some(),
                                                    );
                                                let pane_area =
                                                    frame.as_ref().map(|fd| {
                                                        active_pane_content_rect(
                                                            fd,
                                                            server_frame_area(
                                                                cols, rows,
                                                            ),
                                                            hide_borders,
                                                        )
                                                    });
                                                let completed_selection =
                                                    finalize_mouse_selection(
                                                        mouse_select.take(),
                                                        mouse_drag_origin,
                                                        mouse,
                                                        pane_area,
                                                    );
                                                if let Some(sel) =
                                                    completed_selection
                                                {
                                                    if let Some(ref fd) = frame
                                                    {
                                                        if selection_is_caret(
                                                            &sel,
                                                        ) {
                                                            if mouse.modifiers.contains(
                                                                KeyModifiers::CONTROL,
                                                            ) {
                                                                if let Some(url) = detect_url_at_click(
                                                                    fd,
                                                                    sel.start_row,
                                                                    sel.start_col,
                                                                    hide_borders,
                                                                ) {
                                                                    open_url(&url);
                                                                    status_notice = Some((
                                                                        format!(
                                                                            "opening {}",
                                                                            truncate_status_url(&url)
                                                                        ),
                                                                        Instant::now()
                                                                            + Duration::from_secs(3),
                                                                    ));
                                                                }
                                                            }
                                                            last_mouse_click =
                                                                Some(LastMouseClick {
                                                                    row: sel.start_row,
                                                                    col: sel.start_col,
                                                                    at: Instant::now(),
                                                                });
                                                        } else {
                                                            last_mouse_click =
                                                                None;
                                                            copy_drag_selection(
                                                                fd,
                                                                &sel,
                                                                hide_borders,
                                                                &mut status_notice,
                                                                &mut last_drawn_counter,
                                                            );
                                                        }
                                                    }
                                                } else if mouse_drag_origin
                                                    .is_some()
                                                {
                                                    // Down 时未建立选区（非双击），Up 时也未拖拽 → 单击
                                                    if let Some(ref fd) = frame
                                                    {
                                                        if mouse.modifiers.contains(
                                                            KeyModifiers::CONTROL,
                                                        ) {
                                                            if let Some(url) = detect_url_at_click(
                                                                fd,
                                                                mouse.row,
                                                                mouse.column,
                                                                hide_borders,
                                                            ) {
                                                                open_url(&url);
                                                                status_notice = Some((
                                                                    format!(
                                                                        "opening {}",
                                                                        truncate_status_url(&url)
                                                                    ),
                                                                    Instant::now()
                                                                        + Duration::from_secs(3),
                                                                ));
                                                            }
                                                        }
                                                        last_mouse_click = Some(
                                                            LastMouseClick {
                                                                row: mouse.row,
                                                                col: mouse
                                                                    .column,
                                                                at:
                                                                    Instant::now(
                                                                    ),
                                                            },
                                                        );
                                                    }
                                                }
                                                mouse_drag_origin = None;
                                                if refresh_ansi {
                                                    last_drawn_counter = 0;
                                                }
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                                InputMode::CopyMode => match mouse.kind {
                                    MouseEventKind::ScrollUp => {
                                        tabs.scroll_at(
                                            mouse,
                                            hide_borders,
                                            "up",
                                        );
                                        last_drawn_counter = 0;
                                    }
                                    MouseEventKind::ScrollDown => {
                                        tabs.scroll_at(
                                            mouse,
                                            hide_borders,
                                            "down",
                                        );
                                        last_drawn_counter = 0;
                                    }
                                    MouseEventKind::Down(MouseButton::Left) => {
                                        if let Some(ref fd) = frame {
                                            let (cols, rows) = terminal::size()
                                                .unwrap_or((80, 24));
                                            let fa =
                                                server_frame_area(cols, rows);
                                            let pa = active_pane_content_rect(
                                                fd,
                                                fa,
                                                hide_borders,
                                            );
                                            if mouse.column >= pa.x
                                                && mouse.column
                                                    < pa.x + pa.width
                                                && mouse.row >= pa.y
                                                && mouse.row < pa.y + pa.height
                                            {
                                                if let Some(sel) =
                                                    try_word_select_on_double_click(
                                                        &mut last_mouse_click,
                                                        fd,
                                                        mouse.row,
                                                        mouse.column,
                                                        hide_borders,
                                                    )
                                                {
                                                    mouse_select = Some(sel);
                                                    mouse_drag_origin = None;
                                                } else {
                                                    mouse_select = Some(
                                                        caret_selection(
                                                            mouse.row,
                                                            mouse.column,
                                                        ),
                                                    );
                                                    mouse_drag_origin = Some(
                                                        MouseDragOrigin {
                                                            row: mouse.row,
                                                            col: mouse.column,
                                                        },
                                                    );
                                                }
                                            } else if tabs.focus_at(
                                                mouse.column,
                                                mouse.row,
                                                hide_borders,
                                            ) {
                                                mode = InputMode::Normal;
                                                copy_mode_confirmed = false;
                                                copy_mode_sync_suppress_frame =
                                                    Some(current_counter);
                                                mouse_select = None;
                                            }
                                        }
                                    }
                                    MouseEventKind::Drag(MouseButton::Left) => {
                                        if let Some(ref mut sel) = mouse_select
                                        {
                                            if let Some(ref fd) = frame {
                                                let (cols, rows) =
                                                    terminal::size()
                                                        .unwrap_or((80, 24));
                                                let fa = server_frame_area(
                                                    cols, rows,
                                                );
                                                let pa =
                                                    active_pane_content_rect(
                                                        fd,
                                                        fa,
                                                        hide_borders,
                                                    );
                                                sel.end_col = mouse
                                                    .column
                                                    .saturating_add(1)
                                                    .max(pa.x)
                                                    .min(pa.x + pa.width);
                                                sel.end_row =
                                                    mouse.row.max(pa.y).min(
                                                        pa.y + pa
                                                            .height
                                                            .saturating_sub(1),
                                                    );
                                            } else {
                                                sel.end_col = mouse.column;
                                                sel.end_row = mouse.row;
                                            }
                                        } else if let Some(origin) =
                                            mouse_drag_origin
                                        {
                                            if let Some(ref fd) = frame {
                                                mouse_select = Some(
                                                    mouse_selection_from_drag(
                                                        origin,
                                                        mouse,
                                                        fd,
                                                        hide_borders,
                                                    ),
                                                );
                                            }
                                        }
                                    }
                                    MouseEventKind::Up(MouseButton::Left) => {
                                        let refresh_ansi =
                                            frame.as_ref().is_some_and(|fd| {
                                                fd.ansi.is_some()
                                            });
                                        let pane_area =
                                            frame.as_ref().map(|fd| {
                                                active_pane_content_rect(
                                                    fd,
                                                    server_frame_area(
                                                        cols, rows,
                                                    ),
                                                    hide_borders,
                                                )
                                            });
                                        let completed_selection =
                                            finalize_mouse_selection(
                                                mouse_select.take(),
                                                mouse_drag_origin,
                                                mouse,
                                                pane_area,
                                            );
                                        if let Some(sel) = completed_selection {
                                            if let Some(ref fd) = frame {
                                                if selection_is_caret(&sel) {
                                                    if mouse.modifiers.contains(
                                                        KeyModifiers::CONTROL,
                                                    ) {
                                                        if let Some(url) =
                                                            detect_url_at_click(
                                                                fd,
                                                                sel.start_row,
                                                                sel.start_col,
                                                                hide_borders,
                                                            )
                                                        {
                                                            open_url(&url);
                                                            status_notice = Some((
                                                                format!(
                                                                    "opening {}",
                                                                    truncate_status_url(&url)
                                                                ),
                                                                Instant::now()
                                                                    + Duration::from_secs(3),
                                                            ));
                                                        }
                                                    }
                                                    last_mouse_click =
                                                        Some(LastMouseClick {
                                                            row: sel.start_row,
                                                            col: sel.start_col,
                                                            at: Instant::now(),
                                                        });
                                                } else {
                                                    last_mouse_click = None;
                                                    copy_drag_selection(
                                                        fd,
                                                        &sel,
                                                        hide_borders,
                                                        &mut status_notice,
                                                        &mut last_drawn_counter,
                                                    );
                                                }
                                            }
                                        }
                                        mouse_drag_origin = None;
                                        if refresh_ansi {
                                            last_drawn_counter = 0;
                                        }
                                    }
                                    _ => {}
                                },
                                _ => {}
                            }
                        }
                        Event::Paste(text) => {
                            handle_paste_event(
                                tabs.focused_client(),
                                &mut mode,
                                text,
                            );
                        }
                        Event::Resize(new_cols, new_rows) => {
                            tabs.resize_all(server_content_size(
                                new_cols, new_rows,
                            ));
                            last_drawn_counter = 0;
                        }
                        _ => {}
                    }
                    // Input modes and overlays are client-side state. Redraw once
                    // after an event changes them, rather than refreshing every
                    // 16 ms while the terminal is otherwise idle.
                    last_drawn_counter = 0;
                }
            }
            Ok(())
        })();

        let _ = terminal::disable_raw_mode();
        if keyboard_enhancement_enabled {
            let _ =
                execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
        }
        let _ = execute!(
            terminal.backend_mut(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen,
            SetCursorStyle::DefaultUserShape,
            cursor::Show
        );
        let _ = write_mouse_pointer_shape(
            terminal.backend_mut(),
            MousePointerShape::Default,
        );
        run_result
    }
}

fn spawn_ssh_connect(
    host: String,
) -> mpsc::Receiver<Result<crate::domain::cloud::CloudClient, String>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = crate::domain::connect_ssh(&host, Size::new(24, 80))
            .map_err(|e| e.to_string());
        let _ = tx.send(result);
    });
    rx
}

fn send_slot_state(
    client: &dyn DomainHandle,
    slot_id: u64,
    state: &str,
    generation: u64,
) {
    let msg = crate::domain::attach::DomainSlotState {
        slot_id,
        state: state.to_string(),
        generation,
    };
    if let Ok(json) = serde_json::to_string(&msg) {
        client.send_control_line(&format!("DOMAIN_SLOT_STATE {json}"));
    }
}

fn server_content_size(cols: u16, rows: u16) -> Size {
    Size::new(rows.saturating_sub(1).max(1), cols.max(1))
}

fn server_frame_area(cols: u16, rows: u16) -> ratatui::layout::Rect {
    ratatui::layout::Rect {
        x: 0,
        y: 1,
        width: cols,
        height: rows.saturating_sub(1),
    }
}

fn server_frame_area_from(
    area: ratatui::layout::Rect,
) -> ratatui::layout::Rect {
    ratatui::layout::Rect {
        x: area.x,
        y: area.y + 1,
        width: area.width,
        height: area.height.saturating_sub(1),
    }
}

fn server_layout_area(cols: u16, rows: u16) -> ratatui::layout::Rect {
    let frame = server_frame_area(cols, rows);
    ratatui::layout::Rect {
        x: frame.x,
        y: frame.y,
        width: frame.width,
        height: frame.height.saturating_sub(1),
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn mouse_for_pane(
    mut mouse: MouseEvent,
    fd: &FrameData,
    layout_area: ratatui::layout::Rect,
    hide_borders: bool,
) -> Option<MouseEvent> {
    if mouse.row == 0 {
        return None;
    }
    let (_, pa) =
        find_active_pane_content(&fd.layout, layout_area, hide_borders);
    if mouse.column < pa.x
        || mouse.column >= pa.x + pa.width
        || mouse.row < pa.y
        || mouse.row >= pa.y + pa.height
    {
        return None;
    }
    mouse.column -= pa.x;
    mouse.row -= pa.y;
    Some(mouse)
}

#[derive(Clone, Copy)]
struct MouseSelection {
    start_col: u16,
    start_row: u16,
    /// Exclusive screen column (one past the last selected column).
    end_col: u16,
    end_row: u16,
}

impl MouseSelection {
    fn normalized_bounds(self) -> (u16, u16, u16, u16) {
        normalized_mouse_selection(&self)
    }
}

fn mouse_selection_bounds_eq(a: &MouseSelection, b: &MouseSelection) -> bool {
    a.normalized_bounds() == b.normalized_bounds()
}

fn selection_is_empty(sel: &MouseSelection) -> bool {
    if selection_is_caret(sel) {
        return true;
    }
    let (start_row, start_col, end_row, end_col) =
        normalized_mouse_selection(sel);
    start_row == end_row && start_col >= end_col
}

fn selection_is_caret(sel: &MouseSelection) -> bool {
    sel.start_row == sel.end_row && sel.start_col == sel.end_col
}

fn caret_selection(row: u16, col: u16) -> MouseSelection {
    MouseSelection {
        start_col: col,
        start_row: row,
        end_col: col,
        end_row: row,
    }
}

fn try_word_select_on_double_click(
    last: &mut Option<LastMouseClick>,
    fd: &FrameData,
    row: u16,
    col: u16,
    hide_borders: bool,
) -> Option<MouseSelection> {
    let is_double = last.as_ref().is_some_and(|prev| {
        prev.row == row
            && prev.col == col
            && prev.at.elapsed() <= DOUBLE_CLICK_INTERVAL
    });
    if is_double {
        last.take();
        word_selection_at_click(fd, row, col, hide_borders)
    } else {
        None
    }
}

/// Display-column range `[start, end)` for the token bounded by whitespace.
fn word_display_range_in_line(
    line: &str,
    pane_col: usize,
) -> Option<(usize, usize)> {
    use unicode_width::UnicodeWidthChar;

    let spans: Vec<(usize, usize, char)> = line
        .chars()
        .scan(0usize, |col, ch| {
            let width = ch.width().unwrap_or(1);
            let start = *col;
            *col += width;
            Some((start, *col, ch))
        })
        .collect();
    let idx = spans
        .iter()
        .position(|(start, end, _)| pane_col >= *start && pane_col < *end)?;
    let is_space = |ch: char| ch.is_whitespace();

    let mut start_idx = idx;
    let mut end_idx = idx;
    if is_space(spans[idx].2) {
        while start_idx > 0 && is_space(spans[start_idx - 1].2) {
            start_idx -= 1;
        }
        while end_idx + 1 < spans.len() && is_space(spans[end_idx + 1].2) {
            end_idx += 1;
        }
    } else {
        while start_idx > 0 && !is_space(spans[start_idx - 1].2) {
            start_idx -= 1;
        }
        while end_idx + 1 < spans.len() && !is_space(spans[end_idx + 1].2) {
            end_idx += 1;
        }
    }
    Some((spans[start_idx].0, spans[end_idx].1))
}

fn pane_coords_at_screen(
    fd: &FrameData,
    screen_row: u16,
    screen_col: u16,
    hide_borders: bool,
) -> Option<(Vec<PaneContentRow>, ratatui::layout::Rect, usize, usize)> {
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let layout_area = server_layout_area(cols, rows);
    let (row_texts, content_area) =
        find_active_pane_content(&fd.layout, layout_area, hide_borders);
    if screen_row < content_area.y
        || screen_row >= content_area.y + content_area.height
        || screen_col < content_area.x
        || screen_col >= content_area.x + content_area.width
    {
        return None;
    }
    let pane_row = (screen_row - content_area.y) as usize;
    let pane_col = (screen_col - content_area.x) as usize;
    Some((row_texts, content_area, pane_row, pane_col))
}

fn selection_for_pane_range(
    content_area: ratatui::layout::Rect,
    screen_row: u16,
    pane_start_col: usize,
    pane_end_col: usize,
) -> MouseSelection {
    MouseSelection {
        start_col: content_area.x + pane_start_col as u16,
        start_row: screen_row,
        end_col: content_area.x + pane_end_col as u16,
        end_row: screen_row,
    }
}

fn copy_drag_selection(
    fd: &FrameData,
    sel: &MouseSelection,
    hide_borders: bool,
    status_notice: &mut Option<(String, Instant)>,
    last_drawn_counter: &mut u64,
) {
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let text = extract_text_from_frame_in_area(
        fd,
        sel,
        server_layout_area(cols, rows),
        hide_borders,
    );
    copy_text_and_notify(&text, status_notice, last_drawn_counter);
}

fn word_selection_at_click(
    fd: &FrameData,
    screen_row: u16,
    screen_col: u16,
    hide_borders: bool,
) -> Option<MouseSelection> {
    let (row_texts, content_area, pane_row, pane_col) =
        pane_coords_at_screen(fd, screen_row, screen_col, hide_borders)?;
    let line = row_texts.get(pane_row)?.text.as_str();
    let (start, end) = word_display_range_in_line(line, pane_col)?;
    Some(selection_for_pane_range(
        content_area,
        screen_row,
        start,
        end,
    ))
}

fn copy_text_and_notify(
    text: &str,
    status_notice: &mut Option<(String, Instant)>,
    last_drawn_counter: &mut u64,
) {
    if text.is_empty() {
        return;
    }
    let result = copy_to_clipboard(text);
    *status_notice = Some((
        match result {
            ClipboardCopyResult::System => {
                format!("copied {} chars", text.chars().count())
            }
            ClipboardCopyResult::Osc52 => {
                format!("sent {} chars via OSC 52", text.chars().count())
            }
            ClipboardCopyResult::Unavailable => format!(
                "yanked {} chars (clipboard unavailable)",
                text.chars().count()
            ),
        },
        Instant::now() + Duration::from_secs(3),
    ));
    *last_drawn_counter = 0;
}

fn mouse_selection_from_drag(
    origin: MouseDragOrigin,
    mouse: MouseEvent,
    fd: &FrameData,
    hide_borders: bool,
) -> MouseSelection {
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let fa = server_frame_area(cols, rows);
    let pa = active_pane_content_rect(fd, fa, hide_borders);
    MouseSelection {
        start_col: origin.col,
        start_row: origin.row,
        end_col: mouse
            .column
            .saturating_add(1)
            .max(pa.x)
            .min(pa.x + pa.width),
        end_row: mouse.row.max(pa.y).min(pa.y + pa.height.saturating_sub(1)),
    }
}

fn finalize_mouse_selection(
    current: Option<MouseSelection>,
    origin: Option<MouseDragOrigin>,
    mouse: MouseEvent,
    pane_area: Option<ratatui::layout::Rect>,
) -> Option<MouseSelection> {
    let Some(origin) = origin else {
        // Double-click word selection has no drag origin and is already final.
        return current;
    };
    if origin.row == mouse.row
        && origin.col == mouse.column
        && current.as_ref().is_none_or(selection_is_caret)
    {
        // Preserve CopyMode's caret selection; Normal mode represents the same
        // no-motion click as None.
        return current;
    }

    // Some terminals can coalesce a fast drag into Down -> Up without an
    // intermediate Drag event. Build the selection here if necessary, and
    // always use the Up coordinates so the final part of a drag is not lost.
    let mut selection = current.unwrap_or(MouseSelection {
        start_col: origin.col,
        start_row: origin.row,
        end_col: origin.col,
        end_row: origin.row,
    });
    if let Some(pa) = pane_area {
        selection.end_col = mouse
            .column
            .saturating_add(1)
            .max(pa.x)
            .min(pa.x + pa.width);
        selection.end_row =
            mouse.row.max(pa.y).min(pa.y + pa.height.saturating_sub(1));
    } else {
        selection.end_col = mouse.column.saturating_add(1);
        selection.end_row = mouse.row;
    }
    Some(selection)
}

struct PaneContentRow {
    text: String,
    line: Option<usize>,
}

fn normalized_mouse_selection(sel: &MouseSelection) -> (u16, u16, u16, u16) {
    let end_inclusive_col = sel.end_col.saturating_sub(1);
    if (sel.start_row, sel.start_col) <= (sel.end_row, end_inclusive_col) {
        (sel.start_row, sel.start_col, sel.end_row, sel.end_col)
    } else {
        (
            sel.end_row,
            end_inclusive_col,
            sel.start_row,
            sel.start_col.saturating_add(1),
        )
    }
}

fn render_mouse_selection(
    f: &mut ratatui::Frame,
    sel: &MouseSelection,
    fd: &FrameData,
    hide_borders: bool,
) {
    use ratatui::style::{Color, Modifier, Style};

    if selection_is_empty(sel) {
        return;
    }

    let (start_row, start_col, end_row, end_col) =
        normalized_mouse_selection(sel);

    let frame_area = f.area();
    let pa = active_pane_content_rect(
        fd,
        server_frame_area_from(frame_area),
        hide_borders,
    );

    let clamp_row =
        |r: u16| r.max(pa.y).min(pa.y + pa.height.saturating_sub(1));
    let clamp_col = |c: u16| c.max(pa.x).min(pa.x + pa.width);

    let sr = clamp_row(start_row);
    let sc = clamp_col(start_col);
    let er = clamp_row(end_row);
    let ec = clamp_col(end_col);

    if sr == er && sc >= ec {
        return;
    }

    let buf = f.buffer_mut();
    for row in sr..=er {
        let col_begin = if row == sr { sc } else { pa.x };
        let col_end = if row == er { ec } else { pa.x + pa.width };
        for col in col_begin..col_end {
            if col >= frame_area.width {
                break;
            }
            let cell = &mut buf[(col, row)];
            cell.set_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::LightCyan)
                    .remove_modifier(Modifier::REVERSED),
            );
        }
    }
}

#[cfg(test)]
fn extract_text_from_frame(
    fd: &FrameData,
    sel: &MouseSelection,
    hide_borders: bool,
) -> String {
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let layout_area = ratatui::layout::Rect {
        x: 0,
        y: 0,
        width: cols,
        height: rows.saturating_sub(1),
    };
    extract_text_from_frame_in_area(fd, sel, layout_area, hide_borders)
}

fn extract_text_from_frame_in_area(
    fd: &FrameData,
    sel: &MouseSelection,
    layout_area: ratatui::layout::Rect,
    hide_borders: bool,
) -> String {
    let (start_row, start_col, end_row, end_col) =
        normalized_mouse_selection(sel);
    if selection_is_empty(sel) {
        return String::new();
    }
    let (rows, content_area) =
        find_active_pane_content(&fd.layout, layout_area, hide_borders);
    if rows.is_empty() {
        return String::new();
    }
    let clamp_row = |r: u16| {
        r.max(content_area.y)
            .min(content_area.y + content_area.height.saturating_sub(1))
    };
    let clamp_col = |c: u16| {
        c.max(content_area.x)
            .min(content_area.x + content_area.width)
    };
    let start_row = clamp_row(start_row);
    let start_col = clamp_col(start_col);
    let end_row = clamp_row(end_row);
    let end_col = clamp_col(end_col);
    if start_row == end_row && start_col >= end_col {
        return String::new();
    }
    let pane_start_row = (start_row - content_area.y) as usize;
    let pane_start_col = (start_col - content_area.x) as usize;
    let pane_end_row = (end_row - content_area.y) as usize;
    let pane_end_col = (end_col - content_area.x) as usize;
    let max_row = rows.len().saturating_sub(1);
    let pane_start_row = pane_start_row.min(max_row);
    let pane_end_row = pane_end_row.min(max_row);
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_line = None;
    let mut has_current = false;
    for row in pane_start_row..=pane_end_row {
        let Some(row_data) = rows.get(row) else {
            continue;
        };
        let col_start = if row == pane_start_row {
            pane_start_col
        } else {
            0
        };
        let col_end = if row == pane_end_row {
            pane_end_col
        } else {
            usize::MAX
        };
        let s = slice_by_display_col(&row_data.text, col_start, col_end);
        if has_current
            && row_data.line.is_some()
            && current_line == row_data.line
        {
            current.push_str(&s);
        } else {
            if has_current {
                lines.push(current.trim_end().to_string());
            }
            current = s;
            current_line = row_data.line;
            has_current = true;
        }
    }
    if has_current {
        lines.push(current.trim_end().to_string());
    }
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

fn slice_by_display_col(s: &str, col_start: usize, col_end: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut result = String::new();
    let mut col = 0usize;
    for ch in s.chars() {
        let w = ch.width().unwrap_or(1);
        if col + w > col_end {
            break;
        }
        if col >= col_start {
            result.push(ch);
        }
        col += w;
    }
    result
}

fn active_pane_layout_rect(
    layout: &LayoutJson,
    area: ratatui::layout::Rect,
    hide_borders: bool,
) -> ratatui::layout::Rect {
    match layout {
        LayoutJson::Leaf { active: true, .. } => area,
        LayoutJson::Split {
            direction,
            sizes,
            children,
        } => {
            let chunks = split_layout_rects_for_extract(
                area,
                direction,
                sizes,
                children.len(),
                hide_borders,
            );
            for (child, chunk) in children.iter().zip(chunks.iter()) {
                let rect = active_pane_layout_rect(child, *chunk, hide_borders);
                if pane_tree_has_active(child) {
                    return rect;
                }
            }
            area
        }
        LayoutJson::Leaf { .. } => area,
        LayoutJson::External {
            graft: Some(g),
            active: true,
            ..
        } => active_pane_layout_rect(g, area, false),
        LayoutJson::External { .. } => area,
    }
}

fn pane_tree_has_active(layout: &LayoutJson) -> bool {
    match layout {
        LayoutJson::Leaf { active: true, .. } => true,
        LayoutJson::Leaf { .. } => false,
        LayoutJson::Split { children, .. } => {
            children.iter().any(pane_tree_has_active)
        }
        LayoutJson::External {
            graft: Some(g),
            active: true,
            ..
        } => pane_tree_has_active(g),
        LayoutJson::External { active, .. } => *active,
    }
}

fn active_pane_content_rect(
    fd: &FrameData,
    frame_area: ratatui::layout::Rect,
    hide_borders: bool,
) -> ratatui::layout::Rect {
    let content_height = frame_area.height.saturating_sub(1);
    let layout_area = ratatui::layout::Rect {
        x: frame_area.x,
        y: frame_area.y,
        width: frame_area.width,
        height: content_height,
    };
    let (_, pane_area) =
        find_active_pane_content(&fd.layout, layout_area, hide_borders);
    pane_area
}

fn find_active_pane_content(
    layout: &LayoutJson,
    area: ratatui::layout::Rect,
    hide_borders: bool,
) -> (Vec<PaneContentRow>, ratatui::layout::Rect) {
    match layout {
        LayoutJson::Leaf {
            active: true,
            rows_v2,
            ..
        } => {
            let content_area =
                if !hide_borders && area.width > 2 && area.height > 2 {
                    ratatui::layout::Rect {
                        x: area.x + 1,
                        y: area.y + 1,
                        width: area.width - 2,
                        height: area.height - 2,
                    }
                } else {
                    area
                };
            let rows = rows_v2
                .iter()
                .map(|row| PaneContentRow {
                    text: row.runs.iter().map(|r| r.text.as_str()).collect(),
                    line: row.line,
                })
                .collect();
            (rows, content_area)
        }
        LayoutJson::Split {
            direction,
            sizes,
            children,
        } => {
            let chunks = split_layout_rects_for_extract(
                area,
                direction,
                sizes,
                children.len(),
                hide_borders,
            );
            for (child, chunk) in children.iter().zip(chunks.iter()) {
                if matches!(child, LayoutJson::Leaf { active: true, .. }) {
                    let (rows, content_area) =
                        find_active_pane_content(child, *chunk, hide_borders);
                    if !rows.is_empty() {
                        return (rows, content_area);
                    }
                }
            }
            for (child, chunk) in children.iter().zip(chunks.into_iter()) {
                let (rows, content_area) =
                    find_active_pane_content(child, chunk, hide_borders);
                if !rows.is_empty() {
                    return (rows, content_area);
                }
            }
            (Vec::new(), area)
        }
        LayoutJson::Leaf { .. } => (Vec::new(), area),
        LayoutJson::External {
            graft: Some(g),
            active: true,
            ..
        } => find_active_pane_content(g, area, false),
        LayoutJson::External { .. } => (Vec::new(), area),
    }
}

/// Walk the layout tree and return the pane id whose area contains (col, row).
/// Returns None if the coordinate falls outside all panes (e.g. on a border).
fn find_pane_id_at(
    layout: &LayoutJson,
    area: ratatui::layout::Rect,
    col: u16,
    row: u16,
    hide_borders: bool,
) -> Option<usize> {
    match layout {
        LayoutJson::Leaf { id, .. } => {
            if col >= area.x
                && col < area.x + area.width
                && row >= area.y
                && row < area.y + area.height
            {
                Some(*id)
            } else {
                None
            }
        }
        LayoutJson::Split {
            direction,
            sizes,
            children,
        } => {
            let chunks = split_layout_rects_for_extract(
                area,
                direction,
                sizes,
                children.len(),
                hide_borders,
            );
            for (child, chunk) in children.iter().zip(chunks.into_iter()) {
                if let Some(id) =
                    find_pane_id_at(child, chunk, col, row, hide_borders)
                {
                    return Some(id);
                }
            }
            None
        }
        LayoutJson::External { graft: Some(g), .. } => {
            find_pane_id_at(g, area, col, row, hide_borders)
        }
        LayoutJson::External { id, .. } => {
            if col >= area.x
                && col < area.x + area.width
                && row >= area.y
                && row < area.y + area.height
            {
                Some(*id)
            } else {
                None
            }
        }
    }
}

fn active_pane_id(layout: &LayoutJson) -> Option<usize> {
    match layout {
        LayoutJson::Leaf {
            id, active: true, ..
        } => Some(*id),
        LayoutJson::Split { children, .. } => {
            children.iter().find_map(active_pane_id)
        }
        LayoutJson::Leaf { .. } => None,
        LayoutJson::External {
            graft: Some(g),
            active: true,
            ..
        } => active_pane_id(g),
        LayoutJson::External { id, active, .. } if *active => Some(*id),
        LayoutJson::External { .. } => None,
    }
}

fn split_layout_rects_for_extract(
    area: ratatui::layout::Rect,
    direction: &str,
    sizes: &[u16],
    count: usize,
    hide_borders: bool,
) -> Vec<ratatui::layout::Rect> {
    if count == 0 {
        return Vec::new();
    }
    let horizontal = direction == "horizontal";
    let total_dim = if horizontal { area.width } else { area.height };
    let gap: u16 = if hide_borders { 0 } else { 1 };
    let borders = count.saturating_sub(1) as u16 * gap;
    let available = total_dim.saturating_sub(borders);
    let total_pct = sizes.iter().copied().sum::<u16>().max(1);
    let mut rects = Vec::with_capacity(count);
    let mut offset = 0u16;
    for (index, &pct) in sizes.iter().enumerate().take(count) {
        let dim = if index + 1 == count {
            available.saturating_sub(offset)
        } else {
            (available as u32 * pct as u32 / total_pct as u32) as u16
        };
        rects.push(if horizontal {
            ratatui::layout::Rect {
                x: area.x + offset,
                y: area.y,
                width: dim,
                height: area.height,
            }
        } else {
            ratatui::layout::Rect {
                x: area.x,
                y: area.y + offset,
                width: area.width,
                height: dim,
            }
        });
        offset += dim + gap;
    }
    rects
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MousePointerShape {
    Default,
    Text,
}

impl MousePointerShape {
    fn osc_name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Text => "text",
        }
    }
}

fn write_mouse_pointer_shape<W: Write>(
    writer: &mut W,
    shape: MousePointerShape,
) -> io::Result<()> {
    write!(writer, "\x1b]22;{}\x07", shape.osc_name())?;
    writer.flush()
}

fn apply_mouse_pointer_shape<W: Write>(
    writer: &mut W,
    applied: &mut Option<MousePointerShape>,
    desired: Option<MousePointerShape>,
    force: bool,
) -> io::Result<()> {
    let Some(desired) = desired else {
        return Ok(());
    };
    if !force && *applied == Some(desired) {
        return Ok(());
    }
    write_mouse_pointer_shape(writer, desired)?;
    *applied = Some(desired);
    Ok(())
}

fn refresh_mouse_pointer<W: Write>(
    writer: &mut W,
    applied: &mut Option<MousePointerShape>,
    last_mouse_pos: Option<(u16, u16)>,
    frame: Option<&FrameData>,
    cols: u16,
    rows: u16,
    hide_borders: bool,
    hide_status: bool,
    ui_overlay_active: bool,
    force: bool,
) -> io::Result<()> {
    let desired = desired_mouse_pointer_shape(
        last_mouse_pos,
        frame,
        cols,
        rows,
        hide_borders,
        hide_status,
        ui_overlay_active,
    );
    apply_mouse_pointer_shape(writer, applied, desired, force)
}

fn desired_mouse_pointer_shape(
    last_mouse_pos: Option<(u16, u16)>,
    frame: Option<&FrameData>,
    cols: u16,
    rows: u16,
    hide_borders: bool,
    hide_status: bool,
    ui_overlay_active: bool,
) -> Option<MousePointerShape> {
    if ui_overlay_active {
        return Some(MousePointerShape::Default);
    }
    let (col, row) = last_mouse_pos?;
    let fd = frame?;
    Some(mouse_pointer_shape_at(
        row,
        col,
        cols,
        rows,
        fd,
        hide_borders,
        hide_status,
    ))
}

fn mouse_pointer_shape_at(
    row: u16,
    col: u16,
    cols: u16,
    rows: u16,
    fd: &FrameData,
    hide_borders: bool,
    hide_status: bool,
) -> MousePointerShape {
    if row == 0 {
        return MousePointerShape::Default;
    }
    if let Some(status_row) = status_bar_screen_row(rows, hide_status) {
        if row == status_row {
            return MousePointerShape::Default;
        }
    }
    let layout_area = server_layout_area(cols, rows);
    if col < layout_area.x
        || col >= layout_area.x + layout_area.width
        || row < layout_area.y
        || row >= layout_area.y + layout_area.height
    {
        return MousePointerShape::Default;
    }
    if let (Some(hovered), Some(active)) = (
        find_pane_id_at(&fd.layout, layout_area, col, row, hide_borders),
        active_pane_id(&fd.layout),
    ) {
        if hovered != active {
            return MousePointerShape::Default;
        }
    }
    // 应用程序启用了鼠标模式，鼠标事件直接转发给 PTY，不做文字选择
    if active_mouse_mode(fd) != 0 {
        return MousePointerShape::Default;
    }
    let pane_area =
        active_pane_layout_rect(&fd.layout, layout_area, hide_borders);
    // 去掉 border 占用的 1 格，使光标只在真正的内容区内变为 I 型
    let content_area =
        if !hide_borders && pane_area.width > 2 && pane_area.height > 2 {
            ratatui::layout::Rect {
                x: pane_area.x + 1,
                y: pane_area.y + 1,
                width: pane_area.width - 2,
                height: pane_area.height - 2,
            }
        } else {
            pane_area
        };
    if col >= content_area.x
        && col < content_area.x + content_area.width
        && row >= content_area.y
        && row < content_area.y + content_area.height
    {
        MousePointerShape::Text
    } else {
        MousePointerShape::Default
    }
}

fn cursor_style_for_shape(shape: Option<u8>) -> SetCursorStyle {
    match shape.unwrap_or(crate::pty::CURSOR_SHAPE_UNSET) {
        0 | 1 => SetCursorStyle::BlinkingBlock,
        2 => SetCursorStyle::SteadyBlock,
        3 => SetCursorStyle::BlinkingUnderScore,
        4 => SetCursorStyle::SteadyUnderScore,
        5 => SetCursorStyle::BlinkingBar,
        6 => SetCursorStyle::SteadyBar,
        _ => SetCursorStyle::DefaultUserShape,
    }
}

fn leave_copy_mode_client(
    client: &dyn DomainHandle,
    mode: &mut InputMode,
    copy_mode_confirmed: &mut bool,
    copy_mode_exit_pending: &mut bool,
    display_scrolled: &mut bool,
) {
    client.exit_copy_mode();
    *mode = InputMode::Normal;
    *copy_mode_confirmed = false;
    *copy_mode_exit_pending = true;
    *display_scrolled = false;
}

/// After prefix navigation from copy mode, keep server copy state on the
/// scrolled pane but stop the client from re-entering CopyMode until the
/// next frame reflects the new focus.
fn suppress_copy_mode_client_sync(
    copy_mode_confirmed: &mut bool,
    copy_mode_sync_suppress_frame: &mut Option<u64>,
    current_counter: u64,
) {
    *copy_mode_confirmed = false;
    *copy_mode_sync_suppress_frame = Some(current_counter);
}

fn is_shifted_letter(key: KeyEvent, letter: char) -> bool {
    let KeyCode::Char(c) = key.code else {
        return false;
    };
    if c.to_ascii_uppercase() != letter.to_ascii_uppercase() {
        return false;
    }
    !key.modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        && (c.is_ascii_uppercase()
            || key.modifiers.contains(KeyModifiers::SHIFT))
}

fn prefix_nav_dir(key: KeyEvent) -> Option<crate::layout::NavDir> {
    match (key.code, key.modifiers) {
        (KeyCode::Char('h'), KeyModifiers::NONE) | (KeyCode::Left, _) => {
            Some(crate::layout::NavDir::Left)
        }
        (KeyCode::Char('j'), KeyModifiers::NONE) | (KeyCode::Down, _) => {
            Some(crate::layout::NavDir::Down)
        }
        (KeyCode::Char('k'), KeyModifiers::NONE) | (KeyCode::Up, _) => {
            Some(crate::layout::NavDir::Up)
        }
        (KeyCode::Char('l'), KeyModifiers::NONE) | (KeyCode::Right, _) => {
            Some(crate::layout::NavDir::Right)
        }
        _ => None,
    }
}

fn handle_prefix_key(
    server: &dyn DomainHandle,
    key: KeyEvent,
) -> Option<String> {
    let cmd = match (key.code, key.modifiers) {
        (KeyCode::Char('%'), _) => "split-window -h",
        (KeyCode::Char('"'), _) => "split-window -v",
        (KeyCode::Char('c'), KeyModifiers::NONE) => "new-window",
        (KeyCode::Char('n'), KeyModifiers::NONE) => "select-window -n",
        (KeyCode::Char('p'), KeyModifiers::NONE) => "select-window -p",
        (KeyCode::Char('x'), KeyModifiers::NONE) => "kill-pane",
        (KeyCode::Char('z'), KeyModifiers::NONE) => "zoom-pane",
        _ if is_shifted_letter(key, 'K') => "clear-pane",
        _ if is_shifted_letter(key, 'H') => "set-pane-start-dir",
        (KeyCode::Char('h'), KeyModifiers::NONE) => "select-pane -L",
        (KeyCode::Char('j'), KeyModifiers::NONE) => "select-pane -D",
        (KeyCode::Char('k'), KeyModifiers::NONE) => "select-pane -U",
        (KeyCode::Char('l'), KeyModifiers::NONE) => "select-pane -R",
        (KeyCode::Up, _) => "select-pane -U",
        (KeyCode::Down, _) => "select-pane -D",
        (KeyCode::Left, _) => "select-pane -L",
        (KeyCode::Right, _) => "select-pane -R",
        _ => return None,
    };
    run_command_notice(server, cmd)
}

fn run_client_tab_command(
    tabs: &mut TabManager,
    raw: &str,
    size: Size,
) -> ClientTabCommandResult {
    let mut parsed = ParsedCommand::parse(raw);
    if parsed.len() != 1 {
        return ClientTabCommandResult::NotHandled;
    }
    let cmd = parsed.remove(0);
    match cmd.name.as_str() {
        "new-tab" | "newt" => {
            let title = cmd.flag_value("t").unwrap_or_default().to_string();
            match tabs.create_tab(size) {
                Ok(()) => {
                    tabs.set_active_title(title);
                    ClientTabCommandResult::Handled(Some("new tab".to_string()))
                }
                Err(e) => ClientTabCommandResult::Handled(Some(format!(
                    "new tab failed: {}",
                    e
                ))),
            }
        }
        "new" if cmd.flags.contains_key("t") => {
            let title = cmd.flag_value("t").unwrap_or_default().to_string();
            match tabs.create_tab(size) {
                Ok(()) => {
                    tabs.set_active_title(title);
                    ClientTabCommandResult::Handled(Some("new tab".to_string()))
                }
                Err(e) => ClientTabCommandResult::Handled(Some(format!(
                    "new tab failed: {}",
                    e
                ))),
            }
        }
        "select-tab" | "selectt" => {
            let target = cmd
                .flag_value("t")
                .or_else(|| cmd.args.first().map(String::as_str));
            let Some(target) = target else {
                return ClientTabCommandResult::Handled(Some(
                    "usage: select-tab -t <code|index|title>".to_string(),
                ));
            };
            if let Some(index) = tab_target_index(tabs, target) {
                tabs.select(index);
                tabs.active_client().resize(size);
                ClientTabCommandResult::Handled(None)
            } else {
                ClientTabCommandResult::Handled(Some(format!(
                    "tab not found: {}",
                    target
                )))
            }
        }
        "rename-tab" | "renamet" => {
            let code = cmd
                .flag_value("c")
                .unwrap_or(&tabs.active_code())
                .to_string();
            let title = if cmd.flags.contains_key("t") {
                cmd.flag_value("t").unwrap_or_default().to_string()
            } else {
                cmd.args
                    .first()
                    .cloned()
                    .unwrap_or_else(|| tabs.active_title())
            };
            match tabs.set_active_metadata(&code, title) {
                Ok(()) => ClientTabCommandResult::Handled(Some(
                    "renamed tab".to_string(),
                )),
                Err(e) => ClientTabCommandResult::Handled(Some(e)),
            }
        }
        "next-tab" | "nextt" => {
            tabs.next_tab();
            tabs.active_client().resize(size);
            ClientTabCommandResult::Handled(None)
        }
        "prev-tab" | "prevt" => {
            tabs.prev_tab();
            tabs.active_client().resize(size);
            ClientTabCommandResult::Handled(None)
        }
        "list-tabs" | "lst" => {
            ClientTabCommandResult::Handled(Some(tab_summary(tabs)))
        }
        "ssh-attach" | "ssh" => {
            let host = cmd
                .args
                .first()
                .map(String::as_str)
                .or_else(|| cmd.flag_value("t"));
            let Some(host) = host else {
                return ClientTabCommandResult::Handled(Some(
                    "usage: ssh-attach <host>".to_string(),
                ));
            };
            match tabs.attach_ssh(host, size) {
                Ok(message) => ClientTabCommandResult::Handled(Some(message)),
                Err(e) => ClientTabCommandResult::Handled(Some(format!(
                    "ssh-attach failed: {e}"
                ))),
            }
        }
        "paste-cloud" => match tabs.focused_client().paste_cloud() {
            Ok(message) => ClientTabCommandResult::Handled(Some(message)),
            Err(e) => ClientTabCommandResult::Handled(Some(e)),
        },
        _ => ClientTabCommandResult::NotHandled,
    }
}

fn run_command_notice(server: &dyn DomainHandle, cmd: &str) -> Option<String> {
    if cmd.trim() == "set-pane-start-dir" {
        let output = server.run_command_with_output(cmd);
        let path = output.trim();
        return Some(if path.is_empty() {
            "set start dir failed".to_string()
        } else {
            format!("start dir: {}", path)
        });
    }
    if matches!(
        cmd.split_whitespace().next(),
        Some("set-option" | "set" | "show-options" | "show")
    ) {
        let output = server.run_command_with_output(cmd);
        let message = output.trim();
        return (!message.is_empty()).then(|| message.to_string());
    }
    server.run_command(cmd);
    None
}

fn set_tab_start_dir(tabs: &mut TabManager) -> Option<String> {
    let output = tabs
        .active_client()
        .run_command_with_output("set-pane-start-dir");
    let Some(path) = start_dir_from_command_output(&output) else {
        return Some("set start dir failed".to_string());
    };
    tabs.start_dir = Some(path.clone());
    Some(format!("start dir: {}", path))
}

fn start_dir_from_command_output(output: &str) -> Option<String> {
    let path = output.trim();
    (!path.is_empty()).then(|| path.to_string())
}

fn is_resize_modifier_key(key: KeyEvent) -> bool {
    matches!(
        key.code,
        KeyCode::Modifier(ModifierKeyCode::LeftAlt)
            | KeyCode::Modifier(ModifierKeyCode::RightAlt)
    )
}

fn resize_command_for_key(key: KeyEvent) -> Option<&'static str> {
    if !key.modifiers.contains(KeyModifiers::ALT) {
        return None;
    }
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::SHIFT)
    {
        return None;
    }
    match key.code {
        KeyCode::Char('h') | KeyCode::Left => Some("resize-pane -L"),
        KeyCode::Char('j') | KeyCode::Down => Some("resize-pane -D"),
        KeyCode::Char('k') | KeyCode::Up => Some("resize-pane -U"),
        KeyCode::Char('l') | KeyCode::Right => Some("resize-pane -R"),
        _ => None,
    }
}

fn status_banner_for_mode(
    mode: &InputMode,
    notice: Option<&str>,
) -> Option<String> {
    let mode_label = match mode {
        InputMode::Resize => Some("RESIZE"),
        InputMode::CopyMode => Some("COPY"),
        InputMode::CopySearch { forward, .. } => {
            Some(if *forward { "COPY /" } else { "COPY ?" })
        }
        _ => None,
    };
    match (mode_label, notice) {
        (Some(label), Some(notice)) => Some(format!("{} | {}", label, notice)),
        (Some(label), None) => Some(label.to_string()),
        (None, Some(notice)) => Some(notice.to_string()),
        (None, None) => None,
    }
}

/// Server ANSI writes directly to the terminal, bypassing Ratatui's buffer.
/// Overlays/prompts still redraw on top in the same frame (`AlwaysUpdate` via
/// `begin_floating_panel` / prompt row), so pane output can keep flowing while a
/// chooser is open. Deferring ANSI made live pane output appear frozen until the
/// overlay closed, then jump forward in one burst.
fn should_write_server_ansi(_has_overlay: bool, _has_prompt: bool) -> bool {
    true
}

enum ClipboardCopyResult {
    System,
    Osc52,
    Unavailable,
}

fn copy_to_clipboard(text: &str) -> ClipboardCopyResult {
    if text.is_empty() {
        return ClipboardCopyResult::Unavailable;
    }
    if should_prefer_osc52() {
        if copy_to_clipboard_via_osc52(text).is_ok() {
            return ClipboardCopyResult::Osc52;
        }
        if copy_to_clipboard_via_arboard(text) {
            return ClipboardCopyResult::System;
        }
    } else {
        if copy_to_clipboard_via_arboard(text) {
            return ClipboardCopyResult::System;
        }
        if copy_to_clipboard_via_osc52(text).is_ok() {
            return ClipboardCopyResult::Osc52;
        }
    }
    ClipboardCopyResult::Unavailable
}

fn copy_to_clipboard_via_arboard(text: &str) -> bool {
    Clipboard::new()
        .and_then(|mut clipboard| clipboard.set_text(text.to_string()))
        .is_ok()
}

fn copy_to_clipboard_via_osc52(text: &str) -> io::Result<()> {
    let sequence = build_osc52_sequence(text);
    let mut stdout = io::stdout();
    stdout.write_all(sequence.as_bytes())?;
    stdout.flush()
}

fn build_osc52_sequence(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", STANDARD.encode(text.as_bytes()))
}

fn should_prefer_osc52() -> bool {
    has_ssh_environment(|key| std::env::var(key).ok())
}

fn has_ssh_environment(lookup: impl Fn(&str) -> Option<String>) -> bool {
    ["SSH_TTY", "SSH_CONNECTION", "SSH_CLIENT"]
        .into_iter()
        .any(|key| lookup(key).is_some_and(|value| !value.is_empty()))
}

fn is_copy_plain_key(modifiers: KeyModifiers) -> bool {
    !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

fn is_copy_line_start_key(key: KeyEvent) -> bool {
    is_copy_plain_key(key.modifiers)
        && matches!(key.code, KeyCode::Home | KeyCode::Char('0'))
}

fn is_copy_line_end_key(key: KeyEvent) -> bool {
    is_copy_plain_key(key.modifiers)
        && matches!(key.code, KeyCode::End | KeyCode::Char('$'))
        || (matches!(key.code, KeyCode::Char('4'))
            && key.modifiers.contains(KeyModifiers::SHIFT)
            && is_copy_plain_key(key.modifiers))
}

fn handle_paste_event(
    server: &dyn DomainHandle,
    mode: &mut InputMode,
    text: String,
) {
    if text.is_empty() {
        return;
    }
    match mode.clone() {
        InputMode::Normal => {
            server.send_paste(&text);
        }
        InputMode::Prefix | InputMode::Resize => {
            server.send_paste(&text);
            *mode = InputMode::Normal;
        }
        InputMode::CopySearch {
            mut buf,
            mut cursor,
            forward,
        } => {
            insert_text_at_cursor(&mut buf, &mut cursor, &text);
            *mode = InputMode::CopySearch {
                buf,
                cursor,
                forward,
            };
        }
        InputMode::RenameWindow {
            mut buf,
            mut cursor,
        } => {
            insert_text_at_cursor(&mut buf, &mut cursor, &text);
            *mode = InputMode::RenameWindow { buf, cursor };
        }
        InputMode::RenameSession {
            mut buf,
            mut cursor,
        } => {
            insert_text_at_cursor(&mut buf, &mut cursor, &text);
            *mode = InputMode::RenameSession { buf, cursor };
        }
        InputMode::RenameTab {
            mut code,
            mut code_cursor,
            mut title,
            mut title_cursor,
            editing_code,
            return_to_tab_chooser,
            ..
        } => {
            if editing_code {
                let text = text.to_ascii_uppercase();
                insert_text_at_cursor(&mut code, &mut code_cursor, &text);
            } else {
                insert_text_at_cursor(&mut title, &mut title_cursor, &text);
            }
            *mode = InputMode::RenameTab {
                code,
                code_cursor,
                title,
                title_cursor,
                editing_code,
                error: None,
                return_to_tab_chooser,
            };
        }
        InputMode::Command {
            mut buf,
            mut cursor,
        } => {
            insert_text_at_cursor(&mut buf, &mut cursor, &text);
            *mode = InputMode::Command { buf, cursor };
        }
        InputMode::TabChooser {
            mut query,
            mut cursor,
            ..
        } => {
            insert_text_at_cursor(&mut query, &mut cursor, &text);
            *mode = InputMode::TabChooser {
                query,
                cursor,
                selected: 0,
                search_active: true,
            };
        }
        InputMode::TabQuickSwitch { mut code, .. } => {
            for c in text.chars() {
                if code.len() >= 2 {
                    break;
                }
                if c.is_ascii_alphabetic() {
                    code.push(c.to_ascii_uppercase());
                }
            }
            *mode = InputMode::TabQuickSwitch { code, error: None };
        }
        InputMode::CopyMode
        | InputMode::SessionChooser { .. }
        | InputMode::OptionPanel { .. }
        | InputMode::ConfirmKillTab { .. } => {}
    }
}

fn xterm_modifier_param(modifiers: KeyModifiers) -> u8 {
    let mut param = 1u8;
    if modifiers.contains(KeyModifiers::SHIFT) {
        param += 1;
    }
    if modifiers.contains(KeyModifiers::ALT) {
        param += 2;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        param += 4;
    }
    param
}

fn csi_letter_with_modifiers(letter: char, modifiers: KeyModifiers) -> Vec<u8> {
    let param = xterm_modifier_param(modifiers);
    if param > 1 {
        format!("\x1b[1;{}{}", param, letter).into_bytes()
    } else {
        format!("\x1b[{}", letter).into_bytes()
    }
}

fn csi_tilde_with_modifiers(code: u8, modifiers: KeyModifiers) -> Vec<u8> {
    let param = xterm_modifier_param(modifiers);
    if param > 1 {
        format!("\x1b[{};{}~", code, param).into_bytes()
    } else {
        format!("\x1b[{}~", code).into_bytes()
    }
}

fn fkey_with_modifiers(n: u8, base: &[u8], modifiers: KeyModifiers) -> Vec<u8> {
    let param = xterm_modifier_param(modifiers);
    if param > 1 {
        match n {
            1..=4 => {
                let letter = base[base.len() - 1];
                format!("\x1b[1;{}{}", param, letter as char).into_bytes()
            }
            _ => {
                let code = match n {
                    5 => 15,
                    6 => 17,
                    7 => 18,
                    8 => 19,
                    9 => 20,
                    10 => 21,
                    11 => 23,
                    12 => 24,
                    13 => 25,
                    14 => 26,
                    15 => 28,
                    16 => 29,
                    17 => 31,
                    18 => 32,
                    19 => 33,
                    20 => 34,
                    21 => 42,
                    22 => 43,
                    23 => 44,
                    24 => 45,
                    _ => return vec![],
                };
                format!("\x1b[{};{}~", code, param).into_bytes()
            }
        }
    } else {
        base.to_vec()
    }
}

fn key_to_bytes(key: KeyEvent) -> Vec<u8> {
    let mods = key.modifiers;
    let mods_no_shift = mods & !KeyModifiers::SHIFT;

    let mut bytes = match key.code {
        KeyCode::Enter => b"\r".to_vec(),
        KeyCode::Char(c) if c == '\r' || c == '\n' => b"\r".to_vec(),
        KeyCode::Char(c) => {
            if mods.contains(KeyModifiers::CONTROL) {
                let lower = c.to_ascii_lowercase();
                if lower >= 'a' && lower <= 'z' {
                    vec![lower as u8 - b'a' + 1]
                } else {
                    match c {
                        ' ' | '@' | '2' => vec![0x00],
                        '[' | '3' => vec![0x1b],
                        '\\' | '4' => vec![0x1c],
                        ']' | '5' => vec![0x1d],
                        '^' | '6' => vec![0x1e],
                        '/' | '_' | '7' => vec![0x1f],
                        '?' => vec![0x7f],
                        _ => {
                            if (c as u32) < 0x20 {
                                vec![c as u8]
                            } else {
                                let mut buf = [0u8; 4];
                                c.encode_utf8(&mut buf).as_bytes().to_vec()
                            }
                        }
                    }
                }
            } else if (c as u32) < 0x20 {
                vec![c as u8]
            } else {
                let mut buf = [0u8; 4];
                c.encode_utf8(&mut buf).as_bytes().to_vec()
            }
        }
        KeyCode::Backspace => b"\x7f".to_vec(),
        KeyCode::Esc => b"\x1b".to_vec(),
        KeyCode::Null => vec![0x00],
        KeyCode::Tab => {
            if mods.contains(KeyModifiers::SHIFT) {
                b"\x1b[Z".to_vec()
            } else {
                b"\t".to_vec()
            }
        }
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Up => csi_letter_with_modifiers('A', mods_no_shift),
        KeyCode::Down => csi_letter_with_modifiers('B', mods_no_shift),
        KeyCode::Right => csi_letter_with_modifiers('C', mods_no_shift),
        KeyCode::Left => csi_letter_with_modifiers('D', mods_no_shift),
        KeyCode::Home => csi_letter_with_modifiers('H', mods_no_shift),
        KeyCode::End => csi_letter_with_modifiers('F', mods_no_shift),
        KeyCode::Insert => csi_tilde_with_modifiers(2, mods_no_shift),
        KeyCode::Delete => csi_tilde_with_modifiers(3, mods_no_shift),
        KeyCode::PageUp => csi_tilde_with_modifiers(5, mods_no_shift),
        KeyCode::PageDown => csi_tilde_with_modifiers(6, mods_no_shift),
        KeyCode::F(n) => {
            let base: Vec<u8> = match n {
                1 => b"\x1bOP".to_vec(),
                2 => b"\x1bOQ".to_vec(),
                3 => b"\x1bOR".to_vec(),
                4 => b"\x1bOS".to_vec(),
                5 => b"\x1b[15~".to_vec(),
                6 => b"\x1b[17~".to_vec(),
                7 => b"\x1b[18~".to_vec(),
                8 => b"\x1b[19~".to_vec(),
                9 => b"\x1b[20~".to_vec(),
                10 => b"\x1b[21~".to_vec(),
                11 => b"\x1b[23~".to_vec(),
                12 => b"\x1b[24~".to_vec(),
                13 => b"\x1b[25~".to_vec(),
                14 => b"\x1b[26~".to_vec(),
                15 => b"\x1b[28~".to_vec(),
                16 => b"\x1b[29~".to_vec(),
                17 => b"\x1b[31~".to_vec(),
                18 => b"\x1b[32~".to_vec(),
                19 => b"\x1b[33~".to_vec(),
                20 => b"\x1b[34~".to_vec(),
                21 => b"\x1b[42~".to_vec(),
                22 => b"\x1b[43~".to_vec(),
                23 => b"\x1b[44~".to_vec(),
                24 => b"\x1b[45~".to_vec(),
                _ => vec![],
            };
            fkey_with_modifiers(n, &base, mods_no_shift)
        }
        _ => vec![],
    };
    if !bytes.is_empty()
        && mods.contains(KeyModifiers::ALT)
        && !matches!(
            key.code,
            KeyCode::Esc
                | KeyCode::Modifier(_)
                | KeyCode::Up
                | KeyCode::Down
                | KeyCode::Right
                | KeyCode::Left
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::Insert
                | KeyCode::Delete
                | KeyCode::PageUp
                | KeyCode::PageDown
                | KeyCode::F(_)
        )
    {
        bytes.insert(0, 0x1b);
    }
    bytes
}

fn mouse_to_bytes(event: MouseEvent) -> Vec<u8> {
    let col = event.column;
    let row = event.row;
    let mut cb = match event.kind {
        MouseEventKind::Down(MouseButton::Left) => 0,
        MouseEventKind::Down(MouseButton::Middle) => 1,
        MouseEventKind::Down(MouseButton::Right) => 2,
        MouseEventKind::Up(MouseButton::Left) => 0,
        MouseEventKind::Up(MouseButton::Middle) => 1,
        MouseEventKind::Up(MouseButton::Right) => 2,
        MouseEventKind::Drag(MouseButton::Left) => 32,
        MouseEventKind::Drag(MouseButton::Middle) => 33,
        MouseEventKind::Drag(MouseButton::Right) => 34,
        MouseEventKind::Moved => 35,
        MouseEventKind::ScrollUp => 64,
        MouseEventKind::ScrollDown => 65,
        MouseEventKind::ScrollLeft => 66,
        MouseEventKind::ScrollRight => 67,
    };
    if event.modifiers.contains(KeyModifiers::SHIFT) {
        cb |= 4;
    }
    if event.modifiers.contains(KeyModifiers::ALT) {
        cb |= 8;
    }
    if event.modifiers.contains(KeyModifiers::CONTROL) {
        cb |= 16;
    }
    let suffix = match event.kind {
        MouseEventKind::Up(_) => 'm',
        _ => 'M',
    };
    format!("\x1b[<{};{};{}{}", cb, col + 1, row + 1, suffix).into_bytes()
}

fn char_byte_pos(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

fn insert_text_at_cursor(buf: &mut String, cursor: &mut usize, text: &str) {
    let bp = char_byte_pos(buf, *cursor);
    buf.insert_str(bp, text);
    *cursor += text.chars().count();
}

fn shell_quote(s: &str) -> String {
    if s.contains(|c: char| c.is_whitespace() || c == '"' || c == '\'') {
        format!("\"{}\"", s.replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SessionChooserFocus {
    session_name: String,
    window_index: usize,
    pane_id: usize,
}

fn active_session_focus_from_frame(
    frame: &FrameData,
) -> Option<SessionChooserFocus> {
    let status = frame.status.as_ref()?;
    let session_name = status
        .left
        .trim()
        .strip_prefix('[')?
        .strip_suffix(']')?
        .trim()
        .to_string();
    let window = status.windows.iter().find(|w| w.active)?;
    let pane_id = active_pane_id(&frame.layout)?;
    Some(SessionChooserFocus {
        session_name,
        window_index: window.index,
        pane_id,
    })
}

fn build_initial_session_chooser_state(
    entries: &[SessionTreeEntry],
    focus: Option<SessionChooserFocus>,
) -> (
    std::collections::HashSet<String>,
    std::collections::HashSet<(String, usize)>,
    usize,
) {
    let mut collapsed: std::collections::HashSet<String> = entries
        .iter()
        .filter_map(|e| match e {
            SessionTreeEntry::Session { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    for e in entries {
        if let SessionTreeEntry::Session {
            name,
            is_active: true,
            ..
        } = e
        {
            collapsed.remove(name);
        }
    }
    let mut collapsed_windows = std::collections::HashSet::new();
    for e in entries {
        if let SessionTreeEntry::Window {
            session_name,
            index,
            ..
        } = e
        {
            if collapsed.contains(session_name) {
                continue;
            }
            let keep_panes_visible = focus.as_ref().is_some_and(|f| {
                f.session_name == *session_name && f.window_index == *index
            });
            if !keep_panes_visible {
                collapsed_windows.insert((session_name.clone(), *index));
            }
        }
    }
    let selected = initial_session_chooser_selection(
        entries,
        &collapsed,
        &collapsed_windows,
        focus.as_ref(),
    );
    (collapsed, collapsed_windows, selected)
}

fn initial_session_chooser_selection(
    entries: &[SessionTreeEntry],
    collapsed: &std::collections::HashSet<String>,
    collapsed_windows: &std::collections::HashSet<(String, usize)>,
    focus: Option<&SessionChooserFocus>,
) -> usize {
    let visible = visible_entries_full(entries, collapsed, collapsed_windows);
    if let Some(focus) = focus {
        if let Some(pos) = visible.iter().position(|e| {
            matches!(
                e,
                SessionTreeEntry::Pane {
                    session_name,
                    window_index,
                    pane_id,
                    ..
                } if session_name == &focus.session_name
                    && *window_index == focus.window_index
                    && *pane_id == focus.pane_id
            )
        }) {
            return pos;
        }
    }
    visible
        .iter()
        .position(|e| {
            matches!(
                e,
                SessionTreeEntry::Pane {
                    is_active: true,
                    ..
                }
            )
        })
        .or_else(|| {
            visible.iter().position(|e| {
                matches!(
                    e,
                    SessionTreeEntry::Window {
                        is_active: true,
                        ..
                    }
                )
            })
        })
        .or_else(|| {
            visible.iter().position(|e| {
                matches!(
                    e,
                    SessionTreeEntry::Session {
                        is_active: true,
                        ..
                    }
                )
            })
        })
        .unwrap_or(0)
}

fn clamp_session_chooser_selected(
    selected: usize,
    visible_len: usize,
) -> usize {
    if visible_len == 0 {
        0
    } else {
        selected.min(visible_len - 1)
    }
}

fn visible_entries_full<'a>(
    entries: &'a [SessionTreeEntry],
    collapsed: &std::collections::HashSet<String>,
    collapsed_windows: &std::collections::HashSet<(String, usize)>,
) -> Vec<&'a SessionTreeEntry> {
    entries
        .iter()
        .filter(|e| match e {
            SessionTreeEntry::Session { .. } => true,
            SessionTreeEntry::Window { session_name, .. } => {
                !collapsed.contains(session_name)
            }
            SessionTreeEntry::Pane {
                session_name,
                window_index,
                ..
            } => {
                !collapsed.contains(session_name)
                    && !collapsed_windows
                        .contains(&(session_name.clone(), *window_index))
            }
        })
        .collect()
}

fn client_log_path() -> std::path::PathBuf {
    std::env::temp_dir().join("zmux_client.log")
}

fn log_client(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(client_log_path())
    {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        // Format the whole record first and emit it with a single write so
        // concurrent writers (poll thread, main loop) don't interleave bytes.
        let _ = f.write_all(format!("[{}] {}\n", ts, msg).as_bytes());
    }
}

fn panic_payload_str(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// Install a process-wide panic hook for the client. On panic it best-effort
/// restores the terminal (so the user isn't stranded in raw mode / the alt
/// screen and the message isn't swallowed) and records the location, message
/// and backtrace to the client log.
fn install_client_panic_hook() {
    use std::sync::Once;
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let mut out = io::stdout();
            let _ = terminal::disable_raw_mode();
            let _ = execute!(
                out,
                DisableBracketedPaste,
                DisableMouseCapture,
                LeaveAlternateScreen,
                cursor::Show
            );
            let location = info
                .location()
                .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
                .unwrap_or_else(|| "<unknown>".to_string());
            let msg = panic_payload_str(info.payload());
            let backtrace = std::backtrace::Backtrace::force_capture();
            log_client(&format!("PANIC at {location}: {msg}\n{backtrace}"));
            let _ = writeln!(
                out,
                "\r\nzmux client panicked at {location}: {msg}\r\n(backtrace logged to {})",
                client_log_path().display()
            );
            default_hook(info);
        }));
    });
}

#[cfg(unix)]
fn cleanup_killed_socket(socket_name: &str) {
    if let Ok(path) = crate::ipc::socket_path(socket_name) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(windows)]
fn cleanup_killed_socket(_socket_name: &str) {}

#[cfg(unix)]
fn cleanup_stale_socket(socket_name: &str, error: &io::Error) {
    use std::os::unix::fs::FileTypeExt;

    if !matches!(
        error.kind(),
        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
    ) {
        return;
    }
    let Ok(path) = crate::ipc::socket_path(socket_name) else {
        return;
    };
    let Ok(metadata) = std::fs::metadata(&path) else {
        return;
    };
    if metadata.file_type().is_socket() {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(windows)]
fn cleanup_stale_socket(_socket_name: &str, _error: &io::Error) {}

#[cfg(unix)]
fn discover_all_socket_names(socket_name: &str) -> io::Result<Vec<String>> {
    use std::collections::BTreeSet;

    let socket_path = crate::ipc::socket_path(socket_name)?;
    let Some(dir) = socket_path.parent() else {
        return Ok(vec![socket_name.to_string()]);
    };
    let tab_prefix = format!("{}.tab.", socket_name);
    let mut names = BTreeSet::new();
    if socket_path.exists() {
        names.insert(socket_name.to_string());
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string)
            else {
                continue;
            };
            if name.starts_with(&tab_prefix) {
                names.insert(name);
            }
        }
    }
    Ok(names.into_iter().collect())
}

#[cfg(windows)]
fn discover_all_socket_names(socket_name: &str) -> io::Result<Vec<String>> {
    use std::collections::BTreeSet;

    let pipe_prefix = "zmux-";
    let tab_pipe_prefix = format!("{}{}.tab.", pipe_prefix, socket_name);
    let mut names = BTreeSet::new();
    if let Ok(entries) = std::fs::read_dir(r"\\.\pipe\") {
        for entry in entries.flatten() {
            let pipe_name = entry.file_name().to_string_lossy().to_string();
            if pipe_name == format!("{}{}", pipe_prefix, socket_name)
                || pipe_name.starts_with(&tab_pipe_prefix)
            {
                if let Some(socket) = pipe_name.strip_prefix(pipe_prefix) {
                    names.insert(socket.to_string());
                }
            }
        }
    }
    Ok(names.into_iter().collect())
}

fn ensure_server_and_connect(
    socket_name: &str,
    session_name: &str,
    size: Size,
    clean: bool,
    start_dir: Option<&str>,
) -> io::Result<(SocketClient, bool)> {
    log_client(&format!(
        "ensure_server_and_connect socket='{}' session='{}' size={}x{} clean={} start_dir={:?}",
        socket_name, session_name, size.rows, size.cols, clean, start_dir
    ));

    if !clean {
        match SocketClient::connect(socket_name, size) {
            Ok(client) => {
                log_client("connected to existing server");
                return Ok((client, true));
            }
            Err(e) => {
                log_client(&format!("connect failed: {}", e));
            }
        }
    } else {
        log_client("clean requested; skipping existing server connection");
    }

    #[cfg(unix)]
    {
        if let Ok(path) = crate::ipc::socket_path(socket_name) {
            if path.exists() {
                log_client(&format!(
                    "removing stale socket: {}",
                    path.display()
                ));
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    let exe = std::env::current_exe()?;
    log_client(&format!("spawning server: {}", exe.display()));

    let child = crate::platform::spawn_server_background(
        &exe,
        socket_name,
        session_name,
        start_dir,
    );
    if let Err(e) = child {
        log_client(&format!("spawn failed: {}", e));
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("failed to spawn server ({}): {}", exe.display(), e),
        ));
    }

    for i in 0..100 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        match SocketClient::connect(socket_name, size) {
            Ok(client) => {
                log_client(&format!("connected after {}ms", (i + 1) * 50));
                return Ok((client, false));
            }
            Err(e) if i % 10 == 9 => {
                log_client(&format!(
                    "still waiting ({}ms): {}",
                    (i + 1) * 50,
                    e
                ));
            }
            _ => {}
        }
    }
    let msg = format!(
        "server did not start within 5 seconds (socket: '{}', exe: '{}')",
        socket_name,
        exe.display()
    );
    log_client(&msg);
    Err(io::Error::new(io::ErrorKind::TimedOut, msg))
}

fn detect_url_at_click(
    fd: &FrameData,
    screen_row: u16,
    screen_col: u16,
    hide_borders: bool,
) -> Option<String> {
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let layout_area = server_layout_area(cols, rows);
    let (row_texts, content_area) =
        find_active_pane_content(&fd.layout, layout_area, hide_borders);
    if screen_row < content_area.y
        || screen_row >= content_area.y + content_area.height
        || screen_col < content_area.x
        || screen_col >= content_area.x + content_area.width
    {
        return None;
    }
    let pane_row = (screen_row - content_area.y) as usize;
    let pane_col = (screen_col - content_area.x) as usize;
    let line = &row_texts.get(pane_row)?.text;
    let re = Regex::new(r#"https?://[^\s<>"'()\[\]{}|\\^`\x{FF08}\x{FF09}\x{3001}\x{3002}\x{FF0C}\x{FF1B}]+"#).ok()?;
    for m in re.find_iter(line) {
        use unicode_width::UnicodeWidthChar;
        let start_col: usize = line[..m.start()]
            .chars()
            .map(|c| c.width().unwrap_or(1))
            .sum();
        let end_col: usize = start_col
            + line[m.start()..m.end()]
                .chars()
                .map(|c| c.width().unwrap_or(1))
                .sum::<usize>();
        if pane_col >= start_col && pane_col < end_col {
            let url = m.as_str();
            let url = url.trim_end_matches(|c: char| {
                matches!(c, '.' | ',' | ';' | ':' | '!' | '?')
            });
            return Some(url.to_string());
        }
    }
    None
}

fn open_url(url: &str) {
    let _ = if cfg!(target_os = "macos") {
        std::process::Command::new("open")
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
    } else {
        std::process::Command::new("xdg-open")
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
    };
}

fn truncate_status_url(url: &str) -> String {
    const MAX_CHARS: usize = 50;
    if url.chars().count() <= MAX_CHARS {
        url.to_string()
    } else {
        let end = url
            .char_indices()
            .nth(MAX_CHARS)
            .map(|(i, _)| i)
            .unwrap_or(url.len());
        format!("{}...", &url[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_dir_command_output_is_preserved_for_new_tabs() {
        assert_eq!(
            start_dir_from_command_output("  /Users/me/project\n"),
            Some("/Users/me/project".to_string())
        );
        assert_eq!(start_dir_from_command_output(" \n\t"), None);
    }

    #[test]
    fn server_ansi_keeps_painting_while_overlay_or_prompt_is_visible() {
        assert!(should_write_server_ansi(false, false));
        assert!(
            should_write_server_ansi(true, false),
            "overlay must not freeze pane ANSI (Prefix+t / chooser)"
        );
        assert!(
            should_write_server_ansi(false, true),
            "prompt must not freeze pane ANSI"
        );
        assert!(should_write_server_ansi(true, true));
    }

    #[test]
    fn build_osc52_sequence_base64_encodes_utf8_text() {
        assert_eq!(build_osc52_sequence("hello"), "\x1b]52;c;aGVsbG8=\x07");
        assert_eq!(build_osc52_sequence("中"), "\x1b]52;c;5Lit\x07");
    }

    #[test]
    fn has_ssh_environment_detects_known_ssh_variables() {
        assert!(has_ssh_environment(|key| match key {
            "SSH_CONNECTION" => Some("1 2 3 4".to_string()),
            _ => None,
        }));
        assert!(!has_ssh_environment(|_| None));
    }

    #[test]
    fn find_pane_id_at_returns_clicked_pane() {
        let layout = LayoutJson::Split {
            direction: "horizontal".to_string(),
            sizes: vec![50, 50],
            children: vec![test_leaf(1, true), test_leaf(2, false)],
        };
        let area = ratatui::layout::Rect {
            x: 0,
            y: 0,
            width: 101,
            height: 20,
        };

        assert_eq!(find_pane_id_at(&layout, area, 10, 5, false), Some(1));
        assert_eq!(find_pane_id_at(&layout, area, 75, 5, false), Some(2));
    }

    #[test]
    fn mouse_pointer_is_text_over_active_pane_content() {
        let fd = test_frame(vec![test_row("hello", None)]);
        let (cols, rows) = (80u16, 24u16);
        let (_, content) = find_active_pane_content(
            &fd.layout,
            server_layout_area(cols, rows),
            true,
        );
        assert_eq!(
            mouse_pointer_shape_at(
                content.y, content.x, cols, rows, &fd, true, false,
            ),
            MousePointerShape::Text
        );
        assert_eq!(
            mouse_pointer_shape_at(0, 0, cols, rows, &fd, true, false),
            MousePointerShape::Default
        );
    }

    #[test]
    fn mouse_pointer_is_arrow_over_inactive_pane() {
        let fd = FrameData {
            frame_type: "frame".to_string(),
            layout: LayoutJson::Split {
                direction: "horizontal".to_string(),
                sizes: vec![50, 50],
                children: vec![
                    LayoutJson::Leaf {
                        id: 1,
                        rows: 1,
                        cols: 1,
                        cursor_row: 0,
                        cursor_col: 0,
                        hide_cursor: false,
                        alternate_screen: false,
                        mouse_mode: 0,
                        in_copy_mode: false,
                        scroll_ratio: None,
                        cursor_shape: 0,
                        active: true,
                        rows_v2: vec![test_row("left", None)],
                        title: None,
                    },
                    LayoutJson::Leaf {
                        id: 2,
                        rows: 1,
                        cols: 1,
                        cursor_row: 0,
                        cursor_col: 0,
                        hide_cursor: false,
                        alternate_screen: false,
                        mouse_mode: 0,
                        in_copy_mode: false,
                        scroll_ratio: None,
                        cursor_shape: 0,
                        active: false,
                        rows_v2: vec![test_row("right", None)],
                        title: None,
                    },
                ],
            },
            status: None,
            ansi: None,
            exit: false,
            yank_text: None,
            client_requests: Vec::new(),
        };
        let (cols, rows) = (101u16, 22u16);
        let layout_area = server_layout_area(cols, rows);
        let (_, active_content) =
            find_active_pane_content(&fd.layout, layout_area, false);
        assert_eq!(
            mouse_pointer_shape_at(
                active_content.y + 1,
                active_content.x + 1,
                cols,
                rows,
                &fd,
                false,
                false,
            ),
            MousePointerShape::Text
        );
        assert_eq!(
            mouse_pointer_shape_at(
                layout_area.y + 1,
                layout_area.x + 80,
                cols,
                rows,
                &fd,
                false,
                false,
            ),
            MousePointerShape::Default
        );
    }

    #[test]
    fn mouse_copy_supports_reverse_drag_with_exclusive_end() {
        let fd = test_frame(vec![test_row("abcdef", None)]);
        let sel = MouseSelection {
            start_col: 4,
            start_row: 0,
            end_col: 2,
            end_row: 0,
        };
        assert_eq!(extract_text_from_frame(&fd, &sel, true), "bcde");
    }

    #[test]
    fn mouse_copy_uses_exclusive_end_column() {
        let fd = test_frame(vec![test_row("apps", None)]);
        let sel = MouseSelection {
            start_col: 0,
            start_row: 0,
            end_col: 4,
            end_row: 0,
        };
        assert_eq!(extract_text_from_frame(&fd, &sel, true), "apps");
    }

    #[test]
    fn mouse_up_completes_selection_when_drag_event_was_coalesced() {
        use crossterm::event::{
            KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
        };

        let origin = MouseDragOrigin { row: 4, col: 8 };
        let mouse_up = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 73,
            row: 4,
            modifiers: KeyModifiers::empty(),
        };
        let pane_area = ratatui::layout::Rect {
            x: 1,
            y: 1,
            width: 100,
            height: 20,
        };
        let selection = finalize_mouse_selection(
            None,
            Some(origin),
            mouse_up,
            Some(pane_area),
        )
        .expect("Down -> Up movement must still produce a selection");
        assert_eq!(selection.normalized_bounds(), (4, 8, 4, 74));
    }

    #[test]
    fn mouse_copy_joins_copy_mode_wrapped_rows() {
        let fd = test_frame(vec![
            test_row("abcdef", Some(0)),
            test_row("ghij", Some(0)),
        ]);
        let sel = MouseSelection {
            start_col: 0,
            start_row: 0,
            end_col: 4,
            end_row: 1,
        };

        assert_eq!(extract_text_from_frame(&fd, &sel, true), "abcdefghij");
    }

    #[test]
    fn mouse_copy_keeps_newline_without_copy_row_metadata() {
        let fd = test_frame(vec![test_row("abc", None), test_row("def", None)]);
        let sel = MouseSelection {
            start_col: 0,
            start_row: 0,
            end_col: 3,
            end_row: 1,
        };

        assert_eq!(extract_text_from_frame(&fd, &sel, true), "abc\ndef");
    }

    #[test]
    fn mouse_copy_uses_screen_coordinates_below_tab_bar() {
        let fd =
            test_frame(vec![test_row("hello", None), test_row("world", None)]);
        let layout_area = server_layout_area(80, 24);
        let (_, content_area) =
            find_active_pane_content(&fd.layout, layout_area, true);
        let sel = MouseSelection {
            start_col: content_area.x,
            start_row: content_area.y,
            end_col: content_area.x + 5,
            end_row: content_area.y + 1,
        };
        assert_eq!(
            extract_text_from_frame_in_area(&fd, &sel, layout_area, true),
            "hello\nworld"
        );
    }

    #[test]
    fn mouse_for_pane_maps_screen_coords_to_pane_local() {
        use crossterm::event::{
            KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
        };

        let layout_area = ratatui::layout::Rect {
            x: 0,
            y: 1,
            width: 80,
            height: 22,
        };
        let fd = test_frame(vec![test_row("hello", None)]);

        let screen_col = 10u16;
        let screen_row = 5u16;
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: screen_col,
            row: screen_row,
            modifiers: KeyModifiers::empty(),
        };
        let pane_mouse =
            mouse_for_pane(mouse, &fd, layout_area, true).expect("inside pane");
        assert_eq!(pane_mouse.column, screen_col);
        assert_eq!(pane_mouse.row, screen_row - layout_area.y);

        let bytes = mouse_to_bytes(pane_mouse);
        let expected = format!(
            "\x1b[<0;{};{}M",
            pane_mouse.column + 1,
            pane_mouse.row + 1
        );
        assert_eq!(String::from_utf8(bytes).unwrap(), expected);
    }

    #[test]
    fn mouse_for_pane_with_border_subtracts_content_origin() {
        use crossterm::event::{
            KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
        };

        let layout_area = ratatui::layout::Rect {
            x: 0,
            y: 1,
            width: 80,
            height: 22,
        };
        let fd = test_frame(vec![test_row("hello", None)]);
        let (_, pa) = find_active_pane_content(&fd.layout, layout_area, false);

        let pane_col = 4u16;
        let pane_row = 2u16;
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: pa.x + pane_col,
            row: pa.y + pane_row,
            modifiers: KeyModifiers::empty(),
        };
        let mapped = mouse_for_pane(mouse, &fd, layout_area, false)
            .expect("inside pane");
        assert_eq!(mapped.column, pane_col);
        assert_eq!(mapped.row, pane_row);

        let bytes = mouse_to_bytes(mapped);
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            format!("\x1b[<0;{};{}M", pane_col + 1, pane_row + 1)
        );
    }

    #[test]
    fn mouse_for_pane_ignores_tab_row_and_outside_content() {
        use crossterm::event::{
            KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
        };

        let layout_area = ratatui::layout::Rect {
            x: 0,
            y: 1,
            width: 80,
            height: 22,
        };
        let fd = test_frame(vec![test_row("hello", None)]);
        let tab_mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 0,
            modifiers: KeyModifiers::empty(),
        };
        assert!(mouse_for_pane(tab_mouse, &fd, layout_area, false).is_none());

        let border_mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 1,
            modifiers: KeyModifiers::empty(),
        };
        assert!(mouse_for_pane(border_mouse, &fd, layout_area, false).is_none());
    }

    #[test]
    fn session_chooser_opens_on_current_pane_from_frame() {
        let entries = vec![
            SessionTreeEntry::Session {
                name: "main".to_string(),
                window_count: 2,
                is_active: true,
            },
            SessionTreeEntry::Window {
                session_name: "main".to_string(),
                index: 0,
                name: "dev".to_string(),
                pane_count: 2,
                is_active: true,
            },
            SessionTreeEntry::Pane {
                session_name: "main".to_string(),
                window_index: 0,
                pane_id: 10,
                index: 0,
                is_active: false,
            },
            SessionTreeEntry::Pane {
                session_name: "main".to_string(),
                window_index: 0,
                pane_id: 11,
                index: 1,
                is_active: true,
            },
            SessionTreeEntry::Window {
                session_name: "main".to_string(),
                index: 1,
                name: "logs".to_string(),
                pane_count: 1,
                is_active: false,
            },
            SessionTreeEntry::Pane {
                session_name: "main".to_string(),
                window_index: 1,
                pane_id: 20,
                index: 0,
                is_active: false,
            },
        ];
        let focus = SessionChooserFocus {
            session_name: "main".to_string(),
            window_index: 0,
            pane_id: 11,
        };
        let (collapsed, collapsed_windows, selected) =
            build_initial_session_chooser_state(&entries, Some(focus));
        let visible =
            visible_entries_full(&entries, &collapsed, &collapsed_windows);
        assert!(!collapsed_windows.contains(&("main".to_string(), 0)));
        assert!(collapsed_windows.contains(&("main".to_string(), 1)));
        assert!(matches!(
            visible[selected],
            SessionTreeEntry::Pane {
                pane_id: 11,
                index: 1,
                ..
            }
        ));
    }

    #[test]
    fn session_chooser_selected_clamps_after_collapse() {
        assert_eq!(clamp_session_chooser_selected(5, 3), 2);
        assert_eq!(clamp_session_chooser_selected(0, 0), 0);
    }

    fn test_frame(rows_v2: Vec<RowRunsJson>) -> FrameData {
        FrameData {
            frame_type: "frame".to_string(),
            layout: LayoutJson::Leaf {
                id: 1,
                rows: rows_v2.len() as u16,
                cols: 20,
                cursor_row: 0,
                cursor_col: 0,
                hide_cursor: false,
                alternate_screen: false,
                mouse_mode: 0,
                in_copy_mode: true,
                scroll_ratio: None,
                cursor_shape: 0,
                active: true,
                rows_v2,
                title: None,
            },
            status: None,
            ansi: None,
            exit: false,
            yank_text: None,
            client_requests: Vec::new(),
        }
    }

    fn test_row(text: &str, line: Option<usize>) -> RowRunsJson {
        RowRunsJson {
            runs: vec![CellRunJson {
                text: text.to_string(),
                fg: "default".to_string(),
                bg: "default".to_string(),
                flags: 0,
                width: text.chars().count() as u16,
            }],
            line,
            start_col: 0,
            end_col: text.chars().count(),
        }
    }

    fn test_leaf(id: usize, active: bool) -> LayoutJson {
        LayoutJson::Leaf {
            id,
            rows: 1,
            cols: 1,
            cursor_row: 0,
            cursor_col: 0,
            hide_cursor: false,
            alternate_screen: false,
            mouse_mode: 0,
            in_copy_mode: false,
            scroll_ratio: None,
            cursor_shape: 0,
            active,
            rows_v2: vec![],
            title: None,
        }
    }

    #[test]
    fn word_display_range_selects_until_whitespace() {
        assert_eq!(word_display_range_in_line("foo bar", 1), Some((0, 3)));
        assert_eq!(word_display_range_in_line("foo bar", 5), Some((4, 7)));
        assert_eq!(word_display_range_in_line("foo-bar baz", 2), Some((0, 7)));
        assert_eq!(word_display_range_in_line("a=b", 1), Some((0, 3)));
    }

    #[test]
    fn mouse_copy_reads_single_char_from_server_frame() {
        let fd = test_frame(vec![test_row("abc", None)]);
        let layout_area = server_layout_area(80, 24);
        let (_, content_area) =
            find_active_pane_content(&fd.layout, layout_area, true);
        let sel = MouseSelection {
            start_col: content_area.x + 1,
            start_row: content_area.y,
            end_col: content_area.x + 2,
            end_row: content_area.y,
        };
        assert_eq!(
            extract_text_from_frame_in_area(&fd, &sel, layout_area, true),
            "b"
        );
    }

    #[test]
    fn mouse_copy_reads_word_from_server_frame() {
        let fd = test_frame(vec![test_row("foo bar", None)]);
        let layout_area = server_layout_area(80, 24);
        let (_, content_area) =
            find_active_pane_content(&fd.layout, layout_area, true);
        let sel = word_selection_at_click(
            &fd,
            content_area.y,
            content_area.x + 1,
            true,
        )
        .expect("word selection");
        assert_eq!(
            extract_text_from_frame_in_area(&fd, &sel, layout_area, true),
            "foo"
        );
    }

    /// Integration helpers: build a real server frame and run mouse copy on it.
    mod frame_copy_pipeline {
        use std::{io, time::Duration};

        use super::*;
        use crate::{
            layout::serialize_frame,
            output::{frame_ansi_area, serialize_frame_ansi, FrameAnsiOptions},
            pty::{spawn_pane, SpawnOptions},
            types::{
                session::{Size, WindowFlags},
                LayoutNode, Pane, Rect, Window, WindowOptions,
            },
        };

        const PANE_ROWS: u16 = 8;
        const PANE_COLS: u16 = 128;

        fn terminal_size() -> (u16, u16) {
            // Tab bar + pane + status bar.
            (PANE_COLS, PANE_ROWS + 2)
        }

        fn server_size() -> Size {
            let (cols, rows) = terminal_size();
            Size::new(rows.saturating_sub(1).max(1), cols.max(1))
        }

        fn layout_area() -> Rect {
            let sz = server_size();
            Rect::new(0, 0, sz.cols.max(1), sz.rows.saturating_sub(1).max(1))
        }

        fn silent_test_pane() -> io::Result<Pane> {
            #[cfg(unix)]
            let command = Some("/bin/cat");
            #[cfg(windows)]
            let command = Some("C:\\Windows\\System32\\timeout.exe");
            spawn_pane(SpawnOptions {
                pane_id: 1,
                rows: PANE_ROWS,
                cols: PANE_COLS,
                history_limit: 2_000,
                command,
                start_dir: None,
                env: vec![],
                scroll_on_erase_in_display: false,
                zmux_socket: None,
            })
        }

        fn test_window(pane: Pane) -> Window {
            Window {
                id: 1,
                name: "test".to_string(),
                root: LayoutNode::Leaf(pane),
                active_pane_path: vec![],
                options: WindowOptions::with_defaults(),
                pane_mru: vec![1],
                zoom_state: None,
                flags: WindowFlags::default(),
                layout_index: 0,
                last_output_time: std::time::Instant::now(),
                last_seen_version: 0,
                default_start_dir: None,
            }
        }

        /// Mirror server render_loop: layout JSON first, then ANSI paint.
        fn build_frame_data(win: &Window) -> FrameData {
            let json = serialize_frame(win, layout_area(), true);
            serde_json::from_str(&json)
                .expect("serialize_frame should produce FrameData")
        }

        fn assert_ansi_paints(win: &Window, text: &str) {
            let ansi = serialize_frame_ansi(
                win,
                frame_ansi_area(server_size()),
                true,
                FrameAnsiOptions {
                    clear_display: true,
                    force_repaint: true,
                },
            );
            // Per-cell CUP inserts CSI between glyphs, so strip CSI before matching.
            let visible: String = {
                let mut out = String::new();
                let mut chars = ansi.chars().peekable();
                while let Some(ch) = chars.next() {
                    if ch == '\x1b' {
                        if chars.peek() == Some(&'[') {
                            chars.next();
                            for c in chars.by_ref() {
                                if c.is_ascii_alphabetic() || c == '~' {
                                    break;
                                }
                            }
                        }
                        continue;
                    }
                    if ch >= ' ' {
                        out.push(ch);
                    }
                }
                out
            };
            assert!(
                visible.contains(text),
                "ANSI paint should include {text:?}, visible={visible:?}, raw={ansi:?}"
            );
        }

        fn mouse_copy_first_row(fd: &FrameData, text_len: u16) -> String {
            let (term_cols, term_rows) = terminal_size();
            let layout_area = server_layout_area(term_cols, term_rows);
            let (_, content_area) =
                find_active_pane_content(&fd.layout, layout_area, true);
            let sel = MouseSelection {
                start_col: content_area.x,
                start_row: content_area.y,
                end_col: content_area.x + text_len,
                end_row: content_area.y,
            };
            extract_text_from_frame_in_area(fd, &sel, layout_area, true)
        }

        fn feed_pane(pane: &Pane, bytes: &[u8]) {
            let mut parser = pane.parser.lock().expect("parser lock");
            parser.process(bytes);
        }

        #[test]
        fn mouse_copy_reads_text_from_server_frame() {
            let pane = silent_test_pane().expect("test pane");
            feed_pane(&pane, b"plain_copy_text");
            let win = test_window(pane);
            let fd = build_frame_data(&win);
            assert_ansi_paints(&win, "plain_copy_text");
            assert_eq!(
                mouse_copy_first_row(&fd, "plain_copy_text".len() as u16),
                "plain_copy_text"
            );
        }

        #[test]
        fn mouse_copy_reads_sync_output_from_server_frame() {
            let pane = silent_test_pane().expect("test pane");
            feed_pane(&pane, b"\x1b[?2026hsync_copy_text");
            std::thread::sleep(Duration::from_millis(160));
            let win = test_window(pane);
            let fd = build_frame_data(&win);
            assert_ansi_paints(&win, "sync_copy_text");
            assert_eq!(
                mouse_copy_first_row(&fd, "sync_copy_text".len() as u16),
                "sync_copy_text",
                "rows_v2 must match ANSI after sync flush; \
                 without write_leaf flush this returns empty while the screen shows text"
            );
        }

        #[test]
        fn large_synchronized_output_is_not_painted_mid_replay() {
            let pane = silent_test_pane().expect("test pane");
            let mut partial = b"\x1b[?2026;25h".to_vec();
            partial.resize(partial.len() + 5_000, b'x');
            feed_pane(&pane, &partial);
            let win = test_window(pane);

            let intermediate = serialize_frame_ansi(
                &win,
                frame_ansi_area(server_size()),
                true,
                FrameAnsiOptions {
                    clear_display: false,
                    force_repaint: true,
                },
            );
            assert!(!intermediate.contains("xxxx"));

            let pane =
                crate::layout::active_pane(&win.root, &win.active_pane_path)
                    .unwrap();
            feed_pane(pane, b"\x1b[?2026;25l");
            assert_ansi_paints(&win, "xxxx");
        }

        #[test]
        fn mouse_copy_reads_indented_colored_git_status_row() {
            let pane = silent_test_pane().expect("test pane");
            let text =
                "both modified:   apps/box-desktop/src/rust/tenga-bridge/src/lib.rs";
            feed_pane(&pane, format!("\x1b[31m\t{text}\x1b[m").as_bytes());
            let win = test_window(pane);
            let fd = build_frame_data(&win);
            let (term_cols, term_rows) = terminal_size();
            let layout_area = server_layout_area(term_cols, term_rows);
            let (_, content_area) =
                find_active_pane_content(&fd.layout, layout_area, true);
            let sel = MouseSelection {
                start_col: content_area.x + 8,
                start_row: content_area.y,
                end_col: content_area.x + 8 + text.len() as u16,
                end_row: content_area.y,
            };
            assert_eq!(
                extract_text_from_frame_in_area(&fd, &sel, layout_area, true,),
                text
            );
        }
    }
}
