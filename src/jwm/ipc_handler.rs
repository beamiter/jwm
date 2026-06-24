// IPC handling: command processing, queries, and event broadcasting

use crate::backend::api::Backend;
use crate::config::CONFIG;
use crate::ipc::{self, IpcEvent, IpcResponse, MonitorInfoIpc, TreeNode, WindowInfo, WorkspaceInfo};
use crate::ipc_server::IncomingIpc;
use crate::Jwm;

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
            "get_config" => {
                IpcResponse::ok(Some(serde_json::json!({
                    "border_px": cfg.border_px(),
                    "gap_px": cfg.gap_px(),
                    "snap": cfg.snap(),
                    "m_fact": cfg.m_fact(),
                    "n_master": cfg.n_master(),
                    "tags_length": cfg.tags_length(),
                    "show_bar": cfg.show_bar(),
                    "do_not_disturb": self.do_not_disturb,
                })))
            }
            "get_dnd" => IpcResponse::ok(Some(serde_json::json!({
                "enabled": self.do_not_disturb,
            }))),
            "get_hdr_status" => {
                let outputs: Vec<serde_json::Value> = backend
                    .output_ops()
                    .enumerate_outputs()
                    .into_iter()
                    .map(|o| {
                        let metadata = o.hdr_metadata.as_ref().map(|m| serde_json::json!({
                            "max_luminance_nits": m.max_luminance_nits,
                            "min_luminance_nits": m.min_luminance_nits,
                            "supports_pq": m.supports_pq,
                            "supports_hlg": m.supports_hlg,
                            "supports_bt2020": m.supports_bt2020,
                        }));
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

        let target_mon_key = self
            .state
            .monitor_order
            .iter()
            .copied()
            .find(|&mk| {
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
                    IpcResponse::ok(Some(serde_json::json!({"status": "started", "frames": frames, "warmup": warmup})))
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
