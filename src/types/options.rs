use std::collections::HashMap;

use super::mode::KeyBinding;

#[derive(Clone)]
pub struct GlobalOptions {
    pub default_shell: String,
    pub history_limit: usize,
    pub escape_time_ms: u64,
    pub repeat_time_ms: u64,
    pub status_interval: u64,
    pub mouse_enabled: bool,
    pub focus_events: bool,
    pub set_clipboard: String,
    pub copy_command: String,
    pub destroy_unattached: bool,
    pub exit_empty: bool,
    pub key_tables: HashMap<String, Vec<KeyBinding>>,
    pub hooks: HashMap<String, Vec<String>>,
    pub environment: HashMap<String, String>,
    pub user_options: HashMap<String, String>,
    pub command_aliases: HashMap<String, String>,
    pub update_environment: Vec<String>,
}

impl Default for GlobalOptions {
    fn default() -> Self {
        Self {
            default_shell: String::new(),
            history_limit: 2000,
            escape_time_ms: 500,
            repeat_time_ms: 500,
            status_interval: 15,
            mouse_enabled: true,
            focus_events: false,
            set_clipboard: "on".to_string(),
            copy_command: String::new(),
            destroy_unattached: false,
            exit_empty: true,
            key_tables: HashMap::new(),
            hooks: HashMap::new(),
            environment: HashMap::new(),
            user_options: HashMap::new(),
            command_aliases: HashMap::new(),
            update_environment: vec![
                "DISPLAY".to_string(),
                "SSH_AUTH_SOCK".to_string(),
                "SSH_CONNECTION".to_string(),
            ],
        }
    }
}

#[derive(Clone, Default)]
pub struct SessionOptions {
    pub base_index: usize,
    pub pane_base_index: usize,
    pub renumber_windows: bool,
    pub automatic_rename: bool,
    pub allow_rename: bool,
    pub remain_on_exit: bool,
    pub monitor_activity: bool,
    pub monitor_silence: u64,
    pub bell_action: String,
    pub status_position: String,
    pub status_visible: bool,
    pub status_left: String,
    pub status_right: String,
    pub status_left_length: usize,
    pub status_right_length: usize,
    pub status_style: String,
    pub status_left_style: String,
    pub status_right_style: String,
    pub status_justify: String,
    pub status_lines: usize,
    pub window_status_format: String,
    pub window_status_current_format: String,
    pub window_status_separator: String,
    pub message_style: String,
    pub mode_style: String,
    pub mode_keys: String,
}

impl SessionOptions {
    pub fn with_defaults() -> Self {
        Self {
            base_index: 0,
            pane_base_index: 0,
            renumber_windows: false,
            automatic_rename: true,
            allow_rename: true,
            remain_on_exit: false,
            monitor_activity: false,
            monitor_silence: 0,
            bell_action: "any".to_string(),
            status_position: "bottom".to_string(),
            status_visible: true,
            status_left: "[#S] ".to_string(),
            status_right: "%H:%M %d-%b-%y".to_string(),
            status_left_length: 10,
            status_right_length: 40,
            status_style: "bg=green,fg=black".to_string(),
            status_left_style: String::new(),
            status_right_style: String::new(),
            status_justify: "left".to_string(),
            status_lines: 1,
            window_status_format: "#I:#W#{?window_flags,#{window_flags}, }"
                .to_string(),
            window_status_current_format:
                "#I:#W#{?window_flags,#{window_flags}, }".to_string(),
            window_status_separator: " ".to_string(),
            message_style: "bg=yellow,fg=black".to_string(),
            mode_style: "bg=yellow,fg=black".to_string(),
            mode_keys: "vi".to_string(),
        }
    }
}

#[derive(Clone, Default)]
pub struct WindowOptions {
    pub pane_border_style: String,
    pub pane_active_border_style: String,
    pub word_separators: String,
    pub synchronize_panes: bool,
    pub aggressive_resize: bool,
    pub window_size: String,
    pub main_pane_width: u16,
    pub main_pane_height: u16,
    pub allow_passthrough: String,
}

impl WindowOptions {
    pub fn with_defaults() -> Self {
        Self {
            pane_border_style: String::new(),
            pane_active_border_style: "fg=green".to_string(),
            word_separators: " -_@".to_string(),
            synchronize_panes: false,
            aggressive_resize: false,
            window_size: "latest".to_string(),
            main_pane_width: 0,
            main_pane_height: 0,
            allow_passthrough: "off".to_string(),
        }
    }
}
