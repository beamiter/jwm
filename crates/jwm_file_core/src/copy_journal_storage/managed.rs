//! Opt-in journal namespace whose readers and writers cooperate with retirement.
use super::{
    CopyJournalRecord, CopyJournalReservation, CopyJournalSnapshot, DirectoryHandle,
    inspect_slots, invalid, open_dir, open_journal_at, private_error, reserve_intent, slot_name,
    validate_intent,
};
use rustix::{
    fd::OwnedFd,
    fs::{self, AtFlags, Dir, FileType, FlockOperation, RenameFlags, Stat},
    process::geteuid,
};
use std::io::{self, ErrorKind};

const MANAGED: &str = "jwm-files-copy-journal-v2";
const RETIRED: &str = ".retired";

fn verify_named(parent: &OwnedFd, name: &str, held: &OwnedFd) -> io::Result<()> {
    let expected = fs::fstat(held)?;
    let found = fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
    if !same_dir(&expected, &found) {
        return Err(invalid("managed journal selection changed"));
    }
    Ok(())
}

fn same_dir(first: &Stat, second: &Stat) -> bool {
    first.st_dev == second.st_dev
        && first.st_ino == second.st_ino
        && FileType::from_raw_mode(second.st_mode) == FileType::Directory
        && second.st_uid == geteuid().as_raw()
        && second.st_mode & 0o7777 == 0o700
}

fn locked(
    base: &DirectoryHandle,
    create: bool,
    operation: FlockOperation,
) -> io::Result<OwnedFd> {
    let journal = open_journal_at(base, MANAGED, create)?;
    fs::flock(&journal, operation)?;
    verify_named(&base.fd, MANAGED, &journal)?;
    Ok(journal)
}

/// Start an explicit managed intent. The reservation holds the shared guard
/// until publication or abandonment. It doesn't change the legacy journal.
pub fn prepare_managed_copy_journal(
    base: &DirectoryHandle,
    intent: CopyJournalRecord,
) -> io::Result<CopyJournalReservation> {
    validate_intent(&intent)?;
    let journal = locked(base, true, FlockOperation::NonBlockingLockShared)?;
    reserve_intent(journal, intent, true)
}

/// Bounded inspection. No destination is opened and missing state is not created.
pub fn inspect_managed_copy_journal(base: &DirectoryHandle) -> io::Result<CopyJournalSnapshot> {
    let journal = match locked(base, false, FlockOperation::NonBlockingLockShared) {
        Ok(journal) => journal,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(CopyJournalSnapshot { occupied: Vec::new() });
        }
        Err(error) => return Err(error),
    };
    let snapshot = inspect_slots(&journal)?;
    verify_named(&base.fd, MANAGED, &journal)?;
    Ok(snapshot)
}

#[derive(Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Artifact {
    name: Vec<u8>,
    revision: [i128; 7],
}

fn artifacts(slot: &OwnedFd) -> io::Result<Vec<Artifact>> {
    let mut result = Vec::new();
    for item in Dir::read_from(slot)? {
        let item = item?;
        let name = item.file_name();
        let bytes = name.to_bytes();
        if matches!(bytes, b"." | b"..") {
            continue;
        }
        if result.len() == 4
            || !matches!(
                bytes,
                b"intent" | b"published" | b".intent.writing" | b".published.writing"
            )
        {
            return Err(invalid("unknown artifact in managed journal slot"));
        }
        let meta = fs::statat(slot, name, AtFlags::SYMLINK_NOFOLLOW)?;
        if FileType::from_raw_mode(meta.st_mode) != FileType::RegularFile
            || meta.st_uid != geteuid().as_raw()
            || meta.st_mode & 0o7777 != 0o600
            || meta.st_nlink != 1
        {
            return Err(private_error());
        }
        result.push(Artifact {
            name: bytes.to_vec(),
            revision: [
                i128::from(meta.st_dev),
                i128::from(meta.st_ino),
                i128::from(meta.st_size),
                i128::from(meta.st_mtime),
                i128::from(meta.st_mtime_nsec),
                i128::from(meta.st_ctime),
                i128::from(meta.st_ctime_nsec),
            ],
        });
    }
    result.sort_unstable();
    Ok(result)
}

