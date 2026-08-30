//! Bounded execution for small session helpers used by JWM features.

use std::io::{self, Read};
use std::os::fd::AsRawFd as _;
use std::os::unix::process::CommandExt as _;
use std::process::{Command, ExitStatus, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const HELPER_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_HELPER_OUTPUT_BYTES: usize = 1024 * 1024;
const HELPER_POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAX_DRAIN_BYTES_PER_POLL: usize = 64 * 1024;

pub(super) fn output(cmd: &str, args: &[&str]) -> io::Result<Output> {
    output_with_limits(cmd, args, HELPER_TIMEOUT, MAX_HELPER_OUTPUT_BYTES)
}

pub(super) fn output_with_limits(
    cmd: &str,
    args: &[&str],
    timeout: Duration,
    output_limit: usize,
) -> io::Result<Output> {
    let mut command = Command::new(cmd);
    command.args(args);
    command_output_bounded(&mut command, timeout, output_limit)
}

/// Run a helper that consumes a caller-provided stdin, discards stdout, and
/// retains bounded stderr for diagnostics.
pub(super) fn output_with_input(
    cmd: &str,
    args: &[&str],
    stdin: Stdio,
    timeout: Duration,
    stderr_limit: usize,
) -> io::Result<Output> {
    let mut command = Command::new(cmd);
    command.args(args);
    command_output_bounded_with_stdio(&mut command, stdin, false, timeout, stderr_limit)
}

/// Run a helper whose output is irrelevant, with a hard wall-time limit.
pub(super) fn status_with_timeout(
    cmd: &str,
    args: &[&str],
    timeout: Duration,
) -> io::Result<ExitStatus> {
    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // A synchronous helper may not leave a background process
                // running after its direct child has reported completion.
                kill_process_group(child.id());
                return Ok(status);
            }
            Ok(None) => {}
            Err(error) => {
                terminate_child_group(&mut child);
                return Err(error);
            }
        }
        let elapsed = started.elapsed();
        if elapsed >= timeout {
            terminate_child_group(&mut child);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("helper exceeded {timeout:?}"),
            ));
        }
        thread::sleep(HELPER_POLL_INTERVAL.min(timeout.saturating_sub(elapsed)));
    }
}

fn command_output_bounded(
    command: &mut Command,
    timeout: Duration,
    output_limit: usize,
) -> io::Result<Output> {
    command_output_bounded_with_stdio(command, Stdio::null(), true, timeout, output_limit)
}

fn command_output_bounded_with_stdio(
    command: &mut Command,
    stdin: Stdio,
    capture_stdout: bool,
    timeout: Duration,
    output_limit: usize,
) -> io::Result<Output> {
    command.stdin(stdin);
    if capture_stdout {
        command.stdout(Stdio::piped());
    } else {
        command.stdout(Stdio::null());
    }
    command
        .stderr(Stdio::piped())
        // Keep every descendant in one group so none can outlive a bounded
        // helper or keep a capture pipe open after the direct child exits.
        .process_group(0);
    let mut child = command.spawn()?;
    let child_id = child.id();
    let mut stdout = if capture_stdout {
        let Some(stdout) = child.stdout.take() else {
            terminate_child_group(&mut child);
            return Err(io::Error::other("helper stdout was not captured"));
        };
        Some(stdout)
    } else {
        None
    };
    let Some(mut stderr) = child.stderr.take() else {
        terminate_child_group(&mut child);
        return Err(io::Error::other("helper stderr was not captured"));
    };

    let nonblocking = stdout
        .as_ref()
        .map_or(Ok(()), |stdout| set_nonblocking(stdout.as_raw_fd()))
        .and_then(|()| set_nonblocking(stderr.as_raw_fd()));
    if let Err(error) = nonblocking {
        terminate_child_group(&mut child);
        return Err(error);
    }

    let started = Instant::now();
    let mut stdout_bytes = Vec::with_capacity(output_limit.min(4096));
    let mut stderr_bytes = Vec::with_capacity(output_limit.min(4096));
    let mut stdout_eof = !capture_stdout;
    let mut stderr_eof = false;
    let mut status = None;
    let result = (|| {
        loop {
            if !stdout_eof {
                let stdout = stdout
                    .as_mut()
                    .expect("captured stdout remains available until EOF");
                let drain = drain_available(stdout, &mut stdout_bytes, output_limit)?;
                stdout_eof = drain.eof;
                if drain.oversized {
                    return Err(output_too_large(output_limit));
                }
            }
            if !stderr_eof {
                let drain = drain_available(&mut stderr, &mut stderr_bytes, output_limit)?;
                stderr_eof = drain.eof;
                if drain.oversized {
                    return Err(output_too_large(output_limit));
                }
            }

            if status.is_none() {
                status = child.try_wait()?;
                if status.is_some() {
                    // Any process still holding these pipes is a descendant
                    // of an already-completed synchronous helper. Stop it,
                    // then keep draining until both pipes reach EOF.
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

            let elapsed = started.elapsed();
            if elapsed >= timeout {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("helper exceeded {timeout:?}"),
                ));
            }
            thread::sleep(HELPER_POLL_INTERVAL.min(timeout.saturating_sub(elapsed)));
        }
    })();
    if result.is_err() {
        terminate_child_group(&mut child);
    }
    result
}

