//! Read-only, locally scoped directory walk for an explicitly selected index root.
use crate::{Cancellation, DirectoryHandle, FileIdentity, FileKind, ListError};
use rustix::fd::{BorrowedFd, OwnedFd};
use rustix::fs::{self, Dir, FileType, Mode, OFlags, ResolveFlags};
use rustix::io::Errno;
use std::{
    ffi::OsStr,
    io,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

const MAX_ENTRIES: usize = 100_000;
const MAX_DIRS: usize = 4_096;
const MAX_DEPTH: usize = 48;
const MAX_PATH: usize = 4_096;
const WITHIN_ROOT: ResolveFlags = ResolveFlags::BENEATH
    .union(ResolveFlags::NO_SYMLINKS)
    .union(ResolveFlags::NO_XDEV);

#[derive(Clone, Copy, Debug)]
pub struct WalkOptions {
    pub entries: usize,
    pub directories: usize,
    pub depth: usize,
    pub show_hidden: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WalkSummary {
    pub observed_names: usize,
    pub read_directories: usize,
    pub skipped: usize,
    /// Mount boundaries intentionally outside the selected root's search scope.
    pub excluded_mounts: usize,
    pub incomplete: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct IndexEntry<'a> {
    pub relative: &'a Path,
    pub kind: FileKind,
    pub identity: FileIdentity,
}

fn identity(stat: &fs::Stat) -> FileIdentity {
    FileIdentity {
        device: stat.st_dev,
        inode: stat.st_ino,
    }
}

fn kind(stat: &fs::Stat) -> FileKind {
    match FileType::from_raw_mode(stat.st_mode) {
        FileType::Directory => FileKind::Directory,
        FileType::RegularFile => FileKind::Regular,
        FileType::Symlink => FileKind::Symlink,
        _ => FileKind::Other,
    }
}

fn invalid(root: &Path, details: &str) -> ListError {
    ListError::ReadDirectory {
        path: root.to_path_buf(),
        source: io::Error::new(io::ErrorKind::InvalidInput, details),
    }
}

/// Scan only beneath an explicit, absolute, non-symlink directory matching
/// the caller's observation. Visit own-mount names; never enter another mount
/// or a symlink, never open regular file contents, and never write anything.
/// An excluded nested mount is not an incomplete read of eligible names.
/// Call on a worker. Visitors see a provisional prefix: an incomplete result
/// or error must not be treated as a complete durable index.
pub fn visit_local_index(
    displayed_root: &Path,
    expected_root: FileIdentity,
    options: WalkOptions,
    cancel: &Cancellation,
    visit: impl FnMut(IndexEntry<'_>),
) -> Result<WalkSummary, ListError> {
    visit_local_index_with_open(
        displayed_root,
        expected_root,
        options,
        cancel,
        open_entry,
        visit,
    )
}

fn open_entry(parent: BorrowedFd<'_>, name: &std::ffi::CStr) -> Result<OwnedFd, Errno> {
    // PATH obtains metadata without opening contents; NOFOLLOW describes a
    // trailing symlink itself. NO_XDEV also catches same-device bind mounts.
    fs::openat2(
        parent,
        name,
        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
        WITHIN_ROOT,
    )
}

fn visit_local_index_with_open(
    displayed_root: &Path,
    expected_root: FileIdentity,
    options: WalkOptions,
    cancel: &Cancellation,
    mut open: impl FnMut(BorrowedFd<'_>, &std::ffi::CStr) -> Result<OwnedFd, Errno>,
    mut visit: impl FnMut(IndexEntry<'_>),
) -> Result<WalkSummary, ListError> {
    use rustix::fd::AsFd;
    if !displayed_root.is_absolute()
        || !(1..=MAX_ENTRIES).contains(&options.entries)
        || !(1..=MAX_DIRS).contains(&options.directories)
        || options.depth > MAX_DEPTH
    {
        return Err(invalid(
            displayed_root,
            "invalid local index root or budget",
        ));
    }
    let root_error = |source| ListError::ReadDirectory {
        path: displayed_root.to_path_buf(),
        source,
    };
    cancel.check()?;
    let root = DirectoryHandle::open_without_symlinks(displayed_root).map_err(root_error)?;
    if root.identity() != expected_root {
        return Err(invalid(displayed_root, "local index root changed"));
    }
    let mut summary = WalkSummary::default();
    let mut pending = vec![(PathBuf::new(), expected_root, 0usize)];
    'walk: while let Some((relative, expected_dir, depth)) = pending.pop() {
        cancel.check()?;
        let token = if relative.as_os_str().is_empty() {
            OsStr::new(".")
        } else {
            relative.as_os_str()
        };
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        let dir = (|| {
            let fd = fs::openat2(&root.fd, token, flags, Mode::empty(), WITHIN_ROOT)?;
            if identity(&fs::fstat(&fd)?) != expected_dir {
                return Err(rustix::io::Errno::STALE);
            }
            let reader = Dir::read_from(&fd)?;
            Ok::<_, rustix::io::Errno>((fd, reader))
        })();
        let (fd, directory) = match dir {
            Ok(dir) => dir,
            Err(error) if depth == 0 => return Err(root_error(error.into())),
            Err(_) => {
                summary.incomplete = true;
                summary.skipped += 1;
                continue;
            }
        };
        summary.read_directories += 1;
        for entry in directory {
            cancel.check()?;
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    summary.incomplete = true;
                    summary.skipped += 1;
                    break; // bounded even if an underlying readdir repeats an error
                }
            };
            let name = entry.file_name();
            let raw = name.to_bytes();
            if raw == b"." || raw == b".." {
                continue;
            }
            if summary.observed_names >= options.entries {
                summary.incomplete = true;
                break 'walk;
            }
            summary.observed_names += 1;
            if !options.show_hidden && raw.starts_with(b".") {
                continue;
            }
            let path = relative.join(OsStr::from_bytes(raw));
            if path.as_os_str().len() > MAX_PATH {
                summary.incomplete = true;
                summary.skipped += 1;
                continue;
            }
            let candidate = match open(fd.as_fd(), name) {
                Ok(candidate) => candidate,
                // This lookup is a single basename from readdir, with dot
                // and dot-dot filtered above. EXDEV here is a mount boundary,
                // never a reason to ignore failures reopening queued branches.
                Err(Errno::XDEV) => {
                    summary.excluded_mounts += 1;
                    continue;
                }
                Err(_) => {
                    summary.incomplete = true;
                    summary.skipped += 1;
                    continue;
                }
            };
            let Ok(stat) = fs::fstat(&candidate) else {
                summary.incomplete = true;
                summary.skipped += 1;
                continue;
            };
            let id = identity(&stat);
            if id.device != expected_root.device {
                summary.incomplete = true;
                summary.skipped += 1;
                continue;
            }
            let file_kind = kind(&stat);
            visit(IndexEntry {
                relative: &path,
                kind: file_kind,
                identity: id,
            });
            if file_kind == FileKind::Directory {
                if depth >= options.depth
                    || summary.read_directories + pending.len() >= options.directories
                {
                    summary.incomplete = true;
                    summary.skipped += 1;
                } else {
                    pending.push((path, id, depth + 1));
                }
            }
        }
        let still_here = fs::openat2(
            &root.fd,
            token,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
            WITHIN_ROOT,
        )
        .and_then(|fd| fs::fstat(&fd))
        .is_ok_and(|stat| identity(&stat) == expected_dir);
        if !still_here {
            summary.incomplete = true;
            summary.skipped += 1;
        }
    }
    cancel.check()?;
    if DirectoryHandle::open_without_symlinks(displayed_root)
        .map_err(root_error)?
        .identity()
        != expected_root
    {
        return Err(invalid(
            displayed_root,
            "local index root changed during scan",
        ));
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let n = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory =
                std::env::temp_dir().join(format!("jwm-index-walk-{}-{n}", std::process::id()));
            std::fs::create_dir(&directory).unwrap();
            Self(directory)
        }

        fn id(&self) -> FileIdentity {
            DirectoryHandle::open_without_symlinks(&self.0)
                .unwrap()
                .identity()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn options() -> WalkOptions {
        WalkOptions {
            entries: 50,
            directories: 10,
            depth: 4,
            show_hidden: false,
        }
    }

    #[test]
    fn bounded_index_walk_preserves_raw_keys_and_never_follows_links() {
        let root = Fixture::new();
        let outside = Fixture::new();
        std::fs::write(outside.0.join("not-in-index"), b"x").unwrap();
        std::fs::create_dir(root.0.join("branch")).unwrap();
        std::fs::create_dir(root.0.join(".hidden")).unwrap();
        symlink(&outside.0, root.0.join("outside-link")).unwrap();
        let raw = Path::new(OsStr::from_bytes(b"branch/name-%20-\x80"));
        let raw_peer = Path::new(OsStr::from_bytes(b"branch/name-%20-\x81"));
        std::fs::write(root.0.join(raw), b"x").unwrap();
        std::fs::write(root.0.join(raw_peer), b"y").unwrap();
        let mut found = Vec::new();
        let summary = visit_local_index(
            &root.0,
            root.id(),
            options(),
            &Cancellation::default(),
            |row| found.push((row.relative.to_path_buf(), row.kind)),
        )
        .unwrap();
        assert!(!summary.incomplete);
        assert_eq!(summary.read_directories, 2);
        assert!(found.contains(&(raw.to_path_buf(), FileKind::Regular)));
        assert_eq!(raw.to_string_lossy(), raw_peer.to_string_lossy());
        assert!(found.contains(&(raw_peer.to_path_buf(), FileKind::Regular)));
        assert!(found.contains(&(PathBuf::from("outside-link"), FileKind::Symlink)));
        assert_eq!(found.len(), 4);
    }

    #[test]
    fn capped_or_moved_directory_is_never_a_complete_index() {
        let root = Fixture::new();
        let outside = Fixture::new();
        std::fs::create_dir(root.0.join("branch")).unwrap();
        std::fs::write(root.0.join("branch/touch"), b"x").unwrap();
        let halted = visit_local_index(
            &root.0,
            root.id(),
            WalkOptions {
                directories: 1,
                ..options()
            },
            &Cancellation::default(),
            |_| {},
        )
        .unwrap();
        assert!(halted.incomplete);
        assert_eq!(halted.read_directories, 1);

        let summary = visit_local_index(
            &root.0,
            root.id(),
            options(),
            &Cancellation::default(),
            |row| {
                if row.relative == Path::new("branch/touch") {
                    std::fs::rename(root.0.join("branch"), outside.0.join("branch")).unwrap();
                }
            },
        )
        .unwrap();
        assert!(summary.incomplete);
        assert!(outside.0.join("branch/touch").is_file());
    }

    #[test]
    fn hidden_names_use_the_budget_without_becoming_results() {
        let root = Fixture::new();
        for name in [".one", ".two", ".three"] {
            std::fs::write(root.0.join(name), b"x").unwrap();
        }
        let summary = visit_local_index(
            &root.0,
            root.id(),
            WalkOptions {
                entries: 2,
                ..options()
            },
            &Cancellation::default(),
            |_| panic!("hidden entry must not be published"),
        )
        .unwrap();
        assert_eq!(summary.observed_names, 2);
        assert!(summary.incomplete);
        let exact = visit_local_index(
            &root.0,
            root.id(),
            WalkOptions {
                entries: 3,
                ..options()
            },
            &Cancellation::default(),
            |_| panic!("hidden entry must not be published"),
        )
        .unwrap();
        assert_eq!(exact.observed_names, 3);
        assert!(!exact.incomplete);
    }

    #[test]
    fn wrong_root_identity_and_cancelled_scan_emit_no_rows() {
        let root = Fixture::new();
        let other = Fixture::new();
        let cancel = Cancellation::default();
        assert!(visit_local_index(&root.0, other.id(), options(), &cancel, |_| panic!()).is_err());
        cancel.cancel();
        assert!(matches!(
            visit_local_index(&root.0, root.id(), options(), &cancel, |_| panic!()),
            Err(ListError::Cancelled)
        ));
    }

    #[test]
    fn replacing_the_displayed_root_discards_the_walk() {
        let root = Fixture::new();
        let outside = Fixture::new();
        std::fs::write(root.0.join("original"), b"data").unwrap();
        let original = root.id();
        let moved = outside.0.join("moved");
        let mut visited = false;
        let result = visit_local_index(
            &root.0,
            original,
            options(),
            &Cancellation::default(),
            |row| {
                assert_eq!(row.relative, Path::new("original"));
                assert!(!visited);
                visited = true;
                std::fs::rename(&root.0, &moved).unwrap();
                std::fs::create_dir(&root.0).unwrap();
                std::fs::write(root.0.join("replacement"), b"new").unwrap();
            },
        );
        assert!(visited);
        assert!(matches!(result, Err(ListError::ReadDirectory { .. })));
        assert!(moved.join("original").is_file());
    }

    #[test]
    fn omitted_mount_is_complete_but_a_discovery_error_is_not() {
        let root = Fixture::new();
        std::fs::write(root.0.join("local"), b"data").unwrap();
        std::fs::create_dir(root.0.join("nested")).unwrap();
        std::fs::write(root.0.join("nested/not-eligible"), b"data").unwrap();
        for (failure, incomplete) in [(Errno::XDEV, false), (Errno::IO, true)] {
            let mut seen = Vec::new();
            let result = visit_local_index_with_open(
                &root.0,
                root.id(),
                options(),
                &Cancellation::default(),
                |parent, name| {
                    if name.to_bytes() == b"nested" {
                        Err(failure)
                    } else {
                        open_entry(parent, name)
                    }
                },
                |row| seen.push(row.relative.to_path_buf()),
            )
            .unwrap();
            assert_eq!(seen, [PathBuf::from("local")]);
            assert_eq!(result.observed_names, 2);
            assert_eq!(result.read_directories, 1);
            assert_eq!(result.excluded_mounts, if incomplete { 0 } else { 1 });
            assert_eq!(result.skipped, if incomplete { 1 } else { 0 });
            assert_eq!(result.incomplete, incomplete);
        }
    }

    #[test]
    fn refuses_a_symlink_as_an_explicit_root() {
        let root = Fixture::new();
        let outside = Fixture::new();
        std::fs::write(root.0.join("original"), b"data").unwrap();
        let alias = outside.0.join("alias");
        symlink(&root.0, &alias).unwrap();
        assert!(
            visit_local_index(
                &alias,
                root.id(),
                options(),
                &Cancellation::default(),
                |_| panic!("explicit root alias must never yield rows"),
            )
            .is_err()
        );
    }
}
