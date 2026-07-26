//! Volume and backlight control for the OSD and control center.
//!
//! Mutations shell out to the session's native tools with a fallback chain —
//! volume: `wpctl` (PipeWire) → `pactl` (PulseAudio) → `amixer` (ALSA);
//! brightness: `brightnessctl` → direct sysfs. The first tool that works is
//! cached for the rest of the session so a key repeat spawns one process, not
//! three. All output parsing lives in pure functions so it stays testable
//! without the tools installed.

use std::process::Command;
use std::sync::OnceLock;

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
    let output = Command::new(cmd).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn run_ok(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd)
        .args(args)
        .output()
        .is_ok_and(|o| o.status.success())
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
fn parse_brightnessctl(output: &str) -> Option<u8> {
    output
        .trim()
        .split(',')
        .filter_map(|field| field.strip_suffix('%'))
        .filter_map(|percent| percent.parse::<u8>().ok())
        .next()
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
// Brightness
// ---------------------------------------------------------------------------

fn sysfs_backlight() -> Option<std::path::PathBuf> {
    std::fs::read_dir("/sys/class/backlight")
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .next()
}

fn sysfs_brightness_percent() -> Option<u8> {
    let dir = sysfs_backlight()?;
    let read_u32 = |name: &str| -> Option<u32> {
        std::fs::read_to_string(dir.join(name))
            .ok()?
            .trim()
            .parse()
            .ok()
    };
    let current = read_u32("brightness")?;
    let max = read_u32("max_brightness")?.max(1);
    Some(((current * 100 + max / 2) / max).min(100) as u8)
}

fn detect_brightness_tool() -> Option<BrightnessTool> {
    *BRIGHTNESS_TOOL.get_or_init(|| {
        if run("brightnessctl", &["-m"])
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
        BrightnessTool::Brightnessctl => parse_brightnessctl(&run("brightnessctl", &["-m"])?),
        BrightnessTool::Sysfs => sysfs_brightness_percent(),
    }
}

fn sysfs_set_percent(percent: u8) -> Option<u8> {
    let dir = sysfs_backlight()?;
    let max: u32 = std::fs::read_to_string(dir.join("max_brightness"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let raw = (u32::from(percent.min(100)) * max + 50) / 100;
    // Direct sysfs writes need udev backlight permissions; failure falls
    // through to None and the OSD simply is not shown.
    std::fs::write(dir.join("brightness"), raw.to_string()).ok()?;
    sysfs_brightness_percent()
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
            if !run_ok("brightnessctl", &["-n1", "set", &step]) {
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
            if !run_ok(
                "brightnessctl",
                &["-n1", "set", &format!("{}%", percent.min(100))],
            ) {
                return None;
            }
            brightness_percent()
        }
        BrightnessTool::Sysfs => sysfs_set_percent(percent.max(1)),
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

    #[test]
    fn brightnessctl_machine_output_parses_percent() {
        assert_eq!(
            parse_brightnessctl("intel_backlight,backlight,4800,50%,9600\n"),
            Some(50)
        );
        assert_eq!(parse_brightnessctl("no percent here"), None);
    }
}
