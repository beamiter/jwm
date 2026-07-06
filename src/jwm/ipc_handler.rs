// IPC handling: command processing, queries, and event broadcasting

use crate::Jwm;
use crate::backend::api::Backend;
use crate::config::{ArgumentConfig, BackendFamily, CONFIG, get_backend_family};
use crate::core::layout::LayoutEnum;
use crate::ipc::{
    self, IpcEvent, IpcResponse, MonitorInfoIpc, TreeNode, WindowInfo, WorkspaceInfo,
};
use crate::ipc_server::IncomingIpc;

fn env_flag(name: &str) -> bool {
    std::env::var_os(name).as_deref() == Some(std::ffi::OsStr::new("1"))
}

fn optional_protocol_enabled(default_enabled: bool, flag_name: &str) -> bool {
    default_enabled || env_flag("JWM_OPTIONAL_GLOBALS") || env_flag(flag_name)
}

fn wayland_protocol_status() -> serde_json::Value {
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
        ("zwlr_screencopy_manager_v1", true, "JWM_ENABLE_SCREENCOPY"),
        (
            "wp_tearing_control_manager_v1",
            true,
            "JWM_ENABLE_TEARING_CONTROL",
        ),
        ("wp_color_manager_v1", false, "JWM_ENABLE_COLOR_MANAGEMENT"),
        (
            "zwlr_output_manager_v1",
            true,
            "JWM_ENABLE_OUTPUT_MANAGEMENT",
        ),
        (
            "zwlr_output_power_manager_v1",
            true,
            "JWM_ENABLE_OUTPUT_POWER",
        ),
        ("ext_workspace_manager_v1", true, "JWM_ENABLE_WORKSPACE"),
        (
            "ext_image_copy_capture_manager_v1",
            true,
            "JWM_ENABLE_IMAGE_COPY_CAPTURE",
        ),
        (
            "zwlr_gamma_control_manager_v1",
            true,
            "JWM_ENABLE_GAMMA_CONTROL",
        ),
        (
            "zwlr_foreign_toplevel_manager_v1",
            true,
            "JWM_ENABLE_FOREIGN_TOPLEVEL_MANAGEMENT",
        ),
        (
            "zwlr_virtual_pointer_manager_v1",
            true,
            "JWM_ENABLE_VIRTUAL_POINTER",
        ),
    ];

    serde_json::json!({
        "core": core
            .iter()
            .map(|name| serde_json::json!({ "name": name, "enabled": true }))
            .collect::<Vec<_>>(),
        "optional": optional
            .iter()
            .map(|(name, default_enabled, flag)| {
                serde_json::json!({
                    "name": name,
                    "enabled": optional_protocol_enabled(*default_enabled, flag),
                    "default_enabled": default_enabled,
                    "env_flag": flag,
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

        // Special command: benchmark (requires mutable backend)
        if name == "benchmark" {
            return self.handle_benchmark_command(backend, args);
        }

        if name == "set_config" {
            return self.handle_set_config_command(backend, args);
        }

        if name == "move_window_to_monitor" {
            return self.handle_move_window_to_monitor(backend, args);
        }

        if name == "set_hdr_metadata" {
            return self.handle_set_hdr_metadata_command(backend, args);
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
        &self,
        name: &str,
        _args: &serde_json::Value,
        backend: &dyn Backend,
    ) -> IpcResponse {
        let cfg = CONFIG.load();
        match name {
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
            "get_config" => IpcResponse::ok(Some(serde_json::json!({
                "border_px": cfg.border_px(),
                "gap_px": cfg.gap_px(),
                "snap": cfg.snap(),
                "m_fact": cfg.m_fact(),
                "n_master": cfg.n_master(),
                "tags_length": cfg.tags_length(),
                "show_bar": cfg.show_bar(),
                "do_not_disturb": self.do_not_disturb,
            }))),
            "get_dnd" => IpcResponse::ok(Some(serde_json::json!({
                "enabled": self.do_not_disturb,
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
                let detail: Vec<serde_json::Value> = surfaces
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "surface_object_id": s.surface_object_id,
                            "identity": s.identity,
                            "tf_named": s.tf_named,
                            "tf_power": s.tf_power,
                            "primaries_named": s.primaries_named,
                            "min_lum": s.min_lum,
                            "max_lum": s.max_lum,
                            "reference_lum": s.reference_lum,
                            "mastering_min_lum": s.mastering_min_lum,
                            "mastering_max_lum": s.mastering_max_lum,
                            "max_cll": s.max_cll,
                            "max_fall": s.max_fall,
                        })
                    })
                    .collect();
                IpcResponse::ok(Some(serde_json::json!({
                    "surface_count": surfaces.len(),
                    "surfaces": detail,
                })))
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
            "get_version" => IpcResponse::ok(Some(serde_json::json!({
                "version": env!("CARGO_PKG_VERSION"),
                "name": "jwm",
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
        let protocol_bind_counts = backend
            .compositor_protocol_bind_counts()
            .into_iter()
            .map(|(protocol, count)| {
                serde_json::json!({
                    "protocol": protocol,
                    "bind_count": count,
                })
            })
            .collect::<Vec<_>>();

        let color_surfaces = backend.compositor_color_managed_surfaces();
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
                if let Some(obj) = protocols.as_object_mut() {
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
            "scrolling": self.query_scrolling_status(),
            "gestures": self.query_gesture_status(),
            "metrics": metrics,
            "direct_scanout": direct_scanout,
            "presentation_timing": presentation_timing,
            "output_management": output_management,
            "hdr": {
                "config_enabled": cfg.behavior().hdr_enabled,
                "config_peak_nits": cfg.behavior().hdr_peak_nits,
                "capable_output_count": outputs.iter().filter(|o| o.hdr_capable).count(),
            },
            "tearing": {
                "active_surface_count": backend.compositor_tearing_hint_count(),
            },
            "session_lock": {
                "locked": backend.compositor_session_locked(),
                "lock_surface_count": backend.compositor_session_lock_surface_count(),
            },
            "color_management": {
                "surface_count": color_surfaces.len(),
            },
            "blur": blur,
            "not_yet_exposed": [],
        })
    }

    /// Apply a single in-memory config override (does not touch the file).
    /// args: { "key": "appearance.border_px", "value": <json> }
    /// args: { "output": "<name>", "enabled": true|false }
    /// Pushes (or clears) the HDR_OUTPUT_METADATA blob on a KMS connector.
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
        let target_num = match args.get("monitor").and_then(|v| v.as_i64()) {
            Some(v) => v as i32,
            None => {
                return IpcResponse::err(
                    "move_window_to_monitor: missing 'monitor' (i32)".to_string(),
                );
            }
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
                let frames = args.get("frames").and_then(|v| v.as_u64()).unwrap_or(600) as u32;
                let warmup = args.get("warmup").and_then(|v| v.as_u64()).unwrap_or(60) as u32;
                if backend.compositor_benchmark_start(frames, warmup) {
                    IpcResponse::ok(Some(
                        serde_json::json!({"status": "started", "frames": frames, "warmup": warmup}),
                    ))
                } else {
                    IpcResponse::err("compositor not available".to_string())
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

    pub(crate) fn query_windows(&self) -> Vec<WindowInfo> {
        let sel_client = self.get_selected_client_key();
        self.state
            .client_order
            .iter()
            .filter_map(|&ck| {
                let c = self.state.clients.get(ck)?;
                Some(WindowInfo {
                    id: c.win.raw(),
                    name: c.name.clone(),
                    class: c.class.clone(),
                    instance: c.instance.clone(),
                    tags: c.state.tags,
                    monitor: c.monitor_num as i32,
                    x: c.geometry.x,
                    y: c.geometry.y,
                    w: c.geometry.w,
                    h: c.geometry.h,
                    is_floating: c.state.is_floating,
                    is_fullscreen: c.state.is_fullscreen,
                    is_urgent: c.state.is_urgent,
                    is_sticky: c.state.is_sticky,
                    is_focused: sel_client == Some(ck),
                })
            })
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
            let client_count = self
                .state
                .monitor_clients
                .get(mk)
                .map(|v| v.len())
                .unwrap_or(0);
            for i in 0..cfg.tags_length() {
                let tag_bit = 1u32 << i;
                let is_active = (active_tags & tag_bit) != 0;
                result.push(WorkspaceInfo {
                    tag_mask: tag_bit,
                    tag_index: i,
                    monitor: mon.num,
                    layout: format!("{:?}", *mon.lt[mon.sel_lt]),
                    m_fact: mon.layout.m_fact,
                    n_master: mon.layout.n_master,
                    num_clients: if is_active { client_count } else { 0 },
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
                    layout: format!("{:?}", *m.lt[m.sel_lt]),
                    focused: self.state.sel_mon == Some(mk),
                })
            })
            .collect()
    }

    pub(crate) fn query_scrolling_status(&self) -> serde_json::Value {
        let monitors = self
            .state
            .monitor_order
            .iter()
            .filter_map(|&mk| {
                let mon = self.state.monitors.get(mk)?;
                let layout = &*mon.lt[mon.sel_lt];
                let active_tags = mon.get_active_tags();
                let state_key = self.scrolling_state_key(mk);
                let state = self.scrolling_state_for_monitor(mk);
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

        serde_json::json!({
            "active_monitor_count": active_monitor_count,
            "stored_state_count": self.scrolling_states.len(),
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
                            .filter_map(|&ck| {
                                let c = self.state.clients.get(ck)?;
                                Some(WindowInfo {
                                    id: c.win.raw(),
                                    name: c.name.clone(),
                                    class: c.class.clone(),
                                    instance: c.instance.clone(),
                                    tags: c.state.tags,
                                    monitor: c.monitor_num as i32,
                                    x: c.geometry.x,
                                    y: c.geometry.y,
                                    w: c.geometry.w,
                                    h: c.geometry.h,
                                    is_floating: c.state.is_floating,
                                    is_fullscreen: c.state.is_fullscreen,
                                    is_urgent: c.state.is_urgent,
                                    is_sticky: c.state.is_sticky,
                                    is_focused: sel_client == Some(ck),
                                })
                            })
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
                        layout: format!("{:?}", *m.lt[m.sel_lt]),
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
