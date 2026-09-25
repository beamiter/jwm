//! Bounded execution for small synchronous helpers used by JWM.

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

pub(crate) fn output_with_limits(
    cmd: &str,
    args: &[&str],
    timeout: Duration,
    output_limit: usize,
) -> io::Result<Output> {
    let mut command = Command::new(cmd);
    command.args(args);
    command_output_bounded(&mut command, timeout, output_limit)
}

/// Run a short daemon launcher while preserving descendants only after the
/// direct launcher exits successfully. Failures, timeouts, and oversized
/// output still terminate the launcher's entire process group.
pub(crate) fn daemon_launcher_output_with_limits(
    cmd: &str,
    args: &[&str],
    timeout: Duration,
    output_limit: usize,
) -> io::Result<Output> {
    let mut command = Command::new(cmd);
    command.args(args);
    command_output_bounded_with_policy(
        &mut command,
        Stdio::null(),
        true,
        timeout,
        output_limit,
        SuccessfulDescendants::Preserve,
    )
}

/// Run a synchronous helper that reads caller-provided stdin, under the same
/// deadline, output bound and descendant cleanup as [`output_with_limits`].
///
/// The stdin is the caller's to prepare — a pipe whose write end is already
/// closed, typically, so a helper that reads more than it was given sees end
/// of file instead of waiting out the deadline. Unlike the selection-owner
/// runner below, nothing the helper forks outlives it.
pub(crate) fn output_with_input_and_limits(
    cmd: &str,
    args: &[&str],
    stdin: Stdio,
    timeout: Duration,
    output_limit: usize,
) -> io::Result<Output> {
    let mut command = Command::new(cmd);
    command.args(args);
    command_output_bounded_with_policy(
        &mut command,
        stdin,
        true,
        timeout,
        output_limit,
        SuccessfulDescendants::Terminate,
    )
}

/// Run a selection-owner launcher that consumes caller-provided stdin.
///
/// `wl-copy` forks after reading the payload and its successful descendant is
/// the process that must remain alive to serve paste requests. Failed or
/// timed-out launchers still have their entire process group terminated.
pub(super) fn selection_owner_output_with_input(
    cmd: &str,
    args: &[&str],
    stdin: Stdio,
    timeout: Duration,
    stderr_limit: usize,
) -> io::Result<Output> {
    let mut command = Command::new(cmd);
    command.args(args);
    command_output_bounded_with_policy(
        &mut command,
        stdin,
        false,
        timeout,
        stderr_limit,
        SuccessfulDescendants::Preserve,
    )
}

