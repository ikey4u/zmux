use std::{
    io,
    path::{Path, PathBuf},
};

use crossterm::event::{KeyCode, KeyModifiers};

use crate::types::{
    mode::{Action, KeyBinding},
    options::GlobalOptions,
};

pub fn find_config_file() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("ZMUX_CONFIG") {
        let p = PathBuf::from(&path);
        if p.exists() {
            return Some(p);
        }
    }
    let home = home_dir()?;
    let candidates = [
        home.join(".zmux.conf"),
        home.join(".config").join("zmux").join("zmux.conf"),
        home.join(".tmux.conf"),
        PathBuf::from("/etc/zmux.conf"),
    ];
    candidates.into_iter().find(|p| p.exists())
}

pub fn home_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    return std::env::var("HOME").ok().map(PathBuf::from);
    #[cfg(windows)]
    return std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
        .map(PathBuf::from);
}

pub struct ConfigLine {
    pub command: String,
    pub args: String,
    pub source_file: PathBuf,
    pub line_num: usize,
}

pub fn load_config_lines(path: &Path) -> io::Result<Vec<ConfigLine>> {
    let content = std::fs::read_to_string(path)?;
    let mut lines = Vec::new();
    for (i, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (cmd, rest) = split_first_word(line);
        lines.push(ConfigLine {
            command: cmd.to_string(),
            args: rest.to_string(),
            source_file: path.to_path_buf(),
            line_num: i + 1,
        });
    }
    Ok(lines)
}

fn split_first_word(s: &str) -> (&str, &str) {
    let s = s.trim();
    if let Some(pos) = s.find(|c: char| c.is_whitespace()) {
        (&s[..pos], s[pos..].trim())
    } else {
        (s, "")
    }
}

pub fn parse_key(s: &str) -> Option<KeyBinding> {
    let s = s.trim();
    parse_key_combo(s).map(|key| KeyBinding {
        key,
        action: Action::Command(String::new()),
        repeat: false,
    })
}

pub fn parse_key_combo(s: &str) -> Option<(KeyCode, KeyModifiers)> {
    let s = s.trim();
    let mut modifiers = KeyModifiers::empty();
    let mut rest = s;

    loop {
        if let Some(r) =
            rest.strip_prefix("C-").or_else(|| rest.strip_prefix("^"))
        {
            modifiers |= KeyModifiers::CONTROL;
            rest = r;
        } else if let Some(r) = rest.strip_prefix("M-") {
            modifiers |= KeyModifiers::ALT;
            rest = r;
        } else if let Some(r) = rest.strip_prefix("S-") {
            modifiers |= KeyModifiers::SHIFT;
            rest = r;
        } else {
            break;
        }
    }

    let code = match rest {
        "Space" | "space" => KeyCode::Char(' '),
        "Enter" | "enter" => KeyCode::Enter,
        "Escape" | "escape" | "Esc" | "esc" => KeyCode::Esc,
        "Tab" | "tab" => KeyCode::Tab,
        "BSpace" | "bspace" | "BackSpace" | "backspace" => KeyCode::Backspace,
        "Up" | "up" => KeyCode::Up,
        "Down" | "down" => KeyCode::Down,
        "Left" | "left" => KeyCode::Left,
        "Right" | "right" => KeyCode::Right,
        "Home" | "home" => KeyCode::Home,
        "End" | "end" => KeyCode::End,
        "PgUp" | "pgup" | "PageUp" => KeyCode::PageUp,
        "PgDn" | "pgdn" | "PageDown" => KeyCode::PageDown,
        "Delete" | "delete" | "Del" | "del" => KeyCode::Delete,
        "Insert" | "insert" | "Ins" | "ins" => KeyCode::Insert,
        "F1" => KeyCode::F(1),
        "F2" => KeyCode::F(2),
        "F3" => KeyCode::F(3),
        "F4" => KeyCode::F(4),
        "F5" => KeyCode::F(5),
        "F6" => KeyCode::F(6),
        "F7" => KeyCode::F(7),
        "F8" => KeyCode::F(8),
        "F9" => KeyCode::F(9),
        "F10" => KeyCode::F(10),
        "F11" => KeyCode::F(11),
        "F12" => KeyCode::F(12),
        s if s.chars().count() == 1 => {
            let c = s.chars().next().unwrap();
            KeyCode::Char(c)
        }
        _ => return None,
    };

    Some((code, modifiers))
}
