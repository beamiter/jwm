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
    self, IpcEvent, IpcResponse, MonitorInfoIpc, RuntimeCounts, RuntimeFeatureStates,
    RuntimeHealth, RuntimeStatusV1, TreeNode, WindowInfo, WorkspaceInfo,
};
use crate::ipc_server::IncomingIpc;

fn runtime_health(config_status: &serde_json::Value, monitor_count: usize) -> RuntimeHealth {
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
    RuntimeHealth::from_reasons(reasons)
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

fn client_window_info(client: &WMClient, monitor: i32, is_focused: bool) -> WindowInfo {
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
        is_pip: client.state.is_pip,
        is_minimized: client.state.is_hidden,
        is_focused,
    }
}

fn recording_file_is_valid(path: &str) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.len() > 0)
        && std::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=codec_name",
                "-of",
                "default=nw=1:nk=1",
                path,
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
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

fn workspace_layout_state(mon: &WMMonitor, tag_index: usize) -> (String, f32, u32) {
    let current_layout = || format!("{:?}", *mon.lt);
    let Some(pertag_index) = tag_index.checked_add(1) else {
        return (current_layout(), mon.layout.m_fact, mon.layout.n_master);
    };
    let Some(pertag) = mon.pertag.as_ref() else {
        return (current_layout(), mon.layout.m_fact, mon.layout.n_master);
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

    (layout, m_fact, n_master)
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

fn color_session_policy_json(
    outputs: &[crate::backend::api::OutputInfo],
    hdr_enabled: bool,
    render_path_enabled: bool,
    scene_linear_enabled: bool,
    advanced_enabled: bool,
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
    if hdr_postprocess_requested {
        blockers.push("hdr_signalling_enable_unavailable_until_external_elements_adapted");
    }

    // This status endpoint has no last-frame delivery snapshot. Report the
    // implemented policy and its eligibility constraints, not an invented
    // claim that software regions or KMS properties are active right now.
    let per_output_delivery_available = render_path_enabled && scene_linear_enabled;

    serde_json::json!({
        "hdr_output_count": hdr_output_count,
        "sdr_output_count": sdr_output_count,
        "mixed_hdr_outputs": mixed_hdr_outputs,
        "heterogeneous_output_profiles": heterogeneous_output_profiles,
        // Compatibility field now reflects the physical signal instead of a
        // configuration request. HDR enable is fail-closed in this slice.
        "hdr_active": false,
        "hdr_active_semantics": "physical_output_signal_fail_closed_sdr",
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
            "runtime_output_signal_policy": "sdr_srgb_fail_closed",
            "hdr_signalling_enable_available": false,
        },
        "fallback_policy": {
            "route": "global_srgb",
            "selection": "per_frame",
            "route_observation": "last_frame_not_exposed",
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
        "limitations": [
            "absolute_luminance_normalization_unavailable",
            "non_d65_chromatic_adaptation_unavailable",
            "surface_color_description_commit_latching_unavailable",
            "hdr_signalling_enable_unavailable_until_external_elements_adapted",
            "kms_external_elements_not_color_adapted",
            "kms_color_properties_and_framebuffer_not_atomic"
        ],
    })
}

fn output_color_policy_json(
    output: &crate::backend::api::OutputInfo,
    kms_color: Option<&serde_json::Value>,
    render_path_enabled: bool,
    advanced_enabled: bool,
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
    let runtime_params = srgb_params();
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
        "policy_source": "runtime_srgb_fail_closed",
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
        "selected_profile_semantics": "active_fail_closed_output_target",
        "delivery_route_observation": "last_frame_not_exposed",
    })
}

