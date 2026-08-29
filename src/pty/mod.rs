use std::{
    collections::VecDeque,
    io::{self},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
#[cfg(unix)]
use std::{os::fd::AsRawFd, path::Path};

use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};

use crate::{
    history_store::{PaneHistory, PaneHistoryWriter},
    terminal::AlacrittyTermState,
    types::{Pane, PaneId},
};

mod osc52;
mod osc7;
mod term_queries;

pub use term_queries::PtyWriter;

pub const CURSOR_SHAPE_UNSET: u8 = 255;

#[cfg(unix)]
static HOST_TERMIOS: Mutex<Option<libc::termios>> = Mutex::new(None);

pub struct SpawnOptions<'a> {
    pub pane_id: PaneId,
    pub rows: u16,
    pub cols: u16,
    pub history_limit: usize,
    pub command: Option<&'a str>,
    pub start_dir: Option<&'a str>,
    pub env: Vec<(String, String)>,
    pub scroll_on_erase_in_display: bool,
    pub zmux_socket: Option<&'a str>,
}

pub fn spawn_pane(opts: SpawnOptions<'_>) -> io::Result<Pane> {
    let pty_system = NativePtySystem::default();
    let size = PtySize {
        rows: opts.rows,
        cols: opts.cols,
        pixel_width: 0,
        pixel_height: 0,
    };
    let pair = pty_system
        .openpty(size)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

    #[cfg(unix)]
    configure_pty_nonblocking(&*pair.master);

    let shell = resolve_shell(opts.command);
    let mut cmd = CommandBuilder::new(&shell);

    #[cfg(unix)]
    if opts.command.is_none() {
        if is_zsh_shell(&shell) {
            cmd.arg("-o");
            cmd.arg("emacs");
        }
        cmd.arg("-i");
    }

    #[cfg(windows)]
    if opts.command.is_none() {
        configure_windows_default_shell(&shell, &mut cmd);
    }

    if let Some(dir) = opts.start_dir {
        cmd.cwd(dir);
    }

    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("ZMUX", "1");
    cmd.env("ZMUX_PANE", format!("%{}", opts.pane_id));
    if let Some(socket) = opts.zmux_socket {
        cmd.env("ZMUX_SOCKET", socket);
    }
    // Override host COLUMNS/LINES. Many tools (GNU ls, eza, some prompts) prefer
    // these over TIOCGWINSZ; inheriting the outer terminal width makes columnar
    // output paint as if the pane were full-screen, which then wraps into the
    // neighbouring pane.
    cmd.env("COLUMNS", opts.cols.to_string());
    cmd.env("LINES", opts.rows.to_string());

    for (k, v) in &opts.env {
        cmd.env(k, v);
    }

    #[cfg(unix)]
    apply_host_termios_to_slave(&*pair.master);

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    let child_pid = child.process_id();

    let writer = Arc::new(Mutex::new(
        pair.master
            .take_writer()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?,
    ));

    let mut term_state =
        AlacrittyTermState::new(opts.rows, opts.cols, opts.history_limit);
    term_state.set_scroll_on_erase_in_display(opts.scroll_on_erase_in_display);
    let parser = Arc::new(Mutex::new(term_state));
    let history = Arc::new(Mutex::new(
        PaneHistory::new().unwrap_or_else(|_| PaneHistory::disabled()),
    ));
    let history_writer = PaneHistoryWriter::start(Arc::clone(&history));
    let history_serial = Arc::new(Mutex::new(()));
    let data_version: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
    let cursor_shape: Arc<AtomicU8> =
        Arc::new(AtomicU8::new(CURSOR_SHAPE_UNSET));
    let bell_pending: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let pending_osc52: Arc<Mutex<VecDeque<Vec<u8>>>> =
        Arc::new(Mutex::new(VecDeque::new()));
    let dead: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let render_dirty: Arc<AtomicBool> = Arc::new(AtomicBool::new(true));
    let reported_cwd: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    start_reader_thread(
        pair.master
            .try_clone_reader()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?,
        opts.pane_id,
        Arc::clone(&parser),
        history_writer.clone(),
        Arc::clone(&history_serial),
        Arc::clone(&data_version),
        Arc::clone(&cursor_shape),
        Arc::clone(&bell_pending),
        Arc::clone(&pending_osc52),
        Arc::clone(&dead),
        Arc::clone(&render_dirty),
        Arc::clone(&reported_cwd),
        Arc::clone(&writer),
    );

    Ok(Pane {
        id: opts.pane_id,
        master: pair.master,
        writer,
        child,
        parser,
        history,
        history_writer,
        history_serial,
        last_rows: opts.rows,
        last_cols: opts.cols,
        title: String::new(),
        title_locked: false,
        child_pid,
        data_version,
        last_title_check: Instant::now(),
        dead,
        cursor_shape,
        bell_pending,
        copy_state: None,
        pending_osc52,
        reported_cwd,
        start_dir: opts.start_dir.map(|s| s.to_string()),
        render_dirty,
    })
}

