use std::{
    io::{self, BufReader, Read, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::Duration,
};

use base64::{engine::general_purpose::STANDARD, Engine as _};

use crate::{
    domain::{
        daemon::ensure_local_daemon,
        hello::{
            incompatible_from, negotiate, upgrade_hint, Hello, HelloDomain,
            Incompatible, ProbeReport,
        },
        ids::PaneRef,
        lease,
        paste::validate_paste_text,
        payload::{
            AttachPayload, CmdPayload, DomainFramePayload, InputPayload,
            PastePayload, ResizePayload, RespPayload, WindowUpdatePayload,
        },
        probe::probe_socket,
    },
    ipc::{
        connect_client, recv_frame, recv_resp,
        v2::{
            read_envelope, write_envelope, Envelope, MsgType, PriorityWriter,
            V2Error,
        },
    },
    server::encode_hex,
};

pub fn run_stdio(socket_name: &str, start_if_missing: bool) -> io::Result<()> {
    run(
        io::stdin(),
        io::stdout(),
        socket_name,
        start_if_missing,
        "local",
    )
}

pub fn run<R, W>(
    reader: R,
    writer: W,
    socket_name: &str,
    start_if_missing: bool,
    host_alias: &str,
) -> io::Result<()>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    if start_if_missing {
        ensure_local_daemon(socket_name, "0", None)?;
    }
    let probe = probe_socket(socket_name);
    if !probe.server_running {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no zmux server on socket '{socket_name}'"),
        ));
    }

    let _lease = match lease::try_acquire(socket_name) {
        Ok(lease) => lease,
        Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
            let mut writer = writer;
            let hello = local_hello(&probe, socket_name, host_alias);
            write_envelope(
                &mut writer,
                &Envelope::json(MsgType::Hello, &hello),
            )?;
            let inc = Incompatible {
                reason: "lease_held".to_string(),
                message: err.to_string(),
                hint: "another interactive cloud client holds the size/input lease".to_string(),
                local: Some(hello),
                remote: None,
            };
            write_envelope(
                &mut writer,
                &Envelope::json(MsgType::Incompatible, &inc),
            )?;
            return Err(err);
        }
        Err(err) => return Err(err),
    };

    let hello = local_hello(&probe, socket_name, host_alias);
    let mut writer = PriorityWriter::new(writer);
    writer.enqueue(Envelope::json(MsgType::Hello, &hello))?;
    writer.flush_ready()?;

    let mut reader = reader;
    let peer_env = read_envelope(&mut reader).map_err(io::Error::from)?;
    if peer_env.typed() != Some(MsgType::Hello) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cloud handshake expected HELLO as the second record",
        ));
    }
    let peer: Hello =
        serde_json::from_slice(&peer_env.payload).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bad HELLO: {err}"),
            )
        })?;

    if probe.legacy_remote || probe.protocol.major != hello.protocol.major {
        let inc = Incompatible {
            reason: "legacy_remote".to_string(),
            message: probe.error.clone().unwrap_or_else(|| {
                "running daemon does not support cloud".into()
            }),
            hint: upgrade_hint(&hello_from_probe(
                &probe,
                socket_name,
                host_alias,
            )),
            local: Some(hello),
            remote: Some(peer),
        };
        writer.enqueue(Envelope::json(MsgType::Incompatible, &inc))?;
        writer.flush_ready()?;
        return Err(io::Error::new(io::ErrorKind::InvalidData, inc.message));
    }

    match negotiate(&hello, &peer) {
        Ok(_) => {}
        Err(err) => {
            let inc = incompatible_from(&err, &hello, &peer);
            writer.enqueue(Envelope::json(MsgType::Incompatible, &inc))?;
            writer.flush_ready()?;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                err.message(),
            ));
        }
    }

    let attach_env = read_envelope(&mut reader).map_err(io::Error::from)?;
    if attach_env.typed() != Some(MsgType::Attach) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cloud handshake expected ATTACH after HELLO",
        ));
    }
    let attach: AttachPayload = serde_json::from_slice(&attach_env.payload)
        .map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bad ATTACH: {err}"),
            )
        })?;

    let bridge = UnixBridge::connect(socket_name, attach.rows, attach.cols)?;
    let shutdown = Arc::new(AtomicBool::new(false));
    let (out_tx, out_rx) = mpsc::channel::<Envelope>();
    let writer = Arc::new(Mutex::new(writer));
    let writer_out = Arc::clone(&writer);
    let shutdown_out = Arc::clone(&shutdown);
    thread::spawn(move || {
        for env in out_rx {
            if shutdown_out.load(Ordering::Relaxed) {
                break;
            }
            let mut pending = Some(env);
            while let Some(env) = pending.take() {
                if shutdown_out.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(mut w) = writer_out.lock() else {
                    break;
                };
                if w.can_enqueue(&env) {
                    let _ = w.enqueue(env);
                    let _ = w.flush_ready();
                } else {
                    let _ = w.flush_ready();
                    drop(w);
                    pending = Some(env);
                    thread::sleep(Duration::from_millis(4));
                }
            }
        }
    });

    let frame_tx = out_tx.clone();
    let frame_bridge = bridge.clone();
    let shutdown_frame = Arc::clone(&shutdown);
    let instance = hello.server_instance_id.clone();
    thread::spawn(move || {
        frame_bridge.pump_frames(frame_tx, shutdown_frame, instance);
    });

    let result = process_client(reader, &bridge, &hello, &out_tx);
    shutdown.store(true, Ordering::Relaxed);
    bridge.shutdown();
    result
}

