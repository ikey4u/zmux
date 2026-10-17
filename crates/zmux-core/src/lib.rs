#[cfg(feature = "tui")]
pub mod client;
pub mod commands;
pub mod config;
pub mod copy_mode;
pub mod domain;
pub mod frame;
pub mod history_store;
pub mod ipc;
pub mod layout;
pub mod output;
pub mod platform;
pub mod pty;
pub(crate) mod screen_trace;
pub mod server;
pub mod style;
pub mod terminal;
pub mod types;

pub mod environment;
pub mod runtime;
pub use environment::Environment;
pub use runtime::TerminalRuntime;

mod journal;
pub mod managed;
