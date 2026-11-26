//! Home Trash inspection and recovering a copy without modifying Trash.
use crate::copy::content_observation;
use crate::{
    Cancellation, CopyDestination, CopyError, CopyReceipt, CopySource, DirectoryHandle,
    FileIdentity, FileKind, HomeTrashInfo, TRASH_INFO_LIMIT, parse_home_trash_info,
};
use rustix::{
    fd::{AsRawFd, OwnedFd},
    fs::{self, AtFlags, Dir, FileType, Mode, OFlags},
    io::Errno,
};
use std::{
    ffi::{OsStr, OsString},
    fmt,
    fs::File,
    io::{self, Read},
    os::unix::ffi::OsStrExt,
    path::Path,
};

pub const HOME_TRASH_INSPECT_LIMIT: usize = 4096;

#[derive(Debug, Eq, PartialEq)]
pub struct HomeTrashEntry {
    /// Exact storage basename. Never derive this from the original path.
    pub name: OsString,
    pub file: FileIdentity,
    pub kind: FileKind,
    /// Missing or invalid metadata is shown, never guessed from the basename.
    pub info: Option<HomeTrashInfo>,
    // An inode by itself can be recycled between the listing and the action.
    version: [i128; 5],
}

#[derive(Debug, Default, Eq, PartialEq)]
pub struct HomeTrashSnapshot {
    pub entries: Vec<HomeTrashEntry>,
    pub unreadable_entries: usize,
    /// The scan deliberately did not inspect the remaining directory entries.
    pub incomplete: bool,
}

fn directory_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC
}

fn private_child(parent: &OwnedFd, name: &str) -> io::Result<OwnedFd> {
    let fd = fs::openat(
        parent,
        name,
        directory_flags() | OFlags::NOFOLLOW,
        Mode::empty(),
    )?;
    let stat = fs::fstat(&fd)?;
    if stat.st_uid != rustix::process::geteuid().as_raw()
        || Mode::from_raw_mode(stat.st_mode).intersects(Mode::WGRP | Mode::WOTH)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Trash subdirectory is not user-controlled",
        ));
    }
    Ok(fd)
}

fn read_info(info_dir: &OwnedFd, name: &OsStr, data_home: &Path) -> Option<HomeTrashInfo> {
    let mut basename = name.to_owned();
    basename.push(".trashinfo");
    let pin = fs::openat(
        info_dir,
        &basename,
        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()?;
    let before = fs::fstat(&pin).ok()?;
    if FileType::from_raw_mode(before.st_mode) != FileType::RegularFile
        || before.st_uid != rustix::process::geteuid().as_raw()
        || Mode::from_raw_mode(before.st_mode).intersects(Mode::WGRP | Mode::WOTH)
        || u64::try_from(before.st_size).ok()? > TRASH_INFO_LIMIT as u64
    {
        return None;
    }
    let file = fs::open(
        format!("/proc/self/fd/{}", pin.as_raw_fd()),
        OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()?;
    let mut bytes = Vec::new();
    File::from(file)
        .take(TRASH_INFO_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    let after = fs::fstat(&pin).ok()?;
    if (
        before.st_size,
        before.st_ctime,
        before.st_ctime_nsec,
        before.st_mtime,
        before.st_mtime_nsec,
    ) != (
        after.st_size,
        after.st_ctime,
        after.st_ctime_nsec,
        after.st_mtime,
        after.st_mtime_nsec,
    ) {
        return None;
    }
    parse_home_trash_info(&bytes, data_home)
}

fn check(cancel: &Cancellation) -> io::Result<()> {
    cancel
        .check()
        .map_err(|error| io::Error::new(io::ErrorKind::Interrupted, error))
}

fn open_home_trash(data_home: &Path) -> io::Result<Option<(OwnedFd, OwnedFd)>> {
    if !data_home.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected absolute XDG data path",
        ));
    }
    let data_dir = match fs::open(data_home, directory_flags(), Mode::empty()) {
        Ok(fd) => fd,
        Err(Errno::NOENT) => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let trash = match private_child(&data_dir, "Trash") {
        Ok(fd) => fd,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(err) => return Err(err),
    };
    let files = private_child(&trash, "files")?;
    let info_dir = private_child(&trash, "info")?;
    Ok(Some((files, info_dir)))
}

pub fn list_home_trash(data_home: &Path, cancel: &Cancellation) -> io::Result<HomeTrashSnapshot> {
    check(cancel)?;
    let Some((files, info_dir)) = open_home_trash(data_home)? else {
        return Ok(HomeTrashSnapshot::default());
    };
    let iterate = Dir::read_from(&files)?;
    let mut snapshot = HomeTrashSnapshot::default();
    let mut inspected = 0;
    for item in iterate {
        check(cancel)?;
        let item = item?;
        let raw = item.file_name().to_bytes();
        if raw == b"." || raw == b".." {
            continue;
        }
        if inspected == HOME_TRASH_INSPECT_LIMIT {
            snapshot.incomplete = true;
            break;
        }
        inspected += 1;
        let name = OsStr::from_bytes(raw);
        let observed = match fs::statat(&files, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(observed) => observed,
            Err(_) => {
                snapshot.unreadable_entries += 1;
                continue;
            }
        };
        let kind = match FileType::from_raw_mode(observed.st_mode) {
            FileType::Directory => FileKind::Directory,
            FileType::RegularFile => FileKind::Regular,
            FileType::Symlink => FileKind::Symlink,
            _ => FileKind::Other,
        };
        let info = read_info(&info_dir, name, data_home);
        snapshot.entries.push(HomeTrashEntry {
            name: name.to_owned(),
            file: FileIdentity {
                device: observed.st_dev,
                inode: observed.st_ino,
            },
            kind,
            info,
            version: content_observation(&observed),
        });
    }
    let date = |row: &HomeTrashEntry| {
        row.info
            .as_ref()
            .map_or("", |info| info.deletion_date.as_str())
            .to_owned()
    };
    snapshot
        .entries
        .sort_by_cached_key(|row| (std::cmp::Reverse(date(row)), row.name.clone()));
    check(cancel)?;
    Ok(snapshot)
}

#[derive(Debug)]
pub enum RecoverCopyError {
    NotRegular,
    NoOriginal,
    EntryChanged,
    InvalidTarget,
    Io(io::Error),
    Copy(CopyError),
}

impl fmt::Display for RecoverCopyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRegular => f.write_str("only a regular Trash file can be recovered as a copy"),
            Self::NoOriginal => f.write_str("the original path is not known"),
            Self::EntryChanged => {
                f.write_str("the selected Trash entry changed; refresh and select it again")
            }
            Self::InvalidTarget => f.write_str("cannot recover a copy inside the Trash directory"),
            Self::Io(err) => err.fmt(f),
            Self::Copy(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for RecoverCopyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Copy(err) => Some(err),
            _ => None,
        }
    }
}

