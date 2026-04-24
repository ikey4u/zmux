use std::{
    io::{self, Write},
    time::{Duration, Instant},
};

use arboard::Clipboard;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use crossterm::{
    cursor::{self, SetCursorStyle},
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste,
        EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, KeyboardEnhancementFlags, ModifierKeyCode, MouseButton,
        MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};

mod render;
mod socket;
pub use render::*;
pub use socket::SocketClient;

use crate::{
    server::SessionTreeEntry,
    types::{session::Size, SelectionMode},
};

pub struct ClientApp {
    pub socket_name: String,
    pub session_name: Option<String>,
}

#[derive(Clone, PartialEq)]
enum InputMode {
    Normal,
    Prefix,
    Resize,
    CopyMode,
    CopySearch {
        buf: String,
        cursor: usize,
        forward: bool,
    },
    RenameWindow {
        buf: String,
        cursor: usize,
    },
    RenameSession {
        buf: String,
        cursor: usize,
    },
    Command {
        buf: String,
        cursor: usize,
    },
    SessionChooser {
        entries: Vec<SessionTreeEntry>,
        selected: usize,
        collapsed: std::collections::HashSet<String>,
        collapsed_windows: std::collections::HashSet<(String, usize)>,
    },
}

const RESIZE_IDLE_TIMEOUT: Duration = Duration::from_millis(500);
const SCROLL_LINES: usize = 3;

impl ClientApp {
    pub fn new(socket_name: &str, session_name: Option<String>) -> Self {
        Self {
            socket_name: socket_name.to_string(),
            session_name,
        }
    }

