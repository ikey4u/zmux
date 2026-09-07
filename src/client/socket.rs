use std::{
    io::{self, BufReader, Read, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::Duration,
};

use crate::{
    client::{FrameData, LayoutJson},
    ipc::{connect_client, recv_frame, recv_resp},
    server::{encode_hex, SessionTreeEntry},
    types::{session::Size, SelectionMode},
};

fn log_socket(msg: &str) {
    use std::io::Write;
    let path = std::env::temp_dir().join("zmux_client.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let _ = writeln!(f, "[{}] socket: {}", ts, msg);
    }
}

const INPUT_CHUNK_SIZE: usize = 4096;

pub(crate) trait ClientStream: Read + Write + Send {
    fn try_clone_box(&self) -> io::Result<Box<dyn ClientStream>>;
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()>;
}

#[cfg(unix)]
impl ClientStream for crate::ipc::UnixStream {
    fn try_clone_box(&self) -> io::Result<Box<dyn ClientStream>> {
        Ok(Box::new(crate::ipc::IpcStream::try_clone(self)?))
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        crate::ipc::IpcStream::set_read_timeout(self, timeout)
    }
}

#[cfg(windows)]
impl ClientStream for crate::ipc::PipeStream {
    fn try_clone_box(&self) -> io::Result<Box<dyn ClientStream>> {
        Ok(Box::new(crate::ipc::IpcStream::try_clone(self)?))
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        crate::ipc::IpcStream::set_read_timeout(self, timeout)
    }
}

impl ClientStream for std::net::TcpStream {
    fn try_clone_box(&self) -> io::Result<Box<dyn ClientStream>> {
        Ok(Box::new(self.try_clone()?))
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        std::net::TcpStream::set_read_timeout(self, timeout)
    }
}

pub(crate) trait SocketConnector: Send + Sync {
    fn connect(&self) -> io::Result<Box<dyn ClientStream>>;

    fn connect_compatible(&self) -> io::Result<Box<dyn ClientStream>> {
        let mut stream = self.connect()?;
        stream.set_read_timeout(Some(self.initial_read_timeout()))?;
        crate::ipc::client_handshake(stream.as_mut())?;
        stream.set_read_timeout(None)?;
        Ok(stream)
    }

    fn is_remote(&self) -> bool {
        false
    }

    fn initial_read_timeout(&self) -> Duration {
        Duration::from_secs(2)
    }
}

struct LocalConnector {
    socket_name: String,
}

impl SocketConnector for LocalConnector {
    fn connect(&self) -> io::Result<Box<dyn ClientStream>> {
        Ok(Box::new(connect_client(&self.socket_name)?))
    }
}

enum ControlWrite {
    Line(String),
    Input(Vec<u8>),
}

pub struct SocketClient {
    connector: Arc<dyn SocketConnector>,
    frame_slot: Arc<Mutex<FrameSlot>>,
    session_tree_cache: Arc<Mutex<Vec<SessionTreeEntry>>>,
    session_tree_refresh: Arc<AtomicBool>,
    control_tx: mpsc::Sender<ControlWrite>,
    shutdown: Arc<AtomicBool>,
}

struct FrameSlot {
    frame: Option<FrameData>,
    counter: u64,
}

impl FrameSlot {
    fn publish(&mut self, frame: FrameData) {
        self.frame = Some(frame);
        self.counter = self.counter.wrapping_add(1);
    }

    fn snapshot(&self) -> (Option<FrameData>, u64) {
        (self.frame.clone(), self.counter)
    }
}

pub(crate) fn exit_frame() -> FrameData {
    FrameData {
        frame_type: "frame".to_string(),
        layout: LayoutJson::Leaf {
            id: 0,
            rows: 1,
            cols: 1,
            cursor_row: 0,
            cursor_col: 0,
            hide_cursor: true,
            alternate_screen: false,
            mouse_mode: 0,
            in_copy_mode: false,
            scroll_ratio: None,
            cursor_shape: 255,
            active: false,
            rows_v2: Vec::new(),
            title: None,
        },
        status: None,
        ansi: None,
        exit: true,
        yank_text: None,
    }
}

fn store_exit_frame(frame_slot: &Arc<Mutex<FrameSlot>>, reason: &str) {
    log_socket(reason);
    if let Ok(mut slot) = frame_slot.lock() {
        slot.publish(exit_frame());
    }
}

impl SocketClient {
    pub fn connect(socket_name: &str, size: Size) -> io::Result<Self> {
        Self::connect_with(
            socket_name,
            size,
            Arc::new(LocalConnector {
                socket_name: socket_name.to_string(),
            }),
        )
    }

    pub(crate) fn connect_with(
        socket_name: &str,
        size: Size,
        connector: Arc<dyn SocketConnector>,
    ) -> io::Result<Self> {
        log_socket(&format!(
            "connect socket='{}' size={}x{}",
            socket_name, size.rows, size.cols
        ));

        let stream = connector.connect_compatible()?;
        let initial_timeout = connector.initial_read_timeout();
        stream.set_read_timeout(Some(initial_timeout))?;

        let reader_stream = stream.try_clone_box()?;
        let write_clone = stream.try_clone_box()?;
        let mut writer = write_clone;

        writer.write_all(
            format!(
                "ATTACH\n{}x{}+{}+{}\nFRAME?\n",
                size.rows, size.cols, size.x, size.y
            )
            .as_bytes(),
        )?;
        writer.flush()?;
        log_socket("sent ATTACH + FRAME?");

        let mut probe_reader = BufReader::new(reader_stream);
        let (first_frame, first_frame_json) =
            match recv_frame(&mut probe_reader) {
                Ok(json) => {
                    log_socket(&format!(
                        "got first frame ({} bytes)",
                        json.len()
                    ));
                    let frame = serde_json::from_str::<FrameData>(&json)
                        .map_err(|e| {
                            io::Error::new(
                                io::ErrorKind::InvalidData,
                                format!("failed to decode first frame: {}", e),
                            )
                        })?;
                    if frame.exit {
                        log_socket("first frame was exit frame");
                        return Err(io::Error::new(
                            io::ErrorKind::ConnectionAborted,
                            "server has no attachable sessions",
                        ));
                    }
                    (frame, json)
                }
                Err(e) => {
                    log_socket(&format!("first frame timeout/error: {}", e));
                    return Err(io::Error::new(
                        e.kind(),
                        format!(
                            "server did not respond to first FRAME?: {}",
                            e
                        ),
                    ));
                }
            };

        probe_reader
            .get_ref()
            .set_read_timeout(Some(initial_timeout))?;

        let mut control_stream = connector.connect_compatible()?;
        control_stream.write_all(
            format!(
                "ATTACH\n{}x{}+{}+{}\n",
                size.rows, size.cols, size.x, size.y
            )
            .as_bytes(),
        )?;
        control_stream.flush()?;
        log_socket("opened control connection");
        log_socket("connection established, starting poll thread");

        let frame_slot = Arc::new(Mutex::new(FrameSlot {
            frame: Some(first_frame),
            counter: 1,
        }));
        let write_arc: Arc<Mutex<Box<dyn Write + Send>>> =
            Arc::new(Mutex::new(Box::new(control_stream)));
        let frame_write_arc: Arc<Mutex<Box<dyn Write + Send>>> =
            Arc::new(Mutex::new(Box::new(writer)));
        let shutdown: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
        let (control_tx, control_rx) = mpsc::channel();
        let control_write = Arc::clone(&write_arc);
        let control_shutdown = Arc::clone(&shutdown);
        let control_frame_slot = Arc::clone(&frame_slot);
        thread::spawn(move || {
            for msg in control_rx {
                let sent = match msg {
                    ControlWrite::Line(line) => {
                        send_line_on(&control_write, &line)
                    }
                    ControlWrite::Input(bytes) => {
                        pump_input_chunks(&control_write, &bytes)
                    }
                };
                if !sent {
                    control_shutdown.store(true, Ordering::Relaxed);
                    store_exit_frame(
                        &control_frame_slot,
                        "control connection write failed",
                    );
                    break;
                }
            }
        });
        let session_tree_cache = Arc::new(Mutex::new(Vec::new()));
        let session_tree_refresh = Arc::new(AtomicBool::new(false));

        let frame_slot_poll = Arc::clone(&frame_slot);
        let ws_poll = Arc::clone(&frame_write_arc);
        let shutdown_poll = Arc::clone(&shutdown);

        thread::spawn(move || {
            let mut reader = probe_reader;
            let mut last_frame_json = first_frame_json;
            loop {
                if shutdown_poll.load(Ordering::Relaxed) {
                    break;
                }
                {
                    let mut ws = match ws_poll.lock() {
                        Ok(ws) => ws,
                        Err(_) => {
                            store_exit_frame(
                                &frame_slot_poll,
                                "poll thread lost write stream lock",
                            );
                            break;
                        }
                    };
                    if ws.write_all(b"FRAME?\n").is_err() || ws.flush().is_err()
                    {
                        store_exit_frame(
                            &frame_slot_poll,
                            "poll thread failed to request frame",
                        );
                        break;
                    }
                }
                match recv_frame(&mut reader) {
                    Ok(json) => {
                        if shutdown_poll.load(Ordering::Relaxed) {
                            break;
                        }
                        if json == last_frame_json {
                            thread::sleep(Duration::from_millis(16));
                            if shutdown_poll.load(Ordering::Relaxed) {
                                break;
                            }
                            continue;
                        }
                        last_frame_json = json.clone();
                        if let Ok(mut fd) =
                            serde_json::from_str::<FrameData>(&json)
                        {
                            if fd.exit {
                                log_socket("poll thread received exit frame");
                            }
                            if let Ok(mut slot) = frame_slot_poll.lock() {
                                if fd.yank_text.is_none() {
                                    if let Some(prev) = slot.frame.as_ref() {
                                        if prev.yank_text.is_some() {
                                            fd.yank_text =
                                                prev.yank_text.clone();
                                        }
                                    }
                                }
                                slot.publish(fd);
                            }
                        }
                    }
                    Err(e) => {
                        if shutdown_poll.load(Ordering::Relaxed) {
                            break;
                        }
                        if matches!(
                            e.kind(),
                            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                        ) {
                            continue;
                        }
                        store_exit_frame(
                            &frame_slot_poll,
                            &format!(
                                "poll thread recv_frame failed, treating as exit: {}",
                                e
                            ),
                        );
                        break;
                    }
                }
                thread::sleep(Duration::from_millis(16));
            }
        });

        {
            let connector_tree = Arc::clone(&connector);
            let cache_tree = Arc::clone(&session_tree_cache);
            let refresh_tree = Arc::clone(&session_tree_refresh);
            let shutdown_tree = Arc::clone(&shutdown);
            thread::spawn(move || {
                poll_session_tree(
                    connector_tree,
                    cache_tree,
                    refresh_tree,
                    shutdown_tree,
                )
            });
        }

        Ok(Self {
            connector,
            frame_slot,
            session_tree_cache,
            session_tree_refresh,
            control_tx,
            shutdown,
        })
    }

    pub(crate) fn send_line(&self, line: &str) -> bool {
        self.control_tx
            .send(ControlWrite::Line(line.to_string()))
            .is_ok()
    }

    pub fn latest_frame(&self) -> Option<FrameData> {
        self.frame_slot.lock().ok()?.frame.clone()
    }

    /// Return a frame and its generation under one lock. The renderer must use
    /// this instead of reading the frame and counter separately, otherwise a
    /// freshly published counter can be paired with the previous frame and the
    /// real update gets skipped until the next input event.
    pub fn frame_snapshot(&self) -> (Option<FrameData>, u64) {
        self.frame_slot
            .lock()
            .map(|slot| slot.snapshot())
            .unwrap_or((None, 0))
    }
    pub fn send_input(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let _ = self.control_tx.send(ControlWrite::Input(bytes.to_vec()));
    }

    pub fn run_command(&self, cmd: &str) {
        self.send_line(&format!("CMD {}", cmd));
    }

    pub fn kill_server_socket(socket_name: &str) -> io::Result<()> {
        let connector: Arc<dyn SocketConnector> = Arc::new(LocalConnector {
            socket_name: socket_name.to_string(),
        });
        match kill_server_with(&connector) {
            Ok(()) => {
                cleanup_killed_socket(socket_name);
                Ok(())
            }
            Err(error) if crate::ipc::is_compatibility_error(&error) => {
                Err(error)
            }
            Err(error) => {
                thread::sleep(Duration::from_millis(50));
                if server_reachable(socket_name) {
                    Err(error)
                } else {
                    cleanup_killed_socket(socket_name);
                    Ok(())
                }
            }
        }
    }

    pub fn kill_server(&self) -> io::Result<()> {
        kill_server_with(&self.connector)
    }

    pub fn run_command_with_output(&self, cmd: &str) -> String {
        let stream = match self.connector.connect_compatible() {
            Ok(s) => s,
            Err(_) => return String::new(),
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let mut ws = match stream.try_clone_box() {
            Ok(s) => s,
            Err(_) => return String::new(),
        };
        let reader = BufReader::new(stream);
        if ws
            .write_all(format!("CMD_OUTPUT {}\n", cmd).as_bytes())
            .is_err()
            || ws.flush().is_err()
        {
            return String::new();
        }
        let mut buf_reader = reader;
        crate::ipc::recv_resp(&mut buf_reader).unwrap_or_default()
    }

    pub fn resize(&self, size: Size) {
        self.send_line(&format!(
            "RESIZE {}x{}+{}+{}",
            size.rows, size.cols, size.x, size.y
        ));
    }

    /// Request a complete pane repaint after a client-side overlay has covered
    /// server-rendered content. Incremental frames cannot restore that region.
    pub fn refresh_display(&self) {
        self.send_line("REFRESH_FRAME");
    }

    pub fn set_hide_borders(&self, hide: bool) {
        self.send_line(&format!(
            "HIDE_BORDERS {}",
            if hide { "1" } else { "0" }
        ));
    }

    pub fn scroll_on_erase_in_display(&self) -> bool {
        let stream = match self.connector.connect_compatible() {
            Ok(s) => s,
            Err(_) => return false,
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let mut ws = match stream.try_clone_box() {
            Ok(s) => s,
            Err(_) => return false,
        };
        let reader = BufReader::new(stream);
        if ws.write_all(b"OPTIONS\n").is_err() || ws.flush().is_err() {
            return false;
        }
        let mut buf_reader = reader;
        let json = match crate::ipc::recv_resp(&mut buf_reader) {
            Ok(s) => s,
            Err(_) => return false,
        };
        serde_json::from_str::<serde_json::Value>(&json)
            .ok()
            .and_then(|value| {
                value
                    .get("scroll_on_erase_in_display")
                    .and_then(|v| v.as_bool())
            })
            .unwrap_or(false)
    }

    pub fn set_scroll_on_erase_in_display(&self, enabled: bool) {
        self.send_line(&format!(
            "OPTION scroll_on_erase_in_display {}",
            if enabled { "1" } else { "0" }
        ));
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    pub fn detach(&self) {
        self.send_line("CMD detach");
        self.shutdown();
    }

    pub fn active_window_name(&self) -> String {
        self.frame_slot
            .lock()
            .ok()
            .and_then(|slot| {
                slot.frame
                    .as_ref()
                    .and_then(|fd| fd.status.as_ref())
                    .and_then(|st| {
                        st.windows
                            .iter()
                            .find(|w| w.active)
                            .map(|w| w.name.clone())
                    })
            })
            .unwrap_or_default()
    }

    pub fn session_name(&self) -> String {
        self.frame_slot
            .lock()
            .ok()
            .and_then(|slot| {
                slot.frame.as_ref().and_then(|fd| fd.status.as_ref()).map(
                    |st| {
                        st.left
                            .trim_start_matches('[')
                            .trim_end_matches(']')
                            .to_string()
                    },
                )
            })
            .unwrap_or_default()
    }

    pub fn session_tree(&self) -> Vec<SessionTreeEntry> {
        self.session_tree_cache
            .lock()
            .map(|tree| tree.clone())
            .unwrap_or_default()
    }

    pub fn refresh_session_tree(&self) {
        self.session_tree_refresh.store(true, Ordering::Release);
    }

    pub fn scroll_up(&self, lines: usize) {
        self.send_line(&format!("SCROLL up {}", lines));
    }

    pub fn scroll_down(&self, lines: usize) {
        self.send_line(&format!("SCROLL down {}", lines));
    }

    pub fn scroll_pane(&self, pane_id: usize, direction: &str, lines: usize) {
        self.send_line(&format!("SCROLL {} {} %{}", direction, lines, pane_id));
    }

    pub fn scroll_display(&self, delta: i32) {
        self.send_line(&format!("SCROLL_DISPLAY {}", delta));
    }

    pub fn scroll_display_bottom(&self) {
        self.send_line("SCROLL_DISPLAY bottom");
    }

    pub fn enter_copy_mode(&self) -> bool {
        self.send_line("CMD copy-mode");
        true
    }

    pub fn exit_copy_mode(&self) {
        self.send_line("COPY_KEY exit");
    }

    pub fn copy_move_left(&self) {
        self.send_line("COPY_KEY left");
    }

    pub fn copy_move_right(&self) {
        self.send_line("COPY_KEY right");
    }

    pub fn copy_move_up(&self) {
        self.send_line("COPY_KEY up");
    }

    pub fn copy_move_down(&self) {
        self.send_line("COPY_KEY down");
    }

    pub fn copy_page_up(&self) {
        self.send_line("COPY_KEY page_up");
    }

    pub fn copy_page_down(&self) {
        self.send_line("COPY_KEY page_down");
    }

    pub fn copy_move_to_top(&self) {
        self.send_line("COPY_KEY top");
    }

    pub fn copy_move_to_bottom(&self) {
        self.send_line("COPY_KEY bottom");
    }

    pub fn copy_move_to_line_start(&self) {
        self.send_line("COPY_KEY line_start");
    }

    pub fn copy_move_to_line_end(&self) {
        self.send_line("COPY_KEY line_end");
    }

    pub fn copy_move_word_backward(&self) {
        self.send_line("COPY_KEY word_back");
    }

    pub fn copy_move_word_forward(&self) {
        self.send_line("COPY_KEY word_fwd");
    }

    pub fn copy_move_word_end(&self) {
        self.send_line("COPY_KEY word_end");
    }

    pub fn copy_start_selection(&self, mode: SelectionMode) {
        let key = match mode {
            SelectionMode::Char => "sel_char",
            SelectionMode::Line => "sel_line",
            SelectionMode::Rect => "sel_rect",
        };
        self.send_line(&format!("COPY_KEY {}", key));
    }

    pub fn copy_clear_selection(&self) {
        self.send_line("COPY_KEY clear_sel");
    }

    pub fn copy_search(&self, query: String, forward: bool) -> bool {
        let dir = if forward { "fwd" } else { "bwd" };
        self.send_line(&format!("COPY_SEARCH {} {}", dir, query));
        true
    }

    pub fn copy_search_next(&self) -> bool {
        self.send_line("COPY_SEARCH_NEXT");
        true
    }

    pub fn copy_search_prev(&self) -> bool {
        self.send_line("COPY_SEARCH_PREV");
        true
    }

    pub fn copy_yank_selection(&self) -> String {
        let stream = match self.connector.connect_compatible() {
            Ok(s) => s,
            Err(_) => return String::new(),
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let mut ws = match stream.try_clone_box() {
            Ok(s) => s,
            Err(_) => return String::new(),
        };
        let reader = BufReader::new(stream);
        if ws.write_all(b"COPY_YANK\n").is_err() || ws.flush().is_err() {
            return String::new();
        }
        let mut buf_reader = reader;
        recv_resp(&mut buf_reader).unwrap_or_default()
    }
}

fn kill_server_with(connector: &Arc<dyn SocketConnector>) -> io::Result<()> {
    let stream = connector.connect_compatible()?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut ws = stream.try_clone_box()?;
    let reader = BufReader::new(stream);
    ws.write_all(b"KILL_SERVER\n")?;
    ws.flush()?;
    let mut buf_reader = reader;
    recv_resp(&mut buf_reader).map(|_| ())
}

fn poll_session_tree(
    connector: Arc<dyn SocketConnector>,
    cache: Arc<Mutex<Vec<SessionTreeEntry>>>,
    refresh: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
) {
    while !shutdown.load(Ordering::Relaxed) {
        let connected = match connector.connect_compatible() {
            Err(error) if crate::ipc::is_compatibility_error(&error) => {
                log_socket(&format!(
                    "session tree protocol rejected; stopping retries: {error}"
                ));
                return;
            }
            other => other,
        };
        let Some((mut writer, mut reader)) =
            connected.ok().and_then(|stream| {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let writer = stream.try_clone_box().ok()?;
                Some((writer, BufReader::new(stream)))
            })
        else {
            thread::sleep(Duration::from_millis(500));
            continue;
        };
        while !shutdown.load(Ordering::Relaxed) {
            if writer.write_all(b"SESSION_TREE\n").is_err()
                || writer.flush().is_err()
            {
                break;
            }
            let Ok(json) = crate::ipc::recv_resp(&mut reader) else {
                break;
            };
            if let Ok(mut current) = cache.lock() {
                *current = parse_session_tree_json(&json);
            }
            let ticks = if connector.is_remote() { 15 } else { 3 };
            for _ in 0..ticks {
                if shutdown.load(Ordering::Relaxed) {
                    return;
                }
                if refresh.swap(false, Ordering::AcqRel) {
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
        if !shutdown.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(250));
        }
    }
}

fn send_line_on(
    write_stream: &Arc<Mutex<Box<dyn Write + Send>>>,
    line: &str,
) -> bool {
    let mut ws = match write_stream.lock() {
        Ok(ws) => ws,
        Err(_) => return false,
    };
    ws.write_all(format!("{}\n", line).as_bytes()).is_ok() && ws.flush().is_ok()
}

fn pump_input_chunks(
    write_stream: &Arc<Mutex<Box<dyn Write + Send>>>,
    bytes: &[u8],
) -> bool {
    let mut chunks = bytes.chunks(INPUT_CHUNK_SIZE).peekable();
    while let Some(chunk) = chunks.next() {
        if !send_line_on(write_stream, &format!("INPUT {}", encode_hex(chunk)))
        {
            return false;
        }
        if chunks.peek().is_some() {
            thread::sleep(Duration::from_millis(1));
        }
    }
    true
}

fn server_reachable(socket_name: &str) -> bool {
    connect_client(socket_name).is_ok()
}

#[cfg(unix)]
fn cleanup_killed_socket(socket_name: &str) {
    if let Ok(path) = crate::ipc::socket_path(socket_name) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(windows)]
fn cleanup_killed_socket(_socket_name: &str) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_slot_publishes_frame_and_generation_together() {
        let mut slot = FrameSlot {
            frame: None,
            counter: 7,
        };
        slot.publish(exit_frame());

        let (frame, counter) = slot.snapshot();
        assert_eq!(counter, 8);
        assert!(frame.is_some_and(|frame| frame.exit));
    }

    #[test]
    fn session_tree_parser_keeps_pane_titles_and_accepts_legacy_payloads() {
        let titled = parse_session_tree_json(
            r#"[{"type":"pane","session_name":"main","window_index":0,"pane_id":7,"index":0,"title":"editor","is_active":true}]"#,
        );
        assert!(matches!(
            &titled[0],
            SessionTreeEntry::Pane { title, .. } if title == "editor"
        ));

        let legacy = parse_session_tree_json(
            r#"[{"type":"pane","session_name":"main","window_index":0,"pane_id":7,"index":0,"is_active":true}]"#,
        );
        assert!(matches!(
            &legacy[0],
            SessionTreeEntry::Pane { title, .. } if title.is_empty()
        ));
    }
}

impl Drop for SocketClient {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

pub(crate) fn parse_session_tree_json(json: &str) -> Vec<SessionTreeEntry> {
    let items: Vec<serde_json::Value> = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    items
        .iter()
        .filter_map(|v| {
            let typ = v.get("type")?.as_str()?;
            match typ {
                "session" => Some(SessionTreeEntry::Session {
                    name: v.get("name")?.as_str()?.to_string(),
                    window_count: v.get("window_count")?.as_u64()? as usize,
                    is_active: v.get("is_active")?.as_bool()?,
                }),
                "window" => Some(SessionTreeEntry::Window {
                    session_name: v.get("session_name")?.as_str()?.to_string(),
                    index: v.get("index")?.as_u64()? as usize,
                    name: v.get("name")?.as_str()?.to_string(),
                    pane_count: v.get("pane_count")?.as_u64()? as usize,
                    is_active: v.get("is_active")?.as_bool()?,
                }),
                "pane" => Some(SessionTreeEntry::Pane {
                    session_name: v.get("session_name")?.as_str()?.to_string(),
                    window_index: v.get("window_index")?.as_u64()? as usize,
                    pane_id: v.get("pane_id")?.as_u64()? as usize,
                    index: v.get("index")?.as_u64()? as usize,
                    title: v
                        .get("title")
                        .and_then(|value| value.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    is_active: v.get("is_active")?.as_bool()?,
                }),
                _ => None,
            }
        })
        .collect()
}
