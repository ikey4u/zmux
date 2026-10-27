//! Embedded runtimes must preserve the host process's terminal and signal state.
#![cfg(target_os = "linux")]
use std::{
    fs,
    os::unix::process::{CommandExt, ExitStatusExt},
    process::Command,
    time::Duration,
};

use zmux_core::{managed::ManagedRuntime, Environment};

#[test]
fn session_leader_can_open_and_close_terminals_without_sighup() {
    const CHILD: &str = "ZMUX_SESSION_LEADER_TEST";
    if std::env::var_os(CHILD).is_none() {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "session_leader_can_open_and_close_terminals_without_sighup",
                "--nocapture",
            ])
            .env(CHILD, "1");
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let result = command.output().unwrap();
        assert!(
            result.status.success(),
            "PTY host exited: {:?}, signal={:?}\n{}\n{}",
            result.status,
            result.status.signal(),
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        return;
    }
    assert_eq!(unsafe { libc::getsid(0) }, unsafe { libc::getpid() });
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_DFL);
    }
    assert!(fs::OpenOptions::new().read(true).open("/dev/tty").is_err());
    let root = std::env::temp_dir()
        .join(format!("zmux-session-leader-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let environment = Environment::at(&root, "embedded").unwrap();
    let runtime = ManagedRuntime::new(environment).unwrap();
    for _ in 0..3 {
        let session = runtime
            .open(&root, "Empty test shell", Some("/bin/sh"), 80, 24)
            .unwrap();
        assert!(session.alive);
        assert!(
            fs::OpenOptions::new().read(true).open("/dev/tty").is_err(),
            "PTY slave became the host controlling terminal"
        );
        let pane = session.windows[0].panes[0].id.parse().unwrap();
        let output = runtime
            .pull(&session.id, pane, None, Duration::ZERO)
            .unwrap();
        assert!(!output.closed);
        runtime.close(&session.id, None, None).unwrap();
    }
    let mut disposition: libc::sigaction = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe {
            libc::sigaction(libc::SIGHUP, std::ptr::null(), &mut disposition)
        },
        0
    );
    assert_eq!(
        disposition.sa_sigaction,
        libc::SIG_DFL,
        "Do not hide the bug by ignoring SIGHUP globally"
    );
    drop(runtime);
    fs::remove_dir_all(root).unwrap();
}