fn local_hello(
    probe: &ProbeReport,
    socket_name: &str,
    host_alias: &str,
) -> Hello {
    let instance = probe
        .server_instance_id
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    Hello::offer(
        instance,
        Some(HelloDomain {
            transport: "ssh".to_string(),
            host_alias: host_alias.to_string(),
            remote_socket: socket_name.to_string(),
        }),
        &crate::domain::hello::OPTIONAL_CAPS,
    )
}

fn hello_from_probe(
    probe: &ProbeReport,
    socket_name: &str,
    host_alias: &str,
) -> Hello {
    Hello {
        binary_version: probe
            .server_version
            .clone()
            .unwrap_or_else(|| probe.binary_version.clone()),
        server_instance_id: probe
            .server_instance_id
            .clone()
            .unwrap_or_default(),
        protocol: probe.protocol.clone(),
        capabilities: probe.capabilities.clone(),
        limits: probe.limits.clone(),
        domain: Some(HelloDomain {
            transport: "ssh".to_string(),
            host_alias: host_alias.to_string(),
            remote_socket: socket_name.to_string(),
        }),
    }
}

fn process_client<R: Read>(
    mut reader: R,
    bridge: &UnixBridge,
    hello: &Hello,
    out_tx: &mpsc::Sender<Envelope>,
) -> io::Result<()> {
    use std::collections::HashMap;

    use crate::domain::{
        drop::PartFile,
        payload::{BlobPayload, CancelPayload, ClipPayload, DropOkPayload},
    };

    let mut transfers: HashMap<String, PartFile> = HashMap::new();
    loop {
        let env = match read_envelope(&mut reader) {
            Ok(env) => env,
            Err(V2Error::Io(err))
                if err.kind() == io::ErrorKind::UnexpectedEof
                    || err.kind() == io::ErrorKind::BrokenPipe =>
            {
                break;
            }
            Err(V2Error::Truncated(_)) => break,
            Err(err) => return Err(err.into()),
        };
        match env.typed() {
            Some(MsgType::Input) => {
                let payload: InputPayload =
                    serde_json::from_slice(&env.payload).map_err(|e| {
                        io::Error::new(io::ErrorKind::InvalidData, e)
                    })?;
                check_instance(hello, payload.pane.as_ref())?;
                let bytes =
                    STANDARD.decode(payload.bytes_b64).map_err(|e| {
                        io::Error::new(io::ErrorKind::InvalidData, e)
                    })?;
                bridge.send_input(payload.pane.as_ref(), &bytes);
            }
            Some(MsgType::Paste) => {
                let payload: PastePayload =
                    serde_json::from_slice(&env.payload).map_err(|e| {
                        io::Error::new(io::ErrorKind::InvalidData, e)
                    })?;
                check_instance(hello, payload.pane.as_ref())?;
                validate_paste_text(&payload.text, payload.raw).map_err(
                    |e| io::Error::new(io::ErrorKind::InvalidInput, e),
                )?;
                bridge
                    .send_paste(payload.pane.as_ref(), payload.text.as_bytes());
            }
            Some(MsgType::Cmd) => {
                let payload: CmdPayload = serde_json::from_slice(&env.payload)
                    .map_err(|e| {
                        io::Error::new(io::ErrorKind::InvalidData, e)
                    })?;
                check_instance(hello, payload.pane.as_ref())?;
                let resp = bridge.run_cmd(&payload);
                let _ = out_tx.send(Envelope::json(MsgType::Resp, &resp));
            }
            Some(MsgType::Resize) => {
                let payload: ResizePayload =
                    serde_json::from_slice(&env.payload).map_err(|e| {
                        io::Error::new(io::ErrorKind::InvalidData, e)
                    })?;
                bridge.send_line(&format!(
                    "RESIZE {}x{}",
                    payload.rows, payload.cols
                ));
            }
            Some(MsgType::Refresh) => {
                bridge.send_line("REFRESH_FRAME");
            }
            Some(MsgType::WindowUpdate) => {
                let _ =
                    serde_json::from_slice::<WindowUpdatePayload>(&env.payload);
            }
            Some(MsgType::Clip) => {
                let payload: ClipPayload = serde_json::from_slice(&env.payload)
                    .map_err(|e| {
                        io::Error::new(io::ErrorKind::InvalidData, e)
                    })?;
                check_instance(hello, payload.pane.as_ref())?;
                if payload.size > crate::domain::drop::MAX_FILE_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "CLIP size exceeds 64MiB",
                    ));
                }
                let part = PartFile::create(&payload.name)
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
                transfers.insert(payload.id, part);
            }
            Some(MsgType::Blob) => {
                let payload: BlobPayload = serde_json::from_slice(&env.payload)
                    .map_err(|e| {
                        io::Error::new(io::ErrorKind::InvalidData, e)
                    })?;
                let data = STANDARD.decode(&payload.data_b64).map_err(|e| {
                    io::Error::new(io::ErrorKind::InvalidData, e)
                })?;
                let Some(part) = transfers.get_mut(&payload.id) else {
                    continue;
                };
                part.write_chunk(&data).map_err(|e| {
                    io::Error::new(io::ErrorKind::InvalidInput, e)
                })?;
                let _ = out_tx.send(Envelope::json(
                    MsgType::WindowUpdate,
                    &WindowUpdatePayload {
                        stream_id: 0,
                        credit: data.len() as u32,
                    },
                ));
                if payload.last {
                    let part = transfers.remove(&payload.id).unwrap();
                    let written = part.written;
                    match part.finish(None) {
                        Ok(path) => {
                            let drop = DropOkPayload {
                                id: payload.id,
                                path: path.to_string_lossy().into_owned(),
                                bytes: written,
                                sha256: String::new(),
                            };
                            let _ = out_tx
                                .send(Envelope::json(MsgType::DropOk, &drop));
                        }
                        Err(err) => {
                            return Err(io::Error::new(
                                io::ErrorKind::Other,
                                err,
                            ));
                        }
                    }
                }
            }
            Some(MsgType::Cancel) => {
                if let Ok(payload) =
                    serde_json::from_slice::<CancelPayload>(&env.payload)
                {
                    if let Some(part) = transfers.remove(&payload.id) {
                        part.cancel();
                    }
                }
            }
            Some(MsgType::Hello) | Some(MsgType::Attach) => {}
            Some(MsgType::Incompatible) => break,
            _ => {}
        }
    }
    Ok(())
}

