//! One bounded same-mount directory rename, without a copy/delete fallback.
use super::{RegularMoveError, check, identity, rename_regular, verify_parent};
use crate::{
    Cancellation, DirectoryHandle, FileIdentity, TreeCopyPlanError, TreeCopyTargetError,
    check_tree_copy_target, plan_tree_copy,
};
use rustix::{
    fd::{AsFd, OwnedFd},
    fs::{self, AtFlags, Mode, OFlags, ResolveFlags, StatxFlags},
    io::Errno,
    process::geteuid,
};
use std::{ffi::OsStr, io, path::Path};

pub struct FolderMoveSource<'a> {
    pub parent: &'a DirectoryHandle,
    pub displayed_parent: &'a Path,
    pub name: &'a OsStr,
    pub identity: FileIdentity,
}

#[derive(Debug)]
pub enum FolderMoveError {
    Guard(RegularMoveError),
    NotDirectory,
    SourceNotPrivate,
    MountIdentityUnavailable(io::Error),
    Plan(TreeCopyPlanError),
    Target(TreeCopyTargetError),
}

impl From<RegularMoveError> for FolderMoveError {
    fn from(error: RegularMoveError) -> Self {
        Self::Guard(error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FolderMoveOutcome {
    Moved,
    /// Rename succeeded, but exact publication could not be confirmed.
    /// Do not automatically retry, undo, or delete either name.
    MovedUnverified,
}

fn guard_io(error: Errno) -> FolderMoveError {
    RegularMoveError::Io(error.into()).into()
}

fn mount(fd: impl AsFd) -> Result<u64, FolderMoveError> {
    let info = fs::statx(fd, "", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID)
        .map_err(|error| FolderMoveError::MountIdentityUnavailable(error.into()))?;
    if info.stx_mask & StatxFlags::MNT_ID.bits() == 0 {
        return Err(FolderMoveError::MountIdentityUnavailable(
            io::ErrorKind::Unsupported.into(),
        ));
    }
    Ok(info.stx_mnt_id)
}

fn verify_mounted_parent(
    held: &DirectoryHandle,
    path: &Path,
    expected_mount: u64,
) -> Result<(), FolderMoveError> {
    verify_parent(held, path)?;
    let current = DirectoryHandle::open_without_symlinks(path)
        .map_err(|_| RegularMoveError::ParentChanged)?;
    if current.identity() != held.identity() || mount(&current.fd)? != expected_mount {
        return Err(RegularMoveError::ParentChanged.into());
    }
    Ok(())
}

fn open_leaf(parent: &DirectoryHandle, name: &OsStr) -> Result<OwnedFd, FolderMoveError> {
    fs::openat2(
        &parent.fd,
        name,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
    )
    .map_err(|error| match error {
        Errno::LOOP | Errno::NOTDIR => FolderMoveError::NotDirectory,
        Errno::XDEV => RegularMoveError::CrossMount.into(),
        Errno::NOENT => RegularMoveError::SourceChanged.into(),
        other => guard_io(other),
    })
}

fn observed_folder(
    parent: &DirectoryHandle,
    name: &OsStr,
    expected: FileIdentity,
    expected_mount: u64,
) -> Result<OwnedFd, FolderMoveError> {
    let pin = open_leaf(parent, name)?;
    let stat = fs::fstat(&pin).map_err(guard_io)?;
    if identity(&stat) != expected {
        return Err(RegularMoveError::SourceChanged.into());
    }
    if stat.st_uid != geteuid().as_raw() || stat.st_mode & 0o022 != 0 {
        return Err(FolderMoveError::SourceNotPrivate);
    }
    if mount(&pin)? != expected_mount {
        return Err(RegularMoveError::CrossMount.into());
    }
    Ok(pin)
}

/// Move one observed ordinary directory to the same leaf name under a
/// different observed private parent. Call off the UI thread. Only complete,
/// bounded ordinary trees are accepted: no links, special entries, or mounted
/// descendants. This checks metadata, not bytes, and never opens file payloads.
/// A checked tree is not a snapshot or a barrier against other writers.
/// There is one atomic no-replace rename and no crash durability promise.
pub fn move_folder_no_replace(
    source: FolderMoveSource<'_>,
    destination: &DirectoryHandle,
    displayed_destination: &Path,
    cancel: &Cancellation,
) -> Result<FolderMoveOutcome, FolderMoveError> {
    move_folder_with(
        source,
        destination,
        displayed_destination,
        cancel,
        rename_regular,
    )
}

fn move_folder_with(
    source: FolderMoveSource<'_>,
    destination: &DirectoryHandle,
    displayed_destination: &Path,
    cancel: &Cancellation,
    rename: impl FnOnce(&DirectoryHandle, &OsStr, &DirectoryHandle) -> Result<(), Errno>,
) -> Result<FolderMoveOutcome, FolderMoveError> {
    if !crate::mkdir::single_name(source.name) || source.name.len() > 255 {
        return Err(RegularMoveError::InvalidName.into());
    }
    check(cancel)?;
    verify_parent(source.parent, source.displayed_parent)?;
    verify_parent(destination, displayed_destination)?;
    if source.parent.identity() == destination.identity() {
        return Err(RegularMoveError::SameDirectory.into());
    }
    let mount_id = mount(&source.parent.fd)?;
    if source.parent.identity().device != destination.identity().device
        || mount(&destination.fd)? != mount_id
    {
        return Err(RegularMoveError::CrossMount.into());
    }
    let pin = observed_folder(source.parent, source.name, source.identity, mount_id)?;
    let root = source.displayed_parent.join(source.name);
    let plan = plan_tree_copy(&root, source.identity, cancel).map_err(FolderMoveError::Plan)?;
    check_tree_copy_target(&plan, destination, displayed_destination, cancel)
        .map_err(FolderMoveError::Target)?;
    verify_mounted_parent(source.parent, source.displayed_parent, mount_id)?;
    verify_mounted_parent(destination, displayed_destination, mount_id)?;
    let latest = observed_folder(source.parent, source.name, source.identity, mount_id)?;
    check(cancel)?;
    rename(source.parent, source.name, destination).map_err(|error| match error {
        Errno::EXIST => RegularMoveError::DestinationExists.into(),
        Errno::XDEV => RegularMoveError::CrossMount.into(),
        Errno::NOENT => RegularMoveError::SourceChanged.into(),
        other => guard_io(other),
    })?;

    let at_destination =
        observed_folder(destination, source.name, source.identity, mount_id).is_ok();
    let old_name_does_not_hold_original =
        match fs::statat(&source.parent.fd, source.name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => identity(&stat) != source.identity,
            Err(Errno::NOENT) => true,
            Err(_) => false,
        };
    let verified = at_destination
        && old_name_does_not_hold_original
        && verify_mounted_parent(source.parent, source.displayed_parent, mount_id).is_ok()
        && verify_mounted_parent(destination, displayed_destination, mount_id).is_ok();
    drop(latest);
    drop(pin);
    Ok(if verified {
        FolderMoveOutcome::Moved
    } else {
        FolderMoveOutcome::MovedUnverified
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs as local,
        os::unix::{
            ffi::OsStrExt,
            fs::{DirBuilderExt, PermissionsExt, symlink},
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
            let root = std::env::temp_dir().join(format!(
                "jwm-private-folder-move-{}-{time}",
                std::process::id()
            ));
            local::DirBuilder::new().mode(0o700).create(&root).unwrap();
            local::create_dir(root.join("source")).unwrap();
            local::create_dir(root.join("target")).unwrap();
            Self(root)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = local::remove_dir_all(&self.0);
        }
    }

    struct MoveCase {
        _root: Fixture,
        from_path: PathBuf,
        to_path: PathBuf,
        from: DirectoryHandle,
        to: DirectoryHandle,
        name: &'static OsStr,
        expected: FileIdentity,
    }

    impl MoveCase {
        fn new() -> Self {
            let root = Fixture::new();
            let from_path = root.0.join("source");
            let to_path = root.0.join("target");
            let name = OsStr::from_bytes(b"folder-%\x80");
            local::create_dir(from_path.join(name)).unwrap();
            local::create_dir(from_path.join(name).join("empty")).unwrap();
            local::write(from_path.join(name).join(".raw"), b"exact contents").unwrap();
            let from = DirectoryHandle::open_without_symlinks(&from_path).unwrap();
            let to = DirectoryHandle::open_without_symlinks(&to_path).unwrap();
            let expected = identity(&fs::fstat(open_leaf(&from, name).unwrap()).unwrap());
            Self {
                _root: root,
                from_path,
                to_path,
                from,
                to,
                name,
                expected,
            }
        }

        fn source(&self) -> FolderMoveSource<'_> {
            FolderMoveSource {
                parent: &self.from,
                displayed_parent: &self.from_path,
                name: self.name,
                identity: self.expected,
            }
        }

        fn run(&self, cancel: &Cancellation) -> Result<FolderMoveOutcome, FolderMoveError> {
            move_folder_no_replace(self.source(), &self.to, &self.to_path, cancel)
        }

        fn fixture_intact(&self) {
            assert_eq!(
                identity(&fs::fstat(open_leaf(&self.from, self.name).unwrap()).unwrap()),
                self.expected
            );
            assert_eq!(
                local::read(self.from_path.join(self.name).join(".raw")).unwrap(),
                b"exact contents"
            );
        }
    }

    #[test]
    fn folder_move_preserves_raw_identity_hidden_bytes_and_empty_directories() {
        let case = MoveCase::new();
        assert_eq!(
            case.run(&Cancellation::default()).unwrap(),
            FolderMoveOutcome::Moved
        );
        let target = case.to_path.join(case.name);
        assert!(!case.from_path.join(case.name).exists());
        assert_eq!(
            identity(&fs::fstat(open_leaf(&case.to, case.name).unwrap()).unwrap()),
            case.expected
        );
        assert!(target.join("empty").is_dir());
        assert_eq!(local::read(target.join(".raw")).unwrap(), b"exact contents");
    }

    #[test]
    fn folder_move_keeps_occupied_target_and_refuses_descendant_parent() {
        let case = MoveCase::new();
        let target = case.to_path.join(case.name);
        local::create_dir(&target).unwrap();
        local::write(target.join("keep"), b"sentinel").unwrap();
        assert!(matches!(
            case.run(&Cancellation::default()),
            Err(FolderMoveError::Guard(RegularMoveError::DestinationExists))
        ));
        assert_eq!(local::read(target.join("keep")).unwrap(), b"sentinel");
        let inside = case.from_path.join(case.name).join("empty");
        let handle = DirectoryHandle::open_without_symlinks(&inside).unwrap();
        assert!(matches!(
            move_folder_no_replace(case.source(), &handle, &inside, &Cancellation::default()),
            Err(FolderMoveError::Target(
                TreeCopyTargetError::TargetInsideSource
            ))
        ));
        case.fixture_intact();
    }

    #[test]
    fn folder_move_refuses_cancellation_and_nested_symlink_without_writes() {
        let case = MoveCase::new();
        let cancel = Cancellation::default();
        cancel.cancel();
        assert!(matches!(
            case.run(&cancel),
            Err(FolderMoveError::Guard(RegularMoveError::Cancelled))
        ));
        symlink(&case.to_path, case.from_path.join(case.name).join("link")).unwrap();
        assert!(matches!(
            case.run(&Cancellation::default()),
            Err(FolderMoveError::Plan(TreeCopyPlanError::Unsupported(_)))
        ));
        assert!(!case.to_path.join(case.name).exists());
        case.fixture_intact();
    }

    #[test]
    fn folder_move_rejects_stale_leaf_and_has_no_cross_mount_fallback() {
        let case = MoveCase::new();
        let cancel = Cancellation::default();
        assert!(matches!(
            move_folder_with(case.source(), &case.to, &case.to_path, &cancel, |_, _, _| {
                Err(Errno::XDEV)
            }),
            Err(FolderMoveError::Guard(RegularMoveError::CrossMount))
        ));
        assert!(!case.to_path.join(case.name).exists());
        case.fixture_intact();

        let old = case.from_path.join(case.name);
        let held = case.from_path.join("held-original");
        local::rename(&old, &held).unwrap();
        local::create_dir(&old).unwrap();
        assert!(matches!(
            case.run(&cancel),
            Err(FolderMoveError::Guard(RegularMoveError::SourceChanged))
        ));
        assert!(!case.to_path.join(case.name).exists());
        assert_eq!(local::read(held.join(".raw")).unwrap(), b"exact contents");
    }

    #[test]
    fn folder_move_rejects_nonprivate_root_and_replaced_displayed_parent() {
        let case = MoveCase::new();
        let source = case.from_path.join(case.name);
        local::set_permissions(&source, local::Permissions::from_mode(0o775)).unwrap();
        assert!(matches!(
            case.run(&Cancellation::default()),
            Err(FolderMoveError::SourceNotPrivate)
        ));
        local::set_permissions(&source, local::Permissions::from_mode(0o755)).unwrap();
        let held = case.to_path.with_file_name("held-target-parent");
        local::rename(&case.to_path, &held).unwrap();
        local::create_dir(&case.to_path).unwrap();
        assert!(matches!(
            case.run(&Cancellation::default()),
            Err(FolderMoveError::Guard(RegularMoveError::ParentChanged))
        ));
        assert!(!case.to_path.join(case.name).exists());
        assert!(!held.join(case.name).exists());
        case.fixture_intact();
    }

    #[test]
    fn folder_move_does_not_retry_or_rollback_unverified_publication() {
        let case = MoveCase::new();
        let target = case.to_path.join(case.name);
        let hidden = case.to_path.join("displaced");
        let result = move_folder_with(
            case.source(),
            &case.to,
            &case.to_path,
            &Cancellation::default(),
            |from, name, to| {
                rename_regular(from, name, to)?;
                local::rename(&target, &hidden).unwrap();
                local::create_dir(&target).unwrap();
                local::write(target.join("sentinel"), b"keep").unwrap();
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(result, FolderMoveOutcome::MovedUnverified);
        assert!(!case.from_path.join(case.name).exists());
        assert_eq!(local::read(target.join("sentinel")).unwrap(), b"keep");
        assert_eq!(local::read(hidden.join(".raw")).unwrap(), b"exact contents");
    }
}
