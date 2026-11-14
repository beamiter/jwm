//! Best-effort SSH diagnostics must not backpressure the control reader.
//!
//! A supervised `cat` process owns blocking stderr writes. This process only
//! writes atomic, bounded records to its own nonblocking pipe. Unlike changing
//! fd 2's flags or reopening /proc/self/fd/2, this preserves redirected file
//! offsets, append behavior, and terminal flags. No output thread is detached.

use std::io::{self, Write};
use std::os::fd::{AsFd, AsRawFd};
#[cfg(target_os = "linux")]
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, TryLockError, Weak, mpsc};
use std::time::{Duration, Instant};

// Linux PIPE_BUF: one write is either accepted whole or returns WouldBlock.
const RECORD_BYTES: usize = libc::PIPE_BUF as usize;
const DRAIN_GRACE: Duration = Duration::from_millis(200);

pub(super) struct DiagnosticPump {
    #[cfg(test)]
    pid: u32,
    output: Arc<DiagnosticOutput>,
    stop: Option<mpsc::Sender<()>>,
    supervisor: Option<std::thread::JoinHandle<()>>,
}

pub(super) struct DiagnosticWriter {
    pipe: ChildStdin,
    dropped: u64,
}

// Session ownership is distinct from cheap output references: an in-flight
// diagnostic must never become responsible for joining the supervisor.
struct DiagnosticOutput {
    accepting: AtomicBool,
    writer: Mutex<Option<DiagnosticWriter>>,
}

struct Active {
    owner: Weak<DiagnosticPump>,
    output: Weak<DiagnosticOutput>,
}

static ACTIVE: Mutex<Active> = Mutex::new(Active {
    owner: Weak::new(),
    output: Weak::new(),
});

pub(super) fn emit(arguments: std::fmt::Arguments<'_>) {
    emit_impl(arguments, false);
}

// Telemetry historically ignores stderr errors, including in its Drop path.
pub(super) fn emit_best_effort(arguments: std::fmt::Arguments<'_>) {
    emit_impl(arguments, true);
}

fn emit_impl(arguments: std::fmt::Arguments<'_>, best_effort: bool) {
    let output = match ACTIVE.try_lock() {
        Ok(active) => active.output.upgrade(),
        Err(TryLockError::Poisoned(error)) => error.into_inner().output.upgrade(),
        Err(TryLockError::WouldBlock) => return,
    };
    if let Some(output) = output {
        if !output.accepting.load(Ordering::Acquire) {
            return;
        }
        let record = DiagnosticRecord::format(arguments);
        let mut writer = match output.writer.try_lock() {
            Ok(writer) => writer,
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
            Err(TryLockError::WouldBlock) => return,
        };
        if output.accepting.load(Ordering::Acquire)
            && let Some(writer) = writer.as_mut()
        {
            writer.emit(record.as_str());
        }
    } else if best_effort {
        let _ = writeln!(io::stderr().lock(), "{arguments}");
    } else {
        std::eprintln!("{arguments}");
    }
}

impl DiagnosticPump {
    pub(super) fn install() -> io::Result<Arc<Self>> {
        let mut active = ACTIVE
            .lock()
            .map_err(|_| io::Error::other("SSH diagnostic registry poisoned"))?;
        if let Some(pump) = active.owner.upgrade() {
            return Ok(pump);
        }
        let pump = Arc::new(Self::start()?);
        *active = Active {
            owner: Arc::downgrade(&pump),
            output: Arc::downgrade(&pump.output),
        };
        Ok(pump)
    }