/// Selection pins the exact original journal and slot. It holds no lock while
/// a confirmation is displayed, and cannot be synthesized from a path string.
pub struct ManagedCopyJournalSelection {
    journal: OwnedFd,
    dir: OwnedFd,
    slot: u8,
    artifacts: Vec<Artifact>,
}

/// Capture a read-only selection of an exact, compliant managed slot.
pub fn select_managed_copy_journal(
    base: &DirectoryHandle,
    slot: u8,
) -> io::Result<ManagedCopyJournalSelection> {
    let name = slot_name(slot)?;
    let journal = locked(base, false, FlockOperation::NonBlockingLockShared)?;
    let dir = open_dir(&journal, &name, true)?;
    let snapshot = artifacts(&dir)?;
    verify_named(&journal, &name, &dir)?;
    // Drop the lock, not the pin; final confirmation must independently lock
    // and revalidate both identities rather than upgrade or reuse this lock.
    fs::flock(&journal, FlockOperation::Unlock)?;
    Ok(ManagedCopyJournalSelection {
        journal,
        dir,
        slot,
        artifacts: snapshot,
    })
}

#[derive(Debug)]
pub enum ManagedJournalRetirement {
    Retired,
    /// Rename reported success, but exact visibility/sync was not verified.
    /// This is not permission to restore, retry, or delete a canonical entry.
    RetiredUnverified(io::Error),
}

