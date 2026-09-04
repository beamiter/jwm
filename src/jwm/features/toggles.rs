//! 特性切换功能
//!
//! 这个模块包含所有窗口管理器特性的切换函数（toggle* 系列）

use crate::backend::api::Backend;
use crate::backend::common_define::{EventMaskBits, Mods, StdCursorKind};
use crate::config::CONFIG;
use crate::core::animation::AnimationKind;
use crate::core::models::ClientKey;
use crate::core::types::Rect;
use crate::jwm::Jwm;
use crate::jwm::features::SystemUiState;
use crate::jwm::features::capture::CaptureTarget;
use crate::jwm::features::expose_plan;
use crate::jwm::types::WMArgEnum;
use log::{error, info, warn};
use std::process::Command;

const RECORDING_PROBE_ATTEMPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const RECORDING_CONCAT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5 * 60);

pub(crate) const fn configured_feature_toggle_allowed(active: bool, enabled: bool) -> bool {
    // Config flags gate entry only. An already-active mode must always retain
    // its exit path so it can release input grabs and compositor state.
    active || enabled
}

/// What a shell panel's opener should do about whatever is already on screen.
///
/// The panels are mutually exclusive and every one of them is bound to a
/// toggle, so one press has to answer two questions at once: is this my own
/// panel, and if not, may I have the screen?
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShellEntry {
    /// Nothing is on screen. Open normally.
    Open,
    /// The caller's own panel is up: the key that opened it takes it down.
    Dismiss,
    /// A different panel is up. Take the screen — and its grabs — over.
    TakeOver,
    /// The lock screen is up. A lock that any panel key could push aside is
    /// not a lock, so the press goes nowhere.
    Refuse,
}

/// How much of the pointer a shell panel's grab must deliver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SystemUiPointerGrab {
    /// Keyboard-only panel: no pointer grab at all.
    None,
    /// Modal clicks: button presses and releases.
    Buttons,
    /// Buttons plus motion, for a panel that follows the pointer. The mask is
    /// the expose grab's (`apply_expose_action`): without POINTER_MOTION an
    /// X11 hover would never reach the WM.
    ButtonsAndMotion,
}

impl SystemUiPointerGrab {
    /// The X11 event mask the grab selects, or `None` when the panel takes no
    /// pointer grab at all.
    fn event_mask(self) -> Option<u32> {
        match self {
            Self::None => None,
            Self::Buttons => {
                Some((EventMaskBits::BUTTON_PRESS | EventMaskBits::BUTTON_RELEASE).bits())
            }
            Self::ButtonsAndMotion => Some(
                (EventMaskBits::BUTTON_PRESS
                    | EventMaskBits::BUTTON_RELEASE
                    | EventMaskBits::POINTER_MOTION)
                    .bits(),
            ),
        }
    }
}

/// The one rule every shell panel key follows.
///
/// `Alt+F10` pressed over `Alt+F9`'s calendar dismisses the calendar and opens
/// the Shell Hub in its place, rather than doing nothing: the panels are one
/// surface with several pages, and a key that silently did nothing read as a
/// dropped keypress.
const fn shell_entry(active: bool, locked: bool, mine: bool) -> ShellEntry {
    if !active {
        ShellEntry::Open
    } else if locked {
        ShellEntry::Refuse
    } else if mine {
        ShellEntry::Dismiss
    } else {
        ShellEntry::TakeOver
    }
}

fn should_start_control_snapshot(
    in_flight: bool,
    refreshed_at: Option<std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    !in_flight
        && crate::jwm::features::system_controls::control_center_snapshot_is_stale(
            refreshed_at,
            now,
        )
}

const fn control_snapshot_epoch_matches(spawn_epoch: u64, current_epoch: u64) -> bool {
    spawn_epoch == current_epoch
}

fn finalize_concat_segments(
    list_path: &std::path::Path,
    list_content: &str,
    output_path: &str,
    segments: &[String],
    run_concat: impl FnOnce(&std::path::Path) -> Result<(), String>,
) -> Result<(), String> {
    std::fs::write(list_path, list_content)
        .map_err(|error| format!("cannot write concat list {}: {error}", list_path.display()))?;
    let result = run_concat(list_path);
    if let Err(error) = std::fs::remove_file(list_path) {
        warn!(
            "[recording] could not remove concat list {}: {error}",
            list_path.display()
        );
    }
    result?;

    for segment in segments {
        if std::path::Path::new(segment) == std::path::Path::new(output_path) {
            continue;
        }
        if let Err(error) = std::fs::remove_file(segment) {
            warn!("[recording] could not remove merged segment {segment}: {error}");
        }
    }
    Ok(())
}

impl Jwm {
    /// Adjust the default sink volume by the binding's Int argument
    /// (percentage points) and show the OSD with the result.
    pub(crate) fn volume_adjust(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let delta = match arg {
            WMArgEnum::Int(delta) if *delta != 0 => *delta,
            _ => 5,
        };
        let Some(state) = crate::jwm::features::system_controls::volume_adjust(delta) else {
            return Err("no working volume control (wpctl/pactl/amixer)".into());
        };
        self.cache_control_volume(state);
        self.show_volume_osd(backend, state);
        Ok(())
    }

    /// Toggle the default sink's mute state and show the OSD with the result.
    pub(crate) fn volume_mute(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(state) = crate::jwm::features::system_controls::volume_toggle_mute() else {
            return Err("no working volume control (wpctl/pactl/amixer)".into());
        };
        self.cache_control_volume(state);
        self.show_volume_osd(backend, state);
        Ok(())
    }

    /// Adjust the backlight by the binding's Int argument (percentage points)
    /// and show the OSD with the result.
    pub(crate) fn brightness_adjust(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let delta = match arg {
            WMArgEnum::Int(delta) if *delta != 0 => *delta,
            _ => 5,
        };
        let Some(percent) = crate::jwm::features::system_controls::brightness_adjust(delta) else {
            return Err("no backlight control (brightnessctl or /sys/class/backlight)".into());
        };
        self.cache_control_brightness(percent);
        backend.compositor_show_osd(crate::backend::api::OsdKind::Brightness, percent);
        Ok(())
    }

    fn show_volume_osd(
        &mut self,
        backend: &mut dyn Backend,
        state: crate::jwm::features::system_controls::AudioState,
    ) {
        let kind = if state.muted {
            crate::backend::api::OsdKind::VolumeMuted
        } else {
            crate::backend::api::OsdKind::Volume
        };
        backend.compositor_show_osd(kind, state.percent);
    }

    /// Toggle playback on the active MPRIS player.
    pub(crate) fn media_play_pause(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.dispatch_media(backend, crate::jwm::features::MediaCommand::PlayPause)
    }

    /// Skip to the next track on the active MPRIS player.
    pub(crate) fn media_next(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.dispatch_media(backend, crate::jwm::features::MediaCommand::Next)
    }

    /// Skip to the previous track on the active MPRIS player.
    pub(crate) fn media_previous(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.dispatch_media(backend, crate::jwm::features::MediaCommand::Previous)
    }

    /// Stop the active MPRIS player.
    pub(crate) fn media_stop(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.dispatch_media(backend, crate::jwm::features::MediaCommand::Stop)
    }

    /// Broadcast a transport request and echo the current track on the OSD, so
    /// a media key gives feedback even before the player answers.
    fn dispatch_media(
        &mut self,
        backend: &mut dyn Backend,
        command: crate::jwm::features::MediaCommand,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.send_media_command(command)?;
        if let Some(state) = self.features.media.get() {
            let label = state.osd_label();
            backend.compositor_show_media_osd(&label);
        }
        Ok(())
    }

    fn build_shell_hub_state(&self) -> crate::jwm::features::SystemUiState {
        let controls = self.features.control_snapshot.as_ref();
        let volume = controls
            .and_then(|snapshot| snapshot.volume)
            .map(|state| (state.percent, state.muted));
        let brightness = controls.and_then(|snapshot| snapshot.brightness);
        let audio_defaults = controls.map(|snapshot| &snapshot.audio_defaults);
        let profiles = controls.and_then(|snapshot| snapshot.power_profiles.as_ref());
        let cfg = CONFIG.load();
        let behavior = cfg.behavior();
        crate::jwm::features::SystemUiState::control_center(
            &crate::jwm::features::ControlCenterInputs {
                shell_hub: true,
                notification_count: self.features.notifications.len(),
                clipboard_count: behavior
                    .clipboard_history
                    .then_some(self.features.clipboard.len()),
                wallpaper: (!behavior.wallpaper.trim().is_empty())
                    .then_some(behavior.wallpaper.as_str()),
                media: self.features.media.get(),
                volume,
                brightness,
                audio_output: audio_defaults.and_then(|defaults| {
                    defaults.name(crate::jwm::features::system_controls::AudioDirection::Output)
                }),
                audio_input: audio_defaults.and_then(|defaults| {
                    defaults.name(crate::jwm::features::system_controls::AudioDirection::Input)
                }),
                battery: self.features.battery.as_ref(),
                resources: behavior.resource_rows.then_some(&self.features.resources),
                network: self.features.connectivity.network.as_ref(),
                bluetooth: Some(&self.features.connectivity.bluetooth),
                power_profile: profiles.map(|(_, active)| active.as_str()),
                night_light: self.night_light_active(),
                do_not_disturb: self.do_not_disturb,
                idle_inhibited: self.idle_inhibited,
            },
        )
    }

    /// Adopt one completed slow-control snapshot without ever waiting for it.
    /// A user mutation increments the epoch, so an older worker result is
    /// discarded instead of rolling the visible value back.
    pub(crate) fn poll_control_snapshot_job(&mut self) {
        let Some((epoch, snapshot)) = self
            .features
            .control_snapshot_job
            .as_ref()
            .and_then(crate::jwm::features::connectivity::BackgroundJob::take)
        else {
            return;
        };
        self.features.control_snapshot_job = None;
        if !control_snapshot_epoch_matches(epoch, self.features.control_snapshot_epoch) {
            return;
        }

        let changed = self.features.control_snapshot.as_ref() != Some(&snapshot);
        self.features.control_snapshot = Some(snapshot);
        self.features.control_snapshot_refreshed_at = Some(std::time::Instant::now());
        if changed {
            self.refresh_open_control_center();
        }
    }

    /// Coalesce a stale-while-revalidate read. Opening and ordinary panel
    /// rebuilds call this freely; at most one external-tool worker is alive.
    pub(crate) fn ensure_control_snapshot_refresh(&mut self, now: std::time::Instant) {
        self.poll_control_snapshot_job();
        if !should_start_control_snapshot(
            self.features.control_snapshot_job.is_some(),
            self.features.control_snapshot_refreshed_at,
            now,
        ) {
            return;
        }
        let epoch = self.features.control_snapshot_epoch;
        let job = crate::jwm::features::connectivity::BackgroundJob::spawn(move || {
            (
                epoch,
                crate::jwm::features::system_controls::ControlCenterSnapshot::read(),
            )
        });
        self.features.control_snapshot_job = Some(self.track_background_job(job));
    }

    fn mutate_control_snapshot(
        &mut self,
        update: impl FnOnce(&mut crate::jwm::features::system_controls::ControlCenterSnapshot),
    ) {
        self.features.control_snapshot_epoch = self.features.control_snapshot_epoch.wrapping_add(1);
        update(
            self.features
                .control_snapshot
                .get_or_insert_with(Default::default),
        );
    }

    pub(crate) fn cache_control_volume(
        &mut self,
        state: crate::jwm::features::system_controls::AudioState,
    ) {
        self.mutate_control_snapshot(|snapshot| snapshot.volume = Some(state));
    }

    pub(crate) fn cache_control_brightness(&mut self, percent: u8) {
        self.mutate_control_snapshot(|snapshot| snapshot.brightness = Some(percent));
    }

    pub(crate) fn cache_control_audio_defaults(
        &mut self,
        defaults: crate::jwm::features::system_controls::AudioDefaults,
    ) {
        self.mutate_control_snapshot(|snapshot| snapshot.audio_defaults = defaults);
    }

    pub(crate) fn cache_control_power_profiles(&mut self, available: Vec<String>, active: String) {
        self.mutate_control_snapshot(|snapshot| {
            snapshot.power_profiles = Some((available, active));
        });
    }

    fn wallpaper_picker_state() -> crate::jwm::features::SystemUiState {
        use crate::jwm::features::wallpaper;

        let (current, directory) = {
            let cfg = CONFIG.load();
            let behavior = cfg.behavior();
            let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/"));
            (
                behavior.wallpaper.clone(),
                wallpaper::resolve_directory(&behavior.wallpaper_dir, &behavior.wallpaper, &home),
            )
        };
        let paths = wallpaper::list_wallpapers(&directory);
        crate::jwm::features::SystemUiState::wallpaper_picker(
            &paths,
            &current,
            &directory.to_string_lossy(),
        )
    }

    /// Adopt a completed application scan into the long-lived cache. If the
    /// launcher is currently visible, replace its immutable snapshot and let
    /// the frame tick redraw the current query once.
    pub(crate) fn poll_launcher_catalog_job(&mut self) {
        let Some(entries) = self
            .features
            .launcher_catalog_job
            .as_ref()
            .and_then(crate::jwm::features::connectivity::BackgroundJob::take)
        else {
            return;
        };

        self.features.launcher_catalog_job = None;
        self.features.launcher_catalog = entries;
        self.features.launcher_catalog_refreshed_at = Some(std::time::Instant::now());
        if self
            .features
            .system_ui
            .set_launcher_entries(std::sync::Arc::clone(&self.features.launcher_catalog))
        {
            self.mark_system_ui_dirty();
        }
    }

