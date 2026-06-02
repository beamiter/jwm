use crate::backend::api::Backend;
use crate::core::models::{MonitorKey, WMMonitor, Pertag};
use crate::core::layout::LayoutEnum;
use crate::config::CONFIG;
use std::rc::Rc;
use std::time::Instant;
use std::process::{Command, Stdio};
use std::collections::HashSet;
use log::{info, error};
use shared_structures::SharedRingBuffer;

use super::Jwm;

impl Jwm {
    pub(super) fn createmon(&mut self, show_bar: bool) -> WMMonitor {
        // info!("[createmon]");
        let cfg = CONFIG.load();
        let mut m: WMMonitor = WMMonitor::new();
        m.tag_set[0] = 1;
        m.tag_set[1] = 1;
        m.layout.m_fact = cfg.m_fact();
        m.layout.n_master = cfg.n_master();
        m.lt[0] = Rc::new(LayoutEnum::FIBONACCI);
        m.lt[1] = Rc::new(LayoutEnum::TILE);
        m.lt_symbol = m.lt[0].symbol().to_string();
        m.pertag = Some(Pertag::new(show_bar, cfg.tags_length()));
        // SAFETY: pertag was just set to Some on the line above
        let ref_pertag = m.pertag.as_mut().expect("pertag just initialized");
        ref_pertag.cur_tag = 1;
        ref_pertag.prev_tag = 1;
        let default_layout_0 = m.lt[0].clone();
        let default_layout_1 = m.lt[1].clone();
        for i in 0..=cfg.tags_length() {
            ref_pertag.n_masters[i] = m.layout.n_master;
            ref_pertag.m_facts[i] = m.layout.m_fact;

            ref_pertag.lt_idxs[i][0] = Some(default_layout_0.clone());
            ref_pertag.lt_idxs[i][1] = Some(default_layout_1.clone());
            ref_pertag.sel_lts[i] = m.sel_lt;
        }
        info!("[createmon]: {}", m);
        return m;
    }

    pub(super) fn dirtomon(&mut self, dir: &i32) -> Option<MonitorKey> {
        let selected_monitor_key = self.state.sel_mon?;
        if self.state.monitor_order.is_empty() {
            return None;
        }
        let current_index = self
            .state
            .monitor_order
            .iter()
            .position(|&key| key == selected_monitor_key)?;
        if *dir > 0 {
            let next_index = (current_index + 1) % self.state.monitor_order.len();
            Some(self.state.monitor_order[next_index])
        } else {
            let prev_index = if current_index == 0 {
                self.state.monitor_order.len() - 1
            } else {
                current_index - 1
            };
            Some(self.state.monitor_order[prev_index])
        }
    }

    pub(super) fn ensure_secondary_bars_running(&mut self, now: Instant) {
        // Get all monitor IDs sorted
        let mut all_mon_ids: Vec<i32> = self.state.monitors.values().map(|m| m.num).collect();
        all_mon_ids.sort_unstable();

        // Sequential creation: only create the next bar if all previous bars are managed
        for &mon_id in &all_mon_ids {
            // Check if this bar already exists
            if let Some(bar) = self.secondary_bars.get_mut(&mon_id) {
                // Check if process is still alive
                match bar.child.try_wait() {
                    Ok(Some(status)) => {
                        info!("Bar for monitor {} exited: {}", mon_id, status);
                        self.secondary_bars.remove(&mon_id);
                        // Don't create next bar yet, wait for next tick
                        return;
                    }
                    Ok(None) => {
                        // Process still running
                        // If not yet managed (window not created), don't create next bar
                        if bar.window.is_none() {
                            return;
                        }
                        // This bar is managed, continue to check next
                        continue;
                    }
                    Err(e) => {
                        info!("Error checking bar for monitor {}: {}", mon_id, e);
                        self.secondary_bars.remove(&mon_id);
                        return;
                    }
                }
            }

            // Bar doesn't exist, create it
            info!("Creating bar for monitor {} (sequential creation)", mon_id);
            self.spawn_secondary_bar(mon_id, now);
            // Only create one at a time, stop here
            return;
        }

        // Remove bars for monitors that no longer exist
        let existing_monitors: HashSet<i32> = self.state.monitors.values().map(|m| m.num).collect();
        self.secondary_bars
            .retain(|&mon_id, _| existing_monitors.contains(&mon_id));
    }

