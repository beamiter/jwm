//! Bounded native sibling names for an explicit keep-both copy.
use std::{
    ffi::{OsStr, OsString},
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::Path,
};

const NAME_CAP: usize = 255;
pub(super) const COPY_NAMES: u8 = 32;

fn prefix_within(bytes: &[u8], cap: usize) -> &[u8] {
    if bytes.len() <= cap {
        return bytes;
    }
    let mut end = cap;
    if let Ok(text) = std::str::from_utf8(bytes) {
        while !text.is_char_boundary(end) {
            end -= 1;
        }
    }
    &bytes[..end]
}

/// Keep the complete extension when possible, without normalizing native
/// bytes. The current link attempt, not this name generator, decides whether
/// a candidate is available.
pub(super) fn sibling_copy_name(original: &OsStr, attempt: u8) -> OsString {
    debug_assert!((1..=COPY_NAMES).contains(&attempt));
    let suffix = if attempt == 1 {
        " (copy)".to_owned()
    } else {
        format!(" (copy {attempt})")
    };
    let raw = original.as_bytes();
    let extension = Path::new(original)
        .extension()
        .filter(|value| !value.is_empty());
    // Very long extensions are part of the stem so that adding a suffix
    // never increases the total beyond the conservative component cap.
    let (stem, tail) = match extension {
        Some(ext) if ext.len() < NAME_CAP - suffix.len() - 4 => {
            raw.split_at(raw.len() - ext.len() - 1)
        }
        _ => (raw, &[][..]),
    };
    let mut out = prefix_within(stem, NAME_CAP - suffix.len() - tail.len()).to_vec();
    out.extend_from_slice(suffix.as_bytes());
    out.extend_from_slice(tail);
    OsString::from_vec(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_names_preserve_extensions_and_native_bytes() {
        let f = |name: &str, attempt| sibling_copy_name(OsStr::new(name), attempt);
        assert_eq!(f("report.txt", 1), "report (copy).txt");
        assert_eq!(f("report.txt", 2), "report (copy 2).txt");
        assert_eq!(f(".hidden", COPY_NAMES), ".hidden (copy 32)");
        assert_eq!(f("archive.tar.gz", 1), "archive.tar (copy).gz");
        let raw = OsStr::from_bytes(b"literal-%20-\x80.txt");
        assert_eq!(
            sibling_copy_name(raw, 3).as_bytes(),
            b"literal-%20-\x80 (copy 3).txt"
        );
    }

    #[test]
    fn cap_is_enforced_without_splitting_a_valid_utf8_name() {
        let text = format!("{}.txt", "天".repeat(83));
        let copied = sibling_copy_name(OsStr::new(&text), COPY_NAMES);
        assert!(copied.len() <= NAME_CAP);
        assert!(copied.to_str().unwrap().ends_with(" (copy 32).txt"));
        let huge_ext = format!("a.{}", "b".repeat(253));
        let copied = sibling_copy_name(OsStr::new(&huge_ext), 1);
        assert!(copied.len() <= NAME_CAP);
        assert!(copied.to_str().unwrap().ends_with(" (copy)"));
    }
}
