//! Linux desktop actions for `xbar_core` hosts.
//!
//! This companion crate deliberately stays outside `xbar_core`: executable
//! names, process spawning, child reaping — and which compositor to ask for a
//! screenshot — are host policy rather than bar model semantics.
//!
//! Two destinations live here. Most effects end in a spawned process
//! ([`ProcessActionHandler`]); the screenshot pill instead goes over jwm's
//! control socket ([`jwm_ipc`]) to the compositor's own region capture, which
//! is what [`ScreenshotAction`] selects between.

use std::{
    ffi::{OsStr, OsString},
    fmt, io,
    os::{
        fd::{AsRawFd as _, RawFd},
        unix::ffi::OsStrExt as _,
        unix::process::CommandExt as _,
    },
    path::PathBuf,
    process::{Child, Command, ExitStatus, Output, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use xbar_core::{BarEffect, MonitorGeometry, PlatformEffectHandler, RuntimeUpdate};

pub mod jwm_ipc;

pub use jwm_ipc::{JwmIpc, JwmIpcError};

/// Maximum wall time used by [`CommandRunner::output`].
pub const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
/// Maximum bytes retained from each output stream by [`CommandRunner::output`].
pub const DEFAULT_COMMAND_OUTPUT_LIMIT: usize = 1024 * 1024;
/// Maximum UTF-8 byte length used for process waiter thread names.
pub const MAX_WAITER_THREAD_NAME_BYTES: usize = 15;

const COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAX_DRAIN_BYTES_PER_POLL: usize = 64 * 1024;

/// One executable and its fixed argument list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: OsString,
    pub args: Vec<OsString>,
}

impl CommandSpec {
    #[must_use]
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    #[must_use]
    pub fn with_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }
}

/// Failure to run a command whose captured output is required by a host.
#[derive(Debug)]
pub enum CommandRunError {
    /// The executable could not be spawned.
    Spawn {
        program: OsString,
        source: io::Error,
    },
    /// The executable completed unsuccessfully.
    Exit {
        program: OsString,
        status: ExitStatus,
        stderr: Vec<u8>,
    },
    /// Capturing or waiting for the child failed.
    Io {
        program: OsString,
        source: io::Error,
    },
    /// The child did not complete before its deadline.
    Timeout {
        program: OsString,
        timeout: Duration,
    },
    /// Stdout or stderr exceeded its individual retention limit.
    OutputTooLarge { program: OsString, limit: usize },
}

impl fmt::Display for CommandRunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn { program, source } => {
                write!(f, "failed to run {}: {source}", program.to_string_lossy())
            }
            Self::Exit {
                program,
                status,
                stderr,
            } => {
                write!(f, "{} exited with {status}", program.to_string_lossy())?;
                let stderr = String::from_utf8_lossy(stderr);
                let stderr = stderr.trim();
                if !stderr.is_empty() {
                    write!(f, ": {stderr}")?;
                }
                Ok(())
            }
            Self::Io { program, source } => write!(
                f,
                "failed while running {}: {source}",
                program.to_string_lossy()
            ),
            Self::Timeout { program, timeout } => write!(
                f,
                "{} exceeded the {timeout:?} command timeout",
                program.to_string_lossy()
            ),
            Self::OutputTooLarge { program, limit } => write!(
                f,
                "{} produced more than {limit} bytes on one output stream",
                program.to_string_lossy()
            ),
        }
    }
}

impl std::error::Error for CommandRunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn { source, .. } | Self::Io { source, .. } => Some(source),
            Self::Exit { .. } | Self::Timeout { .. } | Self::OutputTooLarge { .. } => None,
        }
    }
}

/// Executes one [`CommandSpec`] and captures bounded output.
///
/// Unlike [`ProcessActionHandler`], this adapter waits for completion because
/// callers need stdout before they can update status state. Non-zero exits are
/// errors and retain stderr for diagnostics. The default entry point imposes a
/// wall-time limit and a per-stream byte limit; callers with a known slower
/// probe can select explicit limits. Commands are executed directly, without a
/// shell or string interpolation.
#[derive(Debug, Clone, Copy, Default)]
pub struct CommandRunner;

