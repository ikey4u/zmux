use std::{
    fs, io,
    os::unix::{fs::MetadataExt, net},
    path::PathBuf,
};

pub type UnixListener = net::UnixListener;
pub type UnixStream = net::UnixStream;

pub fn socket_path(socket_name: &str) -> io::Result<PathBuf> {
    let uid = unsafe { libc::getuid() };
    let base = std::env::var("TMPDIR")
        .or_else(|_| std::env::var("XDG_RUNTIME_DIR"))
        .unwrap_or_else(|_| "/tmp".to_string());
    let dir = PathBuf::from(base).join(format!("zmux-{}", uid));
    fs::create_dir_all(&dir)?;
    let metadata = fs::metadata(&dir)?;
    if metadata.uid() != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "zmux runtime directory is owned by another user",
        ));
    }
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    Ok(dir.join(socket_name))
}

pub fn bind_server(socket_name: &str) -> io::Result<UnixListener> {
    let path = socket_path(socket_name)?;
    if path.exists() {
        if net::UnixStream::connect(&path).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "zmux server is already listening",
            ));
        }
        let metadata = fs::symlink_metadata(&path)?;
        let uid = unsafe { libc::getuid() };
        if metadata.uid() != uid {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "refusing to remove a socket owned by another user",
            ));
        }
        fs::remove_file(&path)?;
    }
    let listener = net::UnixListener::bind(&path)?;
    Ok(listener)
}

pub fn connect_client(socket_name: &str) -> io::Result<UnixStream> {
    let path = socket_path(socket_name)?;
    net::UnixStream::connect(&path)
}
