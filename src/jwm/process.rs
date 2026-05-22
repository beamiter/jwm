use crate::backend::api::Backend;
use crate::jwm::WMArgEnum;
use libc::{SIG_DFL, SIGCHLD, setsid, sigaction, sigemptyset};
use log::{debug, error, info};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use std::process::Command;

use super::Jwm;

impl Jwm {
    fn is_smithay_backend(backend: &dyn Backend) -> bool {
        backend
            .as_any()
            .is::<crate::backend::wayland_udev::backend::UdevBackend>()
            || backend
                .as_any()
                .is::<crate::backend::wayland_x11::backend::WaylandX11Backend>()
            || backend
                .as_any()
                .is::<crate::backend::wayland_winit::backend::WaylandWinitBackend>()
    }

    /// Returns `true` if `backend` is the udev/KMS backend (no Xwayland, no X11 DISPLAY).
    pub(super) fn is_udev_backend(backend: &dyn Backend) -> bool {
        backend
            .as_any()
            .is::<crate::backend::wayland_udev::backend::UdevBackend>()
    }

    /// Set Wayland-related environment variables on a child `Command` so that
    /// toolkits can connect to this compositor.  When running the udev backend
    /// we propagate the XWayland DISPLAY so X11 apps can connect.
    pub(super) fn setup_smithay_child_env(command: &mut Command, backend: &dyn Backend) {
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
            // In unprivileged DRM sessions GTK4's GDK Wayland backend binds
            // zwp_linux_dmabuf_v1 and sends get_default_feedback() — a path
            // independent of GL that hangs when the compositor can't respond
            // with valid dmabuf feedback without DRM master.  Disabling gl,
            // vulkan, and dmabuf forces pure wl_shm buffer allocation.
            command.env("GDK_DISABLE", "gl,vulkan,dmabuf");
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

    fn resolve_launcher(cmd: &str, backend: &dyn Backend) -> Vec<String> {
        if cmd != "jwm-launcher" {
            return vec![cmd.to_string()];
        }
        if Self::is_smithay_backend(backend) {
            for (bin, args) in [("fuzzel", vec![]), ("wofi", vec!["--show", "run"])] {
                if std::process::Command::new("which").arg(bin).output().map(|o| o.status.success()).unwrap_or(false) {
                    let mut v = vec![bin.to_string()];
                    v.extend(args.iter().map(|s| s.to_string()));
                    return v;
                }
            }
        }
        vec!["dmenu_run".to_string()]
    }

    pub(crate) fn spawn(
        &mut self,
        _backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("[spawn]");

        let mut mut_arg: WMArgEnum = arg.clone();
        if let WMArgEnum::StringVec(ref mut v) = mut_arg {
            if v.first().map(|s| s.as_str()) == Some("jwm-launcher") {
                *v = Self::resolve_launcher("jwm-launcher", _backend);
            }
            info!("[spawn] spawning command: {:?}", v);

            let mut command = Command::new(&v[0]);
            command.args(&v[1..]);

            Self::setup_smithay_child_env(&mut command, _backend);

            // Redirect child stderr to /tmp/jwm-{name}-stderr.log so Python
            // exceptions and other error output survive when JWM runs as daemon.
            let cmd_name = std::path::Path::new(&v[0])
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
                    debug!(
                        "[spawn] successfully spawned process with PID: {}",
                        child.id()
                    );
                }
                Err(e) => {
                    error!("[spawn] failed to spawn command {:?}: {}", v, e);
                    return Err(e.into());
                }
            }
        }

        Ok(())
    }

    pub(super) fn reap_zombies(&mut self) {
        // 使用 WNOHANG 循环回收所有已退出的子进程
        loop {
            match waitpid(None, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(pid, status)) => {
                    info!("Child process {} exited with status {}", pid, status);
                }
                Ok(WaitStatus::Signaled(pid, sig, _)) => {
                    info!("Child process {} killed by signal {:?}", pid, sig);
                }
                // StillAlive 表示还有子进程在运行，Break 退出循环
                Ok(WaitStatus::StillAlive) => break,
                // Err 通常表示没有子进程了 (ECHILD)，也退出循环
                Err(_) => break,
                _ => break,
            }
        }
    }
}