impl CommandRunner {
    /// Run a command with the default wall-time and per-stream output limits.
    pub fn output(command: &CommandSpec) -> Result<Output, CommandRunError> {
        Self::output_with_limits(
            command,
            DEFAULT_COMMAND_TIMEOUT,
            DEFAULT_COMMAND_OUTPUT_LIMIT,
        )
    }

    /// Run a command with an explicit deadline and byte limit for each stream.
    pub fn output_with_limits(
        command: &CommandSpec,
        timeout: Duration,
        output_limit: usize,
    ) -> Result<Output, CommandRunError> {
        let mut child = Command::new(&command.program)
            .args(&command.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .map_err(|source| CommandRunError::Spawn {
                program: command.program.clone(),
                source,
            })?;
        let output =
            capture_child_output(&mut child, timeout, output_limit).map_err(
                |error| match error {
                    CaptureError::Io(source) => CommandRunError::Io {
                        program: command.program.clone(),
                        source,
                    },
                    CaptureError::Timeout => CommandRunError::Timeout {
                        program: command.program.clone(),
                        timeout,
                    },
                    CaptureError::OutputTooLarge => CommandRunError::OutputTooLarge {
                        program: command.program.clone(),
                        limit: output_limit,
                    },
                },
            )?;
        if output.status.success() {
            Ok(output)
        } else {
            Err(CommandRunError::Exit {
                program: command.program.clone(),
                status: output.status,
                stderr: output.stderr,
            })
        }
    }
}

#[derive(Debug)]
enum CaptureError {
    Io(io::Error),
    Timeout,
    OutputTooLarge,
}

fn capture_child_output(
    child: &mut Child,
    timeout: Duration,
    output_limit: usize,
) -> Result<Output, CaptureError> {
    let child_id = child.id();
    let Some(mut stdout) = child.stdout.take() else {
        terminate_child_group(child);
        return Err(CaptureError::Io(io::Error::other(
            "command stdout was not captured",
        )));
    };
    let Some(mut stderr) = child.stderr.take() else {
        terminate_child_group(child);
        return Err(CaptureError::Io(io::Error::other(
            "command stderr was not captured",
        )));
    };
    if let Err(source) =
        set_nonblocking(stdout.as_raw_fd()).and_then(|()| set_nonblocking(stderr.as_raw_fd()))
    {
        terminate_child_group(child);
        return Err(CaptureError::Io(source));
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
                let drain = drain_available(&mut stdout, &mut stdout_bytes, output_limit)
                    .map_err(CaptureError::Io)?;
                stdout_eof = drain.eof;
                if drain.oversized {
                    return Err(CaptureError::OutputTooLarge);
                }
            }
            if !stderr_eof {
                let drain = drain_available(&mut stderr, &mut stderr_bytes, output_limit)
                    .map_err(CaptureError::Io)?;
                stderr_eof = drain.eof;
                if drain.oversized {
                    return Err(CaptureError::OutputTooLarge);
                }
            }

            if status.is_none() {
                status = child.try_wait().map_err(CaptureError::Io)?;
                if status.is_some() {
                    // A synchronous probe must not leave descendants holding
                    // its capture pipes open after the direct child exits.
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
                return Err(CaptureError::Timeout);
            }
            thread::sleep(COMMAND_POLL_INTERVAL.min(timeout.saturating_sub(elapsed)));
        }
    })();
    if result.is_err() {
        terminate_child_group(child);
    }
    result
}

#[derive(Debug, Clone, Copy)]
struct DrainResult {
    eof: bool,
    oversized: bool,
}

