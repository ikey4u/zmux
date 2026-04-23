use std::time::Instant;

use crossterm::event::{KeyCode, KeyModifiers};

use super::history::PaneTextSnapshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMode {
    Char,
    Line,
    Rect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyPoint {
    pub line: usize,
    pub col: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchMatch {
    pub line: usize,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone)]
pub struct WrappedRow {
    pub line: usize,
    pub start_col: usize,
    pub end_col: usize,
    pub text: String,
}

#[derive(Debug, Clone, Default)]
pub struct WrappedSnapshot {
    pub width: usize,
    pub rows: Vec<WrappedRow>,
    pub line_ranges: Vec<(usize, usize)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopySnapshotSource {
    Buffer,
    Parser,
}

#[derive(Clone)]
pub struct CopyModeState {
    pub snapshot: PaneTextSnapshot,
    pub snapshot_source: CopySnapshotSource,
    pub scroll_top: usize,
    pub cursor: CopyPoint,
    pub anchor: Option<CopyPoint>,
    pub selection_mode: SelectionMode,
    pub search_query: String,
    pub search_forward: bool,
    pub search_matches: Vec<SearchMatch>,
    pub search_idx: usize,
    pub preferred_column: usize,
    pub wrapped: WrappedSnapshot,
}

impl CopyModeState {
    pub fn new(snapshot: PaneTextSnapshot) -> Self {
        Self::new_with_source(snapshot, CopySnapshotSource::Buffer)
    }

    pub fn new_with_source(
        snapshot: PaneTextSnapshot,
        snapshot_source: CopySnapshotSource,
    ) -> Self {
        let cursor = CopyPoint {
            line: snapshot.cursor_line,
            col: snapshot.cursor_col,
        };
        Self {
            snapshot,
            snapshot_source,
            scroll_top: 0,
            cursor,
            anchor: None,
            selection_mode: SelectionMode::Char,
            search_query: String::new(),
            search_forward: true,
            search_matches: Vec::new(),
            search_idx: 0,
            preferred_column: 0,
            wrapped: WrappedSnapshot::default(),
        }
    }
}

pub enum Mode {
    Passthrough,
    Prefix { armed_at: Instant },
    CommandPrompt { input: String, cursor: usize },
    CopyMode,
    CopySearch { input: String, forward: bool },
    RenameWindow { input: String },
    RenameSession { input: String },
}

#[cfg(test)]
mod tests {
    use super::CopyModeState;
    use crate::types::{PaneTextSnapshot, SnapshotLine};

    #[test]
    fn copy_mode_state_starts_from_snapshot_cursor() {
        let snapshot = PaneTextSnapshot {
            lines: vec![SnapshotLine {
                text: "hello".to_string(),
                terminated: false,
            }],
            cursor_line: 0,
            cursor_col: 3,
        };
        let state = CopyModeState::new(snapshot);
        assert_eq!(state.cursor.line, 0);
        assert_eq!(state.cursor.col, 3);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusDir {
    Left,
    Right,
    Up,
    Down,
}

pub type KeyCombo = (KeyCode, KeyModifiers);

#[derive(Clone)]
pub struct KeyBinding {
    pub key: KeyCombo,
    pub action: Action,
    pub repeat: bool,
}

#[derive(Clone)]
pub enum Action {
    Command(String),
    CommandChain(Vec<String>),
    SwitchTable(String),
    NewWindow,
    SplitHorizontal,
    SplitVertical,
    KillPane,
    NextWindow,
    PrevWindow,
    SelectPane(FocusDir),
    CopyMode,
    Paste,
    Detach,
    RenameWindow,
    ZoomPane,
    LastWindow,
    LastPane,
    DisplayPanes,
    WindowChooser,
}
