//! Compatibility tests use isolated sockets and never inspect or stop user servers.
#![cfg(unix)]

use std::{
    fs,
    io::{BufReader, Write},
    os::unix::{
        fs::MetadataExt,
        net::{UnixListener, UnixStream},
    },
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use zmux::ipc::{client_handshake, recv_line, recv_resp, ProtocolInfo};

const BIN: &str = env!("CARGO_BIN_EXE_zmux");

struct Fixture {
    root: PathBuf,
    socket: PathBuf,
    server: Option<Child>,
}
impl Fixture {
    fn new(start: bool) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = PathBuf::from(format!(
            "/tmp/zmux-proto-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::write(root.join("config"), "").unwrap();
        let runtime = root.join(format!("zmux-{}", unsafe { libc::getuid() }));
        fs::create_dir(&runtime).unwrap();
        let socket = runtime.join("wire");
        let mut fixture = Self {
            root,
            socket,
            server: None,
        };
        if start {
            fixture.server = Some(
                fixture
                    .command()
                    .arg("server")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap(),
            );
            let deadline = Instant::now() + Duration::from_secs(5);
            while !fixture.socket.exists() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            assert!(fixture.socket.exists(), "isolated server did not start");
        }
        fixture
    }
    fn command(&self) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.args(["-L", "wire", "-s", "keep"])
            .env("TMPDIR", &self.root)
            .env("ZMUX_CONFIG", self.root.join("config"))
            .env("SHELL", "/bin/sh")
            .env("ENV", "/dev/null")
            .current_dir(&self.root);
        cmd
    }
    fn stream(&self) -> UnixStream {
        let stream = UnixStream::connect(&self.socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
    }
    fn request(&self, request: &str) -> String {
        let mut stream = self.stream();
        client_handshake(&mut stream).unwrap();
        writeln!(stream, "{request}").unwrap();
        recv_resp(&mut BufReader::new(stream)).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(mut server) = self.server.take() {
            if let Ok(mut stream) = UnixStream::connect(&self.socket) {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
                if client_handshake(&mut stream).is_ok() {
                    let _ = stream.write_all(b"KILL_SERVER\n");
                    let _ = recv_resp(&mut BufReader::new(stream));
                }
            }
            let _ = server.kill();
            let _ = server.wait();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn metadata_has_no_server_side_effect_and_matches_live_contract() {
    let fixture = Fixture::new(false);
    let output = fixture.command().arg("protocol-info").output().unwrap();
    assert!(output.status.success());
    assert_eq!(
        zmux::ipc::parse_protocol_info(&output.stdout).unwrap(),
        ProtocolInfo::current()
    );
    assert!(!fixture.socket.exists());
}

#[test]
fn unnegotiated_or_incompatible_clients_cannot_mutate_the_server() {
    let fixture = Fixture::new(true);
    let before = fixture.request("LIST");
    assert!(before.contains("keep:"));
    for request in [
        "KILL_SERVER\n",
        "CMD_OUTPUT kill-session -t keep\n",
        "ATTACH\n1x1+0+0\n",
    ] {
        let mut stream = fixture.stream();
        stream.write_all(request.as_bytes()).unwrap();
        let response = recv_line(&mut BufReader::new(stream)).unwrap();
        assert!(
            response.starts_with("ZMUX REJECT ")
                && response.contains("handshake_required"),
            "{response}"
        );
        assert_eq!(fixture.request("LIST"), before);
    }
    let mut info = ProtocolInfo::current();
    info.major += 1;
    let mut stream = fixture.stream();
    writeln!(
        stream,
        "ZMUX HELLO {}",
        serde_json::to_string(&info).unwrap()
    )
    .unwrap();
    stream.write_all(b"KILL_SERVER\n").unwrap();
    let response = recv_line(&mut BufReader::new(stream)).unwrap();
    assert!(response.contains("protocol_version_mismatch"));
    assert_eq!(fixture.request("LIST"), before);
    assert!(fixture
        .command()
        .arg("ls")
        .output()
        .unwrap()
        .status
        .success());
}

#[test]
fn fragmented_hello_and_persistent_readonly_channel_work() {
    let fixture = Fixture::new(true);
    let mut stream = fixture.stream();
    let message = format!(
        "ZMUX HELLO {}\n",
        serde_json::to_string(&ProtocolInfo::current()).unwrap()
    );
    for chunk in message.as_bytes().chunks(7) {
        stream.write_all(chunk).unwrap();
    }
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    assert!(recv_line(&mut reader).unwrap().starts_with("ZMUX WELCOME "));
    for _ in 0..3 {
        stream.write_all(b"SESSION_TREE\n").unwrap();
        let tree = recv_resp(&mut reader).unwrap();
        assert!(tree.contains("keep") && tree.contains("pane"));
    }
    assert!(!fixture.request("OPTIONS").is_empty());
    assert!(fixture
        .request("CMD_OUTPUT set-workspace-home")
        .starts_with('/'));
}

#[test]
fn local_cli_never_replaces_or_kills_a_legacy_server_socket() {
    let fixture = Fixture::new(false);
    let listener = UnixListener::bind(&fixture.socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let inode = fs::metadata(&fixture.socket).unwrap().ino();
    let stop = Arc::new(AtomicBool::new(false));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let worker_stop = Arc::clone(&stop);
    let worker_requests = Arc::clone(&requests);
    let worker = thread::spawn(move || {
        while !worker_stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(1)))
                        .unwrap();
                    if let Ok(line) = recv_line(&mut BufReader::new(stream)) {
                        worker_requests.lock().unwrap().push(line);
                    }
                    // Legacy zmux closes on unknown hello, without a version reply.
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock =>
                {
                    thread::sleep(Duration::from_millis(5))
                }
                Err(error) => panic!("{error}"),
            }
        }
    });
    let outputs: Vec<_> = [
        vec![],
        vec!["a"],
        vec!["--clean"],
        vec!["ls"],
        vec!["kill-server"],
    ]
    .into_iter()
    .map(|args| fixture.command().args(args).output().unwrap())
    .collect();
    stop.store(true, Ordering::Relaxed);
    worker.join().unwrap();
    for output in outputs {
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("handshake_required"),
            "{:?}",
            output
        );
    }
    assert_eq!(fs::metadata(&fixture.socket).unwrap().ino(), inode);
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 5);
    assert!(requests.iter().all(|line| line.starts_with("ZMUX HELLO ")));
}

#[test]
fn stdio_bridge_checks_the_running_server_not_just_executable_metadata() {
    let fixture = Fixture::new(true);
    let mut bridge = fixture
        .command()
        .args(["mux", "--stdio", "--start-if-missing"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = bridge.stdin.take().unwrap();
    let mut reader = BufReader::new(bridge.stdout.take().unwrap());
    let mut incompatible = ProtocolInfo::current();
    incompatible.major += 1;
    writeln!(
        input,
        "ZMUX HELLO {}",
        serde_json::to_string(&incompatible).unwrap()
    )
    .unwrap();
    assert!(recv_line(&mut reader)
        .unwrap()
        .contains("protocol_version_mismatch"));
    drop(input);
    let _ = bridge.wait().unwrap();
    assert!(fixture.request("LIST").contains("keep:"));
}
