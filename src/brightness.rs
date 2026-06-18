use std::process::Command;
use std::time::{Duration, Instant};

use log::warn;

/// Backlight brightness, read and controlled through the `brightnessctl` CLI.
///
/// Percentage is parsed from `brightnessctl -m` machine-readable output
/// (`<device>,<class>,<current>,<percent>%,<max>`).
pub struct BrightnessManager {
    percent: Option<u8>,
    last_update: Instant,
    update_interval: Duration,
}

impl BrightnessManager {
    pub fn new() -> Self {
        let mut manager = Self {
            percent: None,
            last_update: Instant::now(),
            update_interval: Duration::from_secs(2),
        };
        manager.refresh();
        manager
    }

    /// Re-read the current brightness. Returns true if the value changed.
    pub fn refresh(&mut self) -> bool {
        let prev = self.percent;
        self.percent = read_brightness_percent();
        self.last_update = Instant::now();
        prev != self.percent
    }

    /// Refresh only when the cached value is older than the update interval.
    pub fn update_if_needed(&mut self) -> bool {
        if self.last_update.elapsed() >= self.update_interval {
            self.refresh()
        } else {
            false
        }
    }

    pub fn percent(&self) -> Option<u8> {
        self.percent
    }

    /// Adjust brightness by a relative percentage (positive raises, negative lowers).
    /// Returns true if the cached value changed afterwards.
    pub fn adjust(&mut self, delta_percent: i32) -> bool {
        if delta_percent == 0 {
            return false;
        }
        let arg = if delta_percent > 0 {
            format!("{}%+", delta_percent)
        } else {
            format!("{}%-", -delta_percent)
        };
        run_brightnessctl(&["set", &arg]);
        self.refresh()
    }

    /// Set brightness to an absolute percentage.
    pub fn set_percent(&mut self, percent: u8) -> bool {
        run_brightnessctl(&["set", &format!("{}%", percent.clamp(0, 100))]);
        self.refresh()
    }
}

impl Default for BrightnessManager {
    fn default() -> Self {
        Self::new()
    }
}

fn run_brightnessctl(args: &[&str]) {
    if let Err(e) = Command::new("brightnessctl").args(args).output() {
        warn!("Failed to run brightnessctl {:?}: {}", args, e);
    }
}

fn read_brightness_percent() -> Option<u8> {
    let output = Command::new("brightnessctl").arg("-m").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().next()?;
    let field = line.split(',').nth(3)?;
    field.trim().trim_end_matches('%').parse::<u8>().ok()
}
