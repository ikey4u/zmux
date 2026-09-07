use std::{
    io::{self, Write},
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

mod navigation;
mod remote;
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
    pub attach_all: bool,
}

#[derive(Clone, PartialEq)]
enum InputMode {
    Normal,
    Prefix,
    Navigator,
    NavigationHelp {
        scroll: usize,
    },
    ShortcutsHelp {
        scroll: usize,
        return_mode: Box<InputMode>,
    },
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
        return_to_navigator: bool,
    },
    RenameSession {
        buf: String,
        cursor: usize,
        return_to_navigator: bool,
    },
    RenamePane {
        buf: String,
        cursor: usize,
        return_to_navigator: bool,
    },
    RenameIdentity {
        id: navigation::NavigationNodeId,
        buf: String,
        cursor: usize,
    },
    ConfirmNavigationClose {
        entry: navigation::NavigationEntry,
    },
    Command {
        buf: String,
        cursor: usize,
    },
    OptionPanel {
        selected: usize,
        scroll_on_erase_in_display: bool,
    },
}

const RESIZE_IDLE_TIMEOUT: Duration = Duration::from_millis(500);
const SCROLL_LINES: usize = 3;
const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(400);
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

struct WorkspaceConnection {
    socket_name: String,
    machine_id: String,
    client: Box<dyn DomainHandle>,
    visual_focus: VisualFocus,
    navigation_tree: Vec<SessionTreeEntry>,
    navigation_tree_at: Option<Instant>,
    navigation_refresh_until: Option<Instant>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum VisualFocus {
    Local { pane_id: Option<usize> },
}

impl WorkspaceConnection {
    fn local(socket_name: String, client: SocketClient) -> Self {
        Self {
            socket_name,
            machine_id: "local".to_string(),
            client: Box::new(client),
            visual_focus: VisualFocus::Local { pane_id: None },
            navigation_tree: Vec::new(),
            navigation_tree_at: None,
            navigation_refresh_until: None,
        }
    }
}

struct WorkspaceManager {
    sidebar_visible: bool,
    workspaces: Vec<WorkspaceConnection>,
    active: usize,
    base_socket: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemoteMachineState {
    Disconnected,
    Probing,
    Connected,
    Unavailable,
}

struct RemoteMachine {
    host: String,
    id: String,
    route: Vec<String>,
    state: RemoteMachineState,
    error: Option<String>,
    retry_attempt: u32,
    retry_at: Option<Instant>,
    activate_after_probe: bool,
}

struct RemoteRegistry {
    machines: Vec<RemoteMachine>,
    result_tx:
        std::sync::mpsc::Sender<(String, Result<(), remote::RemoteFailure>)>,
    result_rx:
        std::sync::mpsc::Receiver<(String, Result<(), remote::RemoteFailure>)>,
}

fn remote_machine_id(route: &[String]) -> String {
    let mut id = String::from("ssh:");
    for component in route {
        id.push_str(&format!("{}#{component}", component.len()));
    }
    id
}

impl RemoteRegistry {
    fn new() -> Self {
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        Self {
            machines: Vec::new(),
            result_tx,
            result_rx,
        }
    }

    fn add_and_probe(
        &mut self,
        host: &str,
        activate: bool,
    ) -> Result<String, String> {
        let host = host.trim();
        if host.is_empty() || host.starts_with('-') {
            return Err("usage: new -m <SSH_HOST>".to_string());
        }
        let route = vec![host.to_string()];
        let id = remote_machine_id(&route);
        if !self.machines.iter().any(|machine| machine.id == id) {
            self.machines.push(RemoteMachine {
                host: host.to_string(),
                id: id.clone(),
                route,
                state: RemoteMachineState::Disconnected,
                error: None,
                retry_attempt: 0,
                retry_at: None,
                activate_after_probe: false,
            });
        }
        self.start_probe(&id, activate);
        Ok(id)
    }

    fn start_probe(&mut self, alias: &str, activate: bool) -> bool {
        let Some(machine) =
            self.machines.iter_mut().find(|machine| machine.id == alias)
        else {
            return false;
        };
        if machine.state == RemoteMachineState::Probing {
            machine.activate_after_probe |= activate;
            return false;
        }
        machine.state = RemoteMachineState::Probing;
        machine.error = None;
        machine.retry_at = None;
        machine.activate_after_probe |= activate;
        let id = alias.to_string();
        let route = machine.route.clone();
        let tx = self.result_tx.clone();
        std::thread::spawn(move || {
            let result = remote::probe(&route);
            let _ = tx.send((id, result));
        });
        true
    }

    fn mark_disconnected(&mut self, alias: &str, error: String) {
        self.mark_failure(alias, remote::RemoteFailure::transient(error));
    }

    fn mark_failure(&mut self, alias: &str, error: remote::RemoteFailure) {
        if let Some(machine) =
            self.machines.iter_mut().find(|machine| machine.id == alias)
        {
            machine.state = RemoteMachineState::Unavailable;
            machine.error = Some(error.message);
            machine.retry_attempt = machine.retry_attempt.saturating_add(1);
            let backoff =
                1u64 << machine.retry_attempt.saturating_sub(1).min(5);
            machine.retry_at = error
                .retryable
                .then(|| Instant::now() + Duration::from_secs(backoff));
            machine.activate_after_probe = false;
        }
    }

    fn mark_detached(&mut self, id: &str) {
        if let Some(machine) =
            self.machines.iter_mut().find(|machine| machine.id == id)
        {
            machine.state = RemoteMachineState::Disconnected;
            machine.error = None;
            machine.retry_attempt = 0;
            machine.retry_at = None;
            machine.activate_after_probe = false;
        }
    }

    fn due_retries(&self, now: Instant) -> Vec<String> {
        self.machines
            .iter()
            .filter(|machine| {
                machine.state == RemoteMachineState::Unavailable
                    && machine.retry_at.is_some_and(|at| at <= now)
            })
            .map(|machine| machine.id.clone())
            .collect()
    }

    fn display_name(&self, id: &str) -> String {
        self.machines
            .iter()
            .find(|machine| machine.id == id)
            .map(|machine| machine.host.clone())
            .unwrap_or_else(|| id.to_string())
    }
}

enum NavigationOpenResult {
    Toggled,
    Opened,
    Probing(String),
    Failed(String),
}

fn open_navigation_entry(
    workspaces: &mut WorkspaceManager,
    remotes: &mut RemoteRegistry,
    navigation_state: &mut navigation::NavigationState,
    navigation_entries: &[navigation::NavigationEntry],
    entry: &navigation::NavigationEntry,
    size: Size,
) -> NavigationOpenResult {
    use navigation::{NavigationNodeId, NavigationNodeKind};

    if entry.kind != NavigationNodeKind::Machine {
        return if workspaces.activate_navigation_entry(entry, size) {
            NavigationOpenResult::Opened
        } else {
            NavigationOpenResult::Failed(
                "navigation target is unavailable".to_string(),
            )
        };
    }

    let NavigationNodeId::Machine(machine_id) = &entry.id else {
        return NavigationOpenResult::Failed(
            "invalid machine node".to_string(),
        );
    };
    if machine_id == "local" {
        navigation_state.toggle_selected(navigation_entries);
        return NavigationOpenResult::Toggled;
    }

    let connected = remotes
        .machines
        .iter()
        .find(|machine| machine.id == *machine_id)
        .is_some_and(|machine| machine.state == RemoteMachineState::Connected);
    if connected {
        let route = remotes
            .machines
            .iter()
            .find(|machine| machine.id == *machine_id)
            .map(|machine| machine.route.clone())
            .unwrap_or_default();
        return match workspaces
            .connect_remote_machine(machine_id, &route, size, true)
        {
            Ok(()) => NavigationOpenResult::Opened,
            Err(error) => NavigationOpenResult::Failed(format!(
                "failed to open {}: {error}",
                remotes.display_name(machine_id)
            )),
        };
    }

    let name = remotes.display_name(machine_id);
    remotes.start_probe(machine_id, true);
    NavigationOpenResult::Probing(name)
}

fn navigation_kind_label(kind: navigation::NavigationNodeKind) -> &'static str {
    match kind {
        navigation::NavigationNodeKind::Machine => "machine",
        navigation::NavigationNodeKind::Workspace => "workspace",
        navigation::NavigationNodeKind::Session => "session",
        navigation::NavigationNodeKind::Window => "window",
        navigation::NavigationNodeKind::Pane => "pane",
    }
}

fn navigation_rename_value(entry: &navigation::NavigationEntry) -> String {
    match &entry.id {
        navigation::NavigationNodeId::Session { name, .. } => name.clone(),
        navigation::NavigationNodeId::Pane { .. }
            if entry.label.starts_with("pane ") =>
        {
            String::new()
        }
        _ => entry.label.clone(),
    }
}

fn navigation_close_block_reason(
    entries: &[navigation::NavigationEntry],
    entry: &navigation::NavigationEntry,
) -> Option<String> {
    use navigation::NavigationNodeId;
    match &entry.id {
        NavigationNodeId::Machine(machine) if machine == "local" => {
            Some("the local machine cannot be closed".to_string())
        }
        NavigationNodeId::Session {
            machine, workspace, ..
        } => {
            let count = entries
                .iter()
                .filter(|candidate| {
                    matches!(
                        &candidate.id,
                        NavigationNodeId::Session {
                            machine: other_machine,
                            workspace: other_workspace,
                            ..
                        } if other_machine == machine && other_workspace == workspace
                    )
                })
                .count();
            (count <= 1).then(|| {
                "cannot close the last session; close its workspace instead"
                    .to_string()
            })
        }
        NavigationNodeId::Window {
            machine,
            workspace,
            session,
            ..
        } => {
            let count = entries
                .iter()
                .filter(|candidate| {
                    matches!(
                        &candidate.id,
                        NavigationNodeId::Window {
                            machine: other_machine,
                            workspace: other_workspace,
                            session: other_session,
                            ..
                        } if other_machine == machine
                            && other_workspace == workspace
                            && other_session == session
                    )
                })
                .count();
            (count <= 1).then(|| {
                "cannot close the last window in a session".to_string()
            })
        }
        NavigationNodeId::Pane {
            machine,
            workspace,
            session,
            window,
            ..
        } => {
            let pane_count = entries
                .iter()
                .filter(|candidate| {
                    matches!(
                        &candidate.id,
                        NavigationNodeId::Pane {
                            machine: other_machine,
                            workspace: other_workspace,
                            session: other_session,
                            window: other_window,
                            ..
                        } if other_machine == machine
                            && other_workspace == workspace
                            && other_session == session
                            && other_window == window
                    )
                })
                .count();
            let window_count = entries
                .iter()
                .filter(|candidate| {
                    matches!(
                        &candidate.id,
                        NavigationNodeId::Window {
                            machine: other_machine,
                            workspace: other_workspace,
                            session: other_session,
                            ..
                        } if other_machine == machine
                            && other_workspace == workspace
                            && other_session == session
                    )
                })
                .count();
            (pane_count <= 1 && window_count <= 1).then(|| {
                "cannot close the only pane in the last window".to_string()
            })
        }
        NavigationNodeId::Machine(_) | NavigationNodeId::Workspace { .. } => {
            None
        }
    }
}

fn close_navigation_entry(
    workspaces: &mut WorkspaceManager,
    remotes: &mut RemoteRegistry,
    entry: &navigation::NavigationEntry,
    size: Size,
) -> Result<bool, String> {
    use navigation::{NavigationNodeId, NavigationNodeKind};

    if let NavigationNodeId::Workspace { machine, workspace } = &entry.id {
        let Some(index) = workspaces.workspaces.iter().position(|connection| {
            connection.machine_id == *machine
                && connection.socket_name == *workspace
        }) else {
            return Err("workspace is unavailable".to_string());
        };
        workspaces.active = index;
        let empty = workspaces.close_active();
        if machine != "local"
            && !workspaces
                .workspaces
                .iter()
                .any(|connection| connection.machine_id == *machine)
        {
            remotes.mark_detached(machine);
        }
        return Ok(empty);
    }

    if let NavigationNodeId::Machine(machine_id) = &entry.id {
        if machine_id == "local" {
            return Err("the local machine cannot be closed".to_string());
        }
        while let Some(index) = workspaces
            .workspaces
            .iter()
            .position(|workspace| workspace.machine_id == *machine_id)
        {
            workspaces.active = index;
            if workspaces.close_active() {
                remotes.machines.retain(|machine| machine.id != *machine_id);
                return Ok(true);
            }
        }
        remotes.machines.retain(|machine| machine.id != *machine_id);
        return Ok(false);
    }

    if !workspaces.activate_navigation_entry(entry, size) {
        return Err("navigation target is unavailable".to_string());
    }
    let command = match entry.kind {
        NavigationNodeKind::Session => match &entry.id {
            NavigationNodeId::Session { name, .. } => {
                format!("kill-session -t {}", shell_quote(name))
            }
            _ => unreachable!(),
        },
        NavigationNodeKind::Window => "kill-window".to_string(),
        NavigationNodeKind::Pane => "kill-pane".to_string(),
        NavigationNodeKind::Machine | NavigationNodeKind::Workspace => {
            unreachable!()
        }
    };
    workspaces.active_client().run_command(&command);
    Ok(false)
}

impl WorkspaceManager {
    fn set_sidebar_visible(&mut self, visible: bool, cols: u16, rows: u16) {
        if self.sidebar_visible == visible {
            return;
        }
        self.sidebar_visible = visible;
        self.invalidate_visual_pane();
        self.resize_all(server_content_size(cols, rows, visible));
        self.active_client().refresh_display();
    }

