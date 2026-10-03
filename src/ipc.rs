use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::layout::LayoutEnum;
use crate::jwm::{Jwm, WMArgEnum, WMFuncType};

// ---------------------------------------------------------------------------
// Wire protocol types (newline-delimited JSON)
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum IpcMessage {
    Command(IpcCommand),
    Query(IpcQuery),
    Subscribe(IpcSubscribe),
}

impl<'de> Deserialize<'de> for IpcMessage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct MessageVisitor;

        impl<'de> serde::de::Visitor<'de> for MessageVisitor {
            type Value = IpcMessage;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an IPC message object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut command = None;
                let mut query = None;
                let mut subscribe = None;
                let mut args = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "command" => {
                            if command.is_some() {
                                return Err(serde::de::Error::duplicate_field("command"));
                            }
                            command = Some(map.next_value::<String>()?);
                        }
                        "query" => {
                            if query.is_some() {
                                return Err(serde::de::Error::duplicate_field("query"));
                            }
                            query = Some(map.next_value::<String>()?);
                        }
                        "subscribe" => {
                            if subscribe.is_some() {
                                return Err(serde::de::Error::duplicate_field("subscribe"));
                            }
                            subscribe = Some(map.next_value::<Vec<String>>()?);
                        }
                        "args" => {
                            if args.is_some() {
                                return Err(serde::de::Error::duplicate_field("args"));
                            }
                            args = Some(map.next_value::<Value>()?);
                        }
                        // Extension fields are deliberately accepted for
                        // compatibility with clients carrying correlation or
                        // tracing metadata.
                        _ => {
                            map.next_value::<serde::de::IgnoredAny>()?;
                        }
                    }
                }

                // `#[serde(untagged)]` accepted an object containing more than
                // one discriminator as the first matching variant. In
                // particular, `{ "command": ..., "query": ... }` silently
                // executed the command. Require a single unambiguous intent.
                let discriminator_count = usize::from(command.is_some())
                    + usize::from(query.is_some())
                    + usize::from(subscribe.is_some());
                if discriminator_count != 1 {
                    return Err(serde::de::Error::custom(
                        "IPC message must contain exactly one of 'command', 'query', or 'subscribe'",
                    ));
                }

                let args = args.unwrap_or(Value::Null);
                if let Some(command) = command {
                    Ok(IpcMessage::Command(IpcCommand { command, args }))
                } else if let Some(query) = query {
                    Ok(IpcMessage::Query(IpcQuery { query, args }))
                } else {
                    Ok(IpcMessage::Subscribe(IpcSubscribe {
                        subscribe: subscribe.unwrap_or_default(),
                    }))
                }
            }
        }

        deserializer.deserialize_map(MessageVisitor)
    }
}

#[derive(Debug, Deserialize)]
pub struct IpcCommand {
    pub command: String,
    #[serde(default)]
    pub args: Value,
}

#[derive(Debug, Deserialize)]
pub struct IpcQuery {
    pub query: String,
    #[serde(default)]
    pub args: Value,
}

#[derive(Debug, Deserialize)]
pub struct IpcSubscribe {
    pub subscribe: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct IpcResponse {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl IpcResponse {
    pub fn ok(data: Option<Value>) -> Self {
        Self {
            success: true,
            data,
            error: None,
        }
    }
    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            success: false,
            data: None,
            error: Some(msg.into()),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct IpcEvent {
    pub event: String,
    pub payload: Value,
}

// ---------------------------------------------------------------------------
// Protocol discovery and versioned runtime snapshots
// ---------------------------------------------------------------------------

/// Static discovery data for the newline-delimited JSON IPC protocol.
///
/// Keep command/query/topic names here so clients can discover the supported
/// control surface without duplicating JWM's CLI help text. Dispatch commands
/// are separated from commands implemented directly by `Jwm::handle_ipc_command`
/// so configuration validation can continue to accept only bindable WM actions.
pub struct IpcRegistry {
    pub dispatch_commands: &'static [&'static str],
    pub special_commands: &'static [&'static str],
    pub queries: &'static [&'static str],
    pub subscription_topics: &'static [&'static str],
}

