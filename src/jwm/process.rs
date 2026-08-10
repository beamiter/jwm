use crate::backend::api::Backend;
use crate::jwm::WMArgEnum;
use libc::{SIG_DFL, SIGCHLD, setsid, sigaction, sigemptyset};
use log::{debug, error, info, warn};
use nix::errno::Errno;
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;
use std::process::{Child, Command};

use super::Jwm;

pub(crate) const TRANSIENT_CHILD_HANDOFF_ENV: &str = "JWM_TRANSIENT_CHILD_HANDOFF_V1";
const TRANSIENT_CHILD_HANDOFF_VERSION: u32 = 1;
const MAX_TRANSIENT_CHILD_HANDOFF_BYTES: usize = 16 * 1024;
const MAX_TRANSIENT_CHILDREN: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireTransientChildHandoff {
    version: u32,
    parent_pid: u32,
    pids: Vec<u32>,
}

/// Validated exact-PID ownership transferred across one same-process exec.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TransientChildRestartHandoff {
    parent_pid: u32,
    pids: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TransientChildHandoffError {
    PayloadTooLarge { bytes: usize },
    InvalidJson(String),
    UnsupportedVersion(u32),
    InvalidParentPid(u32),
    ParentPidMismatch { expected: u32, actual: u32 },
    TooManyChildren { children: usize },
    InvalidChildPid { index: usize, pid: u32 },
    DuplicateChildPid(u32),
}

impl fmt::Display for TransientChildHandoffError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PayloadTooLarge { bytes } => write!(
                f,
                "transient-child restart handoff is {bytes} bytes (maximum {MAX_TRANSIENT_CHILD_HANDOFF_BYTES})"
            ),
            Self::InvalidJson(error) => {
                write!(f, "malformed transient-child restart handoff: {error}")
            }
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported transient-child handoff version {version}")
            }
            Self::InvalidParentPid(pid) => {
                write!(f, "invalid transient-child parent PID {pid}")
            }
            Self::ParentPidMismatch { expected, actual } => write!(
                f,
                "transient-child parent PID {actual} does not match this process {expected}"
            ),
            Self::TooManyChildren { children } => write!(
                f,
                "transient-child handoff has {children} PIDs (maximum {MAX_TRANSIENT_CHILDREN})"
            ),
            Self::InvalidChildPid { index, pid } => {
                write!(
                    f,
                    "invalid transient child PID {pid} at handoff entry {index}"
                )
            }
            Self::DuplicateChildPid(pid) => {
                write!(f, "duplicate transient child PID {pid} in restart handoff")
            }
        }
    }
}

impl std::error::Error for TransientChildHandoffError {}

impl TransientChildRestartHandoff {
    fn new(parent_pid: u32, pids: Vec<u32>) -> Result<Self, TransientChildHandoffError> {
        let wire = WireTransientChildHandoff {
            version: TRANSIENT_CHILD_HANDOFF_VERSION,
            parent_pid,
            pids,
        };
        validate_transient_child_handoff(&wire)?;
        Ok(Self {
            parent_pid: wire.parent_pid,
            pids: wire.pids,
        })
    }

    pub(crate) fn encode(&self) -> Result<String, TransientChildHandoffError> {
        let payload = serde_json::to_string(&WireTransientChildHandoff {
            version: TRANSIENT_CHILD_HANDOFF_VERSION,
            parent_pid: self.parent_pid,
            pids: self.pids.clone(),
        })
        .map_err(|error| TransientChildHandoffError::InvalidJson(error.to_string()))?;
        if payload.len() > MAX_TRANSIENT_CHILD_HANDOFF_BYTES {
            return Err(TransientChildHandoffError::PayloadTooLarge {
                bytes: payload.len(),
            });
        }
        Ok(payload)
    }

