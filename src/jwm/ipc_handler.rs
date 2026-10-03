// IPC handling: command processing, queries, and event broadcasting

use crate::Jwm;
use crate::application::BenchmarkRequest;
use crate::backend::api::Backend;
use crate::config::{ArgumentConfig, BackendFamily, CONFIG, get_backend_family};
use crate::core::layout::LayoutEnum;
use crate::core::models::{ClientKey, MonitorKey, WMClient, WMMonitor};
use crate::core::state::WMState;
use crate::core::types::Rect;
use crate::ipc::{
    self, CompositorTransitionStatus, IpcEvent, IpcResponse, MonitorInfoIpc, RectIpc,
    RuntimeCounts, RuntimeFeatureStates, RuntimeHealth, RuntimeStatusV1, SizeHintsIpc, TreeNode,
    WindowInfo, WorkspaceInfo,
};
use crate::ipc_server::IncomingIpc;
use crate::jwm::features::recording::RecordingFileIdentity;

fn runtime_health(
    config_status: &serde_json::Value,
    monitor_count: usize,
    compositor_transition_error: Option<&str>,
) -> RuntimeHealth {
    let mut reasons = Vec::new();
    if !config_status
        .get("exists")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        reasons.push("configuration file is missing".to_string());
    }

    let diagnostics = &config_status["diagnostics"];
    let errors = diagnostics
        .get("error_count")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let warnings = diagnostics
        .get("warning_count")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if errors != 0 {
        reasons.push(format!("configuration has {errors} error(s)"));
    }
    if warnings != 0 {
        reasons.push(format!("configuration has {warnings} warning(s)"));
    }
    if config_status["reload"]["last_success"].as_bool() == Some(false) {
        let detail = config_status["reload"]["last_error"]
            .as_str()
            .unwrap_or("unknown error");
        reasons.push(format!("last configuration reload failed: {detail}"));
    }
    if monitor_count == 0 {
        reasons.push("no monitors are available".to_string());
    }
    if let Some(error) = compositor_transition_error {
        reasons.push(format!("last compositor transition failed: {error}"));
    }
    RuntimeHealth::from_reasons(reasons)
}

fn fullscreen_screenshot_submission_response(
    submission: Result<std::path::PathBuf, String>,
) -> IpcResponse {
    match submission {
        Ok(path) => IpcResponse::ok(Some(serde_json::json!({
            "status": "queued",
            "path": path.to_string_lossy(),
        }))),
        Err(error) => IpcResponse::err(error),
    }
}

fn resolved_client_monitor_num(
    monitors: &slotmap::SlotMap<MonitorKey, WMMonitor>,
    client: &WMClient,
) -> i32 {
    client
        .mon
        .and_then(|key| monitors.get(key))
        .map_or(-1, |monitor| monitor.num)
}

#[cfg(test)]
fn tagged_client_count(state: &WMState, monitor: MonitorKey, tag_mask: u32) -> usize {
    state.monitor_clients.get(monitor).map_or(0, |clients| {
        clients
            .iter()
            .filter(|&&key| {
                state
                    .clients
                    .get(key)
                    .is_some_and(|client| client.state.tags & tag_mask != 0)
            })
            .count()
    })
}

fn active_tag_count(active_tags: u32, tag_count: usize) -> u32 {
    let tag_mask = if tag_count >= u32::BITS as usize {
        u32::MAX
    } else {
        (1u32 << tag_count) - 1
    };
    (active_tags & tag_mask).count_ones()
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct TagClientCounts {
    total: usize,
    minimized: usize,
    floating: usize,
    sticky: usize,
    urgent: usize,
    fullscreen: usize,
    pip: usize,
    maximized: usize,
    above: usize,
    below: usize,
    fixed: usize,
    scratchpad: usize,
    tabbed: usize,
    dock: usize,
    desktop: usize,
    never_focus: usize,
    demands_attention: usize,
    skip_taskbar: usize,
    skip_pager: usize,
    no_decorations: usize,
    drag_float: usize,
    swallowed: usize,
    on_view: usize,
    maximize_promoted: usize,
    strut: usize,
    status_bar: usize,
    owns_output: usize,
}

#[derive(Clone, Copy, Default)]
struct WindowFlagCounts {
    floating: usize,
    minimized: usize,
    sticky: usize,
    urgent: usize,
    fullscreen: usize,
    pip: usize,
    focused: Option<ClientKey>,
}

#[derive(Default)]
struct WindowQueryIndex {
    scratchpads: std::collections::HashMap<ClientKey, String>,
    tabs: std::collections::HashMap<(MonitorKey, ClientKey), usize>,
    monitors: std::collections::HashMap<MonitorKey, WindowMonitorProjection>,
    stack_positions: std::collections::HashMap<(MonitorKey, ClientKey), usize>,
    swallowed_by: std::collections::HashMap<ClientKey, u64>,
}

struct WindowMonitorProjection {
    connector: Option<String>,
    monitor_name: Option<String>,
    active_tags: u32,
    layout: String,
}

fn accumulate_client_counts(
    counts: &mut TagClientCounts,
    client: &WMClient,
    scratchpad: bool,
    tabbed: bool,
    has_strut: bool,
    is_status_bar: bool,
    active_tags: u32,
) {
    counts.total += 1;
    counts.minimized += usize::from(client.state.is_hidden);
    counts.floating += usize::from(client.state.is_floating);
    counts.sticky += usize::from(client.state.is_sticky);
    counts.urgent += usize::from(client.state.is_urgent || client.state.demands_attention);
    counts.fullscreen += usize::from(client.state.is_fullscreen);
    counts.pip += usize::from(client.state.is_pip);
    counts.maximized +=
        usize::from(client.state.is_maximized_vert || client.state.is_maximized_horz);
    counts.above += usize::from(client.state.is_above);
    counts.below += usize::from(client.state.is_below);
    counts.fixed += usize::from(client.state.is_fixed);
    counts.scratchpad += usize::from(scratchpad);
    counts.tabbed += usize::from(tabbed);
    counts.dock += usize::from(client.state.is_dock);
    counts.desktop += usize::from(client.state.is_desktop);
    counts.never_focus += usize::from(client.state.never_focus);
    counts.demands_attention += usize::from(client.state.demands_attention);
    counts.skip_taskbar += usize::from(client.state.skip_taskbar);
    counts.skip_pager += usize::from(client.state.skip_pager);
    counts.no_decorations += usize::from(client.state.no_decorations);
    counts.drag_float += usize::from(client.state.is_drag_floating);
    counts.swallowed += usize::from(client.state.is_swallowed);
    counts.on_view += usize::from(client.state.is_sticky || (client.state.tags & active_tags) != 0);
    counts.maximize_promoted += usize::from(client.state.maximize_restore_tiled);
    counts.strut += usize::from(has_strut);
    counts.status_bar += usize::from(is_status_bar);
    counts.owns_output += usize::from(
        client.state.is_fullscreen
            && !client.state.is_hidden
            && !client.state.is_swallowed
            && (client.state.is_sticky || (client.state.tags & active_tags) != 0),
    );
}

fn accumulate_window_counts(counts: &mut TagClientCounts, window: &WindowInfo) {
    counts.total += 1;
    counts.floating += usize::from(window.is_floating);
    counts.minimized += usize::from(window.is_minimized);
    counts.sticky += usize::from(window.is_sticky);
    counts.urgent += usize::from(window.is_urgent);
    counts.fullscreen += usize::from(window.is_fullscreen);
    counts.pip += usize::from(window.is_pip);
    counts.maximized += usize::from(window.is_maximized);
    counts.above += usize::from(window.is_above);
    counts.below += usize::from(window.is_below);
    counts.scratchpad += usize::from(window.is_scratchpad);
    counts.tabbed += usize::from(window.is_tabbed);
    counts.fixed += usize::from(window.is_fixed);
    counts.dock += usize::from(window.is_dock);
    counts.desktop += usize::from(window.is_desktop);
    counts.never_focus += usize::from(window.never_focus);
    counts.demands_attention += usize::from(window.demands_attention);
    counts.skip_taskbar += usize::from(window.skip_taskbar);
    counts.skip_pager += usize::from(window.skip_pager);
    counts.no_decorations += usize::from(window.no_decorations);
    counts.drag_float += usize::from(window.is_drag_floating);
    counts.swallowed += usize::from(window.is_swallowed);
    counts.on_view += usize::from(window.is_on_view);
    counts.maximize_promoted += usize::from(window.maximize_promoted);
    counts.strut += usize::from(window.has_strut);
    counts.status_bar += usize::from(window.is_status_bar);
    counts.owns_output += usize::from(window.owns_output);
}

fn tag_client_counts(
    state: &WMState,
    monitor: MonitorKey,
    tag_mask: u32,
    scratchpads: &std::collections::HashSet<ClientKey>,
    tabbed: &std::collections::HashSet<ClientKey>,
    strut_wins: &std::collections::HashSet<crate::backend::common_define::WindowId>,
    status_bar_name: &str,
    active_tags: u32,
) -> TagClientCounts {
    let mut counts = TagClientCounts::default();
    let Some(clients) = state.monitor_clients.get(monitor) else {
        return counts;
    };
    for &key in clients {
        let Some(client) = state.clients.get(key) else {
            continue;
        };
        if tag_mask != u32::MAX && client.state.tags & tag_mask == 0 {
            continue;
        }
        accumulate_client_counts(
            &mut counts,
            client,
            scratchpads.contains(&key),
            tabbed.contains(&key),
            strut_wins.contains(&client.win),
            client.is_status_bar(status_bar_name),
            active_tags,
        );
    }
    counts
}

fn monitor_tag_client_counts(
    state: &WMState,
    monitor: MonitorKey,
    tag_count: usize,
    scratchpads: &std::collections::HashSet<ClientKey>,
    tabbed: &std::collections::HashSet<ClientKey>,
    strut_wins: &std::collections::HashSet<crate::backend::common_define::WindowId>,
    status_bar_name: &str,
    active_tags: u32,
) -> Vec<TagClientCounts> {
    let mut counts = vec![TagClientCounts::default(); tag_count];
    let Some(clients) = state.monitor_clients.get(monitor) else {
        return counts;
    };
    for &key in clients {
        let Some(client) = state.clients.get(key) else {
            continue;
        };
        let scratchpad = scratchpads.contains(&key);
        let tabbed = tabbed.contains(&key);
        let has_strut = strut_wins.contains(&client.win);
        let is_status_bar = client.is_status_bar(status_bar_name);
        let mut tags = client.state.tags;
        while tags != 0 {
            let tag_index = tags.trailing_zeros() as usize;
            tags &= tags - 1;
            let Some(tag_counts) = counts.get_mut(tag_index) else {
                break;
            };
            accumulate_client_counts(
                tag_counts,
                client,
                scratchpad,
                tabbed,
                has_strut,
                is_status_bar,
                active_tags,
            );
        }
    }
    counts
}

fn client_window_info(
    client: &WMClient,
    monitor: i32,
    is_focused: bool,
    is_on_view: bool,
    is_scratchpad: bool,
    has_strut: bool,
    scratchpad: Option<String>,
    layout: Option<String>,
    connector: Option<String>,
    monitor_name: Option<String>,
    swallowing: Option<u64>,
    swallowed_by: Option<u64>,
    transient_for: Option<u64>,
    is_tabbed: bool,
    tab_index: Option<usize>,
    maximize_restore_anchor: Option<u64>,
    is_status_bar: bool,
    stack_index: Option<usize>,
) -> WindowInfo {
    let dock = client.state.dock_layer_info;
    WindowInfo {
        id: client.win.raw(),
        name: client.name.clone(),
        class: client.class.clone(),
        instance: client.instance.clone(),
        tags: client.state.tags,
        monitor,
        x: client.geometry.x,
        y: client.geometry.y,
        w: client.geometry.w,
        h: client.geometry.h,
        is_floating: client.state.is_floating,
        is_fullscreen: client.state.is_fullscreen,
        is_urgent: client.state.is_urgent,
        is_sticky: client.state.is_sticky,
        is_above: client.state.is_above,
        is_below: client.state.is_below,
        is_pip: client.state.is_pip,
        is_maximized: client.state.is_maximized_vert && client.state.is_maximized_horz,
        is_maximized_vert: client.state.is_maximized_vert,
        is_maximized_horz: client.state.is_maximized_horz,
        maximize_promoted: client.state.maximize_restore_tiled,
        maximize_restore: client.geometry.maximize_restore_rect.map(|r| RectIpc {
            x: r.x,
            y: r.y,
            w: r.w,
            h: r.h,
        }),
        is_minimized: client.state.is_hidden,
        minimized_order: client.state.minimized_order,
        is_swallowed: client.state.is_swallowed,
        swallowing,
        swallowed_by,
        transient_for,
        is_tabbed,
        tab_index,
        is_on_view,
        is_scratchpad,
        is_fixed: client.state.is_fixed,
        is_dock: client.state.is_dock,
        is_desktop: client.state.is_desktop,
        is_drag_floating: client.state.is_drag_floating,
        never_focus: client.state.never_focus,
        skip_taskbar: client.state.skip_taskbar,
        skip_pager: client.state.skip_pager,
        no_decorations: client.state.no_decorations,
        demands_attention: client.state.demands_attention,
        has_strut,
        client_fact: client.state.client_fact,
        border_w: client.geometry.border_w,
        is_focused,
        pid: client.pid,
        connector,
        monitor_name,
        scratchpad,
        layout,
        size_hints: client.size_hints.hints_valid.then_some(SizeHintsIpc {
            base_w: client.size_hints.base_w,
            base_h: client.size_hints.base_h,
            inc_w: client.size_hints.inc_w,
            inc_h: client.size_hints.inc_h,
            max_w: client.size_hints.max_w,
            max_h: client.size_hints.max_h,
            min_w: client.size_hints.min_w,
            min_h: client.size_hints.min_h,
            min_aspect: client.size_hints.min_aspect,
            max_aspect: client.size_hints.max_aspect,
        }),
        float_rect: RectIpc {
            x: client.geometry.floating_x,
            y: client.geometry.floating_y,
            w: client.geometry.floating_w,
            h: client.geometry.floating_h,
        },
        old_geometry: RectIpc {
            x: client.geometry.old_x,
            y: client.geometry.old_y,
            w: client.geometry.old_w,
            h: client.geometry.old_h,
        },
        old_border_w: client.geometry.old_border_w,
        hidden_restore: client.geometry.hidden_restore_rect.map(|r| RectIpc {
            x: r.x,
            y: r.y,
            w: r.w,
            h: r.h,
        }),
        maximize_restore_anchor,
        pip_restore_sticky: client.state.pip_restore_sticky,
        old_state: client.state.old_state,
        remembers_closed_placement: client.state.remembers_closed_placement,
        dock_exclusive_zone: dock.map(|d| d.exclusive_zone),
        dock_anchor_top: dock.is_some_and(|d| d.anchor_top),
        dock_anchor_bottom: dock.is_some_and(|d| d.anchor_bottom),
        dock_anchor_left: dock.is_some_and(|d| d.anchor_left),
        dock_anchor_right: dock.is_some_and(|d| d.anchor_right),
        is_status_bar,
        hidden_x: client.geometry.hidden_x,
        sync_counter: client.state.sync_counter,
        sync_value: client.state.sync_value,
        total_w: client.total_width(),
        total_h: client.total_height(),
        stack_index,
        owns_output: client.state.is_fullscreen
            && is_on_view
            && !client.state.is_hidden
            && !client.state.is_swallowed,
    }
}

/// Whether the finished recording at `path` is playable, as `probe` judges it.
///
/// The probe blocks the event thread, and only success is cached in
/// `RecordingState::finalized`. A file it rejected is therefore remembered in
/// `rejected` (`RecordingState::rejected_probe`) and not probed again until
/// its identity changes: a recorder killed mid-write leaves an MP4 with no
/// moov atom that would otherwise fork ffprobe on every status poll until the
/// next recording starts. The finalization worker's flush, `+faststart`
/// rewrite or move still changes the identity, and that earns a fresh probe.
///
/// `probe` answers `Some(verdict)` when the prober judged the file and `None`
/// when it never finished (it timed out, or could not be started for a
/// transient reason). Only a verdict is remembered: once finalized, the file
/// never changes again, so caching a probe that merely ran out of time would
/// keep `finalized` false for good.
fn recording_output_is_valid(
    path: &str,
    rejected: &mut Option<RecordingFileIdentity>,
    probe: impl FnOnce(&str) -> Option<bool>,
) -> bool {
    let Some(before) = RecordingFileIdentity::of(path) else {
        return false;
    };
    if rejected.as_ref() == Some(&before) {
        return false;
    }
    let verdict = probe(path);
    // Remember a rejection only for bytes that held still across the probe:
    // a file still being written may have failed on a half-written tail. And
    // without an mtime, an in-place rewrite of the same length is invisible.
    *rejected = (verdict == Some(false)
        && before.has_modified_time()
        && RecordingFileIdentity::of(path).as_ref() == Some(&before))
    .then_some(before);
    verdict == Some(true)
}

/// The verdict of one ffprobe run: `Some` when ffprobe judged the file, or
/// when it is not installed (a missing binary would only fail the same way on
/// every poll, and the next recording clears the rejection anyway); `None`
/// when it timed out or failed to start for any other reason, so the next
/// poll asks again.
fn ffprobe_verdict(result: std::io::Result<std::process::ExitStatus>) -> Option<bool> {
    match result {
        Ok(status) => Some(status.success()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(false),
        Err(_) => None,
    }
}

fn recording_file_is_valid(path: &str, rejected: &mut Option<RecordingFileIdentity>) -> bool {
    recording_output_is_valid(path, rejected, |path| {
        let result = crate::jwm::features::external_command::status_with_timeout(
            "ffprobe",
            &[
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=codec_name",
                "-of",
                "default=nw=1:nk=1",
                path,
            ],
            std::time::Duration::from_secs(5),
        );
        ffprobe_verdict(result)
    })
}

fn env_flag(name: &str) -> bool {
    std::env::var_os(name).as_deref() == Some(std::ffi::OsStr::new("1"))
}

fn optional_protocol_enabled(config_enabled: bool, flag_name: &str) -> bool {
    optional_protocol_enabled_from_flags(
        config_enabled,
        env_flag("JWM_OPTIONAL_GLOBALS"),
        env_flag(flag_name),
    )
}

fn optional_protocol_enabled_from_flags(
    config_enabled: bool,
    env_enable_all: bool,
    env_flag_enabled: bool,
) -> bool {
    config_enabled || env_enable_all || env_flag_enabled
}

/// Keep one atomic config transaction bounded before cloning or validating any
/// of its entries. This is deliberately higher than the command limit because
/// applying a config batch performs one final reconciliation, not one dispatch
/// per change.
const MAX_CONFIG_BATCH_CHANGES: usize = 256;
/// Commands execute synchronously on the compositor event loop, so a single
/// client must not turn one IPC frame into unbounded dispatch work.
const MAX_COMMAND_BATCH_ENTRIES: usize = 128;

fn parse_config_batch_changes(
    args: &serde_json::Value,
) -> Result<Vec<(String, serde_json::Value)>, String> {
    if let Some(values) = args.get("values").and_then(|value| value.as_object()) {
        if values.len() > MAX_CONFIG_BATCH_CHANGES {
            return Err(format!(
                "set_config_batch: too many changes ({}; maximum {})",
                values.len(),
                MAX_CONFIG_BATCH_CHANGES
            ));
        }
        let changes = values
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<Vec<_>>();
        if changes.is_empty() {
            return Err("set_config_batch: 'values' must not be empty".to_string());
        }
        return Ok(changes);
    }

    let raw_changes = args.get("changes").unwrap_or(args);
    let raw_changes = raw_changes.as_array().ok_or_else(|| {
        "set_config_batch: expected 'changes' array or 'values' object".to_string()
    })?;
    if raw_changes.len() > MAX_CONFIG_BATCH_CHANGES {
        return Err(format!(
            "set_config_batch: too many changes ({}; maximum {})",
            raw_changes.len(),
            MAX_CONFIG_BATCH_CHANGES
        ));
    }
    let changes = raw_changes
        .iter()
        .enumerate()
        .map(|(idx, item)| {
            let key = item
                .get("key")
                .and_then(|value| value.as_str())
                .ok_or_else(|| format!("set_config_batch: changes[{idx}] missing 'key' string"))?
                .to_string();
            let value = item
                .get("value")
                .cloned()
                .ok_or_else(|| format!("set_config_batch: changes[{idx}] missing 'value'"))?;
            Ok((key, value))
        })
        .collect::<Result<Vec<_>, String>>()?;

    if changes.is_empty() {
        return Err("set_config_batch: 'changes' must not be empty".to_string());
    }
    Ok(changes)
}

fn parse_command_batch_entries(
    args: &serde_json::Value,
) -> Result<Vec<(String, serde_json::Value)>, String> {
    let raw_commands = args.get("commands").unwrap_or(args);
    let raw_commands = raw_commands
        .as_array()
        .ok_or_else(|| "command_batch: expected 'commands' array".to_string())?;
    if raw_commands.len() > MAX_COMMAND_BATCH_ENTRIES {
        return Err(format!(
            "command_batch: too many commands ({}; maximum {})",
            raw_commands.len(),
            MAX_COMMAND_BATCH_ENTRIES
        ));
    }
    let commands = raw_commands
        .iter()
        .enumerate()
        .map(|(idx, item)| {
            let name = item
                .get("command")
                .or_else(|| item.get("name"))
                .and_then(|value| value.as_str())
                .ok_or_else(|| {
                    format!("command_batch: commands[{idx}] missing 'command' string")
                })?;
            if name == "command_batch" || name == "batch" {
                return Err(format!(
                    "command_batch: commands[{idx}] cannot nest '{name}'"
                ));
            }
            let args = item.get("args").cloned().unwrap_or(serde_json::Value::Null);
            Ok((name.to_string(), args))
        })
        .collect::<Result<Vec<_>, String>>()?;

    if commands.is_empty() {
        return Err("command_batch: 'commands' must not be empty".to_string());
    }
    Ok(commands)
}

fn parse_optional_u32_ipc_arg(
    args: &serde_json::Value,
    command: &str,
    field: &str,
    default: u32,
) -> Result<u32, String> {
    let Some(value) = args.get(field) else {
        return Ok(default);
    };
    let raw = value
        .as_u64()
        .ok_or_else(|| format!("{command}: '{field}' must be an unsigned 32-bit integer"))?;
    u32::try_from(raw)
        .map_err(|_| format!("{command}: '{field}' value {raw} is outside the u32 range"))
}

fn parse_benchmark_request(args: &serde_json::Value) -> Result<BenchmarkRequest, String> {
    let frames = parse_optional_u32_ipc_arg(args, "benchmark", "frames", 600)?;
    let warmup = parse_optional_u32_ipc_arg(args, "benchmark", "warmup", 60)?;
    BenchmarkRequest::new(frames, warmup).map_err(|error| format!("benchmark: {error}"))
}

fn parse_required_i32_ipc_arg(
    args: &serde_json::Value,
    command: &str,
    field: &str,
) -> Result<i32, String> {
    let value = args
        .get(field)
        .ok_or_else(|| format!("{command}: missing '{field}' (i32)"))?;

    if let Some(raw) = value.as_i64() {
        return i32::try_from(raw)
            .map_err(|_| format!("{command}: '{field}' value {raw} is outside the i32 range"));
    }
    if let Some(raw) = value.as_u64() {
        return i32::try_from(raw)
            .map_err(|_| format!("{command}: '{field}' value {raw} is outside the i32 range"));
    }

    Err(format!("{command}: '{field}' must be a 32-bit integer"))
}

fn workspace_layout_state(mon: &WMMonitor, tag_index: usize) -> (String, f32, u32, i32) {
    let current_layout = || format!("{:?}", *mon.lt);
    let Some(pertag_index) = tag_index.checked_add(1) else {
        return (
            current_layout(),
            mon.layout.m_fact,
            mon.layout.n_master,
            mon.layout.gap,
        );
    };
    let Some(pertag) = mon.pertag.as_ref() else {
        return (
            current_layout(),
            mon.layout.m_fact,
            mon.layout.n_master,
            mon.layout.gap,
        );
    };

    let layout = pertag
        .lts
        .get(pertag_index)
        .map_or_else(current_layout, |layout| format!("{:?}", **layout));
    let m_fact = pertag
        .m_facts
        .get(pertag_index)
        .copied()
        .unwrap_or(mon.layout.m_fact);
    let n_master = pertag
        .n_masters
        .get(pertag_index)
        .copied()
        .unwrap_or(mon.layout.n_master);
    let gap = pertag
        .gaps
        .get(pertag_index)
        .copied()
        .unwrap_or(mon.layout.gap);

    (layout, m_fact, n_master, gap)
}

/// Per-tag show_bar / prev_layout / selected window id for workspace IPC.
fn workspace_tag_extras(
    mon: &WMMonitor,
    tag_index: usize,
    clients: &slotmap::SlotMap<ClientKey, WMClient>,
) -> (bool, String, Option<u64>) {
    let default_show = mon
        .pertag
        .as_ref()
        .and_then(|p| p.show_bars.get(p.cur_tag).copied())
        .unwrap_or(true);
    let default_prev = format!("{:?}", *mon.prev_lt);
    let Some(pertag_index) = tag_index.checked_add(1) else {
        return (default_show, default_prev, None);
    };
    let Some(pertag) = mon.pertag.as_ref() else {
        return (default_show, default_prev, None);
    };
    let show_bar = pertag
        .show_bars
        .get(pertag_index)
        .copied()
        .unwrap_or(default_show);
    let prev_layout = pertag
        .prev_lts
        .get(pertag_index)
        .map(|layout| format!("{:?}", **layout))
        .unwrap_or(default_prev);
    let selected_id = pertag
        .sel
        .get(pertag_index)
        .copied()
        .flatten()
        .and_then(|ck| clients.get(ck).map(|c| c.win.raw()));
    (show_bar, prev_layout, selected_id)
}

fn system_time_unix_ms(time: std::time::SystemTime) -> Option<u64> {
    time.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
}

fn protocol_bind_count(
    bind_counts: &[crate::backend::api::ProtocolBindStatus],
    name: &str,
) -> Option<u64> {
    bind_counts
        .iter()
        .find(|status| status.protocol == name)
        .map(|status| status.bind_count)
}

fn protocol_last_bound_unix_ms(
    bind_counts: &[crate::backend::api::ProtocolBindStatus],
    name: &str,
) -> Option<u64> {
    bind_counts
        .iter()
        .find(|status| status.protocol == name)
        .and_then(|status| status.last_bound_unix_ms)
}

fn protocol_catalog(
    protocols: &serde_json::Value,
    bind_counts: &[crate::backend::api::ProtocolBindStatus],
) -> Vec<serde_json::Value> {
    let mut catalog = Vec::new();

    if let Some(core) = protocols.get("core").and_then(|v| v.as_array()) {
        for protocol in core {
            let name = protocol.get("name").and_then(|v| v.as_str()).unwrap_or("");
            catalog.push(serde_json::json!({
                "name": name,
                "category": "core",
                "enabled": protocol.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true),
                "published": protocol.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true),
                "bind_count": protocol_bind_count(bind_counts, name),
                "last_bound_unix_ms": protocol_last_bound_unix_ms(bind_counts, name),
                "bind_count_tracked": protocol_bind_count(bind_counts, name).is_some(),
            }));
        }
    }

    if let Some(optional) = protocols.get("optional").and_then(|v| v.as_array()) {
        for protocol in optional {
            let name = protocol.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let enabled = protocol
                .get("enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let count = protocol_bind_count(bind_counts, name).unwrap_or(0);
            catalog.push(serde_json::json!({
                "name": name,
                "category": "optional",
                "enabled": enabled,
                "published": enabled,
                "default_enabled": protocol.get("default_enabled").and_then(|v| v.as_bool()).unwrap_or(false),
                "env_flag": protocol.get("env_flag").and_then(|v| v.as_str()).unwrap_or(""),
                "bind_count": count,
                "last_bound_unix_ms": protocol_last_bound_unix_ms(bind_counts, name),
                "bind_count_tracked": true,
            }));
        }
    }

    for status in bind_counts {
        let known = catalog.iter().any(|entry| {
            entry
                .get("name")
                .and_then(|v| v.as_str())
                .map(|entry_name| entry_name == status.protocol)
                .unwrap_or(false)
        });
        if !known {
            catalog.push(serde_json::json!({
                "name": status.protocol,
                "category": "runtime_only",
                "enabled": true,
                "published": true,
                "bind_count": status.bind_count,
                "last_bound_unix_ms": status.last_bound_unix_ms,
                "bind_count_tracked": true,
            }));
        }
    }

    catalog.sort_by(|a, b| {
        let a_name = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let b_name = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
        a_name.cmp(b_name)
    });
    catalog
}

fn color_transfer_name(tf_named: Option<u32>) -> &'static str {
    match tf_named {
        Some(1) => "bt1886",
        Some(2) => "gamma22",
        Some(5) => "ext_linear",
        Some(9) => "srgb",
        Some(11) => "st2084_pq",
        Some(13) => "hlg",
        Some(_) => "unknown",
        None => "custom_or_unset",
    }
}

fn color_primaries_name(primaries_named: Option<u32>) -> &'static str {
    match primaries_named {
        Some(1) => "srgb",
        Some(6) => "bt2020",
        Some(_) => "unknown",
        None => "custom_or_unset",
    }
}

fn color_surface_is_hdr(surface: &crate::backend::api::ColorManagedSurfaceInfo) -> bool {
    matches!(color_transfer_name(surface.tf_named), "st2084_pq" | "hlg")
        || color_primaries_name(surface.primaries_named) == "bt2020"
        || surface.max_lum.is_some_and(|value| value > 300)
        || surface.max_cll.is_some_and(|value| value > 300)
}

fn color_managed_surface_json(
    surface: &crate::backend::api::ColorManagedSurfaceInfo,
) -> serde_json::Value {
    serde_json::json!({
        "surface_object_id": surface.surface_object_id,
        "identity": surface.identity,
        "transfer_function": color_transfer_name(surface.tf_named),
        "tf_named": surface.tf_named,
        "tf_power": surface.tf_power,
        "primaries": color_primaries_name(surface.primaries_named),
        "primaries_named": surface.primaries_named,
        "primaries_xy": surface.primaries,
        "hdr": color_surface_is_hdr(surface),
        "luminance": {
            "min": surface.min_lum,
            "max": surface.max_lum,
            "reference": surface.reference_lum,
        },
        "mastering": {
            "primaries_xy": surface.mastering_primaries,
            "min_luminance": surface.mastering_min_lum,
            "max_luminance": surface.mastering_max_lum,
        },
        "content_light": {
            "max_cll": surface.max_cll,
            "max_fall": surface.max_fall,
        },
    })
}

fn color_surface_summary_json(
    surfaces: &[crate::backend::api::ColorManagedSurfaceInfo],
) -> serde_json::Value {
    let mut transfer_functions = std::collections::BTreeMap::<String, usize>::new();
    let mut primaries = std::collections::BTreeMap::<String, usize>::new();
    let mut hdr_surface_count = 0usize;
    let mut max_luminance_peak = None::<u32>;

    for surface in surfaces {
        *transfer_functions
            .entry(color_transfer_name(surface.tf_named).to_string())
            .or_default() += 1;
        *primaries
            .entry(color_primaries_name(surface.primaries_named).to_string())
            .or_default() += 1;
        if color_surface_is_hdr(surface) {
            hdr_surface_count += 1;
        }
        if let Some(max_lum) = surface.max_lum {
            max_luminance_peak = Some(max_luminance_peak.map_or(max_lum, |peak| peak.max(max_lum)));
        }
    }

    serde_json::json!({
        "surface_count": surfaces.len(),
        "hdr_surface_count": hdr_surface_count,
        "transfer_functions": transfer_functions,
        "primaries": primaries,
        "max_luminance_peak": max_luminance_peak,
    })
}

/// Whether a `color_delivery` snapshot records a presentation that carried HDR
/// metadata — on one named output, or on any of them when `output` is `None`.
///
/// The paths here must match `api::ColorDeliveryStatus`'s serialization
/// exactly: the per-output field is `output_name`, not `name`. A wrong path
/// does not fail, it silently answers false forever, which would leave every
/// HDR report reading as SDR on a display that had successfully switched.
/// `presented_with_hdr_reads_the_real_serialized_shape` pins it against a
/// value produced by serde rather than one written by hand.
fn presented_with_hdr(color_delivery: Option<&serde_json::Value>, output: Option<&str>) -> bool {
    color_delivery
        .and_then(|delivery| delivery.get("outputs"))
        .and_then(serde_json::Value::as_array)
        .is_some_and(|outputs| {
            outputs.iter().any(|entry| {
                let named = output.is_none_or(|name| {
                    entry.get("output_name").and_then(serde_json::Value::as_str) == Some(name)
                });
                named
                    && entry
                        .get("last_success")
                        .and_then(|success| success.get("hdr_metadata_active"))
                        .and_then(serde_json::Value::as_bool)
                        == Some(true)
            })
        })
}

fn color_session_policy_json(
    outputs: &[crate::backend::api::OutputInfo],
    hdr_enabled: bool,
    render_path_enabled: bool,
    scene_linear_enabled: bool,
    advanced_enabled: bool,
    delivery_observation_available: bool,
    // `hdr_refusals`: per-output refusal names from the compositor's own
    // gate, empty on backends without one. Reported instead of a hardcoded
    // blocker so this object cannot claim HDR is unavailable for a reason the
    // code no longer applies. `hdr_observed_active`: whether any output
    // reports HDR metadata on its last successful presentation — derived,
    // never inferred from configuration.
    hdr_refusals: &[(String, Option<String>)],
    hdr_enable_available: bool,
    hdr_observed_active: bool,
) -> serde_json::Value {
    use crate::backend::color_policy::{params_from_edid, srgb_params};

    let hdr_output_count = outputs.iter().filter(|output| output.hdr_capable).count();
    let sdr_output_count = outputs.len().saturating_sub(hdr_output_count);
    let mixed_hdr_outputs = hdr_output_count > 0 && sdr_output_count > 0;
    let output_profile_classes = outputs
        .iter()
        .map(|output| {
            // Profile heterogeneity describes attached-output capability,
            // independent of whether the advanced render gate is currently
            // enabled. The gate is reported separately as a blocker below.
            let params = output
                .hdr_metadata
                .as_ref()
                .map(params_from_edid)
                .unwrap_or_else(srgb_params);
            (params.primaries_named, params.tf_named)
        })
        .collect::<std::collections::BTreeSet<_>>();
    let heterogeneous_output_profiles = output_profile_classes.len() > 1;
    // `behavior.hdr_enabled` controls JWM's HDR post-process policy; output
    // transfer selection is independently derived from EDID + the advanced
    // color-management gate.  Keep the compatibility field below, but label
    // it as a request rather than an observed scanout route.
    let hdr_postprocess_requested = hdr_enabled && hdr_output_count > 0;

    let sdr_on_hdr_policy = if !hdr_postprocess_requested {
        "hdr_disabled_or_no_hdr_outputs"
    } else if render_path_enabled && advanced_enabled {
        "normalized_transfer_and_gamut_without_absolute_luminance_mapping"
    } else {
        "legacy_sdr_passthrough_on_hdr_output"
    };

    let mixed_hdr_policy = if !advanced_enabled || !render_path_enabled {
        "safe_srgb_legacy_compositing"
    } else if !heterogeneous_output_profiles {
        "single_output_class"
    } else if scene_linear_enabled {
        "per_output_delivery_infrastructure_sdr_signalling_fail_closed"
    } else {
        "per_surface_transform_without_scene_linear_blending"
    };

    let mut blockers = Vec::new();
    if hdr_postprocess_requested && !advanced_enabled {
        blockers.push("advanced_color_management_disabled");
    }
    if hdr_postprocess_requested && !render_path_enabled {
        blockers.push("color_management_render_path_disabled");
    }
    if heterogeneous_output_profiles && !scene_linear_enabled {
        blockers.push("scene_linear_compositing_inactive");
    }
    // The compositor's own per-output gate is the authority on why HDR
    // signalling is not asserted. Before it existed this pushed one fixed
    // string, which stayed in the payload long after the condition it named
    // was addressed.
    // One reason is one entry: the gate's configuration refusal serialises
    // under the same wire name as the static entry above, and a consumer
    // counting or diffing this array must not see a phantom second blocker.
    // Static entries keep their place; gate names follow in a stable order.
    if hdr_postprocess_requested {
        for name in hdr_refusals
            .iter()
            .filter_map(|(_, refusal)| refusal.as_deref())
            .collect::<std::collections::BTreeSet<_>>()
        {
            if !blockers.contains(&name) {
                blockers.push(name);
            }
        }
    }
    // "Available" means the enable *command* would be accepted, which is what
    // a toggle needs to know. It is not "nothing is blocking right now":
    // a momentary refusal — a toast on screen, this frame's delivery route —
    // is exactly what the request latch carries across, so reading one as
    // unavailable would hide the feature on hardware that supports it. The
    // backend answers with the command's own rule rather than this object
    // re-deriving it from wire names.
    let hdr_signalling_enable_available = hdr_enable_available;

    // This object remains the static policy/capability view. The sibling
    // `color_delivery` object carries the separately versioned, vblank-backed
    // attempt and last-success observations.
    let per_output_delivery_available = render_path_enabled && scene_linear_enabled;

    serde_json::json!({
        "hdr_output_count": hdr_output_count,
        "sdr_output_count": sdr_output_count,
        "mixed_hdr_outputs": mixed_hdr_outputs,
        "heterogeneous_output_profiles": heterogeneous_output_profiles,
        // Compatibility field reflects the physical signal, never a
        // configuration request: it is the last successful presentation's
        // own report, not something derived from what was asked for.
        "hdr_active": hdr_observed_active,
        "hdr_active_semantics": "physical_output_signal_last_successful_presentation",
        // Per-output: the name of the first refusal the compositor's gate
        // reported, or null where an assertion is currently legal.
        "hdr_enable_refusals": hdr_refusals
            .iter()
            .map(|(output, refusal)| serde_json::json!({
                "output": output,
                "refusal": refusal,
            }))
            .collect::<Vec<_>>(),
        "hdr_postprocess_requested_on_capable_output": hdr_postprocess_requested,
        "sdr_on_hdr_policy": sdr_on_hdr_policy,
        "mixed_hdr_policy": mixed_hdr_policy,
        "scene_linear_enabled": scene_linear_enabled,
        "blockers": blockers,
        "delivery_capabilities": {
            "capability": if per_output_delivery_available {
                "normalized_linear_srgb_per_output_delivery_infrastructure"
            } else {
                "inactive"
            },
            "working_space": if scene_linear_enabled {
                "normalized_linear_srgb"
            } else {
                "legacy_encoded_srgb"
            },
            "luminance_model": "source_transfer_native_normalized",
            "software_per_output_encode_regions": per_output_delivery_available,
            "software_region_scope": "linear_tail_safe_supported_physical_regions",
            "software_region_requirements": [
                "nonnegative_physical_origin",
                "unit_scale",
                "normal_transform",
                "nonconflicting_overlap"
            ],
            "hardware_delivery_policy": "paired_all_output_crtc_lut_ctm_or_none",
            "runtime_output_signal_policy": "per_output_fail_closed_reconciled_each_frame",
            "hdr_signalling_enable_available": hdr_signalling_enable_available,
        },
        "fallback_policy": {
            "route": "global_srgb",
            "selection": "per_frame",
            "route_observation": if delivery_observation_available {
                "see_color_delivery_last_success"
            } else {
                "unavailable_on_backend"
            },
            "triggers": [
                "encoded_late_overlay",
                "kms_external_cursor",
                "kms_external_drag_icon",
                "kms_external_lock_surface",
                "kms_external_top_or_overlay_layer",
                "capture",
                "unsupported_output_topology",
                "scene_linear_target_unavailable"
            ],
            "normal_desktop_cursor_effect": "usually_selects_global_srgb_fallback"
        },
        // What is still true. Absolute luminance, the surface-description
        // commit latch, KMS atomic colour delivery and external-element
        // internalization all landed; leaving them listed made this array
        // contradict the code and, worse, made it useless for deciding
        // whether an enable is safe.
        "limitations": [
            "non_d65_chromatic_adaptation_unavailable",
            "hardware_lut_route_clips_hdr_headroom",
            "hdr_signalling_requires_software_per_output_delivery_route",
            "framebuffer_and_color_properties_paired_by_ordering_not_one_ioctl",
            "direct_scanout_blocked_while_scene_linear_active"
        ],
    })
}

fn output_color_policy_json(
    output: &crate::backend::api::OutputInfo,
    kms_color: Option<&serde_json::Value>,
    render_path_enabled: bool,
    advanced_enabled: bool,
    delivery_observation_available: bool,
    // Whether this output's last successful presentation carried HDR
    // metadata. The runtime profile is what the display is actually being
    // told, so it follows the observation and never the configuration.
    hdr_signalled: bool,
) -> serde_json::Value {
    use crate::backend::color_policy::{params_from_edid, srgb_params};

    let capability_source = if !advanced_enabled {
        "srgb_safe_default"
    } else if output.hdr_metadata.is_some() {
        "edid_hdr_capability"
    } else {
        "srgb_no_edid"
    };
    let capability_params = if advanced_enabled {
        output
            .hdr_metadata
            .as_ref()
            .map(params_from_edid)
            .unwrap_or_else(srgb_params)
    } else {
        srgb_params()
    };
    // Was a literal `srgb_params()`, which kept reporting sRGB even on an
    // output that had successfully switched — making the per-output profile
    // useless for verifying an enable. It now mirrors `params_for_output`:
    // the EDID-derived profile exactly while HDR is signalled, sRGB
    // otherwise.
    // Signalling HDR requires the advanced gate (the enable policy refuses
    // otherwise), so this combination is unreachable in practice — but the
    // two halves of the report must agree even then.
    let runtime_hdr_profile = advanced_enabled && hdr_signalled;
    let runtime_params = if runtime_hdr_profile {
        output
            .hdr_metadata
            .as_ref()
            .map(params_from_edid)
            .unwrap_or_else(srgb_params)
    } else {
        srgb_params()
    };
    let kms_ctm = kms_color
        .and_then(|c| c.get("ctm_supported"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let kms_gamma = kms_color
        .and_then(|c| c.get("gamma_lut_supported"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let wants_non_srgb = capability_params.primaries_named != Some(1)
        || capability_params.tf_named != Some(crate::backend::color_policy::TF_SRGB);
    let shader_fallback_required = render_path_enabled && wants_non_srgb && !(kms_ctm && kms_gamma);

    serde_json::json!({
        "advanced_enabled": advanced_enabled,
        "render_path_enabled": render_path_enabled,
        "policy_source": if runtime_hdr_profile {
            "runtime_edid_hdr_profile_signalled"
        } else {
            "runtime_srgb_fail_closed"
        },
        "capability_source": capability_source,
        "selected_transfer_function": color_transfer_name(runtime_params.tf_named),
        "selected_transfer_function_raw": runtime_params.tf_named,
        "selected_primaries": color_primaries_name(runtime_params.primaries_named),
        "selected_primaries_raw": runtime_params.primaries_named,
        "capability_transfer_function": color_transfer_name(capability_params.tf_named),
        "capability_transfer_function_raw": capability_params.tf_named,
        "capability_primaries": color_primaries_name(capability_params.primaries_named),
        "capability_primaries_raw": capability_params.primaries_named,
        "min_luminance": capability_params.min_lum,
        "max_luminance": capability_params.max_lum,
        "reference_luminance": capability_params.reference_lum,
        "non_srgb_profile_needs_software_without_kms_pair": shader_fallback_required,
        // Compatibility alias: this is a static delivery requirement, not a
        // claim that the last frame actually took a shader route.
        "shader_fallback_required": shader_fallback_required,
        "shader_fallback_semantics": "static_non_srgb_profile_hint_not_active_route",
        "selected_profile_semantics": "last_successful_presentation_output_target",
        "delivery_route_observation": if delivery_observation_available {
            "see_color_delivery_last_success"
        } else {
            "unavailable_on_backend"
        },
    })
}

/// Maximum outputs listed per external element class, and maximum bytes in
/// one output name, when reflecting the recorded per-class plan.
const MAX_EXTERNAL_ELEMENT_OUTPUTS: usize = 64;
const MAX_EXTERNAL_ELEMENT_OUTPUT_NAME_BYTES: usize = 256;

/// Reflect the per-class external-element plan recorded with a policy
/// decision, but only when every entry is structurally valid; a malformed
/// payload collapses to `None` the same way an invalid blocker inventory
/// does. Entries are rebuilt from known keys with bounded counts and bounded
/// name lengths, so a future or hostile payload cannot smuggle arbitrary
/// fields into the diagnostic mirror.
fn sanitized_external_element_classes(value: &serde_json::Value) -> Option<serde_json::Value> {
    let entries = value.as_array()?;
    if entries.len() > crate::backend::api::MAX_EXTERNAL_ELEMENT_CLASSES {
        return None;
    }
    let mut sanitized = Vec::with_capacity(entries.len());
    for entry in entries {
        let object = entry.as_object()?;
        let class = object.get("class")?.as_str()?;
        if !crate::backend::api::linear_tail_blocker_name_is_valid(class) {
            return None;
        }
        let visible = object.get("visible")?.as_bool()?;
        let importable = object.get("importable")?.as_bool()?;
        let assembly = object.get("assembly")?.as_str()?;
        if !matches!(assembly, "kms_external" | "common_linear" | "none") {
            return None;
        }
        let blocker = match object.get("blocker") {
            None | Some(serde_json::Value::Null) => None,
            Some(value) => {
                let name = value.as_str()?;
                if !crate::backend::api::linear_tail_blocker_name_is_valid(name) {
                    return None;
                }
                Some(name.to_owned())
            }
        };
        let output_values = object.get("outputs")?.as_array()?;
        if output_values.len() > MAX_EXTERNAL_ELEMENT_OUTPUTS {
            return None;
        }
        let mut outputs = Vec::with_capacity(output_values.len());
        for output in output_values {
            let name = output.as_str()?;
            if name.len() > MAX_EXTERNAL_ELEMENT_OUTPUT_NAME_BYTES {
                return None;
            }
            outputs.push(name.to_owned());
        }
        // Basis tokens share the blocker-name grammar (snake_case, bounded).
        let basis = object.get("basis")?.as_str()?;
        if !crate::backend::api::linear_tail_blocker_name_is_valid(basis) {
            return None;
        }
        sanitized.push(serde_json::json!({
            "class": class,
            "visible": visible,
            "importable": importable,
            "assembly": assembly,
            "blocker": blocker,
            "outputs": outputs,
            "basis": basis,
        }));
    }
    Some(serde_json::Value::Array(sanitized))
}

fn render_decisions_json(
    direct_scanout: Option<&serde_json::Value>,
    blur: Option<&serde_json::Value>,
    outputs: &[serde_json::Value],
    color_delivery: Option<&serde_json::Value>,
    tearing_hint_count: usize,
    presentation: &[crate::backend::api::PresentationOutputStatus],
    hdr_config_enabled: bool,
    blur_config_enabled: bool,
    color_render_path_enabled: bool,
    scene_linear_target_active: bool,
    color_advanced_enabled: bool,
    kms_color_offload_enabled: bool,
) -> serde_json::Value {
    let direct_scanout_decision = match direct_scanout {
        Some(scanout) => {
            let enabled = scanout
                .get("enabled")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            let active = scanout
                .get("active")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            let compositor_reason = scanout
                .get("compositor_reason")
                .and_then(|value| value.as_str())
                .unwrap_or("unknown");
            let kms_blockers = scanout
                .get("kms_outputs")
                .and_then(|value| value.as_array())
                .map(|outputs| {
                    outputs
                        .iter()
                        .filter(|output| {
                            output.get("eligible").and_then(|value| value.as_bool())
                                == Some(false)
                        })
                        .map(|output| {
                            serde_json::json!({
                                "scope": "kms_output",
                                "output": output.get("output_name").and_then(|value| value.as_str()).unwrap_or("unknown"),
                                "reason": output.get("reason").and_then(|value| value.as_str()).unwrap_or("unknown"),
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let mut blockers = Vec::new();
            if !active && !compositor_reason.is_empty() && compositor_reason != "active" {
                blockers.push(serde_json::json!({
                    "scope": "compositor",
                    "reason": compositor_reason,
                }));
            }
            blockers.extend(kms_blockers);
            let reason = if active {
                "active".to_string()
            } else if !enabled {
                "disabled".to_string()
            } else {
                compositor_reason.to_string()
            };
            serde_json::json!({
                "configured": enabled,
                "active": active,
                "reason": reason,
                "candidate_count": scanout.get("candidate_count").and_then(|value| value.as_u64()).unwrap_or(0),
                "blockers": blockers,
            })
        }
        None => serde_json::json!({
            "configured": false,
            "active": false,
            "reason": "status_unavailable",
            "candidate_count": 0,
            "blockers": [{
                "scope": "compositor",
                "reason": "compositor not active or direct-scanout status unavailable",
            }],
        }),
    };

    let blur_decision = match blur {
        Some(blur) => {
            let strength = blur
                .get("current_strength")
                .and_then(|value| value.as_u64())
                .unwrap_or(0);
            let temporal_enabled = blur
                .get("temporal_enabled")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            let active = blur_config_enabled && strength > 0;
            let reason = if active {
                "active"
            } else if !blur_config_enabled {
                "disabled_by_config"
            } else {
                "strength_zero"
            };
            serde_json::json!({
                "configured": blur_config_enabled,
                "active": active,
                "reason": reason,
                "current_strength": strength,
                "temporal_active": temporal_enabled,
                "temporal_reuse_rate_pct": blur.get("temporal_reuse_rate_pct").and_then(|value| value.as_f64()).unwrap_or(0.0),
            })
        }
        None => serde_json::json!({
            "configured": blur_config_enabled,
            "active": false,
            "reason": "status_unavailable",
            "current_strength": 0,
            "temporal_active": false,
            "temporal_reuse_rate_pct": 0.0,
        }),
    };

    let hdr_capable_output_count = outputs
        .iter()
        .filter(|output| output.get("hdr_capable").and_then(|value| value.as_bool()) == Some(true))
        .count();
    let hdr_requested_on_capable_output = hdr_config_enabled && hdr_capable_output_count > 0;
    let participating_delivery_outputs = color_delivery
        .and_then(|status| status.get("outputs"))
        .and_then(|value| value.as_array())
        .map(|outputs| {
            outputs
                .iter()
                .filter(|output| {
                    output
                        .get("participating")
                        .and_then(|value| value.as_bool())
                        == Some(true)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let current_policy_decision =
        color_delivery.and_then(|status| status.get("last_policy_decision"));
    let current_policy_sequence = current_policy_decision
        .and_then(|decision| decision.get("sequence"))
        .and_then(|value| value.as_u64());
    let current_linear_tail_safe = current_policy_decision
        .and_then(|decision| decision.get("linear_tail_safe"))
        .and_then(|value| value.as_bool());
    let current_linear_tail_blockers_value =
        current_policy_decision.and_then(|decision| decision.get("linear_tail_blockers"));
    let linear_tail_inventory = crate::backend::api::summarize_linear_tail_inventory_value(
        current_linear_tail_safe,
        current_linear_tail_blockers_value,
    );
    // Preserve a bounded, structurally valid future inventory in diagnostics,
    // including one whose aggregate safe bit disagrees. Malformed entries or
    // an over-limit array are classified without reflecting their contents.
    let current_linear_tail_blockers = current_linear_tail_blockers_value
        .and_then(serde_json::Value::as_array)
        .filter(|blockers| blockers.len() <= crate::backend::api::MAX_LINEAR_TAIL_BLOCKERS)
        .filter(|blockers| {
            blockers.iter().all(|blocker| {
                blocker
                    .as_str()
                    .is_some_and(crate::backend::api::linear_tail_blocker_name_is_valid)
            })
        })
        .filter(|blockers| {
            blockers.iter().enumerate().all(|(index, blocker)| {
                !blockers[..index].iter().any(|previous| previous == blocker)
            })
        });
    // The per-class external-element plan recorded with the policy decision;
    // absent (legacy payload or legacy render path) stays null, and a
    // structurally invalid payload collapses to null rather than reflecting.
    let external_elements = current_policy_decision
        .and_then(|decision| decision.get("external_elements"))
        .filter(|value| !value.is_null())
        .and_then(sanitized_external_element_classes);
    let delivery_output_count = participating_delivery_outputs.len();
    let successful_deliveries = participating_delivery_outputs
        .iter()
        .filter_map(|output| output.get("last_success"))
        .filter(|success| !success.is_null())
        .collect::<Vec<_>>();
    let observed_policy_sequences = successful_deliveries
        .iter()
        .filter_map(|success| {
            success
                .get("policy_sequence")
                .and_then(|value| value.as_u64())
        })
        .collect::<std::collections::BTreeSet<_>>();
    let delivery_observed = !successful_deliveries.is_empty();
    let delivery_observation_complete =
        delivery_output_count > 0 && successful_deliveries.len() == delivery_output_count;
    let observed_hdr_active = successful_deliveries.iter().any(|success| {
        success
            .get("hdr_metadata_active")
            .and_then(|value| value.as_bool())
            == Some(true)
    });
    let hdr_decision = serde_json::json!({
        "configured": hdr_config_enabled,
        "active": if observed_hdr_active {
            Some(true)
        } else if delivery_observation_complete {
            Some(false)
        } else {
            None
        },
        "active_observation": if observed_hdr_active || delivery_observation_complete {
            "last_successful_presentation"
        } else if delivery_observed {
            "partial_last_successful_presentation"
        } else {
            "no_successful_presentation_observed"
        },
        "observed_output_count": successful_deliveries.len(),
        "expected_output_count": delivery_output_count,
        "policy_sequence": current_policy_sequence,
        "observed_policy_sequences": &observed_policy_sequences,
        "requested_on_capable_output": hdr_requested_on_capable_output,
        "reason": if observed_hdr_active {
            "presented_with_kms_hdr_metadata"
        } else if delivery_observation_complete {
            "presented_without_kms_hdr_metadata"
        } else if delivery_observed {
            "partial_output_observation"
        } else if hdr_requested_on_capable_output {
            "no_successful_presentation_observed"
        } else if hdr_config_enabled {
            "no_hdr_capable_outputs"
        } else {
            "disabled_by_config"
        },
        "capable_output_count": hdr_capable_output_count,
    });

    let non_srgb_software_requirement_output_count = outputs
        .iter()
        .filter(|output| {
            output
                .get("color_management")
                .and_then(|value| {
                    value
                        .get("non_srgb_profile_needs_software_without_kms_pair")
                        .or_else(|| value.get("shader_fallback_required"))
                })
                .and_then(|value| value.as_bool())
                == Some(true)
        })
        .count();
    let mut observed_routes = std::collections::BTreeMap::<String, usize>::new();
    for success in &successful_deliveries {
        let route = success
            .get("route")
            .and_then(|value| value.as_str())
            .unwrap_or("unknown");
        *observed_routes.entry(route.to_string()).or_default() += 1;
    }
    let observed_color_pipeline_active = successful_deliveries.iter().any(|success| {
        matches!(
            success.get("route").and_then(|value| value.as_str()),
            Some("software_per_output_regions" | "kms_ctm_gamma_lut")
        )
    });
    let color_pipeline_decision = serde_json::json!({
        "configured": color_render_path_enabled,
        "active": if observed_color_pipeline_active {
            Some(true)
        } else if delivery_observation_complete {
            Some(false)
        } else {
            None
        },
        "active_observation": if observed_color_pipeline_active || delivery_observation_complete {
            "last_successful_presentation"
        } else if delivery_observed {
            "partial_last_successful_presentation"
        } else {
            "no_successful_presentation_observed"
        },
        "observed_output_count": successful_deliveries.len(),
        "expected_output_count": delivery_output_count,
        "policy_sequence": current_policy_sequence,
        "observed_policy_sequences": &observed_policy_sequences,
        "observed_routes": observed_routes,
        // Compatibility classification retained for existing IPC consumers.
        "linear_tail_observation": match linear_tail_inventory.observation {
            crate::backend::api::LinearTailInventoryObservation::Malformed => "malformed",
            crate::backend::api::LinearTailInventoryObservation::Unknown => "unknown_or_not_applicable",
            crate::backend::api::LinearTailInventoryObservation::ObservedClear
            | crate::backend::api::LinearTailInventoryObservation::ObservedBlocked => "observed_current_policy",
        },
        "linear_tail_inventory_state": linear_tail_inventory.observation.wire_name(),
        "linear_tail_inventory_issue": linear_tail_inventory.issue.map(|issue| issue.wire_name()),
        "linear_tail_blocker_count": linear_tail_inventory.blocker_count,
        "linear_tail_known_blocker_count": linear_tail_inventory.known_blocker_count,
        "linear_tail_unknown_blocker_count": linear_tail_inventory.unknown_blocker_count,
        "linear_tail_safe": current_linear_tail_safe,
        "linear_tail_blockers": current_linear_tail_blockers,
        "linear_tail_inventory_consistent": linear_tail_inventory.is_consistent(),
        "external_elements": external_elements,
        "scene_linear_target_active": scene_linear_target_active,
        "capability": if color_render_path_enabled && scene_linear_target_active {
            "normalized_linear_srgb_per_output_delivery"
        } else if color_render_path_enabled {
            "per_surface_encoded_color_transform"
        } else {
            "disabled"
        },
        "advanced_protocol_enabled": color_advanced_enabled,
        "kms_offload_configured": kms_color_offload_enabled,
        "non_srgb_software_requirement_output_count": non_srgb_software_requirement_output_count,
        // Compatibility alias; it does not identify the active frame route.
        "shader_fallback_output_count": non_srgb_software_requirement_output_count,
        "reason": if !color_render_path_enabled {
            "render_path_disabled_by_config"
        } else if !scene_linear_target_active {
            "scene_linear_target_inactive"
        } else if delivery_observed {
            "last_successful_presentation_observed"
        } else {
            "no_successful_presentation_observed"
        },
    });

    // schema v2: `tearing.active` reports what the compositor DID, not what
    // a client asked for. Under v1 it was literally `hint_count > 0`, which
    // made a client merely requesting tearing indistinguishable from a frame
    // that actually tore — and JWM does not tear at all yet, so v1 reported
    // `active: true` for something that never happened. Client demand moved
    // to `client_demand`; every consumer reading `active` has to be updated
    // with the version, which is why this is not a silent change.
    let tearing_outputs = presentation
        .iter()
        .map(|output| {
            serde_json::json!({
                "output": output.output_name,
                "client_asked_to_tear": output.client_asked_to_tear,
                "vrr": output.vrr,
                "tearing": output.tearing,
                "blocker": output.blocker,
            })
        })
        .collect::<Vec<_>>();
    let tearing_active = presentation.iter().any(|output| output.tearing);
    // Prefer the blocker from an output that actually had demand: one
    // output's "nobody asked" is not the session's reason when another
    // output has a client waiting.
    let tearing_reason = if tearing_active {
        Some("async_page_flip_issued".to_string())
    } else {
        presentation
            .iter()
            .find(|output| output.client_asked_to_tear)
            .or_else(|| presentation.first())
            .and_then(|output| output.blocker.clone())
    };

    serde_json::json!({
        "schema_version": 2,
        "observation_semantics": "tri_state_last_successful_presentation",
        "direct_scanout": direct_scanout_decision,
        "blur": blur_decision,
        "hdr": hdr_decision,
        "tearing": {
            "active": tearing_active,
            "client_demand": tearing_hint_count > 0,
            "hint_count": tearing_hint_count,
            "reason": tearing_reason,
            "outputs": tearing_outputs,
        },
        "vrr": {
            "active": presentation.iter().any(|output| output.vrr),
            "outputs": presentation
                .iter()
                .map(|output| serde_json::json!({
                    "output": output.output_name,
                    "enabled": output.vrr,
                }))
                .collect::<Vec<_>>(),
        },
        "color_pipeline": color_pipeline_decision,
    })
}

fn wayland_protocol_status() -> serde_json::Value {
    let cfg = CONFIG.load();
    let behavior = cfg.behavior();
    let core = [
        "wl_compositor",
        "wl_shm",
        "wl_data_device_manager",
        "primary_selection",
        "xdg_wm_base",
        "xdg_decoration",
        "wl_output",
        "xdg_output",
        "zwlr_layer_shell_v1",
        "xdg_activation_v1",
        "text_input",
        "input_method",
        "virtual_keyboard",
        "pointer_constraints",
        "relative_pointer",
        "session_lock",
        "idle_inhibit",
        "idle_notify",
        "fractional_scale",
        "cursor_shape",
        "presentation_time",
        "pointer_gestures",
        "tablet",
        "fifo",
        "keyboard_shortcuts_inhibit",
        "security_context",
        "commit_timing",
        "xdg_dialog",
        "xdg_foreign",
        "xdg_system_bell",
        "pointer_warp",
        "xwayland_keyboard_grab",
        "data_control",
        "ext_data_control",
        "kde_server_decoration",
        "ext_background_effect",
    ];

    let optional = [
        (
            "zwlr_screencopy_manager_v1",
            true,
            "JWM_ENABLE_SCREENCOPY",
            "behavior.wayland_enable_screencopy",
            behavior.wayland_enable_screencopy,
        ),
        (
            "wp_tearing_control_manager_v1",
            true,
            "JWM_ENABLE_TEARING_CONTROL",
            "behavior.wayland_enable_tearing_control",
            behavior.wayland_enable_tearing_control,
        ),
        (
            "wp_color_manager_v1",
            false,
            "JWM_ENABLE_COLOR_MANAGEMENT",
            "behavior.wayland_enable_color_management",
            behavior.wayland_enable_color_management,
        ),
        (
            "zwlr_output_manager_v1",
            true,
            "JWM_ENABLE_OUTPUT_MANAGEMENT",
            "behavior.wayland_enable_output_management",
            behavior.wayland_enable_output_management,
        ),
        (
            "zwlr_output_power_manager_v1",
            true,
            "JWM_ENABLE_OUTPUT_POWER",
            "behavior.wayland_enable_output_power",
            behavior.wayland_enable_output_power,
        ),
        (
            "ext_workspace_manager_v1",
            true,
            "JWM_ENABLE_WORKSPACE",
            "behavior.wayland_enable_workspace",
            behavior.wayland_enable_workspace,
        ),
        (
            "ext_image_copy_capture_manager_v1",
            true,
            "JWM_ENABLE_IMAGE_COPY_CAPTURE",
            "behavior.wayland_enable_image_copy_capture",
            behavior.wayland_enable_image_copy_capture,
        ),
        (
            "ext_output_image_capture_source_manager_v1",
            true,
            "JWM_ENABLE_IMAGE_COPY_CAPTURE",
            "behavior.wayland_enable_image_copy_capture",
            behavior.wayland_enable_image_copy_capture,
        ),
        (
            "ext_foreign_toplevel_image_capture_source_manager_v1",
            true,
            "JWM_ENABLE_IMAGE_COPY_CAPTURE",
            "behavior.wayland_enable_image_copy_capture",
            behavior.wayland_enable_image_copy_capture,
        ),
        (
            "zwlr_gamma_control_manager_v1",
            true,
            "JWM_ENABLE_GAMMA_CONTROL",
            "behavior.wayland_enable_gamma_control",
            behavior.wayland_enable_gamma_control,
        ),
        (
            "zwlr_foreign_toplevel_manager_v1",
            true,
            "JWM_ENABLE_FOREIGN_TOPLEVEL_MANAGEMENT",
            "behavior.wayland_enable_foreign_toplevel_management",
            behavior.wayland_enable_foreign_toplevel_management,
        ),
        (
            "zwlr_virtual_pointer_manager_v1",
            true,
            "JWM_ENABLE_VIRTUAL_POINTER",
            "behavior.wayland_enable_virtual_pointer",
            behavior.wayland_enable_virtual_pointer,
        ),
    ];

    serde_json::json!({
        "core": core
            .iter()
            .map(|name| serde_json::json!({ "name": name, "enabled": true }))
            .collect::<Vec<_>>(),
        "optional": optional
            .iter()
            .map(|(name, default_enabled, flag, config_key, config_enabled)| {
                serde_json::json!({
                    "name": name,
                    "enabled": optional_protocol_enabled(*config_enabled, flag),
                    "default_enabled": default_enabled,
                    "config_key": config_key,
                    "config_enabled": config_enabled,
                    "env_flag": flag,
                    "env_enabled": env_flag(flag),
                })
            })
            .collect::<Vec<_>>(),
        "env_enable_all": env_flag("JWM_OPTIONAL_GLOBALS"),
    })
}

fn recommended_scrolling_swipes(
    bindings: &[crate::config::GestureSwipeConfig],
) -> Vec<serde_json::Value> {
    let recommendations = [
        (
            3u32,
            "left",
            "scrolling_focus_column",
            ArgumentConfig::Int(1),
        ),
        (
            3u32,
            "right",
            "scrolling_focus_column",
            ArgumentConfig::Int(-1),
        ),
        (
            3u32,
            "up",
            "scrolling_focus_window",
            ArgumentConfig::Int(-1),
        ),
        (
            3u32,
            "down",
            "scrolling_focus_window",
            ArgumentConfig::Int(1),
        ),
    ];

    recommendations
        .into_iter()
        .map(|(fingers, direction, function, argument)| {
            let configured = bindings.iter().any(|binding| {
                binding.fingers == fingers && binding.direction.eq_ignore_ascii_case(direction)
            });
            serde_json::json!({
                "fingers": fingers,
                "direction": direction,
                "function": function,
                "argument": argument,
                "configured": configured,
            })
        })
        .collect()
}

/// Whether the cached control-center list fully answers `set_power_profile`
/// for `profile`, or a fresh read of the driver's list is worth starting.
///
/// That read is `powerprofilesctl list` on most hosts: a Python D-Bus client,
/// so it only ever runs on the control-center worker. A cached list that
/// offers the name answers even when stale, because a driver's profile set
/// does not change at runtime and the worker's re-read after the switch
/// confirms what took. A fresh list answers a rejection too. A stale one that
/// lacks the name — or no list at all — starts a worker read, so the retry
/// the rejection asks for names what the driver offers now.
fn cached_power_profiles_answer(
    cached: Option<&[String]>,
    cached_is_fresh: bool,
    profile: &str,
) -> bool {
    cached.is_some_and(|available| cached_is_fresh || available.iter().any(|name| name == profile))
}

/// The subscribe acknowledgement: the topics no event can match, what the
/// server stored, and what its bounds dropped and why. The ack used to carry
/// no data (`data: null`); every field here (`unknown_topics`, `subscribed`,
/// `dropped`, `dropped_total`) is new and additive, so a client that ignored
/// `data` is unaffected.
fn subscribe_ack(
    unknown: &[String],
    outcome: &crate::ipc_server::SubscriptionOutcome,
) -> serde_json::Value {
    let dropped: Vec<serde_json::Value> = outcome
        .dropped
        .iter()
        .map(|dropped| {
            serde_json::json!({
                "topic": dropped.topic,
                "reason": dropped.reason.as_str(),
            })
        })
        .collect();
    serde_json::json!({
        "unknown_topics": unknown,
        "subscribed": outcome.subscribed,
        "dropped": dropped,
        "dropped_total": outcome.dropped_total,
    })
}

/// The most unknown topics one subscribe acknowledgement names. A typo
/// report needs only a few; the cap keeps a request carrying thousands of
/// junk topics from being echoed back in full.
const MAX_REPORTED_UNKNOWN_TOPICS: usize = 16;

/// The subscription topics that no registered event family can ever match.
///
/// A subscription matches `*`, an exact event name, or any event under
/// `topic/`, and every event is named `<family>/...` after one of
/// `IPC_REGISTRY.subscription_topics`. So a topic whose first `/` segment is
/// not a registered family (`windows` rather than `window`) never delivers
/// anything. Topics are trimmed the way the server stores them; empty ones
/// are skipped because the server drops them and there is nothing to name.
fn unknown_subscription_topics(topics: &[String]) -> Vec<String> {
    let families = ipc::IPC_REGISTRY.subscription_topics;
    let mut unknown: Vec<String> = Vec::new();
    for topic in topics {
        let topic = topic.trim();
        let family = topic.split('/').next().unwrap_or(topic);
        let known = topic == "*" || (family != "*" && families.contains(&family));
        if topic.is_empty() || known || unknown.iter().any(|seen| seen == topic) {
            continue;
        }
        unknown.push(topic.to_string());
        if unknown.len() == MAX_REPORTED_UNKNOWN_TOPICS {
            break;
        }
    }
    unknown
}

impl Jwm {
    pub(crate) fn process_ipc(&mut self, backend: &mut dyn Backend) {
        let ipc = match self.ipc_server.as_mut() {
            Some(s) => s,
            None => return,
        };

        ipc.accept_connections();
        let messages = ipc.poll_clients();

        for msg in messages {
            match msg {
                IncomingIpc::Command {
                    client_id,
                    name,
                    args,
                } => {
                    let resp = self.handle_ipc_command(backend, &name, &args);
                    if let Some(ipc) = self.ipc_server.as_mut() {
                        ipc.respond(client_id, &resp);
                    }
                }
                IncomingIpc::Query {
                    client_id,
                    name,
                    args,
                } => {
                    let resp = self.handle_ipc_query(&name, &args, backend);
                    if let Some(ipc) = self.ipc_server.as_mut() {
                        ipc.respond(client_id, &resp);
                    }
                }
                IncomingIpc::Subscribe { client_id, topics } => {
                    if let Some(ipc) = self.ipc_server.as_mut() {
                        // The subscription is stored as asked, within the
                        // server's bounds; the ack names the topics no event
                        // can ever match, so a typo such as `windows` is not
                        // a silent success that never delivers anything, and
                        // what the bounds dropped, so truncation is not
                        // silent either.
                        let unknown = unknown_subscription_topics(&topics);
                        if !unknown.is_empty() {
                            log::warn!(
                                "[ipc] client {client_id} subscribed to unknown topics {unknown:?}"
                            );
                        }
                        let outcome = ipc.subscribe(client_id, topics);
                        if outcome.dropped_total > 0 {
                            log::warn!(
                                "[ipc] client {client_id}: {} subscription topic(s) not stored",
                                outcome.dropped_total
                            );
                        }
                        ipc.respond(
                            client_id,
                            &IpcResponse::ok(Some(subscribe_ack(&unknown, &outcome))),
                        );
                    }
                }
            }
        }
    }

    pub(crate) fn handle_ipc_command(
        &mut self,
        backend: &mut dyn Backend,
        name: &str,
        args: &serde_json::Value,
    ) -> IpcResponse {
        // This remains a dispatch command so it can be bound to a key, but IPC
        // callers need the asynchronous submission contract and destination
        // rather than a bare success bit that could be mistaken for a saved PNG.
        if name == "take_screenshot_fullscreen" {
            return fullscreen_screenshot_submission_response(
                self.submit_screenshot_fullscreen(backend)
                    .map_err(|error| error.to_string()),
            );
        }

        // Special command: reload_config
        if name == "reload_config" {
            return self.do_config_reload(backend);
        }

        // Special command: notify — post a native toast card.
        if name == "notify" {
            let title = args
                .get("title")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            let body = args
                .get("body")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            if title.is_empty() && body.is_empty() {
                return IpcResponse::err("notify: expected string field 'title' and/or 'body'");
            }
            let urgency = args
                .get("urgency")
                .and_then(|value| value.as_u64())
                .unwrap_or(1)
                .min(2) as u8;
            let timeout_ms = args
                .get("timeout_ms")
                .and_then(|value| value.as_u64())
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or(0);
            // `app`, `replaces_id`, and `default_action` are what the
            // freedesktop bridge forwards; scripts may omit all three.
            let request = crate::jwm::features::NotificationRequest {
                app: args
                    .get("app")
                    .and_then(|value| value.as_str())
                    .unwrap_or("")
                    .to_string(),
                summary: title.to_string(),
                body: body.to_string(),
                urgency,
                replaces_id: args
                    .get("replaces_id")
                    .and_then(|value| value.as_u64())
                    .and_then(|value| u32::try_from(value).ok())
                    .unwrap_or(0),
                actions: crate::jwm::features::notifications::parse_action_args(args),
            };
            let id = self.post_notification(backend, &request, timeout_ms);
            return IpcResponse::ok(Some(serde_json::json!({ "id": id })));
        }

        // Special command: close_notification — drop one history record and
        // tell subscribers, so the bridge can emit NotificationClosed.
        if name == "close_notification" {
            let Some(id) = args
                .get("id")
                .and_then(|value| value.as_u64())
                .and_then(|value| u32::try_from(value).ok())
            else {
                return IpcResponse::err("close_notification: expected integer field 'id'");
            };
            let reason = match args.get("reason").and_then(|value| value.as_u64()) {
                Some(1) => crate::jwm::features::notifications::CloseReason::Expired,
                Some(2) => crate::jwm::features::notifications::CloseReason::Dismissed,
                Some(4) => crate::jwm::features::notifications::CloseReason::Undefined,
                _ => crate::jwm::features::notifications::CloseReason::Requested,
            };
            let closed = self.close_notification(id, reason);
            return IpcResponse::ok(Some(serde_json::json!({ "closed": closed })));
        }

        // Special command: clear_notifications — empty the history.
        if name == "clear_notifications" {
            let cleared = self.clear_notifications();
            return IpcResponse::ok(Some(serde_json::json!({ "cleared": cleared })));
        }

        // Special command: set_media_status — the bridge's MPRIS push. An
        // absent/null player clears the state.
        if name == "set_media_status" {
            let state = crate::jwm::features::media::parse_state_args(args);
            let active = state.is_some();
            self.set_media_status(backend, state);
            return IpcResponse::ok(Some(serde_json::json!({ "active": active })));
        }

        // Special command: bluetooth_pairing_prompt — the pairing helper
        // (`jwm-bridge pair`) relays a bluez agent callback. This only updates
        // session state and the picker's prompt; the answer travels back over
        // the `bluetooth/pairing_response` broadcast. The passkey/code the
        // command may carry is rendered but never logged.
        if name == "bluetooth_pairing_prompt" {
            use crate::jwm::features::pairing;

            let command = match pairing::parse_prompt_command(args) {
                Ok(command) => command,
                Err(error) => {
                    return IpcResponse::err(format!("bluetooth_pairing_prompt: {error}"));
                }
            };
            let Some(session) = &mut self.features.bluetooth_pairing else {
                return IpcResponse::err(
                    "bluetooth_pairing_prompt: no pairing session is active".to_string(),
                );
            };
            if !session.matches(&command.address, &command.cookie) {
                return IpcResponse::err(
                    "bluetooth_pairing_prompt: not the active pairing session".to_string(),
                );
            }
            // An armed inbound window learns its device here and only here:
            // the first callback binds it, and `matches` above enforces it
            // from then on, so the foreign-device rule is late-bound rather
            // than dropped. An outbound session was already bound at spawn
            // and this is a no-op for it.
            if !session.pin_address(&command.address, &command.device_name) {
                return IpcResponse::err(
                    "bluetooth_pairing_prompt: not the device this session is bound to".to_string(),
                );
            }
            // Only a request the user armed may draw a modal prompt. An
            // authorization arriving on an outbound pairing session is a
            // device answering something nobody asked, and is refused rather
            // than rendered.
            if matches!(command.prompt, pairing::PairingPrompt::Authorize { .. })
                && session.kind() != pairing::PairingKind::Inbound
            {
                return IpcResponse::err(
                    "bluetooth_pairing_prompt: authorization is only offered while an inbound window is armed"
                        .to_string(),
                );
            }
            session.apply_prompt(
                command.prompt.clone(),
                command.request_id,
                std::time::Instant::now(),
            );
            self.features
                .system_ui
                .prompt_bluetooth_pairing(&command.prompt, session.device_name());
            self.sync_system_ui(backend);
            return IpcResponse::ok(None);
        }

        // Special command: bluetooth_pairing_done — the helper's terminal
        // report. A successful pairing re-reads the device list so the row
        // shows the new bonded state; a failure stays on the status line so
        // the user can retry.
        if name == "bluetooth_pairing_done" {
            use crate::jwm::features::pairing;

            let done = match pairing::parse_done_command(args) {
                Ok(done) => done,
                Err(error) => return IpcResponse::err(format!("bluetooth_pairing_done: {error}")),
            };
            let Some(session) = &self.features.bluetooth_pairing else {
                return IpcResponse::err(
                    "bluetooth_pairing_done: no pairing session is active".to_string(),
                );
            };
            if !session.matches(&done.address, &done.cookie) {
                return IpcResponse::err(
                    "bluetooth_pairing_done: not the active pairing session".to_string(),
                );
            }
            let device_name = session.device_name().to_string();
            let inbound = session.kind() == pairing::PairingKind::Inbound;
            self.features.bluetooth_pairing = None;
            // A prompt left on screen (bluez cancelled its request before the
            // user answered) is withdrawn without touching the list.
            self.features.system_ui.cancel_pairing_prompt();
            if self.features.system_ui.is_bluetooth_picker() {
                if done.ok {
                    log::info!("Bluetooth: {device_name} allowed/paired");
                    // An inbound window's own answer already went on the
                    // status line ("Allowed"/"Refused"); the window closing
                    // afterwards must not overwrite it with pairing words
                    // that describe a different gesture.
                    if !inbound {
                        self.features.system_ui.set_bluetooth_message(
                            crate::jwm::features::pairing::paired_message(
                                &device_name,
                                done.connected,
                            ),
                        );
                    }
                    // Re-read so the row shows the bond that actually took —
                    // unless a scan is already running. Replacing its handle
                    // only drops the notifier: the worker (and a real
                    // `Adapter1.StartDiscovery` session behind it) runs on
                    // with nowhere to land, and what it heard is thrown away.
                    // The running read lands with the bond state bluez holds
                    // by then, which is what the `s`/`r` keys rely on too.
                    //
                    // `job_in_flight`, not "the slot holds a handle": a job
                    // whose thread the OS refused never publishes anything,
                    // and coalescing on its mere presence would hold this
                    // re-read shut for as long as that handle sits there.
                    let scanning = crate::jwm::features::connectivity::job_in_flight(
                        self.features.bluetooth_scan.as_ref(),
                    );
                    if !scanning
                        && let Some(scan) = crate::jwm::features::connectivity::start_device_scan()
                    {
                        self.features.bluetooth_scan = Some(self.track_background_job(scan));
                    }
                    self.refresh_connectivity();
                } else if inbound {
                    log::info!("Bluetooth: request from {device_name} refused");
                } else {
                    let reason = done.error.unwrap_or_else(|| "pairing failed".to_string());
                    log::warn!("Bluetooth: pairing with {device_name} failed: {reason}");
                    self.features.system_ui.set_bluetooth_message(reason);
                }
            }
            self.sync_system_ui(backend);
            return IpcResponse::ok(None);
        }

        // Special command: bluetooth_pairing_withdraw — bluez cancelled the
        // request behind the prompt on screen (the remote gave up, or bluez
        // withdrew it), so the prompt is no longer answerable and comes down
        // now rather than at the prompt timeout. Only the prompt: the session
        // stays alive — an outbound pairing still owes a `done` from the Pair
        // unwind, an inbound window keeps its own clock — so this touches
        // neither the session record nor the picker's status line.
        if name == "bluetooth_pairing_withdraw" {
            use crate::jwm::features::pairing;

            let withdraw = match pairing::parse_withdraw_command(args) {
                Ok(withdraw) => withdraw,
                Err(error) => {
                    return IpcResponse::err(format!("bluetooth_pairing_withdraw: {error}"));
                }
            };
            let Some(session) = &mut self.features.bluetooth_pairing else {
                return IpcResponse::err(
                    "bluetooth_pairing_withdraw: no pairing session is active".to_string(),
                );
            };
            if !session.matches(&withdraw.address, &withdraw.cookie) {
                return IpcResponse::err(
                    "bluetooth_pairing_withdraw: not the active pairing session".to_string(),
                );
            }
            // Nothing showing — or a withdraw for a prompt a newer one
            // already replaced: the end state it asks for already holds, so
            // this is a no-op rather than an error. A late user answer is
            // refused the same way: the answer paths match on a prompting
            // phase, and a withdrawn prompt leaves the session Working.
            if session.withdraw_prompt(withdraw.request_id) {
                self.features.system_ui.cancel_pairing_prompt();
                self.sync_system_ui(backend);
            }
            return IpcResponse::ok(None);
        }

        // Special command: bluetooth_pairing_failed — the helper's terminal
        // report when the window it was spawned to hold never armed: no
        // system bus, no adapter, or an agent bluez refused. The frame names
        // no address because nothing ever rang, so the cookie alone decides,
        // and only an inbound window nothing has called into yet may end this
        // way (`matches_failure`). The session closes now rather than at the
        // sixty-second inbound deadline, and the status line says what
        // actually happened. No `bluetooth/pairing_response` goes back — the
        // helper that sent this holds nothing outstanding to answer, that is
        // what the frame means — and nothing was ever bound, so there is no
        // device list to re-read either.
        if name == "bluetooth_pairing_failed" {
            use crate::jwm::features::pairing;

            let failed = match pairing::parse_failed_command(args) {
                Ok(failed) => failed,
                Err(error) => {
                    return IpcResponse::err(format!("bluetooth_pairing_failed: {error}"));
                }
            };
            let Some(session) = &self.features.bluetooth_pairing else {
                return IpcResponse::err(
                    "bluetooth_pairing_failed: no pairing session is active".to_string(),
                );
            };
            if !session.matches_failure(&failed.cookie) {
                return IpcResponse::err(
                    "bluetooth_pairing_failed: not the active pairing session".to_string(),
                );
            }
            self.features.bluetooth_pairing = None;
            let reason = failed.error.as_deref().unwrap_or("the helper gave up");
            log::warn!("Bluetooth: inbound window never armed: {reason}");
            // An unrung window can have no prompt on screen — prompting binds
            // the address first — but take one down defensively, as `done`
            // does for a prompt bluez cancelled behind itself.
            self.features.system_ui.cancel_pairing_prompt();
            if self.features.system_ui.is_bluetooth_picker() {
                self.features
                    .system_ui
                    .set_bluetooth_message(pairing::inbound_failed_message(
                        failed.error.as_deref(),
                    ));
            }
            self.sync_system_ui(backend);
            return IpcResponse::ok(None);
        }

        // Special command: clipboard_record — how a backend helper or a
        // script feeds the history. Offers marked secret must be dropped
        // before calling this, not here.
        if name == "clipboard_record" {
            let Some(text) = args.get("text").and_then(|value| value.as_str()) else {
                return IpcResponse::err("clipboard_record: expected string field 'text'");
            };
            let recorded = self.record_clipboard(text);
            return IpcResponse::ok(Some(serde_json::json!({ "recorded": recorded })));
        }

        // Special command: clipboard_copy — put a history entry back on the
        // clipboard by index, the same thing the picker's Enter does.
        if name == "clipboard_copy" {
            // Only an absent index means the newest entry. A string, negative
            // or fractional index is a caller mistake that would otherwise put
            // the newest entry back on the clipboard and report success.
            let index = match args.get("index") {
                None => 0,
                Some(value) => match value.as_u64().and_then(|index| usize::try_from(index).ok()) {
                    Some(index) => index,
                    None => {
                        return IpcResponse::err(
                            "clipboard_copy: 'index' must be a non-negative integer",
                        );
                    }
                },
            };
            let Some(entry) = self.features.clipboard.get(index).cloned() else {
                return IpcResponse::err(format!("clipboard_copy: no entry at index {index}"));
            };
            let offered = match &entry {
                crate::jwm::features::ClipboardEntry::Text { text, .. } => {
                    backend.set_clipboard_text(text)
                }
                crate::jwm::features::ClipboardEntry::Png { bytes, .. } => {
                    self.offer_clipboard_png(backend, bytes)
                }
            };
            if !offered {
                return IpcResponse::err("this backend cannot set the clipboard");
            }
            match entry {
                crate::jwm::features::ClipboardEntry::Text { text, .. } => {
                    self.record_clipboard(&text);
                }
                crate::jwm::features::ClipboardEntry::Png { bytes, .. } => {
                    self.record_clipboard_png(&bytes);
                }
            }
            return IpcResponse::ok(None);
        }

        // Special command: clear_clipboard — forget everything copied.
        if name == "clear_clipboard" {
            let cleared = self.clear_clipboard_history();
            return IpcResponse::ok(Some(serde_json::json!({ "cleared": cleared })));
        }

        // Special command: set_audio_device — switch the default sink/source.
        //
        // Reply semantics: queued, like `set_mic_mute` and the volume keys.
        // The reply acknowledges the submission; the controls worker runs the
        // set plus verifying re-read off the event thread, and
        // `adopt_audio_switch` then moves the marker, publishes
        // `audio/devices`, and raises the named OSD only when the re-read
        // says the switch took — the same path the picker uses.
        if name == "set_audio_device" {
            use crate::jwm::features::system_controls::{self, AudioDirection};

            let direction = match args.get("direction").and_then(|value| value.as_str()) {
                Some("output") | None => AudioDirection::Output,
                Some("input") => AudioDirection::Input,
                Some(other) => {
                    return IpcResponse::err(format!(
                        "set_audio_device: unknown direction {other:?} (want 'output' or 'input')"
                    ));
                }
            };
            let Some(id) = args.get("id").and_then(|value| value.as_str()) else {
                return IpcResponse::err("set_audio_device: expected string field 'id'");
            };
            // Prefer the cached inventory bars already poll; fall back to one
            // sync peek only when nothing has been read yet so a bad id is
            // rejected before a worker round-trip.
            let inventory = self
                .features
                .control_snapshot
                .as_ref()
                .map(|snapshot| snapshot.audio_inventory.clone())
                .unwrap_or_else(system_controls::audio_inventory);
            let devices = inventory.devices(direction);
            if !devices.iter().any(|device| device.id == id) {
                return IpcResponse::err(format!(
                    "set_audio_device: no {} device with id {id:?}",
                    direction.label()
                ));
            }
            if system_controls::queue_control_request(
                system_controls::ControlRequest::AudioSetDefault {
                    direction,
                    id: id.to_string(),
                },
                self.async_update_notifier.clone(),
            )
            .is_none()
            {
                return IpcResponse::err("no working audio control (wpctl/pactl/amixer)");
            }
            return IpcResponse::ok(None);
        }

        // Special command: set_mic_mute — set the default microphone's mute
        // flag.
        //
        // Reply semantics: queued, like the volume keys and
        // `set_audio_device`. The reply acknowledges the submission and the
        // optimistic card is drawn at once; the controls worker's read-back
        // then confirms or corrects it (the OSD refreshes in place, the
        // control-center Input row follows the adopted value). Caching the
        // estimate (and later adopt/revert) also publishes `audio/mic` on
        // the `audio` topic; `get_mic_mute` answers the same cached flag.
        if name == "set_mic_mute" {
            let Some(muted) = args.get("muted").and_then(|value| value.as_bool()) else {
                return IpcResponse::err("set_mic_mute: expected boolean field 'muted'");
            };
            let Some((seq, estimate)) = self.queue_mic_request(
                crate::jwm::features::system_controls::ControlRequest::MicMuteSet(muted),
            ) else {
                // The key path's own answer when no audio tool works.
                return IpcResponse::err("no working audio control (wpctl/pactl/amixer)");
            };
            match estimate {
                // A set knows its target with no confirmed base, so this is
                // always the taken arm today; the `None` shape mirrors the
                // key path — the covering read-back draws the first card.
                Some(muted) => self.show_mic_osd(backend, muted),
                None => self.features.control_feedback.owe_osd(
                    crate::jwm::features::system_controls::ControlDomain::MicMute,
                    seq,
                ),
            }
            return IpcResponse::ok(None);
        }

        // Special command: set_power_profile — switch the platform profile.
        //
        // Reply semantics: queued, like `set_mic_mute` and `set_audio_device`.
        // The name is validated against the cached list only — reading the
        // driver's list, the set and its verifying re-read are each a
        // `powerprofilesctl` run, and those run on the controls worker, never
        // on the event thread this reply is built on. The ack comes with the
        // optimistic row and card; the worker's re-read then publishes
        // `power/profile` with the profile really in effect and corrects a
        // live card whose switch did not take.
        if name == "set_power_profile" {
            let Some(profile) = args.get("profile").and_then(|value| value.as_str()) else {
                return IpcResponse::err("set_power_profile: expected string field 'profile'");
            };
            let now = std::time::Instant::now();
            let cached_is_fresh =
                !crate::jwm::features::system_controls::control_center_snapshot_is_stale(
                    self.features.control_snapshot_refreshed_at,
                    now,
                );
            let cached = self
                .features
                .control_snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.power_profiles.clone());
            let cache_answers = cached_power_profiles_answer(
                cached.as_ref().map(|(available, _)| available.as_slice()),
                cached_is_fresh,
                profile,
            );
            if !cache_answers {
                // What a synchronous read used to answer here, a worker read
                // answers for the retry: at most one runs, coalesced with the
                // control center's own.
                self.ensure_control_snapshot_refresh(now);
            }
            let Some((available, _)) = cached else {
                if self.features.control_snapshot_refreshed_at.is_some() {
                    return IpcResponse::err("this machine has no power profile control");
                }
                return IpcResponse::err(
                    "set_power_profile: the power profiles have not been read yet; \
                     they are being read now, try again shortly",
                );
            };
            if !available.iter().any(|name| name == profile) {
                return IpcResponse::err(format!(
                    "unknown power profile {profile:?} (have {})",
                    available.join(", ")
                ));
            }
            if self
                .queue_power_profile_request(available, profile.to_string())
                .is_none()
            {
                return IpcResponse::err(format!(
                    "could not switch to power profile {profile:?}: the controls worker is not running"
                ));
            }
            self.refresh_open_control_center();
            // Same labeled card the Hub Left/Right path raises, so a bar or
            // script that flips the profile gets the same acknowledgement.
            self.show_power_profile_osd(backend, profile.to_string());
            return IpcResponse::ok(None);
        }

        // Special command: media_control — drive the active player.
        if name == "media_control" {
            let Some(action) = args.get("action").and_then(|value| value.as_str()) else {
                return IpcResponse::err("media_control: expected string field 'action'");
            };
            let command = if action == "seek" {
                let Some(position_us) = args.get("position_us").and_then(|value| value.as_i64())
                else {
                    return IpcResponse::err(
                        "media_control: seek expects integer field 'position_us'",
                    );
                };
                crate::jwm::features::MediaCommand::Seek(position_us.max(0))
            } else {
                let Some(command) = crate::jwm::features::MediaCommand::from_name(action) else {
                    return IpcResponse::err(format!(
                        "media_control: unknown action {action:?} \
                         (play_pause, next, previous, stop, seek)"
                    ));
                };
                command
            };
            return match self.send_media_command(command) {
                Ok(()) => IpcResponse::ok(None),
                Err(error) => IpcResponse::err(error),
            };
        }

        // Special command: benchmark (requires mutable backend)
        if name == "benchmark" {
            return self.handle_benchmark_command(backend, args);
        }

        if name == "set_config" {
            return self.handle_set_config_command(backend, args);
        }

        if name == "set_config_batch" {
            return self.handle_set_config_batch_command(backend, args);
        }

        if name == "command_batch" || name == "batch" {
            return self.handle_command_batch(backend, args);
        }

        if name == "move_window_to_monitor" {
            return self.handle_move_window_to_monitor(backend, args);
        }

        if name == "set_hdr_metadata" {
            return self.handle_set_hdr_metadata_command(backend, args);
        }

        if name == "start_recording" {
            let Some(path) = args.get("path").and_then(|value| value.as_str()) else {
                return IpcResponse::err("start_recording: expected string field 'path'");
            };
            let region_fields = (
                args.get("x").and_then(|value| value.as_i64()),
                args.get("y").and_then(|value| value.as_i64()),
                args.get("width").and_then(|value| value.as_u64()),
                args.get("height").and_then(|value| value.as_u64()),
            );
            let region = match region_fields {
                (None, None, None, None) => Rect::new(0, 0, self.s_w, self.s_h),
                (Some(x), Some(y), Some(width), Some(height)) => {
                    let parsed = i32::try_from(x)
                        .ok()
                        .zip(i32::try_from(y).ok())
                        .zip(i32::try_from(width).ok())
                        .zip(i32::try_from(height).ok())
                        .map(|(((x, y), width), height)| Rect::new(x, y, width, height));
                    let Some(region) = parsed else {
                        return IpcResponse::err(
                            "start_recording: region values are outside supported ranges",
                        );
                    };
                    region
                }
                _ => {
                    return IpcResponse::err(
                        "start_recording: x, y, width and height must be provided together",
                    );
                }
            };
            return match self.start_recording_region(backend, path, region) {
                Ok(()) => {
                    let region = self.features.recording.region;
                    self.broadcast_ipc_event(
                        "recording/started",
                        serde_json::json!({"output_path": path, "region": region.map(|r| serde_json::json!({"x": r.x, "y": r.y, "width": r.w, "height": r.h}))}),
                    );
                    IpcResponse::ok(Some(
                        serde_json::json!({"active": true, "output_path": path, "region": region.map(|r| serde_json::json!({"x": r.x, "y": r.y, "width": r.w, "height": r.h}))}),
                    ))
                }
                Err(error) => {
                    self.features.recording.note_error(error.to_string());
                    self.broadcast_ipc_event(
                        "recording/error",
                        serde_json::json!({"operation": "start", "error": error.to_string()}),
                    );
                    IpcResponse::err(error.to_string())
                }
            };
        }

        if name == "set_recording_region" {
            if !self.features.recording.active {
                return IpcResponse::err("set_recording_region: recording is not active");
            }
            let parsed = args
                .get("x")
                .and_then(|value| value.as_i64())
                .and_then(|value| i32::try_from(value).ok())
                .zip(
                    args.get("y")
                        .and_then(|value| value.as_i64())
                        .and_then(|value| i32::try_from(value).ok()),
                )
                .zip(
                    args.get("width")
                        .and_then(|value| value.as_u64())
                        .and_then(|value| i32::try_from(value).ok()),
                )
                .zip(
                    args.get("height")
                        .and_then(|value| value.as_u64())
                        .and_then(|value| i32::try_from(value).ok()),
                )
                .map(|(((x, y), width), height)| Rect::new(x, y, width, height));
            let Some(region) = parsed else {
                return IpcResponse::err(
                    "set_recording_region: expected integer x, y, width and height",
                );
            };
            let region = match self.normalize_initial_recording_region(region) {
                Ok(region) => region,
                Err(error) => return IpcResponse::err(error.to_string()),
            };
            self.features.recording.set_region(region);
            if let Some(region_tuple) = Self::recording_region_tuple(region) {
                backend.compositor_set_recording_region(region_tuple);
                backend.compositor_force_full_redraw();
            }
            self.broadcast_ipc_event(
                "recording/region_changed",
                serde_json::json!({"x": region.x, "y": region.y, "width": region.w, "height": region.h}),
            );
            return IpcResponse::ok(Some(serde_json::json!({
                "x": region.x,
                "y": region.y,
                "width": region.w,
                "height": region.h,
            })));
        }

        if name == "stop_recording" {
            let was_active = self.features.recording.active;
            let output_path = self.features.recording.output_path.clone();
            return match self.stop_recording(backend) {
                Ok(()) => {
                    if was_active {
                        self.broadcast_ipc_event(
                            "recording/stopped",
                            serde_json::json!({"output_path": output_path}),
                        );
                    }
                    IpcResponse::ok(Some(
                        serde_json::json!({"active": false, "output_path": output_path}),
                    ))
                }
                Err(error) => IpcResponse::err(error.to_string()),
            };
        }

        if name == "start_audio_recording" {
            let Some(path) = args.get("path").and_then(|value| value.as_str()) else {
                return IpcResponse::err(
                    "start_audio_recording: expected absolute .wav/.flac/.opus/.mp3 path in string field 'path'",
                );
            };
            return match self.start_audio_recording(backend, std::path::Path::new(path)) {
                Ok(()) => IpcResponse::ok(Some(serde_json::json!({
                    "active": true,
                    "output_path": path,
                }))),
                Err(error) => {
                    self.broadcast_ipc_event(
                        "audio_recording/error",
                        serde_json::json!({"operation": "start", "error": error.to_string()}),
                    );
                    IpcResponse::err(error.to_string())
                }
            };
        }

        // Reply semantics: confirmed. Unlike the key toggle, this stop waits
        // for the recorder to finalize its file (the direct recorder's header
        // rewrite, or ffmpeg's bounded stop grace), so a successful reply
        // means the file is complete. A recording the key already stopped
        // and that is still finalizing counts as active here: this call is
        // the one that collects it.
        if name == "stop_audio_recording" {
            let was_active = self.features.audio_recording.active
                || self.features.audio_recording.is_finalizing();
            let output_path = self.features.audio_recording.output_path.clone();
            return match self.stop_audio_recording(backend) {
                Ok(()) => IpcResponse::ok(Some(serde_json::json!({
                    "active": false,
                    "was_active": was_active,
                    "output_path": output_path,
                }))),
                Err(error) => IpcResponse::err(error.to_string()),
            };
        }

        match ipc::dispatch_command(name, args) {
            Ok((func, arg)) => match func(self, backend, &arg) {
                Ok(()) => IpcResponse::ok(None),
                Err(e) => IpcResponse::err(format!("{e}")),
            },
            Err(e) => IpcResponse::err(e),
        }
    }

    pub(crate) fn handle_ipc_query(
        &mut self,
        name: &str,
        args: &serde_json::Value,
        backend: &dyn Backend,
    ) -> IpcResponse {
        let cfg = CONFIG.load();
        match name {
            "get_status" | "get_st" => IpcResponse::ok(Some(
                serde_json::to_value(self.query_runtime_status(backend)).unwrap_or_default(),
            )),
            "get_capabilities" | "get_caps" => IpcResponse::ok(Some(
                serde_json::to_value(ipc::ipc_capabilities()).unwrap_or_default(),
            )),
            "get_windows" | "get_clients" | "get_wins" | "get_cli" => {
                let windows = self.query_windows(backend);
                IpcResponse::ok(Some(serde_json::to_value(windows).unwrap_or_default()))
            }
            "get_window" | "get_win" => self.query_window(backend, args),
            "get_workspaces" | "get_tags" | "get_desktops" | "get_ws" => {
                let workspaces = self.query_workspaces(backend);
                IpcResponse::ok(Some(serde_json::to_value(workspaces).unwrap_or_default()))
            }
            "get_monitors" | "get_outputs" | "get_mons" => {
                let monitors = self.query_monitors(backend);
                IpcResponse::ok(Some(serde_json::to_value(monitors).unwrap_or_default()))
            }
            "get_tree" | "get_tr" => {
                let tree = self.query_tree(backend);
                IpcResponse::ok(Some(serde_json::to_value(tree).unwrap_or_default()))
            }
            "get_scrolling_status" | "get_scrolling" => {
                IpcResponse::ok(Some(self.query_scrolling_status()))
            }
            "get_layout" | "get_lt" => IpcResponse::ok(Some(self.query_focused_layout(backend))),
            "get_gaps" | "get_gap" => IpcResponse::ok(Some(self.query_focused_gaps(backend))),
            "get_nmaster" | "get_nm" => IpcResponse::ok(Some(self.query_focused_nmaster(backend))),
            "get_mfact" | "get_mf" => IpcResponse::ok(Some(self.query_focused_mfact(backend))),
            "get_cfact" | "get_cf" => IpcResponse::ok(Some(self.query_focused_cfact(backend))),
            "get_show_bar" | "get_bar" | "get_bar_visible" | "get_owns_output"
            | "get_visible_fullscreen" | "get_vf" => {
                IpcResponse::ok(Some(self.query_focused_show_bar(backend)))
            }
            "get_prev_layout" | "get_pl" => {
                IpcResponse::ok(Some(self.query_focused_prev_layout(backend)))
            }
            "get_selected" | "get_sel" => {
                IpcResponse::ok(Some(self.query_selected_window(backend, false)))
            }
            "get_focused_window" | "get_fw" => {
                IpcResponse::ok(Some(self.query_selected_window(backend, true)))
            }
            "get_scratchpads" | "get_pads" | "get_scratch" => {
                IpcResponse::ok(Some(self.query_scratchpads()))
            }
            "get_struts" | "get_strut" => IpcResponse::ok(Some(self.query_struts(backend))),
            "get_night_light" | "get_night_light_status" | "get_nl" => {
                IpcResponse::ok(Some(self.query_night_light()))
            }
            "get_gesture_status" | "get_gesture" | "get_gest" => {
                IpcResponse::ok(Some(self.query_gesture_status()))
            }
            "get_wayland_status" | "get_wayland" | "get_wl" => {
                IpcResponse::ok(Some(self.query_wayland_status(backend)))
            }
            "get_config_status" | "get_cfg" => IpcResponse::ok(Some(self.query_config_status())),
            "get_config" | "get_conf" => IpcResponse::ok(Some(self.query_config_subset(args))),
            "get_dnd" | "get_do_not_disturb" => IpcResponse::ok(Some(serde_json::json!({
                "enabled": self.do_not_disturb,
            }))),
            "get_notifications" | "get_notif" => IpcResponse::ok(Some(self.notifications_json())),
            "get_system_ui" | "get_ui" => IpcResponse::ok(Some(serde_json::json!({
                "active": self.features.system_ui.is_active(),
                "kind": self.features.system_ui.panel_kind(),
            }))),
            "get_tab_bar" | "get_tabs" | "get_tab" => {
                IpcResponse::ok(Some(self.query_focused_tab_bar(backend)))
            }
            "get_media_status" | "get_media" => IpcResponse::ok(Some(self.media_status_json())),
            "get_power_status" | "get_power" => {
                // Warm the Shell Hub's coalesced snapshot before answering,
                // the `get_connectivity` shape. The contract for the read
                // below is to answer from `features.control_snapshot`
                // (stale-while-revalidate: this call makes the next poll
                // fresh) and to fork nothing — a bar polling every few
                // seconds must never stall a frame on powerprofilesctl.
                self.ensure_control_snapshot_refresh(std::time::Instant::now());
                IpcResponse::ok(Some(self.power_status_json()))
            }
            "get_connectivity" | "get_network" | "get_conn" => {
                IpcResponse::ok(Some(self.connectivity_json()))
            }
            "get_bluetooth_pairing" | "get_bluetooth" | "get_bt" | "get_pair" => {
                IpcResponse::ok(Some(crate::jwm::features::pairing::session_json(
                    self.features.bluetooth_pairing.as_ref(),
                )))
            }
            "get_audio_devices" | "get_audio" | "get_devices" => {
                // Same contract as `get_power_status`, now that the snapshot
                // carries the whole `AudioInventory` and not just the two
                // devices in use: warm the coalesced read so the next poll is
                // current, then answer from memory. The reply's `pending`
                // flag is what tells an empty inventory ("this session has
                // nothing to switch") from an unread one.
                self.ensure_control_snapshot_refresh(std::time::Instant::now());
                IpcResponse::ok(Some(self.audio_devices_json()))
            }
            "get_mic_mute" | "get_mic" | "get_mute" => {
                // Same stale-while-revalidate contract as `get_audio_devices`:
                // warm the coalesced snapshot, then answer from memory. A
                // null `muted` means the flag was never read (no tool / not
                // yet sampled) — never invent unmuted.
                self.ensure_control_snapshot_refresh(std::time::Instant::now());
                IpcResponse::ok(Some(serde_json::json!({
                    "muted": self
                        .features
                        .control_snapshot
                        .as_ref()
                        .and_then(|snapshot| snapshot.mic_muted),
                })))
            }
            "get_wallpaper_colors" | "get_wallpaper" | "get_wall" | "get_wc" => {
                IpcResponse::ok(Some(self.wallpaper_theme_json()))
            }
            "get_idle_status" | "get_idle" | "get_idl" => {
                IpcResponse::ok(Some(self.idle_status_json(backend)))
            }
            "get_resources" | "get_res" => IpcResponse::ok(Some(self.resources_json())),
            "get_clipboard" | "get_clip" => IpcResponse::ok(Some(self.clipboard_json())),
            "get_recording_status" | "get_recording" | "get_rec" => {
                let output_path = self.features.recording.output_path.clone();
                let active = self.features.recording.active;
                // Validation still runs synchronously (with a hard helper
                // deadline), and `stop` deliberately keeps `output_path` so the
                // bar can still report where the recording went. Probe only
                // while the answer can still change: never during recording,
                // never again once the file has passed, and not again for a
                // rejected file until it changes on disk.
                let finalized = !active
                    && (self.features.recording.finalized
                        || output_path.as_deref().is_some_and(|path| {
                            recording_file_is_valid(
                                path,
                                &mut self.features.recording.rejected_probe,
                            )
                        }));
                self.features.recording.finalized = finalized;
                let should_broadcast = finalized && !self.features.recording.finalization_reported;
                if should_broadcast {
                    self.features.recording.finalization_reported = true;
                    self.broadcast_ipc_event(
                        "recording/finalized",
                        serde_json::json!({"output_path": output_path.clone()}),
                    );
                }
                let capture_stats = backend.compositor_recording_stats();
                let elapsed_secs = capture_stats
                    .as_ref()
                    .map(|stats| (stats.elapsed_secs * 10.0).round() / 10.0);
                IpcResponse::ok(Some(serde_json::json!({
                    "active": active,
                    "finalized": finalized,
                    "output_path": output_path,
                    "segment_path": self.features.recording.current_segment.clone(),
                    "fps": cfg.behavior().recording_fps,
                    "encoder": cfg.behavior().recording_encoder,
                    "audio_enabled": cfg.behavior().recording_audio_enabled,
                    "audio_device": cfg.behavior().recording_audio_device,
                    "audio_bitrate": cfg.behavior().recording_audio_bitrate,
                    "region": self.features.recording.region.map(|region| serde_json::json!({
                        "x": region.x,
                        "y": region.y,
                        "width": region.w,
                        "height": region.h,
                    })),
                    // The compositor is authoritative: a height cap makes the
                    // encoded size differ from the region the WM recorded.
                    "output_size": capture_stats
                        .as_ref()
                        .map(|stats| stats.output_size)
                        .or(self.features.recording.output_size)
                        .map(|(width, height)| serde_json::json!({
                            "width": width,
                            "height": height,
                        })),
                    "selecting_region": self.features.recording.selecting_region,
                    "adjusting_region": self.features.recording.adjusting_region,
                    "pending_output_path": self.features.recording.pending_output_path.clone(),
                    "segments": self.features.recording.segments.clone(),
                    "segment_count": self.features.recording.segment_count(),
                    // Top-level twins of nested capture timing / interactive
                    // target / last failure so a bar need not dig into
                    // `capture` or keep separate error topic state.
                    "elapsed_secs": elapsed_secs,
                    "capture_target": self.features.capture.recording.label(),
                    "last_error": self.features.recording.last_error,
                    // What the recording is actually achieving. A recorder that
                    // silently runs at a third of the requested rate, or that is
                    // discarding frames because the encoder cannot keep up,
                    // looks healthy until the file is played back; these are the
                    // numbers that say otherwise while it is still running.
                    // `captured_fps` well under `fps` is normal on a static
                    // screen, where unchanged frames are deliberately skipped.
                    "capture": capture_stats.map(|stats| serde_json::json!({
                        "captured_frames": stats.captured,
                        "dropped_frames": stats.dropped,
                        "captured_fps": (stats.effective_fps() * 10.0).round() / 10.0,
                        "elapsed_secs": (stats.elapsed_secs * 10.0).round() / 10.0,
                    })),
                })))
            }
            "get_audio_recording_status" | "get_audio_recording" | "get_arec" => {
                self.features.audio_recording.refresh();
                let recording = &self.features.audio_recording;
                let (output_exists, output_bytes) = recording
                    .output_path
                    .as_deref()
                    .and_then(|path| std::fs::metadata(path).ok())
                    .map(|metadata| {
                        let len = metadata.len();
                        (len > 0, Some(len))
                    })
                    .unwrap_or((false, None));
                IpcResponse::ok(Some(serde_json::json!({
                    "active": recording.active,
                    // A key-stopped recording still writing its file: the
                    // microphone is released, the output is not complete yet.
                    "finalizing": recording.is_finalizing(),
                    "output_path": recording.output_path,
                    "output_exists": output_exists,
                    "output_bytes": output_bytes,
                    "elapsed_ms": u64::try_from(recording.elapsed().as_millis()).unwrap_or(u64::MAX),
                    "device": recording.device,
                    "backend": recording.backend,
                    "format": recording.format,
                    "sample_rate": recording.sample_rate,
                    "channels": recording.channels,
                    "last_error": recording.last_error,
                })))
            }
            "get_effect_status" | "get_effects" | "get_fx" => {
                IpcResponse::ok(Some(serde_json::json!({
                    "overview": self.features.overview.active,
                    "expose": self.features.expose_active,
                    "audio_recording": self.features.audio_recording.active,
                    "recording": self.features.recording.active,
                    "selecting_recording": self.features.recording.selecting_region,
                    "selecting_screenshot": self.features.screenshot.active
                        && !self.features.screenshot.committed,
                    "magnifier": self.features.magnifier.enabled,
                    "magnifier_zoom": self.features.magnifier.zoom_level,
                    "magnifier_radius": self.features.magnifier.radius,
                    "annotation": self.features.annotation_active,
                    "peek": self.features.peek_active,
                    "compositor_active": backend.has_compositor(),
                    "layout_picker": self.features.system_ui.is_layout_picker(),
                    "tags_overview": self.features.system_ui.is_tags_overview(),
                    "calendar": self.features.system_ui.is_calendar(),
                    "keybindings": self.features.system_ui.is_keybindings(),
                    "monitor_layout": self.features.system_ui.is_monitor_layout(),
                    "launcher": self.features.system_ui.is_launcher(),
                    "session_menu": self.features.system_ui.is_session_menu(),
                    "notifications": self.features.system_ui.is_notification_center(),
                    "control_center": self.features.system_ui.is_control_center(),
                    "clipboard_picker": self.features.system_ui.is_clipboard_picker(),
                    "wifi_picker": self.features.system_ui.is_wifi_picker(),
                    "bluetooth_picker": self.features.system_ui.is_bluetooth_picker(),
                    "wallpaper_picker": self.features.system_ui.is_wallpaper_picker(),
                    "theme_picker": self.features.system_ui.is_theme_picker(),
                    "audio_output_picker": self
                        .features
                        .system_ui
                        .audio_picker_direction()
                        == Some(crate::jwm::features::system_controls::AudioDirection::Output),
                    "audio_input_picker": self
                        .features
                        .system_ui
                        .audio_picker_direction()
                        == Some(crate::jwm::features::system_controls::AudioDirection::Input),
                    "media_players": self.features.system_ui.is_media_players_picker(),
                    "window_switcher": self.features.system_ui.is_window_switcher(),
                    "session_lock": self.features.system_ui.is_session_lock(),
                    "monitor_lock": self
                        .features
                        .system_ui
                        .lock_scope()
                        .is_some_and(|scope| {
                            matches!(scope, crate::jwm::features::LockScope::Monitor(_))
                        }),
                    "debug_hud": self.debug_hud_on,
                    "corner_radius": cfg.behavior().corner_radius,
                    "shadow_enabled": cfg.behavior().shadow_enabled,
                    "blur_enabled": cfg.behavior().blur_enabled,
                    "fading": cfg.behavior().fading,
                    "wobbly_windows": cfg.behavior().wobbly_windows,
                    "motion_trail": cfg.behavior().motion_trail,
                })))
            }
            "get_magnifier" | "get_mag" => IpcResponse::ok(Some(serde_json::json!({
                "enabled": self.features.magnifier.enabled,
                "zoom": self.features.magnifier.zoom_level,
                "radius": self.features.magnifier.radius,
            }))),
            "get_peek" | "get_pk" => IpcResponse::ok(Some(serde_json::json!({
                "active": self.features.peek_active,
                "compositor_active": backend.has_compositor(),
            }))),
            "get_hdr_status" | "get_hdr" => {
                let outputs: Vec<serde_json::Value> = backend
                    .output_ops()
                    .enumerate_outputs()
                    .into_iter()
                    .map(|o| {
                        let metadata = o.hdr_metadata.as_ref().map(|m| {
                            serde_json::json!({
                                "max_luminance_nits": m.max_luminance_nits,
                                "min_luminance_nits": m.min_luminance_nits,
                                // 0.0 means the EDID stated none; the blob
                                // sends that on as "unknown".
                                "max_frame_average_nits": m.max_frame_average_nits,
                                "supports_pq": m.supports_pq,
                                "supports_hlg": m.supports_hlg,
                                "supports_bt2020": m.supports_bt2020,
                            })
                        });
                        serde_json::json!({
                            "name": o.name,
                            "hdr_capable": o.hdr_capable,
                            "edid_metadata": metadata,
                        })
                    })
                    .collect();
                let color_delivery = backend
                    .compositor_color_delivery_status()
                    .and_then(|status| serde_json::to_value(status).ok());
                // Why each output is (or is not) signalling right now. This
                // is the query `jwm-tool` shells out to, so the reasons live
                // here as well as in the session policy — the per-output
                // gate is the only thing that can answer "why not".
                let refusals = backend
                    .compositor_hdr_enable_refusals()
                    .into_iter()
                    .map(|(output, refusal)| {
                        serde_json::json!({ "output": output, "refusal": refusal })
                    })
                    .collect::<Vec<_>>();
                IpcResponse::ok(Some(serde_json::json!({
                    "config_enabled": cfg.behavior().hdr_enabled,
                    "config_peak_nits": cfg.behavior().hdr_peak_nits,
                    "outputs": outputs,
                    "enable_refusals": refusals,
                    "color_delivery": color_delivery,
                })))
            }
            "get_tearing_hints" | "get_tearing" | "get_th" => {
                // `active_surface_count` is client demand and always has
                // been; `outputs` is what the compositor did with it, one
                // row per output with a named reason when it did nothing.
                let presentation = backend.compositor_presentation_statuses();
                IpcResponse::ok(Some(serde_json::json!({
                    "active_surface_count": backend.compositor_tearing_hint_count(),
                    "tearing_outputs": presentation.iter().filter(|o| o.tearing).count(),
                    "outputs": presentation
                        .iter()
                        .map(|output| serde_json::json!({
                            "output": output.output_name,
                            "client_asked_to_tear": output.client_asked_to_tear,
                            "vrr": output.vrr,
                            "tearing": output.tearing,
                            "blocker": output.blocker,
                        }))
                        .collect::<Vec<_>>(),
                })))
            }
            "get_session_lock" | "get_lock" | "get_sess" => {
                IpcResponse::ok(Some(serde_json::json!({
                    "locked": backend.compositor_session_locked(),
                    "lock_surface_count": backend.compositor_session_lock_surface_count(),
                })))
            }
            "get_color_management_status" | "get_color_management" | "get_cm" => {
                let surfaces = backend.compositor_color_managed_surfaces();
                let detail: Vec<serde_json::Value> =
                    surfaces.iter().map(color_managed_surface_json).collect();
                let summary = color_surface_summary_json(&surfaces);
                let color_delivery = backend
                    .compositor_color_delivery_status()
                    .and_then(|status| serde_json::to_value(status).ok());
                IpcResponse::ok(Some(serde_json::json!({
                    "summary": summary,
                    "surface_count": surfaces.len(),
                    "hdr_surface_count": summary
                        .get("hdr_surface_count")
                        .and_then(|value| value.as_u64())
                        .unwrap_or(0),
                    "transfer_functions": summary.get("transfer_functions").cloned().unwrap_or_default(),
                    "primaries": summary.get("primaries").cloned().unwrap_or_default(),
                    "max_luminance_peak": summary.get("max_luminance_peak").cloned().unwrap_or(serde_json::Value::Null),
                    "surfaces": detail,
                    "color_delivery": color_delivery,
                })))
            }
            "get_xwayland_status" | "get_xwayland" | "get_xw" => {
                if let Some(status) = backend.compositor_xwayland_status() {
                    IpcResponse::ok(Some(serde_json::to_value(status).unwrap_or_default()))
                } else {
                    IpcResponse::ok(Some(serde_json::json!({
                        "available": false,
                        "wm_ready": false,
                        "display": std::env::var("DISPLAY").ok(),
                        "mapped_window_count": 0,
                        "associated_surface_count": 0,
                        "pending_association_count": 0,
                    })))
                }
            }
            "get_capture_status" | "get_capture" | "get_cap" => {
                if let Some(status) = backend.compositor_capture_status() {
                    IpcResponse::ok(Some(serde_json::to_value(status).unwrap_or_default()))
                } else {
                    IpcResponse::ok(Some(serde_json::json!({
                        "screencopy": { "enabled": false, "pending_frames": 0 },
                        "image_copy_capture": { "enabled": false, "pending_frames": 0 },
                        "image_copy_output_pending_frames": 0,
                        "image_copy_toplevel_pending_frames": 0,
                        "screencopy_queued_total": 0,
                        "screencopy_failed_total": 0,
                        "screencopy_fulfilled_total": 0,
                        "screencopy_render_failed_total": 0,
                        "image_copy_sessions_total": 0,
                        "image_copy_queued_total": 0,
                        "image_copy_failed_total": 0,
                        "image_copy_fulfilled_total": 0,
                        "image_copy_render_failed_total": 0,
                        "image_copy_output_queued_total": 0,
                        "image_copy_toplevel_queued_total": 0,
                        "last_queued_unix_ms": null,
                        "last_fulfilled_unix_ms": null,
                        "last_failed_unix_ms": null,
                        "last_failure_reason": null,
                        "dmabuf_advertised": false,
                        "dmabuf_format_count": 0,
                        "cursor_capture_supported": false,
                        "sensitive_content_masking": false,
                        "policy": "unavailable",
                    })))
                }
            }
            "get_blur_status" | "get_blur" => match backend.compositor_blur_status() {
                Some(b) => {
                    let hz_table: Vec<serde_json::Value> = b
                        .hz_table
                        .iter()
                        .map(|(hz, s)| serde_json::json!({ "hz": hz, "strength": s }))
                        .collect();
                    let per_monitor_hz: Vec<serde_json::Value> = b
                        .per_monitor_hz
                        .iter()
                        .map(|(id, hz)| serde_json::json!({ "monitor_id": id, "hz": hz }))
                        .collect();
                    let quality: Vec<serde_json::Value> = b
                        .blur_quality_by_monitor
                        .iter()
                        .map(|(id, q)| serde_json::json!({ "monitor_id": id, "quality": q }))
                        .collect();
                    IpcResponse::ok(Some(serde_json::json!({
                        "current_strength": b.current_strength,
                        "temporal_enabled": b.temporal_enabled,
                        "temporal_reuse_rate_pct": b.temporal_reuse_rate_pct,
                        "hz_table": hz_table,
                        "per_monitor_hz": per_monitor_hz,
                        "blur_quality_by_monitor": quality,
                        "status_bar_frosted": b.status_bar_frosted,
                        "glass_backdrop_valid": b.glass_backdrop_valid,
                    })))
                }
                None => IpcResponse::err("compositor not active".to_string()),
            },
            "get_waterlily_status" | "get_waterlily" | "get_wly" => {
                match backend.compositor_waterlily_status() {
                    Some(status) => IpcResponse::ok(Some(serde_json::json!({
                        "enabled": status.enabled,
                        "active": status.active,
                        "worker_connected": status.worker_connected,
                        "frame_width": status.frame_width,
                        "frame_height": status.frame_height,
                        "frame_depth": status.frame_depth,
                        "frame_sequence": status.frame_sequence,
                        "requested_case": status.requested_case,
                        "requested_palette": status.requested_palette,
                        // When the layer is on screen, mirror the last delivered
                        // request as `active_*` so a bar can show what is playing
                        // without treating a stale request after disable as live.
                        "active_case": status
                            .active
                            .then(|| status.requested_case.clone())
                            .flatten(),
                        "active_palette": status
                            .active
                            .then(|| status.requested_palette.clone())
                            .flatten(),
                    }))),
                    None => IpcResponse::err("compositor not active".to_string()),
                }
            }
            "get_version" | "get_ver" => IpcResponse::ok(Some(serde_json::json!({
                "version": env!("CARGO_PKG_VERSION"),
                "name": "jwm",
                "backend": self.runtime_backend.as_str(),
                "build_profile": if cfg!(debug_assertions) { "debug" } else { "release" },
            }))),
            "get_metrics" | "get_perf" => {
                if let Some(metrics) = backend.compositor_get_metrics() {
                    IpcResponse::ok(Some(serde_json::to_value(metrics).unwrap_or_default()))
                } else {
                    // Fallback if no compositor
                    IpcResponse::ok(Some(serde_json::json!({
                        "window_count": self.state.clients.len(),
                        "monitor_count": self.state.monitors.len(),
                        "tag_count": cfg.tags_length(),
                    })))
                }
            }
            "benchmark_report" | "get_bench" | "get_bm" => {
                if let Some(report) = backend.compositor_benchmark_report() {
                    IpcResponse::ok(Some(serde_json::from_str(&report).unwrap_or_default()))
                } else {
                    IpcResponse::err("benchmark not complete or not running".to_string())
                }
            }
            _ => IpcResponse::err(format!("unknown query: {name}")),
        }
    }

    fn query_wayland_status(&self, backend: &dyn Backend) -> serde_json::Value {
        let cfg = CONFIG.load();
        let backend_family = get_backend_family();
        let caps = backend.capabilities();
        let outputs = backend.output_ops().enumerate_outputs();
        let color_render_path_enabled = cfg.behavior().color_management_render_path;
        let color_advanced_enabled =
            crate::backend::color_policy::advanced_color_management_enabled();
        let color_delivery = backend
            .compositor_color_delivery_status()
            .and_then(|status| serde_json::to_value(status).ok());
        let output_details: Vec<serde_json::Value> = outputs
            .iter()
            .map(|o| {
                let vrr = backend.query_vrr_capabilities(o.id).map(|v| {
                    serde_json::json!({
                        "supported": v.supported,
                        "current_enabled": v.current_enabled,
                        "min_refresh_hz": v.min_refresh_hz,
                        "max_refresh_hz": v.max_refresh_hz,
                    })
                });
                let kms_color = backend.query_kms_color_pipeline_caps(o.id).map(|c| {
                    serde_json::json!({
                        "degamma_lut_supported": c.degamma_lut_supported,
                        "degamma_lut_size": c.degamma_lut_size,
                        "gamma_lut_supported": c.gamma_lut_supported,
                        "gamma_lut_size": c.gamma_lut_size,
                        "ctm_supported": c.ctm_supported,
                    })
                });
                let hdr_signalled = presented_with_hdr(color_delivery.as_ref(), Some(&o.name));
                let color_policy = output_color_policy_json(
                    o,
                    kms_color.as_ref(),
                    color_render_path_enabled,
                    color_advanced_enabled,
                    color_delivery.is_some(),
                    hdr_signalled,
                );
                let hdr_metadata = o.hdr_metadata.as_ref().map(|m| {
                    serde_json::json!({
                        "max_luminance_nits": m.max_luminance_nits,
                        "min_luminance_nits": m.min_luminance_nits,
                        // 0.0 means the EDID stated none; the blob sends that
                        // on as "unknown".
                        "max_frame_average_nits": m.max_frame_average_nits,
                        "supports_pq": m.supports_pq,
                        "supports_hlg": m.supports_hlg,
                        "supports_bt2020": m.supports_bt2020,
                    })
                });

                serde_json::json!({
                    "id": o.id.0,
                    "name": o.name,
                    "identity": {
                        "connector": o.identity.connector,
                        "stable_key": o.identity.stable_key,
                        "vendor": o.identity.vendor,
                        "product_code": o.identity.product_code,
                        "serial_number": o.identity.serial_number,
                        "monitor_name": o.identity.monitor_name,
                        "monitor_serial": o.identity.monitor_serial,
                    },
                    "geometry": {
                        "x": o.x,
                        "y": o.y,
                        "width": o.width,
                        "height": o.height,
                    },
                    "scale": o.scale,
                    "refresh_rate_hz": o.refresh_rate,
                    "hdr_capable": o.hdr_capable,
                    "hdr_metadata": hdr_metadata,
                    "color_management": color_policy,
                    "vrr": vrr,
                    "kms_color_pipeline": kms_color,
                })
            })
            .collect();

        let metrics = backend
            .compositor_get_metrics()
            .and_then(|m| serde_json::to_value(m).ok());
        let direct_scanout = backend
            .compositor_direct_scanout_status()
            .and_then(|s| serde_json::to_value(s).ok());
        let presentation_timing = backend
            .compositor_presentation_timing_status()
            .and_then(|s| serde_json::to_value(s).ok());
        let output_management = backend
            .compositor_output_management_status()
            .and_then(|s| serde_json::to_value(s).ok());
        let capture = backend
            .compositor_capture_status()
            .and_then(|s| serde_json::to_value(s).ok());
        let xwayland = backend
            .compositor_xwayland_status()
            .and_then(|s| serde_json::to_value(s).ok())
            .unwrap_or_else(|| {
                serde_json::json!({
                    "available": false,
                    "wm_ready": false,
                    "display": std::env::var("DISPLAY").ok(),
                    "mapped_window_count": 0,
                    "associated_surface_count": 0,
                    "pending_association_count": 0,
                })
            });
        let protocol_bind_counts_raw = backend.compositor_protocol_bind_counts();
        let protocol_bind_counts = protocol_bind_counts_raw
            .iter()
            .map(|status| {
                serde_json::json!({
                    "protocol": status.protocol,
                    "bind_count": status.bind_count,
                    "last_bound_unix_ms": status.last_bound_unix_ms,
                })
            })
            .collect::<Vec<_>>();

        let color_surfaces = backend.compositor_color_managed_surfaces();
        let color_surface_summary = color_surface_summary_json(&color_surfaces);
        let scene_linear_enabled = backend.compositor_scene_linear_active();
        let hdr_enable_refusals = backend.compositor_hdr_enable_refusals();
        // Observed, not configured: the only proof of what a display is being
        // told is the last frame that actually reached it.
        let hdr_observed_active = presented_with_hdr(color_delivery.as_ref(), None);
        let color_session_policy = color_session_policy_json(
            &outputs,
            cfg.behavior().hdr_enabled,
            color_render_path_enabled,
            scene_linear_enabled,
            color_advanced_enabled,
            color_delivery.is_some(),
            &hdr_enable_refusals,
            backend.compositor_hdr_enable_available(),
            hdr_observed_active,
        );
        let color_surface_samples = color_surfaces
            .iter()
            .take(8)
            .map(color_managed_surface_json)
            .collect::<Vec<_>>();
        let blur = backend.compositor_blur_status().map(|b| {
            serde_json::json!({
                "current_strength": b.current_strength,
                "temporal_enabled": b.temporal_enabled,
                "temporal_reuse_rate_pct": b.temporal_reuse_rate_pct,
                "hz_table": b.hz_table
                    .iter()
                    .map(|(hz, strength)| serde_json::json!({ "hz": hz, "strength": strength }))
                    .collect::<Vec<_>>(),
                "per_monitor_hz": b.per_monitor_hz
                    .iter()
                    .map(|(monitor_id, hz)| serde_json::json!({ "monitor_id": monitor_id, "hz": hz }))
                    .collect::<Vec<_>>(),
                "blur_quality_by_monitor": b.blur_quality_by_monitor
                    .iter()
                    .map(|(monitor_id, quality)| serde_json::json!({ "monitor_id": monitor_id, "quality": quality }))
                    .collect::<Vec<_>>(),
                "status_bar_frosted": b.status_bar_frosted,
                "glass_backdrop_valid": b.glass_backdrop_valid,
            })
        });
        let tearing_hint_count = backend.compositor_tearing_hint_count();
        let presentation_statuses = backend.compositor_presentation_statuses();
        let render_decisions = render_decisions_json(
            direct_scanout.as_ref(),
            blur.as_ref(),
            &output_details,
            color_delivery.as_ref(),
            tearing_hint_count,
            &presentation_statuses,
            cfg.behavior().hdr_enabled,
            cfg.behavior().blur_enabled,
            color_render_path_enabled,
            scene_linear_enabled,
            color_advanced_enabled,
            cfg.behavior().kms_color_pipeline_offload,
        );

        serde_json::json!({
            "backend_family": match backend_family {
                BackendFamily::X11 => "x11",
                BackendFamily::Wayland => "wayland",
            },
            "version": env!("CARGO_PKG_VERSION"),
            "capabilities": {
                "can_warp_pointer": caps.can_warp_pointer,
                "supports_client_list": caps.supports_client_list,
            },
            "protocols": if backend_family == BackendFamily::Wayland {
                let mut protocols = wayland_protocol_status();
                let catalog = protocol_catalog(&protocols, &protocol_bind_counts_raw);
                if let Some(obj) = protocols.as_object_mut() {
                    obj.insert(
                        "catalog".to_string(),
                        serde_json::json!(catalog),
                    );
                    obj.insert(
                        "runtime_bind_counts".to_string(),
                        serde_json::json!({
                            "scope": "jwm_owned_globals",
                            "counts": protocol_bind_counts,
                        }),
                    );
                }
                protocols
            } else {
                serde_json::json!({
                    "core": [],
                    "optional": [],
                    "env_enable_all": env_flag("JWM_OPTIONAL_GLOBALS"),
                    "runtime_bind_counts": {
                        "scope": "none",
                        "counts": [],
                    },
                })
            },
            "outputs": output_details,
            "workspaces": self.query_workspaces(backend),
            "windows": self.query_windows(backend),
            "config": self.query_config_status(),
            "scrolling": self.query_scrolling_status(),
            "gestures": self.query_gesture_status(),
            "metrics": metrics,
            "direct_scanout": direct_scanout,
            "presentation_timing": presentation_timing,
            "color_delivery": color_delivery,
            "output_management": output_management,
            "capture": capture,
            "xwayland": xwayland,
            "render_decisions": render_decisions,
            "hdr": {
                "config_enabled": cfg.behavior().hdr_enabled,
                "config_peak_nits": cfg.behavior().hdr_peak_nits,
                "capable_output_count": outputs.iter().filter(|o| o.hdr_capable).count(),
            },
            "tearing": {
                "active_surface_count": tearing_hint_count,
                "outputs": presentation_statuses
                    .iter()
                    .map(|output| serde_json::json!({
                        "output": output.output_name,
                        "client_asked_to_tear": output.client_asked_to_tear,
                        "vrr": output.vrr,
                        "tearing": output.tearing,
                        "blocker": output.blocker,
                    }))
                    .collect::<Vec<_>>(),
            },
            "session_lock": {
                "locked": backend.compositor_session_locked(),
                "lock_surface_count": backend.compositor_session_lock_surface_count(),
            },
            "color_management": {
                "surface_count": color_surfaces.len(),
                "hdr_surface_count": color_surface_summary
                    .get("hdr_surface_count")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0),
                "advanced_enabled": color_advanced_enabled,
                "render_path_enabled": color_render_path_enabled,
                "output_count": outputs.len(),
                "transfer_functions": color_surface_summary
                    .get("transfer_functions")
                    .cloned()
                    .unwrap_or_default(),
                "primaries": color_surface_summary
                    .get("primaries")
                    .cloned()
                    .unwrap_or_default(),
                "max_luminance_peak": color_surface_summary
                    .get("max_luminance_peak")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
                "session_policy": color_session_policy,
                "surface_samples": color_surface_samples,
            },
            "blur": blur,
            "not_yet_exposed": [],
        })
    }

    /// args: { "output": "<name>", "enabled": true|false }
    ///
    /// `enabled: false` clears HDR_OUTPUT_METADATA. `enabled: true` latches a
    /// request that the per-output gate reconciles every frame: the command
    /// itself fails only for a permanent refusal (hardware or configuration,
    /// see `hdr_enable_refusal_is_permanent`), while a momentary one — a
    /// toast on screen, this frame's delivery route — is what the latch
    /// carries across, so it is accepted here and reported through
    /// `get_wayland_status.color_management.session_policy`.
    fn handle_set_hdr_metadata_command(
        &mut self,
        backend: &mut dyn Backend,
        args: &serde_json::Value,
    ) -> IpcResponse {
        let output_name = match args.get("output").and_then(|v| v.as_str()) {
            Some(n) => n.to_string(),
            None => {
                return IpcResponse::err("set_hdr_metadata: missing 'output' string".to_string());
            }
        };
        // Only an absent field means "turn it on". A present value that is
        // not a JSON boolean ("false", 0, null) is a caller mistake; reading
        // it as `true` would latch HDR signalling, the opposite of what a
        // script that wrote `"false"` asked for, and report success.
        let enabled = match args.get("enabled") {
            None => true,
            Some(value) => match value.as_bool() {
                Some(enabled) => enabled,
                None => {
                    return IpcResponse::err(
                        "set_hdr_metadata: 'enabled' must be a boolean".to_string(),
                    );
                }
            },
        };
        let output_id = match backend
            .output_ops()
            .enumerate_outputs()
            .into_iter()
            .find(|o| o.name == output_name)
        {
            Some(o) => o.id,
            None => {
                return IpcResponse::err(format!(
                    "set_hdr_metadata: output '{output_name}' not found"
                ));
            }
        };
        match backend.set_hdr_metadata(output_id, enabled) {
            Ok(()) => IpcResponse::ok(Some(serde_json::json!({
                "output": output_name,
                "enabled": enabled,
            }))),
            Err(e) => IpcResponse::err(format!("{e}")),
        }
    }

    /// Apply a single in-memory config override (does not touch the file).
    /// args: { "key": "appearance.border_px", "value": <json> }
    fn handle_set_config_command(
        &mut self,
        backend: &mut dyn Backend,
        args: &serde_json::Value,
    ) -> IpcResponse {
        let key = match args.get("key").and_then(|v| v.as_str()) {
            Some(k) => k.to_string(),
            None => return IpcResponse::err("set_config: missing 'key' string".to_string()),
        };
        let value = match args.get("value") {
            Some(v) => v.clone(),
            None => return IpcResponse::err("set_config: missing 'value'".to_string()),
        };

        let mut new_cfg = (**CONFIG.load()).clone();
        if let Err(e) = new_cfg.set_value(&key, &value) {
            return IpcResponse::err(e);
        }
        CONFIG.store(std::sync::Arc::new(new_cfg));

        self.apply_config_changes(backend);
        self.broadcast_ipc_event(
            "config/changed",
            serde_json::json!({ "key": key, "value": value }),
        );
        IpcResponse::ok(None)
    }

    fn handle_set_config_batch_command(
        &mut self,
        backend: &mut dyn Backend,
        args: &serde_json::Value,
    ) -> IpcResponse {
        let changes = match parse_config_batch_changes(args) {
            Ok(changes) => changes,
            Err(e) => return IpcResponse::err(e),
        };

        let mut new_cfg = (**CONFIG.load()).clone();
        if let Err(e) = new_cfg.set_values(&changes) {
            return IpcResponse::err(e);
        }
        CONFIG.store(std::sync::Arc::new(new_cfg));

        self.apply_config_changes(backend);
        let changed_keys = changes
            .iter()
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        self.broadcast_ipc_event(
            "config/changed",
            serde_json::json!({
                "batch": true,
                "change_count": changed_keys.len(),
                "keys": changed_keys,
            }),
        );
        IpcResponse::ok(Some(serde_json::json!({
            "applied": changes.len(),
            "atomic": true,
        })))
    }

    fn handle_command_batch(
        &mut self,
        backend: &mut dyn Backend,
        args: &serde_json::Value,
    ) -> IpcResponse {
        let commands = match parse_command_batch_entries(args) {
            Ok(commands) => commands,
            Err(e) => return IpcResponse::err(e),
        };
        let stop_on_error = args
            .get("stop_on_error")
            .and_then(|value| value.as_bool())
            .unwrap_or(true);

        let mut results = Vec::with_capacity(commands.len());
        let mut failed_at = None;
        for (idx, (name, command_args)) in commands.iter().enumerate() {
            let response = self.handle_ipc_command(backend, name, command_args);
            let success = response.success;
            let error = response.error.clone();
            results.push(serde_json::json!({
                "index": idx,
                "command": name,
                "success": success,
                "response": response,
            }));
            if !success {
                failed_at = Some((idx, error.unwrap_or_else(|| "command failed".to_string())));
                if stop_on_error {
                    break;
                }
            }
        }

        let executed = results.len();
        let success = failed_at.is_none();
        let (failed_at_index, error) = failed_at
            .as_ref()
            .map(|(idx, error)| (Some(*idx), Some(error.clone())))
            .unwrap_or((None, None));
        let data = serde_json::json!({
            "success": success,
            "requested": commands.len(),
            "executed": executed,
            "failed_at": failed_at_index,
            "stop_on_error": stop_on_error,
            "results": results,
        });
        IpcResponse {
            success,
            data: Some(data),
            error,
        }
    }

    /// Move a specific window (by raw id) to an absolute monitor index.
    /// args: { "window": <u64>, "monitor": <i32> }
    fn handle_move_window_to_monitor(
        &mut self,
        backend: &mut dyn Backend,
        args: &serde_json::Value,
    ) -> IpcResponse {
        let win_id = match args.get("window").and_then(|v| v.as_u64()) {
            Some(v) => v,
            None => {
                return IpcResponse::err(
                    "move_window_to_monitor: missing 'window' (u64)".to_string(),
                );
            }
        };
        let target_num = match parse_required_i32_ipc_arg(args, "move_window_to_monitor", "monitor")
        {
            Ok(value) => value,
            Err(error) => return IpcResponse::err(error),
        };

        let win = crate::backend::common_define::WindowId::from_raw(win_id);
        let client_key = match self.state.win_to_client.get(&win).copied() {
            Some(k) => k,
            None => {
                return IpcResponse::err(format!("window {win_id:#x} not managed by jwm"));
            }
        };

        let target_mon_key = self.state.monitor_order.iter().copied().find(|&mk| {
            self.state
                .monitors
                .get(mk)
                .map(|m| m.num == target_num)
                .unwrap_or(false)
        });
        let target_mon_key = match target_mon_key {
            Some(k) => k,
            None => {
                return IpcResponse::err(format!("monitor {target_num} not found"));
            }
        };

        self.sendmon(backend, Some(client_key), Some(target_mon_key));
        IpcResponse::ok(None)
    }

    fn handle_benchmark_command(
        &self,
        backend: &mut dyn Backend,
        args: &serde_json::Value,
    ) -> IpcResponse {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
        match action {
            "start" => {
                let request = match parse_benchmark_request(args) {
                    Ok(request) => request,
                    Err(error) => return IpcResponse::err(error),
                };
                if backend.compositor_benchmark_start(request.frames, request.warmup) {
                    IpcResponse::ok(Some(serde_json::json!({
                        "status": "started",
                        "frames": request.frames,
                        "warmup": request.warmup,
                    })))
                } else {
                    IpcResponse::err(
                        "benchmark: compositor unavailable or benchmark could not be started"
                            .to_string(),
                    )
                }
            }
            "stop" => {
                if let Some(report) = backend.compositor_benchmark_stop() {
                    IpcResponse::ok(Some(serde_json::from_str(&report).unwrap_or_default()))
                } else {
                    IpcResponse::err("benchmark not running".to_string())
                }
            }
            _ => IpcResponse::err(format!("unknown benchmark action: {action}")),
        }
    }

    // -------------------------------------------------------------------------
    // Query helpers
    // -------------------------------------------------------------------------

    fn query_runtime_status(&self, backend: &dyn Backend) -> RuntimeStatusV1 {
        let config = self.query_config_status();
        let windows = self.ipc_window_count();
        let monitors = self.ipc_monitor_count();
        let workspaces = monitors * CONFIG.load().tags_length();
        let uptime_ms = u64::try_from(self.started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
        let configured_compositor = CONFIG.load().compositor_enabled();
        let compositor_configured = if matches!(self.runtime_backend.as_str(), "x11rb" | "xcb") {
            crate::config::effective_x11_compositor_enabled(configured_compositor)
        } else {
            configured_compositor
        };
        let window_flags = self.window_flag_counts();

        RuntimeStatusV1 {
            schema_version: 1,
            version: env!("CARGO_PKG_VERSION").to_string(),
            backend: self.runtime_backend.clone(),
            compiled_backends: crate::application::compiled_backends()
                .into_iter()
                .map(str::to_string)
                .collect(),
            pid: std::process::id(),
            allocations: crate::alloc_counter::current(),
            uptime_ms,
            health: runtime_health(
                &config,
                monitors,
                self.features.compositor_transition.last_error.as_deref(),
            ),
            counts: RuntimeCounts {
                windows,
                monitors,
                workspaces,
            },
            config,
            compositor_active: backend.has_compositor(),
            compositor_configured,
            compositor_temporary: self.features.system_ui_temporary_compositor,
            compositor_transition: CompositorTransitionStatus {
                attempts: self.features.compositor_transition.attempts,
                last_requested_active: self.features.compositor_transition.last_requested_active,
                last_attempt_unix_ms: self.features.compositor_transition.last_attempt_unix_ms,
                last_success: self.features.compositor_transition.last_success,
                last_error: self.features.compositor_transition.last_error.clone(),
            },
            features: RuntimeFeatureStates {
                do_not_disturb: self.do_not_disturb,
                screenshot: self.features.screenshot.active,
                overview: self.features.overview.active,
                recording: self.features.recording.active,
                audio_recording: self.features.audio_recording.active,
                magnifier: self.features.magnifier.enabled,
                system_ui: self.features.system_ui.is_active(),
                peek: self.features.peek_active,
                expose: self.features.expose_active,
                annotation: self.features.annotation_active,
                layout_picker: self.features.system_ui.is_layout_picker(),
                tags_overview: self.features.system_ui.is_tags_overview(),
                calendar: self.features.system_ui.is_calendar(),
                keybindings: self.features.system_ui.is_keybindings(),
                monitor_layout: self.features.system_ui.is_monitor_layout(),
                launcher: self.features.system_ui.is_launcher(),
                session_menu: self.features.system_ui.is_session_menu(),
                notifications: self.features.system_ui.is_notification_center(),
                waterlily: backend
                    .compositor_waterlily_status()
                    .is_some_and(|status| status.enabled),
                night_light: self.night_light_active(),
                idle_inhibit: self.idle_inhibited,
                control_center: self.features.system_ui.is_control_center(),
                clipboard_picker: self.features.system_ui.is_clipboard_picker(),
                wifi_picker: self.features.system_ui.is_wifi_picker(),
                bluetooth_picker: self.features.system_ui.is_bluetooth_picker(),
                wallpaper_picker: self.features.system_ui.is_wallpaper_picker(),
                theme_picker: self.features.system_ui.is_theme_picker(),
                audio_output_picker: self.features.system_ui.audio_picker_direction()
                    == Some(crate::jwm::features::system_controls::AudioDirection::Output),
                audio_input_picker: self.features.system_ui.audio_picker_direction()
                    == Some(crate::jwm::features::system_controls::AudioDirection::Input),
                media_players: self.features.system_ui.is_media_players_picker(),
                window_switcher: self.features.system_ui.is_window_switcher(),
                session_lock: self.features.system_ui.is_session_lock(),
                monitor_lock: self.features.system_ui.lock_scope().is_some_and(|scope| {
                    matches!(scope, crate::jwm::features::LockScope::Monitor(_))
                }),
                debug_hud: self.debug_hud_on,
            },
            compositor_metrics: backend
                .compositor_get_metrics()
                .and_then(|metrics| serde_json::to_value(metrics).ok()),
            resources: Some(self.resources_json()),
            connectivity: Some(self.connectivity_json()),
            power: Some(self.power_status_json()),
            media: Some(self.media_status_json()),
            notifications: Some(self.notifications_status_summary()),
            blur: Some(self.blur_status_summary(backend)),
            hdr: Some(self.hdr_status_summary(backend)),
            capture: Some(self.capture_status_summary(backend)),
            idle: Some(self.idle_status_summary(backend)),
            recording: Some(self.recording_status_summary(backend)),
            audio_recording: Some(self.audio_recording_status_summary()),
            clipboard: Some(self.clipboard_status_summary()),
            waterlily: Some(self.waterlily_status_summary(backend)),
            night_light: Some(self.night_light_status_summary()),
            magnifier: Some(self.magnifier_status_summary()),
            peek: Some(self.peek_status_summary(backend)),
            expose: Some(self.expose_status_summary()),
            gesture: Some(self.gesture_status_summary()),
            wayland: Some(self.wayland_status_summary(backend)),
            dnd: Some(serde_json::json!({ "enabled": self.do_not_disturb })),
            session_lock: Some(self.session_lock_status_summary(backend)),
            tearing: Some(self.tearing_status_summary(backend)),
            xwayland: Some(self.xwayland_status_summary(backend)),
            scrolling: Some(self.scrolling_status_summary()),
            color_management: Some(self.color_management_status_summary(backend)),
            audio: Some(self.audio_status_summary()),
            wallpaper: Some(self.wallpaper_status_summary()),
            bluetooth: Some(self.bluetooth_status_summary()),
            system_ui: Some(self.system_ui_status_summary()),
            layout: Some(self.layout_status_summary(backend)),
            tabs: Some(self.tabs_status_summary(backend)),
            struts: Some(self.struts_status_summary(backend)),
            scratchpads: Some(self.scratchpads_status_summary()),
            gaps: Some(self.query_focused_gaps(backend)),
            mfact: Some(self.query_focused_mfact(backend)),
            nmaster: Some(self.query_focused_nmaster(backend)),
            show_bar: Some(self.query_focused_show_bar(backend)),
            metrics: self.query_runtime_status_metrics(backend),
            version_info: Some(serde_json::json!({
                "version": env!("CARGO_PKG_VERSION"),
                "backend": self.runtime_backend,
            })),
            monitors: Some(self.monitors_status_summary()),
            workspaces: Some(self.workspaces_status_summary()),
            windows: Some(self.windows_status_summary()),
            tree: Some(self.tree_status_summary()),
            focused: Some(self.query_selected_window(backend, true)),
            cfact: Some(self.query_focused_cfact(backend)),
            prev_layout: Some(self.query_focused_prev_layout(backend)),
            effects: Some(self.effects_status_summary()),
            mic: Some(self.mic_status_summary()),
            capabilities: Some(
                serde_json::to_value(crate::ipc::ipc_capabilities()).unwrap_or_default(),
            ),
            selected: Some(self.query_selected_window(backend, false)),
            bench: Some(self.bench_status_summary(backend)),
            floating: Some(self.window_flag_status_summary(window_flags, "floating")),
            minimized: Some(self.window_flag_status_summary(window_flags, "minimized")),
            sticky: Some(self.window_flag_status_summary(window_flags, "sticky")),
            urgent: Some(self.window_flag_status_summary(window_flags, "urgent")),
            fullscreen: Some(self.window_flag_status_summary(window_flags, "fullscreen")),
            pip: Some(self.window_flag_status_summary(window_flags, "pip")),
        }
    }

    fn window_info(
        &self,
        backend: &dyn Backend,
        client_key: ClientKey,
        is_focused: bool,
    ) -> Option<WindowInfo> {
        self.window_info_indexed(backend, client_key, is_focused, None)
    }

    fn window_info_indexed(
        &self,
        backend: &dyn Backend,
        client_key: ClientKey,
        is_focused: bool,
        index: Option<&WindowQueryIndex>,
    ) -> Option<WindowInfo> {
        let client = self.state.clients.get(client_key)?;
        let scratchpad = index.map_or_else(
            || {
                self.scratchpads
                    .iter()
                    .find(|(_, key)| **key == client_key)
                    .map(|(name, _)| name.clone())
            },
            |index| index.scratchpads.get(&client_key).cloned(),
        );
        let is_scratchpad = scratchpad.is_some();
        let (connector, monitor_name, is_on_view, layout, is_tabbed, tab_index) = match client.mon {
            Some(mk) => {
                let cached_monitor = index.and_then(|index| index.monitors.get(&mk));
                let mon = self.state.monitors.get(mk);
                let is_on_view = cached_monitor.map_or_else(
                    || {
                        mon.is_some_and(|monitor| {
                            client.state.is_sticky
                                || (client.state.tags & monitor.get_active_tags()) != 0
                        })
                    },
                    |monitor| {
                        client.state.is_sticky || (client.state.tags & monitor.active_tags) != 0
                    },
                );
                let layout = cached_monitor
                    .map(|monitor| monitor.layout.clone())
                    .or_else(|| mon.map(|monitor| format!("{:?}", *monitor.lt)));
                let tab_index = if let Some(index) = index {
                    index.tabs.get(&(mk, client_key)).copied()
                } else {
                    self.tab_group_clients(mk)
                        .iter()
                        .position(|&ck| ck == client_key)
                };
                let is_tabbed = tab_index.is_some();
                (
                    cached_monitor.map_or_else(
                        || self.output_key_for_monitor(backend, mk),
                        |monitor| monitor.connector.clone(),
                    ),
                    cached_monitor.map_or_else(
                        || self.output_monitor_name_for_monitor(backend, mk),
                        |monitor| monitor.monitor_name.clone(),
                    ),
                    is_on_view,
                    layout,
                    is_tabbed,
                    tab_index,
                )
            }
            None => (None, None, false, None, false, None),
        };
        let swallowing = client.swallowing.and_then(|parent_key| {
            self.state
                .clients
                .get(parent_key)
                .map(|parent| parent.win.raw())
        });
        let swallowed_by = if client.state.is_swallowed {
            index.map_or_else(
                || {
                    self.state.client_order.iter().find_map(|&ck| {
                        let other = self.state.clients.get(ck)?;
                        (other.swallowing == Some(client_key)).then_some(other.win.raw())
                    })
                },
                |index| index.swallowed_by.get(&client_key).copied(),
            )
        } else {
            None
        };
        let transient_for = backend
            .property_ops()
            .transient_for(client.win)
            .map(|win| win.raw());
        let has_strut = self.external_struts.contains_key(&client.win);
        let maximize_restore_anchor = client
            .state
            .maximize_restore_anchor
            .and_then(|anchor| self.state.clients.get(anchor).map(|c| c.win.raw()));
        let is_status_bar = client.is_status_bar(CONFIG.load().status_bar_name());
        let stack_index = client.mon.and_then(|mk| {
            index.map_or_else(
                || {
                    self.state
                        .monitor_clients
                        .get(mk)
                        .and_then(|clients| clients.iter().position(|&ck| ck == client_key))
                },
                |index| index.stack_positions.get(&(mk, client_key)).copied(),
            )
        });
        Some(client_window_info(
            client,
            resolved_client_monitor_num(&self.state.monitors, client),
            is_focused,
            is_on_view,
            is_scratchpad,
            has_strut,
            scratchpad,
            layout,
            connector,
            monitor_name,
            swallowing,
            swallowed_by,
            transient_for,
            is_tabbed,
            tab_index,
            maximize_restore_anchor,
            is_status_bar,
            stack_index,
        ))
    }

    pub(crate) fn query_windows(&self, backend: &dyn Backend) -> Vec<WindowInfo> {
        let sel_client = self.get_selected_client_key();
        let index = self.window_query_index(backend);
        self.state
            .client_order
            .iter()
            .filter_map(|&ck| {
                self.window_info_indexed(backend, ck, sel_client == Some(ck), Some(&index))
            })
            .collect()
    }

    fn window_query_index(&self, backend: &dyn Backend) -> WindowQueryIndex {
        let mut scratchpads = std::collections::HashMap::new();
        for (name, &key) in &self.scratchpads {
            scratchpads.entry(key).or_insert_with(|| name.clone());
        }
        let mut tabs = std::collections::HashMap::new();
        let mut monitors = std::collections::HashMap::new();
        for (monitor, state) in &self.state.monitors {
            monitors.insert(
                monitor,
                WindowMonitorProjection {
                    connector: self.output_key_for_monitor(backend, monitor),
                    monitor_name: self.output_monitor_name_for_monitor(backend, monitor),
                    active_tags: state.get_active_tags(),
                    layout: format!("{:?}", *state.lt),
                },
            );
            for (position, client) in self.tab_group_clients(monitor).into_iter().enumerate() {
                tabs.entry((monitor, client)).or_insert(position);
            }
        }
        let mut stack_positions = std::collections::HashMap::new();
        for (monitor, clients) in &self.state.monitor_clients {
            for (position, &client) in clients.iter().enumerate() {
                stack_positions.entry((monitor, client)).or_insert(position);
            }
        }
        let mut swallowed_by = std::collections::HashMap::new();
        for &key in &self.state.client_order {
            if let Some(client) = self.state.clients.get(key)
                && let Some(parent) = client.swallowing
            {
                swallowed_by.entry(parent).or_insert(client.win.raw());
            }
        }
        WindowQueryIndex {
            scratchpads,
            tabs,
            monitors,
            stack_positions,
            swallowed_by,
        }
    }

    pub(crate) fn query_workspaces(&self, backend: &dyn Backend) -> Vec<WorkspaceInfo> {
        let cfg = CONFIG.load();
        let scratchpads: std::collections::HashSet<ClientKey> =
            self.scratchpads.values().copied().collect();
        let strut_wins: std::collections::HashSet<_> =
            self.external_struts.keys().copied().collect();
        let status_bar_name = cfg.status_bar_name();
        let mut result = Vec::new();
        for &mk in &self.state.monitor_order {
            let mon = match self.state.monitors.get(mk) {
                Some(m) => m,
                None => continue,
            };
            let connector = self.output_key_for_monitor(backend, mk);
            let monitor_name = self.output_monitor_name_for_monitor(backend, mk);
            let active_tags = mon.get_active_tags();
            let tabbed: std::collections::HashSet<_> =
                self.tab_group_clients(mk).into_iter().collect();
            let per_tag_counts = monitor_tag_client_counts(
                &self.state,
                mk,
                cfg.tags_length(),
                &scratchpads,
                &tabbed,
                &strut_wins,
                status_bar_name,
                active_tags,
            );
            let (occupied_tags_mask, urgent_tags_mask) = {
                const EMPTY_CLIENTS: &[ClientKey] = &[];
                let monitor_clients = self
                    .state
                    .monitor_clients
                    .get(mk)
                    .map_or(EMPTY_CLIENTS, Vec::as_slice);
                crate::jwm::StatusBarBuilder::calculate_tag_masks(
                    &self.state.clients,
                    monitor_clients,
                )
            };
            for (i, counts) in per_tag_counts.into_iter().enumerate() {
                let tag_bit = 1u32 << i;
                let is_active = (active_tags & tag_bit) != 0;
                let (layout, m_fact, n_master, gap) = workspace_layout_state(mon, i);
                let (show_bar, prev_layout, selected_id) =
                    workspace_tag_extras(mon, i, &self.state.clients);
                result.push(WorkspaceInfo {
                    tag_mask: tag_bit,
                    tag_index: i,
                    monitor: mon.num,
                    layout,
                    m_fact,
                    n_master,
                    gap,
                    num_clients: counts.total,
                    focused: is_active && self.state.sel_mon == Some(mk),
                    is_urgent: (urgent_tags_mask & tag_bit) != 0,
                    is_occupied: (occupied_tags_mask & tag_bit) != 0,
                    has_fullscreen: counts.fullscreen != 0,
                    has_visible_fullscreen: is_active
                        && self.monitor_has_visible_fullscreen(mk),
                    connector: connector.clone(),
                    monitor_name: monitor_name.clone(),
                    show_bar,
                    prev_layout,
                    selected_id,
                    minimized_count: counts.minimized,
                    floating_count: counts.floating,
                    sticky_count: counts.sticky,
                    urgent_count: counts.urgent,
                    fullscreen_count: counts.fullscreen,
                    pip_count: counts.pip,
                    maximized_count: counts.maximized,
                    above_count: counts.above,
                    below_count: counts.below,
                    fixed_count: counts.fixed,
                    scratchpad_count: counts.scratchpad,
                    tabbed_count: counts.tabbed,
                    dock_count: counts.dock,
                    desktop_count: counts.desktop,
                    never_focus_count: counts.never_focus,
                    demands_attention_count: counts.demands_attention,
                    skip_taskbar_count: counts.skip_taskbar,
                    skip_pager_count: counts.skip_pager,
                    no_decorations_count: counts.no_decorations,
                    drag_float_count: counts.drag_float,
                    swallowed_count: counts.swallowed,
                    on_view_count: counts.on_view,
                    maximize_promoted_count: counts.maximize_promoted,
                    strut_count: counts.strut,
                    status_bar_count: counts.status_bar,
                    owns_output_count: if is_active {
                        self.monitor_owns_output_count(mk)
                    } else {
                        0
                    },
                });
            }
        }
        result
    }

    pub(crate) fn query_monitors(&self, backend: &dyn Backend) -> Vec<MonitorInfoIpc> {
        self.state
            .monitor_order
            .iter()
            .filter_map(|&mk| {
                let m = self.state.monitors.get(mk)?;
                Some(self.monitor_info_ipc(backend, mk, m))
            })
            .collect()
    }

    fn monitor_info_ipc(
        &self,
        backend: &dyn Backend,
        mk: MonitorKey,
        m: &WMMonitor,
    ) -> MonitorInfoIpc {
        let work = self.monitor_work_area(mk).unwrap_or_else(|| {
            crate::core::types::Rect::new(
                m.geometry.w_x,
                m.geometry.w_y,
                m.geometry.w_w,
                m.geometry.w_h,
            )
        });
        let (scale, refresh_mhz) = self.output_scale_refresh_for_monitor(backend, mk);
        let (vendor, product_code, serial_number, monitor_serial) =
            self.output_edid_ids_for_monitor(backend, mk);
        let (vrr_supported, vrr_enabled, vrr_min_hz, vrr_max_hz) =
            self.output_vrr_for_monitor(backend, mk);
        let (physical_width_mm, physical_height_mm) =
            self.output_physical_mm_for_monitor(backend, mk);
        let (preferred_width, preferred_height, preferred_refresh_mhz) =
            self.output_preferred_mode_for_monitor(backend, mk);
        let hdr_metadata = self.output_hdr_metadata_for_monitor(backend, mk);
        let (strut_top, strut_bottom, strut_left, strut_right) = self.get_strut_reserved(mk);
        let show_bar = m
            .pertag
            .as_ref()
            .and_then(|p| p.show_bars.get(p.cur_tag).copied())
            .unwrap_or(true);
        let selected_id = m
            .sel
            .and_then(|ck| self.state.clients.get(ck).map(|c| c.win.raw()));
        let sel_tags = m.sel_tags & 1;
        let previous_tags = m.tag_set[1 - sel_tags];
        let (cur_tag, prev_tag) = m
            .pertag
            .as_ref()
            .map(|p| (p.cur_tag, p.prev_tag))
            .unwrap_or((0, 0));
        let scratchpads: std::collections::HashSet<_> =
            self.scratchpads.values().copied().collect();
        let tabbed: std::collections::HashSet<_> = self.tab_group_clients(mk).into_iter().collect();
        let strut_wins: std::collections::HashSet<_> =
            self.external_struts.keys().copied().collect();
        let cfg = CONFIG.load();
        let counts = tag_client_counts(
            &self.state,
            mk,
            u32::MAX,
            &scratchpads,
            &tabbed,
            &strut_wins,
            cfg.status_bar_name(),
            m.get_active_tags(),
        );
        MonitorInfoIpc {
            num: m.num,
            x: m.geometry.m_x,
            y: m.geometry.m_y,
            w: m.geometry.m_w,
            h: m.geometry.m_h,
            wx: work.x,
            wy: work.y,
            ww: work.w,
            wh: work.h,
            active_tags: m.get_active_tags(),
            layout: format!("{:?}", *m.lt),
            focused: self.state.sel_mon == Some(mk),
            locked: self.monitor_is_locked(m.num),
            connector: self.output_key_for_monitor(backend, mk),
            name: self.output_name_for_monitor(backend, mk),
            monitor_name: self.output_monitor_name_for_monitor(backend, mk),
            vendor,
            product_code,
            serial_number,
            monitor_serial,
            scale,
            refresh_mhz,
            hdr_capable: self.output_hdr_capable_for_monitor(backend, mk),
            vrr_supported,
            vrr_enabled,
            gap: m.layout.gap,
            m_fact: m.layout.m_fact,
            n_master: m.layout.n_master,
            transform: self.output_transform_for_monitor(backend, mk),
            tab_bar_reserved: self.tab_bar_reserved(mk),
            hdr_metadata,
            physical_width_mm,
            physical_height_mm,
            preferred_width,
            preferred_height,
            preferred_refresh_mhz,
            vrr_min_hz,
            vrr_max_hz,
            prev_layout: format!("{:?}", *m.prev_lt),
            show_bar,
            bar_visible: self.monitor_shows_status_bar(mk),
            has_visible_fullscreen: self.monitor_has_visible_fullscreen(mk),
            strut_top,
            strut_bottom,
            strut_left,
            strut_right,
            selected_id,
            sel_tags,
            previous_tags,
            cur_tag,
            prev_tag,
            output_connector: self.output_connector_for_monitor(backend, mk),
            lt_symbol: m.lt_symbol.clone(),
            output_id: self.output_id_for_monitor(backend, mk),
            window_count: counts.total,
            floating_count: counts.floating,
            minimized_count: counts.minimized,
            sticky_count: counts.sticky,
            urgent_count: counts.urgent,
            fullscreen_count: counts.fullscreen,
            pip_count: counts.pip,
            maximized_count: counts.maximized,
            above_count: counts.above,
            below_count: counts.below,
            fixed_count: counts.fixed,
            scratchpad_count: counts.scratchpad,
            tabbed_count: counts.tabbed,
            dock_count: counts.dock,
            desktop_count: counts.desktop,
            never_focus_count: counts.never_focus,
            demands_attention_count: counts.demands_attention,
            skip_taskbar_count: counts.skip_taskbar,
            skip_pager_count: counts.skip_pager,
            no_decorations_count: counts.no_decorations,
            drag_float_count: counts.drag_float,
            swallowed_count: counts.swallowed,
            on_view_count: counts.on_view,
            maximize_promoted_count: counts.maximize_promoted,
            strut_count: counts.strut,
            status_bar_count: counts.status_bar,
            owns_output_count: self.monitor_owns_output_count(mk),
        }
    }

    /// Focused monitor's window tab strip, if any.
    pub(crate) fn query_focused_tab_bar(&self, backend: &dyn Backend) -> serde_json::Value {
        let Some(mk) = self.state.sel_mon else {
            return serde_json::json!({
                "monitor": serde_json::Value::Null,
                "reserved": 0,
                "windows": [],
            });
        };
        let mon_num = self
            .state
            .monitors
            .get(mk)
            .map(|m| m.num)
            .unwrap_or_default();
        let group = self.tab_group_clients(mk);
        let reserved = self.tab_bar_reserved(mk);
        let windows: Vec<serde_json::Value> = group
            .iter()
            .filter_map(|&ck| {
                let client = self.state.clients.get(ck)?;
                Some(serde_json::json!({
                    "id": client.win.raw(),
                    "name": client.name,
                    "class": client.class,
                    "focused": self.state.monitors.get(mk).and_then(|m| m.sel) == Some(ck),
                }))
            })
            .collect();
        let mut value = serde_json::json!({
            "monitor": mon_num,
            "reserved": reserved,
            "windows": windows,
        });
        if let Some(selected_id) = self
            .state
            .monitors
            .get(mk)
            .and_then(|m| m.sel)
            .and_then(|ck| self.state.clients.get(ck).map(|client| client.win.raw()))
            .filter(|_| !group.is_empty())
        {
            value
                .as_object_mut()
                .expect("tab bar object")
                .insert("selected_id".into(), serde_json::json!(selected_id));
        }
        if let Some(connector) = self.output_key_for_monitor(backend, mk) {
            value
                .as_object_mut()
                .expect("tab bar object")
                .insert("connector".into(), serde_json::Value::String(connector));
        }
        value
    }

    /// Focused monitor's live layout parameters (`setlayout` / `setmfact` /
    /// `incnmaster` / `setgaps` targets).
    pub(crate) fn query_focused_layout(&self, backend: &dyn Backend) -> serde_json::Value {
        self.focused_layout_snapshot(backend).unwrap_or_else(|| {
            serde_json::json!({
                "monitor": serde_json::Value::Null,
                "layout": serde_json::Value::Null,
                "m_fact": serde_json::Value::Null,
                "n_master": serde_json::Value::Null,
                "gap": serde_json::Value::Null,
            })
        })
    }

    /// Focused monitor's tiling gap only (same source as [`Self::query_focused_layout`]).
    pub(crate) fn query_focused_gaps(&self, backend: &dyn Backend) -> serde_json::Value {
        match self.focused_layout_snapshot(backend) {
            Some(snapshot) => {
                let mut value = serde_json::json!({
                    "monitor": snapshot["monitor"].clone(),
                    "gap": snapshot["gap"].clone(),
                });
                if let Some(connector) = snapshot.get("connector") {
                    value
                        .as_object_mut()
                        .expect("gaps snapshot object")
                        .insert("connector".into(), connector.clone());
                }
                value
            }
            None => serde_json::json!({
                "monitor": serde_json::Value::Null,
                "gap": serde_json::Value::Null,
            }),
        }
    }

    /// Focused monitor's `n_master` only (same source as [`Self::query_focused_layout`]).
    pub(crate) fn query_focused_nmaster(&self, backend: &dyn Backend) -> serde_json::Value {
        match self.focused_layout_snapshot(backend) {
            Some(snapshot) => {
                let mut value = serde_json::json!({
                    "monitor": snapshot["monitor"].clone(),
                    "n_master": snapshot["n_master"].clone(),
                });
                if let Some(connector) = snapshot.get("connector") {
                    value
                        .as_object_mut()
                        .expect("nmaster snapshot object")
                        .insert("connector".into(), connector.clone());
                }
                value
            }
            None => serde_json::json!({
                "monitor": serde_json::Value::Null,
                "n_master": serde_json::Value::Null,
            }),
        }
    }

    /// Focused monitor's `m_fact` only (same source as [`Self::query_focused_layout`]).
    pub(crate) fn query_focused_mfact(&self, backend: &dyn Backend) -> serde_json::Value {
        match self.focused_layout_snapshot(backend) {
            Some(snapshot) => {
                let mut value = serde_json::json!({
                    "monitor": snapshot["monitor"].clone(),
                    "m_fact": snapshot["m_fact"].clone(),
                });
                if let Some(connector) = snapshot.get("connector") {
                    value
                        .as_object_mut()
                        .expect("mfact snapshot object")
                        .insert("connector".into(), connector.clone());
                }
                value
            }
            None => serde_json::json!({
                "monitor": serde_json::Value::Null,
                "m_fact": serde_json::Value::Null,
            }),
        }
    }

    /// Focused client's `client_fact` (twin of [`Self::query_focused_mfact`]).
    pub(crate) fn query_focused_cfact(&self, backend: &dyn Backend) -> serde_json::Value {
        let Some(ck) = self.get_selected_client_key() else {
            return serde_json::json!({
                "id": serde_json::Value::Null,
                "client_fact": serde_json::Value::Null,
            });
        };
        let Some(client) = self.state.clients.get(ck) else {
            return serde_json::json!({
                "id": serde_json::Value::Null,
                "client_fact": serde_json::Value::Null,
            });
        };
        let mut value = serde_json::json!({
            "id": client.win.raw(),
            "client_fact": client.state.client_fact,
        });
        if let Some(mk) = client.mon {
            if let Some(connector) = self.output_key_for_monitor(backend, mk) {
                value
                    .as_object_mut()
                    .expect("cfact snapshot object")
                    .insert("connector".into(), serde_json::Value::String(connector));
            }
        }
        value
    }

    /// Status-bar preference and occupancy for one monitor.
    ///
    /// Object keys: `monitor`, `show_bar`, `bar_visible`,
    /// `has_visible_fullscreen`, `owns_output_count`, optional `connector`.
    /// Also `tag` (`Pertag.cur_tag`) because `show_bar` is per-tag.
    /// Also `layout` (`WMMonitor.lt_symbol`).
    /// Also `gap` (`MonitorLayout.gap`).
    /// Also `mfact` (`MonitorLayout.m_fact`).
    /// Also `nmaster` (`MonitorLayout.n_master`).
    /// Also `prev_tag` (`Pertag.prev_tag`).
    /// Also `selected_id` (`WMMonitor.sel` window id).
    /// Also `sel_tags` (`WMMonitor.sel_tags` dual-tagset index).
    /// Also `previous_tags` (inactive tagset mask).
    /// Also `active_tags` (current tagset mask).
    /// Also `window_count` (clients attached to this monitor).
    /// Also `on_view_count` (clients visible on the current tagset).
    /// Also `floating_count` (attached floating clients).
    /// Also `minimized_count` (attached hidden clients).
    /// Also `sticky_count` (attached sticky clients).
    /// Also `urgent_count` (attached urgent or demands-attention clients).
    /// Also `fullscreen_count` (attached fullscreen clients).
    /// Also `pip_count` (attached picture-in-picture clients).
    /// Also `maximized_count` (attached vert- or horz-maximized clients).
    /// Also `above_count` (attached keep-above clients).
    /// Also `below_count` (attached keep-below clients).
    /// Also `scratchpad_count` (attached scratchpad-bound clients).
    /// Also `tabbed_count` (clients in the monitor's window-tab strip).
    /// Also `dock_count` (attached dock clients).
    /// Also `desktop_count` (attached desktop clients).
    /// Also `never_focus_count` (attached never-focus clients).
    /// Also `skip_taskbar_count` (attached skip-taskbar clients).
    /// Also `skip_pager_count` (attached skip-pager clients).
    pub(crate) fn query_show_bar_for_monitor(
        &self,
        backend: &dyn Backend,
        mk: MonitorKey,
    ) -> serde_json::Value {
        let Some(mon) = self.state.monitors.get(mk) else {
            return serde_json::json!({
                "monitor": serde_json::Value::Null,
                "tag": serde_json::Value::Null,
                "layout": serde_json::Value::Null,
                "gap": serde_json::Value::Null,
                "mfact": serde_json::Value::Null,
                "nmaster": serde_json::Value::Null,
                "prev_tag": serde_json::Value::Null,
                "selected_id": serde_json::Value::Null,
                "sel_tags": serde_json::Value::Null,
                "previous_tags": serde_json::Value::Null,
                "active_tags": serde_json::Value::Null,
                "window_count": serde_json::Value::Null,
                "on_view_count": serde_json::Value::Null,
                "floating_count": serde_json::Value::Null,
                "minimized_count": serde_json::Value::Null,
                "sticky_count": serde_json::Value::Null,
                "urgent_count": serde_json::Value::Null,
                "fullscreen_count": serde_json::Value::Null,
                "pip_count": serde_json::Value::Null,
                "maximized_count": serde_json::Value::Null,
                "above_count": serde_json::Value::Null,
                "below_count": serde_json::Value::Null,
                "scratchpad_count": serde_json::Value::Null,
                "tabbed_count": serde_json::Value::Null,
                "dock_count": serde_json::Value::Null,
                "desktop_count": serde_json::Value::Null,
                "never_focus_count": serde_json::Value::Null,
                "skip_taskbar_count": serde_json::Value::Null,
                "skip_pager_count": serde_json::Value::Null,
                "show_bar": serde_json::Value::Null,
                "bar_visible": serde_json::Value::Null,
                "has_visible_fullscreen": serde_json::Value::Null,
                "owns_output_count": serde_json::Value::Null,
            });
        };
        let tag = mon.pertag.as_ref().map(|p| p.cur_tag);
        let prev_tag = mon.pertag.as_ref().map(|p| p.prev_tag);
        let selected_id = mon
            .sel
            .and_then(|ck| self.state.clients.get(ck).map(|c| c.win.raw()));
        let show_bar = mon
            .pertag
            .as_ref()
            .and_then(|p| p.show_bars.get(p.cur_tag).copied())
            .unwrap_or(true);
        let mut value = serde_json::json!({
            "monitor": mon.num,
            "tag": tag,
            "layout": mon.lt_symbol,
            "gap": mon.layout.gap,
            "mfact": mon.layout.m_fact,
            "nmaster": mon.layout.n_master,
            "prev_tag": prev_tag,
            "selected_id": selected_id,
            "sel_tags": mon.sel_tags & 1,
            "previous_tags": mon.tag_set[1 - (mon.sel_tags & 1)],
            "active_tags": mon.tag_set[mon.sel_tags & 1],
            "window_count": self
                .state
                .monitor_clients
                .get(mk)
                .map(|keys| keys.len())
                .unwrap_or(0),
            "on_view_count": self
                .state
                .monitor_clients
                .get(mk)
                .map(|keys| {
                    keys.iter()
                        .filter(|&&ck| self.is_client_visible_on_monitor(ck, mk))
                        .count()
                })
                .unwrap_or(0),
            "floating_count": self
                .state
                .monitor_clients
                .get(mk)
                .map(|keys| {
                    keys.iter()
                        .filter(|&&ck| {
                            self.state
                                .clients
                                .get(ck)
                                .is_some_and(|c| c.state.is_floating)
                        })
                        .count()
                })
                .unwrap_or(0),
            "minimized_count": self
                .state
                .monitor_clients
                .get(mk)
                .map(|keys| {
                    keys.iter()
                        .filter(|&&ck| {
                            self.state
                                .clients
                                .get(ck)
                                .is_some_and(|c| c.state.is_hidden)
                        })
                        .count()
                })
                .unwrap_or(0),
            "sticky_count": self
                .state
                .monitor_clients
                .get(mk)
                .map(|keys| {
                    keys.iter()
                        .filter(|&&ck| {
                            self.state
                                .clients
                                .get(ck)
                                .is_some_and(|c| c.state.is_sticky)
                        })
                        .count()
                })
                .unwrap_or(0),
            "urgent_count": self
                .state
                .monitor_clients
                .get(mk)
                .map(|keys| {
                    keys.iter()
                        .filter(|&&ck| {
                            self.state.clients.get(ck).is_some_and(|c| {
                                c.state.is_urgent || c.state.demands_attention
                            })
                        })
                        .count()
                })
                .unwrap_or(0),
            "fullscreen_count": self
                .state
                .monitor_clients
                .get(mk)
                .map(|keys| {
                    keys.iter()
                        .filter(|&&ck| {
                            self.state
                                .clients
                                .get(ck)
                                .is_some_and(|c| c.state.is_fullscreen)
                        })
                        .count()
                })
                .unwrap_or(0),
            "pip_count": self
                .state
                .monitor_clients
                .get(mk)
                .map(|keys| {
                    keys.iter()
                        .filter(|&&ck| {
                            self.state
                                .clients
                                .get(ck)
                                .is_some_and(|c| c.state.is_pip)
                        })
                        .count()
                })
                .unwrap_or(0),
            "maximized_count": self
                .state
                .monitor_clients
                .get(mk)
                .map(|keys| {
                    keys.iter()
                        .filter(|&&ck| {
                            self.state.clients.get(ck).is_some_and(|c| {
                                c.state.is_maximized_vert || c.state.is_maximized_horz
                            })
                        })
                        .count()
                })
                .unwrap_or(0),
            "above_count": self
                .state
                .monitor_clients
                .get(mk)
                .map(|keys| {
                    keys.iter()
                        .filter(|&&ck| {
                            self.state
                                .clients
                                .get(ck)
                                .is_some_and(|c| c.state.is_above)
                        })
                        .count()
                })
                .unwrap_or(0),
            "below_count": self
                .state
                .monitor_clients
                .get(mk)
                .map(|keys| {
                    keys.iter()
                        .filter(|&&ck| {
                            self.state
                                .clients
                                .get(ck)
                                .is_some_and(|c| c.state.is_below)
                        })
                        .count()
                })
                .unwrap_or(0),
            "scratchpad_count": self
                .state
                .monitor_clients
                .get(mk)
                .map(|keys| {
                    keys.iter()
                        .filter(|&&ck| self.scratchpads.values().any(|&pad| pad == ck))
                        .count()
                })
                .unwrap_or(0),
            "tabbed_count": self.tab_group_clients(mk).len(),
            "dock_count": self
                .state
                .monitor_clients
                .get(mk)
                .map(|keys| {
                    keys.iter()
                        .filter(|&&ck| {
                            self.state
                                .clients
                                .get(ck)
                                .is_some_and(|c| c.state.is_dock)
                        })
                        .count()
                })
                .unwrap_or(0),
            "desktop_count": self
                .state
                .monitor_clients
                .get(mk)
                .map(|keys| {
                    keys.iter()
                        .filter(|&&ck| {
                            self.state
                                .clients
                                .get(ck)
                                .is_some_and(|c| c.state.is_desktop)
                        })
                        .count()
                })
                .unwrap_or(0),
            "never_focus_count": self
                .state
                .monitor_clients
                .get(mk)
                .map(|keys| {
                    keys.iter()
                        .filter(|&&ck| {
                            self.state
                                .clients
                                .get(ck)
                                .is_some_and(|c| c.state.never_focus)
                        })
                        .count()
                })
                .unwrap_or(0),
            "skip_taskbar_count": self
                .state
                .monitor_clients
                .get(mk)
                .map(|keys| {
                    keys.iter()
                        .filter(|&&ck| {
                            self.state
                                .clients
                                .get(ck)
                                .is_some_and(|c| c.state.skip_taskbar)
                        })
                        .count()
                })
                .unwrap_or(0),
            "skip_pager_count": self
                .state
                .monitor_clients
                .get(mk)
                .map(|keys| {
                    keys.iter()
                        .filter(|&&ck| {
                            self.state
                                .clients
                                .get(ck)
                                .is_some_and(|c| c.state.skip_pager)
                        })
                        .count()
                })
                .unwrap_or(0),
            "show_bar": show_bar,
            "bar_visible": self.monitor_shows_status_bar(mk),
            "has_visible_fullscreen": self.monitor_has_visible_fullscreen(mk),
            "owns_output_count": self.monitor_owns_output_count(mk),
        });
        if let Some(connector) = self.output_key_for_monitor(backend, mk) {
            value
                .as_object_mut()
                .expect("show_bar snapshot object")
                .insert("connector".into(), serde_json::Value::String(connector));
        }
        value
    }

    /// Focused monitor's status-bar preference and whether the bar window
    /// currently occupies the output (`bar_visible` is false during F11
    /// fullscreen even when `show_bar` stays true).
    pub(crate) fn query_focused_show_bar(&self, backend: &dyn Backend) -> serde_json::Value {
        match self.state.sel_mon {
            Some(mk) => self.query_show_bar_for_monitor(backend, mk),
            None => serde_json::json!({
                "monitor": serde_json::Value::Null,
                "tag": serde_json::Value::Null,
                "layout": serde_json::Value::Null,
                "gap": serde_json::Value::Null,
                "mfact": serde_json::Value::Null,
                "nmaster": serde_json::Value::Null,
                "prev_tag": serde_json::Value::Null,
                "selected_id": serde_json::Value::Null,
                "sel_tags": serde_json::Value::Null,
                "previous_tags": serde_json::Value::Null,
                "active_tags": serde_json::Value::Null,
                "window_count": serde_json::Value::Null,
                "on_view_count": serde_json::Value::Null,
                "floating_count": serde_json::Value::Null,
                "minimized_count": serde_json::Value::Null,
                "sticky_count": serde_json::Value::Null,
                "urgent_count": serde_json::Value::Null,
                "fullscreen_count": serde_json::Value::Null,
                "pip_count": serde_json::Value::Null,
                "maximized_count": serde_json::Value::Null,
                "above_count": serde_json::Value::Null,
                "below_count": serde_json::Value::Null,
                "scratchpad_count": serde_json::Value::Null,
                "tabbed_count": serde_json::Value::Null,
                "dock_count": serde_json::Value::Null,
                "desktop_count": serde_json::Value::Null,
                "never_focus_count": serde_json::Value::Null,
                "skip_taskbar_count": serde_json::Value::Null,
                "skip_pager_count": serde_json::Value::Null,
                "show_bar": serde_json::Value::Null,
                "bar_visible": serde_json::Value::Null,
                "has_visible_fullscreen": serde_json::Value::Null,
                "owns_output_count": serde_json::Value::Null,
            }),
        }
    }

    /// Focused monitor's previous layout symbol.
    pub(crate) fn query_focused_prev_layout(&self, backend: &dyn Backend) -> serde_json::Value {
        match self.state.sel_mon.and_then(|mk| {
            let mon = self.state.monitors.get(mk)?;
            Some((mk, mon.num, format!("{:?}", *mon.prev_lt)))
        }) {
            Some((mk, num, prev_layout)) => {
                let mut value = serde_json::json!({
                    "monitor": num,
                    "prev_layout": prev_layout,
                });
                if let Some(connector) = self.output_key_for_monitor(backend, mk) {
                    value
                        .as_object_mut()
                        .expect("prev_layout snapshot object")
                        .insert("connector".into(), serde_json::Value::String(connector));
                }
                value
            }
            None => serde_json::json!({
                "monitor": serde_json::Value::Null,
                "prev_layout": serde_json::Value::Null,
            }),
        }
    }

    /// Focused / selected window id (and optional full [`WindowInfo`]).
    pub(crate) fn query_selected_window(
        &self,
        backend: &dyn Backend,
        full: bool,
    ) -> serde_json::Value {
        let Some(ck) = self.get_selected_client_key() else {
            return serde_json::json!({
                "id": serde_json::Value::Null,
            });
        };
        if full {
            if let Some(info) = self.window_info(backend, ck, true) {
                return serde_json::to_value(info)
                    .unwrap_or_else(|_| serde_json::json!({ "id": serde_json::Value::Null }));
            }
        }
        let id = self.state.clients.get(ck).map(|c| c.win.raw()).unwrap_or(0);
        serde_json::json!({ "id": id })
    }

    /// Compact notification nest for `get_status` (count / center / DND).
    fn notifications_status_summary(&self) -> serde_json::Value {
        let full = self.notifications_json();
        serde_json::json!({
            "count": full.get("count").cloned().unwrap_or(serde_json::json!(0)),
            "center_open": full.get("center_open").cloned().unwrap_or(serde_json::json!(false)),
            "do_not_disturb": full.get("do_not_disturb").cloned().unwrap_or(serde_json::json!(false)),
        })
    }

    /// Compact blur nest for `get_status`.
    fn blur_status_summary(&self, backend: &dyn Backend) -> serde_json::Value {
        let cfg = CONFIG.load();
        match backend.compositor_blur_status() {
            Some(b) => serde_json::json!({
                "config_enabled": cfg.behavior().blur_enabled,
                "current_strength": b.current_strength,
                "temporal_enabled": b.temporal_enabled,
                "status_bar_frosted": b.status_bar_frosted,
            }),
            None => serde_json::json!({
                "config_enabled": cfg.behavior().blur_enabled,
                "current_strength": serde_json::Value::Null,
                "temporal_enabled": false,
                "status_bar_frosted": false,
            }),
        }
    }

    /// Compact HDR nest for `get_status`.
    fn hdr_status_summary(&self, backend: &dyn Backend) -> serde_json::Value {
        let cfg = CONFIG.load();
        let outputs = backend.output_ops().enumerate_outputs();
        let capable = outputs.iter().filter(|o| o.hdr_capable).count();
        serde_json::json!({
            "config_enabled": cfg.behavior().hdr_enabled,
            "config_peak_nits": cfg.behavior().hdr_peak_nits,
            "outputs_total": outputs.len(),
            "outputs_capable": capable,
        })
    }

    /// Compact capture nest for `get_status`.
    fn capture_status_summary(&self, backend: &dyn Backend) -> serde_json::Value {
        match backend.compositor_capture_status() {
            Some(status) => serde_json::json!({
                "screencopy_enabled": status.screencopy.enabled,
                "screencopy_pending_frames": status.screencopy.pending_frames,
                "image_copy_capture_enabled": status.image_copy_capture.enabled,
                "image_copy_capture_pending_frames": status.image_copy_capture.pending_frames,
                "dmabuf_advertised": status.dmabuf_advertised,
            }),
            None => serde_json::json!({
                "screencopy_enabled": false,
                "screencopy_pending_frames": 0,
                "image_copy_capture_enabled": false,
                "image_copy_capture_pending_frames": 0,
                "dmabuf_advertised": false,
            }),
        }
    }

    /// Compact idle nest for `get_status`.
    fn idle_status_summary(&self, backend: &dyn Backend) -> serde_json::Value {
        let full = self.idle_status_json(backend);
        serde_json::json!({
            "inhibited": full.get("inhibited").cloned().unwrap_or(serde_json::json!(false)),
            "caffeine": full.get("caffeine").cloned().unwrap_or(serde_json::json!(false)),
            "dimmed": full.get("dimmed").cloned().unwrap_or(serde_json::json!(false)),
            "screen_off": full.get("screen_off").cloned().unwrap_or(serde_json::json!(false)),
            "locked": full.get("locked").cloned().unwrap_or(serde_json::json!(false)),
            "idle_for": full.get("idle_for").cloned().unwrap_or(serde_json::json!(0)),
        })
    }

    /// Compact recording nest for `get_status`.
    fn recording_status_summary(&self, backend: &dyn Backend) -> serde_json::Value {
        let capture_stats = backend.compositor_recording_stats();
        let elapsed_secs = capture_stats
            .as_ref()
            .map(|stats| (stats.elapsed_secs * 10.0).round() / 10.0);
        serde_json::json!({
            "active": self.features.recording.active,
            "selecting_region": self.features.recording.selecting_region,
            "elapsed_secs": elapsed_secs,
            "capture_target": self.features.capture.recording.label(),
            "last_error": self.features.recording.last_error,
            "segment_count": self.features.recording.segment_count(),
        })
    }

    /// Compact audio-recording nest for `get_status`.
    fn audio_recording_status_summary(&self) -> serde_json::Value {
        let recording = &self.features.audio_recording;
        serde_json::json!({
            "active": recording.active,
            "finalizing": recording.is_finalizing(),
            "elapsed_ms": u64::try_from(recording.elapsed().as_millis()).unwrap_or(u64::MAX),
            "last_error": recording.last_error,
        })
    }

    /// Compact clipboard nest for `get_status`.
    fn clipboard_status_summary(&self) -> serde_json::Value {
        let full = self.clipboard_json();
        serde_json::json!({
            "enabled": full.get("enabled").cloned().unwrap_or(serde_json::json!(false)),
            "count": full.get("count").cloned().unwrap_or(serde_json::json!(0)),
            "capacity": full.get("capacity").cloned().unwrap_or(serde_json::json!(0)),
        })
    }

    fn waterlily_status_summary(&self, backend: &dyn Backend) -> serde_json::Value {
        match backend.compositor_waterlily_status() {
            Some(status) => serde_json::json!({
                "enabled": status.enabled,
                "active": status.active,
                "worker_connected": status.worker_connected,
                "requested_case": status.requested_case,
                "requested_palette": status.requested_palette,
            }),
            None => serde_json::json!({
                "enabled": false,
                "active": false,
                "worker_connected": false,
                "requested_case": serde_json::Value::Null,
                "requested_palette": serde_json::Value::Null,
            }),
        }
    }

    fn night_light_status_summary(&self) -> serde_json::Value {
        let full = self.query_night_light();
        serde_json::json!({
            "active": full.get("active").cloned().unwrap_or(serde_json::json!(false)),
            "override": full.get("override").cloned().unwrap_or(serde_json::Value::Null),
            "temp": full.get("temp").cloned().unwrap_or(serde_json::Value::Null),
        })
    }

    fn magnifier_status_summary(&self) -> serde_json::Value {
        serde_json::json!({
            "enabled": self.features.magnifier.enabled,
            "zoom": self.features.magnifier.zoom_level,
            "radius": self.features.magnifier.radius,
        })
    }

    fn peek_status_summary(&self, backend: &dyn Backend) -> serde_json::Value {
        serde_json::json!({
            "active": self.features.peek_active,
            "compositor_active": backend.has_compositor(),
        })
    }

    fn expose_status_summary(&self) -> serde_json::Value {
        serde_json::json!({
            "active": self.features.expose_active,
        })
    }

    fn gesture_status_summary(&self) -> serde_json::Value {
        self.query_gesture_status()
    }

    fn wayland_status_summary(&self, backend: &dyn Backend) -> serde_json::Value {
        let full = self.query_wayland_status(backend);
        serde_json::json!({
            "backend_family": full.get("backend_family").cloned().unwrap_or(serde_json::Value::Null),
            "outputs": full.get("outputs").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0),
        })
    }

    fn session_lock_status_summary(&self, backend: &dyn Backend) -> serde_json::Value {
        serde_json::json!({
            "locked": backend.compositor_session_locked(),
            "lock_surface_count": backend.compositor_session_lock_surface_count(),
        })
    }

    fn tearing_status_summary(&self, backend: &dyn Backend) -> serde_json::Value {
        let presentation = backend.compositor_presentation_statuses();
        serde_json::json!({
            "active_surface_count": backend.compositor_tearing_hint_count(),
            "tearing_outputs": presentation.iter().filter(|o| o.tearing).count(),
            "outputs": presentation.len(),
        })
    }

    fn xwayland_status_summary(&self, backend: &dyn Backend) -> serde_json::Value {
        match backend.compositor_xwayland_status() {
            Some(status) => serde_json::to_value(status)
                .unwrap_or_else(|_| serde_json::json!({ "available": true })),
            None => serde_json::json!({ "available": false }),
        }
    }

    fn scrolling_status_summary(&self) -> serde_json::Value {
        let full = self.query_scrolling_status();
        serde_json::json!({
            "active_monitor_count": full
                .get("active_monitor_count")
                .cloned()
                .unwrap_or(serde_json::json!(0)),
            "monitors": full
                .get("monitors")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0),
        })
    }

    fn color_management_status_summary(&self, backend: &dyn Backend) -> serde_json::Value {
        let surfaces = backend.compositor_color_managed_surfaces();
        serde_json::json!({
            "surface_count": surfaces.len(),
            "hdr_surface_count": surfaces
                .iter()
                .filter(|s| color_surface_is_hdr(s))
                .count(),
        })
    }

    fn audio_status_summary(&self) -> serde_json::Value {
        let full = self.audio_devices_json();
        serde_json::json!({
            "output_count": full
                .get("output")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0),
            "input_count": full
                .get("input")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0),
            "pending": full.get("pending").cloned().unwrap_or(serde_json::json!(false)),
        })
    }

    fn wallpaper_status_summary(&self) -> serde_json::Value {
        let full = self.wallpaper_theme_json();
        serde_json::json!({
            "enabled": full.get("enabled").cloned().unwrap_or(serde_json::json!(false)),
            "wallpaper": full.get("wallpaper").cloned().unwrap_or(serde_json::Value::Null),
            "pending": full.get("pending").cloned().unwrap_or(serde_json::json!(false)),
        })
    }

    fn bluetooth_status_summary(&self) -> serde_json::Value {
        crate::jwm::features::pairing::session_json(self.features.bluetooth_pairing.as_ref())
    }

    fn system_ui_status_summary(&self) -> serde_json::Value {
        serde_json::json!({
            "active": self.features.system_ui.is_active(),
            "kind": self.features.system_ui.panel_kind(),
        })
    }

    fn layout_status_summary(&self, backend: &dyn Backend) -> serde_json::Value {
        let full = self.query_focused_layout(backend);
        serde_json::json!({
            "layout": full.get("layout").cloned().unwrap_or(serde_json::Value::Null),
            "monitor": full.get("monitor").cloned().unwrap_or(serde_json::Value::Null),
        })
    }

    fn tabs_status_summary(&self, backend: &dyn Backend) -> serde_json::Value {
        let full = self.query_focused_tab_bar(backend);
        serde_json::json!({
            "monitor": full.get("monitor").cloned().unwrap_or(serde_json::Value::Null),
            "reserved": full.get("reserved").cloned().unwrap_or(serde_json::json!(0)),
            "window_count": full
                .get("windows")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0),
            "selected_id": full.get("selected_id").cloned().unwrap_or(serde_json::Value::Null),
        })
    }

    fn struts_status_summary(&self, backend: &dyn Backend) -> serde_json::Value {
        let full = self.query_struts(backend);
        serde_json::json!({
            "monitor_count": full
                .as_array()
                .map(|a| a.len())
                .or_else(|| full.get("monitors").and_then(|v| v.as_array()).map(|a| a.len()))
                .unwrap_or(0),
        })
    }

    fn scratchpads_status_summary(&self) -> serde_json::Value {
        let full = self.query_scratchpads();
        let count = full.as_object().map(|o| o.len()).unwrap_or(0);
        serde_json::json!({ "count": count })
    }

    fn monitors_status_summary(&self) -> serde_json::Value {
        let focused = self
            .state
            .sel_mon
            .filter(|key| self.state.monitor_order.contains(key))
            .and_then(|key| self.state.monitors.get(key).map(|monitor| monitor.num));
        serde_json::json!({
            "count": self.ipc_monitor_count(),
            "focused": focused,
        })
    }

    fn workspaces_status_summary(&self) -> serde_json::Value {
        let tag_count = CONFIG.load().tags_length();
        let monitor_count = self.ipc_monitor_count();
        let focused_count = self
            .state
            .sel_mon
            .and_then(|key| self.state.monitors.get(key))
            .map_or(0, |monitor| {
                active_tag_count(monitor.get_active_tags(), tag_count) as usize
                    * self
                        .state
                        .monitor_order
                        .iter()
                        .filter(|&&key| Some(key) == self.state.sel_mon)
                        .count()
            });
        serde_json::json!({
            "count": monitor_count * tag_count,
            "focused_count": focused_count,
        })
    }

    fn windows_status_summary(&self) -> serde_json::Value {
        let selected = self
            .get_selected_client_key()
            .filter(|key| self.state.client_order.contains(key));
        serde_json::json!({
            "count": self.ipc_window_count(),
            "focused_id": selected.and_then(|key| self.state.clients.get(key)).map(|client| client.win.raw()),
        })
    }

    fn tree_status_summary(&self) -> serde_json::Value {
        let monitor_count = self.ipc_monitor_count();
        let window_count = self
            .state
            .monitor_order
            .iter()
            .filter(|key| self.state.monitors.contains_key(**key))
            .filter_map(|key| self.state.monitor_clients.get(*key))
            .flatten()
            .filter(|key| self.state.clients.contains_key(**key))
            .count();
        serde_json::json!({
            "monitor_count": monitor_count,
            "window_count": window_count,
        })
    }

    fn ipc_monitor_count(&self) -> usize {
        self.state
            .monitor_order
            .iter()
            .filter(|key| self.state.monitors.contains_key(**key))
            .count()
    }

    fn ipc_window_count(&self) -> usize {
        self.state
            .client_order
            .iter()
            .filter(|key| self.state.clients.contains_key(**key))
            .count()
    }

    fn effects_status_summary(&self) -> serde_json::Value {
        serde_json::json!({
            "overview": self.features.overview.active,
            "expose": self.features.expose_active,
            "magnifier": self.features.magnifier.enabled,
            "peek": self.features.peek_active,
            "annotation": self.features.annotation_active,
            "layout_picker": self.features.system_ui.is_layout_picker(),
            "debug_hud": self.debug_hud_on,
        })
    }

    fn mic_status_summary(&self) -> serde_json::Value {
        serde_json::json!({
            "muted": self
                .features
                .control_snapshot
                .as_ref()
                .and_then(|s| s.mic_muted)
                .unwrap_or(false),
        })
    }

    fn bench_status_summary(&self, backend: &dyn Backend) -> serde_json::Value {
        match backend.compositor_benchmark_report() {
            Some(report) => serde_json::json!({
                "ready": true,
                "report": serde_json::from_str::<serde_json::Value>(&report).unwrap_or_default(),
            }),
            None => serde_json::json!({ "ready": false }),
        }
    }

    #[cfg(test)]
    fn windows_flag_status_summary(&self, flag: &str) -> serde_json::Value {
        self.window_flag_status_summary(self.window_flag_counts(), flag)
    }

    fn window_flag_counts(&self) -> WindowFlagCounts {
        let focused = self
            .get_selected_client_key()
            .filter(|key| self.state.client_order.contains(key));
        let mut counts = WindowFlagCounts {
            focused,
            ..WindowFlagCounts::default()
        };
        for client in self
            .state
            .client_order
            .iter()
            .filter_map(|key| self.state.clients.get(*key))
        {
            counts.floating += usize::from(client.state.is_floating);
            counts.minimized += usize::from(client.state.is_hidden);
            counts.sticky += usize::from(client.state.is_sticky);
            counts.urgent += usize::from(client.state.is_urgent || client.state.demands_attention);
            counts.fullscreen += usize::from(client.state.is_fullscreen);
            counts.pip += usize::from(client.state.is_pip);
        }
        counts
    }

    fn window_flag_status_summary(
        &self,
        counts: WindowFlagCounts,
        flag: &str,
    ) -> serde_json::Value {
        let matches = |client: &WMClient| match flag {
            "floating" => client.state.is_floating,
            "minimized" => client.state.is_hidden,
            "sticky" => client.state.is_sticky,
            "urgent" => client.state.is_urgent || client.state.demands_attention,
            "fullscreen" => client.state.is_fullscreen,
            "pip" => client.state.is_pip,
            _ => false,
        };
        let count = match flag {
            "floating" => counts.floating,
            "minimized" => counts.minimized,
            "sticky" => counts.sticky,
            "urgent" => counts.urgent,
            "fullscreen" => counts.fullscreen,
            "pip" => counts.pip,
            _ => 0,
        };
        serde_json::json!({
            "count": count,
            "focused_id": counts.focused.and_then(|key| self.state.clients.get(key)).filter(|client| matches(client)).map(|client| client.win.raw()),
        })
    }

    fn query_runtime_status_metrics(&self, backend: &dyn Backend) -> Option<serde_json::Value> {
        backend
            .compositor_get_metrics()
            .and_then(|metrics| serde_json::to_value(metrics).ok())
            .map(|metrics| {
                serde_json::json!({
                    "available": true,
                    "metrics": metrics,
                })
            })
            .or_else(|| Some(serde_json::json!({ "available": false })))
    }

    /// Named scratchpads currently bound to a managed window (`name` → id).
    pub(crate) fn query_scratchpads(&self) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        for (name, &client_key) in &self.scratchpads {
            if let Some(client) = self.state.clients.get(client_key) {
                map.insert(name.clone(), serde_json::json!(client.win.raw()));
            }
        }
        serde_json::Value::Object(map)
    }

    /// Per-monitor strut reservations plus the external windows that contribute.
    pub(crate) fn query_struts(&self, backend: &dyn Backend) -> serde_json::Value {
        let mut out = Vec::new();
        for &mk in &self.state.monitor_order {
            let Some(mon) = self.state.monitors.get(mk) else {
                continue;
            };
            let (top, bottom, left, right) = self.get_strut_reserved(mk);
            let windows: Vec<u64> = self
                .external_struts
                .iter()
                .filter(|(_, (_, host))| host.is_none_or(|host| host == mk))
                .map(|(win, _)| win.raw())
                .collect();
            let mut value = serde_json::json!({
                "monitor": mon.num,
                "top": top,
                "bottom": bottom,
                "left": left,
                "right": right,
                "windows": windows,
            });
            if let Some(connector) = self.output_key_for_monitor(backend, mk) {
                value
                    .as_object_mut()
                    .expect("struts snapshot object")
                    .insert("connector".into(), serde_json::Value::String(connector));
            }
            out.push(value);
        }
        serde_json::Value::Array(out)
    }

    /// Night light schedule / override snapshot for bars and OSD consumers.
    pub(crate) fn query_night_light(&self) -> serde_json::Value {
        let cfg = CONFIG.load();
        serde_json::json!({
            "active": self.night_light_active(),
            "override": self.night_light_override,
            "temp": cfg.behavior().night_light_temp,
        })
    }

    /// Single-window filter over [`Self::query_windows`] (`args.id`).
    pub(crate) fn query_window(
        &self,
        backend: &dyn Backend,
        args: &serde_json::Value,
    ) -> IpcResponse {
        let id = args
            .get("id")
            .or_else(|| args.get("value"))
            .or_else(|| args.get("v"))
            .and_then(|value| value.as_u64())
            .or_else(|| args.as_u64());
        let Some(id) = id else {
            return IpcResponse::err("get_window requires an \"id\" argument (window id as u64)");
        };
        match self
            .query_windows(backend)
            .into_iter()
            .find(|window| window.id == id)
        {
            Some(window) => IpcResponse::ok(Some(serde_json::to_value(window).unwrap_or_default())),
            None => IpcResponse::err(format!("window {id:#x} not found")),
        }
    }

    fn focused_layout_snapshot(&self, backend: &dyn Backend) -> Option<serde_json::Value> {
        let mk = self.state.sel_mon?;
        let mon = self.state.monitors.get(mk)?;
        let mut value = serde_json::json!({
            "monitor": mon.num,
            "layout": format!("{:?}", *mon.lt),
            "m_fact": mon.layout.m_fact,
            "n_master": mon.layout.n_master,
            "gap": mon.layout.gap,
        });
        if let Some(connector) = self.output_key_for_monitor(backend, mk) {
            value
                .as_object_mut()
                .expect("layout snapshot object")
                .insert("connector".into(), serde_json::Value::String(connector));
        }
        Some(value)
    }

    pub(crate) fn query_scrolling_status(&self) -> serde_json::Value {
        let cfg = CONFIG.load();
        let monitors = self
            .state
            .monitor_order
            .iter()
            .filter_map(|&mk| {
                let mon = self.state.monitors.get(mk)?;
                let layout = &*mon.lt;
                let active_tags = mon.get_active_tags();
                let state_key = self.scrolling_state_key(mk);
                let state = self.scrolling_state_for_monitor(mk);
                let visible_clients = self
                    .state
                    .monitor_clients
                    .get(mk)
                    .map(|clients| {
                        clients
                            .iter()
                            .copied()
                            .filter(|&ck| self.is_client_visible_by_key(ck))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let columns = state
                    .map(|s| {
                        s.columns
                            .iter()
                            .enumerate()
                            .map(|(idx, column)| {
                                let windows = column
                                    .iter()
                                    .filter_map(|key| {
                                        self.state.clients.get(*key).map(|client| {
                                            serde_json::json!({
                                                "id": client.win.raw(),
                                                "name": client.name,
                                                "class": client.class,
                                                "focused": mon.sel == Some(*key),
                                            })
                                        })
                                    })
                                    .collect::<Vec<_>>();
                                let focused_window = s
                                    .focused_clients
                                    .get(idx)
                                    .copied()
                                    .flatten()
                                    .and_then(|key| self.state.clients.get(key))
                                    .map(|client| client.win.raw());
                                serde_json::json!({
                                    "index": idx,
                                    "width_factor": s.column_width_factors.get(idx).copied().unwrap_or(1.0),
                                    "focused_window": focused_window,
                                    "windows": windows,
                                })
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let overview_order = state
                    .map(|s| s.ordered_visible_clients(&visible_clients))
                    .unwrap_or_else(|| visible_clients.clone())
                    .into_iter()
                    .filter_map(|key| self.state.clients.get(key).map(|client| client.win.raw()))
                    .collect::<Vec<_>>();
                let overview_strip = state.map(|s| {
                    let focused_column = s.focused_column_index();
                    let weights = s
                        .columns
                        .iter()
                        .enumerate()
                        .map(|(idx, _)| {
                            s.column_width_factors
                                .get(idx)
                                .copied()
                                .unwrap_or(1.0)
                                .max(0.1)
                        })
                        .collect::<Vec<_>>();
                    let total_weight = weights.iter().sum::<f32>().max(0.1);
                    let mut cursor = 0.0f32;
                    let strip_columns = s
                        .columns
                        .iter()
                        .enumerate()
                        .map(|(idx, column)| {
                            let width = weights.get(idx).copied().unwrap_or(1.0) / total_weight;
                            let x = cursor / total_weight;
                            cursor += weights.get(idx).copied().unwrap_or(1.0);
                            let windows = column
                                .iter()
                                .filter_map(|key| {
                                    self.state.clients.get(*key).map(|client| {
                                        serde_json::json!({
                                            "id": client.win.raw(),
                                            "focused": mon.sel == Some(*key),
                                        })
                                    })
                                })
                                .collect::<Vec<_>>();
                            serde_json::json!({
                                "index": idx,
                                "x_ratio": x,
                                "width_ratio": width,
                                "focused": focused_column == Some(idx),
                                "window_count": windows.len(),
                                "windows": windows,
                            })
                        })
                        .collect::<Vec<_>>();

                    serde_json::json!({
                        "visible": !s.columns.is_empty(),
                        "column_count": s.columns.len(),
                        "focused_column": focused_column,
                        "viewport_x": s.viewport_x,
                        "columns": strip_columns,
                        "overview_order": overview_order,
                    })
                });

                let focused_window = mon
                    .sel
                    .and_then(|key| self.state.clients.get(key))
                    .map(|client| client.win.raw());

                Some(serde_json::json!({
                    "monitor": mon.num,
                    "focused_monitor": self.state.sel_mon == Some(mk),
                    "layout": format!("{layout:?}"),
                    "active": *layout == LayoutEnum::SCROLLING,
                    "active_tags": active_tags,
                    "state_key": state_key.map(|(_, tag_mask)| serde_json::json!({
                        "monitor": mon.num,
                        "tag_mask": tag_mask,
                    })),
                    "viewport_x": state.map(|s| s.viewport_x).unwrap_or(0.0),
                    "focused_column": state.and_then(|s| s.focused_column_index()),
                    "focused_window": focused_window,
                    "attach_new_windows_to_focused_column": state
                        .map(|s| s.attach_new_windows_to_focused_column)
                        .unwrap_or(false),
                    "column_count": state.map(|s| s.columns.len()).unwrap_or(0),
                    "overview_strip": overview_strip,
                    "columns": columns,
                }))
            })
            .collect::<Vec<_>>();

        let active_monitor_count = monitors
            .iter()
            .filter(|monitor| {
                monitor
                    .get("active")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false)
            })
            .count();

        let mut stored_states = self
            .scrolling_states
            .iter()
            .map(|((mk, tag_mask), state)| {
                let monitor_num = self.state.monitors.get(*mk).map(|mon| mon.num);
                let focused_window = state
                    .focused_column_index()
                    .and_then(|idx| state.target_for_column(idx))
                    .and_then(|key| self.state.clients.get(key))
                    .map(|client| client.win.raw());
                serde_json::json!({
                    "monitor": monitor_num,
                    "tag_mask": tag_mask,
                    "column_count": state.columns.len(),
                    "focused_column": state.focused_column_index(),
                    "focused_window": focused_window,
                    "viewport_x": state.viewport_x,
                    "attach_new_windows_to_focused_column": state.attach_new_windows_to_focused_column,
                })
            })
            .collect::<Vec<_>>();
        stored_states.sort_by_key(|state| {
            (
                state
                    .get("monitor")
                    .and_then(|value| value.as_i64())
                    .unwrap_or(i64::MAX),
                state
                    .get("tag_mask")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0),
            )
        });

        serde_json::json!({
            "active_monitor_count": active_monitor_count,
            "stored_state_count": self.scrolling_states.len(),
            "column_width_rule_count": cfg.behavior().scrolling_column_width_rules.len(),
            "column_width_rules": cfg.behavior().scrolling_column_width_rules.clone(),
            "stored_states": stored_states,
            "monitors": monitors,
        })
    }

    pub(crate) fn query_gesture_status(&self) -> serde_json::Value {
        let cfg = CONFIG.load();
        let bindings = &cfg.behavior().gesture_swipe;
        let mut intercepted_fingers = bindings
            .iter()
            .filter(|binding| binding.fingers >= 3)
            .map(|binding| binding.fingers)
            .collect::<Vec<_>>();
        intercepted_fingers.sort_unstable();
        intercepted_fingers.dedup();

        let binding_details = bindings
            .iter()
            .map(|binding| {
                let scrolling_related = matches!(
                    binding.function.as_str(),
                    "scrolling_focus_column"
                        | "scrolling_move_column"
                        | "scrolling_focus_window"
                        | "scrolling_consume"
                        | "scrolling_expel"
                        | "scrolling_toggle_attach_mode"
                );
                serde_json::json!({
                    "fingers": binding.fingers,
                    "direction": binding.direction,
                    "function": binding.function,
                    "argument": binding.argument,
                    "will_intercept": binding.fingers >= 3,
                    "scrolling_related": scrolling_related,
                })
            })
            .collect::<Vec<_>>();

        let scrolling_binding_count = binding_details
            .iter()
            .filter(|binding| {
                binding
                    .get("scrolling_related")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
            })
            .count();

        serde_json::json!({
            "swipe_threshold": cfg.behavior().gesture_swipe_threshold,
            "binding_count": bindings.len(),
            "scrolling_binding_count": scrolling_binding_count,
            "intercepted_fingers": intercepted_fingers,
            "recommended_scrolling_swipes": recommended_scrolling_swipes(bindings),
            "bindings": binding_details,
        })
    }

    pub(crate) fn query_config_status(&self) -> serde_json::Value {
        let path = crate::config::Config::resolve_load_path();
        let diagnostics = crate::config::CONFIG.load().diagnostics();
        let modified_unix_ms = crate::config::Config::get_config_modified_time()
            .ok()
            .and_then(system_time_unix_ms);
        serde_json::json!({
            "path": path.display().to_string(),
            "exists": path.exists(),
            "modified_unix_ms": modified_unix_ms,
            "diagnostics": {
                "error_count": diagnostics.error_count(),
                "warning_count": diagnostics.warning_count(),
                "issues": diagnostics.issues(),
            },
            "reload": {
                "attempt_count": self.config_reload_count,
                "last_attempt_unix_ms": self.config_reload_last_unix_ms,
                "last_success": self.config_reload_last_success,
                "last_error": self.config_reload_last_error,
            },
        })
    }

    /// Full `get_config` snapshot, optionally filtered by `args.keys` (an
    /// array of field names). Unknown keys are omitted; an empty / missing
    /// `keys` returns the whole subset.
    pub(crate) fn query_config_subset(&self, args: &serde_json::Value) -> serde_json::Value {
        let cfg = CONFIG.load();
        // Split across two `json!` trees so the macro stays under the crate
        // recursion limit as the key set grows.
        let mut full = serde_json::json!({
            "border_px": cfg.border_px(),
            "gap_px": cfg.gap_px(),
            "snap": cfg.snap(),
            "m_fact": cfg.m_fact(),
            "n_master": cfg.n_master(),
            "tags_length": cfg.tags_length(),
            "show_bar": cfg.show_bar(),
            "do_not_disturb": self.do_not_disturb,
            "screenshot_freeze_enabled": cfg.behavior().screenshot_freeze_enabled,
            "recording_fps": cfg.behavior().recording_fps,
            "recording_encoder": cfg.behavior().recording_encoder,
            "recording_audio_enabled": cfg.behavior().recording_audio_enabled,
            "recording_audio_device": cfg.behavior().recording_audio_device,
            "recording_audio_bitrate": cfg.behavior().recording_audio_bitrate,
            "audio_recording_device": cfg.behavior().audio_recording_device,
            "audio_recording_backend": cfg.behavior().audio_recording_backend,
            "audio_recording_format": cfg.behavior().audio_recording_format,
            "audio_recording_bitrate": cfg.behavior().audio_recording_bitrate,
            "audio_recording_sample_rate": cfg.behavior().audio_recording_sample_rate,
            "audio_recording_channels": cfg.behavior().audio_recording_channels,
            "corner_radius": cfg.behavior().corner_radius,
            "shadow_enabled": cfg.behavior().shadow_enabled,
            "blur_enabled": cfg.behavior().blur_enabled,
            "fading": cfg.behavior().fading,
            "wobbly_windows": cfg.behavior().wobbly_windows,
            "motion_trail": cfg.behavior().motion_trail,
            "overview_enabled": cfg.behavior().overview_enabled,
            "modkey": cfg.modkey(),
            "hdr_enabled": cfg.behavior().hdr_enabled,
            "idle_dim_secs": cfg.behavior().idle_dim_secs,
            "idle_dim_level": cfg.behavior().idle_dim_level,
            "night_light": cfg.behavior().night_light,
            "night_light_temp": cfg.behavior().night_light_temp,
            "night_light_start": cfg.behavior().night_light_start,
            "night_light_end": cfg.behavior().night_light_end,
            "night_light_transition_mins": cfg.behavior().night_light_transition_mins,
            "remember_closed_placement": cfg.behavior().remember_closed_placement,
            // WaterLily is env-driven (no `[behavior]` keys); mirror the
            // compositor's startup read so scripts can filter `get_config`.
            "waterlily_enabled": std::env::var("JWM_WATERLILY_ENABLED")
                .map(|value| value != "0" && !value.eq_ignore_ascii_case("false"))
                .unwrap_or(false),
            "waterlily_opacity": std::env::var("JWM_WATERLILY_OPACITY")
                .ok()
                .and_then(|value| value.parse::<f32>().ok())
                .filter(|value| value.is_finite())
                .unwrap_or(1.0)
                .clamp(0.0, 1.0),
        });
        let extra = serde_json::json!({
            "expose_enabled": cfg.behavior().expose_enabled,
            "peek_enabled": cfg.behavior().peek_enabled,
            "tags_overview_enabled": cfg.behavior().tags_overview_enabled,
            "layout_picker": cfg.behavior().layout_picker,
            "magnifier_enabled": cfg.behavior().magnifier_enabled,
            "magnifier_radius": cfg.behavior().magnifier_radius,
            "magnifier_zoom": cfg.behavior().magnifier_zoom,
            "window_tabs": cfg.behavior().window_tabs,
            "tab_bar_height": cfg.behavior().tab_bar_height,
            "vrr_enabled": cfg.behavior().vrr_enabled,
            "vrr_min_fps": cfg.behavior().vrr_min_fps,
            "vrr_max_fps": cfg.behavior().vrr_max_fps,
            "compositor": cfg.behavior().compositor,
            "swallow_enabled": cfg.behavior().swallow_enabled,
            "idle_lock_secs": cfg.behavior().idle_lock_secs,
            "idle_screen_off_secs": cfg.behavior().idle_screen_off_secs,
            "genie_minimize": cfg.behavior().genie_minimize,
            "focus_highlight": cfg.behavior().focus_highlight,
            "snap_preview": cfg.behavior().snap_preview,
            "blur_strength": cfg.behavior().blur_strength,
            "shadow_radius": cfg.behavior().shadow_radius,
            "inactive_opacity": cfg.behavior().inactive_opacity,
            "active_opacity": cfg.behavior().active_opacity,
            "wallpaper": cfg.behavior().wallpaper,
            "wallpaper_mode": cfg.behavior().wallpaper_mode,
            "persist_tags": cfg.layout_persist_tags(),
            "status_bar_name": cfg.status_bar_name(),
            "status_bar_height": cfg.status_bar_height(),
            "status_bar_padding": cfg.status_bar_padding(),
            "cursor_theme": cfg.cursor_theme(),
            "cursor_size": cfg.cursor_size(),
            "system_ui_font": cfg.system_ui_font(),
            "recording_bitrate": cfg.behavior().recording_bitrate,
            "recording_max_height": cfg.behavior().recording_max_height,
            "recording_output_dir": cfg.behavior().recording_output_dir,
        });
        let polish = serde_json::json!({
            "clipboard_history": cfg.behavior().clipboard_history,
            "border_glow_enabled": cfg.behavior().border_glow_enabled,
            "border_glow_focused_only": cfg.behavior().border_glow_focused_only,
            "border_glow_radius": cfg.behavior().border_glow_radius,
            "border_glow_intensity": cfg.behavior().border_glow_intensity,
            "shadow_inactive_opacity": cfg.behavior().shadow_inactive_opacity,
            "shadow_offset": cfg.behavior().shadow_offset,
            "shadow_color": cfg.behavior().shadow_color,
            "genie_duration_ms": cfg.behavior().genie_duration_ms,
            "focus_highlight_duration_ms": cfg.behavior().focus_highlight_duration_ms,
            "snap_preview_color": cfg.behavior().snap_preview_color,
            "new_client_position": cfg.behavior().new_client_position,
            "drag_threshold_px": cfg.behavior().drag_threshold_px,
            "client_moveresize": cfg.behavior().client_moveresize,
            "resize_hints": cfg.behavior().resize_hints,
            "lock_fullscreen": cfg.behavior().lock_fullscreen,
            "compositor_api": cfg.behavior().compositor_api,
            "resource_rows": cfg.behavior().resource_rows,
            "gesture_swipe_threshold": cfg.behavior().gesture_swipe_threshold,
            "wayland_enable_tearing_control": cfg.behavior().wayland_enable_tearing_control,
            "window_animation": cfg.behavior().window_animation,
            "window_animation_style": cfg.behavior().window_animation_style,
            "attention_animation": cfg.behavior().attention_animation,
        });
        let polish2 = serde_json::json!({
            "focus_follows_new_window": cfg.behavior().focus_follows_new_window,
            "border_enabled": cfg.behavior().border_enabled,
            "border_width": cfg.behavior().border_width,
            "border_color_focused": cfg.behavior().border_color_focused,
            "border_color_unfocused": cfg.behavior().border_color_unfocused,
            "border_glow_color": cfg.behavior().border_glow_color,
            "border_gradient_enabled": cfg.behavior().border_gradient_enabled,
            "border_gradient_color_a": cfg.behavior().border_gradient_color_a,
            "border_gradient_color_b": cfg.behavior().border_gradient_color_b,
            "border_gradient_angle": cfg.behavior().border_gradient_angle,
            "border_gradient_speed": cfg.behavior().border_gradient_speed,
            "debug_hud": cfg.behavior().debug_hud,
            "debug_hud_extended": cfg.behavior().debug_hud_extended,
            "profiling_enabled": cfg.behavior().profiling_enabled,
            "direct_scanout_enabled": cfg.behavior().direct_scanout_enabled,
            "window_animation_scale": cfg.behavior().window_animation_scale,
            "inactive_dim": cfg.behavior().inactive_dim,
            "inactive_desaturate": cfg.behavior().inactive_desaturate,
            "edge_glow": cfg.behavior().edge_glow,
            "edge_glow_color": cfg.behavior().edge_glow_color,
            "edge_glow_width": cfg.behavior().edge_glow_width,
            "attention_color": cfg.behavior().attention_color,
            "pip_border_color": cfg.behavior().pip_border_color,
            "pip_border_width": cfg.behavior().pip_border_width,
            "window_tilt": cfg.behavior().window_tilt,
            "tilt_amount": cfg.behavior().tilt_amount,
            "expose_gap": cfg.behavior().expose_gap,
            "snap_animation_duration_ms": cfg.behavior().snap_animation_duration_ms,
            "motion_trail_frames": cfg.behavior().motion_trail_frames,
            "motion_trail_opacity": cfg.behavior().motion_trail_opacity,
        });
        let polish3 = serde_json::json!({
            "ripple_on_open": cfg.behavior().ripple_on_open,
            "ripple_duration": cfg.behavior().ripple_duration,
            "ripple_amplitude": cfg.behavior().ripple_amplitude,
            "focus_highlight_color": cfg.behavior().focus_highlight_color,
            "wallpaper_crossfade": cfg.behavior().wallpaper_crossfade,
            "wallpaper_crossfade_duration_ms": cfg.behavior().wallpaper_crossfade_duration_ms,
            "wallpaper_dir": cfg.behavior().wallpaper_dir,
            "overview_thumbnail_gap": cfg.behavior().overview_thumbnail_gap,
            "wobbly_stiffness": cfg.behavior().wobbly_stiffness,
            "wobbly_damping": cfg.behavior().wobbly_damping,
            "wobbly_grid_size": cfg.behavior().wobbly_grid_size,
            "gesture_swipe": cfg.behavior().gesture_swipe,
            "hdr_peak_nits": cfg.behavior().hdr_peak_nits,
            "fade_in_step": cfg.behavior().fade_in_step,
            "fade_out_step": cfg.behavior().fade_out_step,
            "blur_status_bar": cfg.behavior().blur_status_bar,
            "blur_temporal_enabled": cfg.behavior().blur_temporal_enabled,
            "wayland_enable_screencopy": cfg.behavior().wayland_enable_screencopy,
            "wayland_enable_color_management": cfg.behavior().wayland_enable_color_management,
            "wayland_enable_output_management": cfg.behavior().wayland_enable_output_management,
            "wayland_enable_gamma_control": cfg.behavior().wayland_enable_gamma_control,
            "suspend_command": cfg.behavior().suspend_command,
            "hibernate_command": cfg.behavior().hibernate_command,
            "reboot_command": cfg.behavior().reboot_command,
            "shutdown_command": cfg.behavior().shutdown_command,
            "idle_screen_off_command": cfg.behavior().idle_screen_off_command,
            "idle_screen_on_command": cfg.behavior().idle_screen_on_command,
            "audio_recording_output_dir": cfg.behavior().audio_recording_output_dir,
            "recording_quality": cfg.behavior().recording_quality,
            "colorblind_mode": cfg.behavior().colorblind_mode,
            "annotation_color": cfg.behavior().annotation_color,
            "annotation_line_width": cfg.behavior().annotation_line_width,
        });
        let polish4 = serde_json::json!({
            "blur_quality_auto": cfg.behavior().blur_quality_auto,
            "blur_temporal_mix_ratio": cfg.behavior().blur_temporal_mix_ratio,
            "detect_client_opacity": cfg.behavior().detect_client_opacity,
            "fullscreen_unredirect": cfg.behavior().fullscreen_unredirect,
            "vsync_method": cfg.behavior().vsync_method,
            "enable_audio_sync": cfg.behavior().enable_audio_sync,
            "audio_buffer_latency_ms": cfg.behavior().audio_buffer_latency_ms,
            "present_enabled": cfg.behavior().present_enabled,
            "wlr_output_mgmt_allow_modeset": cfg.behavior().wlr_output_mgmt_allow_modeset,
            "wayland_enable_output_power": cfg.behavior().wayland_enable_output_power,
            "wayland_enable_workspace": cfg.behavior().wayland_enable_workspace,
            "wayland_enable_image_copy_capture": cfg.behavior().wayland_enable_image_copy_capture,
            "wayland_enable_foreign_toplevel_management": cfg
                .behavior()
                .wayland_enable_foreign_toplevel_management,
            "wayland_enable_virtual_pointer": cfg.behavior().wayland_enable_virtual_pointer,
            "color_temperature": cfg.behavior().color_temperature,
            "saturation": cfg.behavior().saturation,
            "brightness": cfg.behavior().brightness,
            "contrast": cfg.behavior().contrast,
            "invert_colors": cfg.behavior().invert_colors,
            "grayscale": cfg.behavior().grayscale,
            "tone_mapping_method": cfg.behavior().tone_mapping_method,
            "color_management_render_path": cfg.behavior().color_management_render_path,
            "scene_linear_compositing": cfg.behavior().scene_linear_compositing,
            "kms_color_pipeline_offload": cfg.behavior().kms_color_pipeline_offload,
            "gl_state_tracking_enabled": cfg.behavior().gl_state_tracking_enabled,
            "blur_use_frame_extents": cfg.behavior().blur_use_frame_extents,
            "shadow_bottom_extra": cfg.behavior().shadow_bottom_extra,
            "transition_mode": cfg.behavior().transition_mode,
            "tilt_perspective": cfg.behavior().tilt_perspective,
            "tilt_speed": cfg.behavior().tilt_speed,
            "tilt_grid": cfg.behavior().tilt_grid,
            "frosted_glass_strength": cfg.behavior().frosted_glass_strength,
            "wobbly_restore_stiffness": cfg.behavior().wobbly_restore_stiffness,
            "particle_effects": cfg.behavior().particle_effects,
            "particle_count": cfg.behavior().particle_count,
            "particle_lifetime": cfg.behavior().particle_lifetime,
            "particle_gravity": cfg.behavior().particle_gravity,
            "shader_hot_reload": cfg.behavior().shader_hot_reload,
            "shader_dir": cfg.behavior().shader_dir,
            "wallpaper_colors": cfg.behavior().wallpaper_colors,
            "do_not_disturb": cfg.behavior().do_not_disturb,
        });
        if let (Some(base), Some(more)) = (full.as_object_mut(), extra.as_object()) {
            for (key, value) in more {
                base.insert(key.clone(), value.clone());
            }
        }
        if let (Some(base), Some(more)) = (full.as_object_mut(), polish.as_object()) {
            for (key, value) in more {
                base.insert(key.clone(), value.clone());
            }
        }
        if let (Some(base), Some(more)) = (full.as_object_mut(), polish2.as_object()) {
            for (key, value) in more {
                base.insert(key.clone(), value.clone());
            }
        }
        if let (Some(base), Some(more)) = (full.as_object_mut(), polish3.as_object()) {
            for (key, value) in more {
                base.insert(key.clone(), value.clone());
            }
        }
        if let (Some(base), Some(more)) = (full.as_object_mut(), polish4.as_object()) {
            for (key, value) in more {
                base.insert(key.clone(), value.clone());
            }
        }
        let polish5 = serde_json::json!({
            "shadow_exclude_count": cfg.behavior().shadow_exclude.len(),
            "opacity_rules_count": cfg.behavior().opacity_rules.len(),
            "blur_exclude_count": cfg.behavior().blur_exclude.len(),
            "fade_exclude_count": cfg.behavior().fade_exclude.len(),
            "rounded_corners_exclude_count": cfg.behavior().rounded_corners_exclude.len(),
            "game_classes_count": cfg.behavior().game_classes.len(),
            "border_glow_include_count": cfg.behavior().border_glow_include.len(),
            "border_glow_exclude_count": cfg.behavior().border_glow_exclude.len(),
            "corner_radius_rules_count": cfg.behavior().corner_radius_rules.len(),
            "scale_rules_count": cfg.behavior().scale_rules.len(),
            "frosted_glass_rules_count": cfg.behavior().frosted_glass_rules.len(),
            "peek_exclude_count": cfg.behavior().peek_exclude.len(),
            "scrolling_column_width_rules_count": cfg
                .behavior()
                .scrolling_column_width_rules
                .len(),
            "wallpaper_monitors_count": cfg.behavior().wallpaper_monitors.len(),
            "wallpaper_tags_count": cfg.behavior().wallpaper_tags.len(),
            "swallow_terminals": cfg.behavior().swallow_terminals,
            "swallow_exceptions": cfg.behavior().swallow_exceptions,
            "blur_strength_by_hz": cfg.behavior().blur_strength_by_hz,
            "blur_quality_by_monitor": cfg.behavior().blur_quality_by_monitor,
            "ui_theme": cfg.ui_theme(),
            "border_px": cfg.border_px(),
            "gap_px": cfg.gap_px(),
            "snap": cfg.snap(),
        });
        if let (Some(base), Some(more)) = (full.as_object_mut(), polish5.as_object()) {
            for (key, value) in more {
                base.insert(key.clone(), value.clone());
            }
        }
        let client_moveresize = match cfg.client_moveresize() {
            crate::config::ClientMoveResize::Always => "always",
            crate::config::ClientMoveResize::FloatingOnly => "floating-only",
            crate::config::ClientMoveResize::Never => "never",
        };
        let new_client_position = match cfg.new_client_position() {
            crate::config::NewClientPosition::Master => "master",
            crate::config::NewClientPosition::Tail => "tail",
            crate::config::NewClientPosition::AfterFocused => "after_focused",
        };
        let polish6 = serde_json::json!({
            "status_bar_name": cfg.status_bar_name(),
            "status_bar_height": cfg.status_bar_height(),
            "status_bar_padding": cfg.status_bar_padding(),
            "system_ui_font": cfg.system_ui_font(),
            "cursor_theme": cfg.cursor_theme(),
            "cursor_size": cfg.cursor_size(),
            "drag_threshold_px": cfg.drag_threshold_px(),
            "motion_enabled": cfg.motion_enabled(),
            "client_moveresize": client_moveresize,
            "new_client_position": new_client_position,
            "animation_enabled": cfg.animation_enabled(),
            "key_configs_count": cfg.key_configs().len(),
            "rules_count": cfg.get_rules().len(),
            "layout_tags_count": cfg.layout_tags().len(),
            "compositor_enabled": cfg.compositor_enabled(),
            "tagmask": cfg.tagmask(),
            "modkey": cfg.modkey(),
            "show_bar": cfg.show_bar(),
            "m_fact": cfg.m_fact(),
            "n_master": cfg.n_master(),
            "tags_length": cfg.tags_length(),
        });
        if let (Some(base), Some(more)) = (full.as_object_mut(), polish6.as_object()) {
            for (key, value) in more {
                base.insert(key.clone(), value.clone());
            }
        }
        let animation_speed = match cfg.animation_speed() {
            crate::core::animation::AnimationSpeed::Instant => "instant",
            crate::core::animation::AnimationSpeed::Fast => "fast",
            crate::core::animation::AnimationSpeed::Normal => "normal",
            crate::core::animation::AnimationSpeed::Slow => "slow",
        };
        let animation_easing = format!("{:?}", cfg.animation_easing());
        let polish7 = serde_json::json!({
            "animation_speed": animation_speed,
            "animation_easing": animation_easing,
            "animation_duration_ms": cfg.animation_duration().as_millis() as u64,
            "backend_family": format!("{:?}", crate::config::get_backend_family()),
            "buttons_count": cfg.get_buttons().len(),
            "layout_persist_tags": cfg.layout_persist_tags(),
            "chord_leader_key": cfg.chord_config().leader_key,
            "chord_timeout_ms": cfg.chord_config().timeout_ms,
            "chord_bindings_count": cfg.chord_config().bindings.len(),
            "termcmd_len": crate::config::Config::get_termcmd().len(),
            "scratchpad_termcmd_len": crate::config::Config::get_scratchpad_termcmd().len(),
            "ui_theme": cfg.ui_theme(),
            "colors_cyan": cfg.colors().cyan,
            "colors_white": cfg.colors().white,
            "colors_black": cfg.colors().black,
            "behavior_shadow_enabled": cfg.behavior().shadow_enabled,
            "behavior_corner_radius": cfg.behavior().corner_radius,
            "behavior_fading": cfg.behavior().fading,
            "behavior_vrr_enabled": cfg.behavior().vrr_enabled,
            "behavior_swallow_enabled": cfg.behavior().swallow_enabled,
            "behavior_window_tabs": cfg.behavior().window_tabs,
            "behavior_overview_enabled": cfg.behavior().overview_enabled,
            "behavior_expose_enabled": cfg.behavior().expose_enabled,
            "behavior_peek_enabled": cfg.behavior().peek_enabled,
            "behavior_magnifier_enabled": cfg.behavior().magnifier_enabled,
            "behavior_clipboard_history": cfg.behavior().clipboard_history,
        });
        if let (Some(base), Some(more)) = (full.as_object_mut(), polish7.as_object()) {
            for (key, value) in more {
                base.insert(key.clone(), value.clone());
            }
        }
        let Some(keys) = args.get("keys").and_then(|v| v.as_array()) else {
            return full;
        };
        if keys.is_empty() {
            return full;
        }
        let mut filtered = serde_json::Map::new();
        let obj = full.as_object().expect("config object");
        for key in keys {
            let Some(name) = key.as_str() else {
                continue;
            };
            if let Some(value) = obj.get(name) {
                filtered.insert(name.to_string(), value.clone());
            }
        }
        serde_json::Value::Object(filtered)
    }

    pub(crate) fn query_tree(&self, backend: &dyn Backend) -> Vec<TreeNode> {
        // `is_focused` means the one window with input focus, exactly as
        // `get_windows` reports it. Each monitor's own selection would mark
        // one window per monitor, and a script looking for "the focused
        // window" in the tree would pick the wrong one.
        let sel_client = self.get_selected_client_key();
        let index = self.window_query_index(backend);
        self.state
            .monitor_order
            .iter()
            .filter_map(|&mk| {
                let m = self.state.monitors.get(mk)?;
                let mut counts = TagClientCounts::default();
                let windows: Vec<WindowInfo> = self
                    .state
                    .monitor_clients
                    .get(mk)
                    .map(|clients| {
                        clients
                            .iter()
                            .filter_map(|&ck| {
                                let window = self.window_info_indexed(
                                    backend,
                                    ck,
                                    sel_client == Some(ck),
                                    Some(&index),
                                )?;
                                accumulate_window_counts(&mut counts, &window);
                                Some(window)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                Some(TreeNode {
                    monitor: self.monitor_info_ipc(backend, mk, m),
                    window_count: counts.total,
                    urgent_count: counts.urgent,
                    floating_count: counts.floating,
                    minimized_count: counts.minimized,
                    sticky_count: counts.sticky,
                    fullscreen_count: counts.fullscreen,
                    pip_count: counts.pip,
                    maximized_count: counts.maximized,
                    above_count: counts.above,
                    below_count: counts.below,
                    scratchpad_count: counts.scratchpad,
                    tabbed_count: counts.tabbed,
                    fixed_count: counts.fixed,
                    dock_count: counts.dock,
                    desktop_count: counts.desktop,
                    never_focus_count: counts.never_focus,
                    demands_attention_count: counts.demands_attention,
                    skip_taskbar_count: counts.skip_taskbar,
                    skip_pager_count: counts.skip_pager,
                    no_decorations_count: counts.no_decorations,
                    drag_float_count: counts.drag_float,
                    swallowed_count: counts.swallowed,
                    on_view_count: counts.on_view,
                    maximize_promoted_count: counts.maximize_promoted,
                    strut_count: counts.strut,
                    status_bar_count: counts.status_bar,
                    owns_output_count: self.monitor_owns_output_count(mk),
                    selected_id: m
                        .sel
                        .and_then(|ck| self.state.clients.get(ck).map(|client| client.win.raw())),
                    windows,
                })
            })
            .collect()
    }

    // =========================================================================
    // IPC event broadcast helper
    // =========================================================================

    pub(crate) fn broadcast_ipc_event(&mut self, event_type: &str, payload: serde_json::Value) {
        if let Some(ipc) = self.ipc_server.as_mut() {
            ipc.broadcast(&IpcEvent {
                event: event_type.to_string(),
                payload,
            });
        }
    }

    /// Push a `window/state` event with the current [`WindowInfo`] for
    /// `client_key`. Subscribers of `window` / `window/state` / `*` see
    /// maximize (and later sibling) flips without polling `get_windows`.
    /// Missing clients are a no-op; serialization failure is ignored.
    pub(crate) fn broadcast_window_state_ipc(
        &mut self,
        backend: &dyn Backend,
        client_key: ClientKey,
    ) {
        let focused = self.get_selected_client_key() == Some(client_key);
        let Some(info) = self.window_info(backend, client_key, focused) else {
            return;
        };
        let Ok(payload) = serde_json::to_value(&info) else {
            return;
        };
        self.broadcast_ipc_event("window/state", payload);
    }

    /// Push `window/state` for every visible client on `mon` after a layout
    /// reorder (zoom / movestack / scrolling column move) changed many
    /// geometries at once.
    pub(crate) fn broadcast_visible_window_states_on_monitor(
        &mut self,
        backend: &dyn Backend,
        mon: MonitorKey,
    ) {
        let keys: Vec<_> = self
            .state
            .monitor_clients
            .get(mon)
            .map(|clients| clients.iter().copied().collect())
            .unwrap_or_default();
        for client_key in keys {
            if self.is_client_visible_by_key(client_key) {
                self.broadcast_window_state_ipc(backend, client_key);
            }
        }
    }

    /// Like [`Self::broadcast_visible_window_states_on_monitor`] for every
    /// monitor — used after a global `arrange(None)` from strut / topology
    /// changes that rewrite work areas on all outputs.
    pub(crate) fn broadcast_visible_window_states_all_monitors(&mut self, backend: &dyn Backend) {
        let monitors: Vec<_> = self.state.monitor_order.clone();
        for mon in monitors {
            self.broadcast_visible_window_states_on_monitor(backend, mon);
        }
    }

    /// Occupancy snapshot for one output (`show_bar` / `bar_visible` /
    /// `has_visible_fullscreen` / `owns_output_count`). Subscribers of
    /// `monitor` / `monitor/bar` / `bar` / `*` see F11 and togglebar without
    /// polling (`bar` stores as `monitor/bar`).
    pub(crate) fn broadcast_monitor_bar_ipc(&mut self, backend: &dyn Backend, mk: MonitorKey) {
        self.broadcast_ipc_event("monitor/bar", self.query_show_bar_for_monitor(backend, mk));
    }

    /// Occupancy snapshot for every output after a global rearrange (session
    /// restore, struts, topology).
    pub(crate) fn broadcast_monitor_bar_all_monitors(&mut self, backend: &dyn Backend) {
        let monitors: Vec<_> = self.state.monitor_order.clone();
        for mk in monitors {
            self.broadcast_monitor_bar_ipc(backend, mk);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_COMMAND_BATCH_ENTRIES, MAX_CONFIG_BATCH_CHANGES, MAX_REPORTED_UNKNOWN_TOPICS,
        active_tag_count, cached_power_profiles_answer, client_window_info,
        color_managed_surface_json, color_session_policy_json, color_surface_summary_json,
        ffprobe_verdict, fullscreen_screenshot_submission_response, monitor_tag_client_counts,
        optional_protocol_enabled_from_flags, output_color_policy_json, parse_benchmark_request,
        parse_command_batch_entries, parse_config_batch_changes, parse_optional_u32_ipc_arg,
        parse_required_i32_ipc_arg, presented_with_hdr, recording_output_is_valid,
        render_decisions_json, resolved_client_monitor_num, runtime_health, tag_client_counts,
        tagged_client_count, unknown_subscription_topics, workspace_layout_state,
    };

    /// A backend with no HDR signalling gate of its own reports no per-output
    /// refusals at all — which is *not* the same as "no refusals", and the
    /// policy object must not read it as one.
    const REFUSED: [(String, Option<String>); 0] = [];
    use crate::Jwm;
    use crate::application::BenchmarkRequest;
    use crate::backend::api::{
        Backend, BackendDiagnostics, Capabilities, ColorAllocator, ColorManagedSurfaceInfo,
        CompositorAnnotation, CompositorBenchmark, CompositorControl, CompositorMedia,
        CompositorWindowEffects, CompositorWorkspaceEffects, DisplayControl, EventHandler, OsdKind,
        OutputIdentity, OutputInfo, RenderScheduler,
    };
    use crate::backend::common_define::{OutputId, WindowId};
    use crate::backend::edid::EdidHdrCapabilities;
    use crate::backend::error::BackendError;
    use crate::backend::wayland_dummy_ops::{
        DummyColorAllocator, DummyCursorProvider, DummyInputOps, DummyKeyOps, DummyOutputOps,
        DummyPropertyOps, DummyWindowOps,
    };
    use crate::core::layout::LayoutEnum;
    use crate::core::models::{Pertag, WMClient, WMMonitor};
    use crate::core::state::WMState;
    use crate::ipc::RuntimeHealthStatus;
    use crate::jwm::features::SystemUiState;
    use crate::jwm::features::pairing::{PairingPrompt, PairingSession};
    use std::any::Any;
    use std::rc::Rc;

    #[test]
    fn fullscreen_screenshot_ipc_reports_an_explicit_queued_submission() {
        let response = fullscreen_screenshot_submission_response(Ok(std::path::PathBuf::from(
            "/tmp/screenshot.png",
        )));

        assert!(response.success);
        assert!(response.error.is_none());
        let data = response.data.expect("queued submission data");
        assert_eq!(data["status"], "queued");
        assert_eq!(data["path"], "/tmp/screenshot.png");
    }

    #[test]
    fn fullscreen_screenshot_ipc_reports_submission_errors() {
        let response = fullscreen_screenshot_submission_response(Err(
            "cannot create screenshot staging file".to_string(),
        ));

        assert!(!response.success);
        assert!(response.data.is_none());
        assert_eq!(
            response.error.as_deref(),
            Some("cannot create screenshot staging file")
        );
    }

    fn output(hdr_metadata: Option<EdidHdrCapabilities>) -> OutputInfo {
        OutputInfo {
            id: OutputId(1),
            name: "HDMI-A-1".into(),
            x: 0,
            y: 0,
            width: 3840,
            height: 2160,
            scale: 1.0,
            refresh_rate: 60_000,
            transform: 0,
            hdr_capable: hdr_metadata.is_some(),
            hdr_metadata,
            identity: OutputIdentity::connector_only("HDMI-A-1"),
            physical_width_mm: 0,
            physical_height_mm: 0,
            preferred_width: 0,
            preferred_height: 0,
            preferred_refresh_mhz: 0,
        }
    }

    #[test]
    fn runtime_health_reports_config_and_monitor_problems() {
        let healthy_config = serde_json::json!({
            "exists": true,
            "diagnostics": {"error_count": 0, "warning_count": 0},
            "reload": {"last_success": null, "last_error": null}
        });
        assert_eq!(
            runtime_health(&healthy_config, 1, None).status,
            RuntimeHealthStatus::Healthy
        );

        let transition_failure =
            runtime_health(&healthy_config, 1, Some("selection already owned"));
        assert_eq!(transition_failure.status, RuntimeHealthStatus::Degraded);
        assert_eq!(
            transition_failure.reasons,
            ["last compositor transition failed: selection already owned"]
        );

        let failed_reload = serde_json::json!({
            "exists": true,
            "diagnostics": {"error_count": 0, "warning_count": 1},
            "reload": {"last_success": false, "last_error": "bad TOML"}
        });
        let health = runtime_health(&failed_reload, 0, None);
        assert_eq!(health.status, RuntimeHealthStatus::Degraded);
        assert_eq!(health.reasons.len(), 3);
        assert!(
            health
                .reasons
                .iter()
                .any(|reason| reason.contains("bad TOML"))
        );
    }

    #[test]
    fn ipc_window_helpers_use_live_monitor_keys_and_per_tag_counts() {
        let mut state = WMState::new();
        let mut monitor = WMMonitor::new();
        monitor.num = 7;
        let monitor_key = state.monitors.insert(monitor);

        let mut first = WMClient::new(WindowId::from_raw(1));
        first.mon = Some(monitor_key);
        first.state.tags = 0b0101;
        let first_key = state.clients.insert(first);

        let mut second = WMClient::new(WindowId::from_raw(2));
        second.mon = Some(monitor_key);
        second.state.tags = 0b0010;
        let second_key = state.clients.insert(second);
        state
            .monitor_clients
            .insert(monitor_key, vec![first_key, second_key]);

        let first = state.clients.get(first_key).unwrap();
        assert_eq!(resolved_client_monitor_num(&state.monitors, first), 7);
        assert_eq!(tagged_client_count(&state, monitor_key, 0b0001), 1);
        assert_eq!(tagged_client_count(&state, monitor_key, 0b0010), 1);
        assert_eq!(tagged_client_count(&state, monitor_key, 0b0100), 1);
        assert_eq!(tagged_client_count(&state, monitor_key, 0b1000), 0);
    }

    #[test]
    fn client_counts_preserve_overlapping_flags_in_one_pass() {
        let mut state = WMState::new();
        let monitor_key = state.monitors.insert(WMMonitor::new());

        let mut first = WMClient::new(WindowId::from_raw(1));
        first.state.tags = 0b0001;
        first.state.is_hidden = true;
        first.state.is_floating = true;
        first.state.is_urgent = true;
        first.state.is_fullscreen = true;
        first.state.is_maximized_vert = true;
        first.state.is_sticky = true;
        first.state.demands_attention = true;
        first.state.maximize_restore_tiled = true;
        let first_key = state.clients.insert(first);

        let mut second = WMClient::new(WindowId::from_raw(2));
        second.state.tags = 0b0010;
        second.state.is_pip = true;
        second.state.is_below = true;
        second.state.skip_taskbar = true;
        let second_key = state.clients.insert(second);
        state
            .monitor_clients
            .insert(monitor_key, vec![first_key, second_key]);

        let scratchpads = std::collections::HashSet::from([first_key]);
        let tabbed = std::collections::HashSet::from([second_key]);
        let struts = std::collections::HashSet::from([WindowId::from_raw(2)]);
        let counts = tag_client_counts(
            &state,
            monitor_key,
            u32::MAX,
            &scratchpads,
            &tabbed,
            &struts,
            "never-a-status-bar",
            0b0010,
        );

        assert_eq!(counts.total, 2);
        assert_eq!(counts.minimized, 1);
        assert_eq!(counts.floating, 1);
        assert_eq!(counts.urgent, 1);
        assert_eq!(counts.fullscreen, 1);
        assert_eq!(counts.maximized, 1);
        assert_eq!(counts.scratchpad, 1);
        assert_eq!(counts.tabbed, 1);
        assert_eq!(counts.pip, 1);
        assert_eq!(counts.below, 1);
        assert_eq!(counts.strut, 1);
        assert_eq!(counts.on_view, 2);
        assert_eq!(counts.maximize_promoted, 1);
        assert_eq!(counts.skip_taskbar, 1);
        assert_eq!(counts.owns_output, 0);

        let first_tag = tag_client_counts(
            &state,
            monitor_key,
            0b0001,
            &scratchpads,
            &tabbed,
            &struts,
            "never-a-status-bar",
            0b0010,
        );
        assert_eq!(first_tag.total, 1);
        assert_eq!(first_tag.scratchpad, 1);
        assert_eq!(first_tag.tabbed, 0);
        assert_eq!(first_tag.on_view, 1);

        let batched = monitor_tag_client_counts(
            &state,
            monitor_key,
            2,
            &scratchpads,
            &tabbed,
            &struts,
            "never-a-status-bar",
            0b0010,
        );
        for (index, actual) in batched.iter().enumerate() {
            let expected = tag_client_counts(
                &state,
                monitor_key,
                1 << index,
                &scratchpads,
                &tabbed,
                &struts,
                "never-a-status-bar",
                0b0010,
            );
            assert_eq!(*actual, expected, "tag {index}");
        }
    }

    #[test]
    fn window_query_projection_distinguishes_minimized_from_focus() {
        let mut client = WMClient::new(WindowId::from_raw(0x2a));
        client.state.is_hidden = true;
        client.geometry.x = -2560;
        client.geometry.y = 40;
        client.geometry.w = 640;
        client.geometry.h = 480;

        let info = client_window_info(
            &client, 7, false, false, false, false, None, None, None, None, None, None, None,
            false, None, None, false, None,
        );
        assert_eq!(info.id, 0x2a);
        assert_eq!(info.monitor, 7);
        assert!(info.is_minimized);
        assert_eq!(info.minimized_order, 0);
        assert!(!info.is_focused);
        assert!(!info.is_swallowed);
        assert!(!info.maximize_promoted);
        assert!(info.maximize_restore.is_none());
        assert!(info.swallowing.is_none());
        assert!(info.swallowed_by.is_none());
        assert!(info.transient_for.is_none());
        assert!(!info.is_tabbed);
        assert!(info.tab_index.is_none());
        assert!(!info.is_on_view);
        assert!(!info.is_scratchpad);
        assert!(!info.is_fixed);
        assert!(!info.is_dock);
        assert!(!info.is_desktop);
        assert!(!info.is_drag_floating);
        assert!(!info.never_focus);
        assert!(!info.skip_taskbar);
        assert!(!info.skip_pager);
        assert!(!info.no_decorations);
        assert!(!info.demands_attention);
        assert!(!info.has_strut);
        assert_eq!(info.client_fact, 0.0);
        assert_eq!(info.border_w, 0);
        assert_eq!(info.old_border_w, 0);
        assert!(info.hidden_restore.is_none());
        assert!(info.maximize_restore_anchor.is_none());
        assert!(!info.pip_restore_sticky);
        assert!(!info.old_state);
        assert!(!info.remembers_closed_placement);
        assert!(info.dock_exclusive_zone.is_none());
        assert!(!info.is_status_bar);
        assert!(info.hidden_x.is_none());
        assert!(info.sync_counter.is_none());
        assert_eq!(info.sync_value, 0);
        assert!(info.layout.is_none());
        assert!(info.scratchpad.is_none());

        client.state.is_hidden = false;
        client.state.is_swallowed = true;
        client.state.is_fixed = true;
        client.state.is_dock = true;
        client.state.is_desktop = true;
        client.state.client_fact = 1.25;
        client.state.maximize_restore_tiled = true;
        client.state.minimized_order = 9;
        client.state.pip_restore_sticky = true;
        client.state.old_state = true;
        client.state.remembers_closed_placement = true;
        client.state.sync_counter = Some(0xabc);
        client.state.sync_value = 42;
        client.geometry.border_w = 4;
        client.geometry.old_border_w = 2;
        client.geometry.hidden_x = Some(-4096);
        client.geometry.maximize_restore_rect = Some(crate::core::types::Rect::new(10, 20, 30, 40));
        client.geometry.hidden_restore_rect = Some(crate::core::types::Rect::new(1, 2, 3, 4));
        client.state.dock_layer_info = Some(crate::backend::api::LayerSurfaceInfo {
            exclusive_zone: 32,
            anchor_top: true,
            anchor_bottom: false,
            anchor_left: false,
            anchor_right: false,
        });
        let restored = client_window_info(
            &client,
            7,
            true,
            true,
            true,
            true,
            Some("term".into()),
            Some("TILE".into()),
            Some("DP-1".into()),
            Some("Dell U2720Q".into()),
            Some(99),
            Some(88),
            Some(77),
            true,
            Some(1),
            Some(0x99),
            true,
            Some(3),
        );
        assert!(!restored.is_minimized);
        assert_eq!(restored.minimized_order, 9);
        assert!(restored.is_focused);
        assert!(restored.is_swallowed);
        assert!(restored.maximize_promoted);
        assert_eq!(
            restored.maximize_restore,
            Some(crate::ipc::RectIpc {
                x: 10,
                y: 20,
                w: 30,
                h: 40
            })
        );
        assert_eq!(restored.swallowing, Some(99));
        assert_eq!(restored.swallowed_by, Some(88));
        assert_eq!(restored.transient_for, Some(77));
        assert!(restored.is_tabbed);
        assert_eq!(restored.tab_index, Some(1));
        assert!(restored.is_on_view);
        assert!(restored.is_scratchpad);
        assert!(restored.is_fixed);
        assert!(restored.is_dock);
        assert!(restored.is_desktop);
        assert!(!restored.is_drag_floating);
        assert!(!restored.never_focus);
        assert!(!restored.skip_taskbar);
        assert!(!restored.skip_pager);
        assert!(!restored.no_decorations);
        assert!(!restored.demands_attention);
        assert!(restored.has_strut);
        assert_eq!(restored.client_fact, 1.25);
        assert_eq!(restored.border_w, 4);
        assert_eq!(restored.old_border_w, 2);
        assert_eq!(
            restored.hidden_restore,
            Some(crate::ipc::RectIpc {
                x: 1,
                y: 2,
                w: 3,
                h: 4
            })
        );
        assert_eq!(restored.maximize_restore_anchor, Some(0x99));
        assert!(restored.pip_restore_sticky);
        assert!(restored.old_state);
        assert!(restored.remembers_closed_placement);
        assert_eq!(restored.dock_exclusive_zone, Some(32));
        assert!(restored.dock_anchor_top);
        assert!(!restored.dock_anchor_bottom);
        assert!(restored.is_status_bar);
        assert_eq!(restored.scratchpad.as_deref(), Some("term"));
        assert_eq!(restored.layout.as_deref(), Some("TILE"));
        assert_eq!(restored.connector.as_deref(), Some("DP-1"));
        assert_eq!(restored.monitor_name.as_deref(), Some("Dell U2720Q"));
        assert_eq!(restored.hidden_x, Some(-4096));
        assert_eq!(restored.sync_counter, Some(0xabc));
        assert_eq!(restored.sync_value, 42);
    }

    #[test]
    fn numeric_ipc_args_accept_boundaries_and_defaults() {
        assert_eq!(
            parse_optional_u32_ipc_arg(&serde_json::json!({}), "benchmark", "frames", 600),
            Ok(600)
        );
        assert_eq!(
            parse_optional_u32_ipc_arg(
                &serde_json::json!({"frames": u32::MAX}),
                "benchmark",
                "frames",
                600,
            ),
            Ok(u32::MAX)
        );
        assert_eq!(
            parse_required_i32_ipc_arg(
                &serde_json::json!({"monitor": i32::MIN}),
                "move_window_to_monitor",
                "monitor",
            ),
            Ok(i32::MIN)
        );
        assert_eq!(
            parse_required_i32_ipc_arg(
                &serde_json::json!({"monitor": i32::MAX}),
                "move_window_to_monitor",
                "monitor",
            ),
            Ok(i32::MAX)
        );
    }

    #[test]
    fn numeric_ipc_args_reject_invalid_or_out_of_range_values() {
        let too_many_frames = u64::from(u32::MAX) + 1;
        let err = parse_optional_u32_ipc_arg(
            &serde_json::json!({"frames": too_many_frames}),
            "benchmark",
            "frames",
            600,
        )
        .unwrap_err();
        assert!(err.contains("frames"));
        assert!(err.contains("outside the u32 range"));

        for invalid in [
            serde_json::json!(-1),
            serde_json::json!(1.5),
            serde_json::json!("600"),
            serde_json::Value::Null,
        ] {
            let err = parse_optional_u32_ipc_arg(
                &serde_json::json!({"warmup": invalid}),
                "benchmark",
                "warmup",
                60,
            )
            .unwrap_err();
            assert!(err.contains("warmup"));
            assert!(err.contains("unsigned 32-bit integer"));
        }

        for invalid in [
            serde_json::json!(i64::from(i32::MIN) - 1),
            serde_json::json!(i64::from(i32::MAX) + 1),
            serde_json::json!(u64::MAX),
        ] {
            let err = parse_required_i32_ipc_arg(
                &serde_json::json!({"monitor": invalid}),
                "move_window_to_monitor",
                "monitor",
            )
            .unwrap_err();
            assert!(err.contains("monitor"));
            assert!(err.contains("outside the i32 range"));
        }

        let err = parse_required_i32_ipc_arg(
            &serde_json::json!({"monitor": 1.5}),
            "move_window_to_monitor",
            "monitor",
        )
        .unwrap_err();
        assert!(err.contains("32-bit integer"));
    }

    #[test]
    fn benchmark_ipc_request_enforces_application_resource_limits() {
        assert_eq!(
            parse_benchmark_request(&serde_json::json!({"action": "start"})),
            BenchmarkRequest::new(600, 60)
        );
        assert_eq!(
            parse_benchmark_request(&serde_json::json!({
                "frames": BenchmarkRequest::MAX_FRAMES,
                "warmup": BenchmarkRequest::MAX_WARMUP_FRAMES,
            })),
            BenchmarkRequest::new(
                BenchmarkRequest::MAX_FRAMES,
                BenchmarkRequest::MAX_WARMUP_FRAMES,
            )
        );

        for (request, expected_detail) in [
            (serde_json::json!({"frames": 0}), "greater than zero"),
            (
                serde_json::json!({"frames": BenchmarkRequest::MAX_FRAMES + 1}),
                "exceeds the maximum",
            ),
            (
                serde_json::json!({"warmup": BenchmarkRequest::MAX_WARMUP_FRAMES + 1}),
                "exceeds the maximum",
            ),
            (
                serde_json::json!({"frames": u32::MAX}),
                "exceeds the maximum",
            ),
        ] {
            let error = parse_benchmark_request(&request).unwrap_err();
            assert!(error.starts_with("benchmark:"), "{error}");
            assert!(error.contains(expected_detail), "{error}");
        }
    }

    #[test]
    fn workspace_layout_state_uses_one_based_pertag_slots() {
        let mut monitor = WMMonitor::new();
        monitor.layout.m_fact = 0.55;
        monitor.layout.n_master = 1;
        monitor.layout.gap = 0;

        let mut pertag = Pertag::new(true, 2);
        pertag.m_facts[0] = 0.11;
        pertag.n_masters[0] = 9;
        pertag.gaps[0] = 1;
        pertag.m_facts[1] = 0.61;
        pertag.n_masters[1] = 2;
        pertag.gaps[1] = 8;
        pertag.lts[1] = Rc::new(LayoutEnum::MONOCLE);
        pertag.m_facts[2] = 0.72;
        pertag.n_masters[2] = 3;
        pertag.gaps[2] = 16;
        pertag.lts[2] = Rc::new(LayoutEnum::GRID);
        monitor.pertag = Some(pertag);

        assert_eq!(
            workspace_layout_state(&monitor, 0),
            (format!("{:?}", LayoutEnum::MONOCLE), 0.61, 2, 8)
        );
        assert_eq!(
            workspace_layout_state(&monitor, 1),
            (format!("{:?}", LayoutEnum::GRID), 0.72, 3, 16)
        );
    }

    #[test]
    fn workspace_layout_state_falls_back_without_safe_pertag_data() {
        let mut monitor = WMMonitor::new();
        monitor.lt = Rc::new(LayoutEnum::TILE);
        monitor.layout.m_fact = 0.66;
        monitor.layout.n_master = 4;
        monitor.layout.gap = 12;
        let current = (format!("{:?}", LayoutEnum::TILE), 0.66, 4, 12);

        assert_eq!(workspace_layout_state(&monitor, 0), current);

        monitor.pertag = Some(Pertag::new(true, 0));
        assert_eq!(workspace_layout_state(&monitor, 1), current);
        assert_eq!(workspace_layout_state(&monitor, usize::MAX), current);
    }

    #[test]
    fn focused_layout_queries_report_live_monitor_params() {
        let mut backend = PairingIpcBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let mk = jwm.state.sel_mon.expect("selected monitor");
        {
            let mon = jwm.state.monitors.get_mut(mk).expect("monitor");
            mon.layout.m_fact = 0.42;
            mon.layout.n_master = 3;
            mon.layout.gap = 14;
            mon.lt = Rc::new(LayoutEnum::MONOCLE);
            mon.update_current_tag_layout_params();
            if let Some(pertag) = mon.pertag.as_mut() {
                let cur = pertag.clamp_tag(pertag.cur_tag);
                if let Some(slot) = pertag.lts.get_mut(cur) {
                    *slot = Rc::new(LayoutEnum::MONOCLE);
                }
            }
        }

        let layout = jwm.query_focused_layout(&backend);
        assert!((layout["m_fact"].as_f64().unwrap() - 0.42).abs() < 1e-6);
        assert_eq!(layout["n_master"], 3);
        assert_eq!(layout["gap"], 14);
        assert_eq!(layout["layout"], format!("{:?}", LayoutEnum::MONOCLE));
        assert!(layout["monitor"].as_i64().is_some());

        let gaps = jwm.query_focused_gaps(&backend);
        assert_eq!(gaps["gap"], 14);
        assert_eq!(gaps["monitor"], layout["monitor"]);

        let nmaster = jwm.query_focused_nmaster(&backend);
        assert_eq!(nmaster["n_master"], 3);
        assert_eq!(nmaster["monitor"], layout["monitor"]);

        let mfact = jwm.query_focused_mfact(&backend);
        assert!((mfact["m_fact"].as_f64().unwrap() - 0.42).abs() < 1e-6);
        assert_eq!(mfact["monitor"], layout["monitor"]);

        let night = jwm.query_night_light();
        assert!(night.get("active").and_then(|v| v.as_bool()).is_some());
        assert!(night.get("temp").and_then(|v| v.as_f64()).is_some());
        assert!(night.get("override").is_some());

        let scratchpads = jwm.query_scratchpads();
        assert!(scratchpads.as_object().is_some());

        let struts = jwm.query_struts(&backend);
        assert!(struts.as_array().is_some_and(|rows| !rows.is_empty()));
        assert!(
            struts[0]
                .get("windows")
                .and_then(|v| v.as_array())
                .is_some()
        );
        assert!(struts[0].get("top").is_some());

        let monitors = jwm.query_monitors(&backend);
        assert_eq!(monitors[0].gap, 14);
        assert_eq!(monitors[0].n_master, 3);
        assert!((monitors[0].m_fact as f64 - 0.42).abs() < 1e-6);
        assert_eq!(monitors[0].transform, 0);
        let workspaces = jwm.query_workspaces(&backend);
        let focused = workspaces
            .iter()
            .find(|ws| ws.focused)
            .expect("focused workspace");
        assert_eq!(focused.gap, 14);
        assert_eq!(focused.n_master, 3);
        assert!((focused.m_fact as f64 - 0.42).abs() < 1e-6);
    }

    #[test]
    fn color_policy_uses_safe_srgb_when_advanced_disabled() {
        let value = output_color_policy_json(
            &output(Some(EdidHdrCapabilities {
                max_luminance_nits: 1000.0,
                min_luminance_nits: 0.05,
                max_frame_average_nits: 400.0,
                supports_bt2020: true,
                supports_pq: true,
                supports_hlg: false,
            })),
            None,
            true,
            false,
            true,
            false,
        );

        assert_eq!(value["policy_source"], "runtime_srgb_fail_closed");
        assert_eq!(value["capability_source"], "srgb_safe_default");
        assert_eq!(value["selected_transfer_function"], "srgb");
        assert_eq!(value["selected_primaries"], "srgb");
    }

    #[test]
    fn color_policy_reports_hdr_edid_when_advanced_enabled() {
        let value = output_color_policy_json(
            &output(Some(EdidHdrCapabilities {
                max_luminance_nits: 1000.0,
                min_luminance_nits: 0.05,
                max_frame_average_nits: 400.0,
                supports_bt2020: true,
                supports_pq: true,
                supports_hlg: false,
            })),
            None,
            true,
            true,
            true,
            false,
        );

        // Capable but not signalled: the runtime profile is what the display
        // is being told right now, which is still sRGB.
        assert_eq!(value["policy_source"], "runtime_srgb_fail_closed");
        assert_eq!(value["capability_source"], "edid_hdr_capability");
        assert_eq!(value["selected_transfer_function"], "srgb");
        assert_eq!(value["selected_primaries"], "srgb");
        assert_eq!(value["capability_transfer_function"], "st2084_pq");
        assert_eq!(value["capability_primaries"], "bt2020");
        assert_eq!(
            value["non_srgb_profile_needs_software_without_kms_pair"],
            true
        );
        assert_eq!(value["shader_fallback_required"], true);
        assert_eq!(
            value["shader_fallback_semantics"],
            "static_non_srgb_profile_hint_not_active_route"
        );
        assert_eq!(
            value["delivery_route_observation"],
            "see_color_delivery_last_success"
        );
    }

    #[test]
    fn presented_with_hdr_reads_the_real_serialized_shape() {
        use crate::backend::api::{
            ColorDeliveryOutputStatus, ColorDeliveryPresentationStatus, ColorDeliveryStatus,
        };

        let presentation = |hdr_metadata_active: bool| ColorDeliveryPresentationStatus {
            generation: 3,
            policy_sequence: 7,
            route: "software_per_output_regions".into(),
            working_space: "normalized_linear_srgb".into(),
            target_transfer_function: "st2084_pq".into(),
            target_primaries: "bt2020".into(),
            hdr_metadata_active,
            colorspace_signal: if hdr_metadata_active {
                "bt2020_rgb".into()
            } else {
                "default_sdr".into()
            },
            fallback_reason: None,
            presented_at_monotonic_ms: Some(1),
            presented_ago_ms: Some(0),
        };
        // Built by serde from the real type, not written by hand: a lookup
        // keyed on a field name that does not exist would silently answer
        // false forever, and a hand-written fixture would encode the same
        // mistake as the code it is checking.
        let status = ColorDeliveryStatus {
            schema_version: 1,
            observation: "last_successful_presentation".into(),
            generation: 3,
            last_policy_decision: None,
            outputs: vec![
                ColorDeliveryOutputStatus {
                    output_name: "HDMI-A-1".into(),
                    participating: true,
                    last_success: Some(presentation(true)),
                },
                ColorDeliveryOutputStatus {
                    output_name: "DP-1".into(),
                    participating: true,
                    last_success: Some(presentation(false)),
                },
                ColorDeliveryOutputStatus {
                    output_name: "DP-2".into(),
                    participating: false,
                    last_success: None,
                },
            ],
        };
        let value = serde_json::to_value(&status).expect("serialize");

        assert!(presented_with_hdr(Some(&value), None), "any output");
        assert!(presented_with_hdr(Some(&value), Some("HDMI-A-1")));
        assert!(!presented_with_hdr(Some(&value), Some("DP-1")));
        assert!(
            !presented_with_hdr(Some(&value), Some("DP-2")),
            "no success"
        );
        assert!(
            !presented_with_hdr(Some(&value), Some("HDMI-A-2")),
            "an output that is not in the snapshot is not a match"
        );

        // Backends with no colour delivery at all report nothing, rather than
        // a fabricated answer.
        assert!(!presented_with_hdr(None, None));
        assert!(!presented_with_hdr(Some(&serde_json::json!({})), None));

        // And with nothing presented yet, HDR is not claimed.
        let unpresented = ColorDeliveryStatus {
            outputs: vec![ColorDeliveryOutputStatus {
                output_name: "HDMI-A-1".into(),
                participating: true,
                last_success: None,
            }],
            ..status
        };
        let value = serde_json::to_value(&unpresented).expect("serialize");
        assert!(!presented_with_hdr(Some(&value), None));
    }

    #[test]
    fn a_signalled_output_reports_the_profile_the_display_is_actually_told() {
        let hdr = EdidHdrCapabilities {
            max_luminance_nits: 1000.0,
            min_luminance_nits: 0.05,
            max_frame_average_nits: 400.0,
            supports_bt2020: true,
            supports_pq: true,
            supports_hlg: false,
        };
        // The runtime profile used to be a literal sRGB, so an output that
        // had successfully switched still reported sRGB — which made the
        // per-output policy useless for verifying an enable.
        let signalled =
            output_color_policy_json(&output(Some(hdr.clone())), None, true, true, true, true);
        assert_eq!(
            signalled["policy_source"],
            "runtime_edid_hdr_profile_signalled"
        );
        assert_eq!(signalled["selected_transfer_function"], "st2084_pq");
        assert_eq!(signalled["selected_primaries"], "bt2020");
        assert_eq!(
            signalled["selected_profile_semantics"],
            "last_successful_presentation_output_target"
        );

        // With advanced colour management off the output is advertised as
        // exact sRGB to clients, so it must report sRGB whatever the last
        // frame did — the two must not disagree.
        let gated = output_color_policy_json(&output(Some(hdr)), None, true, false, true, true);
        assert_eq!(gated["policy_source"], "runtime_srgb_fail_closed");
        assert_eq!(gated["selected_transfer_function"], "srgb");
    }

    #[test]
    fn color_surface_summary_reports_hdr_and_named_distributions() {
        let surfaces = vec![
            ColorManagedSurfaceInfo {
                surface_object_id: "surface-a".into(),
                identity: 1,
                tf_named: Some(2),
                tf_power: None,
                primaries_named: Some(1),
                primaries: None,
                min_lum: None,
                max_lum: None,
                reference_lum: None,
                mastering_primaries: None,
                mastering_min_lum: None,
                mastering_max_lum: None,
                max_cll: None,
                max_fall: None,
            },
            ColorManagedSurfaceInfo {
                surface_object_id: "surface-b".into(),
                identity: 2,
                tf_named: Some(11),
                tf_power: None,
                primaries_named: Some(6),
                primaries: Some([
                    708000, 292000, 170000, 797000, 131000, 46000, 312700, 329000,
                ]),
                min_lum: Some(500),
                max_lum: Some(1000),
                reference_lum: Some(203),
                mastering_primaries: None,
                mastering_min_lum: None,
                mastering_max_lum: Some(1000),
                max_cll: Some(1000),
                max_fall: Some(400),
            },
        ];

        let summary = color_surface_summary_json(&surfaces);
        assert_eq!(summary["surface_count"], 2);
        assert_eq!(summary["hdr_surface_count"], 1);
        assert_eq!(summary["transfer_functions"]["gamma22"], 1);
        assert_eq!(summary["transfer_functions"]["st2084_pq"], 1);
        assert_eq!(summary["primaries"]["srgb"], 1);
        assert_eq!(summary["primaries"]["bt2020"], 1);
        assert_eq!(summary["max_luminance_peak"], 1000);

        let detail = color_managed_surface_json(&surfaces[1]);
        assert_eq!(detail["transfer_function"], "st2084_pq");
        assert_eq!(detail["primaries"], "bt2020");
        assert_eq!(detail["hdr"], true);
        assert_eq!(detail["primaries_xy"][0], 708000);
    }

    #[test]
    fn color_session_policy_reports_mixed_hdr_path_and_blockers() {
        let hdr = output(Some(EdidHdrCapabilities {
            max_luminance_nits: 1000.0,
            min_luminance_nits: 0.05,
            max_frame_average_nits: 400.0,
            supports_bt2020: true,
            supports_pq: true,
            supports_hlg: false,
        }));
        let mut sdr = output(None);
        sdr.id = OutputId(2);
        sdr.name = "DP-1".into();
        sdr.identity = OutputIdentity::connector_only("DP-1");

        let full = color_session_policy_json(
            &[hdr.clone(), sdr.clone()],
            true,
            true,
            true,
            true,
            true,
            &REFUSED,
            false,
            false,
        );
        assert_eq!(full["mixed_hdr_outputs"], true);
        assert_eq!(full["heterogeneous_output_profiles"], true);
        assert_eq!(
            full["sdr_on_hdr_policy"],
            "normalized_transfer_and_gamut_without_absolute_luminance_mapping"
        );
        assert_eq!(
            full["hdr_active_semantics"],
            "physical_output_signal_last_successful_presentation"
        );
        // Nothing has been presented with HDR metadata, so nothing claims it.
        assert_eq!(full["hdr_active"], false);
        assert_eq!(
            full["mixed_hdr_policy"],
            "per_output_delivery_infrastructure_sdr_signalling_fail_closed"
        );
        // A backend with no per-output gate contributes no refusal names, and
        // the object does not invent one.
        assert_eq!(full["blockers"], serde_json::json!([]));
        assert_eq!(full["hdr_enable_refusals"], serde_json::json!([]));
        assert_eq!(
            full["delivery_capabilities"]["hdr_signalling_enable_available"], false,
            "no gate reporting in is not an availability claim"
        );
        assert_eq!(
            full["delivery_capabilities"]["working_space"],
            "normalized_linear_srgb"
        );
        assert_eq!(
            full["delivery_capabilities"]["luminance_model"],
            "source_transfer_native_normalized"
        );
        assert_eq!(
            full["delivery_capabilities"]["software_per_output_encode_regions"],
            true
        );
        assert_eq!(
            full["delivery_capabilities"]["hardware_delivery_policy"],
            "paired_all_output_crtc_lut_ctm_or_none"
        );
        assert_eq!(full["fallback_policy"]["route"], "global_srgb");
        assert_eq!(
            full["fallback_policy"]["route_observation"],
            "see_color_delivery_last_success"
        );
        assert_eq!(
            full["fallback_policy"]["normal_desktop_cursor_effect"],
            "usually_selects_global_srgb_fallback"
        );
        // Absolute luminance, the commit latch, KMS atomic colour delivery
        // and external-element internalization all landed; a limitations
        // list that still named them contradicted the code and was useless
        // for deciding whether an enable is safe.
        assert_eq!(
            full["limitations"],
            serde_json::json!([
                "non_d65_chromatic_adaptation_unavailable",
                "hardware_lut_route_clips_hdr_headroom",
                "hdr_signalling_requires_software_per_output_delivery_route",
                "framebuffer_and_color_properties_paired_by_ordering_not_one_ioctl",
                "direct_scanout_blocked_while_scene_linear_active"
            ])
        );

        let unavailable = color_session_policy_json(
            std::slice::from_ref(&hdr),
            true,
            true,
            true,
            true,
            false,
            &REFUSED,
            false,
            false,
        );
        assert_eq!(
            unavailable["fallback_policy"]["route_observation"],
            "unavailable_on_backend"
        );
        let output_unavailable = output_color_policy_json(&hdr, None, true, true, false, false);
        assert_eq!(
            output_unavailable["delivery_route_observation"],
            "unavailable_on_backend"
        );

        let legacy = color_session_policy_json(
            &[hdr, sdr],
            true,
            false,
            false,
            false,
            true,
            &REFUSED,
            false,
            false,
        );
        assert_eq!(
            legacy["sdr_on_hdr_policy"],
            "legacy_sdr_passthrough_on_hdr_output"
        );
        assert_eq!(legacy["mixed_hdr_policy"], "safe_srgb_legacy_compositing");
        assert_eq!(
            legacy["blockers"],
            serde_json::json!([
                "advanced_color_management_disabled",
                "color_management_render_path_disabled",
                "scene_linear_compositing_inactive"
            ])
        );

        let configured_without_render_path = color_session_policy_json(
            &[output(None), output(None)],
            false,
            false,
            crate::config::scene_linear_render_path_requested(false, true),
            false,
            true,
            &REFUSED,
            false,
            false,
        );
        assert_eq!(
            configured_without_render_path["scene_linear_enabled"],
            false
        );
        assert_eq!(
            configured_without_render_path["delivery_capabilities"]["working_space"],
            "legacy_encoded_srgb"
        );
        assert_eq!(
            configured_without_render_path["delivery_capabilities"]["software_per_output_encode_regions"],
            false
        );

        let pq = output(Some(EdidHdrCapabilities {
            max_luminance_nits: 1000.0,
            min_luminance_nits: 0.05,
            max_frame_average_nits: 400.0,
            supports_bt2020: true,
            supports_pq: true,
            supports_hlg: false,
        }));
        let mut hlg = output(Some(EdidHdrCapabilities {
            max_luminance_nits: 1000.0,
            min_luminance_nits: 0.05,
            max_frame_average_nits: 400.0,
            supports_bt2020: true,
            supports_pq: false,
            supports_hlg: true,
        }));
        hlg.id = OutputId(3);
        hlg.name = "HDMI-A-2".into();
        hlg.identity = OutputIdentity::connector_only("HDMI-A-2");
        let pq_hlg = color_session_policy_json(
            &[pq, hlg],
            true,
            true,
            false,
            true,
            true,
            &REFUSED,
            false,
            false,
        );
        assert_eq!(pq_hlg["mixed_hdr_outputs"], false);
        assert_eq!(pq_hlg["heterogeneous_output_profiles"], true);
        assert_eq!(
            pq_hlg["mixed_hdr_policy"],
            "per_surface_transform_without_scene_linear_blending"
        );
        assert_eq!(
            pq_hlg["blockers"],
            serde_json::json!(["scene_linear_compositing_inactive"])
        );
    }

    #[test]
    fn optional_protocol_enabled_respects_config_and_env_overrides() {
        assert!(optional_protocol_enabled_from_flags(true, false, false));
        assert!(optional_protocol_enabled_from_flags(false, true, false));
        assert!(optional_protocol_enabled_from_flags(false, false, true));
        assert!(!optional_protocol_enabled_from_flags(false, false, false));
    }

    #[test]
    fn config_batch_parser_accepts_changes_array() {
        let changes = parse_config_batch_changes(&serde_json::json!({
            "changes": [
                {"key": "appearance.gap_px", "value": 8},
                {"key": "status_bar.show_bar", "value": false}
            ]
        }))
        .unwrap();

        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].0, "appearance.gap_px");
        assert_eq!(changes[0].1, serde_json::json!(8));
    }

    #[test]
    fn config_batch_parser_accepts_values_object() {
        let changes = parse_config_batch_changes(&serde_json::json!({
            "values": {
                "appearance.gap_px": 8,
                "status_bar.show_bar": false
            }
        }))
        .unwrap();

        assert_eq!(changes.len(), 2);
        assert!(
            changes.iter().any(|(key, value)| {
                key == "appearance.gap_px" && *value == serde_json::json!(8)
            })
        );
        assert!(changes.iter().any(|(key, value)| {
            key == "status_bar.show_bar" && *value == serde_json::json!(false)
        }));
    }

    #[test]
    fn config_batch_parser_allows_the_limit_and_rejects_before_entry_validation() {
        let allowed = (0..MAX_CONFIG_BATCH_CHANGES)
            .map(|index| {
                serde_json::json!({
                    "key": format!("test.key.{index}"),
                    "value": index,
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(
            parse_config_batch_changes(&serde_json::json!({ "changes": allowed }))
                .unwrap()
                .len(),
            MAX_CONFIG_BATCH_CHANGES
        );

        // The first entry is intentionally malformed. The size error must win,
        // proving an oversized atomic transaction is rejected before any entry
        // is parsed (and therefore before the handler can apply any of it).
        let oversized = (0..=MAX_CONFIG_BATCH_CHANGES)
            .map(|index| {
                if index == 0 {
                    serde_json::json!({ "value": "must not be parsed" })
                } else {
                    serde_json::json!({
                        "key": format!("test.key.{index}"),
                        "value": index,
                    })
                }
            })
            .collect::<Vec<_>>();
        let error =
            parse_config_batch_changes(&serde_json::json!({ "changes": oversized })).unwrap_err();
        assert_eq!(
            error,
            format!(
                "set_config_batch: too many changes ({}; maximum {})",
                MAX_CONFIG_BATCH_CHANGES + 1,
                MAX_CONFIG_BATCH_CHANGES
            )
        );
    }

    #[test]
    fn config_batch_values_object_cannot_bypass_the_limit() {
        let values = (0..=MAX_CONFIG_BATCH_CHANGES)
            .map(|index| (format!("test.key.{index}"), serde_json::json!(index)))
            .collect::<serde_json::Map<_, _>>();

        let error =
            parse_config_batch_changes(&serde_json::json!({ "values": values })).unwrap_err();
        assert!(error.contains("too many changes"));
        assert!(error.contains(&MAX_CONFIG_BATCH_CHANGES.to_string()));
    }

    #[test]
    fn command_batch_parser_accepts_commands_array() {
        let commands = parse_command_batch_entries(&serde_json::json!({
            "commands": [
                {"command": "view", "args": {"tag": 1}},
                {"name": "focusstack", "args": {"value": -1}}
            ]
        }))
        .unwrap();

        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].0, "view");
        assert_eq!(commands[0].1, serde_json::json!({"tag": 1}));
        assert_eq!(commands[1].0, "focusstack");
        assert_eq!(commands[1].1, serde_json::json!({"value": -1}));
    }

    #[test]
    fn command_batch_parser_allows_the_limit_and_rejects_atomically_above_it() {
        let allowed = (0..MAX_COMMAND_BATCH_ENTRIES)
            .map(|index| serde_json::json!({ "command": "view", "args": { "tag": index } }))
            .collect::<Vec<_>>();
        assert_eq!(
            parse_command_batch_entries(&serde_json::json!({ "commands": allowed }))
                .unwrap()
                .len(),
            MAX_COMMAND_BATCH_ENTRIES
        );

        // A malformed first entry would normally fail entry validation. The
        // batch-size error winning here proves no prefix is parsed or returned
        // for execution.
        let oversized = (0..=MAX_COMMAND_BATCH_ENTRIES)
            .map(|index| {
                if index == 0 {
                    serde_json::json!({ "args": { "must_not_execute": true } })
                } else {
                    serde_json::json!({ "command": "view", "args": { "tag": index } })
                }
            })
            .collect::<Vec<_>>();
        let error =
            parse_command_batch_entries(&serde_json::json!({ "commands": oversized })).unwrap_err();
        assert_eq!(
            error,
            format!(
                "command_batch: too many commands ({}; maximum {})",
                MAX_COMMAND_BATCH_ENTRIES + 1,
                MAX_COMMAND_BATCH_ENTRIES
            )
        );
    }

    #[test]
    fn command_batch_parser_rejects_nested_batch() {
        let err = parse_command_batch_entries(&serde_json::json!({
            "commands": [
                {"command": "command_batch", "args": {"commands": []}}
            ]
        }))
        .unwrap_err();

        assert!(err.contains("cannot nest"));
    }

    #[test]
    fn tearing_reports_what_the_compositor_did_not_what_a_client_asked_for() {
        use crate::backend::api::PresentationOutputStatus;

        let status = |name: &str, asked: bool, vrr: bool, tearing: bool, blocker: Option<&str>| {
            PresentationOutputStatus {
                output_name: name.to_string(),
                client_asked_to_tear: asked,
                vrr,
                tearing,
                blocker: blocker.map(str::to_string),
            }
        };
        let render = |presentation: &[PresentationOutputStatus], hints: usize| {
            render_decisions_json(
                None,
                None,
                &[],
                None,
                hints,
                presentation,
                false,
                false,
                false,
                false,
                false,
                false,
            )
        };

        // A client asked and the compositor refused: demand is true, the
        // outcome is false, and the reason is the refusal — under v1 this
        // read as `active: true`, which described a frame that never
        // happened.
        let refused = render(
            &[status(
                "HDMI-A-1",
                true,
                true,
                false,
                Some("submission_cannot_request_async_flip"),
            )],
            1,
        );
        assert_eq!(refused["tearing"]["active"], false);
        assert_eq!(refused["tearing"]["client_demand"], true);
        assert_eq!(
            refused["tearing"]["reason"],
            "submission_cannot_request_async_flip"
        );
        assert_eq!(refused["tearing"]["outputs"][0]["output"], "HDMI-A-1");
        // VRR is a separate mechanism reading the same evidence, and it did
        // fire even though tearing did not.
        assert_eq!(refused["vrr"]["active"], true);
        assert_eq!(refused["vrr"]["outputs"][0]["enabled"], true);

        // The session's reason comes from an output that actually had a
        // client waiting, not from whichever output happens to be first.
        let mixed = render(
            &[
                status("DP-1", false, false, false, Some("no_client_asked_to_tear")),
                status(
                    "HDMI-A-1",
                    true,
                    true,
                    false,
                    Some("driver_cannot_flip_async"),
                ),
            ],
            1,
        );
        assert_eq!(mixed["tearing"]["reason"], "driver_cannot_flip_async");

        // And when a frame does tear, that is what it says.
        let torn = render(&[status("HDMI-A-1", true, true, true, None)], 1);
        assert_eq!(torn["tearing"]["active"], true);
        assert_eq!(torn["tearing"]["reason"], "async_page_flip_issued");

        // No outputs at all (every non-KMS backend): no demand, no outcome,
        // no fabricated reason.
        let none = render(&[], 0);
        assert_eq!(none["tearing"]["active"], false);
        assert_eq!(none["tearing"]["client_demand"], false);
        assert!(none["tearing"]["reason"].is_null());
        assert_eq!(none["vrr"]["active"], false);
    }

    #[test]
    fn render_decisions_reports_direct_scanout_blockers() {
        let decisions = render_decisions_json(
            Some(&serde_json::json!({
                "enabled": true,
                "active": false,
                "candidate_count": 1,
                "compositor_reason": "overlay present",
                "kms_outputs": [
                    {"output_name": "HDMI-A-1", "eligible": false, "reason": "cursor plane busy"}
                ]
            })),
            None,
            &[],
            None,
            0,
            &[],
            false,
            false,
            false,
            false,
            false,
            false,
        );

        assert_eq!(decisions["direct_scanout"]["active"], false);
        assert_eq!(decisions["direct_scanout"]["reason"], "overlay present");
        assert_eq!(
            decisions["direct_scanout"]["blockers"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn render_decisions_reports_hdr_without_capable_outputs() {
        let decisions = render_decisions_json(
            None,
            Some(&serde_json::json!({
                "current_strength": 3,
                "temporal_enabled": true,
                "temporal_reuse_rate_pct": 80.0
            })),
            &[serde_json::json!({"hdr_capable": false})],
            None,
            1,
            &[],
            true,
            true,
            true,
            true,
            true,
            false,
        );

        assert_eq!(decisions["blur"]["active"], true);
        assert_eq!(decisions["hdr"]["active"], serde_json::Value::Null);
        assert_eq!(
            decisions["hdr"]["active_observation"],
            "no_successful_presentation_observed"
        );
        assert_eq!(decisions["hdr"]["requested_on_capable_output"], false);
        assert_eq!(decisions["hdr"]["reason"], "no_hdr_capable_outputs");
        // schema v2: a client asking to tear is demand, not an outcome. With
        // no per-output verdicts there is nothing that tore.
        assert_eq!(decisions["schema_version"], 2);
        assert_eq!(decisions["tearing"]["active"], false);
        assert_eq!(decisions["tearing"]["client_demand"], true);
        assert_eq!(decisions["tearing"]["hint_count"], 1);
        assert_eq!(
            decisions["color_pipeline"]["active"],
            serde_json::Value::Null
        );
        assert_eq!(
            decisions["color_pipeline"]["active_observation"],
            "no_successful_presentation_observed"
        );
        assert_eq!(
            decisions["color_pipeline"]["scene_linear_target_active"],
            true
        );
        assert_eq!(
            decisions["color_pipeline"]["capability"],
            "normalized_linear_srgb_per_output_delivery"
        );
    }

    #[test]
    fn render_decisions_use_last_successful_color_delivery() {
        let delivery = serde_json::json!({
            "schema_version": 1,
            "last_policy_decision": {
                "sequence": 9,
                "composited_route": "software_per_output_regions",
                "linear_tail_safe": false,
                "linear_tail_blockers": ["cursor"]
            },
            "outputs": [{
                "output_name": "DP-1",
                "participating": true,
                "last_success": {
                    "policy_sequence": 9,
                    "route": "software_per_output_regions",
                    "hdr_metadata_active": false
                }
            }]
        });
        let decisions = render_decisions_json(
            None,
            None,
            &[serde_json::json!({"hdr_capable": true})],
            Some(&delivery),
            0,
            &[],
            true,
            false,
            true,
            true,
            true,
            true,
        );

        assert_eq!(decisions["hdr"]["active"], false);
        assert_eq!(
            decisions["hdr"]["active_observation"],
            "last_successful_presentation"
        );
        assert_eq!(
            decisions["hdr"]["reason"],
            "presented_without_kms_hdr_metadata"
        );
        assert_eq!(decisions["color_pipeline"]["active"], true);
        assert_eq!(
            decisions["color_pipeline"]["observed_routes"]["software_per_output_regions"],
            1
        );
        assert_eq!(
            decisions["color_pipeline"]["linear_tail_observation"],
            "observed_current_policy"
        );
        assert_eq!(decisions["color_pipeline"]["linear_tail_safe"], false);
        assert_eq!(
            decisions["color_pipeline"]["linear_tail_blockers"],
            serde_json::json!(["cursor"])
        );
        assert_eq!(
            decisions["color_pipeline"]["linear_tail_inventory_consistent"],
            true
        );
        assert_eq!(
            decisions["color_pipeline"]["linear_tail_inventory_state"],
            "observed_blocked"
        );
        assert_eq!(
            decisions["color_pipeline"]["linear_tail_inventory_issue"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn render_decisions_distinguish_unknown_and_inconsistent_tail_inventory() {
        let delivery = |decision: serde_json::Value| {
            serde_json::json!({
                "schema_version": 1,
                "last_policy_decision": decision,
                "outputs": []
            })
        };
        let render = |delivery: &serde_json::Value| {
            render_decisions_json(
                None,
                None,
                &[],
                Some(delivery),
                0,
                &[],
                true,
                false,
                true,
                true,
                true,
                true,
            )
        };

        let legacy = delivery(serde_json::json!({
            "sequence": 1,
            "linear_tail_safe": false
        }));
        let decisions = render(&legacy);
        assert_eq!(
            decisions["color_pipeline"]["linear_tail_observation"],
            "unknown_or_not_applicable"
        );
        assert_eq!(
            decisions["color_pipeline"]["linear_tail_inventory_consistent"],
            true
        );
        assert_eq!(
            decisions["color_pipeline"]["linear_tail_inventory_state"],
            "unknown"
        );
        assert_eq!(
            decisions["color_pipeline"]["linear_tail_inventory_issue"],
            serde_json::Value::Null
        );

        let inconsistent = delivery(serde_json::json!({
            "sequence": 2,
            "linear_tail_safe": true,
            "linear_tail_blockers": ["cursor", "cursor"]
        }));
        let decisions = render(&inconsistent);
        assert_eq!(
            decisions["color_pipeline"]["linear_tail_inventory_consistent"],
            false
        );
        assert_eq!(
            decisions["color_pipeline"]["linear_tail_inventory_issue"],
            "duplicate_blocker"
        );
        assert_eq!(
            decisions["color_pipeline"]["linear_tail_blockers"],
            serde_json::Value::Null
        );

        let malformed = delivery(serde_json::json!({
            "sequence": 3,
            "linear_tail_safe": false,
            "linear_tail_blockers": "cursor"
        }));
        let decisions = render(&malformed);
        assert_eq!(
            decisions["color_pipeline"]["linear_tail_observation"],
            "malformed"
        );
        assert_eq!(
            decisions["color_pipeline"]["linear_tail_inventory_consistent"],
            false
        );
        assert_eq!(
            decisions["color_pipeline"]["linear_tail_blockers"],
            serde_json::Value::Null
        );
        assert_eq!(
            decisions["color_pipeline"]["linear_tail_inventory_issue"],
            "non_array"
        );

        let future = delivery(serde_json::json!({
            "sequence": 4,
            "linear_tail_safe": false,
            "linear_tail_blockers": ["cursor", "future_blocker_2"]
        }));
        let decisions = render(&future);
        assert_eq!(
            decisions["color_pipeline"]["linear_tail_inventory_state"],
            "observed_blocked"
        );
        assert_eq!(
            decisions["color_pipeline"]["linear_tail_known_blocker_count"],
            1
        );
        assert_eq!(
            decisions["color_pipeline"]["linear_tail_unknown_blocker_count"],
            1
        );
        assert_eq!(
            decisions["color_pipeline"]["linear_tail_inventory_consistent"],
            true
        );

        let clear = delivery(serde_json::json!({
            "sequence": 5,
            "linear_tail_safe": true,
            "linear_tail_blockers": []
        }));
        assert_eq!(
            render(&clear)["color_pipeline"]["linear_tail_inventory_state"],
            "observed_clear"
        );
    }

    #[test]
    fn render_decisions_reflect_the_per_class_external_element_plan() {
        let delivery = |decision: serde_json::Value| {
            serde_json::json!({
                "schema_version": 1,
                "last_policy_decision": decision,
                "outputs": []
            })
        };
        let render = |delivery: &serde_json::Value| {
            render_decisions_json(
                None,
                None,
                &[],
                Some(delivery),
                0,
                &[],
                true,
                false,
                true,
                true,
                true,
                true,
            )
        };

        // Legacy decisions without the field report null, not a fabrication.
        let legacy = delivery(serde_json::json!({
            "sequence": 1,
            "linear_tail_safe": true,
            "linear_tail_blockers": []
        }));
        assert_eq!(
            render(&legacy)["color_pipeline"]["external_elements"],
            serde_json::Value::Null
        );

        let planned = delivery(serde_json::json!({
            "sequence": 2,
            "linear_tail_safe": false,
            "linear_tail_blockers": ["cursor", "overlay_layer_surface"],
            "external_elements": [
                {
                    "class": "cursor",
                    "visible": true,
                    "importable": true,
                    "assembly": "kms_external",
                    "blocker": "cursor",
                    "outputs": ["HDMI-A-1"],
                    "basis": "pointer_on_output"
                },
                {
                    "class": "overlay_layer_surface",
                    "visible": true,
                    "importable": false,
                    "assembly": "kms_external",
                    "blocker": "overlay_layer_surface",
                    "outputs": ["HDMI-A-1"],
                    "basis": "layer_overlaps_output"
                },
                {
                    "class": "session_lock_surface",
                    "visible": false,
                    "importable": false,
                    "assembly": "none",
                    "blocker": null,
                    "outputs": [],
                    "basis": "session_unlocked"
                },
                {
                    "class": "top_layer_surface",
                    "visible": true,
                    "importable": true,
                    "assembly": "common_linear",
                    "blocker": null,
                    "outputs": ["HDMI-A-1"],
                    "basis": "layer_overlaps_output"
                }
            ]
        }));
        let elements = render(&planned)["color_pipeline"]["external_elements"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(elements.len(), 4);
        assert_eq!(elements[0]["class"], "cursor");
        assert_eq!(elements[0]["blocker"], "cursor");
        assert_eq!(elements[1]["importable"], false);
        assert_eq!(elements[1]["basis"], "layer_overlaps_output");
        assert_eq!(elements[2]["assembly"], "none");
        assert_eq!(elements[2]["blocker"], serde_json::Value::Null);
        assert_eq!(elements[3]["assembly"], "common_linear");
        assert_eq!(elements[3]["blocker"], serde_json::Value::Null);

        // A structurally invalid payload collapses to null, like an invalid
        // blocker inventory.
        for bad in [
            serde_json::json!({"class": "Cursor", "visible": true, "importable": true,
                "assembly": "kms_external", "blocker": null, "outputs": [], "basis": "x"}),
            serde_json::json!({"class": "cursor", "visible": "yes", "importable": true,
                "assembly": "kms_external", "blocker": null, "outputs": [], "basis": "x"}),
            serde_json::json!({"class": "cursor", "visible": true, "importable": true,
                "assembly": "kms_external", "blocker": "Bad Name", "outputs": [], "basis": "x"}),
            serde_json::json!({"class": "cursor", "visible": true, "importable": true,
                "assembly": "compositor_internal", "blocker": null, "outputs": [], "basis": "x"}),
            serde_json::json!({"class": "cursor", "visible": true, "importable": true,
                "assembly": "kms_external", "blocker": null, "outputs": [1], "basis": "x"}),
        ] {
            let delivery = delivery(serde_json::json!({
                "sequence": 3,
                "linear_tail_safe": true,
                "linear_tail_blockers": [],
                "external_elements": [bad]
            }));
            assert_eq!(
                render(&delivery)["color_pipeline"]["external_elements"],
                serde_json::Value::Null
            );
        }
    }

    #[test]
    fn render_decisions_require_success_from_every_participating_output() {
        let delivery = serde_json::json!({
            "schema_version": 1,
            "last_policy_decision": {
                "sequence": 12,
                "composited_route": "kms_ctm_gamma_lut"
            },
            "outputs": [
                {
                    "output_name": "DP-1",
                    "participating": true,
                    "last_success": {
                        "policy_sequence": 12,
                        "route": "kms_ctm_gamma_lut",
                        "hdr_metadata_active": true
                    }
                },
                {
                    "output_name": "HDMI-A-1",
                    "participating": true,
                    "last_success": {
                        "policy_sequence": 11,
                        "route": "software_per_output_regions",
                        "hdr_metadata_active": false
                    }
                },
                {
                    "output_name": "DP-2",
                    "participating": false,
                    "last_success": {
                        "policy_sequence": 12,
                        "route": "kms_ctm_gamma_lut",
                        "hdr_metadata_active": true
                    }
                }
            ]
        });
        let decisions = render_decisions_json(
            None,
            None,
            &[serde_json::json!({"hdr_capable": true})],
            Some(&delivery),
            0,
            &[],
            true,
            false,
            true,
            true,
            true,
            true,
        );

        assert_eq!(decisions["hdr"]["active"], true);
        assert_eq!(
            decisions["hdr"]["active_observation"], "last_successful_presentation",
            "a positive observation is conclusive across the physically visible cohort"
        );
        assert_eq!(decisions["hdr"]["observed_output_count"], 2);
        assert_eq!(decisions["hdr"]["expected_output_count"], 2);
        assert_eq!(
            decisions["hdr"]["observed_policy_sequences"],
            serde_json::json!([11, 12]),
            "an older cohort can still be the latest frame physically visible on an output"
        );
        assert_eq!(decisions["color_pipeline"]["active"], true);

        let mut sdr_partial = delivery;
        sdr_partial["outputs"][0]["last_success"]["hdr_metadata_active"] = serde_json::json!(false);
        sdr_partial["outputs"][1]["last_success"] = serde_json::Value::Null;
        let decisions = render_decisions_json(
            None,
            None,
            &[serde_json::json!({"hdr_capable": true})],
            Some(&sdr_partial),
            0,
            &[],
            true,
            false,
            true,
            true,
            true,
            true,
        );
        assert_eq!(decisions["hdr"]["active"], serde_json::Value::Null);
        assert_eq!(decisions["hdr"]["reason"], "partial_output_observation");
    }

    /// The gate's configuration refusal serialises under the same wire name
    /// as the static advanced-colour-management entry; the session-level
    /// array names that reason once, static entries first, while the
    /// per-output list still reports every output.
    #[test]
    fn session_policy_blockers_name_each_reason_once() {
        let hdr = output(Some(EdidHdrCapabilities {
            max_luminance_nits: 1000.0,
            min_luminance_nits: 0.05,
            max_frame_average_nits: 400.0,
            supports_bt2020: true,
            supports_pq: true,
            supports_hlg: false,
        }));
        let refusals = [
            (
                "HDMI-A-1".to_string(),
                Some("advanced_color_management_disabled".to_string()),
            ),
            ("DP-1".to_string(), Some("linear_tail_unsafe".to_string())),
            (
                "DP-2".to_string(),
                Some("advanced_color_management_disabled".to_string()),
            ),
        ];
        let policy = color_session_policy_json(
            std::slice::from_ref(&hdr),
            true,
            true,
            true,
            false,
            true,
            &refusals,
            false,
            false,
        );
        assert_eq!(
            policy["blockers"],
            serde_json::json!(["advanced_color_management_disabled", "linear_tail_unsafe"])
        );
        assert_eq!(
            policy["hdr_enable_refusals"].as_array().map(Vec::len),
            Some(3)
        );

        // With the static reason absent the gate's name is still reported,
        // once, so the dedup did not swallow the gate.
        let gate_only = color_session_policy_json(
            std::slice::from_ref(&hdr),
            true,
            true,
            true,
            true,
            true,
            &refusals[..1],
            false,
            false,
        );
        assert_eq!(
            gate_only["blockers"],
            serde_json::json!(["advanced_color_management_disabled"])
        );
    }

    /// `bluetooth_pairing_done` re-reads the device list so the row shows
    /// the bond that took, but never over a scan already in flight: the
    /// replaced handle would detach a live discovery and drop its result.
    ///
    /// The guard has to ask whether work is really running, not whether the
    /// slot holds a handle: a job whose thread the OS refused never publishes
    /// anything, and coalescing on its presence would hold the re-read shut
    /// for as long as that handle sat there. Needles are assembled at runtime
    /// and the haystack is the handler alone, so this cannot match its own
    /// text.
    #[test]
    fn pairing_done_reread_coalesces_on_a_running_scan() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        let handler = SOURCE
            .split_once(&format!("if name == \"{}\"", "bluetooth_pairing_done"))
            .expect("bluetooth_pairing_done handler")
            .1
            .split_once(&format!("if name == \"{}\"", "clipboard_record"))
            .expect("the command handled after bluetooth_pairing_done")
            .0;
        let guard = format!("connectivity::{}(", "job_in_flight");
        let stale = format!("bluetooth_scan.{}()", "is_none");
        let reread = format!("connectivity::{}()", "start_device_scan");
        assert!(
            !handler.contains(&stale),
            "the re-read coalesces on a handle existing, not on work running"
        );
        let guard_at = handler.find(&guard).expect("the re-read is guarded");
        let reread_at = handler
            .find(&reread)
            .expect("the post-pairing re-read is still started");
        assert!(
            guard_at < reread_at,
            "bluetooth_pairing_done must test for a running scan before starting the re-read"
        );
    }

    /// The two control reads a bar polls warm the Shell Hub's coalesced
    /// snapshot before answering, so each reply can come from memory and no
    /// query forks `powerprofilesctl` or `wpctl` under the frame. Warming is
    /// only honest once the snapshot carries what the query returns:
    /// `power_status_json` reads `power_profiles` and `audio_devices_json`
    /// reads `audio_inventory`, both of which `ControlCenterSnapshot::read`
    /// samples on the worker.
    #[test]
    fn control_read_queries_warm_the_control_snapshot() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        let queries = SOURCE
            .split_once(&format!("pub(crate) fn {}(", "handle_ipc_query"))
            .expect("query dispatcher")
            .1;
        let refresh = format!("{}(", "ensure_control_snapshot_refresh");
        // Match the canonical name inside an alias pattern, then stop at the
        // next arm. Formatting may put the arrow on this line or a later one.
        let arm_of = |query: &str| {
            let arm = queries
                .split_once(&format!("\"{query}\""))
                .unwrap_or_else(|| panic!("{query} arm"))
                .1;
            arm.split_once("\n            \"")
                .map_or(arm, |(body, _)| body)
                .to_string()
        };
        for query in ["get_power_status", "get_audio_devices", "get_mic_mute"] {
            assert!(
                arm_of(query).contains(&refresh),
                "{query} must start the coalesced snapshot refresh before answering"
            );
        }
    }

    /// A backend built from the shared dummy ops, so the pairing IPC arms can
    /// be driven end to end without a display.
    struct PairingIpcBackend {
        window_ops: DummyWindowOps,
        input_ops: DummyInputOps,
        property_ops: DummyPropertyOps,
        output_ops: DummyOutputOps,
        key_ops: DummyKeyOps,
        cursor_provider: DummyCursorProvider,
        color_allocator: DummyColorAllocator,
        /// Every OSD card the WM asked for, in order.
        osd: Vec<(OsdKind, u8)>,
        /// Every text the WM put on the clipboard, in order.
        clipboard: Vec<String>,
        /// Every HDR metadata request, in order.
        hdr: Vec<(OutputId, bool)>,
        /// Every MIC chip push, in order.
        mic_indicator: Vec<bool>,
        /// Every toast the WM pushed, in order.
        toasts: Vec<crate::backend::api::ToastNotification>,
    }

    impl PairingIpcBackend {
        fn new() -> Self {
            Self {
                window_ops: DummyWindowOps,
                input_ops: DummyInputOps,
                property_ops: DummyPropertyOps,
                output_ops: DummyOutputOps,
                key_ops: DummyKeyOps,
                cursor_provider: DummyCursorProvider,
                color_allocator: DummyColorAllocator,
                osd: Vec::new(),
                clipboard: Vec::new(),
                hdr: Vec::new(),
                mic_indicator: Vec::new(),
                toasts: Vec::new(),
            }
        }

        fn toast_titles(&self) -> Vec<&str> {
            self.toasts
                .iter()
                .map(|toast| toast.title.as_str())
                .collect()
        }
    }

    impl CompositorBenchmark for PairingIpcBackend {}
    impl BackendDiagnostics for PairingIpcBackend {}
    impl CompositorControl for PairingIpcBackend {}
    impl CompositorMedia for PairingIpcBackend {
        fn compositor_set_mic_indicator(&mut self, active: bool) {
            self.mic_indicator.push(active);
        }
    }
    impl CompositorWorkspaceEffects for PairingIpcBackend {
        fn compositor_show_osd(&mut self, kind: OsdKind, percent: u8) {
            self.osd.push((kind, percent));
        }

        fn compositor_push_toast(&mut self, toast: crate::backend::api::ToastNotification) {
            self.toasts.push(toast);
        }
    }
    impl CompositorWindowEffects for PairingIpcBackend {}
    impl CompositorAnnotation for PairingIpcBackend {}
    impl DisplayControl for PairingIpcBackend {
        fn set_hdr_metadata(
            &mut self,
            output: OutputId,
            enabled: bool,
        ) -> Result<(), BackendError> {
            self.hdr.push((output, enabled));
            Ok(())
        }
    }
    impl RenderScheduler for PairingIpcBackend {}

    impl Backend for PairingIpcBackend {
        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }

        fn root_window(&self) -> Option<WindowId> {
            Some(WindowId::from_raw(0))
        }

        fn as_any(&self) -> &dyn Any {
            self
        }

        fn check_existing_wm(&self) -> Result<(), BackendError> {
            Ok(())
        }

        fn window_ops(&self) -> &dyn crate::backend::api::WindowOps {
            &self.window_ops
        }

        fn input_ops(&self) -> &dyn crate::backend::api::InputOps {
            &self.input_ops
        }

        fn property_ops(&self) -> &dyn crate::backend::api::PropertyOps {
            &self.property_ops
        }

        fn output_ops(&self) -> &dyn crate::backend::api::OutputOps {
            &self.output_ops
        }

        fn key_ops(&self) -> &dyn crate::backend::api::KeyOps {
            &self.key_ops
        }

        fn key_ops_mut(&mut self) -> &mut dyn crate::backend::api::KeyOps {
            &mut self.key_ops
        }

        fn cursor_provider(&mut self) -> &mut dyn crate::backend::api::CursorProvider {
            &mut self.cursor_provider
        }

        fn color_allocator(&mut self) -> &mut dyn ColorAllocator {
            &mut self.color_allocator
        }

        fn run(&mut self, _handler: &mut dyn EventHandler) -> Result<(), BackendError> {
            Ok(())
        }

        fn set_clipboard_text(&mut self, text: &str) -> bool {
            self.clipboard.push(text.to_string());
            true
        }
    }

    #[test]
    fn lightweight_status_summaries_match_full_queries_with_sparse_orders() {
        let mut backend = PairingIpcBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        jwm.state = WMState::new();

        let mut monitor = WMMonitor::new();
        monitor.num = 7;
        monitor.tag_set[0] = 0b101;
        let monitor_key = jwm.state.monitors.insert(monitor);
        let stale_monitor = jwm.state.monitors.insert(WMMonitor::new());
        jwm.state.monitors.remove(stale_monitor);
        jwm.state.monitor_order = vec![monitor_key, stale_monitor, monitor_key];
        jwm.state.sel_mon = Some(monitor_key);

        let mut focused = WMClient::new(WindowId::from_raw(0x11));
        focused.mon = Some(monitor_key);
        focused.state.tags = 0b001;
        focused.state.is_floating = true;
        focused.state.is_hidden = true;
        focused.state.is_sticky = true;
        focused.state.is_fullscreen = true;
        focused.state.is_pip = true;
        let focused_key = jwm.state.clients.insert(focused);

        let mut urgent = WMClient::new(WindowId::from_raw(0x22));
        urgent.mon = Some(monitor_key);
        urgent.state.tags = 0b100;
        urgent.state.demands_attention = true;
        let urgent_key = jwm.state.clients.insert(urgent);
        let stale_client = jwm
            .state
            .clients
            .insert(WMClient::new(WindowId::from_raw(0x33)));
        jwm.state.clients.remove(stale_client);
        jwm.state.client_order = vec![focused_key, stale_client, urgent_key, focused_key];
        jwm.state
            .monitor_clients
            .insert(monitor_key, vec![focused_key, stale_client, urgent_key]);
        jwm.state.monitors[monitor_key].sel = Some(focused_key);

        let monitors = jwm.query_monitors(&backend);
        let monitor_summary = jwm.monitors_status_summary();
        assert_eq!(monitor_summary["count"], monitors.len());
        assert_eq!(
            monitor_summary["focused"],
            serde_json::json!(
                monitors
                    .iter()
                    .find(|monitor| monitor.focused)
                    .map(|monitor| monitor.num)
            )
        );

        let workspaces = jwm.query_workspaces(&backend);
        let workspace_summary = jwm.workspaces_status_summary();
        assert_eq!(workspace_summary["count"], workspaces.len());
        assert_eq!(
            workspace_summary["focused_count"],
            workspaces
                .iter()
                .filter(|workspace| workspace.focused)
                .count()
        );

        let windows = jwm.query_windows(&backend);
        let window_summary = jwm.windows_status_summary();
        assert_eq!(window_summary["count"], windows.len());
        assert_eq!(
            window_summary["focused_id"],
            serde_json::json!(
                windows
                    .iter()
                    .find(|window| window.is_focused)
                    .map(|window| window.id)
            )
        );

        let tree = jwm.query_tree(&backend);
        let tree_summary = jwm.tree_status_summary();
        assert_eq!(tree_summary["monitor_count"], tree.len());
        assert_eq!(
            tree_summary["window_count"],
            tree.iter().map(|node| node.window_count).sum::<usize>()
        );

        for flag in [
            "floating",
            "minimized",
            "sticky",
            "urgent",
            "fullscreen",
            "pip",
        ] {
            let summary = jwm.windows_flag_status_summary(flag);
            let matches = |window: &&crate::ipc::WindowInfo| match flag {
                "floating" => window.is_floating,
                "minimized" => window.is_minimized,
                "sticky" => window.is_sticky,
                "urgent" => window.is_urgent || window.demands_attention,
                "fullscreen" => window.is_fullscreen,
                "pip" => window.is_pip,
                _ => unreachable!(),
            };
            assert_eq!(
                summary["count"],
                windows.iter().filter(matches).count(),
                "{flag}"
            );
            assert_eq!(
                summary["focused_id"],
                serde_json::json!(
                    windows
                        .iter()
                        .find(|window| window.is_focused && matches(window))
                        .map(|window| window.id)
                ),
                "{flag}"
            );
        }

        let runtime = jwm.query_runtime_status(&backend);
        assert_eq!(runtime.counts.windows, windows.len());
        assert_eq!(runtime.counts.monitors, monitors.len());
        assert_eq!(runtime.counts.workspaces, workspaces.len());
        for (flag, summary) in [
            ("floating", runtime.floating.as_ref()),
            ("minimized", runtime.minimized.as_ref()),
            ("sticky", runtime.sticky.as_ref()),
            ("urgent", runtime.urgent.as_ref()),
            ("fullscreen", runtime.fullscreen.as_ref()),
            ("pip", runtime.pip.as_ref()),
        ] {
            let expected = jwm.windows_flag_status_summary(flag);
            assert_eq!(summary, Some(&expected), "batched runtime flag {flag}");
        }
    }

    #[test]
    fn active_tag_count_handles_empty_and_full_width_safely() {
        assert_eq!(active_tag_count(u32::MAX, 0), 0);
        assert_eq!(active_tag_count(0b1011, 3), 2);
        assert_eq!(active_tag_count(u32::MAX, 32), 32);
        assert_eq!(active_tag_count(u32::MAX, usize::MAX), 32);
    }

    /// A jwm with the Bluetooth picker open and an inbound window armed — the
    /// state the picker's `a` key leaves behind while `jwm-bridge accept` runs.
    fn jwm_with_armed_inbound_window(cookie: &str) -> (PairingIpcBackend, Jwm) {
        let mut backend = PairingIpcBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        jwm.features.system_ui =
            SystemUiState::bluetooth_picker("Accepting incoming requests (60s)");
        jwm.features.bluetooth_pairing = Some(PairingSession::inbound(
            cookie.to_string(),
            std::time::Instant::now(),
        ));
        (backend, jwm)
    }

    #[test]
    fn bluetooth_pairing_failed_closes_the_armed_window_and_says_why() {
        let (mut backend, mut jwm) = jwm_with_armed_inbound_window("cookie-1");
        // Not reachable over the wire — a real prompt binds the window to its
        // device, and `matches_failure` then refuses the frame — but the
        // prompt cancel is unconditional, exactly like `done`'s.
        jwm.features
            .system_ui
            .prompt_bluetooth_pairing(&PairingPrompt::Confirm { passkey: 42 }, "MX Master 3S");
        assert!(jwm.features.system_ui.pairing_prompt().is_some());

        let response = jwm.handle_ipc_command(
            &mut backend,
            "bluetooth_pairing_failed",
            &serde_json::json!({"cookie": "cookie-1", "error": "no system bus"}),
        );

        assert!(response.success, "{response:?}");
        assert!(jwm.features.bluetooth_pairing.is_none());
        assert!(jwm.features.system_ui.pairing_prompt().is_none());
        let items = jwm.features.system_ui.overlay_parts().items;
        assert!(
            items.iter().any(|row| {
                row.contains("Cannot accept incoming requests") && row.contains("no system bus")
            }),
            "the picker says why the window never armed: {items:?}"
        );
    }

    #[test]
    fn bluetooth_pairing_failed_with_another_cookie_keeps_the_window_armed() {
        let (mut backend, mut jwm) = jwm_with_armed_inbound_window("cookie-1");

        let response = jwm.handle_ipc_command(
            &mut backend,
            "bluetooth_pairing_failed",
            &serde_json::json!({"cookie": "someone-elses-cookie", "error": "no system bus"}),
        );

        assert!(!response.success);
        assert_eq!(
            response.error.as_deref(),
            Some("bluetooth_pairing_failed: not the active pairing session")
        );
        assert!(jwm.features.bluetooth_pairing.is_some());
        let items = jwm.features.system_ui.overlay_parts().items;
        assert!(
            items
                .iter()
                .any(|row| row.contains("Accepting incoming requests")),
            "the picker still claims the armed window: {items:?}"
        );
    }

    #[test]
    fn bluetooth_pairing_failed_without_a_session_is_refused() {
        let mut backend = PairingIpcBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();

        let response = jwm.handle_ipc_command(
            &mut backend,
            "bluetooth_pairing_failed",
            &serde_json::json!({"cookie": "cookie-1", "error": "no system bus"}),
        );

        assert!(!response.success);
        assert_eq!(
            response.error.as_deref(),
            Some("bluetooth_pairing_failed: no pairing session is active")
        );
    }

    #[test]
    fn bluetooth_pairing_failed_rejects_malformed_frames() {
        let (mut backend, mut jwm) = jwm_with_armed_inbound_window("cookie-1");

        for args in [
            serde_json::json!({}),
            serde_json::json!({"error": "no system bus"}),
            serde_json::json!({"cookie": ""}),
            serde_json::json!({"cookie": 7}),
        ] {
            let response = jwm.handle_ipc_command(&mut backend, "bluetooth_pairing_failed", &args);
            assert!(!response.success, "accepted {args}");
            let error = response.error.unwrap_or_default();
            assert!(error.starts_with("bluetooth_pairing_failed: "), "{error}");
            assert!(
                jwm.features.bluetooth_pairing.is_some(),
                "a malformed frame ended the window: {args}"
            );
        }
    }

    /// The reply is the queued contract: an immediate ack carrying the
    /// estimate, never the worker's read-back. A set needs no confirmed base
    /// — the estimate is the asked target itself — so the snapshot cache and
    /// the mic card move synchronously and deterministically.
    #[test]
    fn set_mic_mute_queues_the_set_and_draws_the_estimate() {
        let mut backend = PairingIpcBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let control_queue = crate::jwm::features::system_controls::TestControlQueueGuard::install();

        let response = jwm.handle_ipc_command(
            &mut backend,
            "set_mic_mute",
            &serde_json::json!({"muted": true}),
        );

        assert!(response.success, "{response:?}");
        assert_eq!(
            jwm.features
                .control_snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.mic_muted),
            Some(true),
            "the estimate is cached for the control-center Input row"
        );
        assert_eq!(
            backend.osd.as_slice(),
            &[(OsdKind::MicMute(true), 0)],
            "the ack draws the mic card; the percent slot is unused"
        );

        // An absolute target, not a flip: asking again re-queues and redraws
        // with the new target, whatever the current estimate says.
        let response = jwm.handle_ipc_command(
            &mut backend,
            "set_mic_mute",
            &serde_json::json!({"muted": false}),
        );

        assert!(response.success, "{response:?}");
        assert_eq!(
            jwm.features
                .control_snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.mic_muted),
            Some(false)
        );
        assert_eq!(
            backend.osd.as_slice(),
            &[(OsdKind::MicMute(true), 0), (OsdKind::MicMute(false), 0)]
        );
        assert_eq!(
            control_queue.requests(),
            vec![
                crate::jwm::features::system_controls::ControlRequest::MicMuteSet(true),
                crate::jwm::features::system_controls::ControlRequest::MicMuteSet(false),
            ],
            "both absolute targets are submitted to the controls queue"
        );
    }

    #[test]
    fn set_mic_mute_rejects_malformed_frames() {
        let mut backend = PairingIpcBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();

        for args in [
            serde_json::json!({}),
            serde_json::json!({"muted": "yes"}),
            serde_json::json!({"muted": 1}),
            serde_json::json!({"muted": null}),
        ] {
            let response = jwm.handle_ipc_command(&mut backend, "set_mic_mute", &args);
            assert!(!response.success, "accepted {args}");
            assert_eq!(
                response.error.as_deref(),
                Some("set_mic_mute: expected boolean field 'muted'"),
                "{args}"
            );
        }
        assert!(backend.osd.is_empty(), "a malformed frame drew a card");
        assert_eq!(
            jwm.features
                .control_snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.mic_muted),
            None,
            "a malformed frame moved the cached flag"
        );
    }

    /// The arm must queue on the controls worker — never run the audio
    /// helper on the event thread an IPC call runs on — and its no-tool
    /// answer is the mic-mute key's own error string, so a caller cannot
    /// tell which path found no tool. The haystacks are the arm and the key
    /// handler alone, and the needles are assembled at runtime so this test
    /// cannot match its own source.
    #[test]
    fn set_mic_mute_queues_and_mirrors_the_key_paths_no_tool_answer() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        let arm = SOURCE
            .split_once(&format!("if name == \"{}\"", "set_mic_mute"))
            .expect("set_mic_mute handler")
            .1
            .split_once(&format!("if name == \"{}\"", "set_power_profile"))
            .expect("the command handled after set_mic_mute")
            .0;
        let queue = format!("self.{}(", "queue_mic_request");
        assert!(
            arm.contains(&queue),
            "set_mic_mute no longer queues on the controls worker ({queue})"
        );
        for primitive in ["mic_set_mute", "mic_toggle_mute", "mic_mute_state"] {
            let needle = format!("system_controls::{primitive}(");
            assert!(
                !arm.contains(&needle),
                "set_mic_mute regained a blocking tool call: {needle}"
            );
        }

        const TOGGLES: &str = include_str!("features/toggles.rs");
        let key = TOGGLES
            .split_once("pub(crate) fn toggle_mic_mute")
            .expect("toggle_mic_mute")
            .1
            .split_once("pub(crate) fn brightness_adjust")
            .expect("the end of toggle_mic_mute")
            .0;
        let error = format!("no working {} (wpctl/pactl/amixer)", "audio control");
        assert!(key.contains(&error), "the key path's no-tool answer moved");
        assert!(
            arm.contains(&error),
            "set_mic_mute must answer the key path's no-tool error ({error})"
        );
    }

    /// A successful `set_power_profile` must raise the same labeled OSD the
    /// Hub Left/Right cycle does — a silent IPC switch left the card for
    /// keybindings only. The switch is queued on the controls worker like
    /// `set_mic_mute`: `powerprofilesctl` (a Python D-Bus client) must never
    /// run on the event thread an IPC call is answered on, and the card is
    /// the queued acknowledgement the worker's re-read later corrects. The
    /// haystack is the arm alone so this pin cannot match its own source.
    #[test]
    fn set_power_profile_raises_the_osd_on_success() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        let arm = SOURCE
            .split_once(&format!("if name == \"{}\"", "set_power_profile"))
            .expect("set_power_profile handler")
            .1
            .split_once(&format!("if name == \"{}\"", "media_control"))
            .expect("the command handled after set_power_profile")
            .0;
        for blocking in ["set_profile", "profiles"] {
            let needle = format!("power::{blocking}(");
            assert!(
                !arm.contains(&needle),
                "set_power_profile regained a blocking tool call: {needle}"
            );
        }
        let queue = arm
            .find(&format!(".{}(", "queue_power_profile_request"))
            .expect("set_power_profile no longer queues on the controls worker");
        let show = arm
            .find(&format!("self.{}(", "show_power_profile_osd"))
            .expect("set_power_profile no longer raises a Power Profile OSD");
        // The card is the acknowledgement of a queued switch — it must sit
        // after the name check and the queueing, never before a rejection.
        let reject = arm
            .find("unknown power profile")
            .expect("the unknown-name rejection");
        assert!(
            reject < queue && queue < show,
            "the OSD must only follow a validated, queued switch"
        );
    }

    /// A successful `set_audio_device` must queue on the controls worker —
    /// never block the IPC thread on `set_audio_device` / a verifying
    /// re-read — and leave the named OSD to `adopt_audio_switch` after the
    /// re-read confirms the switch took. Needles are built at runtime.
    #[test]
    fn set_audio_device_queues_like_the_picker() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        let arm = SOURCE
            .split_once(&format!("if name == \"{}\"", "set_audio_device"))
            .expect("set_audio_device handler")
            .1
            .split_once(&format!("if name == \"{}\"", "set_mic_mute"))
            .expect("the command handled after set_audio_device")
            .0;
        let queue = format!("{}(", "queue_control_request");
        assert!(
            arm.contains(&queue) && arm.contains("ControlRequest::AudioSetDefault"),
            "set_audio_device must queue AudioSetDefault on the controls worker ({queue})"
        );
        for needle in [
            "system_controls::set_audio_device(",
            "compositor_show_osd",
            "OsdKind::AudioDevice",
            "did not keep",
        ] {
            assert!(
                !arm.contains(needle),
                "set_audio_device regained a sync confirmed path: {needle}"
            );
        }
        assert!(
            arm.contains("no working audio control"),
            "set_audio_device must mirror the no-tool answer when the worker is absent"
        );
    }

    /// `get_mic_mute` answers the cached flag — never invents unmuted — and
    /// seeding the flag through the same cache helper `set_mic_mute` uses is
    /// what a subsequent query reads. The seed bypasses the audio-tool peek
    /// so a parallel suite that once marked the tool absent cannot starve
    /// this read.
    #[test]
    fn get_mic_mute_answers_the_cached_flag() {
        let mut backend = PairingIpcBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        // Freeze the cache in a deterministic, freshly-observed unread state.
        // The query still exercises stale-while-revalidate, but cannot race a
        // real PipeWire/PulseAudio/ALSA probe on the test host.
        jwm.features.control_snapshot = None;
        jwm.features.control_snapshot_job = None;
        jwm.features.control_snapshot_refreshed_at = Some(std::time::Instant::now());

        let unread = jwm.handle_ipc_query("get_mic_mute", &serde_json::json!({}), &backend);
        assert!(unread.success, "{unread:?}");
        assert_eq!(
            unread.data.as_ref().and_then(|data| data.get("muted")),
            Some(&serde_json::Value::Null),
            "never invent unmuted when the flag was never read"
        );

        jwm.cache_control_mic_mute(true);

        let muted = jwm.handle_ipc_query("get_mic_mute", &serde_json::json!({}), &backend);
        assert!(muted.success, "{muted:?}");
        assert_eq!(
            muted.data.as_ref().and_then(|data| data.get("muted")),
            Some(&serde_json::Value::Bool(true)),
            "the query reads the same estimate the cache helper published"
        );
    }

    /// Caching a shown mic flag publishes `audio/mic` so the `audio` topic
    /// carries the same value `get_mic_mute` answers. The haystack is the
    /// cache helper alone; the event name is assembled at runtime so this
    /// test cannot match its own source.
    #[test]
    fn caching_mic_mute_publishes_audio_mic() {
        const TOGGLES: &str = include_str!("features/toggles.rs");
        let helper = TOGGLES
            .split_once("pub(crate) fn cache_control_mic_mute")
            .expect("cache_control_mic_mute")
            .1
            .split_once("pub(crate) fn cache_control_power_profiles")
            .expect("the end of cache_control_mic_mute")
            .0;
        let event = format!("\"{}/{}\"", "audio", "mic");
        let broadcast = format!("self.{}(", "broadcast_ipc_event");
        assert!(
            helper.contains(&broadcast) && helper.contains(&event),
            "cache_control_mic_mute must publish {event} ({broadcast})"
        );
        // Revert-to-unread stays on mutate: a null is not an event payload.
        let poll = TOGGLES
            .split_once("pub(crate) fn poll_control_feedback")
            .expect("poll_control_feedback")
            .1
            .split_once("fn adopt_audio_switch")
            .expect("the end of poll_control_feedback")
            .0;
        assert!(
            poll.contains("snapshot.mic_muted = None"),
            "revert-to-unread must clear without inventing an audio/mic bool"
        );
    }

    /// Only an absent `enabled` means "on". A value that is present but not
    /// a JSON boolean used to read as `true`, so a script that sent
    /// `"false"` or `0` to turn HDR off latched it on and got a success.
    #[test]
    fn set_hdr_metadata_rejects_a_non_boolean_enabled() {
        let mut backend = PairingIpcBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();

        for enabled in [
            serde_json::json!("false"),
            serde_json::json!(0),
            serde_json::Value::Null,
        ] {
            let args = serde_json::json!({"output": "Virtual-1", "enabled": enabled});
            let response = jwm.handle_ipc_command(&mut backend, "set_hdr_metadata", &args);
            assert!(!response.success, "accepted {args}");
            assert_eq!(
                response.error.as_deref(),
                Some("set_hdr_metadata: 'enabled' must be a boolean"),
                "{args}"
            );
        }
        assert!(
            backend.hdr.is_empty(),
            "a malformed frame reached the backend"
        );

        for (args, expected) in [
            (serde_json::json!({"output": "Virtual-1"}), true),
            (
                serde_json::json!({"output": "Virtual-1", "enabled": false}),
                false,
            ),
        ] {
            let response = jwm.handle_ipc_command(&mut backend, "set_hdr_metadata", &args);
            assert!(response.success, "{args}: {response:?}");
            assert_eq!(
                response.data.as_ref().and_then(|data| data.get("enabled")),
                Some(&serde_json::Value::Bool(expected)),
                "{args}"
            );
        }
        assert_eq!(
            backend.hdr,
            vec![(OutputId(0), true), (OutputId(0), false)],
            "an absent field still means on, and a real false turns it off"
        );
    }

    /// Only an absent `index` means the newest entry. A string, negative or
    /// fractional index used to fall back to entry 0, put the newest entry
    /// back on the clipboard and report success.
    #[test]
    fn clipboard_copy_rejects_a_malformed_index() {
        let mut backend = PairingIpcBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        for (captured_unix_ms, text) in [(1, "oldest"), (2, "middle"), (3, "newest")] {
            assert!(jwm.features.clipboard.record(text, captured_unix_ms));
        }

        for index in [
            serde_json::json!("1"),
            serde_json::json!(-1),
            serde_json::json!(1.5),
            serde_json::Value::Null,
        ] {
            let args = serde_json::json!({ "index": index });
            let response = jwm.handle_ipc_command(&mut backend, "clipboard_copy", &args);
            assert!(!response.success, "accepted {args}");
            assert_eq!(
                response.error.as_deref(),
                Some("clipboard_copy: 'index' must be a non-negative integer"),
                "{args}"
            );
        }
        assert!(
            backend.clipboard.is_empty(),
            "a malformed index put an entry on the clipboard"
        );

        // No index first: re-recording the newest entry keeps the order, so
        // the real index that follows still names the entry it did before.
        let response =
            jwm.handle_ipc_command(&mut backend, "clipboard_copy", &serde_json::json!({}));
        assert!(response.success, "{response:?}");
        let response = jwm.handle_ipc_command(
            &mut backend,
            "clipboard_copy",
            &serde_json::json!({"index": 2}),
        );
        assert!(response.success, "{response:?}");
        assert_eq!(
            backend.clipboard,
            vec!["newest".to_string(), "oldest".to_string()],
            "no index copies the newest entry, and a real index copies that entry"
        );
    }

    /// `get_tree` and `get_windows` fill the same `is_focused` field, so they
    /// must agree: one focused window, the selected monitor's selection. Each
    /// monitor's own selection used to count, marking one window per monitor.
    #[test]
    fn get_tree_marks_only_the_focused_window_like_get_windows() {
        let mut backend = PairingIpcBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        jwm.state = WMState::new();
        let mut monitors = Vec::new();
        let mut clients = Vec::new();
        for (num, raw_window) in [(0, 0x10), (1, 0x20)] {
            let mut monitor = WMMonitor::new();
            monitor.num = num;
            let monitor_key = jwm.state.monitors.insert(monitor);
            let mut client = WMClient::new(WindowId::from_raw(raw_window));
            client.mon = Some(monitor_key);
            let client_key = jwm.state.clients.insert(client);
            // Every monitor keeps a selection of its own; only one of them
            // is where input goes.
            if let Some(monitor) = jwm.state.monitors.get_mut(monitor_key) {
                monitor.sel = Some(client_key);
            }
            jwm.state.monitor_order.push(monitor_key);
            jwm.state.client_order.push(client_key);
            jwm.state
                .monitor_clients
                .insert(monitor_key, vec![client_key]);
            monitors.push(monitor_key);
            clients.push(client_key);
        }
        jwm.state.sel_mon = Some(monitors[1]);

        let focused_in_tree: Vec<u64> = jwm
            .query_tree(&backend)
            .iter()
            .flat_map(|node| node.windows.iter())
            .filter(|window| window.is_focused)
            .map(|window| window.id)
            .collect();
        let focused_in_windows: Vec<u64> = jwm
            .query_windows(&backend)
            .iter()
            .filter(|window| window.is_focused)
            .map(|window| window.id)
            .collect();

        assert_eq!(
            focused_in_tree,
            vec![0x20],
            "one focused window in the tree"
        );
        assert_eq!(focused_in_tree, focused_in_windows);
    }

    #[test]
    fn sparse_tag_counts_preserve_high_bits_and_configured_bounds() {
        let mut state = WMState::new();
        let monitor = state.monitors.insert(WMMonitor::new());
        let mut keys = Vec::new();
        for tags in [0, 1, 1 << 31, (1 << 31) | 1, u32::MAX] {
            let mut client = WMClient::new(WindowId::from_raw(keys.len() as u64 + 1));
            client.state.tags = tags;
            client.state.demands_attention = true;
            keys.push(state.clients.insert(client));
        }
        let stale = state.clients.insert(WMClient::new(WindowId::from_raw(99)));
        state.clients.remove(stale);
        keys.extend([stale, keys[1]]);
        state.monitor_clients.insert(monitor, keys);
        let empty = std::collections::HashSet::new();
        let struts = std::collections::HashSet::new();
        for tag_count in [0, 1, 9, 32, 35] {
            let actual = monitor_tag_client_counts(
                &state, monitor, tag_count, &empty, &empty, &struts, "", 1,
            );
            assert_eq!(actual.len(), tag_count);
            for (index, counts) in actual.iter().enumerate() {
                if index < 32 {
                    assert_eq!(
                        *counts,
                        tag_client_counts(
                            &state,
                            monitor,
                            1 << index,
                            &empty,
                            &empty,
                            &struts,
                            "",
                            1,
                        )
                    );
                } else {
                    assert_eq!(*counts, super::TagClientCounts::default());
                }
            }
        }
    }

    #[test]
    fn tree_counts_match_projected_flags_with_duplicates_and_attention() {
        let mut backend = PairingIpcBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        jwm.state = WMState::new();
        let monitor = jwm.state.monitors.insert(WMMonitor::new());
        jwm.state.monitor_order = vec![monitor];
        let mut client = WMClient::new(WindowId::from_raw(77));
        client.mon = Some(monitor);
        client.state.tags = 1;
        client.state.is_floating = true;
        client.state.is_hidden = true;
        client.state.is_sticky = true;
        client.state.is_fullscreen = true;
        client.state.is_pip = true;
        client.state.is_maximized_vert = true;
        client.state.is_above = true;
        client.state.is_below = true;
        client.state.is_fixed = true;
        client.state.is_dock = true;
        client.state.is_desktop = true;
        client.state.never_focus = true;
        client.state.demands_attention = true;
        client.state.skip_taskbar = true;
        client.state.skip_pager = true;
        client.state.no_decorations = true;
        client.state.is_drag_floating = true;
        client.state.is_swallowed = true;
        client.state.maximize_restore_tiled = true;
        let key = jwm.state.clients.insert(client);
        let stale = jwm
            .state
            .clients
            .insert(WMClient::new(WindowId::from_raw(99)));
        jwm.state.clients.remove(stale);
        jwm.state.client_order = vec![key];
        jwm.state
            .monitor_clients
            .insert(monitor, vec![key, stale, key]);
        jwm.scratchpads.insert("scratch".into(), key);
        let tree = jwm.query_tree(&backend);
        let node = &tree[0];
        assert_eq!(node.window_count, 2);
        assert_eq!(node.urgent_count, 0);
        assert_eq!(node.demands_attention_count, 2);
        // A single maximized axis does not make WindowInfo::is_maximized true.
        assert_eq!(node.maximized_count, 0);
        let json = serde_json::to_value(node).unwrap();
        let flags = [
            ("floating_count", "is_floating"),
            ("minimized_count", "is_minimized"),
            ("sticky_count", "is_sticky"),
            ("urgent_count", "is_urgent"),
            ("fullscreen_count", "is_fullscreen"),
            ("pip_count", "is_pip"),
            ("maximized_count", "is_maximized"),
            ("above_count", "is_above"),
            ("below_count", "is_below"),
            ("scratchpad_count", "is_scratchpad"),
            ("tabbed_count", "is_tabbed"),
            ("fixed_count", "is_fixed"),
            ("dock_count", "is_dock"),
            ("desktop_count", "is_desktop"),
            ("never_focus_count", "never_focus"),
            ("demands_attention_count", "demands_attention"),
            ("skip_taskbar_count", "skip_taskbar"),
            ("skip_pager_count", "skip_pager"),
            ("no_decorations_count", "no_decorations"),
            ("drag_float_count", "is_drag_floating"),
            ("swallowed_count", "is_swallowed"),
            ("on_view_count", "is_on_view"),
            ("maximize_promoted_count", "maximize_promoted"),
            ("strut_count", "has_strut"),
            ("status_bar_count", "is_status_bar"),
        ];
        for (count, flag) in flags {
            let expected = json["windows"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|window| window[flag] == true)
                .count();
            assert_eq!(json[count], expected, "{count}");
        }
    }

    #[test]
    fn indexed_window_positions_and_swallowers_preserve_first_matches() {
        let mut backend = PairingIpcBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        jwm.state = WMState::new();
        let monitor = jwm.state.monitors.insert(WMMonitor::new());
        let stale_monitor = jwm.state.monitors.insert(WMMonitor::new());
        jwm.state.monitors.remove(stale_monitor);
        let mut parent = WMClient::new(WindowId::from_raw(10));
        parent.mon = Some(stale_monitor);
        parent.state.is_swallowed = true;
        let parent = jwm.state.clients.insert(parent);
        let mut child = WMClient::new(WindowId::from_raw(20));
        child.mon = Some(monitor);
        child.swallowing = Some(parent);
        let first = jwm.state.clients.insert(child.clone());
        child.win = WindowId::from_raw(30);
        let second = jwm.state.clients.insert(child);
        let stale = jwm
            .state
            .clients
            .insert(WMClient::new(WindowId::from_raw(99)));
        jwm.state.clients.remove(stale);
        jwm.state.client_order = vec![stale, first, second, first, parent];
        jwm.state
            .monitor_clients
            .insert(monitor, vec![stale, second, first, first]);
        jwm.state
            .monitor_clients
            .insert(stale_monitor, vec![stale, parent, parent]);
        for order in [
            vec![stale, first, second, parent],
            vec![stale, second, first, parent],
        ] {
            jwm.state.client_order = order;
            let index = jwm.window_query_index(&backend);
            for key in [parent, first, second] {
                let actual = jwm
                    .window_info_indexed(&backend, key, false, Some(&index))
                    .unwrap();
                let expected = jwm.window_info(&backend, key, false).unwrap();
                assert_eq!(
                    serde_json::to_value(actual).unwrap(),
                    serde_json::to_value(expected).unwrap()
                );
            }
            assert_eq!(
                jwm.window_info_indexed(&backend, first, false, Some(&index))
                    .unwrap()
                    .stack_index,
                Some(2)
            );
            assert_eq!(
                jwm.window_info_indexed(&backend, parent, false, Some(&index))
                    .unwrap()
                    .stack_index,
                Some(1)
            );
        }
    }

    #[test]
    fn indexed_window_projection_matches_individual_projection() {
        let mut backend = PairingIpcBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        jwm.state = WMState::new();
        let monitor = jwm.state.monitors.insert(WMMonitor::new());
        jwm.state.monitor_order.push(monitor);
        let unlisted_monitor = jwm.state.monitors.insert(WMMonitor::new());
        let mut keys = Vec::new();
        for raw in [0x41, 0x42] {
            let mut client = WMClient::new(WindowId::from_raw(raw));
            client.mon = Some(monitor);
            client.state.tags = 1;
            let key = jwm.state.clients.insert(client);
            jwm.state.client_order.push(key);
            keys.push(key);
        }
        jwm.state.monitor_clients.insert(monitor, keys.clone());
        let mut unlisted_client = WMClient::new(WindowId::from_raw(0x43));
        unlisted_client.mon = Some(unlisted_monitor);
        unlisted_client.state.tags = 1;
        let unlisted_key = jwm.state.clients.insert(unlisted_client);
        jwm.state.client_order.push(unlisted_key);
        jwm.state
            .monitor_clients
            .insert(unlisted_monitor, vec![keys[0], unlisted_key]);
        keys.push(unlisted_key);
        jwm.state.monitors[monitor].sel = Some(keys[1]);
        jwm.state.sel_mon = Some(monitor);
        jwm.scratchpads.insert("terminal".into(), keys[0]);
        jwm.scratchpads.insert("terminal-alias".into(), keys[0]);

        let index = jwm.window_query_index(&backend);
        for &key in &keys {
            let focused = key == keys[1];
            let individual = jwm.window_info(&backend, key, focused).unwrap();
            let indexed = jwm
                .window_info_indexed(&backend, key, focused, Some(&index))
                .unwrap();
            assert_eq!(
                serde_json::to_value(indexed).unwrap(),
                serde_json::to_value(individual).unwrap()
            );
        }
    }

    /// A per-test scratch directory, so parallel tests never share a path.
    fn scratch_dir(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "jwm-ipc-handler-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        path
    }

    /// The probe blocks the event thread, and only success used to be
    /// cached, so a recording left without its moov atom forked ffprobe on
    /// every status poll until the next recording. A rejected file is probed
    /// again only once it changes on disk.
    #[test]
    fn a_rejected_recording_is_not_probed_again_until_it_changes() {
        let scratch = scratch_dir("recording-probe");
        let path = scratch.join("broken.mp4");
        let path_text = path.to_str().unwrap();
        let probes = std::cell::Cell::new(0_u32);
        let reject = |_: &str| {
            probes.set(probes.get() + 1);
            Some(false)
        };
        let accept = |_: &str| {
            probes.set(probes.get() + 1);
            Some(true)
        };
        // The slot lives on the recorder state (`rejected_probe`), which a
        // new recording clears.
        let mut rejected = None;

        assert!(
            !recording_output_is_valid(path_text, &mut rejected, reject),
            "missing file"
        );
        std::fs::write(&path, b"").unwrap();
        assert!(
            !recording_output_is_valid(path_text, &mut rejected, reject),
            "empty file"
        );
        assert_eq!(probes.get(), 0, "a missing or empty file is never probed");

        std::fs::write(&path, [0_u8; 16]).unwrap();
        assert!(!recording_output_is_valid(path_text, &mut rejected, reject));
        assert!(!recording_output_is_valid(path_text, &mut rejected, reject));
        assert!(!recording_output_is_valid(path_text, &mut rejected, accept));
        assert_eq!(
            probes.get(),
            1,
            "an unchanged rejected file was probed again"
        );

        // The finalization worker's flush grows the file: that earns a probe.
        let mut grown = std::fs::read(&path).unwrap();
        grown.extend_from_slice(b"moov");
        std::fs::write(&path, grown).unwrap();
        assert!(recording_output_is_valid(path_text, &mut rejected, accept));
        assert_eq!(probes.get(), 2, "a changed file was not probed again");
        assert!(rejected.is_none(), "a passing file clears the rejection");

        std::fs::remove_dir_all(&scratch).unwrap();
    }

    /// A probe that never finished (ffprobe timed out under heavy I/O, or
    /// fork failed) judged nothing, yet it used to be cached as a rejection.
    /// A finalized file never changes again, so that recording stayed
    /// `finalized: false` and `recording/finalized` never fired. Only a real
    /// verdict is remembered now; the next poll probes again.
    #[test]
    fn an_unfinished_recording_probe_is_retried_on_the_next_poll() {
        let scratch = scratch_dir("recording-probe-timeout");
        let path = scratch.join("finished.mp4");
        let path_text = path.to_str().unwrap();
        std::fs::write(&path, [0_u8; 16]).unwrap();
        let probes = std::cell::Cell::new(0_u32);
        let timed_out = |_: &str| {
            probes.set(probes.get() + 1);
            None
        };
        let accept = |_: &str| {
            probes.set(probes.get() + 1);
            Some(true)
        };
        let mut rejected = None;

        assert!(!recording_output_is_valid(
            path_text,
            &mut rejected,
            timed_out
        ));
        assert!(rejected.is_none(), "an unfinished probe was cached");
        assert!(recording_output_is_valid(path_text, &mut rejected, accept));
        assert_eq!(probes.get(), 2, "the unchanged file was not probed again");

        std::fs::remove_dir_all(&scratch).unwrap();
    }

    /// ffprobe's own answer is a verdict, and so is a missing binary (it
    /// would fail the same way on every poll). A timeout or any other start
    /// failure judged nothing.
    #[test]
    fn ffprobe_verdict_separates_a_verdict_from_a_probe_that_never_finished() {
        use std::os::unix::process::ExitStatusExt;

        assert_eq!(
            ffprobe_verdict(Ok(std::process::ExitStatus::from_raw(0))),
            Some(true)
        );
        assert_eq!(
            ffprobe_verdict(Ok(std::process::ExitStatus::from_raw(1 << 8))),
            Some(false)
        );
        assert_eq!(
            ffprobe_verdict(Err(std::io::ErrorKind::NotFound.into())),
            Some(false)
        );
        assert_eq!(
            ffprobe_verdict(Err(std::io::ErrorKind::TimedOut.into())),
            None
        );
        assert_eq!(
            ffprobe_verdict(Err(std::io::ErrorKind::WouldBlock.into())),
            None
        );
    }

    /// A topic whose first segment is no registered event family never
    /// delivers anything, yet the ack used to be a bare success. The ack now
    /// names those topics; registered families, full event names and `*`
    /// are never reported.
    #[test]
    fn unknown_subscription_topics_names_only_topics_no_event_can_match() {
        let topics = |names: &[&str]| {
            names
                .iter()
                .map(|name| name.to_string())
                .collect::<Vec<_>>()
        };

        assert_eq!(
            unknown_subscription_topics(&topics(&[
                "window",
                " tag ",
                "workspace",
                "*",
                "bluetooth/pairing_response",
                "media/status",
                "",
            ])),
            Vec::<String>::new()
        );
        assert_eq!(
            unknown_subscription_topics(&topics(&[
                "windows",
                "tag",
                "tags",
                " windows ",
                "*/window",
                "Window",
            ])),
            topics(&["windows", "tags", "*/window", "Window"]),
            "typos are named once each, trimmed like the stored topic"
        );

        let junk: Vec<String> = (0..MAX_REPORTED_UNKNOWN_TOPICS * 4)
            .map(|index| format!("junk-{index}"))
            .collect();
        assert_eq!(
            unknown_subscription_topics(&junk).len(),
            MAX_REPORTED_UNKNOWN_TOPICS,
            "the report is bounded"
        );
    }

    /// A key stop no longer joins the recorder on the event thread: the MIC
    /// chip clears at once, and the stopped toast waits for the frame tick
    /// to see the file finalized.
    #[test]
    fn a_key_stopped_recording_is_reported_once_its_file_is_finalized() {
        use std::sync::atomic::Ordering;

        let mut backend = PairingIpcBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let (release, finalize) = std::sync::mpsc::channel::<()>();
        jwm.features.audio_recording =
            crate::jwm::features::audio_recording::AudioRecordingState::recording_for_test(
                "/tmp/jwm-key-stop.wav",
                move |stop| {
                    // Stands in for the header rewrite still running after the
                    // stop; the bound only keeps a regression from hanging.
                    let _ = finalize.recv_timeout(std::time::Duration::from_secs(30));
                    if stop.load(Ordering::Acquire) {
                        Ok(())
                    } else {
                        Err("finalized without being asked to stop".into())
                    }
                },
            );

        jwm.toggle_audio_recording(&mut backend, &crate::jwm::types::WMArgEnum::Int(0))
            .expect("the key stop");

        assert!(!jwm.features.audio_recording.active);
        assert!(jwm.features.audio_recording.is_finalizing());
        assert_eq!(
            backend.mic_indicator,
            vec![false],
            "the chip clears at once"
        );
        assert!(
            backend.toasts.is_empty(),
            "nothing to report before the file is done"
        );
        jwm.poll_audio_recording(&mut backend);
        assert!(
            backend.toasts.is_empty(),
            "the tick never waits on the recorder"
        );

        release.send(()).unwrap();
        while jwm.features.audio_recording.is_finalizing() {
            jwm.poll_audio_recording(&mut backend);
            std::thread::yield_now();
        }
        assert_eq!(
            backend.toast_titles(),
            vec!["\u{f130}  Audio recording stopped"]
        );
        assert_eq!(backend.toasts[0].body, "/tmp/jwm-key-stop.wav");
    }

    /// A recorder that ended on its own (a USB microphone unplugged) used to
    /// leave the MIC chip up and idle inhibited until the next keypress; the
    /// frame tick stops it and says why.
    #[test]
    fn the_tick_stops_a_recorder_that_died_on_its_own() {
        let mut backend = PairingIpcBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        jwm.features.audio_recording =
            crate::jwm::features::audio_recording::AudioRecordingState::recording_for_test(
                "/tmp/jwm-died.wav",
                |_stop| Err("audio capture failed: No such device".into()),
            );

        while jwm.features.audio_recording.active {
            jwm.poll_audio_recording(&mut backend);
            std::thread::yield_now();
        }

        assert_eq!(backend.mic_indicator, vec![false]);
        assert_eq!(
            backend.toast_titles(),
            vec!["\u{f130}  Audio recording failed"]
        );
        assert_eq!(backend.toasts[0].urgency, 2, "through do-not-disturb");
        assert_eq!(
            backend.toasts[0].body,
            "audio capture failed: No such device"
        );
        jwm.poll_audio_recording(&mut backend);
        assert_eq!(backend.toasts.len(), 1, "reported once");
    }

    /// IPC `stop_audio_recording` still confirms finalization, including for
    /// a recording the key stopped a moment earlier.
    #[test]
    fn the_ipc_stop_collects_a_recording_the_key_left_finalizing() {
        let mut backend = PairingIpcBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        jwm.features.audio_recording =
            crate::jwm::features::audio_recording::AudioRecordingState::recording_for_test(
                "/tmp/jwm-ipc-stop.wav",
                |_stop| Ok(()),
            );
        jwm.begin_stopping_audio_recording(&mut backend)
            .expect("the key stop");

        let response =
            jwm.handle_ipc_command(&mut backend, "stop_audio_recording", &serde_json::json!({}));

        assert!(response.success, "{response:?}");
        assert_eq!(
            response
                .data
                .as_ref()
                .and_then(|data| data.get("was_active")),
            Some(&serde_json::Value::Bool(true))
        );
        assert!(!jwm.features.audio_recording.is_finalizing());
        assert_eq!(
            backend.toast_titles(),
            vec!["\u{f130}  Audio recording stopped"]
        );
    }

    /// The subscribe ack, which used to carry no data, names the unknown
    /// topics, what the server stored, and what its bounds dropped, with the
    /// reason — a truncated subscription used to be invisible to the client.
    #[test]
    fn the_subscribe_ack_names_what_was_stored_and_what_was_dropped() {
        use crate::ipc_server::{DroppedTopic, DroppedTopicReason, SubscriptionOutcome};

        let outcome = SubscriptionOutcome {
            subscribed: vec!["window".into(), "windows".into()],
            dropped: vec![DroppedTopic {
                topic: "window".into(),
                reason: DroppedTopicReason::Duplicate,
            }],
            dropped_total: 1,
        };

        let ack = super::subscribe_ack(&["windows".to_string()], &outcome);

        assert_eq!(
            ack,
            serde_json::json!({
                "unknown_topics": ["windows"],
                "subscribed": ["window", "windows"],
                "dropped": [{"topic": "window", "reason": "duplicate"}],
                "dropped_total": 1,
            })
        );
    }

    /// `set_power_profile` used to read the driver's list whenever the
    /// control snapshot was older than its two-second window, which is almost
    /// every scripted call: one more `powerprofilesctl` start-up on the event
    /// thread before the set. A cached list that offers the name now answers
    /// even when stale; only a fresh list may reject a name.
    #[test]
    fn cached_power_profiles_answer_a_listed_name_even_when_stale() {
        let listed = ["power-saver".to_string(), "balanced".to_string()];

        assert!(cached_power_profiles_answer(
            Some(&listed),
            false,
            "balanced"
        ));
        assert!(cached_power_profiles_answer(
            Some(&listed),
            true,
            "balanced"
        ));
        assert!(
            cached_power_profiles_answer(Some(&listed), true, "performance"),
            "a fresh list rejects without another read"
        );
        assert!(
            !cached_power_profiles_answer(Some(&listed), false, "performance"),
            "a stale list that lacks the name is read again before rejecting"
        );
        assert!(!cached_power_profiles_answer(None, true, "balanced"));
        assert!(!cached_power_profiles_answer(None, false, "balanced"));

        // The arm consults the cache, and what the cache cannot answer is
        // read by the control-center worker for the retry — never inline.
        const SOURCE: &str = include_str!("ipc_handler.rs");
        let arm = SOURCE
            .split_once(&format!("if name == \"{}\"", "set_power_profile"))
            .expect("set_power_profile handler")
            .1
            .split_once(&format!("if name == \"{}\"", "media_control"))
            .expect("the command handled after set_power_profile")
            .0;
        let consult = arm
            .find(&format!("{}(", "cached_power_profiles_answer"))
            .expect("set_power_profile no longer consults the cached list");
        let read = arm
            .find(&format!("self.{}(", "ensure_control_snapshot_refresh"))
            .expect("the worker read of the driver's list");
        assert!(
            consult < read,
            "the worker read is started before the cache"
        );
        assert!(
            !arm.contains(&format!("power::{}()", "profiles")),
            "set_power_profile reads the driver's list on the event thread again"
        );
    }

    fn power_profile_test_jwm(
        backend: &mut PairingIpcBackend,
        profiles: Option<(&str, &[&str])>,
    ) -> Jwm {
        let mut jwm = Jwm::new_with_runtime_backend(backend, "test").unwrap();
        // A freshly read snapshot: nothing in these tests may start a real
        // `powerprofilesctl` or audio-tool read on the test host.
        jwm.features.control_snapshot_job = None;
        jwm.features.control_snapshot_refreshed_at = Some(std::time::Instant::now());
        jwm.features.control_snapshot = profiles.map(|(active, available)| {
            crate::jwm::features::system_controls::ControlCenterSnapshot {
                power_profiles: Some((
                    available.iter().map(|name| name.to_string()).collect(),
                    active.to_string(),
                )),
                ..Default::default()
            }
        });
        jwm
    }

    fn cached_power_profile(jwm: &Jwm) -> Option<String> {
        jwm.features
            .control_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.power_profiles.as_ref())
            .map(|(_, active)| active.clone())
    }

    const TEST_PROFILES: &[&str] = &["power-saver", "balanced", "performance"];

    #[test]
    fn set_power_profile_queues_the_switch_and_draws_it() {
        use crate::jwm::features::system_controls::{ControlRequest, TestControlQueueGuard};

        let mut backend = PairingIpcBackend::new();
        let mut jwm = power_profile_test_jwm(&mut backend, Some(("balanced", TEST_PROFILES)));
        let control_queue = TestControlQueueGuard::install();

        let response = jwm.handle_ipc_command(
            &mut backend,
            "set_power_profile",
            &serde_json::json!({"profile": "performance"}),
        );

        assert!(response.success, "{response:?}");
        assert_eq!(
            control_queue.requests(),
            vec![ControlRequest::PowerProfileSet("performance".into())],
            "the set runs on the controls worker, not on this thread"
        );
        assert_eq!(cached_power_profile(&jwm).as_deref(), Some("performance"));
        assert_eq!(
            backend.osd.as_slice(),
            &[(OsdKind::PowerProfile("performance".into()), 0)]
        );

        // A name the driver does not offer is still refused with the list,
        // and nothing is queued or drawn for it.
        let response = jwm.handle_ipc_command(
            &mut backend,
            "set_power_profile",
            &serde_json::json!({"profile": "turbo"}),
        );
        assert!(!response.success);
        let error = response.error.unwrap_or_default();
        assert!(
            error.contains("unknown power profile") && error.contains("balanced"),
            "{error}"
        );
        assert_eq!(control_queue.requests().len(), 1);
        assert_eq!(backend.osd.len(), 1);
    }

    #[test]
    fn set_power_profile_answers_honestly_before_the_first_read() {
        use crate::jwm::features::system_controls::TestControlQueueGuard;

        let mut backend = PairingIpcBackend::new();
        let mut jwm = power_profile_test_jwm(&mut backend, None);
        jwm.features.control_snapshot_refreshed_at = None;
        // A read "in flight" that never lands keeps the arm's refresh from
        // starting a real one on the test host.
        jwm.features.control_snapshot_job =
            Some(crate::jwm::features::connectivity::BackgroundJob::refused());
        let control_queue = TestControlQueueGuard::install();

        let response = jwm.handle_ipc_command(
            &mut backend,
            "set_power_profile",
            &serde_json::json!({"profile": "balanced"}),
        );
        assert!(!response.success);
        let error = response.error.unwrap_or_default();
        assert!(error.contains("not been read yet"), "{error}");

        // Once a read has landed and found no profile control, that is the
        // answer, as before.
        jwm.features.control_snapshot_refreshed_at = Some(std::time::Instant::now());
        let response = jwm.handle_ipc_command(
            &mut backend,
            "set_power_profile",
            &serde_json::json!({"profile": "balanced"}),
        );
        assert!(!response.success);
        assert_eq!(
            response.error.as_deref(),
            Some("this machine has no power profile control")
        );
        assert!(control_queue.requests().is_empty());
        assert!(backend.osd.is_empty());
    }

    #[test]
    fn the_worker_readback_decides_what_the_power_profile_row_and_card_show() {
        use crate::jwm::features::system_controls::{PowerProfileReport, TestControlQueueGuard};

        let mut backend = PairingIpcBackend::new();
        let mut jwm = power_profile_test_jwm(&mut backend, Some(("balanced", TEST_PROFILES)));
        let _control_queue = TestControlQueueGuard::install();
        for profile in ["performance", "power-saver"] {
            let response = jwm.handle_ipc_command(
                &mut backend,
                "set_power_profile",
                &serde_json::json!({ "profile": profile }),
            );
            assert!(response.success, "{response:?}");
        }
        let list = || {
            TEST_PROFILES
                .iter()
                .map(|name| name.to_string())
                .collect::<Vec<_>>()
        };

        // The first switch's answer arrives while the second is queued: it
        // must not roll the row back from the newer pick.
        jwm.adopt_power_profile_report(
            &PowerProfileReport {
                seq: 1,
                asked: "performance".into(),
                asked_ok: true,
                profiles: Some((list(), "performance".into())),
            },
            std::time::Instant::now(),
        );
        assert_eq!(cached_power_profile(&jwm).as_deref(), Some("power-saver"));

        // The covering answer says the driver kept `balanced`: the row and
        // the still-live card follow the re-read, not the request.
        jwm.adopt_power_profile_report(
            &PowerProfileReport {
                seq: 2,
                asked: "power-saver".into(),
                asked_ok: true,
                profiles: Some((list(), "balanced".into())),
            },
            std::time::Instant::now(),
        );
        assert_eq!(cached_power_profile(&jwm).as_deref(), Some("balanced"));
        jwm.flush_system_ui(&mut backend);
        assert_eq!(
            backend.osd.last(),
            Some(&(OsdKind::PowerProfile("balanced".into()), 0)),
            "the card is refreshed with the profile really in effect"
        );
    }

    /// Waves 251–300 contract pins: get_config key expansion, effect/magnifier/
    /// peek polish, tab_bar selected_id, TreeNode counts.
    #[test]
    fn evolve7h_waves_251_300_ipc_contract_pins() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        let config = SOURCE
            .split_once("fn query_config_subset")
            .expect("query_config_subset")
            .1
            .split_once("fn query_tree")
            .expect("query_tree follows")
            .0;
        for key in [
            "hdr_enabled",
            "idle_dim_secs",
            "idle_dim_level",
            "night_light",
            "night_light_temp",
            "night_light_start",
            "night_light_end",
            "night_light_transition_mins",
            "remember_closed_placement",
            "waterlily_enabled",
            "waterlily_opacity",
        ] {
            assert!(
                config.contains(&format!("\"{key}\"")),
                "get_config must expose {key}"
            );
        }

        let effect = SOURCE
            .split_once("\"get_effect_status\"")
            .expect("get_effect_status arm")
            .1
            .split_once("\"get_magnifier\" | \"get_mag\" =>")
            .expect("get_magnifier follows")
            .0;
        assert!(effect.contains("\"magnifier_radius\""));
        assert!(effect.contains("\"compositor_active\""));

        let magnifier = SOURCE
            .split_once("\"get_magnifier\" | \"get_mag\" =>")
            .expect("get_magnifier arm")
            .1
            .split_once("\"get_peek\" | \"get_pk\" =>")
            .expect("get_peek follows")
            .0;
        assert!(magnifier.contains("\"radius\""));

        let peek = SOURCE
            .split_once("\"get_peek\" | \"get_pk\" =>")
            .expect("get_peek arm")
            .1
            .split_once("\"get_hdr_status\" | \"get_hdr\" =>")
            .expect("get_hdr_status follows")
            .0;
        assert!(peek.contains("\"compositor_active\""));

        let tab = SOURCE
            .split_once("fn query_focused_tab_bar")
            .expect("query_focused_tab_bar")
            .1
            .split_once("fn query_focused_layout")
            .expect("query_focused_layout follows")
            .0;
        assert!(
            tab.contains("\"selected_id\""),
            "get_tab_bar must report selected_id"
        );

        let tree = SOURCE
            .split_once("fn query_tree")
            .expect("query_tree")
            .1
            .split_once("fn broadcast_ipc_event")
            .expect("broadcast follows")
            .0;
        assert!(tree.contains("urgent_count"));
        assert!(tree.contains("floating_count"));
    }

    /// Waves 301–400 contract pins: Window/Monitor/Workspace fields, get_*
    /// aliases, status nests, get_config expansion, effect_status symmetry.
    #[test]
    fn evolve7h_waves_301_400_ipc_contract_pins() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const IPC: &str = include_str!("../ipc.rs");

        assert!(IPC.contains("pub hidden_x:"));
        assert!(IPC.contains("pub sync_counter:"));
        assert!(IPC.contains("pub sync_value:"));
        assert!(IPC.contains("pub sel_tags:"));
        assert!(IPC.contains("pub previous_tags:"));
        assert!(IPC.contains("pub cur_tag:"));
        assert!(IPC.contains("pub prev_tag:"));
        assert!(IPC.contains("pub output_connector:"));
        assert!(IPC.contains("pub minimized_count:"));
        assert!(IPC.contains("pub floating_count:"));
        assert!(IPC.contains("pub sticky_count:"));
        assert!(IPC.contains("pub control_center:"));
        assert!(IPC.contains("pub clipboard_picker:"));
        assert!(IPC.contains("pub session_lock:"));
        assert!(IPC.contains("pub resources:"));
        assert!(IPC.contains("pub connectivity:"));
        assert!(IPC.contains("pub capture:"));

        assert!(SOURCE.contains("hidden_x: client.geometry.hidden_x"));
        assert!(SOURCE.contains("sync_counter: client.state.sync_counter"));
        assert!(SOURCE.contains("output_connector:"));
        assert!(SOURCE.contains("previous_tags"));
        assert!(SOURCE.contains("minimized_count"));
        assert!(SOURCE.contains("fn query_focused_cfact"));
        assert!(SOURCE.contains("fn query_focused_show_bar"));
        assert!(SOURCE.contains("fn query_focused_prev_layout"));
        assert!(SOURCE.contains("fn query_selected_window"));
        assert!(SOURCE.contains("\"get_cfact\""));
        assert!(SOURCE.contains("\"get_show_bar\""));
        assert!(SOURCE.contains("\"get_selected\""));
        assert!(SOURCE.contains("\"get_focused_window\""));
        assert!(SOURCE.contains("\"get_prev_layout\""));

        let effect = SOURCE
            .split_once("\"get_effect_status\"")
            .expect("get_effect_status arm")
            .1
            .split_once("\"get_magnifier\" | \"get_mag\" =>")
            .expect("get_magnifier follows")
            .0;
        assert!(effect.contains("\"launcher\""));
        assert!(effect.contains("\"session_menu\""));
        assert!(effect.contains("\"notifications\""));

        let features = SOURCE
            .split_once("features: RuntimeFeatureStates {")
            .expect("RuntimeFeatureStates fill")
            .1
            .split_once("compositor_metrics:")
            .expect("metrics follows")
            .0;
        for flag in [
            "control_center",
            "clipboard_picker",
            "wifi_picker",
            "bluetooth_picker",
            "wallpaper_picker",
            "theme_picker",
            "audio_output_picker",
            "audio_input_picker",
            "media_players",
            "window_switcher",
            "session_lock",
        ] {
            assert!(
                features.contains(flag),
                "get_status features must include {flag}"
            );
        }

        let status = SOURCE
            .split_once("compositor_metrics: backend")
            .expect("status metrics")
            .1
            .split_once("fn window_info")
            .expect("window_info follows")
            .0;
        for nest in [
            "resources:",
            "connectivity:",
            "power:",
            "media:",
            "notifications:",
            "blur:",
            "hdr:",
            "capture:",
        ] {
            assert!(status.contains(nest), "get_status must nest {nest}");
        }

        let config = SOURCE
            .split_once("fn query_config_subset")
            .expect("query_config_subset")
            .1
            .split_once("fn query_tree")
            .expect("query_tree follows")
            .0;
        for key in [
            "expose_enabled",
            "peek_enabled",
            "tags_overview_enabled",
            "magnifier_enabled",
            "vrr_enabled",
            "swallow_enabled",
            "wallpaper",
            "persist_tags",
            "recording_bitrate",
        ] {
            assert!(
                config.contains(&format!("\"{key}\"")),
                "get_config must expose {key}"
            );
        }

        assert!(
            IPC.contains("\"set_cfact\"") && IPC.contains("\"toggle_floating\""),
            "dispatch registry must list underscore command twins"
        );
    }

    /// Waves 401–500 contract pins: Window/Monitor/Workspace/Tree leftovers,
    /// short query aliases, status nests, get_config polish, session v12 /
    /// layout show_bar, RuntimeFeatureStates monitor_lock/debug_hud.
    #[test]
    fn evolve7h_waves_401_500_ipc_contract_pins() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const IPC: &str = include_str!("../ipc.rs");
        const SESSION: &str = include_str!("session.rs");
        const PERSIST: &str = include_str!("layout/persist.rs");
        const EXPOSE: &str = include_str!("features/expose_plan.rs");
        const RESOURCES: &str = include_str!("features/resources.rs");
        const CONNECTIVITY: &str = include_str!("features/connectivity.rs");
        const PORTAL: &str = include_str!("../../portal/src/ipc.rs");

        assert!(IPC.contains("pub total_w:"));
        assert!(IPC.contains("pub total_h:"));
        assert!(IPC.contains("pub lt_symbol:"));
        assert!(IPC.contains("pub output_id:"));
        assert!(IPC.contains("pub urgent_count:"));
        assert!(IPC.contains("pub fullscreen_count:"));
        assert!(IPC.contains("pub monitor_lock:"));
        assert!(IPC.contains("pub debug_hud:"));
        assert!(IPC.contains("pub idle:"));
        assert!(IPC.contains("pub recording:"));
        assert!(IPC.contains("pub audio_recording:"));
        assert!(IPC.contains("pub clipboard:"));
        assert!(IPC.contains("\"toggle_scratchpad\""));
        assert!(IPC.contains("\"get_idle\""));
        assert!(IPC.contains("\"get_recording\""));
        assert!(IPC.contains("\"get_blur\""));
        assert!(IPC.contains("\"get_hdr\""));
        assert!(IPC.contains("\"get_power\""));
        assert!(IPC.contains("\"get_media\""));
        assert!(IPC.contains("\"get_waterlily\""));

        assert!(SOURCE.contains("total_w: client.total_width()"));
        assert!(SOURCE.contains("lt_symbol: m.lt_symbol.clone()"));
        assert!(SOURCE.contains("output_id: self.output_id_for_monitor"));
        assert!(SOURCE.contains("\"get_idle\""));
        assert!(SOURCE.contains("\"get_recording\""));
        assert!(SOURCE.contains("fn idle_status_summary"));
        assert!(SOURCE.contains("fn recording_status_summary"));
        assert!(SOURCE.contains("fn clipboard_status_summary"));
        assert!(SOURCE.contains("monitor_lock:"));
        assert!(SOURCE.contains("debug_hud: self.debug_hud_on"));
        assert!(SOURCE.contains("\"clipboard_history\""));
        assert!(SOURCE.contains("\"border_glow_enabled\""));
        assert!(SOURCE.contains("\"resource_rows\""));
        assert!(SOURCE.contains("\"new_client_position\""));

        assert!(SESSION.contains("fn migrate_snapshot_v11"));
        assert!(SESSION.contains("pub never_focus:"));
        assert!(SESSION.contains("pub old_state:"));
        assert!(SESSION.contains("pub pip_restore_sticky:"));
        assert!(SESSION.contains("pub remembers_closed_placement:"));

        assert!(PERSIST.contains("show_bar: pertag.show_bars.get(tag).copied()"));
        assert!(PERSIST.contains("if let Some(show_bar) = entry.show_bar"));

        assert!(EXPOSE.contains("ExposeNavDirection::Left"));
        assert!(EXPOSE.contains("ExposeNavDirection::Right"));

        assert!(RESOURCES.contains("\"available_kib\""));
        assert!(CONNECTIVITY.contains("\"scanning\""));

        assert!(PORTAL.contains("pub is_floating:"));
        assert!(PORTAL.contains("pub is_fullscreen:"));
        assert!(PORTAL.contains("pub is_minimized:"));
        assert!(PORTAL.contains("pub is_urgent:"));
        assert!(PORTAL.contains("pub monitor_name:"));
        assert!(PORTAL.contains("pub lt_symbol:"));
        assert!(PORTAL.contains("pub output_id:"));

        let status = SOURCE
            .split_once("compositor_metrics: backend")
            .expect("status metrics")
            .1
            .split_once("fn window_info")
            .expect("window_info follows")
            .0;
        for nest in ["idle:", "recording:", "audio_recording:", "clipboard:"] {
            assert!(status.contains(nest), "get_status must nest {nest}");
        }
    }

    #[test]
    fn evolve7h_waves_501_600_ipc_contract_pins() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const IPC: &str = include_str!("../ipc.rs");
        const SESSION: &str = include_str!("session.rs");
        const SWITCHER: &str = include_str!("features/switcher.rs");
        const INPUT: &str = include_str!("input_handler.rs");
        const PORTAL: &str = include_str!("../../portal/src/ipc.rs");

        assert!(IPC.contains("pub stack_index:"));
        assert!(IPC.contains("pub pip_count:"));
        assert!(IPC.contains("pub maximized_count:"));
        assert!(IPC.contains("pub above_count:"));
        assert!(IPC.contains("pub scratchpad_count:"));
        assert!(IPC.contains("pub tabbed_count:"));
        assert!(IPC.contains("pub waterlily:"));
        assert!(IPC.contains("pub session_lock:"));
        assert!(IPC.contains("\"get_notif\""));
        assert!(IPC.contains("\"get_effects\""));
        assert!(IPC.contains("\"get_lock\""));
        assert!(IPC.contains("\"minimize_window\""));
        assert!(IPC.contains("\"persist_session\""));

        assert!(SOURCE.contains("fn waterlily_status_summary"));
        assert!(SOURCE.contains("fn session_lock_status_summary"));
        assert!(SOURCE.contains("\"focus_follows_new_window\""));
        assert!(SOURCE.contains("\"border_gradient_enabled\""));
        assert!(SOURCE.contains("\"gesture_swipe\""));
        assert!(SOURCE.contains("\"control_center\": self.features.system_ui.is_control_center()"));

        assert!(SESSION.contains("fn migrate_snapshot_v12"));
        assert!(SESSION.contains("pub old_border_w:"));

        assert!(SWITCHER.contains("4 | 6 => SwitcherPress::Browse(-1)"));
        assert!(SWITCHER.contains("5 | 7 => SwitcherPress::Browse(1)"));

        let compact: String = INPUT.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(compact.contains("MouseButton::Other(4)|MouseButton::Other(6)"));
        assert!(compact.contains("keys::KEY_Up|keys::KEY_Left"));

        assert!(PORTAL.contains("pub is_sticky:"));
        assert!(PORTAL.contains("pub total_w:"));
        assert!(PORTAL.contains("pub window_count:"));

        let status = SOURCE
            .split_once("compositor_metrics: backend")
            .expect("status metrics")
            .1
            .split_once("fn window_info")
            .expect("window_info follows")
            .0;
        for nest in [
            "waterlily:",
            "night_light:",
            "magnifier:",
            "peek:",
            "expose:",
            "gesture:",
            "wayland:",
            "dnd:",
            "session_lock:",
        ] {
            assert!(status.contains(nest), "get_status must nest {nest}");
        }
    }

    #[test]
    fn evolve7h_waves_601_700_ipc_contract_pins() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const IPC: &str = include_str!("../ipc.rs");
        const SESSION: &str = include_str!("session.rs");
        const DISPATCH: &str = include_str!("event_dispatcher.rs");
        const PORTAL: &str = include_str!("../../portal/src/ipc.rs");

        assert!(IPC.contains("pub scratchpad_count:"));
        assert!(IPC.contains("pub tabbed_count:"));
        assert!(IPC.contains("pub urgent_count:"));
        assert!(IPC.contains("pub tearing:"));
        assert!(IPC.contains("pub xwayland:"));
        assert!(IPC.contains("pub scrolling:"));
        assert!(IPC.contains("pub color_management:"));
        assert!(IPC.contains("pub system_ui:"));
        assert!(IPC.contains("\"get_caps\""));
        assert!(IPC.contains("\"get_pads\""));
        assert!(IPC.contains("\"get_mag\""));
        assert!(IPC.contains("\"get_perf\""));
        assert!(IPC.contains("\"get_res\""));
        assert!(IPC.contains("\"get_wins\""));
        assert!(IPC.contains("\"get_devices\""));
        assert!(IPC.contains("\"get_cfg\""));
        assert!(IPC.contains("\"get_ver\""));
        assert!(IPC.contains("\"launcher\""));
        assert!(IPC.contains("\"notif_center\""));
        assert!(IPC.contains("\"screenshot\""));
        assert!(IPC.contains("\"load_session\""));
        assert!(IPC.contains("\"toggle_do_not_disturb\""));
        assert!(IPC.contains("\"layouts\""));
        assert!(IPC.contains("\"lock\""));

        assert!(SOURCE.contains("fn tearing_status_summary"));
        assert!(SOURCE.contains("fn xwayland_status_summary"));
        assert!(SOURCE.contains("fn scrolling_status_summary"));
        assert!(SOURCE.contains("fn color_management_status_summary"));
        assert!(SOURCE.contains("fn audio_status_summary"));
        assert!(SOURCE.contains("fn wallpaper_status_summary"));
        assert!(SOURCE.contains("fn bluetooth_status_summary"));
        assert!(SOURCE.contains("fn system_ui_status_summary"));
        assert!(SOURCE.contains("fn layout_status_summary"));
        assert!(SOURCE.contains("\"blur_quality_auto\""));
        assert!(SOURCE.contains("\"color_temperature\""));
        assert!(SOURCE.contains("\"particle_effects\""));
        assert!(SOURCE.contains("\"wayland_enable_virtual_pointer\""));
        assert!(SOURCE.contains("scratchpad_count,"));
        assert!(SOURCE.contains("tabbed_count,"));

        assert!(SESSION.contains("pub minimized_order:"));
        assert!(SESSION.contains("fn migrate_snapshot_v13"));

        assert!(DISPATCH.contains("4 | 6 =>"));
        assert!(DISPATCH.contains("5 | 7 =>"));

        assert!(PORTAL.contains("pub is_below:"));
        assert!(PORTAL.contains("pub is_scratchpad:"));
        assert!(PORTAL.contains("pub client_fact:"));
        assert!(PORTAL.contains("pub tabbed_count:"));
        assert!(PORTAL.contains("pub refresh_mhz:"));

        let status = SOURCE
            .split_once("compositor_metrics: backend")
            .expect("status metrics")
            .1
            .split_once("fn window_info")
            .expect("window_info follows")
            .0;
        for nest in [
            "tearing:",
            "xwayland:",
            "scrolling:",
            "color_management:",
            "audio:",
            "wallpaper:",
            "bluetooth:",
            "system_ui:",
            "layout:",
        ] {
            assert!(status.contains(nest), "get_status must nest {nest}");
        }
    }

    #[test]
    fn evolve7h_waves_701_800_ipc_contract_pins() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const IPC: &str = include_str!("../ipc.rs");
        const SESSION: &str = include_str!("session.rs");
        const DISPATCH: &str = include_str!("event_dispatcher.rs");
        const PORTAL: &str = include_str!("../../portal/src/ipc.rs");

        assert!(IPC.contains("pub dock_count:"));
        assert!(IPC.contains("pub desktop_count:"));
        assert!(IPC.contains("pub never_focus_count:"));
        assert!(IPC.contains("pub demands_attention_count:"));
        assert!(IPC.contains("pub tabs:"));
        assert!(IPC.contains("pub struts:"));
        assert!(IPC.contains("pub scratchpads:"));
        assert!(IPC.contains("pub gaps:"));
        assert!(IPC.contains("pub mfact:"));
        assert!(IPC.contains("pub nmaster:"));
        assert!(IPC.contains("pub show_bar:"));
        assert!(IPC.contains("pub version_info:"));
        assert!(IPC.contains("\"get_bt\""));
        assert!(IPC.contains("\"get_wl\""));
        assert!(IPC.contains("\"get_nl\""));
        assert!(IPC.contains("\"get_cm\""));
        assert!(IPC.contains("\"get_sess\""));
        assert!(IPC.contains("\"get_strut\""));
        assert!(IPC.contains("\"get_scratch\""));
        assert!(IPC.contains("\"get_mons\""));
        assert!(IPC.contains("\"get_ws\""));
        assert!(IPC.contains("\"get_gap\""));
        assert!(IPC.contains("\"get_nm\""));
        assert!(IPC.contains("\"get_mf\""));
        assert!(IPC.contains("\"get_tab\""));
        assert!(IPC.contains("\"get_bench\""));
        assert!(IPC.contains("\"get_gest\""));
        assert!(IPC.contains("\"get_wall\""));
        assert!(IPC.contains("\"hub\""));
        assert!(IPC.contains("\"switcher\""));
        assert!(IPC.contains("\"tags\""));
        assert!(IPC.contains("\"overview\""));
        assert!(IPC.contains("\"peek\""));
        assert!(IPC.contains("\"mag\""));
        assert!(IPC.contains("\"annotate\""));
        assert!(IPC.contains("\"lily\""));
        assert!(IPC.contains("\"night\""));
        assert!(IPC.contains("\"caffeine\""));
        assert!(IPC.contains("\"wifi\""));
        assert!(IPC.contains("\"bt\""));
        assert!(IPC.contains("\"wall\""));
        assert!(IPC.contains("\"session\""));
        assert!(IPC.contains("\"floating\""));
        assert!(IPC.contains("\"sticky\""));
        assert!(IPC.contains("\"pip\""));
        assert!(IPC.contains("\"maximize\""));

        assert!(SOURCE.contains("fn tabs_status_summary"));
        assert!(SOURCE.contains("fn struts_status_summary"));
        assert!(SOURCE.contains("fn scratchpads_status_summary"));
        assert!(SOURCE.contains("\"swallow_terminals\""));
        assert!(SOURCE.contains("\"shadow_exclude_count\""));
        assert!(SOURCE.contains("\"ui_theme\""));
        assert!(SOURCE.contains("dock_count:"));
        assert!(SOURCE.contains("demands_attention_count:"));

        assert!(SESSION.contains("pub hidden_restore:"));
        assert!(SESSION.contains("fn migrate_snapshot_v14"));

        let compact: String = DISPATCH.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            compact.contains("4|6if!matches!("),
            "system_ui horizontal wheel must twin vertical"
        );
        assert!(
            compact.contains("5|7if!matches!("),
            "system_ui horizontal wheel must twin vertical"
        );

        assert!(PORTAL.contains("pub is_dock:"));
        assert!(PORTAL.contains("pub is_desktop:"));
        assert!(PORTAL.contains("pub has_strut:"));
        assert!(PORTAL.contains("pub dock_count:"));
        assert!(PORTAL.contains("pub demands_attention_count:"));

        let status = SOURCE
            .split_once("compositor_metrics: backend")
            .expect("status metrics")
            .1
            .split_once("fn window_info")
            .expect("window_info follows")
            .0;
        for nest in [
            "tabs:",
            "struts:",
            "scratchpads:",
            "gaps:",
            "mfact:",
            "nmaster:",
            "show_bar:",
            "metrics:",
            "version_info:",
        ] {
            assert!(status.contains(nest), "get_status must nest {nest}");
        }
    }

    #[test]
    fn evolve7h_waves_801_900_ipc_contract_pins() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const IPC: &str = include_str!("../ipc.rs");
        const SESSION: &str = include_str!("session.rs");
        const CALENDAR: &str = include_str!("../../docs/calendar.md");
        const PORTAL: &str = include_str!("../../portal/src/ipc.rs");

        assert!(IPC.contains("pub skip_taskbar_count:"));
        assert!(IPC.contains("pub skip_pager_count:"));
        assert!(IPC.contains("pub no_decorations_count:"));
        assert!(IPC.contains("pub drag_float_count:"));
        assert!(IPC.contains("pub monitors:"));
        assert!(IPC.contains("pub workspaces:"));
        assert!(IPC.contains("pub windows:"));
        assert!(IPC.contains("pub tree:"));
        assert!(IPC.contains("pub focused:"));
        assert!(IPC.contains("pub cfact:"));
        assert!(IPC.contains("pub prev_layout:"));
        assert!(IPC.contains("pub effects:"));
        assert!(IPC.contains("pub mic:"));
        assert!(IPC.contains("\"get_lt\""));
        assert!(IPC.contains("\"get_cf\""));
        assert!(IPC.contains("\"get_sel\""));
        assert!(IPC.contains("\"get_fw\""));
        assert!(IPC.contains("\"get_pl\""));
        assert!(IPC.contains("\"get_bar\""));
        assert!(IPC.contains("\"get_tr\""));
        assert!(IPC.contains("\"get_win\""));
        assert!(IPC.contains("\"get_conn\""));
        assert!(IPC.contains("\"get_st\""));
        assert!(IPC.contains("\"get_fx\""));
        assert!(IPC.contains("\"get_mute\""));
        assert!(IPC.contains("\"get_cli\""));
        assert!(IPC.contains("\"get_wc\""));
        assert!(IPC.contains("\"get_th\""));
        assert!(IPC.contains("\"get_pair\""));
        assert!(IPC.contains("\"cal\""));
        assert!(IPC.contains("\"clip\""));
        assert!(IPC.contains("\"monlayout\""));
        assert!(IPC.contains("\"aout\""));
        assert!(IPC.contains("\"ain\""));
        assert!(IPC.contains("\"unlock\""));
        assert!(IPC.contains("\"snap\""));
        assert!(IPC.contains("\"record\""));
        assert!(IPC.contains("\"arecord\""));
        assert!(IPC.contains("\"bar\""));
        assert!(IPC.contains("\"comp\""));
        assert!(IPC.contains("\"play\""));
        assert!(IPC.contains("\"next\""));
        assert!(IPC.contains("\"prev\""));
        assert!(IPC.contains("\"stop\""));
        assert!(IPC.contains("\"unfocus\""));
        assert!(IPC.contains("\"damage\""));
        assert!(IPC.contains("\"cycle\""));

        assert!(SOURCE.contains("fn monitors_status_summary"));
        assert!(SOURCE.contains("fn workspaces_status_summary"));
        assert!(SOURCE.contains("fn windows_status_summary"));
        assert!(SOURCE.contains("fn tree_status_summary"));
        assert!(SOURCE.contains("fn effects_status_summary"));
        assert!(SOURCE.contains("fn mic_status_summary"));
        assert!(SOURCE.contains("\"status_bar_name\""));
        assert!(SOURCE.contains("\"client_moveresize\""));
        assert!(SOURCE.contains("\"new_client_position\""));
        assert!(SOURCE.contains("skip_taskbar_count:"));
        assert!(SOURCE.contains("drag_float_count:"));

        assert!(SESSION.contains("pub old_geometry:"));
        assert!(SESSION.contains("fn migrate_snapshot_v15"));

        assert!(
            CALENDAR.contains("vertical or horizontal wheel"),
            "calendar docs must advertise horizontal wheel twin"
        );

        assert!(PORTAL.contains("pub is_status_bar:"));
        assert!(PORTAL.contains("pub is_swallowed:"));
        assert!(PORTAL.contains("pub maximize_promoted:"));
        assert!(PORTAL.contains("pub skip_taskbar_count:"));
        assert!(PORTAL.contains("pub drag_float_count:"));

        let status = SOURCE
            .split_once("compositor_metrics: backend")
            .expect("status metrics")
            .1
            .split_once("fn window_info")
            .expect("window_info follows")
            .0;
        for nest in [
            "monitors:",
            "workspaces:",
            "windows:",
            "tree:",
            "focused:",
            "cfact:",
            "prev_layout:",
            "effects:",
            "mic:",
        ] {
            assert!(status.contains(nest), "get_status must nest {nest}");
        }
    }

    #[test]
    fn evolve7h_waves_901_1000_ipc_contract_pins() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const IPC: &str = include_str!("../ipc.rs");
        const SESSION: &str = include_str!("session.rs");
        const CLIPBOARD: &str = include_str!("../../docs/clipboard.md");
        const NOTIF: &str = include_str!("../../docs/notifications.md");
        const PORTAL: &str = include_str!("../../portal/src/ipc.rs");

        assert!(IPC.contains("pub swallowed_count:"));
        assert!(IPC.contains("pub on_view_count:"));
        assert!(IPC.contains("pub maximize_promoted_count:"));
        assert!(IPC.contains("pub strut_count:"));
        assert!(IPC.contains("pub status_bar_count:"));
        assert!(IPC.contains("pub capabilities:"));
        assert!(IPC.contains("pub selected:"));
        assert!(IPC.contains("pub bench:"));
        assert!(IPC.contains("pub floating:"));
        assert!(IPC.contains("pub minimized:"));
        assert!(IPC.contains("pub sticky:"));
        assert!(IPC.contains("pub urgent:"));
        assert!(IPC.contains("pub fullscreen:"));
        assert!(IPC.contains("pub pip:"));
        assert!(IPC.contains("\"get_pk\""));
        assert!(IPC.contains("\"get_bm\""));
        assert!(IPC.contains("\"get_conf\""));
        assert!(IPC.contains("\"get_rec\""));
        assert!(IPC.contains("\"get_arec\""));
        assert!(IPC.contains("\"get_cap\""));
        assert!(IPC.contains("\"get_xw\""));
        assert!(IPC.contains("\"get_wly\""));
        assert!(IPC.contains("\"get_idl\""));
        assert!(IPC.contains("\"kill\""));
        assert!(IPC.contains("\"last\""));
        assert!(IPC.contains("\"loop\""));
        assert!(IPC.contains("\"save\""));
        assert!(IPC.contains("\"restore\""));
        assert!(IPC.contains("\"pad\""));
        assert!(IPC.contains("\"ftab\""));
        assert!(IPC.contains("\"fwin\""));
        assert!(IPC.contains("\"case\""));
        assert!(IPC.contains("\"palette\""));
        assert!(IPC.contains("\"region\""));
        assert!(IPC.contains("\"attach\""));
        assert!(IPC.contains("\"scol\""));
        assert!(IPC.contains("\"smov\""));
        assert!(IPC.contains("\"swin\""));
        assert!(IPC.contains("\"scons\""));
        assert!(IPC.contains("\"sexp\""));
        assert!(IPC.contains("\"twifi\""));
        assert!(IPC.contains("\"tbt\""));
        assert!(IPC.contains("\"clayout\""));

        assert!(SOURCE.contains("fn bench_status_summary"));
        assert!(SOURCE.contains("fn windows_flag_status_summary"));
        assert!(SOURCE.contains("\"animation_speed\""));
        assert!(SOURCE.contains("\"chord_leader_key\""));
        assert!(SOURCE.contains("\"backend_family\""));
        assert!(SOURCE.contains("swallowed_count:"));
        assert!(SOURCE.contains("status_bar_count:"));

        assert!(SESSION.contains("const SESSION_VERSION: u32 = 17"));
        assert!(SESSION.contains("pub hidden_x:"));
        assert!(SESSION.contains("fn migrate_snapshot_v16"));

        assert!(
            CLIPBOARD.contains("vertical or horizontal wheel"),
            "clipboard docs must advertise horizontal wheel twin"
        );
        assert!(
            NOTIF.contains("vertical or horizontal wheel"),
            "notification center docs must advertise horizontal wheel twin"
        );

        assert!(PORTAL.contains("pub swallowed_count:"));
        assert!(PORTAL.contains("pub on_view_count:"));
        assert!(PORTAL.contains("pub maximize_promoted_count:"));
        assert!(PORTAL.contains("pub strut_count:"));
        assert!(PORTAL.contains("pub status_bar_count:"));
        assert!(PORTAL.contains("pub is_maximized_vert:"));
        assert!(PORTAL.contains("pub is_maximized_horz:"));

        let status = SOURCE
            .split_once("compositor_metrics: backend")
            .expect("status metrics")
            .1
            .split_once("fn window_info")
            .expect("window_info follows")
            .0;
        for nest in [
            "capabilities:",
            "selected:",
            "bench:",
            "floating:",
            "minimized:",
            "sticky:",
            "urgent:",
            "fullscreen:",
            "pip:",
        ] {
            assert!(status.contains(nest), "get_status must nest {nest}");
        }
    }

    #[test]
    fn evolve8h_wave_1_monitor_bar_visible_is_distinct_from_show_bar() {
        const IPC: &str = include_str!("../ipc.rs");
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(IPC.contains("pub bar_visible: bool"));
        assert!(SOURCE.contains("bar_visible: self.monitor_shows_status_bar(mk)"));
        assert!(
            DOCS.contains("`bar_visible`"),
            "monitor IPC docs must name bar_visible beside show_bar"
        );
    }

    #[test]
    fn evolve8h_wave_2_get_show_bar_reports_bar_visible() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let query = SOURCE
            .split_once("fn query_show_bar_for_monitor(")
            .expect("query_show_bar_for_monitor")
            .1
            .split_once("fn query_focused_show_bar(")
            .expect("focused follows")
            .0;
        assert!(query.contains("\"bar_visible\""));
        assert!(query.contains("monitor_shows_status_bar(mk)"));
        assert!(
            DOCS.contains("`get_show_bar`\nalso reports `bar_visible`")
                || DOCS.contains("also reports `bar_visible`"),
            "get_show_bar docs must mention bar_visible"
        );
    }

    #[test]
    fn evolve8h_wave_3_monitor_has_visible_fullscreen() {
        const IPC: &str = include_str!("../ipc.rs");
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(IPC.contains("pub has_visible_fullscreen: bool"));
        assert!(SOURCE.contains("has_visible_fullscreen: self.monitor_has_visible_fullscreen(mk)"));
        assert!(DOCS.contains("`has_visible_fullscreen`"));
    }

    #[test]
    fn evolve8h_wave_4_get_show_bar_reports_visible_fullscreen() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let query = SOURCE
            .split_once("fn query_show_bar_for_monitor(")
            .expect("query_show_bar_for_monitor")
            .1
            .split_once("fn query_focused_show_bar(")
            .expect("focused follows")
            .0;
        assert!(query.contains("\"has_visible_fullscreen\""));
        assert!(query.contains("monitor_has_visible_fullscreen(mk)"));
        assert!(DOCS.contains("`has_visible_fullscreen`"));
    }

    #[test]
    fn evolve8h_wave_5_get_bar_visible_aliases_show_bar() {
        const IPC: &str = include_str!("../ipc.rs");
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(IPC.contains("\"get_bar_visible\""));
        assert!(SOURCE.contains("\"get_bar_visible\""));
        assert!(DOCS.contains("`get_bar_visible`"));
    }

    #[test]
    fn evolve8h_wave_6_workspace_has_visible_fullscreen() {
        const IPC: &str = include_str!("../ipc.rs");
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(IPC.contains("pub has_visible_fullscreen: bool"));
        assert!(SOURCE.contains("has_visible_fullscreen: is_active"));
        assert!(DOCS.contains("`has_visible_fullscreen`"));
    }

    #[test]
    fn evolve8h_wave_7_window_owns_output() {
        const IPC: &str = include_str!("../ipc.rs");
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        const TABS: &str = include_str!("../../docs/window-tabs.md");
        assert!(IPC.contains("pub owns_output: bool"));
        assert!(SOURCE.contains("owns_output: client.state.is_fullscreen && is_on_view"));
        assert!(DOCS.contains("`owns_output`"));
        assert!(TABS.contains("owns_output"));
    }

    #[test]
    fn evolve8h_wave_8_owns_output_count_symmetry() {
        const IPC: &str = include_str!("../ipc.rs");
        const SOURCE: &str = include_str!("ipc_handler.rs");
        assert!(IPC.contains("pub owns_output_count: usize"));
        assert!(SOURCE.contains("counts.owns_output"));
        assert!(SOURCE.contains("owns_output_count:"));
    }

    #[test]
    fn evolve8h_wave_9_get_show_bar_reports_owns_output_count() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const CORE: &str = include_str!("../jwm.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let query = SOURCE
            .split_once("fn query_show_bar_for_monitor(")
            .expect("query_show_bar_for_monitor")
            .1
            .split_once("fn query_focused_show_bar(")
            .expect("focused follows")
            .0;
        assert!(query.contains("\"owns_output_count\""));
        assert!(query.contains("monitor_owns_output_count(mk)"));
        assert!(CORE.contains("fn monitor_owns_output_count"));
        assert!(DOCS.contains("`owns_output_count`"));
        assert!(SOURCE.contains("self.query_show_bar_for_monitor(backend, mk)"));
    }

    #[test]
    fn evolve8h_wave_10_monitor_owns_output_count_uses_hide_bar_predicate() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let monitor_info = SOURCE
            .split_once("fn monitor_info_ipc(")
            .expect("monitor_info_ipc")
            .1
            .split_once("fn query_focused_tab_bar(")
            .expect("tab bar follows")
            .0;
        assert!(monitor_info.contains("owns_output_count: self.monitor_owns_output_count(mk)"));
        assert!(DOCS.contains("same visibility as the status-bar hide"));
    }

    #[test]
    fn evolve8h_wave_11_workspace_owns_output_count_on_active_tag() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let workspaces = SOURCE
            .split_once("fn query_workspaces(")
            .expect("query_workspaces")
            .1
            .split_once("fn query_monitors(")
            .expect("query_monitors follows")
            .0;
        assert!(workspaces.contains("owns_output_count: if is_active"));
        assert!(workspaces.contains("self.monitor_owns_output_count(mk)"));
        assert!(DOCS.contains("workspace rows report that\ncount only on the active tag"));
    }

    #[test]
    fn evolve8h_wave_12_tree_owns_output_count_uses_hide_bar_predicate() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let tree = SOURCE
            .split_once("fn query_tree(")
            .expect("query_tree")
            .1
            .split_once("fn broadcast_ipc_event(")
            .expect("broadcast follows")
            .0;
        assert!(tree.contains("owns_output_count: self.monitor_owns_output_count(mk)"));
        assert!(DOCS.contains("`get_tree` uses the same hide-bar"));
    }

    #[test]
    fn evolve8h_wave_13_window_owns_output_excludes_swallowed() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const IPC: &str = include_str!("../ipc.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(SOURCE.contains("!client.state.is_swallowed"));
        assert!(IPC.contains("Swallowed\n    /// terminals never own the output"));
        assert!(DOCS.contains("swallowed terminals never own it"));
    }

    #[test]
    fn evolve8h_wave_14_tag_counts_owns_output_excludes_swallowed() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let accumulate = SOURCE
            .split_once("fn accumulate_client_counts(")
            .expect("accumulate_client_counts")
            .1
            .split_once("fn accumulate_window_counts(")
            .expect("window counts follow")
            .0;
        assert!(accumulate.contains("!client.state.is_swallowed"));
        assert!(DOCS.contains("tag counts skip swallowed terminals"));
    }

    #[test]
    fn evolve8h_wave_15_get_owns_output_aliases_show_bar() {
        const IPC: &str = include_str!("../ipc.rs");
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(IPC.contains("\"get_owns_output\""));
        assert!(SOURCE.contains("\"get_owns_output\""));
        assert!(DOCS.contains("`get_owns_output`"));
    }

    #[test]
    fn evolve8h_wave_16_show_bar_snapshot_is_per_monitor() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(SOURCE.contains("fn query_show_bar_for_monitor"));
        assert!(SOURCE.contains("self.query_show_bar_for_monitor(backend, mk)"));
        assert!(DOCS.contains("per-monitor show-bar snapshot"));
    }

    #[test]
    fn evolve8h_wave_17_fullscreen_broadcasts_monitor_bar() {
        const STATE: &str = include_str!("window_state.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(STATE.contains("broadcast_monitor_bar_ipc(backend, mon_key)"));
        assert!(DOCS.contains("`monitor/bar`"));
    }

    #[test]
    fn evolve8h_wave_18_togglebar_broadcasts_monitor_bar() {
        const NAV: &str = include_str!("navigation.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(NAV.contains("broadcast_monitor_bar_ipc(backend, sel_mon_key)"));
        assert!(DOCS.contains("`togglebar` emits the same event"));
    }

    #[test]
    fn evolve8h_wave_19_broadcast_monitor_bar_helper() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const STATE: &str = include_str!("window_state.rs");
        const NAV: &str = include_str!("navigation.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(SOURCE.contains("fn broadcast_monitor_bar_ipc"));
        assert!(STATE.contains("broadcast_monitor_bar_ipc(backend, mon_key)"));
        assert!(NAV.contains("broadcast_monitor_bar_ipc(backend, sel_mon_key)"));
        assert!(
            DOCS.contains("shared `monitor/bar` helper")
                || DOCS.contains("shared\n`monitor/bar` helper")
        );
    }

    #[test]
    fn evolve8h_wave_20_view_broadcasts_monitor_bar() {
        const NAV: &str = include_str!("navigation.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let view = NAV
            .split_once("pub fn view(")
            .expect("view")
            .1
            .split_once("pub fn toggleview(")
            .expect("toggleview follows")
            .0;
        assert!(view.contains("broadcast_monitor_bar_ipc(backend, sel_mon_key)"));
        assert!(DOCS.contains("Tag `view` emits it"));
    }

    #[test]
    fn evolve8h_wave_21_toggleview_broadcasts_monitor_bar() {
        const NAV: &str = include_str!("navigation.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let toggleview = NAV
            .split_once("pub fn toggleview(")
            .expect("toggleview")
            .1
            .split_once("pub fn toggletag(")
            .expect("toggletag follows")
            .0;
        assert!(toggleview.contains("broadcast_monitor_bar_ipc(backend, sel_mon_key)"));
        assert!(DOCS.contains("`toggleview`"));
    }

    #[test]
    fn evolve8h_wave_22_fullscreen_layout_broadcasts_monitor_bar() {
        const LAYOUT: &str = include_str!("layout/state.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let transition = LAYOUT
            .split_once("fn handle_fullscreen_layout_transition(")
            .expect("fullscreen layout transition")
            .1
            .split_once("pub(crate) fn set_new_layout(")
            .expect("set_new_layout follows")
            .0;
        assert!(transition.contains("broadcast_monitor_bar_ipc(backend, mon_key)"));
        assert!(DOCS.contains("Fullscreen layout enter/leave"));
    }

    #[test]
    fn evolve8h_wave_23_minimize_fullscreen_broadcasts_monitor_bar() {
        const STATE: &str = include_str!("window_state.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let minimize = STATE
            .split_once("pub(crate) fn set_client_minimized(")
            .expect("set_client_minimized")
            .1
            .split_once("mod restore_rect_tests")
            .expect("restore tests follow")
            .0;
        assert!(minimize.contains("was_fullscreen"));
        assert!(minimize.contains("broadcast_monitor_bar_ipc(backend, mon_key)"));
        assert!(DOCS.contains("minimizing a\nfullscreen client") || DOCS.contains("minimizing a fullscreen client"));
    }

    #[test]
    fn evolve8h_wave_24_unmanage_fullscreen_broadcasts_monitor_bar() {
        const CLIENT: &str = include_str!("client.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let unmanage = CLIENT
            .split_once("pub(crate) fn unmanage(")
            .expect("unmanage")
            .1
            .split_once("pub(crate) fn is_popup_like(")
            .expect("is_popup_like follows")
            .0;
        assert!(unmanage.contains("c.state.is_fullscreen"));
        assert!(unmanage.contains("broadcast_monitor_bar_ipc(backend, mon_key)"));
        assert!(DOCS.contains("`window/close`"));
    }

    #[test]
    fn evolve8h_wave_25_swallow_broadcasts_monitor_bar() {
        const SWALLOW: &str = include_str!("swallowing.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let try_swallow = SWALLOW
            .split_once("pub(crate) fn try_swallow(")
            .expect("try_swallow")
            .1
            .split_once("pub(crate) fn try_unswallow(")
            .expect("try_unswallow")
            .0;
        let try_unswallow = SWALLOW
            .split_once("pub(crate) fn try_unswallow(")
            .expect("try_unswallow")
            .1
            .split_once("fn can_enter_swallowed_state(")
            .expect("can_enter_swallowed_state")
            .0;
        assert!(try_swallow.contains("broadcast_monitor_bar_ipc"));
        assert!(try_unswallow.contains("broadcast_monitor_bar_ipc"));
        assert!(DOCS.contains("Swallowing a terminal"));
    }

    #[test]
    fn evolve8h_wave_26_sendmon_fullscreen_broadcasts_monitor_bar() {
        const TAGS: &str = include_str!("tag_manager.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let sendmon = TAGS
            .split_once("pub(crate) fn sendmon(")
            .expect("sendmon")
            .1
            .split_once("pub(crate) fn setclienttagprop(")
            .expect("setclienttagprop follows")
            .0;
        assert!(sendmon.contains("was_fullscreen"));
        assert!(sendmon.contains("broadcast_monitor_bar_ipc(backend, mon_key)"));
        assert!(sendmon.contains("broadcast_monitor_bar_ipc(backend, target_mon_key)"));
        assert!(DOCS.contains("both monitors"));
    }

    #[test]
    fn evolve8h_wave_27_focusmon_broadcasts_monitor_bar() {
        const FOCUS: &str = include_str!("focus_manager.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(FOCUS.contains("\"monitor/focus\""));
        assert!(FOCUS.contains("broadcast_monitor_bar_ipc(backend, target_mon_key)"));
        assert!(DOCS.contains("`focusmon`"));
    }

    #[test]
    fn evolve8h_wave_28_pointer_switch_broadcasts_monitor_focus_and_bar() {
        const CLIENT: &str = include_str!("client.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let switch = CLIENT
            .split_once("pub(crate) fn handle_monitor_switch_by_key(")
            .expect("handle_monitor_switch_by_key")
            .1
            .split_once("pub(crate) fn unmanage(")
            .expect("unmanage follows")
            .0;
        assert!(switch.contains("\"monitor/focus\""));
        assert!(switch.contains("broadcast_monitor_bar_ipc(backend, monitor_key)"));
        assert!(DOCS.contains("Pointer crossings"));
    }

    #[test]
    fn evolve8h_wave_29_session_restore_broadcasts_all_monitor_bars() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const SESSION: &str = include_str!("session.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(SOURCE.contains("fn broadcast_monitor_bar_all_monitors"));
        assert!(SESSION.contains("broadcast_monitor_bar_all_monitors(backend)"));
        assert!(DOCS.contains("Session restore emits"));
    }

    #[test]
    fn evolve8h_wave_30_strut_changes_broadcast_all_monitor_bars() {
        const STRUT: &str = include_str!("strut_manager.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert_eq!(
            STRUT.matches("broadcast_monitor_bar_all_monitors(backend)").count(),
            3
        );
        assert!(DOCS.contains("External strut"));
    }

    #[test]
    fn evolve8h_wave_31_output_hotplug_broadcasts_all_monitor_bars() {
        const MONITOR: &str = include_str!("monitor.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(MONITOR.contains("broadcast_monitor_bar_all_monitors(backend)"));
        assert!(DOCS.contains("Output hotplug"));
    }

    #[test]
    fn evolve8h_wave_32_bar_subscription_aliases_monitor_bar() {
        const IPC: &str = include_str!("../ipc.rs");
        const SERVER: &str = include_str!("../ipc_server.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(IPC.contains("\"bar\""));
        assert!(SERVER.contains("if topic == \"bar\""));
        assert!(SERVER.contains("return \"monitor/bar\".to_string()"));
        assert!(DOCS.contains("Subscribe topic `bar`"));
    }

    #[test]
    fn evolve8h_wave_33_jwm_tool_documents_bar_subscription() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("bar (monitor/bar occupancy)"));
        assert!(TOOL.contains("--subscribe 'bar'"));
        assert!(DOCS.contains("`jwm-tool msg --subscribe bar`"));
    }

    #[test]
    fn evolve8h_wave_34_readme_subscribe_example_includes_bar() {
        const README: &str = include_str!("../../README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(README.contains("--subscribe 'window,tag,layout,bar'"));
        assert!(
            README.contains("`bar` is stored as `monitor/bar`")
                || README.contains("stored as `monitor/bar`")
        );
        assert!(
            DOCS.contains("stores it as `monitor/bar`")
                || DOCS.contains("store it as `monitor/bar`")
        );
    }

    #[test]
    fn evolve8h_wave_35_tools_readme_subscribe_example_includes_bar() {
        const TOOLS: &str = include_str!("../../tools/README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOLS.contains("--subscribe 'window,tag,layout,bar'"));
        assert!(DOCS.contains("`tools/README.md`"));
    }

    #[test]
    fn evolve8h_wave_36_workspace_owns_output_count_rustdoc() {
        const IPC: &str = include_str!("../ipc.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(IPC.contains("Zero when"));
        assert!(IPC.contains("the tag is off-view"));
        assert!(DOCS.contains("workspace `owns_output_count` is zero off-view"));
    }

    #[test]
    fn evolve8h_wave_37_tree_owns_output_count_rustdoc() {
        const IPC: &str = include_str!("../ipc.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(IPC.contains("documented on the tree row type") || IPC.contains("hide-bar occupancy"));
        assert!(DOCS.contains("documented on the tree row type"));
    }

    #[test]
    fn evolve8h_wave_38_monitor_owns_output_count_rustdoc() {
        const IPC: &str = include_str!("../ipc.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(IPC.contains("swallowed terminals excluded"));
        assert!(DOCS.contains("swallowed terminals\nexcluded") || DOCS.contains("swallowed terminals excluded"));
    }

    #[test]
    fn evolve8h_wave_39_show_bar_snapshot_json_keys_documented() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let prod = SOURCE.split_once("mod tests {").expect("tests module").0;
        assert!(prod.contains("Object keys: `monitor`, `show_bar`, `bar_visible`"));
        assert!(prod.contains("optional `connector`."));
        assert!(DOCS.contains("JSON keys `monitor`"));
    }

    #[test]
    fn evolve8h_wave_40_broadcast_rustdoc_names_bar_alias() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let prod = SOURCE.split_once("mod tests {").expect("tests module").0;
        assert!(prod.contains("`bar` stores as `monitor/bar`"));
        assert!(DOCS.contains("Subscribe topic `bar`"));
    }

    #[test]
    fn evolve8h_wave_41_get_status_show_bar_occupancy_rustdoc() {
        const IPC: &str = include_str!("../ipc.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(IPC.contains("Compact twin of `get_show_bar` / `get_bar`"));
        assert!(DOCS.contains("`get_status.show_bar` is the same occupancy snapshot"));
    }

    #[test]
    fn evolve8h_wave_42_toggletag_fullscreen_broadcasts_monitor_bar() {
        const NAV: &str = include_str!("navigation.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let toggletag = NAV
            .split_once("pub fn toggletag(")
            .expect("toggletag")
            .1
            .split_once("pub fn quit(")
            .expect("quit follows")
            .0;
        assert!(toggletag.contains("was_fullscreen"));
        assert!(toggletag.contains("broadcast_monitor_bar_ipc(backend, mk)"));
        assert!(DOCS.contains("`toggletag` on a fullscreen client"));
    }

    #[test]
    fn evolve8h_wave_43_tag_move_fullscreen_broadcasts_monitor_bar() {
        const TAGS: &str = include_str!("tag_manager.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let move_to_tag = TAGS
            .split_once("pub(crate) fn move_client_to_tag(")
            .expect("move_client_to_tag")
            .1
            .split_once("pub fn tagmon(")
            .expect("tagmon follows")
            .0;
        assert!(move_to_tag.contains("was_fullscreen"));
        assert!(move_to_tag.contains("broadcast_monitor_bar_ipc(backend, mk)"));
        assert!(DOCS.contains("as does `tag`"));
    }

    #[test]
    fn evolve8h_wave_44_sticky_fullscreen_broadcasts_monitor_bar() {
        const TOGGLES: &str = include_str!("features/toggles.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let sticky = TOGGLES
            .split_once("pub(crate) fn set_client_sticky(")
            .expect("set_client_sticky")
            .1
            .split_once("pub(crate) fn prepare_for_compositor_disable(")
            .expect("prepare_for_compositor_disable follows")
            .0;
        assert!(sticky.contains("was_fullscreen"));
        assert!(sticky.contains("broadcast_monitor_bar_ipc(backend, mk)"));
        assert!(DOCS.contains("and sticky"));
    }

    #[test]
    fn evolve8h_wave_45_window_tabs_docs_name_bar_subscription() {
        const TABS: &str = include_str!("../../docs/window-tabs.md");
        assert!(TABS.contains("subscribe `bar`"));
        assert!(TABS.contains("`monitor/bar` occupancy"));
    }

    #[test]
    fn evolve8h_wave_46_get_visible_fullscreen_aliases_show_bar() {
        const IPC: &str = include_str!("../ipc.rs");
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(IPC.contains("\"get_visible_fullscreen\""));
        assert!(SOURCE.contains("\"get_visible_fullscreen\""));
        assert!(DOCS.contains("`get_visible_fullscreen`"));
    }

    #[test]
    fn evolve8h_wave_47_get_status_show_bar_names_visible_fullscreen_alias() {
        const IPC: &str = include_str!("../ipc.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(IPC.contains("`get_visible_fullscreen` / `get_vf` (preference,") || IPC.contains("`get_visible_fullscreen` (preference, occupancy,"));
        assert!(DOCS.contains("including `get_visible_fullscreen`"));
    }

    #[test]
    fn evolve8h_wave_48_get_vf_aliases_visible_fullscreen() {
        const IPC: &str = include_str!("../ipc.rs");
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(IPC.contains("\"get_vf\""));
        assert!(SOURCE.contains("\"get_vf\""));
        assert!(DOCS.contains("`get_vf`"));
    }

    #[test]
    fn evolve8h_wave_49_get_status_show_bar_names_get_vf() {
        const IPC: &str = include_str!("../ipc.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(IPC.contains("`get_visible_fullscreen` / `get_vf` (preference,"));
        assert!(DOCS.contains("`get_visible_fullscreen` / `get_vf`"));
    }

    #[test]
    fn evolve8h_wave_50_jwm_tool_help_lists_occupancy_queries() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("get_show_bar, get_vf"));
        assert!(DOCS.contains("`jwm-tool msg` help lists `get_show_bar`"));
    }

    #[test]
    fn evolve8h_wave_51_readme_control_example_get_show_bar() {
        const README: &str = include_str!("../../README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(README.contains("jwm-tool msg get_show_bar"));
        assert!(DOCS.contains("README control examples include `get_show_bar`"));
    }

    #[test]
    fn evolve8h_wave_52_tools_readme_control_example_get_show_bar() {
        const TOOLS: &str = include_str!("../../tools/README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOLS.contains("jwm-tool msg get_show_bar"));
        assert!(
            DOCS.contains("as does `tools/README.md`")
                || DOCS.contains("as does\n`tools/README.md`")
        );
    }

    #[test]
    fn evolve8h_wave_53_jwm_tool_help_lists_get_owns_output() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("get_owns_output"));
        assert!(DOCS.contains("`get_owns_output`"));
    }

    #[test]
    fn evolve8h_wave_54_jwm_tool_help_lists_get_visible_fullscreen() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("get_visible_fullscreen"));
        assert!(DOCS.contains("`get_visible_fullscreen`"));
    }

    #[test]
    fn evolve8h_wave_55_jwm_tool_help_lists_get_bar_visible() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("get_bar_visible"));
        assert!(DOCS.contains("`get_bar_visible`"));
    }

    #[test]
    fn evolve8h_wave_56_jwm_tool_help_lists_get_bar() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("get_bar_visible, get_bar"));
        assert!(DOCS.contains("`get_bar`."));
    }

    #[test]
    fn evolve8h_wave_57_readme_control_example_get_vf() {
        const README: &str = include_str!("../../README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(README.contains("jwm-tool msg get_vf"));
        assert!(DOCS.contains("`get_show_bar` and `get_vf`"));
    }

    #[test]
    fn evolve8h_wave_58_tools_readme_control_example_get_vf() {
        const TOOLS: &str = include_str!("../../tools/README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOLS.contains("jwm-tool msg get_vf"));
        assert!(DOCS.contains("`get_show_bar` and `get_vf`"));
    }

    #[test]
    fn evolve8h_wave_59_jwm_tool_after_help_example_get_vf() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("jwm-tool msg get_vf"));
        assert!(DOCS.contains("after-help examples include `get_vf`"));
    }

    #[test]
    fn evolve8h_wave_60_jwm_tool_after_help_example_get_show_bar() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("jwm-tool msg get_show_bar"));
        assert!(DOCS.contains("and `get_show_bar`"));
    }

    #[test]
    fn evolve8h_wave_61_capabilities_text_lists_subscribe_aliases() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("bar->monitor/bar, workspace->tag"));
        assert!(DOCS.contains("`bar->monitor/bar`"));
    }

    #[test]
    fn evolve8h_wave_62_capabilities_text_lists_occupancy_query_aliases() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("query aliases: get_bar,get_bar_visible"));
        assert!(DOCS.contains("occupancy\nquery aliases") || DOCS.contains("occupancy query aliases"));
    }

    #[test]
    fn evolve8h_wave_63_health_prints_occupancy() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("show_bar: preference="));
        assert!(TOOL.contains("owns_output={owns}"));
        assert!(DOCS.contains("`jwm-tool health` prints focused-bar occupancy"));
    }

    #[test]
    fn evolve8h_wave_64_health_occupancy_includes_fullscreen() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("fullscreen={fullscreen}"));
        assert!(DOCS.contains("`has_visible_fullscreen`"));
    }

    #[test]
    fn evolve8h_wave_65_health_occupancy_includes_connector() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("connector={connector}"));
        assert!(DOCS.contains("appends `connector` when known"));
    }

    #[test]
    fn evolve8h_wave_66_health_occupancy_includes_monitor_number() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("monitor={monitor}"));
        assert!(DOCS.contains("includes the monitor\nnumber") || DOCS.contains("includes the monitor number"));
    }

    #[test]
    fn evolve8h_wave_67_occupancy_snapshot_includes_tag() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let query = SOURCE
            .split_once("fn query_show_bar_for_monitor(")
            .expect("query_show_bar_for_monitor")
            .1
            .split_once("fn query_focused_show_bar(")
            .expect("focused follows")
            .0;
        assert!(query.contains("\"tag\": tag"));
        assert!(query.contains("p.cur_tag"));
        assert!(DOCS.contains("`tag` / `show_bar`") || DOCS.contains("/ `tag` / `show_bar`"));
    }

    #[test]
    fn evolve8h_wave_68_health_occupancy_includes_tag() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("tag={tag}"));
        assert!(DOCS.contains("and the current `tag`"));
    }

    #[test]
    fn evolve8h_wave_69_occupancy_snapshot_includes_layout() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let query = SOURCE
            .split_once("fn query_show_bar_for_monitor(")
            .expect("query_show_bar_for_monitor")
            .1
            .split_once("fn query_focused_show_bar(")
            .expect("focused follows")
            .0;
        assert!(query.contains("\"layout\": mon.lt_symbol"));
        assert!(DOCS.contains("also include `layout`"));
    }

    #[test]
    fn evolve8h_wave_70_layout_change_broadcasts_monitor_bar() {
        const LAYOUT: &str = include_str!("layout/state.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let apply = LAYOUT
            .split_once("fn apply_layout_change<F>(")
            .expect("apply_layout_change")
            .1
            .split_once("pub(crate) fn setlayout(")
            .expect("setlayout follows")
            .0;
        assert!(apply.contains("broadcast_monitor_bar_ipc(backend, sel_mon_key)"));
        assert!(DOCS.contains("Layout changes emit\n`monitor/bar` after `layout/set`") || DOCS.contains("Layout changes emit `monitor/bar` after `layout/set`"));
    }

    #[test]
    fn evolve8h_wave_71_health_occupancy_includes_layout() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("layout={layout}"));
        assert!(DOCS.contains("and `layout`") || DOCS.contains("and the current `tag` and `layout`"));
    }

    #[test]
    fn evolve8h_wave_72_occupancy_snapshot_includes_gap() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let query = SOURCE
            .split_once("fn query_show_bar_for_monitor(")
            .expect("query_show_bar_for_monitor")
            .1
            .split_once("fn query_focused_show_bar(")
            .expect("focused follows")
            .0;
        assert!(query.contains("\"gap\": mon.layout.gap"));
        assert!(DOCS.contains("also include `gap`"));
    }

    #[test]
    fn evolve8h_wave_73_health_occupancy_includes_gap() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("gap={gap}"));
        assert!(DOCS.contains("and `gap`"));
    }

    #[test]
    fn evolve8h_wave_74_setgaps_broadcasts_monitor_bar() {
        const LAYOUT: &str = include_str!("layout/state.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let setgaps = LAYOUT
            .split_once("pub(crate) fn setgaps(")
            .expect("setgaps")
            .1
            .split_once("fn exit_fullscreen_on_monitor(")
            .expect("exit fullscreen follows")
            .0;
        assert!(setgaps.contains("broadcast_monitor_bar_ipc(backend, sel_mon_key)"));
        assert!(DOCS.contains("`setgaps` emits `monitor/bar`"));
    }

    #[test]
    fn evolve8h_wave_75_occupancy_snapshot_includes_mfact() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let query = SOURCE
            .split_once("fn query_show_bar_for_monitor(")
            .expect("query_show_bar_for_monitor")
            .1
            .split_once("fn query_focused_show_bar(")
            .expect("focused follows")
            .0;
        assert!(query.contains("\"mfact\": mon.layout.m_fact"));
        assert!(DOCS.contains("also include `mfact`"));
    }

    #[test]
    fn evolve8h_wave_76_health_occupancy_includes_mfact() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("mfact={mfact}"));
        assert!(DOCS.contains("and `mfact`"));
    }

    #[test]
    fn evolve8h_wave_77_setmfact_broadcasts_monitor_bar() {
        const LAYOUT: &str = include_str!("layout/state.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let setmfact = LAYOUT
            .split_once("pub(crate) fn setmfact(")
            .expect("setmfact")
            .1
            .split_once("pub(crate) fn setgaps(")
            .expect("setgaps follows")
            .0;
        assert!(setmfact.contains("broadcast_monitor_bar_ipc(backend, sel_mon_key)"));
        assert!(DOCS.contains("`setmfact` emits `monitor/bar`"));
    }

    #[test]
    fn evolve8h_wave_78_occupancy_snapshot_includes_nmaster() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let query = SOURCE
            .split_once("fn query_show_bar_for_monitor(")
            .expect("query_show_bar_for_monitor")
            .1
            .split_once("fn query_focused_show_bar(")
            .expect("focused follows")
            .0;
        assert!(query.contains("\"nmaster\": mon.layout.n_master"));
        assert!(DOCS.contains("also include `nmaster`"));
    }

    #[test]
    fn evolve8h_wave_79_health_occupancy_includes_nmaster() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("nmaster={nmaster}"));
        assert!(DOCS.contains("and `nmaster`"));
    }

    #[test]
    fn evolve8h_wave_80_setnmaster_broadcasts_monitor_bar() {
        const LAYOUT: &str = include_str!("layout/state.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let setnmaster = LAYOUT
            .split_once("pub(crate) fn setnmaster(")
            .expect("setnmaster")
            .1
            .split_once("fn is_scrolling_layout(")
            .expect("is_scrolling_layout follows")
            .0;
        assert!(setnmaster.contains("broadcast_monitor_bar_ipc(backend, sel_mon_key)"));
        assert!(DOCS.contains("`setnmaster` emits `monitor/bar`"));
    }

    #[test]
    fn evolve8h_wave_81_scrolling_column_width_broadcasts_monitor_bar() {
        const SCROLL: &str = include_str!("layout/scrolling.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let set_width = SCROLL
            .split_once("fn scrolling_set_column_width(")
            .expect("scrolling_set_column_width")
            .1
            .split_once("fn scrolling_toggle_attach_mode(")
            .expect("attach mode follows")
            .0;
        assert!(set_width.contains("broadcast_monitor_bar_ipc(backend, mon_key)"));
        assert!(DOCS.contains("column width) emits `monitor/bar`") || DOCS.contains("column-width `setmfact` emits"));
    }

    #[test]
    fn evolve8h_wave_82_occupancy_snapshot_includes_prev_tag() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let query = SOURCE
            .split_once("fn query_show_bar_for_monitor(")
            .expect("query_show_bar_for_monitor")
            .1
            .split_once("fn query_focused_show_bar(")
            .expect("focused follows")
            .0;
        assert!(query.contains("\"prev_tag\": prev_tag"));
        assert!(DOCS.contains("also include `prev_tag`"));
    }

    #[test]
    fn evolve8h_wave_83_health_occupancy_includes_prev_tag() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("prev_tag={prev_tag}"));
        assert!(DOCS.contains("and `prev_tag`"));
    }

    #[test]
    fn evolve8h_wave_84_get_status_show_bar_rustdoc_names_layout_knobs() {
        const IPC: &str = include_str!("../ipc.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(IPC.contains("Nested object also carries `tag`, `prev_tag`"));
        assert!(DOCS.contains("plus `tag` / `prev_tag`"));
    }

    #[test]
    fn evolve8h_wave_85_readme_names_occupancy_layout_knobs() {
        const README: &str = include_str!("../../README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(README.contains("occupancy JSON includes `tag`, `prev_tag`"));
        assert!(DOCS.contains("README names occupancy `tag`"));
    }

    #[test]
    fn evolve8h_wave_86_tools_readme_names_occupancy_layout_knobs() {
        const TOOLS: &str = include_str!("../../tools/README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOLS.contains("occupancy JSON includes `tag`, `prev_tag`"));
        assert!(DOCS.contains("`tools/README.md` names the same occupancy"));
    }

    #[test]
    fn evolve8h_wave_87_window_tabs_names_occupancy_layout_knobs() {
        const TABS: &str = include_str!("../../docs/window-tabs.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TABS.contains("`monitor/bar` occupancy, including `tag`"));
        assert!(DOCS.contains("Window-tabs docs name occupancy `tag`"));
    }

    #[test]
    fn evolve8h_wave_88_occupancy_snapshot_includes_selected_id() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let query = SOURCE
            .split_once("fn query_show_bar_for_monitor(")
            .expect("query_show_bar_for_monitor")
            .1
            .split_once("fn query_focused_show_bar(")
            .expect("focused follows")
            .0;
        assert!(query.contains("\"selected_id\": selected_id"));
        assert!(DOCS.contains("also include `selected_id`"));
    }

    #[test]
    fn evolve8h_wave_89_health_occupancy_includes_selected_id() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("selected={selected}"));
        assert!(DOCS.contains("and `selected_id`"));
    }

    #[test]
    fn evolve8h_wave_90_focusstack_broadcasts_monitor_bar() {
        const FOCUS: &str = include_str!("focus_manager.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let focusstack = FOCUS
            .split_once("pub fn focusstack(")
            .expect("focusstack")
            .1
            .split_once("pub fn focus_none(")
            .expect("focus_none follows")
            .0;
        assert!(focusstack.contains("broadcast_monitor_bar_ipc(backend, mk)"));
        assert!(DOCS.contains("`focusstack` emits `monitor/bar`"));
    }

    #[test]
    fn evolve8h_wave_91_scrolling_focus_broadcasts_monitor_bar() {
        const SCROLL: &str = include_str!("layout/scrolling.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let focus = SCROLL
            .split_once("fn scrolling_focus_window(")
            .expect("scrolling_focus_window")
            .1
            .split_once("fn scrolling_column_width_rule_for_window(")
            .expect("column width rule follows")
            .0;
        assert!(focus.contains("broadcast_monitor_bar_ipc(backend, mon_key)"));
        assert!(DOCS.contains("in-column focus emits `monitor/bar`"));
    }

    #[test]
    fn evolve8h_wave_92_focus_none_broadcasts_monitor_bar() {
        const FOCUS: &str = include_str!("focus_manager.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let focus_none = FOCUS
            .split_once("pub fn focus_none(")
            .expect("focus_none")
            .1
            .split_once("pub fn focus_window(")
            .expect("focus_window follows")
            .0;
        assert!(focus_none.contains("broadcast_monitor_bar_ipc(backend, mk)"));
        assert!(DOCS.contains("`focus_none` emits `monitor/bar`"));
    }

    #[test]
    fn evolve8h_wave_93_focus_window_broadcasts_monitor_bar() {
        const FOCUS: &str = include_str!("focus_manager.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let focus_window = FOCUS
            .split_once("pub fn focus_window(")
            .expect("focus_window")
            .1
            .split_once("fn capture_reveal_navigation(")
            .expect("capture_reveal_navigation follows")
            .0;
        assert!(focus_window.contains("broadcast_monitor_bar_ipc(backend, mk)"));
        assert!(DOCS.contains("`focus_window` emits `monitor/bar`"));
    }

    #[test]
    fn evolve8h_wave_94_get_status_show_bar_rustdoc_names_selected_id() {
        const IPC: &str = include_str!("../ipc.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(IPC.contains("`nmaster`, and `selected_id`"));
        assert!(DOCS.contains("`nmaster` / `selected_id`"));
    }

    #[test]
    fn evolve8h_wave_95_readme_occupancy_names_selected_id() {
        const README: &str = include_str!("../../README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(README.contains("`nmaster`, and `selected_id`"));
        assert!(DOCS.contains("README occupancy JSON also names\n`selected_id`") || DOCS.contains("README occupancy JSON also names `selected_id`"));
    }

    #[test]
    fn evolve8h_wave_96_tools_readme_occupancy_names_selected_id() {
        const TOOLS: &str = include_str!("../../tools/README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOLS.contains("`nmaster`, and `selected_id`"));
        assert!(DOCS.contains("including `selected_id`"));
    }

    #[test]
    fn evolve8h_wave_97_window_tabs_occupancy_names_selected_id() {
        const TABS: &str = include_str!("../../docs/window-tabs.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TABS.contains("`nmaster` / `selected_id`"));
        assert!(DOCS.contains("Window-tabs docs name occupancy `tag`"));
        assert!(DOCS.contains("`nmaster` / `selected_id`"));
    }

    #[test]
    fn evolve8h_wave_98_occupancy_snapshot_includes_sel_tags() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let query = SOURCE
            .split_once("fn query_show_bar_for_monitor(")
            .expect("query_show_bar_for_monitor")
            .1
            .split_once("fn query_focused_show_bar(")
            .expect("focused follows")
            .0;
        assert!(query.contains("\"sel_tags\": mon.sel_tags & 1"));
        assert!(DOCS.contains("also include `sel_tags`"));
    }

    #[test]
    fn evolve8h_wave_99_occupancy_snapshot_includes_previous_tags() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let query = SOURCE
            .split_once("fn query_show_bar_for_monitor(")
            .expect("query_show_bar_for_monitor")
            .1
            .split_once("fn query_focused_show_bar(")
            .expect("focused follows")
            .0;
        assert!(query.contains("\"previous_tags\": mon.tag_set[1 - (mon.sel_tags & 1)]"));
        assert!(DOCS.contains("also include `previous_tags`"));
    }

    #[test]
    fn evolve8h_wave_100_health_occupancy_includes_sel_tags() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("sel_tags={sel_tags}"));
        assert!(DOCS.contains("and `sel_tags`"));
    }

    #[test]
    fn evolve8h_wave_101_health_occupancy_includes_previous_tags() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("previous_tags={previous_tags}"));
        assert!(DOCS.contains("and `previous_tags`"));
    }

    #[test]
    fn evolve8h_wave_102_occupancy_snapshot_includes_active_tags() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let query = SOURCE
            .split_once("fn query_show_bar_for_monitor(")
            .expect("query_show_bar_for_monitor")
            .1
            .split_once("fn query_focused_show_bar(")
            .expect("focused follows")
            .0;
        assert!(query.contains("\"active_tags\": mon.tag_set[mon.sel_tags & 1]"));
        assert!(DOCS.contains("also include `active_tags`"));
    }

    #[test]
    fn evolve8h_wave_103_health_occupancy_includes_active_tags() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("active_tags={active_tags}"));
        assert!(DOCS.contains("and `active_tags`"));
    }

    #[test]
    fn evolve8h_wave_104_get_status_show_bar_rustdoc_names_tagset_masks() {
        const IPC: &str = include_str!("../ipc.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(IPC.contains("`sel_tags`, `previous_tags`"));
        assert!(DOCS.contains("`previous_tags` / `active_tags`"));
    }

    #[test]
    fn evolve8h_wave_105_readme_occupancy_names_tagset_masks() {
        const README: &str = include_str!("../../README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(README.contains("`sel_tags`, `previous_tags`, and `active_tags`"));
        assert!(DOCS.contains("README occupancy JSON also names `sel_tags`"));
    }

    #[test]
    fn evolve8h_wave_106_tools_readme_occupancy_names_tagset_masks() {
        const TOOLS: &str = include_str!("../../tools/README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOLS.contains("`sel_tags`, `previous_tags`, and `active_tags`"));
        assert!(DOCS.contains("`tools/README.md` also names `sel_tags`"));
    }

    #[test]
    fn evolve8h_wave_107_window_tabs_occupancy_names_tagset_masks() {
        const TABS: &str = include_str!("../../docs/window-tabs.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TABS.contains("`sel_tags` / `previous_tags`"));
        assert!(DOCS.contains("Window-tabs docs also name `sel_tags`"));
    }

    #[test]
    fn evolve8h_wave_108_zoom_broadcasts_monitor_bar() {
        const NAV: &str = include_str!("navigation.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let zoom = NAV
            .split_once("pub fn zoom(")
            .expect("zoom")
            .1
            .split_once("pub fn loopview(")
            .expect("loopview follows")
            .0;
        assert!(zoom.contains("broadcast_monitor_bar_ipc(backend, sel_mon_key)"));
        assert!(DOCS.contains("`zoom` emits `monitor/bar`"));
    }

    #[test]
    fn evolve8h_wave_109_occupancy_snapshot_includes_window_count() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let query = SOURCE
            .split_once("fn query_show_bar_for_monitor(")
            .expect("query_show_bar_for_monitor")
            .1
            .split_once("fn query_focused_show_bar(")
            .expect("focused follows")
            .0;
        assert!(query.contains("\"window_count\":"));
        assert!(query.contains("monitor_clients"));
        assert!(DOCS.contains("also include `window_count`"));
    }

    #[test]
    fn evolve8h_wave_110_health_occupancy_includes_window_count() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("windows={windows}"));
        assert!(DOCS.contains("and `window_count`"));
    }

    #[test]
    fn evolve8h_wave_111_manage_broadcasts_monitor_bar() {
        const CLIENT: &str = include_str!("client.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let manage = CLIENT
            .split_once("pub(crate) fn manage(")
            .expect("manage")
            .1
            .split_once("pub(crate) fn setup_client_window(")
            .expect("setup_client_window follows")
            .0;
        assert!(manage.contains("broadcast_monitor_bar_ipc(backend, mk)"));
        assert!(DOCS.contains("Managing a client emits `monitor/bar`"));
    }

    #[test]
    fn evolve8h_wave_112_unmanage_always_broadcasts_monitor_bar() {
        const CLIENT: &str = include_str!("client.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let unmanage = CLIENT
            .split_once("pub(crate) fn unmanage(")
            .expect("unmanage")
            .1
            .split_once("pub(crate) fn is_popup_like(")
            .expect("is_popup_like follows")
            .0;
        assert!(unmanage.contains("Some((_, _, Some(mon_key), _))"));
        assert!(unmanage.contains("broadcast_monitor_bar_ipc(backend, mon_key)"));
        assert!(DOCS.contains("even when it was not fullscreen"));
    }

    #[test]
    fn evolve8h_wave_113_get_status_show_bar_rustdoc_names_window_count() {
        const IPC: &str = include_str!("../ipc.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(IPC.contains("Also `window_count`."));
        assert!(DOCS.contains("`active_tags` / `window_count`"));
    }

    #[test]
    fn evolve8h_wave_114_readme_occupancy_names_window_count() {
        const README: &str = include_str!("../../README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(README.contains("It also includes `window_count`."));
        assert!(DOCS.contains("README occupancy JSON also names\n`window_count`") || DOCS.contains("README occupancy JSON also names `window_count`"));
    }

    #[test]
    fn evolve8h_wave_115_tools_readme_occupancy_names_window_count() {
        const TOOLS: &str = include_str!("../../tools/README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOLS.contains("It also includes `window_count`."));
        assert!(DOCS.contains("`tools/README.md` also names `window_count`"));
    }

    #[test]
    fn evolve8h_wave_116_window_tabs_occupancy_names_window_count() {
        const TABS: &str = include_str!("../../docs/window-tabs.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TABS.contains("`active_tags` / `window_count`"));
        assert!(DOCS.contains("Window-tabs docs also name `window_count`"));
    }

    #[test]
    fn evolve8h_wave_117_occupancy_snapshot_includes_on_view_count() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let query = SOURCE
            .split_once("fn query_show_bar_for_monitor(")
            .expect("query_show_bar_for_monitor")
            .1
            .split_once("fn query_focused_show_bar(")
            .expect("focused follows")
            .0;
        assert!(query.contains("\"on_view_count\":"));
        assert!(query.contains("is_client_visible_on_monitor(ck, mk)"));
        assert!(DOCS.contains("also include `on_view_count`"));
    }

    #[test]
    fn evolve8h_wave_118_health_occupancy_includes_on_view_count() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("on_view={on_view}"));
        assert!(DOCS.contains("and `on_view_count`"));
    }

    #[test]
    fn evolve8h_wave_119_get_status_show_bar_rustdoc_names_on_view_count() {
        const IPC: &str = include_str!("../ipc.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(IPC.contains("Also `on_view_count`."));
        assert!(DOCS.contains("`window_count` / `on_view_count`"));
    }

    #[test]
    fn evolve8h_wave_120_readme_occupancy_names_on_view_count() {
        const README: &str = include_str!("../../README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(README.contains("It also includes `on_view_count`."));
        assert!(DOCS.contains("README occupancy JSON also names `on_view_count`"));
    }

    #[test]
    fn evolve8h_wave_121_tools_readme_occupancy_names_on_view_count() {
        const TOOLS: &str = include_str!("../../tools/README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOLS.contains("It also includes `on_view_count`."));
        assert!(DOCS.contains("`tools/README.md` also names `on_view_count`"));
    }

    #[test]
    fn evolve8h_wave_122_window_tabs_occupancy_names_on_view_count() {
        const TABS: &str = include_str!("../../docs/window-tabs.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TABS.contains("`window_count` / `on_view_count`"));
        assert!(DOCS.contains("Window-tabs docs also name `on_view_count`"));
    }

    #[test]
    fn evolve8h_wave_123_occupancy_snapshot_includes_floating_count() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let query = SOURCE
            .split_once("fn query_show_bar_for_monitor(")
            .expect("query_show_bar_for_monitor")
            .1
            .split_once("fn query_focused_show_bar(")
            .expect("focused follows")
            .0;
        assert!(query.contains("\"floating_count\":"));
        assert!(query.contains("c.state.is_floating"));
        assert!(DOCS.contains("also include `floating_count`"));
    }

    #[test]
    fn evolve8h_wave_124_health_occupancy_includes_floating_count() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("floating={floating}"));
        assert!(DOCS.contains("and `floating_count`"));
    }

    #[test]
    fn evolve8h_wave_125_togglefloating_broadcasts_monitor_bar() {
        const TOGGLES: &str = include_str!("features/toggles.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let togglefloating = TOGGLES
            .split_once("pub fn togglefloating(")
            .expect("togglefloating")
            .1
            .split_once("pub fn togglesticky(")
            .expect("togglesticky follows")
            .0;
        assert!(togglefloating.contains("broadcast_monitor_bar_ipc(backend, sel_mon_key)"));
        assert!(DOCS.contains("`togglefloating` emits `monitor/bar`"));
    }

    #[test]
    fn evolve8h_wave_126_get_status_show_bar_rustdoc_names_floating_count() {
        const IPC: &str = include_str!("../ipc.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(IPC.contains("Also `floating_count`."));
        assert!(DOCS.contains("`on_view_count` /\n`floating_count`") || DOCS.contains("`on_view_count` / `floating_count`"));
    }

    #[test]
    fn evolve8h_wave_127_readme_occupancy_names_floating_count() {
        const README: &str = include_str!("../../README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(README.contains("It also includes `floating_count`."));
        assert!(DOCS.contains("README occupancy JSON also names `floating_count`"));
    }

    #[test]
    fn evolve8h_wave_128_tools_readme_occupancy_names_floating_count() {
        const TOOLS: &str = include_str!("../../tools/README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOLS.contains("It also includes `floating_count`."));
        assert!(DOCS.contains("`tools/README.md` also names `floating_count`"));
    }

    #[test]
    fn evolve8h_wave_129_window_tabs_occupancy_names_floating_count() {
        const TABS: &str = include_str!("../../docs/window-tabs.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TABS.contains("`on_view_count` / `floating_count`"));
        assert!(DOCS.contains("Window-tabs docs also name `floating_count`"));
    }

    #[test]
    fn evolve8h_wave_130_occupancy_snapshot_includes_minimized_count() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let query = SOURCE
            .split_once("fn query_show_bar_for_monitor(")
            .expect("query_show_bar_for_monitor")
            .1
            .split_once("fn query_focused_show_bar(")
            .expect("focused follows")
            .0;
        assert!(query.contains("\"minimized_count\":"));
        assert!(query.contains("c.state.is_hidden"));
        assert!(DOCS.contains("also include `minimized_count`"));
    }

    #[test]
    fn evolve8h_wave_131_health_occupancy_includes_minimized_count() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("minimized={minimized}"));
        assert!(DOCS.contains("and `minimized_count`"));
    }

    #[test]
    fn evolve8h_wave_132_minimize_always_broadcasts_monitor_bar() {
        const STATE: &str = include_str!("window_state.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let minimize = STATE
            .split_once("pub(crate) fn set_client_minimized(")
            .expect("set_client_minimized")
            .1
            .split_once("mod restore_rect_tests")
            .expect("restore tests follow")
            .0;
        assert!(minimize.contains("let _ = was_fullscreen"));
        assert!(minimize.contains("broadcast_monitor_bar_ipc(backend, mon_key)"));
        assert!(DOCS.contains("Minimizing or restoring a client emits `monitor/bar` even when it was not fullscreen"));
    }

    #[test]
    fn evolve8h_wave_133_get_status_show_bar_rustdoc_names_minimized_count() {
        const IPC: &str = include_str!("../ipc.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(IPC.contains("Also `minimized_count`."));
        assert!(DOCS.contains("`floating_count` / `minimized_count`"));
    }

    #[test]
    fn evolve8h_wave_134_readme_occupancy_names_minimized_count() {
        const README: &str = include_str!("../../README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(README.contains("It also includes `minimized_count`."));
        assert!(DOCS.contains("README occupancy JSON also names `minimized_count`"));
    }

    #[test]
    fn evolve8h_wave_135_tools_readme_occupancy_names_minimized_count() {
        const TOOLS: &str = include_str!("../../tools/README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOLS.contains("It also includes `minimized_count`."));
        assert!(DOCS.contains("`tools/README.md` also names `minimized_count`"));
    }

    #[test]
    fn evolve8h_wave_136_window_tabs_occupancy_names_minimized_count() {
        const TABS: &str = include_str!("../../docs/window-tabs.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TABS.contains("`floating_count` / `minimized_count`"));
        assert!(DOCS.contains("Window-tabs docs also name `minimized_count`"));
    }

    #[test]
    fn evolve8h_wave_137_occupancy_snapshot_includes_sticky_count() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let query = SOURCE
            .split_once("fn query_show_bar_for_monitor(")
            .expect("query_show_bar_for_monitor")
            .1
            .split_once("fn query_focused_show_bar(")
            .expect("focused follows")
            .0;
        assert!(query.contains("\"sticky_count\":"));
        assert!(query.contains("c.state.is_sticky"));
        assert!(DOCS.contains("also include `sticky_count`"));
    }

    #[test]
    fn evolve8h_wave_138_health_occupancy_includes_sticky_count() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("sticky={sticky}"));
        assert!(DOCS.contains("and `sticky_count`"));
    }

    #[test]
    fn evolve8h_wave_139_sticky_always_broadcasts_monitor_bar() {
        const TOGGLES: &str = include_str!("features/toggles.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let sticky = TOGGLES
            .split_once("pub(crate) fn set_client_sticky(")
            .expect("set_client_sticky")
            .1
            .split_once("pub(crate) fn prepare_for_compositor_disable(")
            .expect("prepare_for_compositor_disable follows")
            .0;
        assert!(sticky.contains("let _ = was_fullscreen"));
        assert!(sticky.contains("broadcast_monitor_bar_ipc(backend, mk)"));
        assert!(DOCS.contains("Toggling sticky emits `monitor/bar` even when the client was not fullscreen"));
    }

    #[test]
    fn evolve8h_wave_140_get_status_show_bar_rustdoc_names_sticky_count() {
        const IPC: &str = include_str!("../ipc.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(IPC.contains("Also `sticky_count`."));
        assert!(DOCS.contains("`get_status.show_bar` rustdoc also names `sticky_count`"));
    }

    #[test]
    fn evolve8h_wave_141_readme_occupancy_names_sticky_count() {
        const README: &str = include_str!("../../README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(README.contains("It also includes `sticky_count`."));
        assert!(DOCS.contains("README occupancy JSON also names `sticky_count`"));
    }

    #[test]
    fn evolve8h_wave_142_tools_readme_occupancy_names_sticky_count() {
        const TOOLS: &str = include_str!("../../tools/README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOLS.contains("It also includes `sticky_count`."));
        assert!(DOCS.contains("`tools/README.md` also names `sticky_count`"));
    }

    #[test]
    fn evolve8h_wave_143_window_tabs_occupancy_names_sticky_count() {
        const TABS: &str = include_str!("../../docs/window-tabs.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TABS.contains("`minimized_count` / `sticky_count`"));
        assert!(DOCS.contains("Window-tabs docs also name `sticky_count`"));
    }

    #[test]
    fn evolve8h_wave_144_occupancy_snapshot_includes_urgent_count() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let query = SOURCE
            .split_once("fn query_show_bar_for_monitor(")
            .expect("query_show_bar_for_monitor")
            .1
            .split_once("fn query_focused_show_bar(")
            .expect("focused follows")
            .0;
        assert!(query.contains("\"urgent_count\":"));
        assert!(query.contains("c.state.demands_attention"));
        assert!(DOCS.contains("also include `urgent_count`"));
    }

    #[test]
    fn evolve8h_wave_145_health_occupancy_includes_urgent_count() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("urgent={urgent}"));
        assert!(DOCS.contains("and `urgent_count`"));
    }

    #[test]
    fn evolve8h_wave_146_urgency_changes_broadcast_monitor_bar() {
        const SOURCE: &str = include_str!("window_state.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let urgent = SOURCE
            .split_once("fn sync_client_urgent_state(")
            .expect("sync_client_urgent_state")
            .1
            .split_once("pub(super) fn setclientstate(")
            .expect("setclientstate follows")
            .0;
        assert!(urgent.contains("broadcast_monitor_bar_ipc(backend, mk)"));
        assert!(DOCS.contains("Urgency changes emit `monitor/bar`"));
    }

    #[test]
    fn evolve8h_wave_147_demands_attention_broadcasts_monitor_bar() {
        const SOURCE: &str = include_str!("event_dispatcher.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let attention = SOURCE
            .split_once("pub(super) fn set_client_demands_attention(")
            .expect("set_client_demands_attention")
            .1
            .split_once("mod tests {")
            .expect("tests follow")
            .0;
        assert!(attention.contains("broadcast_monitor_bar_ipc(backend, mk)"));
        assert!(DOCS.contains("Demands-attention changes emit `monitor/bar`"));
    }

    #[test]
    fn evolve8h_wave_148_get_status_show_bar_rustdoc_names_urgent_count() {
        const IPC: &str = include_str!("../ipc.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(IPC.contains("Also `urgent_count`."));
        assert!(DOCS.contains("`get_status.show_bar` rustdoc also names `urgent_count`"));
    }

    #[test]
    fn evolve8h_wave_149_readme_occupancy_names_urgent_count() {
        const README: &str = include_str!("../../README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(README.contains("It also includes `urgent_count`."));
        assert!(DOCS.contains("README occupancy JSON also names `urgent_count`"));
    }

    #[test]
    fn evolve8h_wave_150_tools_readme_occupancy_names_urgent_count() {
        const TOOLS: &str = include_str!("../../tools/README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOLS.contains("It also includes `urgent_count`."));
        assert!(DOCS.contains("`tools/README.md` also names `urgent_count`"));
    }

    #[test]
    fn evolve8h_wave_151_window_tabs_occupancy_names_urgent_count() {
        const TABS: &str = include_str!("../../docs/window-tabs.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TABS.contains("`sticky_count` / `urgent_count`"));
        assert!(DOCS.contains("Window-tabs docs also name `urgent_count`"));
    }

    #[test]
    fn evolve8h_wave_152_occupancy_snapshot_includes_fullscreen_count() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let query = SOURCE
            .split_once("fn query_show_bar_for_monitor(")
            .expect("query_show_bar_for_monitor")
            .1
            .split_once("fn query_focused_show_bar(")
            .expect("focused follows")
            .0;
        assert!(query.contains("\"fullscreen_count\":"));
        assert!(query.contains("c.state.is_fullscreen"));
        assert!(DOCS.contains("also include `fullscreen_count`"));
    }

    #[test]
    fn evolve8h_wave_153_health_occupancy_includes_fullscreen_count() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("fullscreen_count={fullscreen_count}"));
        assert!(DOCS.contains("and `fullscreen_count`"));
    }

    #[test]
    fn evolve8h_wave_154_get_status_show_bar_rustdoc_names_fullscreen_count() {
        const IPC: &str = include_str!("../ipc.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(IPC.contains("Also `fullscreen_count`."));
        assert!(DOCS.contains("`get_status.show_bar` rustdoc also names `fullscreen_count`"));
    }

    #[test]
    fn evolve8h_wave_155_readme_occupancy_names_fullscreen_count() {
        const README: &str = include_str!("../../README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(README.contains("It also includes `fullscreen_count`."));
        assert!(DOCS.contains("README occupancy JSON also names `fullscreen_count`"));
    }

    #[test]
    fn evolve8h_wave_156_tools_readme_occupancy_names_fullscreen_count() {
        const TOOLS: &str = include_str!("../../tools/README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOLS.contains("It also includes `fullscreen_count`."));
        assert!(DOCS.contains("`tools/README.md` also names `fullscreen_count`"));
    }

    #[test]
    fn evolve8h_wave_157_window_tabs_occupancy_names_fullscreen_count() {
        const TABS: &str = include_str!("../../docs/window-tabs.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TABS.contains("`urgent_count` / `fullscreen_count`"));
        assert!(DOCS.contains("Window-tabs docs also name `fullscreen_count`"));
    }

    #[test]
    fn evolve8h_wave_158_occupancy_snapshot_includes_pip_count() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let query = SOURCE
            .split_once("fn query_show_bar_for_monitor(")
            .expect("query_show_bar_for_monitor")
            .1
            .split_once("fn query_focused_show_bar(")
            .expect("focused follows")
            .0;
        assert!(query.contains("\"pip_count\":"));
        assert!(query.contains("c.state.is_pip"));
        assert!(DOCS.contains("also include `pip_count`"));
    }

    #[test]
    fn evolve8h_wave_159_health_occupancy_includes_pip_count() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("pip={pip}"));
        assert!(DOCS.contains("and `pip_count`"));
    }

    #[test]
    fn evolve8h_wave_160_pip_changes_broadcast_monitor_bar() {
        const SOURCE: &str = include_str!("window_state.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let pip = SOURCE
            .split_once("pub(super) fn set_client_pip(")
            .expect("set_client_pip")
            .1
            .split_once("fn set_client_pip_inner(")
            .expect("inner follows")
            .0;
        assert!(pip.contains("broadcast_monitor_bar_ipc(backend, mk)"));
        assert!(DOCS.contains("Toggling picture-in-picture emits `monitor/bar`"));
    }

    #[test]
    fn evolve8h_wave_161_get_status_show_bar_rustdoc_names_pip_count() {
        const IPC: &str = include_str!("../ipc.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(IPC.contains("Also `pip_count`."));
        assert!(DOCS.contains("`get_status.show_bar` rustdoc also names `pip_count`"));
    }

    #[test]
    fn evolve8h_wave_162_readme_occupancy_names_pip_count() {
        const README: &str = include_str!("../../README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(README.contains("It also includes `pip_count`."));
        assert!(DOCS.contains("README occupancy JSON also names `pip_count`"));
    }

    #[test]
    fn evolve8h_wave_163_tools_readme_occupancy_names_pip_count() {
        const TOOLS: &str = include_str!("../../tools/README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOLS.contains("It also includes `pip_count`."));
        assert!(DOCS.contains("`tools/README.md` also names `pip_count`"));
    }

    #[test]
    fn evolve8h_wave_164_window_tabs_occupancy_names_pip_count() {
        const TABS: &str = include_str!("../../docs/window-tabs.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TABS.contains("`fullscreen_count` / `pip_count`"));
        assert!(DOCS.contains("Window-tabs docs also name `pip_count`"));
    }

    #[test]
    fn evolve8h_wave_165_occupancy_snapshot_includes_maximized_count() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let query = SOURCE
            .split_once("fn query_show_bar_for_monitor(")
            .expect("query_show_bar_for_monitor")
            .1
            .split_once("fn query_focused_show_bar(")
            .expect("focused follows")
            .0;
        assert!(query.contains("\"maximized_count\":"));
        assert!(query.contains("c.state.is_maximized_horz"));
        assert!(DOCS.contains("also include `maximized_count`"));
    }

    #[test]
    fn evolve8h_wave_166_health_occupancy_includes_maximized_count() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("maximized={maximized}"));
        assert!(DOCS.contains("and `maximized_count`"));
    }

    #[test]
    fn evolve8h_wave_167_maximize_broadcasts_monitor_bar() {
        const SOURCE: &str = include_str!("maximize.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let tx = SOURCE
            .split_once("fn maximize_transaction(")
            .expect("maximize_transaction")
            .1
            .split_once("fn sync_maximize_restore_property(")
            .expect("sync follows")
            .0;
        assert!(tx.contains("broadcast_monitor_bar_ipc(backend, mk)"));
        assert!(DOCS.contains("Toggling maximize emits `monitor/bar`"));
    }

    #[test]
    fn evolve8h_wave_168_unmaximize_in_place_broadcasts_monitor_bar() {
        const SOURCE: &str = include_str!("maximize.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let unmax = SOURCE
            .split_once("pub(crate) fn unmaximize_in_place(")
            .expect("unmaximize_in_place")
            .1
            .split_once("pub(crate) fn maximize_snapshot(")
            .expect("snapshot follows")
            .0;
        assert!(unmax.contains("broadcast_monitor_bar_ipc(backend, mk)"));
        assert!(DOCS.contains("Unmaximize-in-place emits `monitor/bar`"));
    }

    #[test]
    fn evolve8h_wave_169_reinstate_maximize_broadcasts_monitor_bar() {
        const SOURCE: &str = include_str!("maximize.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let reinstate = SOURCE
            .split_once("pub(crate) fn reinstate_maximize_snapshot(")
            .expect("reinstate_maximize_snapshot")
            .1
            .split_once("pub(crate) fn refit_maximized_clients(")
            .expect("refit follows")
            .0;
        assert!(reinstate.contains("broadcast_monitor_bar_ipc(backend, mk)"));
        assert!(DOCS.contains("Reinstating a maximize snapshot emits `monitor/bar`"));
    }

    #[test]
    fn evolve8h_wave_170_get_status_show_bar_rustdoc_names_maximized_count() {
        const IPC: &str = include_str!("../ipc.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(IPC.contains("Also `maximized_count`."));
        assert!(DOCS.contains("`get_status.show_bar` rustdoc also names `maximized_count`"));
    }

    #[test]
    fn evolve8h_wave_171_readme_occupancy_names_maximized_count() {
        const README: &str = include_str!("../../README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(README.contains("It also includes `maximized_count`."));
        assert!(DOCS.contains("README occupancy JSON also names `maximized_count`"));
    }

    #[test]
    fn evolve8h_wave_172_tools_readme_occupancy_names_maximized_count() {
        const TOOLS: &str = include_str!("../../tools/README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOLS.contains("It also includes `maximized_count`."));
        assert!(DOCS.contains("`tools/README.md` also names `maximized_count`"));
    }

    #[test]
    fn evolve8h_wave_173_window_tabs_occupancy_names_maximized_count() {
        const TABS: &str = include_str!("../../docs/window-tabs.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TABS.contains("`pip_count` / `maximized_count`"));
        assert!(DOCS.contains("Window-tabs docs also name `maximized_count`"));
    }

    #[test]
    fn evolve8h_wave_174_occupancy_snapshot_includes_above_count() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let query = SOURCE
            .split_once("fn query_show_bar_for_monitor(")
            .expect("query_show_bar_for_monitor")
            .1
            .split_once("fn query_focused_show_bar(")
            .expect("focused follows")
            .0;
        assert!(query.contains("\"above_count\":"));
        assert!(query.contains("c.state.is_above"));
        assert!(DOCS.contains("also include `above_count`"));
    }

    #[test]
    fn evolve8h_wave_175_health_occupancy_includes_above_count() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("above={above}"));
        assert!(DOCS.contains("and `above_count`"));
    }

    #[test]
    fn evolve8h_wave_176_stacking_flags_broadcast_monitor_bar() {
        const SOURCE: &str = include_str!("event_dispatcher.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let stacking = SOURCE
            .split_once("pub(crate) fn apply_external_stacking_request(")
            .expect("apply_external_stacking_request")
            .1
            .split_once("fn requested_attention_state(")
            .expect("attention follows")
            .0;
        assert!(stacking.contains("broadcast_monitor_bar_ipc(backend, mk)"));
        assert!(DOCS.contains("Keep-above and keep-below changes emit `monitor/bar`"));
    }

    #[test]
    fn evolve8h_wave_177_get_status_show_bar_rustdoc_names_above_count() {
        const IPC: &str = include_str!("../ipc.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(IPC.contains("Also `above_count`."));
        assert!(DOCS.contains("`get_status.show_bar` rustdoc also names `above_count`"));
    }

    #[test]
    fn evolve8h_wave_178_readme_occupancy_names_above_count() {
        const README: &str = include_str!("../../README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(README.contains("It also includes `above_count`."));
        assert!(DOCS.contains("README occupancy JSON also names `above_count`"));
    }

    #[test]
    fn evolve8h_wave_179_tools_readme_occupancy_names_above_count() {
        const TOOLS: &str = include_str!("../../tools/README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOLS.contains("It also includes `above_count`."));
        assert!(DOCS.contains("`tools/README.md` also names `above_count`"));
    }

    #[test]
    fn evolve8h_wave_180_window_tabs_occupancy_names_above_count() {
        const TABS: &str = include_str!("../../docs/window-tabs.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TABS.contains("`maximized_count` / `above_count`"));
        assert!(DOCS.contains("Window-tabs docs also name `above_count`"));
    }

    #[test]
    fn evolve8h_wave_181_occupancy_snapshot_includes_below_count() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let query = SOURCE
            .split_once("fn query_show_bar_for_monitor(")
            .expect("query_show_bar_for_monitor")
            .1
            .split_once("fn query_focused_show_bar(")
            .expect("focused follows")
            .0;
        assert!(query.contains("\"below_count\":"));
        assert!(query.contains("c.state.is_below"));
        assert!(DOCS.contains("also include `below_count`"));
    }

    #[test]
    fn evolve8h_wave_182_health_occupancy_includes_below_count() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("below={below}"));
        assert!(DOCS.contains("and `below_count`"));
    }

    #[test]
    fn evolve8h_wave_183_get_status_show_bar_rustdoc_names_below_count() {
        const IPC: &str = include_str!("../ipc.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(IPC.contains("Also `below_count`."));
        assert!(DOCS.contains("`get_status.show_bar` rustdoc also names `below_count`"));
    }

    #[test]
    fn evolve8h_wave_184_readme_occupancy_names_below_count() {
        const README: &str = include_str!("../../README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(README.contains("It also includes `below_count`."));
        assert!(DOCS.contains("README occupancy JSON also names `below_count`"));
    }

    #[test]
    fn evolve8h_wave_185_tools_readme_occupancy_names_below_count() {
        const TOOLS: &str = include_str!("../../tools/README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOLS.contains("It also includes `below_count`."));
        assert!(DOCS.contains("`tools/README.md` also names `below_count`"));
    }

    #[test]
    fn evolve8h_wave_186_window_tabs_occupancy_names_below_count() {
        const TABS: &str = include_str!("../../docs/window-tabs.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TABS.contains("`above_count` / `below_count`"));
        assert!(DOCS.contains("Window-tabs docs also name `below_count`"));
    }

    #[test]
    fn evolve8h_wave_187_occupancy_snapshot_includes_scratchpad_count() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let query = SOURCE
            .split_once("fn query_show_bar_for_monitor(")
            .expect("query_show_bar_for_monitor")
            .1
            .split_once("fn query_focused_show_bar(")
            .expect("focused follows")
            .0;
        assert!(query.contains("\"scratchpad_count\":"));
        assert!(query.contains("self.scratchpads.values()"));
        assert!(DOCS.contains("also include `scratchpad_count`"));
    }

    #[test]
    fn evolve8h_wave_188_health_occupancy_includes_scratchpad_count() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("scratchpad={scratchpad}"));
        assert!(DOCS.contains("and `scratchpad_count`"));
    }

    #[test]
    fn evolve8h_wave_189_hiding_scratchpad_broadcasts_monitor_bar() {
        const SOURCE: &str = include_str!("features/toggles.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let hide = SOURCE
            .split_once("pub fn togglescratchpad(")
            .expect("togglescratchpad")
            .1
            .split_once("let was_minimized = self")
            .expect("show path follows")
            .0;
        assert!(hide.contains("broadcast_monitor_bar_ipc(backend, mk)"));
        assert!(DOCS.contains("Hiding a scratchpad emits `monitor/bar`"));
    }

    #[test]
    fn evolve8h_wave_190_showing_scratchpad_broadcasts_monitor_bar() {
        const SOURCE: &str = include_str!("features/toggles.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let show = SOURCE
            .split_once("let was_minimized = self")
            .expect("show path")
            .1
            .split_once("pub fn togglepip(")
            .expect("togglepip follows")
            .0;
        assert!(show.contains("broadcast_monitor_bar_ipc(backend, mk)"));
        assert!(DOCS.contains("Showing a scratchpad emits `monitor/bar`"));
    }

    #[test]
    fn evolve8h_wave_191_get_status_show_bar_rustdoc_names_scratchpad_count() {
        const IPC: &str = include_str!("../ipc.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(IPC.contains("Also `scratchpad_count`."));
        assert!(DOCS.contains("`get_status.show_bar` rustdoc also names `scratchpad_count`"));
    }

    #[test]
    fn evolve8h_wave_192_readme_occupancy_names_scratchpad_count() {
        const README: &str = include_str!("../../README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(README.contains("It also includes `scratchpad_count`."));
        assert!(DOCS.contains("README occupancy JSON also names `scratchpad_count`"));
    }

    #[test]
    fn evolve8h_wave_193_tools_readme_occupancy_names_scratchpad_count() {
        const TOOLS: &str = include_str!("../../tools/README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOLS.contains("It also includes `scratchpad_count`."));
        assert!(DOCS.contains("`tools/README.md` also names `scratchpad_count`"));
    }

    #[test]
    fn evolve8h_wave_194_window_tabs_occupancy_names_scratchpad_count() {
        const TABS: &str = include_str!("../../docs/window-tabs.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TABS.contains("`below_count` / `scratchpad_count`"));
        assert!(DOCS.contains("Window-tabs docs also name `scratchpad_count`"));
    }

    #[test]
    fn evolve8h_wave_195_occupancy_snapshot_includes_tabbed_count() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let query = SOURCE
            .split_once("fn query_show_bar_for_monitor(")
            .expect("query_show_bar_for_monitor")
            .1
            .split_once("fn query_focused_show_bar(")
            .expect("focused follows")
            .0;
        assert!(query.contains("\"tabbed_count\":"));
        assert!(query.contains("self.tab_group_clients(mk)"));
        assert!(DOCS.contains("also include `tabbed_count`"));
    }

    #[test]
    fn evolve8h_wave_196_health_occupancy_includes_tabbed_count() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("tabbed={tabbed}"));
        assert!(DOCS.contains("and `tabbed_count`"));
    }

    #[test]
    fn evolve8h_wave_197_get_status_show_bar_rustdoc_names_tabbed_count() {
        const IPC: &str = include_str!("../ipc.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(IPC.contains("Also `tabbed_count`."));
        assert!(DOCS.contains("`get_status.show_bar` rustdoc also names `tabbed_count`"));
    }

    #[test]
    fn evolve8h_wave_198_readme_occupancy_names_tabbed_count() {
        const README: &str = include_str!("../../README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(README.contains("It also includes `tabbed_count`."));
        assert!(DOCS.contains("README occupancy JSON also names `tabbed_count`"));
    }

    #[test]
    fn evolve8h_wave_199_tools_readme_occupancy_names_tabbed_count() {
        const TOOLS: &str = include_str!("../../tools/README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOLS.contains("It also includes `tabbed_count`."));
        assert!(DOCS.contains("`tools/README.md` also names `tabbed_count`"));
    }

    #[test]
    fn evolve8h_wave_200_window_tabs_occupancy_names_tabbed_count() {
        const TABS: &str = include_str!("../../docs/window-tabs.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TABS.contains("`scratchpad_count` / `tabbed_count`"));
        assert!(DOCS.contains("Window-tabs docs also name `tabbed_count`"));
    }

    #[test]
    fn evolve8h_wave_201_occupancy_snapshot_includes_dock_count() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let query = SOURCE
            .split_once("fn query_show_bar_for_monitor(")
            .expect("query_show_bar_for_monitor")
            .1
            .split_once("fn query_focused_show_bar(")
            .expect("focused follows")
            .0;
        assert!(query.contains("\"dock_count\":"));
        assert!(query.contains("c.state.is_dock"));
        assert!(DOCS.contains("also include `dock_count`"));
    }

    #[test]
    fn evolve8h_wave_202_health_occupancy_includes_dock_count() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("dock={dock}"));
        assert!(DOCS.contains("and `dock_count`"));
    }

    #[test]
    fn evolve8h_wave_203_get_status_show_bar_rustdoc_names_dock_count() {
        const IPC: &str = include_str!("../ipc.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(IPC.contains("Also `dock_count`."));
        assert!(DOCS.contains("`get_status.show_bar` rustdoc also names `dock_count`"));
    }

    #[test]
    fn evolve8h_wave_204_readme_occupancy_names_dock_count() {
        const README: &str = include_str!("../../README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(README.contains("It also includes `dock_count`."));
        assert!(DOCS.contains("README occupancy JSON also names `dock_count`"));
    }

    #[test]
    fn evolve8h_wave_205_tools_readme_occupancy_names_dock_count() {
        const TOOLS: &str = include_str!("../../tools/README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOLS.contains("It also includes `dock_count`."));
        assert!(DOCS.contains("`tools/README.md` also names `dock_count`"));
    }

    #[test]
    fn evolve8h_wave_206_window_tabs_occupancy_names_dock_count() {
        const TABS: &str = include_str!("../../docs/window-tabs.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TABS.contains("`tabbed_count` / `dock_count`"));
        assert!(DOCS.contains("Window-tabs docs also name `dock_count`"));
    }

    #[test]
    fn evolve8h_wave_207_occupancy_snapshot_includes_desktop_count() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let query = SOURCE
            .split_once("fn query_show_bar_for_monitor(")
            .expect("query_show_bar_for_monitor")
            .1
            .split_once("fn query_focused_show_bar(")
            .expect("focused follows")
            .0;
        assert!(query.contains("\"desktop_count\":"));
        assert!(query.contains("c.state.is_desktop"));
        assert!(DOCS.contains("also include `desktop_count`"));
    }

    #[test]
    fn evolve8h_wave_208_health_occupancy_includes_desktop_count() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("desktop={desktop}"));
        assert!(DOCS.contains("and `desktop_count`"));
    }

    #[test]
    fn evolve8h_wave_209_get_status_show_bar_rustdoc_names_desktop_count() {
        const IPC: &str = include_str!("../ipc.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(IPC.contains("Also `desktop_count`."));
        assert!(DOCS.contains("`get_status.show_bar` rustdoc also names `desktop_count`"));
    }

    #[test]
    fn evolve8h_wave_210_readme_occupancy_names_desktop_count() {
        const README: &str = include_str!("../../README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(README.contains("It also includes `desktop_count`."));
        assert!(DOCS.contains("README occupancy JSON also names `desktop_count`"));
    }

    #[test]
    fn evolve8h_wave_211_tools_readme_occupancy_names_desktop_count() {
        const TOOLS: &str = include_str!("../../tools/README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOLS.contains("It also includes `desktop_count`."));
        assert!(DOCS.contains("`tools/README.md` also names `desktop_count`"));
    }

    #[test]
    fn evolve8h_wave_212_window_tabs_occupancy_names_desktop_count() {
        const TABS: &str = include_str!("../../docs/window-tabs.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TABS.contains("`dock_count` / `desktop_count`"));
        assert!(DOCS.contains("Window-tabs docs also name `desktop_count`"));
    }

    #[test]
    fn evolve8h_wave_213_occupancy_snapshot_includes_never_focus_count() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let query = SOURCE
            .split_once("fn query_show_bar_for_monitor(")
            .expect("query_show_bar_for_monitor")
            .1
            .split_once("fn query_focused_show_bar(")
            .expect("focused follows")
            .0;
        assert!(query.contains("\"never_focus_count\":"));
        assert!(query.contains("c.state.never_focus"));
        assert!(DOCS.contains("also include `never_focus_count`"));
    }

    #[test]
    fn evolve8h_wave_214_health_occupancy_includes_never_focus_count() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("never_focus={never_focus}"));
        assert!(DOCS.contains("and `never_focus_count`"));
    }

    #[test]
    fn evolve8h_wave_215_get_status_show_bar_rustdoc_names_never_focus_count() {
        const IPC: &str = include_str!("../ipc.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(IPC.contains("Also `never_focus_count`."));
        assert!(DOCS.contains("`get_status.show_bar` rustdoc also names `never_focus_count`"));
    }

    #[test]
    fn evolve8h_wave_216_readme_occupancy_names_never_focus_count() {
        const README: &str = include_str!("../../README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(README.contains("It also includes `never_focus_count`."));
        assert!(DOCS.contains("README occupancy JSON also names `never_focus_count`"));
    }

    #[test]
    fn evolve8h_wave_217_tools_readme_occupancy_names_never_focus_count() {
        const TOOLS: &str = include_str!("../../tools/README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOLS.contains("It also includes `never_focus_count`."));
        assert!(DOCS.contains("`tools/README.md` also names `never_focus_count`"));
    }

    #[test]
    fn evolve8h_wave_218_window_tabs_occupancy_names_never_focus_count() {
        const TABS: &str = include_str!("../../docs/window-tabs.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TABS.contains("`desktop_count` / `never_focus_count`"));
        assert!(DOCS.contains("Window-tabs docs also name `never_focus_count`"));
    }

    #[test]
    fn evolve8h_wave_219_occupancy_snapshot_includes_skip_taskbar_count() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let query = SOURCE
            .split_once("fn query_show_bar_for_monitor(")
            .expect("query_show_bar_for_monitor")
            .1
            .split_once("fn query_focused_show_bar(")
            .expect("focused follows")
            .0;
        assert!(query.contains("\"skip_taskbar_count\":"));
        assert!(query.contains("c.state.skip_taskbar"));
        assert!(DOCS.contains("also include `skip_taskbar_count`"));
    }

    #[test]
    fn evolve8h_wave_220_health_occupancy_includes_skip_taskbar_count() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("skip_taskbar={skip_taskbar}"));
        assert!(DOCS.contains("and `skip_taskbar_count`"));
    }

    #[test]
    fn evolve8h_wave_221_get_status_show_bar_rustdoc_names_skip_taskbar_count() {
        const IPC: &str = include_str!("../ipc.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(IPC.contains("Also `skip_taskbar_count`."));
        assert!(DOCS.contains("`get_status.show_bar` rustdoc also names `skip_taskbar_count`"));
    }

    #[test]
    fn evolve8h_wave_222_readme_occupancy_names_skip_taskbar_count() {
        const README: &str = include_str!("../../README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(README.contains("It also includes `skip_taskbar_count`."));
        assert!(DOCS.contains("README occupancy JSON also names `skip_taskbar_count`"));
    }

    #[test]
    fn evolve8h_wave_223_tools_readme_occupancy_names_skip_taskbar_count() {
        const TOOLS: &str = include_str!("../../tools/README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOLS.contains("It also includes `skip_taskbar_count`."));
        assert!(DOCS.contains("`tools/README.md` also names `skip_taskbar_count`"));
    }

    #[test]
    fn evolve8h_wave_224_window_tabs_occupancy_names_skip_taskbar_count() {
        const TABS: &str = include_str!("../../docs/window-tabs.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TABS.contains("`never_focus_count` / `skip_taskbar_count`"));
        assert!(DOCS.contains("Window-tabs docs also name `skip_taskbar_count`"));
    }

    #[test]
    fn evolve8h_wave_225_occupancy_snapshot_includes_skip_pager_count() {
        const SOURCE: &str = include_str!("ipc_handler.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let query = SOURCE
            .split_once("fn query_show_bar_for_monitor(")
            .expect("query_show_bar_for_monitor")
            .1
            .split_once("fn query_focused_show_bar(")
            .expect("focused follows")
            .0;
        assert!(query.contains("\"skip_pager_count\":"));
        assert!(query.contains("c.state.skip_pager"));
        assert!(DOCS.contains("also include `skip_pager_count`"));
    }

    #[test]
    fn evolve8h_wave_226_health_occupancy_includes_skip_pager_count() {
        const TOOL: &str = include_str!("../../tools/jwm_tool.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOL.contains("skip_pager={skip_pager}"));
        assert!(DOCS.contains("and `skip_pager_count`"));
    }

    #[test]
    fn evolve8h_wave_227_get_status_show_bar_rustdoc_names_skip_pager_count() {
        const IPC: &str = include_str!("../ipc.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(IPC.contains("Also `skip_pager_count`."));
        assert!(DOCS.contains("`get_status.show_bar` rustdoc also names `skip_pager_count`"));
    }

    #[test]
    fn evolve8h_wave_228_readme_occupancy_names_skip_pager_count() {
        const README: &str = include_str!("../../README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(README.contains("It also includes `skip_pager_count`."));
        assert!(DOCS.contains("README occupancy JSON also names `skip_pager_count`"));
    }

    #[test]
    fn evolve8h_wave_229_tools_readme_occupancy_names_skip_pager_count() {
        const TOOLS: &str = include_str!("../../tools/README.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TOOLS.contains("It also includes `skip_pager_count`."));
        assert!(DOCS.contains("`tools/README.md` also names `skip_pager_count`"));
    }

    #[test]
    fn evolve8h_wave_230_window_tabs_occupancy_names_skip_pager_count() {
        const TABS: &str = include_str!("../../docs/window-tabs.md");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        assert!(TABS.contains("`skip_taskbar_count` / `skip_pager_count`"));
        assert!(DOCS.contains("Window-tabs docs also name `skip_pager_count`"));
    }

    #[test]
    fn evolve8h_wave_231_skip_taskbar_ewmh_broadcasts_monitor_bar() {
        const SOURCE: &str = include_str!("event_dispatcher.rs");
        const DOCS: &str = include_str!("../../docs/monitor-lock.md");
        let skip = SOURCE
            .split_once("NetWmState::SkipTaskbar => {")
            .expect("SkipTaskbar")
            .1
            .split_once("NetWmState::SkipPager => {")
            .expect("SkipPager follows")
            .0;
        assert!(skip.contains("broadcast_monitor_bar_ipc(backend, mk)"));
        assert!(DOCS.contains("`_NET_WM_STATE_SKIP_TASKBAR` emits `monitor/bar`"));
    }
}
