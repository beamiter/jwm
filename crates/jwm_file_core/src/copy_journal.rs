//! A bounded frame for one copy intent or observed publication.
//! Decoding never touches disk, and no state authorizes an automatic undo.
use crate::{DirectoryHandle, FileIdentity};
use rustix::{
    fs::{self, AtFlags, FileType, Stat},
    io::Errno,
};
use std::{
    ffi::{OsStr, OsString},
    os::unix::ffi::OsStrExt,
    path::{Component, Path, PathBuf},
};

const MAGIC: &[u8; 8] = b"JWMCPY01";
const PATH_LIMIT: usize = 4096;
const NAME_LIMIT: usize = 255;
const HEADER: usize = MAGIC.len() + 1 + 2 + 2 + 16;
const OBSERVATION: usize = 16 + 8 + 8 + 4 + 8 + 4;
pub const COPY_JOURNAL_LIMIT: usize = HEADER + PATH_LIMIT + NAME_LIMIT + OBSERVATION;

/// A metadata snapshot useful for review, not proof of identity or ownership.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CopyObservation {
    pub file: FileIdentity,
    pub size: u64,
    pub mtime_seconds: i64,
    pub mtime_nanos: u32,
    pub ctime_seconds: i64,
    pub ctime_nanos: u32,
}

impl CopyObservation {
    fn from_stat(stat: &Stat) -> Option<Self> {
        let result = Self {
            file: FileIdentity {
                device: stat.st_dev,
                inode: stat.st_ino,
            },
            size: u64::try_from(i128::from(stat.st_size)).ok()?,
            mtime_seconds: i64::try_from(i128::from(stat.st_mtime)).ok()?,
            mtime_nanos: u32::try_from(i128::from(stat.st_mtime_nsec)).ok()?,
            ctime_seconds: i64::try_from(i128::from(stat.st_ctime)).ok()?,
            ctime_nanos: u32::try_from(i128::from(stat.st_ctime_nsec)).ok()?,
        };
        result.valid().then_some(result)
    }