    pub(crate) fn decode_for_parent_pid(
        payload: &str,
        expected_parent_pid: u32,
    ) -> Result<Self, TransientChildHandoffError> {
        if payload.len() > MAX_TRANSIENT_CHILD_HANDOFF_BYTES {
            return Err(TransientChildHandoffError::PayloadTooLarge {
                bytes: payload.len(),
            });
        }
        let wire: WireTransientChildHandoff = serde_json::from_str(payload)
            .map_err(|error| TransientChildHandoffError::InvalidJson(error.to_string()))?;
        validate_transient_child_handoff(&wire)?;
        if wire.parent_pid != expected_parent_pid {
            return Err(TransientChildHandoffError::ParentPidMismatch {
                expected: expected_parent_pid,
                actual: wire.parent_pid,
            });
        }
        Ok(Self {
            parent_pid: wire.parent_pid,
            pids: wire.pids,
        })
    }
}

fn validate_transient_child_handoff(
    wire: &WireTransientChildHandoff,
) -> Result<(), TransientChildHandoffError> {
    if wire.version != TRANSIENT_CHILD_HANDOFF_VERSION {
        return Err(TransientChildHandoffError::UnsupportedVersion(wire.version));
    }
    if wire.parent_pid == 0 || wire.parent_pid > i32::MAX as u32 {
        return Err(TransientChildHandoffError::InvalidParentPid(
            wire.parent_pid,
        ));
    }
    if wire.pids.len() > MAX_TRANSIENT_CHILDREN {
        return Err(TransientChildHandoffError::TooManyChildren {
            children: wire.pids.len(),
        });
    }

    let mut seen = HashSet::with_capacity(wire.pids.len());
    for (index, &pid) in wire.pids.iter().enumerate() {
        if pid == 0 || pid > i32::MAX as u32 || pid == wire.parent_pid {
            return Err(TransientChildHandoffError::InvalidChildPid { index, pid });
        }
        if !seen.insert(pid) {
            return Err(TransientChildHandoffError::DuplicateChildPid(pid));
        }
    }
    Ok(())
}

/// Owns only fire-and-forget children explicitly launched by JWM.
///
/// Keeping the `Child` handles makes reaping backend-neutral and prevents a
/// broad `waitpid(-1, ...)` from consuming exit status owned by bars, ffmpeg,
/// or another subsystem.
#[derive(Debug, Default)]
pub(crate) struct TransientChildSupervisor {
    children: Vec<Child>,
    /// Children inherited across `exec`. They remain exact children of the
    /// unchanged JWM process PID, but stable Rust cannot reconstruct `Child`.
    inherited_pids: Vec<u32>,
}

impl TransientChildSupervisor {
    fn supervise(&mut self, child: Child) {
        self.children.push(child);
    }

    fn restart_handoff(
        &self,
        parent_pid: u32,
    ) -> Result<TransientChildRestartHandoff, TransientChildHandoffError> {
        let pids = self
            .children
            .iter()
            .map(Child::id)
            .chain(self.inherited_pids.iter().copied())
            .collect();
        TransientChildRestartHandoff::new(parent_pid, pids)
    }

    fn install_restart_handoff(
        &mut self,
        handoff: TransientChildRestartHandoff,
        current_pid: u32,
    ) -> Result<(), TransientChildHandoffError> {
        if handoff.parent_pid != current_pid {
            return Err(TransientChildHandoffError::ParentPidMismatch {
                expected: current_pid,
                actual: handoff.parent_pid,
            });
        }

        let owned: HashSet<_> = self.children.iter().map(Child::id).collect();
        for pid in handoff.pids {
            if !owned.contains(&pid) && !self.inherited_pids.contains(&pid) {
                self.inherited_pids.push(pid);
            }
        }
        Ok(())
    }

