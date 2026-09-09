//! Volume and backlight control for the OSD and control center.
//!
//! Mutations never run on the event thread: a key press or slider motion
//! queues a [`ControlRequest`] on one session-wide worker and draws an
//! optimistic estimate at once, while the worker shells out to the session's
//! native tools with a fallback chain — volume: `wpctl` (PipeWire) → `pactl`
//! (PulseAudio) → `amixer` (ALSA); the microphone's mute rides the same
//! chain against the default source; brightness: `brightnessctl` → direct
//! sysfs. The worker folds everything still queued into the newest level
//! before running, so a key-repeat storm or slider drag costs one write, not
//! one spawn per repeat, and its read-back confirms or corrects the estimate
//! from the frame tick. The first tool that works is cached for the rest of
//! the session so each step spawns one process, not three. All output
//! parsing — and the queue folding, estimates, and feedback decisions —
//! lives in pure functions so it stays testable without the tools installed.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use crate::backend::update_notifier::AsyncUpdateNotifier;

use super::sysfs::{bounded_paths, read_attribute};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioState {
    /// 0..=150 (PipeWire allows >100%; the OSD clamps its bar at 100).
    pub percent: u8,
    pub muted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VolumeTool {
    Wpctl,
    Pactl,
    Amixer,
}

static VOLUME_TOOL: OnceLock<Option<VolumeTool>> = OnceLock::new();
static BRIGHTNESS_TOOL: OnceLock<Option<BrightnessTool>> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrightnessTool {
    Brightnessctl,
    Sysfs,
}

fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let output = super::external_command::output(cmd, args).ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn run_ok(cmd: &str, args: &[&str]) -> bool {
    super::external_command::output(cmd, args).is_ok_and(|output| output.status.success())
}

// ---------------------------------------------------------------------------
// Output parsers (pure, unit-tested)
// ---------------------------------------------------------------------------

/// `wpctl get-volume @DEFAULT_AUDIO_SINK@` → `Volume: 0.45` or `Volume: 0.45 [MUTED]`.
fn parse_wpctl(output: &str) -> Option<AudioState> {
    let rest = output.trim().strip_prefix("Volume:")?.trim();
    let muted = rest.contains("[MUTED]");
    let value: f32 = rest.split_whitespace().next()?.parse().ok()?;
    Some(AudioState {
        percent: (value * 100.0).round().clamp(0.0, 150.0) as u8,
        muted,
    })
}

/// `pactl get-sink-volume @DEFAULT_SINK@` → lines containing `... / 45% / ...`.
fn parse_pactl_volume(output: &str) -> Option<u8> {
    output
        .split('/')
        .filter_map(|field| field.trim().strip_suffix('%'))
        .filter_map(|percent| percent.trim().parse::<u8>().ok())
        .next()
}

/// `pactl get-sink-mute @DEFAULT_SINK@` → `Mute: yes` / `Mute: no`.
fn parse_pactl_mute(output: &str) -> Option<bool> {
    let value = output.trim().strip_prefix("Mute:")?.trim();
    Some(value.eq_ignore_ascii_case("yes"))
}

/// `amixer get Master` → lines like `... [45%] [on]`.
fn parse_amixer(output: &str) -> Option<AudioState> {
    let line = output
        .lines()
        .find(|line| line.contains('%') && line.contains('['))?;
    let percent: u8 = line
        .split('[')
        .filter_map(|part| part.split(']').next())
        .filter_map(|part| part.strip_suffix('%'))
        .filter_map(|part| part.parse().ok())
        .next()?;
    let muted = line.contains("[off]");
    Some(AudioState { percent, muted })
}

/// `brightnessctl -m` → `intel_backlight,backlight,4800,50%,9600`.
///
/// The class column is checked rather than trusted. `brightnessctl` enumerates
/// LEDs as well as panels, and on a desktop with no panel at all the first line
/// is something like `igc-08400-led1,leds,1,100%,1` — a network card's status
/// light. Taking the first percentage in the output would report that LED as
/// the screen's brightness and, worse, let the brightness keys blink it.
fn parse_brightnessctl(output: &str) -> Option<u8> {
    output.lines().find_map(|line| {
        let mut fields = line.trim().split(',');
        let _device = fields.next()?;
        if fields.next()? != "backlight" {
            return None;
        }
        let _current = fields.next()?;
        fields.next()?.strip_suffix('%')?.parse::<u8>().ok()
    })
}

// ---------------------------------------------------------------------------
// Volume
// ---------------------------------------------------------------------------

fn detect_volume_tool() -> Option<VolumeTool> {
    *VOLUME_TOOL.get_or_init(|| {
        if run("wpctl", &["get-volume", "@DEFAULT_AUDIO_SINK@"])
            .as_deref()
            .and_then(parse_wpctl)
            .is_some()
        {
            return Some(VolumeTool::Wpctl);
        }
        if run("pactl", &["get-sink-volume", "@DEFAULT_SINK@"])
            .as_deref()
            .and_then(parse_pactl_volume)
            .is_some()
        {
            return Some(VolumeTool::Pactl);
        }
        if run("amixer", &["get", "Master"])
            .as_deref()
            .and_then(parse_amixer)
            .is_some()
        {
            return Some(VolumeTool::Amixer);
        }
        log::warn!("[controls] no working volume tool (tried wpctl, pactl, amixer)");
        None
    })
}

/// Current sink volume and mute state, or `None` when no tool works.
pub fn volume_state() -> Option<AudioState> {
    match detect_volume_tool()? {
        VolumeTool::Wpctl => parse_wpctl(&run("wpctl", &["get-volume", "@DEFAULT_AUDIO_SINK@"])?),
        VolumeTool::Pactl => {
            let percent =
                parse_pactl_volume(&run("pactl", &["get-sink-volume", "@DEFAULT_SINK@"])?)?;
            let muted = parse_pactl_mute(&run("pactl", &["get-sink-mute", "@DEFAULT_SINK@"])?)?;
            Some(AudioState { percent, muted })
        }
        VolumeTool::Amixer => parse_amixer(&run("amixer", &["get", "Master"])?),
    }
}

/// Adjust the default sink by `delta` percentage points (clamped at 100%),
/// returning the resulting state. Runs on the controls worker; the event
/// thread queues a [`ControlRequest`] instead.
fn volume_adjust(delta: i32) -> Option<AudioState> {
    let magnitude = delta.unsigned_abs();
    let ok = match detect_volume_tool()? {
        VolumeTool::Wpctl => {
            let step = format!("{magnitude}%{}", if delta >= 0 { "+" } else { "-" });
            run_ok(
                "wpctl",
                &["set-volume", "-l", "1.0", "@DEFAULT_AUDIO_SINK@", &step],
            )
        }
        VolumeTool::Pactl => {
            let step = format!("{}{magnitude}%", if delta >= 0 { "+" } else { "-" });
            // pactl has no built-in limit; clamp by reading back below.
            run_ok("pactl", &["set-sink-volume", "@DEFAULT_SINK@", &step])
        }
        VolumeTool::Amixer => {
            let step = format!("{magnitude}%{}", if delta >= 0 { "+" } else { "-" });
            run_ok("amixer", &["set", "Master", &step])
        }
    };
    if !ok {
        return None;
    }
    let state = volume_state()?;
    // Enforce the 100% ceiling for tools without a native limit flag.
    if state.percent > 100 && matches!(detect_volume_tool(), Some(VolumeTool::Pactl)) {
        let _ = run_ok("pactl", &["set-sink-volume", "@DEFAULT_SINK@", "100%"]);
        return volume_state();
    }
    Some(state)
}

/// Toggle the default sink's mute state, returning the resulting state.
/// Runs on the controls worker.
fn volume_toggle_mute() -> Option<AudioState> {
    let ok = match detect_volume_tool()? {
        VolumeTool::Wpctl => run_ok("wpctl", &["set-mute", "@DEFAULT_AUDIO_SINK@", "toggle"]),
        VolumeTool::Pactl => run_ok("pactl", &["set-sink-mute", "@DEFAULT_SINK@", "toggle"]),
        VolumeTool::Amixer => run_ok("amixer", &["set", "Master", "toggle"]),
    };
    if !ok {
        return None;
    }
    volume_state()
}

/// Set the default sink to an absolute percent (0..=100). Runs on the
/// controls worker; the slider's round-11 unmute chain wraps this in
/// [`volume_set_unmuting`].
fn volume_set(percent: u8) -> Option<AudioState> {
    let percent = percent.min(100);
    let ok = match detect_volume_tool()? {
        VolumeTool::Wpctl => run_ok(
            "wpctl",
            &["set-volume", "@DEFAULT_AUDIO_SINK@", &format!("{percent}%")],
        ),
        VolumeTool::Pactl => run_ok(
            "pactl",
            &["set-sink-volume", "@DEFAULT_SINK@", &format!("{percent}%")],
        ),
        VolumeTool::Amixer => run_ok("amixer", &["set", "Master", &format!("{percent}%")]),
    };
    if !ok {
        return None;
    }
    volume_state()
}

// ---------------------------------------------------------------------------
// Microphone mute
// ---------------------------------------------------------------------------
//
// The source half of the sink's tool chain: the one detected audio tool
// answers for the microphone too, so a session whose chain fell through to
// ALSA — or to no tool at all — behaves identically at both ends, and the
// known-absent peek the key path reads needs no microphone twin.

/// Current default-source mute state, or `None` when no tool works.
pub fn mic_mute_state() -> Option<bool> {
    match detect_volume_tool()? {
        // `get-volume` reports the source's level and its [MUTED] flag in
        // one read; only the flag is asked for here.
        VolumeTool::Wpctl => {
            Some(parse_wpctl(&run("wpctl", &["get-volume", "@DEFAULT_AUDIO_SOURCE@"])?)?.muted)
        }
        VolumeTool::Pactl => {
            parse_pactl_mute(&run("pactl", &["get-source-mute", "@DEFAULT_SOURCE@"])?)
        }
        VolumeTool::Amixer => Some(parse_amixer(&run("amixer", &["get", "Capture"])?)?.muted),
    }
}

/// Set the default source's mute flag, returning the read-back. Runs on the
/// controls worker.
fn mic_set_mute(muted: bool) -> Option<bool> {
    let flag = if muted { "1" } else { "0" };
    let ok = match detect_volume_tool()? {
        VolumeTool::Wpctl => run_ok("wpctl", &["set-mute", "@DEFAULT_AUDIO_SOURCE@", flag]),
        VolumeTool::Pactl => run_ok("pactl", &["set-source-mute", "@DEFAULT_SOURCE@", flag]),
        VolumeTool::Amixer => run_ok(
            "amixer",
            &["set", "Capture", if muted { "mute" } else { "unmute" }],
        ),
    };
    if !ok {
        return None;
    }
    mic_mute_state()
}

/// Toggle the default source's mute flag, returning the read-back. Runs on
/// the controls worker.
fn mic_toggle_mute() -> Option<bool> {
    let ok = match detect_volume_tool()? {
        VolumeTool::Wpctl => run_ok("wpctl", &["set-mute", "@DEFAULT_AUDIO_SOURCE@", "toggle"]),
        VolumeTool::Pactl => run_ok("pactl", &["set-source-mute", "@DEFAULT_SOURCE@", "toggle"]),
        VolumeTool::Amixer => run_ok("amixer", &["set", "Capture", "toggle"]),
    };
    if !ok {
        return None;
    }
    mic_mute_state()
}

// ---------------------------------------------------------------------------
// Audio devices
// ---------------------------------------------------------------------------

/// Which end of the audio pipeline a device sits on. The two are listed and
/// switched by different subcommands but are otherwise identical, so every
/// function below takes the direction rather than being written twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioDirection {
    /// Speakers, headphones, HDMI — a sink.
    Output,
    /// Microphones — a source.
    Input,
}

impl AudioDirection {
    fn wpctl_section(self) -> &'static str {
        match self {
            Self::Output => "Sinks:",
            Self::Input => "Sources:",
        }
    }

    fn pactl_noun(self) -> &'static str {
        match self {
            Self::Output => "sinks",
            Self::Input => "sources",
        }
    }

    /// Label for messages and the picker title.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Output => "output",
            Self::Input => "input",
        }
    }
}

/// One selectable audio device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioDevice {
    /// What the tool needs to make this the default: a wpctl node id or a
    /// PulseAudio node name. Opaque to everything above this module.
    pub id: String,
    /// Human-readable name, as the sound server presents it.
    pub description: String,
    pub is_default: bool,
}

/// `wpctl status`, restricted to the requested section of the audio tree.
///
/// The Video tree has a `Sources:` section too — cameras — so the scan only
/// runs between the `Audio` heading and the next top-level one. Getting this
/// wrong would offer a webcam as a microphone.
fn parse_wpctl_devices(status: &str, direction: AudioDirection) -> Vec<AudioDevice> {
    let mut devices = Vec::new();
    let mut in_audio = false;
    let mut in_section = false;

    for line in status.lines() {
        // Top-level headings carry no indentation and no tree drawing.
        let heading = line.trim_end();
        if !heading.starts_with(char::is_whitespace) && !heading.is_empty() {
            let heading = heading.trim();
            if heading.ends_with(':') || heading.contains('[') {
                // "PipeWire 'pipewire-0' [...]" and the like.
                continue;
            }
            in_audio = heading.eq_ignore_ascii_case("Audio");
            in_section = false;
            continue;
        }

        let content = line
            .trim_matches(|c: char| c.is_whitespace() || "│├└─".contains(c))
            .trim();
        if content.is_empty() {
            continue;
        }
        if content.ends_with(':') {
            in_section = in_audio && content == direction.wpctl_section();
            continue;
        }
        if !in_section {
            continue;
        }

        let is_default = content.starts_with('*');
        let entry = content.trim_start_matches('*').trim();
        let Some((id, rest)) = entry.split_once('.') else {
            continue;
        };
        let Ok(id) = id.trim().parse::<u32>() else {
            continue;
        };
        // The volume suffix is state, not identity; the row shows the name.
        let description = rest
            .split('[')
            .next()
            .unwrap_or(rest)
            .trim()
            .trim_end_matches(char::is_whitespace)
            .to_string();
        if description.is_empty() {
            continue;
        }
        devices.push(AudioDevice {
            id: id.to_string(),
            description,
            is_default,
        });
    }
    devices
}

