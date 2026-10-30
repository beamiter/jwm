use std::io::{self, Read};
use std::os::fd::AsRawFd as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use log::warn;

const BRIGHTNESSCTL_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_BRIGHTNESSCTL_OUTPUT_BYTES: usize = 64 * 1024;
const COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAX_DRAIN_BYTES_PER_POLL: usize = 64 * 1024;
const BACKLIGHT_ROOT: &str = "/sys/class/backlight";
const MAX_BACKLIGHT_ENTRIES: usize = 64;
const MAX_BACKLIGHT_ATTRIBUTE_BYTES: u64 = 4096;

/// Panel brightness through `brightnessctl`, with a sysfs/logind fallback.
///
/// Percentage is parsed from `brightnessctl -m` machine-readable output
/// (`<device>,<class>,<current>,<percent>%,<max>`). When the helper is absent or
/// cannot write the backlight, sysfs supplies the level and logind's session
/// SetBrightness method handles permissions without elevating the bar.
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

    /// Adjust brightness and return an error if neither backend succeeds.
    pub fn try_adjust(&mut self, delta_percent: i32) -> io::Result<bool> {
        if delta_percent == 0 {
            return Ok(false);
        }
        let arg = if delta_percent > 0 {
            format!("{}%+", delta_percent)
        } else {
            format!("{}%-", delta_percent.unsigned_abs())
        };
        if let Err(error) = run_brightnessctl(&["-n1", "set", &arg]) {
            set_backlight_fallback(None, delta_percent).map_err(|fallback| {
                io::Error::other(format!("{error}; backlight fallback failed: {fallback}"))
            })?;
        }
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

    /// Set brightness and return an error if neither backend succeeds.
    pub fn try_set_percent(&mut self, percent: u8) -> io::Result<bool> {
        let arg = format!("{}%", percent.min(100));
        if let Err(error) = run_brightnessctl(&["-n1", "set", &arg]) {
            set_backlight_fallback(Some(percent), 0).map_err(|fallback| {
                io::Error::other(format!("{error}; backlight fallback failed: {fallback}"))
            })?;
        }
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
    // Never mistake a keyboard or network LED for the display backlight.
    command.args(["-c", "backlight"]).args(args);
    // Use the same deterministic device for the helper and native fallback.
    if let Some(device) = read_backlight(Path::new(BACKLIGHT_ROOT))? {
        command.arg("-d").arg(device.path.file_name().unwrap());
    }
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
    read_brightness_percent_with(Path::new(BACKLIGHT_ROOT), || {
        let output = brightnessctl_output(&["-m"])?;
        let stdout = std::str::from_utf8(&output.stdout).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("brightnessctl -m returned non-UTF-8 output: {error}"),
            )
        })?;
        parse_brightness_percent(stdout)
    })
}

fn read_brightness_percent_with(
    root: &Path,
    probe: impl FnOnce() -> io::Result<Option<u8>>,
) -> io::Result<Option<u8>> {
    let result = probe();
    if matches!(result, Ok(Some(_))) {
        return result;
    }
    match read_backlight(root)? {
        Some(device) => Ok(Some(device.percent())),
        None => result,
    }
}

struct Backlight {
    path: PathBuf,
    current: u32,
    max: u32,
}

impl Backlight {
    fn percent(&self) -> u8 {
        ((u64::from(self.current) * 100 + u64::from(self.max) / 2) / u64::from(self.max)).min(100)
            as u8
    }

    fn raw_for_percent(&self, percent: u8) -> u32 {
        // A slider drag or repeated decrease must not switch the panel off.
        ((u64::from(percent.min(100)) * u64::from(self.max) + 50) / 100).max(1) as u32
    }
}

fn read_backlight(root: &Path) -> io::Result<Option<Backlight>> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut paths = entries
        .take(MAX_BACKLIGHT_ENTRIES + 1)
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<io::Result<Vec<_>>>()?;
    if paths.len() > MAX_BACKLIGHT_ENTRIES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "backlight directory exceeds entry limit",
        ));
    }
    paths.sort();
    for path in paths {
        if let (Ok(current), Ok(max)) = (
            read_backlight_attribute(&path.join("brightness")),
            read_backlight_attribute(&path.join("max_brightness")),
        ) && max > 0
        {
            return Ok(Some(Backlight { path, current, max }));
        }
    }
    Ok(None)
}

fn read_backlight_attribute(path: &Path) -> io::Result<u32> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_BACKLIGHT_ATTRIBUTE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BACKLIGHT_ATTRIBUTE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "backlight attribute exceeds read limit",
        ));
    }
    std::str::from_utf8(&bytes)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid backlight attribute"))
}

fn set_backlight_fallback(percent: Option<u8>, delta: i32) -> io::Result<()> {
    let device = read_backlight(Path::new(BACKLIGHT_ROOT))?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no panel backlight"))?;
    let percent = percent.unwrap_or_else(|| {
        i32::from(device.percent())
            .saturating_add(delta)
            .clamp(0, 100) as u8
    });
    write_backlight_with(&device, percent, set_session_brightness)
}

fn write_backlight_with(
    device: &Backlight,
    percent: u8,
    session_set: impl FnOnce(&Path, u32) -> io::Result<()>,
) -> io::Result<()> {
    let raw = device.raw_for_percent(percent);
    if std::fs::write(device.path.join("brightness"), raw.to_string()).is_ok() {
        return Ok(());
    }
    session_set(&device.path, raw)
}