    /// Reap exited children through their owning handles and return every PID
    /// whose handle was retired. A status-query error also retires the handle:
    /// without a usable owner handle, retrying forever would retain stale PIDs.
    fn reap_exited(&mut self) -> Vec<u32> {
        let mut retired = Vec::new();
        self.children.retain_mut(|child| {
            let pid = child.id();
            match child.try_wait() {
                Ok(None) => true,
                Ok(Some(status)) => {
                    info!("Transient child {pid} exited with status {status}");
                    retired.push(pid);
                    false
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => true,
                Err(error) => {
                    warn!("Could not query transient child {pid}: {error}; retiring its handle");
                    retired.push(pid);
                    false
                }
            }
        });

        self.inherited_pids.retain(|&raw_pid| {
            let pid = Pid::from_raw(raw_pid as i32);
            match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::StillAlive) => true,
                Ok(WaitStatus::Exited(_, status)) => {
                    info!("Inherited transient child {raw_pid} exited with status {status}");
                    retired.push(raw_pid);
                    false
                }
                Ok(WaitStatus::Signaled(_, signal, _)) => {
                    info!("Inherited transient child {raw_pid} exited on signal {signal}");
                    retired.push(raw_pid);
                    false
                }
                Ok(_) => true,
                Err(Errno::EINTR) => true,
                Err(Errno::ECHILD) => {
                    warn!(
                        "Inherited transient PID {raw_pid} is no longer a child; retiring exact-PID ownership"
                    );
                    retired.push(raw_pid);
                    false
                }
                Err(error) => {
                    warn!("Could not reap inherited transient child {raw_pid}: {error}");
                    true
                }
            }
        });
        retired
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.children.is_empty() && self.inherited_pids.is_empty()
    }
}

impl Jwm {
    fn is_smithay_backend(backend: &dyn Backend) -> bool {
        #[allow(unused_mut)]
        let mut is_smithay = Self::is_udev_backend(backend);
        #[cfg(feature = "backend-wayland-nested")]
        {
            is_smithay = is_smithay
                || backend
                    .as_any()
                    .is::<crate::backend::wayland_x11::backend::WaylandX11Backend>()
                || backend
                    .as_any()
                    .is::<crate::backend::wayland_winit::backend::WaylandWinitBackend>();
        }
        is_smithay
    }

    /// Returns `true` if `backend` is the udev/KMS backend (no Xwayland, no X11 DISPLAY).
    pub(super) fn is_udev_backend(backend: &dyn Backend) -> bool {
        #[cfg(feature = "backend-wayland-udev")]
        {
            backend
                .as_any()
                .is::<crate::backend::wayland_udev::backend::UdevBackend>()
        }
        #[cfg(not(feature = "backend-wayland-udev"))]
        {
            let _ = backend;
            false
        }
    }

