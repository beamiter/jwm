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
use super::key::{KeySource, generate_session_key};
use super::x11_viewer::focus_existing_viewer;
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
    /// `[USER@]HOST` or an alias from the SSH configuration.
    pub destination: String,
    /// Extra `ssh -o` options, passed through unchanged.
    pub ssh_options: Vec<String>,
    /// Path of `jwm-remote` on the other machine; found automatically if unset.
    pub remote_command: Option<String>,
    /// Share one RandR monitor of the other machine.
    pub monitor: Option<String>,
    /// Local X11 display for the viewer window.
    pub display: Option<String>,
    pub view_only: bool,
    pub grab_input: bool,
    pub clipboard: bool,
    pub escape_key: String,
}

pub fn run_ssh(options: SshOptions) -> RemoteResult<()> {
    validate_destination(&options.destination)?;
    let label = peer_label(&options);
    // One session per machine and monitor. Pressing a shortcut again means
    // "take me back there", not "start a second host fighting over input".
    let Some(_session_lock) = SessionLock::acquire(&label)? else {
        if focus_existing_viewer(options.display.as_deref(), &label)? {
            eprintln!("jwm-remote: {label} is already open; brought it to the front");
        } else {
            eprintln!("jwm-remote: a connection to {label} is already starting");
        }
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
                address: format!("127.0.0.1:{local_port}"),
                peer_label: label.clone(),
                key: KeySource::Bytes(key),
                display: options.display.clone(),
                view_only: options.view_only,
                grab_input: options.grab_input,
                clipboard: options.clipboard,
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

/// An exclusive lock held for as long as one session to a peer runs.
struct SessionLock {
    _file: File,
}

impl SessionLock {
    /// `None` when another `jwm-remote ssh` already holds it.
    fn acquire(label: &str) -> io::Result<Option<Self>> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(session_lock_path(label))?;
        // SAFETY: flock only reads the descriptor, which `file` keeps open.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(Some(Self { _file: file }));
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            Ok(None)
        } else {
            Err(error)
        }
    }
}

/// A per-user lock file named after the peer. The kernel drops the lock with
/// the process, so a crashed session never blocks the next one.
fn session_lock_path(label: &str) -> PathBuf {
    let directory = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| {
            // SAFETY: getuid has no preconditions and cannot fail.
            std::env::temp_dir().join(format!("jwm-remote-{}", unsafe { libc::getuid() }))
        });
    let _ = std::fs::create_dir_all(&directory);
    let name: String = label
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
    match &options.monitor {
        Some(monitor) => format!("{} ({monitor})", options.destination),
        None => options.destination.clone(),
    }
}

/// An SSH destination as accepted here: no option-like or multi-word values,
/// so it can only ever be read by `ssh` as the destination.
fn validate_destination(destination: &str) -> io::Result<()> {
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
    }
    if let Some(monitor) = &options.monitor {
        host.push_str(" --monitor ");
        host.push_str(&shell_quote(monitor));
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
            destination: "ubuntu@desk".into(),
            ssh_options: Vec::new(),
            remote_command: None,
            monitor: None,
            display: None,
            view_only: false,
            grab_input: true,
            clipboard: false,
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
             [--display]\n[auto]\n[--allow-input]\n[--monitor]\n[DP-0 'left']\n"
        );

        let mut view = options();
        view.view_only = true;
        view.clipboard = true;
        let output = run_script(&remote_host_script(&view, 50001), &path);
        let printed = String::from_utf8(output.stdout).unwrap();
        assert!(!printed.contains("--allow-input"), "{printed}");
        assert!(printed.contains("[--allow-clipboard]"), "{printed}");
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
    fn only_one_session_per_peer_runs_at_a_time() {
        let label = format!("test@lock-{:016x} (DP-0)", rand::random::<u64>());
        let other_monitor = label.replace("DP-0", "DP-2");
        let first = SessionLock::acquire(&label).unwrap();
        assert!(first.is_some());
        assert!(SessionLock::acquire(&label).unwrap().is_none());
        // Another monitor of the same machine is a different session.
        assert!(SessionLock::acquire(&other_monitor).unwrap().is_some());
        drop(first);
        assert!(SessionLock::acquire(&label).unwrap().is_some());
        let path = session_lock_path(&label);
        assert!(path.to_str().unwrap().ends_with("_DP-0_.lock"), "{path:?}");
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(session_lock_path(&other_monitor));
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
