use std::io;
use std::process::{Command, Output};
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
        match self.try_refresh() {
            Ok(changed) => changed,
            Err(error) => {
                warn!("Failed to read brightness: {error}");
                false
            }
        }
    }

    /// Re-read the current brightness, returning a command error to the caller.
    pub fn try_refresh(&mut self) -> io::Result<bool> {
        let percent = read_brightness_percent();
        // Keep failed probes rate-limited just like successful ones.
        self.last_update = Instant::now();
        let percent = percent?;
        let changed = self.percent != percent;
        self.percent = percent;
        Ok(changed)
    }

    /// Refresh only when the cached value is older than the update interval.
    pub fn update_if_needed(&mut self) -> bool {
        match self.try_update_if_needed() {
            Ok(changed) => changed,
            Err(error) => {
                warn!("Failed to read brightness: {error}");
                false
            }
        }
    }

    /// Refresh stale state, returning a command error to the caller.
    pub fn try_update_if_needed(&mut self) -> io::Result<bool> {
        if self.last_update.elapsed() >= self.update_interval {
            self.try_refresh()
        } else {
            Ok(false)
        }
    }

    pub fn percent(&self) -> Option<u8> {
        self.percent
    }

    /// Adjust brightness by a relative percentage (positive raises, negative lowers).
    /// Returns true if the cached value changed afterwards.
    pub fn adjust(&mut self, delta_percent: i32) -> bool {
        match self.try_adjust(delta_percent) {
            Ok(changed) => changed,
            Err(error) => {
                warn!("Failed to adjust brightness: {error}");
                false
            }
        }
    }

    /// Adjust brightness and return any `brightnessctl` command error to the caller.
    pub fn try_adjust(&mut self, delta_percent: i32) -> io::Result<bool> {
        if delta_percent == 0 {
            return Ok(false);
        }
        let arg = if delta_percent > 0 {
            format!("{}%+", delta_percent)
        } else {
            format!("{}%-", delta_percent.unsigned_abs())
        };
        run_brightnessctl(&["set", &arg])?;
        self.try_refresh()
    }

    /// Set brightness to an absolute percentage.
    pub fn set_percent(&mut self, percent: u8) -> bool {
        match self.try_set_percent(percent) {
            Ok(changed) => changed,
            Err(error) => {
                warn!("Failed to set brightness: {error}");
                false
            }
        }
    }

    /// Set brightness and return any `brightnessctl` command error to the caller.
    pub fn try_set_percent(&mut self, percent: u8) -> io::Result<bool> {
        let arg = format!("{}%", percent.min(100));
        run_brightnessctl(&["set", &arg])?;
        self.try_refresh()
    }
}

impl Default for BrightnessManager {
    fn default() -> Self {
        Self::new()
    }
}

fn brightnessctl_output(args: &[&str]) -> io::Result<Output> {
    let output = Command::new("brightnessctl").args(args).output()?;
    if output.status.success() {
        return Ok(output);
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    let detail = if stderr.is_empty() {
        output.status.to_string()
    } else {
        format!("{}: {stderr}", output.status)
    };
    Err(io::Error::other(format!(
        "brightnessctl {args:?} failed ({detail})"
    )))
}

fn run_brightnessctl(args: &[&str]) -> io::Result<()> {
    brightnessctl_output(args).map(|_| ())
}

fn read_brightness_percent() -> io::Result<Option<u8>> {
    let output = brightnessctl_output(&["-m"])?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_brightness_percent(&stdout))
}

fn parse_brightness_percent(output: &str) -> Option<u8> {
    let line = output.lines().next()?;
    let field = line.split(',').nth(3)?;
    field
        .trim()
        .strip_suffix('%')?
        .parse::<u8>()
        .ok()
        .map(|percent| percent.min(100))
}

#[cfg(test)]
mod tests {
    use super::parse_brightness_percent;

    #[test]
    fn parses_machine_readable_output() {
        let output = "intel_backlight,backlight,48000,42%,120000\n";
        assert_eq!(parse_brightness_percent(output), Some(42));
    }

    #[test]
    fn parses_only_the_first_device() {
        let output = concat!(
            "intel_backlight,backlight,48000,40%,120000\n",
            "leds,led,1,100%,1\n",
        );
        assert_eq!(parse_brightness_percent(output), Some(40));
    }

    #[test]
    fn rejects_malformed_machine_readable_output() {
        assert_eq!(parse_brightness_percent(""), None);
        assert_eq!(
            parse_brightness_percent("device,backlight,1,not-a-percent,2"),
            None
        );
        assert_eq!(parse_brightness_percent("device,backlight,1,42,2"), None);
    }

    #[test]
    fn clamps_out_of_range_machine_readable_output() {
        assert_eq!(
            parse_brightness_percent("device,backlight,1,150%,2"),
            Some(100)
        );
    }
}
