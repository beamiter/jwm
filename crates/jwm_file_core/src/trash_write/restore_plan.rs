//! Local staging prototype, intended to sit inside the strict Home Trash provider.
//! Preparing the plan only opens and reads descriptors.
use super::{TrashWriteRoots, identity, require_same_mount, require_user_controlled};
use crate::{
    Cancellation, DirectoryHandle, FileIdentity, FileKind, HomeTrashEntry, HomeTrashInfo,
    copy::content_observation, parse_home_trash_info,
};
use rustix::{
    fd::OwnedFd,
    fs::{self, AtFlags, FileType, OFlags, RenameFlags},
    io::Errno,
};
use std::{
    ffi::{OsStr, OsString},
    io,
    os::unix::ffi::OsStrExt,
    path::{Component, Path, PathBuf},
};
mod sidecar_witness;
use sidecar_witness::SidecarWitness;

/// Exact user-visible proposal. Holds the observed descriptors until explicit
/// confirmation or drop; callers cannot change its destination.
pub struct HomeTrashRestorePlan {
    roots: TrashWriteRoots,
    source: OwnedFd,
    storage_key: OsString,
    source_identity: FileIdentity,
    source_version: [i128; 5],
    sidecar: SidecarWitness,
    path: PathBuf,
    parent: DirectoryHandle,
}

/// Outcomes after the file's rename succeeded. A refusal is returned only
/// when this call did not rename any payload.
#[derive(Debug, Eq, PartialEq)]
pub enum HomeTrashRestoreOutcome {
    Restored,
    /// The original payload was verified, but matching metadata could not
    /// be safely removed. Leave it untouched and do not automatically replay.
    RestoredWithMetadataRemnant,
    /// Rename succeeded, but the observed file could not be verified in
    /// the original directory. Retain metadata and do not undo or replay.
    RenamedUnverified,
}

fn check(cancel: &Cancellation) -> io::Result<()> {
    cancel.check().map_err(|_| io::ErrorKind::Interrupted.into())
}

fn changed() -> io::Error {
    io::Error::other("the selected Trash entry or original directory changed")
}

/// The in-place destructive action accepts a deliberately stricter subset
/// than the freedesktop listing: other file managers do not agree on repeated
/// keys or groups. Require a single absolute, URI-escaped Path and date.
fn unambiguous_original(bytes: &[u8], data_home: &Path) -> Option<HomeTrashInfo> {
    if bytes.contains(&0) || std::str::from_utf8(bytes).is_err() {
        return None;
    }
    let mut lines = bytes
        .split(|byte| *byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line));
    if lines.next()? != b"[Trash Info]" {
        return None;
    }
    let (mut path, mut date) = (false, false);
    for line in lines {
        if line.is_empty() || line.starts_with(b"#") {
            continue;
        }
        if let Some(value) = line.strip_prefix(b"Path=") {
            if path
                || !value.starts_with(b"/")
                || value
                    .iter()
                    .any(|byte| !matches!(*byte, b'!'..=b'~') || *byte == b'\\')
            {
                return None;
            }
            path = true;
        } else if line.starts_with(b"DeletionDate=") && !date {
            date = true;
        } else {
            // Includes repeated groups, duplicate aliases with spaces, and
            // noncanonical key syntax. Do not interpret those as a destination.
            return None;
        }
    }
    if !path || !date {
        return None;
    }
    parse_home_trash_info(bytes, data_home)
}

