use std::collections::VecDeque;

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

pub struct PaneTextBuffer {
    max_bytes: usize,
    total_bytes: usize,
    lines: VecDeque<SnapshotLine>,
    current: Vec<char>,
    current_bytes: usize,
    current_cursor: usize,
    utf8_pending: Vec<u8>,
    escape_state: EscapeState,
    alt_screen: bool,
    reflow_enabled: bool,
}

enum EscapeState {
    Ground,
    Escape,
    Csi(String),
    Osc,
    OscEscape,
    Charset,
}

impl PaneTextBuffer {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            total_bytes: 0,
            lines: VecDeque::new(),
            current: Vec::new(),
            current_bytes: 0,
            current_cursor: 0,
            utf8_pending: Vec::new(),
            escape_state: EscapeState::Ground,
            alt_screen: false,
            reflow_enabled: true,
        }
    }

    pub fn reflow_enabled(&self) -> bool {
        self.reflow_enabled
    }

    pub fn snapshot(&self) -> PaneTextSnapshot {
        let mut lines: Vec<SnapshotLine> = self.lines.iter().cloned().collect();
        let mut current = self.current.clone();
        while current.len() < self.current_cursor {
            current.push(' ');
        }
        let current_text: String = current.iter().collect();
        let (cursor_line, cursor_col) =
            if lines.is_empty() || !current_text.is_empty() {
                lines.push(SnapshotLine {
                    text: current_text,
                    terminated: false,
                });
                (lines.len().saturating_sub(1), self.current_cursor)
            } else {
                (lines.len(), 0)
            };
        if lines.is_empty() {
            lines.push(SnapshotLine {
                text: String::new(),
                terminated: false,
            });
        }
        PaneTextSnapshot {
            lines,
            cursor_line,
            cursor_col,
        }
    }

    pub fn push_bytes(&mut self, data: &[u8]) {
        for &byte in data {
            self.push_byte(byte);
        }
        self.enforce_limit();
    }

    fn push_byte(&mut self, byte: u8) {
        match &mut self.escape_state {
            EscapeState::Ground => match byte {
                0x1b => {
                    self.utf8_pending.clear();
                    self.escape_state = EscapeState::Escape;
                }
                b'\r' => {
                    self.utf8_pending.clear();
                    self.carriage_return();
                }
                b'\n' => {
                    self.utf8_pending.clear();
                    self.line_feed();
                }
                0x08 => {
                    self.utf8_pending.clear();
                    self.backspace();
                }
                b'\t' => {
                    self.utf8_pending.clear();
                    self.tab();
                }
                0x00..=0x1f | 0x7f => {
                    self.utf8_pending.clear();
                }
                b if b.is_ascii() => {
                    self.utf8_pending.clear();
                    self.emit_char(b as char);
                }
                b => self.push_utf8_byte(b),
            },
            EscapeState::Escape => match byte {
                b'[' => self.escape_state = EscapeState::Csi(String::new()),
                b']' => self.escape_state = EscapeState::Osc,
                b'(' | b')' => self.escape_state = EscapeState::Charset,
                _ => {
                    self.disable_reflow();
                    self.escape_state = EscapeState::Ground;
                }
            },
            EscapeState::Csi(buf) => {
                if (0x40..=0x7e).contains(&byte) {
                    let params = std::mem::take(buf);
                    self.escape_state = EscapeState::Ground;
                    self.handle_csi(&params, byte as char);
                } else {
                    buf.push(byte as char);
                }
            }
            EscapeState::Osc => {
                if byte == 0x07 {
                    self.escape_state = EscapeState::Ground;
                } else if byte == 0x1b {
                    self.escape_state = EscapeState::OscEscape;
                }
            }
            EscapeState::OscEscape => {
                if byte == b'\\' {
                    self.escape_state = EscapeState::Ground;
                } else {
                    self.escape_state = EscapeState::Osc;
                }
            }
            EscapeState::Charset => {
                self.escape_state = EscapeState::Ground;
            }
        }
    }

    fn push_utf8_byte(&mut self, byte: u8) {
        self.utf8_pending.push(byte);
        match std::str::from_utf8(&self.utf8_pending) {
            Ok(decoded) => {
                let decoded = decoded.to_string();
                self.utf8_pending.clear();
                for ch in decoded.chars() {
                    self.emit_char(ch);
                }
            }
            Err(err) => {
                if err.error_len().is_some() || self.utf8_pending.len() >= 4 {
                    self.utf8_pending.clear();
                }
            }
        }
    }

    fn handle_csi(&mut self, params: &str, final_char: char) {
        let private = params.starts_with('?');
        let params = if private { &params[1..] } else { params };
        let values = parse_params(params);
        if private && matches!(final_char, 'h' | 'l') {
            if values
                .iter()
                .any(|value| matches!(*value, 47 | 1047 | 1049))
            {
                self.alt_screen = final_char == 'h';
            } else if values.is_empty()
                || values
                    .iter()
                    .any(|value| !is_layout_neutral_private_mode(*value))
            {
                self.disable_reflow();
            }
            return;
        }
        if self.alt_screen {
            return;
        }
        match final_char {
            'C' => self.move_cursor_right(first_param(&values, 1) as usize),
            'D' => self.move_cursor_left(first_param(&values, 1) as usize),
            'G' => self.move_cursor_to(
                first_param(&values, 1).saturating_sub(1) as usize,
            ),
            'K' => self.erase_in_line(first_param(&values, 0)),
            '@' => self.insert_spaces(first_param(&values, 1) as usize),
            'P' => self.delete_chars(first_param(&values, 1) as usize),
            'm' | 'q' => {}
            _ => self.disable_reflow(),
        }
    }

    fn carriage_return(&mut self) {
        if self.alt_screen {
            return;
        }
        self.current_cursor = 0;
    }

    fn line_feed(&mut self) {
        if self.alt_screen {
            return;
        }
        let text: String = self.current.iter().collect();
        self.lines.push_back(SnapshotLine {
            text,
            terminated: true,
        });
        self.total_bytes += 1;
        self.current.clear();
        self.current_bytes = 0;
        self.current_cursor = 0;
        self.enforce_limit();
    }

    fn backspace(&mut self) {
        if self.alt_screen {
            return;
        }
        self.current_cursor = self.current_cursor.saturating_sub(1);
    }

    fn tab(&mut self) {
        if self.alt_screen {
            return;
        }
        let spaces = 8 - (self.current_cursor % 8);
        for _ in 0..spaces {
            self.emit_char(' ');
        }
    }

    fn emit_char(&mut self, ch: char) {
        if self.alt_screen {
            return;
        }
        self.pad_to_cursor();
        if self.current_cursor < self.current.len() {
            let old = self.current[self.current_cursor];
            self.current[self.current_cursor] = ch;
            self.current_bytes =
                self.current_bytes + ch.len_utf8() - old.len_utf8();
        } else {
            self.current.push(ch);
            self.current_bytes += ch.len_utf8();
        }
        self.current_cursor += 1;
        self.recompute_total_bytes();
    }

    fn move_cursor_left(&mut self, amount: usize) {
        self.current_cursor = self.current_cursor.saturating_sub(amount);
    }

    fn move_cursor_right(&mut self, amount: usize) {
        self.current_cursor = self.current_cursor.saturating_add(amount);
    }

    fn move_cursor_to(&mut self, col: usize) {
        self.current_cursor = col;
    }

    fn erase_in_line(&mut self, mode: u16) {
        match mode {
            0 => {
                if self.current_cursor < self.current.len() {
                    let removed: usize = self.current[self.current_cursor..]
                        .iter()
                        .map(|ch| ch.len_utf8())
                        .sum();
                    self.current.truncate(self.current_cursor);
                    self.current_bytes =
                        self.current_bytes.saturating_sub(removed);
                }
            }
            1 => {
                let end = self.current_cursor.min(self.current.len());
                for idx in 0..end {
                    let old = self.current[idx];
                    if old != ' ' {
                        self.current[idx] = ' ';
                        self.current_bytes =
                            self.current_bytes + 1 - old.len_utf8();
                    }
                }
            }
            2 => {
                self.current.clear();
                self.current_bytes = 0;
                self.current_cursor = 0;
            }
            _ => {}
        }
        self.recompute_total_bytes();
    }

    fn insert_spaces(&mut self, count: usize) {
        self.pad_to_cursor();
        if count == 0 {
            return;
        }
        self.current.splice(
            self.current_cursor..self.current_cursor,
            std::iter::repeat_n(' ', count),
        );
        self.current_bytes += count;
        self.recompute_total_bytes();
    }

    fn delete_chars(&mut self, count: usize) {
        if count == 0 || self.current_cursor >= self.current.len() {
            return;
        }
        let end = (self.current_cursor + count).min(self.current.len());
        let removed: usize = self.current[self.current_cursor..end]
            .iter()
            .map(|ch| ch.len_utf8())
            .sum();
        self.current.drain(self.current_cursor..end);
        self.current_bytes = self.current_bytes.saturating_sub(removed);
        self.recompute_total_bytes();
    }

    fn pad_to_cursor(&mut self) {
        while self.current.len() < self.current_cursor {
            self.current.push(' ');
            self.current_bytes += 1;
        }
    }

    fn disable_reflow(&mut self) {
        if !self.alt_screen {
            self.reflow_enabled = false;
        }
    }

    fn recompute_total_bytes(&mut self) {
        let committed: usize = self
            .lines
            .iter()
            .map(|line| line.text.len() + usize::from(line.terminated))
            .sum();
        self.total_bytes = committed + self.current_bytes;
        self.enforce_limit();
    }

    fn enforce_limit(&mut self) {
        while self.total_bytes > self.max_bytes && !self.lines.is_empty() {
            if let Some(line) = self.lines.pop_front() {
                self.total_bytes = self.total_bytes.saturating_sub(
                    line.text.len() + usize::from(line.terminated),
                );
            }
        }
        while self.total_bytes > self.max_bytes && !self.current.is_empty() {
            let removed = self.current.remove(0).len_utf8();
            self.current_bytes = self.current_bytes.saturating_sub(removed);
            self.total_bytes = self.total_bytes.saturating_sub(removed);
            self.current_cursor = self.current_cursor.saturating_sub(1);
        }
    }
}