/// Recover a specific listed home-Trash entry by creating a new file at the
/// recorded original path. The Trash bytes and metadata are untouched. Do not
/// infer the source from its timestamp, follow destination symlinks, create
/// missing directories, or replace an existing file.
pub fn recover_home_trash_copy(
    data_home: &Path,
    entry: &HomeTrashEntry,
    cancel: &Cancellation,
) -> Result<CopyReceipt, RecoverCopyError> {
    check(cancel).map_err(RecoverCopyError::Io)?;
    if entry.kind != FileKind::Regular {
        return Err(RecoverCopyError::NotRegular);
    }
    if !crate::mkdir::single_name(&entry.name) {
        return Err(RecoverCopyError::EntryChanged);
    }
    let expected = entry.info.as_ref().ok_or(RecoverCopyError::NoOriginal)?;
    let Some((files, info_dir)) = open_home_trash(data_home).map_err(RecoverCopyError::Io)? else {
        return Err(RecoverCopyError::EntryChanged);
    };
    let pin = fs::openat(
        &files,
        &entry.name,
        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| match error {
        Errno::NOENT => RecoverCopyError::EntryChanged,
        other => RecoverCopyError::Io(other.into()),
    })?;
    let observed = fs::fstat(&pin).map_err(|e| RecoverCopyError::Io(e.into()))?;
    if FileType::from_raw_mode(observed.st_mode) != FileType::RegularFile
        || observed.st_dev != entry.file.device
        || observed.st_ino != entry.file.inode
        || content_observation(&observed) != entry.version
        || read_info(&info_dir, &entry.name, data_home).as_ref() != Some(expected)
    {
        return Err(RecoverCopyError::EntryChanged);
    }
    let trash = data_home.join("Trash");
    if expected.original.starts_with(&trash) {
        return Err(RecoverCopyError::InvalidTarget);
    }
    let (Some(parent_path), Some(name)) =
        (expected.original.parent(), expected.original.file_name())
    else {
        return Err(RecoverCopyError::NoOriginal);
    };
    let parent =
        DirectoryHandle::open_without_symlinks(parent_path).map_err(RecoverCopyError::Io)?;
    let from = DirectoryHandle::from_directory_fd(files).map_err(RecoverCopyError::Io)?;
    if parent.identity() == from.identity() {
        return Err(RecoverCopyError::InvalidTarget);
    }
    let result = parent.copy_with_observation(
        CopySource {
            parent: &from,
            displayed_parent: &trash.join("files"),
            name: &entry.name,
            expected_file: entry.file,
        },
        CopyDestination {
            displayed_parent: parent_path,
            name,
        },
        cancel,
        Some(entry.version),
    );
    drop(pin); // Keep the observed inode alive until the copy finishes.
    result.map_err(RecoverCopyError::Copy)
}