/// `pactl list sinks` / `list sources`, whose blocks carry both the name the
/// tool needs and the description a person reads.
///
/// `default_name` is what `pactl get-default-sink` reported; monitor sources
/// are dropped because recording a sink's own output is not what "pick a
/// microphone" means.
fn parse_pactl_devices(listing: &str, default_name: &str) -> Vec<AudioDevice> {
    fn flush(
        name: &mut Option<String>,
        description: &mut Option<String>,
        is_monitor: &mut bool,
        devices: &mut Vec<AudioDevice>,
        default_name: &str,
    ) {
        if let Some(id) = name.take() {
            let description = description.take().unwrap_or_else(|| id.clone());
            if !*is_monitor {
                devices.push(AudioDevice {
                    is_default: id == default_name,
                    id,
                    description,
                });
            }
        }
        *is_monitor = false;
    }

    let mut devices = Vec::new();
    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut is_monitor = false;

    for line in listing.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Sink #") || trimmed.starts_with("Source #") {
            flush(
                &mut name,
                &mut description,
                &mut is_monitor,
                &mut devices,
                default_name,
            );
        } else if let Some(value) = trimmed.strip_prefix("Name:") {
            let value = value.trim();
            is_monitor = value.ends_with(".monitor");
            name = Some(value.to_string());
        } else if let Some(value) = trimmed.strip_prefix("Description:") {
            description = Some(value.trim().to_string());
        }
    }
    flush(
        &mut name,
        &mut description,
        &mut is_monitor,
        &mut devices,
        default_name,
    );
    devices
}

/// Leading indices of `pactl list short …` output, one per line.
fn parse_short_indices(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|field| field.chars().all(|c| c.is_ascii_digit()))
        .map(str::to_string)
        .collect()
}

/// Selectable devices for `direction`, most preferred first, or an empty list
/// when this session's audio tool cannot switch devices at all (ALSA's
/// `amixer` has no notion of a default device).
#[must_use]
pub fn audio_devices(direction: AudioDirection) -> Vec<AudioDevice> {
    match detect_volume_tool() {
        Some(VolumeTool::Wpctl) => run("wpctl", &["status"])
            .map(|status| parse_wpctl_devices(&status, direction))
            .unwrap_or_default(),
        Some(VolumeTool::Pactl) => {
            let default = run(
                "pactl",
                &[match direction {
                    AudioDirection::Output => "get-default-sink",
                    AudioDirection::Input => "get-default-source",
                }],
            )
            .unwrap_or_default();
            run("pactl", &["list", direction.pactl_noun()])
                .map(|listing| parse_pactl_devices(&listing, default.trim()))
                .unwrap_or_default()
        }
        Some(VolumeTool::Amixer) | None => Vec::new(),
    }
}

/// The device currently in use, for the control-center row.
#[must_use]
pub fn default_audio_device(direction: AudioDirection) -> Option<AudioDevice> {
    audio_devices(direction)
        .into_iter()
        .find(|device| device.is_default)
}

/// The devices in use at both ends, cached by the caller.
///
/// The control center is rebuilt on every media push, and listing devices
/// means spawning the audio tool — so the rows read from a snapshot taken
/// when the panel opens and after a switch, not on every repaint.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudioDefaults {
    pub output: Option<AudioDevice>,
    pub input: Option<AudioDevice>,
}

/// Both halves of the sound-server topology from one coherent read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudioInventory {
    pub output: Vec<AudioDevice>,
    pub input: Vec<AudioDevice>,
}

impl AudioInventory {
    #[must_use]
    pub fn devices(&self, direction: AudioDirection) -> &[AudioDevice] {
        match direction {
            AudioDirection::Output => &self.output,
            AudioDirection::Input => &self.input,
        }
    }

    #[must_use]
    pub fn defaults(&self) -> AudioDefaults {
        AudioDefaults {
            output: self.output.iter().find(|device| device.is_default).cloned(),
            input: self.input.iter().find(|device| device.is_default).cloned(),
        }
    }
}

fn parse_wpctl_inventory(status: &str) -> AudioInventory {
    AudioInventory {
        output: parse_wpctl_devices(status, AudioDirection::Output),
        input: parse_wpctl_devices(status, AudioDirection::Input),
    }
}

/// Read both directions. PipeWire exposes them in one `wpctl status`, so this
/// is also the primitive for IPC snapshots and post-switch verification.
#[must_use]
pub fn audio_inventory() -> AudioInventory {
    match detect_volume_tool() {
        Some(VolumeTool::Wpctl) => run("wpctl", &["status"])
            .map_or_else(AudioInventory::default, |status| {
                parse_wpctl_inventory(&status)
            }),
        Some(VolumeTool::Pactl) => AudioInventory {
            output: audio_devices(AudioDirection::Output),
            input: audio_devices(AudioDirection::Input),
        },
        Some(VolumeTool::Amixer) | None => AudioInventory::default(),
    }
}

impl AudioDefaults {
    /// Read both ends from the sound server.
    #[must_use]
    pub fn read() -> Self {
        audio_inventory().defaults()
    }

    /// Name of the device in use, or `None` when this session cannot switch
    /// devices and the row should not appear at all.
    #[must_use]
    pub fn name(&self, direction: AudioDirection) -> Option<&str> {
        match direction {
            AudioDirection::Output => self.output.as_ref(),
            AudioDirection::Input => self.input.as_ref(),
        }
        .map(|device| device.description.as_str())
    }
}

/// Slow, externally sourced rows shown by the Shell Hub.
///
/// Everything here may spawn a session tool. Keeping it in one immutable
/// value lets the event loop rebuild the panel from memory while a worker
/// refreshes the next snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ControlCenterSnapshot {
    pub volume: Option<AudioState>,
    /// The default microphone's mute flag. The control-center Input row
    /// swaps its icon for the muted microphone while this is `Some(true)`,
    /// and it is the confirmed base the mic-mute key's optimistic flip
    /// estimates from, adopted from the controls worker's read-backs.
    pub mic_muted: Option<bool>,
    pub brightness: Option<u8>,
    pub audio_defaults: AudioDefaults,
    /// Both full device lists, not just the two in use: `get_audio_devices`
    /// answers with the whole inventory, and a query that had to fork
    /// `wpctl status` to build it would stall a frame every time a bar
    /// polled. The defaults above are derived from this same read, so the
    /// row and the query can never disagree.
    pub audio_inventory: AudioInventory,
    pub power_profiles: Option<(Vec<String>, String)>,
}

impl ControlCenterSnapshot {
    /// Read every slow control domain. This must run on a background worker;
    /// the function is intentionally synchronous so each domain keeps its
    /// established fallback order and error semantics.
    #[must_use]
    pub fn read() -> Self {
        // One inventory read feeds both fields: the `AudioDefaults` reader is
        // itself an inventory read plus `defaults()`, so asking for it
        // separately would fork the audio tool twice per worker pass and
        // could return two views of a topology that changed in between.
        let audio_inventory = audio_inventory();
        Self {
            volume: volume_state(),
            mic_muted: mic_mute_state(),
            brightness: brightness_percent(),
            audio_defaults: audio_inventory.defaults(),
            audio_inventory,
            power_profiles: crate::jwm::features::power::profiles(),
        }
    }
}

/// A visible control center refreshes volatile values often enough to follow
/// external changes, but never performs the reads on the compositor thread.
pub const CONTROL_CENTER_SNAPSHOT_TTL: Duration = Duration::from_secs(2);

#[must_use]
pub fn control_center_snapshot_is_stale(refreshed_at: Option<Instant>, now: Instant) -> bool {
    refreshed_at.is_none_or(|refreshed_at| {
        now.saturating_duration_since(refreshed_at) >= CONTROL_CENTER_SNAPSHOT_TTL
    })
}

/// Make `id` the default device, taking already-playing streams with it.
///
/// Moving the streams is the whole point of the switch: plugging in
/// headphones and having the music stay in the speakers is the failure this
/// avoids. WirePlumber does it on its own; PulseAudio needs to be told.
pub fn set_audio_device(direction: AudioDirection, id: &str) -> bool {
    match detect_volume_tool() {
        Some(VolumeTool::Wpctl) => run_ok("wpctl", &["set-default", id]),
        Some(VolumeTool::Pactl) => {
            let (set, list, move_stream) = match direction {
                AudioDirection::Output => ("set-default-sink", "sink-inputs", "move-sink-input"),
                AudioDirection::Input => {
                    ("set-default-source", "source-outputs", "move-source-output")
                }
            };
            if !run_ok("pactl", &[set, id]) {
                return false;
            }
            let streams = run("pactl", &["list", "short", list]).unwrap_or_default();
            for index in parse_short_indices(&streams) {
                // A stream that refuses to move (a dead client, a filter) is
                // not a reason to report the switch as failed.
                let _ = run_ok("pactl", &[move_stream, &index, id]);
            }
            true
        }
        Some(VolumeTool::Amixer) | None => false,
    }
}

/// One picker row: a filled marker for the device in use, hollow otherwise.
#[must_use]
pub fn device_row(device: &AudioDevice) -> String {
    let marker = if device.is_default {
        "\u{f192}" // fa-dot-circle-o
    } else {
        "\u{f10c}" // fa-circle-o
    };
    format!("{marker}  {}", device.description)
}

/// The control-center row for the device currently in use.
#[must_use]
pub fn device_control_row(direction: AudioDirection, device: Option<&AudioDevice>) -> String {
    let (icon, label) = match direction {
        AudioDirection::Output => ("\u{f028}", "Output"), // fa-volume-up
        AudioDirection::Input => ("\u{f130}", "Input"),   // fa-microphone
    };
    let name = device.map_or("none", |device| device.description.as_str());
    format!("{icon}  {label:<12} {name}")
}

// ---------------------------------------------------------------------------
// Brightness
// ---------------------------------------------------------------------------

const MAX_BACKLIGHT_ENTRIES: usize = 64;

fn sysfs_backlight_in(root: &Path) -> Option<PathBuf> {
    bounded_paths(root, MAX_BACKLIGHT_ENTRIES)?
        .into_iter()
        .next()
}

fn sysfs_backlight() -> Option<PathBuf> {
    sysfs_backlight_in(Path::new("/sys/class/backlight"))
}

fn brightness_percent_from_raw(current: u32, max: u32) -> Option<u8> {
    if max == 0 {
        return None;
    }
    let max = u64::from(max);
    let rounded = (u64::from(current) * 100 + max / 2) / max;
    Some(rounded.min(100) as u8)
}

fn raw_brightness_for_percent(percent: u8, max: u32) -> Option<u32> {
    if max == 0 {
        return None;
    }
    let raw = (u64::from(percent.min(100)) * u64::from(max) + 50) / 100;
    u32::try_from(raw).ok()
}

fn sysfs_brightness_percent_in(dir: &Path) -> Option<u8> {
    let read_u32 =
        |name: &str| -> Option<u32> { read_attribute(dir.join(name))?.trim().parse().ok() };
    brightness_percent_from_raw(read_u32("brightness")?, read_u32("max_brightness")?)
}

fn sysfs_brightness_percent() -> Option<u8> {
    let dir = sysfs_backlight()?;
    sysfs_brightness_percent_in(&dir)
}

/// Every `brightnessctl` call is pinned to the backlight class.
///
/// Without it the tool operates on whatever device it lists first, which on a
/// desktop with no panel is an LED belonging to some unrelated device.
const BACKLIGHT_CLASS: [&str; 2] = ["-c", "backlight"];

fn brightnessctl(args: &[&str]) -> Vec<String> {
    BACKLIGHT_CLASS
        .iter()
        .chain(args)
        .map(|arg| (*arg).to_string())
        .collect()
}

fn run_brightnessctl(args: &[&str]) -> Option<String> {
    let args = brightnessctl(args);
    run(
        "brightnessctl",
        &args.iter().map(String::as_str).collect::<Vec<_>>(),
    )
}

fn run_brightnessctl_ok(args: &[&str]) -> bool {
    let args = brightnessctl(args);
    run_ok(
        "brightnessctl",
        &args.iter().map(String::as_str).collect::<Vec<_>>(),
    )
}

fn detect_brightness_tool() -> Option<BrightnessTool> {
    *BRIGHTNESS_TOOL.get_or_init(|| {
        if run_brightnessctl(&["-m"])
            .as_deref()
            .and_then(parse_brightnessctl)
            .is_some()
        {
            return Some(BrightnessTool::Brightnessctl);
        }
        if sysfs_brightness_percent().is_some() {
            return Some(BrightnessTool::Sysfs);
        }
        log::warn!("[controls] no backlight control (tried brightnessctl, /sys/class/backlight)");
        None
    })
}

/// Current backlight level in percent, or `None` without a backlight.
pub fn brightness_percent() -> Option<u8> {
    match detect_brightness_tool()? {
        BrightnessTool::Brightnessctl => parse_brightnessctl(&run_brightnessctl(&["-m"])?),
        BrightnessTool::Sysfs => sysfs_brightness_percent(),
    }
}

