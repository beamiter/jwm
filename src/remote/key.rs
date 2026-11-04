//! Private pre-shared key files for LAN remote-control sessions.

use super::RemoteResult;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

const KEY_BYTES: usize = 32;
const PRINTABLE_KEY_BYTES: usize = KEY_BYTES * 2;

/// Where a session's pre-shared key comes from.
#[derive(Clone)]
pub enum KeySource {
    /// A private key file, checked by [`load_key_file`].
    File(PathBuf),
    /// One printable key line on standard input. `jwm-remote ssh` hands the
    /// host a fresh key this way, over the SSH channel that started it, so no
    /// key file has to be copied between machines.
    Stdin,
    /// A key already in memory, for the side that generated it.
    Bytes(Vec<u8>),
}

impl std::fmt::Debug for KeySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File(path) => f.debug_tuple("File").field(path).finish(),
            Self::Stdin => f.write_str("Stdin"),
            // Never print key material, even in a debug dump.
            Self::Bytes(_) => f.write_str("Bytes(<redacted>)"),
        }
    }
}

impl KeySource {
    pub fn load(&self) -> RemoteResult<Vec<u8>> {
        match self {
            Self::File(path) => load_key_file(path),
            Self::Stdin => read_key_line(&mut io::stdin().lock()),
            Self::Bytes(key) if key.len() == KEY_BYTES => Ok(key.clone()),
            Self::Bytes(_) => Err(invalid_data("remote key must be 32 bytes").into()),
        }
    }
}

/// A fresh random key and its printable form, for one session.
#[must_use]
pub fn generate_session_key() -> (Vec<u8>, String) {
    let key: [u8; KEY_BYTES] = rand::random();
    (key.to_vec(), printable_key(&key))
}

fn printable_key(key: &[u8]) -> String {
    let mut printable = String::with_capacity(PRINTABLE_KEY_BYTES);
    for byte in key {
        use std::fmt::Write as _;
        write!(&mut printable, "{byte:02x}").expect("writing to a String cannot fail");
    }
    printable
}

/// Read one printable key line, byte by byte so nothing after it is consumed.
pub fn read_key_line<R: Read>(reader: &mut R) -> RemoteResult<Vec<u8>> {
    let mut line = Vec::with_capacity(PRINTABLE_KEY_BYTES + 1);
    let mut byte = [0_u8; 1];
    while line.len() <= PRINTABLE_KEY_BYTES + 1 {
        match reader.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                line.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    if !line.ends_with(b"\n") {
        return Err(invalid_data("expected one key line of 64 hexadecimal digits").into());
    }
    parse_printable_key(&line)
}

/// Generate a 256-bit printable key in a new mode-0600 file.
///
/// Existing paths are never replaced.
pub fn generate_key_file(path: &Path) -> RemoteResult<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    let mut file = options.open(path)?;
    let random: [u8; KEY_BYTES] = rand::random();
    let mut printable = printable_key(&random);
    printable.push('\n');
    if let Err(error) = file
        .write_all(printable.as_bytes())
        .and_then(|()| file.sync_all())
    {
        let _ = fs::remove_file(path);
        return Err(error.into());
    }
    Ok(())
}

/// Read a key only when its ownership, type and Unix mode are private.
pub fn load_key_file(path: &Path) -> RemoteResult<Vec<u8>> {
    let path_metadata = fs::symlink_metadata(path)?;
    if path_metadata.file_type().is_symlink() {
        return Err(permission_denied("remote key file must not be a symbolic link").into());
    }
    if !path_metadata.is_file() {
        return Err(permission_denied("remote key path is not a regular file").into());
    }
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(permission_denied("remote key path is not a regular file").into());
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(permission_denied("remote key file is not owned by the current user").into());
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(permission_denied(
            "remote key file is accessible by group or other users; run chmod 600",
        )
        .into());
    }

    let mut key = Vec::with_capacity(PRINTABLE_KEY_BYTES + 1);
    (&mut file)
        .take((PRINTABLE_KEY_BYTES + 3) as u64)
        .read_to_end(&mut key)?;
    parse_printable_key(&key)
}

