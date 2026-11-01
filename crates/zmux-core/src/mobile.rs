//! Opt-in terminal snapshots for narrow, resizable mobile viewers.
use alacritty_terminal::term::cell::Flags;

use crate::{
    output::{write_style_diff, CharacterStyles, RESET_STYLES},
    terminal::{AlacrittyTermState, TerminalCell},
    types::Pane,
};

pub const HISTORY_ROWS: usize = 4000;
pub const HISTORY_BYTES: usize = 256 * 1024;

/// ANSI history to replay at the snapshot's column count before painting its screen.
#[derive(Default)]
pub struct HistorySnapshot {
    pub ansi: String,
    pub rows: usize,
    pub truncated: bool,
}

pub fn history_snapshot(terminal: &AlacrittyTermState) -> HistorySnapshot {
    history_with_limits(terminal, HISTORY_ROWS, HISTORY_BYTES)
}

fn history_with_limits(
    terminal: &AlacrittyTermState,
    row_limit: usize,
    byte_limit: usize,
) -> HistorySnapshot {
    if terminal.alternate_screen() {
        return HistorySnapshot::default();
    }
    let (rows, _, _) = terminal.snapshot_rows();
    let end = rows.len().saturating_sub(terminal.size().0 as usize);
    let start = end.saturating_sub(row_limit);
    let mut encoded = Vec::new();
    // Reserve room for a final reset and a pending soft wrap into the screen.
    let mut bytes = 0;
    let budget = byte_limit.saturating_sub(16);
    for row in rows[start..end].iter().rev() {
        let line = encode_row(row);
        if bytes + line.len() > budget {
            break;
        }
        bytes += line.len();
        encoded.push(line);
    }
    let count = encoded.len();
    let mut ansi = String::with_capacity(bytes + 16);
    if count > 0 {
        ansi.push_str("\x1b[0m");
        for row in encoded.into_iter().rev() {
            ansi.push_str(&row);
        }
        if is_wrapped(&rows[end - 1]) {
            // Trigger the deferred autowrap without hard-breaking the history
            // line. The following authoritative paint replaces this space.
            ansi.push_str(" \r");
        }
        ansi.push_str("\x1b[0m");
    }
    HistorySnapshot {
        ansi,
        rows: count,
        truncated: count < end,
    }
}

fn is_wrapped(row: &[Option<TerminalCell>]) -> bool {
    row.iter()
        .flatten()
        .any(|cell| cell.flags.contains(Flags::WRAPLINE))
}

fn encode_row(row: &[Option<TerminalCell>]) -> String {
    let wrapped = is_wrapped(row);
    let end = row
        .iter()
        .rposition(|cell| {
            cell.as_ref().is_some_and(|cell| {
                wrapped
                    || cell.text != " "
                    || CharacterStyles::from_cell(cell) != RESET_STYLES
            })
        })
        .map(|index| index + row[index].as_ref().unwrap().width.max(1) as usize)
        .unwrap_or(0)
        .min(row.len());
    let mut out = String::new();
    let mut style = RESET_STYLES;
    let mut index = 0;
    while index < end {
        if let Some(cell) = &row[index] {
            write_style_diff(
                &mut style,
                CharacterStyles::from_cell(cell),
                &mut out,
            );
            out.push_str(&cell.text);
            // A wide glyph's following None cell is a spacer, not another space.
            index += cell.width.max(1) as usize;
        } else {
            write_style_diff(&mut style, RESET_STYLES, &mut out);
            out.push(' ');
            index += 1;
        }
    }
    out.push_str("\x1b[0m");
    if !wrapped {
        out.push_str("\r\n");
    }
    out
}

pub(crate) fn pane_snapshot(pane: &Pane) -> (String, String, bool) {
    let (screen, _) = crate::output::pane_snapshot_with_history(pane, false);
    let history = pane
        .parser
        .lock()
        .map(|parser| history_snapshot(&parser))
        .unwrap_or_default();
    (screen, history.ansi, history.truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::vte::ansi::{Color, NamedColor};

    fn logical_history(terminal: &AlacrittyTermState) -> Vec<String> {
        let (rows, _, _) = terminal.snapshot_rows();
        let end = rows.len().saturating_sub(terminal.size().0 as usize);
        let mut lines = vec![String::new()];
        for row in &rows[..end] {
            let text = row
                .iter()
                .flatten()
                .map(|cell| cell.text.as_str())
                .collect::<String>();
            lines.last_mut().unwrap().push_str(text.trim_end());
            if !is_wrapped(row) {
                lines.push(String::new());
            }
        }
        lines
    }

    #[test]
    fn soft_wrapped_coloured_unicode_history_reflows_on_a_small_screen() {
        let mut source = AlacrittyTermState::new(3, 9, 100);
        let text = "mobile-你好-e\u{301}-long-line-終端";
        source.process(
            format!("\x1b[31m{text}\x1b[0m\r\none\r\ntwo\r\nthree\r\n")
                .as_bytes(),
        );
        let snapshot = history_snapshot(&source);
        assert!(snapshot.rows > 0);
        assert!(!snapshot.truncated);
        let mut restored = AlacrittyTermState::new(3, 9, 100);
        restored.process(snapshot.ansi.as_bytes());
        restored.process(b"\r\n\r\n");
        assert_eq!(logical_history(&source), logical_history(&restored));
        let rows = restored.snapshot_rows().0;
        let glyph = rows
            .iter()
            .flatten()
            .flatten()
            .find(|cell| cell.text == "m")
            .unwrap();
        assert_eq!(glyph.fg, Color::Named(NamedColor::Red));
        restored.resize(3, 17);
        assert!(logical_history(&restored)
            .iter()
            .any(|line| line.contains(text)));
    }

    #[test]
    fn row_and_byte_limits_keep_the_newest_complete_utf8_rows() {
        let mut source = AlacrittyTermState::new(2, 40, 100);
        for index in 0..30 {
            source.process(format!("line-{index:02}-你好\r\n").as_bytes());
        }
        let snapshot = history_with_limits(&source, 4, HISTORY_BYTES);
        assert_eq!(snapshot.rows, 4);
        assert!(snapshot.truncated);
        assert!(snapshot.ansi.contains("line-28"));
        assert!(!snapshot.ansi.contains("line-00"));
        let bounded = history_with_limits(&source, 100, 100);
        assert!(bounded.ansi.len() <= 100);
        assert!(bounded.truncated);
        assert!(bounded.ansi.contains("line-28"));
        assert!(!bounded.ansi.contains("line-00"));
    }

    #[test]
    fn alternate_screen_never_replays_normal_history() {
        let mut source = AlacrittyTermState::new(2, 10, 100);
        source.process(b"before\r\none\r\ntwo\r\n\x1b[?1049hfull screen");
        let snapshot = history_snapshot(&source);
        assert!(snapshot.ansi.is_empty());
        assert_eq!(snapshot.rows, 0);
        assert!(!snapshot.truncated);
    }

    #[test]
    fn a_wrap_crossing_the_history_screen_boundary_stays_soft() {
        let mut source = AlacrittyTermState::new(2, 8, 100);
        source.process(b"12345678abcdefghIJK");
        let snapshot = history_snapshot(&source);
        assert_eq!(snapshot.rows, 1);
        let mut restored = AlacrittyTermState::new(2, 8, 100);
        restored.process(snapshot.ansi.as_bytes());
        restored.process(b"\r\n");
        assert_eq!(logical_history(&source), logical_history(&restored));
        assert!(is_wrapped(&restored.snapshot_rows().0[0]));
    }
}
