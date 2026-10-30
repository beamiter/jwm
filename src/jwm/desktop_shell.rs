//! One shared sidebar host per compositor session. Its lifetime, IPC snapshot
//! and role registry do not depend on the selected top-bar frontend.
use std::collections::{HashMap, HashSet};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use slotmap::Key as _;
use xbar_core::desktop_shell::{SHELL_PROTOCOL_VERSION, ShellOutput, ShellSnapshot, ShellTask};
use xbar_core::{BarEvent, BarModel, ModelConfig, MonitorGeometry, ThemeMode, WmCommand};

use super::{Jwm, wm_session_id};
use crate::backend::api::Backend;
use crate::backend::compositor_common::ui_theme::UiTheme;
use crate::config::CONFIG;
use crate::ipc::IpcResponse;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(10);
const HEALTH_INTERVAL: Duration = Duration::from_secs(1);
const STATE_INTERVAL: Duration = Duration::from_millis(100);
const MAX_PANELS: usize = 128 * 3;

/// Managed edges use the same light/dark material as the compositor's menus.
/// Parse through UiTheme so aliases and unknown-value fallback stay identical.
fn desktop_shell_theme(name: &str) -> ThemeMode {
    match UiTheme::from_config(name) {
        UiTheme::Glass | UiTheme::Paper => ThemeMode::Light,
        _ => ThemeMode::Dark,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ShellPanel {
    pub output: i32,
    pub output_identity: u64,
    pub role: String,
    pub window: u64,
}

#[derive(Default)]
pub(crate) struct DesktopShellSupervisor {
    child: Option<Child>,
    started: Option<Instant>,
    heartbeat: Option<Instant>,
    next_health: Option<Instant>,
    retry_at: Option<Instant>,
    failures: u32,
    panels: Vec<ShellPanel>,
    ready: bool,
    last_error: Option<String>,
    revision: u64,
    last_published_revision: u64,
    last_state: Option<Value>,
    next_state: Option<Instant>,
    next_legacy_scan: Option<Instant>,
    last_observer_tick: Option<Instant>,
    output_identities: HashMap<u64, (Option<u64>, u64)>,
    next_output_identity: u64,
}

fn bounded_text(value: &str) -> String {
    let mut end = value.len().min(4096);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn retry_delay(failures: u32) -> Duration {
    Duration::from_secs(1u64 << failures.saturating_sub(1).min(6)).min(Duration::from_secs(60))
}

fn validate_panels(panels: &[ShellPanel], outputs: &HashSet<i32>) -> Result<(), &'static str> {
    if panels.len() > MAX_PANELS {
        return Err("too many shell panels");
    }
    let mut roles = HashSet::new();
    let mut windows = HashSet::new();
    for panel in panels {
        if !matches!(panel.role.as_str(), "left" | "right" | "bottom") {
            return Err("shell role must be left, right or bottom");
        }
        if panel.output_identity == 0 {
            return Err("missing shell output identity");
        }
        if !outputs.contains(&panel.output) {
            return Err("shell panel names a disconnected output");
        }
        if panel.window == 0 || !windows.insert(panel.window) {
            return Err("invalid or duplicate shell window");
        }
        if !roles.insert((panel.output, panel.role.as_str())) {
            return Err("duplicate shell role on output");
        }
    }
    Ok(())
}

impl Drop for DesktopShellSupervisor {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = super::monitor_management::terminate_secondary_bar_child(
                child,
                Duration::from_millis(250),
            );
        }
    }
}

impl DesktopShellSupervisor {
    pub(crate) fn reserves_space(&self) -> bool {
        // The health tick owns timeout transitions. Computing expiry here
        // would release the flag before that tick records its old value,
        // skipping the arrange needed to withdraw the prepared work area.
        self.child.is_some() && (self.ready || self.started.is_some())
    }

    fn identify_output(&mut self, monitor: u64, output: Option<u64>) -> u64 {
        if let Some(&(existing, identity)) = self.output_identities.get(&monitor) {
            if existing == output {
                return identity;
            }
        }
        self.next_output_identity = self
            .next_output_identity
            .checked_add(1)
            .expect("desktop shell output identity exhausted");
        self.output_identities
            .insert(monitor, (output, self.next_output_identity));
        self.next_output_identity
    }

