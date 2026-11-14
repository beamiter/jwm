//! `jwm-remote ssh`: control another JWM desktop with one command.
//!
//! Sharing a desktop otherwise takes three manual steps on two machines: copy
//! a key file across, log in to the other machine and start a host with the
//! right `DISPLAY` and flags, then connect. Anyone who can already SSH to that
//! machine as its user can do all of that, so this command does it for them:
//!
//! - it generates a fresh key for this one session and hands it to the host
//!   over the SSH channel that starts it, so no key file is copied or kept;
//! - the host listens on the remote loopback only, reached through an SSH
//!   local forward, so nothing is opened on the LAN and the traffic gains
//!   SSH's forward secrecy on top of the session's own encryption;
//! - the host finds the display of the user's running JWM session itself;
//! - the host stops when the SSH channel closes, so closing or killing the
//!   viewer leaves nothing running on the other machine.
//!
//! Control is not a new grant: the SSH login can already run anything as
//! that user, including a host with `--allow-input`.

use super::RemoteResult;
use super::client::{ClientOptions, run_client};
use super::clipboard_policy::{ClipboardDirection, ClipboardMode};
use super::key::{KeySource, generate_session_key};
use super::messages::FrameLimits;
use super::scaling::ScalingPolicy;
use super::ssh_diagnostics::DiagnosticPump;
use super::x11_capture::CaptureArea;
use super::x11_viewer::{close_existing_viewer, focus_existing_viewer};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::fd::{AsFd, AsRawFd};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

/// Long enough to type an SSH password or confirm a new host key.
const HOST_START_TIMEOUT: Duration = Duration::from_secs(120);
/// How long the remote host gets to say goodbye after the viewer closes.
const HOST_STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// Where a managed release bundle keeps the helper, which a non-interactive
/// SSH shell's `PATH` usually does not include.
const MANAGED_REMOTE_COMMAND: &str = "/usr/local/lib/jwm/current/bin/jwm-remote";
/// Remote loopback ports are drawn from the dynamic range.
const REMOTE_PORT_BASE: u16 = 49152;
const REMOTE_PORT_SPAN: u16 = 16000;

#[derive(Clone, Debug)]
pub struct SshOptions {
    /// Explicit local client-area startup size; None preserves automatic sizing.
    pub window_size: Option<super::window_size::WindowSize>,
    /// Explicit local best-effort startup position; coordinates are bounded by i16.
    pub window_position: Option<super::window_position::WindowPosition>,
    /// `[USER@]HOST` or an alias from the SSH configuration.
    pub destination: String,
    /// Extra `ssh -o` options, passed through unchanged.
    pub ssh_options: Vec<String>,
    /// Path of `jwm-remote` on the other machine; found automatically if unset.
    pub remote_command: Option<String>,
    /// Share one RandR monitor of the other machine.
    pub monitor: Option<String>,
    /// Explicit additional host grants, fixed for the session.
    pub shared_sources: Vec<CaptureArea>,
    pub fps: Option<u16>,
    pub jpeg_quality: Option<u8>,
    pub max_width: Option<u16>,
    pub frame_limits: FrameLimits,
    /// Local X11 display for the viewer window.
    pub display: Option<String>,
    pub view_only: bool,
    pub grab_input: bool,
    pub seamless_input: bool,
    pub clipboard: bool,
    pub clipboard_mode: ClipboardMode,
    pub clipboard_direction: ClipboardDirection,
    /// Largest file copy shared through the clipboard; 0 shares no files.
    pub clipboard_file_limit: u64,
    pub escape_key: String,
    pub scaling: ScalingPolicy,
}

pub fn run_ssh(options: SshOptions) -> RemoteResult<()> {
    if options
        .shared_sources
        .iter()
        .any(|area| matches!(area, CaptureArea::Window(_)))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "window instance grants must be authorized by a local host --window command",
        )
        .into());
    }
    if let Some(size) = options.window_size {
        size.validate()?;
    }
    options.frame_limits.validate()?;
    validate_destination(&options.destination)?;
    // Reject local input errors before creating a session slot or starting SSH.
    super::viewer_backend::ViewerBackend::X11.validate_seamless_input(
        options.seamless_input,
        options.grab_input,
        options.view_only,
    )?;
    super::x11_viewer::validate_escape_key(&options.escape_key)?;
    let _diagnostics = DiagnosticPump::install()?;
    let label = peer_label(&options);
    let Some(_session_lock) = claim_session(&options, &label)? else {
        return Ok(());
    };
    let (key, printable_key) = generate_session_key();
    let local_port = free_local_port()?;
    let remote_port = REMOTE_PORT_BASE + rand::random::<u16>() % REMOTE_PORT_SPAN;
    let remote_script = remote_host_script(&options, remote_port);

    eprintln!(
        "jwm-remote: starting a host on {} over SSH",
        options.destination
    );
    let mut child = Command::new("ssh")
        .args(ssh_arguments(
            &options,
            local_port,
            remote_port,
            &remote_script,
        ))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                io::Error::new(
                    error.kind(),
                    "the ssh command was not found; install an OpenSSH client",
                )
            } else {
                error
            }
        })?;
    let mut stdin = child.stdin.take();
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("ssh standard error was not captured"))?;
    let (ready_tx, ready_rx) = mpsc::channel();
    let relay_stop = Arc::new(AtomicBool::new(false));
    let reader = RelayReader::new(stderr, Arc::clone(&relay_stop));
    let relay = thread::Builder::new()
        .name("jwm-remote-ssh-relay".into())
        .spawn(move || {
            relay_host_messages_with_output(BufReader::new(reader), &ready_tx, |line| {
                eprintln!("{line}")
            });
        });
    let result = match relay {
        Ok(_) => wait_until_ready(
            &options.destination,
            &mut child,
            stdin.as_mut(),
            &printable_key,
            &ready_rx,
        )
        .and_then(|()| {
            run_client(ClientOptions {
                control_duration: Default::default(),
                viewer_backend: crate::remote::viewer_backend::ViewerBackend::X11,
                viewer_wayland_display: None,
                wayland_keyboard: false,
                wayland_pointer: false,
                wayland_control: false,
                resumable_control: false,
                wayland_seamless_input: false,
                wayland_text: None,
                static_lossless: false,
                wayland_scroll: false,
                wayland_horizontal_scroll: false,
                wayland_popup_pointer: false,
                resize_source: None,
                follow_source_size: false,
                window_size: options.window_size,
                window_position: options.window_position,
                address: format!("127.0.0.1:{local_port}"),
                peer_label: label.clone(),
                key: KeySource::Bytes(key),
                display: options.display.clone(),
                view_only: options.view_only,
                grab_input: options.grab_input,
                seamless_input: options.seamless_input,
                clipboard: options.clipboard,
                clipboard_mode: options.clipboard_mode,
                clipboard_direction: options.clipboard_direction,
                clipboard_file_limit: options.clipboard_file_limit,
                frame_limits: options.frame_limits,
                escape_key: options.escape_key.clone(),
                scaling: options.scaling,
            })
        }),
        Err(ref error) => Err(io::Error::new(error.kind(), error.to_string()).into()),
    };
    // Closing stdin is the host's signal to stop; SSH is ended regardless
    // after a grace period, so no session outlives this command.
    drop(stdin);
    stop_child(&mut child);
    // A surviving ProxyCommand/wrapper child can inherit the diagnostic pipe;
    // do not require every such process to close it before the reader can stop.
    relay_stop.store(true, Ordering::Release);
    if let Ok(relay) = relay {
        let _ = relay.join();
    }
    result
}