#[cfg(unix)]
pub fn remember_host_termios() {
    let fd = io::stdin().as_raw_fd();
    let mut t = std::mem::MaybeUninit::<libc::termios>::uninit();
    if unsafe { libc::tcgetattr(fd, t.as_mut_ptr()) } != 0 {
        return;
    }
    if let Ok(mut slot) = HOST_TERMIOS.lock() {
        if slot.is_none() {
            *slot = Some(unsafe { t.assume_init() });
        }
    }
}

pub fn default_start_dir() -> Option<String> {
    std::env::current_dir()
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
}

pub fn pane_current_dir(pane: &Pane) -> Option<String> {
    pane.reported_cwd
        .lock()
        .ok()
        .and_then(|cwd| cwd.clone())
        .or_else(|| {
            pane.child_pid
                .or_else(|| pane.child.process_id())
                .and_then(process_current_dir)
        })
        .or_else(|| pane.start_dir.clone())
}

#[cfg(target_os = "linux")]
fn process_current_dir(pid: u32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
}

#[cfg(target_os = "macos")]
fn process_current_dir(pid: u32) -> Option<String> {
    let mut info = std::mem::MaybeUninit::<libc::proc_vnodepathinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
    let rc = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if rc != size {
        return None;
    }
    let info = unsafe { info.assume_init() };
    let ptr = info.pvi_cdir.vip_path.as_ptr().cast::<libc::c_char>();
    let path = unsafe { std::ffi::CStr::from_ptr(ptr) }.to_str().ok()?;
    if path.is_empty() {
        None
    } else {
        Some(path.to_string())
    }
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn process_current_dir(_pid: u32) -> Option<String> {
    None
}

#[cfg(windows)]
mod windows_cwd;

#[cfg(windows)]
fn process_current_dir(pid: u32) -> Option<String> {
    windows_cwd::process_current_dir(pid)
}

#[cfg(all(not(unix), not(windows)))]
fn process_current_dir(_pid: u32) -> Option<String> {
    None
}

fn resolve_shell(command: Option<&str>) -> String {
    if let Some(cmd) = command {
        return cmd.to_string();
    }
    #[cfg(unix)]
    {
        std::env::var("SHELL").unwrap_or_else(|_| {
            for candidate in &["/bin/zsh", "/bin/bash", "/bin/sh"] {
                if std::path::Path::new(candidate).exists() {
                    return candidate.to_string();
                }
            }
            "/bin/sh".to_string()
        })
    }
    #[cfg(windows)]
    {
        resolve_windows_shell()
    }
}

#[cfg(windows)]
fn resolve_windows_shell() -> String {
    find_windows_executable("pwsh")
        .or_else(|| find_windows_executable("powershell"))
        .or_else(|| find_windows_executable("cmd"))
        .or_else(|| std::env::var("COMSPEC").ok())
        .unwrap_or_else(|| "cmd.exe".to_string())
}

#[cfg(windows)]
fn windows_powershell_emacs_script() -> &'static str {
    r"function global:__zmux_emit_cwd { try { $p = (Get-Location).Path -replace '\\','/'; $e = [char]27; [Console]::Write([string]::Concat($e, ']7;file:///', $p, $e, '\')) } catch {} }; function global:prompt { __zmux_emit_cwd; 'PS ' + $executionContext.SessionState.Path.CurrentLocation + '> ' }; __zmux_emit_cwd; try { Import-Module PSReadLine -ErrorAction Stop; Set-PSReadLineOption -EditMode Emacs -ErrorAction Stop; function global:__zmux_bind($c,$f) { try { Set-PSReadLineKeyHandler -Chord $c -Function $f -ErrorAction Stop } catch {} }; __zmux_bind 'Ctrl+a' BeginningOfLine; __zmux_bind 'Ctrl+b' BackwardChar; __zmux_bind 'Ctrl+d' DeleteCharOrExit; __zmux_bind 'Ctrl+e' EndOfLine; __zmux_bind 'Ctrl+f' ForwardChar; __zmux_bind 'Ctrl+k' KillLine; __zmux_bind 'Ctrl+l' ClearScreen; __zmux_bind 'Ctrl+n' NextHistory; __zmux_bind 'Ctrl+p' PreviousHistory; __zmux_bind 'Ctrl+r' ReverseSearchHistory; __zmux_bind 'Ctrl+s' ForwardSearchHistory; __zmux_bind 'Ctrl+t' SwapCharacters; __zmux_bind 'Ctrl+u' BackwardKillInput; Remove-Item Function:\__zmux_bind -ErrorAction SilentlyContinue } catch {}"
}