fn sysfs_set_percent_in(dir: &Path, percent: u8) -> Option<u8> {
    let max: u32 = read_attribute(dir.join("max_brightness"))?
        .trim()
        .parse()
        .ok()?;
    let raw = raw_brightness_for_percent(percent, max)?;
    // Direct sysfs writes need udev backlight permissions; failure falls
    // through to None and the OSD simply is not shown.
    std::fs::write(dir.join("brightness"), raw.to_string()).ok()?;
    sysfs_brightness_percent_in(dir)
}

fn sysfs_set_percent(percent: u8) -> Option<u8> {
    let dir = sysfs_backlight()?;
    sysfs_set_percent_in(&dir, percent)
}

/// Adjust the backlight by `delta` percentage points, returning the result.
/// Runs on the controls worker (the sysfs half spawns nothing, but it rides
/// the same queue so every mutation keeps one ordered path).
fn brightness_adjust(delta: i32) -> Option<u8> {
    match detect_brightness_tool()? {
        BrightnessTool::Brightnessctl => {
            let magnitude = delta.unsigned_abs();
            let step = if delta >= 0 {
                format!("{magnitude}%+")
            } else {
                // `-n1` keeps at least a minimal raw level so the panel never
                // turns fully black from a key repeat.
                format!("{magnitude}%-")
            };
            if !run_brightnessctl_ok(&["-n1", "set", &step]) {
                return None;
            }
            brightness_percent()
        }
        BrightnessTool::Sysfs => {
            let current = i32::from(sysfs_brightness_percent()?);
            sysfs_set_percent(current.saturating_add(delta).clamp(1, 100) as u8)
        }
    }
}

/// Set the backlight to an absolute percent. Runs on the controls worker.
fn brightness_set(percent: u8) -> Option<u8> {
    match detect_brightness_tool()? {
        BrightnessTool::Brightnessctl => {
            if !run_brightnessctl_ok(&["-n1", "set", &format!("{}%", percent.min(100))]) {
                return None;
            }
            brightness_percent()
        }
        BrightnessTool::Sysfs => sysfs_set_percent(percent.max(1)),
    }
}

// ---------------------------------------------------------------------------
// Off-thread control mutations
// ---------------------------------------------------------------------------
//
// Every mutation above blocks on a session tool for up to the helper timeout
// — a set plus its read-back, sometimes twice for the unmute chain — so none
// of them may run on the event thread. Key presses and slider drags queue a
// `ControlRequest` here and draw an optimistic estimate at once; the worker
// drains the queue, folds what it finds pending so only the newest level is
// applied, and publishes the read-back for the frame tick to confirm or
// correct the estimate.

/// One requested change, in the order the user asked for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ControlRequest {
    /// Relative nudge in percentage points (volume keys, arrows,
    /// scroll-on-slider).
    VolumeAdjust(i32),
    /// Absolute level from click-to-position and slider drags. On a muted
    /// sink the worker unmutes after the set — pointing at a level is an
    /// explicit ask for that much sound.
    VolumeSet(u8),
    /// An event, not a value: it is never folded into a level, and only an
    /// adjacent twin cancels it (two flips with nothing between are no flip).
    VolumeToggleMute,
    /// Microphone mute as an absolute target. A value like the level sets:
    /// two queued sets fold to the newest. The IPC `set_mic_mute` command
    /// constructs it; the toggle key remains the only mic interaction that
    /// is an event rather than a value.
    MicMuteSet(bool),
    /// The microphone half of [`Self::VolumeToggleMute`]: an event, never
    /// folded into a set, cancelled only by an adjacent twin.
    MicMuteToggle,
    BrightnessAdjust(i32),
    BrightnessSet(u8),
    /// Make a device the default sink/source (the audio picker's Enter). A
    /// value like the sets: two queued switches fold to the newest. The
    /// worker answers with the set's own result *and* a full re-read of the
    /// topology, because a sound server routinely accepts the request and
    /// then puts the default back.
    AudioSetDefault {
        direction: AudioDirection,
        /// wpctl node id or PulseAudio node name, as the picker listed it.
        id: String,
    },
}

/// Which control a request, an estimate, or a report concerns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ControlDomain {
    Volume,
    Brightness,
    /// The default microphone's mute flag.
    MicMute,
    /// The default audio device. It never carries an OSD estimate or debt —
    /// its feedback is the picker's re-read rows — so the OSD-side matches
    /// turn it away empty-handed.
    AudioDevice,
}

impl ControlRequest {
    fn domain(&self) -> ControlDomain {
        match self {
            Self::VolumeAdjust(_) | Self::VolumeSet(_) | Self::VolumeToggleMute => {
                ControlDomain::Volume
            }
            Self::BrightnessAdjust(_) | Self::BrightnessSet(_) => ControlDomain::Brightness,
            Self::MicMuteSet(_) | Self::MicMuteToggle => ControlDomain::MicMute,
            Self::AudioSetDefault { .. } => ControlDomain::AudioDevice,
        }
    }
}

/// A request plus its submission sequence. Reports carry the sequence of the
/// newest request they cover, so the event thread can tell a read-back that
/// confirms the estimate on screen from one that predates it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct QueuedRequest {
    seq: u64,
    request: ControlRequest,
}

/// `percent + delta` within a domain's on-screen bounds: the floor is 0 for
/// volume and 1 for brightness — both brightness backends floor a decrease
/// at a nonzero level (`brightnessctl -n1`, sysfs's `max(1)`).
fn adjusted_level(percent: u8, delta: i32, floor: u8) -> u8 {
    (i32::from(percent) + delta).clamp(i32::from(floor), 100) as u8
}

/// Fold two queued value requests for one domain into the single write that
/// reaches the same end state. Only ever applied to requests still queued —
/// nothing has run yet, so the fold itself is unobservable.
fn merge_value_requests(pending: &ControlRequest, next: &ControlRequest) -> ControlRequest {
    match (pending, next) {
        // Relative nudges add up: ten queued repeats of +5 are one +50.
        (ControlRequest::VolumeAdjust(a), ControlRequest::VolumeAdjust(b)) => {
            ControlRequest::VolumeAdjust(a.saturating_add(*b))
        }
        (ControlRequest::BrightnessAdjust(a), ControlRequest::BrightnessAdjust(b)) => {
            ControlRequest::BrightnessAdjust(a.saturating_add(*b))
        }
        // A nudge after a queued set retargets the set itself.
        (ControlRequest::VolumeSet(percent), ControlRequest::VolumeAdjust(delta)) => {
            ControlRequest::VolumeSet(adjusted_level(*percent, *delta, 0))
        }
        (ControlRequest::BrightnessSet(percent), ControlRequest::BrightnessAdjust(delta)) => {
            ControlRequest::BrightnessSet(adjusted_level(*percent, *delta, 1))
        }
        // An absolute set — or a device switch — makes whatever was queued
        // before it moot.
        (_, ControlRequest::VolumeSet(_))
        | (_, ControlRequest::BrightnessSet(_))
        | (_, ControlRequest::MicMuteSet(_))
        | (_, ControlRequest::AudioSetDefault { .. }) => next.clone(),
        // Toggles never reach here and cross-domain pairs are never merged;
        // the folder below guarantees both.
        _ => pending.clone(),
    }
}

/// The newest submission sequence folded away per toggle domain in one
/// drain. A cancelled toggle pair changed no state, but the estimate on
/// screen is still owed a read-back whose sequence covers the cancelled
/// submissions — tracked per domain so a cancelled sink pair never forces a
/// microphone read-back (an extra spawn for nothing) nor vice versa.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct CancelledToggles {
    volume: Option<u64>,
    mic: Option<u64>,
}

/// Fold a queued request into the batch the worker is about to run.
///
/// Latest value wins: a value request merges into the batch's last pending
/// value for its domain. A mute toggle is an event, not a value — it never
/// merges and never drops, and a value behind a toggle queues instead of
/// merging across it, because the set-on-muted unmute chain depends on the
/// order. The one fold a toggle allows is against its own adjacent twin:
/// two flips with nothing between are no flip, and cancelling the pair keeps
/// a toggle flood from piling up behind a hung helper. Sink and microphone
/// toggles are different events in different domains: they never cancel
/// each other. `cancelled` records the newest sequence folded away per
/// toggle domain this drain so the worker can still publish a read-back
/// covering it.
fn fold_request(
    batch: &mut Vec<QueuedRequest>,
    cancelled: &mut CancelledToggles,
    next: QueuedRequest,
) {
    let domain = next.request.domain();
    let last = batch
        .iter()
        .rposition(|queued| queued.request.domain() == domain);
    match (&next.request, last) {
        (ControlRequest::VolumeToggleMute, Some(index))
            if batch[index].request == ControlRequest::VolumeToggleMute =>
        {
            let removed = batch.remove(index);
            cancelled.volume = Some(removed.seq.max(next.seq));
        }
        (ControlRequest::VolumeToggleMute, _) => batch.push(next),
        (ControlRequest::MicMuteToggle, Some(index))
            if batch[index].request == ControlRequest::MicMuteToggle =>
        {
            let removed = batch.remove(index);
            cancelled.mic = Some(removed.seq.max(next.seq));
        }
        (ControlRequest::MicMuteToggle, _) => batch.push(next),
        (_, Some(index))
            if !matches!(
                batch[index].request,
                ControlRequest::VolumeToggleMute | ControlRequest::MicMuteToggle
            ) =>
        {
            let merged = merge_value_requests(&batch[index].request, &next.request);
            batch[index] = QueuedRequest {
                seq: batch[index].seq.max(next.seq),
                request: merged,
            };
        }
        // The first request for a domain, or a value behind a toggle.
        _ => batch.push(next),
    }
}

/// What the worker confirmed after applying a batch: per domain, the
/// read-back after its newest executed request — or that request's failure.
/// A domain left `None` was untouched by the batch.
#[derive(Debug, Default, Clone)]
pub(crate) struct ControlReport {
    pub volume: Option<VolumeReport>,
    pub brightness: Option<BrightnessReport>,
    pub audio: Option<AudioReport>,
    pub mic: Option<MicReport>,
}

/// What the worker confirmed after a device switch: the set's own answer and,
/// decisively, the topology re-read that follows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AudioReport {
    /// Submission this answer covers; the newest switch's report is the only
    /// one worth keeping, and the publish slot overwrites accordingly.
    pub seq: u64,
    pub direction: AudioDirection,
    /// The device the user asked for.
    pub asked_id: String,
    /// `set_audio_device`'s own answer. Not evidence on its own — a sound
    /// server routinely accepts the request and then puts the default back —
    /// but it still tells "the tool refused outright" apart from "the tool
    /// said yes and the server disagreed".
    pub asked_ok: bool,
    /// Both directions re-read after the set; the picker and the
    /// control-center rows adopt this, never the request.
    pub inventory: AudioInventory,
}

/// What the picker's re-read says about a requested switch. Pure so the
/// adopt/revert decision is testable without a sound server: `took` is the
/// adopt case (the marker moves to the asked device), a kept old default is
/// the revert case, and the message says which happened — the same four
/// outcomes the synchronous path reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AudioSwitchVerdict {
    /// Whether the asked device actually became the default.
    pub took: bool,
    /// What reports as default after the re-read, when anything does.
    pub in_use: Option<String>,
    /// The picker status line for the outcome.
    pub message: String,
}

