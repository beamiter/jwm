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

    fn resolve_launcher(cmd: &str, backend: &dyn Backend) -> Vec<String> {
        if cmd != "jwm-launcher" {
            return vec![cmd.to_string()];
        }
        if Self::is_smithay_backend(backend) {
            for (bin, args) in [("fuzzel", vec![]), ("wofi", vec!["--show", "run"])] {
                if std::process::Command::new("which")
                    .arg(bin)
                    .output()
                    .map(|o| o.status.success())
                    .unwrap_or(false)
                {
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
        // 回收已退出的子进程。
        //
        // 关键:状态栏(secondary_bars)进程由 `std::process::Child` 句柄拥有,关闭路径
        // 会用 `child.try_wait()` 等待并按 PID 发送 SIGTERM/SIGKILL。若这里用裸
        // `waitpid(None)` 抢先把 bar 进程回收掉,Rust 的 Child 句柄并不知情,后续
        // `try_wait()` 会得到 ECHILD,而那个 PID 可能已被内核复用给无关进程 —— 此时
        // 按 PID 发 SIGKILL 会误杀别人。
        //
        // 因此先用 WNOWAIT 窥探待回收子进程的 PID(不消费):若属于受管的 bar,改为通过
        // 它自己的 Child 句柄回收(保持 Rust 端状态一致);其余瞬时子进程才真正 reap。
        loop {
            let peek = waitpid(None, Some(WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT));
            let pid = match peek {
                Ok(WaitStatus::Exited(pid, _)) | Ok(WaitStatus::Signaled(pid, _, _)) => pid,
                // StillAlive(尚有运行中的子进程但无可回收) / ECHILD(无子进程) / 其它 → 结束
                _ => break,
            };

            let raw = pid.as_raw() as u32;
            let bar_key = self
                .secondary_bars
                .iter()
                .find(|(_, b)| b.child.id() == raw)
                .map(|(k, _)| *k);
            if let Some(key) = bar_key {
                // 通过 Child 句柄回收(内部 waitpid(pid)),Rust 会缓存退出状态,
                // 使关闭路径的 try_wait() 拿到正确结果而非 ECHILD。
                if let Some(bar) = self.secondary_bars.get_mut(&key) {
                    match bar.child.try_wait() {
                        Ok(Some(status)) => info!("Status bar child {} reaped: {:?}", raw, status),
                        _ => {
                            // 兜底:句柄回收异常时仍消费掉该僵尸,避免死循环。
                            let _ = waitpid(pid, Some(WaitPidFlag::WNOHANG));
                        }
                    }
                }
                // 该 bar 已退出,从表中移除:既避免向已死进程的 shm 写状态,
                // 也防止关闭路径按已被内核复用的 PID 误发信号。
                self.secondary_bars.remove(&key);
            } else {
                match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
                    Ok(WaitStatus::Exited(p, status)) => {
                        info!("Child process {} exited with status {}", p, status);
                    }
                    Ok(WaitStatus::Signaled(p, sig, _)) => {
                        info!("Child process {} killed by signal {:?}", p, sig);
                    }
                    _ => {}
                }
            }
        }
    }
}
