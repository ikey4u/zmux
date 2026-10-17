//! Headless terminal API. Each terminal owns a view, so resize/focus cannot leak to another terminal.
//! The runtime owns the shells, not a client subscription. Dropping it deliberately ends its shells.
use std::{
    collections::{HashMap, VecDeque},
    fs::File,
    io,
    path::Path,
    sync::{mpsc, Arc, Mutex},
    time::{Duration, Instant},
};

use base64::{engine::general_purpose::STANDARD, Engine as _};

use crate::{server::InProcessServer, types::session::Size, Environment};
const MAX_OUTPUT: usize = 2 * 1024 * 1024;
const MAX_CHUNK: usize = 256 * 1024;
pub struct TerminalRuntime {
    environment: Environment,
    _lock: File,
    sessions: Mutex<HashMap<String, Terminal>>,
}
struct Terminal {
    host: Arc<InProcessServer>,
    input: mpsc::SyncSender<Vec<u8>>,
    sequence: Option<u64>,
    chunks: VecDeque<(u64, Vec<u8>)>,
    start: u64,
    end: u64,
    bytes: usize,
}
#[derive(Debug)]
pub struct TerminalOutput {
    pub data: Vec<u8>,
    pub offset: u64,
    pub truncated: bool,
    pub closed: bool,
}
impl TerminalRuntime {
    pub fn new(environment: Environment) -> io::Result<Self> {
        let lock = environment.server_lock("embedded-runtime")?;
        Ok(Self {
            environment,
            _lock: lock,
            sessions: Mutex::new(HashMap::new()),
        })
    }
    pub fn environment(&self) -> &Environment {
        &self.environment
    }
    pub fn open(&self, cwd: &Path, cols: u16, rows: u16) -> io::Result<String> {
        if !cwd.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "terminal working directory is not a directory",
            ));
        }
        self.environment.enter(|| {
            let id = crate::domain::new_instance_id();
            let host = Arc::new(InProcessServer::start_in(
                &self.environment,
                id.clone(),
                Size::terminal(rows.clamp(2, 500), cols.clamp(2, 500)),
                None,
                Some(cwd.to_string_lossy().into_owned()),
            )?);
            host.set_hide_borders(true);
            let (input, receive) = mpsc::sync_channel::<Vec<u8>>(64);
            let writer = host.clone();
            crate::environment::spawn(move || {
                while let Ok(bytes) = receive.recv() {
                    if writer.is_stopped() {
                        break;
                    }
                    writer.send_input(&bytes);
                }
            });
            self.sessions.lock().map_err(poisoned)?.insert(
                id.clone(),
                Terminal {
                    host,
                    input,
                    sequence: None,
                    chunks: VecDeque::new(),
                    start: 0,
                    end: 0,
                    bytes: 0,
                },
            );
            Ok(id)
        })
    }
    pub fn input(&self, id: &str, data: &[u8]) -> io::Result<()> {
        if data.len() > 64 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "terminal input exceeds 64 KiB",
            ));
        }
        self.environment.enter(|| {
            let sessions = self.sessions.lock().map_err(poisoned)?;
            let t = sessions.get(id).ok_or_else(missing)?;
            if t.host.is_stopped() {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "terminal has exited",
                ));
            }
            t.input.try_send(data.to_vec()).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!("terminal input queue unavailable: {e}"),
                )
            })
        })
    }
    pub fn resize(&self, id: &str, cols: u16, rows: u16) -> io::Result<()> {
        self.environment.enter(|| {
            let sessions = self.sessions.lock().map_err(poisoned)?;
            let t = sessions.get(id).ok_or_else(missing)?;
            if t.host.is_stopped() {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "terminal has exited",
                ));
            }
            t.host
                .resize(Size::terminal(rows.clamp(2, 500), cols.clamp(2, 500)));
            Ok(())
        })
    }
    pub fn close(&self, id: &str) -> bool {
        self.environment.enter(|| {
            self.sessions
                .lock()
                .is_ok_and(|mut s| s.remove(id).is_some())
        })
    }
    pub fn list(&self) -> io::Result<Vec<String>> {
        Ok(self
            .sessions
            .lock()
            .map_err(poisoned)?
            .keys()
            .cloned()
            .collect())
    }
    /// No subscription lifetime is attached to this call. An absent reader never blocks PTY draining.
    pub fn pull(
        &self,
        id: &str,
        after: u64,
        wait: Duration,
    ) -> io::Result<TerminalOutput> {
        self.environment.enter(|| {
            let deadline = Instant::now() + wait.min(Duration::from_secs(30));
            loop {
                let (result, wait_host, wait_sequence) = {
                    let mut sessions =
                        self.sessions.lock().map_err(poisoned)?;
                    let t = sessions.get_mut(id).ok_or_else(missing)?;
                    if let Some((frame, sequence, snapshot)) =
                        t.host.frame_since(t.sequence)
                    {
                        if t.sequence != Some(sequence) || t.sequence.is_none()
                        {
                            let data = STANDARD
                                .decode(frame.ansi.unwrap_or_default())
                                .map_err(|e| {
                                    io::Error::new(
                                        io::ErrorKind::InvalidData,
                                        e,
                                    )
                                })?;
                            if snapshot && t.sequence.is_some() {
                                t.chunks.clear();
                                t.bytes = 0;
                                t.start = t.end;
                            }
                            t.sequence = Some(sequence);
                            if !data.is_empty() {
                                let begin = t.end;
                                t.end = t.end.saturating_add(data.len() as u64);
                                t.bytes += data.len();
                                t.chunks.push_back((begin, data));
                            }
                            while t.bytes > MAX_OUTPUT && t.chunks.len() > 1 {
                                if let Some((_, data)) = t.chunks.pop_front() {
                                    t.bytes -= data.len();
                                }
                            }
                            t.start = t
                                .chunks
                                .front()
                                .map(|(pos, _)| *pos)
                                .unwrap_or(t.end);
                        }
                    }
                    if after > t.end {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "terminal output offset is ahead of this session",
                        ));
                    }
                    let truncated = after < t.start;
                    if truncated {
                        // A fresh viewer needs current state, not a suffix of old
                        // dirty paints whose base screen has already been evicted.
                        if let Some((frame, sequence, _)) =
                            t.host.frame_since(None)
                        {
                            let snapshot = STANDARD
                                .decode(frame.ansi.unwrap_or_default())
                                .map_err(|e| {
                                    io::Error::new(
                                        io::ErrorKind::InvalidData,
                                        e,
                                    )
                                })?;
                            t.chunks.clear();
                            t.bytes = snapshot.len();
                            t.start = t.end;
                            if !snapshot.is_empty() {
                                let begin = t.end;
                                t.end =
                                    t.end.saturating_add(snapshot.len() as u64);
                                t.chunks.push_back((begin, snapshot));
                            }
                            t.sequence = Some(sequence);
                        }
                    }
                    let begin = after.max(t.start);
                    let mut data = Vec::new();
                    for (offset, chunk) in &t.chunks {
                        let end = offset + chunk.len() as u64;
                        if end <= begin {
                            continue;
                        }
                        let skip = begin.saturating_sub(*offset) as usize;
                        let remaining = MAX_CHUNK - data.len();
                        data.extend_from_slice(
                            &chunk[skip..]
                                [..(chunk.len() - skip).min(remaining)],
                        );
                        if data.len() == MAX_CHUNK {
                            break;
                        }
                    }
                    (
                        TerminalOutput {
                            offset: begin + data.len() as u64,
                            data,
                            truncated,
                            closed: t.host.is_stopped(),
                        },
                        t.host.clone(),
                        t.sequence.unwrap_or(0),
                    )
                };
                if !result.data.is_empty()
                    || result.closed
                    || result.truncated
                    || Instant::now() >= deadline
                {
                    return Ok(result);
                }
                wait_host.wait_for_frame(
                    wait_sequence,
                    deadline.saturating_duration_since(Instant::now()),
                );
            }
        })
    }
}
fn missing() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        "terminal does not exist in this environment",
    )
}
fn poisoned<T>(_: std::sync::PoisonError<T>) -> io::Error {
    io::Error::other("terminal runtime lock poisoned")
}

impl Drop for TerminalRuntime {
    fn drop(&mut self) {
        if let Ok(sessions) = self.sessions.get_mut() {
            sessions.clear();
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.host.shutdown();
    }
}