/// Decide what a device switch resolved to, believing the re-read over the
/// request — an HDMI output with no monitor or a headset microphone with no
/// headset accepts the set and still never becomes the default.
#[must_use]
pub(crate) fn audio_switch_verdict(report: &AudioReport) -> AudioSwitchVerdict {
    let devices = report.inventory.devices(report.direction);
    let took = devices
        .iter()
        .any(|device| device.id == report.asked_id && device.is_default);
    let in_use = report
        .inventory
        .defaults()
        .name(report.direction)
        .map(str::to_string);
    let message = match (took, in_use.as_deref()) {
        (true, Some(name)) => format!("Using {name}"),
        (false, Some(name)) => format!("Unavailable \u{2014} still using {name}"),
        (_, None) if report.asked_ok => "Switched, but nothing reports as default".to_string(),
        (_, None) => "Could not switch device".to_string(),
    };
    AudioSwitchVerdict {
        took,
        in_use,
        message,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VolumeReport {
    /// Read-back after the newest applied request; the sequence says which
    /// submissions this answer covers.
    Applied(u64, AudioState),
    /// The change or its read-back did not take — the same `None` the
    /// synchronous path returned.
    Failed(u64),
}

impl VolumeReport {
    fn split(self) -> (u64, Option<AudioState>) {
        match self {
            Self::Applied(seq, state) => (seq, Some(state)),
            Self::Failed(seq) => (seq, None),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BrightnessReport {
    Applied(u64, u8),
    Failed(u64),
}

impl BrightnessReport {
    fn split(self) -> (u64, Option<u8>) {
        match self {
            Self::Applied(seq, percent) => (seq, Some(percent)),
            Self::Failed(seq) => (seq, None),
        }
    }
}

/// The microphone counterpart of [`VolumeReport`]: the read-back carries
/// only the mute flag — the mic card has no level to confirm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MicReport {
    /// Read-back after the newest applied mic request; the sequence says
    /// which submissions this answer covers.
    Applied(u64, bool),
    /// The change or its read-back did not take — the same `None` the
    /// synchronous path returned.
    Failed(u64),
}

impl MicReport {
    fn split(self) -> (u64, Option<bool>) {
        match self {
            Self::Applied(seq, muted) => (seq, Some(muted)),
            Self::Failed(seq) => (seq, None),
        }
    }
}

/// The one session-wide control queue. An unbounded channel is still bounded
/// in practice — the worker folds everything pending each time it finishes a
/// step — but the fold, not the channel, is what guarantees a drag never
/// piles up a backlog of stale levels.
struct ControlsWorker {
    sender: mpsc::Sender<QueuedRequest>,
    report: Arc<Mutex<ControlReport>>,
    notifier: Arc<Mutex<Option<AsyncUpdateNotifier>>>,
    next_seq: AtomicU64,
    /// Whether the OS gave the worker a thread. When it did not, every
    /// submission fails fast and callers keep their old error paths rather
    /// than queueing work nothing will ever run.
    started: bool,
}

static CONTROLS_WORKER: OnceLock<ControlsWorker> = OnceLock::new();

fn controls_worker() -> &'static ControlsWorker {
    CONTROLS_WORKER.get_or_init(ControlsWorker::start)
}

impl ControlsWorker {
    fn start() -> Self {
        let (sender, receiver) = mpsc::channel();
        let report = Arc::new(Mutex::new(ControlReport::default()));
        let notifier = Arc::new(Mutex::new(None));
        // `std::thread::spawn` panics when the OS refuses a thread (a pids
        // cgroup limit, RLIMIT_NPROC); a named Builder turns that into a
        // logged warning, like `BackgroundJob::spawn`.
        let spawned = std::thread::Builder::new()
            .name("jwm-controls".into())
            .spawn({
                let report = Arc::clone(&report);
                let notifier = Arc::clone(&notifier);
                move || run_control_queue(&receiver, &report, &notifier)
            });
        if let Err(error) = &spawned {
            log::warn!("[controls] could not spawn the controls worker thread: {error}");
        }
        Self {
            sender,
            report,
            notifier,
            next_seq: AtomicU64::new(1),
            started: spawned.is_ok(),
        }
    }
}

/// `volume_set` plus the round-11 unmute chain: a level set on a muted sink
/// unmutes it, because pointing at a level is an explicit ask for that much
/// sound, and both wpctl and pactl keep the mute flag on a plain set-volume.
/// The state after the unmute is the one adopted — the same end state the
/// synchronous slider path produced.
fn volume_set_unmuting(percent: u8) -> Option<AudioState> {
    let state = volume_set(percent)?;
    if state.muted {
        volume_toggle_mute().or(Some(state))
    } else {
        Some(state)
    }
}

fn run_control_queue(
    receiver: &mpsc::Receiver<QueuedRequest>,
    report: &Mutex<ControlReport>,
    notifier: &Mutex<Option<AsyncUpdateNotifier>>,
) {
    while let Ok(first) = receiver.recv() {
        let mut batch = vec![first];
        let mut cancelled = CancelledToggles::default();
        // Everything submitted while the previous step ran — a slider drag's
        // worth of levels — folds into the newest one before any of it runs.
        while let Ok(next) = receiver.try_recv() {
            fold_request(&mut batch, &mut cancelled, next);
        }

        let mut outcome = ControlReport::default();
        for queued in batch {
            match queued.request {
                ControlRequest::VolumeAdjust(delta) => {
                    outcome.volume = Some(
                        volume_adjust(delta).map_or(VolumeReport::Failed(queued.seq), |state| {
                            VolumeReport::Applied(queued.seq, state)
                        }),
                    );
                }
                ControlRequest::VolumeSet(percent) => {
                    outcome.volume = Some(
                        volume_set_unmuting(percent)
                            .map_or(VolumeReport::Failed(queued.seq), |state| {
                                VolumeReport::Applied(queued.seq, state)
                            }),
                    );
                }
                ControlRequest::VolumeToggleMute => {
                    outcome.volume = Some(
                        volume_toggle_mute().map_or(VolumeReport::Failed(queued.seq), |state| {
                            VolumeReport::Applied(queued.seq, state)
                        }),
                    );
                }
                ControlRequest::MicMuteSet(muted) => {
                    outcome.mic = Some(
                        mic_set_mute(muted).map_or(MicReport::Failed(queued.seq), |state| {
                            MicReport::Applied(queued.seq, state)
                        }),
                    );
                }
                ControlRequest::MicMuteToggle => {
                    outcome.mic = Some(
                        mic_toggle_mute().map_or(MicReport::Failed(queued.seq), |muted| {
                            MicReport::Applied(queued.seq, muted)
                        }),
                    );
                }
                ControlRequest::BrightnessAdjust(delta) => {
                    outcome.brightness = Some(
                        brightness_adjust(delta)
                            .map_or(BrightnessReport::Failed(queued.seq), |percent| {
                                BrightnessReport::Applied(queued.seq, percent)
                            }),
                    );
                }
                ControlRequest::BrightnessSet(percent) => {
                    outcome.brightness = Some(
                        brightness_set(percent)
                            .map_or(BrightnessReport::Failed(queued.seq), |percent| {
                                BrightnessReport::Applied(queued.seq, percent)
                            }),
                    );
                }
                ControlRequest::AudioSetDefault { direction, id } => {
                    // The set's exit code is not the answer; the re-read is.
                    // Both run here, never on the event thread — two bounded-
                    // but-blocking spawns, seconds of stall behind a hung
                    // wpctl, is the round-13 bug shape.
                    let asked_ok = set_audio_device(direction, &id);
                    let inventory = audio_inventory();
                    outcome.audio = Some(AudioReport {
                        seq: queued.seq,
                        direction,
                        asked_id: id,
                        asked_ok,
                        inventory,
                    });
                }
            }
        }
        // A batch reduced to nothing by a cancelled toggle pair changed no
        // state, but the estimate on screen still needs a read-back whose
        // sequence covers the cancelled submissions. When the batch did run
        // a command for that domain, its answer — success or failure —
        // already ran after everything the cancelled pair could have
        // toggled, so it covers those submissions too.
        if let Some(cancelled_seq) = cancelled.volume {
            match &mut outcome.volume {
                Some(report) => {
                    let seq = match report {
                        VolumeReport::Applied(seq, _) | VolumeReport::Failed(seq) => seq,
                    };
                    *seq = (*seq).max(cancelled_seq);
                }
                None => {
                    outcome.volume = Some(
                        volume_state().map_or(VolumeReport::Failed(cancelled_seq), |state| {
                            VolumeReport::Applied(cancelled_seq, state)
                        }),
                    );
                }
            }
        }
        if let Some(cancelled_seq) = cancelled.mic {
            match &mut outcome.mic {
                Some(report) => {
                    let seq = match report {
                        MicReport::Applied(seq, _) | MicReport::Failed(seq) => seq,
                    };
                    *seq = (*seq).max(cancelled_seq);
                }
                None => {
                    outcome.mic = Some(
                        mic_mute_state().map_or(MicReport::Failed(cancelled_seq), |muted| {
                            MicReport::Applied(cancelled_seq, muted)
                        }),
                    );
                }
            }
        }
        if outcome.volume.is_none()
            && outcome.brightness.is_none()
            && outcome.audio.is_none()
            && outcome.mic.is_none()
        {
            continue;
        }

        let notifier = {
            let mut guard = report.lock().unwrap_or_else(PoisonError::into_inner);
            // Only the newest answer per domain is worth waking for; one the
            // tick has not collected yet is simply overwritten.
            if outcome.volume.is_some() {
                guard.volume = outcome.volume;
            }
            if outcome.brightness.is_some() {
                guard.brightness = outcome.brightness;
            }
            if outcome.audio.is_some() {
                guard.audio = outcome.audio;
            }
            if outcome.mic.is_some() {
                guard.mic = outcome.mic;
            }
            // Publish before signalling, mirroring `BackgroundJob`: a handler
            // woken by the eventfd must find the value already visible.
            notifier
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        };
        if let Some(notifier) = notifier {
            notifier.notify();
        }
    }
}

/// Queue a mutation on the controls worker and (re)attach the event loop's
/// wakeup. Returns the submission's sequence number — the report that covers
/// it carries one at least as new — or `None` when no worker thread exists
/// to run it, in which case nothing was queued.
pub(crate) fn queue_control_request(
    request: ControlRequest,
    notifier: Option<AsyncUpdateNotifier>,
) -> Option<u64> {
    let worker = controls_worker();
    if !worker.started {
        return None;
    }
    {
        let mut guard = worker
            .notifier
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *guard = notifier;
    }
    let seq = worker.next_seq.fetch_add(1, Ordering::Relaxed);
    // The send cannot fail while the worker holds the receiver, but if a
    // panic ever took that thread down, fail the submission here instead of
    // queueing work nothing will run.
    worker.sender.send(QueuedRequest { seq, request }).ok()?;
    Some(seq)
}

/// The newest uncollected read-backs, if the worker published any since the
/// last poll. Never spawns the worker just to ask.
pub(crate) fn take_control_report() -> Option<ControlReport> {
    let worker = CONTROLS_WORKER.get()?;
    let mut guard = worker.report.lock().unwrap_or_else(PoisonError::into_inner);
    let taken = std::mem::take(&mut *guard);
    (taken.volume.is_some()
        || taken.brightness.is_some()
        || taken.audio.is_some()
        || taken.mic.is_some())
    .then_some(taken)
}

/// Whether detection already concluded that no volume tool works — the one
/// answer the event thread may read without spawning anything, so the key
/// binding keeps its old error path instead of drawing an estimate the
/// worker would only have to take back.
pub(crate) fn volume_tool_known_absent() -> bool {
    matches!(VOLUME_TOOL.get(), Some(None))
}

/// The brightness counterpart of [`volume_tool_known_absent`].
pub(crate) fn brightness_tool_known_absent() -> bool {
    matches!(BRIGHTNESS_TOOL.get(), Some(None))
}

// ---------------------------------------------------------------------------
// Optimistic on-screen feedback
// ---------------------------------------------------------------------------

/// The volume to show before the worker confirms. A `None` base means no
/// read has ever landed — show nothing rather than invent a level; the
/// read-back then owns the first card.
pub(crate) fn optimistic_volume(
    base: Option<AudioState>,
    request: &ControlRequest,
) -> Option<AudioState> {
    match request {
        // A set needs no base, and the worker's unmute chain makes the
        // estimate unmuted: pointing at a level is an ask for that much sound.
        ControlRequest::VolumeSet(percent) => Some(AudioState {
            percent: (*percent).min(100),
            muted: false,
        }),
        // A plain adjust never unmutes, and every backend caps the result of
        // an adjust at 100 — including one that started above it.
        ControlRequest::VolumeAdjust(delta) => base.map(|base| AudioState {
            percent: adjusted_level(base.percent, *delta, 0),
            muted: base.muted,
        }),
        ControlRequest::VolumeToggleMute => base.map(|base| AudioState {
            muted: !base.muted,
            ..base
        }),
        ControlRequest::BrightnessAdjust(_)
        | ControlRequest::BrightnessSet(_)
        | ControlRequest::MicMuteSet(_)
        | ControlRequest::MicMuteToggle
        | ControlRequest::AudioSetDefault { .. } => None,
    }
}

/// The brightness to show before the worker confirms; same rules as
/// [`optimistic_volume`], with the brightness floor of 1.
pub(crate) fn optimistic_brightness(base: Option<u8>, request: &ControlRequest) -> Option<u8> {
    match request {
        ControlRequest::BrightnessSet(percent) => Some((*percent).clamp(1, 100)),
        ControlRequest::BrightnessAdjust(delta) => base.map(|base| adjusted_level(base, *delta, 1)),
        ControlRequest::VolumeAdjust(_)
        | ControlRequest::VolumeSet(_)
        | ControlRequest::VolumeToggleMute
        | ControlRequest::MicMuteSet(_)
        | ControlRequest::MicMuteToggle
        | ControlRequest::AudioSetDefault { .. } => None,
    }
}

/// The microphone mute flag to show before the worker confirms; same rules
/// as [`optimistic_volume`]: a set needs no base (it knows its target), a
/// toggle flips the base, and a `None` base shows nothing — the read-back
/// owns the first card.
pub(crate) fn optimistic_mic_mute(base: Option<bool>, request: &ControlRequest) -> Option<bool> {
    match request {
        ControlRequest::MicMuteSet(muted) => Some(*muted),
        ControlRequest::MicMuteToggle => base.map(|muted| !muted),
        ControlRequest::VolumeAdjust(_)
        | ControlRequest::VolumeSet(_)
        | ControlRequest::VolumeToggleMute
        | ControlRequest::BrightnessAdjust(_)
        | ControlRequest::BrightnessSet(_)
        | ControlRequest::AudioSetDefault { .. } => None,
    }
}

/// An estimate drawn ahead of the worker's confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OptimisticValue<T> {
    /// Submission the estimate came from. A read-back with a smaller
    /// sequence does not cover it.
    pub(crate) seq: u64,
    /// What is on screen right now.
    pub(crate) shown: T,
    /// The last confirmed value — what a failed change reverts to. Kept
    /// across chained estimates so a storm of repeats reverts to the truth,
    /// not to an intermediate guess.
    pub(crate) previous: Option<T>,
}

/// What the frame tick does with one worker report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FeedbackAction<T> {
    /// The read-back covers the estimate: show the confirmed value.
    Adopt(T),
    /// The change failed: restore the last confirmed value (which may be
    /// "no row" when nothing was ever read).
    Revert(Option<T>),
    /// The read-back predates the estimate on screen; the worker's next
    /// report resolves it.
    KeepEstimate,
}

/// The one decision rule for a worker report: sequence numbers decide
/// whether the report covers what is on screen, and a covered failure
/// restores the last confirmed value instead of leaving the estimate up.
pub(crate) fn decide_feedback<T: Copy>(
    optimistic: Option<OptimisticValue<T>>,
    outcome_seq: u64,
    result: Option<T>,
) -> FeedbackAction<T> {
    if let Some(estimate) = optimistic
        && estimate.seq > outcome_seq
    {
        return FeedbackAction::KeepEstimate;
    }
    match result {
        Some(value) => FeedbackAction::Adopt(value),
        None => FeedbackAction::Revert(optimistic.and_then(|estimate| estimate.previous)),
    }
}

/// A queued OSD refresh: the confirmed value to put on the card, consumed by
/// the frame tick's panel flush (the poll that resolves feedback has no
/// backend to show it with).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OsdCorrection {
    pub(crate) domain: ControlDomain,
    pub(crate) percent: u8,
    pub(crate) muted: bool,
}

