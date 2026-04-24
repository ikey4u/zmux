use std::{
    io::{self, BufReader, Write},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use crate::{
    client::{FrameData, LayoutJson},
    ipc::{connect_client, recv_frame},
    server::{encode_hex, SessionTreeEntry},
    types::{session::Size, SelectionMode},
};

fn log_socket(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/zmux_client.log")
    {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let _ = writeln!(f, "[{}] socket: {}", ts, msg);
    }
}

pub struct SocketClient {
    socket_name: String,
    latest_frame: Arc<Mutex<Option<FrameData>>>,
    write_stream: Arc<Mutex<Box<dyn Write + Send>>>,
}

fn exit_frame() -> FrameData {
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
            cursor_shape: 255,
            active: false,
            rows_v2: Vec::new(),
            title: None,
        },
        status: None,
        exit: true,
        yank_text: None,
    }
}

fn store_exit_frame(
    latest_frame: &Arc<Mutex<Option<FrameData>>>,
    reason: &str,
) {
    log_socket(reason);
    if let Ok(mut frame) = latest_frame.lock() {
        *frame = Some(exit_frame());
    }
}

impl SocketClient {
    pub fn connect(socket_name: &str, size: Size) -> io::Result<Self> {
        log_socket(&format!(
            "connect socket='{}' size={}x{}",
            socket_name, size.rows, size.cols
        ));

        let stream = connect_client(socket_name)?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;

        let reader_stream = stream.try_clone()?;
        let write_clone = stream.try_clone()?;
        let mut writer = write_clone;

        writer.write_all(
            format!("ATTACH\n{}x{}\nFRAME?\n", size.rows, size.cols).as_bytes(),
        )?;
        writer.flush()?;
        log_socket("sent ATTACH + FRAME?");

        let mut probe_reader = BufReader::new(reader_stream);
        let first_frame = match recv_frame(&mut probe_reader) {
            Ok(json) => {
                log_socket(&format!("got first frame ({} bytes)", json.len()));
                let frame =
                    serde_json::from_str::<FrameData>(&json).map_err(|e| {
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
                frame
            }
            Err(e) => {
                log_socket(&format!("first frame timeout/error: {}", e));
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("server did not respond to first FRAME?: {}", e),
                ));
            }
        };

        probe_reader.get_ref().set_read_timeout(None)?;
        log_socket("connection established, starting poll thread");

        let latest_frame: Arc<Mutex<Option<FrameData>>> =
            Arc::new(Mutex::new(Some(first_frame)));
        let write_arc: Arc<Mutex<Box<dyn Write + Send>>> =
            Arc::new(Mutex::new(Box::new(writer)));

        let frame_arc = Arc::clone(&latest_frame);
        let ws_poll = Arc::clone(&write_arc);

        thread::spawn(move || {
            let mut reader = probe_reader;
            loop {
                {
                    let mut ws = match ws_poll.lock() {
                        Ok(ws) => ws,
                        Err(_) => {
                            store_exit_frame(
                                &frame_arc,
                                "poll thread lost write stream lock",
                            );
                            break;
                        }
                    };
                    if ws.write_all(b"FRAME?\n").is_err() || ws.flush().is_err()
                    {
                        store_exit_frame(
                            &frame_arc,
                            "poll thread failed to request frame",
                        );
                        break;
                    }
                }
                match recv_frame(&mut reader) {
                    Ok(json) => {
                        if let Ok(fd) = serde_json::from_str::<FrameData>(&json)
                        {
                            if fd.exit {
                                log_socket("poll thread received exit frame");
                            }
                            if let Ok(mut f) = frame_arc.lock() {
                                *f = Some(fd);
                            }
                        }
                    }
                    Err(e) => {
                        store_exit_frame(
                            &frame_arc,
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

        Ok(Self {
            socket_name: socket_name.to_string(),
            latest_frame,
            write_stream: write_arc,
        })
    }

    fn send_line(&self, line: &str) -> bool {
        let mut ws = match self.write_stream.lock() {
            Ok(ws) => ws,
            Err(_) => return false,
        };
        ws.write_all(format!("{}\n", line).as_bytes()).is_ok()
            && ws.flush().is_ok()
    }

    pub fn latest_frame(&self) -> Option<FrameData> {
        self.latest_frame.lock().ok()?.clone()
    }

    pub fn send_input(&self, bytes: &[u8]) {
        self.send_line(&format!("INPUT {}", encode_hex(bytes)));
    }

    pub fn run_command(&self, cmd: &str) {
        self.send_line(&format!("CMD {}", cmd));
    }

    pub fn run_command_with_output(&self, cmd: &str) -> String {
        self.run_command(cmd);
        String::new()
    }

    pub fn resize(&self, size: Size) {
        self.send_line(&format!("RESIZE {}x{}", size.rows, size.cols));
    }

    pub fn set_hide_borders(&self, hide: bool) {
        self.send_line(&format!(
            "HIDE_BORDERS {}",
            if hide { "1" } else { "0" }
        ));
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    pub fn detach(&self) {
        self.send_line("CMD detach");
    }

    pub fn active_window_name(&self) -> String {
        self.latest_frame
            .lock()
            .ok()
            .and_then(|f| {
                f.as_ref().and_then(|fd| fd.status.as_ref()).and_then(|st| {
                    st.windows.iter().find(|w| w.active).map(|w| w.name.clone())
                })
            })
            .unwrap_or_default()
    }

    pub fn session_name(&self) -> String {
        self.latest_frame
            .lock()
            .ok()
            .and_then(|f| {
                f.as_ref().and_then(|fd| fd.status.as_ref()).map(|st| {
                    st.left
                        .trim_start_matches('[')
                        .trim_end_matches(']')
                        .to_string()
                })
            })
            .unwrap_or_default()
    }

    pub fn session_tree(&self) -> Vec<SessionTreeEntry> {
        let stream = match connect_client(&self.socket_name) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let mut ws = match stream.try_clone() {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let reader = BufReader::new(stream);
        if ws.write_all(b"SESSION_TREE\n").is_err() || ws.flush().is_err() {
            return Vec::new();
        }
        let mut buf_reader = reader;
        let json = match crate::ipc::recv_resp(&mut buf_reader) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        parse_session_tree_json(&json)
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
        self.send_line("COPY_YANK");
        for _ in 0..50 {
            thread::sleep(Duration::from_millis(20));
            if let Some(frame) =
                self.latest_frame.lock().ok().and_then(|f| f.clone())
            {
                if let Some(text) = frame.yank_text {
                    if let Ok(mut f) = self.latest_frame.lock() {
                        if let Some(ref mut fd) = *f {
                            fd.yank_text = None;
                        }
                    }
                    return text;
                }
            }
        }
        String::new()
    }
}

fn parse_session_tree_json(json: &str) -> Vec<SessionTreeEntry> {
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
                    is_active: v.get("is_active")?.as_bool()?,
                }),
                _ => None,
            }
        })
        .collect()
}
