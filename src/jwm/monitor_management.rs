use crate::backend::api::Backend;
use crate::config::CONFIG;
use crate::core::layout::LayoutEnum;
use crate::core::models::{MonitorKey, Pertag, WMMonitor};
use log::{error, info, warn};
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use std::collections::HashSet;
use std::io;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant};
use xbar_core::shared_structures::SharedRingBufferOptions;

use super::Jwm;

const BAR_MAP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const BAR_MAX_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(60);
const BAR_EXIT_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Stop and reap a secondary bar through its owning `Child` handle.
///
/// `Child::kill` only sends SIGKILL; dropping the handle immediately afterwards
/// can therefore leave a zombie. Give the bar a bounded opportunity to handle
/// SIGTERM, then force it down and synchronously collect the exit status.
pub(super) fn terminate_secondary_bar_child(
    child: &mut Child,
    terminate_timeout: Duration,
) -> io::Result<ExitStatus> {
    if let Some(status) = child.try_wait()? {
        return Ok(status);
    }

    let pid = Pid::from_raw(child.id() as i32);
    let _ = signal::kill(pid, Signal::SIGTERM);
    let deadline = Instant::now() + terminate_timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }

        let now = Instant::now();
        if now >= deadline {
            break;
        }
        std::thread::sleep(BAR_EXIT_POLL_INTERVAL.min(deadline.saturating_duration_since(now)));
    }

    match child.kill() {
        Ok(()) => loop {
            match child.wait() {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => return result,
            }
        },
        Err(kill_error) => match child.try_wait()? {
            Some(status) => Ok(status),
            None => Err(kill_error),
        },
    }
}

