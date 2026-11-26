use jwm_file_core::FileEntry;

/// Find the same observed row, never a row that inherited its old position
/// or a different pathname that happens to be a hard link.
pub(crate) fn find_restorable_row(before: &FileEntry, after: &[FileEntry]) -> Option<usize> {
    after.iter().position(|entry| {
        entry.location == before.location
            && entry.kind == before.kind
            && entry.identity == before.identity
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use jwm_file_core::{FileIdentity, FileKind, Location};

    fn file(name: &str, inode: u64) -> FileEntry {
        FileEntry {
            location: Location::Local(name.into()),
            name: name.into(),
            kind: FileKind::Regular,
            identity: Some(FileIdentity { device: 1, inode }),
            bytes: None,
            modified: None,
        }
    }

    #[test]
    fn restores_raw_path_and_identity_at_the_new_position() {
        let selected = file("middle", 2);
        let changed = [file("first", 1), selected.clone(), file("tail", 3)];
        assert_eq!(find_restorable_row(&selected, &changed), Some(1));
        let changed = [
            file("ahead", 4),
            file("first", 1),
            selected.clone(),
            file("tail", 3),
        ];
        assert_eq!(find_restorable_row(&selected, &changed), Some(2));
    }

    #[test]
    fn does_not_select_a_replacement_or_hard_link_alias() {
        let selected = file("middle", 2);
        let changed = [file("middle", 42), file("linked", 2), file("tail", 3)];
        assert_eq!(find_restorable_row(&selected, &changed), None);
    }
}
