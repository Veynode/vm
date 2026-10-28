//! The cua-driver tools the MCP serves: their prefix on the main MCP and
//! the scope each needs.
/// Prefix of the curated tools on the main MCP (`cua` tool group).
pub const PREFIX: &str = "cua_";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Observe,
    Input,
    Browser,
    Clipboard,
    Execute,
}
impl Scope {
    pub fn name(self) -> &'static str {
        match self {
            Self::Observe => "observe",
            Self::Input => "input",
            Self::Browser => "browser",
            Self::Clipboard => "clipboard",
            Self::Execute => "execute",
        }
    }
}

/// Never sent to the driver and never listed: they change the driver itself or download code.
pub const NEVER_EXPOSED: [&str; 4] = [
    "install_extension",
    "check_for_update",
    "install_ffmpeg",
    "set_config",
];

/// The scope a driver tool needs, or None for a never-exposed or unknown tool.
/// The first rows are the docs' table; the rest are the driver 0.38 tools the table
/// leaves implicit, placed by what they change.
pub fn scope(tool: &str) -> Option<Scope> {
    if NEVER_EXPOSED.contains(&tool) {
        return None;
    }
    Some(match tool {
        "list_apps"
        | "list_windows"
        | "get_window_state"
        | "get_accessibility_tree"
        | "get_desktop_state"
        | "zoom"
        | "verify_state"
        | "get_browser_state" => Scope::Observe,
        // Reads of the driver's own state and its bookkeeping sessions.
        "get_screen_size"
        | "get_cursor_position"
        | "get_agent_cursor_state"
        | "check_permissions"
        | "health_report"
        | "get_config"
        | "get_guide"
        | "parse_visual_regions"
        | "get_recording_state"
        | "start_session"
        | "escalate_session"
        | "get_session"
        | "list_sessions"
        | "get_session_state"
        | "end_session" => Scope::Observe,
        "click"
        | "double_click"
        | "right_click"
        | "drag"
        | "mouse_button_down"
        | "mouse_drag"
        | "mouse_button_up"
        | "parallel_mouse_drag"
        | "type_text"
        | "press_key"
        | "hotkey"
        | "scroll"
        | "move_cursor"
        | "set_value"
        | "invoke_menu"
        | "set_window_frame"
        | "bring_to_front"
        | "run_steps"
        | "set_agent_cursor_enabled"
        | "set_agent_cursor_motion"
        | "set_agent_cursor_theme" => Scope::Input,
        "browser_prepare"
        | "browser_navigate"
        | "browser_click"
        | "browser_type"
        | "browser_select"
        | "browser_dialog"
        | "browser_set_input_files"
        | "browser_download"
        | "browser_pointer" => Scope::Browser,
        "clipboard_read" | "clipboard_write" => Scope::Clipboard,
        // `page` runs JavaScript in a page; recordings write Guest files and replays act.
        "run_script" | "launch_app" | "kill_app" | "page" | "start_recording"
        | "stop_recording" | "replay_trajectory" => Scope::Execute,
        _ => return None,
    })
}
