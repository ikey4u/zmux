use std::{
    collections::{HashMap, VecDeque},
    io::{self, Write},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use base64::{engine::general_purpose::STANDARD, Engine as _};

/// Makes client-requested full repaints distinct from an otherwise identical
/// incremental frame so the polling client cannot deduplicate them away.
static OVERLAY_RESTORE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

const FRAME_HISTORY_ENTRIES: usize = 512;
const FRAME_HISTORY_BYTES: usize = 16 * 1024 * 1024;

#[derive(Default)]
struct FrameStore {
    latest: Option<FrameData>,
    sequence: u64,
    ansi_history: VecDeque<(u64, Vec<u8>)>,
    ansi_history_bytes: usize,
    history_floor: u64,
}

impl FrameStore {
    fn publish(&mut self, frame: FrameData) {
        self.sequence = self.sequence.wrapping_add(1);
        if let Some(encoded) = frame.ansi.as_deref() {
            if let Ok(bytes) = STANDARD.decode(encoded) {
                if !bytes.is_empty() {
                    self.ansi_history_bytes += bytes.len();
                    self.ansi_history.push_back((self.sequence, bytes));
                }
            }
        }
        while self.ansi_history.len() > FRAME_HISTORY_ENTRIES
            || self.ansi_history_bytes > FRAME_HISTORY_BYTES
        {
            let Some((sequence, bytes)) = self.ansi_history.pop_front() else {
                break;
            };
            self.history_floor = sequence;
            self.ansi_history_bytes =
                self.ansi_history_bytes.saturating_sub(bytes.len());
        }
        self.latest = Some(frame);
    }

    fn frame_since(&self, sequence: u64) -> (Option<FrameData>, u64, bool) {
        let mut frame = self.latest.clone();
        let missed_history = sequence < self.history_floor;
        if let Some(frame) = frame.as_mut() {
            let mut ansi = Vec::new();
            for (_, bytes) in self
                .ansi_history
                .iter()
                .filter(|(published, _)| *published > sequence)
            {
                ansi.extend_from_slice(bytes);
            }
            // Never replay `latest.ansi` when there are no new publications.
            // Each frame connection receives only deltas newer than its own
            // acknowledged sequence.
            frame.ansi = Some(STANDARD.encode(ansi));
        }
        (frame, self.sequence, missed_history)
    }
}

use crate::{
    client::FrameData,
    commands::ParsedCommand,
    layout::{
        compute_rects, first_leaf_path, kill_pane_at_path, serialize_frame,
        split_node, BORDER_SIZE,
    },
    output::{
        encode_ansi_base64, frame_ansi_area, layout_fingerprint,
        serialize_frame_ansi, FrameAnsiOptions,
    },
    pty::{resize_pane, spawn_pane, SpawnOptions},
    types::{
        events::{mark_data_ready, PTY_DATA_READY},
        layout::{LayoutNode, Rect, SplitDirection},
        options::{SessionOptions, WindowOptions, MAX_HISTORY_LIMIT},
        session::{PaneId, Server, Session, Size, Window},
    },
};

#[derive(Debug, Clone, PartialEq)]
pub enum SessionTreeEntry {
    Session {
        name: String,
        window_count: usize,
        is_active: bool,
    },
    Window {
        session_name: String,
        index: usize,
        name: String,
        pane_count: usize,
        is_active: bool,
    },
    Pane {
        session_name: String,
        window_index: usize,
        pane_id: usize,
        index: usize,
        is_active: bool,
    },
}

pub struct InProcessServer {
    state: Arc<Mutex<Server>>,
    latest_frame: Arc<Mutex<FrameStore>>,
    size: Arc<Mutex<Size>>,
}

impl InProcessServer {
    pub fn start(
        session_name: String,
        size: Size,
        socket_name: Option<String>,
        start_dir: Option<String>,
    ) -> io::Result<Self> {
        let state = Arc::new(Mutex::new(Server::new()));
        let latest_frame = Arc::new(Mutex::new(FrameStore::default()));
        let size_arc = Arc::new(Mutex::new(size));

        {
            let mut s = state.lock().unwrap();
            if let Err(e) = create_initial_session(
                &mut s,
                &session_name,
                size,
                start_dir.as_deref(),
            ) {
                log_server(&format!("create initial session failed: {}", e));
                return Err(e);
            }
        }
        log_server("initial session created");

        let state2 = Arc::clone(&state);
        let frame2 = Arc::clone(&latest_frame);
        let size2 = Arc::clone(&size_arc);

        thread::spawn(move || {
            render_loop(state2, frame2, size2, socket_name);
        });

        Ok(Self {
            state,
            latest_frame,
            size: size_arc,
        })
    }

    pub fn latest_frame(&self) -> Option<FrameData> {
        self.latest_frame.lock().ok()?.latest.clone()
    }

    pub fn send_input(&self, bytes: &[u8]) {
        let writer = clone_active_pane_writer(&self.state);
        let Some(writer) = writer else {
            return;
        };
        if let Err(e) = write_pty_writer(&writer, bytes) {
            log_server(&format!("input write failed: {}", e));
        } else {
            mark_data_ready();
        }
    }

    pub fn run_command(&self, cmd: &str) {
        let sz = self.size.lock().map(|s| *s).unwrap_or(Size::new(24, 80));
        let mut s = match self.state.lock() {
            Ok(s) => s,
            Err(_) => return,
        };
        execute_command_string(&mut s, cmd, sz);
    }

    pub fn run_command_with_output(&self, cmd: &str) -> String {
        let sz = self.size.lock().map(|s| *s).unwrap_or(Size::new(24, 80));
        let mut s = match self.state.lock() {
            Ok(s) => s,
            Err(_) => return String::new(),
        };
        execute_command_with_output(&mut s, cmd, sz)
    }

    pub fn resize(&self, new_size: Size) {
        if let Ok(mut sz) = self.size.lock() {
            *sz = new_size;
        }
        if let Ok(mut s) = self.state.lock() {
            resize_all_panes(&mut s, new_size);
        }
        schedule_delayed_frame_refresh(
            &self.state,
            &self.latest_frame,
            &self.size,
            new_size,
        );
    }

    pub fn set_hide_borders(&self, hide: bool) {
        let sz = self.size.lock().map(|s| *s).unwrap_or(Size::new(24, 80));
        if let Ok(mut s) = self.state.lock() {
            if s.hide_borders != hide {
                s.hide_borders = hide;
                resize_all_panes(&mut s, sz);
            }
        }
        mark_data_ready();
    }

    pub fn scroll_on_erase_in_display(&self) -> bool {
        self.state
            .lock()
            .map(|s| s.options.scroll_on_erase_in_display)
            .unwrap_or(false)
    }

    pub fn set_scroll_on_erase_in_display(&self, enabled: bool) {
        if let Ok(mut s) = self.state.lock() {
            set_scroll_on_erase_in_display(&mut s, enabled);
        }
        mark_data_ready();
    }

    pub fn is_empty(&self) -> bool {
        self.state
            .lock()
            .map(|s| server_is_empty(&s))
            .unwrap_or(true)
    }

    pub fn active_window_name(&self) -> String {
        self.state
            .lock()
            .ok()
            .and_then(|s| {
                let sess = s.active_session()?;
                sess.windows
                    .get(sess.active_window_idx)
                    .map(|w| w.name.clone())
            })
            .unwrap_or_default()
    }

    pub fn session_name(&self) -> String {
        self.state
            .lock()
            .ok()
            .and_then(|s| s.active_session().map(|sess| sess.name.clone()))
            .unwrap_or_default()
    }

    pub fn list_sessions(&self) -> Vec<(String, usize, bool)> {
        self.state
            .lock()
            .ok()
            .map(|s| {
                let active_idx = s.active_session_idx;
                s.sessions
                    .iter()
                    .enumerate()
                    .map(|(i, sess)| {
                        (sess.name.clone(), sess.windows.len(), i == active_idx)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn session_tree(&self) -> Vec<SessionTreeEntry> {
        self.state
            .lock()
            .ok()
            .map(|s| {
                let active_sess_idx = s.active_session_idx;
                s.sessions
                    .iter()
                    .enumerate()
                    .flat_map(|(si, sess)| {
                        let active_win_idx = sess.active_window_idx;
                        let is_active_sess = si == active_sess_idx;
                        let mut entries = vec![SessionTreeEntry::Session {
                            name: sess.name.clone(),
                            window_count: sess.windows.len(),
                            is_active: is_active_sess,
                        }];
                        for (wi, win) in sess.windows.iter().enumerate() {
                            let pane_ids =
                                crate::layout::collect_pane_ids(&win.root);
                            let active_pane_id = crate::layout::active_pane(
                                &win.root,
                                &win.active_pane_path,
                            )
                            .map(|p| p.id);
                            let is_active_win =
                                is_active_sess && wi == active_win_idx;
                            entries.push(SessionTreeEntry::Window {
                                session_name: sess.name.clone(),
                                index: wi,
                                name: win.name.clone(),
                                pane_count: pane_ids.len(),
                                is_active: is_active_win,
                            });
                            for (pi, &pane_id) in pane_ids.iter().enumerate() {
                                entries.push(SessionTreeEntry::Pane {
                                    session_name: sess.name.clone(),
                                    window_index: wi,
                                    pane_id,
                                    index: pi,
                                    is_active: is_active_win
                                        && Some(pane_id) == active_pane_id,
                                });
                            }
                        }
                        entries
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn enter_copy_mode(&self) -> bool {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(_) => return false,
        };
        let changed = with_active_pane_mut(&mut state, |pane| {
            crate::copy_mode::enter(pane)
        })
        .unwrap_or(false);
        if changed {
            mark_data_ready();
        }
        changed
    }

    pub fn exit_copy_mode(&self) {
        if let Ok(mut state) = self.state.lock() {
            let changed = with_active_pane_mut(&mut state, |pane| {
                let active = pane.copy_state.is_some();
                crate::copy_mode::exit(pane);
                active
            })
            .unwrap_or(false);
            if changed {
                state.force_clear_display = true;
                mark_data_ready();
            }
        }
    }

    pub fn copy_move_left(&self) {
        self.apply_copy_mutation(crate::copy_mode::move_left);
    }

    pub fn copy_move_right(&self) {
        self.apply_copy_mutation(crate::copy_mode::move_right);
    }

    pub fn copy_move_up(&self) {
        self.apply_copy_mutation(crate::copy_mode::move_up);
    }

    pub fn copy_move_down(&self) {
        self.apply_copy_mutation(crate::copy_mode::move_down);
    }

    pub fn copy_page_up(&self) {
        self.apply_copy_mutation(crate::copy_mode::page_up);
    }

    pub fn copy_page_down(&self) {
        self.apply_copy_mutation(crate::copy_mode::page_down);
    }

    pub fn copy_move_to_top(&self) {
        self.apply_copy_mutation(crate::copy_mode::move_to_top);
    }

    pub fn copy_move_to_bottom(&self) {
        self.apply_copy_mutation(crate::copy_mode::move_to_bottom);
    }

    pub fn copy_move_to_line_start(&self) {
        self.apply_copy_mutation(crate::copy_mode::move_to_line_start);
    }

    pub fn copy_move_to_line_end(&self) {
        self.apply_copy_mutation(crate::copy_mode::move_to_line_end);
    }

    pub fn copy_move_word_backward(&self) {
        self.apply_copy_mutation(crate::copy_mode::move_word_backward);
    }

    pub fn copy_move_word_forward(&self) {
        self.apply_copy_mutation(crate::copy_mode::move_word_forward);
    }

    pub fn copy_move_word_end(&self) {
        self.apply_copy_mutation(crate::copy_mode::move_word_end);
    }

    pub fn copy_start_selection(&self, mode: crate::types::SelectionMode) {
        self.apply_copy_mutation(|pane| {
            crate::copy_mode::start_selection(pane, mode)
        });
    }

    pub fn copy_clear_selection(&self) {
        self.apply_copy_mutation(crate::copy_mode::clear_selection);
    }

    pub fn copy_search(&self, query: String, forward: bool) -> bool {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(_) => return false,
        };
        let changed = with_active_pane_mut(&mut state, |pane| {
            crate::copy_mode::search(pane, query, forward)
        })
        .unwrap_or(false);
        if changed {
            mark_data_ready();
        }
        changed
    }

    pub fn copy_search_next(&self) -> bool {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(_) => return false,
        };
        let changed =
            with_active_pane_mut(&mut state, crate::copy_mode::search_next)
                .unwrap_or(false);
        if changed {
            mark_data_ready();
        }
        changed
    }

    pub fn copy_search_prev(&self) -> bool {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(_) => return false,
        };
        let changed =
            with_active_pane_mut(&mut state, crate::copy_mode::search_prev)
                .unwrap_or(false);
        if changed {
            mark_data_ready();
        }
        changed
    }

    pub fn copy_yank_selection(&self) -> String {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(_) => return String::new(),
        };
        let text =
            with_active_pane_mut(&mut state, crate::copy_mode::yank_selection)
                .unwrap_or_default();
        if !text.is_empty() {
            mark_data_ready();
        }
        text
    }

    fn apply_copy_mutation(&self, f: impl FnOnce(&mut crate::types::Pane)) {
        if let Ok(mut state) = self.state.lock() {
            let changed = with_active_pane_mut(&mut state, f).is_some();
            if changed {
                mark_data_ready();
            }
        }
    }

    pub fn run_socket_server(&self, socket_name: &str) -> io::Result<()> {
        use crate::ipc::bind_server;

        let listener = bind_server(socket_name)?;
        log_server(&format!("listening on socket {}", socket_name));
        #[cfg(unix)]
        let socket_name = socket_name.to_string();
        for stream in listener.incoming() {
            let stream = match stream {
                Ok(s) => s,
                Err(_) => continue,
            };
            let state = Arc::clone(&self.state);
            let latest_frame = Arc::clone(&self.latest_frame);
            let size_arc = Arc::clone(&self.size);
            let state_check = Arc::clone(&self.state);
            let size_check = Arc::clone(&self.size);
            #[cfg(unix)]
            let socket_name_clone = socket_name.clone();
            thread::spawn(move || {
                let _ = handle_client(stream, state, latest_frame, size_arc);
                let is_empty = state_check
                    .lock()
                    .map(|mut s| {
                        let sz = size_check
                            .lock()
                            .map(|s| *s)
                            .unwrap_or(Size::new(24, 80));
                        reap_dead_panes(&mut s, sz);
                        server_is_empty(&s)
                    })
                    .unwrap_or(false);
                if is_empty {
                    log_server(
                        "server is empty after client disconnect, exiting",
                    );
                    #[cfg(unix)]
                    if let Ok(path) =
                        crate::ipc::socket_path(&socket_name_clone)
                    {
                        let _ = std::fs::remove_file(path);
                    }
                    std::process::exit(0);
                }
            });
        }
        Ok(())
    }
}

fn server_is_empty(state: &Server) -> bool {
    state.sessions.is_empty()
        || state.sessions.iter().all(|sess| sess.windows.is_empty())
}

fn prune_empty_sessions(state: &mut Server) -> bool {
    let old_len = state.sessions.len();
    state.sessions.retain(|session| !session.windows.is_empty());
    if state.sessions.is_empty() {
        state.active_session_idx = 0;
    } else if state.active_session_idx >= state.sessions.len() {
        state.active_session_idx = state.sessions.len() - 1;
    }
    old_len != state.sessions.len()
}

fn server_log_path() -> std::path::PathBuf {
    std::env::temp_dir().join("zmux_server.log")
}

fn log_server(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(server_log_path())
    {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        // Format the whole record first and write it once so concurrent
        // worker threads (render loop, per-client handlers) don't interleave.
        let _ = f.write_all(format!("[{}] {}\n", ts, msg).as_bytes());
    }
}

fn panic_payload_str(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// Install a process-wide panic hook for the server daemon. The daemon's
/// stdout/stderr are redirected to /dev/null, so a panic would otherwise be
/// completely silent. This records the panicking thread, location, message and
/// backtrace to the server log before delegating to the default hook.
pub fn install_server_panic_hook() {
    use std::sync::Once;
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let thread = std::thread::current();
            let thread_name = thread.name().unwrap_or("<unnamed>").to_string();
            let location = info
                .location()
                .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
                .unwrap_or_else(|| "<unknown>".to_string());
            let msg = panic_payload_str(info.payload());
            let backtrace = std::backtrace::Backtrace::force_capture();
            log_server(&format!(
                "PANIC in thread '{thread_name}' at {location}: {msg}\n{backtrace}"
            ));
            default_hook(info);
        }));
    });
}

fn handle_client<S>(
    stream: S,
    state: Arc<Mutex<Server>>,
    latest_frame: Arc<Mutex<FrameStore>>,
    size_arc: Arc<Mutex<Size>>,
) -> io::Result<()>
where
    S: crate::ipc::IpcStream,
{
    use std::io::BufReader;

    use crate::ipc::{recv_line, send_frame, send_resp};

    log_server("new client connection");
    let mut write_stream = stream.try_clone()?;
    let mut reader = BufReader::new(stream);

    let hello = recv_line(&mut reader)?;
    log_server(&format!("hello line: {:?}", hello));
    match hello.as_str() {
        "LIST" => {
            let s = state.lock().unwrap();
            let output = cmd_list_sessions(&s);
            send_resp(&mut write_stream, &output)?;
            log_server("LIST served, closing");
            return Ok(());
        }
        "SESSION_TREE" => {
            let json = build_session_tree_json(&state);
            send_resp(&mut write_stream, &json)?;
            log_server("SESSION_TREE served, closing");
            return Ok(());
        }
        "OPTIONS" => {
            let json = state
                .lock()
                .map(|s| options_json(&s))
                .unwrap_or_else(|_| "{}".to_string());
            send_resp(&mut write_stream, &json)?;
            log_server("OPTIONS served, closing");
            return Ok(());
        }
        "KILL_SERVER" => {
            if let Ok(mut s) = state.lock() {
                kill_all_panes(&mut s);
                s.sessions.clear();
                s.active_session_idx = 0;
            }
            mark_data_ready();
            send_resp(&mut write_stream, "OK")?;
            log_server("KILL_SERVER served, exiting");
            return Ok(());
        }
        line if line.starts_with("CMD_OUTPUT ") => {
            let cmd = &line["CMD_OUTPUT ".len()..];
            let sz = size_arc.lock().map(|s| *s).unwrap_or(Size::new(24, 80));
            let output = state
                .lock()
                .map(|mut s| execute_command_with_output(&mut s, cmd, sz))
                .unwrap_or_default();
            send_resp(&mut write_stream, &output)?;
            log_server("CMD_OUTPUT served, closing");
            return Ok(());
        }
        "COPY_YANK" => {
            let text = state
                .lock()
                .map(|mut s| {
                    with_active_pane_mut(
                        &mut s,
                        crate::copy_mode::yank_selection,
                    )
                    .unwrap_or_default()
                })
                .unwrap_or_default();
            if !text.is_empty() {
                mark_data_ready();
            }
            send_resp(&mut write_stream, &text)?;
            log_server("COPY_YANK served, closing");
            return Ok(());
        }
        line if line.starts_with("ATTACH") => {}
        _ => {
            log_server(&format!("unknown hello {:?}, closing", hello));
            return Ok(());
        }
    }

    let sz_line = recv_line(&mut reader)?;
    log_server(&format!("size line: {:?}", sz_line));
    let (rows, cols) = parse_size_line(&sz_line).unwrap_or((24, 80));
    let new_size = Size::new(rows, cols);
    let mut last_frame_sequence =
        latest_frame.lock().map(|store| store.sequence).unwrap_or(0);
    {
        if let Ok(mut sz) = size_arc.lock() {
            *sz = new_size;
        }
        if let Ok(mut s) = state.lock() {
            resize_all_panes(&mut s, new_size);
            refresh_latest_frame(&latest_frame, &s, new_size);
        }
    }
    mark_data_ready();

    log_server("entering main loop");
    let mut pending_yank: Option<String> = None;
    loop {
        let line = match recv_line(&mut reader) {
            Ok(l) if l.is_empty() => {
                log_server("EOF from client, exiting loop");
                break;
            }
            Ok(l) => l,
            Err(e) => {
                log_server(&format!("recv_line error: {}, exiting loop", e));
                break;
            }
        };
        if line.starts_with("INPUT ") {
            let hex = &line["INPUT ".len()..];
            if let Ok(bytes) = decode_hex(hex) {
                let writer = clone_active_pane_writer(&state);
                if let Some(writer) = writer {
                    if let Err(e) = write_pty_writer(&writer, &bytes) {
                        log_server(&format!("input write failed: {}", e));
                    }
                }
            }
        } else if line.starts_with("CMD ") {
            let cmd = &line["CMD ".len()..];
            let sz = size_arc.lock().map(|s| *s).unwrap_or(Size::new(24, 80));
            if cmd == "detach" {
                break;
            }
            if let Ok(mut s) = state.lock() {
                execute_command_string(&mut s, cmd, sz);
            }
        } else if line.starts_with("COPY_KEY ") {
            let rest = &line["COPY_KEY ".len()..];
            handle_copy_key_line(&state, rest);
        } else if line.starts_with("COPY_SEARCH ") {
            let rest = &line["COPY_SEARCH ".len()..];
            handle_copy_search_line(&state, rest);
        } else if line == "COPY_SEARCH_NEXT" {
            handle_copy_nav(&state, "next");
        } else if line == "COPY_SEARCH_PREV" {
            handle_copy_nav(&state, "prev");
        } else if line == "COPY_YANK" {
            if let Ok(mut s) = state.lock() {
                let text = with_active_pane_mut(
                    &mut s,
                    crate::copy_mode::yank_selection,
                )
                .unwrap_or_default();
                if !text.is_empty() {
                    pending_yank = Some(text);
                    mark_data_ready();
                }
            }
        } else if line.starts_with("SCROLL ") {
            let rest = &line["SCROLL ".len()..];
            handle_scroll_line(&state, rest);
        } else if line.starts_with("SCROLL_DISPLAY ") {
            let rest = &line["SCROLL_DISPLAY ".len()..];
            if rest.trim() == "bottom" {
                handle_scroll_display_bottom(&state);
            } else if let Ok(delta) = rest.trim().parse::<i32>() {
                handle_scroll_display(&state, delta);
            }
        } else if line.starts_with("RESIZE ") {
            let rest = &line["RESIZE ".len()..];
            if let Some((rows, cols)) = parse_size_line(rest) {
                let new_size = Size::new(rows, cols);
                if let Ok(mut sz) = size_arc.lock() {
                    *sz = new_size;
                }
                if let Ok(mut s) = state.lock() {
                    resize_all_panes(&mut s, new_size);
                }
                schedule_delayed_frame_refresh(
                    &state,
                    &latest_frame,
                    &size_arc,
                    new_size,
                );
            }
        } else if line == "REFRESH_FRAME" {
            let size = size_arc.lock().map(|s| *s).unwrap_or(Size::new(24, 80));
            if let Ok(s) = state.lock() {
                // A floating overlay is drawn only by the client. Once it closes,
                // the next frame must contain every pane so its covered rectangle
                // is restored instead of replaying a ping-only incremental frame.
                refresh_latest_frame(&latest_frame, &s, size);
                if let Ok(mut latest) = latest_frame.lock() {
                    if let Some(frame) = latest.latest.as_mut() {
                        frame.frame_type = format!(
                            "overlay-restore-{}",
                            OVERLAY_RESTORE_SEQUENCE
                                .fetch_add(1, Ordering::Relaxed,)
                        );
                    }
                }
            }
        } else if line.starts_with("OPTION ") {
            let rest = &line["OPTION ".len()..];
            let mut parts = rest.split_whitespace();
            if matches!(parts.next(), Some("scroll_on_erase_in_display")) {
                if let Some(value) = parts.next() {
                    let enabled = matches!(value, "1" | "true" | "on");
                    if let Ok(mut s) = state.lock() {
                        set_scroll_on_erase_in_display(&mut s, enabled);
                    }
                    mark_data_ready();
                }
            }
        } else if line.starts_with("HIDE_BORDERS ") {
            let hide = &line["HIDE_BORDERS ".len()..] == "1";
            let sz = size_arc.lock().map(|s| *s).unwrap_or(Size::new(24, 80));
            if let Ok(mut s) = state.lock() {
                if s.hide_borders != hide {
                    s.hide_borders = hide;
                    resize_all_panes(&mut s, sz);
                }
            }
            mark_data_ready();
        } else if line == "FRAME?" {
            let yank_ref = pending_yank.take();
            // Keep every ANSI delta published since this frame connection's last
            // poll. A single shared "latest frame" loses nvim updates whenever a
            // cursor-only frame or a busy sibling pane (for example `ping`)
            // publishes before the client polls again.
            let (mut frame, mut published_sequence, missed_history) =
                latest_frame
                    .lock()
                    .map(|store| store.frame_since(last_frame_sequence))
                    .unwrap_or((None, last_frame_sequence, false));
            if missed_history {
                let size = size_arc
                    .lock()
                    .map(|size| *size)
                    .unwrap_or(Size::new(24, 80));
                if let Ok(state) = state.lock() {
                    let baseline = latest_frame
                        .lock()
                        .map(|store| store.sequence)
                        .unwrap_or(last_frame_sequence);
                    refresh_latest_frame(&latest_frame, &state, size);
                    last_frame_sequence = baseline;
                }
                (frame, published_sequence, _) = latest_frame
                    .lock()
                    .map(|store| store.frame_since(last_frame_sequence))
                    .unwrap_or((None, last_frame_sequence, false));
            }
            if frame.is_none() {
                let is_empty = state
                    .lock()
                    .map(|state| server_is_empty(&state))
                    .unwrap_or(true);
                if is_empty {
                    log_server("all sessions empty, sending exit frame");
                    let json = "{\"type\":\"frame\",\"exit\":true,\"layout\":{\"type\":\"leaf\",\"id\":0,\"rows\":1,\"cols\":1,\"cursor_row\":0,\"cursor_col\":0,\"hide_cursor\":true,\"alternate_screen\":false,\"mouse_mode\":0,\"in_copy_mode\":false,\"cursor_shape\":255,\"active\":false,\"rows_v2\":[]}}";
                    if send_frame(&mut write_stream, json).is_err() {
                        break;
                    }
                    break;
                }
                // ATTACH normally publishes a full frame before the first poll.
                // Cover the startup race with a one-time full publication; steady
                // state polls never serialize pane output themselves.
                let size = size_arc
                    .lock()
                    .map(|size| *size)
                    .unwrap_or(Size::new(24, 80));
                if let Ok(state) = state.lock() {
                    refresh_latest_frame(&latest_frame, &state, size);
                }
                (frame, published_sequence, _) = latest_frame
                    .lock()
                    .map(|store| store.frame_since(last_frame_sequence))
                    .unwrap_or((None, last_frame_sequence, false));
            }
            let Some(mut frame) = frame else {
                log_server("no frame available after startup refresh");
                break;
            };
            frame.yank_text = yank_ref;
            let json = serde_json::to_string(&frame).unwrap_or_default();
            if send_frame(&mut write_stream, &json).is_err() {
                break;
            }
            last_frame_sequence = published_sequence;
            if frame.exit {
                log_server("sent exit frame, closing connection");
                break;
            }
        }
    }
    log_server("handle_client exiting");
    Ok(())
}

fn handle_copy_key_line(state: &Arc<Mutex<Server>>, key: &str) {
    let pane_fn: Option<fn(&mut crate::types::Pane)> = match key {
        "left" => Some(crate::copy_mode::move_left),
        "right" => Some(crate::copy_mode::move_right),
        "up" => Some(crate::copy_mode::move_up),
        "down" => Some(crate::copy_mode::move_down),
        "page_up" => Some(crate::copy_mode::page_up),
        "page_down" => Some(crate::copy_mode::page_down),
        "top" => Some(crate::copy_mode::move_to_top),
        "bottom" => Some(crate::copy_mode::move_to_bottom),
        "line_start" => Some(crate::copy_mode::move_to_line_start),
        "line_end" => Some(crate::copy_mode::move_to_line_end),
        "word_back" => Some(crate::copy_mode::move_word_backward),
        "word_fwd" => Some(crate::copy_mode::move_word_forward),
        "word_end" => Some(crate::copy_mode::move_word_end),
        "enter" | "exit" => Some(crate::copy_mode::exit),
        _ => None,
    };
    if let Some(f) = pane_fn {
        if let Ok(mut s) = state.lock() {
            let force_clear = key == "exit" || key == "enter";
            let changed = with_active_pane_mut(&mut s, |pane| {
                let had_copy = pane.copy_state.is_some();
                f(pane);
                pane.mark_render_dirty();
                had_copy && pane.copy_state.is_none()
            })
            .unwrap_or(false);
            if changed && force_clear {
                s.force_clear_display = true;
            }
            mark_data_ready();
        }
    }
    if key.starts_with("sel_") {
        let mode = match &key["sel_".len()..] {
            "char" => Some(crate::types::SelectionMode::Char),
            "line" => Some(crate::types::SelectionMode::Line),
            "rect" => Some(crate::types::SelectionMode::Rect),
            _ => None,
        };
        if let Some(m) = mode {
            if let Ok(mut s) = state.lock() {
                with_active_pane_mut(&mut s, |pane| {
                    crate::copy_mode::start_selection(pane, m);
                    pane.mark_render_dirty();
                });
                mark_data_ready();
            }
        }
    }
    if key == "clear_sel" {
        if let Ok(mut s) = state.lock() {
            with_active_pane_mut(&mut s, |pane| {
                crate::copy_mode::clear_selection(pane);
                pane.mark_render_dirty();
            });
            mark_data_ready();
        }
    }
}

fn handle_scroll_line(state: &Arc<Mutex<Server>>, rest: &str) {
    let mut parts = rest.split_whitespace();
    let direction = match parts.next() {
        Some("up") => "up",
        Some("down") => "down",
        _ => return,
    };
    let lines = parts
        .next()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(3);
    let pane_id = parts
        .next()
        .and_then(|target| target.strip_prefix('%'))
        .and_then(|id| id.parse::<usize>().ok());
    if parts.next().is_some() {
        return;
    }
    if lines == 0 {
        return;
    }
    if let Ok(mut s) = state.lock() {
        let scroll = |pane: &mut crate::types::Pane| {
            let result = match direction {
                "up" => crate::copy_mode::scroll_up(pane, lines),
                "down" => crate::copy_mode::scroll_down(pane, lines),
                _ => crate::copy_mode::CopyScrollResult::Unavailable,
            };
            if !matches!(
                result,
                crate::copy_mode::CopyScrollResult::Unavailable
            ) {
                pane.mark_render_dirty();
            }
            result
        };
        let result = if let Some(pane_id) = pane_id {
            with_pane_by_id_mut(&mut s, pane_id, scroll)
        } else {
            with_active_pane_mut(&mut s, scroll)
        }
        .unwrap_or(crate::copy_mode::CopyScrollResult::Unavailable);
        if result.needs_full_clear() {
            s.force_clear_display = true;
        }
        mark_data_ready();
    }
}

fn handle_scroll_display_bottom(state: &Arc<Mutex<Server>>) {
    if let Ok(mut s) = state.lock() {
        let scrolled = with_active_pane_mut(&mut s, |pane| {
            if let Ok(mut parser) = pane.parser.lock() {
                if parser.display_offset() > 0 {
                    parser.scrollback_bottom();
                    pane.mark_render_dirty();
                    return true;
                }
            }
            false
        })
        .unwrap_or(false);
        if scrolled {
            s.force_clear_display = true;
            mark_data_ready();
        }
    }
}

fn handle_scroll_display(state: &Arc<Mutex<Server>>, delta: i32) {
    if delta == 0 {
        return;
    }
    if let Ok(mut s) = state.lock() {
        let scrolled = with_active_pane_mut(&mut s, |pane| {
            if let Ok(mut parser) = pane.parser.lock() {
                if parser.scroll_display_delta(delta) {
                    pane.mark_render_dirty();
                    return true;
                }
            }
            false
        })
        .unwrap_or(false);
        if scrolled {
            s.force_clear_display = true;
        } else {
            let fallback = with_active_pane_mut(&mut s, |pane| {
                let result = if delta > 0 {
                    crate::copy_mode::scroll_up(pane, delta as usize)
                } else {
                    crate::copy_mode::scroll_down(pane, (-delta) as usize)
                };
                if !matches!(
                    result,
                    crate::copy_mode::CopyScrollResult::Unavailable
                ) {
                    pane.mark_render_dirty();
                }
                result
            })
            .unwrap_or(crate::copy_mode::CopyScrollResult::Unavailable);
            if fallback.needs_full_clear() {
                s.force_clear_display = true;
            }
        }
        mark_data_ready();
    }
}

fn handle_copy_search_line(state: &Arc<Mutex<Server>>, rest: &str) {
    let (forward, query) = if rest.starts_with("fwd ") {
        (true, rest["fwd ".len()..].to_string())
    } else if rest.starts_with("bwd ") {
        (false, rest["bwd ".len()..].to_string())
    } else {
        return;
    };
    if let Ok(mut s) = state.lock() {
        with_active_pane_mut(&mut s, |pane| {
            crate::copy_mode::search(pane, query.clone(), forward);
            pane.mark_render_dirty();
        });
        mark_data_ready();
    }
}

fn handle_copy_nav(state: &Arc<Mutex<Server>>, dir: &str) {
    let f: fn(&mut crate::types::Pane) -> bool = match dir {
        "next" => crate::copy_mode::search_next,
        _ => crate::copy_mode::search_prev,
    };
    if let Ok(mut s) = state.lock() {
        with_active_pane_mut(&mut s, |pane| {
            f(pane);
            pane.mark_render_dirty();
        });
        mark_data_ready();
    }
}

fn schedule_delayed_frame_refresh(
    state: &Arc<Mutex<Server>>,
    latest_frame: &Arc<Mutex<FrameStore>>,
    size_arc: &Arc<Mutex<Size>>,
    target_size: Size,
) {
    let state = Arc::clone(state);
    let latest_frame = Arc::clone(latest_frame);
    let size_arc = Arc::clone(size_arc);
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(60));
        if size_arc.lock().map(|size| *size).ok() != Some(target_size) {
            return;
        }
        if let Ok(state) = state.lock() {
            refresh_latest_frame(&latest_frame, &state, target_size);
            mark_data_ready();
        }
    });
}