#[cfg(windows)]
fn configure_windows_default_shell(shell: &str, cmd: &mut CommandBuilder) {
    if is_windows_powershell_shell(shell) {
        cmd.arg("-NoLogo");
        cmd.arg("-NoProfile");
        cmd.arg("-NoExit");
        cmd.arg("-Command");
        cmd.arg(windows_powershell_emacs_script());
    } else if is_windows_cmd_shell(shell) {
        cmd.arg("/K");
        cmd.arg(r"prompt $E]7;file://$P$E\$G");
    }
}

#[cfg(windows)]
fn is_windows_cmd_shell(shell: &str) -> bool {
    let Some(name) = std::path::Path::new(shell)
        .file_stem()
        .and_then(|name| name.to_str())
    else {
        return false;
    };
    name.eq_ignore_ascii_case("cmd")
}

#[cfg(windows)]
fn is_windows_powershell_shell(shell: &str) -> bool {
    let Some(name) = std::path::Path::new(shell)
        .file_stem()
        .and_then(|name| name.to_str())
    else {
        return false;
    };
    matches!(name.to_ascii_lowercase().as_str(), "pwsh" | "powershell")
}

#[cfg(windows)]
fn find_windows_executable(name: &str) -> Option<String> {
    let path = std::path::Path::new(name);
    if path.components().count() > 1 && path.is_file() {
        return Some(path.to_string_lossy().into_owned());
    }

    let path_env = std::env::var_os("PATH")?;
    let pathext = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
    let names = if path.extension().is_some() {
        vec![name.to_string()]
    } else {
        pathext
            .split(';')
            .filter(|ext| !ext.is_empty())
            .map(|ext| {
                if ext.starts_with('.') {
                    format!("{}{}", name, ext)
                } else {
                    format!("{}.{}", name, ext)
                }
            })
            .collect()
    };

    for dir in std::env::split_paths(&path_env) {
        for candidate_name in &names {
            let candidate = dir.join(candidate_name);
            if candidate.is_file() {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
    }
    None
}

#[cfg(unix)]
fn is_zsh_shell(shell: &str) -> bool {
    matches!(
        Path::new(shell).file_name().and_then(|s| s.to_str()),
        Some("zsh")
    )
}

fn start_reader_thread(
    mut reader: Box<dyn io::Read + Send>,
    _pane_id: PaneId,
    parser: Arc<Mutex<AlacrittyTermState>>,
    history_writer: PaneHistoryWriter,
    history_serial: Arc<Mutex<()>>,
    data_version: Arc<AtomicU64>,
    cursor_shape: Arc<AtomicU8>,
    bell_pending: Arc<AtomicBool>,
    pending_osc52: Arc<Mutex<VecDeque<Vec<u8>>>>,
    dead_flag: Arc<AtomicBool>,
    render_dirty: Arc<AtomicBool>,
    reported_cwd: Arc<Mutex<Option<String>>>,
    pty_writer: PtyWriter,
) {
    thread::spawn(move || {
        let mut buf = [0u8; 65536];
        let mut cursor_tracker = CursorShapeTracker::default();
        let mut cwd_tracker = osc7::CwdTracker::default();
        let mut osc52_tracker = osc52::Osc52Tracker::default();
        let mut color_tracker =
            crate::terminal::osc_colors::OscColorTracker::default();
        let mut query_tracker = term_queries::TermQueryTracker::default();
        let render_debounce_seq = Arc::new(AtomicU64::new(0));
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    dead_flag.store(true, Ordering::Relaxed);
                    data_version.fetch_add(1, Ordering::Relaxed);
                    render_dirty.store(true, Ordering::Relaxed);
                    crate::types::events::mark_data_ready();
                    break;
                }
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::TimedOut =>
                {
                    thread::sleep(Duration::from_millis(16));
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    dead_flag.store(true, Ordering::Relaxed);
                    data_version.fetch_add(1, Ordering::Relaxed);
                    render_dirty.store(true, Ordering::Relaxed);
                    crate::types::events::mark_data_ready();
                    break;
                }
                Ok(n) => {
                    let data = &buf[..n];
                    let _history_guard = history_serial
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    let (should_render, mut history_rows, clear_history) =
                        parser
                            .lock()
                            .map(|mut parser| {
                                let should_render = parser.process(data);
                                let rows = parser.take_history_rows();
                                let clear_history =
                                    parser.take_history_clear_requested();
                                (should_render, rows, clear_history)
                            })
                            .unwrap_or_else(|_| (true, Vec::new(), false));
                    for &b in data {
                        if b == 0x07 {
                            bell_pending.store(true, Ordering::Relaxed);
                        }
                    }
                    cursor_tracker.process(data, &cursor_shape);
                    cwd_tracker.process(data, &reported_cwd);
                    osc52_tracker.process(data, &pending_osc52);
                    color_tracker.process(data, &parser);
                    query_tracker.process(data, &parser, &pty_writer);
                    let mut post_query_rows = parser
                        .lock()
                        .map(|mut parser| parser.take_history_rows())
                        .unwrap_or_default();
                    history_rows.append(&mut post_query_rows);
                    data_version.fetch_add(1, Ordering::Relaxed);
                    render_dirty.store(true, Ordering::Relaxed);
                    crate::types::events::mark_data_ready();
                    if !should_render {
                        schedule_debounced_render(&render_debounce_seq);
                    }
                    if clear_history {
                        // The terminal capture split discarded only rows before
                        // the exact clear boundary. Queue the durable clear
                        // first so FIFO ordering preserves rows appended below
                        // without making the PTY reader wait for SQLite.
                        history_writer.clear();
                    }
                    // The screen can repaint from the hot parser immediately;
                    // cold-history I/O must not delay that notification.
                    persist_history_rows(&history_writer, history_rows);
                }
            }
        }
    });
}