impl Jwm {
    pub(super) fn createmon(&mut self, show_bar: bool) -> WMMonitor {
        // info!("[createmon]");
        let cfg = CONFIG.load();
        let mut m: WMMonitor = WMMonitor::new();
        m.tag_set[0] = 1;
        m.tag_set[1] = 1;
        m.layout.m_fact = cfg.m_fact();
        m.layout.n_master = cfg.n_master();
        m.layout.gap = cfg.gap_px() as i32;
        m.lt = Rc::new(LayoutEnum::FIBONACCI);
        m.prev_lt = Rc::new(LayoutEnum::TILE);
        m.lt_symbol = m.lt.symbol().to_string();
        m.pertag = Some(Pertag::new(show_bar, cfg.tags_length()));
        // SAFETY: pertag was just set to Some on the line above
        let ref_pertag = m.pertag.as_mut().expect("pertag just initialized");
        ref_pertag.cur_tag = 1;
        ref_pertag.prev_tag = 1;
        let default_lt = m.lt.clone();
        let default_prev_lt = m.prev_lt.clone();
        for i in 0..=cfg.tags_length() {
            ref_pertag.n_masters[i] = m.layout.n_master;
            ref_pertag.m_facts[i] = m.layout.m_fact;
            ref_pertag.gaps[i] = m.layout.gap;

            ref_pertag.lts[i] = default_lt.clone();
            ref_pertag.prev_lts[i] = default_prev_lt.clone();
        }
        // Saved per-tag layouts land on top of those defaults. The monitor is
        // appended by `insert_monitor`, so the index it will answer to is the
        // current length — which is what the saved entries are keyed by.
        let mon_index = self.state.monitor_order.len() as i32;
        crate::jwm::layout::persist::seed_pertag_from_config(&mut m, mon_index, &cfg);
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

    pub(super) fn ensure_secondary_bars_running(
        &mut self,
        backend: &mut dyn Backend,
        now: Instant,
    ) {
        // Get all monitor IDs sorted
        let mut all_mon_ids: Vec<i32> = self.state.monitors.values().map(|m| m.num).collect();
        all_mon_ids.sort_unstable();

        // Sequential creation: only create the next bar if all previous bars are managed
        for &mon_id in &all_mon_ids {
            if let Some(&retry_after) = self.secondary_bar_retry_after.get(&mon_id) {
                if now < retry_after {
                    continue;
                }
                self.secondary_bar_retry_after.remove(&mon_id);
            }

            // Check if this bar already exists
            if self.secondary_bars.contains_key(&mon_id) {
                let mut remove_reason: Option<String> = None;
                let mut waiting_for_map = false;
                let lost_managed_window = self
                    .secondary_bars
                    .get(&mon_id)
                    .and_then(|bar| bar.client_key)
                    .is_some_and(|client_key| !self.state.clients.contains_key(client_key));

                // Check if process is still alive
                if let Some(bar) = self.secondary_bars.get_mut(&mon_id) {
                    if lost_managed_window {
                        remove_reason = Some("managed bar window disappeared".to_owned());
                    } else {
                        match bar.child.try_wait() {
                            Ok(Some(status)) => {
                                remove_reason = Some(format!("exited: {status}"));
                            }
                            Ok(None) => {
                                // Process still running. If it never maps a window, treat it as a
                                // failed bar after a short grace period and let the WM keep going.
                                if bar.window.is_none() {
                                    if now.saturating_duration_since(bar.last_spawn)
                                        > BAR_MAP_TIMEOUT
                                    {
                                        remove_reason = Some(format!(
                                            "did not map a window within {}s",
                                            BAR_MAP_TIMEOUT.as_secs()
                                        ));
                                    } else {
                                        waiting_for_map = true;
                                    }
                                }
                            }
                            Err(e) => {
                                remove_reason = Some(format!("try_wait failed: {e}"));
                            }
                        }
                    }
                }

                if let Some(reason) = remove_reason {
                    info!("Bar for monitor {} failed: {}", mon_id, reason);
                    self.handle_secondary_bar_failure(backend, mon_id, now, &reason);
                    continue;
                }

                if waiting_for_map {
                    return;
                }

                // This bar is managed, continue to check next
                continue;
            }

            // Bar doesn't exist, create it
            info!("Creating bar for monitor {} (sequential creation)", mon_id);
            self.spawn_secondary_bar(mon_id, now);
            // Only create one at a time, stop here
            return;
        }

        // Remove bars for monitors that no longer exist
        let existing_monitors: HashSet<i32> = self.state.monitors.values().map(|m| m.num).collect();
        let removed_monitors: Vec<_> = self
            .secondary_bars
            .keys()
            .copied()
            .filter(|monitor| !existing_monitors.contains(monitor))
            .collect();
        for monitor in removed_monitors {
            let _ = self.retire_secondary_bar(backend, monitor);
        }
        self.secondary_bar_failures
            .retain(|&mon_id, _| existing_monitors.contains(&mon_id));
        self.secondary_bar_retry_after
            .retain(|&mon_id, _| existing_monitors.contains(&mon_id));
    }

    /// Apply the common cleanup/backoff policy after a managed bar fails.
    /// Whether failure was an observed exit, lost window, or map timeout, the
    /// owning `Child` is terminated and reaped before its bar entry is dropped.
    pub(super) fn handle_secondary_bar_failure(
        &mut self,
        backend: &mut dyn Backend,
        monitor_id: i32,
        now: Instant,
        reason: &str,
    ) {
        if let Some(mut bar) = self.secondary_bars.remove(&monitor_id)
            && let Err(error) = terminate_secondary_bar_child(&mut bar.child, Duration::ZERO)
        {
            warn!(
                "Could not stop and reap failed bar on monitor {}: {}",
                monitor_id, error
            );
        }
        self.clear_minimized_dock_for_monitor(backend, monitor_id);
        self.note_secondary_bar_failure(monitor_id, now, reason);
    }

    /// Permanently retire the bar for an output that no longer exists.
    /// Unlike a crash, this is not retried and must also remove its managed
    /// Dock client before monitor migration can attach that bar to another
    /// output. Returns the retired client key so monitor ownership cleanup can
    /// explicitly keep it out of the reassignment set even if unmanage reports
    /// a late backend error.
    pub(super) fn retire_secondary_bar(
        &mut self,
        backend: &mut dyn Backend,
        monitor_id: i32,
    ) -> Option<crate::core::models::ClientKey> {
        self.clear_minimized_dock_for_monitor(backend, monitor_id);
        self.pending_bar_updates.remove(&monitor_id);
        self.minimized_projection_epochs.remove(&monitor_id);
        self.reconciled_minimized_target_generations
            .remove(&monitor_id);
        self.secondary_bar_failures.remove(&monitor_id);
        self.secondary_bar_retry_after.remove(&monitor_id);

        let Some(mut bar) = self.secondary_bars.remove(&monitor_id) else {
            return None;
        };
        let retired_client = bar.client_key;

        if let Some(client_key) = bar.client_key
            && self.state.clients.contains_key(client_key)
            && let Err(error) = self.unmanage(backend, Some(client_key), true)
        {
            warn!(
                "Could not unmanage retired bar client on monitor {}: {}",
                monitor_id, error
            );
        }

        if let Err(error) = terminate_secondary_bar_child(&mut bar.child, Duration::ZERO) {
            warn!(
                "Could not stop and reap retired bar on monitor {}: {}",
                monitor_id, error
            );
        }

        retired_client
    }

    pub(super) fn note_secondary_bar_failure(
        &mut self,
        monitor_id: i32,
        now: Instant,
        reason: &str,
    ) {
        let failures = self.secondary_bar_failures.entry(monitor_id).or_insert(0);
        *failures = failures.saturating_add(1);

        let delay_secs = match *failures {
            0 | 1 => 5,
            2 => 10,
            3 => 30,
            _ => BAR_MAX_RETRY_DELAY.as_secs(),
        };
        let delay = std::time::Duration::from_secs(delay_secs).min(BAR_MAX_RETRY_DELAY);
        self.secondary_bar_retry_after
            .insert(monitor_id, now + delay);

        warn!(
            "Secondary bar for monitor {} failed ({}); retrying in {}s (failure #{})",
            monitor_id,
            reason,
            delay.as_secs(),
            failures
        );
    }

    pub(super) fn spawn_secondary_bar(&mut self, monitor_id: i32, now: Instant) {
        // Create unique shared memory path for this monitor
        let shared_path = format!("/dev/shm/jwm_bar_mon_{}", monitor_id);

        // JWM is the single supervisor for per-monitor bar buffers. Reclaim a
        // mapping whose creator died before Drop could unlink its flink, then
        // atomically create a replacement.
        let ring_buffer = match SharedRingBufferOptions::new()
            // A full minimized shelf publishes one shelf anchor plus up to
            // MAX_MINIMIZED_WINDOWS item anchors. Leave headroom for hover
            // and restore commands while that geometry snapshot is queued.
            .command_capacity(32)
            .reclaim_stale(true)
            .open_or_create(&shared_path)
        {
            Ok(rb) => rb,
            Err(e) => {
                let reason = format!("shared-memory setup failed: {e}");
                error!(
                    "Failed to prepare shared memory for monitor {}: {}",
                    monitor_id, e
                );
                self.note_secondary_bar_failure(monitor_id, now, &reason);
                return;
            }
        };

        // open_or_create only returns a non-creator when another live
        // supervisor still owns this monitor's buffer. Do not attach a second
        // bar to it; wait for the normal retry path instead.
        if !ring_buffer.is_creator() {
            let reason = format!(
                "shared memory is owned by live creator process {}",
                ring_buffer.creator_pid()
            );
            self.note_secondary_bar_failure(monitor_id, now, &reason);
            return;
        }

        // Prepare command
        let cfg = CONFIG.load();
        let bar_name = cfg.status_bar_name();
        let mut command = {
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
                let reason = format!("process spawn failed: {e}");
                error!(
                    "Failed to spawn secondary bar for monitor {}: {}",
                    monitor_id, e
                );
                self.note_secondary_bar_failure(monitor_id, now, &reason);
            }
        }
    }