pub const IPC_REGISTRY: IpcRegistry = IpcRegistry {
    dispatch_commands: &[
        "adjust_recording_region",
        "ain",
        "annotate",
        "aout",
        "app_launcher",
        "arecord",
        "attach",
        "audio_input_picker",
        "audio_output_picker",
        "bar",
        "bluetooth_picker",
        "bt",
        "caffeine",
        "cal",
        "calendar",
        "case",
        "clayout",
        "clip",
        "clipboard_picker",
        "comp",
        "control_center",
        "cycle",
        "cycle_layout",
        "cycle_overview",
        "cyclelayout",
        "damage",
        "exit",
        "floating",
        "focus_mon",
        "focus_none",
        "focus_stack",
        "focus_tab",
        "focus_window",
        "focusmon",
        "focusstack",
        "ftab",
        "fwin",
        "hub",
        "inc_nmaster",
        "incnmaster",
        "kill",
        "kill_client",
        "killclient",
        "last",
        "last_layout",
        "lastlayout",
        "launcher",
        "layout_picker",
        "layouts",
        "lily",
        "load_session",
        "lock",
        "lock_monitor",
        "lock_screen",
        "loop",
        "loop_view",
        "loopview",
        "mag",
        "maximize",
        "media_next",
        "media_play_pause",
        "media_previous",
        "media_stop",
        "minimize",
        "minimize_window",
        "monitor_layout",
        "monlayout",
        "move_stack",
        "movestack",
        "next",
        "night",
        "notif_center",
        "notification_center",
        "overview",
        "pad",
        "palette",
        "peek",
        "persist_session",
        "pip",
        "play",
        "prev",
        "quit",
        "record",
        "refocus",
        "region",
        "reload_wm",
        "restart",
        "restore",
        "restore_session",
        "save",
        "save_session",
        "scol",
        "scons",
        "screenshot",
        "screenshot_fullscreen",
        "scrolling_consume",
        "scrolling_expel",
        "scrolling_focus_column",
        "scrolling_focus_window",
        "scrolling_move_column",
        "scrolling_toggle_attach_mode",
        "session",
        "session_menu",
        "set_cfact",
        "set_gaps",
        "set_layout",
        "set_mfact",
        "set_nmaster",
        "setcfact",
        "setgaps",
        "setlayout",
        "setmfact",
        "setnmaster",
        "sexp",
        "smov",
        "snap",
        "snap_window",
        "spawn",
        "sticky",
        "stop",
        "swin",
        "switcher",
        "tag",
        "tag_mon",
        "tagmon",
        "tags",
        "take_screenshot",
        "take_screenshot_fullscreen",
        "tbt",
        "toggle_annotation",
        "toggle_audio_recording",
        "toggle_bar",
        "toggle_bluetooth",
        "toggle_compositor",
        "toggle_dnd",
        "toggle_do_not_disturb",
        "toggle_floating",
        "toggle_idle_inhibit",
        "toggle_magnifier",
        "toggle_maximize",
        "toggle_night_light",
        "toggle_overview",
        "toggle_partial_damage",
        "toggle_peek",
        "toggle_pip",
        "toggle_recording",
        "toggle_scratchpad",
        "toggle_sticky",
        "toggle_tag",
        "toggle_tags_overview",
        "toggle_view",
        "toggle_waterlily",
        "toggle_wifi",
        "togglebar",
        "togglecompositor",
        "togglefloating",
        "togglemaximize",
        "togglepartialdamage",
        "togglepip",
        "togglescratchpad",
        "togglesticky",
        "toggletag",
        "toggleview",
        "twifi",
        "unfocus",
        "unlock",
        "unlock_monitor",
        "view",
        "wall",
        "wallpaper_picker",
        "waterlily_case",
        "waterlily_palette",
        "wifi",
        "wifi_picker",
        "window_switcher",
        "zoom",
        "zoom_master",
    ],
    special_commands: &[
        "batch",
        "benchmark",
        "bluetooth_pairing_done",
        "bluetooth_pairing_failed",
        "bluetooth_pairing_prompt",
        "bluetooth_pairing_withdraw",
        "clear_clipboard",
        "clear_notifications",
        "clipboard_copy",
        "clipboard_record",
        "close_notification",
        "command_batch",
        "media_control",
        "move_window_to_monitor",
        "notify",
        "reload_config",
        "set_config",
        "set_config_batch",
        "set_hdr_metadata",
        "set_audio_device",
        "set_media_status",
        "set_mic_mute",
        "set_power_profile",
        "set_recording_region",
        "start_audio_recording",
        "start_recording",
        "stop_audio_recording",
        "stop_recording",
    ],
    queries: &[
        "benchmark_report",
        "get_arec",
        "get_audio",
        "get_audio_devices",
        "get_audio_recording",
        "get_audio_recording_status",
        "get_bar",
        "get_bar_visible",
        "get_bench",
        "get_bluetooth",
        "get_bluetooth_pairing",
        "get_blur",
        "get_blur_status",
        "get_bm",
        "get_bt",
        "get_cap",
        "get_capabilities",
        "get_caps",
        "get_capture",
        "get_capture_status",
        "get_cf",
        "get_cfact",
        "get_cfg",
        "get_cli",
        "get_clients",
        "get_clip",
        "get_clipboard",
        "get_cm",
        "get_color_management",
        "get_color_management_status",
        "get_conf",
        "get_config",
        "get_config_status",
        "get_conn",
        "get_connectivity",
        "get_desktops",
        "get_devices",
        "get_dnd",
        "get_do_not_disturb",
        "get_effect_status",
        "get_effects",
        "get_focused_window",
        "get_fw",
        "get_fx",
        "get_gap",
        "get_gaps",
        "get_gest",
        "get_gesture",
        "get_gesture_status",
        "get_hdr",
        "get_hdr_status",
        "get_idl",
        "get_idle",
        "get_idle_status",
        "get_layout",
        "get_lock",
        "get_lt",
        "get_mag",
        "get_magnifier",
        "get_media",
        "get_media_status",
        "get_metrics",
        "get_mf",
        "get_mfact",
        "get_mic",
        "get_mic_mute",
        "get_monitors",
        "get_mons",
        "get_mute",
        "get_network",
        "get_night_light",
        "get_night_light_status",
        "get_nl",
        "get_nm",
        "get_nmaster",
        "get_notif",
        "get_notifications",
        "get_outputs",
        "get_owns_output",
        "get_pads",
        "get_pair",
        "get_peek",
        "get_perf",
        "get_pk",
        "get_pl",
        "get_power",
        "get_power_status",
        "get_prev_layout",
        "get_rec",
        "get_recording",
        "get_recording_status",
        "get_res",
        "get_resources",
        "get_scratch",
        "get_scratchpads",
        "get_scrolling",
        "get_scrolling_status",
        "get_sel",
        "get_selected",
        "get_sess",
        "get_session_lock",
        "get_show_bar",
        "get_st",
        "get_status",
        "get_strut",
        "get_struts",
        "get_system_ui",
        "get_tab",
        "get_tab_bar",
        "get_tabs",
        "get_tags",
        "get_tearing",
        "get_tearing_hints",
        "get_th",
        "get_tr",
        "get_tree",
        "get_ui",
        "get_ver",
        "get_version",
        "get_vf",
        "get_visible_fullscreen",
        "get_wall",
        "get_wallpaper",
        "get_wallpaper_colors",
        "get_waterlily",
        "get_waterlily_status",
        "get_wayland",
        "get_wayland_status",
        "get_wc",
        "get_win",
        "get_window",
        "get_windows",
        "get_wins",
        "get_wl",
        "get_wly",
        "get_workspaces",
        "get_ws",
        "get_xw",
        "get_xwayland",
        "get_xwayland_status",
    ],
    subscription_topics: &[
        "*",
        "audio",
        "audio_recording",
        "bar",
        "bluetooth",
        "clipboard",
        "config",
        "dnd",
        "idle",
        "layout",
        "media",
        "monitor",
        "network",
        "night_light",
        "notification",
        "power",
        "recording",
        "scrolling",
        "tag",
        "theme",
        "window",
        "workspace",
    ],
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeHealthStatus {
    Healthy,
    Degraded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuntimeHealth {
    pub status: RuntimeHealthStatus,
    pub reasons: Vec<String>,
}

impl RuntimeHealth {
    #[must_use]
    pub fn from_reasons(reasons: Vec<String>) -> Self {
        Self {
            status: if reasons.is_empty() {
                RuntimeHealthStatus::Healthy
            } else {
                RuntimeHealthStatus::Degraded
            },
            reasons,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RuntimeCounts {
    pub windows: usize,
    pub monitors: usize,
    pub workspaces: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RuntimeFeatureStates {
    pub do_not_disturb: bool,
    pub screenshot: bool,
    pub overview: bool,
    pub recording: bool,
    pub audio_recording: bool,
    pub magnifier: bool,
    pub system_ui: bool,
    pub peek: bool,
    pub expose: bool,
    pub annotation: bool,
    /// Shell `layout_picker` panel is up.
    pub layout_picker: bool,
    /// Tags overview grid is up.
    pub tags_overview: bool,
    /// Calendar panel is up.
    pub calendar: bool,
    /// Keybindings Info viewer is up.
    pub keybindings: bool,
    /// Monitor layout editor is up.
    pub monitor_layout: bool,
    /// App launcher panel is up.
    pub launcher: bool,
    /// Session menu panel is up.
    pub session_menu: bool,
    /// Notification center panel is up.
    pub notifications: bool,
    /// WaterLily effect toggle is on.
    pub waterlily: bool,
    /// Night light is currently warming the screen.
    pub night_light: bool,
    /// Manual caffeine / idle inhibit is on.
    pub idle_inhibit: bool,
    /// Shell Hub / control center panel is up.
    pub control_center: bool,
    /// Clipboard history picker is up.
    pub clipboard_picker: bool,
    /// Wi-Fi picker is up.
    pub wifi_picker: bool,
    /// Bluetooth picker is up.
    pub bluetooth_picker: bool,
    /// Wallpaper picker is up.
    pub wallpaper_picker: bool,
    /// Theme picker is up.
    pub theme_picker: bool,
    /// Audio output picker is up.
    pub audio_output_picker: bool,
    /// Audio input picker is up.
    pub audio_input_picker: bool,
    /// Media players picker is up.
    pub media_players: bool,
    /// Window switcher is up.
    pub window_switcher: bool,
    /// Session lock screen is up (not a per-monitor lock shade).
    pub session_lock: bool,
    /// Per-monitor lock shade is up (not the session lock).
    pub monitor_lock: bool,
    /// Compositor debug HUD overlay is on.
    pub debug_hud: bool,
}

/// Last runtime compositor hand-off as observed by the WM. This remains
/// available when the renderer is absent, unlike renderer-owned metrics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CompositorTransitionStatus {
    pub attempts: u64,
    pub last_requested_active: Option<bool>,
    pub last_attempt_unix_ms: Option<u64>,
    pub last_success: Option<bool>,
    pub last_error: Option<String>,
}

/// Backend-neutral live status. New fields may be added within a schema
/// version; incompatible changes require incrementing `schema_version`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RuntimeStatusV1 {
    pub schema_version: u32,
    pub version: String,
    pub backend: String,
    /// Backends compiled into this binary (Cargo backend-family features).
    pub compiled_backends: Vec<String>,
    /// Process id of the compositor, for external samplers (perf tooling).
    pub pid: u32,
    /// Heap allocations since start when built with `alloc-counter`.
    pub allocations: Option<u64>,
    pub uptime_ms: u64,
    pub health: RuntimeHealth,
    pub counts: RuntimeCounts,
    pub config: Value,
    /// Actual renderer state, independent of whether metrics are available.
    pub compositor_active: bool,
    /// Effective configured target (including the X11 environment override).
    pub compositor_configured: bool,
    /// True while native mode has leased the compositor for a shell panel.
    pub compositor_temporary: bool,
    pub compositor_transition: CompositorTransitionStatus,
    pub features: RuntimeFeatureStates,
    pub compositor_metrics: Option<Value>,
    /// Compact twin of `get_resources` for bar polls that only hit `get_status`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resources: Option<Value>,
    /// Compact twin of `get_connectivity`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connectivity: Option<Value>,
    /// Compact twin of `get_power_status`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub power: Option<Value>,
    /// Compact twin of `get_media_status`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media: Option<Value>,
    /// Compact twin of `get_notifications` (`count` / `center_open` / DND).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notifications: Option<Value>,
    /// Compact twin of `get_blur_status` (strength / enabled flags).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blur: Option<Value>,
    /// Compact twin of `get_hdr_status` (config + capable summary).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hdr: Option<Value>,
    /// Compact twin of `get_capture_status` (pending / dmabuf flags).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture: Option<Value>,
    /// Compact twin of `get_idle_status` (inhibited / dimmed / locked).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle: Option<Value>,
    /// Compact twin of `get_recording_status` (active / elapsed / target).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recording: Option<Value>,
    /// Compact twin of `get_audio_recording_status`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_recording: Option<Value>,
    /// Compact twin of `get_clipboard` (`enabled` / `count` / capacity).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clipboard: Option<Value>,
    /// Compact twin of `get_waterlily` / `get_waterlily_status`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waterlily: Option<Value>,
    /// Compact twin of `get_night_light`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub night_light: Option<Value>,
    /// Compact twin of `get_magnifier`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub magnifier: Option<Value>,
    /// Compact twin of `get_peek`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peek: Option<Value>,
    /// Compact twin of expose activity (from effect / feature state).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expose: Option<Value>,
    /// Compact twin of `get_gesture`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gesture: Option<Value>,
    /// Compact twin of `get_wayland`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wayland: Option<Value>,
    /// Compact twin of `get_dnd`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dnd: Option<Value>,
    /// Compact twin of `get_session_lock`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_lock: Option<Value>,
    /// Compact twin of `get_tearing` / `get_tearing_hints`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tearing: Option<Value>,
    /// Compact twin of `get_xwayland` / `get_xwayland_status`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub xwayland: Option<Value>,
    /// Compact twin of `get_scrolling` / `get_scrolling_status`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scrolling: Option<Value>,
    /// Compact twin of `get_color_management`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color_management: Option<Value>,
    /// Compact twin of `get_audio` / `get_audio_devices`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<Value>,
    /// Compact twin of `get_wallpaper` / `get_wallpaper_colors`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wallpaper: Option<Value>,
    /// Compact twin of `get_bluetooth` / `get_bluetooth_pairing`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bluetooth: Option<Value>,
    /// Compact twin of `get_system_ui` / `get_ui`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_ui: Option<Value>,
    /// Compact twin of `get_layout` (focused monitor layout symbol).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layout: Option<Value>,
    /// Compact twin of `get_tab_bar` / `get_tabs`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tabs: Option<Value>,
    /// Compact twin of `get_struts`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub struts: Option<Value>,
    /// Compact twin of `get_scratchpads` / `get_pads`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scratchpads: Option<Value>,
    /// Compact twin of `get_gaps`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gaps: Option<Value>,
    /// Compact twin of `get_mfact`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mfact: Option<Value>,
    /// Compact twin of `get_nmaster`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nmaster: Option<Value>,
    /// Compact twin of `get_show_bar` / `get_bar` / `get_bar_visible` /
    /// `get_owns_output` / `get_visible_fullscreen` / `get_vf` (preference,
    /// occupancy, visible-fullscreen count).
    /// Nested object also carries `tag`, `prev_tag`, `layout`, `gap`,
    /// `mfact`, `nmaster`, and `selected_id`.
    /// Also `sel_tags`, `previous_tags`, and `active_tags`.
    /// Also `window_count`.
    /// Also `on_view_count`.
    /// Also `floating_count`.
    /// Also `minimized_count`.
    /// Also `sticky_count`.
    /// Also `urgent_count`.
    /// Also `fullscreen_count`.
    /// Also `pip_count`.
    /// Also `maximized_count`.
    /// Also `above_count`.
    /// Also `below_count`.
    /// Also `scratchpad_count`.
    /// Also `tabbed_count`.
    /// Also `dock_count`.
    /// Also `desktop_count`.
    /// Also `never_focus_count`.
    /// Also `skip_taskbar_count`.
    /// Also `skip_pager_count`.
    /// Also `no_decorations_count`.
    /// Also `drag_float_count`.
    /// Also `swallowed_count`.
    /// Also `demands_attention_count`.
    /// Also `fixed_count`.
    /// Also `strut_count`.
    /// Also `maximize_promoted_count`.
    /// Also `status_bar_count`.
    /// Also `prev_layout`.
    /// Also `closed_placement_count`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_bar: Option<Value>,
    /// Compact twin of `get_metrics` / `get_perf` (renderer metrics when any).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<Value>,
    /// Compact twin of `get_version` / `get_ver`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_info: Option<Value>,
    /// Compact twin of `get_monitors` / `get_mons`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monitors: Option<Value>,
    /// Compact twin of `get_workspaces` / `get_ws`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspaces: Option<Value>,
    /// Compact twin of `get_windows` / `get_wins` (count summary).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub windows: Option<Value>,
    /// Compact twin of `get_tree` (monitor node count).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tree: Option<Value>,
    /// Compact twin of `get_focused_window` / `get_fw`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focused: Option<Value>,
    /// Compact twin of `get_cfact` / `get_cf`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cfact: Option<Value>,
    /// Compact twin of `get_prev_layout` / `get_pl`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev_layout: Option<Value>,
    /// Compact twin of `get_effect_status` / `get_fx`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effects: Option<Value>,
    /// Compact twin of `get_mic_mute` / `get_mute`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mic: Option<Value>,
    /// Compact twin of `get_capabilities` / `get_caps`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Value>,
    /// Compact twin of `get_selected` / `get_sel`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected: Option<Value>,
    /// Compact twin of `benchmark_report` / `get_bench` / `get_bm`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bench: Option<Value>,
    /// Window rollup: floating count / focused floating id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub floating: Option<Value>,
    /// Window rollup: minimized count / focused minimized id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimized: Option<Value>,
    /// Window rollup: sticky count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sticky: Option<Value>,
    /// Window rollup: urgent count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub urgent: Option<Value>,
    /// Window rollup: fullscreen count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fullscreen: Option<Value>,
    /// Window rollup: pip count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pip: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IpcCapabilitiesV1 {
    pub schema_version: u32,
    pub commands: Vec<String>,
    pub queries: Vec<String>,
    pub subscription_topics: Vec<String>,
}

/// Return stable, sorted discovery data suitable for IPC serialization.
#[must_use]
pub fn ipc_capabilities() -> IpcCapabilitiesV1 {
    let mut commands = IPC_REGISTRY
        .dispatch_commands
        .iter()
        .chain(IPC_REGISTRY.special_commands)
        .map(|name| (*name).to_string())
        .collect::<Vec<_>>();
    commands.sort_unstable();
    commands.dedup();

    IpcCapabilitiesV1 {
        schema_version: 1,
        commands,
        queries: IPC_REGISTRY
            .queries
            .iter()
            .map(|name| (*name).to_string())
            .collect(),
        subscription_topics: IPC_REGISTRY
            .subscription_topics
            .iter()
            .map(|name| (*name).to_string())
            .collect(),
    }
}

#[must_use]
pub fn is_supported_query(name: &str) -> bool {
    IPC_REGISTRY.queries.contains(&name)
}

// ---------------------------------------------------------------------------
// Query result types
// ---------------------------------------------------------------------------

/// Content rectangle projected over IPC (`x`/`y`/`w`/`h` naming matches
/// [`WindowInfo`] geometry fields).
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct RectIpc {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// ICCCM / xdg size hints projected over IPC when the client has valid hints.
#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub struct SizeHintsIpc {
    pub base_w: i32,
    pub base_h: i32,
    pub inc_w: i32,
    pub inc_h: i32,
    pub max_w: i32,
    pub max_h: i32,
    pub min_w: i32,
    pub min_h: i32,
    pub min_aspect: f32,
    pub max_aspect: f32,
}

#[derive(Debug, Serialize)]
pub struct WindowInfo {
    pub id: u64,
    pub name: String,
    pub class: String,
    pub instance: String,
    pub tags: u32,
    pub monitor: i32,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub is_floating: bool,
    pub is_fullscreen: bool,
    pub is_urgent: bool,
    pub is_sticky: bool,
    /// `_NET_WM_STATE_ABOVE` / keep-above.
    pub is_above: bool,
    /// `_NET_WM_STATE_BELOW` / keep-below. Exclusive with `is_above`.
    pub is_below: bool,
    pub is_pip: bool,
    /// Both maximize axes are set (what xdg/wlr call maximized).
    pub is_maximized: bool,
    pub is_maximized_vert: bool,
    pub is_maximized_horz: bool,
    /// Maximize was promoted out of the tiling grid
    /// (`ClientState::maximize_restore_tiled`).
    pub maximize_promoted: bool,
    /// Pre-maximize content rectangle when a maximize axis is set; omitted
    /// otherwise. Same convention as `x`/`y`/`w`/`h`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maximize_restore: Option<RectIpc>,
    /// True only for JWM's semantic minimized state. Windows parked off-screen
    /// because their tag is not selected are not minimized.
    pub is_minimized: bool,
    /// Dock / iconic restore order (`ClientState::minimized_order`); `0` when
    /// not minimized.
    pub minimized_order: u64,
    /// True when this client is a terminal swallowed by a child (excluded from
    /// arrange / visibility until the child unmaps).
    pub is_swallowed: bool,
    /// Window id of the parent terminal this client is swallowing; omitted
    /// when this client is not a swallowing child.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub swallowing: Option<u64>,
    /// Window id of the child that swallowed this terminal; omitted when this
    /// client is not currently swallowed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub swallowed_by: Option<u64>,
    /// `WM_TRANSIENT_FOR` / xdg parent window id when known; omitted otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transient_for: Option<u64>,
    /// True when this window is a member of the monitor's window-tab strip.
    pub is_tabbed: bool,
    /// Index within the monitor's tab group when [`Self::is_tabbed`]; omitted
    /// otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tab_index: Option<usize>,
    /// True when the window's tags intersect the active tags on its monitor
    /// (or it is sticky): on the current view, not merely mapped.
    pub is_on_view: bool,
    /// True when this client is a named scratchpad (shown or parked).
    pub is_scratchpad: bool,
    /// Size-hints fixed: the client refuses resize (never maximized, floats).
    pub is_fixed: bool,
    /// Managed `_NET_WM_WINDOW_TYPE_DOCK` / panel chrome.
    pub is_dock: bool,
    /// Managed `_NET_WM_WINDOW_TYPE_DESKTOP` / desktop icon layer.
    pub is_desktop: bool,
    /// Floated only because the user dragged/resized it out of the tiling
    /// grid (`ClientState::is_drag_floating`); re-applying a layout pulls
    /// these back under management.
    pub is_drag_floating: bool,
    /// `WM_HINTS` input flag false / never-focus chrome (`ClientState::never_focus`).
    pub never_focus: bool,
    /// `_NET_WM_STATE_SKIP_TASKBAR` / equivalent.
    pub skip_taskbar: bool,
    /// `_NET_WM_STATE_SKIP_PAGER` / equivalent.
    pub skip_pager: bool,
    /// Client asked for undecorated chrome (`ClientState::no_decorations`).
    pub no_decorations: bool,
    /// `_NET_WM_STATE_DEMANDS_ATTENTION` (distinct from urgency/`is_urgent`).
    pub demands_attention: bool,
    /// True when this window contributes an `_NET_WM_STRUT(_PARTIAL)`
    /// reservation that shrinks a monitor's work area.
    pub has_strut: bool,
    /// Per-window client factor (`ClientState::client_fact`), the twin of
    /// workspace `m_fact` for tiled share.
    pub client_fact: f32,
    /// Drawn border width in pixels (`ClientGeometry::border_w`).
    pub border_w: i32,
    pub is_focused: bool,
    /// Process id when the backend reported one (`_NET_WM_PID` / Wayland
    /// credentials); `None` when unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Output connector / `OutputIdentity.stable_key` for the window's
    /// monitor when known; omitted when the live output map has no identity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connector: Option<String>,
    /// EDID monitor name for the window's monitor when known; omitted when
    /// the live output map has no identity or the EDID did not advertise a
    /// name. Same field as [`MonitorInfoIpc::monitor_name`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monitor_name: Option<String>,
    /// Scratchpad binding name when [`Self::is_scratchpad`]; omitted otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scratchpad: Option<String>,
    /// Current layout on the window's monitor (`TILE`, `MONOCLE`, …), matching
    /// [`MonitorInfoIpc::layout`]. Omitted when the window has no monitor
    /// (a parked scratchpad).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layout: Option<String>,
    /// Valid ICCCM / xdg size hints when known; omitted when the client has
    /// none or they have not been fetched yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_hints: Option<SizeHintsIpc>,
    /// Resting float rectangle (`ClientGeometry::floating_*`). Survives
    /// maximize / fullscreen / tag park so a script can restore the user's
    /// last free placement without guessing from live `x`/`y`/`w`/`h`.
    pub float_rect: RectIpc,
    /// Previous layout / fullscreen rectangle (`ClientGeometry::old_*`),
    /// the twin of live `x`/`y`/`w`/`h` that `resizeclient` uses.
    pub old_geometry: RectIpc,
    /// Border width remembered for fullscreen return (`old_border_w`).
    pub old_border_w: i32,
    /// Geometry parked while minimized or off-view (`hidden_restore_rect`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hidden_restore: Option<RectIpc>,
    /// Neighbor this maximize was promoted against, as a window id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maximize_restore_anchor: Option<u64>,
    /// Sticky bit to restore when leaving PiP.
    pub pip_restore_sticky: bool,
    /// Floating bit remembered under fullscreen / PiP (`old_state`).
    pub old_state: bool,
    /// Eligible for closed-placement memory.
    pub remembers_closed_placement: bool,
    /// Layer-shell exclusive zone when dock layer info is present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dock_exclusive_zone: Option<i32>,
    pub dock_anchor_top: bool,
    pub dock_anchor_bottom: bool,
    pub dock_anchor_left: bool,
    pub dock_anchor_right: bool,
    /// Matches the configured status-bar name (title/class/instance).
    pub is_status_bar: bool,
    /// Off-screen park X when minimized / off-view (`ClientGeometry::hidden_x`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hidden_x: Option<i32>,
    /// XSync counter id when the client advertises one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_counter: Option<u32>,
    /// Last observed XSync counter value (`0` when unused).
    pub sync_value: u64,
    /// Outer width including borders (`w + 2 * border_w`).
    pub total_w: i32,
    /// Outer height including borders (`h + 2 * border_w`).
    pub total_h: i32,
    /// Index within the monitor's client list (`monitor_clients`); omitted
    /// when the window has no monitor (parked scratchpad).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack_index: Option<usize>,
    /// True when this window is a visible fullscreen client on the current
    /// view — it owns the output and tucks the status bar. Swallowed
    /// terminals never own the output.
    pub owns_output: bool,
}

#[derive(Debug, Serialize)]
pub struct WorkspaceInfo {
    pub tag_mask: u32,
    pub tag_index: usize,
    pub monitor: i32,
    pub layout: String,
    pub m_fact: f32,
    pub n_master: u32,
    /// Tiling gap in pixels for this tag (`Pertag.gaps` / `MonitorLayout.gap`).
    pub gap: i32,
    pub num_clients: usize,
    pub focused: bool,
    /// True when any client on this tag (on this monitor) demands attention
    /// / is urgent. Sticky all-tags clients are excluded, matching the
    /// status-bar urgent mask.
    pub is_urgent: bool,
    /// True when at least one non-sticky client on this monitor carries this
    /// tag bit (status-bar occupied mask).
    pub is_occupied: bool,
    /// True when any client on this tag on this monitor is fullscreen.
    pub has_fullscreen: bool,
    /// True when a non-hidden fullscreen client on this tag is on the
    /// current view (same predicate that hides the status bar).
    pub has_visible_fullscreen: bool,
    /// Output connector / `OutputIdentity.stable_key` for this workspace's
    /// monitor when known; omitted when the live output map has no identity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connector: Option<String>,
    /// EDID monitor name for this workspace's monitor when known; omitted
    /// when unknown. Same field as [`MonitorInfoIpc::monitor_name`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monitor_name: Option<String>,
    /// Whether the status bar is shown for this tag (`Pertag.show_bars`).
    pub show_bar: bool,
    /// Previous layout symbol for this tag (`Pertag.prev_lts`).
    pub prev_layout: String,
    /// Selected client window id on this tag when any (`Pertag.sel`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_id: Option<u64>,
    /// How many clients on this tag on this monitor are minimized.
    pub minimized_count: usize,
    /// How many clients on this tag on this monitor are floating.
    pub floating_count: usize,
    /// How many sticky clients on this monitor also carry this tag bit.
    pub sticky_count: usize,
    /// How many clients on this tag on this monitor are urgent /
    /// demand attention.
    pub urgent_count: usize,
    /// How many clients on this tag on this monitor are fullscreen.
    pub fullscreen_count: usize,
    /// How many clients on this tag on this monitor are picture-in-picture.
    pub pip_count: usize,
    /// How many clients on this tag on this monitor are maximized on either axis.
    pub maximized_count: usize,
    /// How many clients on this tag on this monitor are keep-above.
    pub above_count: usize,
    /// How many clients on this tag on this monitor are keep-below.
    pub below_count: usize,
    /// How many clients on this tag on this monitor are size-hints fixed.
    pub fixed_count: usize,
    /// How many clients on this tag on this monitor are named scratchpads.
    pub scratchpad_count: usize,
    /// How many clients on this tag on this monitor are in the window-tab strip.
    pub tabbed_count: usize,
    /// How many clients on this tag on this monitor are docks / panels.
    pub dock_count: usize,
    /// How many clients on this tag on this monitor are desktop-layer chrome.
    pub desktop_count: usize,
    /// How many clients on this tag on this monitor are never-focus chrome.
    pub never_focus_count: usize,
    /// How many clients on this tag on this monitor demand attention
    /// (`demands_attention`, distinct from urgency).
    pub demands_attention_count: usize,
    /// How many clients on this tag on this monitor skip the taskbar.
    pub skip_taskbar_count: usize,
    /// How many clients on this tag on this monitor skip the pager.
    pub skip_pager_count: usize,
    /// How many clients on this tag on this monitor asked for no decorations.
    pub no_decorations_count: usize,
    /// How many clients on this tag on this monitor are hand-floated
    /// (`is_drag_floating`).
    pub drag_float_count: usize,
    /// How many clients on this tag on this monitor are swallowed.
    pub swallowed_count: usize,
    /// How many clients on this tag on this monitor are currently on-view.
    pub on_view_count: usize,
    /// How many clients on this tag on this monitor were maximize-promoted.
    pub maximize_promoted_count: usize,
    /// How many clients on this tag on this monitor publish a strut.
    pub strut_count: usize,
    /// How many clients on this tag on this monitor are the status bar.
    pub status_bar_count: usize,
    /// How many clients on this tag on this monitor remember closed placement.
    pub closed_placement_count: usize,
    /// How many visible fullscreen clients own this tag's output. Zero when
    /// the tag is off-view; otherwise the hide-bar occupancy count.
    pub owns_output_count: usize,
}

#[derive(Debug, Serialize)]
pub struct MonitorInfoIpc {
    pub num: i32,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// Work area (bar / struts / tab bar excluded), matching internal `w_*`
    /// geometry naming compressed like `x`/`y`/`w`/`h` for the full output.
    pub wx: i32,
    pub wy: i32,
    pub ww: i32,
    pub wh: i32,
    pub active_tags: u32,
    pub layout: String,
    pub focused: bool,
    /// Whether this monitor is behind a lock shade. A status bar has no other
    /// way to tell a dark monitor from a locked one, and `focused` cannot say
    /// it: a locked monitor is never the focused one.
    pub locked: bool,
    /// Output connector / `OutputIdentity.stable_key` when known; omitted when
    /// the live output map has no identity for this monitor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connector: Option<String>,
    /// Backend output name (`OutputInfo.name` / wl_output name) when known;
    /// omitted when the live output map has no entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// EDID monitor name when known; omitted when the live output map has no
    /// identity or the EDID did not advertise a name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monitor_name: Option<String>,
    /// EDID vendor string when known; omitted otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    /// EDID product code when known; omitted otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub product_code: Option<u16>,
    /// EDID numeric serial when known; omitted otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial_number: Option<u32>,
    /// EDID monitor serial string when known; omitted otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monitor_serial: Option<String>,
    /// Fractional scale from the live output (`OutputInfo.scale`), typically
    /// `1.0` / `1.25` / `1.5` / `2.0`. `1.0` when the output map has no entry.
    pub scale: f32,
    /// Mode refresh in millihertz (`OutputInfo.refresh_rate`); `60000` is
    /// 60 Hz. `0` when the output map has no entry.
    pub refresh_mhz: u32,
    /// Whether the live output advertised HDR capability (`OutputInfo.hdr_capable`).
    pub hdr_capable: bool,
    /// Whether VRR is supported on this output (`query_vrr_capabilities`).
    pub vrr_supported: bool,
    /// Whether VRR is currently enabled on this output.
    pub vrr_enabled: bool,
    /// Current tiling gap in pixels on this monitor (`MonitorLayout.gap`).
    pub gap: i32,
    /// Live master area factor (`MonitorLayout.m_fact`).
    pub m_fact: f32,
    /// Live master-client count (`MonitorLayout.n_master`).
    pub n_master: u32,
    /// `wl_output` transform (`OutputInfo.transform`, `0..=7`; `0` = normal).
    pub transform: i32,
    /// Pixels reserved at the top of the work area for the window tab bar;
    /// `0` when the strip is not shown on this monitor.
    pub tab_bar_reserved: i32,
    /// EDID HDR static metadata subset when the live output advertised one;
    /// omitted when SDR / unknown. Twin of `get_hdr_status` per-output
    /// `metadata` without the delivery-plan fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hdr_metadata: Option<HdrMetadataIpc>,
    /// Physical panel width in millimetres (`0` when unknown).
    pub physical_width_mm: i32,
    /// Physical panel height in millimetres (`0` when unknown).
    pub physical_height_mm: i32,
    /// Preferred mode width in pixels (`0` when unknown / same as current).
    pub preferred_width: i32,
    /// Preferred mode height in pixels (`0` when unknown / same as current).
    pub preferred_height: i32,
    /// Preferred mode refresh in millihertz (`0` when unknown).
    pub preferred_refresh_mhz: u32,
    /// VRR minimum refresh rate in Hz (`0` when unknown / unsupported).
    pub vrr_min_hz: u32,
    /// VRR maximum refresh rate in Hz (`0` when unknown / unsupported).
    pub vrr_max_hz: u32,
    /// Previous layout symbol on this monitor (`WMMonitor.prev_lt`).
    pub prev_layout: String,
    /// Whether the status bar is shown for the current tag
    /// (`Pertag.show_bars`). Preference only: a visible fullscreen client
    /// can still hide the bar window without flipping this bit.
    pub show_bar: bool,
    /// Whether the status-bar window currently occupies this output.
    /// False while a visible fullscreen client owns the monitor even if
    /// [`Self::show_bar`] is still true.
    pub bar_visible: bool,
    /// True when a non-hidden client on this monitor is fullscreen and on
    /// the current view — the same predicate that tucks the status bar.
    pub has_visible_fullscreen: bool,
    /// External strut reservation on this monitor (top/bottom/left/right).
    pub strut_top: i32,
    pub strut_bottom: i32,
    pub strut_left: i32,
    pub strut_right: i32,
    /// Selected client window id on this monitor (`WMMonitor.sel`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_id: Option<u64>,
    /// Dual tagset index (`WMMonitor.sel_tags` & 1): `0` or `1`.
    pub sel_tags: usize,
    /// Inactive tagset mask (`tag_set[1 - sel_tags]`), the twin of
    /// [`Self::active_tags`].
    pub previous_tags: u32,
    /// Pertag current tag index (`Pertag.cur_tag`; `0` = all-tags slot).
    pub cur_tag: usize,
    /// Pertag previous tag index (`Pertag.prev_tag`).
    pub prev_tag: usize,
    /// Physical connector name (`OutputIdentity.connector`) when known;
    /// omitted when the live output map has no identity. May differ from
    /// [`Self::connector`] when `stable_key` is an EDID-derived key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_connector: Option<String>,
    /// Layout symbol shown on the status bar (`WMMonitor.lt_symbol`).
    pub lt_symbol: String,
    /// Backend output id (`OutputInfo.id`) when the live output map has an
    /// entry; omitted otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_id: Option<u64>,
    /// How many clients are attached to this monitor.
    pub window_count: usize,
    /// How many of those clients are floating.
    pub floating_count: usize,
    /// How many of those clients are minimized.
    pub minimized_count: usize,
    /// How many of those clients are sticky.
    pub sticky_count: usize,
    /// How many of those clients are urgent / demand attention.
    pub urgent_count: usize,
    /// How many of those clients are fullscreen.
    pub fullscreen_count: usize,
    /// How many of those clients are picture-in-picture.
    pub pip_count: usize,
    /// How many of those clients are maximized on either axis.
    pub maximized_count: usize,
    /// How many of those clients are keep-above.
    pub above_count: usize,
    /// How many of those clients are keep-below.
    pub below_count: usize,
    /// How many of those clients are size-hints fixed.
    pub fixed_count: usize,
    /// How many of those clients are named scratchpads.
    pub scratchpad_count: usize,
    /// How many of those clients are in the window-tab strip.
    pub tabbed_count: usize,
    /// How many of those clients are docks / panels.
    pub dock_count: usize,
    /// How many of those clients are desktop-layer chrome.
    pub desktop_count: usize,
    /// How many of those clients are never-focus chrome.
    pub never_focus_count: usize,
    /// How many of those clients demand attention (not merely urgent).
    pub demands_attention_count: usize,
    /// How many of those clients skip the taskbar.
    pub skip_taskbar_count: usize,
    /// How many of those clients skip the pager.
    pub skip_pager_count: usize,
    /// How many of those clients asked for no decorations.
    pub no_decorations_count: usize,
    /// How many of those clients are hand-floated (`is_drag_floating`).
    pub drag_float_count: usize,
    /// How many of those clients are swallowed.
    pub swallowed_count: usize,
    /// How many of those clients are currently on-view.
    pub on_view_count: usize,
    /// How many of those clients were maximize-promoted.
    pub maximize_promoted_count: usize,
    /// How many of those clients publish a strut.
    pub strut_count: usize,
    /// How many of those clients are the status bar.
    pub status_bar_count: usize,
    /// How many of those clients remember closed placement.
    pub closed_placement_count: usize,
    /// How many of those clients currently own the output (visible fullscreen
    /// on the current view, swallowed terminals excluded — the hide-bar
    /// occupancy).
    pub owns_output_count: usize,
}

/// EDID HDR static metadata projected on [`MonitorInfoIpc`] and status queries.
#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub struct HdrMetadataIpc {
    pub max_luminance_nits: f32,
    pub min_luminance_nits: f32,
    /// `0.0` means the display did not state one (unknown, not zero nits).
    pub max_frame_average_nits: f32,
    pub supports_pq: bool,
    pub supports_hlg: bool,
    pub supports_bt2020: bool,
}

#[derive(Debug, Serialize)]
pub struct TreeNode {
    pub monitor: MonitorInfoIpc,
    pub windows: Vec<WindowInfo>,
    /// This monitor's selected client window id (`WMMonitor.sel`), when any.
    /// Distinct from each window's `is_focused` (global input focus).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_id: Option<u64>,
    /// `windows.len()` mirror for scripts that only need a count.
    pub window_count: usize,
    /// How many of `windows` report `is_urgent`.
    pub urgent_count: usize,
    /// How many of `windows` report `is_floating`.
    pub floating_count: usize,
    /// How many of `windows` report `is_minimized`.
    pub minimized_count: usize,
    /// How many of `windows` report `is_sticky`.
    pub sticky_count: usize,
    /// How many of `windows` report `is_fullscreen`.
    pub fullscreen_count: usize,
    /// How many of `windows` report `is_pip`.
    pub pip_count: usize,
    /// How many of `windows` report `is_maximized` (both axes).
    pub maximized_count: usize,
    /// How many of `windows` report `is_above`.
    pub above_count: usize,
    /// How many of `windows` report `is_below`.
    pub below_count: usize,
    /// How many of `windows` report `is_scratchpad`.
    pub scratchpad_count: usize,
    /// How many of `windows` report `is_tabbed`.
    pub tabbed_count: usize,
    /// How many of `windows` report `is_fixed`.
    pub fixed_count: usize,
    /// How many of `windows` report `is_dock`.
    pub dock_count: usize,
    /// How many of `windows` report `is_desktop`.
    pub desktop_count: usize,
    /// How many of `windows` report `never_focus`.
    pub never_focus_count: usize,
    /// How many of `windows` report `demands_attention`.
    pub demands_attention_count: usize,
    /// How many of `windows` report `skip_taskbar`.
    pub skip_taskbar_count: usize,
    /// How many of `windows` report `skip_pager`.
    pub skip_pager_count: usize,
    /// How many of `windows` report `no_decorations`.
    pub no_decorations_count: usize,
    /// How many of `windows` report `is_drag_floating`.
    pub drag_float_count: usize,
    /// How many of `windows` report `is_swallowed`.
    pub swallowed_count: usize,
    /// How many of `windows` report `is_on_view`.
    pub on_view_count: usize,
    /// How many of `windows` report `maximize_promoted`.
    pub maximize_promoted_count: usize,
    /// How many of `windows` report `has_strut`.
    pub strut_count: usize,
    /// How many of `windows` report `is_status_bar`.
    pub status_bar_count: usize,
    /// How many of `windows` currently own the output (visible fullscreen
    /// on the current view, hide-bar occupancy).
    pub owns_output_count: usize,
}

// ---------------------------------------------------------------------------
// Command dispatch — maps command name → (WMFuncType, WMArgEnum)
// ---------------------------------------------------------------------------

pub fn dispatch_command(name: &str, args: &Value) -> Result<(WMFuncType, WMArgEnum), String> {
    match name {
        // --- Window management ---
        "focusstack" | "focus_stack" => {
            Ok((Jwm::focusstack as WMFuncType, parse_int_arg(args, 1)?))
        }
        "app_launcher" | "launcher" => Ok((Jwm::app_launcher as WMFuncType, WMArgEnum::Int(0))),
        "control_center" | "hub" => Ok((Jwm::control_center as WMFuncType, WMArgEnum::Int(0))),
        "notification_center" | "notif_center" => {
            Ok((Jwm::notification_center as WMFuncType, WMArgEnum::Int(0)))
        }
        "media_play_pause" | "play" => Ok((Jwm::media_play_pause as WMFuncType, WMArgEnum::Int(0))),
        "media_next" | "next" => Ok((Jwm::media_next as WMFuncType, WMArgEnum::Int(0))),
        "media_previous" | "prev" => Ok((Jwm::media_previous as WMFuncType, WMArgEnum::Int(0))),
        "media_stop" | "stop" => Ok((Jwm::media_stop as WMFuncType, WMArgEnum::Int(0))),
        "session_menu" | "session" => Ok((Jwm::session_menu as WMFuncType, WMArgEnum::Int(0))),
        "toggle_night_light" | "night" => {
            Ok((Jwm::toggle_night_light as WMFuncType, WMArgEnum::Int(0)))
        }
        "toggle_wifi" | "twifi" => Ok((Jwm::toggle_wifi as WMFuncType, WMArgEnum::Int(0))),
        "wifi_picker" | "wifi" => Ok((Jwm::wifi_picker as WMFuncType, WMArgEnum::Int(0))),
        "audio_output_picker" | "aout" => {
            Ok((Jwm::audio_output_picker as WMFuncType, WMArgEnum::Int(0)))
        }
        "audio_input_picker" | "ain" => {
            Ok((Jwm::audio_input_picker as WMFuncType, WMArgEnum::Int(0)))
        }
        "bluetooth_picker" | "bt" => Ok((Jwm::bluetooth_picker as WMFuncType, WMArgEnum::Int(0))),
        "calendar" | "cal" => Ok((Jwm::calendar as WMFuncType, WMArgEnum::Int(0))),
        "clipboard_picker" | "clip" => Ok((Jwm::clipboard_picker as WMFuncType, WMArgEnum::Int(0))),
        "wallpaper_picker" | "wall" => Ok((Jwm::wallpaper_picker as WMFuncType, WMArgEnum::Int(0))),
        "toggle_bluetooth" | "tbt" => Ok((Jwm::toggle_bluetooth as WMFuncType, WMArgEnum::Int(0))),
        "monitor_layout" | "monlayout" => {
            Ok((Jwm::monitor_layout as WMFuncType, WMArgEnum::Int(0)))
        }
        "lock_screen" | "lock" => Ok((Jwm::lock_screen as WMFuncType, WMArgEnum::Int(0))),
        // `-1` — the default — is the monitor in use for `lock_monitor`, and
        // the most recently locked one for `unlock_monitor`.
        "lock_monitor" => Ok((Jwm::lock_monitor as WMFuncType, parse_int_arg(args, -1)?)),
        "unlock_monitor" | "unlock" => {
            Ok((Jwm::unlock_monitor as WMFuncType, parse_int_arg(args, -1)?))
        }
        "killclient" | "kill_client" | "kill" => Ok((Jwm::killclient, parse_int_arg(args, 0)?)),
        "minimize" | "minimize_window" => Ok((Jwm::minimize, parse_int_arg(args, 0)?)),
        "zoom" | "zoom_master" => Ok((Jwm::zoom, parse_int_arg(args, 0)?)),
        "togglefloating" | "toggle_floating" | "floating" => {
            Ok((Jwm::togglefloating, parse_int_arg(args, 0)?))
        }
        "togglesticky" | "toggle_sticky" | "sticky" => {
            Ok((Jwm::togglesticky, parse_int_arg(args, 0)?))
        }
        "togglepip" | "toggle_pip" | "pip" => Ok((Jwm::togglepip, parse_int_arg(args, 0)?)),
        "togglemaximize" | "toggle_maximize" | "maximize" => {
            Ok((Jwm::togglemaximize, parse_int_arg(args, 0)?))
        }
        "togglescratchpad" | "toggle_scratchpad" | "pad" => {
            let cmd = if argument_is_omitted(args) {
                vec!["term".to_string()]
            } else {
                parse_string_vec_arg(args).map_err(|e| format!("togglescratchpad: {e}"))?
            };
            Ok((Jwm::togglescratchpad, WMArgEnum::StringVec(cmd)))
        }
        "movestack" | "move_stack" => Ok((Jwm::movestack, parse_int_arg(args, 1)?)),
        "focus_none" | "unfocus" => Ok((Jwm::focus_none, parse_int_arg(args, 0)?)),
        "focus_window" | "fwin" => Ok((Jwm::focus_window, parse_window_id_arg(args)?)),
        "focus_tab" | "ftab" => {
            let cmd = if argument_is_omitted(args) {
                vec!["0".to_string(), "0".to_string()]
            } else {
                parse_string_vec_arg(args).map_err(|e| format!("focus_tab: {e}"))?
            };
            Ok((Jwm::focus_tab, WMArgEnum::StringVec(cmd)))
        }
        "refocus" => Ok((Jwm::refocus, parse_int_arg(args, 0)?)),
        "snap_window" | "snap" => {
            let direction = parse_snap_direction_arg(args)?;
            Ok((Jwm::snap_window, WMArgEnum::StringVec(vec![direction])))
        }

        // --- Layout ---
        "setmfact" | "set_mfact" => Ok((Jwm::setmfact, parse_float_arg(args, 0.0)?)),
        "setgaps" | "set_gaps" => Ok((Jwm::setgaps, parse_int_arg(args, 1)?)),
        "setcfact" | "set_cfact" => Ok((Jwm::setcfact, parse_float_arg(args, 0.0)?)),
        "incnmaster" | "inc_nmaster" => Ok((Jwm::incnmaster, parse_int_arg(args, 1)?)),
        "setnmaster" | "set_nmaster" => Ok((Jwm::setnmaster, parse_int_arg(args, 1)?)),
        "scrolling_toggle_attach_mode" | "attach" => {
            Ok((Jwm::scrolling_toggle_attach_mode, parse_int_arg(args, 0)?))
        }
        "scrolling_focus_column" | "scol" => {
            Ok((Jwm::scrolling_focus_column, parse_int_arg(args, 1)?))
        }
        "scrolling_move_column" | "smov" => {
            Ok((Jwm::scrolling_move_column, parse_int_arg(args, 1)?))
        }
        "scrolling_focus_window" | "swin" => {
            Ok((Jwm::scrolling_focus_window, parse_int_arg(args, 1)?))
        }
        "scrolling_consume" | "scons" => Ok((Jwm::scrolling_consume, parse_int_arg(args, 1)?)),
        "scrolling_expel" | "sexp" => Ok((Jwm::scrolling_expel, parse_int_arg(args, 1)?)),
        "setlayout" | "set_layout" => {
            let layout = parse_layout_arg(args)?;
            Ok((Jwm::setlayout, layout))
        }
        "lastlayout" | "last_layout" | "last" => Ok((Jwm::lastlayout, parse_int_arg(args, 0)?)),
        "cyclelayout" | "cycle_layout" | "clayout" => {
            Ok((Jwm::cyclelayout, parse_int_arg(args, 1)?))
        }
        "layout_picker" | "layouts" => Ok((Jwm::layout_picker, parse_int_arg(args, 0)?)),
        "togglebar" | "toggle_bar" | "bar" => Ok((Jwm::togglebar, parse_int_arg(args, 0)?)),

        // --- Tags ---
        "view" => Ok((Jwm::view, parse_configured_tag_mask_arg("view", args)?)),
        "tag" => Ok((Jwm::tag, parse_configured_tag_mask_arg("tag", args)?)),
        "toggleview" | "toggle_view" => Ok((
            Jwm::toggleview,
            parse_configured_tag_mask_arg("toggleview", args)?,
        )),
        "toggletag" | "toggle_tag" => Ok((
            Jwm::toggletag,
            parse_configured_tag_mask_arg("toggletag", args)?,
        )),
        "loopview" | "loop_view" | "loop" => Ok((Jwm::loopview, parse_int_arg(args, 1)?)),
        "window_switcher" | "switcher" => Ok((Jwm::window_switcher, parse_int_arg(args, 1)?)),

        // --- Monitor ---
        "focusmon" | "focus_mon" => Ok((Jwm::focusmon, parse_int_arg(args, 1)?)),
        "tagmon" | "tag_mon" => Ok((Jwm::tagmon, parse_int_arg(args, 1)?)),

        // --- Spawn ---
        "spawn" => {
            let cmd = parse_string_vec_arg(args).map_err(|e| format!("spawn: {e}"))?;
            Ok((Jwm::spawn, WMArgEnum::StringVec(cmd)))
        }

        // --- Capture ---
        // The status bars' screenshot pill comes through here, which is why
        // both of these are dispatch commands rather than key bindings only:
        // a bar asks the compositor that owns the screen to run its own
        // capture, instead of shelling out to whatever external grabber
        // happens to be installed.
        "take_screenshot" | "screenshot" => {
            Ok((Jwm::take_screenshot as WMFuncType, WMArgEnum::Int(0)))
        }
        "take_screenshot_fullscreen" | "screenshot_fullscreen" => Ok((
            Jwm::take_screenshot_fullscreen as WMFuncType,
            WMArgEnum::Int(0),
        )),

        // --- Misc ---
        "quit" | "exit" => Ok((Jwm::quit, parse_int_arg(args, 0)?)),
        "restart" | "reload_wm" => Ok((Jwm::restart, parse_int_arg(args, 0)?)),
        "togglecompositor" | "toggle_compositor" | "comp" => {
            Ok((Jwm::togglecompositor, parse_int_arg(args, 0)?))
        }
        "togglepartialdamage" | "toggle_partial_damage" | "damage" => {
            Ok((Jwm::togglepartialdamage, parse_int_arg(args, 0)?))
        }
        "toggle_waterlily" | "lily" => Ok((Jwm::toggle_waterlily, parse_int_arg(args, 0)?)),
        "waterlily_case" | "case" => {
            let requested = if argument_is_omitted(args) {
                vec!["next".to_string()]
            } else {
                parse_string_vec_arg(args).map_err(|e| format!("waterlily_case: {e}"))?
            };
            Ok((Jwm::waterlily_case, WMArgEnum::StringVec(requested)))
        }
        "waterlily_palette" | "palette" => {
            let requested = if argument_is_omitted(args) {
                vec!["next".to_string()]
            } else {
                parse_string_vec_arg(args).map_err(|e| format!("waterlily_palette: {e}"))?
            };
            Ok((Jwm::waterlily_palette, WMArgEnum::StringVec(requested)))
        }
        // Compatibility only: intentionally omitted from IPC capability discovery.
        "toggle_slime" => {
            log::warn!("IPC action `toggle_slime` is deprecated; use `toggle_waterlily` instead");
            Ok((Jwm::toggle_waterlily, parse_int_arg(args, 0)?))
        }
        "toggle_overview" | "overview" => Ok((Jwm::toggle_overview, parse_int_arg(args, 0)?)),
        "toggle_tags_overview" | "tags" => Ok((Jwm::toggle_tags_overview, parse_int_arg(args, 0)?)),
        "cycle_overview" | "cycle" => Ok((Jwm::cycle_overview, parse_int_arg(args, 1)?)),
        "toggle_magnifier" | "mag" => Ok((Jwm::toggle_magnifier, parse_int_arg(args, 0)?)),
        "toggle_peek" | "peek" => Ok((Jwm::toggle_peek, parse_int_arg(args, 0)?)),
        "toggle_annotation" | "annotate" => Ok((Jwm::toggle_annotation, parse_int_arg(args, 0)?)),
        "toggle_recording" | "record" => Ok((Jwm::toggle_recording, parse_int_arg(args, 0)?)),
        "adjust_recording_region" | "region" => {
            Ok((Jwm::adjust_recording_region, parse_int_arg(args, 0)?))
        }
        "toggle_audio_recording" | "arecord" => {
            Ok((Jwm::toggle_audio_recording, parse_int_arg(args, 0)?))
        }
        "toggle_dnd" | "toggle_do_not_disturb" => Ok((Jwm::toggle_dnd, parse_int_arg(args, 0)?)),
        "toggle_idle_inhibit" | "caffeine" => {
            Ok((Jwm::toggle_idle_inhibit, parse_int_arg(args, 0)?))
        }

        // --- Session ---
        "save_session" | "persist_session" | "save" => {
            Ok((Jwm::save_session, parse_int_arg(args, 0)?))
        }
        "restore_session" | "load_session" | "restore" => {
            Ok((Jwm::restore_session, parse_int_arg(args, 0)?))
        }

        _ => Err(format!("unknown command: {name}")),
    }
}

/// Returns whether `name` identifies an IPC command, independent of whether a
/// particular argument value is valid for that command.
#[must_use]
pub fn is_known_command(name: &str) -> bool {
    match dispatch_command(name, &Value::Null) {
        Ok(_) => true,
        Err(error) => !error.starts_with("unknown command:"),
    }
}

// ---------------------------------------------------------------------------
// Argument parsers
// ---------------------------------------------------------------------------

fn argument_is_omitted(args: &Value) -> bool {
    args.is_null() || args.as_object().is_some_and(serde_json::Map::is_empty)
}

/// Extract an optional scalar argument. `null` and `{}` preserve the historical
/// command defaults, while a non-empty object without a supported key is a
/// caller error rather than silently behaving as if no argument was supplied.
fn scalar_arg_value<'a>(
    args: &'a Value,
    keys: &[&str],
    expected: &str,
) -> Result<Option<&'a Value>, String> {
    match args {
        Value::Null => Ok(None),
        Value::Object(values) => {
            if let Some(value) = keys.iter().find_map(|key| values.get(*key)) {
                if value.is_null() {
                    Ok(None)
                } else {
                    Ok(Some(value))
                }
            } else if values.is_empty() {
                Ok(None)
            } else {
                Err(format!(
                    "expected {expected} directly or in field {}",
                    keys.iter()
                        .map(|key| format!("'{key}'"))
                        .collect::<Vec<_>>()
                        .join("/")
                ))
            }
        }
        value => Ok(Some(value)),
    }
}

fn parse_int_arg(args: &Value, default: i32) -> Result<WMArgEnum, String> {
    let Some(value) = scalar_arg_value(args, &["value", "v"], "an i32 integer")? else {
        return Ok(WMArgEnum::Int(default));
    };
    let Value::Number(number) = value else {
        return Err(format!("expected an i32 integer, got {value}"));
    };
    let parsed = if let Some(value) = number.as_i64() {
        i32::try_from(value)
    } else if let Some(value) = number.as_u64() {
        i32::try_from(value)
    } else {
        return Err(format!("expected an i32 integer, got {value}"));
    }
    .map_err(|_| format!("integer argument {value} is outside the i32 range"))?;
    Ok(WMArgEnum::Int(parsed))
}

fn parse_float_arg(args: &Value, default: f32) -> Result<WMArgEnum, String> {
    let Some(value) = scalar_arg_value(args, &["value", "v"], "a finite number")? else {
        return Ok(WMArgEnum::Float(default));
    };
    let Some(parsed) = value.as_f64() else {
        return Err(format!("expected a finite number, got {value}"));
    };
    if !parsed.is_finite() || parsed < -(f32::MAX as f64) || parsed > f32::MAX as f64 {
        return Err(format!(
            "floating-point argument {value} is outside the finite f32 range"
        ));
    }
    Ok(WMArgEnum::Float(parsed as f32))
}

/// Parse the tag mask of `view`/`tag`/`toggleview`/`toggletag` and check it
/// against the running configuration's `tagmask()`.
///
/// The shape checks run first on purpose: `is_known_command` probes every
/// command with `null`, and config validation calls it while `CONFIG` itself
/// is still being initialised, so a missing argument must be rejected before
/// the global is touched.
fn parse_configured_tag_mask_arg(command: &str, args: &Value) -> Result<WMArgEnum, String> {
    let mask = parse_tag_mask_value(command, args)?;
    validate_tag_mask(command, mask, crate::config::CONFIG.load().tagmask())?;
    Ok(WMArgEnum::UInt(mask))
}

/// Extract the required, non-zero u32 tag mask of a tag command.
///
/// Unlike the optional scalar commands there is no sensible default here:
/// JWM's `view(0)` does not mean "previous tagset" as it does in dwm, it
/// changes nothing. Treating an omitted mask as 0 made the call report
/// success while `view` still announced `tag/view` with tag 0 (no tag
/// visible) and `tag` silently left the window where it was.
fn parse_tag_mask_value(command: &str, args: &Value) -> Result<u32, String> {
    let Some(value) = scalar_arg_value(args, &["tag", "value", "v"], "a u32 tag mask")? else {
        return Err(format!(
            "{command} requires a tag mask (bit N selects tag N+1), e.g. {{\"tag\": 1}}"
        ));
    };
    let Value::Number(number) = value else {
        return Err(format!("expected a u32 tag mask, got {value}"));
    };
    let Some(parsed) = number.as_u64() else {
        return Err(format!(
            "expected a non-negative integer tag mask, got {value}"
        ));
    };
    let parsed =
        u32::try_from(parsed).map_err(|_| format!("tag mask {value} is outside the u32 range"))?;
    if parsed == 0 {
        return Err(format!("{command}: tag mask 0 selects no tag"));
    }
    Ok(parsed)
}

/// Reject a mask with no bit inside the configured tags.
///
/// Every tag executor intersects its argument with `tagmask()`, so a mask
/// whose only bits lie above `tags_length` degrades into the same silent
/// no-op as mask 0. Masks with at least one bit in range stay accepted
/// unchanged: key bindings pass `!0` for "all tags" and rely on that
/// intersection.
fn validate_tag_mask(command: &str, mask: u32, tagmask: u32) -> Result<(), String> {
    if mask & tagmask == 0 {
        return Err(format!(
            "{command}: tag mask {mask:#x} has no bit within the {} configured tags (valid bits: {tagmask:#x})",
            tagmask.count_ones()
        ));
    }
    Ok(())
}

fn parse_string_vec_arg(args: &Value) -> Result<Vec<String>, String> {
    let value = match args {
        Value::Object(values) => values.get("cmd").ok_or_else(|| {
            "expected a command string or string array in field 'cmd'".to_string()
        })?,
        value => value,
    };

    let command = match value {
        Value::String(value) => vec![value.clone()],
        Value::Array(values) => values
            .iter()
            .enumerate()
            .map(|(index, value)| {
                value
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| format!("command element {index} must be a string, got {value}"))
            })
            .collect::<Result<Vec<_>, _>>()?,
        _ => {
            return Err(format!(
                "expected a command string or string array, got {value}"
            ));
        }
    };

    let Some(program) = command.first() else {
        return Err("command array must not be empty".to_string());
    };
    if program.trim().is_empty() {
        return Err("command program must not be empty".to_string());
    }
    Ok(command)
}

