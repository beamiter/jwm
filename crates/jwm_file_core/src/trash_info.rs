//! Parse bounded freedesktop home-Trash metadata without interpreting it as a URI.
use std::{
    ffi::OsString,
    os::unix::ffi::OsStringExt,
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
    use std::os::unix::ffi::OsStrExt;

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
