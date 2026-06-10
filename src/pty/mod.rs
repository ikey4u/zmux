#[cfg(windows)]
use std::io::Write;
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
    terminal::AlacrittyTermState,
    types::{Pane, PaneId},
};

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
    pub command: Option<&'a str>,
    pub start_dir: Option<&'a str>,
    pub env: Vec<(String, String)>,
    pub scroll_on_erase_in_display: bool,
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

    #[cfg(windows)]
    {
        if let Ok(mut w) = writer.lock() {
            send_dsr_response(&mut **w);
        }
    }

    let mut term_state = AlacrittyTermState::new(opts.rows, opts.cols, 2000);
    term_state.set_scroll_on_erase_in_display(opts.scroll_on_erase_in_display);
    let parser = Arc::new(Mutex::new(term_state));
    let data_version: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
    let cursor_shape: Arc<AtomicU8> =
        Arc::new(AtomicU8::new(CURSOR_SHAPE_UNSET));
    let bell_pending: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let output_ring: Arc<Mutex<VecDeque<u8>>> =
        Arc::new(Mutex::new(VecDeque::new()));
    let dead: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let reported_cwd: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    start_reader_thread(
        pair.master
            .try_clone_reader()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?,
        opts.pane_id,
        Arc::clone(&parser),
        Arc::clone(&data_version),
        Arc::clone(&cursor_shape),
        Arc::clone(&bell_pending),
        Arc::clone(&output_ring),
        Arc::clone(&dead),
        Arc::clone(&reported_cwd),
        Arc::clone(&writer),
    );

    Ok(Pane {
        id: opts.pane_id,
        master: pair.master,
        writer,
        child,
        parser,
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
        output_ring,
        reported_cwd,
        start_dir: opts.start_dir.map(|s| s.to_string()),
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
fn configure_windows_default_shell(shell: &str, cmd: &mut CommandBuilder) {
    if is_windows_powershell_shell(shell) {
        cmd.arg("-NoLogo");
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
fn windows_powershell_emacs_script() -> &'static str {
    r"function global:__zmux_emit_cwd { try { $p = (Get-Location).Path -replace '\\','/'; $e = [char]27; [Console]::Write([string]::Concat($e, ']7;file:///', $p, $e, '\')) } catch {} }; function global:prompt { __zmux_emit_cwd; 'PS ' + $executionContext.SessionState.Path.CurrentLocation + '> ' }; __zmux_emit_cwd; try { Import-Module PSReadLine -ErrorAction Stop; Set-PSReadLineOption -EditMode Emacs -ErrorAction Stop; function global:__zmux_bind($c,$f) { try { Set-PSReadLineKeyHandler -Chord $c -Function $f -ErrorAction Stop } catch {} }; __zmux_bind 'Ctrl+a' BeginningOfLine; __zmux_bind 'Ctrl+b' BackwardChar; __zmux_bind 'Ctrl+d' DeleteCharOrExit; __zmux_bind 'Ctrl+e' EndOfLine; __zmux_bind 'Ctrl+f' ForwardChar; __zmux_bind 'Ctrl+k' KillLine; __zmux_bind 'Ctrl+l' ClearScreen; __zmux_bind 'Ctrl+n' NextHistory; __zmux_bind 'Ctrl+p' PreviousHistory; __zmux_bind 'Ctrl+r' ReverseSearchHistory; __zmux_bind 'Ctrl+s' ForwardSearchHistory; __zmux_bind 'Ctrl+t' SwapCharacters; __zmux_bind 'Ctrl+u' BackwardKillInput; Remove-Item Function:\__zmux_bind -ErrorAction SilentlyContinue } catch {}"
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
    data_version: Arc<AtomicU64>,
    cursor_shape: Arc<AtomicU8>,
    bell_pending: Arc<AtomicBool>,
    output_ring: Arc<Mutex<VecDeque<u8>>>,
    dead_flag: Arc<AtomicBool>,
    reported_cwd: Arc<Mutex<Option<String>>>,
    pty_writer: PtyWriter,
) {
    thread::spawn(move || {
        let mut buf = [0u8; 65536];
        let mut cursor_tracker = CursorShapeTracker::default();
        let mut cwd_tracker = osc7::CwdTracker::default();
        let mut color_tracker =
            crate::terminal::osc_colors::OscColorTracker::default();
        let mut query_tracker = term_queries::TermQueryTracker::default();
        let render_debounce_seq = Arc::new(AtomicU64::new(0));
        let mut burst_window_start = Instant::now();
        let mut burst_bytes = 0usize;
        let mut suppress_until: Option<Instant> = None;
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => {
                    dead_flag.store(true, Ordering::Relaxed);
                    data_version.fetch_add(1, Ordering::Relaxed);
                    crate::types::events::mark_data_ready();
                    break;
                }
                Ok(n) => {
                    let data = &buf[..n];
                    let should_render = parser
                        .lock()
                        .map(|mut p| p.process(data))
                        .unwrap_or(true);
                    if let Ok(mut ring) = output_ring.lock() {
                        for &b in data {
                            if ring.len() >= 2 * 1024 * 1024 {
                                ring.pop_front();
                            }
                            ring.push_back(b);
                        }
                    }
                    for &b in data {
                        if b == 0x07 {
                            bell_pending.store(true, Ordering::Relaxed);
                        }
                    }
                    cursor_tracker.process(data, &cursor_shape);
                    cwd_tracker.process(data, &reported_cwd);
                    color_tracker.process(data, &parser);
                    query_tracker.process(data, &parser, &pty_writer);
                    data_version.fetch_add(1, Ordering::Relaxed);
                    let now = Instant::now();
                    if now.duration_since(burst_window_start)
                        > Duration::from_millis(50)
                    {
                        burst_window_start = now;
                        burst_bytes = 0;
                    }
                    burst_bytes = burst_bytes.saturating_add(n);
                    let high_volume = n >= 2048 || burst_bytes >= 8192;
                    if high_volume {
                        suppress_until = Some(now + Duration::from_millis(120));
                    }
                    let suppress_render =
                        suppress_until.is_some_and(|until| now < until);
                    if should_render && !suppress_render {
                        crate::types::events::mark_data_ready();
                    } else {
                        schedule_debounced_render(&render_debounce_seq);
                    }
                }
            }
        }
    });
}

fn schedule_debounced_render(seq: &Arc<AtomicU64>) {
    let generation = seq.fetch_add(1, Ordering::Relaxed) + 1;
    let seq = Arc::clone(seq);
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(120));
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
    pane.last_rows = rows;
    pane.last_cols = cols;
    Ok(())
}

#[cfg(windows)]
fn send_dsr_response(writer: &mut dyn Write) {
    let _ = writer.write_all(b"\x1b[1;1R");
    let _ = writer.flush();
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
            command: Some("/bin/cat"),
            start_dir: None,
            env: vec![],
            scroll_on_erase_in_display: false,
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