    pub(super) fn spawn_secondary_bar(&mut self, monitor_id: i32, now: Instant) {
        // Create unique shared memory path for this monitor
        let shared_path = format!("/dev/shm/jwm_bar_mon_{}", monitor_id);

        // Create shared memory
        let ring_buffer = match SharedRingBuffer::create_aux(&shared_path, None, None) {
            Ok(rb) => rb,
            Err(e) => {
                error!(
                    "Failed to create shared memory for monitor {}: {}",
                    monitor_id, e
                );
                return;
            }
        };

        // Prepare command
        let cfg = CONFIG.load();
        let bar_name = cfg.status_bar_name();
        let mut command = if cfg!(feature = "nixgl") {
            let mut cmd = Command::new("nixGL");
            cmd.arg(bar_name).arg(&shared_path);
            cmd
        } else {
            let mut cmd = Command::new(bar_name);
            cmd.arg(&shared_path);
            cmd
        };

        // Set environment variables
        if let Ok(v) = std::env::var("WAYLAND_DISPLAY") {
            command.env("WAYLAND_DISPLAY", v);
        }
        if let Ok(v) = std::env::var("XDG_RUNTIME_DIR") {
            command.env("XDG_RUNTIME_DIR", v);
        }

        // Tell the bar which monitor it belongs to (for bar's internal use)
        command.env("JWM_MONITOR_ID", monitor_id.to_string());

        // Set empty to prevent GLib auto-discovery of $XDG_RUNTIME_DIR/bus.
        // env_remove is NOT sufficient: GIO falls back to the well-known
        // systemd socket when the var is unset, and on exec-restart the old
        // bar's GtkApplication name may still be registered — causing the new
        // instance to hang in single-instance activation.
        command.env("DBUS_SESSION_BUS_ADDRESS", "");
        command.env("GTK_IM_MODULE", "none");
        command.env("QT_IM_MODULE", "none");
        command.env("XMODIFIERS", "");
        command.env("GTK_A11Y", "none");
        command.env("NO_AT_BRIDGE", "1");

        // Disable GPU paths in GDK and GSK.
        // GSK_RENDERER=cairo prevents GTK4's widget pipeline from using GL.
        // GDK_DISABLE=gl,vulkan,dmabuf prevents GDK from binding zwp_linux_dmabuf_v1
        // and sending get_default_feedback() — a path independent of GL that hangs
        // in unprivileged DRM sessions where the compositor can't provide valid
        // dmabuf feedback without DRM master.  Forces pure wl_shm buffer allocation.
        if std::env::var_os("GSK_RENDERER").is_none() {
            command.env("GSK_RENDERER", "cairo");
        }
        command.env("GDK_DISABLE", "gl,vulkan,dmabuf");

        // Spawn the process
        match command
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
        {
            Ok(child) => {
                let pid = child.id();
                info!(
                    "Spawned secondary bar for monitor {} (PID: {})",
                    monitor_id, pid
                );

                let bar_instance = super::SecondaryBarInstance {
                    monitor_id,
                    shmem: ring_buffer,
                    pid,
                    child,
                    client_key: None,
                    window: None,
                    has_focus: false,
                    last_spawn: now,
                };

                self.secondary_bars.insert(monitor_id, bar_instance);
            }
            Err(e) => {
                error!(
                    "Failed to spawn secondary bar for monitor {}: {}",
                    monitor_id, e
                );
            }
        }
    }

    pub(super) fn flush_pending_bar_updates(&mut self) {
        if self.pending_bar_updates.is_empty() {
            return;
        }

        // Update all monitor bars that have pending updates
        for &mon_id in self.pending_bar_updates.clone().iter() {
            if let Some(mon_key) = self.get_monitor_by_id(mon_id) {
                if !self.is_bar_visible_on_mon(mon_key) {
                    continue;
                }
                self.update_bar_message_for_monitor(Some(mon_key));

                // Send message to this monitor's bar via shared memory
                if let Some(bar) = self.secondary_bars.get_mut(&mon_id) {
                    let _ = bar.shmem.try_write_message(&self.message);
                }
            }
        }
        self.pending_bar_updates.clear();
    }

    pub(super) fn switch_to_monitor(
        &mut self,
        backend: &mut dyn Backend,
        target_monitor_key: MonitorKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.handle_monitor_switch_by_key(backend, Some(target_monitor_key))
    }
}
