//! Battery status and power profiles for the control center.
//!
//! The battery comes straight from `/sys/class/power_supply`; profiles go
//! through `powerprofilesctl` with the ACPI platform-profile sysfs as the
//! fallback, following the same shape as
//! [`system_controls`](super::system_controls): the working tool is cached
//! for the session and every parser is pure so it can be tested without the
//! hardware or the tools.
//!
//! The low-battery warning lives here too, as a state machine over the
//! percentage rather than a timer, so a laptop that hovers at 19% is warned
//! once instead of every poll.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Warn at these levels, highest first. Crossing one downwards while
/// discharging posts one notification.
const WARN_THRESHOLDS: [u8; 3] = [20, 10, 5];
/// Percentage points a battery must climb back above a threshold before that
/// threshold can warn again, so noise around the line does not repeat.
const REARM_HYSTERESIS: u8 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChargeStatus {
    Charging,
    Discharging,
    Full,
    #[default]
    Unknown,
}

impl ChargeStatus {
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value.trim() {
            "Charging" => Self::Charging,
            "Discharging" => Self::Discharging,
            // "Not charging" means plugged in and holding, e.g. at a charge
            // limit: it is not draining, so it must not trigger warnings.
            "Full" | "Not charging" => Self::Full,
            _ => Self::Unknown,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Charging => "charging",
            Self::Discharging => "discharging",
            Self::Full => "full",
            Self::Unknown => "unknown",
        }
    }

    #[must_use]
    pub fn is_draining(self) -> bool {
        matches!(self, Self::Discharging)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatteryState {
    pub percent: u8,
    pub status: ChargeStatus,
    /// Minutes until empty (discharging) or full (charging), when the
    /// hardware reports a usable rate.
    pub time_remaining_mins: Option<u32>,
}

/// Battery-level glyph, with a distinct one for charging so the row reads at
/// a glance.
#[must_use]
pub fn battery_icon(state: &BatteryState) -> &'static str {
    if matches!(state.status, ChargeStatus::Charging) {
        return "\u{f0e7}"; // fa-bolt
    }
    match state.percent {
        0..=10 => "\u{f244}",  // fa-battery-empty
        11..=35 => "\u{f243}", // fa-battery-quarter
        36..=65 => "\u{f242}", // fa-battery-half
        66..=90 => "\u{f241}", // fa-battery-three-quarters
        _ => "\u{f240}",       // fa-battery-full
    }
}

/// `1h 20m`, `45m`, or empty when the hardware gives no usable rate.
#[must_use]
pub fn format_duration(minutes: u32) -> String {
    let hours = minutes / 60;
    let mins = minutes % 60;
    if hours == 0 {
        return format!("{mins}m");
    }
    format!("{hours}h {mins:02}m")
}

/// The control-center row: icon, level, and what the battery is doing.
#[must_use]
pub fn control_row(state: &BatteryState) -> String {
    let detail = match (state.status, state.time_remaining_mins) {
        (ChargeStatus::Full, _) => "full".to_string(),
        (ChargeStatus::Charging, Some(mins)) => {
            format!("charging \u{2022} {} left", format_duration(mins))
        }
        (ChargeStatus::Charging, None) => "charging".to_string(),
        (ChargeStatus::Discharging, Some(mins)) => format!("{} left", format_duration(mins)),
        (ChargeStatus::Discharging, None) => "on battery".to_string(),
        (ChargeStatus::Unknown, _) => String::new(),
    };
    format!(
        "{}  Battery{:>16}%   {detail}",
        battery_icon(state),
        state.percent
    )
}