fn drain_available(
    source: &mut impl io::Read,
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

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn kill_process_group(child_id: u32) {
    if let Ok(process_group) = i32::try_from(child_id) {
        let _ = unsafe { libc::kill(-process_group, libc::SIGKILL) };
    }
}

fn terminate_child_group(child: &mut Child) {
    kill_process_group(child.id());
    let _ = child.kill();
    let _ = child.wait();
}

/// Where a screenshot request goes.
///
/// The default is jwm's own capture, reached over its control socket, rather
/// than an external grabber. The compositor already owns the framebuffer, the
/// pointer grab and the region editor, so asking it is both fewer moving parts
/// and the only route that works when no third-party tool is installed — and
/// under a Wayland session, where an X11 grabber cannot see the screen at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScreenshotAction {
    /// Ask the running jwm for its interactive region capture. `socket` is
    /// normally `None`, meaning "wherever the compositor binds"; a nested
    /// session sets it to reach a private compositor.
    Jwm { socket: Option<PathBuf> },
    /// Launch an external grabber instead, e.g. `flameshot gui`.
    Command(CommandSpec),
    /// The pill does nothing.
    Disabled,
}

impl ScreenshotAction {
    /// Ask whichever jwm this session belongs to.
    #[must_use]
    pub const fn jwm() -> Self {
        Self::Jwm { socket: None }
    }
}

impl Default for ScreenshotAction {
    fn default() -> Self {
        Self::jwm()
    }
}

/// Configurable policy for the effects this adapter understands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessActionConfig {
    pub screenshot: ScreenshotAction,
    pub audio_control: Option<CommandSpec>,
    pub waiter_thread_prefix: String,
}

impl Default for ProcessActionConfig {
    fn default() -> Self {
        Self {
            screenshot: ScreenshotAction::jwm(),
            audio_control: Some(CommandSpec::new("pavucontrol")),
            waiter_thread_prefix: "xbar-wait".to_owned(),
        }
    }
}

/// Failure to accept, launch or forward one effect.
#[derive(Debug)]
pub enum ProcessActionError {
    UnsupportedEffect(BarEffect),
    DisabledEffect(BarEffect),
    /// The effect is handled, but not by spawning anything — asked for as a
    /// command by a caller that wanted a [`CommandSpec`].
    NotAProcess(BarEffect),
    /// jwm was asked for the capture and could not take it.
    Jwm(JwmIpcError),
    Spawn {
        program: OsString,
        source: io::Error,
    },
    SpawnWaiter {
        program: OsString,
        source: io::Error,
    },
    WaiterStopped {
        program: OsString,
    },
}

impl fmt::Display for ProcessActionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedEffect(effect) => {
                write!(f, "process action adapter does not handle {effect:?}")
            }
            Self::DisabledEffect(effect) => {
                write!(f, "process command is disabled for {effect:?}")
            }
            Self::NotAProcess(effect) => {
                write!(f, "{effect:?} is not handled by spawning a process")
            }
            Self::Jwm(source) => write!(f, "screenshot request failed: {source}"),
            Self::Spawn { program, source } => {
                write!(
                    f,
                    "failed to launch {}: {source}",
                    program.to_string_lossy()
                )
            }
            Self::SpawnWaiter { program, source } => write!(
                f,
                "failed to start child waiter for {}: {source}",
                program.to_string_lossy()
            ),
            Self::WaiterStopped { program } => write!(
                f,
                "child waiter stopped before accepting {}",
                program.to_string_lossy()
            ),
        }
    }
}

impl std::error::Error for ProcessActionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn { source, .. } | Self::SpawnWaiter { source, .. } => Some(source),
            Self::Jwm(source) => Some(source),
            Self::UnsupportedEffect(_)
            | Self::DisabledEffect(_)
            | Self::NotAProcess(_)
            | Self::WaiterStopped { .. } => None,
        }
    }
}

/// `PlatformEffectHandler` for screenshot and audio-control process effects.
#[derive(Debug, Clone, Default)]
pub struct ProcessActionHandler {
    config: ProcessActionConfig,
}

