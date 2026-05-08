#[derive(Debug, Clone)]
pub struct SnapshotLine {
    pub text: String,
    pub terminated: bool,
}

#[derive(Debug, Clone)]
pub struct PaneTextSnapshot {
    pub lines: Vec<SnapshotLine>,
    pub cursor_line: usize,
    pub cursor_col: usize,
}