/// Retire one confirmed managed slot atomically. No files are deleted here:
/// at most one retired entry can wait for a separate bounded cleanup.
pub fn retire_managed_copy_journal(
    base: &DirectoryHandle,
    selection: ManagedCopyJournalSelection,
) -> io::Result<ManagedJournalRetirement> {
    let name = slot_name(selection.slot)?;
    let journal = locked(base, false, FlockOperation::NonBlockingLockExclusive)?;
    verify_named(&base.fd, MANAGED, &selection.journal)?;
    verify_named(&journal, &name, &selection.dir)?;
    if artifacts(&selection.dir)? != selection.artifacts {
        return Err(invalid("managed journal slot contents changed"));
    }
    verify_named(&base.fd, MANAGED, &journal)?;
    verify_named(&journal, &name, &selection.dir)?;
    fs::renameat_with(&journal, &name, &journal, RETIRED, RenameFlags::NOREPLACE)?;
    let verified = (|| {
        fs::fsync(&journal)?;
        verify_named(&base.fd, MANAGED, &journal)?;
        verify_named(&journal, RETIRED, &selection.dir)
    })();
    Ok(match verified {
        Ok(()) => ManagedJournalRetirement::Retired,
        Err(error) => ManagedJournalRetirement::RetiredUnverified(error),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CopyJournalSlotState, CopyJournalState, CopyObservation, FileIdentity};
    use std::{
        ffi::OsString,
        fs as stdfs,
        os::unix::fs::DirBuilderExt,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let name = format!("jwm-managed-journal-{}-{stamp}-{id}", std::process::id());
            let root = std::env::temp_dir().join(name);
            stdfs::DirBuilder::new().mode(0o700).create(&root).unwrap();
            Self(root)
        }

        fn open(&self) -> DirectoryHandle {
            DirectoryHandle::open_without_symlinks(&self.0).unwrap()
        }

        fn intent(&self, name: &str) -> CopyJournalRecord {
            CopyJournalRecord {
                destination_parent: self.0.clone(),
                parent: self.open().identity(),
                name: OsString::from(name),
                state: CopyJournalState::Prepared,
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = stdfs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn managed_journal_retirement_is_atomic_bounded_and_legacy_untouched() {
        let f = Fixture::new();
        let root = f.open();
        let before = inspect_managed_copy_journal(&root).unwrap();
        assert!(before.occupied().is_empty());
        assert!(!f.0.join(MANAGED).exists());
        let legacy = crate::prepare_copy_journal(&root, f.intent("legacy")).unwrap();
        let old = prepare_managed_copy_journal(&root, f.intent("original")).unwrap();
        assert_eq!(legacy.slot(), old.slot());
        let selected = select_managed_copy_journal(&root, old.slot()).unwrap();
        let busy = retire_managed_copy_journal(&root, selected).unwrap_err();
        assert_eq!(busy.kind(), ErrorKind::WouldBlock);
        drop(old);

        let original = f.0.join("original");
        stdfs::write(&original, b"target unchanged").unwrap();
        let selected = select_managed_copy_journal(&root, 0).unwrap();
        let old_frame = stdfs::read(f.0.join(MANAGED).join("00/intent")).unwrap();
        assert!(matches!(
            retire_managed_copy_journal(&root, selected).unwrap(),
            ManagedJournalRetirement::Retired
        ));
        assert!(inspect_managed_copy_journal(&root).unwrap().occupied().is_empty());
        assert_eq!(stdfs::read(f.0.join(MANAGED).join(".retired/intent")).unwrap(), old_frame);

        let replacement = f.intent("replacement");
        drop(prepare_managed_copy_journal(&root, replacement.clone()).unwrap());
        let selected = select_managed_copy_journal(&root, 0).unwrap();
        let occupied = retire_managed_copy_journal(&root, selected).unwrap_err();
        assert_eq!(occupied.kind(), ErrorKind::AlreadyExists);
        let snapshot = inspect_managed_copy_journal(&root).unwrap();
        assert_eq!(snapshot.occupied()[0].state, CopyJournalSlotState::Readable(replacement));
        assert_eq!(crate::read_copy_journal(&root, legacy.slot()).unwrap(), f.intent("legacy"));
        assert_eq!(stdfs::read(original).unwrap(), b"target unchanged");
    }

    #[test]
    fn managed_journal_refuses_stale_selection_after_publication() {
        let f = Fixture::new();
        let root = f.open();
        let mut record = f.intent("target");
        let writer = prepare_managed_copy_journal(&root, record.clone()).unwrap();
        let before = select_managed_copy_journal(&root, writer.slot()).unwrap();
        record.state = CopyJournalState::Published(CopyObservation {
            file: FileIdentity {
                device: 17,
                inode: 11,
            },
            size: 4,
            mtime_seconds: 1,
            mtime_nanos: 2,
            ctime_seconds: 3,
            ctime_nanos: 4,
        });
        writer.record_publication(&record).unwrap();
        drop(writer);
        assert!(retire_managed_copy_journal(&root, before).is_err());
        assert!(!f.0.join(MANAGED).join(RETIRED).exists());
        let result = inspect_managed_copy_journal(&root).unwrap();
        assert_eq!(result.occupied()[0].state, CopyJournalSlotState::Readable(record));
    }

    #[test]
    fn managed_journal_refuses_foreign_artifacts_and_new_slot_identity() {
        let f = Fixture::new();
        let root = f.open();
        let journal = f.0.join(MANAGED);
        drop(prepare_managed_copy_journal(&root, f.intent("old")).unwrap());
        let selected = select_managed_copy_journal(&root, 0).unwrap();
        let old_name = journal.join("00");
        stdfs::rename(&old_name, journal.join("untouched-old")).unwrap();
        drop(prepare_managed_copy_journal(&root, f.intent("new")).unwrap());
        assert!(retire_managed_copy_journal(&root, selected).is_err());

        let selected = select_managed_copy_journal(&root, 0).unwrap();
        stdfs::write(old_name.join("other"), b"not our journal").unwrap();
        assert!(retire_managed_copy_journal(&root, selected).is_err());
        assert!(select_managed_copy_journal(&root, 0).is_err());
        assert_eq!(stdfs::read(old_name.join("other")).unwrap(), b"not our journal");
        assert!(journal.join("untouched-old/intent").is_file());
        assert!(!journal.join(RETIRED).exists());
    }
}