/// Queue terminal rows captured outside the parser's hot scrollback. SQLite
/// work happens on a bounded worker so a slow or failed disk cannot block PTY
/// reads or grow an unbounded retry buffer.
fn persist_history_rows(
    history_writer: &PaneHistoryWriter,
    rows: Vec<crate::terminal::TerminalHistoryRow>,
) {
    if rows.is_empty() {
        return;
    }

    let lines = crate::copy_mode::snapshot_lines_from_history_rows(&rows);
    history_writer.append(lines);
}

/// Flush rows captured by non-reader paths such as resize or synchronized
/// output timeout handling.
pub(crate) fn persist_pending_history(pane: &Pane) {
    let _history_guard = pane
        .history_serial
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    persist_pending_history_serialized(pane);
}

/// Drain captured rows while the caller holds `pane.history_serial` across a
/// larger consistency window (for example parser snapshot + cold-tail read).
pub(crate) fn persist_pending_history_serialized(pane: &Pane) {
    let rows = pane
        .parser
        .lock()
        .map(|mut parser| parser.take_history_rows())
        .unwrap_or_default();
    persist_history_rows(&pane.history_writer, rows);
}

const RENDER_DEBOUNCE_MS: u64 = 16;

fn schedule_debounced_render(seq: &Arc<AtomicU64>) {
    let generation = seq.fetch_add(1, Ordering::Relaxed) + 1;
    let seq = Arc::clone(seq);
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(RENDER_DEBOUNCE_MS));
        if seq.load(Ordering::Relaxed) == generation {
            crate::types::events::mark_data_ready();
        }
    });
}