    /// Set Wayland-related environment variables on a child `Command` so that
    /// toolkits can connect to this compositor.  When running the udev backend
    /// we propagate the XWayland DISPLAY so X11 apps can connect.
    pub(super) fn setup_smithay_child_env(command: &mut Command, backend: &dyn Backend) {
        // Share the session's Xcursor theme/size with every launched client so
        // its *own* windows use the same pointer. On X11 the WM cannot re-cursor
        // a client's content window; libXcursor in the client reads these env
        // vars (or the root RESOURCE_MANAGER the XCB backend also publishes).
        // Idempotent on Wayland, where the session env already carries them.
        let (cursor_theme, cursor_size) = crate::config::CONFIG.load().resolved_cursor();
        command.env("XCURSOR_THEME", &cursor_theme);
        command.env("XCURSOR_SIZE", cursor_size.to_string());

        if Self::is_smithay_backend(backend) {
            if let Ok(v) = std::env::var("WAYLAND_DISPLAY") {
                command.env("WAYLAND_DISPLAY", &v);
            }
            if let Ok(v) = std::env::var("XDG_RUNTIME_DIR") {
                command.env("XDG_RUNTIME_DIR", &v);
            }
            if std::env::var_os("XDG_SESSION_TYPE").is_none() {
                command.env("XDG_SESSION_TYPE", "wayland");
            }
            command.env(
                "XDG_CURRENT_DESKTOP",
                std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_else(|_| "jwm".to_string()),
            );
            command.env(
                "XDG_SESSION_DESKTOP",
                std::env::var("XDG_SESSION_DESKTOP").unwrap_or_else(|_| "jwm".to_string()),
            );
            command.env(
                "DESKTOP_SESSION",
                std::env::var("DESKTOP_SESSION").unwrap_or_else(|_| "jwm".to_string()),
            );
            if std::env::var_os("WINIT_UNIX_BACKEND").is_none() {
                command.env("WINIT_UNIX_BACKEND", "wayland");
            }
        }
        if Self::is_udev_backend(backend) {
            // With XWayland running, DISPLAY is set to e.g. ":0" and is valid.
            // Propagate it so X11 apps can connect via XWayland.
            if let Ok(display) = std::env::var("DISPLAY") {
                command.env("DISPLAY", &display);
            }
            // In nested mode (JWM running inside another Wayland compositor),
            // backend.rs already cleared DBUS_SESSION_BUS_ADDRESS from the process
            // env so children don't reach the parent compositor's session bus
            // (gnome-terminal-server in the parent would steal the window).
            // In primary-session mode (launched from a login manager), the env
            // var holds the real session bus address that children actually need.
            // Propagate whatever the process env says: if empty, isolate; if set,
            // let children use it (e.g. gnome-terminal-server activation).
            let dbus_addr = std::env::var("DBUS_SESSION_BUS_ADDRESS").unwrap_or_default();
            if dbus_addr.is_empty() {
                command.env("DBUS_SESSION_BUS_ADDRESS", "");
                // GTK4 apps block indefinitely on IBus/fcitx5 D-Bus negotiation
                // when a bus is reachable. Only suppress IM in nested/no-bus mode.
                command.env("GTK_IM_MODULE", "none");
                command.env("QT_IM_MODULE", "none");
                command.env("XMODIFIERS", "");
            }
            // In unprivileged DRM sessions the GTK4 GSK GL renderer uses EGL
            // to render into wl_egl_window buffers (DMA-buf or wl_drm), but
            // jwm in nested/unprivileged mode can't complete the DMA-buf
            // feedback exchange, so those buffers contain zero pixels (black).
            // GSK_RENDERER=cairo forces CPU Cairo rendering into plain wl_shm
            // buffers which always contain correct content regardless of DRM
            // master status.  Disable vulkan+dmabuf to prevent feedback hangs.
            // NOTE: GTK4 apps here run with GDK paths for Vulkan/DMABuf disabled
            // while `GSK_RENDERER=cairo` forces the fallback Cairo+wl_shm path.
            command.env("GSK_RENDERER", "cairo");
            command.env("GDK_DISABLE", "vulkan,dmabuf");
            // GTK3 apps (e.g. terminator/VTE) may use GL via wl_egl_window which
            // also produces DMA-buf buffers with zero pixels in unprivileged mode.
            // GDK_GL=disable turns off the GL renderer in GTK3 so it falls back
            // to Cairo wl_shm, which always has correct pixel content.
            command.env("GDK_GL", "disable");
        }
    }

