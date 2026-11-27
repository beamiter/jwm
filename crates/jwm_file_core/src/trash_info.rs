//! Parse bounded freedesktop home-Trash metadata without interpreting it as a URI.
use std::{
    ffi::{OsStr, OsString},
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Component, Path, PathBuf},
};

pub const TRASH_INFO_LIMIT: usize = 16 * 1024;

#[derive(Debug, Eq, PartialEq)]
pub struct HomeTrashInfo {
    pub original: PathBuf,
    /// Unambiguous display ordering only, not item identity or a unique timestamp.
    pub deletion_date: String,
}

/// Read the first Path and DeletionDate fields. Relative paths are rooted in
/// XDG_DATA_HOME, not in Trash, and must not traverse above that directory.
pub fn parse_home_trash_info(bytes: &[u8], data_home: &Path) -> Option<HomeTrashInfo> {
    if bytes.len() > TRASH_INFO_LIMIT || !data_home.is_absolute() {
        return None;
    }
    let mut lines = bytes
        .split(|ch| *ch == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line));
    if lines.next()? != b"[Trash Info]" {
        return None;
    }
    let mut path = None;
    let mut date = None;
    for line in lines {
        if path.is_none()
            && let Some(value) = line.strip_prefix(b"Path=")
        {
            path = Some(decode_path(value)?);
        }
        if date.is_none()
            && let Some(value) = line.strip_prefix(b"DeletionDate=")
        {
            date = Some(deletion_date(value)?);
        }
        if path.is_some() && date.is_some() {
            break;
        }
    }
    let path = path?;
    if path.file_name().is_none() || path.components().any(|part| part == Component::ParentDir) {
        return None;
    }
    let original = if path.is_absolute() {
        path
    } else {
        data_home.join(path)
    };
    Some(HomeTrashInfo {
        original,
        deletion_date: date?,
    })
}

/// Encode bounded home-Trash metadata for an already chosen exact native
/// absolute path. This function writes nothing and does not choose a storage
/// name or authorize moving the original file.
pub(crate) fn encode_home_trash_info(original: &Path, local_date: &str) -> Option<Vec<u8>> {
    const HEADER: &[u8] = b"[Trash Info]\nPath=";
    const DATE_FIELD: &[u8] = b"\nDeletionDate=";
    const HEX: &[u8; 16] = b"0123456789ABCDEF";

    let raw = original.as_os_str().as_bytes();
    if raw.len() > TRASH_INFO_LIMIT
        || !original.is_absolute()
        || original.file_name().is_none()
        || raw.contains(&0)
        || raw.ends_with(b"/")
        || original
            .components()
            .any(|part| part == Component::ParentDir)
        || raw.split(|byte| *byte == b'/').any(|part| part == b".")
    {
        return None;
    }
    let date = deletion_date(local_date.as_bytes())?;
    let reserved = DATE_FIELD.len() + date.len() + 1;
    let mut body = Vec::with_capacity(raw.len().min(TRASH_INFO_LIMIT - reserved));
    body.extend_from_slice(HEADER);
    for &byte in raw {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'.' | b'_' | b'~') {
            body.push(byte);
        } else {
            body.extend_from_slice(&[b'%', HEX[(byte >> 4) as usize], HEX[(byte & 15) as usize]]);
        }
        if body.len() > TRASH_INFO_LIMIT - reserved {
            return None;
        }
    }
    body.extend_from_slice(DATE_FIELD);
    body.extend_from_slice(date.as_bytes());
    body.push(b'\n');
    Some(body)
}

/// Form a readable bounded key from an observed leaf and caller-owned token.
/// The key is not authority to create anything. The caller must still reserve
/// its metadata with O_EXCL and publish the payload without replacement.
pub(crate) fn trash_storage_key(leaf: &OsStr, token: &str) -> Option<OsString> {
    const SAFE_LEAF_BYTES: usize = 255 - b".trashinfo".len();
    const TAG: &[u8] = b".jwm-";
    let raw = leaf.as_bytes();
    if raw.is_empty()
        || matches!(raw, b"." | b"..")
        || raw.iter().any(|&byte| matches!(byte, b'/' | b'\0'))
        || token.is_empty()
        || token.len() > 64
        || !token.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    let prefix = raw.len().min(SAFE_LEAF_BYTES - TAG.len() - token.len());
    let mut key = raw[..prefix].to_vec();
    key.extend_from_slice(TAG);
    key.extend_from_slice(token.as_bytes());
    Some(OsString::from_vec(key))
}

fn decode_path(encoded: &[u8]) -> Option<PathBuf> {
    let mut decoded = Vec::with_capacity(encoded.len());
    let mut cursor = 0;
    while cursor < encoded.len() {
        let ch = encoded[cursor];
        cursor += 1;
        let ch = if ch == b'%' {
            let hi = hex(*encoded.get(cursor)?)?;
            let lo = hex(*encoded.get(cursor + 1)?)?;
            cursor += 2;
            (hi << 4) | lo
        } else {
            ch
        };
        if ch == 0 {
            return None;
        }
        decoded.push(ch);
    }
    if decoded.is_empty() {
        return None;
    }
    Some(OsString::from_vec(decoded).into())
}

fn hex(ch: u8) -> Option<u8> {
    match ch {
        b'0'..=b'9' => Some(ch - b'0'),
        b'a'..=b'f' => Some(ch - b'a' + 10),
        b'A'..=b'F' => Some(ch - b'A' + 10),
        _ => None,
    }
}

