use std::sync::{Arc, Mutex};

pub fn parse_osc7_payload(payload: &str) -> Option<String> {
    let payload = payload.trim();
    let file_url = payload.strip_prefix("file://").unwrap_or(payload);
    let path = if let Some(rest) = file_url.strip_prefix('/') {
        if rest.len() >= 2 && rest.as_bytes().get(1) == Some(&b':') {
            rest
        } else if let Some(slash) = file_url.find('/') {
            &file_url[slash..]
        } else {
            file_url
        }
    } else if file_url.len() >= 2 && file_url.as_bytes().get(1) == Some(&b':') {
        file_url
    } else if let Some(slash) = file_url.find('/') {
        &file_url[slash..]
    } else {
        file_url
    };
    let path = path.trim_start_matches('/');
    if path.is_empty() {
        return None;
    }
    Some(path.replace('/', "\\"))
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

    #[test]
    fn parses_windows_file_uri() {
        assert_eq!(
            parse_osc7_payload("file:///C:/Users/test"),
            Some(r"C:\Users\test".to_string())
        );
    }

    #[test]
    fn parses_cmd_style_file_uri() {
        assert_eq!(
            parse_osc7_payload(r"file://C:\Users\test"),
            Some(r"C:\Users\test".to_string())
        );
    }

    #[test]
    fn tracker_extracts_osc7_from_output() {
        let cwd = Arc::new(Mutex::new(None));
        let mut tracker = CwdTracker::default();
        tracker.process(b"hello\x1b]7;file:///D:/Projects/zmux\x07world", &cwd);
        assert_eq!(cwd.lock().unwrap().as_deref(), Some(r"D:\Projects\zmux"));
    }
}
