//! Owner-only storage for bounded, read-only copy interruption evidence.
use crate::{COPY_JOURNAL_LIMIT, CopyJournalRecord, CopyJournalState, DirectoryHandle};
use rustix::{
    fd::{AsRawFd, OwnedFd},
    fs::{self, AtFlags, FileType, Mode, OFlags, RenameFlags},
    io::Errno,
    process::geteuid,
};
use std::{
    fs::File,
    io::{self, ErrorKind, Read, Write},
};

const JOURNAL: &str = "jwm-files-copy-journal-v1";
const INTENT: &str = "intent";
const PUBLISHED: &str = "published";
pub const COPY_JOURNAL_SLOTS: u8 = 64;

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message)
}

fn private_error() -> io::Error {
    io::Error::new(ErrorKind::PermissionDenied, "copy journal is not private")
}

fn open_dir(parent: &OwnedFd, name: &str, exact: bool) -> io::Result<OwnedFd> {
    let fd = fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let stat = fs::fstat(&fd)?;
    let allowed_mode = if exact {
        stat.st_mode & 0o7777 == 0o700
    } else {
        stat.st_mode & 0o022 == 0
    };
    if stat.st_uid != geteuid().as_raw() || !allowed_mode {
        return Err(private_error());
    }
    Ok(fd)
}

fn open_journal(base: &DirectoryHandle, create: bool) -> io::Result<OwnedFd> {
    let root = open_dir(&base.fd, ".", false)?;
    if create {
        match fs::mkdirat(&root, JOURNAL, Mode::RWXU) {
            Ok(()) | Err(Errno::EXIST) => {}
            Err(error) => return Err(error.into()),
        }
    }
    let journal = open_dir(&root, JOURNAL, true)?;
    if create {
        fs::fsync(&root)?;
    }
    Ok(journal)
}

fn slot_name(slot: u8) -> io::Result<String> {
    if slot >= COPY_JOURNAL_SLOTS {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "unknown copy journal slot",
        ));
    }
    Ok(format!("{slot:02}"))
}

