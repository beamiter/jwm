//! Find names in an existing immutable directory snapshot without touching the filesystem.
use crate::{Cancellation, FileEntry, ListError};

/// One short, local name query. This is not a glob, path, command, or index.
pub struct NameQuery(String);

impl NameQuery {
    pub const MAX_BYTES: usize = 1024;

    pub fn parse(input: &str) -> Option<Self> {
        let text = input.trim();
        if text.is_empty() || text.len() > Self::MAX_BYTES {
            return None;
        }
        Some(Self(text.to_lowercase()))
    }

    /// Returns indices in the same order as the snapshot. This never opens,
    /// changes or filters underlying rows; consumers must check the snapshot
    /// generation before using any index as a selection.
    pub fn matching_rows(
        &self,
        entries: &[FileEntry],
        cancel: &Cancellation,
    ) -> Result<Vec<usize>, ListError> {
        cancel.check()?;
        let mut matched = Vec::new();
        for (index, entry) in entries.iter().enumerate() {
            if index.is_multiple_of(128) {
                cancel.check()?;
            }
            let name = entry.name.to_string_lossy();
            // Normal ASCII names need no allocation or display escaping.
            // Match the escaped row label for Unicode, backslashes and
            // control characters, with no path decoding or shell syntax.
            let found = if self.0.is_ascii() && simple_ascii(&name) {
                ascii_contains(name.as_bytes(), self.0.as_bytes())
            } else {
                let label = name.escape_debug().to_string().to_lowercase();
                label.contains(&self.0)
            };
            if found {
                matched.push(index);
            }
        }
        cancel.check()?;
        Ok(matched)
    }
}

fn simple_ascii(name: &str) -> bool {
    name.bytes()
        .all(|byte| (b' '..=b'~').contains(&byte) && byte != b'\\')
}

fn ascii_contains(name: &[u8], query: &[u8]) -> bool {
    name.windows(query.len())
        .any(|candidate| candidate.eq_ignore_ascii_case(query))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileKind, Location};
    use std::{ffi::OsString, path::PathBuf};

    fn entry(name: impl Into<OsString>) -> FileEntry {
        let name = name.into();
        FileEntry {
            location: Location::Local(PathBuf::from(&name)),
            name,
            kind: FileKind::Regular,
            identity: None,
            bytes: None,
            modified: None,
        }
    }

    #[test]
    fn returns_original_indices_for_literal_unicode_and_escaped_names() {
        let entries = [
            entry("Alpha.txt"),
            entry("unrelated"),
            entry("half ALPHA note"),
            entry("École"),
            entry("line\n.txt"),
            entry("[globs?]"),
        ];
        let cancel = Cancellation::default();
        for (query, expected) in [
            (" alpha ", &[0, 2][..]),
            ("ÉCOLE", &[3][..]),
            ("\\n.txt", &[4][..]),
            ("[globs?]", &[5][..]),
            ("missing", &[][..]),
        ] {
            let rows = NameQuery::parse(query)
                .unwrap()
                .matching_rows(&entries, &cancel)
                .unwrap();
            assert_eq!(rows, expected);
        }
    }

    #[test]
    fn snapshot_query_cancels_and_does_not_cap_visible_match_positions() {
        let entries: Vec<_> = (0..100_000)
            .map(|index| entry(format!("record-{index:06}")))
            .collect();
        let cancel = Cancellation::default();
        let query = NameQuery::parse("record-099999").unwrap();
        assert_eq!(query.matching_rows(&entries, &cancel).unwrap(), [99_999]);
        cancel.cancel();
        assert!(matches!(
            query.matching_rows(&entries, &cancel),
            Err(ListError::Cancelled)
        ));
        assert!(NameQuery::parse("  ").is_none());
        assert!(NameQuery::parse(&"a".repeat(NameQuery::MAX_BYTES + 1)).is_none());
    }
}
