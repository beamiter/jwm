//! Shared, bounded parsing for compositor power-supply probes.

use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

pub(crate) const MAX_POWER_SUPPLY_ATTRIBUTE_BYTES: usize = 4 * 1024;
const MAX_POWER_SUPPLY_ENTRIES: usize = 64;

pub(crate) fn read_attribute(path: &Path) -> io::Result<String> {
    let file = OpenOptions::new()
        .read(true)
        // A replaced final component must neither redirect the probe nor
        // block the compositor while opening a FIFO/device.
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "power-supply attribute is not a regular file",
        ));
    }

    let mut bytes = Vec::with_capacity(MAX_POWER_SUPPLY_ATTRIBUTE_BYTES + 1);
    file.take((MAX_POWER_SUPPLY_ATTRIBUTE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_POWER_SUPPLY_ATTRIBUTE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "power-supply attribute exceeds the 4 KiB limit",
        ));
    }
    String::from_utf8(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub(crate) fn parse_percentage(text: &str) -> io::Result<u32> {
    let value = text.trim().parse::<u32>().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("power-supply percentage is invalid: {error}"),
        )
    })?;
    if value > 100 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "power-supply percentage exceeds 100",
        ));
    }
    Ok(value)
}

/// First `/sys/class/power_supply/*` entry whose `type` is `Battery`.
///
/// Multi-battery laptops often expose `BAT1` (or only `BAT1`) rather than
/// `BAT0`; hardcoding a single name silently keeps the compositor on the AC
/// power profile. Enumeration matches the control-center probe in
/// `jwm::features::power`.
pub(crate) fn first_battery_dir(power_supply_root: &Path) -> Option<PathBuf> {
    let mut entries = fs::read_dir(power_supply_root).ok()?.flatten().collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries.into_iter().take(MAX_POWER_SUPPLY_ENTRIES) {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Ok(supply_type) = read_attribute(&path.join("type")) else {
            continue;
        };
        if supply_type.trim() == "Battery" {
            return Some(path);
        }
    }
    None
}

#[cfg(feature = "x11-backends")]
pub(crate) fn parse_nonnegative_finite(text: &str) -> Option<f64> {
    text.trim()
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && *value >= 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt as _;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    fn test_directory() -> std::path::PathBuf {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "jwm-power-supply-test-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn attribute_reads_are_bounded_and_reject_special_files() {
        let directory = test_directory();
        let exact = directory.join("exact");
        std::fs::write(&exact, vec![b'x'; MAX_POWER_SUPPLY_ATTRIBUTE_BYTES]).unwrap();
        assert_eq!(
            read_attribute(&exact).unwrap().len(),
            MAX_POWER_SUPPLY_ATTRIBUTE_BYTES
        );

        let oversized = directory.join("oversized");
        std::fs::write(&oversized, vec![b'x'; MAX_POWER_SUPPLY_ATTRIBUTE_BYTES + 1]).unwrap();
        assert_eq!(
            read_attribute(&oversized).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        let fifo = directory.join("fifo");
        let fifo_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);
        assert_eq!(
            read_attribute(&fifo).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        let symlink = directory.join("symlink");
        std::os::unix::fs::symlink(&exact, &symlink).unwrap();
        assert!(read_attribute(&symlink).is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn numeric_attributes_reject_impossible_values() {
        assert_eq!(parse_percentage("0\n").unwrap(), 0);
        assert_eq!(parse_percentage("100").unwrap(), 100);
        assert_eq!(
            parse_percentage("101").unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        #[cfg(feature = "x11-backends")]
        {
            assert_eq!(parse_nonnegative_finite("1.5\n"), Some(1.5));
            for invalid in ["-1", "NaN", "inf", "malformed"] {
                assert_eq!(parse_nonnegative_finite(invalid), None);
            }
        }
    }

    #[test]
    fn first_battery_dir_skips_non_batteries_and_prefers_sorted_name() {
        let root = test_directory();
        let ac = root.join("AC");
        let bat1 = root.join("BAT1");
        let bat0 = root.join("BAT0");
        std::fs::create_dir_all(&ac).unwrap();
        std::fs::create_dir_all(&bat1).unwrap();
        std::fs::create_dir_all(&bat0).unwrap();
        std::fs::write(ac.join("type"), "Mains\n").unwrap();
        std::fs::write(bat1.join("type"), "Battery\n").unwrap();
        std::fs::write(bat0.join("type"), "Battery\n").unwrap();

        assert_eq!(first_battery_dir(&root).as_deref(), Some(bat0.as_path()));
        std::fs::remove_dir_all(root).unwrap();
    }
}
