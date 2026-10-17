use std::collections::HashSet;

use crate::server::SessionTreeEntry;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum NavigationNodeId {
    Machine(String),
    Workspace {
        machine: String,
        workspace: String,
    },
    Session {
        machine: String,
        workspace: String,
        name: String,
    },
    Window {
        machine: String,
        workspace: String,
        session: String,
        index: usize,
    },
    Pane {
        machine: String,
        workspace: String,
        session: String,
        window: usize,
        pane_id: usize,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavigationNodeKind {
    Machine,
    Workspace,
    Session,
    Window,
    Pane,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NavigationEntry {
    pub id: NavigationNodeId,
    pub kind: NavigationNodeKind,
    pub depth: u16,
    pub label: String,
    pub meta: Option<String>,
    pub connection: Option<MachineConnectionState>,
    pub active: bool,
    pub expandable: bool,
    pub expanded: bool,
}

pub struct WorkspaceNavigationView<'a> {
    pub socket_name: &'a str,
    pub title: &'a str,
    pub active: bool,
    pub tree: &'a [SessionTreeEntry],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MachineConnectionState {
    Local,
    Disconnected,
    Probing,
    Connected,
    Unavailable,
}

pub struct RemoteMachineNavigationView<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub state: MachineConnectionState,
    pub active: bool,
    pub workspaces: Vec<WorkspaceNavigationView<'a>>,
}

#[derive(Default)]
pub struct NavigationState {
    pub selected: usize,
    selected_id: Option<NavigationNodeId>,
    collapsed: HashSet<NavigationNodeId>,
}

impl NavigationState {
    pub fn clamp_selection(&mut self, entries: &[NavigationEntry]) {
        if let Some(id) = &self.selected_id {
            if let Some(index) =
                entries.iter().position(|entry| &entry.id == id)
            {
                self.selected = index;
            }
        }
        self.selected = self.selected.min(entries.len().saturating_sub(1));
        self.remember_selection(entries);
    }

    pub fn select_next(&mut self, entries: &[NavigationEntry]) {
        if !entries.is_empty() {
            self.selected = (self.selected + 1).min(entries.len() - 1);
            self.remember_selection(entries);
        }
    }

    pub fn select_prev(&mut self, entries: &[NavigationEntry]) {
        self.selected = self.selected.saturating_sub(1);
        self.remember_selection(entries);
    }

    pub fn select_first(&mut self, entries: &[NavigationEntry]) {
        self.selected = 0;
        self.remember_selection(entries);
    }

    pub fn select_last(&mut self, entries: &[NavigationEntry]) {
        self.selected = entries.len().saturating_sub(1);
        self.remember_selection(entries);
    }

    pub fn select_index(&mut self, entries: &[NavigationEntry], index: usize) {
        self.selected = index.min(entries.len().saturating_sub(1));
        self.remember_selection(entries);
    }

    pub fn toggle_selected(&mut self, entries: &[NavigationEntry]) {
        let Some(entry) = entries.get(self.selected) else {
            return;
        };
        if !entry.expandable {
            return;
        }
        if !self.collapsed.remove(&entry.id) {
            self.collapsed.insert(entry.id.clone());
        }
    }

    pub fn expand_or_child(&mut self, entries: &[NavigationEntry]) {
        let Some(entry) = entries.get(self.selected) else {
            return;
        };
        if entry.expandable && !entry.expanded {
            self.collapsed.remove(&entry.id);
            return;
        }
        if entries
            .get(self.selected + 1)
            .is_some_and(|next| next.depth > entry.depth)
        {
            self.selected += 1;
            self.remember_selection(entries);
        }
    }

    pub fn collapse_or_parent(&mut self, entries: &[NavigationEntry]) {
        let Some(entry) = entries.get(self.selected) else {
            return;
        };
        if entry.expandable && entry.expanded {
            self.collapsed.insert(entry.id.clone());
            return;
        }
        let depth = entry.depth;
        if depth == 0 {
            return;
        }
        if let Some(parent) = entries[..self.selected]
            .iter()
            .rposition(|candidate| candidate.depth < depth)
        {
            self.selected = parent;
            self.remember_selection(entries);
        }
    }

    pub fn sync_to_active(&mut self, entries: &[NavigationEntry]) {
        if let Some(index) = entries.iter().rposition(|entry| entry.active) {
            self.selected = index;
            self.remember_selection(entries);
        }
    }

    fn remember_selection(&mut self, entries: &[NavigationEntry]) {
        self.selected_id =
            entries.get(self.selected).map(|entry| entry.id.clone());
    }
}

pub fn disclosure_hit(
    entry: &NavigationEntry,
    origin_x: u16,
    column: u16,
) -> bool {
    if !entry.expandable {
        return false;
    }
    let start = origin_x
        .saturating_add(1)
        .saturating_add(entry.depth.saturating_mul(2));
    column >= start && column < start.saturating_add(2)
}

pub fn sidebar_entry_at(
    selected: usize,
    entry_count: usize,
    area_y: u16,
    area_height: u16,
    row: u16,
) -> Option<usize> {
    let (content_offset, view_height) = sidebar_viewport(area_height);
    let content_y = area_y.saturating_add(content_offset);
    if row < content_y || row >= content_y.saturating_add(view_height) {
        return None;
    }
    let view_height = view_height as usize;
    if view_height == 0 {
        return None;
    }
    let scroll = sidebar_scroll_offset(selected, entry_count, view_height);
    let index = scroll + row.saturating_sub(content_y) as usize;
    (index < entry_count).then_some(index)
}

/// Keep sidebar geometry in one place so rendering and mouse hit-testing never
/// drift apart. Nodes start at the first row; two footer rows are kept
/// for contextual shortcuts whenever the terminal is tall enough.
pub fn sidebar_viewport(area_height: u16) -> (u16, u16) {
    let footer = if area_height >= 7 { 2 } else { 0 };
    (0, area_height.saturating_sub(footer))
}

/// Scroll with a small amount of context above and below the selection instead
/// of pinning the selected node to the bottom edge of the tree.
pub fn sidebar_scroll_offset(
    selected: usize,
    entry_count: usize,
    view_height: usize,
) -> usize {
    if view_height == 0 || entry_count <= view_height {
        return 0;
    }
    let context = (view_height / 3).max(1);
    selected
        .saturating_sub(context)
        .min(entry_count.saturating_sub(view_height))
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn build_navigation_entries(
    machine_id: &str,
    machine_name: &str,
    workspaces: &[WorkspaceNavigationView<'_>],
    state: &NavigationState,
) -> Vec<NavigationEntry> {
    build_navigation_tree(machine_id, machine_name, workspaces, &[], state)
}

pub fn build_navigation_tree(
    machine_id: &str,
    machine_name: &str,
    workspaces: &[WorkspaceNavigationView<'_>],
    remotes: &[RemoteMachineNavigationView<'_>],
    state: &NavigationState,
) -> Vec<NavigationEntry> {
    let mut entries = Vec::new();
    let mut visited = HashSet::new();
    let local_active = !remotes.iter().any(|remote| remote.active);
    append_machine(
        &mut entries,
        machine_id,
        machine_name,
        MachineConnectionState::Local,
        local_active,
        workspaces,
        state,
        0,
        &mut visited,
    );
    for remote in remotes {
        if !visited.contains(remote.id) {
            append_machine(
                &mut entries,
                remote.id,
                remote.name,
                remote.state,
                remote.active,
                &remote.workspaces,
                state,
                0,
                &mut visited,
            );
        }
    }
    entries
}

#[allow(clippy::too_many_arguments)]
fn append_machine(
    entries: &mut Vec<NavigationEntry>,
    machine_id: &str,
    machine_name: &str,
    connection: MachineConnectionState,
    active: bool,
    workspaces: &[WorkspaceNavigationView<'_>],
    state: &NavigationState,
    depth: u16,
    visited: &mut HashSet<String>,
) {
    if !visited.insert(machine_id.to_string()) {
        return;
    }
    let machine_id = NavigationNodeId::Machine(machine_id.to_string());
    let machine_expanded = !state.collapsed.contains(&machine_id);
    let id_text = match &machine_id {
        NavigationNodeId::Machine(id) => id.clone(),
        _ => unreachable!(),
    };
    entries.push(NavigationEntry {
        id: machine_id,
        kind: NavigationNodeKind::Machine,
        depth,
        label: machine_name.to_string(),
        meta: None,
        connection: Some(connection),
        active,
        expandable: !workspaces.is_empty(),
        expanded: machine_expanded,
    });
    if !machine_expanded {
        return;
    }

    for workspace in workspaces {
        let id = NavigationNodeId::Workspace {
            machine: id_text.clone(),
            workspace: workspace.socket_name.to_string(),
        };
        let expanded = !state.collapsed.contains(&id);
        entries.push(NavigationEntry {
            id,
            kind: NavigationNodeKind::Workspace,
            depth: depth + 1,
            label: if workspace.title.trim().is_empty() {
                workspace.socket_name.to_string()
            } else {
                workspace.title.to_string()
            },
            meta: None,
            connection: None,
            active: workspace.active,
            expandable: true,
            expanded,
        });
        if expanded {
            append_session_tree(entries, &id_text, workspace, state, depth + 2);
        }
    }
}

fn append_session_tree(
    entries: &mut Vec<NavigationEntry>,
    machine: &str,
    workspace: &WorkspaceNavigationView<'_>,
    state: &NavigationState,
    depth: u16,
) {
    for node in workspace.tree {
        match node {
            SessionTreeEntry::Session {
                name, is_active, ..
            } => {
                let id = NavigationNodeId::Session {
                    machine: machine.to_string(),
                    workspace: workspace.socket_name.to_string(),
                    name: name.clone(),
                };
                entries.push(NavigationEntry {
                    expanded: !state.collapsed.contains(&id),
                    id,
                    kind: NavigationNodeKind::Session,
                    depth,
                    label: name.clone(),
                    meta: None,
                    connection: None,
                    active: workspace.active && *is_active,
                    expandable: true,
                });
            }
            SessionTreeEntry::Window {
                session_name,
                index,
                name,
                is_active,
                ..
            } => {
                let session_id = NavigationNodeId::Session {
                    machine: machine.to_string(),
                    workspace: workspace.socket_name.to_string(),
                    name: session_name.clone(),
                };
                if state.collapsed.contains(&session_id) {
                    continue;
                }
                let id = NavigationNodeId::Window {
                    machine: machine.to_string(),
                    workspace: workspace.socket_name.to_string(),
                    session: session_name.clone(),
                    index: *index,
                };
                entries.push(NavigationEntry {
                    expanded: !state.collapsed.contains(&id),
                    id,
                    kind: NavigationNodeKind::Window,
                    depth: depth + 1,
                    label: name.clone(),
                    meta: None,
                    connection: None,
                    active: workspace.active && *is_active,
                    expandable: true,
                });
            }
            SessionTreeEntry::Pane {
                session_name,
                window_index,
                pane_id,
                index,
                title,
                is_active,
            } => {
                let session_id = NavigationNodeId::Session {
                    machine: machine.to_string(),
                    workspace: workspace.socket_name.to_string(),
                    name: session_name.clone(),
                };
                let window_id = NavigationNodeId::Window {
                    machine: machine.to_string(),
                    workspace: workspace.socket_name.to_string(),
                    session: session_name.clone(),
                    index: *window_index,
                };
                if state.collapsed.contains(&session_id)
                    || state.collapsed.contains(&window_id)
                {
                    continue;
                }
                entries.push(NavigationEntry {
                    id: NavigationNodeId::Pane {
                        machine: machine.to_string(),
                        workspace: workspace.socket_name.to_string(),
                        session: session_name.clone(),
                        window: *window_index,
                        pane_id: *pane_id,
                    },
                    kind: NavigationNodeKind::Pane,
                    depth: depth + 2,
                    label: if title.trim().is_empty() {
                        format!("pane {index}")
                    } else {
                        title.clone()
                    },
                    // Pane ids remain part of the stable node identity, but
                    // showing `%1`/`%2` as right-aligned metadata looks like
                    // mojibake and adds no useful navigation context.
                    meta: None,
                    connection: None,
                    active: workspace.active && *is_active,
                    expandable: false,
                    expanded: false,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_machine_workspace_session_window_pane_hierarchy() {
        let tree = vec![
            SessionTreeEntry::Session {
                name: "main".into(),
                window_count: 1,
                is_active: true,
            },
            SessionTreeEntry::Window {
                session_name: "main".into(),
                index: 0,
                name: "editor".into(),
                pane_count: 1,
                is_active: true,
            },
            SessionTreeEntry::Pane {
                session_name: "main".into(),
                window_index: 0,
                pane_id: 7,
                index: 0,
                title: String::new(),
                is_active: true,
            },
        ];
        let state = NavigationState::default();
        let entries = build_navigation_entries(
            "local",
            "my-host",
            &[WorkspaceNavigationView {
                socket_name: "default",
                title: "project",
                active: true,
                tree: &tree,
            }],
            &state,
        );
        assert_eq!(entries.len(), 5);
        assert_eq!(
            entries.iter().map(|entry| entry.depth).collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4]
        );
        assert_eq!(entries[1].kind, NavigationNodeKind::Workspace);
        assert_eq!(entries[1].label, "project");
        assert_eq!(entries[2].label, "main");
        assert!(entries.iter().all(|entry| entry.meta.is_none()));
    }

    #[test]
    fn workspaces_are_independent_branches_even_with_identical_session_names() {
        let tree = vec![SessionTreeEntry::Session {
            name: "main".into(),
            window_count: 0,
            is_active: true,
        }];
        let workspaces = [
            WorkspaceNavigationView {
                socket_name: "one",
                title: "Project",
                active: true,
                tree: &tree,
            },
            WorkspaceNavigationView {
                socket_name: "two",
                title: "Project",
                active: false,
                tree: &tree,
            },
        ];
        let mut state = NavigationState::default();
        let entries =
            build_navigation_entries("local", "host", &workspaces, &state);
        assert_eq!(entries.len(), 5);
        assert_ne!(entries[1].id, entries[3].id);
        assert_ne!(entries[2].id, entries[4].id);
        state.select_index(&entries, 2);
        state.select_prev(&entries);
        assert_eq!(state.selected, 1);
        state.toggle_selected(&entries);
        let collapsed =
            build_navigation_entries("local", "host", &workspaces, &state);
        assert_eq!(collapsed.len(), 4);
        assert_eq!(collapsed[1].kind, NavigationNodeKind::Workspace);
        assert_eq!(collapsed[2].id, entries[3].id);
        assert_eq!(collapsed[3].id, entries[4].id);
        state.select_next(&collapsed);
        assert_eq!(state.selected, 2);
    }

    #[test]
    fn workspace_is_visible_before_its_session_query_completes() {
        let entries = build_navigation_entries(
            "local",
            "host",
            &[WorkspaceNavigationView {
                socket_name: "loading",
                title: "",
                active: true,
                tree: &[],
            }],
            &NavigationState::default(),
        );
        assert_eq!(entries.len(), 2);
        assert!(entries[0].expandable);
        assert_eq!(entries[1].kind, NavigationNodeKind::Workspace);
        assert_eq!(entries[1].label, "loading");
    }

    #[test]
    fn collapsing_session_hides_its_descendants() {
        let tree = vec![
            SessionTreeEntry::Session {
                name: "main".into(),
                window_count: 1,
                is_active: true,
            },
            SessionTreeEntry::Window {
                session_name: "main".into(),
                index: 0,
                name: "editor".into(),
                pane_count: 0,
                is_active: true,
            },
        ];
        let workspace = WorkspaceNavigationView {
            socket_name: "default",
            title: "",
            active: true,
            tree: &tree,
        };
        let mut state = NavigationState::default();
        let entries =
            build_navigation_entries("local", "host", &[workspace], &state);
        state.selected = 2;
        state.toggle_selected(&entries);
        let workspace = WorkspaceNavigationView {
            socket_name: "default",
            title: "",
            active: true,
            tree: &tree,
        };
        let entries =
            build_navigation_entries("local", "host", &[workspace], &state);
        assert_eq!(entries.len(), 3);
    }

    #[test]
    fn remote_machines_are_peer_roots_after_local_machine() {
        let state = NavigationState::default();
        let remotes = vec![
            RemoteMachineNavigationView {
                id: "prod",
                name: "prod",
                state: MachineConnectionState::Connected,
                active: false,
                workspaces: Vec::new(),
            },
            RemoteMachineNavigationView {
                id: "gpu",
                name: "gpu",
                state: MachineConnectionState::Disconnected,
                active: false,
                workspaces: Vec::new(),
            },
        ];
        let entries =
            build_navigation_tree("local", "host", &[], &remotes, &state);
        assert_eq!(
            entries
                .iter()
                .map(|entry| (entry.label.clone(), entry.depth))
                .collect::<Vec<_>>(),
            vec![("host".into(), 0), ("prod".into(), 0), ("gpu".into(), 0),]
        );
    }

    #[test]
    fn identical_remote_and_local_nodes_have_distinct_ids() {
        let tree = vec![SessionTreeEntry::Session {
            name: "main".into(),
            window_count: 0,
            is_active: true,
        }];
        let local = [WorkspaceNavigationView {
            socket_name: "default",
            title: "",
            active: true,
            tree: &tree,
        }];
        let remotes = [RemoteMachineNavigationView {
            id: "prod",
            name: "prod",
            state: MachineConnectionState::Connected,
            active: false,
            workspaces: vec![WorkspaceNavigationView {
                socket_name: "default",
                title: "",
                active: false,
                tree: &tree,
            }],
        }];
        let entries = build_navigation_tree(
            "local",
            "host",
            &local,
            &remotes,
            &NavigationState::default(),
        );
        let session_ids = entries
            .iter()
            .filter(|entry| entry.kind == NavigationNodeKind::Session)
            .map(|entry| entry.id.clone())
            .collect::<Vec<_>>();
        assert_eq!(session_ids.len(), 2);
        assert_ne!(session_ids[0], session_ids[1]);
    }

    #[test]
    fn selection_follows_node_identity_when_entries_are_inserted() {
        let original = vec![SessionTreeEntry::Session {
            name: "second".into(),
            window_count: 0,
            is_active: true,
        }];
        let mut state = NavigationState::default();
        let entries = build_navigation_entries(
            "local",
            "host",
            &[WorkspaceNavigationView {
                socket_name: "default",
                title: "",
                active: true,
                tree: &original,
            }],
            &state,
        );
        state.select_index(&entries, 2);

        let updated = vec![
            SessionTreeEntry::Session {
                name: "first".into(),
                window_count: 0,
                is_active: false,
            },
            original[0].clone(),
        ];
        let entries = build_navigation_entries(
            "local",
            "host",
            &[WorkspaceNavigationView {
                socket_name: "default",
                title: "",
                active: true,
                tree: &updated,
            }],
            &state,
        );
        state.clamp_selection(&entries);
        assert_eq!(entries[state.selected].label, "second");
    }

    #[test]
    fn disclosure_hit_only_matches_expand_icon_columns() {
        let entry = NavigationEntry {
            id: NavigationNodeId::Machine("local".into()),
            kind: NavigationNodeKind::Machine,
            depth: 2,
            label: "host".into(),
            meta: None,
            connection: Some(MachineConnectionState::Local),
            active: true,
            expandable: true,
            expanded: true,
        };
        assert!(disclosure_hit(&entry, 0, 5));
        assert!(disclosure_hit(&entry, 0, 6));
        assert!(!disclosure_hit(&entry, 0, 4));
        assert!(!disclosure_hit(&entry, 0, 7));
    }

    #[test]
    fn sidebar_mouse_rows_start_at_top_and_follow_scroll() {
        assert_eq!(sidebar_entry_at(0, 10, 0, 5, 0), Some(0));
        assert_eq!(sidebar_entry_at(0, 10, 0, 5, 1), Some(1));
        assert_eq!(sidebar_entry_at(0, 10, 0, 5, 2), Some(2));
        assert_eq!(sidebar_entry_at(7, 10, 0, 5, 2), Some(7));
        assert_eq!(sidebar_entry_at(7, 10, 0, 5, 4), Some(9));
        assert_eq!(sidebar_entry_at(7, 10, 0, 5, 5), None);
        assert_eq!(sidebar_entry_at(0, 10, 3, 8, 2), None);
        assert_eq!(sidebar_entry_at(0, 10, 3, 8, 3), Some(0));
        assert_eq!(sidebar_entry_at(0, 10, 3, 8, 9), None); // footer
        assert_eq!(sidebar_entry_at(0, 1, 0, 1, 0), Some(0));
    }

    #[test]
    fn sidebar_scroll_keeps_context_around_selection() {
        assert_eq!(sidebar_scroll_offset(0, 20, 9), 0);
        assert_eq!(sidebar_scroll_offset(7, 20, 9), 4);
        assert_eq!(sidebar_scroll_offset(19, 20, 9), 11);
    }
}