fn check_instance(hello: &Hello, pane: Option<&PaneRef>) -> io::Result<()> {
    let Some(pane) = pane else {
        return Ok(());
    };
    if pane.domain_id.server_instance_id != hello.server_instance_id
        && hello.server_instance_id != "unknown"
        && !pane.domain_id.server_instance_id.is_empty()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "stale PaneRef instance {} != {}",
                pane.domain_id.server_instance_id, hello.server_instance_id
            ),
        ));
    }
    Ok(())
}

#[derive(Clone)]
struct UnixBridge {
    socket_name: String,
    control_tx: mpsc::Sender<String>,
    shutdown: Arc<AtomicBool>,
    frame_write: Arc<Mutex<Box<dyn Write + Send>>>,
    frame_reader: Arc<Mutex<BufReader<Box<dyn Read + Send>>>>,
}

impl UnixBridge {
    fn connect(socket_name: &str, rows: u16, cols: u16) -> io::Result<Self> {
        let stream = connect_client(socket_name)?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        let reader_stream = stream.try_clone()?;
        let mut writer = stream.try_clone()?;
        writer
            .write_all(format!("ATTACH\n{rows}x{cols}\nFRAME?\n").as_bytes())?;
        writer.flush()?;

        let mut probe_reader = BufReader::new(reader_stream);
        recv_frame(&mut probe_reader)?;
        probe_reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_secs(2)))?;

        let mut control = connect_client(socket_name)?;
        control.write_all(format!("ATTACH\n{rows}x{cols}\n").as_bytes())?;
        control.flush()?;
        let (control_tx, control_rx) = mpsc::channel::<String>();
        thread::spawn(move || {
            let mut control = control;
            for line in control_rx {
                if control.write_all(line.as_bytes()).is_err()
                    || control.write_all(b"\n").is_err()
                    || control.flush().is_err()
                {
                    break;
                }
            }
        });

        Ok(Self {
            socket_name: socket_name.to_string(),
            control_tx,
            shutdown: Arc::new(AtomicBool::new(false)),
            frame_write: Arc::new(Mutex::new(Box::new(writer))),
            frame_reader: Arc::new(Mutex::new(BufReader::new(Box::new(
                probe_reader.into_inner(),
            )
                as Box<dyn Read + Send>))),
        })
    }

    fn send_line(&self, line: &str) {
        let _ = self.control_tx.send(line.to_string());
    }

    fn send_input(&self, pane: Option<&PaneRef>, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let hex = encode_hex(bytes);
        let line = if let Some(pane) = pane {
            format!("INPUT %{} {hex}", pane.pane_id)
        } else {
            format!("INPUT {hex}")
        };
        self.send_line(&line);
    }

    fn send_paste(&self, pane: Option<&PaneRef>, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let hex = encode_hex(bytes);
        let line = if let Some(pane) = pane {
            format!("PASTE %{} {hex}", pane.pane_id)
        } else {
            format!("PASTE {hex}")
        };
        self.send_line(&line);
    }

    fn run_cmd(&self, payload: &CmdPayload) -> RespPayload {
        if payload.cmd == "detach" {
            self.send_line("CMD detach");
            return RespPayload {
                id: payload.id,
                ok: true,
                output: String::new(),
                error: None,
            };
        }
        if let Some(pane) = payload.pane.as_ref() {
            self.send_line(&format!("CMD select-pane -t %{}", pane.pane_id));
        }
        let oneshot = match payload.cmd.as_str() {
            "SESSION_TREE" => Some("SESSION_TREE"),
            "COPY_YANK" => Some("COPY_YANK"),
            "OPTIONS" => Some("OPTIONS"),
            _ if payload.want_output => None,
            _ => {
                if payload.cmd.starts_with("COPY_KEY ")
                    || payload.cmd.starts_with("COPY_SEARCH")
                    || payload.cmd.starts_with("SCROLL")
                    || payload.cmd.starts_with("HIDE_BORDERS")
                    || payload.cmd.starts_with("OPTION ")
                {
                    self.send_line(&payload.cmd);
                } else {
                    self.send_line(&format!("CMD {}", payload.cmd));
                }
                return RespPayload {
                    id: payload.id,
                    ok: true,
                    output: String::new(),
                    error: None,
                };
            }
        };
        let line = if let Some(kind) = oneshot {
            format!("{kind}\n")
        } else {
            format!("CMD_OUTPUT {}\n", payload.cmd)
        };
        match oneshot_resp(&self.socket_name, &line) {
            Ok(output) => RespPayload {
                id: payload.id,
                ok: true,
                output,
                error: None,
            },
            Err(err) => RespPayload {
                id: payload.id,
                ok: false,
                output: String::new(),
                error: Some(err.to_string()),
            },
        }
    }

    fn pump_frames(
        &self,
        tx: mpsc::Sender<Envelope>,
        shutdown: Arc<AtomicBool>,
        instance: String,
    ) {
        let mut last_json = String::new();
        let mut sequence = 0u64;
        loop {
            if shutdown.load(Ordering::Relaxed)
                || self.shutdown.load(Ordering::Relaxed)
            {
                break;
            }
            {
                let mut ws = match self.frame_write.lock() {
                    Ok(ws) => ws,
                    Err(_) => break,
                };
                if ws.write_all(b"FRAME?\n").is_err() || ws.flush().is_err() {
                    break;
                }
            }
            let json = {
                let mut reader = match self.frame_reader.lock() {
                    Ok(r) => r,
                    Err(_) => break,
                };
                match recv_frame(&mut *reader) {
                    Ok(json) => json,
                    Err(err)
                        if matches!(
                            err.kind(),
                            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                        ) =>
                    {
                        thread::sleep(Duration::from_millis(16));
                        continue;
                    }
                    Err(_) => break,
                }
            };
            if json == last_json {
                thread::sleep(Duration::from_millis(16));
                continue;
            }
            last_json = json.clone();
            let frame = match serde_json::from_str(&json) {
                Ok(frame) => frame,
                Err(_) => continue,
            };
            sequence = sequence.saturating_add(1);
            let payload = DomainFramePayload {
                server_instance_id: instance.clone(),
                sequence,
                base_sequence: sequence.saturating_sub(1),
                full: true,
                layout_revision: sequence,
                frame,
            };
            if tx
                .send(Envelope::json(MsgType::DomainFrame, &payload))
                .is_err()
            {
                break;
            }
            thread::sleep(Duration::from_millis(16));
        }
    }

    fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        self.send_line("CMD detach");
    }
}

