use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::layout::LayoutEnum;
use crate::jwm::{Jwm, WMArgEnum, WMFuncType};

// ---------------------------------------------------------------------------
// Wire protocol types (newline-delimited JSON)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum IpcMessage {
    Command(IpcCommand),
    Query(IpcQuery),
    Subscribe(IpcSubscribe),
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
        "app_launcher",
        "cycle_overview",
        "cyclelayout",
        "focus_none",
        "focus_tab",
        "focus_window",
        "focusmon",
        "focusstack",
        "incnmaster",
        "killclient",
        "lock_screen",
        "loopview",
        "monitor_layout",
        "movestack",
        "quit",
        "refocus",
        "restart",
        "restore_session",
        "save_session",
        "scrolling_consume",
        "scrolling_expel",
        "scrolling_focus_column",
        "scrolling_focus_window",
        "scrolling_move_column",
        "scrolling_toggle_attach_mode",
        "setcfact",
        "setlayout",
        "setmfact",
        "spawn",
        "tag",
        "tagmon",
        "toggle_annotation",
        "adjust_recording_region",
        "toggle_audio_recording",
        "toggle_dnd",
        "toggle_magnifier",
        "toggle_overview",
        "toggle_peek",
        "toggle_recording",
        "toggle_waterlily",
        "togglebar",
        "togglecompositor",
        "togglefloating",
        "togglepartialdamage",
        "togglepip",
        "togglescratchpad",
        "togglesticky",
        "toggletag",
        "toggleview",
        "view",
        "waterlily_case",
        "zoom",
    ],
    special_commands: &[
        "batch",
        "benchmark",
        "command_batch",
        "move_window_to_monitor",
        "reload_config",
        "set_config",
        "set_config_batch",
        "set_hdr_metadata",
        "set_recording_region",
        "start_audio_recording",
        "start_recording",
        "stop_audio_recording",
        "stop_recording",
    ],
    queries: &[
        "benchmark_report",
        "get_audio_recording_status",
        "get_blur_status",
        "get_capabilities",
        "get_capture_status",
        "get_color_management_status",
        "get_config",
        "get_config_status",
        "get_dnd",
        "get_effect_status",
        "get_gesture_status",
        "get_hdr_status",
        "get_metrics",
        "get_monitors",
        "get_recording_status",
        "get_scrolling_status",
        "get_session_lock",
        "get_status",
        "get_tearing_hints",
        "get_tree",
        "get_version",
        "get_wayland_status",
        "get_windows",
        "get_workspaces",
        "get_xwayland_status",
    ],
    subscription_topics: &[
        "*",
        "audio_recording",
        "config",
        "dnd",
        "layout",
        "monitor",
        "recording",
        "scrolling",
        "tag",
        "window",
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
}

/// Backend-neutral live status. New fields may be added within a schema
/// version; incompatible changes require incrementing `schema_version`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RuntimeStatusV1 {
    pub schema_version: u32,
    pub version: String,
    pub backend: String,
    pub uptime_ms: u64,
    pub health: RuntimeHealth,
    pub counts: RuntimeCounts,
    pub config: Value,
    pub features: RuntimeFeatureStates,
    pub compositor_metrics: Option<Value>,
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
    pub is_pip: bool,
    pub is_focused: bool,
}

#[derive(Debug, Serialize)]
pub struct WorkspaceInfo {
    pub tag_mask: u32,
    pub tag_index: usize,
    pub monitor: i32,
    pub layout: String,
    pub m_fact: f32,
    pub n_master: u32,
    pub num_clients: usize,
    pub focused: bool,
}

#[derive(Debug, Serialize)]
pub struct MonitorInfoIpc {
    pub num: i32,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub active_tags: u32,
    pub layout: String,
    pub focused: bool,
}

#[derive(Debug, Serialize)]
pub struct TreeNode {
    pub monitor: MonitorInfoIpc,
    pub windows: Vec<WindowInfo>,
}

// ---------------------------------------------------------------------------
// Command dispatch — maps command name → (WMFuncType, WMArgEnum)
// ---------------------------------------------------------------------------