    pub fn run(&self) -> io::Result<()> {
        let (cols, rows) = terminal::size().unwrap_or((80, 24));
        let size = Size::new(rows, cols);
        let session_name =
            self.session_name.clone().unwrap_or_else(|| "0".to_string());

        #[cfg(unix)]
        crate::pty::remember_host_termios();

        let server =
            ensure_server_and_connect(&self.socket_name, &session_name, size)?;

        terminal::enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            cursor::Hide,
            EnableBracketedPaste,
            EnableMouseCapture,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES,
            )
        )?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        let mut mouse_select: Option<MouseSelection> = None;

        let prefix_key = (KeyCode::Char('a'), KeyModifiers::CONTROL);
        let mut mode = InputMode::Normal;
        let mut copy_mode_confirmed = false;
        let mut resize_deadline: Option<Instant> = None;
        let mut status_notice: Option<(String, Instant)> = None;
        let mut hide_borders = false;
        let mut applied_cursor_style: Option<SetCursorStyle> = None;

        let run_result: io::Result<()> = (|| {
            loop {
                let frame = server.latest_frame();
                if let Some(ref fd) = frame {
                    if fd.exit {
                        log_client("received exit frame, breaking main loop");
                        break;
                    }
                    if mode == InputMode::CopyMode {
                        if active_in_copy_mode(fd) {
                            copy_mode_confirmed = true;
                        } else if copy_mode_confirmed {
                            mode = InputMode::Normal;
                            copy_mode_confirmed = false;
                        }
                    }
                }
                let desired_cursor_style = cursor_style_for_shape(
                    frame.as_ref().and_then(active_cursor_shape),
                );
                if applied_cursor_style != Some(desired_cursor_style) {
                    execute!(terminal.backend_mut(), desired_cursor_style)?;
                    applied_cursor_style = Some(desired_cursor_style);
                }
                let now = Instant::now();
                if mode == InputMode::Resize
                    && matches!(resize_deadline, Some(expires_at) if now >= expires_at)
                {
                    mode = InputMode::Normal;
                    resize_deadline = None;
                }
                if mode != InputMode::Resize {
                    resize_deadline = None;
                }
                if matches!(status_notice.as_ref(), Some((_, expires_at)) if now >= *expires_at)
                {
                    status_notice = None;
                }
                let status_notice_text =
                    status_notice.as_ref().map(|(text, _)| text.clone());
                let status_banner = status_banner_for_mode(
                    &mode,
                    status_notice_text.as_deref(),
                );
                let has_prompt = matches!(
                    mode,
                    InputMode::CopySearch { .. }
                        | InputMode::RenameWindow { .. }
                        | InputMode::RenameSession { .. }
                        | InputMode::Command { .. }
                );
                let chooser_entries = if let InputMode::SessionChooser {
                    ref entries,
                    selected,
                    ref collapsed,
                    ref collapsed_windows,
                } = mode
                {
                    Some((
                        entries.clone(),
                        selected,
                        collapsed.clone(),
                        collapsed_windows.clone(),
                    ))
                } else {
                    None
                };

                terminal.draw(|f| {
                    let in_prefix = mode == InputMode::Prefix;
                    if let Some(ref fd) = frame {
                        let mut display_frame = fd.clone();
                        if let Some(ref message) = status_banner {
                            if let Some(status) = display_frame.status.as_mut()
                            {
                                status.right = message.clone();
                            }
                        }
                        render_frame_ex(
                            f,
                            &display_frame,
                            in_prefix,
                            has_prompt || chooser_entries.is_some(),
                            hide_borders,
                        );
                    } else {
                        render_loading(f);
                    }
                    match &mode {
                        InputMode::CopySearch { buf, forward, .. } => {
                            render_prompt(
                                f,
                                if *forward { "/" } else { "?" },
                                buf,
                            )
                        }
                        InputMode::RenameWindow { buf, .. } => {
                            render_prompt(f, "Rename window: ", buf)
                        }
                        InputMode::RenameSession { buf, .. } => {
                            render_prompt(f, "Rename session: ", buf)
                        }
                        InputMode::Command { buf, .. } => {
                            render_prompt(f, ":", buf)
                        }
                        InputMode::SessionChooser {
                            entries,
                            selected,
                            collapsed,
                            collapsed_windows,
                        } => render_session_chooser(
                            f,
                            entries,
                            *selected,
                            collapsed,
                            collapsed_windows,
                        ),
                        _ => {}
                    }
                    if let Some(ref sel) = mouse_select {
                        if let Some(ref fd) = frame {
                            render_mouse_selection(f, sel, fd, hide_borders);
                        }
                    }
                })?;

                if event::poll(Duration::from_millis(8))? {
                    match event::read()? {
                        Event::Key(key)
                            if key.kind == KeyEventKind::Press
                                || key.kind == KeyEventKind::Repeat =>
                        {
                            match mode.clone() {
                                InputMode::Normal => {
                                    if (key.code, key.modifiers) == prefix_key {
                                        mode = InputMode::Prefix;
                                    } else {
                                        if frame
                                            .as_ref()
                                            .map_or(false, active_in_copy_mode)
                                        {
                                            server.exit_copy_mode();
                                        }
                                        let bytes = key_to_bytes(key);
                                        if !bytes.is_empty() {
                                            server.send_input(&bytes);
                                        }
                                    }
                                }

                                InputMode::Prefix => {
                                    mode = InputMode::Normal;
                                    if (key.code, key.modifiers) == prefix_key {
                                        let bytes = key_to_bytes(key);
                                        if !bytes.is_empty() {
                                            server.send_input(&bytes);
                                        }
                                        continue;
                                    }
                                    if is_resize_modifier_key(key) {
                                        mode = InputMode::Resize;
                                        resize_deadline = Some(
                                            Instant::now()
                                                + RESIZE_IDLE_TIMEOUT,
                                        );
                                        continue;
                                    }
                                    if let Some(cmd) =
                                        resize_command_for_key(key)
                                    {
                                        server.run_command(cmd);
                                        mode = InputMode::Resize;
                                        resize_deadline = Some(
                                            Instant::now()
                                                + RESIZE_IDLE_TIMEOUT,
                                        );
                                        continue;
                                    }
                                    match (key.code, key.modifiers) {
                                        (
                                            KeyCode::Char('d'),
                                            KeyModifiers::NONE,
                                        ) => {
                                            server.detach();
                                            break;
                                        }
                                        (KeyCode::Char(','), _) => {
                                            let cur =
                                                server.active_window_name();
                                            let len = cur.len();
                                            mode = InputMode::RenameWindow {
                                                buf: cur,
                                                cursor: len,
                                            };
                                        }
                                        (KeyCode::Char('$'), _) => {
                                            let cur = server.session_name();
                                            let len = cur.len();
                                            mode = InputMode::RenameSession {
                                                buf: cur,
                                                cursor: len,
                                            };
                                        }
                                        (KeyCode::Char(':'), _) => {
                                            mode = InputMode::Command {
                                                buf: String::new(),
                                                cursor: 0,
                                            };
                                        }
                                        (KeyCode::Char('['), _) => {
                                            if server.enter_copy_mode() {
                                                mode = InputMode::CopyMode;
                                                copy_mode_confirmed = false;
                                            } else {
                                                status_notice = Some((
                                                    "copy mode unavailable"
                                                        .to_string(),
                                                    Instant::now()
                                                        + Duration::from_secs(
                                                            3,
                                                        ),
                                                ));
                                            }
                                        }
                                        (
                                            KeyCode::Char('s'),
                                            KeyModifiers::NONE,
                                        ) => {
                                            let entries = server.session_tree();
                                            // 默认折叠所有 session，只展开当前活动 session
                                            let mut collapsed: std::collections::HashSet<String> =
                                                entries.iter().filter_map(|e| match e {
                                                    SessionTreeEntry::Session { name, .. } => Some(name.clone()),
                                                    _ => None,
                                                }).collect();
                                            // 展开当前活动 session
                                            for e in &entries {
                                                if let SessionTreeEntry::Session { name, is_active: true, .. } = e {
                                                    collapsed.remove(name);
                                                }
                                            }
                                            let sel = {
                                                let vis: Vec<_> = entries.iter().filter(|e| match e {
                                                    SessionTreeEntry::Session { .. } => true,
                                                    SessionTreeEntry::Window { session_name, .. } => !collapsed.contains(session_name),
                                                    SessionTreeEntry::Pane { .. } => false,
                                                }).collect();
                                                vis.iter().position(|e| matches!(e,
                                                    SessionTreeEntry::Session { is_active: true, .. }
                                                )).unwrap_or(0)
                                            };
                                            mode = InputMode::SessionChooser {
                                                entries,
                                                selected: sel,
                                                collapsed,
                                                collapsed_windows: std::collections::HashSet::new(),
                                            };
                                        }
                                        (KeyCode::Char('('), _) => {
                                            server.run_command("prev-session");
                                        }
                                        (KeyCode::Char(')'), _) => {
                                            server.run_command("next-session");
                                        }
                                        (
                                            KeyCode::Char('b'),
                                            KeyModifiers::NONE,
                                        ) => {
                                            hide_borders = !hide_borders;
                                            server
                                                .set_hide_borders(hide_borders);
                                        }
                                        _ => {
                                            if let Some(message) =
                                                handle_prefix_key(&server, key)
                                            {
                                                status_notice = Some((
                                                    message,
                                                    Instant::now()
                                                        + Duration::from_secs(
                                                            3,
                                                        ),
                                                ));
                                            }
                                        }
                                    }
                                }

                                InputMode::Resize => {
                                    if is_resize_modifier_key(key) {
                                        resize_deadline = Some(
                                            Instant::now()
                                                + RESIZE_IDLE_TIMEOUT,
                                        );
                                        continue;
                                    }
                                    if let Some(cmd) =
                                        resize_command_for_key(key)
                                    {
                                        server.run_command(cmd);
                                        resize_deadline = Some(
                                            Instant::now()
                                                + RESIZE_IDLE_TIMEOUT,
                                        );
                                        continue;
                                    }
                                    if key.code == KeyCode::Esc {
                                        mode = InputMode::Normal;
                                        resize_deadline = None;
                                        continue;
                                    }
                                    if (key.code, key.modifiers) == prefix_key {
                                        mode = InputMode::Prefix;
                                        resize_deadline = None;
                                        continue;
                                    }
                                    mode = InputMode::Normal;
                                    resize_deadline = None;
                                }

                                InputMode::Command {
                                    mut buf,
                                    mut cursor,
                                } => match key.code {
                                    KeyCode::Enter => {
                                        let trimmed = buf.trim().to_string();
                                        mode = InputMode::Normal;
                                        if !trimmed.is_empty() {
                                            if let Some(message) =
                                                run_command_notice(
                                                    &server, &trimmed,
                                                )
                                            {
                                                status_notice = Some((
                                                    message,
                                                    Instant::now()
                                                        + Duration::from_secs(
                                                            3,
                                                        ),
                                                ));
                                            }
                                        }
                                    }
                                    KeyCode::Esc => {
                                        mode = InputMode::Normal;
                                    }
                                    KeyCode::Backspace => {
                                        if cursor > 0 {
                                            let bp =
                                                char_byte_pos(&buf, cursor - 1);
                                            let ep =
                                                char_byte_pos(&buf, cursor);
                                            buf.drain(bp..ep);
                                            cursor -= 1;
                                        }
                                        mode =
                                            InputMode::Command { buf, cursor };
                                    }
                                    KeyCode::Left => {
                                        if cursor > 0 {
                                            cursor -= 1;
                                        }
                                        mode =
                                            InputMode::Command { buf, cursor };
                                    }
                                    KeyCode::Right => {
                                        let m = buf.chars().count();
                                        if cursor < m {
                                            cursor += 1;
                                        }
                                        mode =
                                            InputMode::Command { buf, cursor };
                                    }
                                    KeyCode::Char(c)
                                        if key.modifiers
                                            == KeyModifiers::NONE
                                            || key.modifiers
                                                == KeyModifiers::SHIFT =>
                                    {
                                        let bp = char_byte_pos(&buf, cursor);
                                        buf.insert(bp, c);
                                        cursor += 1;
                                        mode =
                                            InputMode::Command { buf, cursor };
                                    }
                                    _ => {
                                        mode =
                                            InputMode::Command { buf, cursor };
                                    }
                                },

                                InputMode::CopyMode => {
                                    match (key.code, key.modifiers) {
                                        (KeyCode::Esc, _)
                                        | (
                                            KeyCode::Char('q'),
                                            KeyModifiers::NONE,
                                        ) => {
                                            server.exit_copy_mode();
                                            mode = InputMode::Normal;
                                            copy_mode_confirmed = false;
                                        }
                                        (KeyCode::Char('/'), _) => {
                                            mode = InputMode::CopySearch {
                                                buf: String::new(),
                                                cursor: 0,
                                                forward: true,
                                            };
                                        }
                                        (KeyCode::Char('?'), _) => {
                                            mode = InputMode::CopySearch {
                                                buf: String::new(),
                                                cursor: 0,
                                                forward: false,
                                            };
                                        }
                                        (KeyCode::Char('h'), _)
                                        | (KeyCode::Left, _) => {
                                            server.copy_move_left();
                                            mode = InputMode::CopyMode;
                                        }
                                        (KeyCode::Char('l'), _)
                                        | (KeyCode::Right, _) => {
                                            server.copy_move_right();
                                            mode = InputMode::CopyMode;
                                        }
                                        (KeyCode::Char('k'), _)
                                        | (KeyCode::Up, _) => {
                                            server.copy_move_up();
                                            mode = InputMode::CopyMode;
                                        }
                                        (KeyCode::Char('j'), _)
                                        | (KeyCode::Down, _) => {
                                            server.copy_move_down();
                                            mode = InputMode::CopyMode;
                                        }
                                        (
                                            KeyCode::Char('b'),
                                            KeyModifiers::NONE,
                                        ) => {
                                            server.copy_move_word_backward();
                                            mode = InputMode::CopyMode;
                                        }
                                        (
                                            KeyCode::Char('w'),
                                            KeyModifiers::NONE,
                                        ) => {
                                            server.copy_move_word_forward();
                                            mode = InputMode::CopyMode;
                                        }
                                        (
                                            KeyCode::Char('e'),
                                            KeyModifiers::NONE,
                                        ) => {
                                            server.copy_move_word_end();
                                            mode = InputMode::CopyMode;
                                        }
                                        (KeyCode::PageUp, _)
                                        | (
                                            KeyCode::Char('b'),
                                            KeyModifiers::CONTROL,
                                        ) => {
                                            server.copy_page_up();
                                            mode = InputMode::CopyMode;
                                        }
                                        (KeyCode::PageDown, _)
                                        | (
                                            KeyCode::Char('f'),
                                            KeyModifiers::CONTROL,
                                        ) => {
                                            server.copy_page_down();
                                            mode = InputMode::CopyMode;
                                        }
                                        (
                                            KeyCode::Char('g'),
                                            KeyModifiers::NONE,
                                        ) => {
                                            server.copy_move_to_top();
                                            mode = InputMode::CopyMode;
                                        }
                                        (KeyCode::Char('G'), _) => {
                                            server.copy_move_to_bottom();
                                            mode = InputMode::CopyMode;
                                        }
                                        _ if is_copy_line_start_key(key) => {
                                            server.copy_move_to_line_start();
                                            mode = InputMode::CopyMode;
                                        }
                                        _ if is_copy_line_end_key(key) => {
                                            server.copy_move_to_line_end();
                                            mode = InputMode::CopyMode;
                                        }
                                        (
                                            KeyCode::Char('v'),
                                            KeyModifiers::NONE,
                                        )
                                        | (
                                            KeyCode::Char(' '),
                                            KeyModifiers::NONE,
                                        ) => {
                                            server.copy_start_selection(
                                                SelectionMode::Char,
                                            );
                                            mode = InputMode::CopyMode;
                                        }
                                        (KeyCode::Char('V'), _) => {
                                            server.copy_start_selection(
                                                SelectionMode::Line,
                                            );
                                            mode = InputMode::CopyMode;
                                        }
                                        (
                                            KeyCode::Char('v'),
                                            KeyModifiers::CONTROL,
                                        ) => {
                                            server.copy_start_selection(
                                                SelectionMode::Rect,
                                            );
                                            mode = InputMode::CopyMode;
                                        }
                                        (
                                            KeyCode::Char('n'),
                                            KeyModifiers::NONE,
                                        ) => {
                                            server.copy_search_next();
                                            mode = InputMode::CopyMode;
                                        }
                                        (KeyCode::Char('N'), _) => {
                                            server.copy_search_prev();
                                            mode = InputMode::CopyMode;
                                        }
                                        (
                                            KeyCode::Char('y'),
                                            KeyModifiers::NONE,
                                        )
                                        | (KeyCode::Enter, _) => {
                                            let text =
                                                server.copy_yank_selection();
                                            if text.is_empty() {
                                                status_notice = Some((
                                                    "no selection".to_string(),
                                                    Instant::now()
                                                        + Duration::from_secs(
                                                            3,
                                                        ),
                                                ));
                                                mode = InputMode::CopyMode;
                                            } else {
                                                server.exit_copy_mode();
                                                mode = InputMode::Normal;
                                                copy_mode_confirmed = false;
                                                let copy_result =
                                                    copy_to_clipboard(&text);
                                                status_notice = Some((
                                                    match copy_result {
                                                        ClipboardCopyResult::System => format!(
                                                            "copied {} chars",
                                                            text.chars()
                                                                .count()
                                                        ),
                                                        ClipboardCopyResult::Osc52 => format!(
                                                            "copied {} chars via OSC 52",
                                                            text.chars()
                                                                .count()
                                                        ),
                                                        ClipboardCopyResult::Unavailable => format!(
                                                            "yanked {} chars",
                                                            text.chars()
                                                                .count()
                                                        ),
                                                    },
                                                    Instant::now()
                                                        + Duration::from_secs(
                                                            3,
                                                        ),
                                                ));
                                            }
                                        }
                                        _ => {
                                            mode = InputMode::CopyMode;
                                        }
                                    }
                                }

                                InputMode::CopySearch {
                                    mut buf,
                                    mut cursor,
                                    forward,
                                } => {
                                    match key.code {
                                        KeyCode::Enter => {
                                            if !buf.is_empty() {
                                                let found = server.copy_search(
                                                    buf.clone(),
                                                    forward,
                                                );
                                                if !found {
                                                    status_notice = Some((
                                                    format!("not found: {}", buf),
                                                    Instant::now()
                                                        + Duration::from_secs(3),
                                                ));
                                                }
                                            }
                                            mode = InputMode::CopyMode;
                                        }
                                        KeyCode::Esc => {
                                            mode = InputMode::CopyMode;
                                        }
                                        KeyCode::Backspace => {
                                            if cursor > 0 {
                                                let bp = char_byte_pos(
                                                    &buf,
                                                    cursor - 1,
                                                );
                                                let ep =
                                                    char_byte_pos(&buf, cursor);
                                                buf.drain(bp..ep);
                                                cursor -= 1;
                                            }
                                            mode = InputMode::CopySearch {
                                                buf,
                                                cursor,
                                                forward,
                                            };
                                        }
                                        KeyCode::Left => {
                                            if cursor > 0 {
                                                cursor -= 1;
                                            }
                                            mode = InputMode::CopySearch {
                                                buf,
                                                cursor,
                                                forward,
                                            };
                                        }
                                        KeyCode::Right => {
                                            let m = buf.chars().count();
                                            if cursor < m {
                                                cursor += 1;
                                            }
                                            mode = InputMode::CopySearch {
                                                buf,
                                                cursor,
                                                forward,
                                            };
                                        }
                                        KeyCode::Char(c)
                                            if key.modifiers
                                                == KeyModifiers::NONE
                                                || key.modifiers
                                                    == KeyModifiers::SHIFT =>
                                        {
                                            let bp =
                                                char_byte_pos(&buf, cursor);
                                            buf.insert(bp, c);
                                            cursor += 1;
                                            mode = InputMode::CopySearch {
                                                buf,
                                                cursor,
                                                forward,
                                            };
                                        }
                                        _ => {
                                            mode = InputMode::CopySearch {
                                                buf,
                                                cursor,
                                                forward,
                                            };
                                        }
                                    }
                                }

                                InputMode::SessionChooser {
                                    entries,
                                    mut selected,
                                    mut collapsed,
                                    mut collapsed_windows,
                                } => {
                                    let visible = visible_entries_full(
                                        &entries,
                                        &collapsed,
                                        &collapsed_windows,
                                    );
                                    match key.code {
                                        KeyCode::Esc | KeyCode::Char('q') => {
                                            mode = InputMode::Normal;
                                        }
                                        KeyCode::Up | KeyCode::Char('k') => {
                                            if selected > 0 {
                                                selected -= 1;
                                            }
                                            mode = InputMode::SessionChooser {
                                                entries,
                                                selected,
                                                collapsed,
                                                collapsed_windows,
                                            };
                                        }
                                        KeyCode::Down | KeyCode::Char('j') => {
                                            if selected + 1 < visible.len() {
                                                selected += 1;
                                            }
                                            mode = InputMode::SessionChooser {
                                                entries,
                                                selected,
                                                collapsed,
                                                collapsed_windows,
                                            };
                                        }
                                        KeyCode::Char('l') => {
                                            let new_sel = if let Some(entry) =
                                                visible.get(selected)
                                            {
                                                match entry {
                                                    SessionTreeEntry::Session { name, .. } => {
                                                        if collapsed.contains(name) {
                                                            collapsed.remove(name);
                                                            selected
                                                        } else {
                                                            // 已展开 → 跳到第一个 window
                                                            let n = name.clone();
                                                            let v2 = visible_entries_full(&entries, &collapsed, &collapsed_windows);
                                                            v2.iter().position(|e| matches!(e,
                                                                SessionTreeEntry::Window { session_name, index, .. }
                                                                if *session_name == n && *index == 0
                                                            )).unwrap_or(selected)
                                                        }
                                                    }
                                                    SessionTreeEntry::Window { session_name, index, .. } => {
                                                        let key_w = (session_name.clone(), *index);
                                                        if collapsed_windows.contains(&key_w) {
                                                            collapsed_windows.remove(&key_w);
                                                            selected
                                                        } else {
                                                            // 已展开 → 跳到第一个 pane
                                                            let sn = session_name.clone();
                                                            let wi = *index;
                                                            let v2 = visible_entries_full(&entries, &collapsed, &collapsed_windows);
                                                            v2.iter().position(|e| matches!(e,
                                                                SessionTreeEntry::Pane { session_name, window_index, index, .. }
                                                                if *session_name == sn && *window_index == wi && *index == 0
                                                            )).unwrap_or(selected)
                                                        }
                                                    }
                                                    SessionTreeEntry::Pane { .. } => selected,
                                                }
                                            } else {
                                                selected
                                            };
                                            mode = InputMode::SessionChooser {
                                                entries,
                                                selected: new_sel,
                                                collapsed,
                                                collapsed_windows,
                                            };
                                        }
                                        KeyCode::Char('h') => {
                                            if let Some(entry) =
                                                visible.get(selected)
                                            {
                                                match entry {
                                                    SessionTreeEntry::Session { name, .. } => {
                                                        collapsed.insert(name.clone());
                                                    }
                                                    SessionTreeEntry::Window { session_name, index, .. } => {
                                                        let key_w = (session_name.clone(), *index);
                                                        if collapsed_windows.contains(&key_w) {
                                                            // 已折叠 → 跳回父 session 并折叠 session
                                                            collapsed.insert(session_name.clone());
                                                            let sn = session_name.clone();
                                                            let v2 = visible_entries_full(&entries, &collapsed, &collapsed_windows);
                                                            selected = v2.iter().position(|e| matches!(e,
                                                                SessionTreeEntry::Session { name, .. } if *name == sn
                                                            )).unwrap_or(0);
                                                        } else {
                                                            // 展开 → 折叠 window
                                                            collapsed_windows.insert(key_w);
                                                        }
                                                    }
                                                    SessionTreeEntry::Pane { session_name, window_index, .. } => {
                                                        // 跳回父 window 行
                                                        let sn = session_name.clone();
                                                        let wi = *window_index;
                                                        let v2 = visible_entries_full(&entries, &collapsed, &collapsed_windows);
                                                        selected = v2.iter().position(|e| matches!(e,
                                                            SessionTreeEntry::Window { session_name, index, .. }
                                                            if *session_name == sn && *index == wi
                                                        )).unwrap_or(selected);
                                                    }
                                                }
                                            }
                                            mode = InputMode::SessionChooser {
                                                entries,
                                                selected,
                                                collapsed,
                                                collapsed_windows,
                                            };
                                        }
                                        KeyCode::Enter => {
                                            if let Some(entry) =
                                                visible.get(selected)
                                            {
                                                let cmd = match entry {
                                                    SessionTreeEntry::Session { name, .. } =>
                                                        format!("switch-client -t {}", name),
                                                    SessionTreeEntry::Window { session_name, index, .. } =>
                                                        format!("switch-client -t {}; select-window -t {}", session_name, index),
                                                    SessionTreeEntry::Pane { session_name, window_index, pane_id, .. } =>
                                                        format!("switch-client -t {}; select-window -t {}; select-pane -t %{}", session_name, window_index, pane_id),
                                                };
                                                server.run_command(&cmd);
                                            }
                                            mode = InputMode::Normal;
                                        }
                                        _ => {
                                            mode = InputMode::SessionChooser {
                                                entries,
                                                selected,
                                                collapsed,
                                                collapsed_windows,
                                            };
                                        }
                                    }
                                }

                                InputMode::RenameWindow {
                                    mut buf,
                                    mut cursor,
                                } => match key.code {
                                    KeyCode::Enter => {
                                        if !buf.is_empty() {
                                            server.run_command(&format!(
                                                "rename-window {}",
                                                shell_quote(&buf)
                                            ));
                                        }
                                        mode = InputMode::Normal;
                                    }
                                    KeyCode::Esc => {
                                        mode = InputMode::Normal;
                                    }
                                    KeyCode::Backspace => {
                                        if cursor > 0 {
                                            let bp =
                                                char_byte_pos(&buf, cursor - 1);
                                            let ep =
                                                char_byte_pos(&buf, cursor);
                                            buf.drain(bp..ep);
                                            cursor -= 1;
                                        }
                                        mode = InputMode::RenameWindow {
                                            buf,
                                            cursor,
                                        };
                                    }
                                    KeyCode::Left => {
                                        if cursor > 0 {
                                            cursor -= 1;
                                        }
                                        mode = InputMode::RenameWindow {
                                            buf,
                                            cursor,
                                        };
                                    }
                                    KeyCode::Right => {
                                        let m = buf.chars().count();
                                        if cursor < m {
                                            cursor += 1;
                                        }
                                        mode = InputMode::RenameWindow {
                                            buf,
                                            cursor,
                                        };
                                    }
                                    KeyCode::Char(c)
                                        if key.modifiers
                                            == KeyModifiers::NONE
                                            || key.modifiers
                                                == KeyModifiers::SHIFT =>
                                    {
                                        let bp = char_byte_pos(&buf, cursor);
                                        buf.insert(bp, c);
                                        cursor += 1;
                                        mode = InputMode::RenameWindow {
                                            buf,
                                            cursor,
                                        };
                                    }
                                    _ => {
                                        mode = InputMode::RenameWindow {
                                            buf,
                                            cursor,
                                        };
                                    }
                                },

                                InputMode::RenameSession {
                                    mut buf,
                                    mut cursor,
                                } => match key.code {
                                    KeyCode::Enter => {
                                        if !buf.is_empty() {
                                            server.run_command(&format!(
                                                "rename-session {}",
                                                shell_quote(&buf)
                                            ));
                                        }
                                        mode = InputMode::Normal;
                                    }
                                    KeyCode::Esc => {
                                        mode = InputMode::Normal;
                                    }
                                    KeyCode::Backspace => {
                                        if cursor > 0 {
                                            let bp =
                                                char_byte_pos(&buf, cursor - 1);
                                            let ep =
                                                char_byte_pos(&buf, cursor);
                                            buf.drain(bp..ep);
                                            cursor -= 1;
                                        }
                                        mode = InputMode::RenameSession {
                                            buf,
                                            cursor,
                                        };
                                    }
                                    KeyCode::Left => {
                                        if cursor > 0 {
                                            cursor -= 1;
                                        }
                                        mode = InputMode::RenameSession {
                                            buf,
                                            cursor,
                                        };
                                    }
                                    KeyCode::Right => {
                                        let m = buf.chars().count();
                                        if cursor < m {
                                            cursor += 1;
                                        }
                                        mode = InputMode::RenameSession {
                                            buf,
                                            cursor,
                                        };
                                    }
                                    KeyCode::Char(c)
                                        if key.modifiers
                                            == KeyModifiers::NONE
                                            || key.modifiers
                                                == KeyModifiers::SHIFT =>
                                    {
                                        let bp = char_byte_pos(&buf, cursor);
                                        buf.insert(bp, c);
                                        cursor += 1;
                                        mode = InputMode::RenameSession {
                                            buf,
                                            cursor,
                                        };
                                    }
                                    _ => {
                                        mode = InputMode::RenameSession {
                                            buf,
                                            cursor,
                                        };
                                    }
                                },
                            }
                        }
                        Event::Mouse(mouse) => {
                            match mode {
                                InputMode::Normal
                                | InputMode::Prefix
                                | InputMode::Resize => {
                                    if mode != InputMode::Normal {
                                        mode = InputMode::Normal;
                                    }
                                    let mouse_mode = frame
                                        .as_ref()
                                        .map(active_mouse_mode)
                                        .unwrap_or(0);
                                    if mouse_mode != 0 {
                                        mouse_select = None;
                                        let bytes = mouse_to_bytes(mouse);
                                        if !bytes.is_empty() {
                                            server.send_input(&bytes);
                                        }
                                    } else {
                                        match mouse.kind {
                                            MouseEventKind::ScrollUp => {
                                                server.scroll_up(SCROLL_LINES);
                                            }
                                            MouseEventKind::ScrollDown => {
                                                server
                                                    .scroll_down(SCROLL_LINES);
                                            }
                                            MouseEventKind::Down(
                                                MouseButton::Left,
                                            ) => {
                                                if let Some(ref fd) = frame {
                                                    let (cols, rows) =
                                                        terminal::size()
                                                            .unwrap_or((
                                                                80, 24,
                                                            ));
                                                    let fa =
                                                        ratatui::layout::Rect {
                                                            x: 0,
                                                            y: 0,
                                                            width: cols,
                                                            height: rows,
                                                        };
                                                    let pa = active_pane_content_rect(fd, fa, hide_borders);
                                                    if mouse.column >= pa.x
                                                        && mouse.column
                                                            < pa.x + pa.width
                                                        && mouse.row >= pa.y
                                                        && mouse.row
                                                            < pa.y + pa.height
                                                    {
                                                        mouse_select = Some(
                                                            MouseSelection {
                                                                start_col:
                                                                    mouse.column,
                                                                start_row:
                                                                    mouse.row,
                                                                end_col: mouse
                                                                    .column,
                                                                end_row: mouse
                                                                    .row,
                                                            },
                                                        );
                                                    }
                                                }
                                            }
                                            MouseEventKind::Drag(
                                                MouseButton::Left,
                                            ) => {
                                                if let Some(ref mut sel) =
                                                    mouse_select
                                                {
                                                    if let Some(ref fd) = frame
                                                    {
                                                        let (cols, rows) =
                                                            terminal::size()
                                                                .unwrap_or((
                                                                    80, 24,
                                                                ));
                                                        let fa = ratatui::layout::Rect { x: 0, y: 0, width: cols, height: rows };
                                                        let pa = active_pane_content_rect(fd, fa, hide_borders);
                                                        sel.end_col = mouse.column
                                                            .max(pa.x)
                                                            .min(pa.x + pa.width.saturating_sub(1));
                                                        sel.end_row = mouse.row
                                                            .max(pa.y)
                                                            .min(pa.y + pa.height.saturating_sub(1));
                                                    } else {
                                                        sel.end_col =
                                                            mouse.column;
                                                        sel.end_row = mouse.row;
                                                    }
                                                }
                                            }
                                            MouseEventKind::Up(
                                                MouseButton::Left,
                                            ) => {
                                                if let Some(sel) =
                                                    mouse_select.take()
                                                {
                                                    if let Some(ref fd) = frame
                                                    {
                                                        let text = extract_text_from_frame(fd, &sel, hide_borders);
                                                        if !text.is_empty() {
                                                            let result = copy_to_clipboard(&text);
                                                            status_notice = Some((
                                                                match result {
                                                                    ClipboardCopyResult::System => format!(
                                                                        "copied {} chars",
                                                                        text.chars().count()
                                                                    ),
                                                                    ClipboardCopyResult::Osc52 => format!(
                                                                        "copied {} chars via OSC 52",
                                                                        text.chars().count()
                                                                    ),
                                                                    ClipboardCopyResult::Unavailable => format!(
                                                                        "yanked {} chars (clipboard unavailable)",
                                                                        text.chars().count()
                                                                    ),
                                                                },
                                                                Instant::now() + Duration::from_secs(3),
                                                            ));
                                                        }
                                                    }
                                                }
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                                InputMode::CopyMode => match mouse.kind {
                                    MouseEventKind::ScrollUp => {
                                        server.scroll_up(SCROLL_LINES);
                                    }
                                    MouseEventKind::ScrollDown => {
                                        server.scroll_down(SCROLL_LINES);
                                    }
                                    _ => {}
                                },
                                _ => {}
                            }
                        }
                        Event::Paste(text) => {
                            handle_paste_event(&server, &mut mode, text);
                        }
                        Event::Resize(new_cols, new_rows) => {
                            server.resize(Size::new(new_rows, new_cols));
                        }
                        _ => {}
                    }
                }
            }
            Ok(())
        })();

        let _ = terminal::disable_raw_mode();
        let _ = execute!(
            terminal.backend_mut(),
            DisableBracketedPaste,
            DisableMouseCapture,
            PopKeyboardEnhancementFlags,
            LeaveAlternateScreen,
            SetCursorStyle::DefaultUserShape,
            cursor::Show
        );
        run_result
    }
}

struct MouseSelection {
    start_col: u16,
    start_row: u16,
    end_col: u16,
    end_row: u16,
}

fn render_mouse_selection(
    f: &mut ratatui::Frame,
    sel: &MouseSelection,
    fd: &FrameData,
    hide_borders: bool,
) {
    use ratatui::style::{Color, Modifier, Style};

    let (start_row, start_col, end_row, end_col) =
        if (sel.start_row, sel.start_col) <= (sel.end_row, sel.end_col) {
            (sel.start_row, sel.start_col, sel.end_row, sel.end_col)
        } else {
            (sel.end_row, sel.end_col, sel.start_row, sel.start_col)
        };
    if start_row == end_row && start_col == end_col {
        return;
    }

    let frame_area = f.area();
    let pa = active_pane_content_rect(fd, frame_area, hide_borders);

    let clamp_row =
        |r: u16| r.max(pa.y).min(pa.y + pa.height.saturating_sub(1));
    let clamp_col = |c: u16| c.max(pa.x).min(pa.x + pa.width);

    let sr = clamp_row(start_row);
    let sc = clamp_col(start_col);
    let er = clamp_row(end_row);
    let ec = clamp_col(end_col);

    if sr == er && sc == ec {
        return;
    }

    let buf = f.buffer_mut();
    for row in sr..=er {
        let col_begin = if row == sr { sc } else { pa.x };
        let col_end = if row == er { ec } else { pa.x + pa.width };
        for col in col_begin..col_end {
            if col >= frame_area.width {
                break;
            }
            let cell = &mut buf[(col, row)];
            cell.set_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::LightCyan)
                    .remove_modifier(Modifier::REVERSED),
            );
        }
    }
}

