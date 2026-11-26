use crate::{DirectoryHandle, FileIdentity};
use rustix::fs::{self, AtFlags, RenameFlags};
use std::ffi::OsStr;
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

impl DirectoryHandle {
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
