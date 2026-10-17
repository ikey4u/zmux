use std::{io, path::PathBuf};

pub fn spawn_server_background(
    exe: &PathBuf,
    socket_name: &str,
    session_name: &str,
    start_dir: Option<&str>,
) -> io::Result<()> {
    use std::{
        os::unix::process::CommandExt,
        process::{Command, Stdio},
    };
    crate::environment::validate_socket(socket_name)?;
    let environment = crate::environment::current();
    let mut command = Command::new(exe);
    command
        .arg("--environment-id")
        .arg(environment.id())
        .arg("--environment-root")
        .arg(environment.root())
        .arg("-L")
        .arg(socket_name)
        .arg("-s")
        .arg(session_name);
    if let Some(directory) = start_dir {
        command.arg("--directory").arg(directory);
    }
    command
        .arg("server")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Only async-signal-safe libc runs after fork. Constructing Rust strings or
    // allocating after a manual fork can deadlock a multithreaded UI process.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            let pid = libc::fork();
            if pid < 0 {
                return Err(io::Error::last_os_error());
            }
            if pid > 0 {
                libc::_exit(0)
            }
            // The daemon must not be a session leader: openpty's slave could
            // otherwise become its controlling terminal before the shell claims it.
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    let _ = child.wait()?;
    Ok(())
}

pub fn setup_signals() {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
}

pub fn kill_process_group(pid: u32) {
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGHUP);
    }
}
