//! One coordinator for the graphical session, started by JWM, never by a terminal.
use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    io,
    os::unix::{
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        net::UnixStream,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
};

static STARTED: AtomicBool = AtomicBool::new(false);

struct Plan {
    session: String,
    runtime: PathBuf,
    socket: PathBuf,
    profile: PathBuf,
    daemon: PathBuf,
    desktop: PathBuf,
}

fn local_session(enabled: bool, ssh: bool, custom_socket: bool, session: &str) -> bool {
    enabled && !ssh && !custom_socket && companion_protocol::identifier(session)
}

fn executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn daemon_path(desktop: &Path, home: Option<&Path>, path: Option<OsString>) -> Option<PathBuf> {
    desktop
        .parent()
        .map(|p| p.join("jcompaniond"))
        .into_iter()
        .chain(home.map(|p| p.join(".local/bin/jcompaniond")))
        .chain(path.into_iter().flat_map(|p| {
            std::env::split_paths(&p)
                .map(|p| p.join("jcompaniond"))
                .collect::<Vec<_>>()
        }))
        .find(|p| executable(p))
}

fn plan(enabled: bool) -> io::Result<Option<Plan>> {
    let session = crate::child_environment::var("XDG_SESSION_ID").unwrap_or_default();
    if !local_session(
        enabled,
        std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some(),
        crate::child_environment::var_os("EVA_COMPANION_SOCKET").is_some(),
        &session,
    ) {
        return Ok(None);
    }
    let runtime = crate::child_environment::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("XDG_RUNTIME_DIR is required"))?;
    let m = fs::symlink_metadata(&runtime)?;
    if !runtime.is_absolute()
        || !m.is_dir()
        || m.file_type().is_symlink()
        || m.uid() != unsafe { libc::geteuid() }
        || m.mode() & 0o077 != 0
    {
        return Err(io::Error::other(
            "graphical runtime directory must be owned and private",
        ));
    }
    let home = crate::child_environment::var_os("HOME").map(PathBuf::from);
    let state = crate::child_environment::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home.as_ref().map(|p| p.join(".local/state")))
        .ok_or_else(|| io::Error::other("state directory is unavailable"))?;
    let desktop = std::env::current_exe()?.canonicalize()?;
    let daemon = match crate::child_environment::var_os("JWM_COMPANION_DAEMON") {
        Some(p) => {
            let p = PathBuf::from(p);
            if !p.is_absolute() || !executable(&p) {
                return Err(io::Error::other(
                    "JWM_COMPANION_DAEMON must name an executable absolute path",
                ));
            }
            p
        }
        None => daemon_path(
            &desktop,
            home.as_deref(),
            crate::child_environment::var_os("PATH"),
        )
        .ok_or_else(|| {
            io::Error::other("jcompaniond is not installed beside JWM, in ~/.local/bin, or on PATH")
        })?,
    };
    Ok(Some(Plan {
        session,
        socket: runtime.join("jcompanion/eva.sock"),
        runtime,
        profile: state.join("jcompanion-eva"),
        daemon,
        desktop,
    }))
}

fn launch(plan: &Plan) -> io::Result<Option<Child>> {
    match UnixStream::connect(&plan.socket) {
        Ok(_) => return Ok(None),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) => {}
        Err(e) => return Err(e),
    }
    // Preserve the coordinator's authority journal and its own exclusive locks.
    // A stale/uncertain journal is never deleted or replaced to make startup work.
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&plan.profile)?;
    let m = fs::symlink_metadata(&plan.profile)?;
    if !m.is_dir()
        || m.file_type().is_symlink()
        || m.uid() != unsafe { libc::geteuid() }
        || m.mode() & 0o077 != 0
    {
        return Err(io::Error::other(
            "Eva profile must be an owned private directory",
        ));
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(plan.profile.join("coordinator.log"))?;
    let mut command = Command::new(&plan.daemon);
    command
        .arg(&plan.profile)
        .env_clear()
        .env("XDG_SESSION_ID", &plan.session)
        .env("XDG_RUNTIME_DIR", &plan.runtime)
        .env("EVA_COMPANION_DESKTOP_EXE", &plan.desktop)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // Do not import model credentials or JWM's one-shot restart environment.
    for key in [
        "HOME",
        "USER",
        "LOGNAME",
        "PATH",
        "LANG",
        "LC_ALL",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
    ] {
        if let Some(value) = crate::child_environment::var_os(key) {
            command.env(key, value);
        }
    }
    crate::external_command::unblock_sigchld_in_child(&mut command);
    command.spawn().map(Some)
}

pub(crate) fn start(enabled: bool) {
    if !enabled || STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    if let Err(e) = std::thread::Builder::new()
        .name("jwm-companion-startup".into())
        .spawn(move || {
            let result = plan(enabled).and_then(|plan| match plan {
                Some(plan) => launch(&plan),
                None => Ok(None),
            });
            match result {
                Ok(Some(child)) => {
                    log::info!("[companion] started session coordinator pid={}", child.id())
                }
                Ok(None) => {}
                Err(e) => log::warn!("[companion] coordinator startup failed: {e}"),
            }
        })
    {
        log::warn!("[companion] cannot start coordinator worker: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_is_local_and_does_not_take_over_explicit_socket_management() {
        assert!(local_session(true, false, false, "c2"));
        assert!(!local_session(false, false, false, "c2"));
        assert!(!local_session(true, true, false, "c2"));
        assert!(!local_session(true, false, true, "c2"));
        assert!(!local_session(true, false, false, ""));
        assert!(!local_session(true, false, false, "../other"));
    }

    #[test]
    fn a_running_coordinator_is_reused_without_spawning_or_touching_its_profile() {
        use std::os::unix::net::UnixListener;
        let root = std::env::temp_dir().join(format!(
            "jwm-companion-reuse-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let socket = root.join("eva.sock");
        let _listener = UnixListener::bind(&socket).unwrap();
        let profile = root.join("must-not-be-created");
        let plan = Plan {
            session: "test".into(),
            runtime: root.clone(),
            socket,
            profile: profile.clone(),
            daemon: root.join("missing-daemon"),
            desktop: root.join("jwm"),
        };
        assert!(launch(&plan).unwrap().is_none());
        assert!(!profile.exists());
        fs::remove_dir_all(root).unwrap();
    }
}