    /// Build a launcher panel from the last complete snapshot and kick off a
    /// stale-while-revalidate scan when necessary. No directory traversal or
    /// PATH inspection happens on this event-loop path.
    fn cached_launcher_state(&mut self) -> SystemUiState {
        // A worker may have finished between frame ticks. Taking its result is
        // non-blocking and avoids showing the indexing row for an extra frame.
        self.poll_launcher_catalog_job();

        let now = std::time::Instant::now();
        if self.features.launcher_catalog_job.is_none()
            && crate::jwm::features::system_ui::application_catalog_is_stale(
                self.features.launcher_catalog_refreshed_at,
                now,
            )
        {
            let job = crate::jwm::features::system_ui::start_application_discovery();
            self.features.launcher_catalog_job = Some(self.track_background_job(job));
        }

        let indexing = self.features.launcher_catalog.is_empty()
            && self.features.launcher_catalog_job.is_some();
        SystemUiState::open_launcher(
            std::sync::Arc::clone(&self.features.launcher_catalog),
            self.launcher_window_snapshot(),
            indexing,
        )
    }

    /// Swap from the shell home page to one of its native child pages while
    /// retaining the keyboard/pointer grabs. Escape returns to the hub.
    pub(crate) fn open_shell_hub_route(
        &mut self,
        backend: &mut dyn Backend,
        route: crate::jwm::features::ShellHubRoute,
    ) -> Result<(), Box<dyn std::error::Error>> {
        use crate::jwm::features::ShellHubRoute;

        let next = match route {
            ShellHubRoute::Applications => self.cached_launcher_state(),
            ShellHubRoute::Notifications => SystemUiState::notification_center(
                &self.features.notifications,
                crate::jwm::features::notifications::now_unix_ms(),
            ),
            ShellHubRoute::Clipboard => {
                if !CONFIG.load().behavior().clipboard_history {
                    return Err("clipboard history is disabled (behavior.clipboard_history)".into());
                }
                SystemUiState::clipboard_picker(&self.features.clipboard)
            }
            ShellHubRoute::Calendar => SystemUiState::calendar(chrono::Local::now().naive_local()),
            ShellHubRoute::Wallpaper => Self::wallpaper_picker_state(),
        };

        self.features.system_ui_return_to_hub = true;
        self.features.system_ui = next;
        self.sync_system_ui(backend);
        Ok(())
    }

    /// Open the shell from a status bar rather than from a key binding.
    ///
    /// `None` opens the hub home page; a route opens that page directly with
    /// Escape returning to the hub, matching what the keyboard path does. This
    /// is the only shell entry point that starts from an unfocused surface, so
    /// unlike [`Self::open_shell_hub_route`] it has to acquire the grabs first
    /// and hand them back if the requested page turns out to be unavailable.
    pub(crate) fn open_shell_from_status_bar(
        &mut self,
        backend: &mut dyn Backend,
        route: Option<crate::jwm::features::ShellHubRoute>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.begin_shell_from_status_bar(backend, route)? {
            return Ok(());
        }

        // The bar still owns the pointer, because the click that asked for
        // this is still going on. Park the request and let the event loop
        // retry it — see `features::deferred_grab`.
        self.features.deferred_grab = Some(crate::jwm::features::DeferredGrab::new(
            crate::jwm::features::DeferredGrabAction::ShellHub { route },
            std::time::Instant::now(),
        ));
        Ok(())
    }

    /// Open the shell, reporting `Ok(false)` when the pointer is not free
    /// *yet* — a retryable condition, distinct from the hard errors.
    pub(crate) fn begin_shell_from_status_bar(
        &mut self,
        backend: &mut dyn Backend,
        route: Option<crate::jwm::features::ShellHubRoute>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        if self.features.system_ui.is_active() {
            // The user is already in the shell. Re-opening would steal the
            // grabs and throw away the page they are on, so a stray click on
            // the bar does nothing instead.
            return Ok(true);
        }

        // A status bar click holds an implicit pointer grab for as long as the
        // button is down, so a busy pointer here means "the click that asked
        // for this is still going on" — retryable, not a failure. This is the
        // only caller that wants that distinction; the other fourteen are
        // keyboard-invoked, where a busy pointer really is an error.
        let label = route.map_or("shell hub (status bar)", |_| "shell page (status bar)");
        if !self.prepare_system_ui_deferrable(backend, label)? {
            return Ok(false);
        }
        let Some(route) = route else {
            // Same as the key-bound control center: open on the cached
            // connectivity reading and let the background re-read update the
            // rows in place, because nmcli can block for seconds.
            self.ensure_control_snapshot_refresh(std::time::Instant::now());
            self.ensure_connectivity_refresh();
            self.features.system_ui_return_to_hub = false;
            self.features.system_ui = self.build_shell_hub_state();
            self.sync_system_ui(backend);
            return Ok(true);
        };

        self.open_shell_hub_route(backend, route)
            .inspect_err(|_| {
                // A disabled route (clipboard history switched off, say) must
                // not leave the keyboard grabbed with nothing on screen.
                self.close_system_ui(backend);
            })
            .map(|()| true)
    }

    /// Rebuild the shell home page after leaving a child page. This path does
    /// not reacquire grabs or toggle the compositor.
    pub(crate) fn return_to_shell_hub(&mut self, backend: &mut dyn Backend) {
        // Leaving the Bluetooth picker for the hub abandons any live pairing,
        // the same as closing the panel would.
        self.cancel_bluetooth_pairing();
        self.ensure_control_snapshot_refresh(std::time::Instant::now());
        self.ensure_connectivity_refresh();
        self.features.system_ui_return_to_hub = false;
        self.features.system_ui = self.build_shell_hub_state();
        self.sync_system_ui(backend);
    }

    /// Open the Quickshell-inspired Shell Hub: native routes, live badges,
    /// grouped quick settings and system status in one keyboard-driven surface.
    pub(crate) fn control_center(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // A child route reached from the hub is still the shell the key
        // opened, so the same key closes the whole stack from any page.
        let in_shell = self.features.system_ui_return_to_hub;
        if self.toggle_off_system_ui(backend, |state| in_shell || state.is_control_center()) {
            return Ok(());
        }
        self.prepare_system_ui(backend, "control center", SystemUiPointerGrab::Buttons)?;
        self.ensure_control_snapshot_refresh(std::time::Instant::now());
        // Open with the cached connectivity reading — read_state() shells out
        // to nmcli and can block for seconds — and re-read in the background;
        // the rows update in place once the fresh state is adopted.
        self.ensure_connectivity_refresh();
        self.features.system_ui_return_to_hub = false;
        self.features.system_ui = self.build_shell_hub_state();
        self.sync_system_ui(backend);
        Ok(())
    }

    /// Rebuild an open control center entirely from cached state, preserving
    /// the selected action even when async hardware rows were inserted or
    /// removed.
    pub(crate) fn refresh_open_control_center(&mut self) {
        if !matches!(
            self.features.system_ui,
            crate::jwm::features::SystemUiState::ControlCenter { .. }
        ) {
            return;
        }
        let selected_kind = self.features.system_ui.selected_control();
        let selected = match &self.features.system_ui {
            crate::jwm::features::SystemUiState::ControlCenter { selected, .. } => *selected,
            _ => 0,
        };
        let mut rebuilt = self.build_shell_hub_state();
        rebuilt.restore_control_selection_kind(selected_kind, selected);
        self.features.system_ui = rebuilt;
        // Rebuilt in memory only. Half this function's callers — a
        // connectivity re-read, a battery poll — have no backend to push
        // with, so the frame tick does it.
        self.mark_system_ui_dirty();
    }

    /// Open the session menu: lock, suspend, hibernate, log out, restart,
    /// shut down. Destructive rows need a second Enter to confirm.
    pub(crate) fn session_menu(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.toggle_off_system_ui(backend, SystemUiState::is_session_menu) {
            return Ok(());
        }
        self.prepare_system_ui(backend, "session menu", SystemUiPointerGrab::Buttons)?;
        self.features.system_ui = crate::jwm::features::SystemUiState::session_menu();
        self.sync_system_ui(backend);
        Ok(())
    }

    /// Run one session action. Lock swaps in the lock overlay; log out quits
    /// the window manager; the rest run their configured command.
    pub(crate) fn run_session_action(
        &mut self,
        backend: &mut dyn Backend,
        action: crate::jwm::features::SessionAction,
    ) -> Result<(), Box<dyn std::error::Error>> {
        use crate::jwm::features::SessionAction;

        let cfg = CONFIG.load();
        let command = match action {
            SessionAction::Lock => {
                // Keep the grabs: the lock overlay wants them anyway.
                // A lock is terminal, never a page the Hub can be backed out
                // to, so the Escape target goes with the panel it belonged to.
                self.features.system_ui_return_to_hub = false;
                self.features.system_ui = crate::jwm::features::SystemUiState::lock();
                self.sync_system_ui(backend);
                return Ok(());
            }
            SessionAction::Logout => {
                self.close_system_ui(backend);
                info!("Session menu: logging out");
                self.quit(backend, &WMArgEnum::Int(0))?;
                return Ok(());
            }
            SessionAction::Suspend => cfg.behavior().suspend_command.clone(),
            SessionAction::Hibernate => cfg.behavior().hibernate_command.clone(),
            SessionAction::Reboot => cfg.behavior().reboot_command.clone(),
            SessionAction::Shutdown => cfg.behavior().shutdown_command.clone(),
        };

        let Some((program, args)) = crate::jwm::features::session::split_command(&command) else {
            return Err(format!("no command configured for {}", action.as_str()).into());
        };
        // Close the panel first: suspend hands control to logind, and coming
        // back to a stale menu would be confusing.
        self.close_system_ui(backend);
        info!("Session menu: {} -> {command}", action.as_str());
        match Command::new(&program).args(&args).spawn() {
            Ok(child) => {
                self.supervise_transient_child(child);
                Ok(())
            }
            Err(error) => Err(format!("could not run {command:?}: {error}").into()),
        }
    }

    /// Open the audio output picker.
    pub(crate) fn audio_output_picker(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.open_audio_picker(
            backend,
            crate::jwm::features::system_controls::AudioDirection::Output,
        )
    }

    /// Open the audio input picker.
    pub(crate) fn audio_input_picker(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.open_audio_picker(
            backend,
            crate::jwm::features::system_controls::AudioDirection::Input,
        )
    }

    /// Shared entry point for both audio pickers.
    ///
    /// Listing devices is a local socket round-trip, unlike a Wi-Fi scan, so
    /// it happens inline and the panel opens already filled.
    pub(crate) fn open_audio_picker(
        &mut self,
        backend: &mut dyn Backend,
        direction: crate::jwm::features::system_controls::AudioDirection,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.toggle_off_system_ui(backend, |state| {
            state.audio_picker_direction() == Some(direction)
        }) {
            return Ok(());
        }
        let devices = crate::jwm::features::system_controls::audio_devices(direction);
        if devices.is_empty() {
            return Err(format!(
                "no sound server that can switch audio {} devices",
                direction.label()
            )
            .into());
        }
        self.prepare_system_ui(
            backend,
            "the audio device picker",
            SystemUiPointerGrab::Buttons,
        )?;
        self.features.system_ui =
            crate::jwm::features::SystemUiState::audio_picker(direction, &devices);
        self.sync_system_ui(backend);
        Ok(())
    }

    /// Make the selected device the default, then re-read so the marker
    /// shows what actually took effect rather than what was asked for.
    pub(crate) fn use_selected_audio_device(&mut self, backend: &mut dyn Backend) {
        use crate::jwm::features::system_controls;

        let Some(direction) = self.features.system_ui.audio_picker_direction() else {
            return;
        };
        let Some(id) = self.features.system_ui.selected_audio_device() else {
            return;
        };
        let asked = system_controls::set_audio_device(direction, &id);
        // The tool's exit code is not evidence: a sound server routinely
        // accepts the request and then puts the default back, because the
        // device is not actually available — an HDMI output with no monitor,
        // a headset microphone with no headset. Believe the re-read.
        let inventory = system_controls::audio_inventory();
        let devices = inventory.devices(direction);
        let took = devices
            .iter()
            .any(|device| device.id == id && device.is_default);
        self.features
            .system_ui
            .set_audio_devices(direction, devices);
        let defaults = inventory.defaults();
        let in_use = defaults.name(direction).map(str::to_string);
        self.cache_control_audio_defaults(defaults);
        let message = match (took, in_use) {
            (true, Some(name)) => {
                log::info!("audio: {} is now {name}", direction.label());
                format!("Using {name}")
            }
            (false, Some(name)) => {
                log::warn!(
                    "audio: {} stayed on {name} after asking for {id}",
                    direction.label()
                );
                format!("Unavailable \u{2014} still using {name}")
            }
            (_, None) if asked => "Switched, but nothing reports as default".to_string(),
            (_, None) => "Could not switch device".to_string(),
        };
        self.features
            .system_ui
            .set_audio_message(direction, message);
        self.sync_system_ui(backend);
    }

    /// Open the Wi-Fi picker and start a scan on a worker thread.
    pub(crate) fn wifi_picker(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.toggle_off_system_ui(backend, SystemUiState::is_wifi_picker) {
            return Ok(());
        }
        let Some(scan) = crate::jwm::features::connectivity::start_scan() else {
            return Err("no NetworkManager to scan with (nmcli not available)".into());
        };
        self.prepare_system_ui(backend, "the Wi-Fi picker", SystemUiPointerGrab::Buttons)?;
        self.features.wifi_scan = Some(self.track_background_job(scan));
        self.features.system_ui =
            crate::jwm::features::SystemUiState::wifi_picker("Scanning\u{2026}");
        self.sync_system_ui(backend);
        Ok(())
    }

    /// Open the wallpaper picker on the configured (or inferred) directory.
    pub(crate) fn wallpaper_picker(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.toggle_off_system_ui(backend, SystemUiState::is_wallpaper_picker) {
            return Ok(());
        }
        let next = Self::wallpaper_picker_state();
        self.prepare_system_ui(
            backend,
            "the wallpaper picker",
            SystemUiPointerGrab::Buttons,
        )?;
        self.features.system_ui_return_to_hub = false;
        self.features.system_ui = next;
        self.sync_system_ui(backend);
        Ok(())
    }