fn extract_text_from_frame(
    fd: &FrameData,
    sel: &MouseSelection,
    hide_borders: bool,
) -> String {
    let (start_row, start_col, end_row, end_col) =
        if (sel.start_row, sel.start_col) <= (sel.end_row, sel.end_col) {
            (sel.start_row, sel.start_col, sel.end_row, sel.end_col)
        } else {
            (sel.end_row, sel.end_col, sel.start_row, sel.start_col)
        };
    if start_row == end_row && start_col == end_col {
        return String::new();
    }
    let term_area = ratatui::layout::Rect {
        x: 0,
        y: 0,
        width: terminal::size().map(|(c, _)| c).unwrap_or(80),
        height: terminal::size().map(|(_, r)| r).unwrap_or(24),
    };
    let content_height = term_area.height.saturating_sub(1);
    let layout_area = ratatui::layout::Rect {
        x: 0,
        y: 0,
        width: term_area.width,
        height: content_height,
    };
    let (rows, content_area) =
        find_active_pane_content(&fd.layout, layout_area, hide_borders);
    if rows.is_empty() {
        return String::new();
    }
    let clamp_row = |r: u16| {
        r.max(content_area.y)
            .min(content_area.y + content_area.height.saturating_sub(1))
    };
    let clamp_col = |c: u16| {
        c.max(content_area.x)
            .min(content_area.x + content_area.width)
    };
    let start_row = clamp_row(start_row);
    let start_col = clamp_col(start_col);
    let end_row = clamp_row(end_row);
    let end_col = clamp_col(end_col);
    if start_row == end_row && start_col == end_col {
        return String::new();
    }
    let pane_start_row = (start_row - content_area.y) as usize;
    let pane_start_col = (start_col - content_area.x) as usize;
    let pane_end_row = (end_row - content_area.y) as usize;
    let pane_end_col = (end_col - content_area.x) as usize;
    let max_row = rows.len().saturating_sub(1);
    let pane_start_row = pane_start_row.min(max_row);
    let pane_end_row = pane_end_row.min(max_row);
    let mut lines: Vec<String> = Vec::new();
    for row in pane_start_row..=pane_end_row {
        let row_text = rows.get(row).map(|s| s.as_str()).unwrap_or("");
        let chars: Vec<char> = row_text.chars().collect();
        let line_start = if row == pane_start_row {
            pane_start_col
        } else {
            0
        };
        let line_end = if row == pane_end_row {
            pane_end_col.min(chars.len())
        } else {
            chars.len()
        };
        let s: String = chars
            .iter()
            .skip(line_start)
            .take(line_end.saturating_sub(line_start))
            .collect();
        lines.push(s);
    }
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    for line in &mut lines {
        let trimmed = line.trim_end().to_string();
        *line = trimmed;
    }
    lines.join("\n")
}

