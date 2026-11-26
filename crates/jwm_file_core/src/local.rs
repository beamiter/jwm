use crate::FileIdentity;
use crate::{
    Cancellation, DirectorySnapshot, FileEntry, FileKind, FileProvider, ListError, ListOptions,
    Location,
};
#[cfg(not(target_os = "linux"))]
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
        list_local(location, options, cancellation, ScanMode::Plain)
    }

    fn list_with_watch(
        &self,
        location: &Location,
        options: ListOptions,
        cancellation: &Cancellation,
    ) -> Result<DirectorySnapshot, ListError> {
        cancellation.check()?;
        list_local(location, options, cancellation, ScanMode::Watched)
    }
}

#[derive(Clone, Copy)]
enum ScanMode {
    Plain,
    Watched,
    #[cfg(target_os = "linux")]
    Tree(Option<FileIdentity>),
}

#[cfg(target_os = "linux")]
impl LocalProvider {
    /// Enumerate only real directory children, on a worker. A child branch
    /// must still be the directory observed when reading its parent.
    pub fn list_tree_children(
        &self,
        location: &Location,
        expected: Option<FileIdentity>,
        options: ListOptions,
        cancellation: &Cancellation,
    ) -> Result<DirectorySnapshot, ListError> {
        cancellation.check()?;
        list_local(location, options, cancellation, ScanMode::Tree(expected))
    }
}

#[cfg(not(target_os = "linux"))]
fn list_local(
    location: &Location,
    options: ListOptions,
    cancellation: &Cancellation,
    _mode: ScanMode,
) -> Result<DirectorySnapshot, ListError> {
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
    sort_entries(&mut entries, cancellation)?;
    Ok(DirectorySnapshot {
        location: location.clone(),
        entries,
        skipped_entries,
    })
}

fn sort_entries(entries: &mut [FileEntry], cancellation: &Cancellation) -> Result<(), ListError> {
    cancellation.check()?;
    entries.sort_unstable_by(|a, b| {
        (a.kind != FileKind::Directory, a.name.as_encoded_bytes())
            .cmp(&(b.kind != FileKind::Directory, b.name.as_encoded_bytes()))
    });
    cancellation.check()
}

#[cfg(all(unix, not(target_os = "linux")))]
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

#[cfg(target_os = "linux")]
fn list_local(
    location: &Location,
    options: ListOptions,
    cancellation: &Cancellation,
    mode: ScanMode,
) -> Result<DirectorySnapshot, ListError> {
    use crate::{DirectoryHandle, DirectoryWatch};
    use rustix::fs::{self, AtFlags, Dir, FileType, Mode, OFlags};
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::sync::Arc;

    let Location::Local(path) = location;
    let open_error = |source| ListError::ReadDirectory {
        path: path.clone(),
        source,
    };
    let parent = match mode {
        ScanMode::Tree(Some(expected)) => {
            let handle = DirectoryHandle::open_tree_node(path).map_err(open_error)?;
            if handle.identity() != expected {
                return Err(open_error(std::io::Error::other("tree directory changed")));
            }
            handle
        }
        _ => DirectoryHandle::open(path).map_err(open_error)?,
    };
    let tree = matches!(mode, ScanMode::Tree(_));
    let local_watch = matches!(mode, ScanMode::Watched)
        .then(|| DirectoryWatch::new(&parent).ok())
        .flatten();
    let fd = fs::openat(
        &parent.fd,
        ".",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| open_error(e.into()))?;
    let dir = Dir::new(fd).map_err(|e| open_error(e.into()))?;
    let mut entries = Vec::new();
    let mut skipped_entries = 0;
    for next in dir {
        cancellation.check()?;
        let Ok(entry) = next else {
            skipped_entries += 1;
            continue;
        };
        let raw_name = entry.file_name();
        let bytes = raw_name.to_bytes();
        if bytes == b"." || bytes == b".." || (!options.show_hidden && bytes.starts_with(b".")) {
            continue;
        }
        if tree && !matches!(entry.file_type(), FileType::Directory | FileType::Unknown) {
            continue;
        }
        let Ok(stat) = fs::statat(&parent.fd, raw_name, AtFlags::SYMLINK_NOFOLLOW) else {
            skipped_entries += 1;
            continue;
        };
        let kind = match FileType::from_raw_mode(stat.st_mode) {
            FileType::Directory => FileKind::Directory,
            FileType::RegularFile => FileKind::Regular,
            FileType::Symlink => FileKind::Symlink,
            _ => FileKind::Other,
        };
        if tree && kind != FileKind::Directory {
            continue;
        }
        let name = OsStr::from_bytes(bytes);
        entries.push(FileEntry {
            location: Location::Local(path.join(name)),
            name: name.to_owned(),
            kind,
            identity: Some(FileIdentity {
                device: stat.st_dev,
                inode: stat.st_ino,
            }),
            bytes: if kind == FileKind::Regular {
                u64::try_from(stat.st_size).ok()
            } else {
                None
            },
            modified: timestamp(stat.st_mtime, stat.st_mtime_nsec),
        });
    }
    sort_entries(&mut entries, cancellation)?;
    Ok(DirectorySnapshot {
        location: location.clone(),
        entries,
        skipped_entries,
        local_parent: Some(Arc::new(parent)),
        local_watch,
    })
}

#[cfg(target_os = "linux")]
fn timestamp(secs: impl Into<i64>, nanos: impl TryInto<u64>) -> Option<std::time::SystemTime> {
    use std::time::{Duration, UNIX_EPOCH};
    let secs = secs.into();
    let nanos = nanos.try_into().ok()?;
    if nanos >= 1_000_000_000 {
        return None;
    }
    let delta = Duration::from_secs(secs.unsigned_abs());
    let whole = if secs < 0 {
        UNIX_EPOCH.checked_sub(delta)?
    } else {
        UNIX_EPOCH.checked_add(delta)?
    };
    whole.checked_add(Duration::from_nanos(nanos))
}
