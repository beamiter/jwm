//! Guarded shader-source loading shared by the X11 and Wayland compositors.

use std::fs::{self, OpenOptions};
use std::io::{self, Read as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

/// Shader sources are normally a few kilobytes. One mebibyte leaves ample
/// room for generated sources while preventing hot reload from allocating an
/// arbitrary user-controlled file on the compositor event loop.
const MAX_SHADER_SOURCE_BYTES: u64 = 1024 * 1024;

pub(crate) fn read_shader_source(path: &Path) -> io::Result<String> {
    read_shader_source_with_limit(path, MAX_SHADER_SOURCE_BYTES)
}

fn read_shader_source_with_limit(path: &Path, limit: u64) -> io::Result<String> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "shader source is not a regular file",
        ));
    }
    if metadata.len() > limit {
        return Err(shader_too_large(limit));
    }

    // O_NONBLOCK prevents a metadata/open replacement race from stalling on
    // a FIFO. It does not alter ordinary-file reads; symlinks to regular
    // development sources remain supported.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let opened_metadata = file.metadata()?;
    if !opened_metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "shader source is not a regular file",
        ));
    }
    if opened_metadata.len() > limit {
        return Err(shader_too_large(limit));
    }

    let sentinel_limit = limit
        .checked_add(1)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "shader limit overflow"))?;
    let mut bytes = Vec::with_capacity(opened_metadata.len() as usize);
    file.take(sentinel_limit).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(shader_too_large(limit));
    }
    String::from_utf8(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn shader_too_large(limit: u64) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("shader source exceeds the {limit}-byte limit"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_PATH: AtomicU64 = AtomicU64::new(0);

    fn test_path(name: &str) -> std::path::PathBuf {
        let sequence = NEXT_TEST_PATH.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "jwm-shader-source-{name}-{}-{sequence}",
            std::process::id()
        ))
    }

    #[test]
    fn exact_limit_is_accepted_and_one_extra_byte_is_rejected() {
        let path = test_path("size");
        fs::write(&path, b"12345678").unwrap();
        assert_eq!(read_shader_source_with_limit(&path, 8).unwrap(), "12345678");

        fs::write(&path, b"123456789").unwrap();
        let error = read_shader_source_with_limit(&path, 8).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn invalid_utf8_and_non_regular_sources_are_rejected() {
        let invalid = test_path("utf8");
        fs::write(&invalid, [0xff]).unwrap();
        assert_eq!(
            read_shader_source_with_limit(&invalid, 8)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        fs::remove_file(invalid).unwrap();

        let socket = test_path("socket");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        assert_eq!(
            read_shader_source_with_limit(&socket, 8)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        drop(listener);
        fs::remove_file(socket).unwrap();
    }
}