fn parse_params(params: &str) -> Vec<u16> {
    if params.is_empty() {
        return Vec::new();
    }
    params
        .split(';')
        .filter_map(|value| value.parse::<u16>().ok())
        .collect()
}

fn first_param(values: &[u16], default: u16) -> u16 {
    values.first().copied().unwrap_or(default)
}

fn is_layout_neutral_private_mode(value: u16) -> bool {
    matches!(
        value,
        1 | 12 | 25 | 1000 | 1002 | 1003 | 1004 | 1005 | 1006 | 1015 | 2004
    )
}

#[cfg(test)]
mod tests {
    use super::PaneTextBuffer;

    #[test]
    fn snapshot_tracks_cursor_after_trailing_newline() {
        let mut buffer = PaneTextBuffer::new(1024);
        buffer.push_bytes(b"hello\n");
        let snapshot = buffer.snapshot();
        assert_eq!(snapshot.lines.len(), 1);
        assert_eq!(snapshot.lines[0].text, "hello");
        assert_eq!(snapshot.cursor_line, 1);
        assert_eq!(snapshot.cursor_col, 0);
    }

    #[test]
    fn snapshot_pads_current_line_to_cursor_column() {
        let mut buffer = PaneTextBuffer::new(1024);
        buffer.push_bytes(b"ab\x1b[5G");
        let snapshot = buffer.snapshot();
        assert_eq!(snapshot.lines.len(), 1);
        assert_eq!(snapshot.lines[0].text, "ab  ");
        assert_eq!(snapshot.cursor_line, 0);
        assert_eq!(snapshot.cursor_col, 4);
        assert!(buffer.reflow_enabled());
    }