    /// Apply the selected wallpaper through the same configuration path a
    /// `set_config` takes, so both compositors pick it up on their next
    /// `apply_config` exactly as they would from a reload.
    pub(crate) fn apply_selected_wallpaper(&mut self, backend: &mut dyn Backend) {
        let Some(path) = self
            .features
            .system_ui
            .selected_wallpaper()
            .map(str::to_string)
        else {
            return;
        };
        let mut updated = (**CONFIG.load()).clone();
        if let Err(error) = updated.set_value(
            "behavior.wallpaper",
            &serde_json::Value::String(path.clone()),
        ) {
            error!("Wallpaper: {error}");
            return;
        }
        CONFIG.store(std::sync::Arc::new(updated));
        self.apply_config_changes(backend);
        info!("Wallpaper: {path}");
        self.broadcast_ipc_event(
            "config/changed",
            serde_json::json!({ "key": "behavior.wallpaper", "value": path }),
        );
        self.close_system_ui(backend);
    }

    /// Open the clipboard picker.
    pub(crate) fn clipboard_picker(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.toggle_off_system_ui(backend, SystemUiState::is_clipboard_picker) {
            return Ok(());
        }
        if !CONFIG.load().behavior().clipboard_history {
            return Err("clipboard history is disabled (behavior.clipboard_history)".into());
        }
        self.prepare_system_ui(
            backend,
            "the clipboard picker",
            SystemUiPointerGrab::Buttons,
        )?;
        self.features.system_ui =
            crate::jwm::features::SystemUiState::clipboard_picker(&self.features.clipboard);
        self.sync_system_ui(backend);
        Ok(())
    }

    /// Put the selected entry back on the clipboard and close the picker.
    pub(crate) fn copy_selected_clipboard(&mut self, backend: &mut dyn Backend) {
        let Some(index) = self.features.system_ui.selected_clipboard() else {
            return;
        };
        let Some(text) = self
            .features
            .clipboard
            .get(index)
            .map(|entry| entry.text.clone())
        else {
            return;
        };
        if backend.set_clipboard_text(&text) {
            // Copying an old entry makes it the most recent one, exactly as
            // if the user had copied it again from the source.
            self.record_clipboard(&text);
            self.close_system_ui(backend);
        } else {
            self.features
                .system_ui
                .set_clipboard_message("this backend cannot set the clipboard");
            self.sync_system_ui(backend);
        }
    }

    /// Forget the selected entry.
    pub(crate) fn forget_selected_clipboard(&mut self, backend: &mut dyn Backend) {
        let Some(index) = self.features.system_ui.selected_clipboard() else {
            return;
        };
        if self.features.clipboard.remove(index) {
            self.features
                .system_ui
                .refresh_clipboard(&self.features.clipboard);
            self.sync_system_ui(backend);
        }
    }

    /// Open the calendar card on the current month.
    pub(crate) fn calendar(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.toggle_off_system_ui(backend, SystemUiState::is_calendar) {
            return Ok(());
        }
        self.prepare_system_ui(backend, "the calendar", SystemUiPointerGrab::Buttons)?;
        self.features.system_ui =
            crate::jwm::features::SystemUiState::calendar(chrono::Local::now().naive_local());
        self.sync_system_ui(backend);
        Ok(())
    }

    /// Open the Bluetooth picker and read the device list on a worker thread.
    pub(crate) fn bluetooth_picker(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.toggle_off_system_ui(backend, SystemUiState::is_bluetooth_picker) {
            return Ok(());
        }
        let Some(scan) = crate::jwm::features::connectivity::start_device_scan() else {
            return Err("no bluetoothctl to list devices with".into());
        };
        self.prepare_system_ui(
            backend,
            "the Bluetooth picker",
            SystemUiPointerGrab::Buttons,
        )?;
        self.features.bluetooth_scan = Some(self.track_background_job(scan));
        self.features.system_ui =
            crate::jwm::features::SystemUiState::bluetooth_picker("Reading devices\u{2026}");
        self.sync_system_ui(backend);
        Ok(())
    }

    /// Adopt a finished device list or connect/disconnect attempt.
    pub(crate) fn poll_bluetooth_jobs(&mut self, backend: &mut dyn Backend) {
        if !self.features.system_ui.is_bluetooth_picker() {
            self.features.bluetooth_scan = None;
            self.features.bluetooth_action = None;
            return;
        }
        let mut changed = false;

        // Pairing deadlines, enforced from the frame tick. The helper runs its
        // own clocks too; these keep the panel honest when the helper is slow
        // or gone. (`maintenance_next_wakeup_at` wakes the loop for them.)
        let now = std::time::Instant::now();
        if let Some(session) = &self.features.bluetooth_pairing {
            if session.session_timed_out(now) {
                // The helper vanished without a `done`: end the session.
                log::warn!("Bluetooth: pairing helper never reported back");
                self.cancel_bluetooth_pairing();
                self.features
                    .system_ui
                    .set_bluetooth_message("Pairing timed out");
                changed = true;
            } else if session.prompt_timed_out(now) {
                // An unanswered prompt: withdraw it and cancel the helper's
                // outstanding request. The session stays until the helper's
                // `done` lands (its request failure unwinds Pair promptly).
                let cookie = session.cookie().to_string();
                let request_id = session.request_id();
                if let Some(session) = &mut self.features.bluetooth_pairing {
                    session.clear_prompt();
                }
                self.features.system_ui.cancel_pairing_prompt();
                self.broadcast_ipc_event(
                    crate::jwm::features::pairing::RESPONSE_EVENT,
                    crate::jwm::features::pairing::response_payload(
                        &cookie,
                        request_id,
                        crate::jwm::features::pairing::PairingAnswer::Cancelled,
                    ),
                );
                self.features
                    .system_ui
                    .set_bluetooth_message("Pairing timed out");
                changed = true;
            }
        }

        if let Some(devices) = self
            .features
            .bluetooth_scan
            .as_ref()
            .and_then(crate::jwm::features::connectivity::BackgroundJob::take)
        {
            self.features.bluetooth_scan = None;
            self.features.system_ui.set_bluetooth_devices(&devices);
            changed = true;
        }

        if let Some(result) = self
            .features
            .bluetooth_action
            .as_ref()
            .and_then(crate::jwm::features::connectivity::BackgroundJob::take)
        {
            self.features.bluetooth_action = None;
            match result {
                Ok(address) => {
                    log::info!("Bluetooth: {address} done");
                    // Re-read so the row shows the state that actually took.
                    if let Some(scan) = crate::jwm::features::connectivity::start_device_scan() {
                        self.features.bluetooth_scan = Some(self.track_background_job(scan));
                    }
                    self.refresh_connectivity();
                }
                Err(error) => {
                    log::warn!("Bluetooth: {error}");
                    self.features.system_ui.set_bluetooth_message(error);
                }
            }
            changed = true;
        }

        if changed {
            self.sync_system_ui(backend);
        }
    }

    /// Connect, disconnect, or pair the selected device.
    pub(crate) fn activate_selected_bluetooth(&mut self, backend: &mut dyn Backend) {
        let Some((address, name, action)) = self.features.system_ui.selected_bluetooth() else {
            return;
        };
        if action == "pair" {
            self.start_bluetooth_pairing(backend, &address, &name);
            return;
        }
        self.features
            .system_ui
            .set_bluetooth_message(format!("{action}ing\u{2026}"));
        let job = crate::jwm::features::connectivity::start_device_action(&address, action);
        self.features.bluetooth_action = Some(self.track_background_job(job));
        self.sync_system_ui(backend);
    }

    /// Start a pairing session: mint the cookie, spawn the one-shot
    /// `jwm-bridge pair` helper, and park the session record so the helper's
    /// prompt/done commands can be matched to it. The helper speaks to bluez;
    /// this side only renders its questions and returns the user's answers.
    fn start_bluetooth_pairing(&mut self, backend: &mut dyn Backend, address: &str, name: &str) {
        use crate::jwm::features::pairing;

        if self.features.bluetooth_pairing.is_some() {
            self.features
                .system_ui
                .set_bluetooth_message("A pairing is already running");
            self.sync_system_ui(backend);
            return;
        }
        let cookie = pairing::new_cookie();
        let Some(session) =
            pairing::PairingSession::new(address, name, cookie.clone(), std::time::Instant::now())
        else {
            self.features
                .system_ui
                .set_bluetooth_message("Not a Bluetooth address");
            self.sync_system_ui(backend);
            return;
        };
        // The cookie authorizes prompt answers; hand it over through the
        // environment, not argv, which `ps` exposes on machines without
        // hidepid.
        let spawn = crate::jwm::features::external_command::spawn_detached(
            "jwm-bridge",
            &["pair", address],
            &[("JWM_PAIRING_COOKIE", cookie.as_str())],
        );
        match spawn {
            Ok(child) => {
                self.supervise_transient_child(child);
                log::info!("Bluetooth: pairing with {address} started");
                self.features.bluetooth_pairing = Some(session);
                self.features
                    .system_ui
                    .set_bluetooth_message(format!("Pairing with {name}\u{2026}"));
            }
            Err(error) => {
                log::warn!("Bluetooth: could not start jwm-bridge: {error}");
                self.features
                    .system_ui
                    .set_bluetooth_message("jwm-bridge is not installed");
            }
        }
        self.sync_system_ui(backend);
    }

    /// Arm a bounded window in which an incoming Bluetooth request may be
    /// accepted, by spawning the one-shot `jwm-bridge accept` helper.
    ///
    /// This is deliberately an explicit gesture with no persistent form.
    /// Without it there is no agent of ours registered, BlueZ answers an
    /// inbound `RequestAuthorization`/`AuthorizeService` by refusing it, and
    /// the controller is neither pairable nor discoverable — which is the
    /// safe resting state and stays the default. The helper holds the window
    /// open for as long as jwm's session record lives and puts everything
    /// back on the way out.
    pub(crate) fn arm_bluetooth_inbound_authorization(&mut self, backend: &mut dyn Backend) {
        use crate::jwm::features::pairing;

        if self.features.bluetooth_pairing.is_some() {
            // One session at a time, in either direction: an inbound window
            // must never displace or race a pairing the user started.
            self.features
                .system_ui
                .set_bluetooth_message("A Bluetooth session is already running");
            self.sync_system_ui(backend);
            return;
        }
        let cookie = pairing::new_cookie();
        let spawn = crate::jwm::features::external_command::spawn_detached(
            "jwm-bridge",
            &["accept"],
            &[("JWM_PAIRING_COOKIE", cookie.as_str())],
        );
        match spawn {
            Ok(child) => {
                self.supervise_transient_child(child);
                log::info!("Bluetooth: accepting incoming requests for one window");
                self.features.bluetooth_pairing = Some(pairing::PairingSession::inbound(
                    cookie,
                    std::time::Instant::now(),
                ));
                let seconds = pairing::INBOUND_WINDOW.as_secs();
                self.features
                    .system_ui
                    .set_bluetooth_message(format!("Accepting incoming requests ({seconds}s)"));
            }
            Err(error) => {
                log::warn!("Bluetooth: could not start jwm-bridge: {error}");
                self.features
                    .system_ui
                    .set_bluetooth_message("jwm-bridge is not installed");
            }
        }
        self.sync_system_ui(backend);
    }

    /// Hand the user's typed PIN to the helper and drop our copy.
    pub(crate) fn submit_bluetooth_pin(&mut self, backend: &mut dyn Backend) {
        use crate::jwm::features::pairing;

        let Some(valid) = self
            .features
            .system_ui
            .pairing_pin()
            .map(pairing::valid_pin)
        else {
            return;
        };
        if !valid {
            self.features
                .system_ui
                .set_bluetooth_message("A PIN is 1-16 characters");
            self.sync_system_ui(backend);
            return;
        }
        let Some(session) = &mut self.features.bluetooth_pairing else {
            return;
        };
        if !matches!(session.phase(), pairing::PairingPhase::AwaitingPin) {
            return;
        }
        let cookie = session.cookie().to_string();
        let request_id = session.request_id();
        // The take hands the only copy over; wipe it once the broadcast is
        // out so the PIN does not linger in a freed allocation.
        let Some(mut pin) = self.features.system_ui.take_pairing_pin() else {
            return;
        };
        if let Some(session) = &mut self.features.bluetooth_pairing {
            session.clear_prompt();
        }
        self.broadcast_ipc_event(
            pairing::RESPONSE_EVENT,
            pairing::response_payload(&cookie, request_id, pairing::PairingAnswer::Pin(&pin)),
        );
        unsafe { pin.as_bytes_mut().fill(0) };
        self.features
            .system_ui
            .set_bluetooth_message("Pairing\u{2026}");
        self.sync_system_ui(backend);
    }

    /// Answer a numeric-comparison prompt: `y`/Enter confirms, `n` rejects.
    pub(crate) fn answer_bluetooth_confirm(&mut self, backend: &mut dyn Backend, accepted: bool) {
        use crate::jwm::features::pairing;

        let Some(session) = &mut self.features.bluetooth_pairing else {
            return;
        };
        // The user must be answering the question that was actually asked:
        // both yes/no phases route here, and nothing else may.
        let authorizing = match session.phase() {
            pairing::PairingPhase::AwaitingConfirm { .. } => false,
            pairing::PairingPhase::AwaitingAuthorization { .. } => true,
            _ => return,
        };
        let cookie = session.cookie().to_string();
        let request_id = session.request_id();
        session.clear_prompt();
        self.features.system_ui.cancel_pairing_prompt();
        self.broadcast_ipc_event(
            pairing::RESPONSE_EVENT,
            pairing::response_payload(
                &cookie,
                request_id,
                if accepted {
                    pairing::PairingAnswer::Confirmed
                } else {
                    pairing::PairingAnswer::Rejected
                },
            ),
        );
        self.features
            .system_ui
            .set_bluetooth_message(match (authorizing, accepted) {
                (true, true) => "Allowed",
                (true, false) => "Refused",
                (false, true) => "Pairing\u{2026}",
                (false, false) => "Passkey rejected",
            });
        self.sync_system_ui(backend);
    }