fn active_pane_content_rect(
    fd: &FrameData,
    frame_area: ratatui::layout::Rect,
    hide_borders: bool,
) -> ratatui::layout::Rect {
    let content_height = frame_area.height.saturating_sub(1);
    let layout_area = ratatui::layout::Rect {
        x: 0,
        y: 0,
        width: frame_area.width,
        height: content_height,
    };
    let (_, pane_area) =
        find_active_pane_content(&fd.layout, layout_area, hide_borders);
    pane_area
}

fn find_active_pane_content(
    layout: &LayoutJson,
    area: ratatui::layout::Rect,
    hide_borders: bool,
) -> (Vec<String>, ratatui::layout::Rect) {
    match layout {
        LayoutJson::Leaf {
            active: true,
            rows_v2,
            ..
        } => {
            let content_area =
                if !hide_borders && area.width > 2 && area.height > 2 {
                    ratatui::layout::Rect {
                        x: area.x + 1,
                        y: area.y + 1,
                        width: area.width - 2,
                        height: area.height - 2,
                    }
                } else {
                    area
                };
            let rows = rows_v2
                .iter()
                .map(|row| row.runs.iter().map(|r| r.text.as_str()).collect())
                .collect();
            (rows, content_area)
        }
        LayoutJson::Split {
            direction,
            sizes,
            children,
        } => {
            let chunks = split_layout_rects_for_extract(
                area,
                direction,
                sizes,
                children.len(),
                hide_borders,
            );
            for (child, chunk) in children.iter().zip(chunks.into_iter()) {
                let (rows, content_area) =
                    find_active_pane_content(child, chunk, hide_borders);
                if !rows.is_empty() {
                    return (rows, content_area);
                }
            }
            (Vec::new(), area)
        }
        LayoutJson::Leaf { .. } => (Vec::new(), area),
    }
}

