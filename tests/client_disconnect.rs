//! Closing a PTY must detach the client even when no SIGHUP is delivered.
#![cfg(unix)]
use std::{
    fs::{self, File},
    io::{Read, Write},
    os::{fd::FromRawFd, unix::process::CommandExt},
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const BIN: &str = env!("CARGO_BIN_EXE_zmux");

struct Session {
    root: PathBuf,
    child: Option<Child>,
}

impl Session {
    fn command(&self) -> Command {
        let mut command = Command::new(BIN);
        command
            .args(["--environment-root"])
            .arg(&self.root)
            .args(["-L", "disconnect"])
            .env("TMPDIR", &self.root)
            .env("ZMUX_CONFIG", self.root.join("zmux.conf"))
            .env("SHELL", "/bin/sh")
            .env("ENV", "/dev/null")
            .env("PS1", "DISCONNECT_READY> ")
            .env("TERM", "xterm-256color")
            .current_dir(&self.root);
        command
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = self
            .command()
            .arg("kill-server")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn pty_eof_without_sighup_exits_client_and_preserves_shell() {
    let root = std::env::temp_dir()
        .join(format!("zmux-disconnect-{}", std::process::id()));
    fs::create_dir(&root).unwrap();
    fs::write(root.join("zmux.conf"), "").unwrap();
    let mut session = Session { root, child: None };
    let (mut master_fd, mut slave_fd) = (-1, -1);
    let size = libc::winsize {
        ws_row: 24,
        ws_col: 100,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null(),
                &size,
            )
        },
        0
    );
    let mut master = unsafe { File::from_raw_fd(master_fd) };
    let slave = unsafe { File::from_raw_fd(slave_fd) };
    // Keep the master out of the child, and make parent reads bounded.
    assert_eq!(
        unsafe { libc::fcntl(master_fd, libc::F_SETFD, libc::FD_CLOEXEC) },
        0
    );
    let flags = unsafe { libc::fcntl(master_fd, libc::F_GETFL) };
    assert!(flags >= 0);
    assert_eq!(
        unsafe {
            libc::fcntl(master_fd, libc::F_SETFL, flags | libc::O_NONBLOCK)
        },
        0
    );
    let mut command = session.command();
    command
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave);
    unsafe {
        command.pre_exec(|| {
            // No controlling terminal: closing the master delivers EOF but
            // cannot send SIGHUP. This exercises the reader, not the handler.
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    session.child = Some(command.spawn().unwrap());
    drop(command);
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut output = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        match master.read(&mut buffer) {
            Ok(n) => output.extend_from_slice(&buffer[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => panic!("PTY read failed: {e}"),
        }
        if output
            .windows(b"\x1b[?2026l".len())
            .any(|s| s == b"\x1b[?2026l")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "client never became ready: {}",
            String::from_utf8_lossy(&output)
        );
        thread::sleep(Duration::from_millis(10));
    }
    // Record the actual shell PID, so a replacement cannot satisfy the test.
    master.write_all(b"echo $$ > shell.pid\r").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let shell_file = session.root.join("shell.pid");
    while !shell_file.exists() {
        let _ = master.read(&mut buffer);
        assert!(Instant::now() < deadline, "shell did not execute input");
        thread::sleep(Duration::from_millis(10));
    }
    let shell_pid: i32 = fs::read_to_string(&shell_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    drop(master);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) =
            session.child.as_mut().unwrap().try_wait().unwrap()
        {
            assert!(
                status.success(),
                "client failed instead of detaching: {status}"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "client hung after PTY EOF without SIGHUP"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        unsafe { libc::kill(shell_pid, 0) },
        0,
        "original shell died"
    );
    let listed = session.command().arg("ls").output().unwrap();
    assert!(listed.status.success());
    assert!(
        String::from_utf8_lossy(&listed.stdout).contains("0:"),
        "session disappeared"
    );
}
