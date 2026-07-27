use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

const MAX_SEQUENCE_BYTES: usize = 1024 * 1024;
const MAX_QUEUE_BYTES: usize = 2 * 1024 * 1024;

/// Collect OSC 52 sequences emitted by programs running in a pane.
///
/// A pane is rendered through zmux's terminal emulator, so OSC sequences are
/// consumed before they can reach the outer terminal. Keeping the complete,
/// validated sequence here lets the renderer relay it to the attached client.
#[derive(Default)]
pub struct Osc52Tracker {
    pending: Vec<u8>,
}

impl Osc52Tracker {
    pub fn process(
        &mut self,
        data: &[u8],
        queue: &Arc<Mutex<VecDeque<Vec<u8>>>>,
    ) {
        let mut bytes = std::mem::take(&mut self.pending);
        bytes.extend_from_slice(data);

        let mut i = 0usize;
        while i < bytes.len() {
            let Some(relative_start) = bytes[i..]
                .windows(5)
                .position(|window| window == b"\x1b]52;")
            else {
                // Retain a possible partial OSC 52 introducer at the end.
                let keep = bytes.len().saturating_sub(4).max(i);
                i = keep;
                break;
            };
            let start = i + relative_start;
            let payload_start = start + 2;
            let mut terminator = None;
            let mut j = start + 5;
            while j < bytes.len() {
                if bytes[j] == 0x07 {
                    terminator = Some((j, j + 1));
                    break;
                }
                if bytes[j] == 0x1b
                    && j + 1 < bytes.len()
                    && bytes[j + 1] == b'\\'
                {
                    terminator = Some((j, j + 2));
                    break;
                }
                j += 1;
            }

            let Some((payload_end, sequence_end)) = terminator else {
                if bytes.len().saturating_sub(start) <= MAX_SEQUENCE_BYTES {
                    i = start;
                    break;
                }
                // Never retain an unbounded unterminated control sequence.
                i = start + 5;
                continue;
            };

            let payload = &bytes[payload_start..payload_end];
            if is_valid_osc52_payload(payload) {
                enqueue(queue, bytes[start..sequence_end].to_vec());
            }
            i = sequence_end;
        }

        if i < bytes.len() {
            self.pending.extend_from_slice(&bytes[i..]);
        }
    }
}

fn is_valid_osc52_payload(payload: &[u8]) -> bool {
    let Some((code, rest)) = payload.split_first_chunk::<3>() else {
        return false;
    };
    if code != b"52;" {
        return false;
    }
    let Some(separator) = rest.iter().position(|&byte| byte == b';') else {
        return false;
    };
    let (selection, encoded) = rest.split_at(separator);
    let encoded = &encoded[1..];
    !selection.is_empty()
        && selection.iter().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(*byte, b',' | b'?')
        })
        && encoded.iter().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(*byte, b'+' | b'/' | b'=')
        })
}

fn enqueue(queue: &Arc<Mutex<VecDeque<Vec<u8>>>>, sequence: Vec<u8>) {
    if sequence.len() > MAX_SEQUENCE_BYTES {
        return;
    }
    let Ok(mut queue) = queue.lock() else {
        return;
    };
    let mut queued_bytes = queue.iter().map(Vec::len).sum::<usize>();
    while queued_bytes + sequence.len() > MAX_QUEUE_BYTES {
        let Some(dropped) = queue.pop_front() else {
            break;
        };
        queued_bytes = queued_bytes.saturating_sub(dropped.len());
    }
    queue.push_back(sequence);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relays_fragmented_bel_terminated_sequence() {
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        let mut tracker = Osc52Tracker::default();
        tracker.process(b"before\x1b]52;c;aGVs", &queue);
        tracker.process(b"bG8=\x07after", &queue);

        assert_eq!(
            queue.lock().unwrap().pop_front(),
            Some(b"\x1b]52;c;aGVsbG8=\x07".to_vec())
        );
    }

    #[test]
    fn ignores_invalid_payloads() {
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        let mut tracker = Osc52Tracker::default();
        tracker.process(b"\x1b]52;c;not valid!\x07", &queue);

        assert!(queue.lock().unwrap().is_empty());
    }
}