/// Hand the host its key, then wait until it listens or SSH gives up.
fn wait_until_ready(
    destination: &str,
    child: &mut Child,
    stdin: Option<&mut std::process::ChildStdin>,
    printable_key: &str,
    ready: &mpsc::Receiver<()>,
) -> RemoteResult<()> {
    if let Some(stdin) = stdin {
        // A write fails only when SSH already exited; the wait below reports
        // that with SSH's own explanation.
        let _ = stdin
            .write_all(format!("{printable_key}\n").as_bytes())
            .and_then(|()| stdin.flush());
    }
    let deadline = Instant::now() + HOST_START_TIMEOUT;
    loop {
        match ready.recv_timeout(Duration::from_millis(100)) {
            Ok(()) => return Ok(()),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // EOF is not proof that SSH exited: a wrapper can close stderr
                // and remain alive. Never wait here without a deadline. Return
                // to run_ssh so its bounded shutdown path can reap the child.
                let error = match child.try_wait()? {
                    Some(status) => ssh_ended_early(status, destination),
                    None => io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "ssh closed its diagnostic stream before the remote host was ready",
                    ),
                };
                return Err(error.into());
            }
        }
        if let Some(status) = child.try_wait()? {
            return Err(ssh_ended_early(status, destination).into());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the remote host did not start within two minutes",
            )
            .into());
        }
    }
}

fn ssh_ended_early(status: ExitStatus, destination: &str) -> io::Error {
    let why = match status.code() {
        // SSH passes the remote exit code through, and 2 is what the helper's
        // argument parser exits with when it does not know an option.
        Some(2) => format!(
            "the jwm-remote on {destination} did not accept the host options; it is probably \
             older than this one, so install the same version on both machines"
        ),
        Some(255) => format!("ssh could not connect or log in to {destination}"),
        _ => format!("ssh ended before the remote host was ready ({status})"),
    };
    io::Error::new(
        io::ErrorKind::ConnectionAborted,
        format!("{why}; the lines above say more"),
    )
}

/// Give the host time to stop after its stdin closed, then end SSH.
fn stop_child(child: &mut Child) {
    let deadline = Instant::now() + HOST_STOP_TIMEOUT;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => thread::sleep(Duration::from_millis(50)),
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// A relay owns its read descriptor exclusively. Polling before each read lets
/// shutdown interrupt an idle pipe even when a descendant inherited its writer.
/// This bounds read-side cancellation only; diagnostic output may still block.
struct RelayReader<R> {
    input: R,
    stop: Arc<AtomicBool>,
    drain_remaining: usize,
}

impl<R> RelayReader<R> {
    fn new(input: R, stop: Arc<AtomicBool>) -> Self {
        Self {
            input,
            stop,
            drain_remaining: 4 * MAX_HOST_LINE_BYTES,
        }
    }

    fn stopped() -> io::Error {
        // Not EOF: do not emit an incomplete diagnostic on cancellation.
        io::Error::new(
            io::ErrorKind::ConnectionAborted,
            "SSH diagnostic relay stopped",
        )
    }
}

impl<R: Read + AsFd> Read for RelayReader<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        loop {
            let stopping = self.stop.load(Ordering::Acquire);
            if stopping && self.drain_remaining == 0 {
                return Err(Self::stopped());
            }
            let mut descriptors = [nix::poll::PollFd::new(
                self.input.as_fd(),
                nix::poll::PollFlags::POLLIN,
            )];
            // Preserve already queued final errors without waiting for new data.
            // A bounded drain also handles descendants that keep writing forever.
            let timeout: u16 = if stopping { 0 } else { 100 };
            match nix::poll::poll(&mut descriptors, timeout) {
                Ok(0) if stopping => return Err(Self::stopped()),
                Ok(0) | Err(nix::errno::Errno::EINTR) => continue,
                Err(error) => return Err(io::Error::from_raw_os_error(error as i32)),
                Ok(_) => {}
            }
            if descriptors[0]
                .revents()
                .is_some_and(|events| events.contains(nix::poll::PollFlags::POLLNVAL))
            {
                return Err(io::Error::from_raw_os_error(libc::EBADF));
            }
            let stopping = self.stop.load(Ordering::Acquire);
            let length = if stopping {
                output.len().min(self.drain_remaining)
            } else {
                output.len()
            };
            if length == 0 {
                return Err(Self::stopped());
            }
            // This is the sole reader, so another consumer cannot steal the
            // readiness between poll and read. HUP/ERR are resolved by read.
            let count = self.input.read(&mut output[..length])?;
            if stopping {
                self.drain_remaining -= count;
            }
            return Ok(count);
        }
    }
}

/// Keep SSH/host diagnostics bounded even when a peer never writes a newline.
/// Oversized lines are discarded whole, so a truncated prefix cannot signal
/// readiness. Continue draining the stream so SSH cannot block on a full pipe.
const MAX_HOST_LINE_BYTES: usize = 16 * 1024;

fn read_host_line(reader: &mut impl BufRead, line: &mut Vec<u8>) -> io::Result<bool> {
    line.clear();
    let mut oversized = false;
    loop {
        let chunk = match reader.fill_buf() {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if chunk.is_empty() {
            return Ok(!oversized && !line.is_empty());
        }
        let newline = chunk.iter().position(|&byte| byte == b'\n');
        let length = newline.unwrap_or(chunk.len());
        if !oversized {
            if length > MAX_HOST_LINE_BYTES - line.len() {
                oversized = true;
                line.clear();
            } else {
                line.extend_from_slice(&chunk[..length]);
            }
        }
        reader.consume(length + usize::from(newline.is_some()));
        if newline.is_some() {
            if oversized {
                oversized = false;
                continue;
            }
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(true);
        }
    }
}

/// Relay diagnostics, emitting at most one readiness notification per session.
fn relay_host_messages_with_output(
    mut reader: impl BufRead,
    ready: &mpsc::Sender<()>,
    mut emit: impl FnMut(&str),
) {
    let mut line = Vec::with_capacity(MAX_HOST_LINE_BYTES);
    let mut signalled = false;
    while matches!(read_host_line(&mut reader, &mut line), Ok(true)) {
        let Ok(line) = std::str::from_utf8(&line) else {
            break;
        };
        match classify_host_line(line) {
            HostLine::Listening => {
                if !signalled {
                    let _ = ready.send(());
                    signalled = true;
                }
            }
            HostLine::Telemetry => {}
            HostLine::Host(message) => emit(&format!("jwm-remote: host: {message}")),
            // SSH's own messages already say "ssh:"; anything else is the
            // remote program speaking without the helper's prefix.
            HostLine::Other => emit(line),
        }
    }
}

#[cfg(test)]
fn relay_host_messages(reader: impl BufRead, ready: &mpsc::Sender<()>) {
    relay_host_messages_with_output(reader, ready, |_| {});
}

#[derive(Debug, PartialEq, Eq)]
enum HostLine<'a> {
    /// The host is ready; its internal loopback port is not worth showing.
    Listening,
    /// Periodic pipeline counters, useful when tuning, noise otherwise.
    Telemetry,
    Host(&'a str),
    /// SSH itself, or the remote program before it is the helper.
    Other,
}

fn classify_host_line(line: &str) -> HostLine<'_> {
    let Some(message) = line.strip_prefix("jwm-remote: ") else {
        return HostLine::Other;
    };
    if message.starts_with("listening on ") {
        HostLine::Listening
    } else if message.starts_with("host ") && message.contains(" mode ") {
        HostLine::Telemetry
    } else {
        HostLine::Host(message)
    }
}