    /// Cancel any live pairing session — outbound or an armed inbound window
    /// — by telling the helper (its one outstanding bluez request fails, or
    /// Pair is cancelled outright), wiping any prompt, and dropping the
    /// session record. Closing, handing over, or timing out the picker all
    /// funnel here: a session must never outlive its panel, and an inbound
    /// window in particular must not keep the controller discoverable after
    /// the user has moved on.
    pub(crate) fn cancel_bluetooth_pairing(&mut self) {
        use crate::jwm::features::pairing;

        let Some(session) = self.features.bluetooth_pairing.take() else {
            self.features.system_ui.cancel_pairing_prompt();
            return;
        };
        let inbound = session.kind() == pairing::PairingKind::Inbound;
        log::info!(
            "Bluetooth: {} session with {} cancelled",
            session.kind().as_str(),
            session.address().unwrap_or("no device yet"),
        );
        self.broadcast_ipc_event(
            pairing::RESPONSE_EVENT,
            // The prompt on screen, when there is one: a cancel that answers
            // an outstanding request must name it, and one with nothing on
            // screen answers nothing and becomes a `CancelPairing` instead.
            pairing::response_payload(
                session.cookie(),
                session.request_id(),
                pairing::PairingAnswer::SessionClosed,
            ),
        );
        self.features.system_ui.cancel_pairing_prompt();
        self.features.system_ui.set_bluetooth_message(if inbound {
            "Not accepting incoming requests"
        } else {
            "Pairing cancelled"
        });
    }

    /// Adopt a finished scan or connection attempt. Called from the frame
    /// tick; does nothing unless the picker is open with work outstanding.
    pub(crate) fn poll_wifi_jobs(&mut self, backend: &mut dyn Backend) {
        if !self.features.system_ui.is_wifi_picker() {
            // The panel was closed while the work was still running; drop the
            // handles so a later picker does not adopt a stale result.
            self.features.wifi_scan = None;
            self.features.wifi_connect = None;
            return;
        }
        let mut changed = false;

        if let Some(networks) = self
            .features
            .wifi_scan
            .as_ref()
            .and_then(crate::jwm::features::connectivity::BackgroundJob::take)
        {
            self.features.wifi_scan = None;
            self.features.system_ui.set_wifi_networks(&networks);
            changed = true;
        }

        if let Some(result) = self
            .features
            .wifi_connect
            .as_ref()
            .and_then(crate::jwm::features::connectivity::BackgroundJob::take)
        {
            self.features.wifi_connect = None;
            match result {
                Ok(ssid) => {
                    log::info!("Wi-Fi: joined {ssid}");
                    self.refresh_connectivity();
                    self.close_system_ui(backend);
                    return;
                }
                Err(error) => {
                    log::warn!("Wi-Fi: {error}");
                    self.features.system_ui.set_wifi_message(error);
                    changed = true;
                }
            }
        }

        if changed {
            self.sync_system_ui(backend);
        }
    }

    /// Act on the selected network: join it, or ask for its passphrase first.
    pub(crate) fn join_selected_wifi(&mut self, backend: &mut dyn Backend) {
        use crate::jwm::features::connectivity::{self, ConnectPlan};

        let Some((ssid, secured)) = self.features.system_ui.selected_wifi() else {
            return;
        };
        let mut passphrase = self.features.system_ui.take_wifi_passphrase();

        // `plan_connect` only needs to know whether the network is secured.
        let network = connectivity::WifiNetwork {
            ssid: ssid.clone(),
            signal: 0,
            security: if secured {
                "WPA2".to_string()
            } else {
                String::new()
            },
            in_use: false,
        };
        let saved = connectivity::has_saved_profile(&ssid);
        let plan = connectivity::plan_connect(&network, saved, passphrase.as_deref());

        if plan == ConnectPlan::NeedsPassphrase {
            self.features.system_ui.prompt_wifi_passphrase();
            self.sync_system_ui(backend);
            return;
        }

        self.features
            .system_ui
            .set_wifi_message(format!("Connecting to {ssid}\u{2026}"));
        let job = connectivity::start_connect(&ssid, &plan, passphrase.clone());
        self.features.wifi_connect = Some(self.track_background_job(job));
        if let Some(secret) = passphrase.as_mut() {
            // The worker owns its own copy; wipe ours rather than dropping it.
            unsafe { secret.as_bytes_mut().fill(0) };
        }
        self.sync_system_ui(backend);
    }

    /// Toggle the Wi-Fi radio.
    pub(crate) fn toggle_wifi(
        &mut self,
        _backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let enabled = !self
            .features
            .connectivity
            .network
            .as_ref()
            .is_some_and(|state| state.wifi_enabled);
        if !crate::jwm::features::connectivity::set_wifi(enabled) {
            return Err("no working Wi-Fi control (nmcli or rfkill)".into());
        }
        self.refresh_connectivity();
        Ok(())
    }

    /// Toggle the Bluetooth controller.
    pub(crate) fn toggle_bluetooth(
        &mut self,
        _backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let enabled = !self.features.connectivity.bluetooth.powered;
        if !crate::jwm::features::connectivity::set_bluetooth(enabled) {
            return Err("no working Bluetooth control (bluetoothctl or rfkill)".into());
        }
        self.refresh_connectivity();
        Ok(())
    }

    /// Toggle night light on top of its schedule. The override sticks until
    /// toggled back, so a user who wants warmth at noon gets it.
    pub(crate) fn toggle_night_light(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let enabled = !self.night_light_active();
        self.set_night_light_override(backend, enabled);
        Ok(())
    }

    /// Whether the screen is currently warmed, by schedule or by override.
    pub(crate) fn night_light_active(&self) -> bool {
        if let Some(forced) = self.night_light_override {
            return forced;
        }
        let cfg = CONFIG.load();
        let behavior = cfg.behavior();
        behavior.night_light
            && Self::compute_night_light_temp(
                &behavior.night_light_start,
                &behavior.night_light_end,
                behavior.night_light_temp,
                behavior.night_light_transition_mins,
            ) > 0.0
    }

    /// Force night light on or off and apply it immediately, rather than
    /// waiting for the once-a-minute schedule tick.
    pub(crate) fn set_night_light_override(&mut self, backend: &mut dyn Backend, enabled: bool) {
        self.night_light_override = Some(enabled);
        let temperature = if enabled {
            CONFIG.load().behavior().night_light_temp
        } else {
            0.0
        };
        backend.compositor_set_color_temperature(temperature);
        self.last_night_light_update = Some(std::time::Instant::now());
        log::info!("Night light {}", if enabled { "ON" } else { "OFF" });
        self.broadcast_ipc_event(
            "night_light/toggle",
            serde_json::json!({ "enabled": enabled }),
        );
    }

    /// Ensure the compositor needed to draw a built-in system UI is running,
    /// then acquire the X11 modal input grabs. If JWM was deliberately running
    /// without compositing, the compositor is leased only for the lifetime of
    /// the panel and restored to off by [`Self::close_system_ui`].
    /// Acquire the grabs for a shell surface. A pointer that is already taken
    /// is an error: every keyboard-invoked entry point wants to fail loudly
    /// rather than open half-grabbed.
    pub(crate) fn prepare_system_ui(
        &mut self,
        backend: &mut dyn Backend,
        label: &str,
        pointer_grab: SystemUiPointerGrab,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.prepare_system_ui_inner(backend, label, pointer_grab)? {
            return Ok(());
        }
        Err(format!("could not grab pointer for {label}").into())
    }

    /// As [`Self::prepare_system_ui`], but reports a busy pointer as
    /// `Ok(false)` so the caller can park the request and retry.
    pub(crate) fn prepare_system_ui_deferrable(
        &mut self,
        backend: &mut dyn Backend,
        label: &str,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        self.prepare_system_ui_inner(backend, label, SystemUiPointerGrab::Buttons)
    }

    /// `Ok(false)` means the pointer was not available; the keyboard grab and
    /// any temporary compositor have already been handed back.
    fn prepare_system_ui_inner(
        &mut self,
        backend: &mut dyn Backend,
        label: &str,
        pointer_grab: SystemUiPointerGrab,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        // Never over the lock card. `toggle_off_system_ui` already refuses for
        // every opener that asks it (`ShellEntry::Refuse`), but two openers
        // reach here without asking — the layout picker and the keybinding
        // viewer — and the layout picker is reachable over IPC. Refusing here
        // makes "nothing replaces the lock screen" true of the one function
        // every panel has to come through, rather than of a list that has to
        // stay complete.
        if self.features.system_ui.is_locked() {
            return Err(format!("{label} cannot replace the lock screen").into());
        }

        // A drag in flight holds a real pointer grab that carries motion
        // events; the grab a panel takes below silently replaces it and drops
        // them. Both `on_motion_notify` and `on_button_release` then bail on
        // `is_active()`, so the drag would be neither committed nor cancelled
        // and would stay armed after the panel closed. Alt+drag leaves the
        // keyboard free, so a panel key really is reachable mid-drag.
        if self.drag_ctl.is_some() {
            self.cancel_pointer_drag(backend);
        }
        // A tab reorder drag holds no grab, but it is cancelled the same
        // way — otherwise it would linger past the panel and commit on some
        // later release.
        self.tab_drag = None;

        // Any other panel still on screen at this point means a hand-over: one
        // shell key pressed while another key's panel was up. The keyboard and
        // the compositor are already ours, so inherit them rather than
        // releasing and reacquiring, which would flap a temporarily leased
        // compositor and open a window for the desktop to take the keyboard
        // back mid-swap.
        if self.features.system_ui.is_active() {
            // The pointer is *not* guaranteed: the keybinding viewer opens
            // keyboard-only, and a panel that inherited its grabs would let
            // clicks through to the windows underneath. Re-grabbing costs a
            // round-trip and always succeeds for the client that already holds
            // it, so ask before anything is torn down — a refusal has to leave
            // the panel on screen alone. (The reverse case is harmless: a
            // keyboard-only panel taking over from one that held the pointer
            // simply stays more modal than it asked to be, and
            // `close_system_ui` hands both back.)
            if let Some(pointer_mask) = pointer_grab.event_mask()
                && !backend.input_ops().grab_pointer(pointer_mask, None)?
            {
                // An error rather than `Ok(false)`: that reply promises the
                // keyboard and any leased compositor have been handed back,
                // which is exactly what a hand-over must never do.
                return Err(format!("could not grab pointer for {label}").into());
            }
            log::info!("Shell: {label} takes over from the panel on screen");
            self.hand_over_system_ui(backend);
            return Ok(true);
        }
        if !backend.has_compositor() {
            match self.set_compositor_enabled_reconciled(backend, true) {
                Ok(true) if backend.has_compositor() => {
                    self.features.system_ui_temporary_compositor = true;
                    log::info!("Temporarily enabled compositor for {label}");
                }
                Ok(_) => {
                    return Err(format!(
                        "{label} requires the JWM compositor, and this backend could not start it"
                    )
                    .into());
                }
                // The backend can reach ON and then report a trailing client
                // presentation failure. The reconciler deliberately surfaces
                // that partial failure, but abandoning the opener here would
                // leave an unowned compositor running forever: no panel would
                // exist to drive the normal lease-release path. Keep the
                // renderer as this panel's temporary lease, continue with the
                // grabs, and let close/failure cleanup return the session to
                // native mode.
                Err(error) if backend.has_compositor() => {
                    self.features.system_ui_temporary_compositor = true;
                    log::warn!(
                        "Compositor partially enabled for {label}; retaining it as a temporary lease: {error}"
                    );
                }
                Err(error) => {
                    return Err(format!("could not start compositor for {label}: {error}").into());
                }
            }
        }

        let Some(root) = backend.root_window() else {
            return Ok(true);
        };
        if let Err(error) = backend.key_ops().grab_keyboard(root) {
            self.release_temporary_system_ui_compositor(backend, label);
            return Err(error.into());
        }
        let Some(pointer_mask) = pointer_grab.event_mask() else {
            return Ok(true);
        };

        match backend.input_ops().grab_pointer(pointer_mask, None) {
            Ok(true) => Ok(true),
            // Hand the keyboard straight back: a caller that parks and retries
            // must not sit on it while it waits.
            Ok(false) => {
                let _ = backend.key_ops().ungrab_keyboard();
                self.release_temporary_system_ui_compositor(backend, label);
                Ok(false)
            }
            Err(error) => {
                let _ = backend.key_ops().ungrab_keyboard();
                self.release_temporary_system_ui_compositor(backend, label);
                Err(error.into())
            }
        }
    }

    fn release_temporary_system_ui_compositor(&mut self, backend: &mut dyn Backend, label: &str) {
        if !self.features.system_ui_temporary_compositor {
            return;
        }

        // A previous transition may already have completed even though the UI
        // lease flag survived (for example after an error response).  There is
        // nothing left to release in that case.
        if !backend.has_compositor() {
            self.features.system_ui_temporary_compositor = false;
            return;
        }

        if let Err(error) = self.prepare_for_compositor_disable(backend) {
            // Keep the lease flag: the compositor is still the temporary one,
            // and a later close/reload can retry the safe hand-back.
            log::warn!(
                "Could not restore compositor to OFF after {label}; disable preparation failed: {error}"
            );
            return;
        }

        match self.set_compositor_enabled_reconciled(backend, false) {
            Ok(true) if !backend.has_compositor() => {
                self.features.system_ui_temporary_compositor = false;
                log::info!("Restored compositor to OFF after {label}");
            }
            Ok(false) if !backend.has_compositor() => {
                self.features.system_ui_temporary_compositor = false;
            }
            Ok(true) => {
                log::warn!(
                    "Could not restore compositor to OFF after {label}: backend reported a transition but remains ON"
                );
            }
            Ok(false) => {
                log::warn!(
                    "Could not restore compositor to OFF after {label}: state unchanged (still ON)"
                );
            }
            Err(error) => {
                log::warn!("Could not restore compositor to OFF after {label}: {error}");
            }
        }
    }

