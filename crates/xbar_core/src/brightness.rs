use std::io::{self, Read};
use std::os::fd::AsRawFd as _;
use std::os::unix::process::CommandExt as _;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use log::warn;

const BRIGHTNESSCTL_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_BRIGHTNESSCTL_OUTPUT_BYTES: usize = 64 * 1024;
const COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAX_DRAIN_BYTES_PER_POLL: usize = 64 * 1024;

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
        self.try_refresh_with(read_brightness_percent)
    }

    fn try_refresh_with(
        &mut self,
        probe: impl FnOnce() -> io::Result<Option<u8>>,
    ) -> io::Result<bool> {
        let percent = probe();
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
    let mut command = Command::new("brightnessctl");
    command.args(args);
    let output = command_output_bounded(
        &mut command,
        BRIGHTNESSCTL_TIMEOUT,
        MAX_BRIGHTNESSCTL_OUTPUT_BYTES,
    )?;
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

fn command_output_bounded(
    command: &mut Command,
    timeout: Duration,
    output_limit: usize,
) -> io::Result<Output> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Give the helper its own process group so a timed-out wrapper cannot
        // leave a descendant holding either capture pipe open.
        .process_group(0);
    let mut child = command.spawn()?;
    let child_id = child.id();
    let Some(mut stdout) = child.stdout.take() else {
        terminate_child_group(&mut child);
        return Err(io::Error::other(
            "brightness helper stdout was not captured",
        ));
    };
    let Some(mut stderr) = child.stderr.take() else {
        terminate_child_group(&mut child);
        return Err(io::Error::other(
            "brightness helper stderr was not captured",
        ));
    };

    if let Err(error) =
        set_nonblocking(stdout.as_raw_fd()).and_then(|()| set_nonblocking(stderr.as_raw_fd()))
    {
        terminate_child_group(&mut child);
        return Err(error);
    }

    let started = Instant::now();
    let mut stdout_bytes = Vec::with_capacity(output_limit.min(4096));
    let mut stderr_bytes = Vec::with_capacity(output_limit.min(4096));
    let mut stdout_eof = false;
    let mut stderr_eof = false;
    let mut status = None;
    let result = (|| {
        loop {
            if !stdout_eof {
                stdout_eof = drain_available(&mut stdout, &mut stdout_bytes, output_limit)?;
            }
            if !stderr_eof {
                stderr_eof = drain_available(&mut stderr, &mut stderr_bytes, output_limit)?;
            }

            if status.is_none() {
                status = child.try_wait()?;
                if status.is_some() {
                    // A successful wrapper may have left background helpers
                    // holding these pipes. They belong to this synchronous
                    // probe and must not outlive it.
                    kill_process_group(child_id);
                }
            }
            if stdout_eof
                && stderr_eof
                && let Some(status) = status
            {
                return Ok(Output {
                    status,
                    stdout: stdout_bytes,
                    stderr: stderr_bytes,
                });
            }
            if started.elapsed() >= timeout {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("brightness helper exceeded {timeout:?}"),
                ));
            }
            thread::sleep(COMMAND_POLL_INTERVAL.min(timeout.saturating_sub(started.elapsed())));
        }
    })();
    if result.is_err() {
        terminate_child_group(&mut child);
    }
    result
}

