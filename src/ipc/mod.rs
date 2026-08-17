use std::{io, time::Duration};

pub mod protocol;
#[cfg(unix)]
pub mod unix;
pub mod v2;
#[cfg(windows)]
pub mod windows;

pub use protocol::*;
#[cfg(unix)]
pub use unix::{
    bind_server, connect_client, socket_path, UnixListener, UnixStream,
};
#[cfg(windows)]
pub use windows::{
    bind_server, connect_client, pipe_name, PipeListener, PipeStream,
};

pub trait IpcRead: io::Read + Send + 'static {}
pub trait IpcWrite: io::Write + Send + 'static {}

impl<T: io::Read + Send + 'static> IpcRead for T {}
impl<T: io::Write + Send + 'static> IpcWrite for T {}

pub trait IpcStream: io::Read + io::Write + Send + 'static {
    fn try_clone(&self) -> io::Result<Self>
    where
        Self: Sized;

    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()>;
}

#[cfg(unix)]
impl IpcStream for std::os::unix::net::UnixStream {
    fn try_clone(&self) -> io::Result<Self> {
        std::os::unix::net::UnixStream::try_clone(self)
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        std::os::unix::net::UnixStream::set_read_timeout(self, timeout)
    }
}

#[cfg(windows)]
impl IpcStream for PipeStream {
    fn try_clone(&self) -> io::Result<Self> {
        PipeStream::try_clone(self)
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        PipeStream::set_read_timeout(self, timeout)
    }
}
