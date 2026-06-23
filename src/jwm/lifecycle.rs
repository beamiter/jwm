// Lifecycle management: cleanup, config reload, and resource management

use crate::backend::api::{Backend, WindowChanges};
use crate::backend::common_define::{ArgbColor, ColorScheme, EventMaskBits, SchemeType, WindowId};
use crate::config::CONFIG;
use crate::core::models::{ClientKey, MonitorKey};
use crate::ipc::IpcResponse;
use crate::Jwm;
use log::{info, warn};
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

impl Jwm {
    pub fn cleanup(&mut self, backend: &mut dyn Backend) -> Result<(), Box<dyn std::error::Error>> {
        info!("[cleanup] Starting essential cleanup (letting Rust handle memory)");
        // Shut down IPC server (also handled by Drop, but explicit is clearer)
        if let Some(ref mut ipc) = self.ipc_server {
            ipc.shutdown();
        }
        self.ipc_server = None;
        self.cleanup_x11_resources(backend)?;
        self.cleanup_system_resources()?;
        backend.color_allocator().free_all_theme_pixels()?;
        backend.window_ops().flush()?;
        info!("[cleanup] Essential cleanup completed (Rust will handle the rest)");
        Ok(())
    }

    pub(crate) fn cleanup_x11_resources(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("[cleanup_x11_resources] Cleaning X11 resources");

        // Stop recording on shutdown. We do NOT cross-restart resume: previously
        // that caused silent runaway recordings spanning many restarts (segment
        // count grew by 1 per restart, output never finalized). Segments are
        // left on /tmp; concat manually if needed — finalize_recording's thread
        // would not survive the exec() restart path anyway.
        if self.features.recording.active {
            backend.compositor_stop_recording();
            if let Some(seg) = self.features.recording.current_segment.take() {
                self.features.recording.segments.push(seg);
            }
            let n = self.features.recording.segments.len();
            let target = self.features.recording.output_path.as_deref().unwrap_or("(unset)");
            info!(
                "[cleanup_x11_resources] Recording stopped on shutdown: {n} segment(s) on /tmp/jwm-rec-*, target was {target}"
            );
            self.features.recording.active = false;
        }

        self.cleanup_all_clients_x11_state(backend)?;

        self.cleanup_key_grabs(backend)?;

        self.reset_input_focus(backend)?;

        backend.cleanup()?;

        if let Err(e) = backend.cursor_provider().cleanup() {
            log::warn!("cursor cleanup failed: {:?}", e);
        }

        info!("[cleanup_x11_resources] X11 resources cleaned");
        Ok(())
    }

    pub(crate) fn cleanup_system_resources(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        info!("[cleanup_system_resources] Cleaning system resources");

        self.cleanup_statusbar_processes()?;

        self.cleanup_shared_memory_resources()?;

        info!("[cleanup_system_resources] System resources cleaned");
        Ok(())
    }

    pub(crate) fn cleanup_all_clients_x11_state(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("[cleanup_all_clients_x11_state]");
        let restarting = self.is_restarting.load(Ordering::SeqCst);

        let mut clients_to_process = Vec::new();
        for &mon_key in &self.state.monitor_order {
            if let Some(stack) = self.state.monitor_stack.get(mon_key) {
                for &ck in stack {
                    if let Some(c) = self.state.clients.get(ck) {
                        clients_to_process.push((c.win, c.geometry.old_border_w, ck));
                    }
                }
            }
        }
        for (win, old_border_w, ck) in clients_to_process {
            if let Some(_) = self.state.clients.get(ck) {
                if restarting {
                    backend.window_ops().ungrab_all_buttons(win)?;
                } else {
                    let _ = self.restore_client_x11_state(backend, win, old_border_w);
                }
            }
        }

        Ok(())
    }

    pub(crate) fn restore_client_x11_state(
        &mut self,
        backend: &mut dyn Backend,
        win: WindowId,
        old_border_w: i32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Err(e) = backend
            .window_ops()
            .change_event_mask(win, EventMaskBits::NONE.bits())
        {
            log::warn!("Failed to clear events for {:?}: {:?}", win, e);
        }
        let changes = WindowChanges {
            border_width: Some(old_border_w as u32),
            ..Default::default()
        };
        if let Err(e) = backend.window_ops().apply_window_changes(win, changes) {
            log::warn!("Failed to restore border for {:?}: {:?}", win, e);
        }
        if let Err(e) = backend.window_ops().ungrab_all_buttons(win) {
            log::warn!("Failed to ungrab buttons for {:?}: {:?}", win, e);
        }
        if let Err(e) = self.setclientstate(backend, win, crate::jwm::WITHDRAWN_STATE as i64) {
            log::warn!("Failed to set withdrawn state for {:?}: {:?}", win, e);
        }
        Ok(())
    }

    pub(crate) fn cleanup_statusbar_processes(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // Clean up secondary bars
        self.cleanup_secondary_bars()?;
        Ok(())
    }

