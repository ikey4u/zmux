//! Typed pane operations never depend on another viewer's active selection.
use super::*;
use crate::{
    managed::{PaneInfo, PaneOutput, WindowInfo},
    output::pane_snapshot,
};
fn missing() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "terminal target no longer exists")
}
fn poisoned<T>(e: T) -> io::Error
where
    T: std::fmt::Display,
{
    io::Error::other(e.to_string())
}
impl InProcessServer {
    pub(crate) fn managed_start(
        environment: &crate::Environment,
        id: &str,
        cwd: &str,
        shell: Option<&str>,
        cols: u16,
        rows: u16,
    ) -> io::Result<Self> {
        environment.prepare()?;
        let mut state = Server::new();
        state.hide_borders = true;
        state.sessions.push(Session::new(0, id.into()));
        let host = Self {
            environment: environment.clone(),
            stopped: Arc::new(AtomicBool::new(false)),
            state: Arc::new(Mutex::new(state)),
            latest_frame: Arc::new(Mutex::new(FrameStore::default())),
            size: Arc::new(Mutex::new(Size::terminal(rows, cols))),
        };
        host.managed_create(None, None, "Shell", shell, cwd, cols, rows)?;
        Ok(host)
    }

    pub(crate) fn managed_windows(&self) -> io::Result<Vec<WindowInfo>> {
        self.environment.enter(|| {
            let s = self.state.lock().map_err(poisoned)?;
            Ok(s.sessions
                .first()
                .map(|session| {
                    session
                        .windows
                        .iter()
                        .map(|w| WindowInfo {
                            id: w.id.to_string(),
                            name: w.name.clone(),
                            panes: crate::layout::collect_pane_ids(&w.root)
                                .into_iter()
                                .filter_map(|id| {
                                    crate::layout::find_pane_by_id(&w.root, id)
                                })
                                .map(|p| PaneInfo {
                                    id: p.id.to_string(),
                                    name: if p.title.is_empty() {
                                        "Shell".into()
                                    } else {
                                        p.title.clone()
                                    },
                                    cwd: crate::pty::pane_current_dir(p)
                                        .unwrap_or_default(),
                                    cols: p.last_cols,
                                    rows: p.last_rows,
                                    alive: !p.dead.load(Ordering::Relaxed),
                                    preview: p
                                        .parser
                                        .lock()
                                        .ok()
                                        .map(|term| {
                                            let rows = term.visible_rows();
                                            let lines: Vec<String> = rows
                                                .iter()
                                                .map(|row| {
                                                    row.iter()
                                                        .flatten()
                                                        .map(|c| {
                                                            c.text.as_str()
                                                        })
                                                        .collect::<String>()
                                                        .trim_end()
                                                        .to_owned()
                                                })
                                                .collect();
                                            let end = lines
                                                .iter()
                                                .rposition(|l| !l.is_empty())
                                                .map(|i| i + 1)
                                                .unwrap_or(0);
                                            lines[end.saturating_sub(4)..end]
                                                .join("\n")
                                                .chars()
                                                .take(480)
                                                .collect()
                                        })
                                        .unwrap_or_default(),
                                })
                                .collect(),
                        })
                        .collect()
                })
                .unwrap_or_default())
        })
    }
    pub(crate) fn managed_create(
        &self,
        window: Option<usize>,
        pane: Option<usize>,
        name: &str,
        shell: Option<&str>,
        cwd: &str,
        cols: u16,
        rows: u16,
    ) -> io::Result<(String, String)> {
        self.environment.enter(|| {
            let mut s = self.state.lock().map_err(poisoned)?;
            let history_limit = s.options.history_limit;
            let scroll = s.options.scroll_on_erase_in_display;
            let session = s.sessions.first_mut().ok_or_else(missing)?;
            let pid = session.alloc_pane_id();
            let wid = if let Some(id) = window {
                id
            } else {
                session.alloc_window_id()
            };
            if let Some(id) = window {
                let w = session
                    .windows
                    .iter()
                    .find(|w| w.id == id)
                    .ok_or_else(missing)?;
                if !crate::layout::collect_pane_ids(&w.root)
                    .contains(&pane.ok_or_else(missing)?)
                {
                    return Err(missing());
                }
            }
            let p = spawn_pane(SpawnOptions {
                pane_id: pid,
                rows,
                cols,
                history_limit,
                command: shell,
                start_dir: Some(cwd),
                env: vec![],
                scroll_on_erase_in_display: scroll,
                zmux_socket: None,
            })?;
            if let Some(id) = window {
                let w = session
                    .windows
                    .iter_mut()
                    .find(|w| w.id == id)
                    .ok_or_else(missing)?;
                let path =
                    crate::layout::find_pane_path(&w.root, pane.unwrap())
                        .ok_or_else(missing)?;
                // Preserve zmux's real layout tree. Focused mobile views size each PTY independently.
                let old = std::mem::replace(
                    &mut w.root,
                    LayoutNode::Split {
                        direction: SplitDirection::Horizontal,
                        sizes: vec![],
                        children: vec![],
                    },
                );
                w.root = split_node(
                    old,
                    &path,
                    SplitDirection::Horizontal,
                    p,
                    false,
                );
            } else {
                session.windows.push(Window {
                    id: wid,
                    name: name.into(),
                    root: LayoutNode::Leaf(p),
                    active_pane_path: vec![],
                    options: WindowOptions::with_defaults(),
                    pane_mru: vec![pid],
                    zoom_state: None,
                    flags: Default::default(),
                    layout_index: 0,
                    last_output_time: Instant::now(),
                    last_seen_version: 0,
                    default_start_dir: Some(cwd.into()),
                });
            }
            mark_data_ready();
            Ok((wid.to_string(), pid.to_string()))
        })
    }
    pub(crate) fn managed_input(
        &self,
        pane: usize,
        data: &[u8],
    ) -> io::Result<()> {
        let writer = self.environment.enter(|| {
            let s = self.state.lock().map_err(poisoned)?;
            let p = s
                .sessions
                .first()
                .and_then(|x| {
                    x.windows.iter().find_map(|w| {
                        crate::layout::find_pane_by_id(&w.root, pane)
                    })
                })
                .ok_or_else(missing)?;
            if p.dead.load(Ordering::Relaxed) {
                return Err(missing());
            }
            Ok(p.writer.clone())
        })?;
        write_pty_writer(&writer, data)
    }
    pub(crate) fn managed_resize(
        &self,
        pane: usize,
        cols: u16,
        rows: u16,
    ) -> io::Result<()> {
        self.environment.enter(|| {
            let mut s = self.state.lock().map_err(poisoned)?;
            let p = s
                .sessions
                .first_mut()
                .and_then(|x| {
                    x.windows.iter_mut().find_map(|w| {
                        crate::layout::find_pane_by_id_mut(&mut w.root, pane)
                    })
                })
                .ok_or_else(missing)?;
            resize_pane(p, rows, cols)?;
            mark_data_ready();
            Ok(())
        })
    }
    pub(crate) fn managed_close(
        &self,
        window: usize,
        pane: Option<usize>,
    ) -> io::Result<()> {
        self.environment.enter(|| {
            let mut s = self.state.lock().map_err(poisoned)?;
            let session = s.sessions.first_mut().ok_or_else(missing)?;
            let index = session
                .windows
                .iter()
                .position(|w| w.id == window)
                .ok_or_else(missing)?;
            if let Some(id) = pane {
                let w = &mut session.windows[index];
                let path = crate::layout::find_pane_path(&w.root, id)
                    .ok_or_else(missing)?;
                if path.is_empty() {
                    session.windows.remove(index);
                } else {
                    remove_pane_from_window(w, &path);
                }
            } else {
                session.windows.remove(index);
            }
            session.active_window_idx = session
                .active_window_idx
                .min(session.windows.len().saturating_sub(1));
            mark_data_ready();
            Ok(())
        })
    }
    pub(crate) fn managed_rename(
        &self,
        window: Option<usize>,
        pane: Option<usize>,
        name: &str,
    ) -> io::Result<()> {
        self.environment.enter(|| {
            let mut s = self.state.lock().map_err(poisoned)?;
            let session = s.sessions.first_mut().ok_or_else(missing)?;
            let w = session
                .windows
                .iter_mut()
                .find(|w| Some(w.id) == window)
                .ok_or_else(missing)?;
            if let Some(id) = pane {
                let p = crate::layout::find_pane_by_id_mut(&mut w.root, id)
                    .ok_or_else(missing)?;
                p.title = name.into();
                p.title_locked = true
            } else {
                w.name = name.into()
            }
            Ok(())
        })
    }
    pub(crate) fn managed_read(
        &self,
        pane: usize,
        after: Option<u64>,
    ) -> io::Result<PaneOutput> {
        self.environment.enter(|| {
            let s = self.state.lock().map_err(poisoned)?;
            let p = s
                .sessions
                .first()
                .and_then(|x| {
                    x.windows.iter().find_map(|w| {
                        crate::layout::find_pane_by_id(&w.root, pane)
                    })
                })
                .ok_or_else(missing)?;
            let _serial = p.history_serial.lock().map_err(poisoned)?;
            let journal = p.stream.lock().map_err(poisoned)?;
            if let Some(after) = after {
                if let Some(data) = journal.read(after) {
                    return Ok(PaneOutput {
                        cols: p.last_cols,
                        rows: p.last_rows,
                        offset: after + data.len() as u64,
                        data,
                        snapshot: false,
                        closed: p.dead.load(Ordering::Relaxed),
                        history: String::new(),
                    });
                }
            }
            let (data, history) = pane_snapshot(p);
            Ok(PaneOutput {
                cols: p.last_cols,
                rows: p.last_rows,
                offset: journal.end,
                data: data.into_bytes(),
                snapshot: true,
                closed: p.dead.load(Ordering::Relaxed),
                history,
            })
        })
    }
}