/// The card currently on screen, if one is: the OSD is a single
/// replace-in-place card, so only the latest domain owns it, and a
/// contradicting read-back for the other domain must not yank it.
#[derive(Debug, Clone, Copy)]
struct LastOsd {
    domain: ControlDomain,
    percent: u8,
    muted: bool,
    shown_at: Instant,
}

/// Optimistic control values and OSD bookkeeping, owned by the event thread.
///
/// A key press or slider motion draws the estimate immediately and queues
/// the real change; the frame tick adopts the worker's read-back when it
/// covers the estimate (re-syncing the panel only when the value actually
/// moved) or reverts to the last confirmed value when the change failed.
#[derive(Debug, Default)]
pub struct ControlFeedback {
    volume: Option<OptimisticValue<AudioState>>,
    brightness: Option<OptimisticValue<u8>>,
    mic: Option<OptimisticValue<bool>>,
    /// A press that had nothing to estimate from is owed its first card from
    /// the read-back; the sequence keeps a stale report from paying the debt.
    volume_osd_owed: Option<u64>,
    brightness_osd_owed: Option<u64>,
    mic_osd_owed: Option<u64>,
    last_osd: Option<LastOsd>,
    /// A read-back that contradicts the visible card queues a re-show here.
    pending_osd: Option<OsdCorrection>,
}

impl ControlFeedback {
    /// The estimate on screen, ahead of any confirmed value. Chained presses
    /// estimate from it, so a repeat storm follows its own display.
    pub(crate) fn volume_shown(&self) -> Option<AudioState> {
        self.volume.map(|estimate| estimate.shown)
    }

    /// The brightness counterpart of [`Self::volume_shown`].
    pub(crate) fn brightness_shown(&self) -> Option<u8> {
        self.brightness.map(|estimate| estimate.shown)
    }

    /// The microphone counterpart of [`Self::volume_shown`].
    pub(crate) fn mic_shown(&self) -> Option<bool> {
        self.mic.map(|estimate| estimate.shown)
    }

    /// Record an estimate just drawn. A chained estimate keeps the original
    /// `previous`, so reverting a storm of repeats restores the last
    /// confirmed value, not an intermediate guess.
    pub(crate) fn note_volume_estimate(
        &mut self,
        seq: u64,
        shown: AudioState,
        confirmed: Option<AudioState>,
    ) {
        match &mut self.volume {
            Some(estimate) => {
                estimate.seq = seq;
                estimate.shown = shown;
            }
            None => {
                self.volume = Some(OptimisticValue {
                    seq,
                    shown,
                    previous: confirmed,
                });
            }
        }
    }

    /// The brightness counterpart of [`Self::note_volume_estimate`].
    pub(crate) fn note_brightness_estimate(&mut self, seq: u64, shown: u8, confirmed: Option<u8>) {
        match &mut self.brightness {
            Some(estimate) => {
                estimate.seq = seq;
                estimate.shown = shown;
            }
            None => {
                self.brightness = Some(OptimisticValue {
                    seq,
                    shown,
                    previous: confirmed,
                });
            }
        }
    }

    /// The microphone counterpart of [`Self::note_volume_estimate`].
    pub(crate) fn note_mic_estimate(&mut self, seq: u64, shown: bool, confirmed: Option<bool>) {
        match &mut self.mic {
            Some(estimate) => {
                estimate.seq = seq;
                estimate.shown = shown;
            }
            None => {
                self.mic = Some(OptimisticValue {
                    seq,
                    shown,
                    previous: confirmed,
                });
            }
        }
    }

    /// The card a key press just drew, so a contradicting read-back can
    /// refresh it in place.
    pub(crate) fn note_osd_shown(
        &mut self,
        domain: ControlDomain,
        percent: u8,
        muted: bool,
        shown_at: Instant,
    ) {
        self.last_osd = Some(LastOsd {
            domain,
            percent,
            muted,
            shown_at,
        });
    }

    /// A press that had nothing to estimate from owes its first card to the
    /// read-back covering this submission.
    pub(crate) fn owe_osd(&mut self, domain: ControlDomain, seq: u64) {
        match domain {
            ControlDomain::Volume => self.volume_osd_owed = Some(seq),
            ControlDomain::Brightness => self.brightness_osd_owed = Some(seq),
            ControlDomain::MicMute => self.mic_osd_owed = Some(seq),
            // A device switch owes no card: its feedback is the picker's
            // re-read rows, not the OSD.
            ControlDomain::AudioDevice => {}
        }
    }

    /// The queued OSD refresh, if any.
    pub(crate) fn take_pending_osd(&mut self) -> Option<OsdCorrection> {
        self.pending_osd.take()
    }

    /// Resolve a volume report against the estimate on screen, clearing the
    /// estimate when the report covers it and queueing any OSD correction.
    pub(crate) fn resolve_volume(
        &mut self,
        report: VolumeReport,
        now: Instant,
    ) -> FeedbackAction<AudioState> {
        let (seq, result) = report.split();
        let action = decide_feedback(self.volume, seq, result);
        if matches!(action, FeedbackAction::KeepEstimate) {
            return action;
        }
        self.volume = None;
        let value = match action {
            FeedbackAction::Adopt(state) => Some((state.percent, state.muted)),
            FeedbackAction::Revert(previous) => previous.map(|state| (state.percent, state.muted)),
            FeedbackAction::KeepEstimate => None,
        };
        self.resolve_osd(ControlDomain::Volume, seq, value, now);
        action
    }

    /// The brightness counterpart of [`Self::resolve_volume`].
    pub(crate) fn resolve_brightness(
        &mut self,
        report: BrightnessReport,
        now: Instant,
    ) -> FeedbackAction<u8> {
        let (seq, result) = report.split();
        let action = decide_feedback(self.brightness, seq, result);
        if matches!(action, FeedbackAction::KeepEstimate) {
            return action;
        }
        self.brightness = None;
        let value = match action {
            FeedbackAction::Adopt(percent) => Some((percent, false)),
            FeedbackAction::Revert(previous) => previous.map(|percent| (percent, false)),
            FeedbackAction::KeepEstimate => None,
        };
        self.resolve_osd(ControlDomain::Brightness, seq, value, now);
        action
    }

    /// The microphone counterpart of [`Self::resolve_volume`]. The card
    /// value pair carries a 0 percent: the mic card draws no bar, only the
    /// mute flag.
    pub(crate) fn resolve_mic(&mut self, report: MicReport, now: Instant) -> FeedbackAction<bool> {
        let (seq, result) = report.split();
        let action = decide_feedback(self.mic, seq, result);
        if matches!(action, FeedbackAction::KeepEstimate) {
            return action;
        }
        self.mic = None;
        let value = match action {
            FeedbackAction::Adopt(muted) => Some((0, muted)),
            FeedbackAction::Revert(previous) => previous.map(|muted| (0, muted)),
            FeedbackAction::KeepEstimate => None,
        };
        self.resolve_osd(ControlDomain::MicMute, seq, value, now);
        action
    }

    /// Queue an OSD refresh when the report calls for one: an owed first
    /// card, or a live card whose value the read-back just contradicted.
    fn resolve_osd(
        &mut self,
        domain: ControlDomain,
        seq: u64,
        value: Option<(u8, bool)>,
        now: Instant,
    ) {
        // The debt is paid only by a report that covers the owed submission,
        // and a failed change pays nothing — the binding's old error path
        // showed no card either.
        let owed = match domain {
            ControlDomain::Volume => &mut self.volume_osd_owed,
            ControlDomain::Brightness => &mut self.brightness_osd_owed,
            ControlDomain::MicMute => &mut self.mic_osd_owed,
            // Never owed: a device switch's feedback is the picker's re-read
            // rows, and this helper is only ever called for the OSD domains.
            ControlDomain::AudioDevice => return,
        };
        if owed.is_some_and(|owed_seq| owed_seq <= seq) {
            *owed = None;
            if let Some((percent, muted)) = value {
                self.note_osd_shown(domain, percent, muted, now);
                self.pending_osd = Some(OsdCorrection {
                    domain,
                    percent,
                    muted,
                });
            }
            return;
        }
        // A live card the read-back contradicted is re-shown in place. Past
        // the envelope the card is gone, and showing now would pop a new one.
        let (Some((percent, muted)), Some(last)) = (value, self.last_osd) else {
            return;
        };
        if last.domain == domain
            && (last.percent, last.muted) != (percent, muted)
            && now.saturating_duration_since(last.shown_at)
                <= crate::backend::compositor_common::osd::OSD_VISIBLE_WINDOW
        {
            self.note_osd_shown(domain, percent, muted, now);
            self.pending_osd = Some(OsdCorrection {
                domain,
                percent,
                muted,
            });
        }
    }
}

#[must_use]
pub fn audio_inventory_json(inventory: &AudioInventory) -> serde_json::Value {
    let list = |devices: &[AudioDevice]| {
        devices
            .iter()
            .map(|device| {
                serde_json::json!({
                    "id": device.id,
                    "description": device.description,
                    "default": device.is_default,
                })
            })
            .collect::<Vec<_>>()
    };
    serde_json::json!({
        "output": list(&inventory.output),
        "input": list(&inventory.input),
    })
}

/// The `get_audio_devices` answer: the cached inventory plus the flag that
/// tells an empty answer apart from an unread one.
///
/// Two empty lists are ambiguous on their own — this session may have no
/// switchable audio at all (no wpctl, no pactl), or the control-center
/// worker may simply not have read yet. `pending` is the discriminator, and
/// its evidence is `read_completed` — whether a worker read has ever landed
/// — never the mere existence of a snapshot, which `mutate_control_snapshot`
/// creates the first time a volume key is pressed.
///
/// Devices in hand are an answer whatever the worker has done, exactly as
/// `power_profile_report` treats profiles in hand: a post-switch cache write
/// is as real a read as the worker's own.
#[must_use]
pub fn audio_devices_payload(
    inventory: Option<&AudioInventory>,
    read_completed: bool,
) -> serde_json::Value {
    let empty = AudioInventory::default();
    let inventory = inventory.unwrap_or(&empty);
    let known = !inventory.output.is_empty() || !inventory.input.is_empty();
    let mut payload = audio_inventory_json(inventory);
    payload["pending"] = serde_json::Value::Bool(!known && !read_completed);
    payload
}

impl crate::jwm::Jwm {
    /// Both device lists, with the one in use marked. Bars and scripts use
    /// this to build their own audio menus.
    ///
    /// A read of memory only: the inventory is what the control-center
    /// worker last sampled, never a `wpctl status` forked here. The
    /// `get_audio_devices` arm asks `ensure_control_snapshot_refresh` first,
    /// so the next poll is current, and `pending` says whether an empty
    /// answer is "nothing to switch" or "not read yet".
    pub(crate) fn audio_devices_json(&self) -> serde_json::Value {
        audio_devices_payload(
            self.features
                .control_snapshot
                .as_ref()
                .map(|snapshot| &snapshot.audio_inventory),
            self.features.control_snapshot_refreshed_at.is_some(),
        )
    }