    pub(super) fn start() -> io::Result<Self> {
        Self::start_to(
            "/bin/cat",
            Stdio::from(io::stderr().as_fd().try_clone_to_owned()?),
        )
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("could not start bounded SSH diagnostic helper (/bin/cat): {error}"),
            )
        })
    }

    pub(super) fn start_to(program: &str, output: Stdio) -> io::Result<Self> {
        let program = program.to_owned();
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (stop_tx, stop_rx) = mpsc::channel();
        // PDEATHSIG follows the creating thread on Linux. A dedicated owner
        // outlives every sharing SSH session, so one session thread ending must
        // not kill the helper used by another. This thread never writes logs.
        let supervisor = std::thread::Builder::new()
            .name("jwm-ssh-diagnostic-owner".into())
            .spawn(move || match spawn_helper(&program, output) {
                Ok((mut child, writer)) => {
                    let pid = child.id();
                    if started_tx.send(Ok((writer, pid))).is_ok() {
                        let _ = stop_rx.recv();
                    }
                    stop_helper(&mut child);
                }
                Err(error) => {
                    let _ = started_tx.send(Err(error));
                }
            })?;
        match started_rx.recv() {
            Ok(Ok((writer, _pid))) => Ok(Self {
                #[cfg(test)]
                pid: _pid,
                output: Arc::new(DiagnosticOutput {
                    accepting: AtomicBool::new(true),
                    writer: Mutex::new(Some(writer)),
                }),
                stop: Some(stop_tx),
                supervisor: Some(supervisor),
            }),
            result => {
                drop(stop_tx);
                let _ = supervisor.join();
                match result {
                    Ok(Err(error)) => Err(error),
                    _ => Err(io::Error::other(
                        "diagnostic supervisor stopped during startup",
                    )),
                }
            }
        }
    }

    #[cfg(test)]
    pub(super) fn writer(&mut self) -> &mut DiagnosticWriter {
        Arc::get_mut(&mut self.output)
            .expect("isolated test output")
            .writer
            .get_mut()
            .unwrap()
            .as_mut()
            .expect("diagnostic writer not taken")
    }

    #[cfg(test)]
    pub(super) fn take_writer(&mut self) -> DiagnosticWriter {
        Arc::get_mut(&mut self.output)
            .expect("isolated test output")
            .writer
            .get_mut()
            .unwrap()
            .take()
            .expect("diagnostic writer taken once")
    }
}

fn spawn_helper(program: &str, output: Stdio) -> io::Result<(Child, DiagnosticWriter)> {
    #[cfg(target_os = "linux")]
    let parent = std::process::id() as libc::pid_t;
    let mut command = Command::new(program);
    command
        .stdin(Stdio::piped())
        .stdout(output)
        .stderr(Stdio::null());
    // SAFETY: this post-fork callback only uses async-signal-safe system calls.
    // No allocations, locks, or Rust I/O occur before exec.
    #[cfg(target_os = "linux")]
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() != parent {
                libc::_exit(1);
            }
            Ok(())
        });
    }
    #[cfg(not(target_os = "linux"))]
    return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "bounded SSH diagnostics require Linux",
    ));
    let mut child = command.spawn()?;
    let setup = (|| {
        let pipe = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("diagnostic helper stdin was not captured"))?;
        // SAFETY: pipe owns this fresh descriptor for both fcntl calls. It is
        // our private write end, not stderr or a duplicate of stderr's OFD.
        let flags = unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_GETFL) };
        if flags == -1
            || unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                == -1
        {
            return Err(io::Error::last_os_error());
        }
        Ok(DiagnosticWriter { pipe, dropped: 0 })
    })();
    match setup {
        Ok(writer) => Ok((child, writer)),
        Err(error) => {
            drop(child.stdin.take());
            stop_helper(&mut child);
            Err(error)
        }
    }
}

fn stop_helper(child: &mut Child) {
    let deadline = Instant::now() + DRAIN_GRACE;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Err(_) => break,
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    // Linux uninterruptible filesystem I/O can delay reaping; there is no hard
    // kernel D-state bound. No potentially blocked log-writing thread remains.
}

impl Drop for DiagnosticPump {
    fn drop(&mut self) {
        self.output.accepting.store(false, Ordering::Release);
        drop(
            self.output
                .writer
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take(),
        );
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(supervisor) = self.supervisor.take() {
            let _ = supervisor.join();
        }
    }
}

struct DiagnosticRecord {
    bytes: [u8; RECORD_BYTES],
    length: usize,
}

const FORMAT_SUFFIX: &str = "... [truncated]";

impl std::fmt::Write for DiagnosticRecord {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        let available = RECORD_BYTES - 1 - FORMAT_SUFFIX.len() - self.length;
        let mut count = text.len().min(available);
        while !text.is_char_boundary(count) {
            count -= 1;
        }
        self.bytes[self.length..self.length + count].copy_from_slice(&text.as_bytes()[..count]);
        self.length += count;
        if count < text.len() {
            Err(std::fmt::Error)
        } else {
            Ok(())
        }
    }
}

