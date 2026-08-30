//! Small, blocking-safe helpers for feature probes under `/sys`.

use std::fs::OpenOptions;
use std::io::Read as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

pub(super) const MAX_ATTRIBUTE_BYTES: u64 = 4 * 1024;

/// Read a bounded regular-file attribute without following a replaced final
/// component or blocking the window-manager loop on a FIFO/device.
pub(super) fn read_text_bounded(path: impl AsRef<Path>, max_bytes: u64) -> Option<String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }

    let mut bytes = Vec::with_capacity(usize::try_from(max_bytes.min(4096)).ok()?);
    file.take(max_bytes.checked_add(1)?)
        .read_to_end(&mut bytes)
        .ok()?;
    if u64::try_from(bytes.len()).ok()? > max_bytes {
        return None;
    }
    String::from_utf8(bytes).ok()
}

pub(super) fn read_attribute(path: impl AsRef<Path>) -> Option<String> {
    read_text_bounded(path, MAX_ATTRIBUTE_BYTES)
}

/// Return at most `limit` directory entries in deterministic path order.
/// `take` precedes error filtering so even a noisy directory has bounded work.
pub(super) fn bounded_paths(path: impl AsRef<Path>, limit: usize) -> Option<Vec<PathBuf>> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(path)
        .ok()?
        .take(limit)
        .flatten()
        .map(|entry| entry.path())
        .collect();
    paths.sort();
    Some(paths)
}
