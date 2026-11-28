//! Provider-neutral directory snapshots for the JWM file workspace.
//! Providers may block; front ends must invoke them off the UI thread.

#[cfg(target_os = "linux")]
mod copy;
#[cfg(target_os = "linux")]
mod copy_batch;
#[cfg(target_os = "linux")]
mod copy_journal;
#[cfg(target_os = "linux")]
mod copy_journal_storage;
#[cfg(target_os = "linux")]
mod copy_names;
#[cfg(target_os = "linux")]
mod index_walk;
mod local;
#[cfg(target_os = "linux")]
mod mkdir;
#[cfg(target_os = "linux")]
mod move_file;
mod name_find;
#[cfg(target_os = "linux")]
mod preview;
#[cfg(target_os = "linux")]
mod rename;
mod scan_worker;
#[cfg(target_os = "linux")]
mod session_state_file;
#[cfg(target_os = "linux")]
mod terminal_handoff;
#[cfg(target_os = "linux")]
mod trash_info;
#[cfg(target_os = "linux")]
mod trash_snapshot;
#[cfg(target_os = "linux")]
mod trash_write;
#[cfg(target_os = "linux")]
mod tree_copy;
#[cfg(target_os = "linux")]
mod tree_copy_payload;
#[cfg(target_os = "linux")]
mod tree_copy_plan;
#[cfg(target_os = "linux")]
mod tree_copy_stage;
#[cfg(target_os = "linux")]
mod watch;

#[cfg(target_os = "linux")]
pub use copy::{
    CopyDestination, CopyError, CopyEvidence, CopyReceipt, CopySiblingReceipt, CopySource,
    TrackedCopyError, TrackedCopyReceipt,
};
#[cfg(target_os = "linux")]
pub use copy_batch::{COPY_BATCH_LIMIT, CopyBatchInputError, CopyBatchItem};
#[cfg(target_os = "linux")]
pub use copy_journal::{
    COPY_JOURNAL_LIMIT, CopyJournalRecord, CopyJournalState, CopyObservation, CopyReview,
};
#[cfg(target_os = "linux")]
pub use copy_journal_storage::{
    COPY_JOURNAL_SLOTS, CopyJournalReservation, CopyJournalSlot, CopyJournalSlotState,
    CopyJournalSnapshot, ManagedCopyJournalSelection, ManagedJournalRetirement,
    ManagedJournalSnapshot, ManagedRetiredCleanup, ManagedRetiredSelection, ManagedRetiredState,
    cleanup_managed_retired, inspect_copy_journal, inspect_managed_copy_journal,
    inspect_managed_journal, managed_retired_state, prepare_copy_journal,
    prepare_managed_copy_journal, read_copy_journal, retire_managed_copy_journal,
    select_managed_copy_journal, select_managed_retired,
};
#[cfg(target_os = "linux")]
pub use index_walk::{IndexEntry, WalkOptions, WalkSummary, visit_local_index};
pub use local::LocalProvider;
#[cfg(target_os = "linux")]
pub use mkdir::{DirectoryHandle, MakeDirectoryError};
#[cfg(target_os = "linux")]
pub use move_file::{
    FolderMoveError, FolderMoveOutcome, FolderMoveSource, RegularMoveError, RegularMoveOutcome,
    RegularMoveSource, move_folder_no_replace, move_regular_no_replace,
};
pub use name_find::NameQuery;
#[cfg(target_os = "linux")]
pub use preview::{
    IMAGE_PREVIEW_BYTE_LIMIT, ImagePreview, PreviewError, TEXT_PREVIEW_LIMIT, TextPreview,
};
#[cfg(target_os = "linux")]
pub use rename::{RenameError, RenameReceipt};
#[cfg(target_os = "linux")]
pub use scan_worker::WatchNotice;
pub use scan_worker::{ScanId, ScanReply, ScanWorker};
#[cfg(target_os = "linux")]
pub use session_state_file::{
    FILES_TAB_STATE_CAP, open_files_state_root, read_files_tab_state, write_files_tab_state,
};
#[cfg(target_os = "linux")]
pub use terminal_handoff::{TerminalChildSetup, TerminalHandoff, TerminalLocator};
#[cfg(target_os = "linux")]
pub use trash_info::{HomeTrashInfo, TRASH_INFO_LIMIT, parse_home_trash_info};
#[cfg(target_os = "linux")]
pub use trash_snapshot::{
    HOME_TRASH_INSPECT_LIMIT, HomeTrashCursor, HomeTrashEntry, HomeTrashSnapshot, RecoverCopyError,
    list_home_trash, recover_home_trash_copy,
};
#[cfg(target_os = "linux")]
pub use trash_write::{
    HomeTrashRestoreOutcome, HomeTrashRestorePlan, HomeTrashWriteError, HomeTrashWriteOutcome,
    trash_regular_at_home,
};
#[cfg(target_os = "linux")]
pub use tree_copy::{TreeCopyError, TreeCopyReceipt, copy_tree_no_replace};
#[cfg(target_os = "linux")]
pub use tree_copy_plan::{
    TreeCopyEntry, TreeCopyPlan, TreeCopyPlanError, TreeCopyTargetError, check_tree_copy_target,
    plan_tree_copy,
};
#[cfg(target_os = "linux")]
pub use watch::{DirectoryChange, DirectoryWatch};

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
    /// Optional worker-owned watch installed before enumerating the snapshot.
    #[cfg(target_os = "linux")]
    pub local_watch: Option<DirectoryWatch>,
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

    /// Providers that can watch may arm a watch before reading a snapshot.
    fn list_with_watch(
        &self,
        location: &Location,
        options: ListOptions,
        cancellation: &Cancellation,
    ) -> Result<DirectorySnapshot, ListError> {
        self.list(location, options, cancellation)
    }
}