fn read_record(dir: &OwnedFd, name: &str) -> io::Result<CopyJournalRecord> {
    let pin = fs::openat(
        dir,
        name,
        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let stat = fs::fstat(&pin)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
        || stat.st_uid != geteuid().as_raw()
        || stat.st_mode & 0o7777 != 0o600
        || stat.st_nlink != 1
    {
        return Err(private_error());
    }
    let read = fs::open(
        format!("/proc/self/fd/{}", pin.as_raw_fd()),
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let mut bytes = Vec::new();
    File::from(read)
        .take(COPY_JOURNAL_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > COPY_JOURNAL_LIMIT {
        return Err(invalid("copy journal record exceeds its limit"));
    }
    CopyJournalRecord::decode(&bytes).ok_or_else(|| invalid("invalid copy journal record"))
}

fn write_record(dir: &OwnedFd, name: &str, record: &CopyJournalRecord) -> io::Result<()> {
    let bytes = record
        .encode()
        .ok_or_else(|| invalid("invalid copy journal record"))?;
    match fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => return Err(Errno::EXIST.into()),
        Err(Errno::NOENT) => {}
        Err(error) => return Err(error.into()),
    }
    // Each slot has exactly two immutable record names and a distinct exclusive
    // scratch for each. An interrupted scratch is not a valid observation.
    let scratch = format!(".{name}.writing");
    let fd = fs::openat(
        dir,
        scratch.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )?;
    let mut file = File::from(fd);
    fs::fchmod(&file, Mode::RUSR | Mode::WUSR)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::renameat_with(dir, scratch.as_str(), dir, name, RenameFlags::NOREPLACE)?;
    fs::fsync(dir)?;
    Ok(())
}

fn same_location(first: &CopyJournalRecord, second: &CopyJournalRecord) -> bool {
    first.destination_parent.as_os_str() == second.destination_parent.as_os_str()
        && first.parent == second.parent
        && first.name == second.name
}

/// A reservation owns the exact directory; it is not authorization to write
/// the copy destination. No slot is automatically removed or recycled.
pub struct CopyJournalReservation {
    slot: u8,
    dir: OwnedFd,
    intent: CopyJournalRecord,
}

impl CopyJournalReservation {
    pub fn slot(&self) -> u8 {
        self.slot
    }

    /// Store only an observation from the caller's verified, held output.
    /// Failure does not mean the copy destination is absent or safe to retry.
    pub fn record_publication(&self, record: &CopyJournalRecord) -> io::Result<()> {
        let intent = read_record(&self.dir, INTENT)?;
        if !matches!(record.state, CopyJournalState::Published(_))
            || !same_location(&self.intent, record)
            || !same_location(&intent, &self.intent)
            || intent.state != CopyJournalState::Prepared
        {
            return Err(invalid("copy publication does not match its intent"));
        }
        write_record(&self.dir, PUBLISHED, record)
    }
}

/// Explicitly reserve and persist a copy intent before the caller publishes.
/// Fully occupied and interrupted slots both consume one of the fixed slots.
pub fn prepare_copy_journal(
    base: &DirectoryHandle,
    intent: CopyJournalRecord,
) -> io::Result<CopyJournalReservation> {
    if intent.state != CopyJournalState::Prepared || intent.encode().is_none() {
        return Err(invalid("invalid copy journal intent"));
    }
    let journal = open_journal(base, true)?;
    for slot in 0..COPY_JOURNAL_SLOTS {
        let name = slot_name(slot)?;
        match fs::mkdirat(&journal, name.as_str(), Mode::RWXU) {
            Ok(()) => {}
            Err(Errno::EXIST) => continue,
            Err(error) => return Err(error.into()),
        }
        let dir = open_dir(&journal, &name, true)?;
        write_record(&dir, INTENT, &intent)?;
        fs::fsync(&journal)?;
        return Ok(CopyJournalReservation { slot, dir, intent });
    }
    Err(io::Error::new(
        ErrorKind::StorageFull,
        "copy journal slots are full",
    ))
}

/// Reopen one slot without creating state or touching the copy destination.
/// A malformed or mismatched publication is an error, never proof of success.
pub fn read_copy_journal(base: &DirectoryHandle, slot: u8) -> io::Result<CopyJournalRecord> {
    let name = slot_name(slot)?;
    let journal = open_journal(base, false)?;
    let dir = open_dir(&journal, &name, true)?;
    read_slot(&dir)
}

fn read_slot(dir: &OwnedFd) -> io::Result<CopyJournalRecord> {
    let intent = read_record(dir, INTENT)?;
    if intent.state != CopyJournalState::Prepared {
        return Err(invalid("copy journal slot does not contain an intent"));
    }
    match read_record(dir, PUBLISHED) {
        Ok(record)
            if matches!(record.state, CopyJournalState::Published(_))
                && same_location(&intent, &record) =>
        {
            Ok(record)
        }
        Ok(_) => Err(invalid("copy publication does not match its intent")),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(intent),
        Err(error) => Err(error),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CopyJournalSlotState {
    /// A compliant slot directory had no committed intent when inspected.
    Incomplete,
    /// An occupied name or frame could not be safely verified.
    Unreadable,
    /// A bounded, matching intent and optional publication record.
    Readable(CopyJournalRecord),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CopyJournalSlot {
    pub slot: u8,
    pub state: CopyJournalSlotState,
}

/// A point-in-time report, not a reservation or a removal token. Absent slot
/// numbers were not occupied when inspected. Non-canonical names are ignored.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CopyJournalSnapshot {
    occupied: Vec<CopyJournalSlot>,
}

impl CopyJournalSnapshot {
    pub fn occupied(&self) -> &[CopyJournalSlot] {
        &self.occupied
    }

    pub fn available(&self) -> usize {
        usize::from(COPY_JOURNAL_SLOTS) - self.occupied.len()
    }
}

/// Inspect at most 64 exact slot names without creating state, changing a
/// record, opening a copy destination, or probing arbitrary siblings. Call
/// this on a worker. It has no effect on active copies or on retention.
pub fn inspect_copy_journal(base: &DirectoryHandle) -> io::Result<CopyJournalSnapshot> {
    let mut view = CopyJournalSnapshot {
        occupied: Vec::new(),
    };
    let root = open_dir(&base.fd, ".", false)?;
    let journal = match open_dir(&root, JOURNAL, true) {
        Ok(journal) => journal,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(view),
        Err(error) => return Err(error),
    };
    for slot in 0..COPY_JOURNAL_SLOTS {
        let name = slot_name(slot)?;
        match fs::statat(&journal, name.as_str(), AtFlags::SYMLINK_NOFOLLOW) {
            Ok(_) => {}
            Err(Errno::NOENT) => continue,
            Err(error) => return Err(error.into()),
        }
        let state = match open_dir(&journal, &name, true) {
            Ok(dir) => match fs::statat(&dir, INTENT, AtFlags::SYMLINK_NOFOLLOW) {
                Err(Errno::NOENT) => CopyJournalSlotState::Incomplete,
                Ok(_) => match read_slot(&dir) {
                    Ok(record) => CopyJournalSlotState::Readable(record),
                    Err(_) => CopyJournalSlotState::Unreadable,
                },
                Err(_) => CopyJournalSlotState::Unreadable,
            },
            // We observed the name. A disappeared, non-directory, aliased,
            // or unsafe entry cannot be counted as free from this result.
            Err(_) => CopyJournalSlotState::Unreadable,
        };
        view.occupied.push(CopyJournalSlot { slot, state });
    }
    Ok(view)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CopyObservation, CopyReview, FileIdentity};
    use std::{
        ffi::OsString,
        fs as stdfs,
        os::unix::{
            ffi::OsStringExt,
            fs::{DirBuilderExt, PermissionsExt, symlink},
        },
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path =
                std::env::temp_dir().join(format!("jwm-copy-state-{}-{now}", std::process::id()));
            stdfs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }

        fn open(&self) -> DirectoryHandle {
            DirectoryHandle::open_without_symlinks(&self.0).unwrap()
        }

        fn intent(&self) -> CopyJournalRecord {
            CopyJournalRecord {
                destination_parent: self.0.clone(),
                parent: self.open().identity(),
                name: OsString::from_vec(b"raw-\x80.txt".to_vec()),
                state: CopyJournalState::Prepared,
            }
        }

        fn slot(&self, slot: u8) -> PathBuf {
            self.0.join(JOURNAL).join(slot_name(slot).unwrap())
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = stdfs::remove_dir_all(&self.0);
        }
    }

    fn publication(intent: &CopyJournalRecord) -> CopyJournalRecord {
        let mut record = intent.clone();
        record.state = CopyJournalState::Published(CopyObservation {
            file: FileIdentity {
                device: 2,
                inode: 3,
            },
            size: 5,
            mtime_seconds: 6,
            mtime_nanos: 7,
            ctime_seconds: 8,
            ctime_nanos: 9,
        });
        record
    }

    fn put_frame(path: &Path, record: &CopyJournalRecord) {
        stdfs::write(path, record.encode().unwrap()).unwrap();
        stdfs::set_permissions(path, stdfs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn journal_inspection_fresh_state_is_read_only() {
        let f = Fixture::new();
        let view = inspect_copy_journal(&f.open()).unwrap();
        assert!(view.occupied().is_empty());
        assert_eq!(view.available(), usize::from(COPY_JOURNAL_SLOTS));
        assert!(!f.0.join(JOURNAL).exists());
    }

    #[test]
    fn journal_inspection_keeps_occupied_without_evidence_distinct() {
        let f = Fixture::new();
        let intent = f.intent();
        let reservation = prepare_copy_journal(&f.open(), intent.clone()).unwrap();
        assert_eq!(reservation.slot(), 0);
        let incomplete = f.slot(1);
        stdfs::DirBuilder::new()
            .mode(0o700)
            .create(&incomplete)
            .unwrap();
        stdfs::write(incomplete.join(".intent.writing"), b"unfinished").unwrap();
        let sentinel = f.0.join(&intent.name);
        stdfs::write(&sentinel, b"unattributed").unwrap();
        symlink(&sentinel, f.slot(2)).unwrap();
        stdfs::write(f.0.join(JOURNAL).join("not-a-slot"), b"untouched").unwrap();

        let view = inspect_copy_journal(&f.open()).unwrap();
        assert_eq!(
            view.occupied(),
            &[
                CopyJournalSlot {
                    slot: 0,
                    state: CopyJournalSlotState::Readable(intent),
                },
                CopyJournalSlot {
                    slot: 1,
                    state: CopyJournalSlotState::Incomplete,
                },
                CopyJournalSlot {
                    slot: 2,
                    state: CopyJournalSlotState::Unreadable,
                },
            ]
        );
        assert_eq!(view.available(), usize::from(COPY_JOURNAL_SLOTS) - 3);
        assert_eq!(stdfs::read(sentinel).unwrap(), b"unattributed");
        assert_eq!(
            stdfs::read(incomplete.join(".intent.writing")).unwrap(),
            b"unfinished"
        );
    }

    #[test]
    fn journal_inspection_at_capacity_can_have_zero_readable_records() {
        let f = Fixture::new();
        let journal = f.0.join(JOURNAL);
        stdfs::DirBuilder::new()
            .mode(0o700)
            .create(&journal)
            .unwrap();
        for slot in 0..COPY_JOURNAL_SLOTS {
            stdfs::DirBuilder::new()
                .mode(0o700)
                .create(f.slot(slot))
                .unwrap();
        }
        let view = inspect_copy_journal(&f.open()).unwrap();
        assert_eq!(view.available(), 0);
        assert_eq!(view.occupied().len(), usize::from(COPY_JOURNAL_SLOTS));
        assert!(
            view.occupied()
                .iter()
                .all(|entry| entry.state == CopyJournalSlotState::Incomplete)
        );
        let error = prepare_copy_journal(&f.open(), f.intent()).err().unwrap();
        assert_eq!(error.kind(), ErrorKind::StorageFull);
        assert_eq!(
            stdfs::read_dir(&journal).unwrap().count(),
            usize::from(COPY_JOURNAL_SLOTS)
        );
    }

    #[test]
    fn journal_inspection_does_not_treat_an_aliased_root_as_empty() {
        let f = Fixture::new();
        let other = Fixture::new();
        symlink(&other.0, f.0.join(JOURNAL)).unwrap();
        assert!(inspect_copy_journal(&f.open()).is_err());
        assert_eq!(stdfs::read_dir(&other.0).unwrap().count(), 0);
    }

    #[test]
    fn persistent_copy_journal_read_never_initializes() {
        let f = Fixture::new();
        assert_eq!(
            read_copy_journal(&f.open(), 0).unwrap_err().kind(),
            ErrorKind::NotFound
        );
        assert_eq!(
            read_copy_journal(&f.open(), COPY_JOURNAL_SLOTS)
                .unwrap_err()
                .kind(),
            ErrorKind::InvalidInput
        );
        assert!(!f.0.join(JOURNAL).exists());
    }

    #[test]
    fn persistent_copy_journal_records_distinct_immutable_intents() {
        let f = Fixture::new();
        let one = f.intent();
        let mut two = one.clone();
        two.name = "other".into();
        let first = prepare_copy_journal(&f.open(), one.clone()).unwrap();
        let second = prepare_copy_journal(&f.open(), two.clone()).unwrap();
        assert_ne!(first.slot(), second.slot());
        assert!(first.record_publication(&publication(&two)).is_err());
        let witness = publication(&one);
        first.record_publication(&witness).unwrap();
        let duplicate = first.record_publication(&witness).unwrap_err();
        assert_eq!(duplicate.kind(), ErrorKind::AlreadyExists);
        assert!(!f.slot(first.slot()).join(".published.writing").exists());
        assert_eq!(read_copy_journal(&f.open(), first.slot()).unwrap(), witness);
        assert_eq!(read_copy_journal(&f.open(), second.slot()).unwrap(), two);
    }

    #[test]
    fn persistent_copy_journal_interrupted_copy_never_attributes_a_present_entry() {
        let f = Fixture::new();
        let intent = f.intent();
        let slot = prepare_copy_journal(&f.open(), intent.clone())
            .unwrap()
            .slot();
        let destination = f.0.join(&intent.name);
        stdfs::write(&destination, b"unattributed").unwrap();
        stdfs::write(f.slot(slot).join(".published.writing"), b"partial").unwrap();
        let opened = read_copy_journal(&f.open(), slot).unwrap();
        assert_eq!(opened.review(), CopyReview::PresentWithoutPublicationRecord);
        assert_eq!(stdfs::read(&destination).unwrap(), b"unattributed");
        stdfs::remove_file(destination).unwrap();
        assert_eq!(opened.review(), CopyReview::DestinationMissing);
    }

    #[test]
    fn persistent_copy_journal_rejects_aliases_hardlinks_and_wrong_frames() {
        let f = Fixture::new();
        let other = Fixture::new();
        symlink(&other.0, f.0.join(JOURNAL)).unwrap();
        assert!(prepare_copy_journal(&f.open(), f.intent()).is_err());
        assert_eq!(stdfs::read_dir(&other.0).unwrap().count(), 0);
        stdfs::remove_file(f.0.join(JOURNAL)).unwrap();

        let intent = f.intent();
        let slot = prepare_copy_journal(&f.open(), intent.clone())
            .unwrap()
            .slot();
        let path = f.slot(slot).join(PUBLISHED);
        let foreign = other.0.join(PUBLISHED);
        put_frame(&foreign, &publication(&intent));
        stdfs::hard_link(&foreign, &path).unwrap();
        assert!(read_copy_journal(&f.open(), slot).is_err());
        stdfs::remove_file(&path).unwrap();

        let mut wrong = publication(&intent);
        wrong.name = "wrong name".into();
        put_frame(&path, &wrong);
        assert!(read_copy_journal(&f.open(), slot).is_err());
        stdfs::set_permissions(f.0.join(JOURNAL), stdfs::Permissions::from_mode(0o750)).unwrap();
        assert!(read_copy_journal(&f.open(), slot).is_err());
        let foreign_frame = stdfs::read(&foreign).unwrap();
        assert_eq!(foreign_frame, publication(&intent).encode().unwrap());
    }

    #[test]
    fn persistent_copy_journal_rejects_oversized_records_without_writes() {
        let f = Fixture::new();
        let intent = f.intent();
        let saved = prepare_copy_journal(&f.open(), intent).unwrap();
        let path = f.slot(saved.slot()).join(INTENT);
        let too_large = vec![b'x'; COPY_JOURNAL_LIMIT + 1];
        stdfs::write(&path, &too_large).unwrap();
        let error = read_copy_journal(&f.open(), saved.slot()).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidData);
        assert_eq!(stdfs::read(&path).unwrap(), too_large);
        assert!(!f.slot(saved.slot()).join(PUBLISHED).exists());
    }

    #[test]
    fn persistent_copy_journal_reserves_at_most_the_fixed_slots() {
        let f = Fixture::new();
        let journal = f.0.join(JOURNAL);
        stdfs::DirBuilder::new()
            .mode(0o700)
            .create(&journal)
            .unwrap();
        for slot in 0..COPY_JOURNAL_SLOTS {
            stdfs::DirBuilder::new()
                .mode(0o700)
                .create(f.slot(slot))
                .unwrap();
        }
        let fail = prepare_copy_journal(&f.open(), f.intent()).err().unwrap();
        assert_eq!(fail.kind(), ErrorKind::StorageFull);
        let count = stdfs::read_dir(journal).unwrap().count();
        assert_eq!(count, usize::from(COPY_JOURNAL_SLOTS));
    }
}