/// Minutes to empty or full from the energy/charge counters.
///
/// `now` and `full` are µWh (or µAh) and `rate` is µW (or µA); the units
/// cancel, so the same arithmetic serves both flavors of battery. A zero or
/// missing rate means the hardware is not reporting one — the row then says
/// what the battery is doing without inventing a number.
#[must_use]
pub fn estimate_minutes(
    now: Option<u64>,
    full: Option<u64>,
    rate: Option<u64>,
    status: ChargeStatus,
) -> Option<u32> {
    let rate = rate.filter(|rate| *rate > 0)?;
    let now = now?;
    let remaining = match status {
        ChargeStatus::Discharging => now,
        ChargeStatus::Charging => full?.saturating_sub(now),
        _ => return None,
    };
    if remaining == 0 {
        return None;
    }
    let minutes = remaining.saturating_mul(60) / rate;
    // A battery reporting a wildly small rate can produce a nonsense estimate;
    // anything past a couple of days is not worth showing.
    u32::try_from(minutes)
        .ok()
        .filter(|minutes| *minutes <= 60 * 48)
}

fn read_field(dir: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(dir.join(name))
        .ok()
        .map(|value| value.trim().to_string())
}

fn read_number(dir: &Path, name: &str) -> Option<u64> {
    read_field(dir, name)?.parse().ok()
}

/// Read one `/sys/class/power_supply/*` directory as a battery.
#[must_use]
pub fn read_battery_in(dir: &Path) -> Option<BatteryState> {
    if read_field(dir, "type").as_deref() != Some("Battery") {
        return None;
    }
    let percent = read_number(dir, "capacity")?.min(100) as u8;
    let status = read_field(dir, "status")
        .map(|value| ChargeStatus::parse(&value))
        .unwrap_or_default();
    // Energy-reporting batteries expose energy_*/power_now; charge-reporting
    // ones expose charge_*/current_now. Either pair works.
    let now = read_number(dir, "energy_now").or_else(|| read_number(dir, "charge_now"));
    let full = read_number(dir, "energy_full").or_else(|| read_number(dir, "charge_full"));
    let rate = read_number(dir, "power_now").or_else(|| read_number(dir, "current_now"));
    Some(BatteryState {
        percent,
        status,
        time_remaining_mins: estimate_minutes(now, full, rate, status),
    })
}

/// The system battery, if this machine has one. Multi-battery laptops report
/// the first entry; a combined figure would need per-battery energy weighting
/// that the shell has no use for.
#[must_use]
pub fn read_battery() -> Option<BatteryState> {
    let entries = std::fs::read_dir("/sys/class/power_supply").ok()?;
    let mut dirs: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    dirs.sort();
    dirs.iter().find_map(|dir| read_battery_in(dir))
}

/// Tracks which low-battery warning has already been posted.
#[derive(Debug, Default)]
pub struct LowBatteryWarner {
    warned_at: Option<u8>,
}

