use std::{
    fs::{File, OpenOptions},
    io,
};

pub struct CloudLease {
    #[cfg(unix)]
    _file: nix::fcntl::Flock<File>,
}

#[cfg(unix)]
pub fn try_acquire(socket_name: &str) -> io::Result<CloudLease> {
    let path = crate::ipc::socket_path(&format!("{socket_name}.cloud-lease"))?;
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)?;
    match nix::fcntl::Flock::lock(
        file,
        nix::fcntl::FlockArg::LockExclusiveNonblock,
    ) {
        Ok(lock) => Ok(CloudLease { _file: lock }),
        Err(_) => Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "cloud lease is held by another interactive client",
        )),
    }
}

#[cfg(not(unix))]
pub fn try_acquire(_socket_name: &str) -> io::Result<CloudLease> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "cloud lease requires unix flock",
    ))
}

pub fn lease_held(socket_name: &str) -> bool {
    match try_acquire(socket_name) {
        Ok(_lease) => false,
        Err(err) if err.kind() == io::ErrorKind::WouldBlock => true,
        Err(_) => false,
    }
}
