//! 特性切换功能
//!
//! 这个模块包含所有窗口管理器特性的切换函数（toggle* 系列）

use crate::backend::api::{Backend, MaximizeAxes};
use crate::backend::common_define::{EventMaskBits, Mods, StdCursorKind};
use crate::config::CONFIG;
use crate::core::animation::AnimationKind;
use crate::core::maximize::MaximizeOrigin;
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

/// The wallpaper picker's status line while its directory is listed.
const WALLPAPER_SCANNING: &str = "Scanning\u{2026}";
/// The wallpaper picker's status line when no worker could list it.
const WALLPAPER_SCAN_REFUSED: &str = "Could not start the wallpaper scan";

/// Replace the wallpaper picker's status line. `SystemUiState` has no setter
/// for this list kind; the scanning state is the only writer.
fn set_wallpaper_picker_message(state: &mut SystemUiState, text: &str) {
    if let SystemUiState::ListPanel {
        kind: crate::jwm::features::system_ui::ListKind::Wallpaper,
        message,
        ..
    } = state
    {
        *message = text.to_string();
    }
}
const RECORDING_CONCAT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5 * 60);

pub(crate) const fn configured_feature_toggle_allowed(active: bool, enabled: bool) -> bool {
    // Config flags gate entry only. An already-active mode must always retain
    // its exit path so it can release input grabs and compositor state.
    active || enabled
}