#[derive(Default)]
struct CursorShapeTracker {
    alt_screen: bool,
    saved_cursor_shape: Option<u8>,
    pending_escape: Vec<u8>,
}

impl CursorShapeTracker {
    fn process(&mut self, data: &[u8], cursor_shape: &AtomicU8) {
        let mut current_shape = cursor_shape.load(Ordering::Relaxed);
        let mut bytes = std::mem::take(&mut self.pending_escape);
        bytes.extend_from_slice(data);
        let mut i = 0usize;
        while i < bytes.len() {
            if bytes[i] != 0x1b {
                i += 1;
                continue;
            }
            if i + 1 >= bytes.len() {
                self.pending_escape.extend_from_slice(&bytes[i..]);
                break;
            }
            if bytes[i + 1] != b'[' {
                i += 1;
                continue;
            }
            let start = i + 2;
            let Some(rel_end) = bytes[start..]
                .iter()
                .position(|&b| (0x40..=0x7e).contains(&b))
            else {
                self.pending_escape.extend_from_slice(&bytes[i..]);
                break;
            };
            let end = start + rel_end;
            self.apply_csi(&bytes[start..end], bytes[end], &mut current_shape);
            i = end + 1;
        }
        cursor_shape.store(current_shape, Ordering::Relaxed);
    }

    fn apply_csi(
        &mut self,
        params: &[u8],
        final_byte: u8,
        current_shape: &mut u8,
    ) {
        match final_byte {
            b'q' => {
                if let Some(shape) = parse_cursor_shape(params) {
                    *current_shape = shape;
                }
            }
            b'h' | b'l' => {
                if !is_alt_screen_mode(params) {
                    return;
                }
                if final_byte == b'h' {
                    if !self.alt_screen {
                        self.saved_cursor_shape = Some(*current_shape);
                    }
                    self.alt_screen = true;
                } else {
                    if self.alt_screen {
                        *current_shape = self
                            .saved_cursor_shape
                            .take()
                            .unwrap_or(CURSOR_SHAPE_UNSET);
                    }
                    self.alt_screen = false;
                }
            }
            _ => {}
        }
    }
}

fn parse_cursor_shape(params: &[u8]) -> Option<u8> {
    if params.starts_with(b"?") {
        return None;
    }
    let s = std::str::from_utf8(params).ok()?.trim();
    if s.is_empty() {
        Some(0)
    } else {
        s.parse::<u8>().ok()
    }
}

fn is_alt_screen_mode(params: &[u8]) -> bool {
    let Some(rest) = params.strip_prefix(b"?") else {
        return false;
    };
    rest.split(|&b| b == b';').any(|mode| {
        matches!(
            std::str::from_utf8(mode).ok(),
            Some("47") | Some("1047") | Some("1049")
        )
    })
}

pub fn resize_pane(pane: &mut Pane, rows: u16, cols: u16) -> io::Result<()> {
    let rows = rows.max(1);
    let cols = cols.max(1);
    if rows == pane.last_rows && cols == pane.last_cols {
        return Ok(());
    }
    let size = PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    };
    pane.master
        .resize(size)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

    if let Ok(mut p) = pane.parser.lock() {
        p.resize(rows, cols);
        p.scrollback_bottom();
    }
    persist_pending_history(pane);
    pane.last_rows = rows;
    pane.last_cols = cols;
    Ok(())
}

/// Non-blocking PTY master writes so input forwarding never blocks a server
/// worker thread indefinitely when the child is busy writing and not reading stdin.
#[cfg(unix)]
fn configure_pty_nonblocking(master: &dyn portable_pty::MasterPty) {
    let Some(fd) = master.as_raw_fd() else {
        return;
    };
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 {
            return;
        }
        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
}

