use crate::{
    client::{socket::SocketClient, FrameData},
    domain::cloud::CloudClient,
    server::SessionTreeEntry,
    types::{session::Size, SelectionMode},
};

pub trait DomainHandle: Send {
    fn latest_frame(&self) -> Option<FrameData>;
    fn frame_snapshot(&self) -> (Option<FrameData>, u64);
    fn send_input(&self, bytes: &[u8]);
    fn send_paste(&self, text: &str);
    fn run_command(&self, cmd: &str);
    fn run_command_with_output(&self, cmd: &str) -> String;
    fn resize(&self, size: Size);
    fn refresh_display(&self);
    fn set_hide_borders(&self, hide: bool);
    fn scroll_on_erase_in_display(&self) -> bool;
    fn set_scroll_on_erase_in_display(&self, enabled: bool);
    fn shutdown(&self);
    fn detach(&self);
    fn active_window_name(&self) -> String;
    fn session_name(&self) -> String;
    fn session_tree(&self) -> Vec<SessionTreeEntry>;
    fn scroll_up(&self, lines: usize);
    fn scroll_down(&self, lines: usize);
    fn scroll_pane(&self, pane_id: usize, direction: &str, lines: usize);
    fn scroll_display(&self, delta: i32);
    fn scroll_display_bottom(&self);
    fn enter_copy_mode(&self) -> bool;
    fn exit_copy_mode(&self);
    fn copy_move_left(&self);
    fn copy_move_right(&self);
    fn copy_move_up(&self);
    fn copy_move_down(&self);
    fn copy_page_up(&self);
    fn copy_page_down(&self);
    fn copy_move_to_top(&self);
    fn copy_move_to_bottom(&self);
    fn copy_move_to_line_start(&self);
    fn copy_move_to_line_end(&self);
    fn copy_move_word_backward(&self);
    fn copy_move_word_forward(&self);
    fn copy_move_word_end(&self);
    fn copy_start_selection(&self, mode: SelectionMode);
    fn copy_clear_selection(&self);
    fn copy_search(&self, query: String, forward: bool) -> bool;
    fn copy_search_next(&self) -> bool;
    fn copy_search_prev(&self) -> bool;
    fn copy_yank_selection(&self) -> String;
    fn domain_label(&self) -> &str;
    fn has_blob(&self) -> bool;
    fn paste_cloud(&self) -> Result<String, String>;
    fn send_control_line(&self, _line: &str) {}
    fn disconnected(&self) -> bool {
        self.latest_frame().is_some_and(|f| f.exit)
    }
    fn blob_notice(&self) -> Option<String> {
        None
    }
}

