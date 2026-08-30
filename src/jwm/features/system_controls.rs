//! Volume and backlight control for the OSD and control center.
//!
//! Mutations shell out to the session's native tools with a fallback chain —
//! volume: `wpctl` (PipeWire) → `pactl` (PulseAudio) → `amixer` (ALSA);
//! brightness: `brightnessctl` → direct sysfs. The first tool that works is
//! cached for the rest of the session so a key repeat spawns one process, not
//! three. All output parsing lives in pure functions so it stays testable
//! without the tools installed.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

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
/// returning the resulting state for the OSD.
pub fn volume_adjust(delta: i32) -> Option<AudioState> {
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
pub fn volume_toggle_mute() -> Option<AudioState> {
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

/// Set the default sink to an absolute percent (0..=100). Used by the
/// control-center slider.
pub fn volume_set(percent: u8) -> Option<AudioState> {
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
    pub brightness: Option<u8>,
    pub audio_defaults: AudioDefaults,
    pub power_profiles: Option<(Vec<String>, String)>,
}

impl ControlCenterSnapshot {
    /// Read every slow control domain. This must run on a background worker;
    /// the function is intentionally synchronous so each domain keeps its
    /// established fallback order and error semantics.
    #[must_use]
    pub fn read() -> Self {
        Self {
            volume: volume_state(),
            brightness: brightness_percent(),
            audio_defaults: AudioDefaults::read(),
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
pub fn brightness_adjust(delta: i32) -> Option<u8> {
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

/// Set the backlight to an absolute percent. Used by the control-center slider.
pub fn brightness_set(percent: u8) -> Option<u8> {
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

impl crate::jwm::Jwm {
    /// Both device lists, with the one in use marked. Bars and scripts use
    /// this to build their own audio menus.
    pub(crate) fn audio_devices_json(&self) -> serde_json::Value {
        audio_inventory_json(&audio_inventory())
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
}