impl HomeTrashRestorePlan {
    pub fn prepare(
        data_home: &Path,
        entry: &HomeTrashEntry,
        cancel: &Cancellation,
    ) -> io::Result<Self> {
        check(cancel)?;
        if entry.kind != FileKind::Regular || !crate::mkdir::single_name(&entry.name) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let expected_info = entry.info.as_ref().ok_or(io::ErrorKind::InvalidData)?;
        let roots = TrashWriteRoots::open(data_home, cancel)?;
        let source = fs::openat(
            &roots.files.fd,
            &entry.name,
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            fs::Mode::empty(),
        )?;
        let before = fs::fstat(&source)?;
        let source_version = content_observation(&before);
        if FileType::from_raw_mode(before.st_mode) != FileType::RegularFile
            || !entry.matches_payload(&before)
        {
            return Err(changed());
        }
        let sidecar = SidecarWitness::capture(&roots.info.fd, &entry.name)?;
        let info = unambiguous_original(sidecar.bytes(), data_home)
            .ok_or_else(|| io::Error::new(
                io::ErrorKind::InvalidData,
                "Trash metadata does not identify an unambiguous absolute original path",
            ))?;
        if &info != expected_info {
            return Err(changed());
        }
        let path = info.original;
        let raw = path.as_os_str().as_bytes();
        if !path.is_absolute()
            || raw.contains(&0)
            || raw.ends_with(b"/")
            || raw.split(|byte| *byte == b'/').any(|part| part == b".")
            || path.components().any(|part| part == Component::ParentDir)
            || path.starts_with(data_home.join("Trash"))
            || path.file_name().is_none_or(|name| name.len() > 255)
        {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let parent =
            DirectoryHandle::open_without_symlinks(path.parent().ok_or(io::ErrorKind::InvalidInput)?)?;
        let plan = Self {
            roots,
            source,
            storage_key: entry.name.clone(),
            source_identity: entry.file,
            source_version,
            sidecar,
            path,
            parent,
        };
        plan.verify_before_publish(cancel)?;
        Ok(plan)
    }

    pub fn original_path(&self) -> &Path {
        &self.path
    }

    pub fn storage_key(&self) -> &OsStr {
        &self.storage_key
    }

    /// Consume the exact confirmed plan. No overwrite, new directory,
    /// permanent payload unlink, cross-mount fallback, or automatic replay.
    pub fn restore(self, cancel: &Cancellation) -> io::Result<HomeTrashRestoreOutcome> {
        self.restore_with(cancel, || {}, || {})
    }

    fn restore_with(
        self,
        cancel: &Cancellation,
        before_publish: impl FnOnce(),
        after_publish: impl FnOnce(),
    ) -> io::Result<HomeTrashRestoreOutcome> {
        let name = self.path.file_name().ok_or(io::ErrorKind::InvalidInput)?;
        before_publish();
        self.verify_before_publish(cancel)?;
        fs::renameat_with(
            &self.roots.files.fd,
            &self.storage_key,
            &self.parent.fd,
            name,
            RenameFlags::NOREPLACE,
        )?;
        after_publish();

        // Cancellation is no longer interpreted as a refusal. Rename can
        // change ctime, so check the still-held payload identity instead.
        let published = self.verify_parent().is_ok()
            && fs::statat(&self.parent.fd, name, AtFlags::SYMLINK_NOFOLLOW)
                .is_ok_and(|stat| {
                    FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile
                        && identity(&stat) == self.source_identity
                });
        if !published {
            return Ok(HomeTrashRestoreOutcome::RenamedUnverified);
        }

        // This is not a namespace lock. Recheck exactly the metadata
        // observed before confirmation; do not touch the payload again.
        let removed = self.roots.revalidate(&Cancellation::default()).is_ok()
            && matches!(
                fs::statat(
                    &self.roots.files.fd,
                    &self.storage_key,
                    AtFlags::SYMLINK_NOFOLLOW,
                ),
                Err(Errno::NOENT)
            )
            && self.sidecar.verify(&self.roots.info.fd).is_ok()
            && fs::unlinkat(
                &self.roots.info.fd,
                self.sidecar.name(),
                AtFlags::empty(),
            )
            .is_ok();
        Ok(if removed {
            HomeTrashRestoreOutcome::Restored
        } else {
            HomeTrashRestoreOutcome::RestoredWithMetadataRemnant
        })
    }

    fn verify_before_publish(&self, cancel: &Cancellation) -> io::Result<()> {
        self.roots.revalidate(cancel)?;
        self.verify_parent()?;
        self.sidecar.verify(&self.roots.info.fd)?;
        let pinned = fs::fstat(&self.source)?;
        let named = fs::statat(
            &self.roots.files.fd,
            &self.storage_key,
            AtFlags::SYMLINK_NOFOLLOW,
        )?;
        for stat in [&pinned, &named] {
            if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
                || identity(stat) != self.source_identity
                || content_observation(stat) != self.source_version
            {
                return Err(changed());
            }
        }
        match fs::statat(
            &self.parent.fd,
            self.path.file_name().ok_or(io::ErrorKind::InvalidInput)?,
            AtFlags::SYMLINK_NOFOLLOW,
        ) {
            Ok(_) => return Err(io::ErrorKind::AlreadyExists.into()),
            Err(Errno::NOENT) => {}
            Err(error) => return Err(error.into()),
        }
        check(cancel)
    }

    fn verify_parent(&self) -> io::Result<()> {
        let displayed = self.path.parent().ok_or(io::ErrorKind::InvalidInput)?;
        let reopened = DirectoryHandle::open_without_symlinks(displayed)?;
        if reopened.identity() != self.parent.identity()
            || self.parent.identity() == self.roots.files.identity()
            || self.parent.identity() == self.roots.info.identity()
            || self.parent.identity() == self.roots.trash.identity()
        {
            return Err(changed());
        }
        require_user_controlled(&self.parent)?;
        require_user_controlled(&reopened)?;
        require_same_mount(&reopened, &self.parent)?;
        require_same_mount(&self.parent, &self.roots.files)
    }
}

#[cfg(test)]
mod metadata_tests {
    use super::*;

