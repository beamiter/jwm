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
use super::x11_capture::CaptureArea;
use super::x11_viewer::{close_existing_viewer, focus_existing_viewer};
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::net::TcpListener;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
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
    pub clipboard: bool,
    pub clipboard_mode: ClipboardMode,
    pub clipboard_direction: ClipboardDirection,
    /// Largest file copy shared through the clipboard; 0 shares no files.
    pub clipboard_file_limit: u64,
    pub escape_key: String,
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
    let relay = thread::Builder::new()
        .name("jwm-remote-ssh-relay".into())
        .spawn(move || relay_host_messages(BufReader::new(stderr), &ready_tx));
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
                resize_source: None,
                window_size: options.window_size,
                window_position: options.window_position,
                address: format!("127.0.0.1:{local_port}"),
                peer_label: label.clone(),
                key: KeySource::Bytes(key),
                display: options.display.clone(),
                view_only: options.view_only,
                grab_input: options.grab_input,
                clipboard: options.clipboard,
                clipboard_mode: options.clipboard_mode,
                clipboard_direction: options.clipboard_direction,
                clipboard_file_limit: options.clipboard_file_limit,
                frame_limits: options.frame_limits,
                escape_key: options.escape_key.clone(),
            })
        }),
        Err(ref error) => Err(io::Error::new(error.kind(), error.to_string()).into()),
    };
    // Closing stdin is the host's signal to stop; SSH is ended regardless
    // after a grace period, so no session outlives this command.
    drop(stdin);
    stop_child(&mut child);
    if let Ok(relay) = relay {
        // The relay ends once SSH has exited and its error stream closed.
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
                // SSH closed its error stream: it has exited or is about to.
                return Err(ssh_ended_early(child.wait()?, destination).into());
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

/// Relay the remote host's diagnostics, signalling once it listens.
fn relay_host_messages(reader: impl BufRead, ready: &mpsc::Sender<()>) {
    for line in reader.lines() {
        let Ok(line) = line else { break };
        match classify_host_line(&line) {
            HostLine::Listening => {
                let _ = ready.send(());
            }
            HostLine::Telemetry => {}
            HostLine::Host(message) => eprintln!("jwm-remote: host: {message}"),
            // SSH's own messages already say "ssh:"; anything else is the
            // remote program speaking without the helper's prefix.
            HostLine::Other => eprintln!("{line}"),
        }
    }
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
            "the existing SSH session has a different or legacy permission policy; close it explicitly before starting this session",
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
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(session_lock_path(destination))?;
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
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| {
            // SAFETY: getuid has no preconditions and cannot fail.
            std::env::temp_dir().join(format!("jwm-remote-{}", unsafe { libc::getuid() }))
        });
    let _ = std::fs::create_dir_all(&directory);
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
            "{:?};clipboard={content},{direction},{file_limit};view_only={};frames={:?}",
            options.shared_sources, options.view_only, options.frame_limits
        )
        .as_bytes(),
    );
    let fingerprint: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{initial} [policy {fingerprint}]")
}

// A monitor change can reuse the machine slot only after checking identical
// immutable policy. Legacy labels have no policy and fail closed.
fn same_session_policy(left: &str, right: &str) -> bool {
    fn fingerprint(label: &str) -> Option<&str> {
        let (_, suffix) = label.rsplit_once(" [policy ")?;
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
            clipboard: false,
            clipboard_mode: ClipboardMode::All,
            clipboard_direction: ClipboardDirection::Both,
            clipboard_file_limit: 2 << 30,
            escape_key: "F12".into(),
        }
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