fn parse_window_id_arg(args: &Value) -> Result<WMArgEnum, String> {
    let v = args
        .get("id")
        .or_else(|| args.get("value"))
        .or_else(|| args.get("v"))
        .and_then(|v| v.as_u64())
        .or_else(|| args.as_u64());
    match v {
        Some(id) => Ok(WMArgEnum::UInt64(id)),
        None => Err("focus_window requires an \"id\" argument (window id as u64)".into()),
    }
}

/// Snap directions accepted by `snap_window` — the keyboard/IPC form of the
/// mouse drop zones (left/right halves, top-edge maximize, corner quarters).
/// Kept in sync with `SnapDirection::from_name` in `jwm::layout::drag_attach`;
/// both sides pin the accepted names in their tests. Unlike most commands the
/// direction is required: snapping has no sensible default.
fn parse_snap_direction_arg(args: &Value) -> Result<String, String> {
    let Some(value) = scalar_arg_value(args, &["direction", "value", "v"], "a snap direction")?
    else {
        return Err(
            "snap_window requires a direction: \"left\", \"right\", \"maximize\", \"top-left\", \"top-right\", \"bottom-left\", or \"bottom-right\"".to_string(),
        );
    };
    let Value::String(name) = value else {
        return Err(format!(
            "snap_window expected a direction string, got {value}"
        ));
    };
    let normalized = name.to_lowercase();
    match normalized.as_str() {
        "left" | "right" | "maximize" | "top-left" | "top-right" | "bottom-left"
        | "bottom-right" => Ok(normalized),
        _ => Err(format!(
            "snap_window unknown direction {name:?}; expected \"left\", \"right\", \"maximize\", \"top-left\", \"top-right\", \"bottom-left\", or \"bottom-right\""
        )),
    }
}