fn render_decisions_json(
    direct_scanout: Option<&serde_json::Value>,
    blur: Option<&serde_json::Value>,
    outputs: &[serde_json::Value],
    tearing_hint_count: usize,
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
    let hdr_decision = serde_json::json!({
        "configured": hdr_config_enabled,
        // The diagnostics backend currently exposes target allocation, but
        // not the actual per-frame output delivery route. Do not report a
        // configuration/capability conjunction as observed HDR scanout.
        "active": serde_json::Value::Null,
        "active_observation": "last_frame_not_exposed",
        "requested_on_capable_output": hdr_requested_on_capable_output,
        "reason": if hdr_requested_on_capable_output {
            "last_frame_not_exposed"
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
    let color_pipeline_decision = serde_json::json!({
        "configured": color_render_path_enabled,
        // Kept for schema compatibility. No last-frame route snapshot exists,
        // so a boolean here would conflate configured intent with runtime use.
        "active": serde_json::Value::Null,
        "active_observation": "last_frame_not_exposed",
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
        } else {
            "last_frame_route_not_exposed"
        },
    });

    serde_json::json!({
        "direct_scanout": direct_scanout_decision,
        "blur": blur_decision,
        "hdr": hdr_decision,
        "tearing": {
            "active": tearing_hint_count > 0,
            "hint_count": tearing_hint_count,
            "reason": if tearing_hint_count > 0 {
                "client_requested_tearing_control"
            } else {
                "no_active_tearing_hints"
            },
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
                        ipc.subscribe(client_id, topics);
                        ipc.respond(client_id, &IpcResponse::ok(None));
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
            let index = args
                .get("index")
                .and_then(|value| value.as_u64())
                .unwrap_or(0) as usize;
            let Some(text) = self
                .features
                .clipboard
                .get(index)
                .map(|entry| entry.text.clone())
            else {
                return IpcResponse::err(format!("clipboard_copy: no entry at index {index}"));
            };
            if !backend.set_clipboard_text(&text) {
                return IpcResponse::err("this backend cannot set the clipboard");
            }
            self.record_clipboard(&text);
            return IpcResponse::ok(None);
        }

        // Special command: clear_clipboard — forget everything copied.
        if name == "clear_clipboard" {
            let cleared = self.clear_clipboard_history();
            return IpcResponse::ok(Some(serde_json::json!({ "cleared": cleared })));
        }

        // Special command: set_power_profile — switch the platform profile.
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
            let devices = system_controls::audio_devices(direction);
            if !devices.iter().any(|device| device.id == id) {
                return IpcResponse::err(format!(
                    "set_audio_device: no {} device with id {id:?}",
                    direction.label()
                ));
            }
            if !system_controls::set_audio_device(direction, id) {
                return IpcResponse::err(format!(
                    "could not switch the audio {}",
                    direction.label()
                ));
            }
            // Re-read rather than trust the exit code: sound servers accept a
            // switch to an unavailable device and then quietly revert it.
            self.features.audio_defaults = system_controls::AudioDefaults::read();
            if !system_controls::audio_devices(direction)
                .iter()
                .any(|device| device.id == id && device.is_default)
            {
                return IpcResponse::err(format!(
                    "the sound server did not keep {id:?} as the {} device; it is likely unavailable",
                    direction.label()
                ));
            }
            self.refresh_open_control_center();
            self.broadcast_ipc_event("audio/devices", self.audio_devices_json());
            return IpcResponse::ok(None);
        }

        if name == "set_power_profile" {
            let Some(profile) = args.get("profile").and_then(|value| value.as_str()) else {
                return IpcResponse::err("set_power_profile: expected string field 'profile'");
            };
            let Some((available, _)) = crate::jwm::features::power::profiles() else {
                return IpcResponse::err("this machine has no power profile control");
            };
            if !available.iter().any(|name| name == profile) {
                return IpcResponse::err(format!(
                    "unknown power profile {profile:?} (have {})",
                    available.join(", ")
                ));
            }
            if !crate::jwm::features::power::set_profile(profile) {
                return IpcResponse::err(format!("could not switch to power profile {profile:?}"));
            }
            self.refresh_open_control_center();
            self.broadcast_ipc_event("power/profile", serde_json::json!({ "active": profile }));
            return IpcResponse::ok(None);
        }

        // Special command: media_control — drive the active player.
        if name == "media_control" {
            let Some(action) = args.get("action").and_then(|value| value.as_str()) else {
                return IpcResponse::err("media_control: expected string field 'action'");
            };
            let Some(command) = crate::jwm::features::MediaCommand::from_name(action) else {
                return IpcResponse::err(format!(
                    "media_control: unknown action {action:?} (play_pause, next, previous, stop)"
                ));
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
            return match self.start_audio_recording(std::path::Path::new(path)) {
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

        if name == "stop_audio_recording" {
            let was_active = self.features.audio_recording.active;
            let output_path = self.features.audio_recording.output_path.clone();
            return match self.stop_audio_recording() {
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
        _args: &serde_json::Value,
        backend: &dyn Backend,
    ) -> IpcResponse {
        let cfg = CONFIG.load();
        match name {
            "get_status" => IpcResponse::ok(Some(
                serde_json::to_value(self.query_runtime_status(backend)).unwrap_or_default(),
            )),
            "get_capabilities" => IpcResponse::ok(Some(
                serde_json::to_value(ipc::ipc_capabilities()).unwrap_or_default(),
            )),
            "get_windows" => {
                let windows = self.query_windows();
                IpcResponse::ok(Some(serde_json::to_value(windows).unwrap_or_default()))
            }
            "get_workspaces" => {
                let workspaces = self.query_workspaces();
                IpcResponse::ok(Some(serde_json::to_value(workspaces).unwrap_or_default()))
            }
            "get_monitors" => {
                let monitors = self.query_monitors();
                IpcResponse::ok(Some(serde_json::to_value(monitors).unwrap_or_default()))
            }
            "get_tree" => {
                let tree = self.query_tree();
                IpcResponse::ok(Some(serde_json::to_value(tree).unwrap_or_default()))
            }
            "get_scrolling_status" => IpcResponse::ok(Some(self.query_scrolling_status())),
            "get_gesture_status" => IpcResponse::ok(Some(self.query_gesture_status())),
            "get_wayland_status" => IpcResponse::ok(Some(self.query_wayland_status(backend))),
            "get_config_status" => IpcResponse::ok(Some(self.query_config_status())),
            "get_config" => IpcResponse::ok(Some(serde_json::json!({
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
            }))),
            "get_dnd" => IpcResponse::ok(Some(serde_json::json!({
                "enabled": self.do_not_disturb,
            }))),
            "get_notifications" => IpcResponse::ok(Some(self.notifications_json())),
            "get_media_status" => IpcResponse::ok(Some(self.media_status_json())),
            "get_power_status" => IpcResponse::ok(Some(self.power_status_json())),
            "get_connectivity" => IpcResponse::ok(Some(self.connectivity_json())),
            "get_audio_devices" => IpcResponse::ok(Some(self.audio_devices_json())),
            "get_wallpaper_colors" => IpcResponse::ok(Some(self.wallpaper_theme_json())),
            "get_idle_status" => IpcResponse::ok(Some(self.idle_status_json())),
            "get_resources" => IpcResponse::ok(Some(self.resources_json())),
            "get_clipboard" => IpcResponse::ok(Some(self.clipboard_json())),
            "get_recording_status" => {
                let output_path = self.features.recording.output_path.clone();
                let active = self.features.recording.active;
                // Validating the file forks ffprobe and blocks this thread on
                // it, and `stop` deliberately keeps `output_path` so the bar can
                // still report where the recording went — so an uncached probe
                // ran on every status poll for the rest of the session. Probe
                // only while the answer can still change: never during
                // recording, and never again once the file has passed.
                let finalized = !active
                    && (self.features.recording.finalized
                        || output_path.as_deref().is_some_and(recording_file_is_valid));
                self.features.recording.finalized = finalized;
                let should_broadcast = finalized && !self.features.recording.finalization_reported;
                if should_broadcast {
                    self.features.recording.finalization_reported = true;
                    self.broadcast_ipc_event(
                        "recording/finalized",
                        serde_json::json!({"output_path": output_path.clone()}),
                    );
                }
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
                    "output_size": backend
                        .compositor_recording_stats()
                        .map(|stats| stats.output_size)
                        .or(self.features.recording.output_size)
                        .map(|(width, height)| serde_json::json!({
                            "width": width,
                            "height": height,
                        })),
                    "selecting_region": self.features.recording.selecting_region,
                    "adjusting_region": self.features.recording.adjusting_region,
                    // What the recording is actually achieving. A recorder that
                    // silently runs at a third of the requested rate, or that is
                    // discarding frames because the encoder cannot keep up,
                    // looks healthy until the file is played back; these are the
                    // numbers that say otherwise while it is still running.
                    // `captured_fps` well under `fps` is normal on a static
                    // screen, where unchanged frames are deliberately skipped.
                    "capture": backend.compositor_recording_stats().map(|stats| serde_json::json!({
                        "captured_frames": stats.captured,
                        "dropped_frames": stats.dropped,
                        "captured_fps": (stats.effective_fps() * 10.0).round() / 10.0,
                        "elapsed_secs": (stats.elapsed_secs * 10.0).round() / 10.0,
                    })),
                })))
            }
            "get_audio_recording_status" => {
                self.features.audio_recording.refresh();
                let recording = &self.features.audio_recording;
                let output_exists = recording.output_path.as_deref().is_some_and(|path| {
                    std::fs::metadata(path).is_ok_and(|metadata| metadata.len() > 0)
                });
                IpcResponse::ok(Some(serde_json::json!({
                    "active": recording.active,
                    "output_path": recording.output_path,
                    "output_exists": output_exists,
                    "elapsed_ms": u64::try_from(recording.elapsed().as_millis()).unwrap_or(u64::MAX),
                    "device": recording.device,
                    "backend": recording.backend,
                    "format": recording.format,
                    "sample_rate": recording.sample_rate,
                    "channels": recording.channels,
                    "last_error": recording.last_error,
                })))
            }
            "get_effect_status" => IpcResponse::ok(Some(serde_json::json!({
                "overview": self.features.overview.active,
                "audio_recording": self.features.audio_recording.active,
                "magnifier": self.features.magnifier.enabled,
                "annotation": self.features.annotation_active,
                "peek": self.features.peek_active,
                "corner_radius": cfg.behavior().corner_radius,
                "shadow_enabled": cfg.behavior().shadow_enabled,
                "blur_enabled": cfg.behavior().blur_enabled,
                "fading": cfg.behavior().fading,
                "wobbly_windows": cfg.behavior().wobbly_windows,
                "motion_trail": cfg.behavior().motion_trail,
            }))),
            "get_hdr_status" => {
                let outputs: Vec<serde_json::Value> = backend
                    .output_ops()
                    .enumerate_outputs()
                    .into_iter()
                    .map(|o| {
                        let metadata = o.hdr_metadata.as_ref().map(|m| {
                            serde_json::json!({
                                "max_luminance_nits": m.max_luminance_nits,
                                "min_luminance_nits": m.min_luminance_nits,
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
                IpcResponse::ok(Some(serde_json::json!({
                    "config_enabled": cfg.behavior().hdr_enabled,
                    "config_peak_nits": cfg.behavior().hdr_peak_nits,
                    "outputs": outputs,
                })))
            }
            "get_tearing_hints" => IpcResponse::ok(Some(serde_json::json!({
                "active_surface_count": backend.compositor_tearing_hint_count(),
            }))),
            "get_session_lock" => IpcResponse::ok(Some(serde_json::json!({
                "locked": backend.compositor_session_locked(),
                "lock_surface_count": backend.compositor_session_lock_surface_count(),
            }))),
            "get_color_management_status" => {
                let surfaces = backend.compositor_color_managed_surfaces();
                let detail: Vec<serde_json::Value> =
                    surfaces.iter().map(color_managed_surface_json).collect();
                let summary = color_surface_summary_json(&surfaces);
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
                })))
            }
            "get_xwayland_status" => {
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
            "get_capture_status" => {
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
            "get_blur_status" => match backend.compositor_blur_status() {
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
                    })))
                }
                None => IpcResponse::err("compositor not active".to_string()),
            },
            "get_waterlily_status" => match backend.compositor_waterlily_status() {
                Some(status) => IpcResponse::ok(Some(serde_json::json!({
                    "enabled": status.enabled,
                    "active": status.active,
                    "worker_connected": status.worker_connected,
                    "frame_width": status.frame_width,
                    "frame_height": status.frame_height,
                    "frame_depth": status.frame_depth,
                    "frame_sequence": status.frame_sequence,
                }))),
                None => IpcResponse::err("compositor not active".to_string()),
            },
            "get_version" => IpcResponse::ok(Some(serde_json::json!({
                "version": env!("CARGO_PKG_VERSION"),
                "name": "jwm",
                "backend": self.runtime_backend.as_str(),
                "build_profile": if cfg!(debug_assertions) { "debug" } else { "release" },
            }))),
            "get_metrics" => {
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
            "benchmark_report" => {
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
                let color_policy = output_color_policy_json(
                    o,
                    kms_color.as_ref(),
                    color_render_path_enabled,
                    color_advanced_enabled,
                );
                let hdr_metadata = o.hdr_metadata.as_ref().map(|m| {
                    serde_json::json!({
                        "max_luminance_nits": m.max_luminance_nits,
                        "min_luminance_nits": m.min_luminance_nits,
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
        let color_session_policy = color_session_policy_json(
            &outputs,
            cfg.behavior().hdr_enabled,
            color_render_path_enabled,
            scene_linear_enabled,
            color_advanced_enabled,
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
            })
        });
        let tearing_hint_count = backend.compositor_tearing_hint_count();
        let render_decisions = render_decisions_json(
            direct_scanout.as_ref(),
            blur.as_ref(),
            &output_details,
            tearing_hint_count,
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
            "workspaces": self.query_workspaces(),
            "windows": self.query_windows(),
            "config": self.query_config_status(),
            "scrolling": self.query_scrolling_status(),
            "gestures": self.query_gesture_status(),
            "metrics": metrics,
            "direct_scanout": direct_scanout,
            "presentation_timing": presentation_timing,
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

    /// Apply a single in-memory config override (does not touch the file).
    /// args: { "key": "appearance.border_px", "value": <json> }
    /// args: { "output": "<name>", "enabled": true|false }
    /// Clears HDR_OUTPUT_METADATA. Enable requests fail closed until KMS-side
    /// external elements participate in the color pipeline.
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
        let enabled = args
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
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
        let windows = self.query_windows().len();
        let monitors = self.query_monitors().len();
        let workspaces = self.query_workspaces().len();
        let uptime_ms = u64::try_from(self.started_at.elapsed().as_millis()).unwrap_or(u64::MAX);

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
            health: runtime_health(&config, monitors),
            counts: RuntimeCounts {
                windows,
                monitors,
                workspaces,
            },
            config,
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
            },
            compositor_metrics: backend
                .compositor_get_metrics()
                .and_then(|metrics| serde_json::to_value(metrics).ok()),
        }
    }

    fn window_info(&self, client_key: ClientKey, is_focused: bool) -> Option<WindowInfo> {
        let client = self.state.clients.get(client_key)?;
        Some(client_window_info(
            client,
            resolved_client_monitor_num(&self.state.monitors, client),
            is_focused,
        ))
    }

    pub(crate) fn query_windows(&self) -> Vec<WindowInfo> {
        let sel_client = self.get_selected_client_key();
        self.state
            .client_order
            .iter()
            .filter_map(|&ck| self.window_info(ck, sel_client == Some(ck)))
            .collect()
    }

    pub(crate) fn query_workspaces(&self) -> Vec<WorkspaceInfo> {
        let cfg = CONFIG.load();
        let mut result = Vec::new();
        for &mk in &self.state.monitor_order {
            let mon = match self.state.monitors.get(mk) {
                Some(m) => m,
                None => continue,
            };
            let active_tags = mon.get_active_tags();
            for i in 0..cfg.tags_length() {
                let tag_bit = 1u32 << i;
                let is_active = (active_tags & tag_bit) != 0;
                let (layout, m_fact, n_master) = workspace_layout_state(mon, i);
                result.push(WorkspaceInfo {
                    tag_mask: tag_bit,
                    tag_index: i,
                    monitor: mon.num,
                    layout,
                    m_fact,
                    n_master,
                    num_clients: tagged_client_count(&self.state, mk, tag_bit),
                    focused: is_active && self.state.sel_mon == Some(mk),
                });
            }
        }
        result
    }

    pub(crate) fn query_monitors(&self) -> Vec<MonitorInfoIpc> {
        self.state
            .monitor_order
            .iter()
            .filter_map(|&mk| {
                let m = self.state.monitors.get(mk)?;
                Some(MonitorInfoIpc {
                    num: m.num,
                    x: m.geometry.m_x,
                    y: m.geometry.m_y,
                    w: m.geometry.m_w,
                    h: m.geometry.m_h,
                    active_tags: m.get_active_tags(),
                    layout: format!("{:?}", *m.lt),
                    focused: self.state.sel_mon == Some(mk),
                })
            })
            .collect()
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

    pub(crate) fn query_tree(&self) -> Vec<TreeNode> {
        self.state
            .monitor_order
            .iter()
            .filter_map(|&mk| {
                let m = self.state.monitors.get(mk)?;
                let sel_client = m.sel;
                let windows: Vec<WindowInfo> = self
                    .state
                    .monitor_clients
                    .get(mk)
                    .map(|clients| {
                        clients
                            .iter()
                            .filter_map(|&ck| self.window_info(ck, sel_client == Some(ck)))
                            .collect()
                    })
                    .unwrap_or_default();
                Some(TreeNode {
                    monitor: MonitorInfoIpc {
                        num: m.num,
                        x: m.geometry.m_x,
                        y: m.geometry.m_y,
                        w: m.geometry.m_w,
                        h: m.geometry.m_h,
                        active_tags: m.get_active_tags(),
                        layout: format!("{:?}", *m.lt),
                        focused: self.state.sel_mon == Some(mk),
                    },
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
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_COMMAND_BATCH_ENTRIES, MAX_CONFIG_BATCH_CHANGES, client_window_info,
        color_managed_surface_json, color_session_policy_json, color_surface_summary_json,
        optional_protocol_enabled_from_flags, output_color_policy_json, parse_benchmark_request,
        parse_command_batch_entries, parse_config_batch_changes, parse_optional_u32_ipc_arg,
        parse_required_i32_ipc_arg, render_decisions_json, resolved_client_monitor_num,
        runtime_health, tagged_client_count, workspace_layout_state,
    };
    use crate::application::BenchmarkRequest;
    use crate::backend::api::{ColorManagedSurfaceInfo, OutputIdentity, OutputInfo};
    use crate::backend::common_define::{OutputId, WindowId};
    use crate::backend::edid::EdidHdrCapabilities;
    use crate::core::layout::LayoutEnum;
    use crate::core::models::{Pertag, WMClient, WMMonitor};
    use crate::core::state::WMState;
    use crate::ipc::RuntimeHealthStatus;
    use std::rc::Rc;

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
            hdr_capable: hdr_metadata.is_some(),
            hdr_metadata,
            identity: OutputIdentity::connector_only("HDMI-A-1"),
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
            runtime_health(&healthy_config, 1).status,
            RuntimeHealthStatus::Healthy
        );

        let failed_reload = serde_json::json!({
            "exists": true,
            "diagnostics": {"error_count": 0, "warning_count": 1},
            "reload": {"last_success": false, "last_error": "bad TOML"}
        });
        let health = runtime_health(&failed_reload, 0);
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
    fn window_query_projection_distinguishes_minimized_from_focus() {
        let mut client = WMClient::new(WindowId::from_raw(0x2a));
        client.state.is_hidden = true;
        client.geometry.x = -2560;
        client.geometry.y = 40;
        client.geometry.w = 640;
        client.geometry.h = 480;

        let info = client_window_info(&client, 7, false);
        assert_eq!(info.id, 0x2a);
        assert_eq!(info.monitor, 7);
        assert!(info.is_minimized);
        assert!(!info.is_focused);

        client.state.is_hidden = false;
        let restored = client_window_info(&client, 7, true);
        assert!(!restored.is_minimized);
        assert!(restored.is_focused);
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

        let mut pertag = Pertag::new(true, 2);
        pertag.m_facts[0] = 0.11;
        pertag.n_masters[0] = 9;
        pertag.m_facts[1] = 0.61;
        pertag.n_masters[1] = 2;
        pertag.lts[1] = Rc::new(LayoutEnum::MONOCLE);
        pertag.m_facts[2] = 0.72;
        pertag.n_masters[2] = 3;
        pertag.lts[2] = Rc::new(LayoutEnum::GRID);
        monitor.pertag = Some(pertag);

        assert_eq!(
            workspace_layout_state(&monitor, 0),
            (format!("{:?}", LayoutEnum::MONOCLE), 0.61, 2)
        );
        assert_eq!(
            workspace_layout_state(&monitor, 1),
            (format!("{:?}", LayoutEnum::GRID), 0.72, 3)
        );
    }

    #[test]
    fn workspace_layout_state_falls_back_without_safe_pertag_data() {
        let mut monitor = WMMonitor::new();
        monitor.lt = Rc::new(LayoutEnum::TILE);
        monitor.layout.m_fact = 0.66;
        monitor.layout.n_master = 4;
        let current = (format!("{:?}", LayoutEnum::TILE), 0.66, 4);

        assert_eq!(workspace_layout_state(&monitor, 0), current);

        monitor.pertag = Some(Pertag::new(true, 0));
        assert_eq!(workspace_layout_state(&monitor, 1), current);
        assert_eq!(workspace_layout_state(&monitor, usize::MAX), current);
    }

    #[test]
    fn color_policy_uses_safe_srgb_when_advanced_disabled() {
        let value = output_color_policy_json(
            &output(Some(EdidHdrCapabilities {
                max_luminance_nits: 1000.0,
                min_luminance_nits: 0.05,
                supports_bt2020: true,
                supports_pq: true,
                supports_hlg: false,
            })),
            None,
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
                supports_bt2020: true,
                supports_pq: true,
                supports_hlg: false,
            })),
            None,
            true,
            true,
        );

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
            "last_frame_not_exposed"
        );
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
            supports_bt2020: true,
            supports_pq: true,
            supports_hlg: false,
        }));
        let mut sdr = output(None);
        sdr.id = OutputId(2);
        sdr.name = "DP-1".into();
        sdr.identity = OutputIdentity::connector_only("DP-1");

        let full = color_session_policy_json(&[hdr.clone(), sdr.clone()], true, true, true, true);
        assert_eq!(full["mixed_hdr_outputs"], true);
        assert_eq!(full["heterogeneous_output_profiles"], true);
        assert_eq!(
            full["sdr_on_hdr_policy"],
            "normalized_transfer_and_gamut_without_absolute_luminance_mapping"
        );
        assert_eq!(
            full["hdr_active_semantics"],
            "physical_output_signal_fail_closed_sdr"
        );
        assert_eq!(full["hdr_active"], false);
        assert_eq!(
            full["mixed_hdr_policy"],
            "per_output_delivery_infrastructure_sdr_signalling_fail_closed"
        );
        assert_eq!(
            full["blockers"],
            serde_json::json!([
                "hdr_signalling_enable_unavailable_until_external_elements_adapted"
            ])
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
            "last_frame_not_exposed"
        );
        assert_eq!(
            full["fallback_policy"]["normal_desktop_cursor_effect"],
            "usually_selects_global_srgb_fallback"
        );
        assert_eq!(
            full["limitations"],
            serde_json::json!([
                "absolute_luminance_normalization_unavailable",
                "non_d65_chromatic_adaptation_unavailable",
                "surface_color_description_commit_latching_unavailable",
                "hdr_signalling_enable_unavailable_until_external_elements_adapted",
                "kms_external_elements_not_color_adapted",
                "kms_color_properties_and_framebuffer_not_atomic"
            ])
        );

        let legacy = color_session_policy_json(&[hdr, sdr], true, false, false, false);
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
                "scene_linear_compositing_inactive",
                "hdr_signalling_enable_unavailable_until_external_elements_adapted"
            ])
        );

        let configured_without_render_path = color_session_policy_json(
            &[output(None), output(None)],
            false,
            false,
            crate::config::scene_linear_render_path_requested(false, true),
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
            supports_bt2020: true,
            supports_pq: true,
            supports_hlg: false,
        }));
        let mut hlg = output(Some(EdidHdrCapabilities {
            max_luminance_nits: 1000.0,
            min_luminance_nits: 0.05,
            supports_bt2020: true,
            supports_pq: false,
            supports_hlg: true,
        }));
        hlg.id = OutputId(3);
        hlg.name = "HDMI-A-2".into();
        hlg.identity = OutputIdentity::connector_only("HDMI-A-2");
        let pq_hlg = color_session_policy_json(&[pq, hlg], true, true, false, true);
        assert_eq!(pq_hlg["mixed_hdr_outputs"], false);
        assert_eq!(pq_hlg["heterogeneous_output_profiles"], true);
        assert_eq!(
            pq_hlg["mixed_hdr_policy"],
            "per_surface_transform_without_scene_linear_blending"
        );
        assert_eq!(
            pq_hlg["blockers"],
            serde_json::json!([
                "scene_linear_compositing_inactive",
                "hdr_signalling_enable_unavailable_until_external_elements_adapted"
            ])
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
            0,
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
            1,
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
            "last_frame_not_exposed"
        );
        assert_eq!(decisions["hdr"]["requested_on_capable_output"], false);
        assert_eq!(decisions["hdr"]["reason"], "no_hdr_capable_outputs");
        assert_eq!(decisions["tearing"]["active"], true);
        assert_eq!(
            decisions["color_pipeline"]["active"],
            serde_json::Value::Null
        );
        assert_eq!(
            decisions["color_pipeline"]["active_observation"],
            "last_frame_not_exposed"
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
}
