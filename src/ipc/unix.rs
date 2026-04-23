use std::{fs, io, os::unix::net, path::PathBuf};

pub type UnixListener = net::UnixListener;
pub type UnixStream = net::UnixStream;

pub fn socket_path(socket_name: &str) -> io::Result<PathBuf> {
    let uid = unsafe { libc::getuid() };
    let base = std::env::var("TMPDIR")
        .or_else(|_| std::env::var("XDG_RUNTIME_DIR"))
        .unwrap_or_else(|_| "/tmp".to_string());
    let dir = PathBuf::from(base).join(format!("zmux-{}", uid));
    fs::create_dir_all(&dir)?;
    Ok(dir.join(socket_name))
}

pub fn bind_server(socket_name: &str) -> io::Result<UnixListener> {
    let path = socket_path(socket_name)?;
    if path.exists() {
        fs::remove_file(&path)?;
    }
    let listener = net::UnixListener::bind(&path)?;
    Ok(listener)
}

pub fn connect_client(socket_name: &str) -> io::Result<UnixStream> {
    let path = socket_path(socket_name)?;
    net::UnixStream::connect(&path)
}