fn refresh_latest_frame(
    latest_frame: &Arc<Mutex<FrameStore>>,
    state: &Server,
    size: Size,
) {
    let Some(session) = state.active_session() else {
        return;
    };
    let Some(win) = session.windows.get(session.active_window_idx) else {
        return;
    };
    let json = build_frame_json(
        session,
        win,
        frame_layout_area(size),
        None,
        state.hide_borders,
        size,
        FrameAnsiOptions {
            clear_display: true,
            force_repaint: true,
        },
    );
    if let Ok(fd) = serde_json::from_str::<FrameData>(&json) {
        if let Ok(mut frame) = latest_frame.lock() {
            frame.publish(fd);
        }
    }
}

fn build_frame_json(
    session: &crate::types::session::Session,
    win: &crate::types::session::Window,
    area: Rect,
    yank_text: Option<&str>,
    hide_borders: bool,
    size: Size,
    ansi_opts: FrameAnsiOptions,
) -> String {
    use crate::layout::serialize_frame;
    let layout_json = serialize_frame(win, area, hide_borders);
    let layout_part = layout_json
        .strip_prefix("{\"type\":\"frame\",\"layout\":")
        .and_then(|s| s.strip_suffix('}'))
        .unwrap_or("{}");
    let ansi = serialize_frame_ansi(
        win,
        frame_ansi_area(size),
        hide_borders,
        ansi_opts,
    );
    let ansi_b64 = encode_ansi_base64(&ansi);
    let session_name = &session.name;
    let active_idx = session.active_window_idx;
    let mut status = String::new();
    status.push_str("{\"left\":\"[");
    json_escape_status(session_name, &mut status);
    status.push_str("]\",\"right\":\"\",\"windows\":[");
    for (i, w) in session.windows.iter().enumerate() {
        if i > 0 {
            status.push(',');
        }
        let is_active = i == active_idx;
        status.push_str("{\"index\":");
        status.push_str(&i.to_string());
        status.push_str(",\"name\":\"");
        json_escape_status(&w.name, &mut status);
        status.push_str("\",\"active\":");
        status.push_str(if is_active { "true" } else { "false" });
        status.push('}');
    }
    status.push_str("]}");
    if let Some(text) = yank_text {
        let escaped = serde_json::to_string(text).unwrap_or_default();
        format!(
            "{{\"type\":\"frame\",\"layout\":{},\"status\":{},\"ansi\":\"{}\",\"yank_text\":{}}}",
            layout_part, status, ansi_b64, escaped
        )
    } else {
        format!(
            "{{\"type\":\"frame\",\"layout\":{},\"status\":{},\"ansi\":\"{}\"}}",
            layout_part, status, ansi_b64
        )
    }
}

