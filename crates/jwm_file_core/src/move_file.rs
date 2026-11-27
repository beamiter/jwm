//! Explicit same-mount no-replace movement of one observed regular file.
use crate::{Cancellation, DirectoryHandle, FileIdentity};
use rustix::{
    fs::{self, AtFlags, FileType, Mode, OFlags, RenameFlags},
    io::Errno,
    process::geteuid,
};
use std::{ffi::OsStr, io, path::Path};

pub struct RegularMoveSource<'a> {
    pub parent: &'a DirectoryHandle,
    pub displayed_parent: &'a Path,
    pub name: &'a OsStr,
    pub identity: FileIdentity,
}

#[derive(Debug)]
pub enum RegularMoveError {
    InvalidName,
    SameDirectory,
    ParentChanged,
    ParentNotPrivate,
    SourceChanged,
    NotRegular,
    DestinationExists,
    CrossMount,
    Cancelled,
    Io(io::Error),
}

/// A successful rename is never reported as cancelled afterwards.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegularMoveOutcome {
    Moved,
    /// Rename completed; the new name could not be verified against the held
    /// source inode. Do not retry, delete, or automatically roll this back.
    MovedUnverified,
}

fn check(cancel: &Cancellation) -> Result<(), RegularMoveError> {
    cancel.check().map_err(|_| RegularMoveError::Cancelled)
}

fn verify_parent(held: &DirectoryHandle, path: &Path) -> Result<(), RegularMoveError> {
    let reopened = DirectoryHandle::open_without_symlinks(path).map_err(|error| {
        if matches!(
            error.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
        ) {
            RegularMoveError::ParentChanged
        } else {
            RegularMoveError::Io(error)
        }
    })?;
    if reopened.identity() != held.identity() {
        return Err(RegularMoveError::ParentChanged);
    }
    let stat = fs::fstat(&held.fd).map_err(|error| RegularMoveError::Io(error.into()))?;
    if stat.st_uid != geteuid().as_raw() || stat.st_mode & 0o022 != 0 {
        return Err(RegularMoveError::ParentNotPrivate);
    }
    Ok(())
}

fn identity(stat: &fs::Stat) -> FileIdentity {
    FileIdentity {
        device: stat.st_dev,
        inode: stat.st_ino,
    }
}

fn source_error(error: Errno) -> RegularMoveError {
    if error == Errno::NOENT {
        RegularMoveError::SourceChanged
    } else {
        RegularMoveError::Io(error.into())
    }
}

/// Move one regular file to the same leaf name in the explicitly selected
/// destination. There is no copy/delete fallback. Both parents must be owned
/// by the effective user and not writable by group or others.
///
/// This is one atomic no-replace rename, not atomic compare-inode-and-rename.
/// Other writers can race namespace checks; a mismatched postcondition is
/// returned separately rather than treated as permission to retry or undo.
/// Call on a worker. No crash- or power-loss durability is implied.
pub fn move_regular_no_replace(
    source: RegularMoveSource<'_>,
    destination: &DirectoryHandle,
    displayed_destination: &Path,
    cancel: &Cancellation,
) -> Result<RegularMoveOutcome, RegularMoveError> {
    move_regular_with(
        source,
        destination,
        displayed_destination,
        cancel,
        rename_regular,
    )
}

fn rename_regular(
    source: &DirectoryHandle,
    name: &OsStr,
    destination: &DirectoryHandle,
) -> Result<(), Errno> {
    fs::renameat_with(
        &source.fd,
        name,
        &destination.fd,
        name,
        RenameFlags::NOREPLACE,
    )
}

