#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::*;
#[cfg(windows)]
pub use windows::*;

pub fn default_socket_name() -> &'static str {
    "default"
}

pub fn zmux_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