#[cfg(unix)]
fn apply_host_termios_to_slave(master: &dyn portable_pty::MasterPty) {
    let path = match master.tty_name() {
        Some(path) => path,
        None => return,
    };

    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    {
        Ok(file) => file,
        Err(_) => return,
    };

    let host_termios = HOST_TERMIOS.lock().ok().and_then(|slot| slot.clone());

    let Some(mut host_termios) = host_termios else {
        return;
    };

    host_termios.c_iflag &= !(libc::IXON as libc::tcflag_t);

    unsafe {
        libc::tcsetattr(file.as_raw_fd(), libc::TCSANOW, &host_termios);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;

    #[test]
    fn restores_saved_shape_after_alt_screen_exit() {
        let cursor_shape = AtomicU8::new(6);
        let mut tracker = CursorShapeTracker::default();

        tracker.process(b"\x1b[?1049h\x1b[2 q", &cursor_shape);
        assert_eq!(cursor_shape.load(Ordering::Relaxed), 2);

        tracker.process(b"\x1b[?1049l", &cursor_shape);
        assert_eq!(cursor_shape.load(Ordering::Relaxed), 6);
    }

    #[test]
    fn post_exit_shape_sequence_overrides_restored_shape() {
        let cursor_shape = AtomicU8::new(6);
        let mut tracker = CursorShapeTracker::default();

        tracker
            .process(b"\x1b[?1049h\x1b[2 q\x1b[?1049l\x1b[5 q", &cursor_shape);

        assert_eq!(cursor_shape.load(Ordering::Relaxed), 5);
    }

    #[test]
    fn split_sequences_still_restore_saved_shape() {
        let cursor_shape = AtomicU8::new(5);
        let mut tracker = CursorShapeTracker::default();

        tracker.process(b"\x1b[?1049", &cursor_shape);
        tracker.process(b"h\x1b[2 q", &cursor_shape);
        assert_eq!(cursor_shape.load(Ordering::Relaxed), 2);

        tracker.process(b"\x1b[?1049", &cursor_shape);
        tracker.process(b"l", &cursor_shape);
        assert_eq!(cursor_shape.load(Ordering::Relaxed), 5);
    }

    #[test]
    fn captured_terminal_rows_are_persisted_outside_the_parser_lock() {
        let parser = Arc::new(Mutex::new(AlacrittyTermState::new(2, 20, 1)));
        let state_dir = std::env::temp_dir()
            .join(format!("zmux-pty-history-test-{}", std::process::id()));
        let history =
            Arc::new(Mutex::new(PaneHistory::for_test(state_dir, 100)));
        let history_writer = PaneHistoryWriter::start(Arc::clone(&history));
        let rows = {
            let mut parser = parser.lock().unwrap();
            parser.process(b"zero\r\none\r\ntwo\r\nthree");
            parser.take_history_rows()
        };

        persist_history_rows(&history_writer, rows);
        history_writer.flush();

        let stored = history.lock().unwrap().tail(10).unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].text, "zero");
        assert!(parser.lock().unwrap().take_history_rows().is_empty());
        history_writer.shutdown_and_discard();
    }

    #[test]
    fn lowering_hot_limit_preserves_order_across_cold_and_hot_tiers() {
        let parser = Arc::new(Mutex::new(AlacrittyTermState::new(2, 20, 6)));
        let state_dir = std::env::temp_dir().join(format!(
            "zmux-pty-lower-history-test-{}",
            std::process::id()
        ));
        let history =
            Arc::new(Mutex::new(PaneHistory::for_test(state_dir, 100)));
        let history_writer = PaneHistoryWriter::start(Arc::clone(&history));

        {
            let mut parser = parser.lock().unwrap();
            parser.process(
                b"line-0\r\nline-1\r\nline-2\r\nline-3\r\nline-4\r\nline-5\r\nline-6\r\nline-7\r\nline-8\r\nline-9",
            );
            persist_history_rows(&history_writer, parser.take_history_rows());
            parser.set_scrollback_limit(2);
            persist_history_rows(&history_writer, parser.take_history_rows());
        }
        history_writer.flush();

        let stored = history.lock().unwrap().tail(100).unwrap();
        assert_eq!(
            stored
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            vec!["line-0", "line-1", "line-2", "line-3", "line-4", "line-5"]
        );
        assert_eq!(parser.lock().unwrap().snapshot_rows().0.len(), 4);
        history_writer.shutdown_and_discard();
    }

    #[cfg(unix)]
    #[test]
    fn spawn_pane_uses_requested_history_limit() -> io::Result<()> {
        let mut pane = spawn_pane(SpawnOptions {
            pane_id: 1,
            rows: 8,
            cols: 20,
            history_limit: 37,
            command: Some("/bin/cat"),
            start_dir: None,
            env: vec![],
            scroll_on_erase_in_display: false,
            zmux_socket: None,
        })?;
        let actual = pane
            .parser
            .lock()
            .map(|parser| parser.scrollback_limit())
            .unwrap_or_default();
        let _ = pane.child.kill();

        assert_eq!(actual, 37);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn dropping_pane_stops_history_worker_and_cleans_shared_database_scope(
    ) -> io::Result<()> {
        let directory = std::env::temp_dir().join(format!(
            "zmux-pane-drop-history-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        let pane = spawn_pane(SpawnOptions {
            pane_id: 1,
            rows: 8,
            cols: 20,
            history_limit: 2,
            command: Some("/bin/cat"),
            start_dir: None,
            env: vec![],
            scroll_on_erase_in_display: false,
            zmux_socket: None,
        })?;
        *pane.history.lock().unwrap() =
            PaneHistory::for_test(directory.clone(), 100);
        pane.history_writer.append(vec![crate::types::SnapshotLine {
            text: "archived".to_string(),
            terminated: true,
            styles: Vec::new(),
        }]);
        pane.history_writer.flush();
        assert!(std::fs::read_dir(&directory)?.any(|entry| entry
            .ok()
            .is_some_and(|entry| entry
                .path()
                .extension()
                .is_some_and(|ext| ext == "sqlite3"))));

        drop(pane);

        let database = directory.join("zmux.sqlite3");
        assert!(database.exists());
        let connection = rusqlite::Connection::open(database).unwrap();
        let retained: i64 = connection
            .query_row("SELECT COUNT(*) FROM history_lines", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(retained, 0);
        let _ = std::fs::remove_dir_all(directory);
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_default_shell_emits_prompt() -> io::Result<()> {
        let started = Instant::now();
        let mut pane = spawn_pane(SpawnOptions {
            pane_id: 1,
            rows: 24,
            cols: 80,
            history_limit: 2_000,
            command: None,
            start_dir: None,
            env: vec![],
            scroll_on_erase_in_display: false,
            zmux_socket: None,
        })?;
        let deadline = started + Duration::from_secs(15);
        let mut saw_bytes = false;
        while Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
            if pane.data_version.load(Ordering::Relaxed) > 0 {
                saw_bytes = true;
            }
            let visible = pane
                .parser
                .lock()
                .map(|mut parser| {
                    parser.flush_sync_for_display();
                    parser
                        .visible_rows()
                        .into_iter()
                        .flat_map(|row| {
                            row.into_iter()
                                .filter_map(|cell| cell.map(|cell| cell.text))
                        })
                        .collect::<String>()
                })
                .unwrap_or_default();
            if visible.contains('>') {
                eprintln!(
                    "windows prompt visible after {:?} (saw_bytes={saw_bytes})",
                    started.elapsed()
                );
                let _ = pane.child.kill();
                return Ok(());
            }
        }
        let _ = pane.child.kill();
        panic!(
            "windows prompt did not appear within 15s (saw_bytes={saw_bytes})"
        );
    }

    #[cfg(unix)]
    #[test]
    fn resize_shrink_hidden_cursor_pane_scrolls_to_bottom() -> io::Result<()> {
        let mut parser = AlacrittyTermState::new(8, 20, 2000);
        parser.process(b"\x1b[?25l");
        let mut output = String::new();
        for i in 0..20 {
            output.push_str(&format!("line {i}\r\n"));
        }
        parser.process(output.as_bytes());
        parser.scrollback_top();

        let mut pane = spawn_pane(SpawnOptions {
            pane_id: 1,
            rows: 8,
            cols: 20,
            history_limit: 2_000,
            command: Some("/bin/cat"),
            start_dir: None,
            env: vec![],
            scroll_on_erase_in_display: false,
            zmux_socket: None,
        })?;
        pane.parser = Arc::new(Mutex::new(parser));
        resize_pane(&mut pane, 4, 20)?;

        let first_visible = pane
            .parser
            .lock()
            .unwrap()
            .visible_rows()
            .into_iter()
            .next()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|cell| cell.map(|cell| cell.text))
            .collect::<String>();
        let _ = pane.child.kill();
        assert_ne!(first_visible.trim_end(), "line 0");
        Ok(())
    }
}