/// Where the overview selection lands once the entries whose window has gone
/// are dropped: on the same window when it survived, otherwise on the
/// survivor that took its place (the next one, or the last when it was at
/// the end). `alive` is one flag per entry, in list order. `None` when
/// nothing survived.
fn overview_index_after_prune(alive: &[bool], index: usize) -> Option<usize> {
    let survivors = alive.iter().filter(|&&alive| alive).count();
    let last = survivors.checked_sub(1)?;
    let before = alive.iter().take(index).filter(|&&alive| alive).count();
    Some(before.min(last))
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

/// Whether a status-bar ShellHub request targets the surface already on screen.
///
/// Hub home (`None`) mirrors `Alt+F10`: the hub itself, or any child page
/// reached from it (`system_ui_return_to_hub`). A named route matches only that
/// page — the same predicate each panel key uses for `toggle_off_system_ui`.
fn status_bar_shell_is_mine(
    return_to_hub: bool,
    state: &crate::jwm::features::SystemUiState,
    route: Option<crate::jwm::features::ShellHubRoute>,
) -> bool {
    use crate::jwm::features::ShellHubRoute;

    match route {
        None => return_to_hub || state.is_control_center(),
        Some(ShellHubRoute::Applications) => state.is_launcher(),
        Some(ShellHubRoute::Notifications) => state.is_notification_center(),
        Some(ShellHubRoute::Clipboard) => state.is_clipboard_picker(),
        Some(ShellHubRoute::Calendar) => state.is_calendar(),
        Some(ShellHubRoute::Wallpaper) => state.is_wallpaper_picker(),
        Some(ShellHubRoute::Theme) => state.is_theme_picker(),
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

/// Whether closing a Bluetooth pairing session should kick a fresh device
/// read.
///
/// Only an inbound window a device actually rang can have left a new bond
/// behind — an outbound session re-reads from `bluetooth_pairing_done`, and a
/// window nothing rang has no address to refresh for. And never while a read
/// is already in flight: replacing that handle only drops the notifier, so the
/// worker — and the real `Adapter1.StartDiscovery` session behind an `s`-key
/// scan — runs on with nowhere to land and what it heard is thrown away. The
/// `bluetooth_pairing_done` handler and the `s`/`r` keys coalesce for exactly
/// that reason; a close must not be the one path that does not.
const fn should_refresh_after_pairing_close(
    inbound: bool,
    bound: bool,
    scan_in_flight: bool,
) -> bool {
    inbound && bound && !scan_in_flight
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

/// The command a session-menu action (suspend, hibernate, reboot, shutdown)
/// runs.
///
/// The menu is driven from the event thread, whose SIGCHLD stays blocked for
/// the run loop's signalfd, and std hands that mask to the child. Only the
/// mask is reset: the command stays in JWM's session (no `setsid`).
fn session_action_command(program: &str, args: &[String]) -> Command {
    let mut command = Command::new(program);
    command.args(args);
    crate::external_command::unblock_sigchld_in_child(&mut command);
    command
}

impl Jwm {
    /// Adjust the default sink volume by the binding's Int argument
    /// (percentage points) and show the OSD with the estimate at once. The
    /// controls worker applies the real change off the event thread; its
    /// read-back refreshes the card if the estimate drifted, or takes it
    /// back if the change failed.
    pub(crate) fn volume_adjust(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let delta = match arg {
            WMArgEnum::Int(delta) if *delta != 0 => *delta,
            _ => 5,
        };
        let Some((seq, estimate)) = self.queue_volume_request(
            crate::jwm::features::system_controls::ControlRequest::VolumeAdjust(delta),
        ) else {
            return Err("no working volume control (wpctl/pactl/amixer)".into());
        };
        match estimate {
            Some(state) => self.show_volume_osd(backend, state),
            // Nothing has ever been read to estimate from: the read-back
            // covering this submission draws the first card.
            None => self.features.control_feedback.owe_osd(
                crate::jwm::features::system_controls::ControlDomain::Volume,
                seq,
            ),
        }
        Ok(())
    }

    /// Toggle the default sink's mute state and show the OSD with the
    /// estimate; the worker performs the toggle off the event thread. A
    /// toggle is an event, not a value: the worker never folds it into a
    /// level (only an adjacent twin cancels it — two flips are no flip).
    pub(crate) fn volume_mute(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some((seq, estimate)) = self.queue_volume_request(
            crate::jwm::features::system_controls::ControlRequest::VolumeToggleMute,
        ) else {
            return Err("no working volume control (wpctl/pactl/amixer)".into());
        };
        match estimate {
            Some(state) => self.show_volume_osd(backend, state),
            None => self.features.control_feedback.owe_osd(
                crate::jwm::features::system_controls::ControlDomain::Volume,
                seq,
            ),
        }
        Ok(())
    }

    /// Toggle the default microphone's mute state (XF86AudioMicMute) and show
    /// the mic OSD with the estimate; the worker performs the toggle off the
    /// event thread. Same event semantics as [`Self::volume_mute`]: never
    /// folded into another request, cancelled only by an adjacent twin. The
    /// mic card carries no bar, so unlike the volume OSD it shows the flag
    /// alone.
    pub(crate) fn toggle_mic_mute(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some((seq, estimate)) = self.queue_mic_request(
            crate::jwm::features::system_controls::ControlRequest::MicMuteToggle,
        ) else {
            return Err("no working audio control (wpctl/pactl/amixer)".into());
        };
        match estimate {
            Some(muted) => self.show_mic_osd(backend, muted),
            None => self.features.control_feedback.owe_osd(
                crate::jwm::features::system_controls::ControlDomain::MicMute,
                seq,
            ),
        }
        Ok(())
    }

    /// Adjust the backlight by the binding's Int argument (percentage
    /// points) and show the OSD with the estimate; the worker applies the
    /// real change off the event thread.
    pub(crate) fn brightness_adjust(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let delta = match arg {
            WMArgEnum::Int(delta) if *delta != 0 => *delta,
            _ => 5,
        };
        let Some((seq, estimate)) = self.queue_brightness_request(
            crate::jwm::features::system_controls::ControlRequest::BrightnessAdjust(delta),
        ) else {
            return Err("no backlight control (brightnessctl or /sys/class/backlight)".into());
        };
        match estimate {
            Some(percent) => {
                self.features.control_feedback.note_osd_shown(
                    crate::jwm::features::system_controls::ControlDomain::Brightness,
                    percent,
                    false,
                    std::time::Instant::now(),
                );
                backend.compositor_show_osd(crate::backend::api::OsdKind::Brightness, percent);
            }
            None => self.features.control_feedback.owe_osd(
                crate::jwm::features::system_controls::ControlDomain::Brightness,
                seq,
            ),
        }
        Ok(())
    }

    /// Queue a volume mutation on the controls worker and draw the estimate
    /// into the control snapshot (through which the control-center row reads
    /// it). Returns the submission's sequence and the estimate.
    ///
    /// `None` when no working volume tool is known to exist or the worker
    /// thread does not — the cases the synchronous path returned `None` for,
    /// so callers keep their old error/no-op behavior. A `None` estimate
    /// means nothing was ever read to estimate from; the caller may owe the
    /// OSD to the read-back instead of inventing a level.
    pub(crate) fn queue_volume_request(
        &mut self,
        request: crate::jwm::features::system_controls::ControlRequest,
    ) -> Option<(
        u64,
        Option<crate::jwm::features::system_controls::AudioState>,
    )> {
        use crate::jwm::features::system_controls;
        if system_controls::volume_tool_known_absent() {
            return None;
        }
        let confirmed = self
            .features
            .control_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.volume);
        // A chain of quick presses estimates from the estimate already on
        // screen, so a repeat storm follows its own display.
        let base = self.features.control_feedback.volume_shown().or(confirmed);
        let estimate = system_controls::optimistic_volume(base, &request);
        let seq =
            system_controls::queue_control_request(request, self.async_update_notifier.clone())?;
        if let Some(estimate) = estimate {
            self.features
                .control_feedback
                .note_volume_estimate(seq, estimate, confirmed);
            self.cache_control_volume(estimate);
        }
        Some((seq, estimate))
    }

    /// The brightness counterpart of [`Self::queue_volume_request`].
    pub(crate) fn queue_brightness_request(
        &mut self,
        request: crate::jwm::features::system_controls::ControlRequest,
    ) -> Option<(u64, Option<u8>)> {
        use crate::jwm::features::system_controls;
        if system_controls::brightness_tool_known_absent() {
            return None;
        }
        let confirmed = self
            .features
            .control_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.brightness);
        let base = self
            .features
            .control_feedback
            .brightness_shown()
            .or(confirmed);
        let estimate = system_controls::optimistic_brightness(base, &request);
        let seq =
            system_controls::queue_control_request(request, self.async_update_notifier.clone())?;
        if let Some(estimate) = estimate {
            self.features
                .control_feedback
                .note_brightness_estimate(seq, estimate, confirmed);
            self.cache_control_brightness(estimate);
        }
        Some((seq, estimate))
    }

    /// The microphone counterpart of [`Self::queue_volume_request`]. The
    /// confirmed base is the snapshot's mic flag — there is no estimate
    /// without one, since a flip needs something to flip — and the
    /// known-absent peek is the volume tool's own: sink and source share
    /// the one detected tool chain.
    pub(crate) fn queue_mic_request(
        &mut self,
        request: crate::jwm::features::system_controls::ControlRequest,
    ) -> Option<(u64, Option<bool>)> {
        use crate::jwm::features::system_controls;
        if system_controls::volume_tool_known_absent() {
            return None;
        }
        let confirmed = self
            .features
            .control_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.mic_muted);
        let base = self.features.control_feedback.mic_shown().or(confirmed);
        let estimate = system_controls::optimistic_mic_mute(base, &request);
        let seq =
            system_controls::queue_control_request(request, self.async_update_notifier.clone())?;
        if let Some(estimate) = estimate {
            self.features
                .control_feedback
                .note_mic_estimate(seq, estimate, confirmed);
            self.cache_control_mic_mute(estimate);
        }
        Some((seq, estimate))
    }

    /// Queue a power-profile switch on the controls worker and draw it on the
    /// row at once. `available` is the cached list the caller picked `name`
    /// from: both callers (the Hub row and IPC `set_power_profile`) validate
    /// against it, so nothing on this path runs `powerprofilesctl`. Returns
    /// the submission's sequence, or `None` when no worker thread exists — in
    /// which case nothing was queued or drawn.
    ///
    /// The drawn profile stands until the worker's re-read lands: the cache
    /// write bumps the snapshot epoch, so a snapshot read already in flight
    /// cannot roll it back, and [`Self::adopt_power_profile_report`] then
    /// shows what really took.
    pub(crate) fn queue_power_profile_request(
        &mut self,
        available: Vec<String>,
        name: String,
    ) -> Option<u64> {
        use crate::jwm::features::system_controls;
        let confirmed = self
            .features
            .control_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.power_profiles.clone());
        let seq = system_controls::queue_control_request(
            system_controls::ControlRequest::PowerProfileSet(name.clone()),
            self.async_update_notifier.clone(),
        )?;
        self.features
            .control_feedback
            .note_power_profile_estimate(seq, name.clone(), confirmed);
        self.cache_control_power_profiles(available, name);
        Some(seq)
    }

    /// Adopt the controls worker's newest read-backs: confirm the estimate
    /// on screen, correct it when the true value drifted, or revert it when
    /// the change failed outright. Runs from the frame tick; never blocks,
    /// and only re-syncs an open panel when a confirmed value actually moved.
    pub(crate) fn poll_control_feedback(&mut self) {
        use crate::jwm::features::system_controls::{self, FeedbackAction};

        let Some(report) = system_controls::take_control_report() else {
            return;
        };
        let now = std::time::Instant::now();
        let mut panel_changed = false;

        if let Some(volume) = report.volume {
            match self.features.control_feedback.resolve_volume(volume, now) {
                FeedbackAction::Adopt(state) => {
                    let current = self
                        .features
                        .control_snapshot
                        .as_ref()
                        .and_then(|snapshot| snapshot.volume);
                    if current != Some(state) {
                        self.cache_control_volume(state);
                        panel_changed = true;
                    }
                }
                FeedbackAction::Revert(previous) => {
                    // A failed change must not leave its estimate on screen:
                    // restore the last confirmed value, which may be "no
                    // row" when nothing was ever read.
                    let current = self
                        .features
                        .control_snapshot
                        .as_ref()
                        .and_then(|snapshot| snapshot.volume);
                    if current != previous {
                        self.mutate_control_snapshot(|snapshot| snapshot.volume = previous);
                        panel_changed = true;
                    }
                    log::debug!("[controls] volume change did not take; estimate reverted");
                }
                FeedbackAction::KeepEstimate => {}
            }
        }

        if let Some(brightness) = report.brightness {
            match self
                .features
                .control_feedback
                .resolve_brightness(brightness, now)
            {
                FeedbackAction::Adopt(percent) => {
                    let current = self
                        .features
                        .control_snapshot
                        .as_ref()
                        .and_then(|snapshot| snapshot.brightness);
                    if current != Some(percent) {
                        self.cache_control_brightness(percent);
                        panel_changed = true;
                    }
                }
                FeedbackAction::Revert(previous) => {
                    let current = self
                        .features
                        .control_snapshot
                        .as_ref()
                        .and_then(|snapshot| snapshot.brightness);
                    if current != previous {
                        self.mutate_control_snapshot(|snapshot| snapshot.brightness = previous);
                        panel_changed = true;
                    }
                    log::debug!("[controls] brightness change did not take; estimate reverted");
                }
                FeedbackAction::KeepEstimate => {}
            }
        }

        if let Some(mic) = report.mic {
            match self.features.control_feedback.resolve_mic(mic, now) {
                FeedbackAction::Adopt(muted) => {
                    let current = self
                        .features
                        .control_snapshot
                        .as_ref()
                        .and_then(|snapshot| snapshot.mic_muted);
                    if current != Some(muted) {
                        self.cache_control_mic_mute(muted);
                        // The control-center Input row reads the flag, so a
                        // move repaints an open panel like any other row's.
                        panel_changed = true;
                    }
                }
                FeedbackAction::Revert(previous) => {
                    let current = self
                        .features
                        .control_snapshot
                        .as_ref()
                        .and_then(|snapshot| snapshot.mic_muted);
                    if current != previous {
                        // A confirmed bool publishes `audio/mic`; reverting
                        // to "never read" only clears the cache — there is
                        // no honest bool to broadcast.
                        match previous {
                            Some(muted) => self.cache_control_mic_mute(muted),
                            None => self.mutate_control_snapshot(|snapshot| {
                                snapshot.mic_muted = None;
                            }),
                        }
                        panel_changed = true;
                    }
                    log::debug!("[controls] mic mute change did not take; estimate reverted");
                }
                FeedbackAction::KeepEstimate => {}
            }
        }

        if let Some(audio) = report.audio {
            self.adopt_audio_switch(audio);
        }

        if let Some(profile) = report.power_profile {
            self.adopt_power_profile_report(&profile, now);
        }

        if panel_changed {
            self.refresh_open_control_center();
        }
    }

    /// Adopt the worker's answer to a device switch: the picker's marker and
    /// status line follow the re-read, never the request, and the cached
    /// topology moves with them so the control-center rows and
    /// `get_audio_devices` agree. Runs from the frame tick via
    /// [`Self::poll_control_feedback`]; never blocks.
    fn adopt_audio_switch(&mut self, report: crate::jwm::features::system_controls::AudioReport) {
        use crate::jwm::features::system_controls;

        let verdict = system_controls::audio_switch_verdict(&report);
        match (verdict.took, verdict.in_use.as_deref()) {
            (true, Some(name)) => {
                log::info!("audio: {} is now {name}", report.direction.label());
                // Named OSD only when the re-read says the switch took — never
                // on queue / "Switching…". Poll has no backend, so the card
                // rides the pending-OSD slot flushed by `flush_system_ui`.
                self.features.control_feedback.queue_audio_device_osd(
                    matches!(report.direction, system_controls::AudioDirection::Input),
                    name.to_string(),
                );
            }
            (false, Some(name)) => {
                log::warn!(
                    "audio: {} stayed on {name} after asking for {}",
                    report.direction.label(),
                    report.asked_id
                );
            }
            _ => {
                log::debug!(
                    "audio: switch #{} of {} resolved with no default reporting",
                    report.seq,
                    report.direction.label()
                );
            }
        }
        if self.features.system_ui.audio_picker_direction() == Some(report.direction) {
            self.features
                .system_ui
                .set_audio_devices(report.direction, report.inventory.devices(report.direction));
            self.features
                .system_ui
                .set_audio_message(report.direction, verdict.message);
            self.mark_system_ui_dirty();
        }
        // The epoch bump discards a worker read that was already in flight,
        // so the pre-switch default marker cannot roll the row back.
        let old_defaults = self
            .features
            .control_snapshot
            .as_ref()
            .map(|snapshot| snapshot.audio_defaults.clone());
        let new_defaults = report.inventory.defaults();
        let devices_payload = system_controls::audio_inventory_json(&report.inventory);
        self.cache_control_audio_inventory(report.inventory);
        // Bars subscribe to `audio/devices`; publish after every resolved
        // switch (took or not) so a poll right after an IPC / picker flip
        // sees the re-read, not the pre-switch marker.
        self.broadcast_ipc_event("audio/devices", devices_payload);
        if old_defaults.as_ref() != Some(&new_defaults) {
            self.refresh_open_control_center();
        }
    }

    /// Adopt the worker's answer to a power-profile switch: the row, the
    /// `power/profile` publish and any correction of a live card follow the
    /// re-read, never the request. Runs from the frame tick via
    /// [`Self::poll_control_feedback`]; never blocks.
    pub(crate) fn adopt_power_profile_report(
        &mut self,
        report: &crate::jwm::features::system_controls::PowerProfileReport,
        now: std::time::Instant,
    ) {
        use crate::jwm::features::system_controls::ProfileFeedback;

        let cached = self
            .features
            .control_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.power_profiles.clone());
        match self
            .features
            .control_feedback
            .resolve_power_profile(report, now)
        {
            ProfileFeedback::KeepEstimate => {}
            ProfileFeedback::Adopt {
                available,
                active,
                took,
            } => {
                if !took {
                    log::warn!(
                        "power: profile stayed on {active} after asking for {}",
                        report.asked
                    );
                }
                let read = (available, active.clone());
                let moved = cached.as_ref() != Some(&read);
                // Cached even when it matches what is shown: the epoch bump
                // discards a snapshot read that started before the switch
                // and would otherwise roll the row back when it lands.
                self.cache_control_power_profiles(read.0, read.1);
                if moved {
                    self.refresh_open_control_center();
                }
                // Every resolved switch publishes the verified profile, took
                // or not, as the synchronous IPC path did after its re-read.
                self.broadcast_ipc_event("power/profile", serde_json::json!({ "active": active }));
            }
            ProfileFeedback::Revert(previous) => {
                log::warn!(
                    "power: could not switch to {}; the profile tool refused",
                    report.asked
                );
                if let Some(previous) = previous
                    && cached.as_ref() != Some(&previous)
                {
                    let (available, active) = previous;
                    self.cache_control_power_profiles(available, active);
                    self.refresh_open_control_center();
                }
            }
            ProfileFeedback::KeepShown => {
                log::debug!(
                    "power: switched to {} but the profile list could not be re-read",
                    report.asked
                );
            }
        }
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
        self.features.control_feedback.note_osd_shown(
            crate::jwm::features::system_controls::ControlDomain::Volume,
            state.percent,
            state.muted,
            std::time::Instant::now(),
        );
        backend.compositor_show_osd(kind, state.percent);
    }

    /// The mic counterpart of [`Self::show_volume_osd`]: the card is the
    /// labeled toggle kind — a microphone has no bar, so the percent slot
    /// carries 0 and only the flag matters.
    pub(crate) fn show_mic_osd(&mut self, backend: &mut dyn Backend, muted: bool) {
        self.features.control_feedback.note_osd_shown(
            crate::jwm::features::system_controls::ControlDomain::MicMute,
            0,
            muted,
            std::time::Instant::now(),
        );
        backend.compositor_show_osd(crate::backend::api::OsdKind::MicMute(muted), 0);
    }

    /// The labeled Power Profile card for a switch just queued, noted so a
    /// re-read that contradicts it can refresh it in place.
    pub(crate) fn show_power_profile_osd(&mut self, backend: &mut dyn Backend, name: String) {
        self.features.control_feedback.note_osd_shown(
            crate::jwm::features::system_controls::ControlDomain::PowerProfile,
            0,
            false,
            std::time::Instant::now(),
        );
        backend.compositor_show_osd(crate::backend::api::OsdKind::PowerProfile(name), 0);
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
        let mic_muted = controls.and_then(|snapshot| snapshot.mic_muted);
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
                ui_theme: Some(cfg.ui_theme()),
                media: self.features.media.get(),
                volume,
                brightness,
                audio_output: audio_defaults.and_then(|defaults| {
                    defaults.name(crate::jwm::features::system_controls::AudioDirection::Output)
                }),
                audio_input: audio_defaults.and_then(|defaults| {
                    defaults.name(crate::jwm::features::system_controls::AudioDirection::Input)
                }),
                mic_muted,
                battery: self.features.battery.as_ref(),
                resources: behavior.resource_rows.then_some(&self.features.resources),
                network: self.features.connectivity.network.as_ref(),
                bluetooth: Some(&self.features.connectivity.bluetooth),
                power_profile: profiles.map(|(_, active)| active.as_str()),
                night_light: self.night_light_active(),
                do_not_disturb: self.do_not_disturb,
                idle_inhibited: self.idle_inhibited,
                can_lock_monitor: self.can_lock_another_monitor(),
                locked_monitor: self.features.monitor_lock.latest(),
            },
        )
    }

    /// Adopt one completed slow-control snapshot without ever waiting for it.
    /// A user mutation increments the epoch, so an older worker result is
    /// discarded instead of rolling the visible value back.
    pub(crate) fn poll_control_snapshot_job(&mut self) {
        // The controls worker's read-backs are the other writer of this
        // snapshot; adopting them rides the same per-tick poll.
        self.poll_control_feedback();
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

    /// Cache the shown mic-mute flag and publish `audio/mic` so bars and
    /// scripts following the `audio` topic see the same optimistic /
    /// adopted value the control-center Input row does. Callers that only
    /// clear the flag back to unread use `mutate_control_snapshot` instead
    /// — a null is not an event payload.
    pub(crate) fn cache_control_mic_mute(&mut self, muted: bool) {
        self.mutate_control_snapshot(|snapshot| snapshot.mic_muted = Some(muted));
        self.broadcast_ipc_event("audio/mic", serde_json::json!({ "muted": muted }));
    }

    pub(crate) fn cache_control_power_profiles(&mut self, available: Vec<String>, active: String) {
        self.mutate_control_snapshot(|snapshot| {
            snapshot.power_profiles = Some((available, active));
        });
    }

    /// Open the wallpaper picker in its scanning state and list the
    /// directory on a worker.
    ///
    /// One readdir of a large `~/Pictures` — or of an NFS/sshfs mount — used
    /// to run right here on the event loop, and resolving the directory stats
    /// the configured paths too; both run on the worker now. The frame tick
    /// fills the rows in through [`Self::poll_wallpaper_listing_job`] while
    /// this picker is still up.
    fn wallpaper_picker_state(&mut self) -> crate::jwm::features::SystemUiState {
        use crate::jwm::features::wallpaper;

        let (current, configured_dir) = {
            let cfg = CONFIG.load();
            let behavior = cfg.behavior();
            (behavior.wallpaper.clone(), behavior.wallpaper_dir.clone())
        };
        let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/"));
        let worker_current = current.clone();
        let job = crate::jwm::features::connectivity::BackgroundJob::spawn(move || {
            let directory = wallpaper::resolve_directory(&configured_dir, &worker_current, &home);
            let paths = wallpaper::list_wallpapers(&directory);
            (directory, paths)
        });
        let mut state = crate::jwm::features::SystemUiState::wallpaper_picker(&[], &current, "");
        if job.started() {
            // Replacing an older listing detaches it: only the newest
            // opening's answer is adopted.
            self.features.wallpaper_listing = Some(self.track_background_job(job));
            set_wallpaper_picker_message(&mut state, WALLPAPER_SCANNING);
        } else {
            self.features.wallpaper_listing = None;
            set_wallpaper_picker_message(&mut state, WALLPAPER_SCAN_REFUSED);
        }
        state
    }

    /// Adopt a finished wallpaper listing into the picker that asked for it.
    ///
    /// A picker closed (or swapped for another page) in the meantime simply
    /// drops the answer. A handle whose thread the OS refused never fills, so
    /// it is dropped here with the reason on the panel rather than leaving
    /// the picker "Scanning…" for good.
    pub(crate) fn poll_wallpaper_listing_job(&mut self) {
        let Some(job) = self.features.wallpaper_listing.as_ref() else {
            return;
        };
        if !job.started() {
            self.features.wallpaper_listing = None;
            if self.features.system_ui.is_wallpaper_picker() {
                set_wallpaper_picker_message(&mut self.features.system_ui, WALLPAPER_SCAN_REFUSED);
                self.mark_system_ui_dirty();
            }
            return;
        }
        let Some((directory, paths)) = job.take() else {
            return;
        };
        self.features.wallpaper_listing = None;
        if !self.features.system_ui.is_wallpaper_picker() {
            return;
        }
        let current = CONFIG.load().behavior().wallpaper.clone();
        self.features.system_ui = crate::jwm::features::SystemUiState::wallpaper_picker(
            &paths,
            &current,
            &directory.to_string_lossy(),
        );
        self.mark_system_ui_dirty();
    }

    fn theme_picker_state() -> crate::jwm::features::SystemUiState {
        let current = CONFIG.load().ui_theme().to_string();
        crate::jwm::features::SystemUiState::theme_picker(&current)
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
            ShellHubRoute::Wallpaper => self.wallpaper_picker_state(),
            ShellHubRoute::Theme => Self::theme_picker_state(),
        };

        self.features.system_ui_return_to_hub = true;
        self.features.system_ui = next;
        self.sync_system_ui(backend);
        Ok(())
    }

    /// Open the shell from a status bar rather than from a key binding.
    ///
    /// `None` opens the hub home page; a route opens that page directly with
    /// Escape returning to the hub, matching what the keyboard path does. While
    /// a panel is already up the request follows the same toggle / hand-over
    /// rule as the panel keys: same page (or Hub home) dismisses, a different
    /// page takes over. This is the only shell entry point that starts from an
    /// unfocused surface, so unlike [`Self::open_shell_hub_route`] it has to
    /// acquire the grabs first and hand them back if the requested page turns
    /// out to be unavailable.
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
        // Mirror the panel keys while anything is already up: same page (or
        // Hub home / Alt+F10) dismisses; a different page hands over inside
        // `prepare_system_ui_inner`. The lock refuses via `toggle_off_system_ui`.
        if self.features.system_ui.is_active() {
            let mine = status_bar_shell_is_mine(
                self.features.system_ui_return_to_hub,
                &self.features.system_ui,
                route,
            );
            if self.toggle_off_system_ui(backend, |_| mine) {
                return Ok(true);
            }
        }

        // A status bar click holds an implicit pointer grab for as long as the
        // button is down, so a busy pointer here means "the click that asked
        // for this is still going on" — retryable, not a failure. This is the
        // only caller that wants that distinction; the other fourteen are
        // keyboard-invoked, where a busy pointer really is an error.
        // Motion comes with the grab, exactly as in the key-bound opener: the
        // hub's slider rows take press-drags, and without POINTER_MOTION in
        // the mask an X11 grab would never deliver a drag's movement.
        let label = route.map_or("shell hub (status bar)", |_| "shell page (status bar)");
        if !self.prepare_system_ui_inner(backend, label, SystemUiPointerGrab::ButtonsAndMotion)? {
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
                // After a hand-over the outgoing panel is already gone, so
                // this also clears the empty grab rather than restoring it.
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
        // Buttons and motion: the slider rows take press-drags, and without
        // POINTER_MOTION in the mask an X11 grab would never deliver the
        // drag's motion to the WM (the tags overview grabs the same way).
        self.prepare_system_ui(
            backend,
            "control center",
            SystemUiPointerGrab::ButtonsAndMotion,
        )?;
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
                self.features
                    .system_ui
                    .set_lock_now_playing(self.features.media.get());
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
        match session_action_command(&program, &args).spawn() {
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

    /// Queue the selected device as the default on the controls worker. The
    /// set plus the verifying re-read are two bounded-but-blocking spawns —
    /// the round-13 bug shape — so they run off the event thread; the picker's
    /// message acknowledges the press now, and the worker's report lands the
    /// marker on what actually took effect from the frame tick.
    pub(crate) fn use_selected_audio_device(&mut self, backend: &mut dyn Backend) {
        use crate::jwm::features::system_controls;

        let Some(direction) = self.features.system_ui.audio_picker_direction() else {
            return;
        };
        let Some(id) = self.features.system_ui.selected_audio_device() else {
            return;
        };
        let queued = system_controls::queue_control_request(
            system_controls::ControlRequest::AudioSetDefault { direction, id },
            self.async_update_notifier.clone(),
        );
        // The picker's rows carry no half-switched state worth inventing, so
        // the honest optimistic surface is the status line; the re-read lands
        // within a tick or two and moves the marker itself.
        let message = if queued.is_some() {
            "Switching\u{2026}"
        } else {
            "Could not switch device"
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
        self.prepare_system_ui(
            backend,
            "the wallpaper picker",
            SystemUiPointerGrab::Buttons,
        )?;
        // Built once the grabs are held, so a picker that cannot open never
        // starts a directory scan.
        let next = self.wallpaper_picker_state();
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

    /// Apply the selected UI theme through the same configuration path a
    /// `set_config` takes, so both compositors rebuild their palettes on the
    /// next `apply_config` exactly as they would from a reload, then surgically
    /// persist `appearance.ui_theme` to the live TOML (comments preserved).
    pub(crate) fn apply_selected_theme(&mut self, backend: &mut dyn Backend) {
        let Some(theme) = self.features.system_ui.selected_theme().map(str::to_string) else {
            return;
        };
        let mut updated = (**CONFIG.load()).clone();
        if let Err(error) = updated.set_value(
            "appearance.ui_theme",
            &serde_json::Value::String(theme.clone()),
        ) {
            error!("Theme: {error}");
            self.report_theme_failure(backend, "Theme not applied", error.to_string());
            return;
        }
        CONFIG.store(std::sync::Arc::new(updated));
        self.apply_config_changes(backend);
        // Stat the file before writing it: an edit the user saved within the
        // last poll interval is otherwise first seen *after* JWM's own write,
        // and settling that write would swallow it. Observed first, it stays
        // pending, and the reload of JWM's revision carries both the theme
        // and the edit.
        self.observe_config_reload(std::time::Instant::now(), "pre-save check");
        let persisted = CONFIG.load().persist_ui_theme(&theme);
        self.settle_theme_persist(backend, &theme, persisted);
        info!("Theme: {theme}");
        self.broadcast_ipc_event(
            "config/changed",
            serde_json::json!({ "key": "appearance.ui_theme", "value": theme }),
        );
        self.close_system_ui(backend);
    }

    /// Account for the theme write: settle JWM's own revision of the file,
    /// or say on screen that the theme was not saved.
    fn settle_theme_persist(
        &mut self,
        backend: &mut dyn Backend,
        theme: &str,
        persisted: Result<std::time::SystemTime, crate::config::ConfigError>,
    ) {
        match persisted {
            Ok(revision) => self.note_config_written_by_us(revision),
            Err(error) => {
                error!("Theme: failed to persist {theme}: {error}");
                // The theme is live, but only in memory: the next reload or
                // login brings the old one back. A refusal to edit the file
                // tells the user to set the key by hand, which does nothing
                // from the journal alone.
                self.report_theme_failure(backend, "Theme not saved", error.to_string());
            }
        }
    }

    /// Put a theme picker failure on screen. Otherwise it reaches only the
    /// journal: an unsaved theme closes the picker exactly as a saved one
    /// does. Through do-not-disturb, like the other failures that lose a
    /// setting.
    fn report_theme_failure(&mut self, backend: &mut dyn Backend, title: &str, body: String) {
        self.push_system_toast(
            backend,
            crate::backend::api::ToastNotification {
                title: format!("\u{f1fc}  {title}"),
                body,
                urgency: 2,
                timeout_ms: 8000,
                ..Default::default()
            },
        );
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
        let Some(entry) = self.features.clipboard.get(index).cloned() else {
            return;
        };
        let offered = match &entry {
            crate::jwm::features::ClipboardEntry::Text { text, .. } => {
                backend.set_clipboard_text(text)
            }
            crate::jwm::features::ClipboardEntry::Png { bytes, .. } => {
                self.offer_clipboard_png(backend, bytes)
            }
        };
        if offered {
            // Copying an old entry makes it the most recent one, exactly as
            // if the user had copied it again from the source.
            match entry {
                crate::jwm::features::ClipboardEntry::Text { text, .. } => {
                    self.record_clipboard(&text);
                }
                crate::jwm::features::ClipboardEntry::Png { bytes, .. } => {
                    self.record_clipboard_png(&bytes);
                }
            }
            self.close_system_ui(backend);
        } else {
            self.features
                .system_ui
                .set_clipboard_message("this backend cannot set the clipboard");
            self.sync_system_ui(backend);
        }
    }

    /// Forget the selected entry — keyboard `d` / Delete and middle-click
    /// on a row share this path. One shot, no arm; never clear-all.
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
                let inbound = session.kind() == crate::jwm::features::pairing::PairingKind::Inbound;
                if inbound {
                    // An armed window reaching its deadline is the expected way
                    // it ends, not a helper that vanished — the inbound
                    // helper's own `done` only ever arrives at teardown, after
                    // this timer has already fired. Keep
                    // `cancel_bluetooth_pairing`'s "Not accepting incoming
                    // requests" rather than overwriting it with a "Pairing
                    // timed out" that describes a pairing nothing asked for; a
                    // device that did ring and bind is refreshed from there.
                    log::info!("Bluetooth: inbound window closed");
                    self.cancel_bluetooth_pairing();
                } else {
                    // The helper vanished without a `done`: end the session.
                    log::warn!("Bluetooth: pairing helper never reported back");
                    self.cancel_bluetooth_pairing();
                    self.features
                        .system_ui
                        .set_bluetooth_message("Pairing timed out");
                }
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

        // A job the OS refused a thread for will never publish a list, and
        // the picker's `s`/`r` keys coalesce on this slot being empty. Drop
        // the dead handle so those keys work again instead of reading as "a
        // scan is already running" for as long as the picker stays open.
        if let Some(job) = &self.features.bluetooth_scan
            && !job.started()
        {
            self.features.bluetooth_scan = None;
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
                    // Re-read so the row shows the state that actually took —
                    // unless a scan is already running. Replacing its handle
                    // only drops the notifier: the worker, and the real
                    // `Adapter1.StartDiscovery` session behind an `s`-key
                    // scan, runs on with nowhere to land and what it heard is
                    // thrown away. `bluetooth_pairing_done` and the `s`/`r`
                    // keys coalesce for exactly that reason.
                    let scanning = crate::jwm::features::connectivity::job_in_flight(
                        self.features.bluetooth_scan.as_ref(),
                    );
                    if !scanning
                        && let Some(scan) = crate::jwm::features::connectivity::start_device_scan()
                    {
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
        // A device rang this window and bound it, so a bond may just have
        // landed; once the window is gone, re-read the list so the newly
        // paired device shows without the user pressing `r`.
        let refresh_after_bind = should_refresh_after_pairing_close(
            inbound,
            session.address().is_some(),
            crate::jwm::features::connectivity::job_in_flight(
                self.features.bluetooth_scan.as_ref(),
            ),
        );
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
        // The scan is adopted by `poll_bluetooth_jobs`; if the picker has
        // already closed (this is also called from `close_system_ui`) that
        // poll drops the handle, which is harmless.
        if refresh_after_bind
            && let Some(scan) = crate::jwm::features::connectivity::start_device_scan()
        {
            self.features.bluetooth_scan = Some(self.track_background_job(scan));
        }
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
                    // The SSID is byte-exact so `nmcli` gets what it needs;
                    // a log line is a place it is *read*, and an access
                    // point's owner chooses those bytes, so an ESC in one
                    // would reach whatever terminal is tailing the journal.
                    log::info!(
                        "Wi-Fi: joined {}",
                        crate::jwm::features::connectivity::display_ssid(&ssid)
                    );
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
        // A forget in flight owns the air: joining now would race the delete
        // and its completion re-read. `forget_selected_wifi` guards both
        // directions; this arm is the recorded asymmetry's other half.
        if connectivity::job_in_flight(self.features.wifi_forget.as_ref()) {
            return;
        }
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

        // The SSID handed to nmcli stays byte-exact; the copy on the status
        // row does not, so an access point's control bytes cannot draw there.
        self.features.system_ui.set_wifi_message(format!(
            "Connecting to {}\u{2026}",
            connectivity::display_ssid(&ssid)
        ));
        let job = connectivity::start_connect(&ssid, &plan, passphrase.clone());
        self.features.wifi_connect = Some(self.track_background_job(job));
        if let Some(secret) = passphrase.as_mut() {
            // The worker owns its own copy; wipe ours rather than dropping it.
            unsafe { secret.as_bytes_mut().fill(0) };
        }
        self.sync_system_ui(backend);
    }

    /// Toggle the Wi-Fi radio. The flip runs on a worker (nmcli/rfkill can
    /// block for seconds behind a wedged bus); the OSD acknowledges the press
    /// at once with the requested target, and the row confirms from the
    /// worker's post-set re-read — GNOME's immediate-acknowledgement shape.
    pub(crate) fn toggle_wifi(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        use crate::jwm::features::connectivity::{self, RadioKind};

        let enabled = !self
            .features
            .connectivity
            .network
            .as_ref()
            .is_some_and(|state| state.wifi_enabled);
        if connectivity::radio_tool_known_absent(RadioKind::Wifi) {
            return Err("no working Wi-Fi control (nmcli or rfkill)".into());
        }
        backend.compositor_show_osd(crate::backend::api::OsdKind::Wifi(enabled), 0);
        self.request_radio_set(RadioKind::Wifi, enabled);
        Ok(())
    }

    /// Toggle the Bluetooth controller. Same shape as [`Self::toggle_wifi`]:
    /// queued off the event thread, acknowledged by the OSD with the
    /// requested target, confirmed by the re-read.
    pub(crate) fn toggle_bluetooth(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        use crate::jwm::features::connectivity::{self, RadioKind};

        let enabled = !self.features.connectivity.bluetooth.powered;
        if connectivity::radio_tool_known_absent(RadioKind::Bluetooth) {
            return Err("no working Bluetooth control (bluetoothctl or rfkill)".into());
        }
        backend.compositor_show_osd(crate::backend::api::OsdKind::Bluetooth(enabled), 0);
        self.request_radio_set(RadioKind::Bluetooth, enabled);
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
        backend.compositor_show_osd(crate::backend::api::OsdKind::NightLight(enabled), 0);
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
        // A shade is not a panel and does not go away when one opens, but a
        // panel drawn on a locked monitor would be: the shade is above it.
        // The selection is never on a locked monitor, so this only fires for
        // a caller that reached past it.
        if self
            .state
            .sel_mon
            .is_some_and(|key| self.monitor_key_is_locked(key))
        {
            return Err(format!("{label} cannot open on a locked monitor").into());
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

        // Expose, the tag overview, annotation and the two capture selectors
        // are not system-UI panels, but each holds its own keyboard/pointer
        // grab and draws its own overlay. A shell key reaches this common
        // opener while one of them is up (the expose key branch deliberately
        // falls through for unhandled keys), and so do the idle lock and IPC,
        // whatever is on screen. The grab a panel takes below would silently
        // replace theirs; then `close_system_ui`'s ungrab drops it, leaving
        // the mode still drawn with no grabs and no way out but its own
        // toggle. Tear them down first, through the same list the compositor
        // disable uses.
        let closed_a_mode = self.close_grab_holding_modes(backend)?;

        // Any other panel still on screen at this point means a hand-over: one
        // shell key pressed while another key's panel was up. The keyboard and
        // the compositor are already ours, so inherit them rather than
        // releasing and reacquiring, which would flap a temporarily leased
        // compositor and open a window for the desktop to take the keyboard
        // back mid-swap.
        if self.features.system_ui.is_active() {
            // Except when a capture selector was up over the panel (entered
            // over IPC, which no modal state gates): its teardown above
            // ungrabbed the keyboard, and an X11 grab is not
            // reference-counted, so the panel's grab went with it. Inheriting
            // nothing would put the incoming panel — the session lock, say —
            // on screen with every keystroke still reaching the focused
            // client. Take the keyboard back first; if it cannot be had, the
            // outgoing panel has lost it too, so it is closed rather than
            // left on screen deaf, and the opener fails as it would from an
            // empty screen.
            if closed_a_mode
                && let Some(root) = backend.root_window()
                && let Err(error) = backend.key_ops().grab_keyboard(root)
            {
                self.close_system_ui(backend);
                return Err(error.into());
            }
            // The pointer is usually already held (every clickable shell
            // panel grabs Buttons or more). Re-grabbing costs a round-trip
            // and always succeeds for the client that already holds it, so
            // ask before anything is torn down — a refusal has to leave the
            // panel on screen alone. The rare keyboard-only open (window
            // switcher when another client holds the pointer) taking over
            // from a grabbed panel simply stays more modal than it asked
            // to be, and `close_system_ui` hands both back.
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

    pub(crate) fn release_temporary_system_ui_compositor(
        &mut self,
        backend: &mut dyn Backend,
        label: &str,
    ) {
        if !self.features.system_ui_temporary_compositor {
            return;
        }
        // A lock shade is drawn by the compositor and outlives every panel.
        // Handing the renderer back here would uncover the monitors it
        // covers; the lease is kept and released by the unlock that takes the
        // last shade down.
        if !self.features.monitor_lock.is_empty() {
            log::info!("Compositor lease kept after {label}: monitors are locked");
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
        // So does an armed control-center slider drag: once the pointer grab
        // is gone its release may never reach the WM.
        self.control_slider_drag = None;
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
        // And an armed slider drag: it belongs to the outgoing panel.
        self.control_slider_drag = None;
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
        if self.features.system_ui.is_session_lock() {
            return Ok(());
        }
        // A monitor's unlock prompt is a lock card, but it is not this lock:
        // it asks for one output's shade while the seat is still open. The
        // session lock outranks it and takes the screen — otherwise a prompt
        // left up would keep the idle timer from ever locking the session,
        // and `prepare_system_ui` refuses to replace any lock card. The shade
        // it was asking about stays up underneath.
        if self.features.system_ui.monitor_lock_target().is_some() {
            self.close_system_ui(backend);
        }
        // Any other panel still up is replaced, not waited out: the launcher,
        // the hub, a picker or the notification center stays until somebody
        // acts on it, so an idle lock that refused (and retried) behind one
        // left an unattended desk unlocked all night. `prepare_system_ui`
        // hands it over like any shell key would — running its own teardown
        // (a browsed layout goes back, a pairing is cancelled) and keeping the
        // keyboard grab and any leased compositor it already holds.
        //
        // On X11, never display a pretend lock if the exclusive keyboard grab
        // failed. Wayland-udev performs interception in its input pipeline.
        self.prepare_system_ui(backend, "lock screen", SystemUiPointerGrab::Buttons)?;
        self.features.system_ui = crate::jwm::features::SystemUiState::lock();
        self.features
            .system_ui
            .set_lock_now_playing(self.features.media.get());
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
        // Fullscreen and PiP own `is_floating` (and stash the state to return
        // to); flipping it underneath them tiled a window still flagged
        // fullscreen, border-less, and saved the monitor rect as its float
        // geometry. dwm refuses here too.
        if self
            .state
            .clients
            .get(sel_client_key)
            .is_some_and(|client| client.state.is_fullscreen || client.state.is_pip)
        {
            return Ok(());
        }
        // Maximize owns a maximized window's geometry and state atoms: end it
        // through its transaction first, so the atoms are cleared and the
        // pre-maximize rect (not the maximized one) becomes the floating rect
        // a later toggle back restores. A window that maximize promoted out
        // of the tiling is re-tiled by that same unmaximize; that is the
        // whole toggle.
        if self
            .state
            .clients
            .get(sel_client_key)
            .is_some_and(|client| client.state.is_maximize_realized())
        {
            self.set_client_maximized(
                backend,
                sel_client_key,
                MaximizeAxes::NONE,
                MaximizeOrigin::User,
            )?;
            if self
                .state
                .clients
                .get(sel_client_key)
                .is_some_and(|client| !client.state.is_floating)
            {
                return Ok(());
            }
        }
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
        let Some(sticky) = self
            .state
            .clients
            .get(sel_client_key)
            .map(|client| client.state.is_sticky)
        else {
            return Ok(());
        };
        self.set_client_sticky(backend, sel_client_key, !sticky);
        Ok(())
    }

    /// The one place a client becomes (un)sticky, for the key binding and
    /// for `_NET_WM_STATE_STICKY` requests alike: a sticky client adopts its
    /// monitor's current tags (and follows every tag switch from then on),
    /// the EWMH state and desktop property are mirrored for pagers and bars,
    /// and the monitor re-arranges so the change is visible at once.
    pub(crate) fn set_client_sticky(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        sticky: bool,
    ) {
        let Some((win, mon)) = self
            .state
            .clients
            .get(client_key)
            .map(|client| (client.win, client.mon))
        else {
            return;
        };
        let current_tags = mon
            .and_then(|mon_key| self.state.monitors.get(mon_key))
            .map(|monitor| monitor.get_active_tags());
        if let Some(client) = self.state.clients.get_mut(client_key) {
            client.state.is_sticky = sticky;
            if sticky && let Some(tags) = current_tags {
                client.state.tags = tags;
            }
        }
        let _ = backend
            .property_ops()
            .set_net_wm_state_flag(win, crate::backend::api::NetWmState::Sticky, sticky);
        let _ = self.setclienttagprop(backend, client_key);
        self.arrange(backend, mon);
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

        self.close_grab_holding_modes(backend)?;
        Ok(())
    }

    /// Take down every mode that holds its own keyboard/pointer grab and
    /// draws its own overlay: the overview prism, expose, annotation, the
    /// screenshot selector and the recording-region selector.
    ///
    /// Shared by the two places that must take the screen from all of them —
    /// a system UI panel opening and the compositor turning off — so the list
    /// cannot drift between them again. It did once: panels skipped both
    /// selectors, so a lock screen the idle timer put over a screenshot
    /// selection took its grabs, and the unlock handed them back, leaving the
    /// selector drawn and armed with nothing to finish or cancel it. Each arm
    /// is guarded by its own flag, so this is a no-op when nothing is up.
    ///
    /// Returns whether anything was taken down. Every exit here ungrabs
    /// input unconditionally, and an X11 grab is not reference-counted, so a
    /// caller that keeps a grab of its own across this call has to take it
    /// again when the answer is `true`.
    fn close_grab_holding_modes(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let mut closed = false;
        if self.features.overview.active {
            self.features.overview.deactivate();
            backend.compositor_set_overview_mode(false, &[]);
            let _ = backend.key_ops().ungrab_keyboard();
            closed = true;
        }
        if self.features.expose_active {
            self.apply_expose_action(backend, expose_plan::ExposeAction::Exit { focus: None })?;
            closed = true;
        }
        if self.features.annotation_active {
            self.features.annotation_active = false;
            self.features.annotation_drawing = false;
            backend.compositor_set_annotation_mode(false);
            let _ = backend.key_ops().ungrab_keyboard();
            let _ = backend.input_ops().ungrab_pointer();
            closed = true;
        }
        if self.features.screenshot.active {
            // As the selector's own cancel key does: a capture still parked
            // for the pointer belongs to the same request.
            self.features.deferred_grab = None;
            self.cancel_screenshot_select(backend);
            closed = true;
        }
        if self.features.recording.selecting_region {
            self.cancel_recording_region_interaction(backend);
            closed = true;
        }
        Ok(closed)
    }

    /// The grab-holding mode already on screen, named for a refusal.
    ///
    /// Overview, expose and annotation each take the keyboard (expose and
    /// annotation the pointer too), and each one's exit hands its grabs back
    /// unconditionally. Two of them up at once cannot end well: whichever
    /// exits first ungrabs input the other still needs, leaving it drawn
    /// with no way to reach its keys. Entering one while another is up is
    /// reachable — the expose and annotation key branches fall through to
    /// the global bindings on purpose, and IPC reaches every toggle — so the
    /// entry paths refuse instead of stacking. Only called on an entry path,
    /// where the entering mode's own flag is necessarily clear.
    pub(crate) fn grab_holding_mode_on_screen(&self) -> Option<&'static str> {
        let features = &self.features;
        if features.system_ui.is_active() {
            Some("a system UI panel")
        } else if features.overview.active {
            Some("the overview")
        } else if features.expose_active {
            Some("expose")
        } else if features.annotation_active {
            Some("screen annotation")
        } else if features.screenshot.active {
            Some("screenshot selection")
        } else if features.recording.selecting_region {
            Some("recording region selection")
        } else {
            None
        }
    }

    /// 切换合成器开关
    pub fn togglecompositor(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let enable = !backend.has_compositor();
        if !enable && !self.features.monitor_lock.is_empty() {
            // Same reason as the lease above: the shades would come off with
            // the renderer that draws them, uncovering monitors the user
            // locked. Refused rather than deferred — nothing here will close
            // on its own, so there is nothing to honour it later.
            return Err("monitors are locked; unlock them before disabling the compositor".into());
        }
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
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.do_not_disturb = !self.do_not_disturb;
        log::info!("DND {}", if self.do_not_disturb { "ON" } else { "OFF" });
        self.broadcast_ipc_event(
            "dnd/toggle",
            serde_json::json!({ "enabled": self.do_not_disturb }),
        );
        // The OSD is the honest surface for DND itself: toasts are DND-gated
        // (a "Do Not Disturb On" toast would be swallowed by the state it
        // announces), the OSD is not.
        backend.compositor_show_osd(
            crate::backend::api::OsdKind::DoNotDisturb(self.do_not_disturb),
            0,
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
        if !self.features.overview.active
            && let Some(mode) = self.grab_holding_mode_on_screen()
        {
            return Err(format!("overview cannot open while {mode} is active").into());
        }
        if self.features.overview.active {
            // End overview: focus the selected window and move it to the
            // front of its group (a tile becomes master; a floating window
            // only moves ahead of the other floating windows, see
            // `Jwm::move_to_front`).
            // A selection whose window closed since has nothing to confirm:
            // the overview just closes, and focus stays where the unmanage
            // put it rather than on a fallback the user never saw.
            if let Some(client_key) = self
                .features
                .overview
                .get_selected_client()
                .filter(|&client_key| self.state.clients.contains_key(client_key))
            {
                if let Some(mon_key) = self.state.sel_mon {
                    self.move_to_front(client_key);
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
        let pruned = self.prune_overview_clients(backend);
        if !self.features.overview.active {
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

        // After a prune the prism still holds the old subset, the gone
        // window's face included, so it is re-sent even when the window did
        // not slide.
        let refresh_window = plan.refresh_window.or_else(|| {
            pruned.then(|| {
                let len = self.features.overview.clients.len();
                let end =
                    (plan.slide_offset + crate::jwm::features::overview_plan::MAX_VISIBLE).min(len);
                (
                    plan.slide_offset,
                    end,
                    plan.index.saturating_sub(plan.slide_offset),
                )
            })
        });
        if let Some((window_start, window_end, selected_in_window)) = refresh_window {
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

    /// Drop the overview entries whose window has been unmanaged since the
    /// prism opened. The selection stays on its window when that survived
    /// and otherwise moves to the one that took its place; the overview
    /// closes when nothing is left. Returns whether anything was dropped.
    ///
    /// Nothing else prunes the list. A stale entry is skipped silently by
    /// `build_overview_layout`, which shifts every later face one place
    /// against the positional selection flag, and an index resting on it
    /// never reaches the compositor's rotation — so Enter confirmed a
    /// window other than the one the prism was facing.
    fn prune_overview_clients(&mut self, backend: &mut dyn Backend) -> bool {
        let alive: Vec<bool> = self
            .features
            .overview
            .clients
            .iter()
            .map(|&client_key| self.state.clients.contains_key(client_key))
            .collect();
        if alive.iter().all(|&alive| alive) {
            return false;
        }
        let index = overview_index_after_prune(&alive, self.features.overview.index);
        let clients = &self.state.clients;
        self.features
            .overview
            .clients
            .retain(|&client_key| clients.contains_key(client_key));
        let Some(index) = index else {
            self.features.overview.deactivate();
            backend.compositor_set_overview_mode(false, &[]);
            let _ = backend.key_ops().ungrab_keyboard();
            return true;
        };
        self.features.overview.index = index;
        self.features.overview.slide_offset = crate::jwm::features::overview_plan::window_start(
            index,
            self.features.overview.clients.len(),
        );
        true
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
        if let Some(mode) = self.grab_holding_mode_on_screen() {
            return Err(format!("screen annotation cannot start while {mode} is active").into());
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
            // The selector takes the keyboard and the pointer, and IPC
            // reaches this whatever is on screen. Stacked over a panel or
            // another mode, whichever exits first ungrabs input the other
            // still needs — and a panel opening over the selector takes it
            // down with the keyboard grab. Refused before anything is
            // probed or created on disk.
            if let Some(mode) = self.grab_holding_mode_on_screen() {
                return Err(format!(
                    "recording region selection cannot start while {mode} is active"
                )
                .into());
            }
            if let Err(error) = Self::require_recording_runtime() {
                self.push_system_toast(
                    backend,
                    crate::backend::api::ToastNotification {
                        title: "\u{f03d}  Recording unavailable".into(),
                        body: error.clone(),
                        urgency: 2,
                        timeout_ms: 8000,
                        ..Default::default()
                    },
                );
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
        // As for the initial selection in `toggle_recording`. An adjustment
        // already under way is its own mode, and stays the no-op below.
        if !self.features.recording.selecting_region
            && let Some(mode) = self.grab_holding_mode_on_screen()
        {
            return Err(
                format!("recording region adjustment cannot start while {mode} is active").into(),
            );
        }
        if !self.features.recording.begin_region_adjustment() {
            return Ok(());
        }
        self.features.capture.recording = CaptureTarget::Region;
        if let Err(error) = self.grab_recording_region_input(backend) {
            self.features.recording.cancel_region_selection();
            return Err(error);
        }
        backend.compositor_set_capture_selection_active(true);
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
        self.features.capture.clear_recording_double_click();
        if let Err(error) = self.grab_recording_region_input(backend) {
            self.features.recording.cancel_region_selection();
            return Err(error);
        }
        backend.compositor_set_capture_selection_active(true);
        // Soft-probe the window under the pointer so hover/click picking works
        // before the first motion event.
        self.preview_recording_capture_target(
            backend,
            crate::backend::api::HitTarget::Background { output: None },
            self.last_mouse_root,
        );
        self.sync_capture_hint(backend);
        info!(
            "[recording] hover/click a window or drag a region, then Enter to start → {output_path}"
        );
        Ok(())
    }

    pub(crate) fn sync_recording_region_overlay(&mut self, backend: &mut dyn Backend) {
        let region = self
            .features
            .recording
            .region
            .and_then(Self::recording_region_tuple);
        let interactive = region.is_some();
        backend.compositor_set_recording_region_overlay(region);
        backend.compositor_set_recording_region_interactive(interactive);
        backend.compositor_force_full_redraw();
        self.sync_capture_hint(backend);
        self.sync_recording_selection_cursor(backend, self.last_mouse_root);
    }

    pub(crate) fn finish_recording_region_interaction(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(region) = self.features.recording.region else {
            self.push_system_toast(
                backend,
                crate::backend::api::ToastNotification {
                    title: "\u{f03d}  Pick a recording source".into(),
                    body: "Hover a window and click, or drag a region, then press Enter or Space"
                        .into(),
                    urgency: 1,
                    timeout_ms: 4000,
                    ..Default::default()
                },
            );
            return Ok(());
        };
        let Some(region_tuple) = Self::recording_region_tuple(region) else {
            return Ok(());
        };
        let adjusting = self.features.recording.adjusting_region;
        let pending_path = self.features.recording.pending_output_path.clone();
        self.features.recording.finish_region_selection();
        self.features.capture.clear_recording_double_click();
        self.release_recording_region_input(backend);
        backend.compositor_set_capture_selection_active(false);
        backend.compositor_set_recording_region_overlay(None);
        backend.compositor_set_capture_hint(None);

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
        self.features.capture.clear_recording_double_click();
        self.release_recording_region_input(backend);
        backend.compositor_set_capture_selection_active(false);
        backend.compositor_set_recording_region_overlay(None);
        backend.compositor_set_capture_hint(None);
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
    ///
    /// The stop half does not wait for the file: the MIC chip clears at
    /// once, and the stopped (or failed) toast follows from the frame tick
    /// when the recorder has finished writing — see
    /// [`Self::begin_stopping_audio_recording`].
    pub fn toggle_audio_recording(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.features.audio_recording.refresh();
        if self.features.audio_recording.active {
            self.begin_stopping_audio_recording(backend)?;
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
                let error = format!("unsupported audio recording format: {format}");
                // Same shape as the screen recorder's unavailable toast: a
                // mic the user believes is recording when it is not is the
                // privacy-relevant case, so the failure breaks through
                // do-not-disturb.
                self.push_system_toast(
                    backend,
                    crate::backend::api::ToastNotification {
                        title: "\u{f130}  Audio recording unavailable".into(),
                        body: error.clone(),
                        urgency: 2,
                        timeout_ms: 8000,
                        ..Default::default()
                    },
                );
                return Err(error.into());
            }
            let path = output_dir.join(format!("jwm-recording-{timestamp}.{format}"));
            self.start_audio_recording(backend, &path)?;
        }
        Ok(())
    }

    pub(crate) fn start_audio_recording(
        &mut self,
        backend: &mut dyn Backend,
        output_path: &std::path::Path,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Report a previous recording that finished finalizing (or died on
        // its own) since the last tick before starting over it, so its toast
        // is not lost and the recorder does not refuse a start it could take.
        self.poll_audio_recording(backend);
        let behavior = CONFIG.load().behavior().clone();
        let started = if self.features.recording.active && behavior.recording_audio_enabled {
            Err(
                "screen recording is already using the configured microphone; stop it first"
                    .to_string(),
            )
        } else {
            self.features.audio_recording.start(
                output_path,
                &behavior.audio_recording_device,
                behavior.audio_recording_sample_rate,
                behavior.audio_recording_channels,
                &behavior.audio_recording_backend,
                &behavior.audio_recording_bitrate,
            )
        };
        if let Err(error) = started {
            // Mirror the screen recorder's unavailable toast, urgency and
            // all: a start that did not happen must not fail silently.
            self.push_system_toast(
                backend,
                crate::backend::api::ToastNotification {
                    title: "\u{f130}  Audio recording unavailable".into(),
                    body: error.clone(),
                    urgency: 2,
                    timeout_ms: 8000,
                    ..Default::default()
                },
            );
            return Err(error.into());
        }
        // The recorder actually started (the same gate the start toast below
        // fires on): park the persistent MIC chip. The compositor cannot
        // derive this the way it derives the REC chip — the audio recorder
        // lives WM-side — so the state is pushed through the backend.
        backend.compositor_set_mic_indicator(true);
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
        // Mirror the screen recorder's start toast: the request was
        // accepted, and the persistent MIC chip pushed above confirms the
        // rest — deliberately a static label, unlike the REC chip's running
        // clock.
        self.push_system_toast(
            backend,
            crate::backend::api::ToastNotification {
                title: "\u{f130}  Audio recording started".into(),
                body: output_path.to_string_lossy().into_owned(),
                urgency: 1,
                timeout_ms: 5000,
                ..Default::default()
            },
        );
        Ok(())
    }

    /// Stop the microphone recorder and wait for its file to be finalized.
    ///
    /// For the paths that need the file finished before they go on: IPC
    /// `stop_audio_recording` (its reply confirms finalization), the screen
    /// recorder's microphone hand-off (the device must be free before ffmpeg
    /// opens it), and the tick's collection of a recorder that died on its
    /// own (already returned, so nothing waits). The key toggle stops through
    /// [`Self::begin_stopping_audio_recording`] instead.
    pub(crate) fn stop_audio_recording(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // A recording the key already stopped is still this call's to
        // report: the blocking stop collects its finalization.
        let was_active =
            self.features.audio_recording.active || self.features.audio_recording.is_finalizing();
        let path = self.features.audio_recording.output_path.clone();
        let stopped = self.features.audio_recording.stop();
        self.settle_audio_recording_stop(backend, was_active, path, stopped)
    }

    /// Stop the microphone recorder without waiting for it to finalize.
    ///
    /// Joining here blocked the event thread for as long as the recorder took
    /// to finish its file — a WAV header rewrite and sync, or up to ffmpeg's
    /// stop grace. The microphone session ends now (the chip clears, and the
    /// recorder reads inactive to idle and the effect queries); the stopped
    /// or failed toast and the `audio_recording/stopped` or `/error` event
    /// follow from [`Self::poll_audio_recording`] once the thread returns.
    pub(crate) fn begin_stopping_audio_recording(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let was_active = self.features.audio_recording.active;
        let path = self.features.audio_recording.output_path.clone();
        let Some(stopped) = self.features.audio_recording.begin_stop() else {
            backend.compositor_set_mic_indicator(false);
            info!(
                "[audio-recording] stopping → {} (finalizing off the event thread)",
                path.as_deref().unwrap_or("(unset)")
            );
            return Ok(());
        };
        // Nothing to wait for (a recorder that died on its own, or none):
        // report it now, exactly as the blocking stop would.
        self.settle_audio_recording_stop(backend, was_active, path, stopped)
    }

    /// Collect the microphone recorder's asynchronous outcomes. Runs from the
    /// frame tick; never blocks.
    ///
    /// A recording the key stopped is reported once its file is finalized.
    /// A recorder that ended on its own — a USB microphone unplugged, ffmpeg
    /// dying — would otherwise leave the MIC chip, the idle inhibit and
    /// `has_active_feature` on until somebody pressed the toggle again; it is
    /// stopped here, which clears the chip and raises the failure toast with
    /// the recorder's own error. Its thread has already returned, so the
    /// blocking stop does not wait.
    pub(crate) fn poll_audio_recording(&mut self, backend: &mut dyn Backend) {
        if let Some(finalized) = self.features.audio_recording.poll_finalized() {
            let _ = self.settle_audio_recording_stop(
                backend,
                true,
                finalized.output_path,
                finalized.outcome,
            );
        }
        if self.features.audio_recording.refresh() {
            let _ = self.stop_audio_recording(backend);
        }
    }

    /// Report a stop whose outcome is known: clear the MIC chip, publish the
    /// result, and toast it. Every stop path ends here, so the three stop
    /// flavors cannot drift apart.
    fn settle_audio_recording_stop(
        &mut self,
        backend: &mut dyn Backend,
        was_active: bool,
        path: Option<String>,
        stopped: Result<(), String>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // The chip answers "is the microphone live right now?", so it clears
        // with the stop request either way: a failed stop means the capture
        // thread could not be joined (it panicked — it is not still
        // recording), and the urgency-2 toast below covers the file.
        backend.compositor_set_mic_indicator(false);
        if let Err(error) = stopped {
            // Subscribers heard `audio_recording/started`; a session that
            // ended in a failure must not end silently for them.
            self.broadcast_ipc_event(
                "audio_recording/error",
                serde_json::json!({
                    "operation": "stop",
                    "error": error,
                    "output_path": path,
                }),
            );
            // A stop that failed leaves the file unfinalized — say so,
            // through do-not-disturb like any recording failure.
            self.push_system_toast(
                backend,
                crate::backend::api::ToastNotification {
                    title: "\u{f130}  Audio recording failed".into(),
                    body: error.clone(),
                    urgency: 2,
                    timeout_ms: 8000,
                    ..Default::default()
                },
            );
            return Err(error.into());
        }
        if was_active {
            info!(
                "[audio-recording] stop → {}",
                path.as_deref().unwrap_or("(unset)")
            );
            self.broadcast_ipc_event(
                "audio_recording/stopped",
                serde_json::json!({"output_path": path}),
            );
            // Mirror the screen recorder's stop toast, output path included.
            self.push_system_toast(
                backend,
                crate::backend::api::ToastNotification {
                    title: "\u{f130}  Audio recording stopped".into(),
                    body: path.clone().unwrap_or_default(),
                    urgency: 1,
                    timeout_ms: 5000,
                    ..Default::default()
                },
            );
        }
        Ok(())
    }

    /// A standalone WAV recording and the synchronized screen audio track
    /// must not race for the same capture device. Finalize the standalone
    /// file before handing the microphone to a screen recording that
    /// `captures_audio`.
    ///
    /// That includes a recording the key stopped a moment ago: it reads
    /// inactive at once but still holds the device while it finalizes (up to
    /// ffmpeg's stop grace), so the screen recorder's ffmpeg would find it
    /// busy. The blocking stop joins it — bounded by that grace — and reports
    /// it exactly once, in place of the frame tick.
    fn free_microphone_for_screen_recording(
        &mut self,
        backend: &mut dyn Backend,
        captures_audio: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if captures_audio
            && (self.features.audio_recording.active
                || self.features.audio_recording.is_finalizing())
        {
            info!("[recording] stopping standalone audio before synchronized capture");
            self.stop_audio_recording(backend)?;
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

        self.free_microphone_for_screen_recording(
            backend,
            CONFIG.load().behavior().recording_audio_enabled,
        )?;

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
        // Mirror the stop toast: the request was accepted, and the persistent
        // REC chip (compositor-drawn while frames flow) confirms the rest.
        self.push_system_toast(
            backend,
            crate::backend::api::ToastNotification {
                title: "\u{f03d}  Recording started".into(),
                body: output_path.to_string(),
                urgency: 1,
                timeout_ms: 5000,
                ..Default::default()
            },
        );
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
        self.push_system_toast(
            backend,
            crate::backend::api::ToastNotification {
                title: "\u{f03d}  Recording stopped".into(),
                body: output_path.clone(),
                urgency: 1,
                timeout_ms: 5000,
                ..Default::default()
            },
        );
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
        if !self.features.expose_active
            && let Some(mode) = self.grab_holding_mode_on_screen()
        {
            return Err(format!("expose cannot open while {mode} is active").into());
        }
        // Collect windows visible on their monitor; eligibility filtering and
        // the enter/exit decision live in the pure plan.
        let candidates = if self.features.expose_active {
            Vec::new()
        } else {
            self.expose_candidates()
        };
        let action = expose_plan::plan_toggle(self.features.expose_active, candidates);
        self.apply_expose_action(backend, action)
    }

    /// The windows expose lays out as thumbnails, in entry order: every
    /// managed window visible on an unlocked monitor, minus the shell chrome.
    /// The title rides along so the compositor can label each thumbnail; the
    /// plan sanitizes it (control characters collapse to spaces, like the
    /// launcher and notification surfaces do).
    ///
    /// The close paths (Delete, middle-click) recompute the grid from this
    /// same collection. It has to be the one `toggle_expose` entered with: a
    /// close only ever removes entries, so the rebuilt grid keeps every
    /// survivor exactly where the list had it — a second copy that forgot a
    /// filter would slip a bar or a locked monitor's window into the grid.
    pub(crate) fn expose_candidates(&self) -> Vec<expose_plan::ExposeCandidate> {
        let mut candidates = Vec::new();
        for &mon_key in &self.state.monitor_order {
            // A locked monitor's windows are behind a shade. Expose spreads
            // its thumbnails across the whole desktop, so one of them would
            // put those windows back on a screen that is not locked — and
            // clicking it could not focus them anyway.
            if self.monitor_key_is_locked(mon_key) {
                continue;
            }
            let Some(clients) = self.state.monitor_clients.get(mon_key) else {
                continue;
            };
            for &ck in clients {
                if !self.is_client_visible_on_monitor(ck, mon_key) {
                    continue;
                }
                let Some(client) = self.state.clients.get(ck) else {
                    continue;
                };
                // A managed bar (polybar, tint2) or desktop window is shell
                // chrome, not workspace content — the tags overview and the
                // switcher leave it out too — and clicking its thumbnail
                // would focus the bar.
                if client.state.is_dock || client.state.is_desktop {
                    continue;
                }
                let g = &client.geometry;
                candidates.push((client.win, g.x, g.y, g.w, g.h, client.name.clone()));
            }
        }
        candidates
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
        ShellEntry, control_snapshot_epoch_matches, shell_entry, status_bar_shell_is_mine,
        should_refresh_after_pairing_close, should_start_control_snapshot,
    };
    use crate::jwm::features::{ShellHubRoute, SystemUiState, system_ui::ControlCenterInputs};

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
    fn status_bar_hub_home_mirrors_alt_f10_ownership() {
        let hub = SystemUiState::control_center(&ControlCenterInputs::default());
        let calendar = SystemUiState::calendar(chrono::NaiveDate::from_ymd_opt(2026, 9, 13)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap());
        let session = SystemUiState::session_menu();

        // Hub home while the hub itself is up — dismiss.
        assert!(status_bar_shell_is_mine(false, &hub, None));
        // Hub home while a child reached from the hub is up — dismiss (Alt+F10).
        assert!(status_bar_shell_is_mine(true, &calendar, None));
        // Hub home over an unrelated panel — hand over, not dismiss.
        assert!(!status_bar_shell_is_mine(false, &calendar, None));
        assert!(!status_bar_shell_is_mine(false, &session, None));
    }

    #[test]
    fn status_bar_named_route_matches_only_that_page() {
        let calendar = SystemUiState::calendar(chrono::NaiveDate::from_ymd_opt(2026, 9, 13)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap());
        let launcher = SystemUiState::open_launcher(std::sync::Arc::from([]), Vec::new(), false);
        let hub = SystemUiState::control_center(&ControlCenterInputs::default());

        assert!(status_bar_shell_is_mine(
            false,
            &calendar,
            Some(ShellHubRoute::Calendar)
        ));
        assert!(!status_bar_shell_is_mine(
            false,
            &calendar,
            Some(ShellHubRoute::Applications)
        ));
        assert!(status_bar_shell_is_mine(
            true,
            &launcher,
            Some(ShellHubRoute::Applications)
        ));
        // Same page only: Hub home is not the Calendar route.
        assert!(!status_bar_shell_is_mine(
            false,
            &hub,
            Some(ShellHubRoute::Calendar)
        ));
    }

    /// The status-bar opener must toggle / hand over while a panel is up,
    /// not swallow the click. Pins the ownership helper and the dismiss path
    /// so a regression cannot quietly restore the old early return.
    #[test]
    fn begin_shell_from_status_bar_toggles_and_hands_over_while_open() {
        const SOURCE: &str = include_str!("toggles.rs");
        let begin = SOURCE
            .split_once("pub(crate) fn begin_shell_from_status_bar")
            .expect("begin_shell_from_status_bar")
            .1
            .split_once("pub(crate) fn return_to_shell_hub")
            .expect("return_to_shell_hub follows")
            .0;
        for needle in [
            "status_bar_shell_is_mine",
            "toggle_off_system_ui",
            "prepare_system_ui_inner",
        ] {
            assert!(
                begin.contains(needle),
                "begin_shell_from_status_bar lost {needle}"
            );
        }
        assert!(
            !begin.contains("stray click on the bar does nothing"),
            "status-bar ShellHub must not ignore clicks while the shell is open"
        );
    }

    #[test]
    fn a_closed_inbound_window_refreshes_only_when_it_bound_and_nothing_is_scanning() {
        // A window a device rang may have left a bond behind, so the list is
        // re-read once the window is gone.
        assert!(should_refresh_after_pairing_close(true, true, false));
        // An outbound session re-reads from `bluetooth_pairing_done`, and a
        // window nothing rang has nothing to refresh for.
        assert!(!should_refresh_after_pairing_close(false, true, false));
        assert!(!should_refresh_after_pairing_close(true, false, false));
        // And never on top of a read already running: `s` starts a real
        // `Adapter1.StartDiscovery` session, and replacing its handle only
        // drops the notifier — the worker runs on with nowhere to land while
        // a second `jwm-bridge discover` overlaps it.
        assert!(!should_refresh_after_pairing_close(true, true, true));
    }

    /// System toasts (recording started/stopped/unavailable) must all pass
    /// the same Do-Not-Disturb gate a notification gets: pushing one straight
    /// into the compositor used to make the recording cards ignore quiet
    /// hours entirely. The needle is assembled at runtime and the haystack
    /// excludes the test modules, so this cannot match its own source.
    #[test]
    fn system_toasts_go_through_the_do_not_disturb_gate() {
        const SOURCE: &str = include_str!("toggles.rs");
        let shipped = SOURCE
            .split_once("#[cfg(test)]")
            .expect("the first test module")
            .0;
        let direct = format!("backend.compositor_{}", "push_toast");
        assert!(
            !shipped.contains(&direct),
            "a system toast bypassed push_system_toast's do-not-disturb gate"
        );
        let gated = format!("self.{}(", "push_system_toast");
        assert!(
            shipped.contains(&gated),
            "the recording toasts no longer go through push_system_toast"
        );
    }

    /// The connect/disconnect completion re-reads the device list the way
    /// `bluetooth_pairing_done` does, and never over a read already running:
    /// replacing that handle only drops the notifier, so the worker — and the
    /// real `Adapter1.StartDiscovery` session behind an `s`-key scan — runs on
    /// with nowhere to land. The needles are assembled at runtime and the
    /// haystack is one function's body, so this cannot match its own source.
    #[test]
    fn the_post_action_reread_coalesces_on_a_running_scan() {
        const SOURCE: &str = include_str!("toggles.rs");
        let poll = SOURCE
            .split_once(&format!("fn {}(", "poll_bluetooth_jobs"))
            .expect("poll_bluetooth_jobs")
            .1
            .split_once(&format!("fn {}(", "activate_selected_bluetooth"))
            .expect("the function that follows poll_bluetooth_jobs")
            .0;
        let guard = format!("{}(", "job_in_flight");
        let reread = format!("{}()", "start_device_scan");
        let guard_at = poll.find(&guard).expect("the re-read is guarded");
        let reread_at = poll
            .find(&reread)
            .expect("the post-action re-read is still started");
        assert!(
            guard_at < reread_at,
            "the connect/disconnect re-read must test for a running scan first"
        );
    }

    /// An SSID reaches the compositor byte-exact because it is the join key,
    /// so every place it is *read* rather than used has to strip it — the
    /// picker row, the status line, and this log line, which an access point's
    /// owner could otherwise fill with terminal escapes. The needle is
    /// assembled at runtime and the haystack is one function's body, so this
    /// cannot match its own source.
    #[test]
    fn the_joined_log_line_does_not_replay_an_access_points_control_bytes() {
        const SOURCE: &str = include_str!("toggles.rs");
        let poll = SOURCE
            .split_once(&format!("fn {}(", "poll_wifi_jobs"))
            .expect("poll_wifi_jobs")
            .1
            .split_once(&format!("fn {}(", "join_selected_wifi"))
            .expect("the function that follows poll_wifi_jobs")
            .0;
        let raw = format!("joined {{{}}}", "ssid");
        assert!(
            !poll.contains(&raw),
            "the joined log line interpolates the raw SSID ({raw})"
        );
        assert!(
            poll.contains(&format!("{}(", "display_ssid")),
            "the joined log line no longer goes through the display helper"
        );
    }

    /// Enter(join) must not race a forget that is still deleting the profile:
    /// the join would spawn against a half-deleted connection and the forget's
    /// completion re-read would then report a world the join already changed.
    /// `forget_selected_wifi` guards both slots; this arm is the other
    /// direction of that recorded asymmetry. Runtime-assembled needles over a
    /// single function body, so this cannot match its own source.
    #[test]
    fn the_join_path_yields_to_a_forget_in_flight() {
        const SOURCE: &str = include_str!("toggles.rs");
        let join = SOURCE
            .split_once(&format!("fn {}(", "join_selected_wifi"))
            .expect("join_selected_wifi")
            .1
            .split_once(&format!("fn {}(", "toggle_wifi"))
            .expect("the function that follows join_selected_wifi")
            .0;
        let guard = join
            .find(&format!(
                "{}(self.features.{}",
                "job_in_flight", "wifi_forget"
            ))
            .expect("the forget-in-flight guard is still there");
        let passphrase = join
            .find(&format!("{}(", "take_wifi_passphrase"))
            .expect("the passphrase take is still there");
        let connect = join
            .find(&format!("{}(", "start_connect"))
            .expect("the worker spawn is still there");
        assert!(
            guard < passphrase && passphrase < connect,
            "the forget guard must precede the passphrase take and the worker spawn"
        );
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

        // The wallpaper picker used to readdir its whole directory on the
        // event loop; both the directory resolution (stats) and the listing
        // must stay inside the worker closure.
        let picker = SOURCE
            .split_once("fn wallpaper_picker_state")
            .unwrap()
            .1
            .split_once("pub(crate) fn poll_wallpaper_listing_job")
            .unwrap()
            .0;
        let spawn = picker
            .find("BackgroundJob::spawn(move ||")
            .expect("the wallpaper listing no longer runs on a worker");
        for call in ["list_wallpapers(", "resolve_directory("] {
            let at = picker
                .find(call)
                .unwrap_or_else(|| panic!("the picker no longer calls {call}"));
            assert!(at > spawn, "{call} runs on the event loop again");
        }
    }

    /// Theme apply must mirror wallpaper's in-memory path, then surgically
    /// persist `appearance.ui_theme` (never a wholesale `save_to_file`).
    #[test]
    fn apply_selected_theme_follows_the_wallpaper_set_config_path() {
        const SOURCE: &str = include_str!("toggles.rs");
        let apply = SOURCE
            .split_once("pub(crate) fn apply_selected_theme")
            .expect("apply_selected_theme")
            .1
            .split_once("pub(crate) fn clipboard_picker")
            .expect("clipboard_picker follows theme apply")
            .0;
        for needle in [
            "appearance.ui_theme",
            "CONFIG.store",
            "apply_config_changes",
            "persist_ui_theme",
            "note_config_written_by_us",
            "config/changed",
            "close_system_ui",
        ] {
            assert!(
                apply.contains(needle),
                "apply_selected_theme lost {needle}"
            );
        }
        assert!(
            !apply.contains("save_to_file"),
            "apply_selected_theme must not wholesale-save the config"
        );
        // An edit saved just before the theme write must be observed before
        // JWM's own write is settled, or it is swallowed.
        let observe = apply
            .find(&format!("self.{}(", "observe_config_reload"))
            .expect("apply_selected_theme no longer stats the config before writing it");
        let persist = apply.find("persist_ui_theme").expect("the theme write");
        assert!(
            observe < persist,
            "the pre-save check must run before the theme is written"
        );
        assert!(
            SOURCE.contains("ShellHubRoute::Theme => Self::theme_picker_state()"),
            "open_shell_hub_route must open Theme via theme_picker_state"
        );
    }

    /// The volume/brightness key handlers used to run the session's tools on
    /// the event thread — two bounded-but-blocking spawns per press, seconds
    /// of WM stall behind a hung wpctl. They must only queue the change and
    /// draw the estimate; the controls worker runs the tools. The haystack is
    /// the three handlers alone, and the needles are assembled at runtime so
    /// this test cannot match its own source.
    #[test]
    fn control_keys_queue_instead_of_shelling_out() {
        const SOURCE: &str = include_str!("toggles.rs");
        let handlers = SOURCE
            .split_once("pub(crate) fn volume_adjust")
            .expect("volume_adjust")
            .1
            .split_once("fn show_volume_osd")
            .expect("the end of the key handlers")
            .0;
        for primitive in [
            "volume_adjust",
            "volume_set",
            "volume_toggle_mute",
            "volume_state",
            "brightness_adjust",
            "brightness_set",
            "brightness_percent",
        ] {
            let needle = format!("system_controls::{primitive}(");
            assert!(
                !handlers.contains(&needle),
                "a control key regained a blocking tool call: {needle}"
            );
        }
        for helper in ["queue_volume_request", "queue_brightness_request"] {
            let needle = format!("self.{helper}(");
            assert!(
                handlers.contains(&needle),
                "a control key no longer queues on the controls worker ({needle})"
            );
        }
    }

    /// The read-back adoption itself must not call the tools either: the
    /// report arrives by value. Same construction as the key-handler pin.
    #[test]
    fn the_feedback_poll_adopts_without_shelling_out() {
        const SOURCE: &str = include_str!("toggles.rs");
        let poll = SOURCE
            .split_once("pub(crate) fn poll_control_feedback")
            .expect("poll_control_feedback")
            .1
            .split_once("fn show_volume_osd")
            .expect("the end of poll_control_feedback")
            .0;
        for primitive in [
            "volume_adjust",
            "volume_set",
            "volume_toggle_mute",
            "volume_state",
            "brightness_adjust",
            "brightness_set",
            "brightness_percent",
            // The audio-device switch report is adopted here too, and rides
            // the same rule: the worker read the tools, the poll only
            // believes the re-read it was handed.
            "set_audio_device",
            "audio_inventory",
        ] {
            let needle = format!("system_controls::{primitive}(");
            assert!(
                !poll.contains(&needle),
                "the feedback poll regained a blocking tool call: {needle}"
            );
        }
        // The power-profile report is adopted by value too.
        for primitive in ["set_profile", "profiles"] {
            let needle = format!("power::{primitive}(");
            assert!(
                !poll.contains(&needle),
                "the feedback poll regained a blocking tool call: {needle}"
            );
        }
        assert!(
            poll.contains(&format!("self.{}(", "adopt_power_profile_report")),
            "the feedback poll no longer adopts the power-profile report"
        );
    }

    /// The control-center Input row reads the mic flag now, so a read-back
    /// that actually moves it — a confirm that corrects the estimate or a
    /// revert of a failed change — must flag the panel for repaint, exactly
    /// like the volume and brightness arms. A read-back that lands on the
    /// value already shown still flags nothing. The haystack is the mic arm
    /// of the poll alone, and the needle is assembled at runtime so this
    /// test cannot match its own source.
    #[test]
    fn the_mic_feedback_arms_flag_the_panel_on_a_real_move() {
        const SOURCE: &str = include_str!("toggles.rs");
        let poll = SOURCE
            .split_once("pub(crate) fn poll_control_feedback")
            .expect("poll_control_feedback")
            .1
            .split_once("fn show_volume_osd")
            .expect("the end of poll_control_feedback")
            .0;
        let mic = poll
            .split_once("if let Some(mic) = report.mic")
            .expect("the mic arm")
            .1
            .split_once("if let Some(audio) = report.audio")
            .expect("the end of the mic arm")
            .0;
        let flag = format!("{} = true", "panel_changed");
        let adopt = mic
            .split_once("FeedbackAction::Adopt")
            .expect("the mic adopt arm")
            .1
            .split_once("FeedbackAction::Revert")
            .expect("the mic revert arm")
            .0;
        assert!(
            adopt.contains(&flag),
            "a mic adopt that moves the flag no longer repaints an open control center ({flag})"
        );
        let revert = mic
            .split_once("FeedbackAction::Revert")
            .expect("the mic revert arm")
            .1
            .split_once("FeedbackAction::KeepEstimate")
            .expect("the mic keep arm")
            .0;
        assert!(
            revert.contains(&flag),
            "a mic revert that restores the flag no longer repaints an open control center ({flag})"
        );
    }

    /// The audio picker's Enter used to run `wpctl set-default` plus a
    /// `wpctl status` re-read as two serial blocking spawns on the event
    /// thread — the round-13 freeze shape. It queues a
    /// `ControlRequest::AudioSetDefault` on the controls worker now and lets
    /// the frame tick adopt the re-read. The haystack is the handler alone,
    /// and the needles are built at runtime so this test cannot match its
    /// own source.
    #[test]
    fn the_audio_picker_enter_queues_instead_of_shelling_out() {
        const SOURCE: &str = include_str!("toggles.rs");
        let handler = SOURCE
            .split_once("pub(crate) fn use_selected_audio_device")
            .expect("use_selected_audio_device")
            .1
            .split_once("pub(crate) fn wifi_picker")
            .expect("the end of the audio-picker Enter handler")
            .0;
        for forbidden in ["set_audio_device", "audio_inventory", "Command"] {
            let needle = format!("{}(", forbidden);
            assert!(
                !handler.contains(&needle),
                "the audio-picker Enter regained a blocking call: {needle}"
            );
        }
        let queue = format!("{}(", "queue_control_request");
        assert!(
            handler.contains(&queue),
            "the audio-picker Enter no longer queues on the controls worker ({queue})"
        );
    }

    /// The key-bound radio toggles used to run `nmcli`/`bluetoothctl`
    /// synchronously — up to the 10 s query timeout — on the event thread.
    /// They queue the flip on the connectivity worker and acknowledge the
    /// press with an OSD showing the requested target; the row confirms from
    /// the worker's re-read. Same construction as the pins above.
    #[test]
    fn the_radio_toggles_queue_and_acknowledge_with_the_osd() {
        const SOURCE: &str = include_str!("toggles.rs");
        for (toggle, next, kind) in [
            (
                "pub(crate) fn toggle_wifi",
                "pub(crate) fn toggle_bluetooth",
                "Wifi",
            ),
            (
                "pub(crate) fn toggle_bluetooth",
                "pub(crate) fn toggle_night_light",
                "Bluetooth",
            ),
        ] {
            let body = SOURCE
                .split_once(toggle)
                .unwrap_or_else(|| panic!("{toggle} not found"))
                .1
                .split_once(next)
                .unwrap_or_else(|| panic!("{toggle} is no longer followed by {next}"))
                .0;
            for primitive in ["set_wifi", "set_bluetooth"] {
                let needle = format!("connectivity::{primitive}(");
                assert!(
                    !body.contains(&needle),
                    "{toggle} regained a synchronous radio set: {needle}"
                );
            }
            let osd = format!("{}::{}{}", "OsdKind", kind, "(");
            assert!(
                body.contains(&osd),
                "{toggle} no longer acknowledges the press with the {kind} OSD ({osd})"
            );
            let queue = format!("self.{}(", "request_radio_set");
            assert!(
                body.contains(&queue),
                "{toggle} no longer queues the flip on the connectivity worker ({queue})"
            );
        }
    }

    /// The mic-mute key follows the round-13 rule the volume keys follow:
    /// queue the change on the controls worker and draw the estimate, never
    /// shell out on the event thread. Same construction as
    /// `control_keys_queue_instead_of_shelling_out`, with the haystack
    /// bounded by the handler that follows in the source.
    #[test]
    fn the_mic_mute_key_queues_and_acknowledges_with_the_mic_osd() {
        const SOURCE: &str = include_str!("toggles.rs");
        let handler = SOURCE
            .split_once("pub(crate) fn toggle_mic_mute")
            .expect("toggle_mic_mute")
            .1
            .split_once("pub(crate) fn brightness_adjust")
            .expect("the end of toggle_mic_mute")
            .0;
        for primitive in ["mic_toggle_mute", "mic_set_mute", "mic_mute_state"] {
            let needle = format!("system_controls::{primitive}(");
            assert!(
                !handler.contains(&needle),
                "the mic-mute key regained a blocking tool call: {needle}"
            );
        }
        let queue = format!("self.{}(", "queue_mic_request");
        assert!(
            handler.contains(&queue),
            "the mic-mute key no longer queues on the controls worker ({queue})"
        );
        let osd = format!("self.{}(backend, muted)", "show_mic_osd");
        assert!(
            handler.contains(&osd),
            "the mic-mute key no longer acknowledges the press with the mic OSD ({osd})"
        );
    }

    /// A picker device switch raises a named OSD only after the re-read says
    /// it took — never on queue / "Switching…". Needles are built at runtime
    /// so this cannot match its own source.
    #[test]
    fn adopt_audio_switch_queues_a_named_osd_only_when_took() {
        const SOURCE: &str = include_str!("toggles.rs");
        let body = SOURCE
            .split_once("fn adopt_audio_switch")
            .expect("adopt_audio_switch")
            .1
            .split_once("fn show_volume_osd")
            .expect("the end of adopt_audio_switch")
            .0;
        let queue = format!("self.features.control_feedback.{}(", "queue_audio_device_osd");
        assert!(
            body.contains(&queue),
            "adopt_audio_switch no longer queues a named audio-device OSD ({queue})"
        );
        let event = format!("\"{}/{}\"", "audio", "devices");
        let broadcast = format!("self.{}(", "broadcast_ipc_event");
        assert!(
            body.contains(&broadcast) && body.contains(&event),
            "adopt_audio_switch must publish {event} so IPC and the picker share the bus"
        );
        // The queue must sit on the took arm, not fire unconditionally.
        let took_arm = body
            .split_once("(true, Some(name))")
            .expect("the took arm")
            .1
            .split_once("(false, Some(name))")
            .expect("the failed arm")
            .0;
        assert!(
            took_arm.contains(&queue),
            "the named OSD must queue only when the switch took"
        );
        let failed_arm = body
            .split_once("(false, Some(name))")
            .expect("the failed arm")
            .1
            .split_once("if self.features.system_ui.audio_picker_direction()")
            .expect("after the verdict match")
            .0;
        assert!(
            !failed_arm.contains(&queue),
            "a failed re-read must not queue an optimistic audio-device OSD"
        );
    }

    /// The lock-screen media passthrough deliberately stays at its ten
    /// keysyms: unmuting a microphone behind a locked screen is a privacy
    /// risk, so the new XF86AudioMicMute binding must never join the
    /// passthrough — neither the keysym set nor the function set. The
    /// passthrough lives in input_handler.rs, outside this change's file
    /// set, so the pin reads its source the way the round-15 pins do.
    #[test]
    fn the_lock_screen_passthrough_never_covers_the_microphone_mute_key() {
        const SOURCE: &str = include_str!("../input_handler.rs");
        let keysyms = SOURCE
            .split_once("fn lock_media_keysym")
            .expect("lock_media_keysym")
            .1
            .split_once("fn lock_media_func")
            .expect("lock_media_func follows lock_media_keysym")
            .0;
        assert!(
            !keysyms.contains("MicMute"),
            "the lock-screen media passthrough must never include the microphone mute keysym"
        );
        let funcs = SOURCE
            .split_once("fn lock_media_func")
            .expect("lock_media_func")
            .1
            .split_once("pub(crate) fn on_button_press_internal")
            .expect("the end of lock_media_func")
            .0;
        assert!(
            !funcs.contains("mic_mute"),
            "the lock-screen media actions must never include the mic-mute toggle"
        );
    }

    /// The audio recorder mirrors the screen recorder's toast contract: a
    /// start toast at normal urgency, a stop toast carrying the output
    /// path, and urgency-2 failure toasts that break through
    /// do-not-disturb — a mic believed recording (or believed stopped) when
    /// the opposite is true is the privacy-relevant case. Needles are
    /// assembled at runtime so this cannot match its own source.
    #[test]
    fn audio_recording_mirrors_the_screen_recorders_toasts() {
        const SOURCE: &str = include_str!("toggles.rs");
        let shipped = SOURCE
            .split_once("#[cfg(test)]")
            .expect("the first test module")
            .0;
        let toast = format!("self.{}(", "push_system_toast");
        for (name, body) in [
            (
                "start",
                shipped
                    .split_once("pub(crate) fn start_audio_recording")
                    .expect("start_audio_recording")
                    .1
                    .split_once("pub(crate) fn stop_audio_recording")
                    .expect("stop_audio_recording")
                    .0,
            ),
            (
                "stop",
                shipped
                    .split_once("pub(crate) fn stop_audio_recording")
                    .expect("stop_audio_recording")
                    .1
                    .split_once("pub(crate) fn start_recording_region")
                    .expect("the end of stop_audio_recording")
                    .0,
            ),
        ] {
            assert!(
                body.contains(&toast),
                "audio recording {name} no longer toasts through the DND gate"
            );
            assert!(
                body.contains("urgency: 1"),
                "audio recording {name} lost its normal-urgency state toast"
            );
            assert!(
                body.contains("urgency: 2"),
                "audio recording {name} lost its through-DND failure toast"
            );
        }
        let stop = shipped
            .split_once("pub(crate) fn stop_audio_recording")
            .expect("stop_audio_recording")
            .1
            .split_once("pub(crate) fn start_recording_region")
            .expect("the end of stop_audio_recording")
            .0;
        assert!(
            stop.contains("body: path"),
            "the audio stop toast no longer carries the output path"
        );

        // Every route into a start or stop hands over the backend the toast
        // needs: the key toggle, and the screen recorder's microphone
        // handoff.
        let toggle = shipped
            .split_once("pub fn toggle_audio_recording")
            .expect("toggle_audio_recording")
            .1
            .split_once("pub(crate) fn start_audio_recording")
            .expect("start_audio_recording")
            .0;
        let start_call = format!("self.{}(backend, &path)?", "start_audio_recording");
        assert!(
            toggle.contains(&start_call),
            "the toggle no longer starts through the toasting path ({start_call})"
        );
        // The key stops without joining the recorder: the toast comes from
        // the settle path once the file is finalized.
        let key_stop_call = format!("self.{}(backend)?", "begin_stopping_audio_recording");
        assert!(
            toggle.contains(&key_stop_call),
            "the toggle no longer stops through the non-blocking path ({key_stop_call})"
        );
        let settle = format!("self.{}(", "settle_audio_recording_stop");
        for (name, body) in [
            (
                "blocking stop",
                stop.split_once("pub(crate) fn begin_stopping_audio_recording")
                    .expect("begin_stopping_audio_recording")
                    .0,
            ),
            (
                "key stop",
                stop.split_once("pub(crate) fn begin_stopping_audio_recording")
                    .expect("begin_stopping_audio_recording")
                    .1
                    .split_once("pub(crate) fn poll_audio_recording")
                    .expect("poll_audio_recording")
                    .0,
            ),
            (
                "finalization poll",
                stop.split_once("pub(crate) fn poll_audio_recording")
                    .expect("poll_audio_recording")
                    .1
                    .split_once("fn settle_audio_recording_stop")
                    .expect("settle_audio_recording_stop")
                    .0,
            ),
        ] {
            assert!(
                body.contains(&settle),
                "the {name} no longer reports through the shared settle path ({settle})"
            );
        }
        let stop_call = format!("self.{}(backend)?", "stop_audio_recording");
        let handoff = shipped
            .split_once("stopping standalone audio before synchronized capture")
            .expect("the screen recorder's microphone handoff")
            .1
            .split_once("self.features.recording.start(")
            .expect("the end of the handoff")
            .0;
        assert!(
            handoff.contains(&stop_call),
            "the synchronized-capture handoff no longer stops through the toasting path ({stop_call})"
        );
        // A recording the key stopped reads inactive while it still holds
        // the device; the handoff waits for it too.
        let handoff_gate = shipped
            .split_once("fn free_microphone_for_screen_recording")
            .expect("the microphone handoff helper")
            .1
            .split_once("stopping standalone audio before synchronized capture")
            .expect("the handoff gate")
            .0;
        let finalizing = format!("audio_recording.{}()", "is_finalizing");
        assert!(
            handoff_gate.contains(&finalizing),
            "the synchronized-capture handoff skips a recorder that is still finalizing"
        );
        // And the screen recorder goes through it before it starts.
        let screen_start = shipped
            .split_once("pub(crate) fn start_recording_region")
            .expect("start_recording_region")
            .1
            .split_once("self.features.recording.start(")
            .expect("the screen recording start")
            .0;
        let handoff_call = format!("self.{}(", "free_microphone_for_screen_recording");
        assert!(
            screen_start.contains(&handoff_call),
            "start_recording_region no longer frees the microphone first ({handoff_call})"
        );
    }

    /// The MIC chip mirrors the round-16 toast's "actually started"
    /// discipline: it appears only once the recorder is really running, and
    /// it clears on stop whether or not the file finalized. Recorded
    /// decision: a screen recording keeps only the compositor-derived REC
    /// chip even when it captures the microphone — the MIC chip is for
    /// standalone audio recording only. Needles are assembled at runtime so
    /// this cannot match its own source.
    #[test]
    fn audio_recording_drives_the_mic_indicator_chip() {
        const SOURCE: &str = include_str!("toggles.rs");
        let shipped = SOURCE
            .split_once("#[cfg(test)]")
            .expect("the first test module")
            .0;
        let start = shipped
            .split_once("pub(crate) fn start_audio_recording")
            .expect("start_audio_recording")
            .1
            .split_once("pub(crate) fn stop_audio_recording")
            .expect("stop_audio_recording")
            .0;
        let stop = shipped
            .split_once("pub(crate) fn stop_audio_recording")
            .expect("stop_audio_recording")
            .1
            .split_once("pub(crate) fn start_recording_region")
            .expect("the end of stop_audio_recording")
            .0;

        let chip_on = format!("backend.compositor_{}(true)", "set_mic_indicator");
        let chip_off = format!("backend.compositor_{}(false)", "set_mic_indicator");
        // Exactly one drive point each: the key toggle, the IPC commands and
        // the screen recorder's microphone handoff all share these two
        // functions.
        assert_eq!(
            start.matches(&chip_on).count(),
            1,
            "start must park the MIC chip exactly once"
        );
        assert!(
            !start.contains(&chip_off),
            "start must never clear the chip it just parked"
        );
        assert!(
            !stop.contains(&chip_on),
            "stop must never park the chip it is tearing down"
        );
        // Two clear points: the shared settle every stop outcome goes
        // through, and the key stop's finalizing branch — the microphone is
        // released the moment the key asks, not when the file is done.
        let settle = stop
            .split_once("fn settle_audio_recording_stop")
            .expect("settle_audio_recording_stop")
            .1;
        assert_eq!(
            settle.matches(&chip_off).count(),
            1,
            "the settle path must clear the MIC chip exactly once"
        );
        let key_stop = stop
            .split_once("pub(crate) fn begin_stopping_audio_recording")
            .expect("begin_stopping_audio_recording")
            .1
            .split_once("pub(crate) fn poll_audio_recording")
            .expect("poll_audio_recording")
            .0;
        let finalizing = key_stop
            .split_once("begin_stop() else {")
            .expect("the finalizing branch")
            .1
            .split_once("return Ok(());")
            .expect("the end of the finalizing branch")
            .0;
        assert!(
            finalizing.contains(&chip_off),
            "the key stop must clear the MIC chip before the file is finalized"
        );

        // The chip goes on only past the failure early-return: a start that
        // did not happen (or an encoder that never initialized) must not
        // leave the privacy cue up.
        let gate = start
            .find("return Err(error.into());")
            .expect("the start failure gate");
        let chip = start.find(&chip_on).expect("the chip call");
        assert!(
            chip > gate,
            "the MIC chip must wait for the recorder to actually start"
        );

        // The chip clears before either stop exit (the failure toast or the
        // success path) — a joined or panicked capture thread is not a live
        // microphone, so no path may leave the cue behind.
        let chip = settle.find(&chip_off).expect("the chip call");
        let first_toast = settle
            .find(&format!("self.{}(", "push_system_toast"))
            .expect("a stop toast");
        assert!(
            chip < first_toast,
            "every stop path must clear the MIC chip"
        );

        let screen = shipped
            .split_once("pub(crate) fn start_recording_region")
            .expect("start_recording_region")
            .1
            .split_once("pub(crate) fn normalize_initial_recording_region")
            .expect("the end of start_recording_region")
            .0;
        assert!(
            !screen.contains(&format!("compositor_{}", "set_mic_indicator")),
            "screen recording keeps only the compositor-derived REC chip"
        );
    }
}

#[cfg(test)]
mod modal_grab_tests {
    use super::{
        WALLPAPER_SCAN_REFUSED, WALLPAPER_SCANNING, overview_index_after_prune,
        set_wallpaper_picker_message,
    };
    use crate::backend::common_define::WindowId;
    use crate::core::models::{ClientKey, WMClient};
    use crate::jwm::Jwm;
    use crate::jwm::features::SystemUiState;
    use crate::jwm::features::connectivity::BackgroundJob;
    use crate::jwm::features::monitor_lock::test_support::{LockSpyBackend, jwm_on_two_monitors};
    use crate::jwm::types::WMArgEnum;
    use std::path::PathBuf;

    const NO_ARG: WMArgEnum = WMArgEnum::Int(0);

    /// A tiled 800x600 window on the first tag of monitor 0, in its client
    /// list and focus stack the way a managed window is, so overview and
    /// expose both find it.
    fn window(jwm: &mut Jwm, raw: u64) -> ClientKey {
        let monitor = jwm.state.monitor_order[0];
        let mut client = WMClient::new(WindowId::from_raw(raw));
        client.mon = Some(monitor);
        client.state.tags = 1;
        client.geometry.w = 800;
        client.geometry.h = 600;
        let client_key = jwm.insert_client(client);
        jwm.attach_to_monitor(client_key, monitor);
        client_key
    }

    /// The idle lock used to refuse (and retry every few seconds, forever)
    /// while any panel was up, and a launcher or notification center stays
    /// up until somebody acts on it: the unattended desk never locked. The
    /// lock takes the screen instead, inheriting the panel's compositor
    /// lease, which its own unlock then hands back.
    #[test]
    fn an_open_panel_does_not_keep_the_session_from_locking() {
        let mut backend = LockSpyBackend::new();
        let mut jwm = jwm_on_two_monitors(&mut backend);
        backend.compositor_enabled = false;
        jwm.notification_center(&mut backend, &NO_ARG)
            .expect("the notification center opens");
        assert!(jwm.features.system_ui.is_notification_center());
        assert!(jwm.features.system_ui_temporary_compositor);

        jwm.lock_screen(&mut backend, &NO_ARG)
            .expect("the lock takes the screen from the panel");

        assert!(jwm.features.system_ui.is_session_lock());
        assert!(backend.compositor_enabled, "the lease carried over");
        jwm.close_system_ui(&mut backend);
        assert!(
            !backend.compositor_enabled,
            "the unlock returns the panel's lease"
        );
        assert!(!jwm.features.system_ui_temporary_compositor);
    }

    /// A panel took the selector's grabs and `close_system_ui` handed them
    /// back, leaving the selector drawn and armed with no grab to finish or
    /// cancel it. Opening the panel takes each selector down first.
    #[test]
    fn a_lock_over_a_capture_selector_takes_the_selector_down() {
        let mut backend = LockSpyBackend::new();
        let mut jwm = jwm_on_two_monitors(&mut backend);
        jwm.features.screenshot.start();

        jwm.lock_screen(&mut backend, &NO_ARG)
            .expect("the idle lock opens over the screenshot selector");

        assert!(jwm.features.system_ui.is_session_lock());
        assert!(!jwm.features.screenshot.active, "the selector is gone");

        let mut backend = LockSpyBackend::new();
        let mut jwm = jwm_on_two_monitors(&mut backend);
        jwm.features
            .recording
            .begin_initial_region_selection("recording.mp4".to_owned());

        jwm.lock_screen(&mut backend, &NO_ARG)
            .expect("the idle lock opens over the region selector");

        assert!(jwm.features.system_ui.is_session_lock());
        assert!(
            !jwm.features.recording.selecting_region,
            "the region selector is gone"
        );
        assert_eq!(jwm.features.recording.pending_output_path, None);
    }

    /// The expose and annotation key branches fall through to the global
    /// bindings, so another mode's key reaches its toggle mid-mode. Stacked,
    /// the first mode's exit ungrabbed input the second still needed; entry
    /// is refused instead, while each mode's own key still takes it down.
    #[test]
    fn overview_expose_and_annotation_refuse_to_stack() {
        let mut backend = LockSpyBackend::new();
        let mut jwm = jwm_on_two_monitors(&mut backend);
        window(&mut jwm, 0x901);
        window(&mut jwm, 0x902);

        jwm.toggle_expose(&mut backend, &NO_ARG)
            .expect("expose opens");
        assert!(jwm.features.expose_active);
        assert!(jwm.toggle_overview(&mut backend, &NO_ARG).is_err());
        assert!(!jwm.features.overview.active);
        assert!(backend.overview_modes.is_empty(), "no prism over the grid");
        assert!(jwm.toggle_annotation(&mut backend, &NO_ARG).is_err());
        assert!(!jwm.features.annotation_active);
        jwm.toggle_expose(&mut backend, &NO_ARG)
            .expect("expose's own key closes it");
        assert!(!jwm.features.expose_active);

        jwm.toggle_annotation(&mut backend, &NO_ARG)
            .expect("annotation starts");
        assert!(jwm.toggle_expose(&mut backend, &NO_ARG).is_err());
        assert!(!jwm.features.expose_active);
        assert_eq!(backend.expose_modes, vec![true, false]);
        jwm.toggle_annotation(&mut backend, &NO_ARG)
            .expect("annotation's own key ends it");
        assert!(!jwm.features.annotation_active);

        jwm.toggle_overview(&mut backend, &NO_ARG)
            .expect("the overview opens");
        assert!(jwm.toggle_expose(&mut backend, &NO_ARG).is_err());
        assert!(jwm.toggle_annotation(&mut backend, &NO_ARG).is_err());
        assert!(!jwm.features.expose_active && !jwm.features.annotation_active);
        assert!(jwm.features.overview.active);
    }

    /// Nothing pruned the overview's list when a window closed under it. The
    /// cycle then rested the selection on the dead entry — which never
    /// reaches the compositor's rotation — and Enter confirmed a window the
    /// prism was not facing.
    #[test]
    fn a_window_closed_under_the_overview_leaves_no_stale_entry() {
        let mut backend = LockSpyBackend::new();
        let mut jwm = jwm_on_two_monitors(&mut backend);
        let a = window(&mut jwm, 0x911);
        let b = window(&mut jwm, 0x912);
        let c = window(&mut jwm, 0x913);
        let d = window(&mut jwm, 0x914);
        jwm.toggle_overview(&mut backend, &NO_ARG)
            .expect("the overview opens");
        assert_eq!(jwm.features.overview.clients, vec![a, b, c, d]);
        // Nothing is focused yet, so the prism opens on the first window.
        assert_eq!(jwm.features.overview.get_selected_client(), Some(a));

        jwm.unmanage(&mut backend, Some(c), true)
            .expect("the window closes");
        jwm.cycle_overview(&mut backend, &WMArgEnum::Int(1))
            .expect("the first cycle");
        assert_eq!(jwm.features.overview.clients, vec![a, b, d]);
        assert_eq!(
            backend.overview_modes,
            vec![true, true],
            "the prism is re-sent without the closed window's face"
        );
        assert_eq!(jwm.features.overview.get_selected_client(), Some(b));
        jwm.cycle_overview(&mut backend, &WMArgEnum::Int(1))
            .expect("the second cycle");
        assert_eq!(
            jwm.features.overview.get_selected_client(),
            Some(d),
            "the step after b is d, not the closed c"
        );

        jwm.toggle_overview(&mut backend, &NO_ARG)
            .expect("Enter confirms");
        assert!(!jwm.features.overview.active);
        assert_eq!(jwm.get_selected_client_key(), Some(d));
    }

    /// Regression: the overview confirm moved the chosen window to the front
    /// by detaching it and reinserting it at index 0. The overview lists
    /// maximized windows too, and a promoted one stays listed, yet the
    /// detach handed its anchor on to the window resting in front of it:
    /// both named the same tile, and returning `b` first left the old
    /// master `a` second. A floating pick now moves only to the front of
    /// the floating windows.
    #[test]
    fn confirming_a_promoted_window_in_the_overview_keeps_neighbours_in_order() {
        use crate::core::layout::LayoutEnum;
        use std::rc::Rc;

        fn toggle_maximize_of(jwm: &mut Jwm, backend: &mut LockSpyBackend, key: ClientKey) {
            let monitor = jwm.state.monitor_order[0];
            jwm.state.monitors[monitor].set_selected_client_for_current_tag(Some(key));
            jwm.togglemaximize(backend, &NO_ARG)
                .expect("togglemaximize");
        }

        for return_first in [0, 1] {
            let mut backend = LockSpyBackend::new();
            let mut jwm = jwm_on_two_monitors(&mut backend);
            let monitor = jwm.state.monitor_order[0];
            jwm.state.monitors[monitor].lt = Rc::new(LayoutEnum::TILE);
            let [a, b, c] = [0x931, 0x932, 0x933].map(|raw| window(&mut jwm, raw));
            jwm.arrange(&mut backend, Some(monitor));
            toggle_maximize_of(&mut jwm, &mut backend, b);
            toggle_maximize_of(&mut jwm, &mut backend, a);
            assert_eq!(jwm.state.monitor_clients[monitor], vec![c, b, a]);

            jwm.state.monitors[monitor].set_selected_client_for_current_tag(Some(b));
            jwm.toggle_overview(&mut backend, &NO_ARG)
                .expect("the overview opens");
            assert_eq!(jwm.features.overview.get_selected_client(), Some(b));
            jwm.toggle_overview(&mut backend, &NO_ARG)
                .expect("Enter confirms");
            // A floating (promoted) pick moves to the front of the floating
            // windows, never ahead of the tiles.
            assert_eq!(jwm.state.monitor_clients[monitor], vec![c, b, a]);

            let order = if return_first == 0 { [a, b] } else { [b, a] };
            for key in order {
                toggle_maximize_of(&mut jwm, &mut backend, key);
            }
            assert_eq!(
                jwm.state.monitor_clients[monitor],
                vec![a, b, c],
                "back first: {return_first}"
            );
        }
    }

    /// A managed polybar or tint2 got an expose thumbnail, and clicking it
    /// focused the bar. Shell chrome stays out, as it does in the tags
    /// overview and the switcher.
    #[test]
    fn expose_leaves_the_shell_chrome_out() {
        let mut backend = LockSpyBackend::new();
        let mut jwm = jwm_on_two_monitors(&mut backend);
        window(&mut jwm, 0x921);
        let bar = window(&mut jwm, 0x922);
        let desktop = window(&mut jwm, 0x923);
        jwm.state.clients[bar].state.is_dock = true;
        jwm.state.clients[desktop].state.is_desktop = true;

        let windows: Vec<WindowId> = jwm
            .expose_candidates()
            .into_iter()
            .map(|candidate| candidate.0)
            .collect();

        assert_eq!(windows, vec![WindowId::from_raw(0x921)]);
    }

    fn wallpaper_picker_message(state: &SystemUiState) -> Option<&str> {
        match state {
            SystemUiState::ListPanel { message, .. } if state.is_wallpaper_picker() => {
                Some(message.as_str())
            }
            _ => None,
        }
    }

    fn scanning_wallpaper_picker() -> SystemUiState {
        let mut state = SystemUiState::wallpaper_picker(&[], "", "");
        set_wallpaper_picker_message(&mut state, WALLPAPER_SCANNING);
        state
    }

    /// Spin (never sleep) until the frame-tick poll has taken the listing.
    fn poll_until_listing_taken(jwm: &mut Jwm) {
        while jwm.features.wallpaper_listing.is_some() {
            jwm.poll_wallpaper_listing_job();
            std::thread::yield_now();
        }
    }

    #[test]
    fn the_wallpaper_listing_fills_the_picker_that_is_still_open() {
        let mut backend = LockSpyBackend::new();
        let mut jwm = jwm_on_two_monitors(&mut backend);
        jwm.features.system_ui = scanning_wallpaper_picker();
        assert_eq!(
            wallpaper_picker_message(&jwm.features.system_ui),
            Some(WALLPAPER_SCANNING)
        );
        jwm.features.wallpaper_listing = Some(BackgroundJob::spawn(|| {
            (
                PathBuf::from("/walls"),
                vec![PathBuf::from("/walls/a.png"), PathBuf::from("/walls/b.jpg")],
            )
        }));
        jwm.system_ui_dirty = false;

        poll_until_listing_taken(&mut jwm);

        assert!(jwm.features.system_ui.is_wallpaper_picker());
        assert_eq!(wallpaper_picker_message(&jwm.features.system_ui), Some(""));
        assert!(
            jwm.features.system_ui.selected_wallpaper().is_some(),
            "the rows arrived"
        );
        assert!(
            jwm.system_ui_dirty,
            "the frame tick pushes the filled picker"
        );
    }

    #[test]
    fn a_listing_for_a_closed_picker_is_dropped() {
        let mut backend = LockSpyBackend::new();
        let mut jwm = jwm_on_two_monitors(&mut backend);
        jwm.features.wallpaper_listing = Some(BackgroundJob::spawn(|| {
            (PathBuf::from("/walls"), vec![PathBuf::from("/walls/a.png")])
        }));

        poll_until_listing_taken(&mut jwm);

        assert!(
            !jwm.features.system_ui.is_active(),
            "a late listing must not reopen the picker"
        );
    }

    #[test]
    fn a_refused_wallpaper_scan_says_so_instead_of_scanning_forever() {
        let mut backend = LockSpyBackend::new();
        let mut jwm = jwm_on_two_monitors(&mut backend);
        jwm.features.system_ui = scanning_wallpaper_picker();
        jwm.features.wallpaper_listing = Some(BackgroundJob::refused());

        jwm.poll_wallpaper_listing_job();

        assert!(jwm.features.wallpaper_listing.is_none());
        assert_eq!(
            wallpaper_picker_message(&jwm.features.system_ui),
            Some(WALLPAPER_SCAN_REFUSED)
        );
    }

    #[test]
    fn a_pruned_selection_stays_on_its_window_or_takes_the_next() {
        let alive = [true, true, false, true];
        assert_eq!(overview_index_after_prune(&alive, 1), Some(1));
        assert_eq!(overview_index_after_prune(&alive, 3), Some(2));
        // The dead entry's place goes to the survivor after it.
        assert_eq!(overview_index_after_prune(&alive, 2), Some(2));
        // At the end, the last survivor.
        assert_eq!(overview_index_after_prune(&[true, true, false], 2), Some(1));
        assert_eq!(overview_index_after_prune(&[false, false], 0), None);
        assert_eq!(overview_index_after_prune(&[], 0), None);
    }
}

#[cfg(test)]
mod keyboard_grab_tests {
    use crate::backend::api::{
        Backend, BackendDiagnostics, Capabilities, ColorAllocator, CompositorAnnotation,
        CompositorBenchmark, CompositorControl, CompositorMedia, CompositorWindowEffects,
        CompositorWorkspaceEffects, CursorProvider, DisplayControl, EventHandler, InputOps, KeyOps,
        OutputOps, PropertyOps, RenderScheduler, ToastNotification, WindowOps,
    };
    use crate::backend::common_define::{KeySym, Mods, WindowId};
    use crate::backend::error::BackendError;
    use crate::backend::wayland_dummy_ops::{
        DummyColorAllocator, DummyCursorProvider, DummyInputOps, DummyOutputOps, DummyPropertyOps,
        DummyWindowOps,
    };
    use crate::core::types::Rect;
    use crate::jwm::Jwm;
    use crate::jwm::features::audio_recording::AudioRecordingState;
    use crate::jwm::types::WMArgEnum;
    use std::sync::atomic::{AtomicBool, Ordering};

    const NO_ARG: WMArgEnum = WMArgEnum::Int(0);

    /// Keyboard grab *state*, the way X11 keeps it: one grab per client, not
    /// a count, so any ungrab drops it whoever took it.
    #[derive(Default)]
    struct KeyboardGrabKeyOps {
        held: AtomicBool,
        /// Refuse the next grabs, as X11 does when another client holds the
        /// keyboard.
        refuse: AtomicBool,
    }

    impl KeyOps for KeyboardGrabKeyOps {
        fn grab_keys(
            &self,
            _root: WindowId,
            _bindings: &[(Mods, KeySym)],
        ) -> Result<(), BackendError> {
            Ok(())
        }

        fn clear_key_grabs(&self, _root: WindowId) -> Result<(), BackendError> {
            Ok(())
        }

        fn grab_keyboard(&self, _root: WindowId) -> Result<(), BackendError> {
            if self.refuse.load(Ordering::Relaxed) {
                return Err(BackendError::Message(
                    "keyboard grab refused: AlreadyGrabbed".into(),
                ));
            }
            self.held.store(true, Ordering::Relaxed);
            Ok(())
        }

        fn ungrab_keyboard(&self) -> Result<(), BackendError> {
            self.held.store(false, Ordering::Relaxed);
            Ok(())
        }

        fn clean_mods(&self, _raw_state: u16) -> Mods {
            Mods::empty()
        }

        fn keysym_from_keycode(&mut self, keycode: u8) -> Result<KeySym, BackendError> {
            Ok(u32::from(keycode))
        }

        fn clear_cache(&mut self) {}
    }

    struct GrabSpyBackend {
        window_ops: DummyWindowOps,
        input_ops: DummyInputOps,
        property_ops: DummyPropertyOps,
        output_ops: DummyOutputOps,
        key_ops: KeyboardGrabKeyOps,
        cursor_provider: DummyCursorProvider,
        color_allocator: DummyColorAllocator,
        /// Every toast pushed past the do-not-disturb gate, in order.
        toasts: Vec<ToastNotification>,
    }

    impl GrabSpyBackend {
        fn new() -> Self {
            Self {
                window_ops: DummyWindowOps,
                input_ops: DummyInputOps,
                property_ops: DummyPropertyOps,
                output_ops: DummyOutputOps,
                key_ops: KeyboardGrabKeyOps::default(),
                cursor_provider: DummyCursorProvider,
                color_allocator: DummyColorAllocator,
                toasts: Vec::new(),
            }
        }

        fn keyboard_held(&self) -> bool {
            self.key_ops.held.load(Ordering::Relaxed)
        }

        fn toast_titles(&self) -> Vec<&str> {
            self.toasts
                .iter()
                .map(|toast| toast.title.as_str())
                .collect()
        }
    }

    impl CompositorBenchmark for GrabSpyBackend {}
    impl BackendDiagnostics for GrabSpyBackend {}
    impl CompositorControl for GrabSpyBackend {}
    impl CompositorMedia for GrabSpyBackend {}
    impl CompositorWorkspaceEffects for GrabSpyBackend {
        fn compositor_push_toast(&mut self, toast: ToastNotification) {
            self.toasts.push(toast);
        }
    }
    impl CompositorWindowEffects for GrabSpyBackend {}
    impl CompositorAnnotation for GrabSpyBackend {}
    impl DisplayControl for GrabSpyBackend {}
    impl RenderScheduler for GrabSpyBackend {
        fn has_compositor(&self) -> bool {
            true
        }
    }

    impl Backend for GrabSpyBackend {
        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }

        fn root_window(&self) -> Option<WindowId> {
            Some(WindowId::from_raw(0))
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        fn check_existing_wm(&self) -> Result<(), BackendError> {
            Ok(())
        }

        fn window_ops(&self) -> &dyn WindowOps {
            &self.window_ops
        }

        fn input_ops(&self) -> &dyn InputOps {
            &self.input_ops
        }

        fn property_ops(&self) -> &dyn PropertyOps {
            &self.property_ops
        }

        fn output_ops(&self) -> &dyn OutputOps {
            &self.output_ops
        }

        fn key_ops(&self) -> &dyn KeyOps {
            &self.key_ops
        }

        fn key_ops_mut(&mut self) -> &mut dyn KeyOps {
            &mut self.key_ops
        }

        fn cursor_provider(&mut self) -> &mut dyn CursorProvider {
            &mut self.cursor_provider
        }

        fn color_allocator(&mut self) -> &mut dyn ColorAllocator {
            &mut self.color_allocator
        }

        fn run(&mut self, _handler: &mut dyn EventHandler) -> Result<(), BackendError> {
            Ok(())
        }
    }

    fn jwm(backend: &mut GrabSpyBackend) -> Jwm {
        Jwm::new_with_runtime_backend(backend, "test").expect("test jwm")
    }

    /// A capture selector started over IPC while a panel was up, then the
    /// idle lock. Taking the selector down ungrabbed the keyboard — X11 keeps
    /// one grab, not a count — and the hand-over re-took only the pointer,
    /// so the session lock went up with every keystroke reaching the focused
    /// client behind it.
    #[test]
    fn a_lock_over_a_selector_stacked_on_a_panel_keeps_the_keyboard() {
        for selector in ["screenshot", "recording region"] {
            let mut backend = GrabSpyBackend::new();
            let mut jwm = jwm(&mut backend);
            jwm.notification_center(&mut backend, &NO_ARG)
                .expect("the notification center opens");
            assert!(backend.keyboard_held());
            // What `take_screenshot` / `toggle_recording` over IPC leave
            // behind: the selector armed on top of the panel, sharing the
            // one keyboard grab.
            match selector {
                "screenshot" => jwm.features.screenshot.start(),
                _ => jwm
                    .features
                    .recording
                    .begin_initial_region_selection("recording.mp4".to_owned()),
            }

            jwm.lock_screen(&mut backend, &NO_ARG)
                .expect("the lock takes the screen");

            assert!(jwm.features.system_ui.is_session_lock(), "{selector}");
            assert!(!jwm.features.screenshot.active, "{selector}");
            assert!(!jwm.features.recording.selecting_region, "{selector}");
            assert!(
                backend.keyboard_held(),
                "the lock over a {selector} selector must hold the keyboard"
            );
        }
    }

    /// The same hand-over when the keyboard cannot be taken back: the panel
    /// that lost it is not left on screen deaf, and no lock is drawn.
    #[test]
    fn a_hand_over_that_cannot_retake_the_keyboard_fails_with_nothing_on_screen() {
        let mut backend = GrabSpyBackend::new();
        let mut jwm = jwm(&mut backend);
        jwm.notification_center(&mut backend, &NO_ARG)
            .expect("the notification center opens");
        jwm.features.screenshot.start();
        backend.key_ops.refuse.store(true, Ordering::Relaxed);

        assert!(jwm.lock_screen(&mut backend, &NO_ARG).is_err());

        assert!(!jwm.features.system_ui.is_active());
        assert!(!jwm.features.system_ui.is_locked());
        assert!(!jwm.features.screenshot.active);
        assert!(!backend.keyboard_held());
    }

    /// The recording selectors take the keyboard and the pointer like the
    /// modes that already refuse to stack; IPC reaches them over any panel.
    #[test]
    fn recording_selectors_refuse_to_start_over_a_panel() {
        let mut backend = GrabSpyBackend::new();
        let mut jwm = jwm(&mut backend);
        jwm.notification_center(&mut backend, &NO_ARG)
            .expect("the notification center opens");

        let error = jwm
            .toggle_recording(&mut backend, &NO_ARG)
            .expect_err("no region selection over the panel");
        assert!(error.to_string().contains("a system UI panel"), "{error}");
        assert!(!jwm.features.recording.selecting_region);
        assert_eq!(jwm.features.recording.pending_output_path, None);

        // A recording already running: its region adjustment is refused the
        // same way, and leaves the recording as it was.
        let region = Rect::new(100, 100, 640, 480);
        jwm.features.recording.start("recording.mp4".to_owned());
        jwm.features.recording.set_region(region);
        assert!(jwm.adjust_recording_region(&mut backend, &NO_ARG).is_err());
        assert!(!jwm.features.recording.selecting_region);
        assert!(!jwm.features.recording.adjusting_region);
        assert_eq!(jwm.features.recording.region, Some(region));
        assert!(jwm.features.system_ui.is_notification_center());
        assert!(backend.keyboard_held(), "the panel keeps its keyboard");

        // With the panel gone the adjustment starts.
        jwm.close_system_ui(&mut backend);
        jwm.adjust_recording_region(&mut backend, &NO_ARG)
            .expect("the adjustment starts over an empty screen");
        assert!(jwm.features.recording.adjusting_region);
    }

    /// The key stops the microphone without waiting for its file, so the
    /// recorder reads inactive while it still holds the device. The screen
    /// recorder's hand-off keyed on "active" alone skipped it, and its
    /// ffmpeg opened a busy device.
    #[test]
    fn a_key_stopped_microphone_recording_is_finalized_before_the_hand_off() {
        let mut backend = GrabSpyBackend::new();
        let mut jwm = jwm(&mut backend);
        let (release, finalize) = std::sync::mpsc::channel::<()>();
        jwm.features.audio_recording =
            AudioRecordingState::recording_for_test("/tmp/jwm-handoff.wav", move |stop| {
                // Stands in for the file still being finalized after the
                // stop; the bound only keeps a regression from hanging.
                let _ = finalize.recv_timeout(std::time::Duration::from_secs(30));
                if stop.load(Ordering::Acquire) {
                    Ok(())
                } else {
                    Err("finalized without being asked to stop".into())
                }
            });
        jwm.toggle_audio_recording(&mut backend, &NO_ARG)
            .expect("the key stop");
        assert!(!jwm.features.audio_recording.active);
        assert!(jwm.features.audio_recording.is_finalizing());

        // A screen recording without audio does not need the device.
        jwm.free_microphone_for_screen_recording(&mut backend, false)
            .expect("nothing to hand off");
        assert!(jwm.features.audio_recording.is_finalizing());

        // Let the recorder finish; the hand-off joins it either way.
        release.send(()).expect("the recorder is waiting");
        jwm.free_microphone_for_screen_recording(&mut backend, true)
            .expect("the hand-off");

        assert!(
            !jwm.features.audio_recording.is_finalizing(),
            "the device is free before the screen recorder opens it"
        );
        assert_eq!(
            backend.toast_titles(),
            vec!["\u{f130}  Audio recording stopped"]
        );
        jwm.poll_audio_recording(&mut backend);
        assert_eq!(backend.toasts.len(), 1, "reported once");
    }

    /// A theme the config file refused to take is live only until the next
    /// reload; the picker closes as if it were saved, so the refusal — which
    /// tells the user to set the key by hand — has to reach the screen.
    #[test]
    fn an_unsaved_theme_says_so_on_screen() {
        let mut backend = GrabSpyBackend::new();
        let mut jwm = jwm(&mut backend);

        jwm.settle_theme_persist(
            &mut backend,
            "nord",
            Err(crate::config::ConfigError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "appearance.ui_theme cannot be edited into config.toml without breaking it; \
                 the file was left unchanged, set the key by hand",
            ))),
        );

        assert_eq!(backend.toasts.len(), 1);
        let toast = &backend.toasts[0];
        assert!(toast.title.ends_with("Theme not saved"), "{}", toast.title);
        assert_eq!(toast.urgency, 2, "through do-not-disturb");
        assert!(toast.body.contains("set the key by hand"), "{}", toast.body);

        // A saved theme says nothing.
        jwm.settle_theme_persist(&mut backend, "nord", Ok(std::time::SystemTime::UNIX_EPOCH));
        assert_eq!(backend.toasts.len(), 1);
    }
}

#[cfg(test)]
mod session_action_command_tests {
    use super::session_action_command;
    use crate::external_command::test_support::{SigchldBlockedOnThisThread, SigchldProbe};

    /// Regression: suspend, reboot and shutdown ran with SIGCHLD blocked,
    /// inherited from the event thread. The command unblocks it and stays in
    /// JWM's session.
    #[test]
    fn session_actions_start_with_sigchld_unblocked_and_in_jwms_session() {
        let _blocked = SigchldBlockedOnThisThread::new();
        let probe = SigchldProbe::new("session-action");

        let mut child = session_action_command("sh", &["-c".to_owned(), probe.script()])
            .spawn()
            .expect("spawn the probe action");

        assert!(child.wait().expect("reap the probe action").success());
        assert!(!probe.child_blocked_sigchld());
        assert_eq!(probe.child_session(), unsafe { libc::getsid(0) });
    }
}