fn move_regular_with(
    source: RegularMoveSource<'_>,
    destination: &DirectoryHandle,
    displayed_destination: &Path,
    cancel: &Cancellation,
    rename: impl FnOnce(&DirectoryHandle, &OsStr, &DirectoryHandle) -> Result<(), Errno>,
) -> Result<RegularMoveOutcome, RegularMoveError> {
    if !crate::mkdir::single_name(source.name) || source.name.len() > 255 {
        return Err(RegularMoveError::InvalidName);
    }
    check(cancel)?;
    verify_parent(source.parent, source.displayed_parent)?;
    verify_parent(destination, displayed_destination)?;
    if source.parent.identity() == destination.identity() {
        return Err(RegularMoveError::SameDirectory);
    }
    if source.parent.identity().device != destination.identity().device {
        return Err(RegularMoveError::CrossMount);
    }
    let pin = fs::openat(
        &source.parent.fd,
        source.name,
        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(source_error)?;
    let before = fs::fstat(&pin).map_err(|error| RegularMoveError::Io(error.into()))?;
    if identity(&before) != source.identity {
        return Err(RegularMoveError::SourceChanged);
    }
    if FileType::from_raw_mode(before.st_mode) != FileType::RegularFile {
        return Err(RegularMoveError::NotRegular);
    }
    verify_parent(source.parent, source.displayed_parent)?;
    verify_parent(destination, displayed_destination)?;
    let visible = fs::statat(&source.parent.fd, source.name, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(source_error)?;
    if identity(&visible) != source.identity {
        return Err(RegularMoveError::SourceChanged);
    }
    check(cancel)?;
    rename(source.parent, source.name, destination).map_err(|error| match error {
        Errno::EXIST => RegularMoveError::DestinationExists,
        Errno::XDEV => RegularMoveError::CrossMount,
        other => source_error(other),
    })?;
    let published = fs::statat(&destination.fd, source.name, AtFlags::SYMLINK_NOFOLLOW);
    let verified = published.is_ok_and(|stat| identity(&stat) == identity(&before))
        && verify_parent(destination, displayed_destination).is_ok();
    // Keep a reference to the inode across both the publication and check.
    drop(pin);
    Ok(if verified {
        RegularMoveOutcome::Moved
    } else {
        RegularMoveOutcome::MovedUnverified
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs as files,
        os::unix::{
            ffi::OsStrExt,
            fs::{DirBuilderExt, MetadataExt, PermissionsExt, symlink},
        },
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let time = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let base = std::env::temp_dir()
                .join(format!("jwm-private-file-move-{}-{time}", std::process::id()));
            files::DirBuilder::new().mode(0o700).create(&base).unwrap();
            files::create_dir(base.join("from")).unwrap();
            files::create_dir(base.join("to")).unwrap();
            Self(base)
        }

        fn opened(&self) -> (DirectoryHandle, PathBuf, DirectoryHandle, PathBuf) {
            let from = self.0.join("from");
            let to = self.0.join("to");
            (
                DirectoryHandle::open_without_symlinks(&from).unwrap(),
                from,
                DirectoryHandle::open_without_symlinks(&to).unwrap(),
                to,
            )
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = files::remove_dir_all(&self.0);
        }
    }

    fn observe<'a>(
        parent: &'a DirectoryHandle,
        path: &'a Path,
        name: &'a OsStr,
    ) -> RegularMoveSource<'a> {
        let m = files::symlink_metadata(path.join(name)).unwrap();
        RegularMoveSource {
            parent,
            displayed_parent: path,
            name,
            identity: FileIdentity {
                device: m.dev(),
                inode: m.ino(),
            },
        }
    }

    #[test]
    fn move_keeps_exact_native_identity_and_refuses_existing_symlink() {
        let fixture = Fixture::new();
        let (from, from_path, to, to_path) = fixture.opened();
        let name = OsStr::from_bytes(b"item%\x80");
        let origin = from_path.join(name);
        let target = to_path.join(name);
        files::write(&origin, b"expected source").unwrap();
        files::write(to_path.join("keep"), b"keep me").unwrap();
        symlink("keep", &target).unwrap();
        let cancel = Cancellation::default();
        let before = observe(&from, &from_path, name).identity;
        assert!(matches!(
            move_regular_no_replace(observe(&from, &from_path, name), &to, &to_path, &cancel),
            Err(RegularMoveError::DestinationExists)
        ));
        assert_eq!(files::read(&origin).unwrap(), b"expected source");
        assert_eq!(files::read_link(&target).unwrap(), Path::new("keep"));
        assert_eq!(files::read(to_path.join("keep")).unwrap(), b"keep me");
        files::remove_file(&target).unwrap();
        assert!(matches!(
            move_regular_no_replace(observe(&from, &from_path, name), &to, &to_path, &cancel),
            Ok(RegularMoveOutcome::Moved)
        ));
        assert!(!origin.exists());
        assert_eq!(observe(&to, &to_path, name).identity, before);
        assert_eq!(files::read(target).unwrap(), b"expected source");
    }

    #[test]
    fn move_refuses_stale_source_and_group_writable_target() {
        let fixture = Fixture::new();
        let (from, from_path, to, to_path) = fixture.opened();
        let name = OsStr::new("item");
        files::write(from_path.join(name), b"old").unwrap();
        let selected = observe(&from, &from_path, name);
        files::rename(from_path.join(name), from_path.join("saved")).unwrap();
        files::write(from_path.join(name), b"replacement").unwrap();
        let cancel = Cancellation::default();
        assert!(matches!(
            move_regular_no_replace(selected, &to, &to_path, &cancel),
            Err(RegularMoveError::SourceChanged)
        ));
        files::set_permissions(&to_path, files::Permissions::from_mode(0o775)).unwrap();
        assert!(matches!(
            move_regular_no_replace(observe(&from, &from_path, name), &to, &to_path, &cancel),
            Err(RegularMoveError::ParentNotPrivate)
        ));
        assert_eq!(files::read(from_path.join(name)).unwrap(), b"replacement");
        assert!(files::read_dir(to_path).unwrap().next().is_none());
    }

    #[test]
    fn move_refuses_symlink_source_and_replaced_destination() {
        let fixture = Fixture::new();
        let (from, from_path, to, to_path) = fixture.opened();
        let name = OsStr::new("link");
        files::write(from_path.join("real"), b"original").unwrap();
        symlink("real", from_path.join(name)).unwrap();
        let cancel = Cancellation::default();
        assert!(matches!(
            move_regular_no_replace(observe(&from, &from_path, name), &to, &to_path, &cancel),
            Err(RegularMoveError::NotRegular)
        ));
        assert_eq!(files::read_link(from_path.join(name)).unwrap(), Path::new("real"));
        assert!(!to_path.join(name).exists());
        files::rename(&to_path, fixture.0.join("old-target")).unwrap();
        files::create_dir(&to_path).unwrap();
        assert!(matches!(
            move_regular_no_replace(
                observe(&from, &from_path, OsStr::new("real")),
                &to,
                &to_path,
                &cancel,
            ),
            Err(RegularMoveError::ParentChanged)
        ));
        assert_eq!(files::read(from_path.join("real")).unwrap(), b"original");
        assert!(files::read_dir(to_path).unwrap().next().is_none());
    }

    #[test]
    fn move_verified_success_survives_late_cancellation_and_same_parent_is_rejected() {
        let fixture = Fixture::new();
        let (from, from_path, to, to_path) = fixture.opened();
        let name = OsStr::new("item");
        files::write(from_path.join(name), b"source").unwrap();
        let cancel = Cancellation::default();
        assert!(matches!(
            move_regular_no_replace(
                observe(&from, &from_path, name),
                &from,
                &from_path,
                &cancel
            ),
            Err(RegularMoveError::SameDirectory)
        ));
        let result = move_regular_with(
            observe(&from, &from_path, name),
            &to,
            &to_path,
            &cancel,
            |from, name, to| {
                rename_regular(from, name, to)?;
                cancel.cancel();
                Ok(())
            },
        );
        assert!(matches!(result, Ok(RegularMoveOutcome::Moved)));
        assert!(!from_path.join(name).exists());
        assert_eq!(files::read(to_path.join(name)).unwrap(), b"source");
    }

    #[test]
    fn move_has_no_fallback_on_reported_cross_mount_or_early_cancel() {
        let fixture = Fixture::new();
        let (from, from_path, to, to_path) = fixture.opened();
        let name = OsStr::new("item");
        files::write(from_path.join(name), b"local").unwrap();
        let cancel = Cancellation::default();
        assert!(matches!(
            move_regular_with(
                observe(&from, &from_path, name),
                &to,
                &to_path,
                &cancel,
                |_, _, _| Err(Errno::XDEV),
            ),
            Err(RegularMoveError::CrossMount)
        ));
        cancel.cancel();
        assert!(matches!(
            move_regular_with(
                observe(&from, &from_path, name),
                &to,
                &to_path,
                &cancel,
                |_, _, _| panic!("rename must not start"),
            ),
            Err(RegularMoveError::Cancelled)
        ));
        assert_eq!(files::read(from_path.join(name)).unwrap(), b"local");
        assert!(!to_path.join(name).exists());
    }

    #[test]
    fn move_does_not_undo_when_the_destination_is_unverified_after_rename() {
        let fixture = Fixture::new();
        let (from, from_path, to, to_path) = fixture.opened();
        let name = OsStr::new("item");
        files::write(from_path.join(name), b"source").unwrap();
        let cancel = Cancellation::default();
        let result = move_regular_with(
            observe(&from, &from_path, name),
            &to,
            &to_path,
            &cancel,
            |from, name, to| {
                rename_regular(from, name, to)?;
                files::rename(to_path.join(name), to_path.join("relocated")).unwrap();
                files::write(to_path.join(name), b"new occupant").unwrap();
                cancel.cancel();
                Ok(())
            },
        );
        assert!(matches!(result, Ok(RegularMoveOutcome::MovedUnverified)));
        assert!(!from_path.join(name).exists());
        assert_eq!(files::read(to_path.join(name)).unwrap(), b"new occupant");
        assert_eq!(files::read(to_path.join("relocated")).unwrap(), b"source");
    }
}