/// Run a helper whose output is irrelevant, with a hard wall-time limit.
pub(crate) fn status_with_timeout(
    cmd: &str,
    args: &[&str],
    timeout: Duration,
) -> io::Result<ExitStatus> {
    let mut command = Command::new(cmd);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // No SIGCHLD hook: a pre_exec closure would take std off posix_spawn
        // (see `unblock_sigchld_in_child`), and a helper that is waited for
        // and whose group is killed afterwards leaves nobody who needs it.
        .process_group(0);
    let mut child = command.spawn()?;
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

/// Spawn a long-lived helper without waiting for it: detached stdio, no
/// output capture, no deadline. The caller owns reaping the returned child —
/// in practice it is handed to the transient-child supervisor. This is the
/// one unbounded path here; everything above waits with a hard limit because
/// those helpers are synchronous, while this one (e.g. the Bluetooth pairing
/// agent) must stay alive until *its* conversation ends.
pub(crate) fn spawn_detached(
    cmd: &str,
    args: &[&str],
    env: &[(&str, &str)],
) -> io::Result<std::process::Child> {
    let mut command = Command::new(cmd);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (key, value) in env {
        command.env(key, value);
    }
    unblock_sigchld_in_child(&mut command);
    command.spawn()
}

/// Unblock `SIGCHLD` in `command`'s child and change nothing else — no
/// `setsid`, no disposition reset, no process group — for a child that
/// outlives the call that started it.
///
/// The child inherits the spawning thread's signal mask (std does not reset
/// it), and JWM keeps SIGCHLD blocked on its event thread and on the worker
/// threads it starts, so the event loop's signalfd is the only taker. A
/// child that relies on a SIGCHLD handler without resetting its own mask —
/// the daemon a launcher leaves behind, the Bluetooth pairing agent — would
/// never receive the signal. The module sits at the crate root, so the
/// policy layer's spawn points and the backends can share this one hook.
///
/// Only long-lived children get it: std spawns through `posix_spawn` (a
/// vfork-style clone that copies nothing) only while a command has no
/// `pre_exec` closure, and with one it forks the whole compositor, page
/// tables included. The synchronous helpers here are waited for and their
/// process group is killed once they finish, so no descendant of theirs is
/// left to need the signal, and they keep the cheap spawn.
pub(crate) fn unblock_sigchld_in_child(command: &mut Command) {
    // SAFETY: the hook only calls sigemptyset/sigaddset/sigprocmask, which
    // are async-signal-safe, and the forked child has one thread.
    unsafe {
        command.pre_exec(|| {
            let mut unblock: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut unblock);
            libc::sigaddset(&mut unblock, libc::SIGCHLD);
            libc::sigprocmask(libc::SIG_UNBLOCK, &unblock, std::ptr::null_mut());
            Ok(())
        });
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
    command_output_bounded_with_policy(
        command,
        stdin,
        capture_stdout,
        timeout,
        output_limit,
        SuccessfulDescendants::Terminate,
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SuccessfulDescendants {
    Terminate,
    Preserve,
}

fn command_output_bounded_with_policy(
    command: &mut Command,
    stdin: Stdio,
    capture_stdout: bool,
    timeout: Duration,
    output_limit: usize,
    successful_descendants: SuccessfulDescendants,
) -> io::Result<Output> {
    command.stdin(stdin);
    if capture_stdout {
        command.stdout(Stdio::piped());
    } else {
        command.stdout(Stdio::null());
    }
    command
        .stderr(Stdio::piped())
        // Keep every descendant in one group so failure paths can clean the
        // whole launch tree. The synchronous policy also cleans it after a
        // successful direct-child exit; daemon launchers deliberately do not.
        .process_group(0);
    if successful_descendants == SuccessfulDescendants::Preserve {
        // A preserved descendant (the daemon a launcher leaves, wl-copy's
        // selection owner) outlives this call. Terminated trees do not, and
        // stay on posix_spawn; see `unblock_sigchld_in_child`.
        unblock_sigchld_in_child(command);
    }
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
    let mut descendants_terminated = false;
    let result = (|| {
        loop {
            let mut stdout_quiescent = stdout_eof;
            if !stdout_eof {
                let stdout = stdout
                    .as_mut()
                    .expect("captured stdout remains available until EOF");
                let drain = drain_available(stdout, &mut stdout_bytes, output_limit)?;
                stdout_eof = drain.eof;
                stdout_quiescent = drain.quiescent;
                if drain.oversized {
                    return Err(output_too_large(output_limit));
                }
            }
            let mut stderr_quiescent = stderr_eof;
            if !stderr_eof {
                let drain = drain_available(&mut stderr, &mut stderr_bytes, output_limit)?;
                stderr_eof = drain.eof;
                stderr_quiescent = drain.quiescent;
                if drain.oversized {
                    return Err(output_too_large(output_limit));
                }
            }

            let completed_before_poll = status.is_some();
            if status.is_none() {
                status = child.try_wait()?;
            }
            let completed_during_poll = !completed_before_poll && status.is_some();
            if let Some(exit_status) = status {
                let preserve_descendants = successful_descendants
                    == SuccessfulDescendants::Preserve
                    && exit_status.success();
                if preserve_descendants
                    && !completed_during_poll
                    && stdout_quiescent
                    && stderr_quiescent
                {
                    // The direct child cannot write after it has exited. A
                    // second drain after observing that exit therefore
                    // captures all launcher output without waiting for EOF
                    // from an intentionally surviving daemon.
                    return Ok(Output {
                        status: exit_status,
                        stdout: stdout_bytes,
                        stderr: stderr_bytes,
                    });
                }
                if !preserve_descendants && !descendants_terminated {
                    // Any process still holding these pipes is a descendant
                    // of an already-completed synchronous helper. Stop it,
                    // then keep draining until both pipes reach EOF.
                    kill_process_group(child_id);
                    descendants_terminated = true;
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
    quiescent: bool,
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
                    quiescent: true,
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
                    quiescent: true,
                });
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(DrainResult {
        eof: false,
        oversized,
        quiescent: false,
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

/// Shared by the spawn-site tests here, in the policy modules that start
/// processes (launched apps, status bar, session, idle and recorder
/// commands) and in the backends' encoder spawn: a thread that blocks
/// SIGCHLD the way JWM's threads do, and a probe that records the signal
/// mask a child ran with.
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::PathBuf;

    /// Blocks SIGCHLD on the calling thread, as JWM's event loop and worker
    /// threads do, and restores the previous mask on drop so a failed
    /// assertion cannot leave a test thread with it blocked.
    pub(crate) struct SigchldBlockedOnThisThread(libc::sigset_t);

    impl SigchldBlockedOnThisThread {
        pub(crate) fn new() -> Self {
            // SAFETY: plain signal-set manipulation on this thread's mask.
            unsafe {
                let mut block: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut block);
                libc::sigaddset(&mut block, libc::SIGCHLD);
                let mut previous: libc::sigset_t = std::mem::zeroed();
                assert_eq!(
                    libc::pthread_sigmask(libc::SIG_BLOCK, &block, &mut previous),
                    0
                );
                Self(previous)
            }
        }
    }

    impl Drop for SigchldBlockedOnThisThread {
        fn drop(&mut self) {
            // SAFETY: restores the mask saved by `new` on the same thread.
            unsafe {
                libc::pthread_sigmask(libc::SIG_SETMASK, &self.0, std::ptr::null_mut());
            }
        }
    }

    /// Whether SIGCHLD is in the `SigBlk` mask of a `/proc/<pid>/status`
    /// dump.
    pub(crate) fn status_blocks_sigchld(status: &str) -> bool {
        let mask = status
            .lines()
            .find_map(|line| line.strip_prefix("SigBlk:"))
            .map(str::trim)
            .and_then(|hex| u64::from_str_radix(hex, 16).ok())
            .expect("a SigBlk line");
        mask & (1 << (libc::SIGCHLD - 1)) != 0
    }

    /// A private directory for one child's `/proc/self/status`, for spawn
    /// paths whose stdout the test cannot capture. Removed on drop.
    pub(crate) struct SigchldProbe(PathBuf);

    impl SigchldProbe {
        pub(crate) fn new(label: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "jwm-sigchld-probe-{label}-{}-{sequence}",
                std::process::id()
            ));
            // `create_dir`, not `create_dir_all`: a leftover of the same name
            // fails the test instead of being written through.
            std::fs::create_dir(&path).expect("create the probe directory");
            Self(path)
        }

        /// A `sh -c` script that dumps the shell's own status into the
        /// probe. `exec` matters: dash resets the mask of the commands it
        /// forks, so only a command it execs in place reports the mask the
        /// shell itself was started with.
        pub(crate) fn script(&self) -> String {
            format!(
                "exec cat /proc/self/status > '{}'",
                self.0.join("status").display()
            )
        }

        /// [`Self::script`] as a file, for a command whose one argument is
        /// the script `sh` runs.
        pub(crate) fn script_file(&self) -> PathBuf {
            let path = self.0.join("probe.sh");
            std::fs::write(&path, self.script()).expect("write the probe script");
            path
        }

        fn status(&self) -> String {
            std::fs::read_to_string(self.0.join("status"))
                .expect("the probed child wrote its status")
        }

        /// Whether the child that ran [`Self::script`] had SIGCHLD blocked.
        pub(crate) fn child_blocked_sigchld(&self) -> bool {
            status_blocks_sigchld(&self.status())
        }

        /// The session the child that ran [`Self::script`] belonged to, as
        /// seen from its own (and this test's) PID namespace — the last
        /// `NSsid` entry.
        pub(crate) fn child_session(&self) -> i32 {
            self.status()
                .lines()
                .find_map(|line| line.strip_prefix("NSsid:"))
                .and_then(|ids| ids.split_whitespace().last())
                .and_then(|id| id.parse().ok())
                .expect("an NSsid line")
        }
    }

    impl Drop for SigchldProbe {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
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
    fn daemon_launcher_output_is_bounded() {
        let error = daemon_launcher_output_with_limits(
            "sh",
            &["-c", "printf 123456789"],
            Duration::from_secs(1),
            8,
        )
        .expect_err("oversized launcher output must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn daemon_launcher_wait_has_a_hard_deadline() {
        let started = Instant::now();
        let error = daemon_launcher_output_with_limits(
            "sh",
            &["-c", "exec sleep 10"],
            Duration::from_millis(25),
            64,
        )
        .expect_err("sleeping launcher must time out");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "launcher was not terminated promptly"
        );
    }

    #[test]
    fn successful_daemon_launcher_preserves_background_process() {
        let started = Instant::now();
        let output = daemon_launcher_output_with_limits(
            "sh",
            &["-c", "sleep 10 & printf %s \"$!\""],
            Duration::from_secs(1),
            64,
        )
        .expect("successful launcher should not wait for its daemon's pipe handles");
        assert!(output.status.success());
        assert!(started.elapsed() < Duration::from_secs(1));

        let descendant = String::from_utf8(output.stdout)
            .unwrap()
            .parse::<u32>()
            .unwrap();
        assert!(
            process_can_run(descendant),
            "successful launcher descendant was terminated"
        );
        let _ = unsafe { libc::kill(descendant as i32, libc::SIGKILL) };
    }

    #[test]
    fn failed_daemon_launcher_stops_background_process() {
        let output = daemon_launcher_output_with_limits(
            "sh",
            &["-c", "sleep 10 & printf %s \"$!\"; exit 7"],
            Duration::from_secs(1),
            64,
        )
        .expect("failed launcher should still return its bounded output");
        assert_eq!(output.status.code(), Some(7));
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
        panic!("failed launcher descendant {descendant} survived its process group");
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
    fn a_helper_reads_the_callers_stdin_and_leaves_no_descendants() {
        use std::io::Write as _;

        let (reader, mut writer) = std::io::pipe().unwrap();
        writer.write_all(b"hunter22\n").unwrap();
        drop(writer);
        let output = output_with_input_and_limits(
            "sh",
            &[
                "-c",
                // Echo the first line back, then try to leave a background
                // process behind; a second read sees end of file at once.
                "IFS= read -r line; printf '%s:' \"$line\"; \
                 if IFS= read -r extra; then exit 9; fi; \
                 sleep 10 & printf %s \"$!\"",
            ],
            Stdio::from(reader),
            Duration::from_secs(2),
            64,
        )
        .expect("the helper completed");

        assert!(output.status.success(), "{output:?}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        let (line, descendant) = stdout.split_once(':').expect("line and pid");
        assert_eq!(line, "hunter22");
        let descendant = descendant.parse::<u32>().unwrap();
        // The runner returned only after the descendant's end of the stdout
        // pipe closed, so it is already exiting; spin (never sleep) past the
        // few instructions between closing its files and becoming a zombie.
        for _ in 0..1_000_000 {
            if !process_can_run(descendant) {
                return;
            }
            thread::yield_now();
        }
        let _ = unsafe { libc::kill(descendant as i32, libc::SIGKILL) };
        panic!("helper descendant {descendant} outlived a stdin-fed helper");
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

    #[test]
    fn detached_spawn_returns_a_live_unsupervised_child() {
        let mut child = spawn_detached(
            "sh",
            &["-c", "test \"$JWM_TEST_DETACHED\" = seen; exit $?"],
            &[("JWM_TEST_DETACHED", "seen")],
        )
        .expect("detached spawn");
        let status = child.wait().expect("reap the child we spawned");
        assert!(status.success(), "extra env did not reach the child");
    }

    /// Regression: helpers start from threads that keep SIGCHLD blocked for
    /// the event loop's signalfd, and std hands that mask to the child, so
    /// the pairing agent and every daemon a launcher left behind ran with
    /// SIGCHLD blocked. Every spawn point whose child outlives the call
    /// unblocks it.
    #[test]
    fn helpers_start_with_sigchld_unblocked() {
        use test_support::{SigchldBlockedOnThisThread, SigchldProbe, status_blocks_sigchld};

        let _blocked = SigchldBlockedOnThisThread::new();
        let status_of = |output: Output| {
            assert!(output.status.success(), "{output:?}");
            String::from_utf8(output.stdout).expect("a UTF-8 status")
        };

        // Control: a plain spawn from this thread inherits the blocked mask,
        // so the assertions below are measuring the hook.
        let plain = Command::new("cat")
            .arg("/proc/self/status")
            .output()
            .expect("run cat");
        assert!(status_blocks_sigchld(&status_of(plain)));

        let launcher = daemon_launcher_output_with_limits(
            "cat",
            &["/proc/self/status"],
            Duration::from_secs(5),
            64 * 1024,
        )
        .expect("daemon launcher");
        assert!(!status_blocks_sigchld(&status_of(launcher)));

        let probe = SigchldProbe::new("selection-owner");
        let owner = selection_owner_output_with_input(
            "sh",
            &["-c", &probe.script()],
            Stdio::null(),
            Duration::from_secs(5),
            64 * 1024,
        )
        .expect("selection owner");
        assert!(owner.status.success(), "{owner:?}");
        assert!(!probe.child_blocked_sigchld());

        let probe = SigchldProbe::new("detached");
        let mut child =
            spawn_detached("sh", &["-c", &probe.script()], &[]).expect("detached spawn");
        assert!(child.wait().expect("reap the child we spawned").success());
        assert!(!probe.child_blocked_sigchld());
    }

    /// Regression: the SIGCHLD hook is a `pre_exec` closure, and std forks
    /// the whole compositor instead of using `posix_spawn` for any command
    /// that carries one. Every periodic poll (wpctl, nmcli, brightnessctl)
    /// went through a full fork once the hook was added to the one-shot
    /// runners. Those helpers are waited for and their group is killed
    /// afterwards, so they carry no hook and keep the mask they inherited —
    /// the observable sign that nothing pushed them off the cheap spawn.
    #[test]
    fn one_shot_helpers_carry_no_sigchld_hook() {
        use test_support::{SigchldBlockedOnThisThread, SigchldProbe, status_blocks_sigchld};

        let _blocked = SigchldBlockedOnThisThread::new();
        let status_of = |output: Output| {
            assert!(output.status.success(), "{output:?}");
            String::from_utf8(output.stdout).expect("a UTF-8 status")
        };

        let bounded = output_with_limits(
            "cat",
            &["/proc/self/status"],
            Duration::from_secs(5),
            64 * 1024,
        )
        .expect("bounded helper");
        assert!(status_blocks_sigchld(&status_of(bounded)));

        let fed = output_with_input_and_limits(
            "cat",
            &["/proc/self/status"],
            Stdio::null(),
            Duration::from_secs(5),
            64 * 1024,
        )
        .expect("stdin-fed helper");
        assert!(status_blocks_sigchld(&status_of(fed)));

        let probe = SigchldProbe::new("status");
        let status = status_with_timeout("sh", &["-c", &probe.script()], Duration::from_secs(5))
            .expect("status helper");
        assert!(status.success());
        assert!(probe.child_blocked_sigchld());
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