fn split_layout_rects_for_extract(
    area: ratatui::layout::Rect,
    direction: &str,
    sizes: &[u16],
    count: usize,
    hide_borders: bool,
) -> Vec<ratatui::layout::Rect> {
    if count == 0 {
        return Vec::new();
    }
    let horizontal = direction == "horizontal";
    let total_dim = if horizontal { area.width } else { area.height };
    let gap: u16 = if hide_borders { 0 } else { 1 };
    let borders = count.saturating_sub(1) as u16 * gap;
    let available = total_dim.saturating_sub(borders);
    let total_pct = sizes.iter().copied().sum::<u16>().max(1);
    let mut rects = Vec::with_capacity(count);
    let mut offset = 0u16;
    for (index, &pct) in sizes.iter().enumerate().take(count) {
        let dim = if index + 1 == count {
            available.saturating_sub(offset)
        } else {
            (available as u32 * pct as u32 / total_pct as u32) as u16
        };
        rects.push(if horizontal {
            ratatui::layout::Rect {
                x: area.x + offset,
                y: area.y,
                width: dim,
                height: area.height,
            }
        } else {
            ratatui::layout::Rect {
                x: area.x,
                y: area.y + offset,
                width: area.width,
                height: dim,
            }
        });
        offset += dim + gap;
    }
    rects
}