    /// Drop the panel, release its grabs, and restore a temporarily enabled
    /// compositor to the user's previous off state.
    pub(crate) fn close_system_ui(&mut self, backend: &mut dyn Backend) {
        // Whatever a rebuild had queued is moot, and leaving the flag set would
        // send the next frame through `flush_system_ui`'s aborted-hand-over
        // backstop for a panel that closed on purpose.
        self.system_ui_dirty = false;
        self.features.system_ui_return_to_hub = false;
        // The Alt+Tab switcher's commit modifiers die with its panel, however
        // the panel went away.
        self.features.window_switcher_mods = Mods::empty();
        // A Bluetooth pairing session belongs to the Bluetooth picker; the
        // picker going away cancels the pairing before the panel drops.
        self.cancel_bluetooth_pairing();
        self.features.system_ui.cancel();
        backend.compositor_set_system_ui(None);
        let _ = backend.key_ops().ungrab_keyboard();
        let _ = backend.input_ops().ungrab_pointer();
        backend.compositor_force_full_redraw();
        self.release_temporary_system_ui_compositor(backend, "system UI");
    }

    /// Every UI key binding is a toggle *and* the panels are mutually
    /// exclusive: the key that put a panel on screen takes it away again, and
    /// any other panel's key replaces it.
    ///
    /// Returns `true` once the press has been fully dealt with and the opener
    /// should return: it dismissed the caller's own panel, or the lock screen
    /// refused it. `false` means carry on and open — either onto an empty
    /// screen or over another panel, which is handed over inside
    /// [`Self::prepare_system_ui`] once the opener's own preconditions have
    /// passed. `mine` decides which case this is; openers call this instead of
    /// a bare `is_active()` guard.
    ///
    /// Note what is deliberately *not* done here: the outgoing panel is left
    /// standing, all the way until [`Self::prepare_system_ui`]. That is where
    /// every opener's own preconditions have already passed — no `nmcli` for
    /// the Wi-Fi picker, clipboard history switched off, fewer than two
    /// outputs for the display layout — so a refusal leaves the user with the
    /// panel they had rather than a grabbed screen with nothing on it.
    pub(crate) fn toggle_off_system_ui(
        &mut self,
        backend: &mut dyn Backend,
        mine: impl FnOnce(&crate::jwm::features::SystemUiState) -> bool,
    ) -> bool {
        let state = &self.features.system_ui;
        match shell_entry(state.is_active(), state.is_locked(), mine(state)) {
            ShellEntry::Open | ShellEntry::TakeOver => false,
            ShellEntry::Dismiss => {
                self.close_system_ui(backend);
                true
            }
            ShellEntry::Refuse => true,
        }
    }

    /// Drop the panel being replaced, keeping the grabs and the compositor for
    /// the one taking its place.
    ///
    /// Unlike [`Self::close_system_ui`] this hands nothing back: the incoming
    /// panel wants the same keyboard and pointer grabs, and a temporarily
    /// leased compositor released here would be switched straight back on —
    /// parking every hidden window twice for a swap the user sees as one
    /// motion. What it does still do is run the teardown each panel owns.
    fn hand_over_system_ui(&mut self, backend: &mut dyn Backend) {
        // The film strip applies each layout as it is browsed. Leaving through
        // the side door must still put back the one the user started on.
        self.restore_layout_picker_origin(backend);
        // A child page's Escape target goes with the page.
        self.features.system_ui_return_to_hub = false;
        // So do the Alt+Tab switcher's commit modifiers.
        self.features.window_switcher_mods = Mods::empty();
        // A Bluetooth pairing session belongs to the outgoing Bluetooth
        // picker; it is cancelled before the panel state drops.
        self.cancel_bluetooth_pairing();
        // `cancel` zeroes the lock password and any Wi-Fi passphrase before
        // the string is dropped.
        self.features.system_ui.cancel();
        // Work started for the outgoing panel has nowhere to land, and a job
        // that finished a frame later would otherwise be adopted by whatever
        // opened next. Openers install their own jobs *after* this runs.
        self.features.wifi_scan = None;
        self.features.wifi_connect = None;
        self.features.bluetooth_scan = None;
        self.features.bluetooth_action = None;
        // Arm the backstop in `flush_system_ui`. Between here and the opener's
        // `sync_system_ui` the screen is grabbed with no panel behind it; no
        // opener can fail in that window today (every one of them installs its
        // state with no `?` in between), but nothing in the type system says
        // so, and the cost of being wrong is a session that cannot type.
        self.mark_system_ui_dirty();
    }

    /// Open the notification center: the bounded history JWM kept while
    /// toasts came and went, including what Do-Not-Disturb suppressed.
    pub(crate) fn notification_center(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.toggle_off_system_ui(backend, SystemUiState::is_notification_center) {
            return Ok(());
        }
        self.prepare_system_ui(backend, "notification center", SystemUiPointerGrab::Buttons)?;
        self.features.system_ui = crate::jwm::features::SystemUiState::notification_center(
            &self.features.notifications,
            crate::jwm::features::notifications::now_unix_ms(),
        );
        self.sync_system_ui(backend);
        Ok(())
    }

    pub(crate) fn app_launcher(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.toggle_off_system_ui(backend, SystemUiState::is_launcher) {
            return Ok(());
        }
        self.prepare_system_ui(
            backend,
            "application launcher",
            SystemUiPointerGrab::Buttons,
        )?;
        self.features.system_ui = self.cached_launcher_state();
        self.sync_system_ui(backend);
        Ok(())
    }

    pub(crate) fn monitor_layout(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.toggle_off_system_ui(backend, SystemUiState::is_monitor_layout) {
            return Ok(());
        }
        #[allow(unused_mut)]
        let mut is_x11 = false;
        #[cfg(feature = "backend-x11rb")]
        {
            is_x11 = is_x11
                || backend
                    .as_any()
                    .is::<crate::backend::x11rb::backend::X11rbBackend>();
        }
        #[cfg(feature = "backend-xcb")]
        {
            is_x11 = is_x11
                || backend
                    .as_any()
                    .is::<crate::backend::xcb::backend::XcbBackend>();
        }
        if !is_x11 {
            return Err("display layout via xrandr is only available on an X11 backend".into());
        }

        let entries: Vec<_> = backend
            .output_ops()
            .enumerate_outputs()
            .into_iter()
            .filter(|output| !output.name.is_empty() && output.width > 0 && output.height > 0)
            .map(|output| crate::jwm::features::MonitorLayoutEntry {
                name: output.name,
                x: output.x,
                y: output.y,
                width: output.width,
                height: output.height,
            })
            .collect();
        if entries.len() < 2 {
            return Err("display layout requires at least two active outputs".into());
        }

        self.prepare_system_ui(backend, "display layout", SystemUiPointerGrab::Buttons)?;
        self.features.system_ui = crate::jwm::features::SystemUiState::monitor_layout(entries);
        self.sync_system_ui(backend);
        Ok(())
    }

    pub(crate) fn lock_screen(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // The one UI key that is not a toggle: a lock the lock key could take
        // back off is not a lock. It comes off with the password.
        if self.features.system_ui.is_locked() {
            return Ok(());
        }
        // Another panel is in the way. Reported rather than swallowed: a
        // caller that believes it locked the session and did not is how a
        // desk gets left unattended and unlocked, and the idle policy uses
        // this to try again in a moment.
        if self.features.system_ui.is_active() {
            return Err("another system UI panel is open".into());
        }
        // On X11, never display a pretend lock if the exclusive keyboard grab
        // failed. Wayland-udev performs interception in its input pipeline.
        self.prepare_system_ui(backend, "lock screen", SystemUiPointerGrab::Buttons)?;
        self.features.system_ui = crate::jwm::features::SystemUiState::lock();
        self.sync_system_ui(backend);
        Ok(())
    }
    /// Minimise the selected window.
    ///
    /// One-way on purpose: a hidden window is on no tag and cannot be
    /// selected, so a key that toggled would have nothing to toggle back.
    /// Bringing one back is the launcher's window search (`reveal_and_focus`),
    /// or a taskbar.
    pub fn minimize(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(client_key) = self.get_selected_client_key() else {
            return Ok(());
        };
        let _changed = self.set_client_minimized(backend, client_key, true)?;
        Ok(())
    }

    /// 切换当前选中窗口的浮动状态
    pub fn togglefloating(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // info!("[togglefloating]");
        let Some(sel_mon_key) = self.state.sel_mon else {
            return Ok(());
        };
        let Some(sel_client_key) = self.state.monitors.get(sel_mon_key).and_then(|m| m.sel) else {
            return Ok(());
        };
        let geom = if let Some(client) = self.state.clients.get_mut(sel_client_key) {
            client.state.is_floating = !client.state.is_floating;
            // Explicit toggling wins over the drag origin: the float stays until
            // the user toggles it back, layout applies must not reclaim it.
            client.state.is_drag_floating = false;
            if client.state.is_floating {
                if client.geometry.floating_w <= 0 || client.geometry.floating_h <= 0 {
                    client.geometry.floating_x = client.geometry.x;
                    client.geometry.floating_y = client.geometry.y;
                    client.geometry.floating_w = client.geometry.w;
                    client.geometry.floating_h = client.geometry.h;
                }
                Some((
                    client.geometry.floating_x,
                    client.geometry.floating_y,
                    client.geometry.floating_w,
                    client.geometry.floating_h,
                ))
            } else {
                client.geometry.floating_x = client.geometry.x;
                client.geometry.floating_y = client.geometry.y;
                client.geometry.floating_w = client.geometry.w;
                client.geometry.floating_h = client.geometry.h;
                None
            }
        } else {
            return Ok(());
        };

        if let Some((x, y, w, h)) = geom {
            self.resize_client(backend, sel_client_key, x, y, w, h, false);
        }

        self.reorder_client_in_monitor_groups(sel_client_key);

        self.arrange(backend, Some(sel_mon_key));
        Ok(())
    }

    /// 切换当前选中窗口的粘性状态（sticky: 显示在所有标签）
    pub fn togglesticky(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(sel_mon_key) = self.state.sel_mon else {
            return Ok(());
        };
        let Some(sel_client_key) = self.state.monitors.get(sel_mon_key).and_then(|m| m.sel) else {
            return Ok(());
        };
        if let Some(client) = self.state.clients.get_mut(sel_client_key) {
            client.state.is_sticky = !client.state.is_sticky;
            if client.state.is_sticky {
                // Ensure sticky client has current monitor tags
                if let Some(monitor) = self.state.monitors.get(sel_mon_key) {
                    let current_tags = monitor.get_active_tags();
                    if let Some(client) = self.state.clients.get_mut(sel_client_key) {
                        client.state.tags = current_tags;
                    }
                }
            }
        }
        self.arrange(backend, Some(sel_mon_key));
        Ok(())
    }

    /// Close compositor-owned modal work before the X11 tree becomes native.
    /// This is shared by manual toggles, config reload and a temporary system
    /// UI lease so no path can leave invisible grabs or a 60 Hz phantom mode.
    pub(crate) fn prepare_for_compositor_disable(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Run the checked barrier before mutating user-visible modes. If one
        // hidden X11 client cannot be parked safely, the compositor stays on
        // and overview/recording/etc. remain exactly as the user left them.
        self.park_hidden_clients_before_compositor_disable(backend)?;
        if self.features.recording.active {
            self.stop_recording(backend)?;
        }

        if self.features.overview.active {
            self.features.overview.deactivate();
            backend.compositor_set_overview_mode(false, &[]);
            let _ = backend.key_ops().ungrab_keyboard();
        }
        if self.features.expose_active {
            self.apply_expose_action(backend, expose_plan::ExposeAction::Exit { focus: None })?;
        }
        if self.features.annotation_active {
            self.features.annotation_active = false;
            self.features.annotation_drawing = false;
            backend.compositor_set_annotation_mode(false);
            let _ = backend.key_ops().ungrab_keyboard();
            let _ = backend.input_ops().ungrab_pointer();
        }
        if self.features.screenshot.active {
            self.features.deferred_grab = None;
            self.cancel_screenshot_select(backend);
        }
        if self.features.recording.selecting_region {
            self.cancel_recording_region_interaction(backend);
        }
        Ok(())
    }

    /// 切换合成器开关
    pub fn togglecompositor(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let enable = !backend.has_compositor();
        if !enable && self.features.system_ui.is_active() {
            // The system UI is compositor-rendered and modal. Removing its
            // renderer here would leave an invisible keyboard/pointer grab or,
            // worse, an invisible lock screen. Honor OFF as soon as it closes.
            self.features.system_ui_temporary_compositor = true;
            log::info!("Compositor disable deferred until the system UI closes");
            return Ok(());
        }
        if !enable && let Err(error) = self.prepare_for_compositor_disable(backend) {
            log::warn!("Compositor remains ON; disable preparation failed: {error}");
            return Err(error);
        }
        match self.set_compositor_enabled_reconciled(backend, enable) {
            Ok(true) => {
                self.features.system_ui_temporary_compositor = false;
                log::info!(
                    "Compositor toggled: now {}",
                    if enable { "ON" } else { "OFF" }
                );
            }
            Ok(false) => {
                log::info!("Compositor state unchanged");
            }
            Err(e) => {
                log::warn!("Failed to toggle compositor: {e}");
                return Err(e.into());
            }
        }
        Ok(())
    }

    /// Toggle do-not-disturb. Broadcasts `dnd/toggle` so bars can update.
    pub fn toggle_dnd(
        &mut self,
        _backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.do_not_disturb = !self.do_not_disturb;
        log::info!("DND {}", if self.do_not_disturb { "ON" } else { "OFF" });
        self.broadcast_ipc_event(
            "dnd/toggle",
            serde_json::json!({ "enabled": self.do_not_disturb }),
        );
        Ok(())
    }

    /// 切换 debug 看板(HUD): 显示 FPS / 帧周期 / 内存 / CPU / 渲染分区耗时
    pub fn toggle_debug_hud(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if !self.debug_hud_on && !backend.has_compositor() {
            return Err("debug HUD requires an active compositor".into());
        }
        self.debug_hud_on = !self.debug_hud_on;
        backend.compositor_set_debug_hud(self.debug_hud_on);
        backend.compositor_set_debug_hud_extended(self.debug_hud_on);
        log::info!("Debug HUD {}", if self.debug_hud_on { "ON" } else { "OFF" });
        Ok(())
    }