fn deletion_date(value: &[u8]) -> Option<String> {
    // Accept the normative date and the compact date in the spec's example.
    let normalized = match value {
        [
            y1,
            y2,
            y3,
            y4,
            m1,
            m2,
            d1,
            d2,
            b'T',
            h1,
            h2,
            b':',
            n1,
            n2,
            b':',
            s1,
            s2,
        ] => [
            *y1, *y2, *y3, *y4, b'-', *m1, *m2, b'-', *d1, *d2, b'T', *h1, *h2, b':', *n1, *n2,
            b':', *s1, *s2,
        ],
        _ => value.try_into().ok()?,
    };
    for (i, separator) in [(4, b'-'), (7, b'-'), (10, b'T'), (13, b':'), (16, b':')] {
        if normalized[i] != separator {
            return None;
        }
    }
    let read = |a, b| {
        let field = std::str::from_utf8(&normalized[a..b]).ok()?;
        field
            .bytes()
            .all(|ch| ch.is_ascii_digit())
            .then(|| field.parse::<u16>().ok())
            .flatten()
    };
    let year = read(0, 4)?;
    let month = read(5, 7)?;
    let day = read(8, 10)?;
    let hour = read(11, 13)?;
    let minute = read(14, 16)?;
    let second = read(17, 19)?;
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let last = match month {
        4 | 6 | 9 | 11 => 30,
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => return None,
    };
    if year == 0 || day == 0 || day > last || hour >= 24 || minute >= 60 || second > 60 {
        return None;
    }
    String::from_utf8(normalized.to_vec()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restores_exact_original_path_bytes_without_treating_plus_as_space() {
        let data = b"[Trash Info]\nPath=/home/test/report%25+%E6%B5%8B%E8%AF%95%80\nDeletionDate=2026-11-26T12:26:00\n";
        let info = parse_home_trash_info(data, Path::new("/home/test/.local/share")).unwrap();
        assert_eq!(
            info.original.as_os_str().as_bytes(),
            b"/home/test/report%+\xe6\xb5\x8b\xe8\xaf\x95\x80"
        );
        assert_eq!(info.deletion_date, "2026-11-26T12:26:00");
    }

    #[test]
    fn trash_writer_round_trips_raw_path_without_extra_metadata_fields() {
        let raw = b"/home/test/report%+\nDeletionDate=forged\r\x80";
        let path = Path::new(OsStr::from_bytes(raw));
        let date = "2024-02-29T12:26:00";
        let record = encode_home_trash_info(path, date).unwrap();
        assert_eq!(record.iter().filter(|&&byte| byte == b'\n').count(), 3);
        assert!(record.is_ascii());
        let parsed = parse_home_trash_info(&record, Path::new("/home/test/.local/share")).unwrap();
        assert_eq!(parsed.original.as_os_str().as_bytes(), raw);
        assert_eq!(parsed.deletion_date, date);
    }

    #[test]
    fn trash_writer_rejects_unsafe_paths_bad_dates_and_encoded_oversize() {
        let date = "2024-02-29T12:26:00";
        for raw in [
            b"relative".as_slice(),
            b"/",
            b"/a/../b",
            b"/a/./b",
            b"/a/",
            b"/x\0y",
        ] {
            assert!(encode_home_trash_info(Path::new(OsStr::from_bytes(raw)), date).is_none());
        }
        let path = Path::new("/home/test/file");
        assert!(encode_home_trash_info(path, "2025-02-29T01:02:03").is_none());
        assert!(
            encode_home_trash_info(path, "20040831T22:32:08")
                .unwrap()
                .ends_with(b"2004-08-31T22:32:08\n")
        );
        let oversized = format!("/{}", "%".repeat(TRASH_INFO_LIMIT / 3));
        assert!(encode_home_trash_info(Path::new(&oversized), date).is_none());
    }

    #[test]
    fn trash_storage_key_bounds_payload_and_info_and_keeps_raw_prefix() {
        let prefix = OsStr::from_bytes(b"report\x81");
        let one = trash_storage_key(prefix, "aba").unwrap();
        let two = trash_storage_key(prefix, "abb").unwrap();
        assert_eq!(one.as_bytes(), b"report\x81.jwm-aba");
        assert_ne!(one, two);
        let raw = vec![b'a'; 255];
        let key = trash_storage_key(OsStr::from_bytes(&raw), &"F".repeat(64)).unwrap();
        assert_eq!(key.as_bytes().len() + b".trashinfo".len(), 255);
        assert!(trash_storage_key(OsStr::new(".."), "1").is_none());
        assert!(trash_storage_key(OsStr::new("nested/path"), "1").is_none());
        assert!(trash_storage_key(OsStr::new("valid"), "../").is_none());
    }

    #[test]
    fn first_metadata_fields_and_compact_date_are_accepted() {
        let data = b"[Trash Info]\r\nPath=relative/name\nPath=/ignored\nDeletionDate=20240229T12:26:00\nDeletionDate=invalid\n";
        let info = parse_home_trash_info(data, Path::new("/data")).unwrap();
        assert_eq!(info.original, Path::new("/data/relative/name"));
        assert_eq!(info.deletion_date, "2024-02-29T12:26:00");
    }

    #[test]
    fn rejects_bad_dates_oversize_and_unsafe_destinations() {
        for path in [
            "/path/../file",
            "a/%2E%2e/file",
            "nul%00name",
            "bad%escape",
            "/",
            "",
        ] {
            let data = format!("[Trash Info]\nPath={path}\nDeletionDate=2026-11-26T12:26:00");
            assert!(
                parse_home_trash_info(data.as_bytes(), Path::new("/data")).is_none(),
                "{path}"
            );
        }
        let bad_date = b"[Trash Info]\nPath=hello\nDeletionDate=2025-02-29T12:26:00";
        assert!(parse_home_trash_info(bad_date, Path::new("/data")).is_none());
        assert!(
            parse_home_trash_info(&vec![0; TRASH_INFO_LIMIT + 1], Path::new("/data")).is_none()
        );
    }
}
