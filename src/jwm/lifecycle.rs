// Lifecycle management: cleanup, config reload, and resource management

use crate::Jwm;
use crate::backend::api::{Backend, WindowChanges};
use crate::backend::common_define::{ArgbColor, ColorScheme, EventMaskBits, SchemeType, WindowId};
use crate::config::CONFIG;
use crate::core::models::{ClientKey, MonitorKey};
use crate::ipc::IpcResponse;
use log::{info, warn};
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime};

const CONFIG_RELOAD_DEBOUNCE: Duration = Duration::from_millis(300);
const CONFIG_RELOAD_POLL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy)]
struct PendingConfigReload {
    revision: SystemTime,
    changed_at: Instant,
}

/// Shared state for event-driven and polling-based config reload detection.
///
/// A revision is its file modification time. Attempts are recorded before the
/// parser runs, so a malformed revision cannot produce an error on every
/// update tick; editing the file gives it a new revision and enables one new
/// attempt.
#[derive(Debug)]
pub(crate) struct ConfigReloadTracker {
    last_observed: Option<SystemTime>,
    last_attempted: Option<SystemTime>,
    pending: Option<PendingConfigReload>,
    last_poll_at: Option<Instant>,
}

impl ConfigReloadTracker {
    pub(crate) fn new(initial_revision: Option<SystemTime>) -> Self {
        Self {
            last_observed: initial_revision,
            // Loading CONFIG during startup settles the initial revision even
            // when parsing falls back to defaults. Wait for the next edit.
            last_attempted: initial_revision,
            pending: None,
            last_poll_at: None,
        }
    }

    fn should_poll(&mut self, now: Instant) -> bool {
        if self.last_poll_at.is_some_and(|last_poll| {
            now.saturating_duration_since(last_poll) < CONFIG_RELOAD_POLL_INTERVAL
        }) {
            return false;
        }

        self.last_poll_at = Some(now);
        true
    }

    fn observe(&mut self, revision: SystemTime, now: Instant) -> bool {
        if self.last_observed == Some(revision) {
            return false;
        }

        self.last_observed = Some(revision);
        if self.last_attempted == Some(revision) {
            self.pending = None;
            return false;
        }

        // A new revision restarts the debounce period. Repeated notifications
        // for the same revision are handled by the early return above and do
        // not postpone a stable reload indefinitely.
        self.pending = Some(PendingConfigReload {
            revision,
            changed_at: now,
        });
        true
    }

    fn take_due_attempt(&mut self, now: Instant) -> Option<SystemTime> {
        let pending = self.pending?;
        if !self.pending_is_due(now) {
            return None;
        }

        self.pending = None;
        self.last_attempted = Some(pending.revision);
        Some(pending.revision)
    }

    fn pending_is_due(&self, now: Instant) -> bool {
        self.pending.is_some_and(|pending| {
            now.saturating_duration_since(pending.changed_at) >= CONFIG_RELOAD_DEBOUNCE
        })
    }

    fn next_wakeup_in(&self, now: Instant) -> Duration {
        let poll_in = self.last_poll_at.map_or(Duration::ZERO, |last_poll| {
            CONFIG_RELOAD_POLL_INTERVAL.saturating_sub(now.saturating_duration_since(last_poll))
        });
        let debounce_in = self.pending.map(|pending| {
            CONFIG_RELOAD_DEBOUNCE.saturating_sub(now.saturating_duration_since(pending.changed_at))
        });

        debounce_in.map_or(poll_in, |debounce_in| poll_in.min(debounce_in))
    }

    fn deadline_is_due(&self, now: Instant) -> bool {
        self.next_wakeup_in(now).is_zero()
    }

    fn mark_attempted(&mut self, revision: SystemTime) {
        self.last_observed = Some(revision);
        self.last_attempted = Some(revision);
        self.pending = None;
    }
}

