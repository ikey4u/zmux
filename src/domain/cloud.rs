use std::{
    collections::HashMap,
    io::{self, Read, Write},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::Duration,
};

use base64::{engine::general_purpose::STANDARD, Engine as _};

use crate::{
    client::FrameData,
    domain::{
        hello::{
            has_cap, negotiate, Hello, HelloDomain, Incompatible, Negotiated,
            CAP_BLOB_V1, OPTIONAL_CAPS,
        },
        ids::{pane_ref_from_frame, DomainId, PaneRef},
        payload::{
            AttachPayload, BlobPayload, CancelPayload, ClipPayload, CmdPayload,
            DomainFramePayload, DropOkPayload, InputPayload, PastePayload,
            ResizePayload, RespPayload, WindowUpdatePayload,
        },
    },
    ipc::v2::{
        read_envelope, write_envelope, Envelope, MsgType, PriorityWriter,
        V2Error,
    },
    server::SessionTreeEntry,
    types::{session::Size, SelectionMode},
};

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

enum Outgoing {
    Envelope(Envelope),
    CmdWait {
        env: Envelope,
        id: u64,
        tx: mpsc::Sender<RespPayload>,
    },
    Credit(u32),
    Shutdown,
}

pub struct CloudClient {
    label: String,
    negotiated: Negotiated,
    #[allow(dead_code)]
    domain: DomainId,
    frame_slot: Arc<Mutex<FrameSlot>>,
    focused: Arc<Mutex<Option<PaneRef>>>,
    out_tx: mpsc::Sender<Outgoing>,
    next_id: AtomicU64,
    shutdown: Arc<AtomicBool>,
    disconnected: Arc<AtomicBool>,
    drop_wait: Arc<Mutex<HashMap<String, mpsc::Sender<DropOkPayload>>>>,
    blob_notice: Arc<Mutex<Option<String>>>,
    generation: u64,
}

