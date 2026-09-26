//! Bounded, metadata-only trace for diagnosing intermittent screen flashes.
//! The UI and PTY threads enqueue records; a worker owns the file I/O.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, OnceLock,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const TRACE_BYTES: u64 = 4 * 1024 * 1024;
const TRACE_QUEUE: usize = 4096;
const TRACE_RETENTION: Duration = Duration::from_secs(3 * 24 * 60 * 60);
static SERVER_TRACE: OnceLock<Option<mpsc::SyncSender<String>>> =
    OnceLock::new();
static CLIENT_TRACE: OnceLock<Option<mpsc::SyncSender<String>>> =
    OnceLock::new();
static SERVER_DROPPED: AtomicU64 = AtomicU64::new(0);
static CLIENT_DROPPED: AtomicU64 = AtomicU64::new(0);

pub(crate) fn server_enabled() -> bool {
    SERVER_TRACE
        .get_or_init(|| start_writer("server"))
        .is_some()
}

pub(crate) fn server(event: impl std::fmt::Display) {
    record(&SERVER_TRACE, &SERVER_DROPPED, "server", event);
}

pub(crate) fn client(event: impl std::fmt::Display) {
    record(&CLIENT_TRACE, &CLIENT_DROPPED, "client", event);
}

fn record(
    trace: &OnceLock<Option<mpsc::SyncSender<String>>>,
    dropped: &AtomicU64,
    role: &'static str,
    event: impl std::fmt::Display,
) {
    let sender = trace.get_or_init(|| start_writer(role));
    if let Some(sender) = sender {
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros();
        let lost = dropped.swap(0, Ordering::Relaxed);
        let suffix = if lost == 0 {
            String::new()
        } else {
            format!(" dropped={lost}")
        };
        let line =
            format!("{time} pid={} {event}{suffix}\n", std::process::id());
        if let Err(mpsc::TrySendError::Full(_)) = sender.try_send(line) {
            dropped.fetch_add(lost + 1, Ordering::Relaxed);
        }
    }
}

fn start_writer(role: &str) -> Option<mpsc::SyncSender<String>> {
    let (path, cleanup_directory) =
        match std::env::var_os("ZMUX_TRACE_SCREEN_MODES") {
            Some(value) if value == "0" || value == "off" => return None,
            Some(value) if role == "server" => (PathBuf::from(value), None),
            Some(mut value) => {
                value.push(format!(".client-{}.log", std::process::id()));
                (PathBuf::from(value), None)
            }
            None => {
                let directory = std::env::temp_dir();
                (
                    directory.join(format!(
                        "zmux-screen-trace-{role}-{}.log",
                        std::process::id()
                    )),
                    Some(directory),
                )
            }
        };
    let (sender, receiver) = mpsc::sync_channel::<String>(TRACE_QUEUE);
    thread::Builder::new()
        .name("zmux-screen-trace".into())
        .spawn(move || {
            if let Some(directory) = cleanup_directory {
                cleanup_old_traces(&directory);
            }
            let mut writer = match RollingTrace::open(path.clone(), TRACE_BYTES)
            {
                Ok(writer) => writer,
                Err(error) => {
                    eprintln!(
                        "zmux: cannot open screen trace {}: {error}",
                        path.display()
                    );
                    return;
                }
            };
            while let Ok(line) = receiver.recv() {
                if let Err(error) = writer.append(&line) {
                    eprintln!(
                        "zmux: cannot write screen trace {}: {error}",
                        path.display()
                    );
                    break;
                }
            }
        })
        .ok()?;
    Some(sender)
}

struct RollingTrace {
    path: PathBuf,
    file: File,
    bytes: u64,
    max_bytes: u64,
}

impl RollingTrace {
    fn open(path: PathBuf, max_bytes: u64) -> io::Result<Self> {
        let file = open_private_append(&path)?;
        let bytes = file.metadata()?.len();
        let mut trace = Self {
            path,
            file,
            bytes,
            max_bytes,
        };
        if bytes >= max_bytes {
            trace.rotate()?;
        }
        Ok(trace)
    }

    fn append(&mut self, line: &str) -> io::Result<()> {
        if self.bytes + line.len() as u64 > self.max_bytes {
            self.rotate()?;
        }
        self.file.write_all(line.as_bytes())?;
        self.bytes += line.len() as u64;
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        let previous = self.path.with_extension("log.prev");
        // Copy before truncating so the preceding segment survives a crash
        // during rotation. Truncating the open append handle also works on
        // platforms that cannot rename an open file.
        self.file.flush()?;
        fs::copy(&self.path, previous)?;
        self.file.set_len(0)?;
        self.bytes = 0;
        Ok(())
    }
}

fn open_private_append(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn cleanup_old_traces(directory: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(pid) = name
            .strip_prefix("zmux-screen-trace-server-")
            .or_else(|| name.strip_prefix("zmux-screen-trace-client-"))
            .and_then(|name| {
                name.strip_suffix(".log.prev")
                    .or_else(|| name.strip_suffix(".log"))
            })
        else {
            continue;
        };
        if pid.is_empty() || !pid.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > TRACE_RETENTION);
        if old {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rolling_trace_keeps_previous_and_current_segments() {
        let dir = std::env::temp_dir().join(format!(
            "zmux-trace-test-{}-{:?}",
            std::process::id(),
            thread::current().id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("screen.log");
        let mut trace = RollingTrace::open(path.clone(), 16).unwrap();
        trace.append("first\n").unwrap();
        trace.append("second\n").unwrap();
        trace.append("third\n").unwrap();
        assert_eq!(
            fs::read_to_string(path.with_extension("log.prev")).unwrap(),
            "first\nsecond\n"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "third\n");
        trace.append("fourth\n").unwrap();
        trace.append("fifth\n").unwrap();
        assert_eq!(
            fs::read_to_string(path.with_extension("log.prev")).unwrap(),
            "third\nfourth\n"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "fifth\n");
        drop(trace);
        fs::remove_dir_all(dir).unwrap();
    }
}
