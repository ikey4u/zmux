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

pub const ZMUX_VERSION: &str = env!("ZMUX_VERSION");

pub fn zmux_version() -> &'static str {
    ZMUX_VERSION
}
