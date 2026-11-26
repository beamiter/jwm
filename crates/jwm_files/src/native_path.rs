//! Explicit local file URIs and byte-preserving path text for desktop actions.
use gtk4::{gio, glib, prelude::*};
use std::path::{Component, Path, PathBuf};

pub(super) const BUS_URI_LIMIT: usize = 4096;
// The address field also accepts the escaped form of a full native path.
const ADDRESS_URI_LIMIT: usize = 16 * 1024;

pub(super) fn local_file_uri(value: &str, limit: usize) -> Result<PathBuf, &'static str> {
    let native_form = value.starts_with("file://") && !value.contains(['?', '#', '\0']);
    if value.len() > limit || !native_form {
        return Err("expected a bounded native file URI without query or fragment");
    }
    let (path, host) = glib::filename_from_uri(value).map_err(|_| "invalid native file URI")?;
    if host
        .as_deref()
        .is_some_and(|host| !host.eq_ignore_ascii_case("localhost"))
        || !path.is_absolute()
        || path.components().any(|part| part == Component::ParentDir)
    {
        return Err("remote or nonlocal folder URI is not supported");
    }
    Ok(path)
}

/// Native names that are not safe as single-line UTF-8 use lossless file URIs.
pub(super) fn path_text(path: &Path) -> (String, bool) {
    if let Some(text) = path.to_str()
        && !text.chars().any(char::is_control)
    {
        return (text.into(), false);
    }
    (gio::File::for_path(path).uri().to_string(), true)
}

pub(super) fn resolve_location(
    input: &str,
    current: &Path,
    home: &Path,
) -> Result<PathBuf, &'static str> {
    if input.starts_with("file://") {
        local_file_uri(input, ADDRESS_URI_LIMIT)
    } else if input == "~" {
        Ok(home.to_path_buf())
    } else if let Some(tail) = input.strip_prefix("~/") {
        Ok(home.join(tail))
    } else {
        Ok(current.join(input))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt};

    #[test]
    fn location_resolves_explicit_file_uri_without_decoding_literal_names() {
        let current = Path::new("/current");
        let home = Path::new("/home/test");
        let raw = Path::new(OsStr::from_bytes(b"/fixture/raw%20-\x80"));
        let (text, is_uri) = path_text(raw);
        assert!(is_uri);
        assert_eq!(resolve_location(&text, current, home).unwrap(), raw);
        assert_eq!(
            resolve_location("literal%20", current, home).unwrap(),
            current.join("literal%20")
        );
        assert_eq!(
            resolve_location("~/literal%20", current, home).unwrap(),
            home.join("literal%20")
        );
        assert_eq!(
            resolve_location("file://localhost/tmp/folder%20name", current, home).unwrap(),
            Path::new("/tmp/folder name")
        );
        assert_eq!(
            resolve_location("file:///", current, home).unwrap(),
            Path::new("/")
        );
        let beyond_bus = format!("file:///{}", "a".repeat(BUS_URI_LIMIT));
        assert!(local_file_uri(&beyond_bus, BUS_URI_LIMIT).is_err());
        assert!(resolve_location(&beyond_bus, current, home).is_ok());
        let beyond_address = format!("file:///{}", "a".repeat(ADDRESS_URI_LIMIT));
        assert!(resolve_location(&beyond_address, current, home).is_err());
        for bad in [
            "file://elsewhere.invalid/fixture",
            "file:///a/../b",
            "file:///a%00",
            "file:///a?b",
            "file:///a#b",
        ] {
            assert!(resolve_location(bad, current, home).is_err(), "{bad}");
        }
    }
}