/// Decode 64 hexadecimal digits, optionally followed by one line ending.
fn parse_printable_key(key: &[u8]) -> RemoteResult<Vec<u8>> {
    let printable = match key {
        bytes if bytes.len() == PRINTABLE_KEY_BYTES => bytes,
        bytes if bytes.len() == PRINTABLE_KEY_BYTES + 1 && bytes.ends_with(b"\n") => {
            &bytes[..PRINTABLE_KEY_BYTES]
        }
        bytes if bytes.len() == PRINTABLE_KEY_BYTES + 2 && bytes.ends_with(b"\r\n") => {
            &bytes[..PRINTABLE_KEY_BYTES]
        }
        _ => {
            return Err(
                invalid_data("remote key must contain exactly 64 hexadecimal digits").into(),
            );
        }
    };
    if printable.iter().all(u8::is_ascii_hexdigit) {
        return printable
            .chunks_exact(2)
            .map(|pair| {
                let high = hex_nibble(pair[0]).expect("validated hexadecimal digit");
                let low = hex_nibble(pair[1]).expect("validated hexadecimal digit");
                Ok((high << 4) | low)
            })
            .collect();
    }
    Err(invalid_data("remote key must contain exactly 64 hexadecimal digits").into())
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn permission_denied(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message.into())
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn temporary_directory() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "jwm-remote-key-test-{}-{:016x}",
            std::process::id(),
            rand::random::<u64>()
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[test]
    fn generated_key_is_private_loadable_and_never_overwritten() {
        let directory = temporary_directory();
        let path = directory.join("remote.key");
        generate_key_file(&path).unwrap();
        let first = load_key_file(&path).unwrap();
        assert_eq!(first.len(), 32);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(generate_key_file(&path).is_err());
        assert_eq!(load_key_file(&path).unwrap(), first);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn permissive_mode_and_short_keys_are_rejected() {
        let directory = temporary_directory();
        let path = directory.join("remote.key");
        fs::write(&path, b"short\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(load_key_file(&path).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_key_file(&path).is_err());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn trailing_data_after_a_complete_key_is_rejected() {
        let directory = temporary_directory();
        let path = directory.join("remote.key");
        let mut contents =
            b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_vec();
        contents.extend_from_slice(b"\nextra");
        fs::write(&path, contents).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(load_key_file(&path).is_err());

        fs::write(
            &path,
            b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n\n\n",
        )
        .unwrap();
        assert!(load_key_file(&path).is_err());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_session_key_round_trips_through_one_stdin_line() {
        let (key, printable) = generate_session_key();
        assert_eq!(key.len(), 32);
        let mut stdin = format!("{printable}\nanything after the key stays unread").into_bytes();
        let mut reader = std::io::Cursor::new(&mut stdin);
        assert_eq!(read_key_line(&mut reader).unwrap(), key);
        assert_eq!(reader.position(), 65, "only the key line was consumed");

        for bad in [
            &b""[..],
            b"0123\n",
            // A complete key with no line ending: stdin closed early.
            b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdeg\n",
        ] {
            assert!(read_key_line(&mut std::io::Cursor::new(bad)).is_err());
        }
        assert!(format!("{:?}", KeySource::Bytes(key)).contains("redacted"));
    }

    #[test]
    fn symbolic_link_key_paths_are_rejected() {
        use std::os::unix::fs::symlink;

        let directory = temporary_directory();
        let key = directory.join("remote.key");
        let link = directory.join("linked.key");
        generate_key_file(&key).unwrap();
        symlink(&key, &link).unwrap();
        assert!(load_key_file(&link).is_err());
        fs::remove_dir_all(directory).unwrap();
    }
}
