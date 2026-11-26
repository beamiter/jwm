//! Bounded native sibling names for an explicit keep-both copy.
use std::{
    ffi::{OsStr, OsString},
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::Path,
};

const NAME_CAP: usize = 255;
pub(super) const COPY_NAMES: u8 = 32;
const TAR_COMPRESSIONS: &[u8] = b"gz bz bz2 bz3 xz zst z lz lzma lrz lz4 lzo";

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
    let ext = Path::new(original)
        .extension()
        .map_or(&[][..], OsStrExt::as_bytes);
    // Keep known compressed-tar endings intact: MIME matching can use the
    // complete suffix even when the file bytes have not yet been sniffed.
    let mut tail_len = 0;
    if !ext.is_empty() && ext.len() < NAME_CAP - suffix.len() - 4 {
        tail_len = ext.len() + 1;
        let before = &raw[..raw.len() - tail_len];
        if before.len() >= 4
            && before[before.len() - 4..].eq_ignore_ascii_case(b".tar")
            && TAR_COMPRESSIONS
                .split(|byte| *byte == b' ')
                .any(|compression| ext.eq_ignore_ascii_case(compression))
        {
            tail_len += 4;
        }
    }
    let (stem, tail) = raw.split_at(raw.len() - tail_len);
    // If the complete archive name is itself hidden (e.g. .tar.gz), retain
    // the leading dot while keeping its recognized compound suffix intact.
    let stem = if stem.is_empty() { b"." } else { stem };
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
        assert_eq!(f("archive.tar.gz", 1), "archive (copy).tar.gz");
        assert_eq!(f("archive.TAR.Z", 3), "archive (copy 3).TAR.Z");
        assert_eq!(f("archive.tar.xz", 2), "archive (copy 2).tar.xz");
        assert_eq!(f("archive.tar.zst", 1), "archive (copy).tar.zst");
        assert_eq!(f(".tar.gz", 1), ". (copy).tar.gz");
        assert_eq!(f("archive.tar.unknown", 1), "archive.tar (copy).unknown");
        let raw_archive = OsStr::from_bytes(b"archive-%20-\x80.tar.gz");
        assert_eq!(
            sibling_copy_name(raw_archive, 2).as_bytes(),
            b"archive-%20-\x80 (copy 2).tar.gz"
        );
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