    #[test]
    fn unsupported_csi_disables_logical_reflow() {
        let mut buffer = PaneTextBuffer::new(1024);
        buffer.push_bytes(b"hello\x1b[2J");
        assert!(!buffer.reflow_enabled());
    }

    #[test]
    fn bracketed_paste_mode_keeps_logical_reflow_enabled() {
        let mut buffer = PaneTextBuffer::new(1024);
        buffer.push_bytes(b"\x1b[?2004hhello\x1b[?2004l");
        assert!(buffer.reflow_enabled());
        let snapshot = buffer.snapshot();
        assert_eq!(snapshot.lines.len(), 1);
        assert_eq!(snapshot.lines[0].text, "hello");
    }

    #[test]
    fn unknown_private_mode_still_disables_logical_reflow() {
        let mut buffer = PaneTextBuffer::new(1024);
        buffer.push_bytes(b"\x1b[?7hhello");
        assert!(!buffer.reflow_enabled());
    }

    #[test]
    fn cursor_shape_sequence_keeps_logical_reflow_enabled() {
        let mut buffer = PaneTextBuffer::new(1024);
        buffer.push_bytes(b"\x1b[5 qhello");
        assert!(buffer.reflow_enabled());
        let snapshot = buffer.snapshot();
        assert_eq!(snapshot.lines.len(), 1);
        assert_eq!(snapshot.lines[0].text, "hello");
    }
}