    fn note_observer_tick(&mut self, now: Instant) {
        let Some(previous) = self.last_observer_tick.replace(now) else {
            return;
        };
        let gap = now.saturating_duration_since(previous);
        if gap <= HEALTH_INTERVAL * 2 {
            return;
        }
        // The WM could not read registrations while its own renderer/readback
        // was blocked. Do not charge that unobserved interval to a healthy
        // host. Exit detection still uses the exact Child handle below.
        for clock in [&mut self.started, &mut self.heartbeat] {
            if let Some(observed) = *clock {
                *clock = Some(observed.checked_add(gap).unwrap_or(now));
            }
        }
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = super::monitor_management::terminate_secondary_bar_child(
                &mut child,
                Duration::from_millis(250),
            );
        }
        self.ready = false;
        self.panels.clear();
        self.heartbeat = None;
        self.started = None;
        self.next_health = None;
    }

    fn failed(&mut self, now: Instant, reason: String) {
        self.stop();
        self.failures = self.failures.saturating_add(1);
        self.retry_at = Some(now + retry_delay(self.failures));
        self.last_error = Some(reason.clone());
        log::warn!(
            "shared desktop shell: {reason}; retry in {:?}",
            retry_delay(self.failures)
        );
    }
}

impl Jwm {
    /// Toggle companion exposure without changing workspace bar visibility or
    /// replacing the supervised process. The persisted edit is committed
    /// before the normal configuration reconciliation sees the new modes.
    pub fn toggle_desktop_shell_mode(
        &mut self,
        backend: &mut dyn Backend,
        _argument: &super::types::WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let current = CONFIG.load_full();
        let effective = current.desktop_shell_config();
        let desired = effective.toggled_mode();
        if desired == effective {
            return Ok(());
        }
        let value = serde_json::to_value(&desired)?;
        let updated = current
            .with_validated_value("desktop_shell", &value)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        self.observe_config_reload(Instant::now(), "desktop shell mode pre-save check");
        let revision = updated.persist_desktop_shell_mode(&desired)?;
        self.note_config_written_by_us(revision);
        CONFIG.store(std::sync::Arc::new(updated));
        self.apply_config_changes(backend);
        self.broadcast_ipc_event(
            "config/changed",
            json!({"key":"desktop_shell","value":value,"persisted":true}),
        );
        Ok(())
    }

    pub(crate) fn stop_desktop_shell(&mut self) {
        self.desktop_shell.stop();
    }

