//! Bounded, lossless snapshot of one Files window's open tabs.
use jwm_file_core::FILES_TAB_STATE_CAP;
use std::{
    io::{self, ErrorKind},
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Component, Path, PathBuf},
};

const MAGIC: &[u8; 8] = b"JWMFILE1";
const MAX_PATH_BYTES: usize = 4096;
pub(super) const MAX_TABS: usize = 16;

#[derive(Debug, PartialEq, Eq)]
pub(super) struct SavedTabs {
    pub(super) paths: Vec<PathBuf>,
    pub(super) active: usize,
}

fn invalid() -> io::Error {
    io::Error::new(ErrorKind::InvalidData, "invalid JWM Files session")
}

fn validate_path(path: &Path) -> io::Result<&[u8]> {
    let raw = path.as_os_str().as_bytes();
    if !path.is_absolute()
        || raw.len() > MAX_PATH_BYTES
        || raw.contains(&0)
        || path.components().any(|part| part == Component::ParentDir)
    {
        return Err(invalid());
    }
    Ok(raw)
}

impl SavedTabs {
    pub(super) fn encode(&self) -> io::Result<Vec<u8>> {
        if self.paths.is_empty() || self.paths.len() > MAX_TABS || self.active >= self.paths.len() {
            return Err(invalid());
        }
        let mut output = MAGIC.to_vec();
        output.push(self.paths.len() as u8);
        output.push(self.active as u8);
        for path in &self.paths {
            let raw = validate_path(path)?;
            output.extend_from_slice(&(raw.len() as u16).to_le_bytes());
            output.extend_from_slice(raw);
        }
        Ok(output)
    }

    pub(super) fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() > FILES_TAB_STATE_CAP {
            return Err(invalid());
        }
        let after_magic = bytes.strip_prefix(MAGIC).ok_or_else(invalid)?;
        let (&count, after_count) = after_magic.split_first().ok_or_else(invalid)?;
        let (&active, mut rest) = after_count.split_first().ok_or_else(invalid)?;
        let (count, active) = (usize::from(count), usize::from(active));
        if count == 0 || count > MAX_TABS || active >= count {
            return Err(invalid());
        }
        let mut paths = Vec::with_capacity(count);
        for _ in 0..count {
            let (prefix, tail) = rest.split_at_checked(2).ok_or_else(invalid)?;
            let len = usize::from(u16::from_le_bytes([prefix[0], prefix[1]]));
            if len == 0 || len > MAX_PATH_BYTES {
                return Err(invalid());
            }
            let (raw, tail) = tail.split_at_checked(len).ok_or_else(invalid)?;
            let path = PathBuf::from(std::ffi::OsString::from_vec(raw.to_vec()));
            validate_path(&path)?;
            paths.push(path);
            rest = tail;
        }
        if !rest.is_empty() {
            return Err(invalid());
        }
        Ok(Self { paths, active })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_unix_bytes_and_tab_order_round_trip() {
        let raw = PathBuf::from(std::ffi::OsString::from_vec(b"/r/raw-%20-\x80".to_vec()));
        let regular = PathBuf::from("/r/raw-%20-\u{fffd}");
        let input = SavedTabs {
            paths: vec![raw, regular, PathBuf::from("/")],
            active: 1,
        };
        let bytes = input.encode().unwrap();
        assert_eq!(SavedTabs::decode(&bytes).unwrap(), input);
        for i in 0..bytes.len() {
            assert!(SavedTabs::decode(&bytes[..i]).is_err(), "truncated at {i}");
        }
        let mut trailing = bytes;
        trailing.push(0);
        assert!(SavedTabs::decode(&trailing).is_err());
    }

    #[test]
    fn refuses_out_of_range_counts_and_unusable_paths() {
        for path in ["relative", "/parent/../folder", "/nul\0", ""] {
            let input = SavedTabs {
                paths: vec![PathBuf::from(path)],
                active: 0,
            };
            assert!(input.encode().is_err(), "{path:?}");
        }
        assert!(SavedTabs::decode(b"JWMFILE1\x01\x01\x01\x00/").is_err());
        assert!(SavedTabs::decode(b"JWMFILE1\x11\x00").is_err());
        assert!(SavedTabs::decode(b"JWMFILE1\x01\x00\x01\x00x").is_err());
        assert!(SavedTabs::decode(b"JWMFILE1\x01\x00\x01\x00\0").is_err());
        assert!(SavedTabs::decode(b"JWMFILE1\x01\x00\x01\x00/").is_ok());
    }
}
