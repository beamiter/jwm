//! Optional frontend logging initialization.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use flexi_logger::{Cleanup, Criterion, Duplicate, FileSpec, Logger, Naming};

/// Initialize the process-global logger used by bar frontends.
///
/// Logs use an owner-only per-user directory below `/var/tmp` or the temporary
/// directory. If neither is safe, logging falls back to stderr. `shared_path` is included in
/// the basename so one frontend process per monitor receives a distinct file.
pub fn init(program_name: &str, shared_path: &str) -> Result<()> {
    let log_spec = std::env::var("RUST_LOG").unwrap_or_else(|_| "debug".to_owned());
    let log_dir = match preferred_log_directory() {
        Ok(directory) => directory,
        Err(error) => {
            Logger::try_with_str(log_spec)?.start()?;
            log::warn!("Private log directory unavailable; logging to stderr: {error}");
            return Ok(());
        }
    };
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let log_filename = format!(
        "{}_{}_p{}",
        frontend_basename(program_name, shared_path),
        timestamp,
        std::process::id()
    );
    Logger::try_with_str(log_spec)?
        .format_for_files(flexi_logger::detailed_format)
        .format_for_stderr(flexi_logger::colored_opt_format)
        .log_to_file(
            FileSpec::default()
                .directory(&log_dir)
                .basename(log_filename)
                .suffix("log"),
        )
        .duplicate_to_stdout(Duplicate::Info)
        .rotate(
            Criterion::Size(10_000_000),
            Naming::Numbers,
            Cleanup::KeepLogFiles(5),
        )
        .start()?;

    log::info!("Log directory: {}", log_dir.display());
    Ok(())
}

#[cfg(unix)]
fn private_log_directory(path: &Path, uid: u32) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != uid
        || metadata.permissions().mode() & 0o777 != 0o700
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "log directory must be a real directory owned by this user with mode 0700",
        ));
    }
    Ok(path.to_owned())
}

#[cfg(unix)]
fn preferred_log_directory() -> std::io::Result<PathBuf> {
    // SAFETY: geteuid has no preconditions and does not mutate process state.
    let uid = unsafe { libc::geteuid() };
    let name = format!("jwm-logs-{uid}");
    private_log_directory(&Path::new("/var/tmp").join(&name), uid)
        .or_else(|_| private_log_directory(&std::env::temp_dir().join(name), uid))
}

#[cfg(not(unix))]
fn preferred_log_directory() -> std::io::Result<PathBuf> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "owner-only file logging is not implemented on this platform",
    ))
}

fn frontend_basename(program_name: &str, shared_path: &str) -> String {
    if shared_path.is_empty() {
        return program_name.to_owned();
    }

    Path::new(shared_path)
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| format!("{program_name}_{name}"))
        .unwrap_or_else(|| program_name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::frontend_basename;

    #[test]
    fn basename_distinguishes_shared_monitor_paths() {
        assert_eq!(frontend_basename("xbar", ""), "xbar");
        assert_eq!(
            frontend_basename("xbar", "/dev/shm/jwm_bar_p4242_mon_2"),
            "xbar_jwm_bar_p4242_mon_2"
        );
    }
    #[cfg(unix)]
    #[test]
    fn private_directory_rejects_symlinks_and_unsafe_permissions() {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt, symlink};
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "jwm-log-test-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .unwrap();
        // SAFETY: geteuid has no preconditions.
        let uid = unsafe { libc::geteuid() };
        let private = root.join("private");
        assert_eq!(
            super::private_log_directory(&private, uid).unwrap(),
            private
        );
        assert!(super::private_log_directory(&private, uid).is_ok());
        assert!(super::private_log_directory(&private, uid.wrapping_add(1)).is_err());
        let link = root.join("link");
        symlink(&private, &link).unwrap();
        assert!(super::private_log_directory(&link, uid).is_err());
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(super::private_log_directory(&private, uid).is_err());
        assert_eq!(
            std::fs::metadata(&private).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let file = root.join("file");
        std::fs::write(&file, "unchanged").unwrap();
        assert!(super::private_log_directory(&file, uid).is_err());
        assert_eq!(std::fs::read_to_string(file).unwrap(), "unchanged");
        std::fs::remove_dir_all(root).unwrap();
    }
}
