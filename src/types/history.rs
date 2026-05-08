/// Per-character style stored in the copy-mode snapshot.
/// `fg` / `bg` are colour names as produced by `terminal::color_name`.
/// `flags` is a compact bitmask matching the wire format used by `frame.rs`:
/// bit 0 = dim, bit 1 = bold, bit 2 = italic, bit 3 = underline, bit 4 = inverse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellStyle {
    pub fg: String,
    pub bg: String,
    pub flags: u8,
}

impl Default for CellStyle {
    fn default() -> Self {
        Self {
            fg: "default".to_string(),
            bg: "default".to_string(),
            flags: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SnapshotLine {
    pub text: String,
    pub terminated: bool,
    /// One `CellStyle` per *character* (not byte) in `text`.
    /// May be shorter than `text.chars().count()` if style info was
    /// unavailable; callers should treat missing entries as `CellStyle::default()`.
    pub styles: Vec<CellStyle>,
}

#[derive(Debug, Clone)]
pub struct PaneTextSnapshot {
    pub lines: Vec<SnapshotLine>,
    pub cursor_line: usize,
    pub cursor_col: usize,
}
