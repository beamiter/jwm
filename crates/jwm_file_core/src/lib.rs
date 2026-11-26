//! Provider-neutral directory snapshots for the JWM file workspace.
//! Providers may block; front ends must invoke them off the UI thread.

mod local;
#[cfg(target_os = "linux")]
mod mkdir;
#[cfg(target_os = "linux")]
mod rename;
mod scan_worker;

pub use local::LocalProvider;
#[cfg(target_os = "linux")]
pub use mkdir::{DirectoryHandle, MakeDirectoryError};
#[cfg(target_os = "linux")]
pub use rename::RenameError;
pub use scan_worker::{ScanId, ScanReply, ScanWorker};

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::SystemTime;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Location {
    Local(PathBuf),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileKind {
    Directory,
    Regular,
    Symlink,
    Other,
}

/// A local observation, not a permanent file ID or authorization token.
/// Inodes can be recycled, and hard links can share the same observation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FileIdentity {
    pub device: u64,
    pub inode: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileEntry {
    pub location: Location,
    pub name: OsString,
    pub kind: FileKind,
    pub identity: Option<FileIdentity>,
    pub bytes: Option<u64>,
    pub modified: Option<SystemTime>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ListOptions {
    pub show_hidden: bool,
}

/// Entries whose metadata disappeared or became inaccessible during a scan are
/// counted separately. A partial result must not be represented as complete.
#[derive(Debug)]
pub struct DirectorySnapshot {
    pub location: Location,
    pub entries: Vec<FileEntry>,
    pub skipped_entries: usize,
    /// Linux local provider only: the exact directory actually enumerated.
    #[cfg(target_os = "linux")]
    pub local_parent: Option<Arc<DirectoryHandle>>,
}

#[derive(Debug)]
pub enum ListError {
    Cancelled,
    ReadDirectory { path: PathBuf, source: io::Error },
}

impl fmt::Display for ListError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("directory scan cancelled"),
            Self::ReadDirectory { path, source } => write!(f, "{path:?}: {source}"),
        }
    }
}

impl std::error::Error for ListError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Cancelled => None,
            Self::ReadDirectory { source, .. } => Some(source),
        }
    }
}

/// Cancellation is cooperative between entries. It cannot interrupt an OS
/// call already blocked on the filesystem.
#[derive(Clone, Default)]
pub struct Cancellation(Arc<AtomicBool>);

impl Cancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn check(&self) -> Result<(), ListError> {
        if self.0.load(Ordering::Relaxed) {
            Err(ListError::Cancelled)
        } else {
            Ok(())
        }
    }
}

pub trait FileProvider: Send + Sync {
    fn list(
        &self,
        location: &Location,
        options: ListOptions,
        cancellation: &Cancellation,
    ) -> Result<DirectorySnapshot, ListError>;
}