impl ProcessActionHandler {
    #[must_use]
    pub const fn new(config: ProcessActionConfig) -> Self {
        Self { config }
    }

    #[must_use]
    pub const fn config(&self) -> &ProcessActionConfig {
        &self.config
    }

    pub fn config_mut(&mut self) -> &mut ProcessActionConfig {
        &mut self.config
    }

    #[must_use]
    pub const fn supports(effect: BarEffect) -> bool {
        matches!(effect, BarEffect::Screenshot | BarEffect::OpenAudioControl)
    }

    /// The executable this effect would spawn.
    ///
    /// A screenshot that goes to jwm has no executable, which is
    /// [`ProcessActionError::NotAProcess`] rather than a failure: the caller
    /// asked the wrong question, not for something impossible.
    pub fn command_for_effect(
        &self,
        effect: BarEffect,
    ) -> Result<&CommandSpec, ProcessActionError> {
        match effect {
            BarEffect::Screenshot => match &self.config.screenshot {
                ScreenshotAction::Command(command) => Ok(command),
                ScreenshotAction::Jwm { .. } => Err(ProcessActionError::NotAProcess(effect)),
                ScreenshotAction::Disabled => Err(ProcessActionError::DisabledEffect(effect)),
            },
            BarEffect::OpenAudioControl => self
                .config
                .audio_control
                .as_ref()
                .ok_or(ProcessActionError::DisabledEffect(effect)),
            _ => Err(ProcessActionError::UnsupportedEffect(effect)),
        }
    }

    /// Ask jwm for its interactive region capture.
    fn request_jwm_screenshot(socket: Option<&PathBuf>) -> Result<(), ProcessActionError> {
        let ipc = match socket {
            Some(socket) => JwmIpc::at(socket.clone()),
            None => JwmIpc::new(),
        };
        ipc.take_screenshot().map_err(ProcessActionError::Jwm)
    }

    /// Launch a configured command and transfer child ownership to a bounded
    /// waiter thread so the host never leaks a zombie process.
    pub fn launch(&self, command: &CommandSpec) -> Result<(), ProcessActionError> {
        let program = command.program.clone();
        let thread_name = waiter_name(&self.config.waiter_thread_prefix, &program);
        let (sender, receiver) = mpsc::sync_channel::<(OsString, Child)>(1);
        thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                if let Ok((program, mut child)) = receiver.recv() {
                    reap_child(&program, &mut child);
                }
            })
            .map_err(|source| ProcessActionError::SpawnWaiter {
                program: command.program.clone(),
                source,
            })?;

        let child = Command::new(&command.program)
            .args(&command.args)
            .spawn()
            .map_err(|source| ProcessActionError::Spawn {
                program: command.program.clone(),
                source,
            })?;

        if let Err(error) = sender.send((program.clone(), child)) {
            let (_, mut child) = error.0;
            reap_child(&program, &mut child);
            return Err(ProcessActionError::WaiterStopped { program });
        }
        Ok(())
    }
}

impl PlatformEffectHandler for ProcessActionHandler {
    type Error = ProcessActionError;

    fn handle(&mut self, effect: BarEffect) -> Result<(), Self::Error> {
        if let (BarEffect::Screenshot, ScreenshotAction::Jwm { socket }) =
            (effect, &self.config.screenshot)
        {
            return Self::request_jwm_screenshot(socket.as_ref());
        }
        let command = self.command_for_effect(effect)?;
        self.launch(command)
    }
}

/// Window-geometry request the host must apply with its native window API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeometryRequest {
    /// Constrain the bar to this monitor rectangle.
    Apply(MonitorGeometry),
    /// The WM constraint is gone; restore the host's default placement.
    Clear,
}