fn build_session_tree_json(state: &Arc<Mutex<Server>>) -> String {
    let s = match state.lock() {
        Ok(s) => s,
        Err(_) => return "[]".to_string(),
    };
    let active_sess_idx = s.active_session_idx;
    let mut out = String::from("[");
    let mut first_entry = true;
    for (si, sess) in s.sessions.iter().enumerate() {
        let active_win_idx = sess.active_window_idx;
        let is_active_sess = si == active_sess_idx;

        if !first_entry {
            out.push(',');
        }
        first_entry = false;
        out.push_str(&format!(
            "{{\"type\":\"session\",\"name\":{},\"window_count\":{},\"is_active\":{}}}",
            serde_json::to_string(&sess.name).unwrap_or_default(),
            sess.windows.len(),
            is_active_sess
        ));

        for (wi, win) in sess.windows.iter().enumerate() {
            let pane_ids = crate::layout::collect_pane_ids(&win.root);
            let active_pane_id =
                crate::layout::active_pane(&win.root, &win.active_pane_path)
                    .map(|p| p.id);
            let is_active_win = is_active_sess && wi == active_win_idx;
            out.push_str(&format!(
                ",{{\"type\":\"window\",\"session_name\":{},\"index\":{},\"name\":{},\"pane_count\":{},\"is_active\":{}}}",
                serde_json::to_string(&sess.name).unwrap_or_default(),
                wi,
                serde_json::to_string(&win.name).unwrap_or_default(),
                pane_ids.len(),
                is_active_win
            ));

            for (pi, &pane_id) in pane_ids.iter().enumerate() {
                out.push_str(&format!(
                    ",{{\"type\":\"pane\",\"session_name\":{},\"window_index\":{},\"pane_id\":{},\"index\":{},\"is_active\":{}}}",
                    serde_json::to_string(&sess.name).unwrap_or_default(),
                    wi,
                    pane_id,
                    pi,
                    is_active_win && Some(pane_id) == active_pane_id
                ));
            }
        }
    }
    out.push(']');
    out
}

fn clone_active_pane_writer(
    state: &Arc<Mutex<Server>>,
) -> Option<crate::pty::PtyWriter> {
    let s = state.lock().ok()?;
    let session = s.active_session()?;
    let win = session.windows.get(session.active_window_idx)?;
    let pane = crate::layout::active_pane(&win.root, &win.active_pane_path)?;
    Some(Arc::clone(&pane.writer))
}

fn write_pty_writer(
    writer: &crate::pty::PtyWriter,
    bytes: &[u8],
) -> io::Result<()> {
    let mut guard = writer.lock().map_err(|_| {
        io::Error::new(io::ErrorKind::Other, "pty writer poisoned")
    })?;
    write_pty_input(&mut **guard, bytes)
}

fn write_pty_input(writer: &mut dyn Write, bytes: &[u8]) -> io::Result<()> {
    let mut written = 0usize;
    let deadline = Instant::now() + Duration::from_secs(10);
    while written < bytes.len() {
        match writer.write(&bytes[written..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "pty write returned zero bytes",
                ));
            }
            Ok(n) => {
                written += n;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(e);
                }
                thread::sleep(Duration::from_millis(1));
            }
            Err(e) => return Err(e),
        }
    }
    writer.flush()
}

fn parse_size_line(s: &str) -> Option<(u16, u16)> {
    let mut parts = s.split('x');
    let rows: u16 = parts.next()?.parse().ok()?;
    let cols: u16 = parts.next()?.parse().ok()?;
    Some((rows, cols))
}

