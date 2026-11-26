use crate::{
    Cancellation, DirectorySnapshot, FileEntry, FileIdentity, FileKind, FileProvider, ListError,
    ListOptions, Location,
};
use std::fs;

#[derive(Clone, Copy, Debug, Default)]
pub struct LocalProvider;

impl FileProvider for LocalProvider {
    fn list(
        &self,
        location: &Location,
        options: ListOptions,
        cancellation: &Cancellation,
    ) -> Result<DirectorySnapshot, ListError> {
        cancellation.check()?;
        let Location::Local(path) = location;
        let read_dir = fs::read_dir(path).map_err(|source| ListError::ReadDirectory {
            path: path.clone(),
            source,
        })?;
        let mut entries = Vec::new();
        let mut skipped_entries = 0;
        for next in read_dir {
            cancellation.check()?;
            let Ok(entry) = next else {
                skipped_entries += 1;
                continue;
            };
            let name = entry.file_name();
            if !options.show_hidden && name.as_encoded_bytes().starts_with(b".") {
                continue;
            }
            let path = entry.path();
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                skipped_entries += 1;
                continue;
            };
            let file_type = metadata.file_type();
            let kind = if file_type.is_symlink() {
                FileKind::Symlink
            } else if file_type.is_dir() {
                FileKind::Directory
            } else if file_type.is_file() {
                FileKind::Regular
            } else {
                FileKind::Other
            };
            entries.push(FileEntry {
                location: Location::Local(path),
                name,
                kind,
                identity: local_identity(&metadata),
                bytes: (kind == FileKind::Regular).then_some(metadata.len()),
                modified: metadata.modified().ok(),
            });
        }
        cancellation.check()?;
        entries.sort_unstable_by(|a, b| {
            (a.kind != FileKind::Directory, a.name.as_encoded_bytes())
                .cmp(&(b.kind != FileKind::Directory, b.name.as_encoded_bytes()))
        });
        cancellation.check()?;
        Ok(DirectorySnapshot {
            location: location.clone(),
            entries,
            skipped_entries,
        })
    }
}

#[cfg(unix)]
fn local_identity(metadata: &fs::Metadata) -> Option<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    Some(FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(not(unix))]
fn local_identity(_: &fs::Metadata) -> Option<FileIdentity> {
    None
}
