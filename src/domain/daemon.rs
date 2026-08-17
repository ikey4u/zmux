use std::{io, thread, time::Duration};

use crate::{ipc::connect_client, platform::spawn_server_background};

pub fn server_reachable(socket_name: &str) -> bool {
    connect_client(socket_name).is_ok()
}

pub fn ensure_local_daemon(
    socket_name: &str,
    session_name: &str,
    start_dir: Option<&str>,
) -> io::Result<()> {
    if server_reachable(socket_name) {
        return Ok(());
    }

    #[cfg(unix)]
    if let Ok(path) = crate::ipc::socket_path(socket_name) {
        if path.exists() && !server_reachable(socket_name) {
            let _ = std::fs::remove_file(&path);
        }
    }

    let exe = std::env::current_exe()?;
    spawn_server_background(&exe, socket_name, session_name, start_dir)?;

    for _ in 0..100 {
        thread::sleep(Duration::from_millis(50));
        if server_reachable(socket_name) {
            return Ok(());
        }
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "server did not start within 5 seconds (socket: '{socket_name}')"
        ),
    ))
}