macro_rules! impl_common_handle {
    ($ty:ty) => {
        fn latest_frame(&self) -> Option<FrameData> {
            Self::latest_frame(self)
        }
        fn frame_snapshot(&self) -> (Option<FrameData>, u64) {
            Self::frame_snapshot(self)
        }
        fn send_input(&self, bytes: &[u8]) {
            Self::send_input(self, bytes)
        }
        fn run_command(&self, cmd: &str) {
            Self::run_command(self, cmd)
        }
        fn run_command_with_output(&self, cmd: &str) -> String {
            Self::run_command_with_output(self, cmd)
        }
        fn resize(&self, size: Size) {
            Self::resize(self, size)
        }
        fn refresh_display(&self) {
            Self::refresh_display(self)
        }
        fn set_hide_borders(&self, hide: bool) {
            Self::set_hide_borders(self, hide)
        }
        fn scroll_on_erase_in_display(&self) -> bool {
            Self::scroll_on_erase_in_display(self)
        }
        fn set_scroll_on_erase_in_display(&self, enabled: bool) {
            Self::set_scroll_on_erase_in_display(self, enabled)
        }
        fn shutdown(&self) {
            Self::shutdown(self)
        }
        fn detach(&self) {
            Self::detach(self)
        }
        fn active_window_name(&self) -> String {
            Self::active_window_name(self)
        }
        fn session_name(&self) -> String {
            Self::session_name(self)
        }
        fn session_tree(&self) -> Vec<SessionTreeEntry> {
            Self::session_tree(self)
        }
        fn scroll_up(&self, lines: usize) {
            Self::scroll_up(self, lines)
        }
        fn scroll_down(&self, lines: usize) {
            Self::scroll_down(self, lines)
        }
        fn scroll_pane(&self, pane_id: usize, direction: &str, lines: usize) {
            Self::scroll_pane(self, pane_id, direction, lines)
        }
        fn scroll_display(&self, delta: i32) {
            Self::scroll_display(self, delta)
        }
        fn scroll_display_bottom(&self) {
            Self::scroll_display_bottom(self)
        }
        fn enter_copy_mode(&self) -> bool {
            Self::enter_copy_mode(self)
        }
        fn exit_copy_mode(&self) {
            Self::exit_copy_mode(self)
        }
        fn copy_move_left(&self) {
            Self::copy_move_left(self)
        }
        fn copy_move_right(&self) {
            Self::copy_move_right(self)
        }
        fn copy_move_up(&self) {
            Self::copy_move_up(self)
        }
        fn copy_move_down(&self) {
            Self::copy_move_down(self)
        }
        fn copy_page_up(&self) {
            Self::copy_page_up(self)
        }
        fn copy_page_down(&self) {
            Self::copy_page_down(self)
        }
        fn copy_move_to_top(&self) {
            Self::copy_move_to_top(self)
        }
        fn copy_move_to_bottom(&self) {
            Self::copy_move_to_bottom(self)
        }
        fn copy_move_to_line_start(&self) {
            Self::copy_move_to_line_start(self)
        }
        fn copy_move_to_line_end(&self) {
            Self::copy_move_to_line_end(self)
        }
        fn copy_move_word_backward(&self) {
            Self::copy_move_word_backward(self)
        }
        fn copy_move_word_forward(&self) {
            Self::copy_move_word_forward(self)
        }
        fn copy_move_word_end(&self) {
            Self::copy_move_word_end(self)
        }
        fn copy_start_selection(&self, mode: SelectionMode) {
            Self::copy_start_selection(self, mode)
        }
        fn copy_clear_selection(&self) {
            Self::copy_clear_selection(self)
        }
        fn copy_search(&self, query: String, forward: bool) -> bool {
            Self::copy_search(self, query, forward)
        }
        fn copy_search_next(&self) -> bool {
            Self::copy_search_next(self)
        }
        fn copy_search_prev(&self) -> bool {
            Self::copy_search_prev(self)
        }
        fn copy_yank_selection(&self) -> String {
            Self::copy_yank_selection(self)
        }
    };
}

impl DomainHandle for SocketClient {
    impl_common_handle!(SocketClient);

    fn send_paste(&self, text: &str) {
        self.send_input(text.as_bytes());
    }

    fn domain_label(&self) -> &str {
        "local"
    }

    fn has_blob(&self) -> bool {
        false
    }

    fn paste_cloud(&self) -> Result<String, String> {
        local_paste_cloud(|text| self.send_input(text.as_bytes()))
    }

    fn send_control_line(&self, line: &str) {
        let _ = self.send_line(line);
    }
}

impl DomainHandle for CloudClient {
    impl_common_handle!(CloudClient);

    fn send_paste(&self, text: &str) {
        Self::send_paste(self, text);
    }

    fn domain_label(&self) -> &str {
        Self::domain_label(self)
    }

    fn has_blob(&self) -> bool {
        Self::has_blob(self)
    }

    fn paste_cloud(&self) -> Result<String, String> {
        Self::paste_cloud(self)
    }

    fn disconnected(&self) -> bool {
        Self::disconnected(self)
    }

    fn blob_notice(&self) -> Option<String> {
        Self::blob_notice(self)
    }
}

fn local_paste_cloud(send_text: impl FnOnce(&str)) -> Result<String, String> {
    let item = crate::domain::clip::read_os_clipboard()?;
    crate::domain::clip::validate_or_text(&item, false)?;
    match item {
        crate::domain::clip::ClipboardItem::Text(text) => {
            send_text(&text);
            Ok(format!("pasted {} chars", text.chars().count()))
        }
        crate::domain::clip::ClipboardItem::ImagePng { bytes, .. } => {
            let path = crate::domain::clip::save_local_image(&bytes)?;
            let quoted = crate::domain::clip::quote_paths(
                &[path.to_string_lossy().into_owned()],
                false,
            )?;
            send_text(&quoted);
            Ok("pasted local image path".into())
        }
        crate::domain::clip::ClipboardItem::Files(files) => {
            let paths: Vec<String> = files
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect();
            let quoted = crate::domain::clip::quote_paths(&paths, false)?;
            send_text(&quoted);
            Ok(format!("pasted {} local path(s)", paths.len()))
        }
    }
}