fn set_nonblocking(fd: std::os::fd::RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Drain one non-blocking pipe turn and report whether EOF was observed.
fn drain_available(
    source: &mut impl Read,
    retained: &mut Vec<u8>,
    limit: usize,
) -> io::Result<bool> {
    let mut drained = 0;
    let mut chunk = [0_u8; 4096];
    while drained < MAX_DRAIN_BYTES_PER_POLL {
        let request = chunk.len().min(MAX_DRAIN_BYTES_PER_POLL - drained);
        match source.read(&mut chunk[..request]) {
            Ok(0) => return Ok(true),
            Ok(read) => {
                drained += read;
                let retain = read.min(limit.saturating_sub(retained.len()));
                retained.extend_from_slice(&chunk[..retain]);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

fn kill_process_group(child_id: u32) {
    if let Ok(process_group) = i32::try_from(child_id) {
        let _ = unsafe { libc::kill(-process_group, libc::SIGKILL) };
    }
}

fn terminate_child_group(child: &mut std::process::Child) {
    kill_process_group(child.id());
    let _ = child.kill();
    let _ = child.wait();
}

fn run_brightnessctl(args: &[&str]) -> io::Result<()> {
    brightnessctl_output(args).map(|_| ())
}

fn read_brightness_percent() -> io::Result<Option<u8>> {
    let output = brightnessctl_output(&["-m"])?;
    let stdout = std::str::from_utf8(&output.stdout).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("brightnessctl -m returned non-UTF-8 output: {error}"),
        )
    })?;
    parse_brightness_percent(stdout)
}

fn parse_brightness_percent(output: &str) -> io::Result<Option<u8>> {
    if output.trim().is_empty() {
        return Ok(None);
    }

    let line = output.lines().next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "brightnessctl -m returned no device row",
        )
    })?;
    let field = line.split(',').nth(3).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "brightnessctl -m row is missing its percentage field",
        )
    })?;
    let percent = field
        .trim()
        .strip_suffix('%')
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "brightnessctl -m percentage has no percent suffix",
            )
        })?
        .parse::<u8>()
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("brightnessctl -m percentage is invalid: {error}"),
            )
        })?;
    Ok(Some(percent.min(100)))
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::process::Command;
    use std::time::{Duration, Instant};

    use super::{BrightnessManager, command_output_bounded, parse_brightness_percent};

    #[test]
    fn parses_machine_readable_output() {
        let output = "intel_backlight,backlight,48000,42%,120000\n";
        assert_eq!(parse_brightness_percent(output).unwrap(), Some(42));
    }

    #[test]
    fn parses_only_the_first_device() {
        let output = concat!(
            "intel_backlight,backlight,48000,40%,120000\n",
            "leds,led,1,100%,1\n",
        );
        assert_eq!(parse_brightness_percent(output).unwrap(), Some(40));
    }

    #[test]
    fn empty_output_means_no_brightness_device() {
        assert_eq!(parse_brightness_percent("").unwrap(), None);
        assert_eq!(parse_brightness_percent(" \n\t").unwrap(), None);
    }

    #[test]
    fn rejects_malformed_machine_readable_output() {
        for output in [
            "device,backlight,1",
            "device,backlight,1,not-a-percent,2",
            "device,backlight,1,42,2",
        ] {
            assert_eq!(
                parse_brightness_percent(output).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }

    #[test]
    fn clamps_out_of_range_machine_readable_output() {
        assert_eq!(
            parse_brightness_percent("device,backlight,1,150%,2").unwrap(),
            Some(100)
        );
    }

    #[test]
    fn failed_refresh_preserves_the_last_good_value_and_is_rate_limited() {
        let previous_update = Instant::now();
        let mut manager = BrightnessManager {
            percent: Some(42),
            last_update: previous_update,
            update_interval: Duration::MAX,
        };

        let error = manager
            .try_refresh_with(|| {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "malformed probe",
                ))
            })
            .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(manager.percent(), Some(42));
        assert!(manager.last_update >= previous_update);
        assert!(!manager.update_if_needed());
    }

    #[test]
    fn helper_output_is_bounded_while_the_pipe_is_drained() {
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "head -c 262144 /dev/zero; head -c 262144 /dev/zero >&2",
        ]);

        let output = command_output_bounded(&mut command, Duration::from_secs(2), 1024).unwrap();

        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 1024);
        assert_eq!(output.stderr.len(), 1024);
    }

    #[test]
    fn helper_runtime_has_a_hard_deadline() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 10 & wait"]);
        let started = Instant::now();

        let error =
            command_output_bounded(&mut command, Duration::from_millis(30), 1024).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn successful_wrapper_does_not_leave_a_background_descendant() {
        let marker = std::env::temp_dir().join(format!(
            "xbar-brightness-descendant-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&marker);

        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                "(sleep 0.1; printf leaked > \"$1\") &",
                "xbar-brightness-test",
            ])
            .arg(&marker);
        let output = command_output_bounded(&mut command, Duration::from_secs(1), 1024).unwrap();
        assert!(output.status.success());

        std::thread::sleep(Duration::from_millis(300));
        let leaked = marker.exists();
        let _ = std::fs::remove_file(marker);
        assert!(!leaked, "background helper survived its wrapper");
    }
}
