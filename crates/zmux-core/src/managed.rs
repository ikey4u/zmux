//! Worker-owned multiplexer. Viewers never own PTYs or change another view's focus.
use std::{
    collections::BTreeMap,
    fs::File,
    io,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde::Serialize;

use crate::{server::InProcessServer, Environment};
#[derive(Clone, Serialize)]
pub struct PaneInfo {
    pub id: String,
    pub name: String,
    pub cwd: String,
    pub cols: u16,
    pub rows: u16,
    pub alive: bool,
    pub preview: String,
}
#[derive(Clone, Serialize)]
pub struct WindowInfo {
    pub id: String,
    pub name: String,
    pub panes: Vec<PaneInfo>,
}
#[derive(Clone, Serialize)]
pub struct SessionInfo {
    pub id: String,
    pub name: String,
    pub cwd: String,
    pub windows: Vec<WindowInfo>,
    pub alive: bool,
}
pub struct PaneOutput {
    pub cols: u16,
    pub rows: u16,
    pub data: Vec<u8>,
    pub offset: u64,
    pub snapshot: bool,
    pub closed: bool,
    pub history: String,
}
struct Hosted {
    host: Arc<InProcessServer>,
    name: String,
    cwd: String,
    shell: Option<String>,
}
pub struct ManagedRuntime {
    environment: Environment,
    _lock: File,
    sessions: Mutex<BTreeMap<String, Hosted>>,
}
fn err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}
fn missing() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        "terminal session ended or Worker restarted",
    )
}
impl ManagedRuntime {
    pub fn new(environment: Environment) -> io::Result<Self> {
        let lock = environment.server_lock("managed-runtime")?;
        Ok(Self {
            environment,
            _lock: lock,
            sessions: Mutex::new(BTreeMap::new()),
        })
    }
    pub fn list(&self) -> io::Result<Vec<SessionInfo>> {
        let sessions = self.sessions.lock().map_err(err)?;
        sessions
            .iter()
            .map(|(id, h)| {
                let windows = h.host.managed_windows()?;
                let alive =
                    windows.iter().flat_map(|w| &w.panes).any(|p| p.alive);
                Ok(SessionInfo {
                    id: id.clone(),
                    name: h.name.clone(),
                    cwd: h.cwd.clone(),
                    windows,
                    alive,
                })
            })
            .collect()
    }
    pub fn open(
        &self,
        cwd: &Path,
        name: &str,
        shell: Option<&str>,
        cols: u16,
        rows: u16,
    ) -> io::Result<SessionInfo> {
        if !cwd.is_absolute() || !cwd.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid working directory",
            ));
        }
        if name.len() > 128 || name.chars().any(char::is_control) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid session name",
            ));
        }
        let mut sessions = self.sessions.lock().map_err(err)?;
        if sessions.len() >= 32 {
            let ended = sessions.iter().find_map(|(id, h)| {
                h.host
                    .managed_windows()
                    .ok()
                    .filter(|ws| {
                        !ws.iter().flat_map(|w| &w.panes).any(|p| p.alive)
                    })
                    .map(|_| id.clone())
            });
            if let Some(id) = ended {
                sessions.remove(&id);
            } else {
                return Err(err("terminal session limit reached"));
            }
        }
        let id = crate::domain::new_instance_id();
        let cwd = cwd.to_string_lossy().into_owned();
        let host = Arc::new(InProcessServer::managed_start(
            &self.environment,
            &id,
            &cwd,
            shell,
            cols.clamp(2, 240),
            rows.clamp(2, 120),
        )?);
        let name = if name.trim().is_empty() {
            format!("Terminal {}", sessions.len() + 1)
        } else {
            name.trim().into()
        };
        let windows = host.managed_windows()?;
        sessions.insert(
            id.clone(),
            Hosted {
                host,
                name: name.clone(),
                cwd: cwd.clone(),
                shell: shell.map(str::to_owned),
            },
        );
        Ok(SessionInfo {
            id,
            name,
            cwd,
            windows,
            alive: true,
        })
    }
    fn host(&self, id: &str) -> io::Result<Arc<InProcessServer>> {
        self.sessions
            .lock()
            .map_err(err)?
            .get(id)
            .map(|h| h.host.clone())
            .ok_or_else(missing)
    }
    pub fn create(
        &self,
        id: &str,
        window: Option<usize>,
        pane: Option<usize>,
        name: &str,
    ) -> io::Result<(String, String)> {
        if name.trim().is_empty()
            || name.len() > 128
            || name.chars().any(char::is_control)
        {
            return Err(err("invalid terminal name"));
        }
        let (h, shell) = {
            let sessions = self.sessions.lock().map_err(err)?;
            let h = sessions.get(id).ok_or_else(missing)?;
            (h.host.clone(), h.shell.clone())
        };
        let windows = h.managed_windows()?;
        if windows.iter().map(|w| w.panes.len()).sum::<usize>() >= 32 {
            return Err(err("pane limit reached"));
        }
        let source = windows
            .iter()
            .flat_map(|w| &w.panes)
            .find(|p| pane.is_none() || p.id == pane.unwrap().to_string())
            .ok_or_else(missing)?;
        h.managed_create(
            window,
            pane,
            name,
            shell.as_deref(),
            &source.cwd,
            source.cols,
            source.rows,
        )
    }
    pub fn input(&self, id: &str, pane: usize, data: &[u8]) -> io::Result<()> {
        if data.len() > 64 * 1024 {
            return Err(err("terminal input exceeds 64 KiB"));
        }
        self.host(id)?.managed_input(pane, data)
    }
    pub fn resize(
        &self,
        id: &str,
        pane: usize,
        cols: u16,
        rows: u16,
    ) -> io::Result<()> {
        self.host(id)?.managed_resize(
            pane,
            cols.clamp(2, 240),
            rows.clamp(2, 120),
        )
    }
    pub fn close(
        &self,
        id: &str,
        window: Option<usize>,
        pane: Option<usize>,
    ) -> io::Result<()> {
        if let Some(w) = window {
            self.host(id)?.managed_close(w, pane)
        } else {
            let h = self
                .sessions
                .lock()
                .map_err(err)?
                .remove(id)
                .ok_or_else(missing)?;
            h.host.shutdown();
            Ok(())
        }
    }
    pub fn rename(
        &self,
        id: &str,
        window: Option<usize>,
        pane: Option<usize>,
        name: &str,
    ) -> io::Result<()> {
        if name.trim().is_empty()
            || name.len() > 128
            || name.chars().any(char::is_control)
        {
            return Err(err("invalid terminal name"));
        }
        if window.is_some() {
            self.host(id)?.managed_rename(window, pane, name)
        } else {
            self.sessions
                .lock()
                .map_err(err)?
                .get_mut(id)
                .ok_or_else(missing)?
                .name = name.into();
            Ok(())
        }
    }
    pub fn pull(
        &self,
        id: &str,
        pane: usize,
        after: Option<u64>,
        wait: Duration,
    ) -> io::Result<PaneOutput> {
        let host = self.host(id)?;
        let deadline = Instant::now() + wait.min(Duration::from_secs(20));
        loop {
            let version = self.environment.wait(0, Duration::ZERO);
            let output = host.managed_read(pane, after)?;
            if !output.data.is_empty()
                || output.snapshot
                || output.closed
                || Instant::now() >= deadline
            {
                return Ok(output);
            }
            self.environment.wait(
                version,
                deadline.saturating_duration_since(Instant::now()),
            );
        }
    }
}
