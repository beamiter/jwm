//! Bounded copy into private staging followed by one no-replace publication.
use crate::{
    Cancellation, CopyDestination, CopyError, CopySource, DirectoryHandle, FileIdentity, FileKind,
    TreeCopyPlan, TreeCopyTargetError, check_tree_copy_target,
    tree_copy_payload::{self as payload, Payload, PayloadError, pinned_child_dir},
    tree_copy_stage::{STAGED_ROOT, Stage},
};
use rustix::{
    fs::{self, AtFlags, FileType, Mode},
    io::Errno,
};
use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    io,
    path::{Path, PathBuf},
};

#[derive(Debug)]
pub enum TreeCopyError {
    InvalidName,
    Target(TreeCopyTargetError),
    SourceChanged,
    StagingChanged,
    ByteBudget,
    DestinationExists,
    Cancelled,
    File(CopyError),
    Io(io::Error),
    CleanupIncomplete {
        cause: Box<TreeCopyError>,
        staging_name: OsString,
    },
}

#[derive(Debug, Eq, PartialEq)]
pub struct TreeCopyReceipt {
    pub files: usize,
    pub directories: usize,
    pub bytes: u64,
    /// Set when publication succeeded but the empty wrapper could not be removed.
    pub staging_remains: Option<OsString>,
}

struct Created {
    relative: PathBuf,
    parent: FileIdentity,
    child: FileIdentity,
    kind: FileType,
}

fn from_payload(error: PayloadError) -> TreeCopyError {
    match error {
        PayloadError::Changed => TreeCopyError::SourceChanged,
        PayloadError::ByteBudget => TreeCopyError::ByteBudget,
        PayloadError::Cancelled => TreeCopyError::Cancelled,
        PayloadError::Io(error) => TreeCopyError::Io(error),
    }
}

fn from_target(error: TreeCopyTargetError) -> TreeCopyError {
    if matches!(
        error,
        TreeCopyTargetError::Plan(crate::TreeCopyPlanError::Walk(crate::ListError::Cancelled))
    ) {
        TreeCopyError::Cancelled
    } else {
        TreeCopyError::Target(error)
    }
}

fn check(cancel: &Cancellation) -> Result<(), TreeCopyError> {
    cancel.check().map_err(|_| TreeCopyError::Cancelled)
}

fn parts(path: &Path) -> Result<(&Path, &OsStr), TreeCopyError> {
    let name = path.file_name().ok_or(TreeCopyError::InvalidName)?;
    let parent = path.parent().ok_or(TreeCopyError::InvalidName)?;
    if !crate::mkdir::single_name(name) {
        return Err(TreeCopyError::InvalidName);
    }
    Ok((parent, name))
}

fn child(
    parent: &DirectoryHandle,
    name: &OsStr,
) -> Result<(FileIdentity, FileType), Errno> {
    let stat = fs::statat(&parent.fd, name, AtFlags::SYMLINK_NOFOLLOW)?;
    Ok((
        FileIdentity {
            device: stat.st_dev,
            inode: stat.st_ino,
        },
        FileType::from_raw_mode(stat.st_mode),
    ))
}

fn remove_created(tree: &DirectoryHandle, created: &[Created]) -> bool {
    // Never enumerate arbitrary children, follow paths, or delete a
    // substituted item. Failed cleanup leaves private staging for review.
    let mut all = true;
    for item in created.iter().rev() {
        let Ok((relative_parent, name)) = parts(&item.relative) else {
            all = false;
            continue;
        };
        let Ok(parent) = pinned_child_dir(tree, relative_parent) else {
            all = false;
            continue;
        };
        if parent.identity() != item.parent
            || child(&parent, name) != Ok((item.child, item.kind))
        {
            all = false;
            continue;
        }
        let flags = if item.kind == FileType::Directory {
            AtFlags::REMOVEDIR
        } else {
            AtFlags::empty()
        };
        if fs::unlinkat(&parent.fd, name, flags).is_err() {
            all = false;
        }
    }
    all
}