fn output_too_large(limit: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("helper output exceeded {limit} bytes"),
    )
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

/// Drain enough each poll that a noisy helper cannot fill its pipes, while
/// retaining at most `limit` bytes for parsing and error reporting.
#[derive(Debug, Clone, Copy)]
struct DrainResult {
    eof: bool,
    oversized: bool,
}

fn drain_available(
    source: &mut impl Read,
    retained: &mut Vec<u8>,
    limit: usize,
) -> io::Result<DrainResult> {
    let mut drained = 0;
    let mut oversized = false;
    let mut chunk = [0_u8; 4096];
    while drained < MAX_DRAIN_BYTES_PER_POLL {
        let request = chunk.len().min(MAX_DRAIN_BYTES_PER_POLL - drained);
        match source.read(&mut chunk[..request]) {
            Ok(0) => {
                return Ok(DrainResult {
                    eof: true,
                    oversized,
                });
            }
            Ok(read) => {
                drained += read;
                let retain = read.min(limit.saturating_sub(retained.len()));
                retained.extend_from_slice(&chunk[..retain]);
                oversized |= retain < read;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                return Ok(DrainResult {
                    eof: false,
                    oversized,
                });
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(DrainResult {
        eof: false,
        oversized,
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_output_is_bounded() {
        let mut command = Command::new("sh");
        command.args(["-c", "printf 123456789"]);
        let error = command_output_bounded(&mut command, Duration::from_secs(1), 8)
            .expect_err("oversized output must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn helper_wait_has_a_hard_deadline() {
        let mut command = Command::new("sh");
        command.args(["-c", "exec sleep 10"]);
        let started = Instant::now();
        let error = command_output_bounded(&mut command, Duration::from_millis(25), 64)
            .expect_err("sleeping helper must time out");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "helper was not terminated promptly"
        );
    }

    #[test]
    fn helper_drains_more_than_one_poll_of_output() {
        let mut command = Command::new("sh");
        command.args(["-c", "yes x | head -c 196608"]);
        let output = command_output_bounded(&mut command, Duration::from_secs(2), 256 * 1024)
            .expect("bounded helper output should be complete");

        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 196_608);
        assert!(output.stderr.is_empty());
    }

    #[test]
    fn completed_helper_stops_descendants_holding_capture_pipes() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 10 & printf %s \"$!\""]);
        let output = command_output_bounded(&mut command, Duration::from_secs(1), 64)
            .expect("the direct helper completed successfully");
        let descendant = String::from_utf8(output.stdout)
            .unwrap()
            .parse::<u32>()
            .unwrap();

        for _ in 0..50 {
            if !process_can_run(descendant) {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let _ = unsafe { libc::kill(descendant as i32, libc::SIGKILL) };
        panic!("helper descendant {descendant} survived its process group");
    }

    #[test]
    fn status_helper_preserves_exit_status_and_enforces_timeout() {
        let status = status_with_timeout("sh", &["-c", "exit 7"], Duration::from_secs(1)).unwrap();
        assert_eq!(status.code(), Some(7));

        let started = Instant::now();
        let error = status_with_timeout("sh", &["-c", "exec sleep 10"], Duration::from_millis(25))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    fn process_can_run(pid: u32) -> bool {
        let Ok(stat) = std::fs::read(format!("/proc/{pid}/stat")) else {
            return false;
        };
        let Some(comm_end) = stat.iter().rposition(|byte| *byte == b')') else {
            return true;
        };
        !matches!(
            stat.get(comm_end + 1..)
                .and_then(|suffix| suffix.iter().find(|byte| !byte.is_ascii_whitespace())),
            Some(b'Z' | b'X' | b'x')
        )
    }
}
