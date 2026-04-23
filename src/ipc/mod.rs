use std::io;

pub mod protocol;
#[cfg(unix)]
pub mod unix;
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