fn set_session_brightness(path: &Path, raw: u32) -> io::Result<()> {
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "missing backlight device name")
    })?;
    let mut command = Command::new("busctl");
    command.args([
        "--system",
        "--allow-interactive-authorization=no",
        "--timeout=2",
        "call",
        "org.freedesktop.login1",
        "/org/freedesktop/login1/session/auto",
        "org.freedesktop.login1.Session",
        "SetBrightness",
        "ssu",
        "backlight",
    ]);
    command.arg(name).arg(raw.to_string());
    let output = command_output_bounded(
        &mut command,
        BRIGHTNESSCTL_TIMEOUT,
        MAX_BRIGHTNESSCTL_OUTPUT_BYTES,
    )?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "logind SetBrightness failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

fn parse_brightness_percent(output: &str) -> io::Result<Option<u8>> {
    if output.trim().is_empty() {
        return Ok(None);
    }

    let Some(line) = output
        .lines()
        .find(|line| line.split(',').nth(1) == Some("backlight"))
    else {
        return Ok(None);
    };
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
    use std::fs;
    use std::io;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use super::{
        Backlight, BrightnessManager, MAX_BACKLIGHT_ATTRIBUTE_BYTES, MAX_BACKLIGHT_ENTRIES,
        command_output_bounded, parse_brightness_percent, read_backlight, read_backlight_attribute,
        read_brightness_percent_with, write_backlight_with,
    };

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let id = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("xbar-backlight-{}-{id}", std::process::id()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn device(&self, name: &str, current: u32, max: u32) -> PathBuf {
            let path = self.0.join(name);
            fs::create_dir(&path).unwrap();
            fs::write(path.join("brightness"), current.to_string()).unwrap();
            fs::write(path.join("max_brightness"), max.to_string()).unwrap();
            path
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn missing_or_failing_helper_reads_the_real_backlight() {
        let root = TestDirectory::new();
        root.device("intel_backlight", 47530, 96000);
        for failure in [io::ErrorKind::NotFound, io::ErrorKind::PermissionDenied] {
            assert_eq!(
                read_brightness_percent_with(&root.0, || Err(io::Error::from(failure))).unwrap(),
                Some(50)
            );
        }
        assert_eq!(
            read_brightness_percent_with(&root.0, || Ok(None)).unwrap(),
            Some(50)
        );
    }

    #[test]
    fn native_device_selection_skips_invalid_entries_and_is_deterministic() {
        let root = TestDirectory::new();
        root.device("z_backlight", 90, 100);
        let selected = root.device("b_backlight", u32::MAX, u32::MAX);
        root.device("a_invalid", 0, 0);
        let device = read_backlight(&root.0).unwrap().unwrap();
        assert_eq!(device.path, selected);
        assert_eq!(device.percent(), 100);
        assert_eq!(device.raw_for_percent(100), u32::MAX);
        assert_eq!(device.raw_for_percent(u8::MAX), u32::MAX);
        assert!(read_backlight(&root.0.join("absent")).unwrap().is_none());
    }

    #[test]
    fn native_probes_bound_directory_and_attribute_reads() {
        let root = TestDirectory::new();
        let oversized = root.0.join("attribute");
        fs::write(
            &oversized,
            vec![b'1'; MAX_BACKLIGHT_ATTRIBUTE_BYTES as usize + 1],
        )
        .unwrap();
        assert_eq!(
            read_backlight_attribute(&oversized).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        for n in 0..MAX_BACKLIGHT_ENTRIES {
            fs::create_dir(root.0.join(format!("device-{n}"))).unwrap();
        }
        assert_eq!(
            read_backlight(&root.0).err().unwrap().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn native_write_uses_raw_units_and_never_blacks_out_the_panel() {
        let root = TestDirectory::new();
        root.device("intel_backlight", 47530, 96000);
        let device = read_backlight(&root.0).unwrap().unwrap();
        for (percent, raw) in [(55, 52800), (0, 1), (u8::MAX, 96000)] {
            write_backlight_with(&device, percent, |_, _| panic!("sysfs is writable")).unwrap();
            assert_eq!(
                read_backlight_attribute(&device.path.join("brightness")).unwrap(),
                raw
            );
        }
    }

    #[test]
    fn failed_direct_write_uses_session_permissions_and_reports_failure() {
        let root = TestDirectory::new();
        let path = root.0.join("intel_backlight");
        // A directory in place of the attribute forces a write failure even
        // when the test runner has root privileges.
        fs::create_dir_all(path.join("brightness")).unwrap();
        let device = Backlight {
            path,
            current: 47530,
            max: 96000,
        };
        write_backlight_with(&device, 55, |path, raw| {
            assert_eq!(path.file_name().unwrap(), "intel_backlight");
            assert_eq!(raw, 52800);
            Ok(())
        })
        .unwrap();
        assert_eq!(
            write_backlight_with(&device, 55, |_, _| Err(io::Error::from(
                io::ErrorKind::PermissionDenied
            )))
            .unwrap_err()
            .kind(),
            io::ErrorKind::PermissionDenied
        );
    }

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
    fn never_reads_keyboard_or_network_leds_as_panel_brightness() {
        let leds = "input3::capslock,leds,0,0%,1\nigc-08400-led1,leds,1,100%,1\n";
        assert_eq!(parse_brightness_percent(leds).unwrap(), None);
        let mixed = format!("{leds}intel_backlight,backlight,47530,50%,96000\n");
        assert_eq!(parse_brightness_percent(&mixed).unwrap(), Some(50));
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
