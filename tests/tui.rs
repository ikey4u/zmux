//! Drive the compiled client through a real PTY and decode its ANSI output.
//! No user configuration, shells, SSH hosts, or existing servers are modified.
#![cfg(unix)]

use portable_pty::{
    native_pty_system, Child, CommandBuilder, MasterPty, PtySize,
};
use std::{
    fs,
    io::{Read, Write},
    path::PathBuf,
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use zmux::terminal::AlacrittyTermState;

const BIN: &str = env!("CARGO_BIN_EXE_zmux");
const END_SYNC: &[u8] = b"\x1b[?2026l";

struct Tui {
    root: PathBuf,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    rx: mpsc::Receiver<Vec<u8>>,
    screen: AlacrittyTermState,
    pending: Vec<u8>,
    raw: Vec<u8>,
    frames: Vec<Vec<String>>,
    extra_servers: Vec<(String, std::process::Child)>,
}

impl Tui {
    fn assert_cwd(&mut self, path: &std::path::Path, marker: &str) {
        // Split the success marker in the command so echoed input cannot
        // satisfy the assertion. Canonicalize macOS /tmp -> /private/tmp.
        let path = fs::canonicalize(path).unwrap();
        self.send(
            format!(
                "[ \"$(pwd -P)\" = '{}' ] && printf 'CWD_%s\\n' '{}'\r",
                path.display(),
                marker,
            )
            .as_bytes(),
        );
        self.wait("new shell uses expected directory", |rows| {
            rows.iter()
                .any(|line| line.contains(&format!("CWD_{marker}")))
        });
    }

    fn set_home(&mut self, path: &std::path::Path) {
        self.send(format!("cd '{}'\r", path.display()).as_bytes());
        self.assert_cwd(path, "BEFORE_HOME");
        self.send(b"\x01H");
        self.wait("Prefix+H sets Home, not help", |rows| {
            // The status bar keeps the path's tail on narrow terminals.
            rows.last()
                .unwrap()
                .contains(path.file_name().unwrap().to_str().unwrap())
                && !rows.iter().any(|line| line.contains("ALL ZMUX SHORTCUTS"))
        });
    }

    fn start() -> Self {
        let mut tui = Self::start_hidden();
        tui.show_sidebar();
        tui
    }

    fn start_hidden() -> Self {
        static NEXT: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(0);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = PathBuf::from(format!(
            "/tmp/zmux-ui-{}-{unique}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::write(root.join("zmux.conf"), "").unwrap();
        // Exercise the SSH subprocess/stdio bridge without touching SSH keys
        // or hosts. The shim executes the same remote payload locally using a
        // second isolated socket directory.
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::create_dir_all(root.join("remote")).unwrap();
        fs::write(
            root.join("bin/zmux"),
            "#!/bin/sh\nexport PS1='READY> '\nexec \"$ZMUX_TEST_BIN\" \"$@\"\n",
        )
        .unwrap();
        fs::write(
            root.join("bin/ssh"),
            r#"#!/bin/sh
last=''
previous=''
for argument do
    previous=$last
    last=$argument
done
printf '%s\n' "$previous" >> "$ZMUX_TEST_ROOT/ssh-hosts"
if [ "$previous" = unavailable ]; then exit 255; fi
case "$previous" in
    protocol-missing)
        if [ ! -f "$ZMUX_TEST_ROOT/repair-missing" ]; then exit 127; fi ;;
    protocol-legacy)
        printf 'unknown command protocol-info\n' >&2
        exit 2 ;;
    protocol-major|protocol-caps|protocol-schema|protocol-noisy)
        cat "$ZMUX_TEST_ROOT/$previous.json"
        exit 0 ;;
    stale-server)
        case "$last" in
            *protocol-info*) exec "$ZMUX_TEST_BIN" protocol-info ;;
            *) printf 'ZMUX REJECT {"code":"protocol_version_mismatch","message":"running server is older than the installed binary"}\n'; exit 0 ;;
        esac ;;
