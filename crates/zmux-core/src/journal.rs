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
        if bytes.is_empty() {
            return;
        }
        let skip = bytes.len().saturating_sub(2 * 1024 * 1024);
        self.chunks
            .push_back((self.end + skip as u64, bytes[skip..].to_vec()));
        self.end += bytes.len() as u64;
        self.bytes += bytes.len() - skip;
        while (self.bytes > 2 * 1024 * 1024 || self.chunks.len() > 4096)
            && self.chunks.len() > 1
        {
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_stream_detects_gaps_and_chunks_without_losing_bytes() {
        let mut j = OutputJournal::default();
        j.append(&vec![b'a'; 3 * 1024 * 1024]);
        assert!(j.read(0).is_none());
        assert_eq!(j.read(1024 * 1024).unwrap().len(), 256 * 1024);
        assert!(j.bytes <= 2 * 1024 * 1024);
        assert!(j.read(j.end).unwrap().is_empty());
        assert!(j.read(j.end + 1).is_none());
        let end = j.end;
        j.append(b"hello");
        assert_eq!(j.read(end).unwrap(), b"hello");
        for _ in 0..10000 {
            j.append(b"");
        }
        assert!(j.chunks.len() <= 2);
        for _ in 0..10000 {
            j.append(b"x");
        }
        assert!(j.chunks.len() <= 4096);
    }
}