    /// Adopt an inventory a command path already read, so the next
    /// `get_audio_devices` does not answer with the pre-switch default
    /// marker while the worker catches up.
    ///
    /// The epoch bump is what every other user mutation of this snapshot
    /// does (`mutate_control_snapshot`): a worker read that was already in
    /// flight when this landed is discarded rather than rolling the marker
    /// back to what it saw before the switch.
    pub(crate) fn cache_control_audio_inventory(&mut self, inventory: AudioInventory) {
        self.features.control_snapshot_epoch = self.features.control_snapshot_epoch.wrapping_add(1);
        let snapshot = self
            .features
            .control_snapshot
            .get_or_insert_with(Default::default);
        snapshot.audio_defaults = inventory.defaults();
        snapshot.audio_inventory = inventory;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wpctl_output_parses_volume_and_mute() {
        assert_eq!(
            parse_wpctl("Volume: 0.45\n"),
            Some(AudioState {
                percent: 45,
                muted: false
            })
        );
        assert_eq!(
            parse_wpctl("Volume: 1.00 [MUTED]\n"),
            Some(AudioState {
                percent: 100,
                muted: true
            })
        );
        assert_eq!(parse_wpctl("garbage"), None);
    }

    #[test]
    fn pactl_output_parses_volume_and_mute() {
        let volume =
            "Volume: front-left: 29491 /  45% / -20.83 dB,   front-right: 29491 /  45% / -20.83 dB";
        assert_eq!(parse_pactl_volume(volume), Some(45));
        assert_eq!(parse_pactl_mute("Mute: yes\n"), Some(true));
        assert_eq!(parse_pactl_mute("Mute: no\n"), Some(false));
        assert_eq!(parse_pactl_mute("nonsense"), None);
    }

    #[test]
    fn amixer_output_parses_volume_and_mute() {
        let on = "  Front Left: Playback 29491 [45%] [-20.83dB] [on]";
        let off = "  Front Left: Playback 0 [0%] [-90.00dB] [off]";
        assert_eq!(
            parse_amixer(on),
            Some(AudioState {
                percent: 45,
                muted: false
            })
        );
        assert_eq!(
            parse_amixer(off),
            Some(AudioState {
                percent: 0,
                muted: true
            })
        );
    }

    const WPCTL_STATUS: &str = "\
PipeWire 'pipewire-0' [1.0.5, ubuntu@host, cookie:1234]
 └─ Clients:
        32. WirePlumber                         [pid:900]

Audio
 ├─ Devices:
 │      46. Built-in Audio                      [alsa]
 │
 ├─ Sinks:
 │  *   49. Built-in Audio Analog Stereo        [vol: 0.45]
 │      52. GA104 High Definition Audio         [vol: 1.00]
 │
 ├─ Sources:
 │  *   50. Built-in Audio Analog Stereo        [vol: 1.00]
 │      51. Yeti Stereo Microphone              [vol: 0.80]
 │
 ├─ Filters:
 │
 └─ Streams:

Video
 ├─ Devices:
 │      47. Integrated Camera                   [v4l2]
 │
 └─ Sources:
     *  48. Integrated Camera                   [v4l2]

Settings
 └─ Default Configured Devices:
         0. Audio/Sink    alsa_output.pci-0000_00_1f.3.analog-stereo
";

    #[test]
    fn wpctl_status_lists_sinks_with_the_default_marked() {
        let sinks = parse_wpctl_devices(WPCTL_STATUS, AudioDirection::Output);
        assert_eq!(sinks.len(), 2);
        assert_eq!(sinks[0].id, "49");
        assert_eq!(sinks[0].description, "Built-in Audio Analog Stereo");
        assert!(sinks[0].is_default);
        assert_eq!(sinks[1].id, "52");
        assert!(!sinks[1].is_default);
    }

    /// The Video tree has a `Sources:` section of its own, and a camera is
    /// not a microphone.
    #[test]
    fn wpctl_status_never_offers_cameras_as_audio_sources() {
        let sources = parse_wpctl_devices(WPCTL_STATUS, AudioDirection::Input);
        assert_eq!(
            sources
                .iter()
                .map(|device| device.description.as_str())
                .collect::<Vec<_>>(),
            ["Built-in Audio Analog Stereo", "Yeti Stereo Microphone"]
        );
    }

    #[test]
    fn one_wpctl_document_builds_both_defaults() {
        let inventory = parse_wpctl_inventory(WPCTL_STATUS);
        assert_eq!(inventory.output.len(), 2);
        assert_eq!(inventory.input.len(), 2);
        let defaults = inventory.defaults();
        assert_eq!(defaults.output.as_ref().map(|d| d.id.as_str()), Some("49"));
        assert_eq!(defaults.input.as_ref().map(|d| d.id.as_str()), Some("50"));
        assert!(
            inventory
                .input
                .iter()
                .all(|device| !device.description.contains("Camera"))
        );
    }

    #[test]
    fn wpctl_status_without_an_audio_tree_lists_nothing() {
        assert!(
            parse_wpctl_devices("PipeWire 'pipewire-0' [1.0.5]\n", AudioDirection::Output)
                .is_empty()
        );
    }

    #[test]
    fn pactl_listing_pairs_names_with_descriptions() {
        let listing = "\
Sink #49
\tState: RUNNING
\tName: alsa_output.pci-0000_00_1f.3.analog-stereo
\tDescription: Built-in Audio Analog Stereo
\tDriver: PipeWire
Sink #52
\tState: SUSPENDED
\tName: alsa_output.pci-0000_01_00.1.hdmi-stereo
\tDescription: GA104 High Definition Audio
";
        let devices = parse_pactl_devices(listing, "alsa_output.pci-0000_01_00.1.hdmi-stereo");
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].description, "Built-in Audio Analog Stereo");
        assert!(!devices[0].is_default);
        assert!(devices[1].is_default);
    }

    /// A sink's monitor is a legitimate PulseAudio source, but offering it in
    /// a microphone picker would hand the user their own output back.
    #[test]
    fn pactl_listing_drops_monitor_sources() {
        let listing = "\
Source #50
\tName: alsa_output.pci-0000_00_1f.3.analog-stereo.monitor
\tDescription: Monitor of Built-in Audio
Source #51
\tName: alsa_input.usb-Blue_Yeti.analog-stereo
\tDescription: Yeti Stereo Microphone
";
        let devices = parse_pactl_devices(listing, "");
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].description, "Yeti Stereo Microphone");
    }

    #[test]
    fn short_listings_yield_stream_indices() {
        let output = "\
5\talsa_output.pci.analog-stereo\tPipeWire\ts16le 2ch 48000Hz\tRUNNING
7\talsa_output.pci.analog-stereo\tPipeWire\tfloat32le 2ch 48000Hz\tRUNNING
";
        assert_eq!(parse_short_indices(output), ["5", "7"]);
        assert!(parse_short_indices("No streams available.\n").is_empty());
    }

    #[test]
    fn rows_mark_the_device_in_use() {
        let default = AudioDevice {
            id: "49".to_string(),
            description: "Built-in Audio".to_string(),
            is_default: true,
        };
        let other = AudioDevice {
            is_default: false,
            ..default.clone()
        };
        assert!(device_row(&default).starts_with('\u{f192}'));
        assert!(device_row(&other).starts_with('\u{f10c}'));
        assert!(device_row(&default).ends_with("Built-in Audio"));
        assert!(device_control_row(AudioDirection::Output, Some(&default)).contains("Built-in"));
        assert!(device_control_row(AudioDirection::Input, None).ends_with("none"));
    }

    #[test]
    fn control_center_snapshot_refresh_uses_a_monotonic_ttl() {
        let now = Instant::now();
        assert!(control_center_snapshot_is_stale(None, now));
        assert!(!control_center_snapshot_is_stale(Some(now), now));
        let almost = now
            .checked_sub(CONTROL_CENTER_SNAPSHOT_TTL - Duration::from_nanos(1))
            .unwrap();
        assert!(!control_center_snapshot_is_stale(Some(almost), now,));
        let expired = now.checked_sub(CONTROL_CENTER_SNAPSHOT_TTL).unwrap();
        assert!(control_center_snapshot_is_stale(Some(expired), now,));
        assert!(!control_center_snapshot_is_stale(
            Some(now + Duration::from_secs(1)),
            now,
        ));
    }

    #[test]
    fn brightnessctl_machine_output_parses_percent() {
        assert_eq!(
            parse_brightnessctl("intel_backlight,backlight,4800,50%,9600\n"),
            Some(50)
        );
        assert_eq!(parse_brightnessctl("no percent here"), None);
    }

    #[test]
    fn sysfs_backlight_probes_are_bounded_deterministic_and_overflow_safe() {
        let root = std::env::temp_dir().join(format!(
            "jwm-backlight-bound-{}-{:016x}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let first = root.join("a-panel");
        let second = root.join("z-panel");
        std::fs::create_dir_all(&first).expect("create first backlight");
        std::fs::create_dir_all(&second).expect("create second backlight");
        assert_eq!(sysfs_backlight_in(&root).as_deref(), Some(first.as_path()));

        std::fs::write(first.join("brightness"), u32::MAX.to_string()).expect("write brightness");
        std::fs::write(first.join("max_brightness"), u32::MAX.to_string())
            .expect("write max brightness");
        assert_eq!(sysfs_brightness_percent_in(&first), Some(100));
        assert_eq!(sysfs_set_percent_in(&first, 50), Some(50));
        assert_eq!(
            std::fs::read_to_string(first.join("brightness")).unwrap(),
            "2147483648"
        );

        std::fs::write(first.join("max_brightness"), "0").expect("write invalid maximum");
        assert_eq!(sysfs_brightness_percent_in(&first), None);
        assert_eq!(sysfs_set_percent_in(&first, 50), None);

        std::fs::remove_dir_all(root).expect("remove temporary backlights");
    }

    #[test]
    fn an_led_is_never_mistaken_for_a_backlight() {
        // A desktop with no panel: `brightnessctl -m` lists a network card's
        // status light first. Reading it as the screen's brightness made the
        // OSD report a level it could not change, and the brightness keys
        // blink the card instead of dimming anything.
        assert_eq!(parse_brightnessctl("igc-08400-led1,leds,1,100%,1\n"), None);
        assert_eq!(
            parse_brightnessctl("input3::capslock,leds,0,0%,1\nigc-08400-led1,leds,1,100%,1\n"),
            None
        );
        // A panel further down the list is still found.
        assert_eq!(
            parse_brightnessctl(
                "input3::capslock,leds,0,0%,1\nintel_backlight,backlight,2400,25%,9600\n"
            ),
            Some(25)
        );
    }

    /// An empty `get_audio_devices` answer is two different sessions: one
    /// with no switchable audio at all, and one whose worker has not read
    /// yet. `pending` is the only thing that tells them apart, and its
    /// evidence must be a completed read, never a snapshot object that a
    /// volume key created.
    #[test]
    fn an_empty_audio_answer_says_whether_anybody_has_looked() {
        let cold = audio_devices_payload(None, false);
        assert_eq!(cold["output"], serde_json::json!([]));
        assert_eq!(cold["input"], serde_json::json!([]));
        assert_eq!(
            cold["pending"],
            serde_json::json!(true),
            "an empty answer before the first read must not read as 'no audio control'"
        );

        // A volume key inserts a default snapshot long before any worker read
        // lands, so an all-default inventory is not evidence of a read.
        let nudged = audio_devices_payload(Some(&AudioInventory::default()), false);
        assert_eq!(nudged["pending"], serde_json::json!(true));

        let read_but_empty = audio_devices_payload(Some(&AudioInventory::default()), true);
        assert_eq!(
            read_but_empty["pending"],
            serde_json::json!(false),
            "a completed read with no devices is the real answer"
        );

        let inventory = AudioInventory {
            output: vec![AudioDevice {
                id: "49".to_string(),
                description: "Speakers".to_string(),
                is_default: true,
            }],
            input: Vec::new(),
        };
        let switched = audio_devices_payload(Some(&inventory), false);
        assert_eq!(
            switched["pending"],
            serde_json::json!(false),
            "devices in hand are an answer even before a worker read lands"
        );
        assert_eq!(switched["output"][0]["id"], "49");
        assert_eq!(switched["output"][0]["default"], serde_json::json!(true));
        assert_eq!(
            inventory.defaults().name(AudioDirection::Output),
            Some("Speakers")
        );
    }

    /// `get_audio_devices` is polled by bars. The answer has to come out of
    /// the control-center snapshot; a `wpctl status` forked here would stall
    /// the frame on every poll. The needle is assembled at runtime and the
    /// haystack is the accessor alone, so this cannot match its own text.
    #[test]
    fn audio_devices_json_never_forks_the_audio_tool_on_the_compositor_thread() {
        const SOURCE: &str = include_str!("system_controls.rs");
        let body = SOURCE
            .split_once("fn audio_devices_json")
            .expect("audio_devices_json")
            .1
            .split_once("\n    }\n")
            .expect("the end of audio_devices_json")
            .0;
        assert!(
            body.contains("control_snapshot"),
            "audio_devices_json no longer serves the control-center snapshot"
        );
        let needle = format!("{}()", "audio_inventory");
        assert!(
            !body.contains(&needle),
            "audio_devices_json regained an inline inventory read"
        );
    }

    /// One worker pass must fork the audio tool once: the `AudioDefaults`
    /// reader is itself an inventory read plus `defaults()`, so asking for
    /// both separately would pay for two `wpctl status` runs and could return
    /// two views of a topology that changed in between. Needle built at
    /// runtime; haystack is the reader alone.
    #[test]
    fn the_control_snapshot_reads_the_audio_topology_once() {
        const SOURCE: &str = include_str!("system_controls.rs");
        let body = SOURCE
            .split_once("impl ControlCenterSnapshot")
            .expect("the snapshot reader")
            .1
            .split_once("\n}\n")
            .expect("the end of the impl block")
            .0;
        let separate = format!("AudioDefaults::{}()", "read");
        assert!(
            !body.contains(&separate),
            "ControlCenterSnapshot::read regained a second audio-tool fork"
        );
        assert!(
            body.contains("audio_inventory.defaults()"),
            "the snapshot's defaults must be derived from the inventory it read"
        );
    }

    // ------------------------------------------------------------------
    // Controls worker: queue folding, estimates, feedback decisions
    // ------------------------------------------------------------------

    fn queued(seq: u64, request: ControlRequest) -> QueuedRequest {
        QueuedRequest { seq, request }
    }

    fn fold_all(
        requests: impl IntoIterator<Item = QueuedRequest>,
    ) -> (Vec<QueuedRequest>, CancelledToggles) {
        let mut batch = Vec::new();
        let mut cancelled = CancelledToggles::default();
        for request in requests {
            fold_request(&mut batch, &mut cancelled, request);
        }
        (batch, cancelled)
    }

    #[test]
    fn queued_adjusts_sum_into_one_write() {
        // A key-repeat storm applies the same total as pressing each key
        // after the worker caught up — but as one write, not a backlog.
        let (batch, cancelled) = fold_all([
            queued(1, ControlRequest::VolumeAdjust(5)),
            queued(2, ControlRequest::VolumeAdjust(5)),
            queued(3, ControlRequest::VolumeAdjust(-12)),
        ]);
        assert_eq!(batch, [queued(3, ControlRequest::VolumeAdjust(-2))]);
        assert_eq!(cancelled, CancelledToggles::default());

        // The sum cannot overflow no matter how long the worker is busy.
        let (batch, _) = fold_all([
            queued(1, ControlRequest::VolumeAdjust(i32::MAX)),
            queued(2, ControlRequest::VolumeAdjust(i32::MAX)),
        ]);
        assert_eq!(batch, [queued(2, ControlRequest::VolumeAdjust(i32::MAX))]);
    }

    #[test]
    fn a_queued_set_makes_an_earlier_level_moot() {
        // A slider drag is a stream of absolute levels; only the newest is
        // ever applied.
        let (batch, _) = fold_all([
            queued(1, ControlRequest::VolumeAdjust(5)),
            queued(2, ControlRequest::VolumeSet(40)),
            queued(3, ControlRequest::VolumeSet(60)),
        ]);
        assert_eq!(batch, [queued(3, ControlRequest::VolumeSet(60))]);
    }

    #[test]
    fn an_adjust_after_a_queued_set_retargets_the_set() {
        let (batch, _) = fold_all([
            queued(1, ControlRequest::VolumeSet(40)),
            queued(2, ControlRequest::VolumeAdjust(5)),
        ]);
        assert_eq!(batch, [queued(2, ControlRequest::VolumeSet(45))]);

        // …within the domain's bounds: volume at 0..=100, brightness at
        // 1..=100 (both brightness backends floor a decrease at nonzero).
        let (batch, _) = fold_all([
            queued(1, ControlRequest::VolumeSet(98)),
            queued(2, ControlRequest::VolumeAdjust(5)),
        ]);
        assert_eq!(batch, [queued(2, ControlRequest::VolumeSet(100))]);
        let (batch, _) = fold_all([
            queued(1, ControlRequest::VolumeSet(2)),
            queued(2, ControlRequest::VolumeAdjust(-5)),
        ]);
        assert_eq!(batch, [queued(2, ControlRequest::VolumeSet(0))]);
        let (batch, _) = fold_all([
            queued(1, ControlRequest::BrightnessSet(2)),
            queued(2, ControlRequest::BrightnessAdjust(-5)),
        ]);
        assert_eq!(batch, [queued(2, ControlRequest::BrightnessSet(1))]);
    }

    #[test]
    fn a_mute_toggle_between_levels_is_never_folded_away() {
        // The toggle is an event: the set-on-muted unmute chain depends on
        // whether it ran, so the levels on either side must not merge across
        // it.
        let (batch, cancelled) = fold_all([
            queued(1, ControlRequest::VolumeSet(30)),
            queued(2, ControlRequest::VolumeToggleMute),
            queued(3, ControlRequest::VolumeSet(60)),
        ]);
        assert_eq!(
            batch,
            [
                queued(1, ControlRequest::VolumeSet(30)),
                queued(2, ControlRequest::VolumeToggleMute),
                queued(3, ControlRequest::VolumeSet(60)),
            ]
        );
        assert_eq!(cancelled, CancelledToggles::default());

        let (batch, _) = fold_all([
            queued(1, ControlRequest::VolumeAdjust(5)),
            queued(2, ControlRequest::VolumeToggleMute),
            queued(3, ControlRequest::VolumeAdjust(5)),
        ]);
        assert_eq!(
            batch,
            [
                queued(1, ControlRequest::VolumeAdjust(5)),
                queued(2, ControlRequest::VolumeToggleMute),
                queued(3, ControlRequest::VolumeAdjust(5)),
            ]
        );
    }

    #[test]
    fn adjacent_mute_toggles_cancel_in_pairs() {
        // Two flips with nothing between are no flip; the newest cancelled
        // sequence still earns a read-back so the estimate can resolve.
        let (batch, cancelled) = fold_all([
            queued(1, ControlRequest::VolumeToggleMute),
            queued(2, ControlRequest::VolumeToggleMute),
        ]);
        assert!(batch.is_empty());
        assert_eq!(cancelled.volume, Some(2));

        // …and once the pair is gone, the levels around it merge — the end
        // state of set-flip-flip-set is the one set.
        let (batch, cancelled) = fold_all([
            queued(1, ControlRequest::VolumeSet(30)),
            queued(2, ControlRequest::VolumeToggleMute),
            queued(3, ControlRequest::VolumeToggleMute),
            queued(4, ControlRequest::VolumeSet(60)),
        ]);
        assert_eq!(batch, [queued(4, ControlRequest::VolumeSet(60))]);
        assert_eq!(cancelled.volume, Some(3));

        // An odd run of toggles keeps exactly one — the parity is the event.
        let (batch, cancelled) = fold_all([
            queued(1, ControlRequest::VolumeToggleMute),
            queued(2, ControlRequest::VolumeToggleMute),
            queued(3, ControlRequest::VolumeToggleMute),
        ]);
        assert_eq!(batch, [queued(3, ControlRequest::VolumeToggleMute)]);
        assert_eq!(cancelled.volume, Some(2));
    }

    #[test]
    fn queued_mic_sets_fold_to_the_newest() {
        // Like the level sets: two queued mic targets are one write, the
        // newest.
        let (batch, cancelled) = fold_all([
            queued(1, ControlRequest::MicMuteSet(true)),
            queued(2, ControlRequest::MicMuteSet(false)),
        ]);
        assert_eq!(batch, [queued(2, ControlRequest::MicMuteSet(false))]);
        assert_eq!(cancelled, CancelledToggles::default());
    }

    #[test]
    fn a_mic_toggle_between_sets_is_never_folded_away() {
        // The microphone half of the volume rule: the toggle is an event,
        // and the sets on either side must not merge across it.
        let (batch, cancelled) = fold_all([
            queued(1, ControlRequest::MicMuteSet(true)),
            queued(2, ControlRequest::MicMuteToggle),
            queued(3, ControlRequest::MicMuteSet(false)),
        ]);
        assert_eq!(
            batch,
            [
                queued(1, ControlRequest::MicMuteSet(true)),
                queued(2, ControlRequest::MicMuteToggle),
                queued(3, ControlRequest::MicMuteSet(false)),
            ]
        );
        assert_eq!(cancelled, CancelledToggles::default());
    }

    #[test]
    fn adjacent_mic_toggles_cancel_in_pairs() {
        // Two mic flips with nothing between are no flip; the newest
        // cancelled sequence still earns a read-back so the estimate can
        // resolve.
        let (batch, cancelled) = fold_all([
            queued(1, ControlRequest::MicMuteToggle),
            queued(2, ControlRequest::MicMuteToggle),
        ]);
        assert!(batch.is_empty());
        assert_eq!(cancelled.mic, Some(2));

        // Once the pair is gone the sets around it merge, exactly like the
        // volume fold.
        let (batch, cancelled) = fold_all([
            queued(1, ControlRequest::MicMuteSet(true)),
            queued(2, ControlRequest::MicMuteToggle),
            queued(3, ControlRequest::MicMuteToggle),
            queued(4, ControlRequest::MicMuteSet(false)),
        ]);
        assert_eq!(batch, [queued(4, ControlRequest::MicMuteSet(false))]);
        assert_eq!(cancelled.mic, Some(3));

        // An odd run keeps exactly one — the parity is the event.
        let (batch, cancelled) = fold_all([
            queued(1, ControlRequest::MicMuteToggle),
            queued(2, ControlRequest::MicMuteToggle),
            queued(3, ControlRequest::MicMuteToggle),
        ]);
        assert_eq!(batch, [queued(3, ControlRequest::MicMuteToggle)]);
        assert_eq!(cancelled.mic, Some(2));
    }

    #[test]
    fn mic_and_volume_toggles_cancel_only_within_their_own_domain() {
        // A sink flip and a mic flip are different events: adjacent across
        // domains they must both run, in the order asked.
        let (batch, cancelled) = fold_all([
            queued(1, ControlRequest::VolumeToggleMute),
            queued(2, ControlRequest::MicMuteToggle),
        ]);
        assert_eq!(
            batch,
            [
                queued(1, ControlRequest::VolumeToggleMute),
                queued(2, ControlRequest::MicMuteToggle),
            ]
        );
        assert_eq!(cancelled, CancelledToggles::default());

        // …and each domain's pair still cancels independently in one drain,
        // so each earns its own covering read-back.
        let (batch, cancelled) = fold_all([
            queued(1, ControlRequest::VolumeToggleMute),
            queued(2, ControlRequest::MicMuteToggle),
            queued(3, ControlRequest::VolumeToggleMute),
            queued(4, ControlRequest::MicMuteToggle),
        ]);
        assert!(batch.is_empty());
        assert_eq!(cancelled.volume, Some(3));
        assert_eq!(cancelled.mic, Some(4));
    }

    #[test]
    fn volume_and_brightness_fold_independently() {
        let (batch, _) = fold_all([
            queued(1, ControlRequest::VolumeAdjust(5)),
            queued(2, ControlRequest::BrightnessAdjust(5)),
            queued(3, ControlRequest::VolumeAdjust(5)),
            queued(4, ControlRequest::BrightnessSet(50)),
        ]);
        assert_eq!(
            batch,
            [
                queued(3, ControlRequest::VolumeAdjust(10)),
                queued(4, ControlRequest::BrightnessSet(50)),
            ]
        );
    }

    fn audio_switch(seq: u64, direction: AudioDirection, id: &str) -> QueuedRequest {
        queued(
            seq,
            ControlRequest::AudioSetDefault {
                direction,
                id: id.to_string(),
            },
        )
    }

    #[test]
    fn queued_audio_switches_fold_to_the_newest() {
        // Two quick Enter presses are one switch: the device asked for last,
        // never both in order.
        let (batch, cancelled) = fold_all([
            audio_switch(1, AudioDirection::Output, "42"),
            audio_switch(2, AudioDirection::Output, "57"),
        ]);
        assert_eq!(batch, [audio_switch(2, AudioDirection::Output, "57")]);
        assert_eq!(cancelled, CancelledToggles::default());
    }

    #[test]
    fn audio_switches_do_not_merge_across_domains() {
        // A device switch and a volume change are different domains: both
        // run, in the order asked.
        let (batch, _) = fold_all([
            audio_switch(1, AudioDirection::Input, "9"),
            queued(2, ControlRequest::VolumeSet(60)),
        ]);
        assert_eq!(
            batch,
            [
                audio_switch(1, AudioDirection::Input, "9"),
                queued(2, ControlRequest::VolumeSet(60)),
            ]
        );
        let (batch, _) = fold_all([
            queued(1, ControlRequest::VolumeSet(60)),
            audio_switch(2, AudioDirection::Input, "9"),
        ]);
        assert_eq!(
            batch,
            [
                queued(1, ControlRequest::VolumeSet(60)),
                audio_switch(2, AudioDirection::Input, "9"),
            ]
        );
    }

    fn audio_report(asked_id: &str, asked_ok: bool, inventory: AudioInventory) -> AudioReport {
        AudioReport {
            seq: 1,
            direction: AudioDirection::Output,
            asked_id: asked_id.to_string(),
            asked_ok,
            inventory,
        }
    }

    fn audio_device(id: &str, description: &str, is_default: bool) -> AudioDevice {
        AudioDevice {
            id: id.to_string(),
            description: description.to_string(),
            is_default,
        }
    }

    #[test]
    fn the_audio_switch_verdict_believes_the_re_read() {
        // Adopt: the asked device reports as default — the marker moves to
        // it and the picker says so.
        let inventory = AudioInventory {
            output: vec![
                audio_device("42", "HDMI Output", true),
                audio_device("57", "Speakers", false),
            ],
            input: Vec::new(),
        };
        let verdict = audio_switch_verdict(&audio_report("42", true, inventory));
        assert!(verdict.took);
        assert_eq!(verdict.in_use.as_deref(), Some("HDMI Output"));
        assert_eq!(verdict.message, "Using HDMI Output");

        // Revert: the server accepted the set and still put the default back
        // — the exit code said yes, the re-read is what counts.
        let inventory = AudioInventory {
            output: vec![
                audio_device("42", "HDMI Output", false),
                audio_device("57", "Speakers", true),
            ],
            input: Vec::new(),
        };
        let verdict = audio_switch_verdict(&audio_report("42", true, inventory));
        assert!(!verdict.took);
        assert_eq!(verdict.in_use.as_deref(), Some("Speakers"));
        assert_eq!(verdict.message, "Unavailable \u{2014} still using Speakers");

        // Nothing reports as default at all: the tool's own answer is all
        // that tells "said yes" from "refused".
        let empty = AudioInventory::default();
        let verdict = audio_switch_verdict(&audio_report("42", true, empty.clone()));
        assert!(!verdict.took);
        assert_eq!(verdict.in_use, None);
        assert_eq!(verdict.message, "Switched, but nothing reports as default");
        let verdict = audio_switch_verdict(&audio_report("42", false, empty));
        assert_eq!(verdict.message, "Could not switch device");

        // The verdict reads the asked direction's list, not the other end's.
        let inventory = AudioInventory {
            output: Vec::new(),
            input: vec![audio_device("9", "Headset Microphone", true)],
        };
        let report = AudioReport {
            direction: AudioDirection::Input,
            ..audio_report("9", true, inventory)
        };
        let verdict = audio_switch_verdict(&report);
        assert!(verdict.took);
        assert_eq!(verdict.message, "Using Headset Microphone");
    }

    #[test]
    fn optimistic_volume_estimates_follow_the_tools_bounds() {
        let base = Some(AudioState {
            percent: 45,
            muted: false,
        });
        // An adjust keeps the mute flag and clamps at the 100 every backend
        // caps an adjust's result at.
        assert_eq!(
            optimistic_volume(base, &ControlRequest::VolumeAdjust(10)),
            Some(AudioState {
                percent: 55,
                muted: false
            })
        );
        assert_eq!(
            optimistic_volume(base, &ControlRequest::VolumeAdjust(900)),
            Some(AudioState {
                percent: 100,
                muted: false
            })
        );
        let quiet = Some(AudioState {
            percent: 5,
            muted: false,
        });
        assert_eq!(
            optimistic_volume(quiet, &ControlRequest::VolumeAdjust(-30)),
            Some(AudioState {
                percent: 0,
                muted: false
            })
        );
        // A level that started above 100 comes back down through the
        // ceiling, like the `-l 1.0` clamp of the result and the read-back
        // dance for the backend without a limit flag.
        let loud = Some(AudioState {
            percent: 120,
            muted: false,
        });
        assert_eq!(
            optimistic_volume(loud, &ControlRequest::VolumeAdjust(-5)),
            Some(AudioState {
                percent: 100,
                muted: false
            })
        );

        // Setting a level unmutes: the worker runs the unmute chain, so the
        // estimate already shows it.
        let muted = Some(AudioState {
            percent: 45,
            muted: true,
        });
        assert_eq!(
            optimistic_volume(muted, &ControlRequest::VolumeSet(60)),
            Some(AudioState {
                percent: 60,
                muted: false
            })
        );
        // A toggle flips the flag and keeps the level.
        assert_eq!(
            optimistic_volume(muted, &ControlRequest::VolumeToggleMute),
            Some(AudioState {
                percent: 45,
                muted: false
            })
        );

        // Nothing ever read: no invented level for relative changes — but an
        // absolute set still knows exactly what to show.
        assert_eq!(
            optimistic_volume(None, &ControlRequest::VolumeAdjust(5)),
            None
        );
        assert_eq!(
            optimistic_volume(None, &ControlRequest::VolumeToggleMute),
            None
        );
        assert_eq!(
            optimistic_volume(None, &ControlRequest::VolumeSet(160)),
            Some(AudioState {
                percent: 100,
                muted: false
            })
        );
    }

    #[test]
    fn optimistic_mic_mute_flips_the_base_and_a_set_needs_none() {
        assert_eq!(
            optimistic_mic_mute(Some(false), &ControlRequest::MicMuteToggle),
            Some(true)
        );
        assert_eq!(
            optimistic_mic_mute(Some(true), &ControlRequest::MicMuteToggle),
            Some(false)
        );
        // Nothing ever read: no invented state for a flip — the read-back
        // owns the first card — but a set knows its target either way.
        assert_eq!(
            optimistic_mic_mute(None, &ControlRequest::MicMuteToggle),
            None
        );
        assert_eq!(
            optimistic_mic_mute(None, &ControlRequest::MicMuteSet(true)),
            Some(true)
        );
        assert_eq!(
            optimistic_mic_mute(Some(true), &ControlRequest::MicMuteSet(false)),
            Some(false)
        );
    }

    #[test]
    fn a_mic_readback_covering_the_estimate_is_adopted_and_a_failure_reverts() {
        let mut feedback = ControlFeedback::default();
        let now = Instant::now();
        feedback.note_mic_estimate(5, true, Some(false));

        // A read-back predating the flip changes nothing on screen — not an
        // adopt, and not a revert either.
        let action = feedback.resolve_mic(MicReport::Applied(4, false), now);
        assert_eq!(action, FeedbackAction::KeepEstimate);
        let action = feedback.resolve_mic(MicReport::Failed(4), now);
        assert_eq!(action, FeedbackAction::KeepEstimate);

        // The covering read-back is adopted…
        let action = feedback.resolve_mic(MicReport::Applied(5, true), now);
        assert_eq!(action, FeedbackAction::Adopt(true));

        // …while a covered failure restores the last confirmed state, not
        // the flip that never landed.
        feedback.note_mic_estimate(6, false, Some(true));
        let action = feedback.resolve_mic(MicReport::Failed(6), now);
        assert_eq!(action, FeedbackAction::Revert(Some(true)));
    }

    #[test]
    fn a_mic_press_with_no_base_is_owed_its_first_card() {
        let mut feedback = ControlFeedback::default();
        let now = Instant::now();
        feedback.owe_osd(ControlDomain::MicMute, 3);

        // A report predating the owed submission pays nothing.
        feedback.resolve_mic(MicReport::Applied(2, true), now);
        assert_eq!(feedback.take_pending_osd(), None);

        // The covering read-back draws the mic card: no bar, just the flag.
        feedback.resolve_mic(MicReport::Applied(3, true), now);
        assert_eq!(
            feedback.take_pending_osd(),
            Some(OsdCorrection {
                domain: ControlDomain::MicMute,
                percent: 0,
                muted: true,
            })
        );

        // …while a failed change pays nothing, matching the binding's old
        // no-card error path.
        feedback.owe_osd(ControlDomain::MicMute, 5);
        let action = feedback.resolve_mic(MicReport::Failed(5), now);
        assert_eq!(action, FeedbackAction::Revert(None));
        assert_eq!(feedback.take_pending_osd(), None);
    }

    #[test]
    fn a_contradicted_live_mic_card_is_refreshed_in_place() {
        let mut feedback = ControlFeedback::default();
        let now = Instant::now();
        feedback.note_osd_shown(ControlDomain::MicMute, 0, true, now);

        // The read-back says the flip never landed: re-show the live card
        // with the true flag.
        feedback.resolve_mic(MicReport::Applied(1, false), now);
        assert_eq!(
            feedback.take_pending_osd(),
            Some(OsdCorrection {
                domain: ControlDomain::MicMute,
                percent: 0,
                muted: false,
            })
        );

        // Another domain's card owns the slot now; a mic read-back must not
        // yank the volume card off the screen.
        feedback.note_osd_shown(ControlDomain::Volume, 60, false, now);
        feedback.resolve_mic(MicReport::Applied(2, true), now);
        assert_eq!(feedback.take_pending_osd(), None);
    }

    #[test]
    fn optimistic_brightness_never_leaves_the_visible_range() {
        assert_eq!(
            optimistic_brightness(Some(45), &ControlRequest::BrightnessAdjust(10)),
            Some(55)
        );
        assert_eq!(
            optimistic_brightness(Some(95), &ControlRequest::BrightnessAdjust(10)),
            Some(100)
        );
        // Both backends floor a decrease at a nonzero level, so the estimate
        // does too.
        assert_eq!(
            optimistic_brightness(Some(5), &ControlRequest::BrightnessAdjust(-30)),
            Some(1)
        );
        assert_eq!(
            optimistic_brightness(None, &ControlRequest::BrightnessAdjust(5)),
            None
        );
        assert_eq!(
            optimistic_brightness(None, &ControlRequest::BrightnessSet(0)),
            Some(1)
        );
    }

    #[test]
    fn a_readback_covering_the_estimate_is_adopted() {
        let estimate = Some(OptimisticValue {
            seq: 5,
            shown: AudioState {
                percent: 60,
                muted: false,
            },
            previous: Some(AudioState {
                percent: 55,
                muted: false,
            }),
        });
        let truth = AudioState {
            percent: 59,
            muted: false,
        };
        assert_eq!(
            decide_feedback(estimate, 5, Some(truth)),
            FeedbackAction::Adopt(truth)
        );
        // A folded batch reports its newest member's sequence, which covers
        // every estimate folded into it.
        assert_eq!(
            decide_feedback(estimate, 7, Some(truth)),
            FeedbackAction::Adopt(truth)
        );
    }

    #[test]
    fn a_readback_older_than_the_estimate_is_ignored() {
        let estimate = Some(OptimisticValue {
            seq: 8,
            shown: AudioState {
                percent: 60,
                muted: false,
            },
            previous: None,
        });
        let stale = AudioState {
            percent: 40,
            muted: false,
        };
        // The estimate came from a submission this read-back does not cover;
        // adopting it would drag the row backwards mid-storm.
        assert_eq!(
            decide_feedback(estimate, 5, Some(stale)),
            FeedbackAction::<AudioState>::KeepEstimate
        );
        // A stale failure must not revert either: the worker is still going
        // to answer for what is on screen.
        assert_eq!(
            decide_feedback(estimate, 5, None),
            FeedbackAction::<AudioState>::KeepEstimate
        );
    }

    #[test]
    fn a_failed_change_reverts_to_the_last_confirmed_value() {
        let confirmed = AudioState {
            percent: 55,
            muted: false,
        };
        let estimate = Some(OptimisticValue {
            seq: 5,
            shown: AudioState {
                percent: 60,
                muted: false,
            },
            previous: Some(confirmed),
        });
        // The estimate may not stick when the real change failed.
        assert_eq!(
            decide_feedback(estimate, 5, None),
            FeedbackAction::Revert(Some(confirmed))
        );
        // Nothing confirmed ever: revert to "no row", the state before the
        // first press.
        let estimate = Some(OptimisticValue {
            seq: 5,
            shown: confirmed,
            previous: None,
        });
        assert_eq!(
            decide_feedback(estimate, 5, None::<AudioState>),
            FeedbackAction::Revert(None)
        );
        // And with no estimate pending at all, failure changes nothing.
        assert_eq!(
            decide_feedback(None, 5, None::<AudioState>),
            FeedbackAction::Revert(None)
        );
    }

    #[test]
    fn chained_estimates_keep_the_first_confirmed_value_for_revert() {
        let mut feedback = ControlFeedback::default();
        let confirmed = AudioState {
            percent: 50,
            muted: false,
        };
        let shown = |percent| AudioState {
            percent,
            muted: false,
        };
        feedback.note_volume_estimate(1, shown(55), Some(confirmed));
        feedback.note_volume_estimate(2, shown(60), Some(confirmed));
        let estimate = feedback.volume.expect("the estimate is pending");
        assert_eq!(estimate.seq, 2);
        assert_eq!(estimate.shown, shown(60));
        // Reverting the storm restores the truth, not the intermediate 55.
        assert_eq!(estimate.previous, Some(confirmed));
    }

    #[test]
    fn a_contradicted_live_card_is_refreshed_and_a_dead_one_is_not() {
        let mut feedback = ControlFeedback::default();
        let now = Instant::now();
        feedback.note_osd_shown(ControlDomain::Volume, 60, false, now);

        // A read-back that agrees with the card changes nothing.
        let action = feedback.resolve_volume(
            VolumeReport::Applied(
                1,
                AudioState {
                    percent: 60,
                    muted: false,
                },
            ),
            now,
        );
        assert_eq!(
            action,
            FeedbackAction::Adopt(AudioState {
                percent: 60,
                muted: false
            })
        );
        assert_eq!(feedback.take_pending_osd(), None);

        // One that contradicts it re-shows in place.
        feedback.resolve_volume(
            VolumeReport::Applied(
                2,
                AudioState {
                    percent: 55,
                    muted: false,
                },
            ),
            now,
        );
        assert_eq!(
            feedback.take_pending_osd(),
            Some(OsdCorrection {
                domain: ControlDomain::Volume,
                percent: 55,
                muted: false,
            })
        );

        // Past the card's envelope the same correction would pop a new card
        // long after the last press — the old one is left to fade.
        feedback.note_osd_shown(ControlDomain::Volume, 60, false, now);
        let late = now
            + crate::backend::compositor_common::osd::OSD_VISIBLE_WINDOW
            + Duration::from_millis(1);
        feedback.resolve_volume(
            VolumeReport::Applied(
                3,
                AudioState {
                    percent: 55,
                    muted: false,
                },
            ),
            late,
        );
        assert_eq!(feedback.take_pending_osd(), None);

        // The other domain's card owns the slot now; a volume read-back must
        // not yank the brightness card off the screen.
        feedback.note_osd_shown(ControlDomain::Brightness, 80, false, now);
        feedback.resolve_volume(
            VolumeReport::Applied(
                4,
                AudioState {
                    percent: 55,
                    muted: false,
                },
            ),
            now,
        );
        assert_eq!(feedback.take_pending_osd(), None);
    }

    #[test]
    fn a_press_with_nothing_to_estimate_from_is_owed_its_first_card() {
        let mut feedback = ControlFeedback::default();
        let now = Instant::now();
        feedback.owe_osd(ControlDomain::Volume, 3);

        // A report predating the owed submission pays nothing.
        feedback.resolve_volume(
            VolumeReport::Applied(
                2,
                AudioState {
                    percent: 40,
                    muted: false,
                },
            ),
            now,
        );
        assert_eq!(feedback.take_pending_osd(), None);

        // The covering read-back draws the card…
        feedback.resolve_volume(
            VolumeReport::Applied(
                3,
                AudioState {
                    percent: 45,
                    muted: false,
                },
            ),
            now,
        );
        assert_eq!(
            feedback.take_pending_osd(),
            Some(OsdCorrection {
                domain: ControlDomain::Volume,
                percent: 45,
                muted: false,
            })
        );

        // …while a failed change pays nothing, matching the binding's old
        // no-card error path.
        feedback.owe_osd(ControlDomain::Volume, 5);
        let action = feedback.resolve_volume(VolumeReport::Failed(5), now);
        assert_eq!(action, FeedbackAction::Revert(None));
        assert_eq!(feedback.take_pending_osd(), None);
    }

    /// The event thread must never call the blocking primitives: every
    /// mutation goes through the queue. The haystack is the worker-side
    /// section alone — the folder, reports, and feedback — so the needles
    /// are built at runtime and this test cannot match its own source.
    #[test]
    fn the_worker_side_has_no_submission_shortcut() {
        const SOURCE: &str = include_str!("system_controls.rs");
        let queue = SOURCE
            .split_once("fn queue_control_request")
            .expect("queue_control_request")
            .1
            .split_once("fn take_control_report")
            .expect("the end of queue_control_request")
            .0;
        for primitive in [
            "volume_adjust",
            "volume_set",
            "volume_toggle_mute",
            "brightness_adjust",
            "brightness_set",
            "mic_set_mute",
            "mic_toggle_mute",
        ] {
            let needle = format!("{primitive}(");
            // The only allowed call shape inside the queue is on
            // `ControlRequest` variants, never the primitives themselves.
            assert!(
                !queue.contains(&needle),
                "queue_control_request runs {needle} inline instead of queueing"
            );
        }
    }
}