fn decode_hex(s: &str) -> Result<Vec<u8>, ()> {
    if s.len() % 2 != 0 {
        return Err(());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| ()))
        .collect()
}

pub fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn frame_layout_area(size: Size) -> Rect {
    Rect::new(0, 0, size.cols.max(1), size.rows.saturating_sub(1).max(1))
}

fn pane_viewport_size(area: Rect, hide_borders: bool) -> (u16, u16) {
    if !hide_borders && area.width > 2 && area.height > 2 {
        (area.height - 2, area.width - 2)
    } else {
        (area.height.max(1), area.width.max(1))
    }
}

fn root_pane_size(size: Size) -> (u16, u16) {
    pane_viewport_size(frame_layout_area(size), false)
}

fn flush_sync_for_display_updates(state: &mut Server) -> bool {
    let mut flushed = false;
    for session in &mut state.sessions {
        for window in &mut session.windows {
            flush_sync_for_display_in_layout(&mut window.root, &mut flushed);
        }
    }
    flushed
}

fn flush_sync_for_display_in_layout(node: &mut LayoutNode, flushed: &mut bool) {
    match node {
        LayoutNode::Leaf(pane) => {
            let did_flush = pane
                .parser
                .lock()
                .map(|mut parser| parser.flush_sync_for_display())
                .unwrap_or(false);
            if did_flush {
                crate::pty::persist_pending_history(pane);
                pane.render_dirty
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                *flushed = true;
            }
        }
        LayoutNode::Split { children, .. } => {
            for child in children {
                flush_sync_for_display_in_layout(child, flushed);
            }
        }
    }
}

fn render_loop(
    state: Arc<Mutex<Server>>,
    latest_frame: Arc<Mutex<FrameStore>>,
    size: Arc<Mutex<Size>>,
    socket_name: Option<String>,
) {
    let mut first = true;
    let mut last_layout_fp = 0u64;
    let mut last_reap = Instant::now() - Duration::from_millis(250);
    loop {
        crate::types::events::wait_render(Duration::from_millis(16));

        let sync_flushed = state
            .lock()
            .ok()
            .map(|mut s| flush_sync_for_display_updates(&mut s))
            .unwrap_or(false);
        let dirty = PTY_DATA_READY.swap(false, Ordering::Relaxed);
        let should_reap = first
            || dirty
            || sync_flushed
            || last_reap.elapsed() >= Duration::from_millis(250);
        let (should_exit, reaped) = if should_reap {
            last_reap = Instant::now();
            let mut s = match state.lock() {
                Ok(s) => s,
                Err(_) => continue,
            };
            let sz = size.lock().map(|s| *s).unwrap_or(Size::new(24, 80));
            let reaped = reap_dead_panes(&mut s, sz);
            (server_is_empty(&s), reaped)
        } else {
            (false, false)
        };
        if !dirty && !first && !reaped && !sync_flushed {
            continue;
        }
        let clear_on_paint = first;
        if should_exit {
            log_server("render loop found server empty, exiting");
            #[cfg(unix)]
            if let Some(socket_name) = socket_name.as_deref() {
                if let Ok(path) = crate::ipc::socket_path(socket_name) {
                    let _ = std::fs::remove_file(path);
                }
            }
            std::process::exit(0);
        }

        let frame_json = {
            let mut s = match state.lock() {
                Ok(s) => s,
                Err(_) => continue,
            };
            let force_clear = s.force_clear_display;
            if force_clear {
                s.force_clear_display = false;
            }
            let sz = size.lock().map(|s| *s).unwrap_or(Size::new(24, 80));
            let session = match s.active_session() {
                Some(s) => s,
                None => continue,
            };
            let win = match session.windows.get(session.active_window_idx) {
                Some(w) => w,
                None => continue,
            };
            let area = frame_layout_area(sz);
            let layout_json = serialize_frame(win, area, s.hide_borders);
            let layout_part = layout_json
                .strip_prefix("{\"type\":\"frame\",\"layout\":")
                .and_then(|s| s.strip_suffix('}'))
                .unwrap_or("{}");

            let session_name = &session.name;
            let active_idx = session.active_window_idx;
            let mut status = String::new();
            status.push_str("{\"left\":\"[");
            json_escape_status(session_name, &mut status);
            status.push_str("]\",\"right\":\"\",\"windows\":[");
            for (i, w) in session.windows.iter().enumerate() {
                if i > 0 {
                    status.push(',');
                }
                let is_active = i == active_idx;
                status.push_str("{\"index\":");
                status.push_str(&i.to_string());
                status.push_str(",\"name\":\"");
                json_escape_status(&w.name, &mut status);
                status.push_str("\",\"active\":");
                status.push_str(if is_active { "true" } else { "false" });
                status.push('}');
            }
            status.push_str("]}");

            let ansi_area = frame_ansi_area(sz);
            let layout_fp = layout_fingerprint(win, ansi_area, s.hide_borders);
            let clear_display =
                clear_on_paint || layout_fp != last_layout_fp || force_clear;
            last_layout_fp = layout_fp;
            first = false;
            let ansi = serialize_frame_ansi(
                win,
                ansi_area,
                s.hide_borders,
                FrameAnsiOptions {
                    clear_display,
                    // Dirty-only: force_repaint every frame erase+paints all panes and
                    // causes visible flicker / white flecks while typing or when a
                    // sibling pane (e.g. ping) is scrolling. Per-row clipping +
                    // selective CUP below keep cross-pane wrap from coming back.
                    force_repaint: false,
                },
            );
            let ansi_b64 = encode_ansi_base64(&ansi);
            format!(
                "{{\"type\":\"frame\",\"layout\":{},\"status\":{},\"ansi\":\"{}\"}}",
                layout_part, status, ansi_b64
            )
        };

        if let Ok(fd) = serde_json::from_str::<FrameData>(&frame_json) {
            if let Ok(mut frame) = latest_frame.lock() {
                frame.publish(fd);
            }
        }
    }
}

fn reap_dead_panes(state: &mut Server, size: Size) -> bool {
    let mut changed = false;
    for session in &mut state.sessions {
        let mut win_idx = 0;
        while win_idx < session.windows.len() {
            mark_exited_panes(&mut session.windows[win_idx].root);
            let dead_ids =
                collect_dead_pane_ids(&session.windows[win_idx].root);
            if dead_ids.is_empty() {
                win_idx += 1;
                continue;
            }
            changed = true;
            for dead_id in dead_ids {
                let path = crate::layout::find_pane_path(
                    &session.windows[win_idx].root,
                    dead_id,
                );
                if let Some(path) = path {
                    let placeholder = LayoutNode::Split {
                        direction: SplitDirection::Horizontal,
                        sizes: vec![],
                        children: vec![],
                    };
                    let old_root = std::mem::replace(
                        &mut session.windows[win_idx].root,
                        placeholder,
                    );
                    if let Some(new_root) = kill_pane_at_path(old_root, &path) {
                        session.windows[win_idx].root = new_root;
                        session.windows[win_idx].active_pane_path =
                            crate::layout::first_leaf_path(
                                &session.windows[win_idx].root,
                            );
                    } else {
                        session.windows.remove(win_idx);
                        if session.active_window_idx >= session.windows.len()
                            && session.active_window_idx > 0
                        {
                            session.active_window_idx -= 1;
                        }
                        break;
                    }
                }
            }
            if win_idx < session.windows.len() {
                win_idx += 1;
            }
        }
    }
    if prune_empty_sessions(state) {
        changed = true;
    }
    if changed && !server_is_empty(state) {
        resize_all_panes(state, size);
    }
    changed
}

fn mark_exited_panes(node: &mut LayoutNode) {
    match node {
        LayoutNode::Leaf(p) => {
            if p.dead.load(Ordering::Relaxed) {
                return;
            }
            if matches!(p.child.try_wait(), Ok(Some(_))) {
                p.dead.store(true, Ordering::Relaxed);
                mark_data_ready();
            }
        }
        LayoutNode::Split { children, .. } => {
            for child in children {
                mark_exited_panes(child);
            }
        }
    }
}

fn kill_all_panes(state: &mut Server) {
    for session in &mut state.sessions {
        for window in &mut session.windows {
            kill_node_panes(&mut window.root);
        }
    }
}

fn kill_node_panes(node: &mut LayoutNode) {
    match node {
        LayoutNode::Leaf(p) => {
            let _ = p.child.kill();
            p.dead.store(true, Ordering::Relaxed);
        }
        LayoutNode::Split { children, .. } => {
            for child in children {
                kill_node_panes(child);
            }
        }
    }
}

fn collect_dead_pane_ids(node: &LayoutNode) -> Vec<PaneId> {
    match node {
        LayoutNode::Leaf(p) => {
            if p.dead.load(Ordering::Relaxed) {
                vec![p.id]
            } else {
                vec![]
            }
        }
        LayoutNode::Split { children, .. } => {
            children.iter().flat_map(collect_dead_pane_ids).collect()
        }
    }
}

fn create_initial_session(
    state: &mut Server,
    name: &str,
    size: Size,
    start_dir: Option<&str>,
) -> io::Result<()> {
    let session_id = state.alloc_session_id();
    let mut session = Session::new(session_id, name.to_string());
    session.options = SessionOptions::with_defaults();

    let pane_id = session.alloc_pane_id();
    let window_id = session.alloc_window_id();

    let start_dir = start_dir
        .map(|dir| dir.to_string())
        .or_else(crate::pty::default_start_dir);
    let (rows, cols) = root_pane_size(size);
    let pane = spawn_pane(SpawnOptions {
        pane_id,
        rows,
        cols,
        history_limit: state.options.history_limit,
        command: None,
        start_dir: start_dir.as_deref(),
        env: vec![],
        scroll_on_erase_in_display: state.options.scroll_on_erase_in_display,
    })?;

    let window = Window {
        id: window_id,
        name: "shell".to_string(),
        root: LayoutNode::Leaf(pane),
        active_pane_path: vec![],
        options: WindowOptions::with_defaults(),
        pane_mru: vec![pane_id],
        zoom_state: None,
        flags: Default::default(),
        layout_index: 0,
        last_output_time: Instant::now(),
        last_seen_version: 0,
        default_start_dir: start_dir.clone(),
    };
    session.windows.push(window);
    state.sessions.push(session);
    Ok(())
}

fn resize_all_panes(state: &mut Server, size: Size) {
    state.force_clear_display = true;
    let hide_borders = state.hide_borders;
    let border_size: u16 = if hide_borders { 0 } else { BORDER_SIZE };
    for session in &mut state.sessions {
        for win in &mut session.windows {
            let area = frame_layout_area(size);
            if let Some(zoom) = &win.zoom_state {
                let zoomed_id = zoom.zoomed_pane_id;
                if let Some(pane) =
                    crate::layout::find_pane_by_id_mut(&mut win.root, zoomed_id)
                {
                    let (rows, cols) = pane_viewport_size(area, hide_borders);
                    let _ = resize_pane(pane, rows, cols);
                    crate::copy_mode::refresh_layout(pane);
                }
            } else {
                let rects = compute_rects(&win.root, area, border_size);
                resize_node_panes(&mut win.root, &rects, None, hide_borders);
            }
        }
    }
}

fn resize_node_panes(
    node: &mut LayoutNode,
    rects: &HashMap<PaneId, Rect>,
    zoom_pane_id: Option<PaneId>,
    hide_borders: bool,
) {
    match node {
        LayoutNode::Leaf(p) => {
            if let Some(zoomed) = zoom_pane_id {
                if p.id != zoomed {
                    return;
                }
            }
            if let Some(&rect) = rects.get(&p.id) {
                let (rows, cols) = pane_viewport_size(rect, hide_borders);
                let _ = resize_pane(p, rows, cols);
                crate::copy_mode::refresh_layout(p);
            }
        }
        LayoutNode::Split { children, .. } => {
            for child in children.iter_mut() {
                resize_node_panes(child, rects, zoom_pane_id, hide_borders);
            }
        }
    }
}

fn set_scroll_on_erase_in_display(state: &mut Server, enabled: bool) {
    state.options.scroll_on_erase_in_display = enabled;
    for session in &mut state.sessions {
        for win in &mut session.windows {
            set_scroll_on_erase_in_display_node(&mut win.root, enabled);
        }
    }
}

fn set_scroll_on_erase_in_display_node(node: &mut LayoutNode, enabled: bool) {
    match node {
        LayoutNode::Leaf(pane) => {
            if let Ok(mut parser) = pane.parser.lock() {
                parser.set_scroll_on_erase_in_display(enabled);
            }
        }
        LayoutNode::Split { children, .. } => {
            for child in children {
                set_scroll_on_erase_in_display_node(child, enabled);
            }
        }
    }
}

fn set_history_limit(state: &mut Server, history_limit: usize) {
    state.options.history_limit = history_limit;
    state.force_clear_display = true;
    for session in &mut state.sessions {
        for win in &mut session.windows {
            set_history_limit_node(&mut win.root, history_limit);
        }
    }
}

fn set_history_limit_node(node: &mut LayoutNode, history_limit: usize) {
    match node {
        LayoutNode::Leaf(pane) => {
            if let Ok(mut parser) = pane.parser.lock() {
                parser.set_scrollback_limit(history_limit);
            }
            crate::pty::persist_pending_history(pane);
            crate::copy_mode::refresh_layout(pane);
            pane.mark_render_dirty();
        }
        LayoutNode::Split { children, .. } => {
            for child in children {
                set_history_limit_node(child, history_limit);
            }
        }
    }
}