    /// Toggle the native-size WaterLily simulation layer rendered by the compositor.
    pub fn toggle_waterlily(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        match backend.compositor_toggle_waterlily_effect() {
            Some(enabled) => log::info!("WaterLily effect {}", if enabled { "ON" } else { "OFF" }),
            None => log::warn!("WaterLily effect is unavailable on this backend"),
        }
        Ok(())
    }

    /// Hot-switch the WaterLily simulation case on the running worker.
    /// An explicit name selects that case; no argument (or `next`) cycles
    /// through the worker's registry.
    pub fn waterlily_case(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let requested = match arg {
            WMArgEnum::StringVec(values) if !values.is_empty() => values[0].as_str(),
            _ => "next",
        };
        match backend.compositor_set_waterlily_case(requested) {
            Some(true) => log::info!("WaterLily case request `{requested}` delivered"),
            Some(false) => {
                log::warn!("WaterLily case request `{requested}` dropped (no worker connected)")
            }
            None => log::warn!("WaterLily effect is unavailable on this backend"),
        }
        Ok(())
    }

    /// Hot-swap the WaterLily render palette on the running worker. An
    /// explicit name selects that palette, no argument (or `next`) cycles the
    /// worker's registry, and `auto` restores the per-case default.
    pub fn waterlily_palette(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let requested = match arg {
            WMArgEnum::StringVec(values) if !values.is_empty() => values[0].as_str(),
            _ => "next",
        };
        match backend.compositor_set_waterlily_palette(requested) {
            Some(true) => log::info!("WaterLily palette request `{requested}` delivered"),
            Some(false) => {
                log::warn!("WaterLily palette request `{requested}` dropped (no worker connected)")
            }
            None => log::warn!("WaterLily effect is unavailable on this backend"),
        }
        Ok(())
    }

    /// 切换部分重绘(scissor 局部刷新,实验性,默认关)
    pub fn togglepartialdamage(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let enable = !backend.has_partial_damage();
        match backend.set_partial_damage(enable) {
            Ok(true) => log::info!(
                "Partial-damage redraw toggled: now {}",
                if enable { "ON" } else { "OFF" }
            ),
            Ok(false) => log::info!("Partial-damage toggle ignored (no compositor active)"),
            Err(e) => log::warn!("Failed to toggle partial-damage: {e}"),
        }
        Ok(())
    }

    /// 切换 Overview 模式（3D 窗口切换器）
    pub fn toggle_overview(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if !configured_feature_toggle_allowed(
            self.features.overview.active,
            CONFIG.load().behavior().overview_enabled,
        ) {
            return Ok(());
        }
        if !self.features.overview.active && !backend.has_compositor() {
            return Err("overview requires an active compositor".into());
        }
        if self.features.overview.active {
            // End overview: focus selected window and promote it to master
            if let Some(&client_key) = self
                .features
                .overview
                .clients
                .get(self.features.overview.index)
            {
                if let Some(mon_key) = self.state.sel_mon {
                    self.detach(client_key);
                    self.attach_front(client_key);
                    self.focus(backend, Some(client_key))?;
                    self.arrange(backend, Some(mon_key));
                } else {
                    self.focus(backend, Some(client_key))?;
                }
            }
            self.features.overview.deactivate();
            backend.compositor_set_overview_mode(false, &[]);
            let _ = backend.key_ops().ungrab_keyboard();
        } else {
            // Start overview: collect visible windows on current monitor
            let sel_mon_key = match self.state.sel_mon {
                Some(k) => k,
                None => return Ok(()),
            };
            let visible: Vec<ClientKey> = {
                let mon_clients = self.state.monitor_clients.get(sel_mon_key);
                match mon_clients {
                    Some(clients) => clients
                        .iter()
                        .copied()
                        .filter(|&ck| self.is_client_visible_by_key(ck))
                        .collect(),
                    None => Vec::new(),
                }
            };
            let visible = {
                let is_scrolling = self
                    .state
                    .monitors
                    .get(sel_mon_key)
                    .map(|monitor| *monitor.lt == crate::core::layout::LayoutEnum::SCROLLING)
                    .unwrap_or(false);
                if is_scrolling {
                    self.scrolling_state_for_monitor(sel_mon_key)
                        .map(|state| state.ordered_visible_clients(&visible))
                        .unwrap_or(visible)
                } else {
                    visible
                }
            };

            if visible.is_empty() {
                return Ok(());
            }

            let focused_index = self
                .state
                .monitors
                .get(sel_mon_key)
                .and_then(|monitor| monitor.sel)
                .and_then(|focused| visible.iter().position(|&client| client == focused));

            // Tell compositor which monitor to render the prism on.
            if let Some(mon) = self.state.monitors.get(sel_mon_key) {
                backend.compositor_set_overview_monitor(
                    mon.geometry.w_x as i32,
                    mon.geometry.w_y as i32,
                    mon.geometry.w_w as u32,
                    mon.geometry.w_h as u32,
                );
            }

            // Activation chooses the focused item and the first bounded prism
            // subset together, so compositor selection and navigation state
            // cannot begin on different windows.
            let Some(plan) = self.features.overview.activate(visible, focused_index) else {
                return Ok(());
            };
            let subset =
                self.features.overview.clients[plan.window_start..plan.window_end].to_vec();
            let mut layout = self.build_overview_layout(&subset);
            for (index, entry) in layout.iter_mut().enumerate() {
                entry.5 = index == plan.selected_in_window;
            }
            backend.compositor_set_overview_mode(true, &layout);
            if let Some(root) = backend.root_window() {
                let _ = backend.key_ops().grab_keyboard(root);
            }
        }
        Ok(())
    }

    /// 在 Overview 模式中循环切换窗口选择
    pub fn cycle_overview(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if !self.features.overview.active || self.features.overview.clients.is_empty() {
            return Ok(());
        }

        let direction = match arg {
            WMArgEnum::Int(d) => *d,
            _ => 1,
        };

        // 导航决策（新索引、窗口偏移、是否需要刷新棱镜子集）由纯策略给出。
        let Some(plan) = crate::jwm::features::overview_plan::plan_cycle(
            self.features.overview.index,
            self.features.overview.slide_offset,
            self.features.overview.clients.len(),
            direction > 0,
        ) else {
            return Ok(());
        };
        self.features.overview.index = plan.index;
        self.features.overview.slide_offset = plan.slide_offset;

        if let Some((window_start, window_end, selected_in_window)) = plan.refresh_window {
            // Window shifted: refresh prism with new 6-client subset.
            let subset: Vec<ClientKey> =
                self.features.overview.clients[window_start..window_end].to_vec();
            let mut layout = self.build_overview_layout(&subset);
            // Mark the correct entry as selected.
            for (i, entry) in layout.iter_mut().enumerate() {
                entry.5 = i == selected_in_window;
            }
            backend.compositor_set_overview_mode(true, &layout);
        }

        // Set selection (rotation) to the newly selected client.
        if let Some(&ck) = self.features.overview.clients.get(plan.index)
            && let Some(client) = self.state.clients.get(ck)
        {
            backend.compositor_set_overview_selection(client.win);
        }
        Ok(())
    }

    /// 切换放大镜功能
    pub fn toggle_magnifier(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if !self.features.magnifier.enabled && !backend.has_compositor() {
            return Err("magnifier requires an active compositor".into());
        }
        self.features.magnifier.enabled = !self.features.magnifier.enabled;
        backend.compositor_set_magnifier(self.features.magnifier.enabled);
        Ok(())
    }

    /// 切换 Peek 模式（Boss Key - 所有窗口淡出）
    pub fn toggle_peek(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if !configured_feature_toggle_allowed(
            self.features.peek_active,
            CONFIG.load().behavior().peek_enabled,
        ) {
            return Ok(());
        }
        if !self.features.peek_active && !backend.has_compositor() {
            return Err("peek requires an active compositor".into());
        }
        self.features.peek_active = !self.features.peek_active;
        backend.compositor_set_peek_mode(self.features.peek_active);
        Ok(())
    }

    /// 切换屏幕标注（Annotation）模式
    pub fn toggle_annotation(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.features.annotation_active {
            self.features.annotation_active = false;
            self.features.annotation_drawing = false;
            backend.compositor_set_annotation_mode(false);
            let _ = backend.key_ops().ungrab_keyboard();
            let _ = backend.input_ops().ungrab_pointer();
            return Ok(());
        }
        if !backend.has_compositor() {
            return Err("screen annotation requires an active compositor".into());
        }

        let keyboard_grabbed = if let Some(root) = backend.root_window() {
            backend.key_ops().grab_keyboard(root)?;
            true
        } else {
            false
        };
        let pointer_mask = (EventMaskBits::BUTTON_PRESS
            | EventMaskBits::BUTTON_RELEASE
            | EventMaskBits::POINTER_MOTION)
            .bits();
        match backend.input_ops().grab_pointer(pointer_mask, None) {
            Ok(true) => {}
            Ok(false) => {
                if keyboard_grabbed {
                    let _ = backend.key_ops().ungrab_keyboard();
                }
                return Err("could not grab pointer for screen annotation".into());
            }
            Err(error) => {
                if keyboard_grabbed {
                    let _ = backend.key_ops().ungrab_keyboard();
                }
                return Err(error.into());
            }
        }

        self.features.annotation_active = true;
        backend.compositor_set_annotation_mode(true);
        Ok(())
    }

    /// 切换屏幕录制
    pub fn toggle_recording(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if !self.features.recording.active {
            if self.features.recording.selecting_region {
                self.cancel_recording_region_interaction(backend);
                return Ok(());
            }
            if let Err(error) = Self::require_recording_runtime() {
                backend.compositor_push_toast(crate::backend::api::ToastNotification {
                    title: "\u{f03d}  Recording unavailable".into(),
                    body: error.clone(),
                    urgency: 2,
                    timeout_ms: 8000,
                    ..Default::default()
                });
                return Err(error.into());
            }
            let output_path = self.prepare_recording_output_path()?;
            self.begin_recording_region_selection(backend, output_path)?;
        } else {
            self.stop_recording(backend)?;
        }
        Ok(())
    }