/// Standard Linux host routing for one [`RuntimeUpdate`].
///
/// Issues and unhandled effects are logged, geometry effects go to the
/// caller's window closure, and screenshot/audio-control effects go to the
/// owned [`ProcessActionHandler`] (whose launch failures are logged rather
/// than aborting the frame). Only the geometry closure can fail the route,
/// because window-system errors are the one thing a frontend must see.
#[derive(Debug, Clone, Default)]
pub struct EffectRouter {
    process: ProcessActionHandler,
}

impl EffectRouter {
    #[must_use]
    pub const fn new(process: ProcessActionHandler) -> Self {
        Self { process }
    }

    pub const fn process_mut(&mut self) -> &mut ProcessActionHandler {
        &mut self.process
    }

    /// Route `update` and report whether the frontend should redraw.
    pub fn route<F, E>(&mut self, update: RuntimeUpdate, mut apply_geometry: F) -> Result<bool, E>
    where
        F: FnMut(GeometryRequest) -> Result<(), E>,
    {
        let needs_redraw = update.needs_redraw();
        for issue in &update.issues {
            log::warn!("xbar runtime issue: {issue}");
        }
        for effect in update.platform_effects {
            match effect {
                BarEffect::ApplyMonitorGeometry(geometry) => {
                    apply_geometry(GeometryRequest::Apply(geometry))?;
                }
                BarEffect::ClearMonitorGeometry => {
                    apply_geometry(GeometryRequest::Clear)?;
                }
                effect if ProcessActionHandler::supports(effect) => {
                    if let Err(error) = PlatformEffectHandler::handle(&mut self.process, effect) {
                        log::warn!("failed to handle process effect: {error}");
                    }
                }
                effect => {
                    log::warn!("no frontend adapter handled platform effect: {effect:?}");
                }
            }
        }
        Ok(needs_redraw)
    }
}

fn waiter_name(prefix: &str, program: &OsStr) -> String {
    let mut name = String::with_capacity(MAX_WAITER_THREAD_NAME_BYTES);
    push_thread_name_text(&mut name, prefix);
    if !name.is_empty()
        && !program.as_bytes().is_empty()
        && name.len().saturating_add(1) < MAX_WAITER_THREAD_NAME_BYTES
    {
        name.push('-');
    }

    // Inspect only a small prefix before lossy conversion. Invalid UTF-8 can
    // expand to a three-byte replacement character, so four source bytes per
    // remaining output byte are already more than enough to fill the budget.
    let remaining = MAX_WAITER_THREAD_NAME_BYTES.saturating_sub(name.len());
    let bytes = program.as_bytes();
    let inspected = bytes.len().min(remaining.saturating_mul(4));
    push_thread_name_text(&mut name, &String::from_utf8_lossy(&bytes[..inspected]));

    if name.is_empty() {
        name.push_str("xbar-wait");
    }
    name
}

fn push_thread_name_text(name: &mut String, text: &str) {
    for character in text.chars() {
        let character = if character == '\0' { '?' } else { character };
        if name.len().saturating_add(character.len_utf8()) > MAX_WAITER_THREAD_NAME_BYTES {
            break;
        }
        name.push(character);
    }
}