    /// Apply common child-process isolation: `setsid()` + restore `SIGCHLD` default.
    pub(super) fn apply_child_pre_exec(command: &mut Command) {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(move || {
                setsid();
                let mut sa: sigaction = std::mem::zeroed();
                sigemptyset(&mut sa.sa_mask);
                sa.sa_flags = 0;
                sa.sa_sigaction = SIG_DFL;
                sigaction(SIGCHLD, &sa, std::ptr::null_mut());
                Ok(())
            });
        }
    }

    pub(crate) fn spawn(
        &mut self,
        _backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("[spawn]");

        if matches!(arg, WMArgEnum::StringVec(v) if v.first().is_some_and(|s| s == "jwm-launcher"))
        {
            return self.app_launcher(_backend, &WMArgEnum::Int(0));
        }

        let mut mut_arg: WMArgEnum = arg.clone();
        if let WMArgEnum::StringVec(ref mut v) = mut_arg {
            info!("[spawn] spawning command: {:?}", v);

            let Some(program) = v.first().filter(|program| !program.trim().is_empty()) else {
                return Err("spawn requires a non-empty command program".into());
            };
            let mut command = Command::new(program);
            command.args(&v[1..]);

            Self::setup_smithay_child_env(&mut command, _backend);

            // Redirect child stderr to /tmp/jwm-{name}-stderr.log so Python
            // exceptions and other error output survive when JWM runs as daemon.
            let cmd_name = std::path::Path::new(program)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("child");
            let stderr_path = format!("/tmp/jwm-{}-stderr.log", cmd_name);
            let stderr_file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&stderr_path)
                .map(std::process::Stdio::from)
                .unwrap_or_else(|_| std::process::Stdio::inherit());

            command
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::inherit())
                .stderr(stderr_file);

            Self::apply_child_pre_exec(&mut command);

            match command.spawn() {
                Ok(child) => {
                    let pid = child.id();
                    debug!("[spawn] successfully spawned process with PID: {}", pid);
                    self.supervise_transient_child(child);
                }
                Err(e) => {
                    error!("[spawn] failed to spawn command {:?}: {}", v, e);
                    return Err(e.into());
                }
            }
        }

        Ok(())
    }

    /// Register a JWM-owned fire-and-forget child for backend-neutral reaping.
    pub(crate) fn supervise_transient_child(&mut self, child: Child) {
        self.transient_children.supervise(child);
    }

    /// Capture every still-owned transient PID before dropping this `Jwm` for
    /// an exec restart. `exec` preserves the process PID and child relation,
    /// allowing the replacement supervisor to reap each PID exactly.
    pub(crate) fn capture_transient_child_restart_handoff(
        &mut self,
    ) -> Result<TransientChildRestartHandoff, TransientChildHandoffError> {
        self.reap_transient_children();
        self.transient_children.restart_handoff(std::process::id())
    }

    /// Install a validated same-process restart handoff. This is also used by
    /// the in-process fallback after `exec` fails and the old `Jwm` is gone.
    pub(crate) fn install_transient_child_restart_handoff(
        &mut self,
        handoff: TransientChildRestartHandoff,
    ) -> Result<(), TransientChildHandoffError> {
        self.transient_children
            .install_restart_handoff(handoff, std::process::id())
    }

    /// Poll only JWM-owned transient children. This is safe to call both from
    /// the common update loop and as an optional SIGCHLD fast path.
    pub(super) fn reap_transient_children(&mut self) {
        for pid in self.transient_children.reap_exited() {
            // Scratchpad spawns share this supervisor. If one dies before its
            // window maps, release the pending-name gate immediately.
            self.remove_exited_pending_scratchpad(pid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    fn wait_until_reaped(supervisor: &mut TransientChildSupervisor) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !supervisor.is_empty() && Instant::now() < deadline {
            supervisor.reap_exited();
            if !supervisor.is_empty() {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    fn wait_until_zombie(pid: u32) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            let state = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| {
                    stat.rsplit_once(") ")
                        .and_then(|(_, suffix)| suffix.chars().next())
                });
            if state == Some('Z') {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("child {pid} did not become a zombie before the deadline");
    }

    #[test]
    fn transient_true_is_removed_and_reaped_via_its_child_handle() {
        let child = Command::new("/bin/true").spawn().unwrap();
        let pid = child.id();
        let mut supervisor = TransientChildSupervisor::default();
        supervisor.supervise(child);

        wait_until_reaped(&mut supervisor);

        assert!(supervisor.is_empty());
        assert_eq!(
            waitpid(Pid::from_raw(pid as i32), Some(WaitPidFlag::WNOHANG)),
            Err(Errno::ECHILD)
        );
    }

    #[test]
    fn long_lived_child_handoff_reaps_after_old_supervisor_is_dropped() {
        let mut child = Command::new("/bin/cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let child_stdin = child.stdin.take().unwrap();
        let mut old = TransientChildSupervisor::default();
        old.supervise(child);

        let handoff = old.restart_handoff(std::process::id()).unwrap();
        let payload = handoff.encode().unwrap();
        let decoded =
            TransientChildRestartHandoff::decode_for_parent_pid(&payload, std::process::id())
                .unwrap();
        drop(old);

        let mut replacement = TransientChildSupervisor::default();
        replacement
            .install_restart_handoff(decoded, std::process::id())
            .unwrap();
        drop(child_stdin);
        wait_until_reaped(&mut replacement);

        assert!(replacement.is_empty());
        assert_eq!(
            waitpid(Pid::from_raw(pid as i32), Some(WaitPidFlag::WNOHANG)),
            Err(Errno::ECHILD)
        );
    }

    #[test]
    fn transient_handoff_payload_is_versioned_bounded_and_pid_scoped() {
        let handoff = TransientChildRestartHandoff::new(8123, vec![9001, 9002]).unwrap();
        let payload = handoff.encode().unwrap();

        assert_eq!(
            TransientChildRestartHandoff::decode_for_parent_pid(&payload, 8123).unwrap(),
            handoff
        );
        assert!(matches!(
            TransientChildRestartHandoff::decode_for_parent_pid(&payload, 8124),
            Err(TransientChildHandoffError::ParentPidMismatch { .. })
        ));

        let wrong_version = serde_json::json!({
            "version": TRANSIENT_CHILD_HANDOFF_VERSION + 1,
            "parent_pid": 8123,
            "pids": [9001]
        })
        .to_string();
        assert!(matches!(
            TransientChildRestartHandoff::decode_for_parent_pid(&wrong_version, 8123),
            Err(TransientChildHandoffError::UnsupportedVersion(_))
        ));

        let duplicate = serde_json::json!({
            "version": TRANSIENT_CHILD_HANDOFF_VERSION,
            "parent_pid": 8123,
            "pids": [9001, 9001]
        })
        .to_string();
        assert!(matches!(
            TransientChildRestartHandoff::decode_for_parent_pid(&duplicate, 8123),
            Err(TransientChildHandoffError::DuplicateChildPid(9001))
        ));

        let too_many = serde_json::json!({
            "version": TRANSIENT_CHILD_HANDOFF_VERSION,
            "parent_pid": 8123,
            "pids": (1..=MAX_TRANSIENT_CHILDREN + 1).collect::<Vec<_>>()
        })
        .to_string();
        assert!(matches!(
            TransientChildRestartHandoff::decode_for_parent_pid(&too_many, 8123),
            Err(TransientChildHandoffError::TooManyChildren { .. })
        ));

        let oversized = "x".repeat(MAX_TRANSIENT_CHILD_HANDOFF_BYTES + 1);
        assert!(matches!(
            TransientChildRestartHandoff::decode_for_parent_pid(&oversized, 8123),
            Err(TransientChildHandoffError::PayloadTooLarge { .. })
        ));
    }

    #[test]
    fn external_child_owner_keeps_its_exit_status() {
        let mut external = Command::new("/bin/true").spawn().unwrap();
        let external_pid = external.id();
        let mut tracked = Command::new("/bin/cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let tracked_stdin = tracked.stdin.take().unwrap();
        let mut supervisor = TransientChildSupervisor::default();
        supervisor.supervise(tracked);

        // Keep the tracked child alive while the external child is definitely
        // waitable. A process-wide waitpid(-1) would steal the external status.
        wait_until_zombie(external_pid);
        supervisor.reap_exited();

        assert!(!supervisor.is_empty());
        assert!(external.wait().unwrap().success());

        drop(tracked_stdin);
        wait_until_reaped(&mut supervisor);
        assert!(supervisor.is_empty());
    }
}