fn cursor_style_for_shape(shape: Option<u8>) -> SetCursorStyle {
    match shape.unwrap_or(crate::pty::CURSOR_SHAPE_UNSET) {
        0 | 1 => SetCursorStyle::BlinkingBlock,
        2 => SetCursorStyle::SteadyBlock,
        3 => SetCursorStyle::BlinkingUnderScore,
        4 => SetCursorStyle::SteadyUnderScore,
        5 => SetCursorStyle::BlinkingBar,
        6 => SetCursorStyle::SteadyBar,
        _ => SetCursorStyle::DefaultUserShape,
    }
}

fn handle_prefix_key(server: &SocketClient, key: KeyEvent) -> Option<String> {
    let cmd = match (key.code, key.modifiers) {
        (KeyCode::Char('%'), _) => "split-window -h",
        (KeyCode::Char('"'), _) => "split-window -v",
        (KeyCode::Char('c'), KeyModifiers::NONE) => "new-window",
        (KeyCode::Char('n'), KeyModifiers::NONE) => "select-window -n",
        (KeyCode::Char('p'), KeyModifiers::NONE) => "select-window -p",
        (KeyCode::Char('x'), KeyModifiers::NONE) => "kill-pane",
        (KeyCode::Char('z'), KeyModifiers::NONE) => "zoom-pane",
        (KeyCode::Char('K'), KeyModifiers::SHIFT) => "clear-pane",
        (KeyCode::Char('H'), KeyModifiers::SHIFT) => "set-pane-start-dir",
        (KeyCode::Char('h'), KeyModifiers::NONE) => "select-pane -L",
        (KeyCode::Char('j'), KeyModifiers::NONE) => "select-pane -D",
        (KeyCode::Char('k'), KeyModifiers::NONE) => "select-pane -U",
        (KeyCode::Char('l'), KeyModifiers::NONE) => "select-pane -R",
        (KeyCode::Up, _) => "select-pane -U",
        (KeyCode::Down, _) => "select-pane -D",
        (KeyCode::Left, _) => "select-pane -L",
        (KeyCode::Right, _) => "select-pane -R",
        _ => return None,
    };
    run_command_notice(server, cmd)
}

fn run_command_notice(server: &SocketClient, cmd: &str) -> Option<String> {
    if cmd.trim() == "set-pane-start-dir" {
        let output = server.run_command_with_output(cmd);
        let path = output.trim();
        return Some(if path.is_empty() {
            "set start dir failed".to_string()
        } else {
            format!("start dir: {}", path)
        });
    }
    server.run_command(cmd);
    None
}

fn is_resize_modifier_key(key: KeyEvent) -> bool {
    matches!(
        key.code,
        KeyCode::Modifier(ModifierKeyCode::LeftAlt)
            | KeyCode::Modifier(ModifierKeyCode::RightAlt)
    )
}

fn resize_command_for_key(key: KeyEvent) -> Option<&'static str> {
    if !key.modifiers.contains(KeyModifiers::ALT) {
        return None;
    }
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::SHIFT)
    {
        return None;
    }
    match key.code {
        KeyCode::Char('h') | KeyCode::Left => Some("resize-pane -L"),
        KeyCode::Char('j') | KeyCode::Down => Some("resize-pane -D"),
        KeyCode::Char('k') | KeyCode::Up => Some("resize-pane -U"),
        KeyCode::Char('l') | KeyCode::Right => Some("resize-pane -R"),
        _ => None,
    }
}

fn status_banner_for_mode(
    mode: &InputMode,
    notice: Option<&str>,
) -> Option<String> {
    let mode_label = match mode {
        InputMode::Resize => Some("RESIZE"),
        InputMode::CopyMode => Some("COPY"),
        InputMode::CopySearch { forward, .. } => {
            Some(if *forward { "COPY /" } else { "COPY ?" })
        }
        _ => None,
    };
    match (mode_label, notice) {
        (Some(label), Some(notice)) => Some(format!("{} | {}", label, notice)),
        (Some(label), None) => Some(label.to_string()),
        (None, Some(notice)) => Some(notice.to_string()),
        (None, None) => None,
    }
}