    fn valid(self) -> bool {
        self.mtime_nanos < 1_000_000_000 && self.ctime_nanos < 1_000_000_000
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CopyJournalState {
    /// A present destination after a crash at this stage may be unrelated.
    Prepared,
    /// A metadata observation made after publication, not a rollback token.
    Published(CopyObservation),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CopyJournalRecord {
    pub destination_parent: PathBuf,
    pub parent: FileIdentity,
    pub name: OsString,
    pub state: CopyJournalState,
}

/// Read-only outcomes. Even matching metadata cannot authorize a mutation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CopyReview {
    InvalidRecord,
    ParentUnavailable,
    ParentChanged,
    DestinationMissing,
    DestinationUnavailable,
    DestinationNotRegular,
    PresentWithoutPublicationRecord,
    DestinationChanged,
    MetadataMatches,
}

fn valid_location(parent: &Path, name: &OsStr) -> bool {
    let bytes = parent.as_os_str().as_bytes();
    let name_bytes = name.as_bytes();
    parent.is_absolute()
        && !parent.components().any(|part| part == Component::ParentDir)
        && bytes.len() <= PATH_LIMIT
        && !bytes.contains(&0)
        && !name_bytes.is_empty()
        && name_bytes.len() <= NAME_LIMIT
        && !matches!(name_bytes, b"." | b"..")
        && !name_bytes.contains(&0)
        && !name_bytes.contains(&b'/')
}

fn push_identity(out: &mut Vec<u8>, id: FileIdentity) {
    out.extend(id.device.to_le_bytes());
    out.extend(id.inode.to_le_bytes());
}

fn take<const N: usize>(input: &mut &[u8]) -> Option<[u8; N]> {
    let (head, rest) = input.split_at_checked(N)?;
    *input = rest;
    head.try_into().ok()
}

fn take_identity(input: &mut &[u8]) -> Option<FileIdentity> {
    Some(FileIdentity {
        device: u64::from_le_bytes(take(input)?),
        inode: u64::from_le_bytes(take(input)?),
    })
}

impl CopyJournalRecord {
    /// Classify the current destination without opening it for data or changing
    /// it. This is a point-in-time diagnostic; call it off the UI thread.
    pub fn review(&self) -> CopyReview {
        if !valid_location(&self.destination_parent, &self.name)
            || matches!(self.state, CopyJournalState::Published(w) if !w.valid())
        {
            return CopyReview::InvalidRecord;
        }
        let Ok(dir) = DirectoryHandle::open_without_symlinks(&self.destination_parent) else {
            return CopyReview::ParentUnavailable;
        };
        if dir.identity() != self.parent {
            return CopyReview::ParentChanged;
        }
        let stat = match fs::statat(&dir.fd, &self.name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(Errno::NOENT) => return CopyReview::DestinationMissing,
            Err(_) => return CopyReview::DestinationUnavailable,
        };
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            return CopyReview::DestinationNotRegular;
        }
        let CopyJournalState::Published(saved) = self.state else {
            return CopyReview::PresentWithoutPublicationRecord;
        };
        match CopyObservation::from_stat(&stat) {
            Some(now) if now == saved => CopyReview::MetadataMatches,
            Some(_) => CopyReview::DestinationChanged,
            None => CopyReview::DestinationUnavailable,
        }
    }

    /// Store one explicit no-overwrite copy intent, with exact Unix path bytes.
    pub fn encode(&self) -> Option<Vec<u8>> {
        if !valid_location(&self.destination_parent, &self.name) {
            return None;
        }
        let path = self.destination_parent.as_os_str().as_bytes();
        let name = self.name.as_bytes();
        let mut out = Vec::with_capacity(
            HEADER + path.len() + name.len()
                + if matches!(self.state, CopyJournalState::Published(_)) {
                    OBSERVATION
                } else {
                    0
                },
        );
        out.extend(MAGIC);
        out.push(u8::from(matches!(self.state, CopyJournalState::Published(_))));
        out.extend(u16::try_from(path.len()).ok()?.to_le_bytes());
        out.extend(u16::try_from(name.len()).ok()?.to_le_bytes());
        push_identity(&mut out, self.parent);
        out.extend(path);
        out.extend(name);
        if let CopyJournalState::Published(w) = self.state {
            if !w.valid() {
                return None;
            }
            push_identity(&mut out, w.file);
            out.extend(w.size.to_le_bytes());
            out.extend(w.mtime_seconds.to_le_bytes());
            out.extend(w.mtime_nanos.to_le_bytes());
            out.extend(w.ctime_seconds.to_le_bytes());
            out.extend(w.ctime_nanos.to_le_bytes());
        }
        Some(out)
    }

    /// Reject oversized, truncated, malformed and unknown-version records.
    pub fn decode(mut input: &[u8]) -> Option<Self> {
        if input.len() > COPY_JOURNAL_LIMIT || take::<8>(&mut input)? != *MAGIC {
            return None;
        }
        let [tag] = take(&mut input)?;
        let path_len = usize::from(u16::from_le_bytes(take(&mut input)?));
        let name_len = usize::from(u16::from_le_bytes(take(&mut input)?));
        if path_len == 0 || path_len > PATH_LIMIT || name_len == 0 || name_len > NAME_LIMIT {
            return None;
        }
        let parent = take_identity(&mut input)?;
        let (path, rest) = input.split_at_checked(path_len)?;
        let (name, mut rest) = rest.split_at_checked(name_len)?;
        let destination_parent = Path::new(OsStr::from_bytes(path));
        let name = OsStr::from_bytes(name);
        if !valid_location(destination_parent, name) {
            return None;
        }
        let state = match tag {
            0 => CopyJournalState::Prepared,
            1 => {
                let witness = CopyObservation {
                    file: take_identity(&mut rest)?,
                    size: u64::from_le_bytes(take(&mut rest)?),
                    mtime_seconds: i64::from_le_bytes(take(&mut rest)?),
                    mtime_nanos: u32::from_le_bytes(take(&mut rest)?),
                    ctime_seconds: i64::from_le_bytes(take(&mut rest)?),
                    ctime_nanos: u32::from_le_bytes(take(&mut rest)?),
                };
                if !witness.valid() {
                    return None;
                }
                CopyJournalState::Published(witness)
            }
            _ => return None,
        };
        if !rest.is_empty() {
            return None;
        }
        Some(Self {
            destination_parent: destination_parent.to_owned(),
            parent,
            name: name.to_owned(),
            state,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs as stdfs,
        os::unix::{
            ffi::OsStringExt,
            fs::{DirBuilderExt, symlink},
        },
        time::{SystemTime, UNIX_EPOCH},
    };

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("jwm-copy-review-{}-{stamp}", std::process::id()));
            stdfs::DirBuilder::new()
                .mode(0o700)
                .create(&path)
                .unwrap();
            Self(path)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = stdfs::remove_dir_all(&self.0);
        }
    }

    fn example(state: CopyJournalState) -> CopyJournalRecord {
        CopyJournalRecord {
            destination_parent: PathBuf::from(OsString::from_vec(b"/mnt/raw%\x80".to_vec())),
            parent: FileIdentity { device: 3, inode: 5 },
            name: OsString::from_vec(b"report%\x81.txt".to_vec()),
            state,
        }
    }

    fn observed() -> CopyObservation {
        CopyObservation {
            file: FileIdentity { device: 3, inode: 9 },
            size: 4096,
            mtime_seconds: -3,
            mtime_nanos: 100,
            ctime_seconds: 123,
            ctime_nanos: 999_999_999,
        }
    }

