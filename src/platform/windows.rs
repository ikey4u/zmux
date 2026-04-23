use std::{io, path::PathBuf};

pub fn spawn_server_background(
    exe: &PathBuf,
    socket_name: &str,
    session_name: &str,
) -> io::Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("server");
    cmd.arg("--socket");
    cmd.arg(socket_name);
    cmd.arg("--session");
    cmd.arg(session_name);
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    cmd.spawn()?;
    Ok(())
}

pub fn setup_signals() {}

pub fn kill_process_group(_pid: u32) {}
