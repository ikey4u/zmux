use std::sync::{Arc, Mutex};

/// Parse an OSC 7 payload (e.g. `file://host/path` or `file:///C:/Users`)
/// into a native filesystem path.
///
/// The `file://` URI is allowed to carry a hostname between the `//` and the
/// first `/` (shells emit `file://$HOST$PWD`). We strip that host so we keep
/// only the absolute path. On Unix the path is returned verbatim (forward
/// slashes, leading `/` intact); on Windows drive-letter paths are rewritten
/// to use backslashes.
pub fn parse_osc7_payload(payload: &str) -> Option<String> {
    let payload = payload.trim();
    let after_scheme = payload.strip_prefix("file://").unwrap_or(payload);

    // After stripping `file://`, one of:
    //   `/abs/path`            -> empty host, unix abs path
    //   `//abs/path`           -> extra slash, treat as abs path
    //   `host/abs/path`        -> host (no `:`) to strip
    //   `C:/Users` / `C:\Users` -> windows drive path
    let path = if after_scheme.starts_with('/') {
        // Empty host (or stray slashes): the real path follows.
        let trimmed = after_scheme.trim_start_matches('/');
        if trimmed.len() >= 2 && trimmed.as_bytes().get(1) == Some(&b':') {
            // `/C:/Users` style drive path on Windows: no leading slash.
            trimmed.to_string()
        } else {
            // `/abs/path` on unix.
            format!("/{}", trimmed)
        }
    } else if after_scheme.len() >= 2
        && after_scheme.as_bytes().get(1) == Some(&b':')
    {
        // Windows drive path like `C:/Users` with no scheme slashes.
        after_scheme.to_string()
    } else if let Some(slash) = after_scheme.find('/') {
        // `host/path` -> keep the path (including its leading slash).
        after_scheme[slash..].to_string()
    } else {
        after_scheme.to_string()
    };

    let path = path.trim();
    if path.is_empty() {
        return None;
    }

    Some(normalize_path(path))
}

#[cfg(windows)]
fn normalize_path(path: &str) -> String {
    path.replace('/', "\\")
}

#[cfg(not(windows))]
fn normalize_path(path: &str) -> String {
    path.to_string()
}

#[derive(Default)]
pub struct CwdTracker {
    pending: Vec<u8>,
}

impl CwdTracker {
    pub fn process(&mut self, data: &[u8], cwd: &Arc<Mutex<Option<String>>>) {
        let mut bytes = std::mem::take(&mut self.pending);
        bytes.extend_from_slice(data);
        let mut i = 0usize;
        while i < bytes.len() {
            if bytes[i] != 0x1b {
                i += 1;
                continue;
            }
            if i + 1 >= bytes.len() {
                break;
            }
            if bytes[i + 1] != b']' {
                i += 1;
                continue;
            }
            let start = i + 2;
            let mut end = None;
            let mut j = start;
            while j < bytes.len() {
                if bytes[j] == 0x07 {
                    end = Some((j, j + 1));
                    break;
                }
                if bytes[j] == 0x1b
                    && j + 1 < bytes.len()
                    && bytes[j + 1] == b'\\'
                {
                    end = Some((j, j + 2));
                    break;
                }
                j += 1;
            }
            let Some((payload_end, seq_end)) = end else {
                break;
            };
            let payload = std::str::from_utf8(&bytes[start..payload_end]).ok();
            if let Some(payload) = payload {
                if let Some(stripped) = payload.strip_prefix("7;") {
                    if let Some(path) = parse_osc7_payload(stripped) {
                        if let Ok(mut slot) = cwd.lock() {
                            *slot = Some(path);
                        }
                    }
                }
            }
            i = seq_end;
        }
        if i < bytes.len() {
            self.pending.extend_from_slice(&bytes[i..]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn parses_windows_file_uri() {
        assert_eq!(
            parse_osc7_payload("file:///C:/Users/test"),
            Some(r"C:\Users\test".to_string())
        );
    }

    #[cfg(windows)]
    #[test]
    fn parses_cmd_style_file_uri() {
        assert_eq!(
            parse_osc7_payload(r"file://C:\Users\test"),
            Some(r"C:\Users\test".to_string())
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn parses_unix_file_uri_with_host() {
        // macOS/Linux shells emit `file://$HOST$PWD`.
        assert_eq!(
            parse_osc7_payload("file://ZHQLI-MC6/Users/z9/Dev/zmux"),
            Some("/Users/z9/Dev/zmux".to_string())
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn parses_unix_file_uri_without_host() {
        assert_eq!(
            parse_osc7_payload("file:///Users/z9/Dev/zmux"),
            Some("/Users/z9/Dev/zmux".to_string())
        );
    }

    #[cfg(windows)]
    #[test]
    fn tracker_extracts_osc7_from_output() {
        let cwd = Arc::new(Mutex::new(None));
        let mut tracker = CwdTracker::default();
        tracker.process(b"hello\x1b]7;file:///D:/Projects/zmux\x07world", &cwd);
        assert_eq!(cwd.lock().unwrap().as_deref(), Some(r"D:\Projects\zmux"));
    }

    #[cfg(not(windows))]
    #[test]
    fn tracker_extracts_osc7_from_output() {
        let cwd = Arc::new(Mutex::new(None));
        let mut tracker = CwdTracker::default();
        tracker.process(
            b"hello\x1b]7;file://ZHQLI-MC6/Users/z9/Dev/zmux\x07world",
            &cwd,
        );
        assert_eq!(cwd.lock().unwrap().as_deref(), Some("/Users/z9/Dev/zmux"));
    }
}
