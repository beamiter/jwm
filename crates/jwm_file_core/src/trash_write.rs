//! Private Home Trash write preparation; opening these handles changes no files.
use crate::{
    Cancellation, DirectoryHandle, FileIdentity, RegularMoveSource,
    trash_info::{encode_home_trash_info, trash_storage_key},
};
use rustix::{
    fs::{self, AtFlags, FileType, Mode, OFlags, RenameFlags, StatxFlags},
    io::Errno,
    process::geteuid,
};
use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io::{self, Write},
    os::{fd::OwnedFd, unix::fs::FileExt},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug)]
pub enum HomeTrashWriteError {
    Refused(io::Error),
    /// The source was not moved by this operation; its metadata reservation
    /// could not be safely cleared. Do not replay or delete the payload.
    SidecarMayRemain {
        storage_key: OsString,
        cause: io::Error,
    },
}

#[derive(Debug, Eq, PartialEq)]
pub enum HomeTrashWriteOutcome {
    Moved {
        storage_key: OsString,
    },
    /// Rename succeeded, but the original identity and matching sidecar
    /// could not both be verified. Do not retry or automatically undo.
    MovedUnverified {
        storage_key: OsString,
    },
}

/// Explicitly move one observed regular file into the Home Trash on the
/// same mount. The XDG data directory must already exist. Call on a worker.
/// This operation has no permanent-unlink or cross-mount fallback and does
/// not promise power-loss durability or protection from every same-user race.
pub fn trash_regular_at_home(
    source: RegularMoveSource<'_>,
    data_home: &Path,
    local_deletion_date: &str,
    cancel: &Cancellation,
) -> Result<HomeTrashWriteOutcome, HomeTrashWriteError> {
    trash_with(source, data_home, local_deletion_date, cancel, || {})
}

fn trash_with(
    source: RegularMoveSource<'_>,
    data_home: &Path,
    local_deletion_date: &str,
    cancel: &Cancellation,
    before_publish: impl FnOnce(),
) -> Result<HomeTrashWriteOutcome, HomeTrashWriteError> {
    let refused = HomeTrashWriteError::Refused;
    let source_path = source.displayed_parent.join(source.name);
    let trash_path = data_home.join("Trash");
    if !crate::mkdir::single_name(source.name)
        || source.name.len() > 255
        || source_path.starts_with(&trash_path)
    {
        return Err(refused(io::ErrorKind::InvalidInput.into()));
    }
    let encoded = encode_home_trash_info(&source_path, local_deletion_date)
        .ok_or_else(|| refused(io::ErrorKind::InvalidInput.into()))?;
    cancel
        .check()
        .map_err(|_| refused(io::ErrorKind::Interrupted.into()))?;
    let pin = observe_source(&source).map_err(refused)?;
    let data = DirectoryHandle::open_without_symlinks(data_home).map_err(refused)?;
    require_user_controlled(&data).map_err(refused)?;
    require_same_mount(&data, source.parent).map_err(refused)?;
    let roots =
        TrashWriteRoots::initialize_existing_data_home(data_home, cancel).map_err(refused)?;
    if roots.data_home.identity() != data.identity() {
        return Err(refused(io::Error::other(
            "the displayed data directory changed",
        )));
    }
    require_same_mount(&roots.data_home, &data).map_err(refused)?;
    require_same_mount(&roots.files, source.parent).map_err(refused)?;
    let (key, sidecar) = reserve_unique(&roots, source.name, encoded, cancel)?;
    before_publish();
    let attempted = (|| -> io::Result<()> {
        roots.revalidate(cancel)?;
        sidecar.verify(&roots)?;
        check_source_name(&source)?;
        cancel.check().map_err(|_| io::ErrorKind::Interrupted)?;
        fs::renameat_with(
            &source.parent.fd,
            source.name,
            &roots.files.fd,
            &key,
            RenameFlags::NOREPLACE,
        )?;
        Ok(())
    })();
    if let Err(cause) = attempted {
        // The rename did not succeed. Remove only the reservation that still
        // points to our held sidecar. No application-level payload unlink.
        if roots.revalidate(&Cancellation::default()).is_ok()
            && sidecar.verify(&roots).is_ok()
            && fs::unlinkat(&roots.info.fd, &sidecar.name, AtFlags::empty()).is_ok()
        {
            return Err(refused(cause));
        }
        return Err(HomeTrashWriteError::SidecarMayRemain {
            storage_key: key,
            cause,
        });
    }
    let independently_verified = roots.revalidate(&Cancellation::default()).is_ok()
        && sidecar.verify(&roots).is_ok()
        && fs::statat(&roots.files.fd, &key, AtFlags::SYMLINK_NOFOLLOW)
            .is_ok_and(|published| identity(&published) == source.identity);
    drop(pin);
    Ok(if independently_verified {
        HomeTrashWriteOutcome::Moved { storage_key: key }
    } else {
        HomeTrashWriteOutcome::MovedUnverified { storage_key: key }
    })
}

