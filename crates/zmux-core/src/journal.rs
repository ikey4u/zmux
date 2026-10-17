//! Bounded per-pane output, independent of viewers. Gaps require a screen snapshot.
use std::collections::VecDeque;
#[derive(Default)]
pub struct OutputJournal {
    chunks: VecDeque<(u64, Vec<u8>)>,
    pub end: u64,
    bytes: usize,
}
impl OutputJournal {
    pub fn append(&mut self, bytes: &[u8]) {
        self.chunks.push_back((self.end, bytes.to_vec()));
        self.end += bytes.len() as u64;
        self.bytes += bytes.len();
        while self.bytes > 2 * 1024 * 1024 && self.chunks.len() > 1 {
            self.bytes -= self.chunks.pop_front().unwrap().1.len();
        }
    }
    pub fn read(&self, after: u64) -> Option<Vec<u8>> {
        if after > self.end
            || after < self.chunks.front().map(|x| x.0).unwrap_or(self.end)
        {
            return None;
        }
        let mut out = Vec::new();
        for (start, data) in &self.chunks {
            let skip = after.saturating_sub(*start) as usize;
            if skip < data.len() {
                let take = (data.len() - skip).min(256 * 1024 - out.len());
                out.extend_from_slice(&data[skip..skip + take]);
                if out.len() == 256 * 1024 {
                    break;
                }
            }
        }
        Some(out)
    }
}