impl DiagnosticRecord {
    fn format(arguments: std::fmt::Arguments<'_>) -> Self {
        let mut text = Self {
            bytes: [0; RECORD_BYTES],
            length: 0,
        };
        if std::fmt::write(&mut text, arguments).is_err() {
            text.bytes[text.length..text.length + FORMAT_SUFFIX.len()]
                .copy_from_slice(FORMAT_SUFFIX.as_bytes());
            text.length += FORMAT_SUFFIX.len();
        }
        text
    }

    fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..self.length]).expect("complete UTF-8 slices")
    }
}

impl DiagnosticWriter {
    /// Best effort: log loss must never delay protocol progress or shutdown.
    pub(super) fn emit(&mut self, message: &str) {
        if self.dropped != 0 {
            let summary = format!(
                "jwm-remote: dropped {} diagnostic line(s) while output was unavailable",
                self.dropped
            );
            if !self.write_record(&summary) {
                self.dropped = self.dropped.saturating_add(1);
                return;
            }
            self.dropped = 0;
        }
        if !self.write_record(message) {
            self.dropped = self.dropped.saturating_add(1);
        }
    }

    fn write_record(&mut self, message: &str) -> bool {
        let mut record = [0u8; RECORD_BYTES];
        let suffix = b"... [truncated]\n";
        let length = if message.len() < RECORD_BYTES {
            record[..message.len()].copy_from_slice(message.as_bytes());
            record[message.len()] = b'\n';
            message.len() + 1
        } else {
            let mut length = RECORD_BYTES - suffix.len();
            while !message.is_char_boundary(length) {
                length -= 1;
            }
            record[..length].copy_from_slice(&message.as_bytes()[..length]);
            record[length..length + suffix.len()].copy_from_slice(suffix);
            length + suffix.len()
        };
        matches!(self.pipe.write(&record[..length]), Ok(count) if count == length)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{File, OpenOptions};
    use std::io::{BufRead, Read, Seek, SeekFrom};
    use std::os::unix::net::UnixStream;

    fn wait_file(path: &std::path::Path, expected: &[u8]) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while std::fs::read(path).unwrap() != expected && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(std::fs::read(path).unwrap(), expected);
    }