    fn require_recording_runtime() -> Result<(), String> {
        let path = std::env::var_os("PATH");
        let missing = crate::jwm::features::recording_plan::missing_runtime_tools(|tool| {
            crate::terminal_prober::command_exists_in_path(tool, path.as_deref())
        });
        if missing.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "missing {} in PATH; install the ffmpeg package",
                missing.join(" and ")
            ))
        }
    }

    /// Enter interactive move/resize mode while keeping the encoder running.
    pub fn adjust_recording_region(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if !self.features.recording.active {
            return Err("recording region adjustment requires an active recording".into());
        }
        if !self.features.recording.begin_region_adjustment() {
            return Ok(());
        }
        self.features.capture.recording = CaptureTarget::Region;
        if let Err(error) = self.grab_recording_region_input(backend) {
            self.features.recording.cancel_region_selection();
            return Err(error);
        }
        self.sync_recording_region_overlay(backend);
        info!("[recording] interactive region adjustment started");
        Ok(())
    }

    fn prepare_recording_output_path(&self) -> Result<String, Box<dyn std::error::Error>> {
        use crate::jwm::features::recording_plan::{output_file_name, resolve_output_directory};

        let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S-%6f");
        let output_dir = resolve_output_directory(
            &CONFIG.load().behavior().recording_output_dir,
            std::env::var("XDG_VIDEOS_DIR")
                .ok()
                .filter(|path| !path.is_empty())
                .map(std::path::PathBuf::from),
            dirs::video_dir(),
            std::env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .map(std::path::PathBuf::from),
        )?;
        std::fs::create_dir_all(&output_dir).map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!(
                    "cannot create recording output directory '{}': {error}",
                    output_dir.display()
                ),
            )
        })?;
        Ok(output_dir
            .join(output_file_name(&timestamp.to_string()))
            .to_string_lossy()
            .to_string())
    }

    fn grab_recording_region_input(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(root) = backend.root_window() {
            backend.key_ops().grab_keyboard(root)?;
        }
        let crosshair = backend
            .cursor_provider()
            .get(StdCursorKind::Crosshair)
            .ok()
            .map(|cursor| cursor.0);
        let pointer_mask = (EventMaskBits::BUTTON_PRESS
            | EventMaskBits::BUTTON_RELEASE
            | EventMaskBits::POINTER_MOTION)
            .bits();
        match backend.input_ops().grab_pointer(pointer_mask, crosshair) {
            Ok(true) => {}
            Ok(false) => {
                let _ = backend.key_ops().ungrab_keyboard();
                return Err("could not grab pointer for recording region selection".into());
            }
            Err(error) => {
                let _ = backend.key_ops().ungrab_keyboard();
                return Err(error.into());
            }
        }
        Ok(())
    }

    fn release_recording_region_input(&mut self, backend: &mut dyn Backend) {
        let _ = backend.key_ops().ungrab_keyboard();
        let _ = backend.input_ops().ungrab_pointer();
        if let Some(root) = backend.root_window() {
            let _ = backend
                .cursor_provider()
                .apply(root, StdCursorKind::LeftPtr);
        }
    }

    fn begin_recording_region_selection(
        &mut self,
        backend: &mut dyn Backend,
        output_path: String,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if !backend.has_compositor() {
            return Err("screen recording requires an active compositor".into());
        }
        self.features
            .recording
            .begin_initial_region_selection(output_path.clone());
        self.features.capture.recording = CaptureTarget::Region;
        if let Err(error) = self.grab_recording_region_input(backend) {
            self.features.recording.cancel_region_selection();
            return Err(error);
        }
        backend.compositor_set_recording_region_overlay(None);
        backend.compositor_force_full_redraw();
        info!("[recording] select a region, then press Enter to start → {output_path}");
        Ok(())
    }

    pub(crate) fn sync_recording_region_overlay(&mut self, backend: &mut dyn Backend) {
        let region = self
            .features
            .recording
            .region
            .and_then(Self::recording_region_tuple);
        backend.compositor_set_recording_region_overlay(region);
        backend.compositor_force_full_redraw();
    }

    pub(crate) fn finish_recording_region_interaction(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(region) = self.features.recording.region else {
            return Ok(());
        };
        let Some(region_tuple) = Self::recording_region_tuple(region) else {
            return Ok(());
        };
        let adjusting = self.features.recording.adjusting_region;
        let pending_path = self.features.recording.pending_output_path.clone();
        self.features.recording.finish_region_selection();
        self.release_recording_region_input(backend);
        backend.compositor_set_recording_region_overlay(None);

        if adjusting {
            backend.compositor_set_recording_region(region_tuple);
            backend.compositor_force_full_redraw();
            info!(
                "[recording] region adjustment committed: {}x{}+{}+{}",
                region.w, region.h, region.x, region.y
            );
            return Ok(());
        }

        let Some(output_path) = pending_path else {
            return Err("recording selection lost its output path".into());
        };
        if let Err(error) = self.start_recording_region(backend, &output_path, region) {
            self.features.recording.cancel();
            backend.compositor_force_full_redraw();
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn cancel_recording_region_interaction(&mut self, backend: &mut dyn Backend) {
        let was_adjusting = self.features.recording.adjusting_region;
        let restored = self.features.recording.cancel_region_selection();
        self.release_recording_region_input(backend);
        backend.compositor_set_recording_region_overlay(None);
        if was_adjusting {
            if let Some(region) = restored.and_then(Self::recording_region_tuple) {
                backend.compositor_set_recording_region(region);
            }
        }
        backend.compositor_force_full_redraw();
        info!(
            "[recording] region {} cancelled",
            if was_adjusting {
                "adjustment"
            } else {
                "selection"
            }
        );
    }

    pub(crate) fn recording_region_tuple(region: Rect) -> Option<(i32, i32, u32, u32)> {
        Some((
            region.x,
            region.y,
            u32::try_from(region.w).ok()?,
            u32::try_from(region.h).ok()?,
        ))
    }

    /// Toggle the built-in microphone recorder (Alt+Ctrl+M by default).
    pub fn toggle_audio_recording(
        &mut self,
        _backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.features.audio_recording.refresh();
        if self.features.audio_recording.active {
            self.stop_audio_recording()?;
        } else {
            let behavior = CONFIG.load().behavior().clone();
            let output_dir = if !behavior.audio_recording_output_dir.is_empty() {
                std::path::PathBuf::from(&behavior.audio_recording_output_dir)
            } else {
                std::env::var("XDG_MUSIC_DIR")
                    .map(std::path::PathBuf::from)
                    .or_else(|_| {
                        std::env::var("HOME")
                            .map(|home| std::path::PathBuf::from(home).join("Music"))
                    })
                    .unwrap_or_else(|_| std::path::PathBuf::from("/tmp"))
            };
            let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S-%6f");
            let format = behavior.audio_recording_format.as_str();
            if !matches!(format, "wav" | "flac" | "opus" | "mp3") {
                return Err(format!("unsupported audio recording format: {format}").into());
            }
            let path = output_dir.join(format!("jwm-recording-{timestamp}.{format}"));
            self.start_audio_recording(&path)?;
        }
        Ok(())
    }

    pub(crate) fn start_audio_recording(
        &mut self,
        output_path: &std::path::Path,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let behavior = CONFIG.load().behavior().clone();
        if self.features.recording.active && behavior.recording_audio_enabled {
            return Err(
                "screen recording is already using the configured microphone; stop it first".into(),
            );
        }
        self.features.audio_recording.start(
            output_path,
            &behavior.audio_recording_device,
            behavior.audio_recording_sample_rate,
            behavior.audio_recording_channels,
            &behavior.audio_recording_backend,
            &behavior.audio_recording_bitrate,
        )?;
        info!(
            "[audio-recording] start → {} (backend={}, format={}, device={}, {} Hz, {} channel(s))",
            output_path.display(),
            self.features.audio_recording.backend,
            self.features.audio_recording.format,
            self.features.audio_recording.device,
            self.features.audio_recording.sample_rate,
            self.features.audio_recording.channels
        );
        self.broadcast_ipc_event(
            "audio_recording/started",
            serde_json::json!({"output_path": output_path}),
        );
        Ok(())
    }

    pub(crate) fn stop_audio_recording(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let was_active = self.features.audio_recording.active;
        let path = self.features.audio_recording.output_path.clone();
        self.features.audio_recording.stop()?;
        if was_active {
            info!(
                "[audio-recording] stop → {}",
                path.as_deref().unwrap_or("(unset)")
            );
            self.broadcast_ipc_event(
                "audio_recording/stopped",
                serde_json::json!({"output_path": path}),
            );
        }
        Ok(())
    }

    /// Start a recording from a source rectangle. The encoded dimensions are
    /// fixed from this initial rectangle while later region updates are scaled
    /// into the same video canvas.
    pub(crate) fn start_recording_region(
        &mut self,
        backend: &mut dyn Backend,
        output_path: &str,
        region: Rect,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.features.recording.active {
            return Err("recording is already active".into());
        }
        if !backend.has_compositor() {
            return Err("screen recording requires an active compositor".into());
        }
        // Do this before mutating RecordingState.  Previously a missing
        // ffmpeg executable made the compositor reject the child spawn while
        // the WM still reported an active recording, then "stopped" without
        // ever creating an MP4.
        Self::require_recording_runtime()?;
        let output = std::path::Path::new(output_path);
        crate::jwm::features::recording_plan::validate_output_path(output)?;
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if output.exists() {
            return Err(format!("recording output already exists: {output_path}").into());
        }
        let region = self.normalize_initial_recording_region(region)?;

        // A standalone WAV recording and the synchronized screen audio track
        // must not race for the same capture device. Finalize the standalone
        // file before handing the microphone to the screen recorder.
        if CONFIG.load().behavior().recording_audio_enabled && self.features.audio_recording.active
        {
            info!("[recording] stopping standalone audio before synchronized capture");
            self.stop_audio_recording()?;
        }

        self.features.recording.start(output_path.to_string());
        self.features.recording.set_region(region);
        self.features.recording.set_output_size_from_region();
        self.features
            .recording
            .start_segment(output_path.to_string());
        let region_tuple = Self::recording_region_tuple(region)
            .ok_or("recording region dimensions are invalid")?;
        info!(
            "[recording] start → {output_path} ({}x{}+{}+{})",
            region.w, region.h, region.x, region.y
        );
        backend.compositor_start_recording_region(output_path, region_tuple);
        Ok(())
    }

    pub(crate) fn normalize_initial_recording_region(
        &self,
        region: Rect,
    ) -> Result<Rect, Box<dyn std::error::Error>> {
        crate::jwm::features::recording_plan::normalize_initial_region(region, self.s_w, self.s_h)
            .map_err(Into::into)
    }

    /// Stop the active recording. This operation is intentionally idempotent.
    pub(crate) fn stop_recording(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if !self.features.recording.active {
            if self.features.recording.selecting_region {
                self.cancel_recording_region_interaction(backend);
            }
            return Ok(());
        }
        if self.features.recording.selecting_region {
            self.cancel_recording_region_interaction(backend);
        }
        backend.compositor_stop_recording();
        self.features.recording.stop();
        let segments = std::mem::take(&mut self.features.recording.segments);
        let output_path = self
            .features
            .recording
            .output_path
            .clone()
            .unwrap_or_default();
        info!(
            "[recording] stop → {output_path} ({} segments)",
            segments.len()
        );
        backend.compositor_push_toast(crate::backend::api::ToastNotification {
            title: "\u{f03d}  Recording stopped".into(),
            body: output_path.clone(),
            urgency: 1,
            timeout_ms: 5000,
            ..Default::default()
        });
        Self::finalize_recording(segments, output_path);
        Ok(())
    }

    /// Validate direct output, or concatenate legacy multi-segment recordings.
    ///
    /// "做什么"由 `recording_plan::plan_finalization` 决定；这里只在后台
    /// 线程里执行 ffprobe 轮询、文件搬移和 ffmpeg concat。
    fn finalize_recording(segments: Vec<String>, output_path: String) {
        use crate::jwm::features::recording_plan::{FinalizationPlan, plan_finalization};

        let plan = plan_finalization(&segments, &output_path);
        let worker = std::thread::Builder::new()
            .name("jwm-record-final".to_owned())
            .spawn(move || {
            match plan {
                FinalizationPlan::Nothing => return,
                FinalizationPlan::ValidateSingle { segment, move_to } => {
                    // Do not move the MP4 before ffmpeg has written the moov
                    // atom, otherwise the final path can point at an unplayable
                    // file. The compositor now hands the encoder off to a writer
                    // thread instead of waiting for it, so this poll has to
                    // outlast ffmpeg's own exit work: flushing the encoder and,
                    // with `+faststart`, rewriting the whole file to move that
                    // atom to the front. That is seconds for a long recording
                    // and longer on a slow disk, so the budget is a minute —
                    // polled tightly at first, then slowly, because it is only
                    // the first second that usually matters.
                    const FINALIZE_BUDGET: std::time::Duration = std::time::Duration::from_secs(60);
                    let deadline = std::time::Instant::now() + FINALIZE_BUDGET;
                    let mut ready = false;
                    let mut attempt = 0_u32;
                    while !ready && std::time::Instant::now() < deadline {
                        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                        ready = crate::jwm::features::external_command::status_with_timeout(
                            "ffprobe",
                            &[
                                "-v",
                                "error",
                                "-show_entries",
                                "format=duration",
                                "-of",
                                "default=nw=1",
                                &segment,
                            ],
                            remaining.min(RECORDING_PROBE_ATTEMPT_TIMEOUT),
                        )
                            .is_ok_and(|status| status.success());
                        if !ready {
                            let backoff = if attempt < 20 { 50 } else { 500 };
                            std::thread::sleep(
                                deadline
                                    .saturating_duration_since(std::time::Instant::now())
                                    .min(std::time::Duration::from_millis(backoff)),
                            );
                            attempt += 1;
                        }
                    }
                    if !ready {
                        log::error!(
                            "[recording] output was not finalized within {}s; leaving it at {segment}",
                            FINALIZE_BUDGET.as_secs()
                        );
                        return;
                    }
                    // New recordings are encoded directly at output_path. Keep the
                    // move for old callers that may still pass a separate segment.
                    if let Some(target) = move_to {
                        if let Err(rename_error) = std::fs::rename(&segment, &target) {
                            if let Err(copy_error) = std::fs::copy(&segment, &target) {
                                log::error!(
                                    "[recording] could not move finalized segment {segment} to {target}: rename failed ({rename_error}); copy failed ({copy_error})"
                                );
                                return;
                            }
                            if let Err(error) = std::fs::remove_file(&segment) {
                                log::warn!(
                                    "[recording] copied {segment} to {target} but could not remove the source: {error}"
                                );
                            }
                        }
                    }
                }
                FinalizationPlan::ConcatSegments {
                    list_path,
                    list_content,
                    output_path: concat_output,
                } => {
                    let finalized = finalize_concat_segments(
                        &list_path,
                        &list_content,
                        &concat_output,
                        &segments,
                        |list_path| {
                            let list_path = list_path.to_str().ok_or_else(|| {
                                "concat list path is not valid UTF-8".to_owned()
                            })?;
                            let status = crate::jwm::features::external_command::status_with_timeout(
                                "ffmpeg",
                                &[
                                    "-f",
                                    "concat",
                                    "-safe",
                                    "0",
                                    "-i",
                                    list_path,
                                    "-c",
                                    "copy",
                                    "-y",
                                    &concat_output,
                                ],
                                RECORDING_CONCAT_TIMEOUT,
                            )
                            .map_err(|error| format!("ffmpeg concat failed: {error}"))?;
                            if status.success() {
                                Ok(())
                            } else {
                                Err(format!("ffmpeg concat exited with {status}"))
                            }
                        },
                    );
                    if let Err(error) = finalized {
                        log::error!(
                            "[recording] {error}; preserving {} source segments",
                            segments.len()
                        );
                        return;
                    }
                }
            }
            log::info!("[recording] finalized → {output_path}");
        });
        if let Err(error) = worker {
            log::error!("[recording] could not start finalization worker: {error}");
        }
    }

    /// 切换 Expose / Mission Control 模式（显示所有窗口缩略图）
    pub fn toggle_expose(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if !configured_feature_toggle_allowed(
            self.features.expose_active,
            CONFIG.load().behavior().expose_enabled,
        ) {
            return Ok(());
        }
        if !self.features.expose_active && !backend.has_compositor() {
            return Err("expose requires an active compositor".into());
        }
        // Collect windows visible on their monitor; eligibility filtering and
        // the enter/exit decision live in the pure plan.
        let mut candidates: Vec<expose_plan::ExposeCandidate> = Vec::new();
        if !self.features.expose_active {
            for &mon_key in &self.state.monitor_order.clone() {
                if let Some(clients) = self.state.monitor_clients.get(mon_key) {
                    for &ck in clients {
                        if !self.is_client_visible_on_monitor(ck, mon_key) {
                            continue;
                        }
                        if let Some(client) = self.state.clients.get(ck) {
                            let g = &client.geometry;
                            candidates.push((client.win, g.x, g.y, g.w, g.h));
                        }
                    }
                }
            }
        }
        let action = expose_plan::plan_toggle(self.features.expose_active, candidates);
        self.apply_expose_action(backend, action)
    }

    /// 执行 expose 计划：进入时排布窗口并抓取输入，退出时统一走同一段
    /// 清理序列（此前在切换、Escape 与两种点击路径中重复了四次）。
    pub(crate) fn apply_expose_action(
        &mut self,
        backend: &mut dyn Backend,
        action: expose_plan::ExposeAction,
    ) -> Result<(), Box<dyn std::error::Error>> {
        match action {
            expose_plan::ExposeAction::Keep => {}
            expose_plan::ExposeAction::Enter { windows } => {
                self.features.expose_active = true;
                backend.compositor_set_expose_mode(true, windows);
                // 高亮落在当前聚焦窗口上：进入后直接回车等于回到原窗口。
                let focused = self
                    .get_selected_client_key()
                    .and_then(|ck| self.state.clients.get(ck))
                    .map(|client| client.win);
                backend.compositor_expose_select(focused);
                if let Some(root) = backend.root_window() {
                    let _ = backend.key_ops().grab_keyboard(root);
                }
                let pointer_mask = (EventMaskBits::BUTTON_PRESS
                    | EventMaskBits::BUTTON_RELEASE
                    | EventMaskBits::POINTER_MOTION)
                    .bits();
                let _ = backend.input_ops().grab_pointer(pointer_mask, None);
            }
            expose_plan::ExposeAction::Exit { focus } => {
                self.features.expose_active = false;
                backend.compositor_set_expose_mode(false, vec![]);
                let _ = backend.key_ops().ungrab_keyboard();
                let _ = backend.input_ops().ungrab_pointer();
                if let Some(wid) = focus
                    && let Some(ck) = self.wintoclient(wid)
                {
                    self.focus(backend, Some(ck))?;
                    if let Some(mon_key) = self.state.sel_mon {
                        let _ = self.restack(backend, Some(mon_key));
                    }
                }
            }
        }
        Ok(())
    }

    /// 更新粘性窗口的标签（当显示器切换标签时调用）
    pub(crate) fn update_sticky_tags(&mut self, mon_key: crate::core::models::MonitorKey) {
        let new_tags = if let Some(monitor) = self.state.monitors.get(mon_key) {
            monitor.get_active_tags()
        } else {
            return;
        };
        let client_keys: Vec<ClientKey> = self
            .state
            .monitor_clients
            .get(mon_key)
            .map(|keys| keys.clone())
            .unwrap_or_default();
        for ck in client_keys {
            if let Some(client) = self.state.clients.get_mut(ck) {
                if client.state.is_sticky {
                    client.state.tags = new_tags;
                }
            }
        }
    }

    /// Toggle a named scratchpad.
    ///
    /// Argument encoding (via `StringVec`):
    ///   `["name", "cmd", "arg1", ...]`  — name + spawn command
    ///   `["name"]`                      — name only (uses default scratchpad terminal)
    ///
    /// Legacy `Int(0)` falls back to the default name `"term"`.
    pub fn togglescratchpad(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let cfg = CONFIG.load();
        // Parse name and optional command from argument
        let (name, spawn_cmd) = match arg {
            WMArgEnum::StringVec(v) if !v.is_empty() => {
                let name = v[0].clone();
                let cmd = if v.len() > 1 {
                    v[1..].to_vec()
                } else {
                    crate::config::Config::get_scratchpad_termcmd()
                };
                (name, cmd)
            }
            _ => (
                "term".to_string(),
                crate::config::Config::get_scratchpad_termcmd(),
            ),
        };

        // Check if the scratchpad's client still exists
        if let Some(&sp_key) = self.scratchpads.get(&name) {
            if self.state.clients.get(sp_key).is_none() {
                self.scratchpads.remove(&name);
            }
        }

        if let Some(&sp_key) = self.scratchpads.get(&name) {
            // Scratchpad exists — toggle visibility
            let is_visible = self.is_client_visible_by_key(sp_key);
            if is_visible {
                // Hide: animate upward then hide
                if let Some(client) = self.state.clients.get(sp_key) {
                    let current_rect = Rect::new(
                        client.geometry.x,
                        client.geometry.y,
                        client.geometry.w,
                        client.geometry.h,
                    );
                    // Target: move up by window height
                    let hidden_y = current_rect.y - current_rect.h - 100;
                    let hidden_rect =
                        Rect::new(current_rect.x, hidden_y, current_rect.w, current_rect.h);

                    if cfg.animation_enabled() {
                        self.animations.start(
                            sp_key,
                            current_rect,
                            hidden_rect,
                            cfg.animation_duration(),
                            cfg.animation_easing(),
                            AnimationKind::Hide,
                        );
                    } else {
                        // If animations disabled, immediately hide
                        if let Some(c) = self.state.clients.get_mut(sp_key) {
                            c.state.tags = 0;
                        }
                    }
                }

                // Mark for deferred hiding after animation completes
                if let Some(c) = self.state.clients.get_mut(sp_key) {
                    c.state.tags = 0;
                }

                let mon_key = self.state.clients.get(sp_key).and_then(|c| c.mon);
                self.focus(backend, None)?;
                if let Some(mk) = mon_key {
                    self.arrange(backend, Some(mk));
                }
            } else {
                let was_minimized = self
                    .state
                    .clients
                    .get(sp_key)
                    .is_some_and(|client| client.state.is_hidden);
                let window = self
                    .state
                    .clients
                    .get(sp_key)
                    .map(|client| client.win)
                    .ok_or("scratchpad disappeared before reveal")?;

                if !self.reveal_and_focus(backend, window)? {
                    return Err("scratchpad disappeared before reveal".into());
                }

                // A minimized scratchpad already has the compositor's reverse
                // Genie. Starting the scratchpad Appear animation as well would
                // fight over the same geometry and can flash the real surface.
                // A merely parked scratchpad keeps its original downward reveal.
                if !was_minimized
                    && let Some(mon_key) = self.state.sel_mon
                    && let Some(area) = self.monitor_work_area(mon_key)
                {
                    let w = area.w.saturating_mul(4) / 5;
                    let h = area.h.saturating_mul(4) / 5;
                    let x = area.x + (area.w - w) / 2;
                    let y = area.y + (area.h - h) / 2;

                    if cfg.animation_enabled() {
                        // Animate from above screen to target position
                        // from_y: window top is at (area.y - h), so window is completely above visible area
                        let from_y = area.y - h;
                        let from_rect = Rect::new(x, from_y, w, h);
                        let to_rect = Rect::new(x, y, w, h);

                        info!(
                            "[togglescratchpad] scratchpad show animation from y={} to y={}",
                            from_y, y
                        );

                        self.animations.start(
                            sp_key,
                            from_rect,
                            to_rect,
                            cfg.animation_duration(),
                            cfg.animation_easing(),
                            AnimationKind::Appear,
                        );
                    }
                }
            }
        } else {
            // No scratchpad with this name — spawn once and bind the pending
            // identity to the exact child PID. Repeated toggles while startup
            // is in flight are no-ops; different names remain independent.
            let now = std::time::Instant::now();
            self.expire_pending_scratchpads(now);
            if let Err(error) = self.scratchpad_pending.ensure_name_can_spawn(&name) {
                match error {
                    crate::jwm::scratchpad_pending::PendingRegistrationError::DuplicateName {
                        pid,
                        ..
                    } => info!(
                        "[togglescratchpad] '{}' is already pending for PID {}; not spawning a duplicate",
                        name, pid
                    ),
                    other => error!(
                        "[togglescratchpad] refusing to spawn pending '{}': {}",
                        name, other
                    ),
                }
                return Ok(());
            }
            if let Some(prog) = spawn_cmd.first() {
                let mut command = Command::new(prog);
                command.args(&spawn_cmd[1..]);

                Self::setup_smithay_child_env(&mut command, backend);
                command
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::inherit())
                    .stderr(std::process::Stdio::inherit());
                Self::apply_child_pre_exec(&mut command);

                match command.spawn() {
                    Ok(child) => {
                        let pid = child.id();
                        let process_start_time =
                            crate::jwm::scratchpad_pending::linux_process_start_time(pid);
                        if process_start_time.is_none() {
                            warn!(
                                "[togglescratchpad] could not read /proc/{pid}/stat; '{}' will use strict PID matching with a short timeout",
                                name
                            );
                        }
                        match self.scratchpad_pending.register_spawned(
                            pid,
                            name.clone(),
                            process_start_time,
                            now,
                        ) {
                            Ok(()) => info!(
                                "[togglescratchpad] spawned '{}' PID: {} (starttime={:?})",
                                name, pid, process_start_time
                            ),
                            Err(error) => error!(
                                "[togglescratchpad] spawned '{}' PID {} but could not register its identity: {}",
                                name, pid, error
                            ),
                        }
                        self.supervise_transient_child(child);
                    }
                    Err(e) => {
                        error!("[togglescratchpad] failed to spawn '{}': {}", name, e);
                    }
                }
            }
        }
        Ok(())
    }

    /// 切换 Picture-in-Picture (PIP) 模式
    ///
    /// 将当前选中的窗口变为小窗悬浮在所有工作区右下角
    pub fn togglepip(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(sel_mon_key) = self.state.sel_mon else {
            return Ok(());
        };
        let Some(sel_client_key) = self.state.monitors.get(sel_mon_key).and_then(|m| m.sel) else {
            return Ok(());
        };

        let is_pip = self
            .state
            .clients
            .get(sel_client_key)
            .map(|c| c.state.is_pip)
            .unwrap_or(false);

        let _ = self.set_client_pip(backend, sel_client_key, !is_pip)?;

        Ok(())
    }
}