pub fn dispatch_command(name: &str, args: &Value) -> Result<(WMFuncType, WMArgEnum), String> {
    match name {
        // --- Window management ---
        "focusstack" => Ok((Jwm::focusstack as WMFuncType, parse_int_arg(args, 1)?)),
        "app_launcher" => Ok((Jwm::app_launcher as WMFuncType, WMArgEnum::Int(0))),
        "monitor_layout" => Ok((Jwm::monitor_layout as WMFuncType, WMArgEnum::Int(0))),
        "lock_screen" => Ok((Jwm::lock_screen as WMFuncType, WMArgEnum::Int(0))),
        "killclient" => Ok((Jwm::killclient, parse_int_arg(args, 0)?)),
        "zoom" => Ok((Jwm::zoom, parse_int_arg(args, 0)?)),
        "togglefloating" => Ok((Jwm::togglefloating, parse_int_arg(args, 0)?)),
        "togglesticky" => Ok((Jwm::togglesticky, parse_int_arg(args, 0)?)),
        "togglepip" => Ok((Jwm::togglepip, parse_int_arg(args, 0)?)),
        "togglescratchpad" => {
            let cmd = if argument_is_omitted(args) {
                vec!["term".to_string()]
            } else {
                parse_string_vec_arg(args).map_err(|e| format!("togglescratchpad: {e}"))?
            };
            Ok((Jwm::togglescratchpad, WMArgEnum::StringVec(cmd)))
        }
        "movestack" => Ok((Jwm::movestack, parse_int_arg(args, 1)?)),
        "focus_none" => Ok((Jwm::focus_none, parse_int_arg(args, 0)?)),
        "focus_window" => Ok((Jwm::focus_window, parse_window_id_arg(args)?)),
        "focus_tab" => {
            let cmd = if argument_is_omitted(args) {
                vec!["0".to_string(), "0".to_string()]
            } else {
                parse_string_vec_arg(args).map_err(|e| format!("focus_tab: {e}"))?
            };
            Ok((Jwm::focus_tab, WMArgEnum::StringVec(cmd)))
        }
        "refocus" => Ok((Jwm::refocus, parse_int_arg(args, 0)?)),

        // --- Layout ---
        "setmfact" => Ok((Jwm::setmfact, parse_float_arg(args, 0.0)?)),
        "setcfact" => Ok((Jwm::setcfact, parse_float_arg(args, 0.0)?)),
        "incnmaster" => Ok((Jwm::incnmaster, parse_int_arg(args, 1)?)),
        "scrolling_toggle_attach_mode" => {
            Ok((Jwm::scrolling_toggle_attach_mode, parse_int_arg(args, 0)?))
        }
        "scrolling_focus_column" => Ok((Jwm::scrolling_focus_column, parse_int_arg(args, 1)?)),
        "scrolling_move_column" => Ok((Jwm::scrolling_move_column, parse_int_arg(args, 1)?)),
        "scrolling_focus_window" => Ok((Jwm::scrolling_focus_window, parse_int_arg(args, 1)?)),
        "scrolling_consume" => Ok((Jwm::scrolling_consume, parse_int_arg(args, 1)?)),
        "scrolling_expel" => Ok((Jwm::scrolling_expel, parse_int_arg(args, 1)?)),
        "setlayout" => {
            let layout = parse_layout_arg(args)?;
            Ok((Jwm::setlayout, layout))
        }
        "cyclelayout" => Ok((Jwm::cyclelayout, parse_int_arg(args, 1)?)),
        "togglebar" => Ok((Jwm::togglebar, parse_int_arg(args, 0)?)),

        // --- Tags ---
        "view" => Ok((Jwm::view, parse_uint_arg(args)?)),
        "tag" => Ok((Jwm::tag, parse_uint_arg(args)?)),
        "toggleview" => Ok((Jwm::toggleview, parse_uint_arg(args)?)),
        "toggletag" => Ok((Jwm::toggletag, parse_uint_arg(args)?)),
        "loopview" => Ok((Jwm::loopview, parse_int_arg(args, 1)?)),

        // --- Monitor ---
        "focusmon" => Ok((Jwm::focusmon, parse_int_arg(args, 1)?)),
        "tagmon" => Ok((Jwm::tagmon, parse_int_arg(args, 1)?)),

        // --- Spawn ---
        "spawn" => {
            let cmd = parse_string_vec_arg(args).map_err(|e| format!("spawn: {e}"))?;
            Ok((Jwm::spawn, WMArgEnum::StringVec(cmd)))
        }

        // --- Misc ---
        "quit" => Ok((Jwm::quit, parse_int_arg(args, 0)?)),
        "restart" => Ok((Jwm::restart, parse_int_arg(args, 0)?)),
        "togglecompositor" => Ok((Jwm::togglecompositor, parse_int_arg(args, 0)?)),
        "togglepartialdamage" => Ok((Jwm::togglepartialdamage, parse_int_arg(args, 0)?)),
        "toggle_waterlily" => Ok((Jwm::toggle_waterlily, parse_int_arg(args, 0)?)),
        "waterlily_case" => {
            let requested = if argument_is_omitted(args) {
                vec!["next".to_string()]
            } else {
                parse_string_vec_arg(args).map_err(|e| format!("waterlily_case: {e}"))?
            };
            Ok((Jwm::waterlily_case, WMArgEnum::StringVec(requested)))
        }
        // Compatibility only: intentionally omitted from IPC capability discovery.
        "toggle_slime" => {
            log::warn!("IPC action `toggle_slime` is deprecated; use `toggle_waterlily` instead");
            Ok((Jwm::toggle_waterlily, parse_int_arg(args, 0)?))
        }
        "toggle_overview" => Ok((Jwm::toggle_overview, parse_int_arg(args, 0)?)),
        "cycle_overview" => Ok((Jwm::cycle_overview, parse_int_arg(args, 1)?)),
        "toggle_magnifier" => Ok((Jwm::toggle_magnifier, parse_int_arg(args, 0)?)),
        "toggle_peek" => Ok((Jwm::toggle_peek, parse_int_arg(args, 0)?)),
        "toggle_annotation" => Ok((Jwm::toggle_annotation, parse_int_arg(args, 0)?)),
        "toggle_recording" => Ok((Jwm::toggle_recording, parse_int_arg(args, 0)?)),
        "adjust_recording_region" => Ok((Jwm::adjust_recording_region, parse_int_arg(args, 0)?)),
        "toggle_audio_recording" => Ok((Jwm::toggle_audio_recording, parse_int_arg(args, 0)?)),
        "toggle_dnd" => Ok((Jwm::toggle_dnd, parse_int_arg(args, 0)?)),

        // --- Session ---
        "save_session" => Ok((Jwm::save_session, parse_int_arg(args, 0)?)),
        "restore_session" => Ok((Jwm::restore_session, parse_int_arg(args, 0)?)),

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

fn parse_uint_arg(args: &Value) -> Result<WMArgEnum, String> {
    let Some(value) = scalar_arg_value(args, &["tag", "value", "v"], "a u32 tag mask")? else {
        return Ok(WMArgEnum::UInt(0));
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
    Ok(WMArgEnum::UInt(parsed))
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
        let (_, arg) = dispatch_command("view", &serde_json::Value::Null).unwrap();
        assert_eq!(arg, WMArgEnum::UInt(0));
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
                .any(|name| name == "get_capabilities")
        );
        assert!(
            capabilities
                .subscription_topics
                .iter()
                .any(|name| name == "window")
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
            uptime_ms: 42,
            health: RuntimeHealth::from_reasons(Vec::new()),
            counts: RuntimeCounts {
                windows: 3,
                monitors: 1,
                workspaces: 9,
            },
            config: serde_json::json!({"exists": true}),
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
            },
            compositor_metrics: None,
        };

        let json = serde_json::to_value(status).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["backend"], "wayland-winit");
        assert_eq!(json["health"]["status"], "healthy");
        assert_eq!(json["counts"]["windows"], 3);
        assert_eq!(json["features"]["overview"], true);
        assert!(json["compositor_metrics"].is_null());
    }
}