/// How long a switch waits for the previous monitor's session to close: the
/// viewer says goodbye, its host stops, and SSH is ended after at most
/// `HOST_STOP_TIMEOUT`.
const SWITCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Take the machine's session slot, or settle the request without one.
///
/// One host captures a screen at a time, so a machine has one session. The
/// same monitor and policy means "take me back there"; another monitor with
/// identical policy means "switch", which closes the open viewer first. A
/// different immutable policy is rejected until the prior session is closed. `None` means
/// the request was settled without starting a session.
fn claim_session(options: &SshOptions, label: &str) -> RemoteResult<Option<SessionLock>> {
    let mut lock = match SessionLock::acquire(&options.destination)? {
        Ok(lock) => lock,
        Err(holder) => match dispatch_held_session(
            &holder,
            label,
            || focus_existing_viewer(options.display.as_deref(), label),
            || switch_from(options, &holder, label),
        )? {
            Some(lock) => lock,
            None => return Ok(None),
        },
    };
    lock.record(label)?;
    Ok(Some(lock))
}

/// Validate immutable policy before either callback can touch an X window.
fn dispatch_held_session<T>(
    holder: &str,
    label: &str,
    focus: impl FnOnce() -> RemoteResult<bool>,
    switch: impl FnOnce() -> RemoteResult<Option<T>>,
) -> RemoteResult<Option<T>> {
    if !same_session_policy(holder, label) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "the existing SSH session has a different or legacy connection identity or permission/input policy; close it explicitly before starting this session",
        ).into());
    }
    if holder == label {
        if focus()? {
            eprintln!("jwm-remote: {label} is already open; brought it to the front");
        } else {
            eprintln!("jwm-remote: a connection to {label} is already starting");
        }
        Ok(None)
    } else {
        switch()
    }
}

/// Close the session showing `holder` and take its slot once it has gone.
fn switch_from(
    options: &SshOptions,
    holder: &str,
    label: &str,
) -> RemoteResult<Option<SessionLock>> {
    // An empty holder has not named its monitor yet, and a holder without a
    // window is still connecting: neither can be closed cleanly yet.
    if holder.is_empty() || !close_existing_viewer(options.display.as_deref(), holder)? {
        let starting = if holder.is_empty() {
            options.destination.as_str()
        } else {
            holder
        };
        eprintln!(
            "jwm-remote: a connection to {starting} is still starting; try again once it is open"
        );
        return Ok(None);
    }
    eprintln!("jwm-remote: switching from {holder} to {label}");
    let lock = SessionLock::wait(&options.destination, SWITCH_TIMEOUT)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!("{holder} did not close within ten seconds"),
        )
    })?;
    Ok(Some(lock))
}

/// An exclusive per-machine lock held for as long as one session runs. Its
/// file names the monitor being shown, for a request to switch away from.
struct SessionLock {
    file: File,
}

impl SessionLock {
    /// The lock, or the label of the session that holds it.
    fn acquire(destination: &str) -> io::Result<Result<Self, String>> {
        let mut file = super::ssh_session_file::open(&session_lock_path(destination))?;
        // SAFETY: flock only reads the descriptor, which `file` keeps open.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(Ok(Self { file }));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::WouldBlock {
            return Err(error);
        }
        let mut holder = String::new();
        std::io::Read::read_to_string(&mut file, &mut holder)?;
        Ok(Err(holder.trim().to_owned()))
    }

    /// Wait for the holder to finish, up to `timeout`.
    fn wait(destination: &str, timeout: Duration) -> io::Result<Option<Self>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(lock) = Self::acquire(destination)? {
                return Ok(Some(lock));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    /// Name the session holding the lock.
    fn record(&mut self, label: &str) -> io::Result<()> {
        use std::io::Seek as _;
        self.file.set_len(0)?;
        self.file.seek(io::SeekFrom::Start(0))?;
        self.file.write_all(label.as_bytes())?;
        self.file.flush()
    }
}

/// A per-user lock file named after the machine. The kernel drops the lock
/// with the process, so a crashed session never blocks the next one.
fn session_lock_path(destination: &str) -> PathBuf {
    let directory = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            // SAFETY: getuid has no preconditions and cannot fail.
            std::env::temp_dir().join(format!("jwm-remote-{}", unsafe { libc::getuid() }))
        });
    let name: String = destination
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "@.-_".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    directory.join(format!("jwm-remote-ssh-{name}.lock"))
}

fn peer_label(options: &SshOptions) -> String {
    let initial = match &options.monitor {
        Some(monitor) => format!("{} ({monitor})", options.destination),
        None => options.destination.clone(),
    };
    // Never focus an older session with a different permission scope. Always
    // include the policy fingerprint, including disabled clipboard, so legacy
    // labels without a policy cannot accidentally reuse a broader session.
    // Preserve exact connection arguments and their order: the legacy lock name
    // can collide, and SSH overrides can target a different host or account.
    // This is argument identity, not a resolution of SSH config or DNS.
    // Local input behavior is immutable too: focusing an existing window cannot
    // enable/disable seamless handoff, change grabs, or update its escape key.
    let (content, direction) = super::clipboard_policy::requested(
        options.clipboard,
        options.clipboard_mode,
        options.clipboard_direction,
        options.clipboard_file_limit,
    );
    let file_limit = if content & 4 != 0 {
        options.clipboard_file_limit
    } else {
        0
    };
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(
        format!(
            "session-v2;connection={:?};{:?};clipboard={content},{direction},{file_limit};view_only={};frames={:?};local_input={:?}",
            (&options.destination, &options.ssh_options, &options.remote_command),
            options.shared_sources,
            options.view_only,
            options.frame_limits,
            (options.grab_input, options.seamless_input, &options.escape_key)
        )
        .as_bytes(),
    );
    let fingerprint: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{initial} [policy-v2 {fingerprint}]")
}

// A monitor change can reuse the machine slot only after checking identical
// immutable policy. Legacy labels have no policy and fail closed.
fn same_session_policy(left: &str, right: &str) -> bool {
    fn fingerprint(label: &str) -> Option<&str> {
        let (_, suffix) = label.rsplit_once(" [policy-v2 ")?;
        let hash = suffix.strip_suffix(']')?;
        (hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some(hash)
    }
    matches!((fingerprint(left), fingerprint(right)), (Some(a), Some(b)) if a == b)
}

/// An SSH destination as accepted here: no option-like or multi-word values,
/// so it can only ever be read by `ssh` as the destination.
pub fn validate_destination(destination: &str) -> io::Result<()> {
    if destination.is_empty()
        || destination.starts_with('-')
        || destination
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "use [USER@]HOST or an SSH config alias, without spaces or a leading '-'",
        ));
    }
    Ok(())
}

