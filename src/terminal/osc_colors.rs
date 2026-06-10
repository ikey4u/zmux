//! Track OSC 10/11/110/111 pane default foreground/background colors.

use std::sync::{Arc, Mutex};

use crate::terminal::AlacrittyTermState;

pub struct OscColorTracker {
    pending: Vec<u8>,
}

impl Default for OscColorTracker {
    fn default() -> Self {
        Self {
            pending: Vec::new(),
        }
    }
}

impl OscColorTracker {
    pub fn process(
        &mut self,
        data: &[u8],
        parser: &Arc<Mutex<AlacrittyTermState>>,
    ) {
        self.pending.extend_from_slice(data);
        let mut consumed = 0usize;
        while let Some((advance, event)) =
            next_osc_event(&self.pending[consumed..])
        {
            consumed += advance;
            if let Some(event) = event {
                if let Ok(mut term) = parser.lock() {
                    match event {
                        OscColorEvent::SetForeground(rgb) => {
                            term.set_pane_default_fg(rgb);
                        }
                        OscColorEvent::SetBackground(rgb) => {
                            term.set_pane_default_bg(rgb);
                        }
                        OscColorEvent::ResetForeground => {
                            term.reset_pane_default_fg();
                        }
                        OscColorEvent::ResetBackground => {
                            term.reset_pane_default_bg();
                        }
                    }
                }
            }
        }
        if consumed > 0 {
            self.pending.drain(..consumed);
        }
        if self.pending.len() > 4096 {
            self.pending.clear();
        }
    }
}

enum OscColorEvent {
    SetForeground((u8, u8, u8)),
    SetBackground((u8, u8, u8)),
    ResetForeground,
    ResetBackground,
}

fn next_osc_event(data: &[u8]) -> Option<(usize, Option<OscColorEvent>)> {
    let start = data.iter().position(|&b| b == 0x1b)?;
    if start + 1 >= data.len() {
        return None;
    }
    if data[start + 1] != b']' {
        return Some((start + 1, None));
    }
    let body_start = start + 2;
    let (body_end, terminator_len) = find_osc_end(&data[body_start..])?;
    let body = &data[body_start..body_start + body_end];
    let advance = start + 2 + body_end + terminator_len;
    let event = parse_osc_color_body(body);
    Some((advance, event))
}

fn find_osc_end(data: &[u8]) -> Option<(usize, usize)> {
    for (i, &b) in data.iter().enumerate() {
        if b == 0x07 {
            return Some((i, 1));
        }
        if b == 0x1b && data.get(i + 1) == Some(&b'\\') {
            return Some((i, 2));
        }
    }
    None
}

fn parse_osc_color_body(body: &[u8]) -> Option<OscColorEvent> {
    let text = std::str::from_utf8(body).ok()?;
    if let Some((code, payload)) = text.split_once(';') {
        let code: u32 = code.parse().ok()?;
        return match code {
            10 => parse_set_color(payload).map(OscColorEvent::SetForeground),
            11 => parse_set_color(payload).map(OscColorEvent::SetBackground),
            110 if payload.is_empty() => Some(OscColorEvent::ResetForeground),
            111 if payload.is_empty() => Some(OscColorEvent::ResetBackground),
            _ => None,
        };
    }
    match text {
        "110" => Some(OscColorEvent::ResetForeground),
        "111" => Some(OscColorEvent::ResetBackground),
        _ => None,
    }
}

fn parse_set_color(payload: &str) -> Option<(u8, u8, u8)> {
    if payload == "?" {
        return None;
    }
    if let Some(hex) = payload.strip_prefix('#') {
        if hex.len() == 6 {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            return Some((r, g, b));
        }
    }
    if let Some(rgb) = payload.strip_prefix("rgb:") {
        let parts: Vec<&str> = rgb.split('/').collect();
        if parts.len() == 3 {
            let r = parse_rgb_component(parts[0])?;
            let g = parse_rgb_component(parts[1])?;
            let b = parse_rgb_component(parts[2])?;
            return Some((r, g, b));
        }
    }
    None
}

fn parse_rgb_component(s: &str) -> Option<u8> {
    if s.is_empty() || !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let byte_str = if s.len() >= 2 { &s[s.len() - 2..] } else { s };
    u8::from_str_radix(byte_str, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_osc11_rgb_set() {
        let mut tracker = OscColorTracker::default();
        let parser = Arc::new(Mutex::new(AlacrittyTermState::new(1, 1, 10)));
        tracker.process(b"\x1b]11;rgb:001a/001a/003a\x07", &parser);
        let term = parser.lock().unwrap();
        assert_eq!(term.pane_default_bg(), Some((26, 26, 58)));
    }

    #[test]
    fn parses_osc110_reset() {
        let mut tracker = OscColorTracker::default();
        let parser = Arc::new(Mutex::new(AlacrittyTermState::new(1, 1, 10)));
        tracker.process(b"\x1b]11;rgb:1/2/3\x07", &parser);
        tracker.process(b"\x1b]111\x07", &parser);
        let term = parser.lock().unwrap();
        assert_eq!(term.pane_default_bg(), None);
    }
}
