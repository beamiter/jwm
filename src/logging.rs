//! Optional frontend logging initialization.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use flexi_logger::{Cleanup, Criterion, Duplicate, FileSpec, Logger, Naming};

/// Initialize the process-global logger used by bar frontends.
///
/// Logs are written below `/var/tmp/jwm` when that directory is available and
/// fall back to the current directory otherwise.  `shared_path` is included in
/// the basename so one frontend process per monitor receives a distinct file.
pub fn init(program_name: &str, shared_path: &str) -> Result<()> {
    let log_dir = preferred_log_directory();
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let log_filename = format!(
        "{}_{}",
        frontend_basename(program_name, shared_path),
        timestamp
    );
    let log_spec = std::env::var("RUST_LOG").unwrap_or_else(|_| "debug".to_owned());

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

fn preferred_log_directory() -> std::path::PathBuf {
    let preferred = Path::new("/var/tmp/jwm");
    if std::fs::create_dir_all(preferred).is_ok()
        && std::fs::metadata(preferred)
            .map(|metadata| metadata.is_dir())
            .unwrap_or(false)
    {
        preferred.to_owned()
    } else {
        Path::new(".").to_owned()
    }
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
            frontend_basename("xbar", "/dev/shm/jwm_bar_mon_2"),
            "xbar_jwm_bar_mon_2"
        );
    }
}
