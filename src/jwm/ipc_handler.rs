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
                })))
            }
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
