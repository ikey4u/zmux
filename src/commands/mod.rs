use std::collections::HashMap;

use crate::types::{
    events::ServerMsg,
    session::{ClientId, SessionId},
};

#[derive(Debug, Clone)]
pub struct ParsedCommand {
    pub name: String,
    pub args: Vec<String>,
    pub flags: HashMap<String, Option<String>>,
}

impl ParsedCommand {
    pub fn parse(input: &str) -> Vec<ParsedCommand> {
        let mut cmds = Vec::new();
        for segment in split_on_semicolon(input) {
            if let Some(cmd) = parse_single(segment.trim()) {
                cmds.push(cmd);
            }
        }
        cmds
    }

    pub fn flag(&self, name: &str) -> bool {
        self.flags.contains_key(name)
    }

    pub fn flag_value(&self, name: &str) -> Option<&str> {
        self.flags.get(name).and_then(|v| v.as_deref())
    }
}

fn split_on_semicolon(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut depth = 0i32;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' | b'(' => depth += 1,
            b'}' | b')' => depth -= 1,
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'\'' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'\'' {
                    i += 1;
                }
            }
            b';' if depth == 0 => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    parts.push(&s[start..]);
    parts
}

fn parse_single(s: &str) -> Option<ParsedCommand> {
    if s.is_empty() {
        return None;
    }
    let tokens = tokenize(s);
    if tokens.is_empty() {
        return None;
    }
    let mut it = tokens.into_iter();
    let name = it.next().unwrap();
    let mut args = Vec::new();
    let mut flags: HashMap<String, Option<String>> = HashMap::new();

    let mut remaining: Vec<String> = it.collect();
    let mut i = 0;
    while i < remaining.len() {
        let tok = &remaining[i];
        if tok == "--" {
            for rest in &remaining[i + 1..] {
                args.push(rest.clone());
            }
            break;
        }
        if tok.starts_with('-') && tok.len() > 1 && !tok.starts_with("--") {
            let flag_chars: Vec<char> = tok[1..].chars().collect();
            for (fi, &fc) in flag_chars.iter().enumerate() {
                let flag_key = fc.to_string();
                if FLAG_HAS_VALUE.contains(&fc) {
                    if fi + 1 < flag_chars.len() {
                        let val: String = flag_chars[fi + 1..].iter().collect();
                        flags.insert(flag_key, Some(val));
                        break;
                    } else if i + 1 < remaining.len() {
                        let val = remaining[i + 1].clone();
                        flags.insert(flag_key, Some(val));
                        i += 1;
                        break;
                    } else {
                        flags.insert(flag_key, None);
                    }
                } else {
                    flags.insert(flag_key, None);
                }
            }
        } else {
            args.push(tok.clone());
        }
        i += 1;
    }

    Some(ParsedCommand { name, args, flags })
}

const FLAG_HAS_VALUE: &[char] = &[
    't', 's', 'n', 'c', 'F', 'f', 'l', 'w', 'h', 'e', 'p', 'T', 'C', 'L', 'I',
];

fn tokenize(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' if current.is_empty() => {}
            ' ' | '\t' => {
                tokens.push(current.clone());
                current.clear();
            }
            '"' => {
                while let Some(c2) = chars.next() {
                    if c2 == '"' {
                        break;
                    }
                    if c2 == '\\' {
                        if let Some(escaped) = chars.next() {
                            current.push(escaped);
                        }
                    } else {
                        current.push(c2);
                    }
                }
            }
            '\'' => {
                while let Some(c2) = chars.next() {
                    if c2 == '\'' {
                        break;
                    }
                    current.push(c2);
                }
            }
            _ => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}