enum ClipboardCopyResult {
    System,
    Osc52,
    Unavailable,
}

fn copy_to_clipboard(text: &str) -> ClipboardCopyResult {
    if text.is_empty() {
        return ClipboardCopyResult::Unavailable;
    }
    if should_prefer_osc52() {
        if copy_to_clipboard_via_osc52(text).is_ok() {
            return ClipboardCopyResult::Osc52;
        }
        if copy_to_clipboard_via_arboard(text) {
            return ClipboardCopyResult::System;
        }
    } else {
        if copy_to_clipboard_via_arboard(text) {
            return ClipboardCopyResult::System;
        }
        if copy_to_clipboard_via_osc52(text).is_ok() {
            return ClipboardCopyResult::Osc52;
        }
    }
    ClipboardCopyResult::Unavailable
}

fn copy_to_clipboard_via_arboard(text: &str) -> bool {
    Clipboard::new()
        .and_then(|mut clipboard| clipboard.set_text(text.to_string()))
        .is_ok()
}

fn copy_to_clipboard_via_osc52(text: &str) -> io::Result<()> {
    let sequence = build_osc52_sequence(text);
    let mut stdout = io::stdout();
    stdout.write_all(sequence.as_bytes())?;
    stdout.flush()
}

fn build_osc52_sequence(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", STANDARD.encode(text.as_bytes()))
}

fn should_prefer_osc52() -> bool {
    has_ssh_environment(|key| std::env::var(key).ok())
}

fn has_ssh_environment(lookup: impl Fn(&str) -> Option<String>) -> bool {
    ["SSH_TTY", "SSH_CONNECTION", "SSH_CLIENT"]
        .into_iter()
        .any(|key| lookup(key).is_some_and(|value| !value.is_empty()))
}

