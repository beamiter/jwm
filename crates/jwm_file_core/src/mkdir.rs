use crate::FileIdentity;
use rustix::fd::OwnedFd;
use rustix::fs::{self, Mode, OFlags};
use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::path::{Component, Path};

/// A handle to the selected parent directory, not a path for later resolution.
/// Relative mutations do not get redirected if the original path is replaced.
#[derive(Debug)]
pub struct DirectoryHandle {
    pub(crate) fd: OwnedFd,
    identity: FileIdentity,
}

#[derive(Debug)]
pub enum MakeDirectoryError {
    InvalidName,
    ParentChanged,
    Io(io::Error),
}

impl fmt::Display for MakeDirectoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName => f.write_str("expected a single directory name"),
            Self::ParentChanged => f.write_str("directory changed before creation"),
            Self::Io(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for MakeDirectoryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidName | Self::ParentChanged => None,
            Self::Io(error) => Some(error),
        }
    }
}

impl DirectoryHandle {
    /// Open the directory on a worker. If path is a symlink, this pins its
    /// current target; later changes to the link do not redirect the handle.
    pub fn open(path: &Path) -> io::Result<Self> {
        Self::open_with_flags(path, OFlags::empty())
    }

    pub(crate) fn open_tree_node(path: &Path) -> io::Result<Self> {
        Self::open_with_flags(path, OFlags::NOFOLLOW)
    }

    fn open_with_flags(path: &Path, extra: OFlags) -> io::Result<Self> {
        let fd = fs::open(
            path,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC | extra,
            Mode::empty(),
        )?;
        let metadata = fs::fstat(&fd)?;
        Ok(Self {
            fd,
            identity: FileIdentity {
                device: metadata.st_dev,
                inode: metadata.st_ino,
            },
        })
    }

    pub fn identity(&self) -> FileIdentity {
        self.identity
    }

    /// Require the displayed path to still resolve to this directory before
    /// writing. The actual mkdir stays anchored even if that path subsequently
    /// moves again. Run on a worker; opening the path can block.
    pub fn create_directory_at(
        &self,
        displayed_path: &Path,
        name: &OsStr,
    ) -> Result<(), MakeDirectoryError> {
        let current = Self::open(displayed_path).map_err(MakeDirectoryError::Io)?;
        if current.identity() != self.identity() {
            return Err(MakeDirectoryError::ParentChanged);
        }
        self.create_directory(name)
    }

    /// Create only a single immediate child. Never replace an existing item
    /// or recursively create missing parents. The process umask still applies.
    /// If the parent is renamed, the operation remains bound to this handle.
    pub fn create_directory(&self, name: &OsStr) -> Result<(), MakeDirectoryError> {
        if !single_name(name) {
            return Err(MakeDirectoryError::InvalidName);
        }
        fs::mkdirat(&self.fd, name, Mode::RWXU | Mode::RWXG | Mode::RWXO)
            .map_err(|e| MakeDirectoryError::Io(e.into()))
    }
}

pub(crate) fn single_name(name: &OsStr) -> bool {
    matches!(Path::new(name).components().next(), Some(Component::Normal(part)) if part == name)
        && !name.as_encoded_bytes().contains(&0)
}