esac
TMPDIR="$ZMUX_TEST_ROOT/remote"
export TMPDIR
PS1='READY> '
export PS1
exec /bin/sh -c "$last"
"#,
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            root.join("bin/zmux"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        fs::set_permissions(
            root.join("bin/ssh"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        Self::attach_raw(root, false)
    }

    fn attach(root: PathBuf) -> Self {
        Self::attach_with_all(root, false)
    }

    fn attach_with_all(root: PathBuf, all: bool) -> Self {
        let mut tui = Self::attach_raw(root, all);
        tui.show_sidebar();
        tui
    }

    fn show_sidebar(&mut self) {
        self.wait("default hidden viewport ready", |rows| {
            rows[0].starts_with('┌')
                && rows.iter().any(|line| line.contains("READY>"))
        });
        self.send(b"\x01M");
        self.wait("explicitly show fixed sidebar", |rows| {
            rows[0].contains('●')
        });
    }

    fn attach_raw(root: PathBuf, all: bool) -> Self {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 28,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(BIN);
        command.args(["-L", "ui", "-s", "main"]);
        if all {
            command.arg("a");
        }
        command.env("TMPDIR", &root);
        command.env("ZMUX_CONFIG", root.join("zmux.conf"));
        command.env("ZMUX_TEST_ROOT", &root);
        command.env("ZMUX_TEST_BIN", BIN);
        let path = std::env::var("PATH").unwrap_or_default();
        command.env("PATH", format!("{}:{path}", root.join("bin").display()));
        command.env("SHELL", "/bin/sh");
        command.env("ENV", "/dev/null");
        command.env("PS1", "READY> ");
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.cwd(&root);
        let child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let writer = pair.master.take_writer().unwrap();
        let mut reader = pair.master.try_clone_reader().unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut bytes = [0; 65536];
            while let Ok(len) = reader.read(&mut bytes) {
                if len == 0 || tx.send(bytes[..len].to_vec()).is_err() {
                    break;
                }
            }
        });
        Self {
            root,
            master: pair.master,
            child,
            writer,
            rx,
            screen: AlacrittyTermState::new(28, 120, 0),
            pending: Vec::new(),
            raw: Vec::new(),
            frames: Vec::new(),
            extra_servers: Vec::new(),
        }
    }

    fn start_extra_workspace(&mut self, socket: &str) {
        let child = Command::new(BIN)
            .args(["-L", socket, "-s", "main", "server"])
            .env("TMPDIR", &self.root)
            .env("ZMUX_CONFIG", self.root.join("zmux.conf"))
            .env("SHELL", "/bin/sh")
            .env("ENV", "/dev/null")
            .env("PS1", "READY> ")
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        self.extra_servers.push((socket.to_string(), child));
        let path = self
            .root
            .join(format!("zmux-{}", unsafe { libc::getuid() }))
            .join(socket);
        let until = Instant::now() + Duration::from_secs(8);
        while !path.exists() && Instant::now() < until {
            self.pump();
        }
        assert!(path.exists(), "second workspace server failed to start");
    }

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
    }

    fn lines(&self) -> Vec<String> {
        self.screen
            .visible_rows()
            .iter()
            .map(|row| {
                row.iter()
                    .map(|cell| {
                        cell.as_ref().map(|c| c.text.as_str()).unwrap_or("")
                    })
                    .collect::<String>()
            })
            .collect()
    }

    fn pump(&mut self) {
        if let Ok(bytes) = self.rx.recv_timeout(Duration::from_millis(25)) {
            self.raw.extend_from_slice(&bytes);
            self.pending.extend(bytes);
            while let Some(at) = self
                .pending
                .windows(END_SYNC.len())
                .position(|w| w == END_SYNC)
            {
                let bytes: Vec<_> =
                    self.pending.drain(..at + END_SYNC.len()).collect();
                self.screen.process(&bytes);
                self.frames.push(self.lines());
            }
        }
    }

    fn wait(&mut self, label: &str, predicate: impl Fn(&[String]) -> bool) {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(8) {
            self.pump();
            if predicate(&self.lines()) {
                return;
            }
        }
        panic!(
            "{label}\n{}\nchild status: {:?}\npending output: {:?}",
            self.lines().join("\n"),
            self.child.try_wait(),
            String::from_utf8_lossy(&self.pending)
        );
    }

    fn ready(&mut self) {
        self.wait("initial prompt and tree", |lines| {
            lines.iter().any(|l| l.contains("READY>"))
                && lines.iter().any(|l| l.contains("pane 0"))
        });
    }

    fn command(&mut self, text: &str) {
        self.send(b"\x01:");
        self.send(text.as_bytes());
        self.send(b"\r");
    }

    fn assert_preserved_since(&self, frame: usize, marker: &str) {
        assert!(self.frames.len() > frame);
        for rows in &self.frames[frame..] {
            assert!(
                rows.iter().any(|l| l.contains(marker)),
                "completed repaint lost {marker}:\n{}",
                rows.join("\n")
            );
        }
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        for (socket, child) in &mut self.extra_servers {
            let _ = Command::new(BIN)
                .args(["-L", socket, "kill-server"])
                .env("TMPDIR", &self.root)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = Command::new(BIN)
            .args(["-L", "ui", "kill-server"])
            .env("TMPDIR", &self.root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = Command::new(BIN)
            .args(["-L", "ui", "kill-server"])
            .env("TMPDIR", self.root.join("remote"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn ssh_protocol_failures_stop_retries_and_manual_repair_reconnects() {
    let mut tui = Tui::start();
    tui.ready();
    let local = zmux::ipc::ProtocolInfo::current();
    let mut major = local.clone();
    major.major += 1;
    let mut caps = local.clone();
    caps.capabilities.retain(|cap| cap != "ssh-stdio-v1");
    let mut schema = local.clone();
    schema.schema += 1;
    for (host, info) in [
        ("protocol-major", major),
        ("protocol-caps", caps),
        ("protocol-schema", schema),
    ] {
        fs::write(
            tui.root.join(format!("{host}.json")),
            serde_json::to_vec(&info).unwrap(),
        )
        .unwrap();
    }
    fs::write(
        tui.root.join("protocol-noisy.json"),
        format!("shell banner\n{}", serde_json::to_string(&local).unwrap()),
    )
    .unwrap();
    let hosts = [
        "protocol-missing",
        "protocol-legacy",
        "protocol-major",
        "protocol-caps",
        "protocol-schema",
        "protocol-noisy",
        "stale-server",
    ];
    for host in hosts {
        tui.command(&format!("new -m {host}"));
        tui.wait("protocol error leaves machine unavailable", |rows| {
            rows.iter().any(|l| l.contains(&format!("! {host}")))
                && rows.last().unwrap().contains("[main]")
        });
    }
    let probes = fs::read_to_string(tui.root.join("ssh-hosts")).unwrap();
    for host in hosts {
        assert_eq!(
            probes.lines().filter(|line| *line == host).count(),
            if host == "stale-server" { 2 } else { 1 },
            "{host}"
        );
    }
    assert!(!tui
        .root
        .join("remote")
        .join(format!("zmux-{}", unsafe { libc::getuid() }))
        .join("ui")
        .exists());
    // Making the missing program available must not trigger an implicit retry.
    fs::write(tui.root.join("repair-missing"), "").unwrap();
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until {
        tui.pump();
    }
    assert_eq!(
        fs::read_to_string(tui.root.join("ssh-hosts")).unwrap(),
        probes
    );
    tui.send(b"\x01h");
    tui.wait("tree focus", |rows| rows.iter().any(|l| l.contains('▌')));
    let index = tui
        .lines()
        .iter()
        .position(|l| l.contains("! protocol-missing"))
        .unwrap();
    tui.send(format!("g{}R", "j".repeat(index)).as_bytes());
    tui.wait("explicit R after repair reconnects", |rows| {
        rows.iter().any(|l| l.contains("● protocol-missing"))
            && rows.iter().filter(|l| l.contains("▣ ui")).count() == 2
    });
}

#[test]
fn remote_tree_focus_rename_split_close_and_offline_rename() {
    let mut tui = Tui::start();
    tui.ready();
    tui.command("new -m loopback");
    tui.wait("remote connected and prompt painted", |rows| {
        rows.iter().any(|l| l.contains("loopback"))
            && rows.last().unwrap().contains(" [0] ")
            && rows.iter().filter(|l| l.contains("pane 0")).count() >= 2
            && rows.iter().any(|l| l.contains("READY>"))
    });
    tui.send(b"echo REMOTE_SENTINEL\r");
    tui.wait("remote shell output", |rows| {
        rows.iter()
            .filter(|l| l.contains("REMOTE_SENTINEL"))
            .count()
            >= 2
    });
    tui.send(b"\x01h");
    tui.wait("remote tree focused", |rows| {
        rows.iter().any(|l| l.contains('▌'))
    });
    let index = tui
        .lines()
        .iter()
        .position(|l| l.contains("loopback"))
        .unwrap();
    tui.send(format!("g{}r\x15", "j".repeat(index)).as_bytes());
    tui.send("远程开发机".as_bytes());
    tui.send(b"\r");
    tui.wait("remote display name changed", |rows| {
        rows.iter().any(|l| l.contains('▌'))
            && rows.iter().any(|l| l.contains("远程开发机"))
    });
    let json: serde_json::Value = serde_json::from_slice(
        &fs::read(tui.root.join("machines.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(json["names"]["ssh:8#loopback"], "远程开发机");
    assert!(fs::read_to_string(tui.root.join("ssh-hosts"))
        .unwrap()
        .lines()
        .all(|l| l == "loopback"));
    tui.send(b"lr\x15Remote workspace\r");
    tui.wait("remote workspace rename", |rows| {
        rows.iter().any(|l| l.contains('▌'))
            && rows.iter().any(|l| l.contains("▣ Remote workspace"))
    });
    let baseline = tui.frames.len();
    tui.send(b"\x01M");
    tui.wait("remote full width", |rows| {
        rows[0].starts_with('┌')
            && rows.iter().any(|l| l.contains("REMOTE_SENTINEL"))
    });
    tui.send(b"\x01h\x01h");
    tui.wait("remote sidebar restored", |rows| {
        rows.iter().any(|l| l.contains('▌'))
            && rows.iter().any(|l| l.contains("Remote workspace"))
    });
    tui.assert_preserved_since(baseline, "REMOTE_SENTINEL");
    tui.send(b"\x01l");
    let remote_home = tui.root.join("remote home");
    fs::create_dir(&remote_home).unwrap();
    tui.set_home(&remote_home);
    tui.send(b"cd /\r");
    tui.command("h");
    tui.wait("global help on SSH workspace", |rows| {
        rows.iter().any(|line| line.contains("ALL ZMUX SHORTCUTS"))
    });
    tui.send(b"q");
    tui.wait("remote help dismissed", |rows| {
        !rows.iter().any(|line| line.contains("zmux shortcuts"))
    });
    tui.send(b"\x01%");
    tui.wait("remote split synchronizes sidebar", |rows| {
        rows.iter().any(|l| l.contains("pane 1"))
            && rows
                .iter()
                .map(|l| l.matches("READY>").count())
                .sum::<usize>()
                >= 2
    });
    tui.assert_cwd(&remote_home, "REMOTE_SPLIT");
    tui.send(b"\x01x");
    tui.wait("remote close synchronizes sidebar", |rows| {
        !rows.iter().any(|l| l.contains("pane 1"))
    });
    tui.send(b"\x01M");
    tui.wait("M hides remote sidebar", |rows| rows[0].starts_with('┌'));
    tui.send(b"\x01m");
    tui.wait("remote floating tree", |rows| {
        rows.iter().any(|l| l.contains("Navigation · Esc close"))
            && rows.iter().any(|l| l.contains("Remote workspace"))
            && rows.iter().any(|l| l.contains("远程开发机"))
    });
    tui.send(b"q");
    tui.wait("remote panes restored after popup", |rows| {
        rows[0].starts_with('┌')
            && !rows.iter().any(|l| l.contains("Navigation · Esc close"))
            && rows.iter().any(|l| l.contains("REMOTE_SENTINEL"))
    });
    tui.send(b"\x01M");
    tui.wait("M restores fixed remote sidebar", |rows| {
        rows[0].contains('●')
    });
    tui.command("new -m unavailable");
    tui.wait("offline root is listed", |rows| {
        rows.iter().any(|l| l.contains("unavailable"))
    });
    tui.send(b"\x01h\x01hGr\x15Offline server\r");
    tui.wait("offline machine can be renamed", |rows| {
        rows.iter().any(|l| l.contains('▌'))
            && rows.iter().any(|l| l.contains("Offline server"))
    });
    let saved = zmux::config::machines::MachineNames::load(
        &tui.root.join("machines.json"),
    )
    .unwrap();
    assert_eq!(saved.names["ssh:11#unavailable"], "Offline server");
    assert_eq!(saved.names["ssh:8#loopback"], "远程开发机");
    assert_eq!(
        saved.workspace_name("ssh:8#loopback", "ssh://ssh:8#loopback/ui"),
        Some("Remote workspace")
    );
    tui.send(b"K");
    tui.wait("offline machine delete requires confirmation", |rows| {
        rows.last().unwrap().starts_with("Close machine")
    });
    tui.send(b"n");
    tui.wait("machine delete cancelled", |rows| {
        rows.iter().any(|l| l.contains('▌'))
            && !rows.last().unwrap().starts_with("Close")
            && rows.iter().any(|l| l.contains("Offline server"))
    });
    tui.send(b"K");
    tui.wait("confirm offline removal again", |rows| {
        rows.last().unwrap().starts_with("Close machine")
    });
    tui.send(b"y");
    tui.wait("offline machine removed", |rows| {
        !rows.iter().any(|l| l.contains("● Offline server"))
            && !rows.last().unwrap().starts_with("Close")
    });
    let index = tui
        .lines()
        .iter()
        .position(|l| l.contains("● 远程开发机"))
        .unwrap();
    tui.send(format!("g{}K", "j".repeat(index)).as_bytes());
    tui.wait("online machine delete requires confirmation", |rows| {
        rows.last().unwrap().starts_with("Close machine")
    });
    tui.send(b"y");
    tui.wait("online machine and its workspaces removed", |rows| {
        !rows.iter().any(|l| {
            l.contains("● 远程开发机") || l.contains("▣ Remote workspace")
        }) && rows.iter().any(|l| l.contains("▣ ui"))
            && rows.last().unwrap().contains("[main]")
    });
    let alive = Command::new(BIN)
        .args(["-L", "ui", "ls"])
        .env("TMPDIR", tui.root.join("remote"))
        .output()
        .unwrap();
    assert!(
        alive.status.success()
            && String::from_utf8_lossy(&alive.stdout).contains("0:"),
        "removing a machine must not kill its remote server"
    );
}

#[test]
fn command_edit_focus_split_close_refresh_and_resize() {
    let mut tui = Tui::start();
    tui.ready();
    tui.send(b"echo LEFT_SENTINEL\r");
    tui.wait("shell output", |rows| {
        rows.iter().filter(|l| l.contains("LEFT_SENTINEL")).count() >= 2
    });

    tui.send(b"\x01:abcdefghijklmnop");
    tui.wait("command typed", |rows| {
        rows.last().unwrap().trim_end() == ":abcdefghijklmnop"
    });
    tui.send(b"\x7f\x7f\x7f\x7f\x7f");
    tui.wait("shorter command erases tail", |rows| {
        rows.last().unwrap().trim_end() == ":abcdefghijk"
    });
    tui.send(b"\x15");
    tui.wait("Ctrl-U clears input and underlying chrome", |rows| {
        rows.last().unwrap().trim_end() == ":"
    });
    tui.send("\x1b[200~中文ab\x1b[201~".as_bytes());
    tui.send(b"\x1b[D\x1b[3~");
    tui.wait("unicode cursor/delete", |rows| {
        rows.last().unwrap().trim_end() == ":中文a"
    });
    assert_eq!(tui.screen.cursor_position(), (27, 6));
    assert!(!tui.screen.hide_cursor());
    tui.send(b"\x15");
    tui.send(format!("\x1b[200~{}END\x1b[201~", "x".repeat(150)).as_bytes());
    tui.wait("long command scrolls to cursor", |rows| {
        rows.last().unwrap().trim_end().ends_with("END")
    });
    assert_eq!(tui.screen.cursor_position(), (27, 119));
    tui.send(b"\x1b[H");
    tui.wait("Home reveals start", |rows| {
        rows.last().unwrap().starts_with(":xxx")
    });
    assert_eq!(tui.screen.cursor_position(), (27, 1));
    tui.send(b"\x1b[F");
    tui.wait("End reveals tail", |rows| {
        rows.last().unwrap().trim_end().ends_with("END")
    });
    tui.send(b"\x1b");
    tui.wait("close command prompt", |rows| {
        !rows.last().unwrap().starts_with(':')
    });

    tui.send(b"\x01h");
    tui.wait("left edge focuses tree", |rows| {
        rows.iter().any(|l| l.contains('▌'))
    });
    tui.send(b"k");
    tui.wait("up in tree", |rows| {
        rows.iter().any(|l| l.contains('▌') && l.contains("shell"))
    });
    tui.send(b"j");
    tui.wait("down in tree", |rows| {
        rows.iter().any(|l| l.contains('▌') && l.contains("pane 0"))
    });
    tui.send(b"\x01l");
    tui.wait("prefix right returns to terminal", |rows| {
        !rows.iter().any(|l| l.contains('▌'))
    });
    tui.send(b"\x01\x08");
    tui.wait("held Ctrl-H enters tree", |rows| {
        rows.iter().any(|l| l.contains('▌'))
    });
    tui.send(b"\x01l");
    tui.wait("return to pane", |rows| {
        !rows.iter().any(|l| l.contains('▌'))
    });

    let baseline = tui.frames.len();
    let start = Instant::now();
    tui.send(b"\x01%");
    tui.wait("horizontal split updates tree and prompt", |rows| {
        rows.iter().any(|l| l.contains("pane 1"))
            && rows
                .iter()
                .map(|l| l.matches("READY>").count())
                .sum::<usize>()
                >= 2
    });
    assert!(
        start.elapsed() < Duration::from_millis(1200),
        "split lagged {:?}",
        start.elapsed()
    );
    tui.assert_preserved_since(baseline, "LEFT_SENTINEL");

    tui.send(b"\x01x");
    tui.wait("close pane updates sidebar", |rows| {
        !rows.iter().any(|l| l.contains("pane 1"))
            && rows.iter().any(|l| l.contains("LEFT_SENTINEL"))
    });
    tui.assert_preserved_since(baseline, "LEFT_SENTINEL");

    tui.send(b"\x01\"");
    tui.wait("vertical split", |rows| {
        rows.iter().any(|l| l.contains("pane 1"))
            && rows.iter().filter(|l| l.contains("READY>")).count() >= 2
    });
    tui.send(b"\x01h");
    tui.wait("vertical split left boundary", |rows| {
        rows.iter().any(|l| l.contains('▌'))
    });
    let refresh_start = tui.frames.len();
    tui.send(b"R");
    // One UI frame and one authoritative server repaint.
    let deadline = Instant::now() + Duration::from_secs(3);
    while tui.frames.len() < refresh_start + 2 && Instant::now() < deadline {
        tui.pump();
    }
    tui.assert_preserved_since(refresh_start, "LEFT_SENTINEL");
    tui.send(b"\x01l");
    tui.master
        .resize(PtySize {
            rows: 30,
            cols: 140,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    tui.screen.resize(30, 140);
    tui.wait("resize preserves content", |rows| {
        rows.iter().any(|l| l.contains("LEFT_SENTINEL"))
            && rows.iter().any(|l| l.contains("pane 1"))
            && rows[0].chars().last() == Some('┐')
            && rows.last().unwrap().contains("[main]")
    });
    let resize_start = tui.frames.len();
    tui.master
        .resize(PtySize {
            rows: 24,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    tui.screen.resize(24, 100);
    tui.wait("shrink repaints borders and status", |rows| {
        rows[0].chars().last() == Some('┐')
            && rows.last().unwrap().contains("[main]")
            && rows.iter().any(|l| l.contains("LEFT_SENTINEL"))
    });
    tui.assert_preserved_since(resize_start, "LEFT_SENTINEL");
    assert!(
        !tui.raw.windows(4).any(|w| w == b"\x1b[2J"),
        "global clear causes sidebar flicker"
    );
}

#[test]
fn windows_sessions_and_repeated_splits_restore_full_content() {
    let mut tui = Tui::start();
    tui.ready();
    tui.send(b"echo ORIGINAL_WINDOW\r");
    tui.wait("original window output", |rows| {
        rows.iter()
            .filter(|l| l.contains("ORIGINAL_WINDOW"))
            .count()
            >= 2
    });
    tui.send(b"\x01c");
    tui.wait("new window", |rows| {
        rows.last().unwrap().contains("*[1]")
            && rows.iter().any(|l| l.contains("READY>"))
            && !rows.iter().any(|l| l.contains("ORIGINAL_WINDOW"))
    });
    tui.send(b"echo SECOND_WINDOW\r");
    tui.wait("second window output", |rows| {
        rows.iter().filter(|l| l.contains("SECOND_WINDOW")).count() >= 2
    });
    tui.send(b"\x01p");
    tui.wait("previous window full repaint", |rows| {
        rows.iter().any(|l| l.contains("ORIGINAL_WINDOW"))
            && !rows.iter().any(|l| l.contains("SECOND_WINDOW"))
    });
    tui.send(b"\x01n");
    tui.wait("next window full repaint", |rows| {
        rows.iter().any(|l| l.contains("SECOND_WINDOW"))
            && !rows.iter().any(|l| l.contains("ORIGINAL_WINDOW"))
    });
    tui.command("kill-window");
    tui.wait("closing window restores sibling", |rows| {
        rows.iter().any(|l| l.contains("ORIGINAL_WINDOW"))
            && rows.iter().filter(|l| l.contains("pane 0")).count() == 1
            && !rows.iter().any(|l| l.contains("SECOND_WINDOW"))
    });
    tui.command("new -s work");
    tui.wait("new session", |rows| {
        rows.last().unwrap().contains("[work]")
            && rows.iter().any(|l| l.contains("READY>"))
            && !rows.iter().any(|l| l.contains("ORIGINAL_WINDOW"))
    });
    tui.command("switch-client -t main");
    tui.wait("switch session restores output", |rows| {
        rows.last().unwrap().contains("[main]")
            && rows.iter().any(|l| l.contains("ORIGINAL_WINDOW"))
    });
    tui.command("kill-session -t work");
    tui.wait("inactive session disappears", |rows| {
        !rows.iter().any(|l| l.contains("◆ work"))
    });
    let baseline = tui.frames.len();
    for _ in 0..4 {
        tui.send(b"\x01%");
        tui.wait("repeated split", |rows| {
            rows.iter().any(|l| l.contains("pane 1"))
        });
        tui.send(b"\x01x");
        tui.wait("repeated close", |rows| {
            !rows.iter().any(|l| l.contains("pane 1"))
        });
    }
    tui.assert_preserved_since(baseline, "ORIGINAL_WINDOW");
}

#[test]
fn multiple_workspaces_switch_detach_and_keep_independent_sessions() {
    let mut original = Tui::start();
    original.ready();
    original.send(b"echo FIRST_WORKSPACE\r");
    original.wait("first workspace content", |rows| {
        rows.iter()
            .filter(|l| l.contains("FIRST_WORKSPACE"))
            .count()
            >= 2
    });
    original.start_extra_workspace("second");
    original.send(b"\x01d");
    let until = Instant::now() + Duration::from_secs(3);
    while original.child.try_wait().unwrap().is_none() && Instant::now() < until
    {
        original.pump();
    }
    let mut tui = Tui::attach_with_all(original.root.clone(), true);
    tui.extra_servers = std::mem::take(&mut original.extra_servers);
    tui.ready();
    tui.wait("both workspaces and identically named sessions", |rows| {
        rows.iter().any(|l| l.contains("▣ second"))
            && rows.iter().filter(|l| l.contains("◆ main")).count() == 2
    });
    tui.send(b"\x01h\x01h");
    tui.wait("tree focused", |rows| rows.iter().any(|l| l.contains('▌')));
    let index = tui
        .lines()
        .iter()
        .position(|l| l.contains("▣ second"))
        .unwrap();
    tui.send(format!("g{}\r", "j".repeat(index)).as_bytes());
    tui.wait("second workspace selected and first content gone", |rows| {
        !rows.iter().any(|l| l.contains('▌'))
            && !rows.iter().any(|l| l.contains("FIRST_WORKSPACE"))
    });
    tui.send(b"echo SECOND_WORKSPACE\r");
    tui.wait("second workspace content", |rows| {
        rows.iter()
            .filter(|l| l.contains("SECOND_WORKSPACE"))
            .count()
            >= 2
    });
    tui.send(b"\x01h\x01hkkkK");
    tui.wait("workspace detach confirmation", |rows| {
        rows.last().unwrap().starts_with("Close workspace")
    });
    tui.send(b"\r");
    tui.wait("Enter cancels workspace removal by default", |rows| {
        rows.iter().any(|l| l.contains('▌'))
            && !rows.last().unwrap().starts_with("Close")
            && rows.iter().any(|l| l.contains("▣ second"))
    });
    tui.send(b"K");
    tui.wait("workspace confirmation again", |rows| {
        rows.last().unwrap().starts_with("Close workspace")
    });
    tui.send(b"y");
    tui.wait("detach restores first workspace completely", |rows| {
        rows.iter().any(|l| l.contains("FIRST_WORKSPACE"))
            && !rows.iter().any(|l| l.contains("SECOND_WORKSPACE"))
            && !rows.iter().any(|l| l.contains("▣ second"))
    });
    let alive = Command::new(BIN)
        .args(["-L", "second", "ls"])
        .env("TMPDIR", &tui.root)
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&alive.stdout).contains("main"),
        "workspace close must not kill sessions"
    );
}

#[test]
fn workspace_home_is_shared_within_workspace_and_isolated_across_workspaces() {
    let mut original = Tui::start();
    original.ready();
    let first_home = original.root.join("first home");
    let second_home = original.root.join("second home");
    fs::create_dir(&first_home).unwrap();
    fs::create_dir(&second_home).unwrap();
    original.set_home(&first_home);
    // Changing a running shell's cwd must not change the pinned Home.
    original.send(b"cd /\r");
    original.assert_cwd(std::path::Path::new("/"), "EXISTING_UNCHANGED");
    original.start_extra_workspace("second");
    original.send(b"\x01d");
    let until = Instant::now() + Duration::from_secs(3);
    while original.child.try_wait().unwrap().is_none() && Instant::now() < until
    {
        original.pump();
    }
    let mut tui = Tui::attach_with_all(original.root.clone(), true);
    tui.extra_servers = std::mem::take(&mut original.extra_servers);
    tui.ready();
    tui.send(b"\x01%");
    tui.wait("split after reattach", |rows| {
        rows.iter().any(|l| l.contains("pane 1"))
    });
    tui.assert_cwd(&first_home, "SPLIT_HOME");
    tui.command("new-window -n home-window");
    tui.wait("new window", |rows| {
        rows.iter().any(|l| l.contains("home-window"))
    });
    tui.assert_cwd(&first_home, "WINDOW_HOME");
    tui.command("new -s home-session");
    tui.wait("new session", |rows| {
        rows.last().unwrap().contains("[home-session]")
    });
    tui.assert_cwd(&first_home, "SESSION_HOME");

    tui.send(b"\x01h\x01h");
    tui.wait("select second workspace", |rows| {
        rows.iter().any(|l| l.contains('▌'))
            && rows.iter().any(|l| l.contains("▣ second"))
    });
    let index = tui
        .lines()
        .iter()
        .position(|l| l.contains("▣ second"))
        .unwrap();
    tui.send(format!("g{}\r", "j".repeat(index)).as_bytes());
    tui.wait("second workspace active", |rows| {
        !rows.iter().any(|l| l.contains('▌'))
            && rows.last().unwrap().contains("[main]")
    });
    let root = tui.root.clone();
    tui.assert_cwd(&root, "SECOND_UNCHANGED");
    tui.set_home(&second_home);
    tui.send(b"cd /\r\x01%");
    tui.wait("second workspace split", |rows| {
        rows.iter().any(|l| l.contains("pane 1"))
    });
    tui.assert_cwd(&second_home, "SECOND_HOME");

    tui.send(b"\x01h\x01h");
    tui.wait("tree focus again", |rows| {
        rows.iter().any(|l| l.contains('▌'))
    });
    let index = tui.lines().iter().position(|l| l.contains("▣ ui")).unwrap();
    tui.send(format!("g{}\r", "j".repeat(index)).as_bytes());
    tui.wait("first workspace restored", |rows| {
        !rows.iter().any(|l| l.contains('▌'))
            && rows.last().unwrap().contains("[home-session]")
    });
    // An old window's default directory must not override Workspace Home.
    tui.command("switch-client -t main");
    tui.wait("original session", |rows| {
        rows.last().unwrap().contains("[main]")
    });
    tui.command("new-window -n isolation-check");
    tui.wait("isolation check window", |rows| {
        rows.iter().any(|l| l.contains("isolation-check"))
    });
    tui.assert_cwd(&first_home, "FIRST_STILL_PINNED");
    assert!(!tui.raw.windows(4).any(|w| w == b"\x1b[2J"));
}

#[test]
fn floating_sidebar_preserves_viewport_prompts_and_node_operations() {
    let mut tui = Tui::start_hidden();
    tui.wait("sidebar is hidden by default", |rows| {
        rows[0].starts_with('┌')
            && rows.iter().any(|l| l.contains("READY>"))
            && !rows.iter().any(|l| l.contains("WORKSPACES"))
    });
    tui.send(b"echo FLOAT_LEFT\r");
    tui.wait("left output", |rows| {
        rows.iter().filter(|l| l.contains("FLOAT_LEFT")).count() >= 2
    });
    tui.send(b"\x01%");
    tui.wait("full width split", |rows| {
        rows.iter()
            .map(|l| l.matches("READY>").count())
            .sum::<usize>()
            >= 2
    });
    tui.send(b"echo FLOAT_RIGHT; BEFORE=$(stty size)\r");
    tui.wait("right output", |rows| {
        rows.iter().filter(|l| l.contains("FLOAT_RIGHT")).count() >= 2
    });
    tui.send(b"\x01m");
    tui.wait("floating tree with both panes", |rows| {
        rows[0].starts_with('┌')
            && rows.iter().any(|l| l.contains("Navigation · Esc close"))
            && rows.iter().any(|l| l.contains("pane 0"))
            && rows.iter().any(|l| l.contains("pane 1"))
    });
    tui.send(b"r\x15floating-pane\r");
    tui.wait("rename inside floating tree", |rows| {
        rows.iter().any(|l| l.contains("Navigation · Esc close"))
            && rows.iter().any(|l| l.contains("floating-pane"))
            && !rows.last().unwrap().starts_with("Rename")
    });
    tui.send(b"kkh");
    tui.wait("collapse window in popup", |rows| {
        !rows.iter().any(|l| l.contains("floating-pane"))
    });
    tui.send(b"l");
    tui.wait("expand window in popup", |rows| {
        rows.iter().any(|l| l.contains("floating-pane"))
    });
    tui.send(b"H");
    tui.wait("popup navigation help", |rows| {
        rows.iter().any(|l| l.contains("Sidebar keys"))
    });
    tui.send(b"\x1b");
    tui.wait("back to popup after help", |rows| {
        rows.iter().any(|l| l.contains("Navigation · Esc close"))
            && !rows.iter().any(|l| l.contains("Sidebar keys"))
    });
    // Mouse hit testing must use the centered tree's origin, not the left edge.
    let lines = tui.lines();
    let row = lines
        .iter()
        .position(|l| l.contains("floating-pane"))
        .unwrap();
    let column = lines[row]
        .chars()
        .collect::<Vec<_>>()
        .windows("floating-pane".len())
        .position(|w| w.iter().collect::<String>() == "floating-pane")
        .unwrap();
    tui.send(
        format!(
            "\x1b[<0;{};{}M\x1b[<0;{};{}m",
            column + 1,
            row + 1,
            column + 1,
            row + 1
        )
        .as_bytes(),
    );
    tui.wait("click activates pane and restores both terminals", |rows| {
        !rows.iter().any(|l| l.contains("Navigation · Esc close"))
            && rows.iter().any(|l| l.contains("FLOAT_LEFT"))
            && rows.iter().any(|l| l.contains("FLOAT_RIGHT"))
    });
    tui.send(
        b"[ \"$(stty size)\" = \"$BEFORE\" ] && printf 'SIZE_%s\\n' SAME\r",
    );
    tui.wait("popup never resized the pane", |rows| {
        rows.iter().any(|l| l.contains("SIZE_SAME"))
    });

    tui.send(b"\x01:unfinished input");
    tui.wait("command input", |rows| {
        rows.last().unwrap().trim_end() == ":unfinished input"
    });
    tui.send(b"\x01m");
    tui.wait("global popup from command mode", |rows| {
        rows.iter().any(|l| l.contains("Navigation · Esc close"))
    });
    tui.send(b"\x01m");
    tui.wait("command restored after popup toggle", |rows| {
        rows.last().unwrap().trim_end() == ":unfinished input"
    });
    tui.send(b"\x01M");
    tui.wait("global M shows sidebar without losing input", |rows| {
        rows[0].contains('●')
            && rows.last().unwrap().trim_end() == ":unfinished input"
    });
    tui.send(b"\x01M");
    tui.wait("global M hides sidebar without losing input", |rows| {
        rows[0].starts_with('┌')
            && rows.last().unwrap().trim_end() == ":unfinished input"
    });
    tui.send(b"\x1b");
    tui.wait("command closed", |rows| {
        !rows.last().unwrap().starts_with(':')
    });

    tui.send(b"\x01m");
    tui.wait("reopen before resize", |rows| {
        rows.iter().any(|l| l.contains("Navigation · Esc close"))
    });
    tui.master
        .resize(PtySize {
            rows: 12,
            cols: 50,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    tui.screen.resize(12, 50);
    tui.wait("popup fits small terminal", |rows| {
        rows.iter().any(|l| l.contains("Navigation · Esc close"))
    });
    tui.master
        .resize(PtySize {
            rows: 28,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    tui.screen.resize(28, 120);
    tui.wait("popup expands with terminal", |rows| {
        rows.iter().any(|l| l.contains("floating-pane"))
    });
    tui.send(b"\x1b[<0;2;2M\x1b[<0;2;2m");
    tui.wait("outside click closes popup", |rows| {
        rows[0].starts_with('┌')
            && !rows.iter().any(|l| l.contains("Navigation · Esc close"))
    });
    assert!(!tui.raw.windows(4).any(|w| w == b"\x1b[2J"));
}

#[test]
fn removed_shortcuts_are_inert_and_k_confirms_each_node_operation() {
    let mut tui = Tui::start_hidden();
    tui.wait("hidden startup", |rows| {
        rows[0].starts_with('┌') && rows.iter().any(|l| l.contains("READY>"))
    });
    for key in [b't', b'T', b'S'] {
        tui.send(&[1, key]);
        let until = Instant::now() + Duration::from_millis(150);
        while Instant::now() < until {
            tui.pump();
        }
        assert!(tui.lines()[0].starts_with('┌'));
        assert!(!tui
            .lines()
            .iter()
            .any(|l| l.contains("WORKSPACES") || l.contains("Sessions")));
    }
    tui.send(b"\x01m");
    tui.wait("floating navigation", |rows| {
        rows.iter().any(|l| l.contains("Navigation · Esc close"))
    });
    tui.send(b"k");
    tui.wait("select expandable window", |rows| {
        rows.iter().any(|l| l.contains('▌') && l.contains("shell"))
    });
    let selected = tui.lines().into_iter().find(|l| l.contains('▌')).unwrap();
    for keys in [
        b"T".as_slice(),
        b"p",
        b"P",
        b"J",
        b" ",
        b"o",
        b"\t",
        b"\x01t",
        b"\x01T",
        b"\x01S",
        b"\x01j",
        b"\x01k",
    ] {
        tui.send(keys);
        let until = Instant::now() + Duration::from_millis(150);
        while Instant::now() < until {
            tui.pump();
        }
        assert_eq!(
            tui.lines().into_iter().find(|l| l.contains('▌')),
            Some(selected.clone()),
            "removed shortcut {keys:?} changed focus"
        );
        assert!(
            tui.lines().iter().any(|l| l.contains("pane 0")),
            "removed key collapsed/opened tree"
        );
    }
    tui.send(b"q");
    tui.wait("q closes popup", |rows| {
        !rows.iter().any(|l| l.contains("Navigation · Esc close"))
    });
    tui.send(b"\x01%\x01m");
    tui.wait("split visible in popup", |rows| {
        rows.iter().any(|l| l.contains("pane 1"))
    });
    tui.send(b"GKr");
    tui.wait("K asks, other keys do not confirm", |rows| {
        rows.last().unwrap().starts_with("Close pane")
            && rows.iter().any(|l| l.contains("pane 1"))
    });
    tui.send(b"\x1b");
    tui.wait("Escape cancels deletion", |rows| {
        !rows.last().unwrap().starts_with("Close")
            && rows.iter().any(|l| l.contains("pane 1"))
    });
    tui.send(b"K");
    tui.wait("pane confirmation", |rows| {
        rows.last().unwrap().starts_with("Close pane")
    });
    tui.send(b"y");
    tui.wait("pane removed", |rows| {
        !rows.iter().any(|l| l.contains("pane 1"))
            && rows.iter().any(|l| l.contains("Navigation · Esc close"))
    });
    tui.send(b"q");
    tui.wait("close after delete", |rows| {
        !rows.iter().any(|l| l.contains("Navigation · Esc close"))
    });
    tui.command("new-window -n disposable");
    tui.wait("second window created", |rows| {
        rows.last().unwrap().contains("disposable")
    });
    tui.send(b"\x01m");
    tui.wait("window appears in navigation snapshot", |rows| {
        rows.iter().any(|l| l.contains("□ disposable"))
    });
    tui.send(b"GkK");
    tui.wait("window confirmation", |rows| {
        rows.last().unwrap().starts_with("Close window")
    });
    tui.send(b"y");
    tui.wait("window removed", |rows| {
        !rows.iter().any(|l| l.contains("□ disposable"))
            && !rows.last().unwrap().starts_with("Close")
    });
    tui.send(b"q");
    tui.wait("leave window tree", |rows| {
        !rows.iter().any(|l| l.contains("Navigation · Esc close"))
    });
    tui.command("new -s disposable-session");
    tui.wait("second session created", |rows| {
        rows.last().unwrap().contains("[disposable-session]")
    });
    tui.send(b"\x01m");
    tui.wait("session appears in navigation snapshot", |rows| {
        rows.iter().any(|l| l.contains("◆ disposable-session"))
    });
    tui.send(b"GkkK");
    tui.wait("session confirmation", |rows| {
        rows.last().unwrap().starts_with("Close session")
    });
    tui.send(b"y");
    tui.wait("session removed", |rows| {
        !rows.iter().any(|l| l.contains("◆ disposable-session"))
            && rows.last().unwrap().contains("[main]")
    });
    tui.send(b"q");
    tui.wait("leave session tree", |rows| {
        !rows.iter().any(|l| l.contains("Navigation · Esc close"))
    });
    tui.send(b"\x01M\x01h");
    tui.wait("fixed sidebar focused", |rows| {
        rows.iter().any(|l| l.contains('▌'))
    });
    tui.send(b"\x1b");
    tui.wait("Escape hides fixed sidebar", |rows| {
        rows[0].starts_with('┌')
    });
}

#[test]
fn global_shortcuts_help_aliases_restore_split_panes() {
    let mut tui = Tui::start();
    tui.ready();
    tui.send(b"echo HELP_LEFT\r");
    tui.wait("left pane output", |rows| {
        rows.iter().filter(|l| l.contains("HELP_LEFT")).count() >= 2
    });
    tui.send(b"\x01%");
    tui.wait("split ready", |rows| {
        rows.iter().any(|l| l.contains("pane 1"))
            && rows
                .iter()
                .map(|l| l.matches("READY>").count())
                .sum::<usize>()
                >= 2
    });
    tui.send(b"echo HELP_RIGHT\r");
    tui.wait("right pane output", |rows| {
        rows.iter().filter(|l| l.contains("HELP_RIGHT")).count() >= 2
    });
    tui.command("h");
    tui.wait("global shortcut popup", |rows| {
        rows.iter().any(|l| l.contains("ALL ZMUX SHORTCUTS"))
            && rows.iter().any(|l| l.contains("Prefix+%"))
    });
    tui.send(b"G");
    tui.wait("full reference scrolls to last section", |rows| {
        rows.iter().any(|l| l.contains("Shell Ctrl+c/d/l/r/z"))
    });
    tui.send(b"\x1b");
    tui.wait("all panes restore after help", |rows| {
        !rows.iter().any(|l| l.contains("zmux shortcuts"))
            && rows.iter().any(|l| l.contains("HELP_LEFT"))
            && rows.iter().any(|l| l.contains("HELP_RIGHT"))
    });

    tui.command("help");
    tui.wait("long help alias", |rows| {
        rows.iter().any(|l| l.contains("ALL ZMUX SHORTCUTS"))
    });
    tui.send(b"\x1b");
    tui.wait("help closes without command residue", |rows| {
        !rows.iter().any(|l| l.contains("zmux shortcuts"))
            && !rows.last().unwrap().starts_with(':')
    });
    tui.send(b"\x01h\x01h");
    tui.wait("navigation focus", |rows| {
        rows.iter().any(|l| l.contains('▌'))
    });
    tui.send(b"H");
    tui.wait("sidebar-only help remains", |rows| {
        rows.iter().any(|l| l.contains("Sidebar keys"))
    });
    tui.send(b"H\x01M");
    tui.wait("sidebar hidden", |rows| rows[0].starts_with('┌'));
    tui.command("help");
    tui.wait("help without sidebar", |rows| {
        rows.iter().any(|l| l.contains("ALL ZMUX SHORTCUTS"))
    });
    tui.send(b"q");
    tui.wait("hidden layout restored", |rows| {
        rows[0].starts_with('┌')
            && !rows.iter().any(|l| l.contains("zmux shortcuts"))
            && rows.iter().any(|l| l.contains("HELP_LEFT"))
            && rows.iter().any(|l| l.contains("HELP_RIGHT"))
    });
    assert!(!tui.raw.windows(4).any(|w| w == b"\x1b[2J"));
}

#[test]
fn sidebar_toggle_help_and_workspace_rename() {
    let mut tui = Tui::start();
    tui.ready();
    tui.send(b"echo TOGGLE_SENTINEL\r");
    tui.wait("original output", |rows| {
        rows.iter()
            .filter(|l| l.contains("TOGGLE_SENTINEL"))
            .count()
            >= 2
    });
    tui.send(b"\x01h\x01h");
    tui.wait("Prefix+h focuses sidebar", |rows| {
        rows.iter().any(|l| l.contains('▌'))
    });
    // pane -> window -> session -> workspace
    tui.send(b"kkkr\x15");
    tui.send("项目空间".as_bytes());
    tui.send(b"\r");
    tui.wait("workspace is renamed, not session", |rows| {
        rows.iter().any(|l| l.contains('▌'))
            && rows.iter().any(|l| l.contains("▣ 项目空间"))
            && rows.iter().any(|l| l.contains("◆ main"))
    });
    let saved = zmux::config::machines::MachineNames::load(
        &tui.root.join("machines.json"),
    )
    .unwrap();
    assert_eq!(saved.workspace_name("local", "ui"), Some("项目空间"));
    tui.send(b"h");
    tui.wait("workspace collapse hides descendants", |rows| {
        !rows.iter().any(|l| l.contains("pane 0"))
            && rows.iter().any(|l| l.contains("项目空间"))
            && !rows.iter().any(|l| l.contains("Sidebar keys"))
    });
    tui.send(b"h");
    tui.wait("h on collapsed workspace selects machine parent", |rows| {
        rows[0].contains('▌') && rows[0].contains('●')
    });
    tui.send(b"l");
    tui.wait("l on expanded machine selects workspace child", |rows| {
        rows.iter()
            .any(|l| l.contains('▌') && l.contains("项目空间"))
    });
    tui.send(b"l");
    tui.wait("workspace expand", |rows| {
        rows.iter().any(|l| l.contains("pane 0"))
    });
    tui.send(b"H");
    tui.wait("H opens complete help", |rows| {
        rows.iter().any(|l| l.contains("Sidebar keys"))
            && rows.iter().any(|l| l.contains("Prefix+M"))
    });
    tui.send(b"H");
    tui.wait("H returns from help to navigation", |rows| {
        rows.iter().any(|l| l.contains('▌'))
            && !rows.iter().any(|l| l.contains("Sidebar keys"))
    });
    tui.send(b"H");
    tui.wait("H reopens help", |rows| {
        rows.iter().any(|l| l.contains("Sidebar keys"))
    });
    tui.send(b"G");
    tui.wait("help scroll reaches last shortcut", |rows| {
        rows.iter().any(|l| l.contains("saved in machines.json"))
    });
    tui.send(b"\x1b");
    tui.wait("help closes and pane repaints", |rows| {
        !rows.iter().any(|l| l.contains("Sidebar keys"))
            && rows.iter().any(|l| l.contains("TOGGLE_SENTINEL"))
    });
    let start = tui.frames.len();
    for _ in 0..4 {
        tui.send(b"q");
        tui.wait("q hides sidebar and terminal fills width", |rows| {
            rows[0].starts_with('┌')
                && !rows.iter().any(|l| l.contains("WORKSPACES"))
        });
        tui.send(b"\x01h\x01h");
        tui.wait("Prefix+h opens sidebar at left boundary", |rows| {
            rows.iter().any(|l| l.contains('▌'))
                && rows.iter().any(|l| l.contains("项目空间"))
        });
    }
    tui.assert_preserved_since(start, "TOGGLE_SENTINEL");
    tui.send(b"\x01l\x01M");
    tui.wait("Prefix+M works from terminal", |rows| {
        rows[0].starts_with('┌')
    });
    tui.send(b"\x01%");
    tui.wait("split while sidebar hidden", |rows| {
        rows[0].starts_with('┌')
            && rows
                .iter()
                .map(|l| l.matches("READY>").count())
                .sum::<usize>()
                >= 2
    });
    tui.send(b"\x01h\x01h");
    tui.wait("open sidebar with split panes", |rows| {
        rows.iter().any(|l| l.contains('▌'))
            && rows.iter().any(|l| l.contains("pane 1"))
    });
    tui.send(b"H\x01M");
    tui.wait("global hide also dismisses help", |rows| {
        rows[0].starts_with('┌')
            && !rows.iter().any(|l| l.contains("Sidebar keys"))
    });
    assert!(!tui.raw.windows(4).any(|w| w == b"\x1b[2J"));
}

#[test]
fn machine_rename_persists_across_client_restart_and_tabs_are_removed() {
    let mut tui = Tui::start();
    tui.ready();
    tui.send(b"\x01hgr\x15");
    tui.send("我的工作机".as_bytes());
    tui.send(b"\r");
    tui.wait("rename local machine", |rows| {
        rows.iter().any(|l| l.contains('▌'))
            && rows.iter().any(|l| l.contains("我的工作机"))
    });
    let json: serde_json::Value = serde_json::from_slice(
        &fs::read(tui.root.join("machines.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(json["names"]["local"], "我的工作机");
    tui.send(b"r\x15Cancelled name\x1b");
    tui.wait("cancel machine rename", |rows| {
        rows.iter().any(|l| l.contains('▌'))
            && !rows.last().unwrap().contains("Rename machine")
    });
    let saved = zmux::config::machines::MachineNames::load(
        &tui.root.join("machines.json"),
    )
    .unwrap();
    assert_eq!(saved.names["local"], "我的工作机");
    tui.send(b"\x01l");
    for key in [b"\x01/".as_slice(), b"\x01\t"] {
        tui.send(key);
        let end = Instant::now() + Duration::from_millis(120);
        while Instant::now() < end {
            tui.pump();
        }
        assert!(!tui.lines().iter().any(|l| l.contains("Tabs")
            || l.contains("Switch Tab")
            || l.contains("Rename Tab")));
    }
    let root = tui.root.clone();
    tui.send(b"\x01d");
    let until = Instant::now() + Duration::from_secs(3);
    while tui.child.try_wait().unwrap().is_none() && Instant::now() < until {
        tui.pump();
    }
    let mut restored = Tui::attach(root);
    restored.ready();
    restored.wait("saved name after attach", |rows| {
        rows.iter().any(|l| l.contains("我的工作机"))
    });
    // Both objects share this isolated test directory; restored drops first.
    assert!(!Command::new(BIN)
        .args(["new", "-t", "obsolete"])
        .output()
        .unwrap()
        .status
        .success());
}