fn is_copy_plain_key(modifiers: KeyModifiers) -> bool {
    !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

fn is_copy_line_start_key(key: KeyEvent) -> bool {
    is_copy_plain_key(key.modifiers)
        && matches!(key.code, KeyCode::Home | KeyCode::Char('0'))
}

fn is_copy_line_end_key(key: KeyEvent) -> bool {
    is_copy_plain_key(key.modifiers)
        && matches!(key.code, KeyCode::End | KeyCode::Char('$'))
        || (matches!(key.code, KeyCode::Char('4'))
            && key.modifiers.contains(KeyModifiers::SHIFT)
            && is_copy_plain_key(key.modifiers))
}

fn handle_paste_event(
    server: &SocketClient,
    mode: &mut InputMode,
    text: String,
) {
    if text.is_empty() {
        return;
    }
    match mode.clone() {
        InputMode::Normal => {
            server.send_input(text.as_bytes());
        }
        InputMode::Prefix | InputMode::Resize => {
            server.send_input(text.as_bytes());
            *mode = InputMode::Normal;
        }
        InputMode::CopySearch {
            mut buf,
            mut cursor,
            forward,
        } => {
            insert_text_at_cursor(&mut buf, &mut cursor, &text);
            *mode = InputMode::CopySearch {
                buf,
                cursor,
                forward,
            };
        }
        InputMode::RenameWindow {
            mut buf,
            mut cursor,
        } => {
            insert_text_at_cursor(&mut buf, &mut cursor, &text);
            *mode = InputMode::RenameWindow { buf, cursor };
        }
        InputMode::RenameSession {
            mut buf,
            mut cursor,
        } => {
            insert_text_at_cursor(&mut buf, &mut cursor, &text);
            *mode = InputMode::RenameSession { buf, cursor };
        }
        InputMode::Command {
            mut buf,
            mut cursor,
        } => {
            insert_text_at_cursor(&mut buf, &mut cursor, &text);
            *mode = InputMode::Command { buf, cursor };
        }
        InputMode::CopyMode | InputMode::SessionChooser { .. } => {}
    }
}

fn xterm_modifier_param(modifiers: KeyModifiers) -> u8 {
    let mut param = 1u8;
    if modifiers.contains(KeyModifiers::SHIFT) {
        param += 1;
    }
    if modifiers.contains(KeyModifiers::ALT) {
        param += 2;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        param += 4;
    }
    param
}

fn csi_letter_with_modifiers(letter: char, modifiers: KeyModifiers) -> Vec<u8> {
    let param = xterm_modifier_param(modifiers);
    if param > 1 {
        format!("\x1b[1;{}{}", param, letter).into_bytes()
    } else {
        format!("\x1b[{}", letter).into_bytes()
    }
}

fn csi_tilde_with_modifiers(code: u8, modifiers: KeyModifiers) -> Vec<u8> {
    let param = xterm_modifier_param(modifiers);
    if param > 1 {
        format!("\x1b[{};{}~", code, param).into_bytes()
    } else {
        format!("\x1b[{}~", code).into_bytes()
    }
}

fn fkey_with_modifiers(n: u8, base: &[u8], modifiers: KeyModifiers) -> Vec<u8> {
    let param = xterm_modifier_param(modifiers);
    if param > 1 {
        match n {
            1..=4 => {
                let letter = base[base.len() - 1];
                format!("\x1b[1;{}{}", param, letter as char).into_bytes()
            }
            _ => {
                let code = match n {
                    5 => 15,
                    6 => 17,
                    7 => 18,
                    8 => 19,
                    9 => 20,
                    10 => 21,
                    11 => 23,
                    12 => 24,
                    13 => 25,
                    14 => 26,
                    15 => 28,
                    16 => 29,
                    17 => 31,
                    18 => 32,
                    19 => 33,
                    20 => 34,
                    21 => 42,
                    22 => 43,
                    23 => 44,
                    24 => 45,
                    _ => return vec![],
                };
                format!("\x1b[{};{}~", code, param).into_bytes()
            }
        }
    } else {
        base.to_vec()
    }
}

fn key_to_bytes(key: KeyEvent) -> Vec<u8> {
    let mods = key.modifiers;
    let mods_no_shift = mods & !KeyModifiers::SHIFT;

    let mut bytes = match key.code {
        KeyCode::Char(c) => {
            if mods.contains(KeyModifiers::CONTROL) {
                let lower = c.to_ascii_lowercase();
                if lower >= 'a' && lower <= 'z' {
                    vec![lower as u8 - b'a' + 1]
                } else {
                    match c {
                        ' ' | '@' | '2' => vec![0x00],
                        '[' | '3' => vec![0x1b],
                        '\\' | '4' => vec![0x1c],
                        ']' | '5' => vec![0x1d],
                        '^' | '6' => vec![0x1e],
                        '/' | '_' | '7' => vec![0x1f],
                        '?' => vec![0x7f],
                        _ => {
                            if (c as u32) < 0x20 {
                                vec![c as u8]
                            } else {
                                let mut buf = [0u8; 4];
                                c.encode_utf8(&mut buf).as_bytes().to_vec()
                            }
                        }
                    }
                }
            } else if (c as u32) < 0x20 {
                vec![c as u8]
            } else {
                let mut buf = [0u8; 4];
                c.encode_utf8(&mut buf).as_bytes().to_vec()
            }
        }
        KeyCode::Enter => b"\r".to_vec(),
        KeyCode::Backspace => b"\x7f".to_vec(),
        KeyCode::Esc => b"\x1b".to_vec(),
        KeyCode::Null => vec![0x00],
        KeyCode::Tab => {
            if mods.contains(KeyModifiers::SHIFT) {
                b"\x1b[Z".to_vec()
            } else {
                b"\t".to_vec()
            }
        }
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Up => csi_letter_with_modifiers('A', mods_no_shift),
        KeyCode::Down => csi_letter_with_modifiers('B', mods_no_shift),
        KeyCode::Right => csi_letter_with_modifiers('C', mods_no_shift),
        KeyCode::Left => csi_letter_with_modifiers('D', mods_no_shift),
        KeyCode::Home => csi_letter_with_modifiers('H', mods_no_shift),
        KeyCode::End => csi_letter_with_modifiers('F', mods_no_shift),
        KeyCode::Insert => csi_tilde_with_modifiers(2, mods_no_shift),
        KeyCode::Delete => csi_tilde_with_modifiers(3, mods_no_shift),
        KeyCode::PageUp => csi_tilde_with_modifiers(5, mods_no_shift),
        KeyCode::PageDown => csi_tilde_with_modifiers(6, mods_no_shift),
        KeyCode::F(n) => {
            let base: Vec<u8> = match n {
                1 => b"\x1bOP".to_vec(),
                2 => b"\x1bOQ".to_vec(),
                3 => b"\x1bOR".to_vec(),
                4 => b"\x1bOS".to_vec(),
                5 => b"\x1b[15~".to_vec(),
                6 => b"\x1b[17~".to_vec(),
                7 => b"\x1b[18~".to_vec(),
                8 => b"\x1b[19~".to_vec(),
                9 => b"\x1b[20~".to_vec(),
                10 => b"\x1b[21~".to_vec(),
                11 => b"\x1b[23~".to_vec(),
                12 => b"\x1b[24~".to_vec(),
                13 => b"\x1b[25~".to_vec(),
                14 => b"\x1b[26~".to_vec(),
                15 => b"\x1b[28~".to_vec(),
                16 => b"\x1b[29~".to_vec(),
                17 => b"\x1b[31~".to_vec(),
                18 => b"\x1b[32~".to_vec(),
                19 => b"\x1b[33~".to_vec(),
                20 => b"\x1b[34~".to_vec(),
                21 => b"\x1b[42~".to_vec(),
                22 => b"\x1b[43~".to_vec(),
                23 => b"\x1b[44~".to_vec(),
                24 => b"\x1b[45~".to_vec(),
                _ => vec![],
            };
            fkey_with_modifiers(n, &base, mods_no_shift)
        }
        _ => vec![],
    };
    if !bytes.is_empty()
        && mods.contains(KeyModifiers::ALT)
        && !matches!(
            key.code,
            KeyCode::Esc
                | KeyCode::Modifier(_)
                | KeyCode::Up
                | KeyCode::Down
                | KeyCode::Right
                | KeyCode::Left
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::Insert
                | KeyCode::Delete
                | KeyCode::PageUp
                | KeyCode::PageDown
                | KeyCode::F(_)
        )
    {
        bytes.insert(0, 0x1b);
    }
    bytes
}

fn mouse_to_bytes(event: MouseEvent) -> Vec<u8> {
    let col = event.column;
    let row = event.row;
    let mut cb = match event.kind {
        MouseEventKind::Down(MouseButton::Left) => 0,
        MouseEventKind::Down(MouseButton::Middle) => 1,
        MouseEventKind::Down(MouseButton::Right) => 2,
        MouseEventKind::Up(MouseButton::Left) => 0,
        MouseEventKind::Up(MouseButton::Middle) => 1,
        MouseEventKind::Up(MouseButton::Right) => 2,
        MouseEventKind::Drag(MouseButton::Left) => 32,
        MouseEventKind::Drag(MouseButton::Middle) => 33,
        MouseEventKind::Drag(MouseButton::Right) => 34,
        MouseEventKind::Moved => 35,
        MouseEventKind::ScrollUp => 64,
        MouseEventKind::ScrollDown => 65,
        MouseEventKind::ScrollLeft => 66,
        MouseEventKind::ScrollRight => 67,
    };
    if event.modifiers.contains(KeyModifiers::SHIFT) {
        cb |= 4;
    }
    if event.modifiers.contains(KeyModifiers::ALT) {
        cb |= 8;
    }
    if event.modifiers.contains(KeyModifiers::CONTROL) {
        cb |= 16;
    }
    let suffix = match event.kind {
        MouseEventKind::Up(_) => 'm',
        _ => 'M',
    };
    format!("\x1b[<{};{};{}{}", cb, col + 1, row + 1, suffix).into_bytes()
}

fn char_byte_pos(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

fn insert_text_at_cursor(buf: &mut String, cursor: &mut usize, text: &str) {
    let bp = char_byte_pos(buf, *cursor);
    buf.insert_str(bp, text);
    *cursor += text.chars().count();
}

fn shell_quote(s: &str) -> String {
    if s.contains(|c: char| c.is_whitespace() || c == '"' || c == '\'') {
        format!("\"{}\"", s.replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

fn visible_entries<'a>(
    entries: &'a [SessionTreeEntry],
    collapsed: &std::collections::HashSet<String>,
) -> Vec<&'a SessionTreeEntry> {
    visible_entries_full(entries, collapsed, &std::collections::HashSet::new())
}

fn visible_entries_full<'a>(
    entries: &'a [SessionTreeEntry],
    collapsed: &std::collections::HashSet<String>,
    collapsed_windows: &std::collections::HashSet<(String, usize)>,
) -> Vec<&'a SessionTreeEntry> {
    entries
        .iter()
        .filter(|e| match e {
            SessionTreeEntry::Session { .. } => true,
            SessionTreeEntry::Window { session_name, .. } => {
                !collapsed.contains(session_name)
            }
            SessionTreeEntry::Pane {
                session_name,
                window_index,
                ..
            } => {
                !collapsed.contains(session_name)
                    && !collapsed_windows
                        .contains(&(session_name.clone(), *window_index))
            }
        })
        .collect()
}

fn log_client(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/zmux_client.log")
    {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let _ = writeln!(f, "[{}] {}", ts, msg);
    }
}

fn ensure_server_and_connect(
    socket_name: &str,
    session_name: &str,
    size: Size,
) -> io::Result<SocketClient> {
    log_client(&format!(
        "ensure_server_and_connect socket='{}' session='{}' size={}x{}",
        socket_name, session_name, size.rows, size.cols
    ));

    match SocketClient::connect(socket_name, size) {
        Ok(client) => {
            log_client("connected to existing server");
            return Ok(client);
        }
        Err(e) => {
            log_client(&format!("connect failed: {}", e));
        }
    }

    #[cfg(unix)]
    {
        if let Ok(path) = crate::ipc::socket_path(socket_name) {
            if path.exists() {
                log_client(&format!(
                    "removing stale socket: {}",
                    path.display()
                ));
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    let exe = std::env::current_exe()?;
    log_client(&format!("spawning server: {}", exe.display()));

    let child = crate::platform::spawn_server_background(
        &exe,
        socket_name,
        session_name,
    );
    if let Err(e) = child {
        log_client(&format!("spawn failed: {}", e));
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("failed to spawn server ({}): {}", exe.display(), e),
        ));
    }

    for i in 0..100 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        match SocketClient::connect(socket_name, size) {
            Ok(client) => {
                log_client(&format!("connected after {}ms", (i + 1) * 50));
                return Ok(client);
            }
            Err(e) if i % 10 == 9 => {
                log_client(&format!(
                    "still waiting ({}ms): {}",
                    (i + 1) * 50,
                    e
                ));
            }
            _ => {}
        }
    }
    let msg = format!(
        "server did not start within 5 seconds (socket: '{}', exe: '{}')",
        socket_name,
        exe.display()
    );
    log_client(&msg);
    Err(io::Error::new(io::ErrorKind::TimedOut, msg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_osc52_sequence_base64_encodes_utf8_text() {
        assert_eq!(build_osc52_sequence("hello"), "\x1b]52;c;aGVsbG8=\x07");
        assert_eq!(build_osc52_sequence("中"), "\x1b]52;c;5Lit\x07");
    }

    #[test]
    fn has_ssh_environment_detects_known_ssh_variables() {
        assert!(has_ssh_environment(|key| match key {
            "SSH_CONNECTION" => Some("1 2 3 4".to_string()),
            _ => None,
        }));
        assert!(!has_ssh_environment(|_| None));
    }
}