/// A loopback port free right now, for SSH's local forward.
fn free_local_port() -> io::Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

fn ssh_arguments(
    options: &SshOptions,
    local_port: u16,
    remote_port: u16,
    remote_script: &str,
) -> Vec<String> {
    let mut arguments: Vec<String> = [
        "-T",
        // Without a working tunnel the viewer could only time out.
        "-o",
        "ExitOnForwardFailure=yes",
        // An X11-forwarded DISPLAY would point the host at this screen.
        "-o",
        "ForwardX11=no",
        // Notice a dead network within about half a minute.
        "-o",
        "ServerAliveInterval=10",
        "-o",
        "ServerAliveCountMax=3",
        "-L",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    arguments.push(format!("127.0.0.1:{local_port}:127.0.0.1:{remote_port}"));
    for option in &options.ssh_options {
        arguments.push("-o".into());
        arguments.push(option.clone());
    }
    arguments.push("--".into());
    arguments.push(options.destination.clone());
    arguments.push(format!("sh -c {}", shell_quote(remote_script)));
    arguments
}

/// The POSIX script the remote login shell runs: find the helper, then
/// replace the shell with a loopback-only host for one session.
fn remote_host_script(options: &SshOptions, remote_port: u16) -> String {
    remote_host_script_with(options, remote_port, MANAGED_REMOTE_COMMAND)
}

fn remote_host_script_with(options: &SshOptions, remote_port: u16, managed: &str) -> String {
    let candidates = match &options.remote_command {
        None => format!("jwm-remote {}", shell_quote(managed)),
        Some(command) => match command.strip_prefix("~/") {
            Some(rest) => format!("\"$HOME\"/{}", shell_quote(rest)),
            None => shell_quote(command),
        },
    };
    let mut host =
        format!("host --listen 127.0.0.1:{remote_port} --key-stdin --once --display auto");
    if !options.view_only {
        host.push_str(" --allow-input");
    }
    if options.clipboard {
        host.push_str(" --allow-clipboard");
        host.push_str(&format!(
            " --clipboard-mode {} --clipboard-direction {}",
            options.clipboard_mode, options.clipboard_direction
        ));
        // Both ends enforce their own limit; give the host the same one.
        host.push_str(&format!(
            " --clipboard-file-limit {}",
            options.clipboard_file_limit
        ));
    }
    if let Some(monitor) = &options.monitor {
        host.push_str(" --monitor ");
        host.push_str(&shell_quote(monitor));
    }
    for area in &options.shared_sources {
        match area {
            // Never reinterpret an unsupported instance grant as a root grant.
            CaptureArea::Window(_) => {
                return "echo 'window grants require local authorization' >&2; exit 64".into();
            }
            CaptureArea::Root => host.push_str(" --share-root"),
            CaptureArea::Monitor(name) => {
                host.push_str(" --share-monitor ");
                host.push_str(&shell_quote(name));
            }
            CaptureArea::Region(region) => {
                host.push_str(" --share-region ");
                host.push_str(&shell_quote(&format!(
                    "{}x{}+{}+{}",
                    region.width, region.height, region.x, region.y
                )));
            }
        }
    }
    for (flag, value) in [
        ("--fps", options.fps.map(u64::from)),
        ("--jpeg-quality", options.jpeg_quality.map(u64::from)),
        ("--max-width", options.max_width.map(u64::from)),
        (
            "--frame-max-width",
            Some(u64::from(options.frame_limits.max_width)),
        ),
        (
            "--frame-max-height",
            Some(u64::from(options.frame_limits.max_height)),
        ),
        ("--frame-max-pixels", Some(options.frame_limits.max_pixels)),
    ] {
        if let Some(value) = value {
            host.push_str(&format!(" {flag} {value}"));
        }
    }
    if options.scaling != ScalingPolicy::Auto {
        host.push_str(&format!(" --scaling {}", options.scaling));
    }
    format!(
        "for candidate in {candidates}; do \
         if command -v \"$candidate\" >/dev/null 2>&1; then exec \"$candidate\" {host}; fi; \
         done; \
         echo 'jwm-remote: jwm-remote is not installed on this machine \
         (looked in PATH and {managed}); pass --remote-command PATH' >&2; \
         exit 127"
    )
}

/// Quote one word for a POSIX shell.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> SshOptions {
        SshOptions {
            window_position: None,
            window_size: None,
            destination: "ubuntu@desk".into(),
            ssh_options: Vec::new(),
            remote_command: None,
            monitor: None,
            shared_sources: Vec::new(),
            fps: None,
            jpeg_quality: None,
            max_width: None,
            frame_limits: FrameLimits::default(),
            display: None,
            view_only: false,
            grab_input: true,
            seamless_input: false,
            clipboard: false,
            clipboard_mode: ClipboardMode::All,
            clipboard_direction: ClipboardDirection::Both,
            clipboard_file_limit: 2 << 30,
            escape_key: "F12".into(),
            scaling: ScalingPolicy::Auto,
        }
    }

    #[test]
    fn ssh_input_preflight_precedes_lock_and_process_creation() {
        const CHILD: &str = "JWM_TEST_SSH_INPUT_PREFLIGHT";
        if let Ok(case) = std::env::var(CHILD) {
            let mut opts = options();
            let expected = match case.as_str() {
                "invalid_escape" => {
                    opts.escape_key = "not_a_keysym".into();
                    "unknown escape key"
                }
                "seamless_without_grab" => {
                    opts.seamless_input = true;
                    opts.grab_input = false;
                    "seamless input requires"
                }
                "seamless_view_only" => {
                    opts.seamless_input = true;
                    opts.view_only = true;
                    "seamless input requires"
                }
                "valid" => {
                    opts.seamless_input = true;
                    "ssh"
                }
                _ => panic!("unexpected preflight case"),
            };
            let error = run_ssh(opts).unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
            if case == "valid" {
                // The fake SSH exits 255 without readiness. Depending on EOF/exit
                // observation order, either existing early-exit diagnostic applies.
                let io = error.downcast_ref::<io::Error>().unwrap();
                assert_eq!(io.kind(), io::ErrorKind::ConnectionAborted);
            }
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!(
            "jwm-ssh-input-preflight-{:016x}",
            rand::random::<u64>()
        ));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let bin = root.join("bin");
        std::fs::create_dir(&bin).unwrap();
        let ssh = bin.join("ssh");
        std::fs::write(
            &ssh,
            "#!/bin/sh\nprintf invoked > \"$JWM_TEST_SSH_MARKER\"\nexit 255\n",
        )
        .unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
        for case in [
            "invalid_escape",
            "seamless_without_grab",
            "seamless_view_only",
            "valid",
        ] {
            let runtime = root.join(case);
            std::fs::create_dir(&runtime).unwrap();
            std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
            let marker = root.join(format!("{case}.invoked"));
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "remote::ssh::tests::ssh_input_preflight_precedes_lock_and_process_creation",
                    "--nocapture",
                ])
                .env(CHILD, case)
                .env("PATH", &bin)
                .env("XDG_RUNTIME_DIR", &runtime)
                .env("JWM_TEST_SSH_MARKER", &marker)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{case}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            assert_eq!(marker.exists(), case == "valid", "{case}");
            assert_eq!(
                std::fs::read_dir(runtime).unwrap().count(),
                usize::from(case == "valid"),
                "{case}"
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ssh_remote_script_forwards_scaling_policy() {
        let mut opts = options();
        assert!(!remote_host_script(&opts, 50000).contains("--scaling"));
        opts.scaling = ScalingPolicy::Cpu;
        assert!(remote_host_script(&opts, 50000).contains("--scaling cpu"));
        opts.scaling = ScalingPolicy::Gpu;
        assert!(remote_host_script(&opts, 50000).contains("--scaling gpu"));
    }

    /// Run the generated script through a real `sh`, with fake helpers on
    /// `PATH` that print the arguments they were given.
    fn run_script(script: &str, path: &str) -> std::process::Output {
        Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .env("PATH", path)
            .output()
            .unwrap()
    }

    fn fake_helper(directory: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(directory).unwrap();
        let helper = directory.join("jwm-remote");
        std::fs::write(
            &helper,
            "#!/bin/sh\nfor a in \"$@\"; do echo \"[$a]\"; done\n",
        )
        .unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn local_window_size_does_not_change_remote_script_or_session_identity() {
        let base = options();
        let mut sized = base.clone();
        sized.window_position = Some(super::super::window_position::WindowPosition {
            x: -32768,
            y: 32767,
        });
        sized.window_size = Some(super::super::window_size::WindowSize {
            width: 1280,
            height: 720,
        });
        assert_eq!(
            remote_host_script(&base, 50000),
            remote_host_script(&sized, 50000)
        );
        assert_eq!(peer_label(&base), peer_label(&sized));
        sized.window_size = Some(super::super::window_size::WindowSize {
            width: 0,
            height: 720,
        });
        assert!(
            run_ssh(sized)
                .unwrap_err()
                .to_string()
                .contains("window size")
        );
    }

    #[test]
    fn held_session_policy_mismatch_never_focuses_or_closes_a_viewer() {
        let mut broad = options();
        broad.clipboard = true;
        let held = peer_label(&broad);
        let mut narrow = broad.clone();
        narrow.clipboard_mode = ClipboardMode::Text;
        narrow.clipboard_direction = ClipboardDirection::FromHost;
        let mut disabled = broad.clone();
        disabled.clipboard = false;
        for (holder, requested) in [
            (held.clone(), peer_label(&narrow)),
            (held.clone(), peer_label(&disabled)),
            ("legacy".into(), peer_label(&disabled)),
            ("legacy".into(), "legacy".into()),
        ] {
            let error = dispatch_held_session::<()>(
                &holder,
                &requested,
                || panic!("policy mismatch must not focus a viewer"),
                || panic!("policy mismatch must not switch or close a viewer"),
            )
            .unwrap_err();
            assert!(error.to_string().contains("close it explicitly"));
        }
        assert!(
            dispatch_held_session::<()>(
                &held,
                &held,
                || Ok(true),
                || panic!("same session must not close")
            )
            .unwrap()
            .is_none()
        );
        let mut other_monitor = broad;
        other_monitor.monitor = Some("DP-2".into());
        assert_eq!(
            dispatch_held_session(
                &held,
                &peer_label(&other_monitor),
                || panic!("monitor switch must not focus old viewer"),
                || Ok(Some(123))
            )
            .unwrap(),
            Some(123)
        );
    }

    #[test]
    fn colliding_destination_slots_never_switch_to_a_different_machine() {
        // Other parallel tests spawn processes. A fork inherits even CLOEXEC
        // descriptors until exec, temporarily extending a flock's lifetime.
        // Isolate this exact last-holder-drop assertion; do not weaken it.
        const CHILD: &str = "JWM_TEST_ISOLATED_DESTINATION_LOCK";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "remote::ssh::tests::colliding_destination_slots_never_switch_to_a_different_machine", "--nocapture"])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            return;
        }
        for (left, right) in [("2001:db8::1", "2001_db8__1"), ("[::1]", "___1_")] {
            let mut first = options();
            let user = format!("test-{:016x}@", rand::random::<u64>());
            first.destination = format!("{user}{left}");
            let mut second = first.clone();
            second.destination = format!("{user}{right}");
            validate_destination(&first.destination).unwrap();
            validate_destination(&second.destination).unwrap();
            // Keep the historical inode for coexistence with already running clients.
            assert_eq!(
                session_lock_path(&first.destination),
                session_lock_path(&second.destination)
            );
            for (held, requested) in [(&first, &second), (&second, &first)] {
                let mut lock = SessionLock::acquire(&held.destination)
                    .unwrap()
                    .ok()
                    .unwrap();
                lock.record(&peer_label(held)).unwrap();
                let holder = SessionLock::acquire(&requested.destination)
                    .unwrap()
                    .err()
                    .unwrap();
                for monitor in [None, Some("DP-2".to_owned())] {
                    let mut requested = requested.clone();
                    requested.monitor = monitor;
                    let error = dispatch_held_session::<()>(
                        &holder,
                        &peer_label(&requested),
                        || panic!("a different destination must not focus the old viewer"),
                        || panic!("a different destination must not close the old viewer"),
                    )
                    .unwrap_err();
                    assert!(error.to_string().contains("close it explicitly"));
                }
                drop(lock);
                // The same slot becomes available normally; no second lock is created.
                assert!(
                    SessionLock::acquire(&requested.destination)
                        .unwrap()
                        .is_ok()
                );
            }
            std::fs::remove_file(session_lock_path(&first.destination)).unwrap();
        }
    }

    #[test]
    fn explicit_connection_overrides_never_silently_reuse_a_viewer() {
        let base = options();
        let mut alternatives = Vec::new();
        for option in [
            "HostName=other.example",
            "Port=2222",
            "User=other",
            "ProxyJump=jump.example",
        ] {
            let mut changed = base.clone();
            changed.ssh_options.push(option.into());
            alternatives.push(changed);
        }
        let mut helper = base.clone();
        helper.remote_command = Some("/opt/other/jwm-remote".into());
        alternatives.push(helper);
        for changed in alternatives {
            for (held, requested) in [(&base, &changed), (&changed, &base)] {
                for monitor in [None, Some("DP-2".to_owned())] {
                    let mut requested = requested.clone();
                    requested.monitor = monitor;
                    let error = dispatch_held_session::<()>(
                        &peer_label(held),
                        &peer_label(&requested),
                        || panic!("changed connection must not focus the old viewer"),
                        || panic!("changed connection must not close the old viewer"),
                    )
                    .unwrap_err();
                    assert!(error.to_string().contains("close it explicitly"));
                }
            }
        }
    }

    #[test]
    fn connection_argument_order_is_preserved_and_not_disclosed_in_labels() {
        let mut left = options();
        left.ssh_options = vec![
            "HostName=first.example".into(),
            "HostName=second.example".into(),
        ];
        left.remote_command = Some("/private/helper".into());
        let mut right = left.clone();
        right.ssh_options.reverse();
        let label = peer_label(&left);
        assert!(!same_session_policy(&label, &peer_label(&right)));
        assert!(!label.contains("first.example"));
        assert!(!label.contains("second.example"));
        assert!(!label.contains("/private/helper"));
        assert_eq!(label, peer_label(&left.clone()));
    }

    #[test]
    fn legacy_policy_labels_cannot_cross_the_identity_version_boundary() {
        let current = peer_label(&options());
        let legacy = current.replace(" [policy-v2 ", " [policy ");
        for (holder, requested) in [(&current, &legacy), (&legacy, &current), (&legacy, &legacy)] {
            assert!(
                dispatch_held_session::<()>(
                    holder,
                    requested,
                    || panic!("legacy identity must not focus"),
                    || panic!("legacy identity must not close"),
                )
                .is_err()
            );
        }
        // The policy-aware legacy parser requires this exact old marker.
        // Pre-policy binaries are outside this compatibility guarantee.
        assert!(current.rsplit_once(" [policy ").is_none());
    }

    #[test]
    fn changed_local_input_contract_never_reuses_or_closes_a_session() {
        let base = options();
        let mut seamless = base.clone();
        seamless.seamless_input = true;
        let mut ungrabbed = base.clone();
        ungrabbed.grab_input = false;
        let mut escape = base.clone();
        escape.escape_key = "F11".into();
        for changed in [seamless, ungrabbed, escape] {
            // Both requesting new behavior and withdrawing old behavior must
            // fail explicitly, even if a monitor change would otherwise close
            // the old viewer. Never silently retain an old escape shortcut.
            for (held, requested) in [(&base, &changed), (&changed, &base)] {
                for monitor in [None, Some("DP-2".to_owned())] {
                    let mut requested = requested.clone();
                    requested.monitor = monitor;
                    let error = dispatch_held_session::<()>(
                        &peer_label(held),
                        &peer_label(&requested),
                        || panic!("changed input contract must not focus the old viewer"),
                        || panic!("changed input contract must not close the old viewer"),
                    )
                    .unwrap_err();
                    assert!(error.to_string().contains("close it explicitly"));
                }
            }
        }
    }

    #[test]
    fn clipboard_scope_changes_session_identity_without_dormant_grants() {
        let mut broad = options();
        broad.clipboard = true;
        let broad_label = peer_label(&broad);
        let mut narrow = broad.clone();
        narrow.clipboard_mode = ClipboardMode::Text;
        narrow.clipboard_direction = ClipboardDirection::FromHost;
        assert_ne!(broad_label, peer_label(&narrow));
        assert!(!same_session_policy(&broad_label, &peer_label(&narrow)));
        let mut disabled = broad.clone();
        disabled.clipboard = false;
        assert_ne!(broad_label, peer_label(&disabled));
        assert!(!same_session_policy(&broad_label, &peer_label(&disabled)));
        let disabled_label = peer_label(&disabled);
        disabled.clipboard_mode = ClipboardMode::Text;
        disabled.clipboard_direction = ClipboardDirection::ToHost;
        disabled.clipboard_file_limit = 123;
        assert_eq!(disabled_label, peer_label(&disabled));

        let mut reduced_limit = broad.clone();
        reduced_limit.clipboard_file_limit /= 2;
        assert_ne!(broad_label, peer_label(&reduced_limit));
        narrow.clipboard_file_limit = 0;
        let text_label = peer_label(&narrow);
        narrow.clipboard_file_limit = u64::MAX;
        assert_eq!(text_label, peer_label(&narrow));
        broad.clipboard_file_limit = 0;
        let no_files_label = peer_label(&broad);
        broad.clipboard_mode = ClipboardMode::TextImages;
        broad.clipboard_file_limit = 123;
        assert_eq!(no_files_label, peer_label(&broad));
        let mut view_only = options();
        view_only.view_only = true;
        assert_ne!(peer_label(&options()), peer_label(&view_only));
        assert!(!same_session_policy(
            &peer_label(&options()),
            &peer_label(&view_only)
        ));
        let mut smaller = options();
        smaller.frame_limits.max_pixels /= 2;
        assert_ne!(peer_label(&options()), peer_label(&smaller));
        assert!(!same_session_policy(
            &peer_label(&options()),
            &peer_label(&smaller)
        ));
        let mut other_monitor = options();
        other_monitor.monitor = Some("DP-2".into());
        assert!(same_session_policy(
            &peer_label(&options()),
            &peer_label(&other_monitor)
        ));
        assert!(!same_session_policy("legacy", &peer_label(&options())));
        assert!(!same_session_policy("legacy", "legacy"));
        // Legacy labels never contained an explicit clipboard policy.
        assert_ne!(peer_label(&disabled), disabled.destination);
    }

    #[test]
    fn source_grants_change_session_identity() {
        let mut opts = options();
        let original = peer_label(&opts);
        opts.shared_sources
            .push(CaptureArea::Monitor("DP-1".into()));
        let limited = peer_label(&opts);
        assert_ne!(original, limited);
        opts.shared_sources.push(CaptureArea::Root);
        assert_ne!(limited, peer_label(&opts));
        assert_eq!(peer_label(&opts), peer_label(&opts.clone()));
    }

    #[test]
    fn extra_source_grants_are_explicit_and_shell_quoted() {
        let mut opts = options();
        assert!(!remote_host_script(&opts, 50000).contains("--share-"));
        opts.shared_sources = vec![
            CaptureArea::Monitor("DP-1 'right'; echo bad".into()),
            CaptureArea::Region(super::super::x11_capture::CaptureRegion {
                x: -10,
                y: 2,
                width: 100,
                height: 80,
            }),
            CaptureArea::Root,
        ];
        let script = remote_host_script(&opts, 50000);
        assert!(script.contains(&format!(
            "--share-monitor {}",
            shell_quote("DP-1 'right'; echo bad")
        )));
        assert!(script.contains("--share-region '100x80+-10+2'"));
        assert!(script.contains("--share-root"));
    }

    #[test]
    fn the_remote_host_listens_on_loopback_for_one_session_with_the_requested_grants() {
        let directory = std::env::temp_dir().join(format!(
            "jwm-remote-ssh-test-{}-{:016x}",
            std::process::id(),
            rand::random::<u64>()
        ));
        fake_helper(&directory);
        let path = format!("{}:/usr/bin:/bin", directory.display());

        let mut control = options();
        control.monitor = Some("DP-0 'left'".into());
        let output = run_script(&remote_host_script(&control, 50000), &path);
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "[host]\n[--listen]\n[127.0.0.1:50000]\n[--key-stdin]\n[--once]\n\
             [--display]\n[auto]\n[--allow-input]\n[--monitor]\n[DP-0 'left']\n\
             [--frame-max-width]\n[16384]\n[--frame-max-height]\n[16384]\n\
             [--frame-max-pixels]\n[67108864]\n"
        );

        let mut view = options();
        view.view_only = true;
        view.clipboard = true;
        view.clipboard_mode = ClipboardMode::Text;
        view.clipboard_direction = ClipboardDirection::FromHost;
        view.fps = Some(24);
        view.jpeg_quality = Some(70);
        view.max_width = Some(1280);
        view.frame_limits = FrameLimits {
            max_width: 1200,
            max_height: 900,
            max_pixels: 800000,
        };
        let output = run_script(&remote_host_script(&view, 50001), &path);
        let printed = String::from_utf8(output.stdout).unwrap();
        assert!(!printed.contains("--allow-input"), "{printed}");
        assert!(printed.contains("[--allow-clipboard]"), "{printed}");
        assert!(
            printed.contains("[--clipboard-mode]\n[text]\n[--clipboard-direction]\n[from-host]\n"),
            "{printed}"
        );
        assert!(
            printed.contains("[--fps]\n[24]\n[--jpeg-quality]\n[70]\n[--max-width]\n[1280]\n"),
            "{printed}"
        );
        assert!(printed.contains("[--frame-max-width]\n[1200]\n[--frame-max-height]\n[900]\n[--frame-max-pixels]\n[800000]\n"), "{printed}");
        // The host enforces the viewer's file limit too.
        assert!(
            printed.contains("[--clipboard-file-limit]\n[2147483648]\n"),
            "{printed}"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_missing_helper_is_reported_rather_than_left_to_the_shell() {
        let script = remote_host_script_with(&options(), 50000, "/nonexistent/jwm-remote");
        let output = run_script(&script, "/nonexistent");
        assert_eq!(output.status.code(), Some(127));
        let message = String::from_utf8(output.stderr).unwrap();
        assert!(message.contains("not installed"), "{message}");
        assert!(message.contains("--remote-command"), "{message}");
    }

    #[test]
    fn an_explicit_remote_command_expands_only_a_leading_home() {
        let mut options = options();
        options.remote_command = Some("~/jrt/jwm-remote".into());
        assert!(
            remote_host_script(&options, 1)
                .starts_with("for candidate in \"$HOME\"/'jrt/jwm-remote';")
        );
        options.remote_command = Some("/opt/x y/jwm-remote".into());
        assert!(
            remote_host_script(&options, 1).starts_with("for candidate in '/opt/x y/jwm-remote';")
        );
    }

    #[test]
    fn ssh_gets_a_loopback_forward_and_the_destination_after_its_options() {
        let mut options = options();
        options.ssh_options = vec!["Port=2222".into()];
        let arguments = ssh_arguments(&options, 40000, 50000, "true");
        let forward = arguments.iter().position(|a| a == "-L").unwrap();
        assert_eq!(arguments[forward + 1], "127.0.0.1:40000:127.0.0.1:50000");
        assert!(arguments.windows(2).any(|w| w == ["-o", "Port=2222"]));
        assert!(arguments.windows(2).any(|w| w == ["-o", "ForwardX11=no"]));
        let end = arguments.iter().position(|a| a == "--").unwrap();
        assert_eq!(arguments[end + 1], "ubuntu@desk");
        assert_eq!(arguments[end + 2], "sh -c 'true'");
        assert_eq!(arguments.len(), end + 3);
    }

    #[test]
    fn option_like_or_multi_word_destinations_are_refused() {
        for bad in ["", "-oProxyCommand=x", "a b", "host\n", "-"] {
            assert!(validate_destination(bad).is_err(), "{bad:?}");
        }
        for good in ["desk", "ubuntu@10.0.15.52", "user@[::1]"] {
            assert!(validate_destination(good).is_ok(), "{good:?}");
        }
    }

    #[test]
    fn diagnostic_eof_before_ready_does_not_wait_for_a_live_ssh_child() {
        // A real process closes stderr but remains alive. Its eventual exit is
        // a test safety net; readiness handling must not wait for that exit.
        let mut child = Command::new("sh")
            .args(["-c", "exec 2>&-; exec sleep 2"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        let relay = thread::spawn(move || {
            relay_host_messages(BufReader::new(stderr), &ready_tx);
        });
        let started = Instant::now();
        let result = wait_until_ready("test-host", &mut child, None, "", &ready_rx);
        let elapsed = started.elapsed();
        let still_alive = child.try_wait().unwrap().is_none();
        let _ = child.kill();
        child.wait().unwrap();
        relay.join().unwrap();
        let error = result.unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::ConnectionAborted
        );
        assert!(elapsed < Duration::from_secs(1), "waited {elapsed:?}");
        assert!(
            still_alive,
            "readiness handling waited for the child to exit"
        );
    }

    #[test]
    fn diagnostic_eof_preserves_an_already_exited_ssh_status() {
        let mut child = Command::new("sh").args(["-c", "exit 255"]).spawn().unwrap();
        child.wait().unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        drop(ready_tx);
        let error = wait_until_ready("test-host", &mut child, None, "", &ready_rx).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("could not connect or log in to test-host")
        );
    }

    #[test]
    fn host_lines_are_relayed_without_telemetry_and_readiness_is_signalled() {
        assert_eq!(
            classify_host_line("jwm-remote: listening on 127.0.0.1:50000"),
            HostLine::Listening
        );
        assert_eq!(
            classify_host_line("jwm-remote: host 5.0s mode overlay/xrender scheduled 1"),
            HostLine::Telemetry
        );
        assert_eq!(
            classify_host_line("jwm-remote: sharing the whole 5120x1440 root"),
            HostLine::Host("sharing the whole 5120x1440 root")
        );
        assert_eq!(
            classify_host_line("Permission denied (publickey)."),
            HostLine::Other
        );

        let (ready_tx, ready_rx) = mpsc::channel();
        let input = "jwm-remote: sharing it\njwm-remote: listening on 127.0.0.1:1\n";
        relay_host_messages(io::Cursor::new(input), &ready_tx);
        assert!(ready_rx.try_recv().is_ok());
    }

    #[test]
    fn relay_read_cancellation_does_not_wait_for_a_live_pipe_writer() {
        let mut child = Command::new("sh")
            .args([
                "-c",
                "printf 'jwm-remote: listening on test\\npartial-diagnostic' >&2; exec sleep 2",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let reader = RelayReader::new(child.stderr.take().unwrap(), Arc::clone(&stop));
        let (ready_tx, ready_rx) = mpsc::channel();
        let relay = thread::spawn(move || {
            relay_host_messages(BufReader::new(reader), &ready_tx);
        });
        let ready = ready_rx.recv_timeout(Duration::from_secs(1));
        let start = Instant::now();
        stop.store(true, Ordering::Release);
        let joined = relay.join();
        let elapsed = start.elapsed();
        let _ = child.kill();
        child.wait().unwrap();
        joined.unwrap();
        ready.unwrap();
        assert!(elapsed < Duration::from_secs(1), "relay waited {elapsed:?}");
        assert!(ready_rx.try_recv().is_err());
    }

    #[test]
    fn relay_reader_preserves_readiness_and_normal_eof() {
        let (input, mut output) = std::os::unix::net::UnixStream::pair().unwrap();
        output
            .write_all(b"jwm-remote: listening on test\n")
            .unwrap();
        drop(output);
        let reader = RelayReader::new(input, Arc::new(AtomicBool::new(false)));
        let (ready_tx, ready_rx) = mpsc::channel();
        relay_host_messages(BufReader::new(reader), &ready_tx);
        assert_eq!(ready_rx.try_iter().count(), 1);
    }

    #[test]
    fn cancelled_relay_drains_queued_complete_lines_without_waiting_for_eof() {
        let (input, mut output) = std::os::unix::net::UnixStream::pair().unwrap();
        output.write_all(b"Permission denied\npartial").unwrap();
        let reader = RelayReader::new(input, Arc::new(AtomicBool::new(true)));
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();
        assert!(read_host_line(&mut reader, &mut line).unwrap());
        assert_eq!(line, b"Permission denied");
        assert_eq!(
            read_host_line(&mut reader, &mut line).unwrap_err().kind(),
            io::ErrorKind::ConnectionAborted
        );
        drop(output);
    }

    #[test]
    fn cancelled_relay_drain_is_bounded_even_when_data_remains_ready() {
        let (input, mut output) = std::os::unix::net::UnixStream::pair().unwrap();
        output.write_all(&[b'x'; 1024]).unwrap();
        let mut reader = RelayReader::new(input, Arc::new(AtomicBool::new(true)));
        // Small budget lets this exercise the exact production counter without
        // depending on the platform socket buffer size.
        reader.drain_remaining = 100;
        let mut buffer = [0; 1024];
        assert_eq!(reader.read(&mut buffer).unwrap(), 100);
        assert_eq!(reader.drain_remaining, 0);
        assert_eq!(
            reader.read(&mut buffer).unwrap_err().kind(),
            io::ErrorKind::ConnectionAborted
        );
    }

    #[test]
    fn blocked_diagnostic_output_does_not_delay_host_readiness_or_shutdown() {
        let (_reader, output) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut pump =
            DiagnosticPump::start_to("/bin/cat", Stdio::from(std::os::fd::OwnedFd::from(output)))
                .unwrap();
        let mut writer = pump.take_writer();
        let input = format!(
            "{}jwm-remote: listening on test\n",
            "jwm-remote: diagnostic\n".repeat(100_000)
        );
        let (ready_tx, ready_rx) = mpsc::channel();
        let start = Instant::now();
        relay_host_messages_with_output(io::Cursor::new(input), &ready_tx, |line| {
            writer.emit(line)
        });
        assert!(ready_rx.try_recv().is_ok());
        drop(writer);
        drop(pump);
        assert!(start.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn repeated_host_readiness_is_only_signalled_once() {
        let input = "jwm-remote: listening on 127.0.0.1:1\n".repeat(100_000);
        let (ready_tx, ready_rx) = mpsc::channel();
        relay_host_messages(io::Cursor::new(input), &ready_tx);
        assert_eq!(ready_rx.try_iter().count(), 1);
    }

    #[test]
    fn oversized_host_lines_are_discarded_without_false_readiness() {
        let mut input = b"jwm-remote: listening on ".to_vec();
        input.extend(vec![b'x'; MAX_HOST_LINE_BYTES * 8]);
        input.extend_from_slice(b"\njwm-remote: listening on real-host\r\n");
        let mut reader = BufReader::with_capacity(7, io::Cursor::new(input));
        let mut line = Vec::with_capacity(MAX_HOST_LINE_BYTES);
        assert!(read_host_line(&mut reader, &mut line).unwrap());
        assert_eq!(line, b"jwm-remote: listening on real-host");
        assert_eq!(line.capacity(), MAX_HOST_LINE_BYTES);
        assert!(!read_host_line(&mut reader, &mut line).unwrap());
    }

    #[test]
    fn oversized_unterminated_host_line_is_not_retained_or_signalled() {
        let mut input = b"jwm-remote: listening on ".to_vec();
        input.extend(vec![b'x'; MAX_HOST_LINE_BYTES * 8]);
        let mut reader = BufReader::with_capacity(4096, io::Cursor::new(&input));
        let mut line = Vec::with_capacity(MAX_HOST_LINE_BYTES);
        assert!(!read_host_line(&mut reader, &mut line).unwrap());
        assert!(line.is_empty());
        assert_eq!(line.capacity(), MAX_HOST_LINE_BYTES);
        let (ready_tx, ready_rx) = mpsc::channel();
        relay_host_messages(io::Cursor::new(input), &ready_tx);
        assert!(ready_rx.try_recv().is_err());
    }

    #[test]
    fn bounded_host_lines_preserve_boundary_empty_and_eof_lines() {
        let mut input = vec![b'x'; MAX_HOST_LINE_BYTES];
        input.extend_from_slice(b"\n\r\nlast line");
        let mut reader = BufReader::with_capacity(31, io::Cursor::new(input));
        let mut line = Vec::with_capacity(MAX_HOST_LINE_BYTES);
        assert!(read_host_line(&mut reader, &mut line).unwrap());
        assert_eq!(line.len(), MAX_HOST_LINE_BYTES);
        assert!(read_host_line(&mut reader, &mut line).unwrap());
        assert!(line.is_empty());
        assert!(read_host_line(&mut reader, &mut line).unwrap());
        assert_eq!(line, b"last line");
        assert!(!read_host_line(&mut reader, &mut line).unwrap());
    }

    #[test]
    fn unsafe_runtime_directory_does_not_fall_back() {
        const CHILD: &str = "JWM_TEST_UNSAFE_LOCK_RUNTIME";
        if let Ok(destination) = std::env::var(CHILD) {
            assert!(SessionLock::acquire(&destination).is_err());
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let directory = std::env::temp_dir().join(format!(
            "jwm-lock-runtime-test-{:016x}",
            rand::random::<u64>()
        ));
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o777)).unwrap();
        for runtime in [directory.clone(), PathBuf::from("relative-runtime")] {
            let destination = format!("test@unsafe-runtime-{:016x}", rand::random::<u64>());
            let fallback = std::env::temp_dir()
                .join(format!("jwm-remote-{}", unsafe { libc::getuid() }))
                .join(format!("jwm-remote-ssh-{destination}.lock"));
            assert!(!fallback.exists());
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "remote::ssh::tests::unsafe_runtime_directory_does_not_fall_back",
                    "--nocapture",
                ])
                .env(CHILD, &destination)
                .env("XDG_RUNTIME_DIR", runtime)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            assert!(!fallback.exists());
        }
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 0);
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn one_session_per_machine_records_which_monitor_it_shows() {
        let destination = format!("test@lock-{:016x}", rand::random::<u64>());
        let mut first = SessionLock::acquire(&destination).unwrap().unwrap();
        // Still starting: the holder has not named its monitor yet.
        assert_eq!(
            SessionLock::acquire(&destination).unwrap().err(),
            Some(String::new())
        );
        first.record("desk (DP-0)").unwrap();
        assert_eq!(
            SessionLock::acquire(&destination).unwrap().err().as_deref(),
            Some("desk (DP-0)")
        );
        // A shorter name replaces a longer one completely.
        first.record("desk").unwrap();
        assert_eq!(
            SessionLock::acquire(&destination).unwrap().err().as_deref(),
            Some("desk")
        );
        assert!(
            SessionLock::wait(&destination, Duration::from_millis(250))
                .unwrap()
                .is_none()
        );

        let releaser = thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            drop(first);
        });
        assert!(
            SessionLock::wait(&destination, Duration::from_secs(5))
                .unwrap()
                .is_some()
        );
        releaser.join().unwrap();
        let path = session_lock_path(&destination);
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            format!("jwm-remote-ssh-{destination}.lock")
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn shell_quoting_survives_embedded_quotes() {
        let output = Command::new("sh")
            .arg("-c")
            .arg(format!("printf %s {}", shell_quote("it's $HOME `x`")))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8(output.stdout).unwrap(), "it's $HOME `x`");
    }
}
