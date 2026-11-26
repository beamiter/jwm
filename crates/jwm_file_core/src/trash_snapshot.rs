//! Read-only home Trash snapshot. No discovery of removable-volume trashes.
use crate::{
    Cancellation, FileIdentity, FileKind, HomeTrashInfo, TRASH_INFO_LIMIT, parse_home_trash_info,
};
use rustix::{
    fd::{AsRawFd, OwnedFd},
    fs::{self, AtFlags, Dir, FileType, Mode, OFlags},
    io::Errno,
};
use std::{
    ffi::{OsStr, OsString},
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

pub fn list_home_trash(data_home: &Path, cancel: &Cancellation) -> io::Result<HomeTrashSnapshot> {
    check(cancel)?;
    if !data_home.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected absolute XDG data path",
        ));
    }
    let data_dir = match fs::open(data_home, directory_flags(), Mode::empty()) {
        Ok(fd) => fd,
        Err(Errno::NOENT) => return Ok(HomeTrashSnapshot::default()),
        Err(err) => return Err(err.into()),
    };
    let trash = match private_child(&data_dir, "Trash") {
        Ok(fd) => fd,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Ok(HomeTrashSnapshot::default());
        }
        Err(err) => return Err(err),
    };
    let files = private_child(&trash, "files")?;
    let info_dir = private_child(&trash, "info")?;
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