    #[test]
    fn copy_journal_review_never_changes_destination() {
        let root = Fixture::new();
        let requested = root.0.join("directory");
        stdfs::create_dir(&requested).unwrap();
        let parent = DirectoryHandle::open_without_symlinks(&requested).unwrap();
        let mut record = example(CopyJournalState::Prepared);
        record.destination_parent = requested.clone();
        record.parent = parent.identity();
        let path = requested.join(&record.name);
        assert_eq!(record.review(), CopyReview::DestinationMissing);
        stdfs::write(&path, b"first").unwrap();
        assert_eq!(record.review(), CopyReview::PresentWithoutPublicationRecord);
        let stat = fs::statat(&parent.fd, &record.name, AtFlags::SYMLINK_NOFOLLOW).unwrap();
        record.state = CopyJournalState::Published(CopyObservation::from_stat(&stat).unwrap());
        let restored = CopyJournalRecord::decode(&record.encode().unwrap()).unwrap();
        assert_eq!(restored.review(), CopyReview::MetadataMatches);
        stdfs::write(&path, b"different length").unwrap();
        assert_eq!(restored.review(), CopyReview::DestinationChanged);
        let saved = requested.join("keep");
        stdfs::rename(&path, &saved).unwrap();
        symlink(&saved, &path).unwrap();
        assert_eq!(restored.review(), CopyReview::DestinationNotRegular);
        assert_eq!(stdfs::read_link(&path).unwrap(), saved);
        stdfs::remove_file(&path).unwrap();
        fs::mknodat(
            &parent.fd,
            &record.name,
            FileType::Fifo,
            fs::Mode::RUSR | fs::Mode::WUSR,
            0,
        )
        .unwrap();
        assert_eq!(restored.review(), CopyReview::DestinationNotRegular);
        let moved = root.0.join("moved");
        stdfs::rename(&requested, &moved).unwrap();
        symlink(&moved, &requested).unwrap();
        assert_eq!(restored.review(), CopyReview::ParentUnavailable);
        stdfs::remove_file(&requested).unwrap();
        stdfs::create_dir(&requested).unwrap();
        assert_eq!(restored.review(), CopyReview::ParentChanged);
        assert_eq!(stdfs::read(moved.join("keep")).unwrap(), b"different length");
    }

    #[test]
    fn copy_journal_round_trips_raw_names_and_both_states() {
        for state in [
            CopyJournalState::Prepared,
            CopyJournalState::Published(observed()),
        ] {
            let record = example(state);
            let value = record.encode().unwrap();
            let restored = CopyJournalRecord::decode(&value).unwrap();
            assert_eq!(restored, record);
            assert_eq!(
                restored.destination_parent.as_os_str().as_bytes(),
                b"/mnt/raw%\x80"
            );
            assert_eq!(restored.name.as_bytes(), b"report%\x81.txt");
        }
    }

    #[test]
    fn copy_journal_rejects_truncation_oversize_and_malformed_frames() {
        let record = example(CopyJournalState::Published(observed()));
        let mut value = record.encode().unwrap();
        for len in 0..value.len() {
            assert!(CopyJournalRecord::decode(&value[..len]).is_none(), "{len}");
        }
        assert!(CopyJournalRecord::decode(&vec![0; COPY_JOURNAL_LIMIT + 1]).is_none());
        value.push(0);
        assert!(CopyJournalRecord::decode(&value).is_none());
        value.pop();
        value[8] = 9;
        assert!(CopyJournalRecord::decode(&value).is_none());
        value[8] = 1;
        value[7] = b'2';
        assert!(CopyJournalRecord::decode(&value).is_none());
        value[7] = b'1';
        let end = value.len();
        value[end - 4..].copy_from_slice(&1_000_000_000u32.to_le_bytes());
        assert!(CopyJournalRecord::decode(&value).is_none());
    }

    #[test]
    fn copy_journal_accepts_only_a_bounded_absolute_parent_and_single_leaf() {
        let mut record = example(CopyJournalState::Prepared);
        for parent in ["relative", "/mnt/../tmp", "/mnt/\0"] {
            record.destination_parent = parent.into();
            assert!(record.encode().is_none());
        }
        record.destination_parent = "/mnt".into();
        for name in ["", ".", "..", "sub/file", "bad\0name"] {
            record.name = name.into();
            assert!(record.encode().is_none());
        }
        record.name = "n".repeat(NAME_LIMIT + 1).into();
        assert!(record.encode().is_none());
        record.name = "n".repeat(NAME_LIMIT).into();
        record.destination_parent = format!("/{}", "p".repeat(PATH_LIMIT - 1)).into();
        record.state = CopyJournalState::Published(observed());
        assert_eq!(record.encode().unwrap().len(), COPY_JOURNAL_LIMIT);
        record.destination_parent.push("extra");
        assert!(record.encode().is_none());
    }
}