fn reserve_unique(
    roots: &TrashWriteRoots,
    basename: &OsStr,
    encoded: Vec<u8>,
    cancel: &Cancellation,
) -> Result<(OsString, SidecarReservation), HomeTrashWriteError> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let clock = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| HomeTrashWriteError::Refused(io::Error::other(error)))?;
    for _ in 0..16 {
        let counter = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let token = format!(
            "{:x}{:08x}{:016x}",
            clock.as_nanos(),
            std::process::id(),
            counter
        );
        let key = trash_storage_key(basename, &token)
            .ok_or_else(|| HomeTrashWriteError::Refused(io::ErrorKind::InvalidInput.into()))?;
        match SidecarReservation::acquire(roots, &key, encoded.clone(), cancel) {
            Ok(sidecar) => return Ok((key, sidecar)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(HomeTrashWriteError::SidecarMayRemain {
                    storage_key: key,
                    cause: error,
                });
            }
        }
    }
    Err(HomeTrashWriteError::Refused(
        io::ErrorKind::AlreadyExists.into(),
    ))
}

fn mount_id(handle: &DirectoryHandle) -> io::Result<u64> {
    let info = fs::statx(&handle.fd, "", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID)?;
    if info.stx_mask & StatxFlags::MNT_ID.bits() == 0 {
        return Err(io::ErrorKind::Unsupported.into());
    }
    Ok(info.stx_mnt_id)
}

fn require_same_mount(a: &DirectoryHandle, b: &DirectoryHandle) -> io::Result<()> {
    if a.identity().device != b.identity().device || mount_id(a)? != mount_id(b)? {
        return Err(Errno::XDEV.into());
    }
    Ok(())
}

fn identity(stat: &fs::Stat) -> FileIdentity {
    FileIdentity {
        device: stat.st_dev,
        inode: stat.st_ino,
    }
}

fn check_source_name(source: &RegularMoveSource<'_>) -> io::Result<()> {
    let parent = DirectoryHandle::open_without_symlinks(source.displayed_parent)?;
    require_user_controlled(&parent)?;
    require_same_mount(&parent, source.parent)?;
    let entry = fs::statat(&source.parent.fd, source.name, AtFlags::SYMLINK_NOFOLLOW)?;
    if parent.identity() != source.parent.identity()
        || identity(&entry) != source.identity
        || FileType::from_raw_mode(entry.st_mode) != FileType::RegularFile
    {
        return Err(io::Error::other("the selected Home Trash source changed"));
    }
    Ok(())
}