fn parse_layout_arg(args: &Value) -> Result<WMArgEnum, String> {
    let name = match scalar_arg_value(args, &["layout", "value"], "a layout name")? {
        None => "tile",
        Some(Value::String(name)) if !name.trim().is_empty() => name,
        Some(value) => return Err(format!("expected a non-empty layout name, got {value}")),
    };
    let layout = match name.to_lowercase().as_str() {
        "tile" => LayoutEnum::TILE,
        "float" => LayoutEnum::FLOAT,
        "monocle" => LayoutEnum::MONOCLE,
        "fibonacci" => LayoutEnum::FIBONACCI,
        "centered_master" | "centeredmaster" => LayoutEnum::CENTERED_MASTER,
        "bstack" => LayoutEnum::BSTACK,
        "grid" => LayoutEnum::GRID,
        "deck" => LayoutEnum::DECK,
        "three_col" | "threecol" => LayoutEnum::THREE_COL,
        "tatami" => LayoutEnum::TATAMI,
        "fullscreen" => LayoutEnum::FULLSCREEN,
        "scrolling" => LayoutEnum::SCROLLING,
        "vstack" | "v_stack" => LayoutEnum::VSTACK,
        _ => return Err(format!("unknown layout: {name}")),
    };
    Ok(WMArgEnum::Layout(std::rc::Rc::new(layout)))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_command_message() {
        let json = r#"{"command": "view", "args": {"tag": 2}}"#;
        let msg: IpcMessage = serde_json::from_str(json).unwrap();
        assert!(
            matches!(msg, IpcMessage::Command(IpcCommand { command, .. }) if command == "view")
        );
    }

    #[test]
    fn parse_query_message() {
        let json = r#"{"query": "get_windows"}"#;
        let msg: IpcMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(msg, IpcMessage::Query(IpcQuery { query, .. }) if query == "get_windows"));
    }

    #[test]
    fn parse_rejects_ambiguous_or_untyped_messages() {
        for json in [
            r#"{"command":"quit","query":"get_version"}"#,
            r#"{"query":"get_version","subscribe":["window"]}"#,
            r#"{"command":"quit","subscribe":["window"]}"#,
            r#"{"command":"quit","query":"get_version","subscribe":[]}"#,
            r#"{"args":null}"#,
        ] {
            let error = serde_json::from_str::<IpcMessage>(json).unwrap_err();
            assert!(
                error.to_string().contains("exactly one"),
                "unexpected error for {json}: {error}"
            );
        }
    }

    #[test]
    fn parse_rejects_duplicate_protocol_fields() {
        for json in [
            r#"{"command":"quit","command":"view"}"#,
            r#"{"query":"get_version","args":null,"args":{}}"#,
            r#"{"subscribe":["window"],"subscribe":["tag"]}"#,
        ] {
            let error = serde_json::from_str::<IpcMessage>(json).unwrap_err();
            assert!(
                error.to_string().contains("duplicate field"),
                "unexpected error for {json}: {error}"
            );
        }
    }

    #[test]
    fn parse_keeps_extension_fields_backwards_compatible() {
        let msg: IpcMessage = serde_json::from_str(
            r#"{"command":"view","args":{"tag":2},"request_id":"legacy-client-1"}"#,
        )
        .unwrap();
        assert!(
            matches!(msg, IpcMessage::Command(IpcCommand { command, .. }) if command == "view")
        );
    }

    #[test]
    fn window_info_serializes_the_explicit_minimized_state() {
        let value = serde_json::to_value(WindowInfo {
            id: 42,
            name: "terminal".to_string(),
            class: "xterm".to_string(),
            instance: "xterm".to_string(),
            tags: 1,
            monitor: 0,
            x: -2560,
            y: 20,
            w: 640,
            h: 480,
            is_floating: false,
            is_fullscreen: false,
            is_urgent: false,
            is_sticky: false,
            is_above: false,
            is_below: false,
            is_pip: false,
            is_maximized: false,
            is_maximized_vert: true,
            is_maximized_horz: false,
            maximize_promoted: false,
            maximize_restore: None,
            is_minimized: true,
            minimized_order: 0,
            is_swallowed: true,
            swallowing: None,
            swallowed_by: None,
            transient_for: None,
            is_tabbed: false,
            tab_index: None,
            is_on_view: false,
            is_scratchpad: true,
            is_fixed: false,
            is_dock: false,
            is_desktop: false,
            is_drag_floating: false,
            never_focus: false,
            skip_taskbar: false,
            skip_pager: false,
            no_decorations: false,
            demands_attention: false,
            has_strut: false,
            client_fact: 1.0,
            border_w: 2,
            is_focused: false,
            pid: Some(1234),
            connector: Some("DP-1".into()),
            monitor_name: None,
            scratchpad: Some("term".into()),
            layout: Some("TILE".into()),
            size_hints: None,
            float_rect: RectIpc {
                x: 0,
                y: 0,
                w: 100,
                h: 100,
            },
            old_geometry: RectIpc {
                x: 0,
                y: 0,
                w: 100,
                h: 100,
            },
            old_border_w: 0,
            hidden_restore: None,
            maximize_restore_anchor: None,
            pip_restore_sticky: false,
            old_state: false,
            remembers_closed_placement: false,
            dock_exclusive_zone: None,
            dock_anchor_top: false,
            dock_anchor_bottom: false,
            dock_anchor_left: false,
            dock_anchor_right: false,
            is_status_bar: false,
            hidden_x: None,
            sync_counter: None,
            sync_value: 0,
            total_w: 800,
            total_h: 600,
            stack_index: None,
            owns_output: false,
        })
        .expect("serialize WindowInfo");

        assert_eq!(value["is_minimized"], true);
        assert!(value.get("is_hidden").is_none());
        assert_eq!(value["owns_output"], false);
        assert_eq!(value["is_maximized"], false);
        assert_eq!(value["is_maximized_vert"], true);
        assert_eq!(value["is_maximized_horz"], false);
        assert_eq!(value["maximize_promoted"], false);
        assert!(value.get("maximize_restore").is_none());
        assert_eq!(value["is_above"], false);
        assert_eq!(value["is_below"], false);
        assert_eq!(value["is_swallowed"], true);
        assert_eq!(value["minimized_order"], 0);
        assert!(value.get("swallowing").is_none());
        assert!(value.get("swallowed_by").is_none());
        assert!(value.get("transient_for").is_none());
        assert_eq!(value["is_tabbed"], false);
        assert!(value.get("tab_index").is_none());
        assert_eq!(value["is_on_view"], false);
        assert_eq!(value["is_scratchpad"], true);
        assert_eq!(value["is_fixed"], false);
        assert_eq!(value["is_dock"], false);
        assert_eq!(value["is_desktop"], false);
        assert_eq!(value["is_drag_floating"], false);
        assert_eq!(value["never_focus"], false);
        assert_eq!(value["skip_taskbar"], false);
        assert_eq!(value["skip_pager"], false);
        assert_eq!(value["no_decorations"], false);
        assert_eq!(value["demands_attention"], false);
        assert_eq!(value["has_strut"], false);
        assert_eq!(value["client_fact"], 1.0);
        assert_eq!(value["border_w"], 2);
        assert_eq!(value["scratchpad"], "term");
        assert_eq!(value["layout"], "TILE");
        assert_eq!(value["pid"], 1234);
        assert_eq!(value["connector"], "DP-1");
        assert!(value.get("size_hints").is_none());
        assert_eq!(value["float_rect"]["w"], 100);
        assert_eq!(value["old_geometry"]["h"], 100);
        assert_eq!(value["old_border_w"], 0);
        assert!(value.get("hidden_restore").is_none());
        assert!(value.get("maximize_restore_anchor").is_none());
        assert_eq!(value["pip_restore_sticky"], false);
        assert_eq!(value["old_state"], false);
        assert_eq!(value["remembers_closed_placement"], false);
        assert!(value.get("dock_exclusive_zone").is_none());
        assert_eq!(value["dock_anchor_top"], false);
        assert_eq!(value["is_status_bar"], false);
        assert!(value.get("hidden_x").is_none());
        assert!(value.get("sync_counter").is_none());
        assert_eq!(value["sync_value"], 0);

        let without_pid = serde_json::to_value(WindowInfo {
            id: 1,
            name: String::new(),
            class: String::new(),
            instance: String::new(),
            tags: 1,
            monitor: 0,
            x: 0,
            y: 0,
            w: 100,
            h: 100,
            is_floating: false,
            is_fullscreen: false,
            is_urgent: false,
            is_sticky: false,
            is_above: false,
            is_below: false,
            is_pip: false,
            is_maximized: false,
            is_maximized_vert: false,
            is_maximized_horz: false,
            maximize_promoted: false,
            maximize_restore: None,
            is_minimized: false,
            minimized_order: 0,
            is_swallowed: false,
            swallowing: None,
            swallowed_by: None,
            transient_for: None,
            is_tabbed: false,
            tab_index: None,
            is_on_view: true,
            is_scratchpad: false,
            is_fixed: false,
            is_dock: false,
            is_desktop: false,
            is_drag_floating: false,
            never_focus: false,
            skip_taskbar: false,
            skip_pager: false,
            no_decorations: false,
            demands_attention: false,
            has_strut: false,
            client_fact: 1.0,
            border_w: 0,
            is_focused: false,
            pid: None,
            connector: None,
            monitor_name: None,
            scratchpad: None,
            layout: None,
            size_hints: None,
            float_rect: RectIpc {
                x: 0,
                y: 0,
                w: 100,
                h: 100,
            },
            old_geometry: RectIpc {
                x: 0,
                y: 0,
                w: 100,
                h: 100,
            },
            old_border_w: 0,
            hidden_restore: None,
            maximize_restore_anchor: None,
            pip_restore_sticky: false,
            old_state: false,
            remembers_closed_placement: false,
            dock_exclusive_zone: None,
            dock_anchor_top: false,
            dock_anchor_bottom: false,
            dock_anchor_left: false,
            dock_anchor_right: false,
            is_status_bar: false,
            hidden_x: None,
            sync_counter: None,
            sync_value: 0,
            total_w: 800,
            total_h: 600,
            stack_index: None,
            owns_output: false,
        })
        .expect("serialize");
        assert!(without_pid.get("pid").is_none());
        assert!(without_pid.get("connector").is_none());
        assert!(without_pid.get("scratchpad").is_none());
        assert!(without_pid.get("layout").is_none());
        assert_eq!(without_pid["is_swallowed"], false);
        assert_eq!(without_pid["is_on_view"], true);
        assert_eq!(without_pid["is_scratchpad"], false);
        assert_eq!(without_pid["border_w"], 0);
    }

    #[test]
    fn window_info_serializes_optional_connector() {
        let with_connector = serde_json::to_value(WindowInfo {
            id: 7,
            name: "app".into(),
            class: "App".into(),
            instance: "app".into(),
            tags: 1,
            monitor: 0,
            x: 0,
            y: 0,
            w: 100,
            h: 100,
            is_floating: false,
            is_fullscreen: false,
            is_urgent: false,
            is_sticky: false,
            is_above: false,
            is_below: false,
            is_pip: false,
            is_maximized: false,
            is_maximized_vert: false,
            is_maximized_horz: false,
            maximize_promoted: false,
            maximize_restore: None,
            is_minimized: false,
            minimized_order: 0,
            is_swallowed: false,
            swallowing: None,
            swallowed_by: None,
            transient_for: None,
            is_tabbed: false,
            tab_index: None,
            is_on_view: true,
            is_scratchpad: false,
            is_fixed: false,
            is_dock: false,
            is_desktop: false,
            is_drag_floating: false,
            never_focus: false,
            skip_taskbar: false,
            skip_pager: false,
            no_decorations: false,
            demands_attention: false,
            has_strut: false,
            client_fact: 1.0,
            border_w: 3,
            is_focused: true,
            pid: None,
            connector: Some("HDMI-A-1".into()),
            monitor_name: Some("Dell U2720Q".into()),
            scratchpad: None,
            layout: Some("MONOCLE".into()),
            size_hints: None,
            float_rect: RectIpc {
                x: 0,
                y: 0,
                w: 100,
                h: 100,
            },
            old_geometry: RectIpc {
                x: 0,
                y: 0,
                w: 100,
                h: 100,
            },
            old_border_w: 0,
            hidden_restore: None,
            maximize_restore_anchor: None,
            pip_restore_sticky: false,
            old_state: false,
            remembers_closed_placement: false,
            dock_exclusive_zone: None,
            dock_anchor_top: false,
            dock_anchor_bottom: false,
            dock_anchor_left: false,
            dock_anchor_right: false,
            is_status_bar: false,
            hidden_x: None,
            sync_counter: None,
            sync_value: 0,
            total_w: 800,
            total_h: 600,
            stack_index: None,
            owns_output: false,
        })
        .expect("serialize");
        assert_eq!(with_connector["connector"], "HDMI-A-1");
        assert_eq!(with_connector["monitor_name"], "Dell U2720Q");
        assert_eq!(with_connector["layout"], "MONOCLE");
        assert_eq!(with_connector["border_w"], 3);

        let without = serde_json::to_value(WindowInfo {
            id: 8,
            name: String::new(),
            class: String::new(),
            instance: String::new(),
            tags: 1,
            monitor: 1,
            x: 0,
            y: 0,
            w: 100,
            h: 100,
            is_floating: false,
            is_fullscreen: false,
            is_urgent: false,
            is_sticky: false,
            is_above: false,
            is_below: false,
            is_pip: false,
            is_maximized: false,
            is_maximized_vert: false,
            is_maximized_horz: false,
            maximize_promoted: false,
            maximize_restore: None,
            is_minimized: false,
            minimized_order: 0,
            is_swallowed: false,
            swallowing: None,
            swallowed_by: None,
            transient_for: None,
            is_tabbed: false,
            tab_index: None,
            is_on_view: false,
            is_scratchpad: false,
            is_fixed: false,
            is_dock: false,
            is_desktop: false,
            is_drag_floating: false,
            never_focus: false,
            skip_taskbar: false,
            skip_pager: false,
            no_decorations: false,
            demands_attention: false,
            has_strut: false,
            client_fact: 1.0,
            border_w: 0,
            is_focused: false,
            pid: None,
            connector: None,
            monitor_name: None,
            scratchpad: None,
            layout: None,
            size_hints: None,
            float_rect: RectIpc {
                x: 0,
                y: 0,
                w: 100,
                h: 100,
            },
            old_geometry: RectIpc {
                x: 0,
                y: 0,
                w: 100,
                h: 100,
            },
            old_border_w: 0,
            hidden_restore: None,
            maximize_restore_anchor: None,
            pip_restore_sticky: false,
            old_state: false,
            remembers_closed_placement: false,
            dock_exclusive_zone: None,
            dock_anchor_top: false,
            dock_anchor_bottom: false,
            dock_anchor_left: false,
            dock_anchor_right: false,
            is_status_bar: false,
            hidden_x: None,
            sync_counter: None,
            sync_value: 0,
            total_w: 800,
            total_h: 600,
            stack_index: None,
            owns_output: false,
        })
        .expect("serialize");
        assert!(without.get("connector").is_none());
        assert!(without.get("monitor_name").is_none());
        assert!(without.get("layout").is_none());
    }

    #[test]
    fn monitor_info_serializes_optional_connector() {
        let with_connector = serde_json::to_value(MonitorInfoIpc {
            num: 0,
            x: 0,
            y: 0,
            w: 1920,
            h: 1080,
            wx: 0,
            wy: 32,
            ww: 1920,
            wh: 1048,
            active_tags: 1,
            layout: "TILE".into(),
            focused: true,
            locked: false,
            connector: Some("DP-1".into()),
            name: None,
            monitor_name: Some("Dell U2720Q".into()),
            vendor: None,
            product_code: None,
            serial_number: None,
            monitor_serial: None,
            scale: 1.5,
            refresh_mhz: 60_000,
            hdr_capable: true,
            vrr_supported: false,
            vrr_enabled: false,
            gap: 8,
            m_fact: 0.55,
            n_master: 1,
            transform: 1,
            tab_bar_reserved: 28,
            hdr_metadata: None,
            physical_width_mm: 600,
            physical_height_mm: 340,
            preferred_width: 1920,
            preferred_height: 1080,
            preferred_refresh_mhz: 60_000,
            vrr_min_hz: 0,
            vrr_max_hz: 0,
            prev_layout: "TILE".into(),
            show_bar: true,
            bar_visible: true,
            has_visible_fullscreen: false,
            strut_top: 0,
            strut_bottom: 0,
            strut_left: 0,
            strut_right: 0,
            selected_id: None,
            sel_tags: 0,
            previous_tags: 0,
            cur_tag: 1,
            prev_tag: 1,
            output_connector: None,
            lt_symbol: "[]".into(),
            output_id: None,
            window_count: 0,
            floating_count: 0,
            minimized_count: 0,
            sticky_count: 0,
            urgent_count: 0,
            fullscreen_count: 0,
            pip_count: 0,
            maximized_count: 0,
            above_count: 0,
            below_count: 0,
            fixed_count: 0,
            scratchpad_count: 0,
            tabbed_count: 0,
            dock_count: 0,
            desktop_count: 0,
            never_focus_count: 0,
            demands_attention_count: 0,
            skip_taskbar_count: 0,
            skip_pager_count: 0,
            no_decorations_count: 0,
            drag_float_count: 0,
            swallowed_count: 0,
            on_view_count: 0,
            maximize_promoted_count: 0,
            strut_count: 0,
            status_bar_count: 0,
            closed_placement_count: 0,
            owns_output_count: 0,
        })
        .expect("serialize");
        assert_eq!(with_connector["connector"], "DP-1");
        assert_eq!(with_connector["monitor_name"], "Dell U2720Q");
        assert!(with_connector.get("name").is_none());
        assert!(with_connector.get("vendor").is_none());
        assert!(with_connector.get("product_code").is_none());
        assert!(with_connector.get("serial_number").is_none());
        assert!(with_connector.get("monitor_serial").is_none());
        assert_eq!(with_connector["locked"], false);
        assert_eq!(with_connector["wx"], 0);
        assert_eq!(with_connector["wy"], 32);
        assert_eq!(with_connector["ww"], 1920);
        assert_eq!(with_connector["wh"], 1048);
        assert_eq!(with_connector["scale"], 1.5);
        assert_eq!(with_connector["refresh_mhz"], 60_000);
        assert_eq!(with_connector["hdr_capable"], true);
        assert_eq!(with_connector["vrr_supported"], false);
        assert_eq!(with_connector["vrr_enabled"], false);
        assert_eq!(with_connector["gap"], 8);
        assert!((with_connector["m_fact"].as_f64().unwrap() - 0.55).abs() < 1e-6);
        assert_eq!(with_connector["n_master"], 1);
        assert_eq!(with_connector["transform"], 1);
        assert_eq!(with_connector["tab_bar_reserved"], 28);
        assert!(with_connector.get("hdr_metadata").is_none());
        assert_eq!(with_connector["physical_width_mm"], 600);
        assert_eq!(with_connector["physical_height_mm"], 340);
        assert_eq!(with_connector["preferred_width"], 1920);
        assert_eq!(with_connector["preferred_height"], 1080);
        assert_eq!(with_connector["preferred_refresh_mhz"], 60_000);
        assert_eq!(with_connector["vrr_min_hz"], 0);
        assert_eq!(with_connector["vrr_max_hz"], 0);
        assert_eq!(with_connector["prev_layout"], "TILE");
        assert_eq!(with_connector["show_bar"], true);
        assert_eq!(with_connector["bar_visible"], true);
        assert_eq!(with_connector["has_visible_fullscreen"], false);
        assert_eq!(with_connector["strut_top"], 0);
        assert!(with_connector.get("selected_id").is_none());
        assert_eq!(with_connector["sel_tags"], 0);
        assert_eq!(with_connector["previous_tags"], 0);
        assert_eq!(with_connector["cur_tag"], 1);
        assert_eq!(with_connector["prev_tag"], 1);
        assert!(with_connector.get("output_connector").is_none());

        let without = serde_json::to_value(MonitorInfoIpc {
            num: 1,
            x: 1920,
            y: 0,
            w: 1920,
            h: 1080,
            wx: 1920,
            wy: 0,
            ww: 1920,
            wh: 1080,
            active_tags: 1,
            layout: "TILE".into(),
            focused: false,
            locked: true,
            connector: None,
            name: None,
            monitor_name: None,
            vendor: None,
            product_code: None,
            serial_number: None,
            monitor_serial: None,
            scale: 1.0,
            refresh_mhz: 0,
            hdr_capable: false,
            vrr_supported: false,
            vrr_enabled: false,
            gap: 0,
            m_fact: 0.55,
            n_master: 1,
            transform: 0,
            tab_bar_reserved: 0,
            hdr_metadata: None,
            physical_width_mm: 0,
            physical_height_mm: 0,
            preferred_width: 0,
            preferred_height: 0,
            preferred_refresh_mhz: 0,
            vrr_min_hz: 0,
            vrr_max_hz: 0,
            prev_layout: "TILE".into(),
            show_bar: true,
            bar_visible: true,
            has_visible_fullscreen: false,
            strut_top: 0,
            strut_bottom: 0,
            strut_left: 0,
            strut_right: 0,
            selected_id: None,
            sel_tags: 0,
            previous_tags: 0,
            cur_tag: 1,
            prev_tag: 1,
            output_connector: None,
            lt_symbol: "[]".into(),
            output_id: None,
            window_count: 0,
            floating_count: 0,
            minimized_count: 0,
            sticky_count: 0,
            urgent_count: 0,
            fullscreen_count: 0,
            pip_count: 0,
            maximized_count: 0,
            above_count: 0,
            below_count: 0,
            fixed_count: 0,
            scratchpad_count: 0,
            tabbed_count: 0,
            dock_count: 0,
            desktop_count: 0,
            never_focus_count: 0,
            demands_attention_count: 0,
            skip_taskbar_count: 0,
            skip_pager_count: 0,
            no_decorations_count: 0,
            drag_float_count: 0,
            swallowed_count: 0,
            on_view_count: 0,
            maximize_promoted_count: 0,
            strut_count: 0,
            status_bar_count: 0,
            closed_placement_count: 0,
            owns_output_count: 0,
        })
        .expect("serialize");
        assert!(without.get("connector").is_none());
        assert!(without.get("monitor_name").is_none());
        assert_eq!(without["locked"], true);
        assert_eq!(without["wx"], 1920);
        assert_eq!(without["wy"], 0);
        assert_eq!(without["ww"], 1920);
        assert_eq!(without["wh"], 1080);
        assert_eq!(without["scale"], 1.0);
        assert_eq!(without["refresh_mhz"], 0);
        assert_eq!(without["hdr_capable"], false);
        assert_eq!(without["gap"], 0);
        assert_eq!(without["transform"], 0);
        assert_eq!(without["tab_bar_reserved"], 0);
        assert!(without.get("hdr_metadata").is_none());
        assert_eq!(without["physical_width_mm"], 0);
        assert_eq!(without["preferred_refresh_mhz"], 0);
    }

    #[test]
    fn workspace_info_serializes_optional_connector() {
        let with_connector = serde_json::to_value(WorkspaceInfo {
            tag_mask: 1,
            tag_index: 0,
            monitor: 0,
            layout: "TILE".into(),
            m_fact: 0.55,
            n_master: 1,
            gap: 12,
            num_clients: 2,
            focused: true,
            is_urgent: true,
            is_occupied: false,
            has_fullscreen: false,
            has_visible_fullscreen: false,
            connector: Some("DP-1".into()),
            monitor_name: Some("Dell U2720Q".into()),
            show_bar: true,
            prev_layout: "TILE".into(),
            selected_id: None,
            minimized_count: 0,
            floating_count: 0,
            sticky_count: 0,
            urgent_count: 0,
            fullscreen_count: 0,
            pip_count: 0,
            maximized_count: 0,
            above_count: 0,
            below_count: 0,
            fixed_count: 0,
            scratchpad_count: 0,
            tabbed_count: 0,
            dock_count: 0,
            desktop_count: 0,
            never_focus_count: 0,
            demands_attention_count: 0,
            skip_taskbar_count: 0,
            skip_pager_count: 0,
            no_decorations_count: 0,
            drag_float_count: 0,
            swallowed_count: 0,
            on_view_count: 0,
            maximize_promoted_count: 0,
            strut_count: 0,
            status_bar_count: 0,
            closed_placement_count: 0,
            owns_output_count: 0,
        })
        .expect("serialize");
        assert_eq!(with_connector["connector"], "DP-1");
        assert_eq!(with_connector["monitor_name"], "Dell U2720Q");
        assert_eq!(with_connector["focused"], true);
        assert_eq!(with_connector["is_urgent"], true);
        assert_eq!(with_connector["is_occupied"], false);
        assert_eq!(with_connector["has_fullscreen"], false);
        assert_eq!(with_connector["has_visible_fullscreen"], false);
        assert_eq!(with_connector["gap"], 12);
        assert_eq!(with_connector["show_bar"], true);
        assert_eq!(with_connector["prev_layout"], "TILE");
        assert!(with_connector.get("selected_id").is_none());
        assert_eq!(with_connector["minimized_count"], 0);
        assert_eq!(with_connector["floating_count"], 0);
        assert_eq!(with_connector["sticky_count"], 0);

        let without = serde_json::to_value(WorkspaceInfo {
            tag_mask: 2,
            tag_index: 1,
            monitor: 1,
            layout: "MONOCLE".into(),
            m_fact: 0.55,
            n_master: 1,
            gap: 0,
            num_clients: 0,
            focused: false,
            is_urgent: false,
            is_occupied: false,
            has_fullscreen: false,
            has_visible_fullscreen: false,
            connector: None,
            monitor_name: None,
            show_bar: true,
            prev_layout: "TILE".into(),
            selected_id: None,
            minimized_count: 0,
            floating_count: 0,
            sticky_count: 0,
            urgent_count: 0,
            fullscreen_count: 0,
            pip_count: 0,
            maximized_count: 0,
            above_count: 0,
            below_count: 0,
            fixed_count: 0,
            scratchpad_count: 0,
            tabbed_count: 0,
            dock_count: 0,
            desktop_count: 0,
            never_focus_count: 0,
            demands_attention_count: 0,
            skip_taskbar_count: 0,
            skip_pager_count: 0,
            no_decorations_count: 0,
            drag_float_count: 0,
            swallowed_count: 0,
            on_view_count: 0,
            maximize_promoted_count: 0,
            strut_count: 0,
            status_bar_count: 0,
            closed_placement_count: 0,
            owns_output_count: 0,
        })
        .expect("serialize");
        assert!(without.get("connector").is_none());
        assert!(without.get("monitor_name").is_none());
        assert_eq!(without["is_urgent"], false);
        assert_eq!(without["gap"], 0);
    }

    #[test]
    fn parse_subscribe_message() {
        let json = r#"{"subscribe": ["window", "tag"]}"#;
        let msg: IpcMessage = serde_json::from_str(json).unwrap();
        match msg {
            IpcMessage::Subscribe(sub) => {
                assert_eq!(sub.subscribe, vec!["window", "tag"]);
            }
            _ => panic!("expected Subscribe"),
        }
    }

    #[test]
    fn dispatch_known_commands() {
        // view
        let args = serde_json::json!({"tag": 4});
        let (_, arg) = dispatch_command("view", &args).unwrap();
        assert_eq!(arg, WMArgEnum::UInt(4));

        // focusstack
        let args = serde_json::json!({"value": -1});
        let (_, arg) = dispatch_command("focusstack", &args).unwrap();
        assert_eq!(arg, WMArgEnum::Int(-1));

        // setmfact
        let args = serde_json::json!(0.05);
        let (_, arg) = dispatch_command("setmfact", &args).unwrap();
        assert_eq!(arg, WMArgEnum::Float(0.05));

        // setgaps
        let args = serde_json::json!({"value": 2});
        let (_, arg) = dispatch_command("setgaps", &args).unwrap();
        assert_eq!(arg, WMArgEnum::Int(2));
        let (_, arg) = dispatch_command("setgaps", &serde_json::Value::Null).unwrap();
        assert_eq!(arg, WMArgEnum::Int(1));

        // killclient (no args)
        let args = serde_json::json!(null);
        let (_, arg) = dispatch_command("killclient", &args).unwrap();
        assert_eq!(arg, WMArgEnum::Int(0));

        // display layout modal
        let (_, arg) = dispatch_command("monitor_layout", &args).unwrap();
        assert_eq!(arg, WMArgEnum::Int(0));

        // The old name remains accepted only as a migration alias.
        let (canonical, _) =
            dispatch_command("toggle_waterlily", &serde_json::Value::Null).unwrap();
        let (deprecated, _) = dispatch_command("toggle_slime", &serde_json::Value::Null).unwrap();
        assert!(std::ptr::fn_addr_eq(canonical, deprecated));
    }

    #[test]
    fn fullscreen_screenshot_remains_a_bindable_dispatch_command() {
        let (command, arg) =
            dispatch_command("take_screenshot_fullscreen", &serde_json::Value::Null).unwrap();

        assert!(std::ptr::fn_addr_eq(
            command,
            Jwm::take_screenshot_fullscreen as WMFuncType
        ));
        assert_eq!(arg, WMArgEnum::Int(0));
    }

    #[test]
    fn dispatch_togglemaximize_command() {
        let (command, arg) = dispatch_command("togglemaximize", &serde_json::Value::Null).unwrap();
        assert!(std::ptr::fn_addr_eq(
            command,
            Jwm::togglemaximize as WMFuncType
        ));
        assert_eq!(arg, WMArgEnum::Int(0));
        assert!(is_known_command("togglemaximize"));
        assert!(IPC_REGISTRY.dispatch_commands.contains(&"togglemaximize"));
    }

    #[test]
    fn dispatch_unknown_command_errors() {
        let args = serde_json::json!(null);
        assert!(dispatch_command("nonexistent", &args).is_err());
        assert!(!is_known_command("nonexistent"));
        assert!(is_known_command("focusstack"));
        assert!(
            is_known_command("spawn"),
            "argument validation must not make a known command look unknown"
        );
    }

    #[test]
    fn dispatch_spawn_command() {
        let args = serde_json::json!({"cmd": ["alacritty", "--title", "test"]});
        let (_, arg) = dispatch_command("spawn", &args).unwrap();
        assert_eq!(
            arg,
            WMArgEnum::StringVec(vec!["alacritty".into(), "--title".into(), "test".into()])
        );

        let (_, arg) = dispatch_command("spawn", &serde_json::json!("alacritty")).unwrap();
        assert_eq!(arg, WMArgEnum::StringVec(vec!["alacritty".into()]));
    }

    #[test]
    fn dispatch_spawn_rejects_empty_or_non_string_commands() {
        for args in [
            serde_json::json!([]),
            serde_json::json!({"cmd": []}),
            serde_json::json!([1]),
            serde_json::json!(["alacritty", 1]),
            serde_json::json!({"cmd": [false]}),
            serde_json::json!(""),
            serde_json::json!({"cmd": ["   "]}),
        ] {
            let error = dispatch_command("spawn", &args).unwrap_err();
            assert!(error.starts_with("spawn:"), "unexpected error: {error}");
        }
    }

    #[test]
    fn optional_string_vector_commands_default_only_when_omitted() {
        let (_, arg) = dispatch_command("togglescratchpad", &serde_json::Value::Null).unwrap();
        assert_eq!(arg, WMArgEnum::StringVec(vec!["term".into()]));

        let (_, arg) = dispatch_command("focus_tab", &serde_json::json!({})).unwrap();
        assert_eq!(arg, WMArgEnum::StringVec(vec!["0".into(), "0".into()]));

        assert!(dispatch_command("togglescratchpad", &serde_json::json!({"bad": 1})).is_err());
        assert!(dispatch_command("focus_tab", &serde_json::json!(["0", 1])).is_err());
    }

    #[test]
    fn integer_arguments_reject_wrong_types_and_overflow() {
        for args in [
            serde_json::json!("1"),
            serde_json::json!(1.5),
            serde_json::json!({"value": false}),
            serde_json::json!({"unexpected": 1}),
            serde_json::json!(i64::from(i32::MAX) + 1),
            serde_json::json!(i64::from(i32::MIN) - 1),
        ] {
            assert!(
                dispatch_command("focusstack", &args).is_err(),
                "accepted invalid integer argument: {args}"
            );
        }

        let (_, arg) = dispatch_command("focusstack", &serde_json::json!(i32::MAX)).unwrap();
        assert_eq!(arg, WMArgEnum::Int(i32::MAX));
        let (_, arg) = dispatch_command("focusstack", &serde_json::json!({"v": i32::MIN})).unwrap();
        assert_eq!(arg, WMArgEnum::Int(i32::MIN));
    }

    #[test]
    fn omitted_integer_arguments_keep_existing_defaults() {
        let (_, arg) = dispatch_command("focusstack", &serde_json::Value::Null).unwrap();
        assert_eq!(arg, WMArgEnum::Int(1));
        let (_, arg) = dispatch_command("killclient", &serde_json::json!({})).unwrap();
        assert_eq!(arg, WMArgEnum::Int(0));
        let (_, arg) = dispatch_command("focusstack", &serde_json::json!({"value": null})).unwrap();
        assert_eq!(arg, WMArgEnum::Int(1));
    }

    #[test]
    fn float_arguments_reject_wrong_types_and_non_f32_values() {
        for args in [
            serde_json::json!("0.1"),
            serde_json::json!({"value": true}),
            serde_json::json!({"unexpected": 0.1}),
            serde_json::json!(1.0e100),
            serde_json::json!(-1.0e100),
        ] {
            assert!(
                dispatch_command("setmfact", &args).is_err(),
                "accepted invalid float argument: {args}"
            );
        }

        let (_, arg) = dispatch_command("setmfact", &serde_json::json!({"value": 1})).unwrap();
        assert_eq!(arg, WMArgEnum::Float(1.0));
        let (_, arg) = dispatch_command("setmfact", &serde_json::Value::Null).unwrap();
        assert_eq!(arg, WMArgEnum::Float(0.0));
    }

    #[test]
    fn tag_arguments_reject_wrong_types_negative_values_and_overflow() {
        for args in [
            serde_json::json!("2"),
            serde_json::json!(-1),
            serde_json::json!(2.5),
            serde_json::json!({"tag": false}),
            serde_json::json!({"unexpected": 2}),
            serde_json::json!(u64::from(u32::MAX) + 1),
        ] {
            assert!(
                dispatch_command("view", &args).is_err(),
                "accepted invalid tag argument: {args}"
            );
        }

        let (_, arg) = dispatch_command("view", &serde_json::json!({"tag": u32::MAX})).unwrap();
        assert_eq!(arg, WMArgEnum::UInt(u32::MAX));
    }

    #[test]
    fn tag_commands_require_a_mask_that_selects_a_configured_tag() {
        // Regression: an omitted, zero or out-of-range mask used to dispatch
        // as a "successful" no-op, and `view` then broadcast `tag/view` with
        // tag 0. Bit 31 is outside every valid configuration because
        // `tags_length` is clamped to 1..=31, so this holds whatever the
        // global config is.
        for command in ["view", "tag", "toggleview", "toggletag"] {
            for args in [
                serde_json::Value::Null,
                serde_json::json!({}),
                serde_json::json!({"tag": null}),
                serde_json::json!(0),
                serde_json::json!({"tag": 0}),
                serde_json::json!({"value": 0}),
                serde_json::json!(1u32 << 31),
                serde_json::json!({"tag": 1u32 << 31}),
            ] {
                let error = dispatch_command(command, &args)
                    .err()
                    .unwrap_or_else(|| panic!("{command} accepted empty tag mask {args}"));
                assert!(
                    error.starts_with(command),
                    "error for {command} {args} should name the command: {error}"
                );
            }
            assert!(
                is_known_command(command),
                "a required mask must not make {command} look unknown"
            );

            // Tag 1 exists in every configuration, and `!0` is what the
            // default key bindings pass for "all tags".
            let (_, arg) = dispatch_command(command, &serde_json::json!({"tag": 1})).unwrap();
            assert_eq!(arg, WMArgEnum::UInt(1));
            let (_, arg) = dispatch_command(command, &serde_json::json!(u32::MAX)).unwrap();
            assert_eq!(arg, WMArgEnum::UInt(u32::MAX));
        }
    }

    #[test]
    fn tag_mask_validation_uses_the_configured_tag_range() {
        let nine_tags = (1u32 << 9) - 1;
        // Tag 10 when only nine tags exist selects nothing.
        let error = validate_tag_mask("view", 1 << 9, nine_tags).unwrap_err();
        assert!(error.contains("0x200"), "unexpected error: {error}");
        assert!(
            error.contains("9 configured tags"),
            "unexpected error: {error}"
        );
        // A mask with at least one bit in range stays accepted unchanged;
        // the executors intersect it with the tag mask themselves.
        assert!(validate_tag_mask("view", 1 << 8, nine_tags).is_ok());
        assert!(validate_tag_mask("tag", (1 << 9) | 1, nine_tags).is_ok());
        assert!(validate_tag_mask("toggleview", u32::MAX, nine_tags).is_ok());
        assert!(validate_tag_mask("toggletag", 1 << 30, (1u32 << 31) - 1).is_ok());
        assert!(validate_tag_mask("toggletag", 1 << 1, 1).is_err());
    }

    #[test]
    fn dispatch_layout_command() {
        let args = serde_json::json!({"layout": "monocle"});
        let result = dispatch_command("setlayout", &args);
        assert!(result.is_ok());
        let (_, arg) = result.unwrap();
        assert!(matches!(arg, WMArgEnum::Layout(_)));
    }

    #[test]
    fn layout_arguments_default_only_when_omitted_and_reject_invalid_shapes() {
        for args in [serde_json::Value::Null, serde_json::json!({})] {
            let (_, arg) = dispatch_command("setlayout", &args).unwrap();
            let WMArgEnum::Layout(layout) = arg else {
                panic!("expected layout argument");
            };
            assert_eq!(*layout, LayoutEnum::TILE);
        }

        for args in [
            serde_json::json!({"layuot": "monocle"}),
            serde_json::json!(7),
            serde_json::json!([]),
            serde_json::json!({"layout": ""}),
            serde_json::json!({"layout": false}),
        ] {
            assert!(
                dispatch_command("setlayout", &args).is_err(),
                "accepted invalid layout argument: {args}"
            );
        }
    }

    #[test]
    fn dispatch_scrolling_layout_command() {
        let args = serde_json::json!({"layout": "scrolling"});
        let result = dispatch_command("setlayout", &args);
        assert!(result.is_ok());
        let (_, arg) = result.unwrap();
        let WMArgEnum::Layout(layout) = arg else {
            panic!("expected layout arg");
        };
        assert_eq!(*layout, LayoutEnum::SCROLLING);
    }

    #[test]
    fn dispatch_snap_window_command() {
        for (args, expected) in [
            (serde_json::json!("left"), "left"),
            (serde_json::json!({"direction": "right"}), "right"),
            (serde_json::json!({"direction": "Right"}), "right"),
            (serde_json::json!({"value": "maximize"}), "maximize"),
            (serde_json::json!({"v": "LEFT"}), "left"),
            (serde_json::json!("top-left"), "top-left"),
            (serde_json::json!({"direction": "Top-Right"}), "top-right"),
            (serde_json::json!({"value": "bottom-left"}), "bottom-left"),
            (serde_json::json!({"v": "BOTTOM-RIGHT"}), "bottom-right"),
        ] {
            let (func, arg) = dispatch_command("snap_window", &args).unwrap();
            assert!(std::ptr::fn_addr_eq(func, Jwm::snap_window as WMFuncType));
            assert_eq!(arg, WMArgEnum::StringVec(vec![expected.to_string()]));
        }
    }

    #[test]
    fn snap_window_arguments_reject_missing_unknown_and_misshapen_directions() {
        for args in [
            serde_json::Value::Null,
            serde_json::json!({}),
            serde_json::json!({"direction": null}),
            serde_json::json!("bottom"),
            serde_json::json!("top"),
            serde_json::json!({"direction": ""}),
            serde_json::json!({"direction": "maximize!"}),
            serde_json::json!({"direction": "topleft"}),
            serde_json::json!({"direction": "top_left"}),
            serde_json::json!({"direction": "left-top"}),
            serde_json::json!(7),
            serde_json::json!(["left", "right"]),
            serde_json::json!({"dir": "left"}),
        ] {
            assert!(
                dispatch_command("snap_window", &args).is_err(),
                "accepted invalid snap_window argument: {args}"
            );
        }

        // A malformed direction is a caller error, but the command stays known.
        assert!(is_known_command("snap_window"));
    }

    #[test]
    fn dispatch_scrolling_navigation_commands() {
        let args = serde_json::json!({"value": -1});
        for command in [
            "scrolling_focus_column",
            "scrolling_move_column",
            "scrolling_focus_window",
            "scrolling_consume",
            "scrolling_expel",
        ] {
            let (_, arg) = dispatch_command(command, &args).unwrap();
            assert!(matches!(arg, WMArgEnum::Int(-1)));
        }
    }

    #[test]
    fn response_serialization() {
        let ok = IpcResponse::ok(Some(serde_json::json!({"version": "0.2.0"})));
        let json = serde_json::to_string(&ok).unwrap();
        assert!(json.contains("\"success\":true"));
        assert!(json.contains("\"version\""));
        assert!(!json.contains("\"error\""));

        let err = IpcResponse::err("bad command");
        let json = serde_json::to_string(&err).unwrap();
        assert!(json.contains("\"success\":false"));
        assert!(json.contains("bad command"));
    }

    #[test]
    fn registry_dispatch_commands_are_all_bindable() {
        for command in IPC_REGISTRY.dispatch_commands {
            assert!(
                is_known_command(command),
                "registry contains non-dispatch command {command:?}"
            );
        }
    }

    #[test]
    fn capabilities_include_special_commands_queries_and_topics() {
        let capabilities = ipc_capabilities();
        assert_eq!(capabilities.schema_version, 1);
        assert!(
            capabilities
                .commands
                .iter()
                .any(|name| name == "focusstack")
        );
        assert!(
            capabilities
                .commands
                .iter()
                .any(|name| name == "reload_config")
        );
        assert!(
            capabilities
                .commands
                .iter()
                .any(|name| name == "start_recording")
        );
        assert!(
            capabilities
                .commands
                .iter()
                .any(|name| name == "set_mic_mute")
        );
        assert!(
            capabilities
                .commands
                .iter()
                .any(|name| name == "toggle_waterlily")
        );
        assert!(
            !capabilities
                .commands
                .iter()
                .any(|name| name == "toggle_slime"),
            "deprecated aliases must not be advertised"
        );
        assert!(capabilities.queries.iter().any(|name| name == "get_status"));
        assert!(
            capabilities
                .queries
                .iter()
                .any(|name| name == "get_mic_mute")
        );
        assert!(capabilities.queries.iter().any(|name| name == "get_layout"));
        assert!(capabilities.queries.iter().any(|name| name == "get_gaps"));
        assert!(
            capabilities
                .queries
                .iter()
                .any(|name| name == "get_nmaster")
        );
        assert!(
            capabilities.queries.iter().any(|name| name == "get_mfact"),
            "get_mfact twins get_gaps/get_nmaster in capabilities"
        );
        assert!(
            capabilities
                .queries
                .iter()
                .any(|name| name == "get_clients"),
            "get_clients aliases get_windows in capabilities"
        );
        assert!(
            capabilities
                .queries
                .iter()
                .any(|name| name == "get_outputs"),
            "get_outputs aliases get_monitors in capabilities"
        );
        assert!(
            capabilities.queries.iter().any(|name| name == "get_tags"),
            "get_tags aliases get_workspaces in capabilities"
        );
        assert!(
            capabilities
                .queries
                .iter()
                .any(|name| name == "get_desktops"),
            "get_desktops aliases get_workspaces in capabilities"
        );
        assert!(
            capabilities
                .queries
                .iter()
                .any(|name| name == "get_night_light")
        );
        assert!(
            capabilities
                .queries
                .iter()
                .any(|name| name == "get_night_light_status")
        );
        assert!(
            capabilities
                .queries
                .iter()
                .any(|name| name == "get_scratchpads")
        );
        assert!(capabilities.queries.iter().any(|name| name == "get_struts"));
        assert!(capabilities.queries.iter().any(|name| name == "get_window"));
        assert!(
            capabilities
                .commands
                .iter()
                .any(|name| name == "set_layout")
        );
        assert!(
            capabilities
                .commands
                .iter()
                .any(|name| name == "set_nmaster")
        );
        assert!(
            capabilities
                .commands
                .iter()
                .any(|name| name == "setnmaster")
        );
        assert!(
            capabilities.commands.iter().any(|name| name == "set_mfact"),
            "set_mfact aliases setmfact in capabilities"
        );
        assert!(
            capabilities.commands.iter().any(|name| name == "set_gaps"),
            "set_gaps aliases setgaps in capabilities"
        );
        assert!(
            capabilities.commands.iter().any(|name| name == "set_cfact"),
            "set_cfact aliases setcfact in capabilities"
        );
        assert!(
            capabilities
                .commands
                .iter()
                .any(|name| name == "toggle_floating"),
            "toggle_floating aliases togglefloating"
        );
        assert!(
            capabilities
                .commands
                .iter()
                .any(|name| name == "kill_client"),
            "kill_client aliases killclient"
        );
        assert!(
            capabilities.queries.iter().any(|name| name == "get_cfact"),
            "get_cfact twins get_mfact in capabilities"
        );
        assert!(
            capabilities
                .queries
                .iter()
                .any(|name| name == "get_show_bar")
        );
        assert!(
            capabilities
                .queries
                .iter()
                .any(|name| name == "get_selected")
        );
        assert!(
            capabilities
                .queries
                .iter()
                .any(|name| name == "get_focused_window")
        );
        assert!(
            capabilities
                .queries
                .iter()
                .any(|name| name == "get_prev_layout")
        );
        assert!(
            capabilities
                .queries
                .iter()
                .any(|name| name == "get_waterlily_status")
        );
        assert!(
            capabilities
                .queries
                .iter()
                .any(|name| name == "get_capabilities")
        );
        assert!(
            capabilities
                .subscription_topics
                .iter()
                .any(|name| name == "window")
        );
        assert!(
            capabilities
                .subscription_topics
                .iter()
                .any(|name| name == "workspace"),
            "workspace aliases tag in subscription topics"
        );
        assert!(
            capabilities
                .subscription_topics
                .iter()
                .any(|name| name == "bar"),
            "bar aliases monitor/bar in subscription topics"
        );
        assert!(is_supported_query("benchmark_report"));
        assert!(!is_supported_query("not_a_query"));
    }

    #[test]
    fn runtime_status_v1_serializes_stable_top_level_fields() {
        let status = RuntimeStatusV1 {
            schema_version: 1,
            version: "0.2.0".into(),
            backend: "wayland-winit".into(),
            compiled_backends: vec!["wayland-winit".into()],
            pid: 4242,
            allocations: None,
            uptime_ms: 42,
            health: RuntimeHealth::from_reasons(Vec::new()),
            counts: RuntimeCounts {
                windows: 3,
                monitors: 1,
                workspaces: 9,
            },
            config: serde_json::json!({"exists": true}),
            compositor_active: false,
            compositor_configured: false,
            compositor_temporary: false,
            compositor_transition: CompositorTransitionStatus {
                attempts: 2,
                last_requested_active: Some(false),
                last_attempt_unix_ms: Some(1_234),
                last_success: Some(true),
                last_error: None,
            },
            features: RuntimeFeatureStates {
                do_not_disturb: false,
                screenshot: false,
                overview: true,
                recording: false,
                audio_recording: false,
                magnifier: false,
                system_ui: false,
                peek: false,
                expose: false,
                annotation: false,
                layout_picker: false,
                tags_overview: false,
                calendar: false,
                keybindings: false,
                monitor_layout: false,
                launcher: false,
                session_menu: false,
                notifications: false,
                waterlily: false,
                night_light: true,
                idle_inhibit: false,
                control_center: false,
                clipboard_picker: false,
                wifi_picker: false,
                bluetooth_picker: false,
                wallpaper_picker: false,
                theme_picker: false,
                audio_output_picker: false,
                audio_input_picker: false,
                media_players: false,
                window_switcher: false,
                session_lock: false,
                monitor_lock: false,
                debug_hud: false,
            },
            compositor_metrics: None,
            resources: None,
            connectivity: None,
            power: None,
            media: None,
            notifications: None,
            blur: None,
            hdr: None,
            capture: None,
            idle: None,
            recording: None,
            audio_recording: None,
            clipboard: None,
            waterlily: None,
            night_light: None,
            magnifier: None,
            peek: None,
            expose: None,
            gesture: None,
            wayland: None,
            dnd: None,
            session_lock: None,
            tearing: None,
            xwayland: None,
            scrolling: None,
            color_management: None,
            audio: None,
            wallpaper: None,
            bluetooth: None,
            system_ui: None,
            layout: None,
            tabs: None,
            struts: None,
            scratchpads: None,
            gaps: None,
            mfact: None,
            nmaster: None,
            show_bar: None,
            metrics: None,
            version_info: None,
            monitors: None,
            workspaces: None,
            windows: None,
            tree: None,
            focused: None,
            cfact: None,
            prev_layout: None,
            effects: None,
            mic: None,
            capabilities: None,
            selected: None,
            bench: None,
            floating: None,
            minimized: None,
            sticky: None,
            urgent: None,
            fullscreen: None,
            pip: None,
        };

        let json = serde_json::to_value(status).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["backend"], "wayland-winit");
        assert_eq!(json["health"]["status"], "healthy");
        assert_eq!(json["counts"]["windows"], 3);
        assert_eq!(json["compositor_active"], false);
        assert_eq!(json["compositor_configured"], false);
        assert_eq!(json["compositor_temporary"], false);
        assert_eq!(json["compositor_transition"]["attempts"], 2);
        assert_eq!(
            json["compositor_transition"]["last_requested_active"],
            false
        );
        assert_eq!(json["compositor_transition"]["last_success"], true);
        assert!(json["compositor_transition"]["last_error"].is_null());
        assert_eq!(json["features"]["overview"], true);
        assert_eq!(json["features"]["layout_picker"], false);
        assert_eq!(json["features"]["tags_overview"], false);
        assert_eq!(json["features"]["calendar"], false);
        assert_eq!(json["features"]["keybindings"], false);
        assert_eq!(json["features"]["monitor_layout"], false);
        assert_eq!(json["features"]["launcher"], false);
        assert_eq!(json["features"]["session_menu"], false);
        assert_eq!(json["features"]["notifications"], false);
        assert_eq!(json["features"]["waterlily"], false);
        assert_eq!(json["features"]["night_light"], true);
        assert_eq!(json["features"]["idle_inhibit"], false);
        assert_eq!(json["features"]["control_center"], false);
        assert_eq!(json["features"]["clipboard_picker"], false);
        assert_eq!(json["features"]["wifi_picker"], false);
        assert_eq!(json["features"]["bluetooth_picker"], false);
        assert_eq!(json["features"]["wallpaper_picker"], false);
        assert_eq!(json["features"]["theme_picker"], false);
        assert_eq!(json["features"]["audio_output_picker"], false);
        assert_eq!(json["features"]["audio_input_picker"], false);
        assert_eq!(json["features"]["media_players"], false);
        assert_eq!(json["features"]["window_switcher"], false);
        assert_eq!(json["features"]["session_lock"], false);
        assert!(json.get("resources").is_none());
        assert!(json.get("connectivity").is_none());
        assert!(json.get("power").is_none());
        assert!(json.get("media").is_none());
        assert!(json.get("notifications").is_none());
        assert!(json.get("blur").is_none());
        assert!(json.get("hdr").is_none());
        assert!(json.get("capture").is_none());
        assert!(json.get("idle").is_none());
        assert!(json.get("recording").is_none());
        assert!(json.get("audio_recording").is_none());
        assert!(json.get("clipboard").is_none());
        assert!(json.get("waterlily").is_none());
        assert!(json.get("night_light").is_none());
        assert!(json.get("magnifier").is_none());
        assert!(json.get("peek").is_none());
        assert!(json.get("expose").is_none());
        assert!(json.get("gesture").is_none());
        assert!(json.get("wayland").is_none());
        assert!(json.get("dnd").is_none());
        assert!(json.get("session_lock").is_none());
        assert_eq!(json["features"]["monitor_lock"], false);
        assert_eq!(json["features"]["debug_hud"], false);
        assert!(json["compositor_metrics"].is_null());
    }

    #[test]
    fn set_cfact_and_toggle_underscore_dispatch_aliases() {
        let cfact = dispatch_command("set_cfact", &serde_json::json!({"value": 1.25}));
        assert!(cfact.is_ok(), "{cfact:?}");
        let floating = dispatch_command("toggle_floating", &serde_json::json!({}));
        assert!(floating.is_ok(), "{floating:?}");
        let sticky = dispatch_command("toggle_sticky", &serde_json::json!({}));
        assert!(sticky.is_ok(), "{sticky:?}");
        let pip = dispatch_command("toggle_pip", &serde_json::json!({}));
        assert!(pip.is_ok(), "{pip:?}");
        let maximize = dispatch_command("toggle_maximize", &serde_json::json!({}));
        assert!(maximize.is_ok(), "{maximize:?}");
        let bar = dispatch_command("toggle_bar", &serde_json::json!({}));
        assert!(bar.is_ok(), "{bar:?}");
        let kill = dispatch_command("kill_client", &serde_json::json!({}));
        assert!(kill.is_ok(), "{kill:?}");
        let focus = dispatch_command("focus_stack", &serde_json::json!({"value": 1}));
        assert!(focus.is_ok(), "{focus:?}");
        let cycle = dispatch_command("cycle_layout", &serde_json::json!({"value": 1}));
        assert!(cycle.is_ok(), "{cycle:?}");
    }

    #[test]
    fn set_layout_and_set_nmaster_dispatch_aliases() {
        let layout = dispatch_command("set_layout", &serde_json::json!({"layout": "tile"}));
        assert!(layout.is_ok(), "{layout:?}");
        let nmaster = dispatch_command("set_nmaster", &serde_json::json!({"value": 2}));
        assert!(nmaster.is_ok(), "{nmaster:?}");
        let setn = dispatch_command("setnmaster", &serde_json::json!({"value": 3}));
        assert!(setn.is_ok(), "{setn:?}");
    }

    #[test]
    fn set_mfact_and_set_gaps_dispatch_aliases() {
        let mfact = dispatch_command("set_mfact", &serde_json::json!({"value": 0.6}));
        assert!(mfact.is_ok(), "{mfact:?}");
        let gaps = dispatch_command("set_gaps", &serde_json::json!({"value": 8}));
        assert!(gaps.is_ok(), "{gaps:?}");
        let canonical_mfact = dispatch_command("setmfact", &serde_json::json!({"value": 0.55}));
        assert!(canonical_mfact.is_ok(), "{canonical_mfact:?}");
        let canonical_gaps = dispatch_command("setgaps", &serde_json::json!({"value": 4}));
        assert!(canonical_gaps.is_ok(), "{canonical_gaps:?}");
    }

    #[test]
    fn tree_node_serializes_selected_id_and_window_count() {
        let node = TreeNode {
            monitor: MonitorInfoIpc {
                num: 0,
                x: 0,
                y: 0,
                w: 1920,
                h: 1080,
                wx: 0,
                wy: 0,
                ww: 1920,
                wh: 1080,
                active_tags: 1,
                layout: "TILE".into(),
                focused: true,
                locked: false,
                connector: None,
                name: None,
                monitor_name: None,
                vendor: None,
                product_code: None,
                serial_number: None,
                monitor_serial: None,
                scale: 1.0,
                refresh_mhz: 60_000,
                hdr_capable: false,
                vrr_supported: false,
                vrr_enabled: false,
                gap: 0,
                m_fact: 0.55,
                n_master: 1,
                transform: 0,
                tab_bar_reserved: 0,
                hdr_metadata: None,
                physical_width_mm: 0,
                physical_height_mm: 0,
                preferred_width: 0,
                preferred_height: 0,
                preferred_refresh_mhz: 0,
                vrr_min_hz: 0,
                vrr_max_hz: 0,
                prev_layout: "TILE".into(),
                show_bar: true,
                bar_visible: true,
                has_visible_fullscreen: false,
                strut_top: 0,
                strut_bottom: 0,
                strut_left: 0,
                strut_right: 0,
                selected_id: None,
                sel_tags: 0,
                previous_tags: 0,
                cur_tag: 1,
                prev_tag: 1,
                output_connector: None,
                lt_symbol: "[]".into(),
                output_id: None,
                window_count: 0,
                floating_count: 0,
                minimized_count: 0,
                sticky_count: 0,
                urgent_count: 0,
                fullscreen_count: 0,
                pip_count: 0,
                maximized_count: 0,
                above_count: 0,
                below_count: 0,
                fixed_count: 0,
                scratchpad_count: 0,
                tabbed_count: 0,
                dock_count: 0,
                desktop_count: 0,
                never_focus_count: 0,
                demands_attention_count: 0,
                skip_taskbar_count: 0,
                skip_pager_count: 0,
                no_decorations_count: 0,
                drag_float_count: 0,
                swallowed_count: 0,
                on_view_count: 0,
                maximize_promoted_count: 0,
                strut_count: 0,
                status_bar_count: 0,
                closed_placement_count: 0,
                owns_output_count: 0,
            },
            windows: Vec::new(),
            selected_id: Some(42),
            window_count: 0,
            urgent_count: 0,
            floating_count: 0,
            minimized_count: 0,
            sticky_count: 0,
            fullscreen_count: 0,
            pip_count: 0,
            maximized_count: 0,
            above_count: 0,
            below_count: 0,
            scratchpad_count: 0,
            tabbed_count: 0,
            fixed_count: 0,
            dock_count: 0,
            desktop_count: 0,
            never_focus_count: 0,
            demands_attention_count: 0,
            skip_taskbar_count: 0,
            skip_pager_count: 0,
            no_decorations_count: 0,
            drag_float_count: 0,
            swallowed_count: 0,
            on_view_count: 0,
            maximize_promoted_count: 0,
            strut_count: 0,
            status_bar_count: 0,
            owns_output_count: 0,
        };
        let json = serde_json::to_value(node).unwrap();
        assert_eq!(json["selected_id"], 42);
        assert_eq!(json["window_count"], 0);
        assert_eq!(json["urgent_count"], 0);
        assert_eq!(json["floating_count"], 0);
        assert!(json["windows"].as_array().unwrap().is_empty());
    }

    #[test]
    fn tree_node_serializes_urgent_and_floating_counts() {
        let node = TreeNode {
            monitor: MonitorInfoIpc {
                num: 0,
                x: 0,
                y: 0,
                w: 800,
                h: 600,
                wx: 0,
                wy: 0,
                ww: 800,
                wh: 600,
                active_tags: 1,
                layout: "TILE".into(),
                focused: true,
                locked: false,
                connector: None,
                name: None,
                monitor_name: None,
                vendor: None,
                product_code: None,
                serial_number: None,
                monitor_serial: None,
                scale: 1.0,
                refresh_mhz: 60_000,
                hdr_capable: false,
                vrr_supported: false,
                vrr_enabled: false,
                gap: 0,
                m_fact: 0.55,
                n_master: 1,
                transform: 0,
                tab_bar_reserved: 0,
                hdr_metadata: None,
                physical_width_mm: 0,
                physical_height_mm: 0,
                preferred_width: 0,
                preferred_height: 0,
                preferred_refresh_mhz: 0,
                vrr_min_hz: 0,
                vrr_max_hz: 0,
                prev_layout: "TILE".into(),
                show_bar: true,
                bar_visible: true,
                has_visible_fullscreen: false,
                strut_top: 0,
                strut_bottom: 0,
                strut_left: 0,
                strut_right: 0,
                selected_id: None,
                sel_tags: 0,
                previous_tags: 0,
                cur_tag: 1,
                prev_tag: 1,
                output_connector: None,
                lt_symbol: "[]".into(),
                output_id: None,
                window_count: 0,
                floating_count: 0,
                minimized_count: 0,
                sticky_count: 0,
                urgent_count: 0,
                fullscreen_count: 0,
                pip_count: 0,
                maximized_count: 0,
                above_count: 0,
                below_count: 0,
                fixed_count: 0,
                scratchpad_count: 0,
                tabbed_count: 0,
                dock_count: 0,
                desktop_count: 0,
                never_focus_count: 0,
                demands_attention_count: 0,
                skip_taskbar_count: 0,
                skip_pager_count: 0,
                no_decorations_count: 0,
                drag_float_count: 0,
                swallowed_count: 0,
                on_view_count: 0,
                maximize_promoted_count: 0,
                strut_count: 0,
                status_bar_count: 0,
                closed_placement_count: 0,
                owns_output_count: 0,
            },
            windows: Vec::new(),
            selected_id: None,
            window_count: 3,
            urgent_count: 1,
            floating_count: 2,
            minimized_count: 0,
            sticky_count: 0,
            fullscreen_count: 0,
            pip_count: 0,
            maximized_count: 0,
            above_count: 0,
            below_count: 0,
            scratchpad_count: 0,
            tabbed_count: 0,
            fixed_count: 0,
            dock_count: 0,
            desktop_count: 0,
            never_focus_count: 0,
            demands_attention_count: 0,
            skip_taskbar_count: 0,
            skip_pager_count: 0,
            no_decorations_count: 0,
            drag_float_count: 0,
            swallowed_count: 0,
            on_view_count: 0,
            maximize_promoted_count: 0,
            strut_count: 0,
            status_bar_count: 0,
            owns_output_count: 0,
        };
        let json = serde_json::to_value(node).unwrap();
        assert_eq!(json["window_count"], 3);
        assert_eq!(json["urgent_count"], 1);
        assert_eq!(json["floating_count"], 2);
    }
}