fn reap_child(program: &OsStr, child: &mut Child) {
    match child.wait() {
        Ok(status) if status.success() => {}
        Ok(status) => log::warn!("{} exited with {status}", program.to_string_lossy()),
        Err(error) => log::warn!(
            "failed to reap {} process: {error}",
            program.to_string_lossy()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The screenshot pill's default is jwm's own capture, not a spawned
    /// grabber — which is why asking for its *command* is a category error
    /// rather than a disabled effect.
    #[test]
    fn the_default_screenshot_goes_to_jwm_rather_than_to_a_process() {
        let handler = ProcessActionHandler::default();
        assert_eq!(handler.config().screenshot, ScreenshotAction::jwm());
        assert!(matches!(
            handler.command_for_effect(BarEffect::Screenshot),
            Err(ProcessActionError::NotAProcess(BarEffect::Screenshot))
        ));
        assert_eq!(
            handler
                .command_for_effect(BarEffect::OpenAudioControl)
                .unwrap(),
            &CommandSpec::new("pavucontrol")
        );
        assert!(ProcessActionHandler::supports(BarEffect::Screenshot));
        assert!(!ProcessActionHandler::supports(BarEffect::ToggleMute));
        assert!(matches!(
            handler.command_for_effect(BarEffect::ToggleMute),
            Err(ProcessActionError::UnsupportedEffect(BarEffect::ToggleMute))
        ));
    }

    /// A host that would rather keep an external grabber can still say so, and
    /// gets exactly the pre-IPC behaviour back.
    #[test]
    fn an_external_grabber_can_still_be_configured() {
        let handler = ProcessActionHandler::new(ProcessActionConfig {
            screenshot: ScreenshotAction::Command(CommandSpec::new("flameshot").with_arg("gui")),
            ..ProcessActionConfig::default()
        });
        assert_eq!(
            handler.command_for_effect(BarEffect::Screenshot).unwrap(),
            &CommandSpec::new("flameshot").with_arg("gui")
        );
    }

    #[test]
    fn configured_effect_can_be_disabled_without_affecting_the_other() {
        let handler = ProcessActionHandler::new(ProcessActionConfig {
            screenshot: ScreenshotAction::Disabled,
            ..ProcessActionConfig::default()
        });

        assert!(matches!(
            handler.command_for_effect(BarEffect::Screenshot),
            Err(ProcessActionError::DisabledEffect(BarEffect::Screenshot))
        ));
        assert!(
            handler
                .command_for_effect(BarEffect::OpenAudioControl)
                .is_ok()
        );
    }

    /// The routing decision itself: a screenshot effect must reach the socket,
    /// never a `Command`. Pointing at a socket nothing is listening on is how
    /// we observe *that* it was attempted without needing a compositor.
    #[test]
    fn a_screenshot_effect_is_forwarded_to_jwm_not_spawned() {
        let mut handler = ProcessActionHandler::new(ProcessActionConfig {
            screenshot: ScreenshotAction::Jwm {
                socket: Some("/definitely/missing/xbar-actions-jwm.sock".into()),
            },
            audio_control: None,
            waiter_thread_prefix: "xbar-jwm-test".to_owned(),
        });

        let error = PlatformEffectHandler::handle(&mut handler, BarEffect::Screenshot).unwrap_err();
        assert!(
            matches!(
                error,
                ProcessActionError::Jwm(crate::jwm_ipc::JwmIpcError::Unreachable { .. })
            ),
            "expected an IPC attempt, got {error:?}"
        );
    }

    #[test]
    fn launch_reports_a_missing_executable_synchronously() {
        let handler = ProcessActionHandler::default();
        let error = handler
            .launch(&CommandSpec::new(
                "/definitely/missing/xbar-linux-actions-test",
            ))
            .unwrap_err();

        assert!(matches!(error, ProcessActionError::Spawn { .. }));
        assert!(error.to_string().contains("failed to launch"));
    }

    #[test]
    fn waiter_names_are_bounded_utf8_and_cannot_panic_thread_creation() {
        let prefix = format!("{}\0ignored", "界".repeat(32));
        let program = OsStr::new("/usr/bin/program-with-a-long-name");
        let name = waiter_name(&prefix, program);

        assert!(!name.contains('\0'));
        assert!(name.len() <= MAX_WAITER_THREAD_NAME_BYTES);
        assert!(name.is_char_boundary(name.len()));

        let nul_name = waiter_name("w\0", OsStr::new("tool\0name"));
        assert_eq!(nul_name, "w?-tool?name");
        thread::Builder::new()
            .name(nul_name)
            .spawn(|| {})
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn command_runner_captures_stdout_and_passes_arguments_without_a_shell() {
        let output = CommandRunner::output(
            &CommandSpec::new("/usr/bin/printf").with_args(["%s", "hello xbar"]),
        )
        .unwrap();

        assert_eq!(output.stdout, b"hello xbar");
        assert!(output.stderr.is_empty());
    }

    #[test]
    fn command_runner_reports_spawn_and_non_zero_exit_failures() {
        let missing = CommandRunner::output(&CommandSpec::new(
            "/definitely/missing/xbar-command-runner-test",
        ))
        .unwrap_err();
        assert!(matches!(missing, CommandRunError::Spawn { .. }));

        let failed = CommandRunner::output(
            &CommandSpec::new("/bin/sh").with_args(["-c", "printf failure >&2; exit 7"]),
        )
        .unwrap_err();
        assert!(matches!(
            &failed,
            CommandRunError::Exit { status, .. } if status.code() == Some(7)
        ));
        assert!(failed.to_string().contains("failure"));
    }

    #[test]
    fn command_runner_bounds_each_output_stream() {
        let error = CommandRunner::output_with_limits(
            &CommandSpec::new("/bin/sh").with_args(["-c", "printf 123456789 >&2"]),
            Duration::from_secs(1),
            8,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            CommandRunError::OutputTooLarge { limit: 8, .. }
        ));
    }

    #[test]
    fn command_runner_enforces_its_deadline_and_reaps_the_group() {
        let started = Instant::now();
        let error = CommandRunner::output_with_limits(
            &CommandSpec::new("/bin/sh").with_args(["-c", "exec sleep 10"]),
            Duration::from_millis(25),
            64,
        )
        .unwrap_err();

        assert!(matches!(error, CommandRunError::Timeout { .. }));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn completed_probe_does_not_wait_for_background_pipe_holders() {
        let started = Instant::now();
        let output = CommandRunner::output_with_limits(
            &CommandSpec::new("/bin/sh").with_args(["-c", "sleep 10 &"]),
            Duration::from_secs(1),
            64,
        )
        .unwrap();

        assert!(output.status.success());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn effect_router_forwards_geometry_and_absorbs_process_failures() {
        let mut router = EffectRouter::new(ProcessActionHandler::new(ProcessActionConfig {
            screenshot: ScreenshotAction::Command(CommandSpec::new("/bin/true")),
            audio_control: None,
            waiter_thread_prefix: "xbar-router-test".to_owned(),
        }));
        let geometry = MonitorGeometry {
            x: 5,
            y: 0,
            width: 800,
            height: 600,
        };
        let update = RuntimeUpdate {
            platform_effects: vec![
                BarEffect::ApplyMonitorGeometry(geometry),
                BarEffect::Screenshot,
                BarEffect::OpenAudioControl,
                BarEffect::RefreshBattery,
                BarEffect::ClearMonitorGeometry,
            ],
            ..RuntimeUpdate::default()
        };

        let mut requests = Vec::new();
        let needs_redraw = router
            .route::<_, std::convert::Infallible>(update, |request| {
                requests.push(request);
                Ok(())
            })
            .unwrap();

        assert!(!needs_redraw);
        assert_eq!(
            requests,
            vec![GeometryRequest::Apply(geometry), GeometryRequest::Clear]
        );
    }

    #[test]
    fn effect_router_propagates_only_window_system_errors() {
        let mut router = EffectRouter::default();
        let update = RuntimeUpdate {
            platform_effects: vec![BarEffect::ClearMonitorGeometry],
            ..RuntimeUpdate::default()
        };

        let error = router.route(update, |_| Err("window gone")).unwrap_err();
        assert_eq!(error, "window gone");
    }

    #[test]
    fn platform_handler_launches_and_reaps_a_real_child() {
        let mut handler = ProcessActionHandler::new(ProcessActionConfig {
            screenshot: ScreenshotAction::Command(CommandSpec::new("/bin/true")),
            audio_control: None,
            waiter_thread_prefix: "xbar-actions-test".to_owned(),
        });

        PlatformEffectHandler::handle(&mut handler, BarEffect::Screenshot).unwrap();
    }
}
