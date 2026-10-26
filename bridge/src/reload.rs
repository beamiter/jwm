//! Retire only this user's replaced notification bridge after an upgrade.
//! Merely replacing the executable does not release its D-Bus name: a stale
//! process can keep applications waiting for the default 25-second timeout.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use zbus::{Connection, fdo::DBusProxy, names::BusName};

use crate::notifications::{NAME, PATH};

type ReloadResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn bus_address() -> Option<String> {
    match std::env::var("DBUS_SESSION_BUS_ADDRESS") {
        Ok(address) => return (!address.is_empty()).then_some(address),
        Err(std::env::VarError::NotUnicode(_)) => return None,
        Err(std::env::VarError::NotPresent) => {}
    }
    // sudo usually removes the bus address. Resolve only the invoking user's
    // existing user bus, never autolaunch a new bus or scan other users.
    let uid = unsafe { libc::geteuid() };
    let directory = PathBuf::from(format!("/run/user/{uid}"));
    if std::fs::metadata(&directory).ok()?.uid() != uid {
        return None;
    }
    let socket = directory.join("bus");
    std::fs::metadata(&socket)
        .ok()?
        .file_type()
        .is_socket()
        .then(|| format!("unix:path={}", socket.display()))
}

fn is_replaced_bridge(executable: &Path, running: (u64, u64), installed: (u64, u64)) -> bool {
    matches!(
        executable.file_name().and_then(|name| name.to_str()),
        Some("jwm-bridge" | "jwm-bridge (deleted)")
    ) && running != installed
}

fn open_process(pid: u32) -> io::Result<OwnedFd> {
    // pidfds pin the exact process. Do not fall back to kill(pid), which could
    // signal a different process if the owner exits and its PID is reused.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

fn retire_process(process: &OwnedFd) -> io::Result<()> {
    if unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            process.as_raw_fd(),
            libc::SIGTERM,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut descriptor = libc::pollfd {
        fd: process.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "old notification bridge did not exit",
            ));
        }
        let ready =
            unsafe { libc::poll(&mut descriptor, 1, remaining.as_millis().min(2000) as i32) };
        if ready > 0 {
            return Ok(());
        }
        if ready < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return Err(io::Error::last_os_error());
        }
    }
}

async fn reload_on(connection: &Connection) -> ReloadResult<bool> {
    let bus = DBusProxy::new(connection).await?;
    let name = BusName::try_from(NAME)?;
    let owner = match bus.get_name_owner(name.clone()).await {
        Ok(owner) => owner,
        Err(zbus::fdo::Error::NameHasNoOwner(_)) => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let uid = unsafe { libc::geteuid() };
    if bus.get_connection_unix_user(owner.clone().into()).await? != uid {
        return Ok(false);
    }
    let pid = bus
        .get_connection_unix_process_id(owner.clone().into())
        .await?;
    if pid == std::process::id() {
        return Ok(false);
    }
    let process = match open_process(pid) {
        Ok(process) => process,
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let proc = PathBuf::from(format!("/proc/{pid}"));
    if std::fs::metadata(&proc)?.uid() != uid {
        return Ok(false);
    }
    let executable = std::fs::read_link(proc.join("exe"))?;
    let running = std::fs::metadata(proc.join("exe"))?;
    let installed = std::fs::metadata(std::env::current_exe()?)?;
    if !is_replaced_bridge(
        &executable,
        (running.dev(), running.ino()),
        (installed.dev(), installed.ino()),
    ) {
        // Another notification server, or this exact build already running.
        return Ok(false);
    }
    match bus.get_name_owner(name.clone()).await {
        Ok(current) if current == owner => {}
        Ok(_) | Err(zbus::fdo::Error::NameHasNoOwner(_)) => return Ok(false),
        Err(error) => return Err(error.into()),
    }
    retire_process(&process)?;
    // Process exit and the bus observing its closed connection are separate
    // events. Do not send discovery back to an owner already on its way out.
    loop {
        match bus.get_name_owner(name.clone()).await {
            Ok(current) if current == owner => tokio::time::sleep(Duration::from_millis(10)).await,
            Ok(_) | Err(zbus::fdo::Error::NameHasNoOwner(_)) => break,
            Err(error) => return Err(error.into()),
        }
    }
    // Let the updated activation file start the installed executable. This is
    // a discovery call, not a notification posted to the user's desktop.
    let reply = connection
        .call_method(Some(NAME), PATH, Some(NAME), "GetCapabilities", &())
        .await?;
    let _: Vec<String> = reply.body().deserialize()?;
    Ok(true)
}

pub async fn run() -> ReloadResult<()> {
    let Some(address) = bus_address() else {
        log::info!("no existing user session bus; notification bridge reload skipped");
        return Ok(());
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        let connection = zbus::connection::Builder::address(address.as_str())?
            .build()
            .await?;
        if reload_on(&connection).await? {
            log::info!("reloaded the replaced notification bridge; GetCapabilities is responding");
        } else {
            log::info!("no replaced notification bridge owned by this user");
        }
        Ok(())
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacement_detection_excludes_other_notification_servers_and_current_build() {
        let old = (1, 100);
        let new = (1, 101);
        assert!(is_replaced_bridge(
            Path::new("/usr/local/bin/jwm-bridge (deleted)"),
            old,
            new
        ));
        assert!(is_replaced_bridge(
            Path::new("/usr/local/lib/jwm/versions/old/bin/jwm-bridge"),
            old,
            new
        ));
        assert!(!is_replaced_bridge(Path::new("/usr/bin/dunst"), old, new));
        assert!(!is_replaced_bridge(
            Path::new("/usr/bin/gnome-shell"),
            old,
            new
        ));
        assert!(!is_replaced_bridge(
            Path::new("/usr/local/bin/jwm-bridge"),
            new,
            new
        ));
        assert!(!is_replaced_bridge(
            Path::new("/tmp/jwm-bridge-helper"),
            old,
            new
        ));
    }

    #[test]
    fn process_handle_reports_a_gone_process_without_signalling_anything() {
        assert!(open_process(i32::MAX as u32).is_err());
    }

    #[test]
    fn retiring_one_process_handle_leaves_other_processes_running() {
        use std::process::{Child, Command};
        struct ChildGuard(Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut old = ChildGuard(Command::new("sleep").arg("30").spawn().unwrap());
        let mut other = ChildGuard(Command::new("sleep").arg("30").spawn().unwrap());
        let handle = open_process(old.0.id()).unwrap();
        retire_process(&handle).unwrap();
        assert!(!old.0.wait().unwrap().success());
        assert!(other.0.try_wait().unwrap().is_none());
    }
}