    fn new(
        base_socket: &str,
        session_name: &str,
        size: Size,
        clean: bool,
        start_dir: Option<String>,
    ) -> io::Result<Self> {
        let (client, _) = ensure_server_and_connect(
            base_socket,
            session_name,
            size,
            clean,
            start_dir.as_deref(),
        )?;
        Ok(Self {
            workspaces: vec![WorkspaceConnection::local(
                base_socket.to_string(),
                client,
            )],
            sidebar_visible: false,
            active: 0,
            base_socket: base_socket.to_string(),
        })
    }

    fn from_existing_sockets(
        base_socket: &str,
        socket_names: Vec<String>,
        target_session: Option<&str>,
        size: Size,
    ) -> io::Result<Self> {
        let mut workspaces = Vec::new();
        let mut connection_error = None;
        for socket_name in socket_names {
            match SocketClient::connect(&socket_name, size) {
                Ok(client) => {
                    if let Some(target) = target_session {
                        client.run_command(&format!(
                            "switch-client -t {}",
                            shell_quote(target)
                        ));
                    }
                    workspaces
                        .push(WorkspaceConnection::local(socket_name, client));
                }
                Err(error) => {
                    cleanup_stale_socket(&socket_name, &error);
                    if !matches!(
                        error.kind(),
                        io::ErrorKind::NotFound
                            | io::ErrorKind::ConnectionRefused
                    ) {
                        connection_error = Some(error);
                    }
                }
            }
        }
        if workspaces.is_empty() {
            if let Some(error) = connection_error {
                return Err(error);
            }
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no running workspaces",
            ));
        }
        Ok(Self {
            workspaces,
            sidebar_visible: false,
            active: 0,
            base_socket: base_socket.to_string(),
        })
    }

    fn active_client(&self) -> &dyn DomainHandle {
        self.workspaces[self.active].client.as_ref()
    }

    fn focused_client(&self) -> &dyn DomainHandle {
        self.active_client()
    }

    fn display_snapshot(&mut self) -> (Option<FrameData>, u64) {
        self.workspaces[self.active].client.frame_snapshot()
    }

    fn visual_move(
        &mut self,
        dir: crate::layout::NavDir,
        hide_borders: bool,
    ) -> bool {
        let hits = self.composed_hits(hide_borders);
        let Some(current) = self.current_visual_target(&hits) else {
            return false;
        };
        if let Some(next) = visual::neighbor_in_dir(&hits, &current, dir) {
            self.apply_visual_target(next);
            return true;
        }
        // No neighbour means a focus boundary. Do not ask the server to wrap
        // focus while the client is entering the navigation sidebar.
        false
    }

    fn resize_visual(&self, dir: crate::layout::NavDir) {
        let cmd = match dir {
            crate::layout::NavDir::Left => "resize-pane -L",
            crate::layout::NavDir::Right => "resize-pane -R",
            crate::layout::NavDir::Up => "resize-pane -U",
            crate::layout::NavDir::Down => "resize-pane -D",
        };
        self.active_client().run_command(cmd);
    }

    fn composed_hits(&self, hide_borders: bool) -> Vec<visual::VisualHit> {
        let (cols, rows) = terminal::size().unwrap_or((80, 24));
        let area = server_layout_area(cols, rows, self.sidebar_visible);
        let workspace = &self.workspaces[self.active];
        let Some(fd) = workspace.client.latest_frame() else {
            return Vec::new();
        };
        visual::collect_visual_hits(&fd.layout, area, hide_borders)
    }

    fn current_visual_target(
        &self,
        hits: &[visual::VisualHit],
    ) -> Option<visual::VisualTarget> {
        let stored = match self.workspaces[self.active].visual_focus {
            VisualFocus::Local {
                pane_id: Some(pane_id),
            } => Some(visual::VisualTarget::Local { pane_id }),
            VisualFocus::Local { pane_id: None } => None,
        };
        visual::current_from_hits(stored.as_ref(), hits)
    }

    fn invalidate_visual_pane(&mut self) {
        self.workspaces[self.active].visual_focus =
            VisualFocus::Local { pane_id: None };
    }

    fn sync_navigation_to_active(
        &self,
        state: &mut navigation::NavigationState,
        entries: &[navigation::NavigationEntry],
    ) {
        use navigation::NavigationNodeId;

        let Some(workspace) = self.workspaces.get(self.active) else {
            state.sync_to_active(entries);
            return;
        };
        let pane_id = match workspace.visual_focus {
            VisualFocus::Local {
                pane_id: Some(pane_id),
            } => Some(pane_id),
            VisualFocus::Local { pane_id: None } => workspace
                .client
                .latest_frame()
                .and_then(|frame| active_pane_id(&frame.layout)),
        };
        let selected = pane_id.and_then(|pane_id| {
            entries.iter().position(|entry| {
                matches!(
                    &entry.id,
                    NavigationNodeId::Pane {
                        machine,
                        workspace: socket,
                        pane_id: entry_pane_id,
                        ..
                    } if machine == &workspace.machine_id
                        && socket == &workspace.socket_name
                        && *entry_pane_id == pane_id
                )
            })
        });
        if let Some(index) = selected {
            state.select_index(entries, index);
        } else {
            state.sync_to_active(entries);
        }
    }

    fn apply_visual_target(&mut self, target: visual::VisualTarget) {
        let workspace = &mut self.workspaces[self.active];
        match target {
            visual::VisualTarget::Local { pane_id } => {
                workspace
                    .client
                    .run_command(&format!("select-pane -t %{pane_id}"));
                workspace.visual_focus = VisualFocus::Local {
                    pane_id: Some(pane_id),
                };
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
            let visual::VisualTarget::Local { pane_id } = hit.target;
            let target = hit.target.clone();
            self.apply_visual_target(target);
            self.focused_client()
                .scroll_pane(pane_id, direction, SCROLL_LINES);
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
        let inner = visual::content_rect(hit.rect, hide_borders);
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
        self.active_client().send_input(&bytes);
        true
    }

    fn kill_visual_pane(&mut self) {
        self.active_client().run_command("kill-pane");
        self.invalidate_active_navigation_tree();
    }

    fn active_socket_name(&self) -> String {
        self.workspaces[self.active].socket_name.clone()
    }

    fn active_machine_id(&self) -> String {
        self.workspaces[self.active].machine_id.clone()
    }

    fn select_socket(&mut self, socket_name: &str) -> bool {
        if let Some(index) = self
            .workspaces
            .iter()
            .position(|workspace| workspace.socket_name == socket_name)
        {
            self.active = index;
            true
        } else {
            false
        }
    }

    fn close_active(&mut self) -> bool {
        self.workspaces[self.active].client.detach();
        self.remove_active()
    }

    fn close_dead_active(&mut self) -> bool {
        self.remove_active()
    }

    fn remove_active(&mut self) -> bool {
        self.workspaces.remove(self.active);
        if self.workspaces.is_empty() {
            return true;
        }
        if self.active >= self.workspaces.len() {
            self.active = self.workspaces.len() - 1;
        }
        false
    }

    fn remove_dead_inactive(&mut self) -> (usize, Vec<String>) {
        let mut removed = 0;
        let mut removed_machines = Vec::new();
        let mut index = 0;
        while index < self.workspaces.len() {
            let dead = index != self.active
                && self.workspaces[index]
                    .client
                    .latest_frame()
                    .as_ref()
                    .is_some_and(|frame| frame.exit);
            if dead {
                if self.workspaces[index].machine_id != "local" {
                    removed_machines
                        .push(self.workspaces[index].machine_id.clone());
                }
                self.workspaces[index].client.shutdown();
                self.workspaces.remove(index);
                removed += 1;
                if index < self.active {
                    self.active -= 1;
                }
            } else {
                index += 1;
            }
        }
        (removed, removed_machines)
    }

    fn detach_all(&self) {
        for workspace in &self.workspaces {
            workspace.client.detach();
        }
    }

    fn resize_all(&self, size: Size) {
        for workspace in &self.workspaces {
            workspace.client.resize(size);
        }
    }

    fn invalidate_active_navigation_tree(&mut self) {
        if let Some(workspace) = self.workspaces.get_mut(self.active) {
            workspace.navigation_tree_at = None;
            workspace.navigation_refresh_until =
                Some(Instant::now() + Duration::from_millis(500));
            workspace.client.refresh_session_tree();
        }
    }

    fn navigation_entries(
        &mut self,
        machine_name: &str,
        remotes: &RemoteRegistry,
        state: &navigation::NavigationState,
    ) -> Vec<navigation::NavigationEntry> {
        let now = Instant::now();
        for workspace in &mut self.workspaces {
            let fast_refresh = workspace
                .navigation_refresh_until
                .is_some_and(|until| now < until);
            let refresh_interval = Duration::from_millis(75);
            if workspace.navigation_tree_at.is_none_or(|at| {
                now.saturating_duration_since(at) >= refresh_interval
            }) {
                workspace.navigation_tree = workspace.client.session_tree();
                workspace.navigation_tree_at = Some(now);
                if fast_refresh {
                    workspace.client.refresh_session_tree();
                }
            }
            if workspace
                .navigation_refresh_until
                .is_some_and(|until| now >= until)
            {
                workspace.navigation_refresh_until = None;
            }
        }
        let local_views: Vec<navigation::WorkspaceNavigationView<'_>> = self
            .workspaces
            .iter()
            .enumerate()
            .filter(|(_, workspace)| workspace.machine_id == "local")
            .map(|(index, workspace)| navigation::WorkspaceNavigationView {
                socket_name: &workspace.socket_name,
                title: "",
                active: index == self.active,
                tree: &workspace.navigation_tree,
            })
            .collect();
        let remote_views: Vec<navigation::RemoteMachineNavigationView<'_>> =
            remotes
                .machines
                .iter()
                .map(|machine| {
                    let workspaces = self
                        .workspaces
                        .iter()
                        .enumerate()
                        .filter(|(_, workspace)| {
                            workspace.machine_id == machine.id
                        })
                        .map(|(index, workspace)| {
                            navigation::WorkspaceNavigationView {
                                socket_name: &workspace.socket_name,
                                title: &self.base_socket,
                                active: index == self.active,
                                tree: &workspace.navigation_tree,
                            }
                        })
                        .collect();
                    let state = match machine.state {
                        RemoteMachineState::Disconnected => {
                            navigation::MachineConnectionState::Disconnected
                        }
                        RemoteMachineState::Probing => {
                            navigation::MachineConnectionState::Probing
                        }
                        RemoteMachineState::Connected => {
                            navigation::MachineConnectionState::Connected
                        }
                        RemoteMachineState::Unavailable => {
                            navigation::MachineConnectionState::Unavailable
                        }
                    };
                    navigation::RemoteMachineNavigationView {
                        id: &machine.id,
                        name: &machine.host,
                        state,
                        active: self.workspaces.get(self.active).is_some_and(
                            |workspace| workspace.machine_id == machine.id,
                        ),
                        workspaces,
                    }
                })
                .collect();
        navigation::build_navigation_tree(
            "local",
            machine_name,
            &local_views,
            &remote_views,
            state,
        )
    }

    fn connect_remote_machine(
        &mut self,
        machine_id: &str,
        route: &[String],
        size: Size,
        activate: bool,
    ) -> io::Result<()> {
        if let Some(index) = self
            .workspaces
            .iter()
            .position(|workspace| workspace.machine_id == machine_id)
        {
            if activate {
                self.active = index;
            }
            self.workspaces[index].client.resize(size);
            return Ok(());
        }
        let socket_name = self.base_socket.clone();
        let client = remote::connect_remote(route, &socket_name, size)?;
        let previous_active = self.active;
        self.workspaces.push(WorkspaceConnection {
            socket_name: format!("ssh://{machine_id}/{socket_name}"),
            machine_id: machine_id.to_string(),
            client: Box::new(client),
            visual_focus: VisualFocus::Local { pane_id: None },
            navigation_tree: Vec::new(),
            navigation_tree_at: None,
            navigation_refresh_until: None,
        });
        self.active = if activate {
            self.workspaces.len() - 1
        } else {
            previous_active
        };
        Ok(())
    }

    fn activate_navigation_entry(
        &mut self,
        entry: &navigation::NavigationEntry,
        size: Size,
    ) -> bool {
        use navigation::NavigationNodeId;
        let (workspace, command) = match &entry.id {
            NavigationNodeId::Machine(_) => return false,
            NavigationNodeId::Workspace { workspace, .. } => (workspace.as_str(), None),
            NavigationNodeId::Session {
                workspace, name, ..
            } => (
                workspace.as_str(),
                Some(format!("switch-client -t {}", shell_quote(name))),
            ),
            NavigationNodeId::Window {
                workspace,
                session,
                index,
                ..
            } => (
                workspace.as_str(),
                Some(format!(
                    "switch-client -t {}; select-window -t {}",
                    shell_quote(session),
                    index
                )),
            ),
            NavigationNodeId::Pane {
                workspace,
                session,
                window,
                pane_id,
                ..
            } => (
                workspace.as_str(),
                Some(format!(
                    "switch-client -t {}; select-window -t {}; select-pane -t %{}",
                    shell_quote(session),
                    window,
                    pane_id
                )),
            ),
        };
        if !self.select_socket(workspace) {
            return false;
        }
        self.active_client().resize(size);
        if let Some(command) = command {
            self.active_client().run_command(&command);
        }
        self.invalidate_visual_pane();
        self.invalidate_active_navigation_tree();
        true
    }
}

/// A workspace switch changes which server owns the physical terminal framebuffer.
/// Inactive workspaces only retain their latest incremental ANSI frame, so that
/// cached frame cannot be replayed as a full-screen snapshot after an automatic
/// switch (for example when the active workspace is closed).
fn request_active_workspace_full_refresh(workspaces: &WorkspaceManager) {
    // REFRESH_FRAME carries a unique frame type, so SocketClient publishes it
    // even when no PTY output arrived after the workspace switch.
    workspaces.active_client().refresh_display();
}

fn floating_overlay_rect(
    mode: &InputMode,
    area: ratatui::layout::Rect,
) -> Option<ratatui::layout::Rect> {
    if area.width == 0 || area.height == 0 {
        return None;
    }
    match mode {
        InputMode::OptionPanel { .. } => Some(options_panel_rect(area)),
        InputMode::NavigationHelp { .. } => Some(navigation_help_rect(area)),
        InputMode::ShortcutsHelp { .. } => Some(shortcuts_help_rect(area)),
        _ => None,
    }
}

// Keep the tree visible while a node operation or navigation help is open.
fn retains_navigation_popup(mode: &InputMode) -> bool {
    matches!(
        mode,
        InputMode::Navigator
            | InputMode::Prefix
            | InputMode::NavigationHelp { .. }
            | InputMode::RenameIdentity { .. }
            | InputMode::ConfirmNavigationClose { .. }
            | InputMode::RenameWindow {
                return_to_navigator: true,
                ..
            }
            | InputMode::RenameSession {
                return_to_navigator: true,
                ..
            }
            | InputMode::RenamePane {
                return_to_navigator: true,
                ..
            }
    )
}

enum ClientCommandResult {
    Handled(Option<String>),
    ShowHelp,
    NotHandled,
}

impl ClientApp {
    pub fn new(
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
            attach_all: false,
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
            attach_all: true,
        }
    }

    pub fn run(&self) -> io::Result<()> {
        install_client_panic_hook();
        let sidebar_visible = false;
        let (cols, rows) = terminal::size().unwrap_or((80, 24));
        let size = server_content_size(cols, rows, sidebar_visible);
        let session_name =
            self.session_name.clone().unwrap_or_else(|| "0".to_string());

        #[cfg(unix)]
        crate::pty::remember_host_termios();

        let mut workspaces = if self.attach_all {
            let socket_names = discover_all_socket_names(&self.socket_name)?;
            match WorkspaceManager::from_existing_sockets(
                &self.socket_name,
                socket_names,
                self.session_name.as_deref(),
                size,
            ) {
                Ok(workspaces) => workspaces,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    WorkspaceManager::new(
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
            WorkspaceManager::new(
                &self.socket_name,
                &session_name,
                size,
                self.clean,
                self.start_dir.clone(),
            )?
        };
        let mut remotes = RemoteRegistry::new();
        let machine_config = crate::config::machines::config_path()?;
        let mut machine_names =
            crate::config::machines::MachineNames::load(&machine_config)?;

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
        let mut navigation_popup = false;
        let mut navigation_popup_return_mode = InputMode::Normal;
        let machine_name = local_machine_name();
        let mut navigation_state = navigation::NavigationState::default();
        let mut copy_mode_confirmed = false;
        let mut prefix_from_copy_mode = false;
        let mut prefix_from_navigator = false;
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
        let mut last_navigation_entries = Vec::new();
        let mut last_drawn_mode: Option<InputMode> = None;
        let mut last_drawn_sidebar = false;
        let mut global_prefix_pending = false;
        let mut global_prefix_origin = InputMode::Normal;
        let run_result: io::Result<()> = (|| {
            loop {
                navigation_popup &= retains_navigation_popup(&mode);
                let sidebar_visible = workspaces.sidebar_visible;
                for machine_id in remotes.due_retries(Instant::now()) {
                    remotes.start_probe(&machine_id, false);
                }
                while let Ok((alias, probe_result)) =
                    remotes.result_rx.try_recv()
                {
                    // A confirmed machine removal must not be undone by an
                    // already-running SSH probe completing afterwards.
                    if !remotes
                        .machines
                        .iter()
                        .any(|machine| machine.id == alias)
                    {
                        continue;
                    }
                    let machine_label = remotes.display_name(&alias);
                    match probe_result {
                        Ok(_) => {
                            let route = remotes
                                .machines
                                .iter()
                                .find(|machine| machine.id == alias)
                                .map(|machine| machine.route.clone())
                                .unwrap_or_default();
                            let activate = remotes
                                .machines
                                .iter()
                                .find(|machine| machine.id == alias)
                                .is_some_and(|machine| {
                                    machine.activate_after_probe
                                });
                            let (cols, rows) =
                                terminal::size().unwrap_or((80, 24));
                            match workspaces.connect_remote_machine(
                                &alias,
                                &route,
                                server_content_size(
                                    cols,
                                    rows,
                                    sidebar_visible,
                                ),
                                activate,
                            ) {
                                Ok(()) => {
                                    if let Some(machine) = remotes
                                        .machines
                                        .iter_mut()
                                        .find(|machine| machine.id == alias)
                                    {
                                        machine.state =
                                            RemoteMachineState::Connected;
                                        machine.error = None;
                                        machine.retry_attempt = 0;
                                        machine.retry_at = None;
                                        machine.activate_after_probe = false;
                                    }
                                    status_notice = Some((
                                        format!("connected to {machine_label}"),
                                        Instant::now() + Duration::from_secs(3),
                                    ));
                                    request_active_workspace_full_refresh(
                                        &workspaces,
                                    );
                                }
                                Err(error) => {
                                    remotes.mark_failure(
                                        &alias,
                                        remote::RemoteFailure::from_io(&error),
                                    );
                                    status_notice = Some((
                                        format!(
                                            "{machine_label}: attach failed: {error}"
                                        ),
                                        Instant::now() + Duration::from_secs(5),
                                    ));
                                }
                            }
                            last_drawn_counter = 0;
                        }
                        Err(error) => {
                            remotes.mark_failure(&alias, error.clone());
                            status_notice = Some((
                                format!("{machine_label}: {}", error.message),
                                Instant::now() + Duration::from_secs(5),
                            ));
                            last_drawn_counter = 0;
                        }
                    }
                }
                let (frame, current_counter) = workspaces.display_snapshot();
                let active_socket_name = workspaces.active_socket_name();
                if matches!(
                    copy_mode_sync_suppress_frame,
                    Some(counter) if counter != current_counter
                ) {
                    copy_mode_sync_suppress_frame = None;
                }
                if let Some(ref fd) = frame {
                    if fd.exit {
                        log_client("received exit frame for active workspace");
                        let disconnected_machine =
                            workspaces.active_machine_id();
                        if workspaces.close_dead_active() {
                            break;
                        }
                        if disconnected_machine != "local" {
                            remotes.mark_disconnected(
                                &disconnected_machine,
                                "SSH connection closed".to_string(),
                            );
                        }
                        mode = InputMode::Normal;
                        copy_mode_confirmed = false;
                        mouse_select = None;
                        request_active_workspace_full_refresh(&workspaces);
                        last_drawn_counter = 0;
                        status_notice = Some((
                            "workspace closed".to_string(),
                            Instant::now() + Duration::from_secs(3),
                        ));
                        continue;
                    }
                    let (removed_dead_workspaces, disconnected_machines) =
                        workspaces.remove_dead_inactive();
                    for machine_id in disconnected_machines {
                        remotes.mark_disconnected(
                            &machine_id,
                            "remote connection closed".to_string(),
                        );
                    }
                    if removed_dead_workspaces > 0 {
                        last_drawn_counter = 0;
                        status_notice = Some((
                            if removed_dead_workspaces == 1 {
                                "closed 1 dead workspace".to_string()
                            } else {
                                format!(
                                    "closed {} dead workspaces",
                                    removed_dead_workspaces
                                )
                            },
                            Instant::now() + Duration::from_secs(3),
                        ));
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
                        | InputMode::RenamePane { .. }
                        | InputMode::RenameIdentity { .. }
                        | InputMode::Command { .. }
                        | InputMode::ConfirmNavigationClose { .. }
                );
                let has_overlay = navigation_popup
                    || matches!(
                        mode,
                        InputMode::OptionPanel { .. }
                            | InputMode::NavigationHelp { .. }
                            | InputMode::ShortcutsHelp { .. }
                    );
                let hide_status = has_prompt;

                let (cols, rows) = terminal::size().unwrap_or((80, 24));
                let mut navigation_entries = workspaces.navigation_entries(
                    &machine_name,
                    &remotes,
                    &navigation_state,
                );
                for entry in &mut navigation_entries {
                    let name = match &entry.id {
                        navigation::NavigationNodeId::Machine(id) => {
                            machine_names.names.get(id).map(String::as_str)
                        }
                        navigation::NavigationNodeId::Workspace {
                            machine,
                            workspace,
                        } => machine_names.workspace_name(machine, workspace),
                        _ => None,
                    };
                    if let Some(name) = name {
                        entry.label = name.to_string();
                    }
                }
                if navigation_entries != last_navigation_entries {
                    last_drawn_counter = 0;
                }
                navigation_state.clamp_selection(&navigation_entries);
                let areas = workspace_areas(cols, rows, sidebar_visible);
                let mut drew_terminal_output = false;
                let terminal_area =
                    ratatui::layout::Rect::new(0, 0, cols, rows);
                let mut current_overlay_rect =
                    floating_overlay_rect(&mode, terminal_area);
                if navigation_popup {
                    let popup = navigation_popup_rect(terminal_area);
                    current_overlay_rect = Some(
                        current_overlay_rect
                            .map_or(popup, |rect| rect.union(popup)),
                    );
                }
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
                    workspaces.active_client().refresh_display();
                    last_drawn_counter = current_counter;
                    last_overlay_rect = current_overlay_rect;
                }

                // A visibility change moves the server-owned ANSI viewport.
                // Keep the old complete screen until a correctly sized frame
                // arrives, then paint terminal and chrome in one sync update.
                let viewport_ready = last_drawn_sidebar == sidebar_visible
                    || frame.as_ref().is_some_and(|fd| {
                        layout_fits_area(&fd.layout, areas.layout, hide_borders)
                    });
                let redraw_needed = viewport_ready
                    && (current_counter != last_drawn_counter
                        || last_drawn_mode.as_ref() != Some(&mode)
                        || last_drawn_sidebar != sidebar_visible);
                let server_frame_new = last_ansi_frame.as_ref().is_none_or(
                    |(socket_name, counter)| {
                        socket_name != &active_socket_name
                            || *counter != current_counter
                    },
                );
                if redraw_needed {
                    drew_terminal_output = true;
                    // Enclose chrome-only draws too: prompt/sidebar updates
                    // must not expose their intermediate blanking writes.
                    begin_server_ansi_update(terminal.backend_mut())?;
                    let server_ansi_update_open = true;
                    if server_frame_new {
                        if let Some(ref fd) = frame {
                            if should_write_server_ansi(has_overlay, has_prompt)
                            {
                                if let Some(ref ansi) = fd.ansi {
                                    if !ansi.trim().is_empty() {
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
                                        sidebar_visible,
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
                            skip_pane_area_for_ansi_in(
                                f,
                                fd,
                                areas.layout,
                                hide_borders,
                            );
                            let mut display_frame = fd.clone();
                            if let Some(ref message) = status_banner {
                                if let Some(status) =
                                    display_frame.status.as_mut()
                                {
                                    status.right = message.clone();
                                }
                            }
                            render_frame_in_area(
                                f,
                                &display_frame,
                                areas.frame,
                                in_prefix,
                                hide_status,
                                hide_borders,
                            );
                        } else {
                            render_loading_in_area(f, areas.frame);
                        }
                        render_navigation_sidebar(
                            f,
                            &navigation_entries,
                            navigation_state.selected,
                            mode == InputMode::Navigator,
                            areas.sidebar,
                        );
                        if navigation_popup {
                            render_navigation_popup(f, &navigation_entries, navigation_state.selected);
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
                            InputMode::RenamePane { buf, .. } => {
                                render_prompt(f, "Rename pane: ", buf)
                            }
                            InputMode::RenameIdentity {
                                id, buf, cursor,
                            } => render_prompt_input(
                                f,
                                if matches!(id, navigation::NavigationNodeId::Workspace { .. }) { "Rename workspace: " } else { "Rename machine: " },
                                buf,
                                *cursor,
                            ),
                            InputMode::Command { buf, cursor } => {
                                render_prompt_input(f, ":", buf, *cursor)
                            }
                            InputMode::ConfirmNavigationClose { entry } => {
                                render_prompt(
                                    f,
                                    &format!(
                                        "Close {} '{}' ({})? [y/N] ",
                                        navigation_kind_label(entry.kind),
                                        entry.label,
                                        match entry.kind {
                                            navigation::NavigationNodeKind::Machine => "detach connections",
                                            navigation::NavigationNodeKind::Workspace => "detach only",
                                            _ => "end processes",
                                        }
                                    ),
                                    "",
                                )
                            }
                            InputMode::OptionPanel {
                                selected,
                                scroll_on_erase_in_display,
                            } => render_options_panel(
                                f,
                                *selected,
                                *scroll_on_erase_in_display,
                            ),
                            InputMode::NavigationHelp { scroll } => render_navigation_help(f, *scroll),
                            InputMode::ShortcutsHelp { scroll, .. } => render_shortcuts_help(f, *scroll),
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

sidebar_visible,
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
                                    let layout_area = server_layout_area(
                                        cols,
                                        rows,
                                        sidebar_visible,
                                    );
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
                            let layout_area =
                                server_layout_area(cols, rows, sidebar_visible);
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
                            if has_prompt {
                                // The prompt renderer owns cursor placement.
                                terminal.show_cursor()?;
                            } else if has_overlay
                                || mode == InputMode::Navigator
                            {
                                terminal.hide_cursor()?;
                            } else {
                                let (cols, rows) =
                                    terminal::size().unwrap_or((80, 24));
                                let frame_area = server_frame_area(
                                    cols,
                                    rows,
                                    sidebar_visible,
                                );
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
                    last_navigation_entries = navigation_entries.clone();
                    last_drawn_mode = Some(mode.clone());
                    last_drawn_sidebar = sidebar_visible;
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
                    sidebar_visible,
                ) {
                    log_client(&format!("failed to set mouse pointer: {err}"));
                }

                if event::poll(Duration::from_millis(8))? {
                    match event::read()? {
                        Event::Key(key)
                            if key.kind == KeyEventKind::Press
                                || key.kind == KeyEventKind::Repeat =>
                        {
                            let sidebar_shortcut = global_prefix_pending;
                            global_prefix_pending = (key.code, key.modifiers)
                                == prefix_key
                                && !sidebar_shortcut;
                            if global_prefix_pending
                                && mode != InputMode::Prefix
                            {
                                global_prefix_origin =
                                    if mode == InputMode::Resize {
                                        InputMode::Normal
                                    } else {
                                        mode.clone()
                                    };
                            }
                            let open_popup = sidebar_shortcut
                                && key.code == KeyCode::Char('m')
                                && !key.modifiers.intersects(
                                    KeyModifiers::SHIFT
                                        | KeyModifiers::ALT
                                        | KeyModifiers::CONTROL,
                                );
                            if open_popup {
                                navigation_popup = !navigation_popup;
                                mode = if navigation_popup {
                                    navigation_popup_return_mode =
                                        global_prefix_origin.clone();
                                    workspaces.sync_navigation_to_active(
                                        &mut navigation_state,
                                        &navigation_entries,
                                    );
                                    InputMode::Navigator
                                } else {
                                    navigation_popup_return_mode.clone()
                                };
                                prefix_from_navigator = false;
                                prefix_from_copy_mode = false;
                                mouse_select = None;
                                mouse_drag_origin = None;
                                last_drawn_mouse_select = None;
                                last_drawn_counter = 0;
                                continue;
                            }
                            let toggle_sidebar =
                                sidebar_shortcut && is_shifted_letter(key, 'M');
                            if toggle_sidebar {
                                let visible = !workspaces.sidebar_visible;
                                navigation_popup = false;
                                workspaces
                                    .set_sidebar_visible(visible, cols, rows);
                                prefix_from_navigator = false;
                                prefix_from_copy_mode = false;
                                mouse_select = None;
                                mouse_drag_origin = None;
                                last_drawn_mouse_select = None;
                                mode = if !matches!(
                                    global_prefix_origin,
                                    InputMode::Navigator
                                        | InputMode::NavigationHelp { .. }
                                        | InputMode::Prefix
                                ) {
                                    global_prefix_origin.clone()
                                } else {
                                    InputMode::Normal
                                };
                                last_drawn_counter = 0;
                                continue;
                            }
                            match mode.clone() {
                                InputMode::Normal => {
                                    if (key.code, key.modifiers) == prefix_key {
                                        mode = InputMode::Prefix;
                                    } else if key.kind == KeyEventKind::Press
                                        && is_cloud_paste_key(key)
                                    {
                                        let message = workspaces
                                            .focused_client()
                                            .paste_cloud()
                                            .unwrap_or_else(|e| e);
                                        status_notice = Some((
                                            message,
                                            Instant::now()
                                                + Duration::from_secs(3),
                                        ));
                                    } else if matches!(
                                        (key.code, key.modifiers),
                                        (KeyCode::Esc, _)
                                            | (
                                                KeyCode::Char('q'),
                                                KeyModifiers::NONE,
                                            )
                                    ) && display_scrolled
                                    {
                                        workspaces
                                            .focused_client()
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
                                                workspaces.focused_client(),
                                                &mut mode,
                                                &mut copy_mode_confirmed,
                                                &mut copy_mode_exit_pending,
                                                &mut display_scrolled,
                                            );
                                        }
                                        let bytes = key_to_bytes(key);
                                        if !bytes.is_empty() {
                                            workspaces
                                                .focused_client()
                                                .send_input(&bytes);
                                        }
                                    }
                                }

                                InputMode::Prefix => {
                                    mode = InputMode::Normal;
                                    if prefix_from_navigator {
                                        prefix_from_navigator = false;
                                        if matches!(
                                            key.code,
                                            KeyCode::Char('t' | 'T' | 'S')
                                        ) {
                                            mode = InputMode::Navigator;
                                            continue;
                                        }
                                        if let Some(dir) = prefix_nav_dir(key) {
                                            mode = navigator_prefix_move(dir);
                                            continue;
                                        }
                                    }
                                    let prefix_started_from_copy_mode =
                                        prefix_from_copy_mode;
                                    prefix_from_copy_mode = false;
                                    if (key.code, key.modifiers) == prefix_key {
                                        let bytes = key_to_bytes(key);
                                        if !bytes.is_empty() {
                                            workspaces
                                                .focused_client()
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
                                        workspaces.resize_visual(dir);
                                        last_drawn_counter = 0;
                                        mode = InputMode::Resize;
                                        resize_deadline = Some(
                                            Instant::now()
                                                + RESIZE_IDLE_TIMEOUT,
                                        );
                                        continue;
                                    }
                                    match (key.code, key.modifiers) {
                                        _ if is_shifted_letter(key, 'H') => {
                                            if let Some(message) =
                                                set_workspace_home(&workspaces)
                                            {
                                                status_notice = Some((
                                                    message,
                                                    Instant::now()
                                                        + Duration::from_secs(
                                                            3,
                                                        ),
                                                ));
                                            }
                                            last_drawn_counter = 0;
                                        }
                                        (
                                            KeyCode::Char('d'),
                                            KeyModifiers::NONE,
                                        ) => {
                                            workspaces.detach_all();
                                            break;
                                        }
                                        (KeyCode::Char(','), _) => {
                                            let cur = workspaces
                                                .active_client()
                                                .active_window_name();
                                            let len = cur.len();
                                            mode = InputMode::RenameWindow {
                                                buf: cur,
                                                cursor: len,
                                                return_to_navigator: false,
                                            };
                                        }
                                        (KeyCode::Char('$'), _) => {
                                            let cur = workspaces
                                                .active_client()
                                                .session_name();
                                            let len = cur.len();
                                            mode = InputMode::RenameSession {
                                                buf: cur,
                                                cursor: len,
                                                return_to_navigator: false,
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
                                                scroll_on_erase_in_display: workspaces.active_client()
                                                    .scroll_on_erase_in_display(),
                                            };
                                        }
                                        (KeyCode::Char('['), _) => {
                                            if workspaces
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
                                        (KeyCode::Char('('), _) => {
                                            workspaces
                                                .active_client()
                                                .run_command("prev-session");
                                            workspaces.invalidate_visual_pane();
                                            workspaces.invalidate_active_navigation_tree();
                                        }
                                        (KeyCode::Char(')'), _) => {
                                            workspaces
                                                .active_client()
                                                .run_command("next-session");
                                            workspaces.invalidate_visual_pane();
                                            workspaces.invalidate_active_navigation_tree();
                                        }
                                        (
                                            KeyCode::Char('b'),
                                            KeyModifiers::NONE,
                                        ) => {
                                            hide_borders = !hide_borders;
                                            workspaces
                                                .active_client()
                                                .set_hide_borders(hide_borders);
                                        }
                                        (
                                            KeyCode::Char(']'),
                                            KeyModifiers::NONE,
                                        ) => {
                                            match workspaces
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
                                                let moved = workspaces
                                                    .visual_move(
                                                        dir,
                                                        hide_borders,
                                                    );
                                                if dir
                                                    == crate::layout::NavDir::Left
                                                    && !moved
                                                {
                                                    workspaces.sync_navigation_to_active(
                                                        &mut navigation_state,
                                                        &navigation_entries,
                                                    );
                                                    mode = InputMode::Navigator;
                                                    workspaces.set_sidebar_visible(true, cols, rows);
                                                }
                                            } else if matches!(
                                                (key.code, key.modifiers),
                                                (
                                                    KeyCode::Char('x'),
                                                    KeyModifiers::NONE
                                                )
                                            ) {
                                                workspaces.kill_visual_pane();
                                                workspaces
                                                    .invalidate_visual_pane();
                                            } else {
                                                let invalidate_focus =
                                                    matches!(
                                                    (key.code, key.modifiers),
                                                    (
                                                        KeyCode::Char('%'),
                                                        _
                                                    ) | (
                                                        KeyCode::Char('"'),
                                                        _
                                                    ) | (
                                                        KeyCode::Char('c'),
                                                        KeyModifiers::NONE
                                                    ) | (
                                                        KeyCode::Char('n'),
                                                        KeyModifiers::NONE
                                                    ) | (
                                                        KeyCode::Char('p'),
                                                        KeyModifiers::NONE
                                                    ) | (
                                                        KeyCode::Char('z'),
                                                        _
                                                    )
                                                );
                                                let invalidate_navigation = matches!(
                                                    (key.code, key.modifiers),
                                                    (KeyCode::Char('%'), _)
                                                        | (
                                                            KeyCode::Char('"'),
                                                            _
                                                        )
                                                        | (
                                                            KeyCode::Char('c'),
                                                            KeyModifiers::NONE
                                                        )
                                                        | (
                                                            KeyCode::Char('n'),
                                                            KeyModifiers::NONE
                                                        )
                                                        | (
                                                            KeyCode::Char('p'),
                                                            KeyModifiers::NONE
                                                        )
                                                );
                                                if let Some(message) =
                                                    handle_prefix_key(
                                                        workspaces
                                                            .focused_client(),
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
                                                if invalidate_focus {
                                                    workspaces
                                                        .invalidate_visual_pane(
                                                        );
                                                }
                                                if invalidate_navigation {
                                                    workspaces.invalidate_active_navigation_tree();
                                                }
                                            }
                                        }
                                    }
                                }

                                InputMode::Navigator => {
                                    if (key.code, key.modifiers) == prefix_key {
                                        prefix_from_navigator = true;
                                        mode = InputMode::Prefix;
                                        continue;
                                    }
                                    match key.code {
                                        KeyCode::Esc | KeyCode::Char('q') => {
                                            mode = if navigation_popup {
                                                navigation_popup = false;
                                                navigation_popup_return_mode
                                                    .clone()
                                            } else {
                                                workspaces.set_sidebar_visible(
                                                    false, cols, rows,
                                                );
                                                InputMode::Normal
                                            };
                                        }
                                        KeyCode::Up | KeyCode::Char('k') => {
                                            navigation_state.select_prev(
                                                &navigation_entries,
                                            );
                                        }
                                        KeyCode::Down | KeyCode::Char('j') => {
                                            navigation_state.select_next(
                                                &navigation_entries,
                                            );
                                        }
                                        KeyCode::Home | KeyCode::Char('g') => {
                                            navigation_state.select_first(
                                                &navigation_entries,
                                            );
                                        }
                                        KeyCode::End | KeyCode::Char('G') => {
                                            navigation_state.select_last(
                                                &navigation_entries,
                                            );
                                        }
                                        _ if is_shifted_letter(key, 'H') => {
                                            mode = InputMode::NavigationHelp {
                                                scroll: 0,
                                            }
                                        }
                                        KeyCode::Left | KeyCode::Char('h') => {
                                            navigation_state
                                                .collapse_or_parent(
                                                    &navigation_entries,
                                                );
                                        }
                                        KeyCode::Right | KeyCode::Char('l') => {
                                            let entry = navigation_entries
                                                .get(navigation_state.selected)
                                                .cloned();
                                            if let Some(entry) = entry {
                                                if entry.expandable {
                                                    navigation_state
                                                        .expand_or_child(
                                                            &navigation_entries,
                                                        );
                                                } else {
                                                    let (cols, rows) =
                                                        terminal::size()
                                                            .unwrap_or((
                                                                80, 24,
                                                            ));
                                                    match open_navigation_entry(
                                                        &mut workspaces,
                                                        &mut remotes,
                                                        &mut navigation_state,
                                                        &navigation_entries,
                                                        &entry,
                                                        server_content_size(
                                                            cols, rows,

sidebar_visible,
),
                                                    ) {
                                                        NavigationOpenResult::Opened => {
                                                            mode = InputMode::Normal;
                                                            copy_mode_confirmed = false;
                                                            mouse_select = None;
                                                            request_active_workspace_full_refresh(
                                                                &workspaces,
                                                            );
                                                        }
                                                        NavigationOpenResult::Probing(name) => {
                                                            status_notice = Some((
                                                                format!("probing {name}…"),
                                                                Instant::now()
                                                                    + Duration::from_secs(6),
                                                            ));
                                                        }
                                                        NavigationOpenResult::Failed(error) => {
                                                            status_notice = Some((
                                                                error,
                                                                Instant::now()
                                                                    + Duration::from_secs(4),
                                                            ));
                                                        }
                                                        NavigationOpenResult::Toggled => {}
                                                    }
                                                }
                                            }
                                        }
                                        KeyCode::Char('R') => {
                                            let selected_remote =
                                                navigation_entries
                                                    .get(
                                                        navigation_state
                                                            .selected,
                                                    )
                                                    .and_then(|entry| {
                                                        match &entry.id {
                                                            navigation::NavigationNodeId::Machine(id)
                                                                if id
                                                                    != "local" =>
                                                            {
                                                                Some(id.clone())
                                                            }
                                                            _ => None,
                                                        }
                                                    });
                                            if let Some(machine_id) =
                                                selected_remote
                                            {
                                                remotes.start_probe(
                                                    &machine_id,
                                                    false,
                                                );
                                            } else {
                                                workspaces
                                                    .active_client()
                                                    .refresh_display();
                                            }
                                        }
                                        KeyCode::Char('r') => {
                                            let entry = navigation_entries
                                                .get(navigation_state.selected)
                                                .cloned();
                                            if let Some(entry) = entry {
                                                let (cols, rows) =
                                                    terminal::size()
                                                        .unwrap_or((80, 24));
                                                let size = server_content_size(
                                                    cols,
                                                    rows,
                                                    sidebar_visible,
                                                );
                                                if matches!(entry.kind, navigation::NavigationNodeKind::Machine | navigation::NavigationNodeKind::Workspace) {
                                                        mode = InputMode::RenameIdentity {
                                                            id: entry.id.clone(),
                                                            cursor: entry.label.chars().count(),
                                                            buf: entry.label.clone(),
                                                        };
                                                } else if !workspaces
                                                    .activate_navigation_entry(&entry, size)
                                                {
                                                    status_notice = Some((
                                                        "navigation target is unavailable"
                                                            .to_string(),
                                                        Instant::now()
                                                            + Duration::from_secs(3),
                                                    ));
                                                } else {
                                                    let buf = navigation_rename_value(&entry);
                                                    let cursor = buf.chars().count();
                                                    mode = match entry.kind {
                                                        navigation::NavigationNodeKind::Session => {
                                                            InputMode::RenameSession {
                                                                buf,
                                                                cursor,
                                                                return_to_navigator: true,
                                                            }
                                                        }
                                                        navigation::NavigationNodeKind::Window => {
                                                            InputMode::RenameWindow {
                                                                buf,
                                                                cursor,
                                                                return_to_navigator: true,
                                                            }
                                                        }
                                                        navigation::NavigationNodeKind::Pane => {
                                                            InputMode::RenamePane {
                                                                buf,
                                                                cursor,
                                                                return_to_navigator: true,
                                                            }
                                                        }
                                                        navigation::NavigationNodeKind::Machine | navigation::NavigationNodeKind::Workspace => {
                                                            unreachable!()
                                                        }
                                                    };
                                                    request_active_workspace_full_refresh(&workspaces);
                                                }
                                            }
                                        }
                                        KeyCode::Char('K')
                                        | KeyCode::Char('d')
                                        | KeyCode::Delete => {
                                            if let Some(entry) =
                                                navigation_entries
                                                    .get(
                                                        navigation_state
                                                            .selected,
                                                    )
                                                    .cloned()
                                            {
                                                if let Some(reason) =
                                                    navigation_close_block_reason(
                                                        &navigation_entries,
                                                        &entry,
                                                    )
                                                {
                                                    status_notice = Some((
                                                        reason,
                                                        Instant::now()
                                                            + Duration::from_secs(4),
                                                    ));
                                                } else {
                                                    mode =
                                                        InputMode::ConfirmNavigationClose {
                                                            entry,
                                                        };
                                                }
                                            }
                                        }
                                        KeyCode::Enter => {
                                            if let Some(entry) =
                                                navigation_entries
                                                    .get(
                                                        navigation_state
                                                            .selected,
                                                    )
                                                    .cloned()
                                            {
                                                let (cols, rows) =
                                                    terminal::size()
                                                        .unwrap_or((80, 24));
                                                match open_navigation_entry(
                                                    &mut workspaces,
                                                    &mut remotes,
                                                    &mut navigation_state,
                                                    &navigation_entries,
                                                    &entry,
                                                    server_content_size(
                                                        cols, rows,

sidebar_visible,
),
                                                ) {
                                                    NavigationOpenResult::Opened => {
                                                        mode =
                                                            InputMode::Normal;
                                                        copy_mode_confirmed =
                                                            false;
                                                        mouse_select = None;
                                                        request_active_workspace_full_refresh(
                                                            &workspaces,
                                                        );
                                                    }
                                                    NavigationOpenResult::Probing(name) => {
                                                        status_notice = Some((
                                                            format!(
                                                                "probing {name}…"
                                                            ),
                                                            Instant::now()
                                                                + Duration::from_secs(6),
                                                        ));
                                                    }
                                                    NavigationOpenResult::Failed(error) => {
                                                        status_notice = Some((
                                                            error,
                                                            Instant::now()
                                                                + Duration::from_secs(4),
                                                        ));
                                                    }
                                                    NavigationOpenResult::Toggled => {}
                                                }
                                            }
                                        }
                                        _ => {}
                                    }
                                    last_drawn_counter = 0;
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
                                        workspaces.resize_visual(dir);
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

                                InputMode::NavigationHelp { mut scroll }
                                | InputMode::ShortcutsHelp {
                                    mut scroll, ..
                                } => {
                                    let return_mode = match &mode {
                                        InputMode::ShortcutsHelp {
                                            return_mode,
                                            ..
                                        } => Some(return_mode.clone()),
                                        _ => None,
                                    };
                                    let area = ratatui::layout::Rect::new(
                                        0, 0, cols, rows,
                                    );
                                    let max_scroll = if return_mode.is_some() {
                                        shortcuts_help_max_scroll(area)
                                    } else {
                                        navigation_help_max_scroll(area)
                                    };
                                    match key.code {
                                        KeyCode::Esc
                                        | KeyCode::Char('q')
                                        | KeyCode::Char('H') => {
                                            mode = return_mode
                                                .map(|previous| *previous)
                                                .unwrap_or(InputMode::Navigator)
                                        }
                                        _ => {
                                            scroll = match key.code {
                                                KeyCode::Down
                                                | KeyCode::Char('j') => {
                                                    scroll.saturating_add(1)
                                                }
                                                KeyCode::Up
                                                | KeyCode::Char('k') => {
                                                    scroll.saturating_sub(1)
                                                }
                                                KeyCode::PageDown => {
                                                    scroll.saturating_add(8)
                                                }
                                                KeyCode::PageUp => {
                                                    scroll.saturating_sub(8)
                                                }
                                                KeyCode::Home
                                                | KeyCode::Char('g') => 0,
                                                KeyCode::End
                                                | KeyCode::Char('G') => {
                                                    max_scroll
                                                }
                                                _ => scroll,
                                            }
                                            .min(max_scroll);
                                            mode = match return_mode {
                                                Some(return_mode) => {
                                                    InputMode::ShortcutsHelp {
                                                        scroll,
                                                        return_mode,
                                                    }
                                                }
                                                None => {
                                                    InputMode::NavigationHelp {
                                                        scroll,
                                                    }
                                                }
                                            };
                                        }
                                    }
                                }
                                InputMode::RenameIdentity {
                                    id,
                                    mut buf,
                                    mut cursor,
                                } => match key.code {
                                    KeyCode::Enter => {
                                        let saved = match &id {
                                            navigation::NavigationNodeId::Machine(machine) => machine_names.rename(&machine_config, machine, &buf),
                                            navigation::NavigationNodeId::Workspace { machine, workspace } => machine_names.rename_workspace(&machine_config, machine, workspace, &buf),
                                            _ => unreachable!(),
                                        };
                                        match saved {
                                            Ok(()) => {
                                                mode = InputMode::Navigator;
                                                status_notice = Some((
                                                    "display name saved"
                                                        .to_string(),
                                                    Instant::now()
                                                        + Duration::from_secs(
                                                            3,
                                                        ),
                                                ));
                                            }
                                            Err(error) => {
                                                status_notice = Some((format!("rename failed: {error}"), Instant::now() + Duration::from_secs(5)));
                                                mode = InputMode::Navigator;
                                            }
                                        }
                                    }
                                    KeyCode::Esc => mode = InputMode::Navigator,
                                    _ => {
                                        edit_prompt(&mut buf, &mut cursor, key);
                                        mode = InputMode::RenameIdentity {
                                            id,
                                            buf,
                                            cursor,
                                        };
                                    }
                                },
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
                                            match run_client_command(
                                                &mut workspaces,
                                                &mut remotes,
                                                &trimmed,
                                                server_content_size(cols, rows, sidebar_visible),
                                            ) {
                                                ClientCommandResult::ShowHelp => {
                                                    mode = InputMode::ShortcutsHelp {
                                                        scroll: 0,
                                                        return_mode: Box::new(InputMode::Normal),
                                                    };
                                                    last_drawn_counter = 0;
                                                }
                                                ClientCommandResult::Handled(message) => {
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
                                                ClientCommandResult::NotHandled => {
                                                    let changes_navigation =
                                                        command_changes_navigation(
                                                            &trimmed,
                                                        );
                                                    if let Some(message) = run_command_notice(
                                                        workspaces.focused_client(),
                                                        &trimmed,
                                                    ) {
                                                        status_notice = Some((
                                                            message,
                                                            Instant::now() + Duration::from_secs(3),
                                                        ));
                                                    }
                                                    if changes_navigation {
                                                        workspaces.invalidate_visual_pane();
                                                        workspaces.invalidate_active_navigation_tree();
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
                                        edit_prompt(&mut buf, &mut cursor, key);
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
                                                workspaces.focused_client(),
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
                                            workspaces
                                                .focused_client()
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
                                            workspaces
                                                .focused_client()
                                                .copy_move_right();
                                            mode = InputMode::CopyMode;
                                        }
                                        (
                                            KeyCode::Char('k'),
                                            KeyModifiers::NONE,
                                        )
                                        | (KeyCode::Up, KeyModifiers::NONE) => {
                                            workspaces
                                                .focused_client()
                                                .copy_move_up();
                                            mode = InputMode::CopyMode;
                                        }
                                        (
                                            KeyCode::Char('j'),
                                            KeyModifiers::NONE,
                                        )
                                        | (KeyCode::Down, KeyModifiers::NONE) =>
                                        {
                                            workspaces
                                                .focused_client()
                                                .copy_move_down();
                                            mode = InputMode::CopyMode;
                                        }
                                        (
                                            KeyCode::Char('b'),
                                            KeyModifiers::NONE,
                                        ) => {
                                            workspaces
                                                .focused_client()
                                                .copy_move_word_backward();
                                            mode = InputMode::CopyMode;
                                        }
                                        (
                                            KeyCode::Char('w'),
                                            KeyModifiers::NONE,
                                        ) => {
                                            workspaces
                                                .focused_client()
                                                .copy_move_word_forward();
                                            mode = InputMode::CopyMode;
                                        }
                                        (
                                            KeyCode::Char('e'),
                                            KeyModifiers::NONE,
                                        ) => {
                                            workspaces
                                                .focused_client()
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
                                            workspaces
                                                .focused_client()
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
                                            workspaces
                                                .focused_client()
                                                .copy_page_down();
                                            mode = InputMode::CopyMode;
                                        }
                                        (
                                            KeyCode::Char('g'),
                                            KeyModifiers::NONE,
                                        ) => {
                                            workspaces
                                                .focused_client()
                                                .copy_move_to_top();
                                            mode = InputMode::CopyMode;
                                        }
                                        (KeyCode::Char('G'), mods)
                                            if is_copy_plain_key(mods) =>
                                        {
                                            workspaces
                                                .focused_client()
                                                .copy_move_to_bottom();
                                            mode = InputMode::CopyMode;
                                        }
                                        _ if is_copy_line_start_key(key) => {
                                            workspaces
                                                .focused_client()
                                                .copy_move_to_line_start();
                                            mode = InputMode::CopyMode;
                                        }
                                        _ if is_copy_line_end_key(key) => {
                                            workspaces
                                                .focused_client()
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
                                            workspaces
                                                .focused_client()
                                                .copy_start_selection(
                                                    SelectionMode::Char,
                                                );
                                            mode = InputMode::CopyMode;
                                        }
                                        (KeyCode::Char('V'), mods)
                                            if is_copy_plain_key(mods) =>
                                        {
                                            workspaces
                                                .focused_client()
                                                .copy_start_selection(
                                                    SelectionMode::Line,
                                                );
                                            mode = InputMode::CopyMode;
                                        }
                                        (
                                            KeyCode::Char('v'),
                                            KeyModifiers::CONTROL,
                                        ) => {
                                            workspaces
                                                .focused_client()
                                                .copy_start_selection(
                                                    SelectionMode::Rect,
                                                );
                                            mode = InputMode::CopyMode;
                                        }
                                        (
                                            KeyCode::Char('n'),
                                            KeyModifiers::NONE,
                                        ) => {
                                            workspaces
                                                .focused_client()
                                                .copy_search_next();
                                            mode = InputMode::CopyMode;
                                        }
                                        (KeyCode::Char('N'), mods)
                                            if is_copy_plain_key(mods) =>
                                        {
                                            workspaces
                                                .focused_client()
                                                .copy_search_prev();
                                            mode = InputMode::CopyMode;
                                        }
                                        (
                                            KeyCode::Char('y'),
                                            KeyModifiers::NONE,
                                        )
                                        | (KeyCode::Enter, _) => {
                                            let text = workspaces
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
                                                    workspaces.focused_client(),
                                                    &mut mode,
                                                    &mut copy_mode_confirmed,
                                                    &mut copy_mode_exit_pending,
                                                    &mut display_scrolled,
                                                );
                                                let copy_result =
                                                    copy_to_clipboard(&text);
                                                status_notice = Some((
                                                    clipboard_copy_notice(
                                                        copy_result,
                                                        &text,
                                                    ),
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
                                                let found = workspaces
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
                                                workspaces.focused_client(),
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
                                        workspaces
                                            .active_client()
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

                                InputMode::ConfirmNavigationClose { entry } => {
                                    match key.code {
                                        KeyCode::Char('y')
                                        | KeyCode::Char('Y') => {
                                            let (cols, rows) = terminal::size()
                                                .unwrap_or((80, 24));
                                            match close_navigation_entry(
                                                &mut workspaces,
                                                &mut remotes,
                                                &entry,
                                                server_content_size(
                                                    cols,
                                                    rows,
                                                    sidebar_visible,
                                                ),
                                            ) {
                                                Ok(empty) => {
                                                    if empty {
                                                        break;
                                                    }
                                                    workspaces.invalidate_active_navigation_tree();
                                                    request_active_workspace_full_refresh(&workspaces);
                                                    status_notice = Some((
                                                        format!(
                                                            "closed {} '{}'",
                                                            navigation_kind_label(entry.kind),
                                                            entry.label
                                                        ),
                                                        Instant::now()
                                                            + Duration::from_secs(3),
                                                    ));
                                                }
                                                Err(error) => {
                                                    status_notice = Some((
                                                        error,
                                                        Instant::now()
                                                            + Duration::from_secs(4),
                                                    ));
                                                }
                                            }
                                            mode = InputMode::Navigator;
                                            copy_mode_confirmed = false;
                                            mouse_select = None;
                                            last_drawn_counter = 0;
                                        }
                                        KeyCode::Esc
                                        | KeyCode::Char('n')
                                        | KeyCode::Char('N')
                                        | KeyCode::Enter => {
                                            mode = InputMode::Navigator;
                                            last_drawn_counter = 0;
                                        }
                                        _ => {
                                            mode = InputMode::ConfirmNavigationClose {
                                                entry,
                                            };
                                        }
                                    }
                                }

                                InputMode::RenameWindow {
                                    mut buf,
                                    mut cursor,
                                    return_to_navigator,
                                } => match key.code {
                                    KeyCode::Enter => {
                                        if !buf.is_empty() {
                                            workspaces
                                                .active_client()
                                                .run_command(&format!(
                                                    "rename-window -- {}",
                                                    shell_quote(&buf)
                                                ));
                                        }
                                        workspaces
                                            .invalidate_active_navigation_tree(
                                            );
                                        mode = if return_to_navigator {
                                            InputMode::Navigator
                                        } else {
                                            InputMode::Normal
                                        };
                                    }
                                    KeyCode::Esc => {
                                        mode = if return_to_navigator {
                                            InputMode::Navigator
                                        } else {
                                            InputMode::Normal
                                        };
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
                                            return_to_navigator,
                                        };
                                    }
                                    KeyCode::Left => {
                                        if cursor > 0 {
                                            cursor -= 1;
                                        }
                                        mode = InputMode::RenameWindow {
                                            buf,
                                            cursor,
                                            return_to_navigator,
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
                                            return_to_navigator,
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
                                            return_to_navigator,
                                        };
                                    }
                                    _ => {
                                        mode = InputMode::RenameWindow {
                                            buf,
                                            cursor,
                                            return_to_navigator,
                                        };
                                    }
                                },

                                InputMode::RenameSession {
                                    mut buf,
                                    mut cursor,
                                    return_to_navigator,
                                } => match key.code {
                                    KeyCode::Enter => {
                                        if !buf.is_empty() {
                                            workspaces
                                                .active_client()
                                                .run_command(&format!(
                                                    "rename-session -- {}",
                                                    shell_quote(&buf)
                                                ));
                                        }
                                        workspaces
                                            .invalidate_active_navigation_tree(
                                            );
                                        mode = if return_to_navigator {
                                            InputMode::Navigator
                                        } else {
                                            InputMode::Normal
                                        };
                                    }
                                    KeyCode::Esc => {
                                        mode = if return_to_navigator {
                                            InputMode::Navigator
                                        } else {
                                            InputMode::Normal
                                        };
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
                                            return_to_navigator,
                                        };
                                    }
                                    KeyCode::Left => {
                                        if cursor > 0 {
                                            cursor -= 1;
                                        }
                                        mode = InputMode::RenameSession {
                                            buf,
                                            cursor,
                                            return_to_navigator,
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
                                            return_to_navigator,
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
                                            return_to_navigator,
                                        };
                                    }
                                    _ => {
                                        mode = InputMode::RenameSession {
                                            buf,
                                            cursor,
                                            return_to_navigator,
                                        };
                                    }
                                },

                                InputMode::RenamePane {
                                    mut buf,
                                    mut cursor,
                                    return_to_navigator,
                                } => match key.code {
                                    KeyCode::Enter => {
                                        if !buf.is_empty() {
                                            workspaces
                                                .active_client()
                                                .run_command(&format!(
                                                    "rename-pane -- {}",
                                                    shell_quote(&buf)
                                                ));
                                        }
                                        workspaces
                                            .invalidate_active_navigation_tree(
                                            );
                                        mode = if return_to_navigator {
                                            InputMode::Navigator
                                        } else {
                                            InputMode::Normal
                                        };
                                    }
                                    KeyCode::Esc => {
                                        mode = if return_to_navigator {
                                            InputMode::Navigator
                                        } else {
                                            InputMode::Normal
                                        };
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
                                        mode = InputMode::RenamePane {
                                            buf,
                                            cursor,
                                            return_to_navigator,
                                        };
                                    }
                                    KeyCode::Left => {
                                        cursor = cursor.saturating_sub(1);
                                        mode = InputMode::RenamePane {
                                            buf,
                                            cursor,
                                            return_to_navigator,
                                        };
                                    }
                                    KeyCode::Right => {
                                        cursor = (cursor + 1)
                                            .min(buf.chars().count());
                                        mode = InputMode::RenamePane {
                                            buf,
                                            cursor,
                                            return_to_navigator,
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
                                        mode = InputMode::RenamePane {
                                            buf,
                                            cursor,
                                            return_to_navigator,
                                        };
                                    }
                                    _ => {
                                        mode = InputMode::RenamePane {
                                            buf,
                                            cursor,
                                            return_to_navigator,
                                        };
                                    }
                                },
                            }
                        }
                        Event::Mouse(mouse) => {
                            let all_help =
                                matches!(mode, InputMode::ShortcutsHelp { .. });
                            if let InputMode::NavigationHelp { scroll }
                            | InputMode::ShortcutsHelp { scroll, .. } =
                                &mut mode
                            {
                                let area = ratatui::layout::Rect::new(
                                    0, 0, cols, rows,
                                );
                                let max_scroll = if all_help {
                                    shortcuts_help_max_scroll(area)
                                } else {
                                    navigation_help_max_scroll(area)
                                };
                                *scroll = match mouse.kind {
                                    event::MouseEventKind::ScrollDown => {
                                        scroll.saturating_add(3).min(max_scroll)
                                    }
                                    event::MouseEventKind::ScrollUp => {
                                        scroll.saturating_sub(3)
                                    }
                                    _ => *scroll,
                                };
                                continue;
                            }
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
                                sidebar_visible,
                            ) {
                                log_client(&format!(
                                    "failed to set mouse pointer on move: {err}"
                                ));
                            }
                            if mode == InputMode::Prefix {
                                prefix_from_copy_mode = false;
                                prefix_from_navigator = false;
                            }
                            let (cols, rows) =
                                terminal::size().unwrap_or((80, 24));
                            let areas =
                                workspace_areas(cols, rows, sidebar_visible);
                            let navigation_area = if navigation_popup {
                                navigation_popup_content_rect(
                                    ratatui::layout::Rect::new(
                                        0, 0, cols, rows,
                                    ),
                                )
                            } else {
                                areas.sidebar
                            };
                            if navigation_popup && mode != InputMode::Navigator
                            {
                                continue;
                            }
                            if navigation_area.contains(
                                ratatui::layout::Position::new(
                                    mouse.column,
                                    mouse.row,
                                ),
                            ) {
                                mouse_select = None;
                                mouse_drag_origin = None;
                                last_mouse_click = None;
                                if matches!(
                                    mouse.kind,
                                    MouseEventKind::ScrollDown
                                        | MouseEventKind::ScrollUp
                                ) {
                                    for _ in 0..3 {
                                        if mouse.kind
                                            == MouseEventKind::ScrollDown
                                        {
                                            navigation_state.select_next(
                                                &navigation_entries,
                                            );
                                        } else {
                                            navigation_state.select_prev(
                                                &navigation_entries,
                                            );
                                        }
                                    }
                                    last_drawn_counter = 0;
                                }
                                if matches!(
                                    mouse.kind,
                                    MouseEventKind::Down(MouseButton::Left)
                                ) {
                                    if let Some(index) =
                                        navigation::sidebar_entry_at(
                                            navigation_state.selected,
                                            navigation_entries.len(),
                                            navigation_area.y,
                                            navigation_area.height,
                                            mouse.row,
                                        )
                                    {
                                        let entry =
                                            navigation_entries[index].clone();
                                        navigation_state.select_index(
                                            &navigation_entries,
                                            index,
                                        );
                                        mode = InputMode::Navigator;
                                        if navigation::disclosure_hit(
                                            &entry,
                                            navigation_area.x,
                                            mouse.column,
                                        ) {
                                            navigation_state.toggle_selected(
                                                &navigation_entries,
                                            );
                                        } else {
                                            match open_navigation_entry(
                                                &mut workspaces,
                                                &mut remotes,
                                                &mut navigation_state,
                                                &navigation_entries,
                                                &entry,
                                                server_content_size(
                                                    cols, rows,

sidebar_visible,
),
                                            ) {
                                                NavigationOpenResult::Opened => {
                                                    mode = InputMode::Normal;
                                                    copy_mode_confirmed = false;
                                                    request_active_workspace_full_refresh(
                                                        &workspaces,
                                                    );
                                                }
                                                NavigationOpenResult::Probing(
                                                    name,
                                                ) => {
                                                    status_notice = Some((
                                                        format!(
                                                            "probing {name}…"
                                                        ),
                                                        Instant::now()
                                                            + Duration::from_secs(
                                                                6,
                                                            ),
                                                    ));
                                                }
                                                NavigationOpenResult::Failed(
                                                    error,
                                                ) => {
                                                    status_notice = Some((
                                                        error,
                                                        Instant::now()
                                                            + Duration::from_secs(
                                                                4,
                                                            ),
                                                    ));
                                                }
                                                NavigationOpenResult::Toggled => {}
                                            }
                                        }
                                        last_drawn_counter = 0;
                                    }
                                }
                                continue;
                            }
                            // The floating tree owns mouse input; never send
                            // clicks or wheel events to a covered terminal pane.
                            if navigation_popup {
                                if matches!(
                                    mouse.kind,
                                    MouseEventKind::Down(MouseButton::Left)
                                ) {
                                    navigation_popup = false;
                                    mode = navigation_popup_return_mode.clone();
                                    last_drawn_counter = 0;
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
                                                        areas.frame.width,
                                                        mouse
                                                            .column
                                                            .saturating_sub(
                                                                areas.frame.x,
                                                            ),
                                                    )
                                                {
                                                    workspaces
                                                        .focused_client()
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
                                        )
                                            && workspaces.focus_at(
                                                mouse.column,
                                                mouse.row,
                                                hide_borders,
                                            );
                                        if !focused_other_pane {
                                            workspaces.send_mouse_at(
                                                mouse,
                                                hide_borders,
                                            );
                                        }
                                    } else {
                                        match mouse.kind {
                                            MouseEventKind::ScrollUp => {
                                                workspaces.scroll_at(
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
                                                workspaces.scroll_at(
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
                                                        cols,
                                                        rows,
                                                        sidebar_visible,
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
                                                                sidebar_visible,
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
                                                        let _ = workspaces
                                                            .focus_at(
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
                                                                cols,
                                                                rows,
                                                                sidebar_visible,
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

sidebar_visible,
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

sidebar_visible,
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

sidebar_visible,
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

sidebar_visible,
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

sidebar_visible,
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
                                        workspaces.scroll_at(
                                            mouse,
                                            hide_borders,
                                            "up",
                                        );
                                        last_drawn_counter = 0;
                                    }
                                    MouseEventKind::ScrollDown => {
                                        workspaces.scroll_at(
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
                                            let fa = server_frame_area(
                                                cols,
                                                rows,
                                                sidebar_visible,
                                            );
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
                                                        sidebar_visible,
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
                                            } else if workspaces.focus_at(
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
                                                    cols,
                                                    rows,
                                                    sidebar_visible,
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
                                                        sidebar_visible,
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
                                                        cols,
                                                        rows,
                                                        sidebar_visible,
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
                                                                sidebar_visible,
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
                                                        sidebar_visible,
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
                                workspaces.focused_client(),
                                &mut mode,
                                text,
                            );
                        }
                        Event::Resize(new_cols, new_rows) => {
                            workspaces.resize_all(server_content_size(
                                new_cols,
                                new_rows,
                                sidebar_visible,
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

const SIDEBAR_MIN_WIDTH: u16 = 22;
const SIDEBAR_MAX_WIDTH: u16 = 32;
const MIN_TERMINAL_CONTENT_WIDTH: u16 = 20;

fn local_machine_name() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|name| !name.trim().is_empty())
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .filter(|output| output.status.success())
                .and_then(|output| String::from_utf8(output.stdout).ok())
                .map(|name| name.trim().to_string())
                .filter(|name| !name.is_empty())
        })
        .unwrap_or_else(|| "local".to_string())
}

#[derive(Clone, Copy)]
struct WorkspaceAreas {
    sidebar: ratatui::layout::Rect,
    frame: ratatui::layout::Rect,
    layout: ratatui::layout::Rect,
}

fn workspace_areas(
    cols: u16,
    rows: u16,
    sidebar_visible: bool,
) -> WorkspaceAreas {
    let preferred_sidebar_width =
        (cols / 4).clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH);
    let sidebar_width = if sidebar_visible {
        preferred_sidebar_width
            .min(cols.saturating_sub(MIN_TERMINAL_CONTENT_WIDTH))
    } else {
        0
    };
    let sidebar = ratatui::layout::Rect::new(0, 0, sidebar_width, rows);
    let frame = ratatui::layout::Rect::new(
        sidebar_width,
        0,
        cols.saturating_sub(sidebar_width).max(1),
        rows.max(1),
    );
    let layout = ratatui::layout::Rect {
        height: frame.height.saturating_sub(1),
        ..frame
    };
    WorkspaceAreas {
        sidebar,
        frame,
        layout,
    }
}

fn server_content_size(cols: u16, rows: u16, sidebar_visible: bool) -> Size {
    let areas = workspace_areas(cols, rows, sidebar_visible);
    Size::viewport(
        areas.frame.height,
        areas.frame.width,
        areas.frame.x,
        areas.frame.y,
    )
}

fn server_frame_area(
    cols: u16,
    rows: u16,
    sidebar_visible: bool,
) -> ratatui::layout::Rect {
    workspace_areas(cols, rows, sidebar_visible).frame
}

fn server_frame_area_from(
    area: ratatui::layout::Rect,

    sidebar_visible: bool,
) -> ratatui::layout::Rect {
    let mut frame =
        workspace_areas(area.width, area.height, sidebar_visible).frame;
    frame.x = frame.x.saturating_add(area.x);
    frame.y = frame.y.saturating_add(area.y);
    frame
}

fn server_layout_area(
    cols: u16,
    rows: u16,
    sidebar_visible: bool,
) -> ratatui::layout::Rect {
    workspace_areas(cols, rows, sidebar_visible).layout
}

fn layout_fits_area(
    layout: &LayoutJson,
    area: ratatui::layout::Rect,
    hide_borders: bool,
) -> bool {
    let geometry = layout_geometry_fingerprint(layout);
    visual::collect_visual_hits(layout, area, hide_borders)
        .iter()
        .all(|hit| {
            let visual::VisualTarget::Local { pane_id } = hit.target;
            let content = visual::content_rect(hit.rect, hide_borders);
            geometry.iter().any(|(id, rows, cols)| {
                *id == pane_id
                    && *rows == content.height
                    && *cols == content.width
            })
        })
}

#[cfg_attr(not(test), allow(dead_code))]
fn mouse_for_pane(
    mut mouse: MouseEvent,
    fd: &FrameData,
    layout_area: ratatui::layout::Rect,
    hide_borders: bool,
) -> Option<MouseEvent> {
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
    sidebar_visible: bool,
) -> Option<MouseSelection> {
    let is_double = last.as_ref().is_some_and(|prev| {
        prev.row == row
            && prev.col == col
            && prev.at.elapsed() <= DOUBLE_CLICK_INTERVAL
    });
    if is_double {
        last.take();
        word_selection_at_click(fd, row, col, hide_borders, sidebar_visible)
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

    sidebar_visible: bool,
) -> Option<(Vec<PaneContentRow>, ratatui::layout::Rect, usize, usize)> {
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let layout_area = server_layout_area(cols, rows, sidebar_visible);
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

    sidebar_visible: bool,
) {
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let text = extract_text_from_frame_in_area(
        fd,
        sel,
        server_layout_area(cols, rows, sidebar_visible),
        hide_borders,
    );
    copy_text_and_notify(&text, status_notice, last_drawn_counter);
}

fn word_selection_at_click(
    fd: &FrameData,
    screen_row: u16,
    screen_col: u16,
    hide_borders: bool,

    sidebar_visible: bool,
) -> Option<MouseSelection> {
    let (row_texts, content_area, pane_row, pane_col) = pane_coords_at_screen(
        fd,
        screen_row,
        screen_col,
        hide_borders,
        sidebar_visible,
    )?;
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
        clipboard_copy_notice(result, text),
        Instant::now() + Duration::from_secs(3),
    ));
    *last_drawn_counter = 0;
}

fn mouse_selection_from_drag(
    origin: MouseDragOrigin,
    mouse: MouseEvent,
    fd: &FrameData,
    hide_borders: bool,

    sidebar_visible: bool,
) -> MouseSelection {
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let fa = server_frame_area(cols, rows, sidebar_visible);
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

    sidebar_visible: bool,
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
        server_frame_area_from(frame_area, sidebar_visible),
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
    }
}

fn pane_tree_has_active(layout: &LayoutJson) -> bool {
    match layout {
        LayoutJson::Leaf { active: true, .. } => true,
        LayoutJson::Leaf { .. } => false,
        LayoutJson::Split { children, .. } => {
            children.iter().any(pane_tree_has_active)
        }
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
    sidebar_visible: bool,
) -> io::Result<()> {
    let desired = desired_mouse_pointer_shape(
        last_mouse_pos,
        frame,
        cols,
        rows,
        hide_borders,
        hide_status,
        ui_overlay_active,
        sidebar_visible,
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

    sidebar_visible: bool,
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
        sidebar_visible,
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

    sidebar_visible: bool,
) -> MousePointerShape {
    if let Some(status_row) = status_bar_screen_row(rows, hide_status) {
        if row == status_row {
            return MousePointerShape::Default;
        }
    }
    let layout_area = server_layout_area(cols, rows, sidebar_visible);
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
    // Holding Ctrl after Ctrl-A is common; accept both Ctrl-H and plain H.
    if key
        .modifiers
        .intersects(KeyModifiers::ALT | KeyModifiers::SUPER)
    {
        return None;
    }
    match (key.code, key.modifiers & !KeyModifiers::CONTROL) {
        (KeyCode::Backspace, _) => Some(crate::layout::NavDir::Left),
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

fn navigator_prefix_move(dir: crate::layout::NavDir) -> InputMode {
    match dir {
        crate::layout::NavDir::Right => return InputMode::Normal,
        crate::layout::NavDir::Up
        | crate::layout::NavDir::Down
        | crate::layout::NavDir::Left => {}
    }
    InputMode::Navigator
}

fn edit_prompt(buf: &mut String, cursor: &mut usize, key: KeyEvent) {
    *cursor = (*cursor).min(buf.chars().count());
    match key.code {
        KeyCode::Home => *cursor = 0,
        KeyCode::End => *cursor = buf.chars().count(),
        KeyCode::Left => *cursor = cursor.saturating_sub(1),
        KeyCode::Right => *cursor = (*cursor + 1).min(buf.chars().count()),
        KeyCode::Backspace if *cursor > 0 => {
            let start = char_byte_pos(buf, *cursor - 1);
            let end = char_byte_pos(buf, *cursor);
            buf.drain(start..end);
            *cursor -= 1;
        }
        KeyCode::Delete if *cursor < buf.chars().count() => {
            let start = char_byte_pos(buf, *cursor);
            let end = char_byte_pos(buf, *cursor + 1);
            buf.drain(start..end);
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            let end = char_byte_pos(buf, *cursor);
            buf.drain(..end);
            *cursor = 0;
        }
        KeyCode::Char(ch)
            if !key.modifiers.intersects(
                KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
            ) && !ch.is_control() =>
        {
            let at = char_byte_pos(buf, *cursor);
            buf.insert(at, ch);
            *cursor += 1;
        }
        _ => {}
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

fn run_client_command(
    workspaces: &mut WorkspaceManager,
    remotes: &mut RemoteRegistry,
    raw: &str,
    _size: Size,
) -> ClientCommandResult {
    let mut parsed = ParsedCommand::parse(raw);
    if parsed.len() != 1 {
        return ClientCommandResult::NotHandled;
    }
    let cmd = parsed.remove(0);
    match cmd.name.as_str() {
        "h" | "help" => {
            if cmd.args.is_empty() && cmd.flags.is_empty() {
                ClientCommandResult::ShowHelp
            } else {
                ClientCommandResult::Handled(Some(
                    "usage: h or help".to_string(),
                ))
            }
        }
        "set-workspace-home" => {
            ClientCommandResult::Handled(set_workspace_home(workspaces))
        }
        "new"
            if cmd.flags.contains_key("t") && !cmd.flags.contains_key("m") =>
        {
            ClientCommandResult::Handled(Some(
                "tab creation was removed; use new -s <session> or new-window"
                    .to_string(),
            ))
        }
        "new" if cmd.flags.contains_key("m") => {
            let Some(host) = cmd.flag_value("m") else {
                return ClientCommandResult::Handled(Some(
                    "usage: new -m <SSH_HOST>".to_string(),
                ));
            };
            match remotes.add_and_probe(host, true) {
                Ok(_) => ClientCommandResult::Handled(Some(format!(
                    "connecting to {host}"
                ))),
                Err(error) => ClientCommandResult::Handled(Some(error)),
            }
        }
        "paste-cloud" => match workspaces.focused_client().paste_cloud() {
            Ok(message) => ClientCommandResult::Handled(Some(message)),
            Err(e) => ClientCommandResult::Handled(Some(e)),
        },
        _ => ClientCommandResult::NotHandled,
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

fn command_changes_navigation(command: &str) -> bool {
    ParsedCommand::parse(command).iter().any(|part| {
        matches!(
            part.name.as_str(),
            "split-window"
                | "splitw"
                | "new"
                | "new-session"
                | "new-window"
                | "neww"
                | "zoom-pane"
                | "zoomp"
                | "kill-pane"
                | "killp"
                | "kill-window"
                | "killw"
                | "kill-session"
                | "kill-s"
                | "rename-pane"
                | "renamep"
                | "rename-window"
                | "renamew"
                | "rename-session"
                | "rename-s"
                | "select-pane"
                | "selectp"
                | "select-window"
                | "selectw"
                | "switch-client"
                | "switchc"
                | "prev-session"
                | "next-session"
        )
    })
}

fn set_workspace_home(workspaces: &WorkspaceManager) -> Option<String> {
    let output = workspaces
        .active_client()
        .run_command_with_output("set-workspace-home");
    let Some(path) = start_dir_from_command_output(&output) else {
        return Some("set workspace home failed".to_string());
    };
    Some(format!("workspace home: {}", path))
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
        InputMode::Navigator => Some("NAV"),
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
    Zsync,
    System,
    Osc52,
    Unavailable,
}

fn clipboard_copy_notice(result: ClipboardCopyResult, text: &str) -> String {
    let n = text.chars().count();
    match result {
        ClipboardCopyResult::Zsync => {
            format!("copied {n} chars via zsync")
        }
        ClipboardCopyResult::System => format!("copied {n} chars"),
        ClipboardCopyResult::Osc52 => {
            format!("sent {n} chars via OSC 52")
        }
        ClipboardCopyResult::Unavailable => {
            format!("yanked {n} chars (clipboard unavailable)")
        }
    }
}

fn copy_to_clipboard(text: &str) -> ClipboardCopyResult {
    if text.is_empty() {
        return ClipboardCopyResult::Unavailable;
    }
    if crate::domain::clip::copy_via_zsync(text) {
        return ClipboardCopyResult::Zsync;
    }
    if copy_to_clipboard_via_arboard(text) {
        return ClipboardCopyResult::System;
    }
    if copy_to_clipboard_via_osc52(text).is_ok() {
        return ClipboardCopyResult::Osc52;
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

fn is_cloud_paste_key(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('v') | KeyCode::Char('V'))
        && key.modifiers.contains(KeyModifiers::SUPER)
        && !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
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
            return_to_navigator,
        } => {
            insert_text_at_cursor(&mut buf, &mut cursor, &text);
            *mode = InputMode::RenameWindow {
                buf,
                cursor,
                return_to_navigator,
            };
        }
        InputMode::RenameSession {
            mut buf,
            mut cursor,
            return_to_navigator,
        } => {
            insert_text_at_cursor(&mut buf, &mut cursor, &text);
            *mode = InputMode::RenameSession {
                buf,
                cursor,
                return_to_navigator,
            };
        }
        InputMode::RenamePane {
            mut buf,
            mut cursor,
            return_to_navigator,
        } => {
            insert_text_at_cursor(&mut buf, &mut cursor, &text);
            *mode = InputMode::RenamePane {
                buf,
                cursor,
                return_to_navigator,
            };
        }
        InputMode::RenameIdentity {
            id,
            mut buf,
            mut cursor,
        } => {
            insert_text_at_cursor(&mut buf, &mut cursor, &text);
            *mode = InputMode::RenameIdentity { id, buf, cursor };
        }
        InputMode::Command {
            mut buf,
            mut cursor,
        } => {
            insert_text_at_cursor(&mut buf, &mut cursor, &text);
            *mode = InputMode::Command { buf, cursor };
        }
        InputMode::Navigator
        | InputMode::NavigationHelp { .. }
        | InputMode::ShortcutsHelp { .. }
        | InputMode::CopyMode
        | InputMode::OptionPanel { .. }
        | InputMode::ConfirmNavigationClose { .. } => {}
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
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
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
    use std::os::unix::fs::FileTypeExt;

    let socket_path = crate::ipc::socket_path(socket_name)?;
    let Some(dir) = socket_path.parent() else {
        return Ok(vec![socket_name.to_string()]);
    };
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
            if entry.file_type().is_ok_and(|kind| kind.is_socket()) {
                names.insert(name);
            }
        }
    }
    let mut names: Vec<_> = names.into_iter().collect();
    names.sort_by_key(|name| name != socket_name);
    Ok(names)
}

#[cfg(windows)]
fn discover_all_socket_names(socket_name: &str) -> io::Result<Vec<String>> {
    use std::collections::BTreeSet;

    let pipe_prefix = "zmux-";
    let mut names = BTreeSet::new();
    if let Ok(entries) = std::fs::read_dir(r"\\.\pipe\") {
        for entry in entries.flatten() {
            let pipe_name = entry.file_name().to_string_lossy().to_string();
            if let Some(socket) = pipe_name.strip_prefix(pipe_prefix) {
                names.insert(socket.to_string());
            }
        }
    }
    let mut names: Vec<_> = names.into_iter().collect();
    names.sort_by_key(|name| name != socket_name);
    Ok(names)
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
                if !matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) {
                    return Err(e);
                }
            }
        }
    } else {
        // --clean is not permission to unlink a live, incompatible server.
        match crate::ipc::connect_client(socket_name) {
            Ok(stream) => {
                crate::ipc::negotiate_client(stream)?;
                return Err(io::Error::new(io::ErrorKind::AddrInUse, "--clean requires an unused socket; choose a new -L name or explicitly stop the existing server after saving sessions"));
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) => {}
            Err(error) => return Err(error),
        }
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
            Err(e) if crate::ipc::is_compatibility_error(&e) => return Err(e),
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

    sidebar_visible: bool,
) -> Option<String> {
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let layout_area = server_layout_area(cols, rows, sidebar_visible);
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
    fn workspace_viewport_reserves_left_sidebar_without_top_bar() {
        let areas = workspace_areas(120, 24, true);
        assert_eq!(areas.sidebar, ratatui::layout::Rect::new(0, 0, 30, 24));
        assert_eq!(areas.frame, ratatui::layout::Rect::new(30, 0, 90, 24));
        assert_eq!(areas.layout, ratatui::layout::Rect::new(30, 0, 90, 23));
        let size = server_content_size(120, 24, true);
        assert_eq!((size.rows, size.cols, size.x, size.y), (24, 90, 30, 0));

        assert_eq!(workspace_areas(80, 24, true).sidebar.width, 22);
        assert_eq!(workspace_areas(200, 24, true).sidebar.width, 32);
        assert_eq!(workspace_areas(30, 24, true).sidebar.width, 10);
        assert_eq!(workspace_areas(30, 24, true).frame.width, 20);
        let hidden = workspace_areas(120, 24, false);
        assert_eq!(hidden.sidebar.width, 0);
        assert_eq!(hidden.frame, ratatui::layout::Rect::new(0, 0, 120, 24));
        let size = server_content_size(120, 24, false);
        assert_eq!((size.rows, size.cols, size.x, size.y), (24, 120, 0, 0));
    }

    #[test]
    fn command_quoting_keeps_rename_text_in_one_command() {
        let name = r#"editor; kill-window \ "notes""#;
        let commands = ParsedCommand::parse(&format!(
            "rename-window -- {}",
            shell_quote(name)
        ));
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name, "rename-window");
        assert_eq!(commands[0].args, [name]);
    }

    #[test]
    fn floating_navigation_survives_node_prompts_but_closes_on_activation() {
        assert!(retains_navigation_popup(&InputMode::Navigator));
        assert!(retains_navigation_popup(&InputMode::Prefix));
        assert!(retains_navigation_popup(&InputMode::NavigationHelp {
            scroll: 0
        }));
        assert!(retains_navigation_popup(&InputMode::RenamePane {
            buf: String::new(),
            cursor: 0,
            return_to_navigator: true,
        }));
        assert!(!retains_navigation_popup(&InputMode::Normal));
        assert!(!retains_navigation_popup(&InputMode::Command {
            buf: String::new(),
            cursor: 0
        }));
        assert!(!retains_navigation_popup(&InputMode::RenamePane {
            buf: String::new(),
            cursor: 0,
            return_to_navigator: false,
        }));
    }

    #[test]
    fn navigation_refresh_tracks_structural_command_chains() {
        assert!(command_changes_navigation(
            "switch-client -t main; select-pane -t %2"
        ));
        assert!(command_changes_navigation("split-window -h"));
        assert!(command_changes_navigation("new -s work"));
        assert!(command_changes_navigation("splitw -h"));
        assert!(!command_changes_navigation("clear-pane"));
    }

    #[test]
    fn remote_machine_ids_do_not_collide_with_alias_path_characters() {
        assert_ne!(
            remote_machine_id(&["a/b".to_string()]),
            remote_machine_id(&["a".to_string(), "b".to_string()])
        );
        assert_ne!(
            remote_machine_id(&["local".to_string()]),
            "local".to_string()
        );
    }

    #[test]
    fn remote_registry_starts_empty_until_user_adds_a_machine() {
        assert!(RemoteRegistry::new().machines.is_empty());
    }

    #[test]
    fn probing_machine_remembers_activation_and_disconnect_schedules_retry() {
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let id = remote_machine_id(&["prod".to_string()]);
        let mut remotes = RemoteRegistry {
            machines: vec![RemoteMachine {
                host: "prod".to_string(),
                id: id.clone(),
                route: vec!["prod".to_string()],
                state: RemoteMachineState::Probing,
                error: None,
                retry_attempt: 0,
                retry_at: None,
                activate_after_probe: false,
            }],
            result_tx,
            result_rx,
        };
        assert!(!remotes.start_probe(&id, true));
        assert!(remotes.machines[0].activate_after_probe);
        remotes.mark_disconnected(&id, "closed".to_string());
        assert_eq!(remotes.machines[0].retry_attempt, 1);
        assert!(remotes.machines[0].retry_at.is_some());
        remotes.mark_failure(
            &id,
            remote::RemoteFailure::permanent("protocol mismatch"),
        );
        assert!(remotes.machines[0].retry_at.is_none());
        assert!(remotes
            .due_retries(Instant::now() + Duration::from_secs(120))
            .is_empty());
        assert_eq!(
            remotes.machines[0].error.as_deref(),
            Some("protocol mismatch")
        );
        remotes.mark_disconnected(&id, "network down".to_string());
        assert!(!remotes
            .due_retries(Instant::now() + Duration::from_secs(120))
            .is_empty());
    }

    #[test]
    fn start_dir_command_output_is_preserved_for_future_splits() {
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
            "overlay must not freeze pane ANSI (navigation / help)"
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
    fn clipboard_copy_notice_labels_zsync() {
        assert_eq!(
            clipboard_copy_notice(ClipboardCopyResult::Zsync, "ab"),
            "copied 2 chars via zsync"
        );
        assert_eq!(
            clipboard_copy_notice(ClipboardCopyResult::System, "ab"),
            "copied 2 chars"
        );
        assert_eq!(
            clipboard_copy_notice(ClipboardCopyResult::Osc52, "ab"),
            "sent 2 chars via OSC 52"
        );
    }

    #[test]
    fn command_v_is_reserved_for_synced_clipboard_paste() {
        assert!(is_cloud_paste_key(KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::SUPER,
        )));
        assert!(is_cloud_paste_key(KeyEvent::new(
            KeyCode::Char('V'),
            KeyModifiers::SUPER | KeyModifiers::SHIFT,
        )));
        assert!(!is_cloud_paste_key(KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::CONTROL,
        )));
        assert!(!is_cloud_paste_key(KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::NONE,
        )));
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
            server_layout_area(cols, rows, true),
            true,
        );
        assert_eq!(
            mouse_pointer_shape_at(
                content.y, content.x, cols, rows, &fd, true, false, true,
            ),
            MousePointerShape::Text
        );
        assert_eq!(
            mouse_pointer_shape_at(0, 0, cols, rows, &fd, true, false, true),
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
        };
        let (cols, rows) = (101u16, 22u16);
        let layout_area = server_layout_area(cols, rows, true);
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
                true,
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
                true,
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
    fn mouse_copy_uses_screen_coordinates_beside_sidebar() {
        let fd =
            test_frame(vec![test_row("hello", None), test_row("world", None)]);
        let layout_area = server_layout_area(80, 24, true);
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
            x: 30,
            y: 0,
            width: 50,
            height: 23,
        };
        let fd = test_frame(vec![test_row("hello", None)]);

        let screen_col = 40u16;
        let screen_row = 5u16;
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: screen_col,
            row: screen_row,
            modifiers: KeyModifiers::empty(),
        };
        let pane_mouse =
            mouse_for_pane(mouse, &fd, layout_area, true).expect("inside pane");
        assert_eq!(pane_mouse.column, screen_col - layout_area.x);
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
            x: 30,
            y: 0,
            width: 50,
            height: 23,
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
    fn mouse_for_pane_ignores_sidebar_and_border() {
        use crossterm::event::{
            KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
        };

        let layout_area = ratatui::layout::Rect {
            x: 30,
            y: 0,
            width: 50,
            height: 23,
        };
        let fd = test_frame(vec![test_row("hello", None)]);
        let sidebar_mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 0,
            modifiers: KeyModifiers::empty(),
        };
        assert!(
            mouse_for_pane(sidebar_mouse, &fd, layout_area, false).is_none()
        );

        let border_mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 30,
            row: 0,
            modifiers: KeyModifiers::empty(),
        };
        assert!(mouse_for_pane(border_mouse, &fd, layout_area, false).is_none());
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
        let layout_area = server_layout_area(80, 24, true);
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
        let layout_area = server_layout_area(80, 24, true);
        let (_, content_area) =
            find_active_pane_content(&fd.layout, layout_area, true);
        let sel = word_selection_at_click(
            &fd,
            content_area.y,
            content_area.x + 1,
            true,
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
            // Sidebar + pane viewport + status bar.
            (PANE_COLS + SIDEBAR_MAX_WIDTH, PANE_ROWS + 1)
        }

        fn server_size() -> Size {
            let (cols, rows) = terminal_size();
            server_content_size(cols, rows, true)
        }

        fn layout_area() -> Rect {
            frame_ansi_area(server_size())
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
            let layout_area = server_layout_area(term_cols, term_rows, true);
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
            let layout_area = server_layout_area(term_cols, term_rows, true);
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
