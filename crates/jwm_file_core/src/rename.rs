use crate::{DirectoryHandle, FileIdentity};
use rustix::{
    fd::OwnedFd,
    fs::{self, AtFlags, Mode, OFlags, RenameFlags},
};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::path::Path;

#[derive(Debug)]
pub enum RenameError {
    InvalidName,
    ParentChanged,
    SourceChanged,
    Io(io::Error),
}

impl fmt::Display for RenameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName => f.write_str("expected a single file or directory name"),
            Self::ParentChanged => f.write_str("directory changed before rename"),
            Self::SourceChanged => f.write_str("selected item changed before rename"),
            Self::Io(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for RenameError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

/// An in-memory, same-directory rename result. Keep the original inode
/// alive so its device/inode pair cannot be recycled while undo is offered.
pub struct RenameReceipt {
    pin: OwnedFd,
    directory: FileIdentity,
    previous: OsString,
    current: OsString,
    file: FileIdentity,
}

impl RenameReceipt {
    pub fn directory_identity(&self) -> FileIdentity {
        self.directory
    }

    /// Reverse a rename if the bound directory and item still match and the
    /// original name is free. Does not overwrite, remove a replacement, or
    /// reconstruct paths from display text. Concurrent namespace changes
    /// can still race the final check as with an ordinary rename.
    pub fn undo(
        &self,
        directory: &DirectoryHandle,
        displayed_path: &Path,
    ) -> Result<(), RenameError> {
        if directory.identity() != self.directory {
            return Err(RenameError::ParentChanged);
        }
        let stat = fs::fstat(&self.pin).map_err(|e| RenameError::Io(e.into()))?;
        if stat.st_ino != self.file.inode || stat.st_dev != self.file.device {
            return Err(RenameError::SourceChanged);
        }
        directory.rename_child_no_replace(displayed_path, &self.current, self.file, &self.previous)
    }
}

impl DirectoryHandle {
    /// Rename as usual, returning an undo token only if the result still
    /// refers to the same pinned item observed before the rename.
    pub fn rename_child_recorded(
        &self,
        displayed_path: &Path,
        old: &OsStr,
        expected: FileIdentity,
        new: &OsStr,
    ) -> Result<Option<RenameReceipt>, RenameError> {
        if !crate::mkdir::single_name(old) || !crate::mkdir::single_name(new) || old == new {
            return Err(RenameError::InvalidName);
        }
        let pin = fs::openat(
            &self.fd,
            old,
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| {
            if e == rustix::io::Errno::NOENT {
                RenameError::SourceChanged
            } else {
                RenameError::Io(e.into())
            }
        })?;
        let before = fs::fstat(&pin).map_err(|e| RenameError::Io(e.into()))?;
        if before.st_ino != expected.inode || before.st_dev != expected.device {
            return Err(RenameError::SourceChanged);
        }
        self.rename_child_no_replace(displayed_path, old, expected, new)?;
        let unchanged = fs::statat(&self.fd, new, AtFlags::SYMLINK_NOFOLLOW)
            .is_ok_and(|after| after.st_ino == expected.inode && after.st_dev == expected.device);
        // An unprovable receipt is not a rollback; the initial operation above
        // did succeed, so never report a rename failure simply for missing undo.
        Ok(unchanged.then(|| RenameReceipt {
            pin,
            directory: self.identity(),
            previous: old.to_owned(),
            current: new.to_owned(),
            file: expected,
        }))
    }

    /// Rename a selected child in place, with atomic no-replace on Linux.
    /// A mismatch observed before the operation is rejected; other processes
    /// may still change the namespace between the identity check and rename.
    pub fn rename_child_no_replace(
        &self,
        displayed_path: &Path,
        old: &OsStr,
        expected_child: FileIdentity,
        new: &OsStr,
    ) -> Result<(), RenameError> {
        if !crate::mkdir::single_name(old) || !crate::mkdir::single_name(new) || old == new {
            return Err(RenameError::InvalidName);
        }
        let current = Self::open(displayed_path).map_err(RenameError::Io)?;
        if current.identity() != self.identity() {
            return Err(RenameError::ParentChanged);
        }
        let stat = fs::statat(&self.fd, old, AtFlags::SYMLINK_NOFOLLOW).map_err(|e| {
            if e == rustix::io::Errno::NOENT {
                RenameError::SourceChanged
            } else {
                RenameError::Io(e.into())
            }
        })?;
        if expected_child.device != stat.st_dev || expected_child.inode != stat.st_ino {
            return Err(RenameError::SourceChanged);
        }
        fs::renameat_with(&self.fd, old, &self.fd, new, RenameFlags::NOREPLACE)
            .map_err(|error| RenameError::Io(error.into()))
    }
}