    #[test]
    fn restore_metadata_refuses_duplicate_path_even_if_stable() {
        let root = Path::new("/data");
        let base = b"[Trash Info]\nPath=/disposable/a/report\nDeletionDate=2026-11-27T16:26:36\n";
        for suffix in [
            b"Path=/disposable/b/report\n".as_slice(),
            b"Path = /disposable/b/report\n".as_slice(),
            b"[Trash Info]\nPath=/disposable/b/report\n".as_slice(),
        ] {
            let mut ambiguous = base.to_vec();
            ambiguous.extend_from_slice(suffix);
            let listing = parse_home_trash_info(&ambiguous, root).unwrap();
            assert_eq!(listing.original, Path::new("/disposable/a/report"));
            assert!(unambiguous_original(&ambiguous, root).is_none());
        }
    }

    #[test]
    fn restore_metadata_accepts_exact_percent_escaped_native_bytes() {
        let bytes = b"[Trash Info]\r\nPath=/data/dir/report%25-%80-%0A\r\nDeletionDate=2026-11-27T16:26:36\r\n";
        let info = unambiguous_original(bytes, Path::new("/data")).unwrap();
        assert_eq!(info.original.as_os_str().as_bytes(), b"/data/dir/report%-\x80-\n");
    }
}

#[cfg(test)]
mod mutation_tests {
    use super::*;
    use std::{
        fs as disk,
        os::unix::fs::{DirBuilderExt, MetadataExt, symlink},
        time::{SystemTime, UNIX_EPOCH},
    };