impl Jwm {
    fn record_config_reload_result(&mut self, success: bool, error: Option<String>) {
        self.config_reload_count = self.config_reload_count.saturating_add(1);
        self.config_reload_last_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX));
        self.config_reload_last_success = Some(success);
        self.config_reload_last_error = error;
    }

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
        // that caused silent runaway recordings spanning many restarts. The
        // compositor now writes directly to the final Videos path, so no
        // temporary segment has to be recovered or moved after shutdown.
        if self.features.recording.active {
            backend.compositor_stop_recording();
            self.features.recording.stop();
            let target = self
                .features
                .recording
                .output_path
                .as_deref()
                .unwrap_or("(unset)");
            info!("[cleanup_x11_resources] Recording stopped on shutdown; output is at {target}");
        }

        if self.features.audio_recording.active {
            let path = self.features.audio_recording.output_path.clone();
            if let Err(error) = self.features.audio_recording.stop() {
                warn!("[cleanup_x11_resources] Failed to stop audio recording: {error}");
            } else {
                info!(
                    "[cleanup_x11_resources] Audio recording finalized: {}",
                    path.as_deref().unwrap_or("(unset)")
                );
            }
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

    pub(crate) fn cleanup_shared_memory_resources(
        &mut self,
    ) -> Result<(), Box<dyn std::error::Error>> {
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
    fn perform_config_reload(&mut self, backend: &mut dyn Backend) -> IpcResponse {
        match crate::config::reload_global() {
            Ok(()) => {
                self.record_config_reload_result(true, None);
                self.apply_config_changes(backend);
                self.broadcast_ipc_event(
                    "config/reload",
                    serde_json::json!({
                        "success": true,
                        "reload_count": self.config_reload_count,
                        "last_reload_unix_ms": self.config_reload_last_unix_ms,
                    }),
                );
                IpcResponse::ok(None)
            }
            Err(e) => {
                let error = format!("config reload failed: {e}");
                self.record_config_reload_result(false, Some(error.clone()));
                self.broadcast_ipc_event(
                    "config/reload",
                    serde_json::json!({
                        "success": false,
                        "reload_count": self.config_reload_count,
                        "last_reload_unix_ms": self.config_reload_last_unix_ms,
                        "error": error,
                    }),
                );
                IpcResponse::err(error)
            }
        }
    }

    /// Explicit reloads (for example IPC) bypass the debounce but still settle
    /// the current revision so the polling fallback cannot reload it again.
    pub(crate) fn do_config_reload(&mut self, backend: &mut dyn Backend) -> IpcResponse {
        if let Ok(revision) = crate::config::Config::get_config_modified_time() {
            self.config_reload_tracker.mark_attempted(revision);
            self.config_last_modified = Some(revision);
        }
        self.config_reload_debounce = None;
        self.perform_config_reload(backend)
    }

    /// Fast-path notification used by backends with inotify support. The
    /// periodic poll below uses the same state, preventing duplicate reloads.
    pub(crate) fn observe_config_reload(&mut self, now: Instant, source: &str) {
        let Ok(revision) = crate::config::Config::get_config_modified_time() else {
            // Atomic replacement can briefly make the path unavailable. The
            // next update tick will observe the completed file.
            return;
        };

        if self.config_reload_tracker.observe(revision, now) {
            self.config_last_modified = Some(revision);
            self.config_reload_debounce = Some(now);
            info!("[config] file change detected via {source}; waiting for the revision to settle");
        }
    }

    /// Backend-neutral fallback run from every backend's periodic update.
    pub(crate) fn poll_config_reload(&mut self, backend: &mut dyn Backend, now: Instant) {
        let polled_now = self.config_reload_tracker.should_poll(now);
        if polled_now {
            self.observe_config_reload(now, "mtime poll");
        }

        // Re-stat once at the debounce boundary even within the one-second
        // polling interval. If an atomic-save burst produced a newer revision,
        // observing it here restarts the debounce instead of loading an older
        // revision and then loading the final revision again on the next poll.
        if !polled_now && self.config_reload_tracker.pending_is_due(now) {
            self.observe_config_reload(now, "debounce verification");
        }

        if self.config_reload_tracker.take_due_attempt(now).is_none() {
            return;
        }
        self.config_reload_debounce = None;

        info!("[config] debounced config revision is stable, reloading");
        let response = self.perform_config_reload(backend);
        if response.success {
            info!("[config] reload successful");
        } else {
            warn!("[config] reload failed: {:?}", response.error);
        }
    }

    pub(crate) fn config_reload_next_wakeup(&self, now: Instant) -> Duration {
        self.config_reload_tracker.next_wakeup_in(now)
    }

    pub(crate) fn config_reload_deadline_is_due(&self, now: Instant) -> bool {
        self.config_reload_tracker.deadline_is_due(now)
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

#[cfg(test)]
mod config_reload_tests {
    use super::*;

    fn revision(seconds: u64) -> SystemTime {
        std::time::UNIX_EPOCH + Duration::from_secs(seconds)
    }

    #[test]
    fn unchanged_revision_does_not_schedule_reload() {
        let now = Instant::now();
        let original = revision(1);
        let mut tracker = ConfigReloadTracker::new(Some(original));

        assert!(!tracker.observe(original, now));
        assert_eq!(tracker.take_due_attempt(now + CONFIG_RELOAD_DEBOUNCE), None);
        assert!(tracker.pending.is_none());
    }

    #[test]
    fn mtime_poll_is_gated_until_interval_expires() {
        let now = Instant::now();
        let mut tracker = ConfigReloadTracker::new(Some(revision(1)));
        let just_before_interval = (now + CONFIG_RELOAD_POLL_INTERVAL)
            .checked_sub(Duration::from_millis(1))
            .unwrap();

        assert!(tracker.should_poll(now));
        assert!(!tracker.should_poll(now + Duration::from_millis(250)));
        assert!(!tracker.should_poll(just_before_interval));
        assert!(tracker.should_poll(now + CONFIG_RELOAD_POLL_INTERVAL));
        assert!(!tracker.should_poll(now + CONFIG_RELOAD_POLL_INTERVAL));
    }

    #[test]
    fn poll_deadline_counts_down_without_continuous_ticks() {
        let now = Instant::now();
        let mut tracker = ConfigReloadTracker::new(Some(revision(1)));

        assert_eq!(tracker.next_wakeup_in(now), Duration::ZERO);
        assert!(tracker.deadline_is_due(now));

        assert!(tracker.should_poll(now));
        assert_eq!(
            tracker.next_wakeup_in(now + Duration::from_millis(250)),
            Duration::from_millis(750)
        );
        let just_before_interval = (now + CONFIG_RELOAD_POLL_INTERVAL)
            .checked_sub(Duration::from_millis(1))
            .unwrap();
        assert!(!tracker.deadline_is_due(just_before_interval));
        assert!(tracker.deadline_is_due(now + CONFIG_RELOAD_POLL_INTERVAL));
    }

    #[test]
    fn next_wakeup_uses_earliest_poll_or_debounce_deadline() {
        let now = Instant::now();
        let mut tracker = ConfigReloadTracker::new(Some(revision(1)));
        assert!(tracker.should_poll(now));

        let changed_at = now + Duration::from_millis(800);
        assert!(tracker.observe(revision(2), changed_at));
        // The next mtime poll is 200ms away, earlier than the 300ms debounce.
        assert_eq!(
            tracker.next_wakeup_in(changed_at),
            Duration::from_millis(200)
        );

        assert!(tracker.should_poll(now + CONFIG_RELOAD_POLL_INTERVAL));
        // Once the earlier poll is serviced, the remaining debounce deadline
        // becomes the next wakeup.
        assert_eq!(
            tracker.next_wakeup_in(now + CONFIG_RELOAD_POLL_INTERVAL),
            Duration::from_millis(100)
        );
        assert!(tracker.deadline_is_due(changed_at + CONFIG_RELOAD_DEBOUNCE));
    }

    #[test]
    fn changed_revision_reloads_once_after_debounce() {
        let now = Instant::now();
        let changed = revision(2);
        let mut tracker = ConfigReloadTracker::new(Some(revision(1)));
        let just_before_debounce = (now + CONFIG_RELOAD_DEBOUNCE)
            .checked_sub(Duration::from_millis(1))
            .unwrap();

        assert!(tracker.observe(changed, now));
        assert!(!tracker.observe(changed, now + Duration::from_millis(100)));
        assert_eq!(tracker.take_due_attempt(just_before_debounce), None);
        assert_eq!(
            tracker.take_due_attempt(now + CONFIG_RELOAD_DEBOUNCE),
            Some(changed)
        );

        assert!(!tracker.observe(changed, now + CONFIG_RELOAD_DEBOUNCE));
        assert_eq!(
            tracker.take_due_attempt(now + CONFIG_RELOAD_DEBOUNCE * 2),
            None
        );
    }

    #[test]
    fn newer_revision_restarts_debounce_window() {
        let now = Instant::now();
        let first_change = revision(2);
        let final_change = revision(3);
        let burst_gap = Duration::from_millis(100);
        let mut tracker = ConfigReloadTracker::new(Some(revision(1)));

        assert!(tracker.observe(first_change, now));
        assert!(tracker.observe(final_change, now + burst_gap));
        assert_eq!(tracker.take_due_attempt(now + CONFIG_RELOAD_DEBOUNCE), None);
        assert_eq!(
            tracker.take_due_attempt(now + burst_gap + CONFIG_RELOAD_DEBOUNCE),
            Some(final_change)
        );
    }

    #[test]
    fn failed_attempt_waits_for_next_revision() {
        let now = Instant::now();
        let malformed = revision(2);
        let fixed = revision(3);
        let mut tracker = ConfigReloadTracker::new(Some(revision(1)));

        assert!(tracker.observe(malformed, now));
        // Taking the due revision records the attempt before parsing. Simulate
        // a parse failure by intentionally providing no success callback.
        assert_eq!(
            tracker.take_due_attempt(now + CONFIG_RELOAD_DEBOUNCE),
            Some(malformed)
        );
        assert!(!tracker.observe(malformed, now + CONFIG_RELOAD_DEBOUNCE * 2));
        assert_eq!(
            tracker.take_due_attempt(now + CONFIG_RELOAD_DEBOUNCE * 2),
            None
        );

        assert!(tracker.observe(fixed, now + CONFIG_RELOAD_DEBOUNCE * 2));
        assert_eq!(
            tracker.take_due_attempt(now + CONFIG_RELOAD_DEBOUNCE * 3),
            Some(fixed)
        );
    }
}