    pub(crate) fn cleanup_secondary_bars(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        for (mon_id, mut bar) in self.secondary_bars.drain() {
            // The child may already have exited (and been reaped via its Child
            // handle in reap_zombies, which caches the status). Once reaped, the
            // PID can be recycled by the kernel, so signalling it would hit an
            // unrelated process. Consult the handle before touching the PID.
            match bar.child.try_wait() {
                Ok(Some(status)) => {
                    info!("Secondary bar {} already exited: {:?}", mon_id, status);
                    continue;
                }
                Err(e) => {
                    warn!(
                        "Secondary bar {} status unknown ({}); not signalling PID",
                        mon_id, e
                    );
                    continue;
                }
                Ok(None) => {}
            }

            let pid = bar.child.id();
            let nix_pid = Pid::from_raw(pid as i32);

            match signal::kill(nix_pid, None) {
                Err(_) => {
                    info!("Secondary bar for monitor {} already terminated", mon_id);
                    continue;
                }
                Ok(_) => {}
            }

            if let Ok(_) = signal::kill(nix_pid, Signal::SIGTERM) {
                let timeout = Duration::from_secs(3);
                let start = Instant::now();
                while start.elapsed() < timeout {
                    match bar.child.try_wait() {
                        Ok(Some(status)) => {
                            info!("Secondary bar {} exited gracefully: {:?}", mon_id, status);
                            break;
                        }
                        Ok(None) => {
                            std::thread::sleep(Duration::from_millis(100));
                        }
                        Err(_) => {
                            break;
                        }
                    }
                }
                if bar.child.try_wait().ok().flatten().is_none() {
                    warn!("Secondary bar {} timeout, forcing kill", mon_id);
                    let _ = signal::kill(nix_pid, Signal::SIGKILL);
                }
            }
        }
        Ok(())
    }

    pub(crate) fn cleanup_shared_memory_resources(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // Clean up all monitor bars shared memory
        for (mon_id, bar) in self.secondary_bars.drain() {
            drop(bar.shmem);
            #[cfg(unix)]
            {
                let path = format!("/dev/shm/jwm_bar_mon_{}", mon_id);
                if std::path::Path::new(&path).exists() {
                    if let Err(e) = std::fs::remove_file(&path) {
                        warn!("Failed to remove {}: {}", path, e);
                    }
                }
            }
        }

        Ok(())
    }

    pub(crate) fn cleanup_key_grabs(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Err(e) = backend
            .key_ops()
            .clear_key_grabs(backend.root_window().expect("no root window"))
        {
            warn!("[cleanup_key_grabs] Failed to ungrab keys: {:?}", e);
        }
        Ok(())
    }
    pub(crate) fn do_config_reload(&mut self, backend: &mut dyn Backend) -> IpcResponse {
        match crate::config::reload_global() {
            Ok(()) => {
                self.apply_config_changes(backend);
                self.broadcast_ipc_event("config/reload", serde_json::json!({}));
                IpcResponse::ok(None)
            }
            Err(e) => IpcResponse::err(format!("config reload failed: {e}")),
        }
    }

    pub(crate) fn apply_config_changes(&mut self, backend: &mut dyn Backend) {
        let cfg = CONFIG.load();

        // 1. Rebind keys
        self.key_bindings = cfg.get_keys();
        self.chord_compiled = cfg.compile_chord();
        self.chord_armed_until = None;
        if let Err(e) = self.grabkeys(backend) {
            warn!("[config] failed to re-grab keys: {e}");
        }
        // Pick up DND default from config (without overriding a runtime toggle: only
        // when the config value differs from our default-on-startup, refresh).
        // Simpler: trust config — reload reflects user's saved preference.
        self.do_not_disturb = cfg.behavior().do_not_disturb;

        // 2. Re-apply color schemes
        let colors = cfg.colors();
        let alloc = backend.color_allocator();
        let _ = alloc.free_all_theme_pixels();
        if let (Ok(norm_fg), Ok(norm_bg), Ok(norm_border)) = (
            ArgbColor::from_hex(&colors.dark_sea_green1, colors.opaque),
            ArgbColor::from_hex(&colors.light_sky_blue1, colors.opaque),
            ArgbColor::from_hex(&colors.light_sky_blue1, colors.opaque),
        ) {
            alloc.set_scheme(
                SchemeType::Norm,
                ColorScheme::new(norm_fg, norm_bg, norm_border),
            );
        }
        if let (Ok(sel_fg), Ok(sel_bg), Ok(sel_border)) = (
            ArgbColor::from_hex(&colors.dark_sea_green2, colors.opaque),
            ArgbColor::from_hex(&colors.pale_turquoise1, colors.opaque),
            ArgbColor::from_hex(&colors.cyan, colors.opaque),
        ) {
            alloc.set_scheme(
                SchemeType::Sel,
                ColorScheme::new(sel_fg, sel_bg, sel_border),
            );
        }
        let _ = alloc.allocate_schemes_pixels();

        // 3. Re-arrange all monitors (border/gap changes take effect)
        let mon_keys: Vec<MonitorKey> = self.state.monitor_order.clone();
        for mk in &mon_keys {
            self.arrange(backend, Some(*mk));
        }

        // 4. Update decoration on all visible clients
        let sel_ck = self.get_selected_client_key();

        // 5. Toggle compositor if config changed
        let compositor_wanted = cfg.compositor_enabled();
        let compositor_active = backend.has_compositor();
        if compositor_wanted != compositor_active {
            match backend.set_compositor_enabled(compositor_wanted) {
                Ok(true) => log::info!(
                    "Compositor {}",
                    if compositor_wanted {
                        "enabled"
                    } else {
                        "disabled"
                    }
                ),
                Ok(false) => {}
                Err(e) => log::warn!("Failed to set compositor: {e}"),
            }
        }

        // 6. Hot-reload all compositor settings
        backend.compositor_apply_config();

        let client_keys: Vec<ClientKey> = self.state.client_order.clone();
        for ck in client_keys {
            if let Some(_client) = self.state.clients.get(ck) {
                let is_sel = sel_ck == Some(ck);
                let _ = self.update_client_decoration(backend, ck, is_sel);
            }
        }
    }
}