    struct Fixture {
        root: PathBuf,
        data: PathBuf,
        original: PathBuf,
        payload: PathBuf,
        sidecar: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "jwm-restore-original-{}-{nonce}",
                std::process::id()
            ));
            disk::DirBuilder::new().mode(0o700).create(&root).unwrap();
            let data = root.join("data");
            let original = root
                .join("original")
                .join(OsStr::from_bytes(b"raw%-\x80-\n"));
            disk::create_dir(original.parent().unwrap()).unwrap();
            disk::create_dir_all(data.join("Trash/files")).unwrap();
            disk::create_dir(data.join("Trash/info")).unwrap();
            let payload = data.join("Trash/files/key-2");
            let sidecar = data.join("Trash/info/key-2.trashinfo");
            disk::write(&payload, b"retained exact bytes").unwrap();
            let info = crate::trash_info::encode_home_trash_info(
                &original,
                "2026-11-27T16:26:36",
            )
            .unwrap();
            disk::write(&sidecar, info).unwrap();
            Self {
                root,
                data,
                original,
                payload,
                sidecar,
            }
        }

        fn row(&self) -> HomeTrashEntry {
            crate::list_home_trash(&self.data, &Cancellation::default())
                .unwrap()
                .entries
                .pop()
                .unwrap()
        }

        fn prepare(&self) -> HomeTrashRestorePlan {
            HomeTrashRestorePlan::prepare(&self.data, &self.row(), &Cancellation::default())
                .unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = disk::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn restore_original_preserves_inode_and_unrelated_entries() {
        let f = Fixture::new();
        let neighbor = f.payload.with_file_name("keep");
        let neighbor_sidecar = f.sidecar.with_file_name("keep.trashinfo");
        disk::write(&neighbor, b"other revision").unwrap();
        let neighbor_info = disk::read(&f.sidecar).unwrap();
        disk::write(&neighbor_sidecar, &neighbor_info).unwrap();
        let row = crate::list_home_trash(&f.data, &Cancellation::default())
            .unwrap()
            .entries
            .into_iter()
            .find(|row| row.name == "key-2")
            .unwrap();
        let cancel = Cancellation::default();
        let plan = HomeTrashRestorePlan::prepare(&f.data, &row, &cancel).unwrap();
        let inode = disk::metadata(&f.payload).unwrap().ino();
        assert_eq!(plan.original_path(), f.original.as_path());
        assert_eq!(plan.storage_key(), OsStr::new("key-2"));
        assert!(!f.original.exists());
        assert_eq!(
            plan.restore_with(&cancel, || {}, || cancel.cancel()).unwrap(),
            HomeTrashRestoreOutcome::Restored
        );
        assert_eq!(disk::metadata(&f.original).unwrap().ino(), inode);
        assert_eq!(disk::read(&f.original).unwrap(), b"retained exact bytes");
        assert!(!f.payload.exists() && !f.sidecar.exists());
        assert_eq!(disk::read(neighbor).unwrap(), b"other revision");
        assert_eq!(disk::read(neighbor_sidecar).unwrap(), neighbor_info);
    }

    #[test]
    fn restore_original_late_conflict_cancel_and_metadata_change_are_non_destructive() {
        let f = Fixture::new();
        let no_cancel = Cancellation::default();
        let sidecar = disk::read(&f.sidecar).unwrap();
        let collision = f.root.join("collision");
        disk::write(&collision, b"untouched").unwrap();
        assert_eq!(
            f.prepare()
                .restore_with(
                    &no_cancel,
                    || symlink(&collision, &f.original).unwrap(),
                    || {},
                )
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(disk::read_link(&f.original).unwrap(), collision);
        assert_eq!(disk::read(&collision).unwrap(), b"untouched");
        disk::remove_file(&f.original).unwrap();
        let cancel = Cancellation::default();
        assert_eq!(
            f.prepare()
                .restore_with(&cancel, || cancel.cancel(), || {})
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(disk::read(&f.sidecar).unwrap(), sidecar);
        let replaced = f.sidecar.with_extension("held");
        assert!(
            f.prepare()
                .restore_with(
                    &no_cancel,
                    || {
                        disk::rename(&f.sidecar, &replaced).unwrap();
                        disk::write(&f.sidecar, &sidecar).unwrap();
                    },
                    || {},
                )
                .is_err()
        );
        assert_eq!(disk::read(&f.sidecar).unwrap(), sidecar);
        assert_eq!(disk::read(&replaced).unwrap(), sidecar);
        assert_eq!(disk::read(&f.payload).unwrap(), b"retained exact bytes");
        assert!(!f.original.exists());
    }

    #[test]
    fn restore_original_ambiguous_record_refuses_mutation_but_can_recover_copy() {
        let f = Fixture::new();
        let other = f.root.join("alternate");
        disk::create_dir(&other).unwrap();
        let target_two = other.join("second");
        let mut ambiguous = disk::read(&f.sidecar).unwrap();
        ambiguous.extend_from_slice(b"Path = ");
        ambiguous.extend_from_slice(target_two.as_os_str().as_bytes());
        ambiguous.push(b'\n');
        disk::write(&f.sidecar, &ambiguous).unwrap();
        let row = f.row();
        let cancel = Cancellation::default();
        assert!(HomeTrashRestorePlan::prepare(&f.data, &row, &cancel).is_err());
        assert!(!f.original.exists() && !target_two.exists());
        assert_eq!(disk::read(&f.sidecar).unwrap(), ambiguous);
        assert_eq!(disk::read(&f.payload).unwrap(), b"retained exact bytes");
        crate::recover_home_trash_copy(&f.data, &row, &cancel).unwrap();
        assert_eq!(disk::read(&f.original).unwrap(), b"retained exact bytes");
        assert_eq!(disk::read(&f.sidecar).unwrap(), ambiguous);
        assert_eq!(disk::read(&f.payload).unwrap(), b"retained exact bytes");
        assert!(!target_two.exists());
    }

    #[test]
    fn restore_original_missing_parent_refuses_without_creation() {
        let f = Fixture::new();
        let parent = f.original.parent().unwrap();
        disk::remove_dir(parent).unwrap();
        let cancel = Cancellation::default();
        let sidecar_bytes = disk::read(&f.sidecar).unwrap();
        let error = HomeTrashRestorePlan::prepare(&f.data, &f.row(), &cancel)
            .err()
            .expect("restore must not create a missing parent");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(!parent.exists());
        assert_eq!(disk::read(&f.sidecar).unwrap(), sidecar_bytes);
        assert_eq!(disk::read(&f.payload).unwrap(), b"retained exact bytes");
    }

    #[test]
    fn restore_original_verified_payload_retains_reclaimed_trash_record() {
        let f = Fixture::new();
        let cancel = Cancellation::default();
        let sidecar = disk::read(&f.sidecar).unwrap();
        let result = f
            .prepare()
            .restore_with(
                &cancel,
                || {},
                || disk::write(&f.payload, b"external new payload").unwrap(),
            )
            .unwrap();
        assert_eq!(result, HomeTrashRestoreOutcome::RestoredWithMetadataRemnant);
        assert_eq!(disk::read(&f.original).unwrap(), b"retained exact bytes");
        assert_eq!(disk::read(&f.payload).unwrap(), b"external new payload");
        assert_eq!(disk::read(&f.sidecar).unwrap(), sidecar);
    }

    #[test]
    fn restore_original_unverified_destination_preserves_metadata_without_rollback() {
        let f = Fixture::new();
        let elsewhere = f.root.join("another-existing-directory");
        disk::create_dir(&elsewhere).unwrap();
        let moved = elsewhere.join("retained");
        let cancel = Cancellation::default();
        let sidecar = disk::read(&f.sidecar).unwrap();
        let result = f
            .prepare()
            .restore_with(
                &cancel,
                || {},
                || disk::rename(&f.original, &moved).unwrap(),
            )
            .unwrap();
        assert_eq!(result, HomeTrashRestoreOutcome::RenamedUnverified);
        assert!(!f.original.exists() && !f.payload.exists());
        assert_eq!(disk::read(&moved).unwrap(), b"retained exact bytes");
        assert_eq!(disk::read(&f.sidecar).unwrap(), sidecar);
    }
}