fn options_json(state: &Server) -> String {
    format!(
        "{{\"scroll_on_erase_in_display\":{},\"history_limit\":{}}}",
        state.options.scroll_on_erase_in_display, state.options.history_limit
    )
}

fn execute_command_string(state: &mut Server, raw: &str, sz: Size) {
    let cmds = ParsedCommand::parse(raw);
    for cmd in cmds {
        dispatch_command(state, &cmd, sz);
    }
    mark_data_ready();
}

fn execute_command_with_output(
    state: &mut Server,
    raw: &str,
    sz: Size,
) -> String {
    let cmds = ParsedCommand::parse(raw);
    let mut out = String::new();
    for cmd in &cmds {
        let result = dispatch_command_output(state, cmd, sz);
        if !result.is_empty() {
            out.push_str(&result);
            out.push('\n');
        }
    }
    mark_data_ready();
    out
}

fn dispatch_command(state: &mut Server, cmd: &ParsedCommand, sz: Size) {
    dispatch_command_output(state, cmd, sz);
}

fn dispatch_command_output(
    state: &mut Server,
    cmd: &ParsedCommand,
    sz: Size,
) -> String {
    match cmd.name.as_str() {
        "split-window" | "splitw" => {
            cmd_split_window(state, cmd, sz);
            String::new()
        }
        "new-window" | "neww" => {
            cmd_new_window(state, cmd, sz);
            String::new()
        }
        "kill-pane" | "killp" => {
            cmd_kill_pane(state, cmd, sz);
            String::new()
        }
        "kill-window" | "killw" => {
            cmd_kill_window(state, sz);
            String::new()
        }
        "select-pane" | "selectp" => {
            cmd_select_pane(state, cmd, sz);
            String::new()
        }
        "resize-pane" | "resizep" => {
            cmd_resize_pane(state, cmd, sz);
            String::new()
        }
        "select-window" | "selectw" => {
            cmd_select_window(state, cmd, sz);
            String::new()
        }
        "rename-window" | "renamew" => {
            cmd_rename_window(state, cmd);
            String::new()
        }
        "rename-session" | "rename-s" => {
            cmd_rename_session(state, cmd);
            String::new()
        }
        "new-session" | "new" => {
            cmd_new_session(state, cmd, sz);
            String::new()
        }
        "kill-session" | "kill-s" => {
            cmd_kill_session(state, cmd, sz);
            String::new()
        }
        "switch-client" | "switchc" => {
            cmd_switch_client(state, cmd, sz);
            String::new()
        }
        "next-session" => {
            cmd_next_session(state, sz);
            String::new()
        }
        "prev-session" => {
            cmd_prev_session(state, sz);
            String::new()
        }
        "list-sessions" | "ls" => cmd_list_sessions(state),
        "set-pane-start-dir" => cmd_set_pane_start_dir(state),
        "zoom-pane" | "zoomp" => {
            cmd_zoom_pane(state, sz);
            String::new()
        }
        "clear-pane" | "clearp" => {
            cmd_clear_pane(state);
            String::new()
        }
        "set-option" | "set" => cmd_set_option(state, cmd),
        "show-options" | "show" => format!(
            "history-limit {}\nscroll-on-erase-in-display {}",
            state.options.history_limit,
            if state.options.scroll_on_erase_in_display {
                "on"
            } else {
                "off"
            }
        ),
        "copy-mode" => {
            with_active_pane_mut(state, |pane| {
                crate::copy_mode::enter(pane);
            });
            state.force_clear_display = true;
            mark_data_ready();
            String::new()
        }
        _ => String::new(),
    }
}

fn cmd_set_option(state: &mut Server, cmd: &ParsedCommand) -> String {
    let Some(name) = cmd.args.first().map(String::as_str) else {
        return "set-option: missing option name".to_string();
    };
    let Some(value) = cmd.args.get(1).map(String::as_str) else {
        return format!("set-option: missing value for {name}");
    };

    match name {
        "history-limit" | "history_limit" => {
            let Ok(history_limit) = value.parse::<usize>() else {
                return format!(
                    "set-option: history-limit must be an integer from 0 to \
                     {MAX_HISTORY_LIMIT}"
                );
            };
            if history_limit > MAX_HISTORY_LIMIT {
                return format!(
                    "set-option: history-limit must be between 0 and \
                     {MAX_HISTORY_LIMIT}"
                );
            }
            set_history_limit(state, history_limit);
            format!("history-limit: {history_limit}")
        }
        _ => format!("set-option: unknown option {name}"),
    }
}

fn active_session_mut(state: &mut Server) -> Option<&mut Session> {
    state.active_session_mut()
}

fn with_active_pane_mut<T>(
    state: &mut Server,
    f: impl FnOnce(&mut crate::types::Pane) -> T,
) -> Option<T> {
    let session = state.active_session_mut()?;
    let win = session.windows.get_mut(session.active_window_idx)?;
    let path = win.active_pane_path.clone();
    let pane = crate::layout::active_pane_mut(&mut win.root, &path)?;
    Some(f(pane))
}

fn with_pane_by_id_mut<T>(
    state: &mut Server,
    pane_id: usize,
    f: impl FnOnce(&mut crate::types::Pane) -> T,
) -> Option<T> {
    let session = state.active_session_mut()?;
    let win = session.windows.get_mut(session.active_window_idx)?;
    let pane = crate::layout::find_pane_by_id_mut(&mut win.root, pane_id)?;
    Some(f(pane))
}

fn active_pane_start_dir(win: &Window) -> Option<String> {
    crate::layout::active_pane(&win.root, &win.active_pane_path)
        .and_then(crate::pty::pane_current_dir)
}

fn active_window_start_dir(session: &Session) -> Option<String> {
    session
        .windows
        .get(session.active_window_idx)
        .and_then(|win| {
            win.default_start_dir
                .clone()
                .or_else(|| active_pane_start_dir(win))
        })
        .or_else(crate::pty::default_start_dir)
}

#[cfg(test)]
fn make_session(state: &mut Server, name: &str, sz: Size) -> io::Result<()> {
    make_session_with_start_dir(
        state,
        name,
        sz,
        crate::pty::default_start_dir(),
    )
}

fn make_session_with_start_dir(
    state: &mut Server,
    name: &str,
    sz: Size,
    start_dir: Option<String>,
) -> io::Result<()> {
    let session_id = state.alloc_session_id();
    let mut session = Session::new(session_id, name.to_string());
    session.options = SessionOptions::with_defaults();
    let pane_id = session.alloc_pane_id();
    let window_id = session.alloc_window_id();
    let (rows, cols) = root_pane_size(sz);
    let pane = spawn_pane(SpawnOptions {
        pane_id,
        rows,
        cols,
        history_limit: state.options.history_limit,
        command: None,
        start_dir: start_dir.as_deref(),
        env: vec![],
        scroll_on_erase_in_display: state.options.scroll_on_erase_in_display,
    })?;
    let win = Window {
        id: window_id,
        name: "shell".to_string(),
        root: LayoutNode::Leaf(pane),
        active_pane_path: vec![],
        options: WindowOptions::with_defaults(),
        pane_mru: vec![pane_id],
        zoom_state: None,
        flags: Default::default(),
        layout_index: 0,
        last_output_time: Instant::now(),
        last_seen_version: 0,
        default_start_dir: start_dir.clone(),
    };
    session.windows.push(win);
    state.sessions.push(session);
    Ok(())
}

fn cmd_new_session(state: &mut Server, cmd: &ParsedCommand, sz: Size) {
    let name = cmd
        .flag_value("s")
        .map(|s| s.to_string())
        .unwrap_or_else(|| state.sessions.len().to_string());
    if state.find_session(&name).is_some() {
        return;
    }
    let start_dir = state
        .active_session()
        .and_then(active_window_start_dir)
        .or_else(crate::pty::default_start_dir);
    if make_session_with_start_dir(state, &name, sz, start_dir).is_err() {
        return;
    }
    let new_idx = state.sessions.len() - 1;
    if !cmd.flag("d") {
        state.active_session_idx = new_idx;
    }
}

fn cmd_kill_session(state: &mut Server, cmd: &ParsedCommand, sz: Size) {
    let target = cmd.flag_value("t").map(|s| s.to_string());
    let idx = if let Some(name) = target {
        state.find_session_idx(&name)
    } else {
        Some(state.active_session_idx)
    };
    if let Some(i) = idx {
        if i < state.sessions.len() {
            state.sessions.remove(i);
            if state.active_session_idx >= state.sessions.len()
                && !state.sessions.is_empty()
            {
                state.active_session_idx = state.sessions.len() - 1;
            }
        }
    }
    if !state.sessions.is_empty() {
        resize_all_panes(state, sz);
    }
}

fn cmd_switch_client(state: &mut Server, cmd: &ParsedCommand, sz: Size) {
    if let Some(name) = cmd.flag_value("t") {
        if let Some(idx) = state.find_session_idx(name) {
            state.active_session_idx = idx;
            resize_all_panes(state, sz);
        }
    }
}

fn cmd_next_session(state: &mut Server, sz: Size) {
    let n = state.sessions.len();
    if n > 1 {
        state.active_session_idx = (state.active_session_idx + 1) % n;
        resize_all_panes(state, sz);
    }
}

fn cmd_prev_session(state: &mut Server, sz: Size) {
    let n = state.sessions.len();
    if n > 1 {
        state.active_session_idx = (state.active_session_idx + n - 1) % n;
        resize_all_panes(state, sz);
    }
}