    pub(crate) fn desktop_shell_next_wakeup(&self, now: Instant) -> Option<Duration> {
        let config = CONFIG.load();
        let host = if !config.desktop_shell_enabled() {
            self.desktop_shell.child.as_ref().map(|_| Duration::ZERO)
        } else {
            match (&self.desktop_shell.child, self.desktop_shell.retry_at) {
                (None, Some(at)) => Some(at.saturating_duration_since(now)),
                (None, None) => Some(Duration::ZERO),
                (Some(_), _) => self
                    .desktop_shell
                    .next_health
                    .map(|at| at.saturating_duration_since(now)),
            }
        };
        let legacy = matches!(
            config.desktop_shell_source(),
            xbar_core::desktop_shell::DesktopShellSource::LegacyEdgeShell
        )
        .then(|| {
            self.desktop_shell
                .next_legacy_scan
                .map_or(Duration::ZERO, |at| at.saturating_duration_since(now))
        });
        match (host, legacy) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (left, right) => left.or(right),
        }
    }

    pub(crate) fn ensure_desktop_shell_running(&mut self, backend: &mut dyn Backend, now: Instant) {
        let before = self.desktop_shell.reserves_space();
        self.desktop_shell.note_observer_tick(now);
        if CONFIG.load().desktop_shell_source()
            == xbar_core::desktop_shell::DesktopShellSource::LegacyEdgeShell
            && self
                .desktop_shell
                .next_legacy_scan
                .is_none_or(|at| at <= now)
        {
            self.desktop_shell.next_legacy_scan = Some(now + HEALTH_INTERVAL);
            if let Some(refreshed) = CONFIG.load().refreshed_legacy_desktop_shell() {
                CONFIG.store(std::sync::Arc::new(refreshed));
                self.reconcile_secondary_bar_configuration(backend);
                for monitor in self.state.monitor_order.clone() {
                    self.arrange(backend, Some(monitor));
                }
            }
        }
        if !CONFIG.load().desktop_shell_enabled() {
            self.desktop_shell.stop();
            self.desktop_shell.retry_at = None;
            self.desktop_shell.failures = 0;
        } else if self.desktop_shell.child.is_some() {
            if self.desktop_shell.next_health.is_none_or(|at| at <= now) {
                self.desktop_shell.next_health = Some(now + HEALTH_INTERVAL);
                let failure = match self.desktop_shell.child.as_mut().unwrap().try_wait() {
                    Ok(Some(status)) => Some(format!("host exited: {status}")),
                    Err(error) => Some(format!("host wait failed: {error}")),
                    Ok(None)
                        if !self.desktop_shell.ready
                            && self.desktop_shell.started.is_some_and(|start| {
                                now.duration_since(start) >= HANDSHAKE_TIMEOUT
                            }) =>
                    {
                        Some("host handshake timed out".into())
                    }
                    Ok(None)
                        if self.desktop_shell.ready
                            && self.desktop_shell.heartbeat.is_some_and(|last| {
                                now.duration_since(last) >= HEARTBEAT_TIMEOUT
                            }) =>
                    {
                        Some("host heartbeat timed out".into())
                    }
                    Ok(None) => None,
                };
                if let Some(reason) = failure {
                    self.desktop_shell.failed(now, reason);
                }
            }
        } else if self.desktop_shell.retry_at.is_none_or(|at| at <= now) {
            let executable =
                std::env::var_os("JWM_SHELL_BIN").unwrap_or_else(|| "jwm-shell".into());
            let mut command = Command::new(executable);
            crate::child_environment::current_or_default().apply(&mut command);
            command
                .env("JWM_DESKTOP_SHELL_BACKEND", &self.runtime_backend)
                .env("JWM_DESKTOP_SHELL_SESSION", wm_session_id().to_string())
                .env(
                    "JWM_DESKTOP_SHELL_VERSION",
                    SHELL_PROTOCOL_VERSION.to_string(),
                )
                .stdin(Stdio::null())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
            crate::external_command::unblock_sigchld_in_child(&mut command);
            match command.spawn() {
                Ok(child) => {
                    log::info!("Started shared desktop shell PID {}", child.id());
                    self.desktop_shell.child = Some(child);
                    self.desktop_shell.started = Some(now);
                    self.desktop_shell.next_health = Some(now + HEALTH_INTERVAL);
                    self.desktop_shell.retry_at = None;
                }
                Err(error) => self
                    .desktop_shell
                    .failed(now, format!("host spawn failed: {error}")),
            }
        }
        if before && !self.desktop_shell.reserves_space() {
            let monitors = self
                .state
                .monitors
                .values()
                .map(|monitor| monitor.num)
                .collect::<Vec<_>>();
            for monitor in monitors {
                self.clear_minimized_preview_for(backend, monitor, None);
                self.minimized_dock_shelves.remove(&monitor);
                self.pending_bar_updates.insert(monitor);
            }
            for client in self
                .state
                .clients
                .values()
                .filter(|client| client.state.is_hidden)
            {
                backend.compositor_set_window_dock_geometry(client.win, None);
            }
        }
        if before != self.desktop_shell.reserves_space() {
            let monitors = self.state.monitor_order.clone();
            for monitor in monitors {
                self.arrange(backend, Some(monitor));
            }
        }
    }

    fn synchronize_desktop_shell_output_identities(&mut self) {
        let live: HashSet<_> = self
            .state
            .monitor_order
            .iter()
            .map(|key| key.data().as_ffi())
            .collect();
        self.desktop_shell
            .output_identities
            .retain(|monitor, _| live.contains(monitor));
        for &key in &self.state.monitor_order {
            self.desktop_shell.identify_output(
                key.data().as_ffi(),
                self.state.output_map.get(key).map(|output| output.0),
            );
        }
    }

    fn desktop_shell_output_identity(&self, key: crate::core::models::MonitorKey) -> u64 {
        self.desktop_shell.output_identities[&key.data().as_ffi()].1
    }

    fn desktop_shell_snapshot(&mut self, backend: &dyn Backend) -> ShellSnapshot {
        self.synchronize_desktop_shell_output_identities();
        let cfg = CONFIG.load();
        let mut outputs = Vec::new();
        for key in self.state.monitor_order.clone().into_iter().take(128) {
            let Some(monitor) = self.state.monitors.get(key) else {
                continue;
            };
            let id = monitor.num;
            let geometry = MonitorGeometry {
                x: monitor.geometry.m_x,
                y: monitor.geometry.m_y,
                width: monitor.geometry.m_w.max(1) as u32,
                height: monitor.geometry.m_h.max(1) as u32,
            };
            let visible = cfg.desktop_shell_enabled()
                && self.monitor_shows_status_bar(key)
                && !self.monitor_key_is_locked(key);
            let (scale, _) = self.output_scale_refresh_for_monitor(backend, key);
            // Generate a fresh WM-owned message without consuming any top-bar
            // ring. Restore the publication scratch slot before returning.
            let saved = self.message.clone();
            self.update_bar_message_for_monitor(Some(key));
            let wm = xbar_core::transport::snapshot_from_shared(self.message.clone());
            self.message = saved;
            let mut model = BarModel::new(ModelConfig {
                tag_count: cfg.tags_length(),
                resolve_client_icons: false,
                initial_theme: desktop_shell_theme(cfg.ui_theme()),
                ..ModelConfig::default()
            })
            .expect("validated bar model configuration");
            let _ = model.update(BarEvent::WindowManager(wm));
            if let Some(media) = self.features.media.get() {
                let _ = model.update(BarEvent::Media(xbar_core::model::MediaState {
                    playback: match media.status {
                        super::features::PlaybackStatus::Playing => {
                            xbar_core::model::MediaPlayback::Playing
                        }
                        super::features::PlaybackStatus::Paused => {
                            xbar_core::model::MediaPlayback::Paused
                        }
                        super::features::PlaybackStatus::Stopped => {
                            xbar_core::model::MediaPlayback::Stopped
                        }
                    },
                    title: Some(bounded_text(&media.title)),
                    artist: Some(bounded_text(&media.artist)),
                    player: Some(bounded_text(&media.player)),
                }));
            }
            let tasks = self
                .state
                .client_order
                .iter()
                .filter_map(|&client_key| {
                    let client = self.state.clients.get(client_key)?;
                    if client.mon != Some(key)
                        || client.state.is_dock
                        || client.state.is_desktop
                        || client.state.is_swallowed
                        || client.state.skip_taskbar
                    {
                        return None;
                    }
                    Some(ShellTask {
                        id: client.win.raw(),
                        incarnation: client_key.data().as_ffi(),
                        name: bounded_text(&client.name),
                        class: bounded_text(&client.class),
                        monitor: id,
                        is_focused: self.get_selected_client_key() == Some(client_key),
                        is_minimized: client.state.is_hidden,
                    })
                })
                .take(64)
                .collect();
            outputs.push(ShellOutput {
                id,
                identity: self.desktop_shell_output_identity(key),
                connector: self.output_connector_for_monitor(backend, key),
                geometry,
                scale: f64::from(scale),
                visible,
                snapshot: model.snapshot(),
                tasks,
            });
        }
        ShellSnapshot {
            protocol_version: SHELL_PROTOCOL_VERSION,
            session: wm_session_id(),
            revision: self.desktop_shell.revision.max(1),
            config: cfg.desktop_shell_config(),
            source: cfg.desktop_shell_source(),
            outputs,
        }
    }

    pub(crate) fn desktop_shell_snapshot_json(&mut self, backend: &dyn Backend) -> Value {
        let snapshot = self.desktop_shell_snapshot(backend);
        let mut state = serde_json::to_value(snapshot).expect("shell snapshot is serializable");
        // Compare state independently of its sequence; repeated snapshots are
        // idempotent, while every changed payload receives a greater revision.
        state["revision"] = json!(0);
        if self.desktop_shell.last_state.as_ref() != Some(&state) {
            self.desktop_shell.revision = self.desktop_shell.revision.saturating_add(1).max(1);
            self.desktop_shell.last_state = Some(state.clone());
        }
        state["revision"] = json!(self.desktop_shell.revision);
        state["host"] = json!({
            "pid": self.desktop_shell.child.as_ref().map(Child::id),
            "executable": self.desktop_shell.child.as_ref().and_then(|child| std::fs::read_link(format!("/proc/{}/exe", child.id())).ok()).map(|path| path.to_string_lossy().into_owned()),
            "requested_executable": std::env::var_os("JWM_SHELL_BIN").unwrap_or_else(|| "jwm-shell".into()).to_string_lossy().into_owned(),
            "backend": self.runtime_backend,
            "protocol_version": SHELL_PROTOCOL_VERSION,
            "ready": self.desktop_shell.ready,
            "synchronized": self.desktop_shell.ready,
            "panels": self.desktop_shell.panels,
            "top_bars": self.secondary_bars.iter().map(|(&output, bar)| json!({"output": output, "pid": bar.pid, "window": bar.window.map(|window| window.raw())})).collect::<Vec<_>>(),
            "reserving": self.desktop_shell.reserves_space(),
            "failures": self.desktop_shell.failures,
            "last_error": self.desktop_shell.last_error,
        });
        state
    }

    pub(crate) fn publish_desktop_shell_state(&mut self, backend: &dyn Backend, now: Instant) {
        if self.desktop_shell.next_state.is_some_and(|at| now < at) {
            return;
        }
        self.desktop_shell.next_state = Some(now + STATE_INTERVAL);
        if self
            .ipc_server
            .as_ref()
            .is_none_or(|ipc| !ipc.has_subscribers("desktop_shell/state"))
        {
            return;
        }
        let snapshot = self.desktop_shell_snapshot_json(backend);
        if self.desktop_shell.revision != self.desktop_shell.last_published_revision {
            self.desktop_shell.last_published_revision = self.desktop_shell.revision;
            self.broadcast_ipc_event("desktop_shell/state", snapshot);
        }
    }

    pub(crate) fn register_desktop_shell(
        &mut self,
        backend: &mut dyn Backend,
        args: &Value,
    ) -> IpcResponse {
        if args["protocol_version"].as_u64() != Some(u64::from(SHELL_PROTOCOL_VERSION)) {
            return IpcResponse::err("incompatible desktop shell protocol");
        }
        if args["session"].as_u64() != Some(wm_session_id()) {
            return IpcResponse::err("stale desktop shell session");
        }
        let Some(child) = self.desktop_shell.child.as_ref() else {
            return IpcResponse::err("no supervised desktop shell");
        };
        if args["pid"].as_u64() != Some(u64::from(child.id())) {
            return IpcResponse::err("desktop shell ownership mismatch");
        }
        let panels: Vec<ShellPanel> = match serde_json::from_value(
            args.get("panels").cloned().unwrap_or_else(|| json!([])),
        ) {
            Ok(panels) => panels,
            Err(error) => return IpcResponse::err(format!("invalid shell panels: {error}")),
        };
        let outputs = self
            .state
            .monitors
            .values()
            .map(|monitor| monitor.num)
            .collect();
        if let Err(error) = validate_panels(&panels, &outputs) {
            return IpcResponse::err(error);
        }
        let config = CONFIG.load().desktop_shell_config();
        let complete = outputs.iter().all(|id| {
            [
                ("left", xbar_core::BarEdge::Left),
                ("right", xbar_core::BarEdge::Right),
                ("bottom", xbar_core::BarEdge::Bottom),
            ]
            .into_iter()
            .all(|(role, edge)| {
                config.mode(edge) == xbar_core::edge_shell::RevealMode::Disabled
                    || panels
                        .iter()
                        .any(|panel| panel.output == *id && panel.role == role)
            })
        });
        self.synchronize_desktop_shell_output_identities();
        for panel in &panels {
            if self
                .get_monitor_by_id(panel.output)
                .map(|key| self.desktop_shell_output_identity(key))
                != Some(panel.output_identity)
            {
                return IpcResponse::err("stale shell panel output identity");
            }
        }
        self.desktop_shell.panels = panels;
        self.desktop_shell.ready = complete;
        self.desktop_shell.heartbeat = Some(Instant::now());
        self.desktop_shell.last_error = None;
        // A successful handshake clears transient failures only after the
        // host survived a full healthy interval, preventing rapid crash loops.
        if self
            .desktop_shell
            .started
            .is_some_and(|start| start.elapsed() >= Duration::from_secs(30))
        {
            self.desktop_shell.failures = 0;
        }
        let _ = backend;
        IpcResponse::ok(Some(
            json!({"session": wm_session_id(), "protocol_version": SHELL_PROTOCOL_VERSION}),
        ))
    }

    pub(crate) fn desktop_shell_action(
        &mut self,
        backend: &mut dyn Backend,
        args: &Value,
    ) -> IpcResponse {
        if args["session"].as_u64() != Some(wm_session_id()) {
            return IpcResponse::err("stale desktop shell session");
        }
        let Some(output) = args["output"]
            .as_i64()
            .and_then(|id| i32::try_from(id).ok())
        else {
            return IpcResponse::err("missing desktop shell output");
        };
        let Some(key) = self.get_monitor_by_id(output) else {
            return IpcResponse::err("disconnected desktop shell output");
        };
        self.synchronize_desktop_shell_output_identities();
        if args["output_identity"].as_u64() != Some(self.desktop_shell_output_identity(key)) {
            return IpcResponse::err("stale desktop shell output identity");
        }
        let command: Option<WmCommand> = match args
            .get("command")
            .map(|value| serde_json::from_value(value.clone()))
            .transpose()
        {
            Ok(command) => command,
            Err(error) => return IpcResponse::err(format!("invalid shell command: {error}")),
        };
        let cleanup = matches!(
            command,
            Some(WmCommand::PreviewWindow { visible: false, .. })
        ) || matches!(command, Some(WmCommand::SetDockGeometry { geometry, .. }) if geometry.width == 0 || geometry.height == 0);
        if !cleanup
            && (!CONFIG.load().desktop_shell_enabled()
                || !self.monitor_shows_status_bar(key)
                || self.monitor_key_is_locked(key))
        {
            return IpcResponse::err("desktop shell is hidden");
        }
        if let Some(command) = command {
            let shared = xbar_core::transport::command_to_shared(command);
            if shared.monitor_id != output {
                return IpcResponse::err("shell command output mismatch");
            }
            if shared.wm_session_id != 0 {
                let generation = self.refresh_minimized_generation(output);
                if !super::valid_dock_command_source(output, generation, &shared) {
                    return IpcResponse::err("stale shell window generation");
                }
            }
            self.process_bar_commands(backend, vec![(output, shared)]);
            IpcResponse::ok(Some(json!({"applied": true})))
        } else {
            let name = args["name"].as_str().unwrap_or("");
            if !matches!(
                name,
                "focus_window" | "media_previous" | "media_next" | "media_play_pause" | "spawn"
            ) {
                return IpcResponse::err("unsupported shell action");
            }
            if name == "focus_window" {
                let id = args["args"]["id"].as_u64().unwrap_or(0);
                let incarnation = args["incarnation"]
                    .as_u64()
                    .or_else(|| args["args"]["incarnation"].as_u64());
                let target = self
                    .state
                    .clients
                    .iter()
                    .find(|(_, client)| client.win.raw() == id);
                if !target.is_some_and(|(key, client)| {
                    Some(key.data().as_ffi()) == incarnation
                        && client.mon == self.get_monitor_by_id(output)
                }) {
                    return IpcResponse::err("stale shell task identity");
                }
            }
            if args.get("generation").is_some_and(|generation| {
                generation.as_u64() != self.refresh_minimized_generation(output)
            }) {
                return IpcResponse::err("stale shell window generation");
            }
            self.handle_ipc_command(backend, name, args.get("args").unwrap_or(&Value::Null))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn managed_edges_follow_menu_theme_including_aliases_and_fallback() {
        for name in [
            "glass",
            "glass-light",
            "frosted",
            "paper",
            "light",
            "unknown",
        ] {
            assert_eq!(desktop_shell_theme(name), ThemeMode::Light, "{name}");
        }
        for name in [
            "glass-dark",
            "glass_dark",
            "aurora",
            "nord",
            "tokyo-night",
            "material",
        ] {
            assert_eq!(desktop_shell_theme(name), ThemeMode::Dark, "{name}");
        }
    }

    #[test]
    fn output_epoch_changes_when_a_surviving_monitor_slot_gets_a_different_connector() {
        let mut host = DesktopShellSupervisor::default();
        let first = host.identify_output(91, Some(60));
        assert_ne!(first, 0);
        assert_eq!(host.identify_output(91, Some(60)), first);
        let replacement = host.identify_output(91, Some(77));
        assert_ne!(replacement, first);
        assert_ne!(host.identify_output(91, Some(60)), first);
        host.output_identities.remove(&91);
        assert_ne!(host.identify_output(91, Some(60)), first);
        assert_ne!(host.identify_output(92, Some(60)), first);
    }

    #[test]
    fn wm_render_stalls_do_not_age_host_heartbeat_or_handshake() {
        let started = Instant::now();
        // The supervisor owns a Child and implements Drop, so struct-update
        // syntax cannot move the remaining non-Copy fields from a default.
        let mut host = DesktopShellSupervisor::default();
        host.started = Some(started);
        host.heartbeat = Some(started);
        host.note_observer_tick(started + Duration::from_secs(1));
        host.note_observer_tick(started + Duration::from_secs(21));
        assert_eq!(host.started, Some(started + Duration::from_secs(20)));
        assert_eq!(host.heartbeat, Some(started + Duration::from_secs(20)));
        // Regular service after the stall does not keep forgiving a genuinely
        // missing heartbeat: its age now advances toward the normal deadline.
        for second in 22..=31 {
            host.note_observer_tick(started + Duration::from_secs(second));
        }
        assert_eq!(host.heartbeat, Some(started + Duration::from_secs(20)));
        assert!(started + Duration::from_secs(31) - host.heartbeat.unwrap() > HEARTBEAT_TIMEOUT);
    }

    #[test]
    fn backoff_is_bounded_and_never_busy_loops() {
        assert_eq!(retry_delay(1), Duration::from_secs(1));
        assert_eq!(retry_delay(2), Duration::from_secs(2));
        assert_eq!(retry_delay(u32::MAX), Duration::from_secs(60));
    }
    #[test]
    fn panel_registry_rejects_duplicates_stale_outputs_and_bad_roles() {
        let outputs = HashSet::from([0, 2]);
        let panel = ShellPanel {
            output: 0,
            output_identity: 1,
            role: "left".into(),
            window: 12,
        };
        assert!(validate_panels(std::slice::from_ref(&panel), &outputs).is_ok());
        assert!(
            validate_panels(
                &[
                    panel.clone(),
                    ShellPanel {
                        window: 13,
                        ..panel.clone()
                    }
                ],
                &outputs
            )
            .is_err()
        );
        assert!(
            validate_panels(
                &[ShellPanel {
                    output: 1,
                    ..panel.clone()
                }],
                &outputs
            )
            .is_err()
        );
        assert!(
            validate_panels(
                &[ShellPanel {
                    role: "top".into(),
                    ..panel.clone()
                }],
                &outputs
            )
            .is_err()
        );
        assert!(validate_panels(&[ShellPanel { window: 0, ..panel }], &outputs).is_err());
    }
}