fn oneshot_resp(socket_name: &str, line: &str) -> io::Result<String> {
    let stream = connect_client(socket_name)?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    writer.write_all(line.as_bytes())?;
    writer.flush()?;
    recv_resp(&mut reader)
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::net::UnixStream;

    use super::*;
    use crate::{
        domain::{
            cloud::CloudClient,
            hello::{negotiate, Hello, REQUIRED_CAPS},
            probe::probe_socket,
        },
        server::InProcessServer,
        types::session::Size,
    };

    fn unique_socket() -> String {
        format!(
            "cloud-itest-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    #[test]
    fn mux_stdio_hello_and_first_frame() -> io::Result<()> {
        let socket = unique_socket();
        let size = Size::new(24, 80);
        let server = InProcessServer::start(
            "0".into(),
            size,
            Some(socket.clone()),
            None,
        )?;
        let socket_server = socket.clone();
        thread::spawn(move || {
            let _ = server.run_socket_server(&socket_server);
        });
        for _ in 0..100 {
            if probe_socket(&socket).server_running {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let report = probe_socket(&socket);
        assert!(report.server_running);
        assert_eq!(report.protocol.major, 2);
        for cap in REQUIRED_CAPS {
            assert!(report.capabilities.iter().any(|c| c == cap), "{cap}");
        }

        let (client_end, mux_end) = UnixStream::pair()?;
        let mux_read = mux_end.try_clone()?;
        let socket_mux = socket.clone();
        thread::spawn(move || {
            let _ = run(mux_read, mux_end, &socket_mux, false, "testhost");
        });
        let client_read = client_end.try_clone()?;
        let client = CloudClient::connect(
            client_read,
            client_end,
            size,
            "testhost",
            &socket,
        )?;
        let frame = client.latest_frame().expect("first remote frame");
        assert!(!frame.exit);
        client.send_input(b"true\n");
        client.detach();
        Ok(())
    }

    #[test]
    fn hello_instance_change_is_a_toc_tou() {
        let local = Hello::offer("client", None, &[]);
        let mut remote = Hello::offer("old-daemon", None, &[]);
        assert!(negotiate(&local, &remote).is_ok());
        remote.server_instance_id = "new-daemon".into();
        assert_ne!(remote.server_instance_id, "old-daemon");
        assert!(negotiate(&local, &remote).is_ok());
    }
}