impl LowBatteryWarner {
    /// Returns the threshold to warn about, once per downward crossing.
    ///
    /// Charging clears the memory immediately: plugging in and unplugging
    /// again is a new situation the user should hear about.
    pub fn update(&mut self, state: &BatteryState) -> Option<u8> {
        if !state.status.is_draining() {
            self.warned_at = None;
            return None;
        }
        if let Some(warned) = self.warned_at
            && state.percent > warned.saturating_add(REARM_HYSTERESIS)
        {
            self.warned_at = None;
        }
        let crossed = WARN_THRESHOLDS
            .iter()
            .copied()
            .filter(|threshold| state.percent <= *threshold)
            .min()?;
        match self.warned_at {
            Some(warned) if crossed >= warned => None,
            _ => {
                self.warned_at = Some(crossed);
                Some(crossed)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Power profiles
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProfileTool {
    PowerProfilesCtl,
    PlatformProfile,
}

static PROFILE_TOOL: OnceLock<Option<ProfileTool>> = OnceLock::new();

const PLATFORM_PROFILE: &str = "/sys/firmware/acpi/platform_profile";
const PLATFORM_PROFILE_CHOICES: &str = "/sys/firmware/acpi/platform_profile_choices";
const MAX_PROFILE_FILE_BYTES: u64 = 4 * 1024;
const MAX_PROFILE_SOURCE_ITEMS: usize = 64;
const MAX_PROFILES: usize = 16;
const MAX_PROFILE_NAME_BYTES: usize = 64;

fn profile_name(value: &str) -> Option<&str> {
    let name = value.trim();
    (!name.is_empty()
        && name.len() <= MAX_PROFILE_NAME_BYTES
        && !name
            .chars()
            .any(|character| character.is_whitespace() || character.is_control()))
    .then_some(name)
}

fn read_profile_text(path: impl AsRef<Path>) -> Option<String> {
    read_profile_text_bounded(path, MAX_PROFILE_FILE_BYTES)
}

fn read_profile_text_bounded(path: impl AsRef<Path>, max_bytes: u64) -> Option<String> {
    let mut bytes = Vec::with_capacity(256);
    std::fs::File::open(path)
        .ok()?
        .take(max_bytes.checked_add(1)?)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > usize::try_from(max_bytes).ok()? {
        return None;
    }
    String::from_utf8(bytes).ok()
}

fn parse_profile_choices(output: &str) -> Vec<String> {
    let mut profiles: Vec<String> = Vec::new();
    for value in output.split_whitespace().take(MAX_PROFILE_SOURCE_ITEMS) {
        let Some(name) = profile_name(value) else {
            continue;
        };
        if !profiles.iter().any(|profile| profile == name) && profiles.len() < MAX_PROFILES {
            profiles.push(name.to_string());
        }
    }
    profiles
}

fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let output = super::external_command::output(cmd, args).ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Parse `powerprofilesctl list`: each profile is a `name:` line, and the
/// active one is marked with `*`.
#[must_use]
pub fn parse_profile_list(output: &str) -> (Vec<String>, Option<String>) {
    let mut profiles: Vec<String> = Vec::new();
    let mut active = None;
    for line in output.lines().take(MAX_PROFILE_SOURCE_ITEMS) {
        let trimmed = line.trim_start();
        let (marked, rest) = match trimmed.strip_prefix("* ") {
            Some(rest) => (true, rest),
            None => (false, trimmed),
        };
        let Some(name) = rest.strip_suffix(':').and_then(profile_name) else {
            // Indented driver/degraded detail lines.
            continue;
        };
        let exists = profiles.iter().any(|profile| profile == name);
        if !exists && profiles.len() >= MAX_PROFILES {
            continue;
        }
        if !exists {
            profiles.push(name.to_string());
        }
        if marked {
            active = Some(name.to_string());
        }
    }
    (profiles, active)
}

fn detect_tool() -> Option<ProfileTool> {
    if run("powerprofilesctl", &["get"]).is_some() {
        return Some(ProfileTool::PowerProfilesCtl);
    }
    Path::new(PLATFORM_PROFILE_CHOICES)
        .exists()
        .then_some(ProfileTool::PlatformProfile)
}

fn tool() -> Option<ProfileTool> {
    *PROFILE_TOOL.get_or_init(detect_tool)
}

/// Selectable profiles and the current one, or `None` when this machine has
/// no profile control at all.
#[must_use]
pub fn profiles() -> Option<(Vec<String>, String)> {
    match tool()? {
        ProfileTool::PowerProfilesCtl => {
            let (profiles, active) = parse_profile_list(&run("powerprofilesctl", &["list"])?);
            if profiles.is_empty() {
                return None;
            }
            let active = active
                .or_else(|| {
                    run("powerprofilesctl", &["get"])
                        .as_deref()
                        .and_then(profile_name)
                        .map(str::to_string)
                })
                .filter(|name| profiles.contains(name))
                .unwrap_or_else(|| profiles[0].clone());
            Some((profiles, active))
        }
        ProfileTool::PlatformProfile => {
            let choices = parse_profile_choices(&read_profile_text(PLATFORM_PROFILE_CHOICES)?);
            if choices.is_empty() {
                return None;
            }
            let active = read_profile_text(PLATFORM_PROFILE)
                .as_deref()
                .and_then(profile_name)
                .filter(|name| choices.iter().any(|choice| choice == *name))
                .map_or_else(|| choices[0].clone(), str::to_string);
            Some((choices, active))
        }
    }
}

/// Switch profiles. Returns false when the tool refused, so the caller can
/// leave the row showing what is really in effect.
#[must_use]
pub fn set_profile(name: &str) -> bool {
    let Some(name) = profile_name(name) else {
        return false;
    };
    match tool() {
        Some(ProfileTool::PowerProfilesCtl) => run("powerprofilesctl", &["set", name]).is_some(),
        Some(ProfileTool::PlatformProfile) => std::fs::write(PLATFORM_PROFILE, name).is_ok(),
        None => false,
    }
}

/// Next profile in the list, wrapping. Pure so the row's Left/Right behavior
/// is testable without a machine that has profiles.
#[must_use]
pub fn cycle_profile(profiles: &[String], current: &str, delta: isize) -> Option<String> {
    if profiles.is_empty() {
        return None;
    }
    let len = profiles.len();
    let index = profiles
        .iter()
        .position(|profile| profile == current)
        .unwrap_or(0);
    // Index arithmetic stays in `usize`: the list is tiny and a signed round
    // trip would only invite a cast lint.
    let steps = delta.unsigned_abs() % len;
    let next = if delta.is_negative() {
        (index + len - steps) % len
    } else {
        (index + steps) % len
    };
    Some(profiles[next].clone())
}

/// Icon for a profile, matched by name because the set is driver-defined.
#[must_use]
pub fn profile_icon(name: &str) -> &'static str {
    match name {
        "power-saver" | "low-power" | "quiet" => "\u{f06c}", // fa-leaf
        "performance" => "\u{f135}",                         // fa-rocket
        _ => "\u{f24e}",                                     // fa-balance-scale
    }
}

/// The control-center row for the profile selector.
#[must_use]
pub fn profile_row(current: &str) -> String {
    format!("{}  Power Profile{:>21}", profile_icon(current), current)
}

/// How often the battery is re-read. Slow: a percentage point takes minutes
/// to move, and this runs on JWM's backend-neutral maintenance update.
pub const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

impl crate::jwm::Jwm {
    /// Re-read the battery, warn if it crossed a threshold, and keep an open
    /// control center current. Called from the maintenance update behind its own
    /// interval gate.
    pub(crate) fn poll_battery(&mut self, backend: &mut dyn crate::backend::api::Backend) {
        let Some(state) = read_battery() else {
            return;
        };
        let changed = self.features.battery != Some(state);
        self.features.battery = Some(state);

        if let Some(threshold) = self.features.low_battery.update(&state) {
            let body = match state.time_remaining_mins {
                Some(mins) => format!("{} remaining", format_duration(mins)),
                None => "Plug in the charger".to_string(),
            };
            let request = crate::jwm::features::NotificationRequest {
                app: "Power".to_string(),
                summary: format!("Battery low \u{2014} {threshold}%"),
                body,
                // The last warning is the one that must survive DND policy
                // debates in the user's head: make it critical.
                urgency: if threshold <= 5 { 2 } else { 1 },
                replaces_id: 0,
                actions: Vec::new(),
            };
            self.post_notification(backend, &request, 0);
        }

        if changed {
            self.refresh_open_control_center();
            self.broadcast_ipc_event(
                "power/battery",
                serde_json::json!({
                    "percent": state.percent,
                    "status": state.status.as_str(),
                    "time_remaining_mins": state.time_remaining_mins,
                }),
            );
        }
    }

    /// JSON snapshot for the `get_power_status` query.
    pub(crate) fn power_status_json(&self) -> serde_json::Value {
        let battery = match self.features.battery {
            Some(state) => serde_json::json!({
                "present": true,
                "percent": state.percent,
                "status": state.status.as_str(),
                "time_remaining_mins": state.time_remaining_mins,
            }),
            None => serde_json::json!({ "present": false }),
        };
        let profile = match profiles() {
            Some((available, active)) => serde_json::json!({
                "available": available,
                "active": active,
            }),
            None => serde_json::Value::Null,
        };
        serde_json::json!({ "battery": battery, "profile": profile })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn battery(percent: u8, status: ChargeStatus) -> BatteryState {
        BatteryState {
            percent,
            status,
            time_remaining_mins: None,
        }
    }

    #[test]
    fn charge_status_covers_the_sysfs_vocabulary() {
        assert_eq!(ChargeStatus::parse("Charging"), ChargeStatus::Charging);
        assert_eq!(
            ChargeStatus::parse("Discharging"),
            ChargeStatus::Discharging
        );
        assert_eq!(ChargeStatus::parse("Full\n"), ChargeStatus::Full);
        assert_eq!(ChargeStatus::parse("Unknown"), ChargeStatus::Unknown);
    }

    #[test]
    fn not_charging_is_not_draining() {
        // Plugged in at a charge limit: never warn about it.
        let status = ChargeStatus::parse("Not charging");
        assert!(!status.is_draining());
    }

    #[test]
    fn time_estimates_use_the_rate_and_direction() {
        // Half of a 57 Wh battery left, drawing 10 W -> ~2h51m.
        let minutes = estimate_minutes(
            Some(28_500_000),
            Some(57_000_000),
            Some(10_000_000),
            ChargeStatus::Discharging,
        );
        assert_eq!(minutes, Some(171));

        // Charging counts the gap to full instead.
        let minutes = estimate_minutes(
            Some(28_500_000),
            Some(57_000_000),
            Some(10_000_000),
            ChargeStatus::Charging,
        );
        assert_eq!(minutes, Some(171));
    }

    #[test]
    fn a_missing_or_zero_rate_yields_no_estimate() {
        assert_eq!(
            estimate_minutes(Some(1000), Some(2000), Some(0), ChargeStatus::Discharging),
            None
        );
        assert_eq!(
            estimate_minutes(Some(1000), Some(2000), None, ChargeStatus::Discharging),
            None
        );
        // A full battery is not counting down to anything.
        assert_eq!(
            estimate_minutes(Some(1000), Some(1000), Some(10), ChargeStatus::Full),
            None
        );
    }

    #[test]
    fn absurd_estimates_are_dropped_rather_than_shown() {
        // A near-zero draw would claim months of runtime.
        assert_eq!(
            estimate_minutes(
                Some(57_000_000),
                Some(57_000_000),
                Some(1),
                ChargeStatus::Discharging
            ),
            None
        );
    }

    #[test]
    fn durations_read_as_hours_and_minutes() {
        assert_eq!(format_duration(45), "45m");
        assert_eq!(format_duration(60), "1h 00m");
        assert_eq!(format_duration(171), "2h 51m");
    }

    #[test]
    fn the_icon_follows_the_level_and_charging_wins() {
        assert_eq!(
            battery_icon(&battery(100, ChargeStatus::Discharging)),
            "\u{f240}"
        );
        assert_eq!(
            battery_icon(&battery(5, ChargeStatus::Discharging)),
            "\u{f244}"
        );
        assert_eq!(
            battery_icon(&battery(5, ChargeStatus::Charging)),
            "\u{f0e7}"
        );
    }

    #[test]
    fn the_row_says_what_the_battery_is_doing() {
        let mut state = battery(64, ChargeStatus::Discharging);
        state.time_remaining_mins = Some(95);
        let row = control_row(&state);
        assert!(row.contains("64%"));
        assert!(row.contains("1h 35m left"));

        assert!(control_row(&battery(100, ChargeStatus::Full)).contains("full"));
        assert!(control_row(&battery(40, ChargeStatus::Discharging)).contains("on battery"));
    }

    #[test]
    fn a_battery_directory_parses_into_state() {
        let dir = std::env::temp_dir().join(format!("jwm-power-{}-battery", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("type"), "Battery\n").expect("write");
        std::fs::write(dir.join("capacity"), "64\n").expect("write");
        std::fs::write(dir.join("status"), "Discharging\n").expect("write");
        std::fs::write(dir.join("energy_now"), "28500000\n").expect("write");
        std::fs::write(dir.join("energy_full"), "57000000\n").expect("write");
        std::fs::write(dir.join("power_now"), "10000000\n").expect("write");

        let state = read_battery_in(&dir).expect("battery parses");
        assert_eq!(state.percent, 64);
        assert_eq!(state.status, ChargeStatus::Discharging);
        assert_eq!(state.time_remaining_mins, Some(171));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn charge_reporting_batteries_are_read_too() {
        let dir = std::env::temp_dir().join(format!("jwm-power-{}-charge", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("type"), "Battery").expect("write");
        std::fs::write(dir.join("capacity"), "50").expect("write");
        std::fs::write(dir.join("status"), "Discharging").expect("write");
        std::fs::write(dir.join("charge_now"), "2000000").expect("write");
        std::fs::write(dir.join("charge_full"), "4000000").expect("write");
        std::fs::write(dir.join("current_now"), "1000000").expect("write");

        let state = read_battery_in(&dir).expect("battery parses");
        assert_eq!(state.time_remaining_mins, Some(120));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn non_battery_supplies_are_skipped() {
        let dir = std::env::temp_dir().join(format!("jwm-power-{}-mains", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("type"), "Mains").expect("write");
        std::fs::write(dir.join("capacity"), "100").expect("write");

        assert!(read_battery_in(&dir).is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn each_threshold_warns_once_on_the_way_down() {
        let mut warner = LowBatteryWarner::default();
        assert_eq!(warner.update(&battery(50, ChargeStatus::Discharging)), None);
        assert_eq!(
            warner.update(&battery(20, ChargeStatus::Discharging)),
            Some(20)
        );
        // Still under 20: already said so.
        assert_eq!(warner.update(&battery(19, ChargeStatus::Discharging)), None);
        assert_eq!(
            warner.update(&battery(10, ChargeStatus::Discharging)),
            Some(10)
        );
        assert_eq!(warner.update(&battery(7, ChargeStatus::Discharging)), None);
        assert_eq!(
            warner.update(&battery(5, ChargeStatus::Discharging)),
            Some(5)
        );
        assert_eq!(warner.update(&battery(1, ChargeStatus::Discharging)), None);
    }

    #[test]
    fn charging_clears_the_memory_so_the_next_drain_warns_again() {
        let mut warner = LowBatteryWarner::default();
        assert_eq!(
            warner.update(&battery(10, ChargeStatus::Discharging)),
            Some(10)
        );
        assert_eq!(warner.update(&battery(11, ChargeStatus::Charging)), None);
        assert_eq!(
            warner.update(&battery(10, ChargeStatus::Discharging)),
            Some(10)
        );
    }

    #[test]
    fn hovering_at_a_threshold_does_not_repeat() {
        let mut warner = LowBatteryWarner::default();
        assert_eq!(
            warner.update(&battery(20, ChargeStatus::Discharging)),
            Some(20)
        );
        // Within the hysteresis band: no re-arm, no repeat.
        for percent in [21, 22, 24, 25, 19, 20] {
            assert_eq!(
                warner.update(&battery(percent, ChargeStatus::Discharging)),
                None
            );
        }
        // Clearly recovered, then drained again: worth saying once more.
        assert_eq!(warner.update(&battery(40, ChargeStatus::Discharging)), None);
        assert_eq!(
            warner.update(&battery(20, ChargeStatus::Discharging)),
            Some(20)
        );
    }

    #[test]
    fn a_full_battery_never_warns() {
        let mut warner = LowBatteryWarner::default();
        assert_eq!(warner.update(&battery(3, ChargeStatus::Full)), None);
        assert_eq!(warner.update(&battery(3, ChargeStatus::Unknown)), None);
    }

    #[test]
    fn profile_lists_are_parsed_with_the_active_marker() {
        // Real `powerprofilesctl list` output, including a degraded note.
        let output = "  performance:\n    Driver:     intel_pstate\n    Degraded:   yes (high-operating-temperature)\n\n* balanced:\n    Driver:     intel_pstate\n\n  power-saver:\n    Driver:     intel_pstate\n";
        let (profiles, active) = parse_profile_list(output);

        assert_eq!(profiles, ["performance", "balanced", "power-saver"]);
        assert_eq!(active.as_deref(), Some("balanced"));
    }

    #[test]
    fn an_empty_profile_list_parses_to_nothing() {
        let (profiles, active) = parse_profile_list("");
        assert!(profiles.is_empty());
        assert!(active.is_none());
    }

    #[test]
    fn profile_sources_bound_items_names_and_duplicates() {
        let mut output = (0..MAX_PROFILES + 4)
            .map(|index| format!("  profile-{index:02}:\n"))
            .collect::<String>();
        output.push_str("* profile-00:\n");
        output.push_str("  profile-00:\n");
        output.push_str(&format!("  {}:\n", "x".repeat(MAX_PROFILE_NAME_BYTES + 1)));
        output.push_str("  two words:\n");
        let (profiles, active) = parse_profile_list(&output);

        assert_eq!(profiles.len(), MAX_PROFILES);
        assert_eq!(active.as_deref(), Some("profile-00"));
        assert_eq!(
            profiles.iter().filter(|name| *name == "profile-00").count(),
            1
        );

        let choices = (0..MAX_PROFILES + 4)
            .flat_map(|index| [format!("profile-{index:02}"), "profile-00".to_string()])
            .collect::<Vec<_>>()
            .join(" ");
        let choices = parse_profile_choices(&choices);
        assert_eq!(choices.len(), MAX_PROFILES);
        assert_eq!(
            choices.iter().filter(|name| *name == "profile-00").count(),
            1
        );

        let chatter = "not a profile\n".repeat(MAX_PROFILE_SOURCE_ITEMS);
        assert!(
            parse_profile_list(&format!("{chatter}  too-late:\n"))
                .0
                .is_empty()
        );
    }

    #[test]
    fn profile_files_and_write_names_have_hard_bounds() {
        let root = std::env::temp_dir().join(format!(
            "jwm-profile-bound-{}-{:016x}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&root).expect("create temporary directory");
        let exact = root.join("exact");
        std::fs::write(&exact, b"balanced").expect("write exact profile file");
        assert_eq!(
            read_profile_text_bounded(&exact, 8).as_deref(),
            Some("balanced")
        );
        let oversized = root.join("oversized");
        std::fs::write(&oversized, b"balanced!").expect("write oversized profile file");
        assert!(read_profile_text_bounded(&oversized, 8).is_none());
        std::fs::remove_dir_all(root).expect("remove temporary directory");

        assert!(profile_name("balanced").is_some());
        assert!(profile_name("two words").is_none());
        assert!(profile_name("bad\0name").is_none());
        let long = "x".repeat(MAX_PROFILE_NAME_BYTES + 1);
        assert!(profile_name(&long).is_none());
        assert!(!set_profile(&long), "invalid names must not reach a helper");
    }

    #[test]
    fn cycling_wraps_in_both_directions() {
        let profiles: Vec<String> = ["performance", "balanced", "power-saver"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();

        assert_eq!(
            cycle_profile(&profiles, "balanced", 1).as_deref(),
            Some("power-saver")
        );
        assert_eq!(
            cycle_profile(&profiles, "power-saver", 1).as_deref(),
            Some("performance")
        );
        assert_eq!(
            cycle_profile(&profiles, "performance", -1).as_deref(),
            Some("power-saver")
        );
        // An unknown current profile starts from the top rather than failing.
        assert_eq!(
            cycle_profile(&profiles, "nonsense", 1).as_deref(),
            Some("balanced")
        );
        assert_eq!(cycle_profile(&[], "balanced", 1), None);
    }

    #[test]
    fn profile_rows_carry_a_matching_icon() {
        assert!(profile_row("power-saver").contains("\u{f06c}"));
        assert!(profile_row("performance").contains("\u{f135}"));
        assert!(profile_row("balanced").contains("\u{f24e}"));
        assert!(profile_row("balanced").contains("balanced"));
    }
}