fn cmd_list_sessions(state: &Server) -> String {
    state
        .sessions
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let active = if i == state.active_session_idx {
                " (attached)"
            } else {
                ""
            };
            format!(
                "{}: {} windows (created {}){}",
                s.name,
                s.windows.len(),
                s.created_at.format("%Y-%m-%d %H:%M"),
                active
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn cmd_split_window(state: &mut Server, cmd: &ParsedCommand, sz: Size) {
    let direction = if cmd.flag("h") {
        SplitDirection::Horizontal
    } else {
        SplitDirection::Vertical
    };
    let scroll_on_erase_in_display = state.options.scroll_on_erase_in_display;
    let history_limit = state.options.history_limit;

    {
        let session = match active_session_mut(state) {
            Some(s) => s,
            None => return,
        };
        let pane_id = session.next_pane_id;
        session.next_pane_id += 1;

        if let Some(win) = session.windows.get_mut(session.active_window_idx) {
            if let Some(zoom) = win.zoom_state.take() {
                let active_id = zoom.zoomed_pane_id;
                restore_split_sizes(&mut win.root, &[], &zoom.saved_sizes);
                win.active_pane_path =
                    crate::layout::find_pane_path(&win.root, active_id)
                        .unwrap_or_else(|| first_leaf_path(&win.root));
            }
        }

        let (fallback_rows, fallback_cols) = root_pane_size(sz);
        let (rows, cols, start_dir) = {
            let win = match session.windows.get(session.active_window_idx) {
                Some(w) => w,
                None => return,
            };
            let start_dir = win
                .default_start_dir
                .clone()
                .or_else(|| active_pane_start_dir(win))
                .or_else(crate::pty::default_start_dir);
            if let Some(p) =
                crate::layout::active_pane(&win.root, &win.active_pane_path)
            {
                (p.last_rows, p.last_cols, start_dir)
            } else {
                (fallback_rows, fallback_cols, start_dir)
            }
        };

        let new_pane = match spawn_pane(SpawnOptions {
            pane_id,
            rows: if direction == SplitDirection::Vertical {
                (rows / 2).max(1)
            } else {
                rows
            },
            cols: if direction == SplitDirection::Horizontal {
                (cols / 2).max(1)
            } else {
                cols
            },
            history_limit,
            command: None,
            start_dir: start_dir.as_deref(),
            env: vec![],
            scroll_on_erase_in_display,
        }) {
            Ok(p) => p,
            Err(_) => return,
        };

        let win = match session.windows.get_mut(session.active_window_idx) {
            Some(w) => w,
            None => return,
        };
        let path = win.active_pane_path.clone();
        let old_root = std::mem::replace(
            &mut win.root,
            LayoutNode::Split {
                direction: SplitDirection::Horizontal,
                sizes: vec![],
                children: vec![],
            },
        );
        win.root = split_node(old_root, &path, direction, new_pane, false);
        let mut new_path = path.clone();
        new_path.push(1);
        win.active_pane_path = new_path;
        win.pane_mru.insert(0, pane_id);
    }

    resize_all_panes(state, sz);
}

fn cmd_new_window(state: &mut Server, cmd: &ParsedCommand, sz: Size) {
    let scroll_on_erase_in_display = state.options.scroll_on_erase_in_display;
    let history_limit = state.options.history_limit;
    {
        let session = match active_session_mut(state) {
            Some(s) => s,
            None => return,
        };
        let pane_id = session.alloc_pane_id();
        let window_id = session.alloc_window_id();
        let name = cmd.flag_value("n").unwrap_or("shell").to_string();
        let start_dir = active_window_start_dir(session);

        let (rows, cols) = root_pane_size(sz);
        let pane = match spawn_pane(SpawnOptions {
            pane_id,
            rows,
            cols,
            history_limit,
            command: None,
            start_dir: start_dir.as_deref(),
            env: vec![],
            scroll_on_erase_in_display,
        }) {
            Ok(p) => p,
            Err(_) => return,
        };

        let win = Window {
            id: window_id,
            name,
            root: LayoutNode::Leaf(pane),
            active_pane_path: vec![],
            options: WindowOptions::with_defaults(),
            pane_mru: vec![pane_id],
            zoom_state: None,
            flags: Default::default(),
            layout_index: 0,
            last_output_time: Instant::now(),
            last_seen_version: 0,
            default_start_dir: None,
        };
        let detached = cmd.flag("d");
        session.windows.push(win);
        if !detached {
            session.active_window_idx = session.windows.len() - 1;
        }
    }

    resize_all_panes(state, sz);
}

fn cmd_set_pane_start_dir(state: &mut Server) -> String {
    let session = match active_session_mut(state) {
        Some(s) => s,
        None => return String::new(),
    };
    let win = match session.windows.get_mut(session.active_window_idx) {
        Some(w) => w,
        None => return String::new(),
    };
    let cwd = match active_pane_start_dir(win) {
        Some(dir) => dir,
        None => return String::new(),
    };
    let path = win.active_pane_path.clone();
    if let Some(pane) = crate::layout::active_pane_mut(&mut win.root, &path) {
        pane.start_dir = Some(cwd.clone());
    }
    win.default_start_dir = Some(cwd.clone());
    cwd
}

fn cmd_kill_pane(state: &mut Server, _cmd: &ParsedCommand, sz: Size) {
    let changed = {
        let session = match active_session_mut(state) {
            Some(s) => s,
            None => return,
        };
        let path = match session.windows.get(session.active_window_idx) {
            Some(w) => w.active_pane_path.clone(),
            None => return,
        };

        if path.is_empty() {
            if session.windows.len() > 1 {
                session.windows.remove(session.active_window_idx);
                session.active_window_idx =
                    session.active_window_idx.saturating_sub(1);
                true
            } else {
                false
            }
        } else {
            let win = match session.windows.get_mut(session.active_window_idx) {
                Some(w) => w,
                None => return,
            };
            let placeholder = LayoutNode::Split {
                direction: SplitDirection::Horizontal,
                sizes: vec![],
                children: vec![],
            };
            let old_root = std::mem::replace(&mut win.root, placeholder);
            if let Some(new_root) = kill_pane_at_path(old_root, &path) {
                win.root = new_root;
                win.active_pane_path = first_leaf_path(&win.root);
                true
            } else {
                false
            }
        }
    };

    if changed {
        resize_all_panes(state, sz);
    }
}

fn cmd_kill_window(state: &mut Server, sz: Size) {
    let session = match active_session_mut(state) {
        Some(s) => s,
        None => return,
    };
    if session.windows.is_empty() {
        return;
    }
    session.windows.remove(session.active_window_idx);
    if session.active_window_idx > 0 {
        session.active_window_idx -= 1;
    }
    if !session.windows.is_empty() {
        resize_all_panes(state, sz);
    }
}

fn cmd_select_pane(state: &mut Server, cmd: &ParsedCommand, sz: Size) {
    use crate::layout::{find_pane_path, pane_path_in_direction, NavDir};

    let hide_borders = state.hide_borders;
    let session = match active_session_mut(state) {
        Some(s) => s,
        None => return,
    };
    let win = match session.windows.get_mut(session.active_window_idx) {
        Some(w) => w,
        None => return,
    };

    if let Some(target) = cmd.flag_value("t") {
        if let Some(id_str) = target.strip_prefix('%') {
            if let Ok(pane_id) = id_str.parse::<usize>() {
                if let Some(path) = find_pane_path(&win.root, pane_id) {
                    win.active_pane_path = path;
                    record_pane_focus(win, pane_id);
                }
            }
        }
        return;
    }

    let area = frame_layout_area(sz);

    let dir = if cmd.flag("L") {
        Some(NavDir::Left)
    } else if cmd.flag("R") {
        Some(NavDir::Right)
    } else if cmd.flag("U") {
        Some(NavDir::Up)
    } else if cmd.flag("D") {
        Some(NavDir::Down)
    } else {
        None
    };

    if let Some(d) = dir {
        let border_size: u16 = if hide_borders { 0 } else { BORDER_SIZE };
        let path = win.active_pane_path.clone();
        let new_path = pane_path_in_direction(
            &win.root,
            &path,
            d,
            area,
            border_size,
            &win.pane_mru,
        );
        if new_path != path {
            let pane_id =
                crate::layout::active_pane(&win.root, &new_path).map(|p| p.id);
            win.active_pane_path = new_path;
            if let Some(pane_id) = pane_id {
                record_pane_focus(win, pane_id);
            }
        }
    }
}

fn record_pane_focus(win: &mut Window, pane_id: PaneId) {
    win.pane_mru.retain(|id| *id != pane_id);
    win.pane_mru.insert(0, pane_id);
}

fn cmd_resize_pane(state: &mut Server, cmd: &ParsedCommand, sz: Size) {
    use crate::layout::NavDir;

    let dir = if cmd.flag("L") {
        Some(NavDir::Left)
    } else if cmd.flag("R") {
        Some(NavDir::Right)
    } else if cmd.flag("U") {
        Some(NavDir::Up)
    } else if cmd.flag("D") {
        Some(NavDir::Down)
    } else {
        None
    };
    let Some(dir) = dir else {
        return;
    };
    let step = cmd
        .args
        .first()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(5)
        .max(1);

    let changed = {
        let session = match active_session_mut(state) {
            Some(s) => s,
            None => return,
        };
        let win = match session.windows.get_mut(session.active_window_idx) {
            Some(w) => w,
            None => return,
        };
        if win.zoom_state.is_some() {
            false
        } else {
            let path = win.active_pane_path.clone();
            resize_layout_in_direction(&mut win.root, &path, dir, step)
        }
    };

    if changed {
        resize_all_panes(state, sz);
        mark_data_ready();
    }
}

fn cmd_select_window(state: &mut Server, cmd: &ParsedCommand, sz: Size) {
    let session = match active_session_mut(state) {
        Some(s) => s,
        None => return,
    };
    let n = session.windows.len();
    if n == 0 {
        return;
    }
    if cmd.flag("n") {
        session.active_window_idx = (session.active_window_idx + 1) % n;
    } else if cmd.flag("p") {
        session.active_window_idx = (session.active_window_idx + n - 1) % n;
    } else if let Some(idx_str) = cmd.flag_value("t") {
        if let Ok(idx) = idx_str.parse::<usize>() {
            if idx < n {
                session.active_window_idx = idx;
            }
        }
    }
    resize_all_panes(state, sz);
}

fn cmd_rename_window(state: &mut Server, cmd: &ParsedCommand) {
    let session = match active_session_mut(state) {
        Some(s) => s,
        None => return,
    };
    let win = match session.windows.get_mut(session.active_window_idx) {
        Some(w) => w,
        None => return,
    };
    if let Some(name) = cmd.args.first() {
        win.name = name.clone();
    }
}

fn cmd_rename_session(state: &mut Server, cmd: &ParsedCommand) {
    let session = match state.sessions.first_mut() {
        Some(s) => s,
        None => return,
    };
    if let Some(name) = cmd.args.first() {
        session.name = name.clone();
    }
}

fn cmd_zoom_pane(state: &mut Server, sz: Size) {
    let session = match active_session_mut(state) {
        Some(s) => s,
        None => return,
    };
    let win = match session.windows.get_mut(session.active_window_idx) {
        Some(w) => w,
        None => return,
    };

    if win.zoom_state.is_some() {
        let zoom = win.zoom_state.take().unwrap();
        let active_id = zoom.zoomed_pane_id;
        restore_split_sizes(&mut win.root, &[], &zoom.saved_sizes);
        win.active_pane_path =
            crate::layout::find_pane_path(&win.root, active_id)
                .unwrap_or_else(|| first_leaf_path(&win.root));
    } else {
        let active_id = match crate::layout::active_pane(
            &win.root,
            &win.active_pane_path,
        ) {
            Some(p) => p.id,
            None => return,
        };

        if matches!(win.root, LayoutNode::Leaf(_)) {
            return;
        }

        let mut saved_sizes: Vec<(Vec<usize>, Vec<u16>)> = Vec::new();
        collect_split_sizes(&win.root, &[], &mut saved_sizes);

        set_all_sizes_to_full(&mut win.root, active_id);

        win.zoom_state = Some(crate::types::session::ZoomState {
            saved_sizes,
            zoomed_pane_id: active_id,
        });
    }

    resize_all_panes(state, sz);
}

fn collect_split_sizes(
    node: &LayoutNode,
    path: &[usize],
    out: &mut Vec<(Vec<usize>, Vec<u16>)>,
) {
    if let LayoutNode::Split {
        sizes, children, ..
    } = node
    {
        out.push((path.to_vec(), sizes.clone()));
        for (i, child) in children.iter().enumerate() {
            let mut child_path = path.to_vec();
            child_path.push(i);
            collect_split_sizes(child, &child_path, out);
        }
    }
}

fn restore_split_sizes(
    node: &mut LayoutNode,
    path: &[usize],
    saved: &[(Vec<usize>, Vec<u16>)],
) {
    if let LayoutNode::Split {
        sizes, children, ..
    } = node
    {
        if let Some((_, saved_sizes)) = saved.iter().find(|(p, _)| p == path) {
            *sizes = saved_sizes.clone();
        }
        for (i, child) in children.iter_mut().enumerate() {
            let mut child_path = path.to_vec();
            child_path.push(i);
            restore_split_sizes(child, &child_path, saved);
        }
    }
}

fn set_all_sizes_to_full(node: &mut LayoutNode, active_id: PaneId) {
    if let LayoutNode::Split {
        sizes, children, ..
    } = node
    {
        let active_child = children
            .iter()
            .position(|c| subtree_contains_pane(c, active_id))
            .unwrap_or(0);
        for (i, s) in sizes.iter_mut().enumerate() {
            *s = if i == active_child { 100 } else { 1 };
        }
        for child in children.iter_mut() {
            set_all_sizes_to_full(child, active_id);
        }
    }
}

fn subtree_contains_pane(node: &LayoutNode, id: PaneId) -> bool {
    match node {
        LayoutNode::Leaf(p) => p.id == id,
        LayoutNode::Split { children, .. } => {
            children.iter().any(|c| subtree_contains_pane(c, id))
        }
    }
}

fn resize_layout_in_direction(
    node: &mut LayoutNode,
    path: &[usize],
    dir: crate::layout::NavDir,
    step: u16,
) -> bool {
    match node {
        LayoutNode::Leaf(_) => false,
        LayoutNode::Split {
            direction,
            sizes,
            children,
        } => {
            let Some((&idx, rest)) = path.split_first() else {
                return false;
            };
            if idx >= children.len() {
                return false;
            }
            if resize_layout_in_direction(&mut children[idx], rest, dir, step) {
                return true;
            }
            if !split_matches_resize_direction(*direction, dir) {
                return false;
            }
            resize_split_sizes(sizes, idx, dir, step)
        }
    }
}

fn split_matches_resize_direction(
    direction: SplitDirection,
    dir: crate::layout::NavDir,
) -> bool {
    matches!(
        (direction, dir),
        (SplitDirection::Horizontal, crate::layout::NavDir::Left)
            | (SplitDirection::Horizontal, crate::layout::NavDir::Right)
            | (SplitDirection::Vertical, crate::layout::NavDir::Up)
            | (SplitDirection::Vertical, crate::layout::NavDir::Down)
    )
}

fn resize_split_sizes(
    sizes: &mut [u16],
    idx: usize,
    dir: crate::layout::NavDir,
    step: u16,
) -> bool {
    let Some((neighbor_idx, grow_active)) =
        resize_target_for_index(idx, sizes.len(), dir)
    else {
        return false;
    };
    shift_split_sizes(sizes, idx, neighbor_idx, grow_active, step)
}

fn resize_target_for_index(
    idx: usize,
    len: usize,
    dir: crate::layout::NavDir,
) -> Option<(usize, bool)> {
    match dir {
        crate::layout::NavDir::Left | crate::layout::NavDir::Up => {
            if idx > 0 {
                Some((idx - 1, true))
            } else if idx + 1 < len {
                Some((idx + 1, false))
            } else {
                None
            }
        }
        crate::layout::NavDir::Right | crate::layout::NavDir::Down => {
            if idx + 1 < len {
                Some((idx + 1, true))
            } else if idx > 0 {
                Some((idx - 1, false))
            } else {
                None
            }
        }
    }
}

fn shift_split_sizes(
    sizes: &mut [u16],
    idx: usize,
    neighbor_idx: usize,
    grow_active: bool,
    step: u16,
) -> bool {
    if idx >= sizes.len() || neighbor_idx >= sizes.len() || idx == neighbor_idx
    {
        return false;
    }
    let donor_idx = if grow_active { neighbor_idx } else { idx };
    let delta = step.min(sizes[donor_idx].saturating_sub(1));
    if delta == 0 {
        return false;
    }
    if grow_active {
        sizes[idx] += delta;
        sizes[neighbor_idx] -= delta;
    } else {
        sizes[idx] -= delta;
        sizes[neighbor_idx] += delta;
    }
    true
}

fn cmd_clear_pane(state: &mut Server) {
    with_active_pane_mut(state, |pane| {
        pane.copy_state = None;
        let _history_guard = pane
            .history_serial
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        pane.history_writer.clear();
        if let Ok(mut parser) = pane.parser.lock() {
            let scroll_on_erase_in_display =
                parser.scroll_on_erase_in_display();
            let history_limit = parser.scrollback_limit();
            *parser = crate::terminal::AlacrittyTermState::new(
                pane.last_rows,
                pane.last_cols,
                history_limit,
            );
            parser.set_scroll_on_erase_in_display(scroll_on_erase_in_display);
            if scroll_on_erase_in_display {
                parser.suppress_next_scroll_on_erase_in_display();
            }
        }
        if let Ok(mut writer) = pane.writer.lock() {
            let _ = writer.write_all(b"\x0c");
            let _ = writer.flush();
        }
    });
}

fn json_escape_status(s: &str, out: &mut String) {
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_frame() -> FrameData {
        serde_json::from_str(
            r#"{"type":"frame","layout":{"type":"leaf","id":1,"rows":1,"cols":1,"cursor_row":0,"cursor_col":0,"hide_cursor":false,"alternate_screen":false,"mouse_mode":0,"in_copy_mode":false,"cursor_shape":0,"active":true,"rows_v2":[]},"ansi":""}"#,
        )
        .unwrap()
    }

    fn active_pane_size(state: &Server) -> (u16, u16) {
        let session = &state.sessions[state.active_session_idx];
        let win = &session.windows[session.active_window_idx];
        let pane = crate::layout::active_pane(&win.root, &win.active_pane_path)
            .unwrap();
        (pane.last_rows, pane.last_cols)
    }

    fn active_pane_history_limit(state: &Server) -> usize {
        let session = &state.sessions[state.active_session_idx];
        let win = &session.windows[session.active_window_idx];
        let pane = crate::layout::active_pane(&win.root, &win.active_pane_path)
            .unwrap();
        pane.parser
            .lock()
            .map(|parser| parser.scrollback_limit())
            .unwrap_or_default()
    }

    fn active_frame_size(
        layout: &crate::client::LayoutJson,
    ) -> Option<(u16, u16)> {
        match layout {
            crate::client::LayoutJson::Leaf {
                rows, cols, active, ..
            } if *active => Some((*rows, *cols)),
            crate::client::LayoutJson::Leaf { .. } => None,
            crate::client::LayoutJson::Split { children, .. } => {
                children.iter().find_map(active_frame_size)
            }
        }
    }

    #[test]
    fn resize_all_panes_sets_force_clear_display() -> io::Result<()> {
        let sz = Size::new(24, 80);
        let mut state = Server::new();
        make_session(&mut state, "0", sz)?;
        resize_all_panes(&mut state, sz);
        assert!(state.force_clear_display);
        Ok(())
    }

    #[test]
    fn history_limit_applies_to_existing_and_new_panes() -> io::Result<()> {
        let sz = Size::new(24, 80);
        let mut state = Server::new();
        state.options.history_limit = 17;
        make_session(&mut state, "0", sz)?;
        assert_eq!(active_pane_history_limit(&state), 17);

        let cmd =
            ParsedCommand::parse("set-option -g history-limit 23").remove(0);
        assert_eq!(cmd_set_option(&mut state, &cmd), "history-limit: 23");
        assert_eq!(state.options.history_limit, 23);
        assert_eq!(active_pane_history_limit(&state), 23);

        let split = ParsedCommand::parse("split-window -h").remove(0);
        cmd_split_window(&mut state, &split, sz);
        let win = &state.sessions[0].windows[0];
        for pane_id in crate::layout::collect_pane_ids(&win.root) {
            let pane =
                crate::layout::find_pane_by_id(&win.root, pane_id).unwrap();
            assert_eq!(
                pane.parser
                    .lock()
                    .map(|parser| parser.scrollback_limit())
                    .unwrap_or_default(),
                23
            );
        }

        let new_window = ParsedCommand::parse("new-window").remove(0);
        cmd_new_window(&mut state, &new_window, sz);
        assert_eq!(active_pane_history_limit(&state), 23);

        let new_session =
            ParsedCommand::parse("new-session -s history-test").remove(0);
        cmd_new_session(&mut state, &new_session, sz);
        assert_eq!(active_pane_history_limit(&state), 23);

        let invalid =
            ParsedCommand::parse("set-option -g history-limit 100001")
                .remove(0);
        assert!(cmd_set_option(&mut state, &invalid).contains("between 0"));
        assert_eq!(state.options.history_limit, 23);
        kill_all_panes(&mut state);
        Ok(())
    }

    #[test]
    fn clear_pane_preserves_its_history_limit() -> io::Result<()> {
        let sz = Size::new(24, 80);
        let mut state = Server::new();
        make_session(&mut state, "0", sz)?;
        {
            let session = state.active_session_mut().unwrap();
            let win = session.active_window_mut().unwrap();
            let pane = crate::layout::active_pane_mut(
                &mut win.root,
                &win.active_pane_path,
            )
            .unwrap();
            pane.parser.lock().unwrap().set_scrollback_limit(29);
            *pane.history.lock().unwrap() =
                crate::history_store::PaneHistory::for_test(
                    std::env::temp_dir().join(format!(
                        "zmux-server-clear-history-test-{}",
                        std::process::id()
                    )),
                    100,
                );
            pane.history
                .lock()
                .unwrap()
                .append(&crate::types::SnapshotLine {
                    text: "archived".to_string(),
                    terminated: true,
                    styles: Vec::new(),
                })
                .unwrap();
        }

        cmd_clear_pane(&mut state);
        assert_eq!(active_pane_history_limit(&state), 29);
        {
            let session = state.active_session().unwrap();
            let win = session.active_window().unwrap();
            let pane =
                crate::layout::active_pane(&win.root, &win.active_pane_path)
                    .unwrap();
            assert!(pane.history.lock().unwrap().is_empty());
        }
        kill_all_panes(&mut state);
        Ok(())
    }

    #[test]
    fn targeted_scroll_does_not_mutate_the_active_sibling_pane(
    ) -> io::Result<()> {
        let sz = Size::new(24, 80);
        let mut state = Server::new();
        make_session(&mut state, "0", sz)?;

        let mut split = ParsedCommand::parse("split-window -h");
        cmd_split_window(&mut state, &split.remove(0), sz);
        let mut split = ParsedCommand::parse("split-window -v");
        cmd_split_window(&mut state, &split.remove(0), sz);

        let pane_ids = {
            let win = &state.sessions[0].windows[0];
            crate::layout::collect_pane_ids(&win.root)
        };
        assert_eq!(pane_ids.len(), 3);
        let p1 = pane_ids[0];
        let p2 = pane_ids[1];

        {
            let win = &mut state.sessions[0].windows[0];
            {
                let pane =
                    crate::layout::find_pane_by_id_mut(&mut win.root, p2)
                        .unwrap();
                let mut parser = pane.parser.lock().unwrap();
                for line in 0..100 {
                    parser.process(format!("p2 line {line}\r\n").as_bytes());
                }
            }
            win.active_pane_path =
                crate::layout::find_pane_path(&win.root, p1).unwrap();
        }

        let state = Arc::new(Mutex::new(state));
        handle_scroll_line(&state, &format!("up 3 %{p2}"));

        let state = state.lock().unwrap();
        let win = &state.sessions[0].windows[0];
        let p1 = crate::layout::find_pane_by_id(&win.root, p1).unwrap();
        let p2 = crate::layout::find_pane_by_id(&win.root, p2).unwrap();
        assert!(
            p1.copy_state.is_none(),
            "scrolling p2 must not put active sibling p1 into copy mode"
        );
        assert!(
            p2.copy_state.is_some(),
            "target pane must receive the scroll operation"
        );
        Ok(())
    }

    #[test]
    fn resize_pane_changes_layout_fingerprint() -> io::Result<()> {
        let sz = Size::new(24, 80);
        let mut state = Server::new();
        make_session(&mut state, "0", sz)?;
        let mut split_cmd = ParsedCommand::parse("split-window -h");
        cmd_split_window(&mut state, &split_cmd.remove(0), sz);

        let win = &state.sessions[0].windows[0];
        let area = frame_ansi_area(sz);
        let before = layout_fingerprint(win, area, false);

        let mut resize_cmd = ParsedCommand::parse("resize-pane -L 5");
        cmd_resize_pane(&mut state, &resize_cmd.remove(0), sz);

        let win = &state.sessions[0].windows[0];
        let after = layout_fingerprint(win, area, false);
        assert_ne!(before, after);
        Ok(())
    }

    fn set_all_render_dirty(node: &LayoutNode, dirty: bool) {
        match node {
            LayoutNode::Leaf(pane) => {
                pane.render_dirty.store(dirty, Ordering::Relaxed)
            }
            LayoutNode::Split { children, .. } => {
                for child in children {
                    set_all_render_dirty(child, dirty);
                }
            }
        }
    }

    fn corner_count(ansi: &str) -> usize {
        ansi.matches('┌').count()
    }

    #[test]
    fn force_repaint_paints_all_panes_without_destructive_clear(
    ) -> io::Result<()> {
        // Two-pane layout. Regression for "other pane's text disappears" and
        // "panes flicker": a steady-state frame (no layout change) must repaint
        // every pane (force_repaint) yet must NOT emit the full-screen clear.
        let sz = Size::new(24, 80);
        let mut state = Server::new();
        make_session(&mut state, "0", sz)?;
        let mut split = ParsedCommand::parse("split-window -h");
        cmd_split_window(&mut state, &split.remove(0), sz);

        let win = &state.sessions[0].windows[0];
        let area = frame_ansi_area(sz);

        // Even with every pane marked clean, force_repaint repaints all panes.
        set_all_render_dirty(&win.root, false);
        let repainted = serialize_frame_ansi(
            win,
            area,
            false,
            FrameAnsiOptions {
                clear_display: false,
                force_repaint: true,
            },
        );
        assert!(
            !repainted.contains("\x1b[K"),
            "steady-state force_repaint must not emit a destructive screen clear"
        );
        assert!(
            corner_count(&repainted) >= 2,
            "force_repaint must repaint both panes even when clean, got: {repainted:?}"
        );

        Ok(())
    }

    #[test]
    fn clean_panes_are_skipped_without_force_repaint() -> io::Result<()> {
        // Incremental frames (no force_repaint, no clear) skip clean panes — this is
        // exactly why a dropped incremental frame loses content, which force_repaint
        // on the FRAME? path now prevents.
        let sz = Size::new(24, 80);
        let mut state = Server::new();
        make_session(&mut state, "0", sz)?;
        let mut split = ParsedCommand::parse("split-window -h");
        cmd_split_window(&mut state, &split.remove(0), sz);

        let win = &state.sessions[0].windows[0];
        let area = frame_ansi_area(sz);

        set_all_render_dirty(&win.root, false);
        let incremental =
            serialize_frame_ansi(win, area, false, FrameAnsiOptions::default());
        assert_eq!(
            corner_count(&incremental),
            0,
            "clean panes must be skipped without force_repaint/clear_display"
        );

        Ok(())
    }

    #[test]
    fn pane_dirty_repaint_does_not_erase_inner_first() -> io::Result<()> {
        // Erasing the inner rect before every dirty paint flashes blank/default
        // cells when synchronized updates are not atomic. Content rows overwrite
        // in place across the full width instead.
        let sz = Size::new(24, 80);
        let mut state = Server::new();
        make_session(&mut state, "0", sz)?;

        let win = &state.sessions[0].windows[0];
        set_all_render_dirty(&win.root, true);
        let ansi = serialize_frame_ansi(
            win,
            frame_ansi_area(sz),
            false,
            FrameAnsiOptions {
                clear_display: false,
                force_repaint: false,
            },
        );
        // A full erase pass would vte_goto every inner row twice (erase + content).
        // In-place paint does one reset-goto per row; a trailing wrap-cancel CUP
        // (no SGR reset) is not an erase.
        let inner_row0_goto = "\x1b[3;2H\x1b[m";
        assert_eq!(
            ansi.matches(inner_row0_goto).count(),
            1,
            "dirty paint must not erase-then-paint (expected one row-start cup), \
             got count {} in: {ansi:?}",
            ansi.matches(inner_row0_goto).count()
        );
        Ok(())
    }

    #[test]
    fn dirty_incremental_paint_restores_vertical_borders() -> io::Result<()> {
        // Incremental dirty frames used to skip borders. A host-width mismatch
        // or last-column wrap then left stray cells just past the right edge
        // for the rest of the session.
        let sz = Size::new(24, 80);
        let mut state = Server::new();
        make_session(&mut state, "0", sz)?;

        let win = &state.sessions[0].windows[0];
        set_all_render_dirty(&win.root, true);
        let ansi = serialize_frame_ansi(
            win,
            frame_ansi_area(sz),
            false,
            FrameAnsiOptions {
                clear_display: false,
                force_repaint: false,
            },
        );
        assert!(
            ansi.contains("\x1b[3;80H") && ansi.contains('│'),
            "dirty incremental paint must restore the right border column, got {ansi:?}"
        );
        assert_eq!(
            corner_count(&ansi),
            0,
            "incremental paint should not redraw full-frame corners"
        );
        Ok(())
    }

    #[test]
    fn clear_display_emits_full_screen_clear() -> io::Result<()> {
        let sz = Size::new(24, 80);
        let mut state = Server::new();
        make_session(&mut state, "0", sz)?;

        let win = &state.sessions[0].windows[0];
        let area = frame_ansi_area(sz);
        let full = serialize_frame_ansi(
            win,
            area,
            false,
            FrameAnsiOptions {
                clear_display: true,
                force_repaint: true,
            },
        );
        assert!(
            !full.contains("\x1b[K"),
            "clear_display must not EL (would wipe sibling panes on the same row)"
        );
        // Space-fill clear starts at the ansi area origin (row 2, col 1 in 1-based).
        assert!(
            full.contains("\x1b[2;1H"),
            "clear_display frame must space-fill the pane area, got {full:?}"
        );
        Ok(())
    }

    #[test]
    fn refresh_latest_frame_rebuilds_after_resize() -> io::Result<()> {
        let initial = Size::new(50, 180);
        let resized = Size::new(30, 180);
        let mut state = Server::new();
        make_session(&mut state, "0", initial)?;
        let latest_frame = Arc::new(Mutex::new(FrameStore::default()));

        refresh_latest_frame(&latest_frame, &state, initial);
        let first = latest_frame.lock().unwrap().latest.clone().unwrap();
        assert_eq!(active_frame_size(&first.layout), Some((47, 178)));

        resize_all_panes(&mut state, resized);
        refresh_latest_frame(&latest_frame, &state, resized);
        let second = latest_frame.lock().unwrap().latest.clone().unwrap();
        assert_eq!(active_frame_size(&second.layout), Some((27, 178)));
        Ok(())
    }

    #[test]
    fn directional_navigation_returns_to_recent_pane_across_nested_split(
    ) -> io::Result<()> {
        let sz = Size::new(24, 80);
        let mut state = Server::new();
        make_session(&mut state, "0", sz)?;

        let mut split = ParsedCommand::parse("split-window -h");
        cmd_split_window(&mut state, &split.remove(0), sz);
        let mut split = ParsedCommand::parse("split-window -v");
        cmd_split_window(&mut state, &split.remove(0), sz);

        let pane_ids =
            crate::layout::collect_pane_ids(&state.sessions[0].windows[0].root);
        assert_eq!(pane_ids.len(), 3);
        let left = pane_ids[0];
        let right_top = pane_ids[1];
        let right_bottom = pane_ids[2];
        let active_id = |state: &Server| {
            let win = &state.sessions[0].windows[0];
            crate::layout::active_pane(&win.root, &win.active_pane_path)
                .unwrap()
                .id
        };
        let select = |state: &mut Server, flag: &str| {
            let mut commands =
                ParsedCommand::parse(&format!("select-pane {flag}"));
            cmd_select_pane(state, &commands.remove(0), sz);
        };

        assert_eq!(active_id(&state), right_bottom);
        select(&mut state, "-L");
        assert_eq!(active_id(&state), left);
        select(&mut state, "-R");
        assert_eq!(
            active_id(&state),
            right_bottom,
            "moving back across a one-to-many split should return to the pane \
             that focus came from"
        );

        select(&mut state, "-U");
        assert_eq!(active_id(&state), right_top);
        select(&mut state, "-L");
        assert_eq!(active_id(&state), left);
        select(&mut state, "-R");
        assert_eq!(
            active_id(&state),
            right_top,
            "MRU tie-breaking must track the latest top/bottom pane"
        );
        select(&mut state, "-D");
        assert_eq!(active_id(&state), right_bottom);
        select(&mut state, "-U");
        assert_eq!(active_id(&state), right_top);
        Ok(())
    }

    #[test]
    fn frame_store_merges_all_unread_ansi_deltas() {
        let mut store = FrameStore::default();
        let mut first = test_frame();
        first.ansi = Some(STANDARD.encode(b"nvim-update"));
        store.publish(first);

        let mut sibling = test_frame();
        sibling.ansi = Some(STANDARD.encode(b"ping-update"));
        store.publish(sibling);

        // Cursor-only output often produces a frame with no dirty rows. It must
        // not replace either unread content update.
        store.publish(test_frame());

        let (frame, sequence, missed) = store.frame_since(0);
        let ansi = STANDARD
            .decode(frame.unwrap().ansi.unwrap())
            .expect("merged ANSI must remain valid base64");
        assert_eq!(ansi, b"nvim-updateping-update");
        assert_eq!(sequence, 3);
        assert!(!missed);
    }

    #[test]
    fn frame_store_does_not_replay_last_delta_after_ack() {
        let mut store = FrameStore::default();
        let mut frame = test_frame();
        frame.ansi = Some(STANDARD.encode(b"one-update"));
        store.publish(frame);

        let (_, sequence, _) = store.frame_since(0);
        let (frame, _, _) = store.frame_since(sequence);
        let ansi = STANDARD
            .decode(frame.unwrap().ansi.unwrap())
            .expect("empty ANSI must remain valid base64");
        assert!(ansi.is_empty());
    }

    #[test]
    fn root_pane_size_matches_visible_content_area() {
        assert_eq!(root_pane_size(Size::new(24, 80)), (21, 78));
        assert_eq!(pane_viewport_size(Rect::new(0, 0, 2, 2), false), (2, 2));
    }

    #[test]
    fn select_window_resizes_new_active_window_pane() -> io::Result<()> {
        let sz = Size::new(24, 80);
        let expected = root_pane_size(sz);
        let mut state = Server::new();
        make_session(&mut state, "0", sz)?;

        let mut new_window_cmd = ParsedCommand::parse("new-window");
        cmd_new_window(&mut state, &new_window_cmd.remove(0), sz);

        {
            let session = &mut state.sessions[0];
            let win = &mut session.windows[1];
            let pane = crate::layout::active_pane_mut(
                &mut win.root,
                &win.active_pane_path,
            )
            .unwrap();
            resize_pane(pane, expected.0, (expected.1 / 2).max(1))?;
            session.active_window_idx = 0;
        }

        let mut select_window_cmd = ParsedCommand::parse("select-window -t 1");
        cmd_select_window(&mut state, &select_window_cmd.remove(0), sz);

        assert_eq!(state.sessions[0].active_window_idx, 1);
        assert_eq!(active_pane_size(&state), expected);
        Ok(())
    }

    #[test]
    fn switch_client_resizes_target_session_pane() -> io::Result<()> {
        let sz = Size::new(24, 80);
        let expected = root_pane_size(sz);
        let mut state = Server::new();
        make_session(&mut state, "alpha", sz)?;
        make_session(&mut state, "beta", sz)?;

        {
            let session = &mut state.sessions[1];
            let win = &mut session.windows[0];
            let pane = crate::layout::active_pane_mut(
                &mut win.root,
                &win.active_pane_path,
            )
            .unwrap();
            resize_pane(pane, expected.0, (expected.1 / 2).max(1))?;
        }

        let mut switch_client_cmd =
            ParsedCommand::parse("switch-client -t beta");
        cmd_switch_client(&mut state, &switch_client_cmd.remove(0), sz);

        assert_eq!(state.active_session_idx, 1);
        assert_eq!(active_pane_size(&state), expected);
        Ok(())
    }

    #[test]
    fn split_window_unzooms_before_splitting() -> io::Result<()> {
        let sz = Size::new(24, 80);
        let full_size = root_pane_size(sz);
        let mut state = Server::new();
        make_session(&mut state, "0", sz)?;

        let mut split_cmd = ParsedCommand::parse("split-window -h");
        cmd_split_window(&mut state, &split_cmd.remove(0), sz);
        let zoomed_id = {
            let session = &state.sessions[0];
            let win = &session.windows[0];
            crate::layout::active_pane(&win.root, &win.active_pane_path)
                .unwrap()
                .id
        };

        cmd_zoom_pane(&mut state, sz);
        assert!(state.sessions[0].windows[0].zoom_state.is_some());
        assert_eq!(active_pane_size(&state), full_size);

        let mut split_zoomed_cmd = ParsedCommand::parse("split-window -h");
        cmd_split_window(&mut state, &split_zoomed_cmd.remove(0), sz);

        let win = &state.sessions[0].windows[0];
        assert!(win.zoom_state.is_none());
        assert_eq!(crate::layout::leaf_count(&win.root), 3);
        let zoomed_pane =
            crate::layout::find_pane_by_id(&win.root, zoomed_id).unwrap();
        assert!(zoomed_pane.last_cols < full_size.1);
        Ok(())
    }

    #[test]
    fn reap_dead_panes_resizes_surviving_pane() -> io::Result<()> {
        let sz = Size::new(24, 80);
        let expected = root_pane_size(sz);
        let mut state = Server::new();
        make_session(&mut state, "0", sz)?;

        let mut split_cmd = ParsedCommand::parse("split-window -h");
        cmd_split_window(&mut state, &split_cmd.remove(0), sz);

        {
            let session = &mut state.sessions[0];
            let win = &mut session.windows[0];
            let pane = crate::layout::active_pane_mut(
                &mut win.root,
                &win.active_pane_path,
            )
            .unwrap();
            pane.dead.store(true, std::sync::atomic::Ordering::Relaxed);
        }

        reap_dead_panes(&mut state, sz);

        assert_eq!(active_pane_size(&state), expected);
        assert!(matches!(
            state.sessions[0].windows[0].root,
            LayoutNode::Leaf(_)
        ));
        Ok(())
    }

    /// Right-pane paint must stay inside its content rect. If we emit more
    /// columns than fit from `inner.x` to the terminal edge, the host wraps to
    /// column 0 and wipes the left pane (the "ls corrupts ping" failure mode).
    #[test]
    fn horizontal_split_ansi_does_not_wrap_into_left_pane() -> io::Result<()> {
        let sz = Size::new(24, 80);
        let mut state = Server::new();
        make_session(&mut state, "0", sz)?;
        let mut split = ParsedCommand::parse("split-window -h");
        cmd_split_window(&mut state, &split.remove(0), sz);

        let win = &state.sessions[0].windows[0];
        let LayoutNode::Split { children, .. } = &win.root else {
            panic!("expected horizontal split");
        };
        let (left, right) = match (&children[0], &children[1]) {
            (LayoutNode::Leaf(l), LayoutNode::Leaf(r)) => (l, r),
            _ => panic!("expected two leaves"),
        };
        // Each pane must be roughly half — not still full-width.
        assert!(
            right.last_cols < 50,
            "right pane still full-width? cols={}",
            right.last_cols
        );
        assert!(
            left.last_cols < 50,
            "left pane still full-width? cols={}",
            left.last_cols
        );

        // Fill the right pane with a dense row (ls-like) so any over-paint wraps.
        {
            let line = "X".repeat(right.last_cols as usize);
            let mut payload = String::new();
            for _ in 0..right.last_rows {
                payload.push_str(&line);
                payload.push('\n');
            }
            if let Ok(mut parser) = right.parser.lock() {
                parser.process(payload.as_bytes());
            }
            right.mark_render_dirty();
        }

        let area = frame_ansi_area(sz);
        let ansi = serialize_frame_ansi(
            win,
            area,
            false,
            FrameAnsiOptions {
                clear_display: false,
                force_repaint: true,
            },
        );

        // Host wrap at the physical line end wipes column 0 of the next row
        // (the left pane). Also ensure right-pane 'X' fill never lands left of
        // the split gap.
        let term_cols = sz.cols as usize;
        let rects = crate::layout::compute_rects(
            &win.root,
            area,
            crate::layout::BORDER_SIZE,
        );
        let right_rect = *rects.get(&right.id).expect("right rect");
        let left_limit = right_rect.x as usize;

        let mut col = 0usize;
        let mut row = 0usize;
        let mut screen: Vec<Vec<char>> =
            vec![vec![' '; term_cols]; sz.rows as usize];
        let mut chars = ansi.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\x1b' {
                if chars.peek() == Some(&'[') {
                    chars.next();
                    let mut params = String::new();
                    let mut final_byte = ' ';
                    for c in chars.by_ref() {
                        if c.is_ascii_alphabetic() || c == '~' {
                            final_byte = c;
                            break;
                        }
                        params.push(c);
                    }
                    if final_byte == 'H' || final_byte == 'f' {
                        let mut parts = params.split(';');
                        let r: usize =
                            parts.next().unwrap_or("1").parse().unwrap_or(1);
                        let c: usize =
                            parts.next().unwrap_or("1").parse().unwrap_or(1);
                        row = r.saturating_sub(1);
                        col = c.saturating_sub(1);
                    }
                }
                continue;
            }
            if ch == '\n' {
                row = row.saturating_add(1);
                col = 0;
                continue;
            }
            if ch == '\r' {
                col = 0;
                continue;
            }
            if ch < ' ' {
                continue;
            }
            let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
            assert!(
                col + w <= term_cols,
                "glyph {ch:?} at ({row},{col}) width {w} wraps past term_cols={term_cols}"
            );
            if row < screen.len() && col < term_cols {
                screen[row][col] = ch;
            }
            col += w;
        }

        for (r, line) in screen.iter().enumerate() {
            for (c, ch) in line.iter().enumerate().take(left_limit) {
                assert_ne!(
                    *ch, 'X',
                    "right-pane content leaked into left pane at ({r},{c})"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn reap_dead_left_pane_expands_nested_right_split() -> io::Result<()> {
        let sz = Size::new(24, 80);
        let mut state = Server::new();
        make_session(&mut state, "0", sz)?;

        let mut split_right_cmd = ParsedCommand::parse("split-window -h");
        cmd_split_window(&mut state, &split_right_cmd.remove(0), sz);
        let mut split_bottom_cmd = ParsedCommand::parse("split-window");
        cmd_split_window(&mut state, &split_bottom_cmd.remove(0), sz);

        {
            let win = &mut state.sessions[0].windows[0];
            let LayoutNode::Split { children, .. } = &mut win.root else {
                panic!("expected horizontal root split");
            };
            let LayoutNode::Leaf(pane) = &mut children[0] else {
                panic!("expected left pane");
            };
            pane.dead.store(true, std::sync::atomic::Ordering::Relaxed);
        }

        reap_dead_panes(&mut state, sz);

        let win = &state.sessions[0].windows[0];
        let LayoutNode::Split {
            direction,
            children,
            ..
        } = &win.root
        else {
            panic!("expected surviving vertical split");
        };
        assert_eq!(*direction, SplitDirection::Vertical);
        assert_eq!(children.len(), 2);
        for child in children {
            let LayoutNode::Leaf(pane) = child else {
                panic!("expected surviving pane");
            };
            assert_eq!(pane.last_cols, 78);
            assert!(pane.last_rows > 0);
        }
        Ok(())
    }
}