fn fill_tree(
    plan: &TreeCopyPlan,
    measurement: &Payload,
    tree: &DirectoryHandle,
    staging_path: &Path,
    cancel: &Cancellation,
    created: &mut Vec<Created>,
) -> Result<(usize, usize, u64), TreeCopyError> {
    let source = DirectoryHandle::open_without_symlinks(&plan.root).map_err(TreeCopyError::Io)?;
    if source.identity() != plan.identity {
        return Err(TreeCopyError::SourceChanged);
    }
    let mut source_dirs = HashMap::from([(PathBuf::new(), plan.identity)]);
    let mut stage_dirs = HashMap::from([(PathBuf::new(), tree.identity())]);
    let (mut files, mut dirs, mut bytes) = (0usize, 1usize, 0u64);
    for (entry, witness) in plan.entries.iter().zip(&measurement.observed) {
        check(cancel)?;
        let (relative_parent, name) = parts(&entry.relative)?;
        let origin = pinned_child_dir(&source, relative_parent).map_err(from_payload)?;
        let target = pinned_child_dir(tree, relative_parent).map_err(from_payload)?;
        if source_dirs.get(relative_parent) != Some(&origin.identity())
            || stage_dirs.get(relative_parent) != Some(&target.identity())
        {
            return Err(TreeCopyError::SourceChanged);
        }
        let kind = match (entry.kind, witness) {
            (FileKind::Directory, None) => {
                if child(&origin, name) != Ok((entry.identity, FileType::Directory)) {
                    return Err(TreeCopyError::SourceChanged);
                }
                fs::mkdirat(&target.fd, name, Mode::RWXU).map_err(|e| TreeCopyError::Io(e.into()))?;
                dirs += 1;
                FileType::Directory
            }
            (FileKind::Regular, Some(observed)) => {
                let result = target
                    .copy_with_observation(
                        CopySource {
                            parent: &origin,
                            displayed_parent: &plan.root.join(relative_parent),
                            name,
                            expected_file: entry.identity,
                        },
                        CopyDestination {
                            displayed_parent: &staging_path.join(relative_parent),
                            name,
                        },
                        cancel,
                        Some(*observed),
                    )
                    .map_err(|error| match error {
                        CopyError::Cancelled => TreeCopyError::Cancelled,
                        other => TreeCopyError::File(other),
                    })?;
                files += 1;
                bytes += result.bytes;
                FileType::RegularFile
            }
            _ => return Err(TreeCopyError::SourceChanged),
        };
        let (id, actual) = child(&target, name).map_err(|e| TreeCopyError::Io(e.into()))?;
        if actual != kind {
            return Err(TreeCopyError::SourceChanged);
        }
        if kind == FileType::Directory {
            source_dirs.insert(entry.relative.clone(), entry.identity);
            stage_dirs.insert(entry.relative.clone(), id);
        }
        created.push(Created {
            relative: entry.relative.clone(),
            parent: target.identity(),
            child: id,
            kind,
        });
    }
    if bytes != measurement.bytes || created.len() != plan.entries.len() {
        return Err(TreeCopyError::SourceChanged);
    }
    Ok((files, dirs, bytes))
}

fn same_destination(target: &DirectoryHandle, path: &Path) -> Result<(), TreeCopyError> {
    let current = DirectoryHandle::open_without_symlinks(path)
        .map_err(|error| TreeCopyError::Target(TreeCopyTargetError::TargetUnavailable(error)))?;
    if current.identity() != target.identity() {
        return Err(TreeCopyError::Target(TreeCopyTargetError::TargetChanged));
    }
    Ok(())
}

fn validate_staging(
    tree: &DirectoryHandle,
    path: &Path,
    created: &[Created],
    cancel: &Cancellation,
) -> Result<(), TreeCopyError> {
    let observed = crate::plan_tree_copy(path, tree.identity(), cancel).map_err(|error| {
        if matches!(error, crate::TreeCopyPlanError::Walk(crate::ListError::Cancelled)) {
            TreeCopyError::Cancelled
        } else {
            TreeCopyError::StagingChanged
        }
    })?;
    if observed.entries.len() != created.len()
        || observed.entries.iter().zip(created).any(|(now, saved)| {
            now.relative != saved.relative
                || now.identity != saved.child
                || !matches!(
                    (now.kind, saved.kind),
                    (FileKind::Regular, FileType::RegularFile)
                        | (FileKind::Directory, FileType::Directory)
                )
        })
    {
        return Err(TreeCopyError::StagingChanged);
    }
    Ok(())
}

