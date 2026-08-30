//! Bounded execution for small session helpers used by JWM features.

use std::io::{self, Read};
use std::os::fd::AsRawFd as _;
use std::os::unix::process::CommandExt as _;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const HELPER_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_HELPER_OUTPUT_BYTES: usize = 1024 * 1024;
const HELPER_POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAX_DRAIN_BYTES_PER_POLL: usize = 64 * 1024;

pub(super) fn output(cmd: &str, args: &[&str]) -> io::Result<Output> {
    let mut command = Command::new(cmd);
    command.args(args);
    command_output_bounded(&mut command, HELPER_TIMEOUT, MAX_HELPER_OUTPUT_BYTES)
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
        // Kill descendants with a timed-out helper so none can keep a capture
        // pipe open after the command itself has gone away.
        .process_group(0);
    let mut child = command.spawn()?;
    let Some(mut stdout) = child.stdout.take() else {
        terminate_child_group(&mut child);
        return Err(io::Error::other("helper stdout was not captured"));
    };
    let Some(mut stderr) = child.stderr.take() else {
        terminate_child_group(&mut child);
        return Err(io::Error::other("helper stderr was not captured"));
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
    let result = (|| {
        loop {
            let oversized = drain_available(&mut stdout, &mut stdout_bytes, output_limit)?
                | drain_available(&mut stderr, &mut stderr_bytes, output_limit)?;
            if oversized {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("helper output exceeded {output_limit} bytes"),
                ));
            }

            if let Some(status) = child.try_wait()? {
                let oversized = drain_available(&mut stdout, &mut stdout_bytes, output_limit)?
                    | drain_available(&mut stderr, &mut stderr_bytes, output_limit)?;
                if oversized {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("helper output exceeded {output_limit} bytes"),
                    ));
                }
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
/// retaining at most `limit` bytes for parsing and error reporting. Returns
/// whether input beyond the limit was observed.
fn drain_available(
    source: &mut impl Read,
    retained: &mut Vec<u8>,
    limit: usize,
) -> io::Result<bool> {
    let mut drained = 0;
    let mut oversized = false;
    let mut chunk = [0_u8; 4096];
    while drained < MAX_DRAIN_BYTES_PER_POLL {
        let request = chunk.len().min(MAX_DRAIN_BYTES_PER_POLL - drained);
        match source.read(&mut chunk[..request]) {
            Ok(0) => return Ok(oversized),
            Ok(read) => {
                drained += read;
                let retain = read.min(limit.saturating_sub(retained.len()));
                retained.extend_from_slice(&chunk[..retain]);
                oversized |= retain < read;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(oversized),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(oversized)
}

fn terminate_child_group(child: &mut std::process::Child) {
    if let Ok(process_group) = i32::try_from(child.id()) {
        let _ = unsafe { libc::kill(-process_group, libc::SIGKILL) };
    }
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
}