#[cfg(test)]
mod configured_feature_gate_tests {
    use super::configured_feature_toggle_allowed;

    #[test]
    fn disabled_feature_blocks_entry_but_preserves_exit() {
        assert!(configured_feature_toggle_allowed(false, true));
        assert!(configured_feature_toggle_allowed(true, true));
        assert!(configured_feature_toggle_allowed(true, false));
        assert!(!configured_feature_toggle_allowed(false, false));
    }
}

#[cfg(test)]
mod recording_finalization_tests {
    use super::finalize_concat_segments;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_PATH: AtomicU64 = AtomicU64::new(0);

    fn scratch_dir() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "jwm-record-finalize-test-{}-{}",
            std::process::id(),
            NEXT_PATH.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn failed_concat_preserves_every_source_segment() {
        let scratch = scratch_dir();
        let first = scratch.join("first.mp4");
        let second = scratch.join("second.mp4");
        let list = scratch.join("output.concat.txt");
        std::fs::write(&first, b"first").unwrap();
        std::fs::write(&second, b"second").unwrap();
        let segments = vec![
            first.to_string_lossy().into_owned(),
            second.to_string_lossy().into_owned(),
        ];

        let error = finalize_concat_segments(
            &list,
            "file 'first.mp4'\nfile 'second.mp4'",
            &scratch.join("output.mp4").to_string_lossy(),
            &segments,
            |_| Err("encoder failed".to_owned()),
        )
        .unwrap_err();

        assert_eq!(error, "encoder failed");
        assert!(first.is_file());
        assert!(second.is_file());
        assert!(!list.exists());
        std::fs::remove_dir_all(scratch).unwrap();
    }

    #[test]
    fn successful_concat_removes_inputs_but_never_its_output() {
        let scratch = scratch_dir();
        let first = scratch.join("first.mp4");
        let output = scratch.join("output.mp4");
        let list = scratch.join("output.concat.txt");
        std::fs::write(&first, b"first").unwrap();
        std::fs::write(&output, b"existing segment").unwrap();
        let segments = vec![
            first.to_string_lossy().into_owned(),
            output.to_string_lossy().into_owned(),
        ];

        finalize_concat_segments(
            &list,
            "file 'first.mp4'\nfile 'output.mp4'",
            &output.to_string_lossy(),
            &segments,
            |_| {
                std::fs::write(&output, b"merged").unwrap();
                Ok(())
            },
        )
        .unwrap();

        assert!(!first.exists());
        assert_eq!(std::fs::read(&output).unwrap(), b"merged");
        assert!(!list.exists());
        std::fs::remove_dir_all(scratch).unwrap();
    }
}

#[cfg(test)]
mod shell_entry_tests {
    use super::{
        ShellEntry, control_snapshot_epoch_matches, shell_entry, should_start_control_snapshot,
    };

    #[test]
    fn an_empty_screen_just_opens() {
        for locked in [false, true] {
            for mine in [false, true] {
                assert_eq!(shell_entry(false, locked, mine), ShellEntry::Open);
            }
        }
    }

    #[test]
    fn a_panel_key_pressed_over_its_own_panel_takes_it_down() {
        assert_eq!(shell_entry(true, false, true), ShellEntry::Dismiss);
    }

    #[test]
    fn a_panel_key_pressed_over_another_panel_takes_the_screen() {
        // Alt+F10 over Alt+F9's calendar: the hub replaces it rather than the
        // press going nowhere. This is what makes the panels read as one
        // surface with several pages.
        assert_eq!(shell_entry(true, false, false), ShellEntry::TakeOver);
    }

    #[test]
    fn nothing_takes_the_screen_from_the_lock_card() {
        // Not just the keyboard path: `jwm_remote` can call an opener by name
        // while the session is locked, and a lock any of them could replace
        // would not be a lock.
        for mine in [false, true] {
            assert_eq!(shell_entry(true, true, mine), ShellEntry::Refuse);
        }
    }

    #[test]
    fn control_snapshot_refreshes_are_coalesced_and_epoch_guarded() {
        let now = std::time::Instant::now();
        assert!(should_start_control_snapshot(false, None, now));
        assert!(!should_start_control_snapshot(true, None, now));
        assert!(!should_start_control_snapshot(false, Some(now), now));

        assert!(control_snapshot_epoch_matches(7, 7));
        assert!(!control_snapshot_epoch_matches(7, 8));
    }

    #[test]
    fn shell_hub_build_and_open_paths_do_not_read_external_controls_inline() {
        const SOURCE: &str = include_str!("toggles.rs");
        let build = SOURCE
            .split_once("fn build_shell_hub_state")
            .unwrap()
            .1
            .split_once("fn wallpaper_picker_state")
            .unwrap()
            .0;
        for forbidden in [
            "volume_state()",
            "brightness_percent()",
            "power::profiles()",
            "AudioDefaults::read()",
        ] {
            assert!(
                !build.contains(forbidden),
                "Shell Hub build regained blocking call {forbidden}"
            );
        }

        let open_paths = SOURCE
            .split_once("pub(crate) fn begin_shell_from_status_bar")
            .unwrap()
            .1
            .split_once("pub(crate) fn session_menu")
            .unwrap()
            .0;
        assert!(!open_paths.contains("AudioDefaults::read()"));
    }
}