/// Copy to the observed destination's one explicitly chosen child name.
/// Successful publication is one Linux rename without replacement. No
/// source is removed, attributes are not preserved, and interruption or
/// power loss may leave private staging. Call off the UI thread.
pub fn copy_tree_no_replace(
    plan: &TreeCopyPlan,
    target: &DirectoryHandle,
    displayed_target: &Path,
    name: &OsStr,
    cancel: &Cancellation,
) -> Result<TreeCopyReceipt, TreeCopyError> {
    if !crate::mkdir::single_name(name) || name.len() > 255 {
        return Err(TreeCopyError::InvalidName);
    }
    check(cancel)?;
    check_tree_copy_target(plan, target, displayed_target, cancel).map_err(from_target)?;
    match child(target, name) {
        Ok(_) => return Err(TreeCopyError::DestinationExists),
        Err(Errno::NOENT) => {}
        Err(e) => return Err(TreeCopyError::Io(e.into())),
    }
    let measurement = payload::measure(plan, cancel).map_err(from_payload)?;
    same_destination(target, displayed_target)?;
    check(cancel)?;
    let stage = Stage::create(target).map_err(|error| {
        let cause = TreeCopyError::Io(error.error);
        if let Some(staging_name) = error.residue {
            TreeCopyError::CleanupIncomplete {
                cause: Box::new(cause),
                staging_name,
            }
        } else {
            cause
        }
    })?;
    let staging_name = stage.name().to_owned();
    let tree = match stage.make_tree() {
        Ok(tree) => tree,
        Err(e) => {
            let cause = TreeCopyError::Io(e);
            return Err(if stage.remove_empty_wrapper().is_err() {
                TreeCopyError::CleanupIncomplete {
                    cause: Box::new(cause),
                    staging_name,
                }
            } else {
                cause
            });
        }
    };
    let staging_path = displayed_target.join(&staging_name).join(STAGED_ROOT);
    let mut created = Vec::with_capacity(plan.entries.len());
    let operation = (|| {
        let counts = fill_tree(plan, &measurement, &tree, &staging_path, cancel, &mut created)?;
        validate_staging(&tree, &staging_path, &created, cancel)?;
        check_tree_copy_target(plan, target, displayed_target, cancel).map_err(from_target)?;
        measurement.validate_current(plan, cancel).map_err(from_payload)?;
        same_destination(target, displayed_target)?;
        check(cancel)?;
        stage.publish(&tree, name).map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                TreeCopyError::DestinationExists
            } else {
                TreeCopyError::Io(error)
            }
        })?;
        Ok(counts)
    })();
    match operation {
        Ok((files, directories, bytes)) => Ok(TreeCopyReceipt {
            files,
            directories,
            bytes,
            staging_remains: stage.remove_empty_wrapper().err().map(|_| staging_name),
        }),
        Err(cause) => {
            if remove_created(&tree, &created)
                && stage.remove_empty_tree(&tree).is_ok()
                && stage.remove_empty_wrapper().is_ok()
            {
                return Err(cause);
            }
            Err(TreeCopyError::CleanupIncomplete {
                cause: Box::new(cause),
                staging_name,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        copy::publication_probe::{Point, with_probe},
        plan_tree_copy,
    };
    use std::{
        fs as files,
        os::unix::{ffi::OsStrExt, fs::DirBuilderExt},
        time::{SystemTime, UNIX_EPOCH},
    };

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let base = std::env::temp_dir()
                .join(format!("jwm-tree-copy-{}-{stamp}", std::process::id()));
            files::DirBuilder::new().mode(0o700).create(&base).unwrap();
            for name in ["source", "target"] {
                files::create_dir(base.join(name)).unwrap();
            }
            Self(base)
        }

        fn source(&self) -> PathBuf {
            self.0.join("source")
        }

        fn target(&self) -> PathBuf {
            self.0.join("target")
        }

        fn observe(&self) -> (TreeCopyPlan, DirectoryHandle) {
            let source = self.source();
            let target = self.target();
            let source_id = DirectoryHandle::open_without_symlinks(&source)
                .unwrap()
                .identity();
            (
                plan_tree_copy(&source, source_id, &Cancellation::default()).unwrap(),
                DirectoryHandle::open_without_symlinks(&target).unwrap(),
            )
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = files::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn tree_copy_publishes_complete_native_tree_and_never_changes_source() {
        let f = Fixture::new();
        let source = f.source();
        let target = f.target();
        let raw = OsStr::from_bytes(b".raw%\x80.txt");
        files::create_dir_all(source.join("nested/empty")).unwrap();
        files::write(source.join("nested").join(raw), b"bytes").unwrap();
        files::write(source.join("zero"), b"").unwrap();
        let (plan, held) = f.observe();
        let receipt = copy_tree_no_replace(
            &plan,
            &held,
            &target,
            OsStr::new("published"),
            &Cancellation::default(),
        )
        .unwrap();
        assert_eq!(
            receipt,
            TreeCopyReceipt {
                files: 2,
                directories: 3,
                bytes: 5,
                staging_remains: None,
            }
        );
        assert!(target.join("published/nested/empty").is_dir());
        assert_eq!(
            files::read(target.join("published/nested").join(raw)).unwrap(),
            b"bytes"
        );
        assert_eq!(files::read(target.join("published/zero")).unwrap(), b"");
        assert_eq!(files::read(source.join("nested").join(raw)).unwrap(), b"bytes");
        assert_eq!(files::read_dir(target).unwrap().count(), 1);
    }

    #[test]
    fn tree_copy_keeps_late_target_conflict_and_cleans_its_own_stage() {
        let f = Fixture::new();
        let target = f.target();
        files::write(f.source().join("leaf"), b"source").unwrap();
        let (plan, held) = f.observe();
        let late = target.join("published");
        let trigger = late.clone();
        let result = with_probe(
            move |point, name| {
                if point == Point::After && name == OsStr::new("leaf") {
                    files::create_dir(&trigger).unwrap();
                    files::write(trigger.join("keep"), b"original").unwrap();
                }
            },
            || {
                copy_tree_no_replace(
                    &plan,
                    &held,
                    &target,
                    OsStr::new("published"),
                    &Cancellation::default(),
                )
            },
        );
        assert!(matches!(result, Err(TreeCopyError::DestinationExists)));
        assert_eq!(files::read(late.join("keep")).unwrap(), b"original");
        assert_eq!(files::read(f.source().join("leaf")).unwrap(), b"source");
        assert_eq!(files::read_dir(target).unwrap().count(), 1);
    }

    #[test]
    fn tree_copy_cancellation_before_final_publication_has_no_visible_result() {
        let f = Fixture::new();
        let target = f.target();
        files::write(f.source().join("leaf"), b"original").unwrap();
        let (plan, held) = f.observe();
        let cancel = Cancellation::default();
        let stop = cancel.clone();
        let result = with_probe(
            move |point, name| {
                if point == Point::After && name == OsStr::new("leaf") {
                    stop.cancel();
                }
            },
            || copy_tree_no_replace(&plan, &held, &target, OsStr::new("published"), &cancel),
        );
        assert!(matches!(result, Err(TreeCopyError::Cancelled)));
        assert_eq!(files::read_dir(&target).unwrap().count(), 0);
        assert_eq!(files::read(f.source().join("leaf")).unwrap(), b"original");
    }

    #[test]
    fn tree_copy_refuses_source_changes_observed_after_staging() {
        let f = Fixture::new();
        let target = f.target();
        let source_file = f.source().join("leaf");
        files::write(&source_file, b"original").unwrap();
        let (plan, held) = f.observe();
        let changed = source_file.clone();
        let result = with_probe(
            move |point, name| {
                if point == Point::After && name == OsStr::new("leaf") {
                    files::write(&changed, b"edited").unwrap();
                }
            },
            || {
                copy_tree_no_replace(
                    &plan,
                    &held,
                    &target,
                    OsStr::new("published"),
                    &Cancellation::default(),
                )
            },
        );
        assert!(matches!(result, Err(TreeCopyError::SourceChanged)));
        assert_eq!(files::read_dir(&target).unwrap().count(), 0);
        assert_eq!(files::read(source_file).unwrap(), b"edited");
    }

    #[test]
    fn tree_copy_cleanup_leaves_untracked_stage_child_untouched() {
        let fixture = Fixture::new();
        let target = fixture.target();
        files::write(fixture.source().join("leaf"), b"source").unwrap();
        let (plan, held) = fixture.observe();
        let probe_target = target.clone();
        let result = with_probe(
            move |point, name| {
                if point == Point::After && name == OsStr::new("leaf") {
                    let mut candidates = files::read_dir(&probe_target).unwrap();
                    let wrapper = candidates.next().unwrap().unwrap().path();
                    assert!(candidates.next().is_none());
                    files::write(wrapper.join("tree/foreign"), b"preserve").unwrap();
                }
            },
            || {
                copy_tree_no_replace(
                    &plan,
                    &held,
                    &target,
                    OsStr::new("published"),
                    &Cancellation::default(),
                )
            },
        );
        let Err(TreeCopyError::CleanupIncomplete { cause, staging_name }) = result else {
            panic!("untracked stage content must be retained");
        };
        assert!(matches!(*cause, TreeCopyError::StagingChanged));
        assert!(!target.join("published").exists());
        let residue = target.join(staging_name).join(STAGED_ROOT);
        assert!(!residue.join("leaf").exists());
        assert_eq!(files::read(residue.join("foreign")).unwrap(), b"preserve");
        assert_eq!(files::read(fixture.source().join("leaf")).unwrap(), b"source");
    }

    #[test]
    fn tree_copy_rejects_oversized_sparse_payload_without_creating_staging() {
        let f = Fixture::new();
        let target = f.target();
        files::File::create(f.source().join("large"))
            .unwrap()
            .set_len((1u64 << 30) + 1)
            .unwrap();
        let (plan, held) = f.observe();
        let result = copy_tree_no_replace(
            &plan,
            &held,
            &target,
            OsStr::new("published"),
            &Cancellation::default(),
        );
        assert!(matches!(result, Err(TreeCopyError::ByteBudget)));
        assert_eq!(files::read_dir(target).unwrap().count(), 0);
    }
}
