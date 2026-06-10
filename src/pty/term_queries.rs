//! Respond to terminal queries (CPR/DSR) written by applications to the PTY.

use std::{
    io::Write,
    sync::{Arc, Mutex},
};

use crate::terminal::AlacrittyTermState;

pub type PtyWriter = Arc<Mutex<Box<dyn Write + Send>>>;

#[derive(Default)]
pub struct TermQueryTracker {
    pending: Vec<u8>,
}

enum TermQuery {
    DeviceStatus,
    CursorPosition,
}

impl TermQueryTracker {
    pub fn process(
        &mut self,
        data: &[u8],
        parser: &Arc<Mutex<AlacrittyTermState>>,
        writer: &PtyWriter,
    ) {
        self.pending.extend_from_slice(data);
        let mut consumed = 0usize;
        while let Some((advance, query)) = next_query(&self.pending[consumed..])
        {
            consumed += advance;
            if let Some(query) = query {
                respond(query, parser, writer);
            }
        }
        if consumed > 0 {
            self.pending.drain(..consumed);
        }
        if self.pending.len() > 512 {
            self.pending.clear();
        }
    }
}

fn next_query(data: &[u8]) -> Option<(usize, Option<TermQuery>)> {
    let start = data.iter().position(|&b| b == 0x1b)?;
    if start + 1 >= data.len() {
        return None;
    }
    if data[start + 1] != b'[' {
        return Some((start + 1, None));
    }
    let params_start = start + 2;
    let Some(rel_end) = data[params_start..]
        .iter()
        .position(|&b| (0x40..=0x7e).contains(&b))
    else {
        return None;
    };
    let final_index = params_start + rel_end;
    let final_byte = data[final_index];
    if final_byte != b'n' {
        return Some((final_index + 1, None));
    }
    let params = &data[params_start..final_index];
    let query = parse_dsr_params(params);
    Some((final_index + 1, query))
}

fn parse_dsr_params(params: &[u8]) -> Option<TermQuery> {
    let code = if params.first() == Some(&b'?') {
        parse_ascii_digits(&params[1..])?
    } else {
        parse_ascii_digits(params)?
    };
    match code {
        5 => Some(TermQuery::DeviceStatus),
        6 => Some(TermQuery::CursorPosition),
        _ => None,
    }
}

fn parse_ascii_digits(params: &[u8]) -> Option<u32> {
    if params.is_empty() {
        return None;
    }
    if !params.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    std::str::from_utf8(params).ok()?.parse().ok()
}

fn respond(
    query: TermQuery,
    parser: &Arc<Mutex<AlacrittyTermState>>,
    writer: &PtyWriter,
) {
    let response = match query {
        TermQuery::DeviceStatus => b"\x1b[0n".to_vec(),
        TermQuery::CursorPosition => {
            let (row, col) =
                parser.lock().map(|p| p.cursor_position()).unwrap_or((0, 0));
            format!("\x1b[{};{}R", row + 1, col + 1).into_bytes()
        }
    };
    if let Ok(mut writer) = writer.lock() {
        let _ = writer.write_all(&response);
        let _ = writer.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::AlacrittyTermState;

    #[test]
    fn responds_to_cpr_with_cursor_position() {
        let parser = Arc::new(Mutex::new(AlacrittyTermState::new(5, 20, 10)));
        parser.lock().unwrap().process(b"hello");
        let sink: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let writer: PtyWriter = {
            let sink = Arc::clone(&sink);
            Arc::new(Mutex::new(
                Box::new(WriterSink(sink)) as Box<dyn Write + Send>
            ))
        };
        let mut tracker = TermQueryTracker::default();
        tracker.process(b"\x1b[6n", &parser, &writer);
        let bytes = sink.lock().unwrap().clone();
        assert!(
            bytes.windows(2).any(|w| w == b"R" || w.ends_with(b"R")),
            "expected CPR response ending in R, got {:?}",
            String::from_utf8_lossy(&bytes)
        );
    }

    struct WriterSink(Arc<Mutex<Vec<u8>>>);

    impl Write for WriterSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
}