fn observe_source(source: &RegularMoveSource<'_>) -> io::Result<OwnedFd> {
    check_source_name(source)?;
    let fd = fs::openat(
        &source.parent.fd,
        source.name,
        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let pinned = fs::fstat(&fd)?;
    if identity(&pinned) != source.identity
        || FileType::from_raw_mode(pinned.st_mode) != FileType::RegularFile
    {
        return Err(io::Error::other("the selected Home Trash source changed"));
    }
    Ok(fd)
}

pub(crate) struct TrashWriteRoots {
    data_home: DirectoryHandle,
    displayed_home: PathBuf,
    trash: DirectoryHandle,
    files: DirectoryHandle,
    info: DirectoryHandle,
}

impl TrashWriteRoots {
    /// Open a previously initialized Home Trash. Never create its directories
    /// implicitly on this path. The reader has different compatibility rules.
    pub(crate) fn open(data_home: &Path, cancel: &Cancellation) -> io::Result<Self> {
        cancel.check().map_err(|_| io::ErrorKind::Interrupted)?;
        if !data_home.is_absolute()
            || data_home
                .components()
                .any(|part| part == Component::ParentDir)
        {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let held = DirectoryHandle::open_without_symlinks(data_home)?;
        require_user_controlled(&held)?;
        let trash = child(&held, "Trash")?;
        let files = child(&trash, "files")?;
        let info = child(&trash, "info")?;
        cancel.check().map_err(|_| io::ErrorKind::Interrupted)?;
        if files.identity() == info.identity() {
            return Err(io::ErrorKind::Unsupported.into());
        }
        require_same_mount(&held, &trash)?;
        require_same_mount(&trash, &files)?;
        require_same_mount(&trash, &info)?;
        Ok(Self {
            data_home: held,
            displayed_home: data_home.to_path_buf(),
            trash,
            files,
            info,
        })
    }

    /// Only an explicit write operation may call this. It fills missing
    /// immediate children under an already validated XDG data directory.
    /// It does not move files, replace unsuitable paths, or roll back
    /// directories another process may already be using.
    pub(crate) fn initialize_existing_data_home(
        data_home: &Path,
        cancel: &Cancellation,
    ) -> io::Result<Self> {
        cancel.check().map_err(|_| io::ErrorKind::Interrupted)?;
        if !data_home.is_absolute()
            || data_home
                .components()
                .any(|part| part == Component::ParentDir)
        {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let held = DirectoryHandle::open_without_symlinks(data_home)?;
        require_user_controlled(&held)?;
        let trash = initialize_child(&held, "Trash", cancel)?;
        require_same_mount(&held, &trash)?;
        // Inspect both before creating either missing child. A symlink,
        // shared directory, or file is not treated as "missing."
        let existing_files = optional_child(&trash, "files")?;
        let existing_info = optional_child(&trash, "info")?;
        for dir in [&existing_files, &existing_info].into_iter().flatten() {
            require_same_mount(&trash, dir)?;
        }
        let files = match existing_files {
            Some(dir) => dir,
            None => initialize_child(&trash, "files", cancel)?,
        };
        let info = match existing_info {
            Some(dir) => dir,
            None => initialize_child(&trash, "info", cancel)?,
        };
        let roots = Self {
            data_home: held,
            displayed_home: data_home.to_path_buf(),
            trash,
            files,
            info,
        };
        roots.revalidate(cancel)?;
        Ok(roots)
    }

    pub(crate) fn revalidate(&self, cancel: &Cancellation) -> io::Result<()> {
        let current = Self::open(&self.displayed_home, cancel)?;
        if self.data_home.identity() != current.data_home.identity()
            || self.trash.identity() != current.trash.identity()
            || self.files.identity() != current.files.identity()
            || self.info.identity() != current.info.identity()
        {
            return Err(io::Error::other(
                "the displayed Home Trash directories changed",
            ));
        }
        require_same_mount(&self.data_home, &current.data_home)?;
        require_same_mount(&self.trash, &current.trash)?;
        require_same_mount(&self.files, &current.files)?;
        require_same_mount(&self.info, &current.info)
    }
}

fn optional_child(parent: &DirectoryHandle, name: &str) -> io::Result<Option<DirectoryHandle>> {
    match child(parent, name) {
        Ok(dir) => Ok(Some(dir)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn initialize_child(
    parent: &DirectoryHandle,
    name: &str,
    cancel: &Cancellation,
) -> io::Result<DirectoryHandle> {
    if let Some(dir) = optional_child(parent, name)? {
        return Ok(dir);
    }
    cancel.check().map_err(|_| io::ErrorKind::Interrupted)?;
    require_user_controlled(parent)?;
    match fs::mkdirat(&parent.fd, name, Mode::RWXU) {
        Ok(()) | Err(Errno::EXIST) => child(parent, name),
        Err(error) => Err(error.into()),
    }
}

fn child(parent: &DirectoryHandle, name: &str) -> io::Result<DirectoryHandle> {
    let fd = fs::openat(
        &parent.fd,
        name,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )?;
    let held = DirectoryHandle::from_directory_fd(fd)?;
    require_user_controlled(&held)?;
    Ok(held)
}

/// Must hold this descriptor until the separately guarded payload rename
/// finishes. A matching native name alone is never proof of a reservation.
struct SidecarReservation {
    name: OsString,
    fd: File,
    expected: Vec<u8>,
}

impl SidecarReservation {
    fn acquire(
        roots: &TrashWriteRoots,
        key: &OsStr,
        encoded: Vec<u8>,
        cancel: &Cancellation,
    ) -> io::Result<Self> {
        if !crate::mkdir::single_name(key)
            || key.len() > 255 - ".trashinfo".len()
            || encoded.len() > crate::TRASH_INFO_LIMIT
        {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        roots.revalidate(cancel)?;
        match fs::statat(&roots.files.fd, key, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(_) => return Err(io::ErrorKind::AlreadyExists.into()),
            Err(Errno::NOENT) => {}
            Err(error) => return Err(error.into()),
        }
        let mut name = key.to_owned();
        name.push(".trashinfo");
        cancel.check().map_err(|_| io::ErrorKind::Interrupted)?;
        let fd = fs::openat(
            &roots.info.fd,
            &name,
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?;
        let mut reservation = Self {
            name,
            fd: File::from(fd),
            expected: encoded,
        };
        reservation.fd.write_all(&reservation.expected)?;
        reservation.verify(roots)?;
        Ok(reservation)
    }

    fn verify(&self, roots: &TrashWriteRoots) -> io::Result<()> {
        let visible = fs::statat(&roots.info.fd, &self.name, AtFlags::SYMLINK_NOFOLLOW)?;
        let held = fs::fstat(&self.fd)?;
        if (visible.st_dev, visible.st_ino) != (held.st_dev, held.st_ino)
            || held.st_uid != geteuid().as_raw()
            || Mode::from_raw_mode(held.st_mode).intersects(Mode::WGRP | Mode::WOTH)
            || u64::try_from(held.st_size).ok() != Some(self.expected.len() as u64)
        {
            return Err(io::Error::other(
                "Home Trash sidecar identity or policy changed",
            ));
        }
        let mut bytes = vec![0; self.expected.len()];
        self.fd.read_exact_at(&mut bytes, 0)?;
        if bytes != self.expected {
            return Err(io::Error::other("Home Trash sidecar contents changed"));
        }
        Ok(())
    }
}

fn require_user_controlled(dir: &DirectoryHandle) -> io::Result<()> {
    let meta = fs::fstat(&dir.fd)?;
    if meta.st_uid != geteuid().as_raw()
        || Mode::from_raw_mode(meta.st_mode).intersects(Mode::WGRP | Mode::WOTH)
    {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(())
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
        time::{SystemTime, UNIX_EPOCH},
    };

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "jwm-trash-write-roots-{}-{nonce}",
                std::process::id()
            ));
            files::DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }

        fn initialize(&self) {
            files::create_dir_all(self.0.join("Trash/files")).unwrap();
            files::create_dir(self.0.join("Trash/info")).unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = files::remove_dir_all(&self.0);
        }
    }

    fn selected<'a>(
        parent: &'a DirectoryHandle,
        path: &'a Path,
        name: &'a OsStr,
    ) -> RegularMoveSource<'a> {
        let st = files::symlink_metadata(path.join(name)).unwrap();
        RegularMoveSource {
            parent,
            displayed_parent: path,
            name,
            identity: FileIdentity {
                device: st.dev(),
                inode: st.ino(),
            },
        }
    }

    const WHEN: &str = "2026-11-27T14:40:00";

    #[test]
    fn home_trash_write_initializes_fresh_home_and_keeps_distinct_raw_revisions() {
        let temp = Fixture::new();
        let origin = temp.0.join("source");
        let data = temp.0.join("data");
        files::create_dir(&origin).unwrap();
        files::create_dir(&data).unwrap();
        let cancel = Cancellation::default();
        assert!(
            crate::list_home_trash(&data, &cancel)
                .unwrap()
                .entries
                .is_empty()
        );
        assert!(!data.join("Trash").exists());
        let held = DirectoryHandle::open_without_symlinks(&origin).unwrap();
        let name = OsStr::from_bytes(b"file%\n\x80");
        let original = origin.join(name);
        let mut keys = Vec::new();
        for bytes in [b"first".as_slice(), b"second"] {
            files::write(&original, bytes).unwrap();
            let source = selected(&held, &origin, name);
            let original_identity = source.identity;
            let HomeTrashWriteOutcome::Moved { storage_key } =
                trash_regular_at_home(source, &data, WHEN, &cancel).unwrap()
            else {
                panic!("verified same-mount move required")
            };
            assert!(!original.exists());
            let published = data.join("Trash/files").join(&storage_key);
            let meta = files::metadata(&published).unwrap();
            assert_eq!(
                (meta.dev(), meta.ino()),
                (original_identity.device, original_identity.inode)
            );
            assert_eq!(files::read(&published).unwrap(), bytes);
            keys.push(storage_key);
        }
        assert_ne!(keys[0], keys[1]);
        let snapshot = crate::list_home_trash(&data, &cancel).unwrap();
        assert_eq!(snapshot.entries.len(), 2);
        for key in keys {
            let row = snapshot.entries.iter().find(|row| row.name == key).unwrap();
            let info = row.info.as_ref().unwrap();
            assert_eq!(info.original, original);
            assert_eq!(info.deletion_date, WHEN);
        }
    }

    #[test]
    fn home_trash_write_rejects_replaced_source_before_initializing_home() {
        let temp = Fixture::new();
        let origin = temp.0.join("source");
        files::create_dir(&origin).unwrap();
        let held = DirectoryHandle::open_without_symlinks(&origin).unwrap();
        let name = OsStr::new("item");
        files::write(origin.join(name), b"original").unwrap();
        let old = selected(&held, &origin, name);
        files::rename(origin.join(name), origin.join("original")).unwrap();
        files::write(origin.join(name), b"replacement").unwrap();
        assert!(matches!(
            trash_regular_at_home(old, &temp.0, WHEN, &Cancellation::default()),
            Err(HomeTrashWriteError::Refused(_))
        ));
        assert!(!temp.0.join("Trash").exists());
        assert_eq!(files::read(origin.join(name)).unwrap(), b"replacement");
        assert_eq!(files::read(origin.join("original")).unwrap(), b"original");
    }

    #[test]
    fn home_trash_write_cancel_before_rename_preserves_source_and_leaves_no_reservation() {
        let temp = Fixture::new();
        let origin = temp.0.join("source");
        files::create_dir(&origin).unwrap();
        let held = DirectoryHandle::open_without_symlinks(&origin).unwrap();
        let name = OsStr::new("item");
        files::write(origin.join(name), b"saved").unwrap();
        let cancel = Cancellation::default();
        let result = trash_with(
            selected(&held, &origin, name),
            &temp.0,
            WHEN,
            &cancel,
            || {
                cancel.cancel();
            },
        );
        assert!(
            matches!(result, Err(HomeTrashWriteError::Refused(e)) if e.kind() == io::ErrorKind::Interrupted)
        );
        assert_eq!(files::read(origin.join(name)).unwrap(), b"saved");
        assert_eq!(
            files::read_dir(temp.0.join("Trash/info")).unwrap().count(),
            0
        );
        assert_eq!(
            files::read_dir(temp.0.join("Trash/files")).unwrap().count(),
            0
        );
    }

    #[test]
    fn home_trash_write_late_payload_conflict_keeps_source_and_unrelated_file() {
        let temp = Fixture::new();
        let origin = temp.0.join("source");
        files::create_dir(&origin).unwrap();
        let held = DirectoryHandle::open_without_symlinks(&origin).unwrap();
        let name = OsStr::new("item");
        files::write(origin.join(name), b"saved").unwrap();
        let cancel = Cancellation::default();
        let result = trash_with(
            selected(&held, &origin, name),
            &temp.0,
            WHEN,
            &cancel,
            || {
                let entry = files::read_dir(temp.0.join("Trash/info"))
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap();
                let metadata = entry.file_name();
                let key = metadata.as_bytes().strip_suffix(b".trashinfo").unwrap();
                files::write(
                    temp.0.join("Trash/files").join(OsStr::from_bytes(key)),
                    b"external sentinel",
                )
                .unwrap();
            },
        );
        assert!(
            matches!(result, Err(HomeTrashWriteError::Refused(e)) if e.kind() == io::ErrorKind::AlreadyExists)
        );
        assert_eq!(files::read(origin.join(name)).unwrap(), b"saved");
        assert_eq!(
            files::read_dir(temp.0.join("Trash/info")).unwrap().count(),
            0
        );
        let entry = files::read_dir(temp.0.join("Trash/files"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        assert_eq!(files::read(entry.path()).unwrap(), b"external sentinel");
    }

    #[test]
    fn sidecar_reservation_does_not_pair_existing_payload_and_keeps_orphan_intent() {
        let temp = Fixture::new();
        temp.initialize();
        let cancel = Cancellation::default();
        let roots = TrashWriteRoots::open(&temp.0, &cancel).unwrap();
        let payload = temp.0.join("Trash/files/collision");
        symlink("elsewhere", &payload).unwrap();
        let info = temp.0.join("Trash/info");
        let encoded = b"[Trash Info]\nPath=/private%80\nDeletionDate=2026-11-27T12:00:00\n";
        assert_eq!(
            SidecarReservation::acquire(&roots, OsStr::new("collision"), encoded.to_vec(), &cancel)
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::AlreadyExists,
        );
        assert!(!info.join("collision.trashinfo").exists());
        assert_eq!(files::read_link(payload).unwrap(), Path::new("elsewhere"));
        let first =
            SidecarReservation::acquire(&roots, OsStr::new("ordinary"), encoded.to_vec(), &cancel)
                .unwrap();
        assert_eq!(
            files::read(info.join("ordinary.trashinfo")).unwrap(),
            encoded
        );
        assert!(!temp.0.join("Trash/files/ordinary").exists());
        assert_eq!(
            SidecarReservation::acquire(&roots, OsStr::new("ordinary"), encoded.to_vec(), &cancel)
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::AlreadyExists,
        );
        first.verify(&roots).unwrap();
        files::rename(info.join("ordinary.trashinfo"), info.join("original")).unwrap();
        files::write(info.join("ordinary.trashinfo"), encoded).unwrap();
        assert!(first.verify(&roots).is_err());
        assert_eq!(
            files::read(info.join("ordinary.trashinfo")).unwrap(),
            encoded
        );
    }

    #[test]
    fn trash_write_roots_do_not_initialize_missing_directories() {
        let temp = Fixture::new();
        let cancel = Cancellation::default();
        assert_eq!(
            TrashWriteRoots::open(&temp.0, &cancel)
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::NotFound,
        );
        assert!(!temp.0.join("Trash").exists());
        cancel.cancel();
        assert_eq!(
            TrashWriteRoots::open(&temp.0, &cancel)
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::Interrupted,
        );
        assert!(!temp.0.join("Trash").exists());
    }

    #[test]
    fn trash_write_roots_refuse_aliases_and_group_writable_directories() {
        let temp = Fixture::new();
        temp.initialize();
        symlink("Trash", temp.0.join("alias")).unwrap();
        let cancel = Cancellation::default();
        assert!(TrashWriteRoots::open(&temp.0.join("alias"), &cancel).is_err());
        let dir = temp.0.join("Trash/info");
        files::set_permissions(&dir, files::Permissions::from_mode(0o775)).unwrap();
        assert_eq!(
            TrashWriteRoots::open(&temp.0, &cancel)
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::PermissionDenied,
        );
        assert_eq!(files::read_dir(dir).unwrap().count(), 0);
    }

    #[test]
    fn explicit_initialization_accepts_a_fresh_or_partial_private_home() {
        for partial in [false, true] {
            let temp = Fixture::new();
            let cancel = Cancellation::default();
            let root = temp.0.join("Trash");
            if partial {
                files::create_dir_all(root.join("info")).unwrap();
                files::write(root.join("info/sentinel"), b"do not change").unwrap();
            }
            assert!(TrashWriteRoots::open(&temp.0, &cancel).is_err());
            assert_eq!(root.exists(), partial);
            assert!(!root.join("files").exists());
            let first = TrashWriteRoots::initialize_existing_data_home(&temp.0, &cancel).unwrap();
            let again = TrashWriteRoots::initialize_existing_data_home(&temp.0, &cancel).unwrap();
            assert_eq!(first.files.identity(), again.files.identity());
            assert_eq!(first.info.identity(), again.info.identity());
            if partial {
                assert_eq!(
                    files::read(root.join("info/sentinel")).unwrap(),
                    b"do not change"
                );
            }
        }
    }

    #[test]
    fn unsuitable_existing_trash_child_prevents_creating_the_missing_sibling() {
        let temp = Fixture::new();
        let root = temp.0.join("Trash");
        files::create_dir(&root).unwrap();
        files::write(root.join("info"), b"unrelated").unwrap();
        files::write(temp.0.join("source"), b"saved").unwrap();
        assert!(
            TrashWriteRoots::initialize_existing_data_home(&temp.0, &Cancellation::default(),)
                .is_err()
        );
        assert!(!root.join("files").exists());
        assert_eq!(files::read(root.join("info")).unwrap(), b"unrelated");
        assert_eq!(files::read(temp.0.join("source")).unwrap(), b"saved");
    }

    #[test]
    fn trash_write_roots_reject_replaced_info_and_preserve_both_sentinels() {
        let temp = Fixture::new();
        temp.initialize();
        let cancel = Cancellation::default();
        let roots = TrashWriteRoots::open(&temp.0, &cancel).unwrap();
        roots.revalidate(&cancel).unwrap();
        let info = temp.0.join("Trash/info");
        files::write(info.join("first"), b"old information").unwrap();
        let elsewhere = temp.0.join("first-info");
        files::rename(&info, &elsewhere).unwrap();
        files::create_dir(&info).unwrap();
        files::write(info.join("second"), b"new information").unwrap();
        assert!(roots.revalidate(&cancel).is_err());
        assert_eq!(
            files::read(elsewhere.join("first")).unwrap(),
            b"old information"
        );
        assert_eq!(
            files::read(info.join("second")).unwrap(),
            b"new information"
        );
    }
}