impl CloudClient {
    pub fn connect<R, W>(
        mut reader: R,
        mut writer: W,
        size: Size,
        label: &str,
        socket_name: &str,
    ) -> io::Result<Self>
    where
        R: Read + Send + 'static,
        W: Write + Send + 'static,
    {
        let mux_hello_env =
            read_envelope(&mut reader).map_err(io::Error::from)?;
        if mux_hello_env.typed() != Some(MsgType::Hello) {
            if mux_hello_env.typed() == Some(MsgType::Incompatible) {
                return incompatible_err(&mux_hello_env.payload);
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "cloud stream did not start with HELLO",
            ));
        }
        let mux_hello: Hello = serde_json::from_slice(&mux_hello_env.payload)
            .map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bad HELLO: {err}"),
            )
        })?;
        let local = Hello::offer(
            "client",
            Some(HelloDomain {
                transport: "ssh".to_string(),
                host_alias: label.to_string(),
                remote_socket: socket_name.to_string(),
            }),
            &OPTIONAL_CAPS,
        );
        write_envelope(&mut writer, &Envelope::json(MsgType::Hello, &local))?;
        let negotiated = match negotiate(&local, &mux_hello) {
            Ok(n) => n,
            Err(err) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    crate::domain::hello::incompatible_from(
                        &err, &local, &mux_hello,
                    )
                    .hint,
                ));
            }
        };
        write_envelope(
            &mut writer,
            &Envelope::json(
                MsgType::Attach,
                &AttachPayload {
                    rows: size.rows,
                    cols: size.cols,
                    pane: None,
                },
            ),
        )?;

        let domain =
            DomainId::ssh(label, socket_name, &mux_hello.server_instance_id);
        let frame_slot = Arc::new(Mutex::new(FrameSlot {
            frame: None,
            counter: 0,
        }));
        let focused = Arc::new(Mutex::new(None));
        let shutdown = Arc::new(AtomicBool::new(false));
        let disconnected = Arc::new(AtomicBool::new(false));
        let drop_wait: Arc<
            Mutex<HashMap<String, mpsc::Sender<DropOkPayload>>>,
        > = Arc::new(Mutex::new(HashMap::new()));
        let blob_notice = Arc::new(Mutex::new(None));
        let (out_tx, out_rx) = mpsc::channel::<Outgoing>();
        let pending: Arc<Mutex<HashMap<u64, mpsc::Sender<RespPayload>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let pending_w = Arc::clone(&pending);
        thread::spawn(move || {
            let mut writer = PriorityWriter::new(writer);
            let mut pending_env: Option<Envelope> = None;
            loop {
                if pending_env.is_none() {
                    match out_rx.recv_timeout(Duration::from_millis(8)) {
                        Ok(Outgoing::Envelope(env)) => {
                            pending_env = Some(env);
                        }
                        Ok(Outgoing::CmdWait { env, id, tx }) => {
                            if let Ok(mut map) = pending_w.lock() {
                                map.insert(id, tx);
                            }
                            pending_env = Some(env);
                        }
                        Ok(Outgoing::Credit(credit)) => {
                            writer.add_credit(credit);
                        }
                        Ok(Outgoing::Shutdown) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                if let Some(env) = pending_env.take() {
                    if writer.can_enqueue(&env) {
                        if writer.enqueue(env).is_err() {
                            break;
                        }
                    } else {
                        pending_env = Some(env);
                    }
                }
                if writer.flush_ready().is_err() {
                    break;
                }
            }
        });

        let frame_slot_r = Arc::clone(&frame_slot);
        let focused_r = Arc::clone(&focused);
        let pending_r = Arc::clone(&pending);
        let shutdown_r = Arc::clone(&shutdown);
        let disconnected_r = Arc::clone(&disconnected);
        let drop_wait_r = Arc::clone(&drop_wait);
        let out_tx_r = out_tx.clone();
        let domain_r = domain.clone();
        let label_r = label.to_string();
        thread::spawn(move || {
            let mut reader = reader;
            loop {
                if shutdown_r.load(Ordering::Relaxed) {
                    break;
                }
                match read_envelope(&mut reader) {
                    Ok(env) => match env.typed() {
                        Some(MsgType::DomainFrame) => {
                            if let Ok(payload) =
                                serde_json::from_slice::<DomainFramePayload>(
                                    &env.payload,
                                )
                            {
                                let mut frame = payload.frame;
                                tag_status(&mut frame, &label_r);
                                if let Some(pref) =
                                    pane_ref_from_frame(&domain_r, &frame)
                                {
                                    if let Ok(mut focused) = focused_r.lock() {
                                        *focused = Some(pref);
                                    }
                                }
                                if let Ok(mut slot) = frame_slot_r.lock() {
                                    slot.publish(frame);
                                }
                            }
                        }
                        Some(MsgType::Resp) => {
                            if let Ok(resp) =
                                serde_json::from_slice::<RespPayload>(
                                    &env.payload,
                                )
                            {
                                if let Ok(mut map) = pending_r.lock() {
                                    if let Some(tx) = map.remove(&resp.id) {
                                        let _ = tx.send(resp);
                                    }
                                }
                            }
                        }
                        Some(MsgType::DropOk) => {
                            if let Ok(drop) =
                                serde_json::from_slice::<DropOkPayload>(
                                    &env.payload,
                                )
                            {
                                if let Ok(mut map) = drop_wait_r.lock() {
                                    if let Some(tx) = map.remove(&drop.id) {
                                        let _ = tx.send(drop);
                                    }
                                }
                            }
                        }
                        Some(MsgType::WindowUpdate) => {
                            if let Ok(upd) =
                                serde_json::from_slice::<WindowUpdatePayload>(
                                    &env.payload,
                                )
                            {
                                let _ =
                                    out_tx_r.send(Outgoing::Credit(upd.credit));
                            }
                        }
                        Some(MsgType::Incompatible) => {
                            let _ = incompatible_err(&env.payload);
                            disconnected_r.store(true, Ordering::Relaxed);
                            store_exit(&frame_slot_r);
                            break;
                        }
                        _ => {}
                    },
                    Err(V2Error::Io(err))
                        if matches!(
                            err.kind(),
                            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                        ) =>
                    {
                        continue;
                    }
                    Err(_) => {
                        disconnected_r.store(true, Ordering::Relaxed);
                        store_exit(&frame_slot_r);
                        break;
                    }
                }
            }
        });

        let client = Self {
            label: label.to_string(),
            negotiated,
            domain,
            frame_slot,
            focused,
            out_tx,
            next_id: AtomicU64::new(1),
            shutdown,
            disconnected,
            drop_wait,
            blob_notice,
            generation: 1,
        };
        client.wait_for_first_frame()?;
        Ok(client)
    }

    fn wait_for_first_frame(&self) -> io::Result<()> {
        for _ in 0..200 {
            if let Some(frame) = self.latest_frame() {
                if frame.exit {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "remote server has no attachable sessions",
                    ));
                }
                return Ok(());
            }
            thread::sleep(Duration::from_millis(25));
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "remote mux did not send the first DOMAIN_FRAME",
        ))
    }

    fn send_env(&self, env: Envelope) {
        let _ = self.out_tx.send(Outgoing::Envelope(env));
    }

    fn focused_pane(&self) -> Option<PaneRef> {
        self.focused.lock().ok().and_then(|g| g.clone())
    }

    fn next_cmd_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn rpc(&self, cmd: String, want_output: bool) -> RespPayload {
        let id = self.next_cmd_id();
        let (tx, rx) = mpsc::channel();
        let payload = CmdPayload {
            id,
            cmd,
            want_output,
            pane: self.focused_pane(),
        };
        let _ = self.out_tx.send(Outgoing::CmdWait {
            env: Envelope::json(MsgType::Cmd, &payload),
            id,
            tx,
        });
        rx.recv_timeout(Duration::from_secs(2))
            .unwrap_or(RespPayload {
                id,
                ok: false,
                output: String::new(),
                error: Some("timeout".into()),
            })
    }

    pub fn domain_label(&self) -> &str {
        &self.label
    }

    pub fn has_blob(&self) -> bool {
        has_cap(&self.negotiated.capabilities, CAP_BLOB_V1)
    }

    pub fn latest_frame(&self) -> Option<FrameData> {
        self.frame_slot.lock().ok()?.frame.clone()
    }

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
        let payload = InputPayload {
            bytes_b64: STANDARD.encode(bytes),
            pane: self.focused_pane(),
        };
        self.send_env(Envelope::json(MsgType::Input, &payload));
    }

    pub fn send_paste(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        let payload = PastePayload {
            text: text.to_string(),
            pane: self.focused_pane(),
            raw: false,
        };
        self.send_env(Envelope::json(MsgType::Paste, &payload));
    }

    pub fn run_command(&self, cmd: &str) {
        let _ = self.rpc(cmd.to_string(), false);
    }

    pub fn run_command_with_output(&self, cmd: &str) -> String {
        self.rpc(cmd.to_string(), true).output
    }

    pub fn resize(&self, size: Size) {
        self.send_env(Envelope::json(
            MsgType::Resize,
            &ResizePayload {
                rows: size.rows,
                cols: size.cols,
            },
        ));
    }

    pub fn refresh_display(&self) {
        self.send_env(Envelope::new(MsgType::Refresh, Vec::new()));
    }

    pub fn set_hide_borders(&self, hide: bool) {
        self.send_env(Envelope::json(
            MsgType::Cmd,
            &CmdPayload {
                id: self.next_cmd_id(),
                cmd: format!("HIDE_BORDERS {}", if hide { "1" } else { "0" }),
                want_output: false,
                pane: None,
            },
        ));
    }

    pub fn scroll_on_erase_in_display(&self) -> bool {
        let json = self.rpc("OPTIONS".into(), true).output;
        serde_json::from_str::<serde_json::Value>(&json)
            .ok()
            .and_then(|v| {
                v.get("scroll_on_erase_in_display")
                    .and_then(|v| v.as_bool())
            })
            .unwrap_or(false)
    }

    pub fn set_scroll_on_erase_in_display(&self, enabled: bool) {
        self.run_command(&format!(
            "OPTION scroll_on_erase_in_display {}",
            if enabled { "1" } else { "0" }
        ));
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = self.out_tx.send(Outgoing::Shutdown);
    }

    pub fn detach(&self) {
        self.run_command("detach");
        self.shutdown();
    }

    pub fn active_window_name(&self) -> String {
        self.latest_frame()
            .as_ref()
            .and_then(|fd| fd.status.as_ref())
            .and_then(|st| {
                st.windows.iter().find(|w| w.active).map(|w| w.name.clone())
            })
            .unwrap_or_default()
    }

    pub fn session_name(&self) -> String {
        self.latest_frame()
            .as_ref()
            .and_then(|fd| fd.status.as_ref())
            .map(|st| {
                st.left
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .to_string()
            })
            .unwrap_or_else(|| self.label.clone())
    }

    pub fn session_tree(&self) -> Vec<SessionTreeEntry> {
        let json = self.rpc("SESSION_TREE".into(), true).output;
        crate::client::socket::parse_session_tree_json(&json)
    }

    pub fn scroll_up(&self, lines: usize) {
        self.send_line_cmd(&format!("SCROLL up {lines}"));
    }

    pub fn scroll_down(&self, lines: usize) {
        self.send_line_cmd(&format!("SCROLL down {lines}"));
    }

    pub fn scroll_pane(&self, pane_id: usize, direction: &str, lines: usize) {
        self.send_line_cmd(&format!("SCROLL {direction} {lines} %{pane_id}"));
    }

    pub fn scroll_display(&self, delta: i32) {
        self.send_line_cmd(&format!("SCROLL_DISPLAY {delta}"));
    }

    pub fn scroll_display_bottom(&self) {
        self.send_line_cmd("SCROLL_DISPLAY bottom");
    }

    pub fn enter_copy_mode(&self) -> bool {
        self.run_command("copy-mode");
        true
    }

    pub fn exit_copy_mode(&self) {
        self.send_line_cmd("COPY_KEY exit");
    }

    pub fn copy_move_left(&self) {
        self.send_line_cmd("COPY_KEY left");
    }
    pub fn copy_move_right(&self) {
        self.send_line_cmd("COPY_KEY right");
    }
    pub fn copy_move_up(&self) {
        self.send_line_cmd("COPY_KEY up");
    }
    pub fn copy_move_down(&self) {
        self.send_line_cmd("COPY_KEY down");
    }
    pub fn copy_page_up(&self) {
        self.send_line_cmd("COPY_KEY page_up");
    }
    pub fn copy_page_down(&self) {
        self.send_line_cmd("COPY_KEY page_down");
    }
    pub fn copy_move_to_top(&self) {
        self.send_line_cmd("COPY_KEY top");
    }
    pub fn copy_move_to_bottom(&self) {
        self.send_line_cmd("COPY_KEY bottom");
    }
    pub fn copy_move_to_line_start(&self) {
        self.send_line_cmd("COPY_KEY line_start");
    }
    pub fn copy_move_to_line_end(&self) {
        self.send_line_cmd("COPY_KEY line_end");
    }
    pub fn copy_move_word_backward(&self) {
        self.send_line_cmd("COPY_KEY word_back");
    }
    pub fn copy_move_word_forward(&self) {
        self.send_line_cmd("COPY_KEY word_fwd");
    }
    pub fn copy_move_word_end(&self) {
        self.send_line_cmd("COPY_KEY word_end");
    }
    pub fn copy_start_selection(&self, mode: SelectionMode) {
        let key = match mode {
            SelectionMode::Char => "sel_char",
            SelectionMode::Line => "sel_line",
            SelectionMode::Rect => "sel_rect",
        };
        self.send_line_cmd(&format!("COPY_KEY {key}"));
    }
    pub fn copy_clear_selection(&self) {
        self.send_line_cmd("COPY_KEY clear_sel");
    }
    pub fn copy_search(&self, query: String, forward: bool) -> bool {
        let dir = if forward { "fwd" } else { "bwd" };
        self.send_line_cmd(&format!("COPY_SEARCH {dir} {query}"));
        true
    }
    pub fn copy_search_next(&self) -> bool {
        self.send_line_cmd("COPY_SEARCH_NEXT");
        true
    }
    pub fn copy_search_prev(&self) -> bool {
        self.send_line_cmd("COPY_SEARCH_PREV");
        true
    }
    pub fn copy_yank_selection(&self) -> String {
        self.rpc("COPY_YANK".into(), true).output
    }

    pub fn paste_cloud(&self) -> Result<String, String> {
        let item = crate::domain::clip::read_os_clipboard()?;
        crate::domain::clip::validate_or_text(&item, false)?;
        match item {
            crate::domain::clip::ClipboardItem::Text(text) => {
                self.send_paste(&text);
                Ok(format!("pasted {} chars", text.chars().count()))
            }
            crate::domain::clip::ClipboardItem::ImagePng { bytes, name } => {
                if !self.has_blob() {
                    return Err(
                        "clipboard holds an image; remote is missing blob-v1"
                            .into(),
                    );
                }
                let path = self.upload_bytes(&name, "image/png", &bytes)?;
                self.paste_remote_paths(&[path], false)
            }
            crate::domain::clip::ClipboardItem::Files(files) => {
                if !self.has_blob() {
                    return Err(
                        "clipboard holds files; remote is missing blob-v1"
                            .into(),
                    );
                }
                let mut paths = Vec::new();
                let mut batch = 0u64;
                for file in &files {
                    let meta =
                        std::fs::metadata(file).map_err(|e| e.to_string())?;
                    batch += meta.len();
                    if batch > crate::domain::drop::MAX_BATCH_BYTES {
                        return Err("clipboard files exceed 256MiB".into());
                    }
                    paths.push(self.upload_file(file)?);
                }
                self.paste_remote_paths(&paths, false)
            }
        }
    }

    pub fn disconnected(&self) -> bool {
        self.disconnected.load(Ordering::Relaxed)
            || self.latest_frame().is_some_and(|f| f.exit)
    }

    pub fn blob_notice(&self) -> Option<String> {
        self.blob_notice.lock().ok().and_then(|g| g.clone())
    }

    fn paste_remote_paths(
        &self,
        paths: &[String],
        raw: bool,
    ) -> Result<String, String> {
        let text = crate::domain::clip::quote_paths(paths, raw)?;
        self.send_paste(&text);
        Ok(format!("pasted {} remote path(s)", paths.len()))
    }

    fn upload_bytes(
        &self,
        name: &str,
        mime: &str,
        data: &[u8],
    ) -> Result<String, String> {
        let id = crate::domain::ids::new_instance_id();
        self.begin_clip(&id, "image", mime, data.len() as u64, name)?;
        let mut offset = 0u64;
        for chunk in data.chunks(64 * 1024) {
            self.send_blob_chunk(
                &id,
                offset,
                chunk,
                offset + chunk.len() as u64 >= data.len() as u64,
            )?;
            offset += chunk.len() as u64;
            if let Ok(mut n) = self.blob_notice.lock() {
                *n = Some(format!("blob {}/{}", offset, data.len()));
            }
        }
        let drop = self.wait_drop(&id)?;
        if let Ok(mut n) = self.blob_notice.lock() {
            *n = None;
        }
        Ok(drop.path)
    }

    fn upload_file(&self, path: &std::path::Path) -> Result<String, String> {
        let id = crate::domain::ids::new_instance_id();
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("file");
        let size = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
        self.begin_clip(&id, "file", "application/octet-stream", size, name)?;
        let mut offset = 0u64;
        crate::domain::clip::stream_file_chunks(path, |chunk| {
            let last = offset + chunk.len() as u64 >= size;
            self.send_blob_chunk(&id, offset, chunk, last)?;
            offset += chunk.len() as u64;
            if let Ok(mut n) = self.blob_notice.lock() {
                *n = Some(format!("blob {}/{}", offset, size));
            }
            Ok(())
        })?;
        let drop = self.wait_drop(&id)?;
        if let Ok(mut n) = self.blob_notice.lock() {
            *n = None;
        }
        Ok(drop.path)
    }

    fn begin_clip(
        &self,
        id: &str,
        kind: &str,
        mime: &str,
        size: u64,
        name: &str,
    ) -> Result<(), String> {
        self.send_env(Envelope::json(
            MsgType::Clip,
            &ClipPayload {
                id: id.to_string(),
                kind: kind.to_string(),
                mime: mime.to_string(),
                size,
                name: name.to_string(),
                sha256: String::new(),
                pane: self.focused_pane(),
                generation: self.generation,
            },
        ));
        Ok(())
    }

    fn send_blob_chunk(
        &self,
        id: &str,
        offset: u64,
        data: &[u8],
        last: bool,
    ) -> Result<(), String> {
        self.send_env(Envelope::json(
            MsgType::Blob,
            &BlobPayload {
                id: id.to_string(),
                offset,
                last,
                data_b64: STANDARD.encode(data),
            },
        ));
        Ok(())
    }

    fn wait_drop(&self, id: &str) -> Result<DropOkPayload, String> {
        let (tx, rx) = mpsc::channel();
        if let Ok(mut map) = self.drop_wait.lock() {
            map.insert(id.to_string(), tx);
        }
        rx.recv_timeout(Duration::from_secs(60)).map_err(|_| {
            let _ = self.out_tx.send(Outgoing::Envelope(Envelope::json(
                MsgType::Cancel,
                &CancelPayload { id: id.to_string() },
            )));
            "blob upload timed out".to_string()
        })
    }

    fn send_line_cmd(&self, cmd: &str) {
        let payload = CmdPayload {
            id: self.next_cmd_id(),
            cmd: cmd.to_string(),
            want_output: false,
            pane: self.focused_pane(),
        };
        self.send_env(Envelope::json(MsgType::Cmd, &payload));
    }
}

impl Drop for CloudClient {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn tag_status(frame: &mut FrameData, label: &str) {
    if let Some(status) = frame.status.as_mut() {
        let tag = format!("[{label}]");
        if !status.left.starts_with(&tag) {
            status.left = format!("{tag} {}", status.left);
        }
    }
}

fn store_exit(slot: &Arc<Mutex<FrameSlot>>) {
    if let Ok(mut slot) = slot.lock() {
        slot.publish(crate::client::socket::exit_frame());
    }
}

fn incompatible_err(payload: &[u8]) -> io::Result<CloudClient> {
    let inc: Incompatible =
        serde_json::from_slice(payload).unwrap_or(Incompatible {
            reason: "incompatible".into(),
            message: "incompatible cloud protocol".into(),
            hint: String::new(),
            local: None,
            remote: None,
        });
    let msg = if inc.hint.is_empty() {
        inc.message
    } else {
        format!("{}\n{}", inc.message, inc.hint)
    };
    Err(io::Error::new(io::ErrorKind::InvalidData, msg))
}