    pub(super) fn flush_pending_bar_updates(&mut self) {
        if self.pending_bar_updates.is_empty() {
            return;
        }

        // Take one batch so updates marked while publishing belong to the next
        // pass. A monitor whose bar has not been spawned yet stays pending:
        // focus can now call this method directly during early startup, before
        // the normal supervisor/update ordering has created every bar.
        let pending = std::mem::take(&mut self.pending_bar_updates);
        for mon_id in pending {
            if let Some(mon_key) = self.get_monitor_by_id(mon_id) {
                if !self.is_bar_visible_on_mon(mon_key) {
                    continue;
                }
                if !self.secondary_bars.contains_key(&mon_id) {
                    self.pending_bar_updates.insert(mon_id);
                    continue;
                }
                self.update_bar_message_for_monitor(Some(mon_key));

                // Send message to this monitor's bar via shared memory
                if let Some(bar) = self.secondary_bars.get_mut(&mon_id) {
                    // Bar state is an authoritative snapshot, not an event
                    // stream. If a slow bar fills its ring, preserving an old
                    // title/Dock list while silently dropping the newest one
                    // is the wrong failure mode; overwrite the oldest unread
                    // snapshot so `try_read_latest_message` always converges.
                    if let Err(error) = bar.shmem.write_message_overwrite(&self.message) {
                        log::warn!(
                            "failed to publish status-bar snapshot for monitor {mon_id}: {error}"
                        );
                    }
                }
            }
        }
    }

    pub(super) fn switch_to_monitor(
        &mut self,
        backend: &mut dyn Backend,
        target_monitor_key: MonitorKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.handle_monitor_switch_by_key(backend, Some(target_monitor_key))
    }
}

#[cfg(test)]
mod secondary_bar_child_tests {
    use super::*;

    #[test]
    fn terminate_secondary_bar_child_reaps_forced_exit() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "trap '' TERM; while :; do :; done"])
            .spawn()
            .expect("spawn stubborn child");

        let status = terminate_secondary_bar_child(&mut child, Duration::from_millis(20))
            .expect("terminate and reap child");

        assert!(!status.success());
        assert_eq!(child.try_wait().expect("query cached status"), Some(status));
    }

    #[test]
    fn terminate_secondary_bar_child_accepts_already_reaped_child() {
        let mut child = Command::new("/bin/true").spawn().expect("spawn child");
        let original = child.wait().expect("reap child");

        let status = terminate_secondary_bar_child(&mut child, Duration::ZERO)
            .expect("return cached status");

        assert_eq!(status, original);
    }
}