    fn wait_owner(child: &mut Child) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "diagnostic owner failed: {status}");
                return;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                child.wait().unwrap();
                panic!("diagnostic owner did not exit");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn global_sink_survives_creator_session_thread_and_shares_one_owner() {
        const CHILD: &str = "JWM_TEST_SHARED_DIAGNOSTIC_OWNER";
        if std::env::var_os(CHILD).is_some() {
            // The first session's thread ends before the retained session: the
            // helper must belong to its supervisor, not the departed thread.
            let first = std::thread::spawn(|| DiagnosticPump::install().unwrap())
                .join()
                .unwrap();
            let second = DiagnosticPump::install().unwrap();
            assert!(Arc::ptr_eq(&first, &second));
            let pid = first.pid;
            std::thread::scope(|scope| {
                for _ in 0..8 {
                    let expected = &first;
                    scope.spawn(move || {
                        let session = DiagnosticPump::install().unwrap();
                        assert!(Arc::ptr_eq(expected, &session));
                        for index in 0..1000 {
                            eprintln!("parallel diagnostic {index}");
                        }
                    });
                }
            });
            for _ in 0..10_000 {
                crate::remote::client::tests::emit_telemetry_for_diagnostic_regression();
            }
            drop(first);
            // Check state without reaping a child owned by the supervisor.
            let state = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
            assert!(!state.split_once(") ").unwrap().1.starts_with('Z'));
            for index in 0..10_000 {
                eprintln!("retained session {index}");
            }
            drop(second);
            assert_reaped(pid);
            assert!(ACTIVE.lock().unwrap().owner.upgrade().is_none());

            let session = DiagnosticPump::install().unwrap();
            let pid = session.pid;
            let output = Arc::clone(&session.output);
            let (entered_tx, entered_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            struct Paused {
                entered: mpsc::Sender<()>,
                release: mpsc::Receiver<()>,
            }
            impl std::fmt::Display for Paused {
                fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    self.entered.send(()).unwrap();
                    self.release.recv().unwrap();
                    formatter.write_str("in-flight diagnostic")
                }
            }
            let emitter = std::thread::spawn(move || {
                emit(format_args!(
                    "{}",
                    Paused {
                        entered: entered_tx,
                        release: release_rx
                    }
                ));
            });
            entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            let (closed_tx, closed_rx) = mpsc::channel();
            let closer = std::thread::spawn(move || {
                drop(session);
                closed_tx.send(()).unwrap();
            });
            // Cleanup must finish while formatting is paused: neither output
            // references nor formatter locks may own the process lifecycle.
            let closed_before_formatter = closed_rx.recv_timeout(Duration::from_secs(3)).is_ok();
            release_tx.send(()).unwrap();
            emitter.join().unwrap();
            closer.join().unwrap();
            assert!(closed_before_formatter, "formatter delayed helper cleanup");
            assert!(output.writer.lock().unwrap().is_none());
            assert_reaped(pid); // lightweight output refs cannot defer child cleanup
            return;
        }
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "remote::ssh_diagnostics::tests::global_sink_survives_creator_session_thread_and_shares_one_owner", "--nocapture"])
            .env(CHILD, "1")
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped())
            .spawn().unwrap();
        // Deliberately never drain stderr. Test control/readiness and cleanup
        // must still finish; the scoped owner, not a detached thread, reaps cat.
        wait_owner(&mut child);
    }

    #[test]
    fn best_effort_telemetry_does_not_panic_without_ssh() {
        const CHILD: &str = "JWM_TEST_TELEMETRY_BROKEN_STDERR";
        if std::env::var_os(CHILD).is_some() {
            assert!(ACTIVE.lock().unwrap().owner.upgrade().is_none());
            crate::remote::client::tests::emit_telemetry_for_diagnostic_regression();
            return;
        }
        let (reader, output) = UnixStream::pair().unwrap();
        drop(reader);
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "remote::ssh_diagnostics::tests::best_effort_telemetry_does_not_panic_without_ssh",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(std::os::fd::OwnedFd::from(output)))
            .spawn()
            .unwrap();
        wait_owner(&mut child);
    }

    #[test]
    fn formatted_records_bound_padding_and_utf8_without_full_allocation() {
        let padded = DiagnosticRecord::format(format_args!("{:0width$}", 1, width = 60_000));
        assert!(padded.as_str().len() < RECORD_BYTES);
        assert!(padded.as_str().ends_with("... [truncated]"));
        let unicode = "测".repeat(RECORD_BYTES);
        let record = DiagnosticRecord::format(format_args!("{unicode}"));
        assert!(record.as_str().len() < RECORD_BYTES);
        assert!(record.as_str().ends_with("... [truncated]"));
    }

    #[test]
    fn owner_exit_without_drop_terminates_blocked_helper() {
        const CHILD: &str = "JWM_TEST_ABRUPT_DIAGNOSTIC_OWNER";
        if std::env::var_os(CHILD).is_some() {
            let session = DiagnosticPump::install().unwrap();
            let pid = session.pid;
            for index in 0..100_000 {
                eprintln!("blocked output {index}");
            }
            println!("PUMP_PID={pid}");
            // No Rust destructors: exercises PDEATHSIG rather than Drop cleanup.
            std::process::exit(0);
        }
        let (reader, output) = UnixStream::pair().unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "remote::ssh_diagnostics::tests::owner_exit_without_drop_terminates_blocked_helper",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(std::os::fd::OwnedFd::from(output)))
            .spawn()
            .unwrap();
        wait_owner(&mut child);
        let mut text = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        let pid: u32 = text
            .lines()
            .find_map(|line| line.strip_prefix("PUMP_PID="))
            .unwrap()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => break,
                Ok(state) if state.split_once(") ").unwrap().1.starts_with('Z') => break,
                _ if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
                _ => panic!("helper survived owner process exit"),
            }
        }
        // Retain the sink until the exit check, otherwise closing it could
        // unblock cat independently of the parent-death signal.
        drop(reader);
    }

    fn assert_reaped(pid: u32) {
        assert_eq!(
            nix::sys::wait::waitpid(
                nix::unistd::Pid::from_raw(pid as i32),
                Some(nix::sys::wait::WaitPidFlag::WNOHANG),
            ),
            Err(nix::errno::Errno::ECHILD)
        );
    }

    #[test]
    fn blocked_output_drops_records_and_helper_is_reaped() {
        for _ in 0..3 {
            let (_reader, output) = UnixStream::pair().unwrap();
            let flags = unsafe { libc::fcntl(output.as_raw_fd(), libc::F_GETFL) };
            let shared = output.try_clone().unwrap();
            let mut pump = DiagnosticPump::start_to(
                "/bin/cat",
                Stdio::from(std::os::fd::OwnedFd::from(output)),
            )
            .unwrap();
            let pid = pump.pid;
            let start = Instant::now();
            for _ in 0..10_000 {
                pump.writer().emit(&"x".repeat(RECORD_BYTES));
            }
            assert!(pump.writer().dropped > 0);
            assert_eq!(
                unsafe { libc::fcntl(shared.as_raw_fd(), libc::F_GETFL) },
                flags
            );
            drop(pump);
            assert!(start.elapsed() < Duration::from_secs(3));
            assert_reaped(pid);
        }
    }

    #[test]
    fn output_preserves_shared_file_position_and_append_semantics() {
        let path = std::env::temp_dir().join(format!(
            "jwm-ssh-diagnostics-{:016x}",
            rand::random::<u64>()
        ));
        std::fs::write(&path, b"prefix----").unwrap();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        file.seek(SeekFrom::Start(6)).unwrap();
        let mut pump =
            DiagnosticPump::start_to("/bin/cat", Stdio::from(file.try_clone().unwrap())).unwrap();
        pump.writer().emit("abcd");
        wait_file(&path, b"prefixabcd\n");
        drop(pump);
        assert_eq!(file.stream_position().unwrap(), 11);
        assert_eq!(std::fs::read(&path).unwrap(), b"prefixabcd\n");
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut pump = DiagnosticPump::start_to("/bin/cat", Stdio::from(file)).unwrap();
        pump.writer().emit("end");
        wait_file(&path, b"prefixabcd\nend\n");
        drop(pump);
        assert_eq!(std::fs::read(&path).unwrap(), b"prefixabcd\nend\n");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn long_utf8_diagnostics_are_atomic_and_visibly_truncated() {
        let (mut reader, output) = UnixStream::pair().unwrap();
        let mut pump =
            DiagnosticPump::start_to("/bin/cat", Stdio::from(std::os::fd::OwnedFd::from(output)))
                .unwrap();
        pump.writer().emit(&"测".repeat(RECORD_BYTES));
        reader
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut bytes = Vec::new();
        std::io::BufReader::new(&mut reader)
            .read_until(b'\n', &mut bytes)
            .unwrap();
        drop(pump);
        assert!(bytes.len() <= RECORD_BYTES);
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(text.ends_with("... [truncated]\n"));
    }

    #[test]
    fn resumed_output_reports_saturated_records_and_continues() {
        let path =
            std::env::temp_dir().join(format!("jwm-ssh-resume-{:016x}", rand::random::<u64>()));
        let mut pump =
            DiagnosticPump::start_to("/bin/cat", Stdio::from(File::create(&path).unwrap()))
                .unwrap();
        let pid = nix::unistd::Pid::from_raw(pump.pid as i32);
        nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGSTOP).unwrap();
        // Wait for the stop before filling the pipe; the pump remains owned and
        // its Drop kills/reaps even if a later assertion fails while stopped.
        nix::sys::wait::waitpid(pid, Some(nix::sys::wait::WaitPidFlag::WUNTRACED)).unwrap();
        for _ in 0..10_000 {
            pump.writer().emit("before resume");
        }
        assert!(pump.writer().dropped > 0);
        nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGCONT).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut reported = 0;
        while pump.writer().dropped != 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
            reported = pump.writer().dropped;
            pump.writer().emit("after resume");
        }
        assert_eq!(pump.writer().dropped, 0);
        drop(pump);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(&format!(
            "dropped {reported} diagnostic line(s) while output was unavailable"
        )));
        assert!(text.contains("after resume\n"));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn closed_output_and_missing_helper_are_safe() {
        assert!(
            DiagnosticPump::start_to("/nonexistent/jwm-diagnostic-helper", Stdio::null()).is_err()
        );
        let (reader, output) = UnixStream::pair().unwrap();
        drop(reader);
        let mut pump =
            DiagnosticPump::start_to("/bin/cat", Stdio::from(std::os::fd::OwnedFd::from(output)))
                .unwrap();
        let pid = pump.pid;
        for _ in 0..100 {
            pump.writer().emit("closed output");
        }
        drop(pump);
        assert_reaped(pid);
    }

    #[test]
    fn dropping_unused_pump_closes_stdin_and_reaps() {
        let pump =
            DiagnosticPump::start_to("/bin/cat", Stdio::from(File::create("/dev/null").unwrap()))
                .unwrap();
        let pid = pump.pid;
        drop(pump);
        assert_reaped(pid);
    }
}
