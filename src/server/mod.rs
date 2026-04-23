use std::{
    collections::HashMap,
    io::{self, Write},
    sync::{atomic::Ordering, Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use crate::{
    client::FrameData,
    commands::ParsedCommand,
    layout::{
        compute_rects, first_leaf_path, kill_pane_at_path, serialize_frame,
        split_node,
    },
    pty::{resize_pane, spawn_pane, SpawnOptions},
    types::{
        events::PTY_DATA_READY,
        layout::{LayoutNode, Rect, SplitDirection},
        options::{SessionOptions, WindowOptions},
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
    latest_frame: Arc<Mutex<Option<FrameData>>>,
    size: Arc<Mutex<Size>>,
}

impl InProcessServer {
    pub fn start(
        session_name: String,
        size: Size,
        socket_name: Option<String>,
    ) -> io::Result<Self> {
        let state = Arc::new(Mutex::new(Server::new()));
        let latest_frame: Arc<Mutex<Option<FrameData>>> =
            Arc::new(Mutex::new(None));
        let size_arc = Arc::new(Mutex::new(size));

        {
            let mut s = state.lock().unwrap();
            create_initial_session(&mut s, &session_name, size)?;
        }

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
        self.latest_frame.lock().ok()?.clone()
    }

    pub fn send_input(&self, bytes: &[u8]) {
        let mut s = match self.state.lock() {
            Ok(s) => s,
            Err(_) => return,
        };
        let session = match s.active_session_mut() {
            Some(s) => s,
            None => return,
        };
        let win = match session.windows.get_mut(session.active_window_idx) {
            Some(w) => w,
            None => return,
        };
        if let Some(pane) =
            crate::layout::active_pane_mut(&mut win.root, &win.active_pane_path)
        {
            let _ = pane.writer.write_all(bytes);
            let _ = pane.writer.flush();
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
        PTY_DATA_READY.store(true, Ordering::Relaxed);
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
            PTY_DATA_READY.store(true, Ordering::Relaxed);
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
                PTY_DATA_READY.store(true, Ordering::Relaxed);
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
            PTY_DATA_READY.store(true, Ordering::Relaxed);
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
            PTY_DATA_READY.store(true, Ordering::Relaxed);
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
            PTY_DATA_READY.store(true, Ordering::Relaxed);
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
            PTY_DATA_READY.store(true, Ordering::Relaxed);
        }
        text
    }

    fn apply_copy_mutation(&self, f: impl FnOnce(&mut crate::types::Pane)) {
        if let Ok(mut state) = self.state.lock() {
            let changed = with_active_pane_mut(&mut state, f).is_some();
            if changed {
                PTY_DATA_READY.store(true, Ordering::Relaxed);
            }
        }
    }

    pub fn run_socket_server(&self, socket_name: &str) -> io::Result<()> {
        use crate::ipc::bind_server;

        let listener = bind_server(socket_name)?;
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
            let socket_name_clone = socket_name.clone();
            thread::spawn(move || {
                let _ = handle_client(stream, state, latest_frame, size_arc);
                let is_empty = state_check
                    .lock()
                    .map(|mut s| {
                        reap_dead_panes(&mut s);
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

#[cfg(unix)]
fn log_server(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/zmux_server.log")
    {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let _ = writeln!(f, "[{}] {}", ts, msg);
    }
}

#[cfg(unix)]
fn handle_client(
    stream: std::os::unix::net::UnixStream,
    state: Arc<Mutex<Server>>,
    _latest_frame: Arc<Mutex<Option<FrameData>>>,
    size_arc: Arc<Mutex<Size>>,
) -> io::Result<()> {
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
    {
        if let Ok(mut sz) = size_arc.lock() {
            *sz = new_size;
        }
        if let Ok(mut s) = state.lock() {
            resize_all_panes(&mut s, new_size);
        }
    }

    log_server("entering main loop");
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
                let mut s = match state.lock() {
                    Ok(s) => s,
                    Err(_) => break,
                };
                let session = match s.active_session_mut() {
                    Some(s) => s,
                    None => continue,
                };
                let win =
                    match session.windows.get_mut(session.active_window_idx) {
                        Some(w) => w,
                        None => continue,
                    };
                if let Some(pane) = crate::layout::active_pane_mut(
                    &mut win.root,
                    &win.active_pane_path,
                ) {
                    let _ = pane.writer.write_all(&bytes);
                    let _ = pane.writer.flush();
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
            }
        } else if line == "FRAME?" {
            let json = {
                let mut s = match state.lock() {
                    Ok(s) => s,
                    Err(_) => break,
                };
                reap_dead_panes(&mut s);
                let sz =
                    size_arc.lock().map(|s| *s).unwrap_or(Size::new(24, 80));
                if server_is_empty(&s) {
                    log_server("all sessions empty, sending exit frame");
                    "{\"type\":\"frame\",\"exit\":true,\"layout\":{\"type\":\"leaf\",\"id\":0,\"rows\":1,\"cols\":1,\"cursor_row\":0,\"cursor_col\":0,\"hide_cursor\":true,\"alternate_screen\":false,\"cursor_shape\":255,\"active\":false,\"rows_v2\":[]}}".to_string()
                } else {
                    let session = match s.active_session() {
                        Some(s) => s,
                        None => continue,
                    };
                    let win =
                        match session.windows.get(session.active_window_idx) {
                            Some(w) => w,
                            None => continue,
                        };
                    let area = frame_layout_area(sz);
                    build_frame_json(session, win, area)
                }
            };
            if send_frame(&mut write_stream, &json).is_err() {
                break;
            }
            if json.contains("\"exit\":true") {
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
            with_active_pane_mut(&mut s, f);
            PTY_DATA_READY.store(true, Ordering::Relaxed);
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
                    crate::copy_mode::start_selection(pane, m)
                });
                PTY_DATA_READY.store(true, Ordering::Relaxed);
            }
        }
    }
    if key == "clear_sel" {
        if let Ok(mut s) = state.lock() {
            with_active_pane_mut(&mut s, crate::copy_mode::clear_selection);
            PTY_DATA_READY.store(true, Ordering::Relaxed);
        }
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
            crate::copy_mode::search(pane, query.clone(), forward)
        });
        PTY_DATA_READY.store(true, Ordering::Relaxed);
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
        });
        PTY_DATA_READY.store(true, Ordering::Relaxed);
    }
}

fn build_frame_json(
    session: &crate::types::session::Session,
    win: &crate::types::session::Window,
    area: Rect,
) -> String {
    use crate::layout::serialize_frame;
    let layout_json = serialize_frame(win, area);
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
    format!(
        "{{\"type\":\"frame\",\"layout\":{},\"status\":{}}}",
        layout_part, status
    )
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

fn pane_viewport_size(area: Rect) -> (u16, u16) {
    if area.width > 2 && area.height > 2 {
        (area.height - 2, area.width - 2)
    } else {
        (area.height.max(1), area.width.max(1))
    }
}

fn root_pane_size(size: Size) -> (u16, u16) {
    pane_viewport_size(frame_layout_area(size))
}

fn render_loop(
    state: Arc<Mutex<Server>>,
    latest_frame: Arc<Mutex<Option<FrameData>>>,
    size: Arc<Mutex<Size>>,
    socket_name: Option<String>,
) {
    let mut first = true;
    loop {
        thread::sleep(Duration::from_millis(16));

        let dirty = PTY_DATA_READY.swap(false, Ordering::Relaxed);
        if !dirty && !first {
            continue;
        }
        first = false;

        let should_exit = {
            let mut s = match state.lock() {
                Ok(s) => s,
                Err(_) => continue,
            };
            reap_dead_panes(&mut s);
            server_is_empty(&s)
        };
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
            let s = match state.lock() {
                Ok(s) => s,
                Err(_) => continue,
            };
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
            let layout_json = serialize_frame(win, area);
            // 去掉外层 {type:frame, layout:...} 包装，只取 layout 部分
            let layout_part = layout_json
                .strip_prefix("{\"type\":\"frame\",\"layout\":")
                .and_then(|s| s.strip_suffix('}'))
                .unwrap_or("{}");

            // 构建 status JSON
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

            format!(
                "{{\"type\":\"frame\",\"layout\":{},\"status\":{}}}",
                layout_part, status
            )
        };

        if let Ok(fd) = serde_json::from_str::<FrameData>(&frame_json) {
            if let Ok(mut frame) = latest_frame.lock() {
                *frame = Some(fd);
            }
        }
    }
}

fn reap_dead_panes(state: &mut Server) -> bool {
    let mut changed = false;
    for session in &mut state.sessions {
        let mut win_idx = 0;
        while win_idx < session.windows.len() {
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
    changed
}

fn collect_dead_pane_ids(node: &LayoutNode) -> Vec<PaneId> {
    use std::sync::atomic::Ordering;
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
) -> io::Result<()> {
    let session_id = state.alloc_session_id();
    let mut session = Session::new(session_id, name.to_string());
    session.options = SessionOptions::with_defaults();

    let pane_id = session.alloc_pane_id();
    let window_id = session.alloc_window_id();

    let start_dir = crate::pty::default_start_dir();
    let (rows, cols) = root_pane_size(size);
    let pane = spawn_pane(SpawnOptions {
        pane_id,
        rows,
        cols,
        command: None,
        start_dir: start_dir.as_deref(),
        env: vec![],
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
        default_start_dir: None,
    };
    session.windows.push(window);
    state.sessions.push(session);
    Ok(())
}

fn resize_all_panes(state: &mut Server, size: Size) {
    for session in &mut state.sessions {
        for win in &mut session.windows {
            let area = frame_layout_area(size);
            if let Some(zoom) = &win.zoom_state {
                let zoomed_id = zoom.zoomed_pane_id;
                if let Some(pane) =
                    crate::layout::find_pane_by_id_mut(&mut win.root, zoomed_id)
                {
                    let (rows, cols) = pane_viewport_size(area);
                    let _ = resize_pane(pane, rows, cols);
                    crate::copy_mode::refresh_layout(pane);
                }
            } else {
                let rects = compute_rects(&win.root, area);
                resize_node_panes(&mut win.root, &rects, None);
            }
        }
    }
}

fn resize_node_panes(
    node: &mut LayoutNode,
    rects: &HashMap<PaneId, Rect>,
    zoom_pane_id: Option<PaneId>,
) {
    match node {
        LayoutNode::Leaf(p) => {
            if let Some(zoomed) = zoom_pane_id {
                if p.id != zoomed {
                    return;
                }
            }
            if let Some(&rect) = rects.get(&p.id) {
                let (rows, cols) = pane_viewport_size(rect);
                let _ = resize_pane(p, rows, cols);
                crate::copy_mode::refresh_layout(p);
            }
        }
        LayoutNode::Split { children, .. } => {
            for child in children.iter_mut() {
                resize_node_panes(child, rects, zoom_pane_id);
            }
        }
    }
}

fn execute_command_string(state: &mut Server, raw: &str, sz: Size) {
    let cmds = ParsedCommand::parse(raw);
    for cmd in cmds {
        dispatch_command(state, &cmd, sz);
    }
    PTY_DATA_READY.store(true, Ordering::Relaxed);
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
    PTY_DATA_READY.store(true, Ordering::Relaxed);
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
            cmd_kill_window(state);
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
            cmd_select_window(state, cmd);
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
            cmd_kill_session(state, cmd);
            String::new()
        }
        "switch-client" | "switchc" => {
            cmd_switch_client(state, cmd);
            String::new()
        }
        "next-session" => {
            cmd_next_session(state);
            String::new()
        }
        "prev-session" => {
            cmd_prev_session(state);
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
        "copy-mode" => {
            with_active_pane_mut(state, |pane| {
                crate::copy_mode::enter(pane);
            });
            PTY_DATA_READY.store(true, Ordering::Relaxed);
            String::new()
        }
        _ => String::new(),
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

fn make_session(state: &mut Server, name: &str, sz: Size) -> io::Result<()> {
    let session_id = state.alloc_session_id();
    let mut session = Session::new(session_id, name.to_string());
    session.options = SessionOptions::with_defaults();
    let pane_id = session.alloc_pane_id();
    let window_id = session.alloc_window_id();
    let start_dir = crate::pty::default_start_dir();
    let (rows, cols) = root_pane_size(sz);
    let pane = spawn_pane(SpawnOptions {
        pane_id,
        rows,
        cols,
        command: None,
        start_dir: start_dir.as_deref(),
        env: vec![],
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
        default_start_dir: None,
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
    let _ = make_session(state, &name, sz);
    let new_idx = state.sessions.len() - 1;
    if !cmd.flag("d") {
        state.active_session_idx = new_idx;
    }
}

fn cmd_kill_session(state: &mut Server, cmd: &ParsedCommand) {
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
}

fn cmd_switch_client(state: &mut Server, cmd: &ParsedCommand) {
    if let Some(name) = cmd.flag_value("t") {
        if let Some(idx) = state.find_session_idx(name) {
            state.active_session_idx = idx;
        }
    }
}

fn cmd_next_session(state: &mut Server) {
    let n = state.sessions.len();
    if n > 1 {
        state.active_session_idx = (state.active_session_idx + 1) % n;
    }
}

fn cmd_prev_session(state: &mut Server) {
    let n = state.sessions.len();
    if n > 1 {
        state.active_session_idx = (state.active_session_idx + n - 1) % n;
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

    {
        let session = match active_session_mut(state) {
            Some(s) => s,
            None => return,
        };
        let pane_id = session.next_pane_id;
        session.next_pane_id += 1;

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
            rows: (rows / 2).max(1),
            cols: if direction == SplitDirection::Horizontal {
                (cols / 2).max(1)
            } else {
                cols
            },
            command: None,
            start_dir: start_dir.as_deref(),
            env: vec![],
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
            command: None,
            start_dir: start_dir.as_deref(),
            env: vec![],
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

fn cmd_kill_window(state: &mut Server) {
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
}

fn cmd_select_pane(state: &mut Server, cmd: &ParsedCommand, sz: Size) {
    use crate::layout::{pane_path_in_direction, NavDir};

    let session = match active_session_mut(state) {
        Some(s) => s,
        None => return,
    };
    let win = match session.windows.get_mut(session.active_window_idx) {
        Some(w) => w,
        None => return,
    };

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
        let path = win.active_pane_path.clone();
        let new_path = pane_path_in_direction(&win.root, &path, d, area);
        if new_path != path {
            win.active_pane_path = new_path;
        } else {
            // 方向上无相邻 pane，保持不变（不循环跳转）
        }
    }
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
    }
}

fn cmd_select_window(state: &mut Server, cmd: &ParsedCommand) {
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
        if let Ok(mut ring) = pane.output_ring.lock() {
            ring.clear();
        }
        if let Ok(mut buf) = pane.text_buffer.lock() {
            *buf = crate::types::PaneTextBuffer::new(5 * 1024 * 1024);
        }
        if let Ok(mut parser) = pane.parser.lock() {
            *parser = vt100::Parser::new(pane.last_rows, pane.last_cols, 2000);
        }
        let _ = pane.writer.write_all(b"\r");
        let _ = pane.writer.flush();
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

    #[test]
    fn root_pane_size_matches_visible_content_area() {
        assert_eq!(root_pane_size(Size::new(24, 80)), (21, 78));
        assert_eq!(pane_viewport_size(Rect::new(0, 0, 2, 2)), (2, 2));
    }
}
