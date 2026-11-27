//! Private source metadata observations for a future folder copy executor.
use crate::{Cancellation, DirectoryHandle, FileKind, TreeCopyPlan};
use rustix::fs::{self, FileType, Mode, OFlags, ResolveFlags};
use std::{io, path::Path};

const BYTE_LIMIT: u64 = 1 << 30;
const WITHIN_ROOT: ResolveFlags = ResolveFlags::BENEATH
    .union(ResolveFlags::NO_SYMLINKS)
    .union(ResolveFlags::NO_XDEV);

#[derive(Debug)]
pub(super) enum PayloadError {
    Io(io::Error),
    Cancelled,
    Changed,
    ByteBudget,
}

pub(super) struct Payload {
    // The slot index is the original observation index. A directory is None.
    pub(super) observed: Vec<Option<[i128; 5]>>,
    pub(super) bytes: u64,
}

fn observe(
    root: &DirectoryHandle,
    entry: &crate::TreeCopyEntry,
) -> Result<Option<[i128; 5]>, PayloadError> {
    let fd = fs::openat2(
        &root.fd,
        entry.relative.as_path(),
        OFlags::PATH | OFlags::CLOEXEC,
        Mode::empty(),
        WITHIN_ROOT,
    )
    .map_err(|e| PayloadError::Io(e.into()))?;
    let meta = fs::fstat(&fd).map_err(|e| PayloadError::Io(e.into()))?;
    if meta.st_ino != entry.identity.inode || meta.st_dev != entry.identity.device {
        return Err(PayloadError::Changed);
    }
    match (entry.kind, FileType::from_raw_mode(meta.st_mode)) {
        (FileKind::Directory, FileType::Directory) => Ok(None),
        (FileKind::Regular, FileType::RegularFile) => {
            Ok(Some(crate::copy::content_observation(&meta)))
        }
        _ => Err(PayloadError::Changed),
    }
}

fn check(cancel: &Cancellation) -> Result<(), PayloadError> {
    cancel.check().map_err(|_| PayloadError::Cancelled)
}

fn reopen(plan: &TreeCopyPlan) -> Result<DirectoryHandle, PayloadError> {
    let root = DirectoryHandle::open_without_symlinks(&plan.root).map_err(PayloadError::Io)?;
    if root.identity() != plan.identity {
        return Err(PayloadError::Changed);
    }
    Ok(root)
}

pub(super) fn measure(plan: &TreeCopyPlan, cancel: &Cancellation) -> Result<Payload, PayloadError> {
    check(cancel)?;
    let root = reopen(plan)?;
    let mut bytes = 0u64;
    let mut observed = Vec::with_capacity(plan.entries.len());
    for entry in &plan.entries {
        check(cancel)?;
        let current = observe(&root, entry)?;
        if let Some(data) = current {
            bytes = bytes
                .checked_add(u64::try_from(data[0]).map_err(|_| PayloadError::ByteBudget)?)
                .filter(|bytes| *bytes <= BYTE_LIMIT)
                .ok_or(PayloadError::ByteBudget)?;
        }
        observed.push(current);
    }
    check(cancel)?;
    Ok(Payload { observed, bytes })
}

impl Payload {
    pub(super) fn validate_current(
        &self,
        plan: &TreeCopyPlan,
        cancel: &Cancellation,
    ) -> Result<(), PayloadError> {
        check(cancel)?;
        if self.observed.len() != plan.entries.len() {
            return Err(PayloadError::Changed);
        }
        let root = reopen(plan)?;
        for (entry, saved) in plan.entries.iter().zip(&self.observed) {
            check(cancel)?;
            if observe(&root, entry)? != *saved {
                return Err(PayloadError::Changed);
            }
        }
        check(cancel)
    }
}

pub(super) fn pinned_child_dir(
    root: &DirectoryHandle,
    relative: &Path,
) -> Result<DirectoryHandle, PayloadError> {
    let path = if relative.as_os_str().is_empty() {
        Path::new(".")
    } else {
        relative
    };
    let fd = fs::openat2(
        &root.fd,
        path,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
        WITHIN_ROOT,
    )
    .map_err(|e| PayloadError::Io(e.into()))?;
    DirectoryHandle::from_directory_fd(fd).map_err(PayloadError::Io)
}
