//! Strict metadata-only preflight for a future atomic directory copy.
use crate::{
    Cancellation, DirectoryHandle, FileIdentity, FileKind, ListError, WalkOptions, visit_local_index,
};
use std::path::{Path, PathBuf};

const LIMIT: WalkOptions = WalkOptions {
    entries: 8_192,
    directories: 1_024,
    depth: 32,
    show_hidden: true,
};
const PATH_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeCopyEntry {
    pub relative: PathBuf,
    pub kind: FileKind,
    pub identity: FileIdentity,
}

#[derive(Debug)]
pub enum TreeCopyPlanError {
    Walk(ListError),
    Incomplete,
    NestedMount,
    Unsupported(PathBuf),
    PathBudget,
}

/// A complete bounded observation, not a source snapshot or capability.
/// Any execution must separately revalidate observations and destination.
#[derive(Debug)]
pub struct TreeCopyPlan {
    pub root: PathBuf,
    pub identity: FileIdentity,
    pub entries: Vec<TreeCopyEntry>,
}

/// Preflight one observed real local folder, reading metadata but not bytes.
/// Includes hidden/empty directories. Produces no partial plan, and refuses
/// symlinks, special files and nested mounts. Call this off the UI thread.
pub fn plan_tree_copy(
    root: &Path,
    identity: FileIdentity,
    cancel: &Cancellation,
) -> Result<TreeCopyPlan, TreeCopyPlanError> {
    plan(root, identity, cancel, LIMIT, PATH_BYTES)
}

#[derive(Debug)]
pub enum TreeCopyTargetError {
    TargetUnavailable(std::io::Error),
    TargetChanged,
    TargetInsideSource,
    SourceChanged,
    Plan(TreeCopyPlanError),
}

/// Revalidate the current source topology and the held destination directory.
/// This checks observations only and never permits a write by itself.
pub fn check_tree_copy_target(
    original: &TreeCopyPlan,
    held_target: &DirectoryHandle,
    displayed_target: &Path,
    cancel: &Cancellation,
) -> Result<(), TreeCopyTargetError> {
    let target_id = held_target.identity();
    let verify_target = || {
        let current = DirectoryHandle::open_without_symlinks(displayed_target)
            .map_err(TreeCopyTargetError::TargetUnavailable)?;
        if current.identity() != target_id {
            return Err(TreeCopyTargetError::TargetChanged);
        }
        Ok(())
    };
    verify_target()?;
    if target_id == original.identity {
        return Err(TreeCopyTargetError::TargetInsideSource);
    }
    let current = plan_tree_copy(&original.root, original.identity, cancel)
        .map_err(TreeCopyTargetError::Plan)?;
    if current
        .entries
        .iter()
        .any(|entry| entry.kind == FileKind::Directory && entry.identity == target_id)
    {
        return Err(TreeCopyTargetError::TargetInsideSource);
    }
    if current.entries != original.entries {
        return Err(TreeCopyTargetError::SourceChanged);
    }
    verify_target()?;
    cancel
        .check()
        .map_err(|e| TreeCopyTargetError::Plan(TreeCopyPlanError::Walk(e)))
}

fn plan(
    root: &Path,
    identity: FileIdentity,
    cancel: &Cancellation,
    options: WalkOptions,
    budget: usize,
) -> Result<TreeCopyPlan, TreeCopyPlanError> {
    let mut entries = Vec::new();
    let mut rejected = None;
    let mut remaining = budget
        .checked_sub(root.as_os_str().len())
        .ok_or(TreeCopyPlanError::PathBudget)?;
    let summary = visit_local_index(root, identity, options, cancel, |entry| {
        if rejected.is_some() {
            return;
        }
        let Some(left) = remaining.checked_sub(entry.relative.as_os_str().len()) else {
            rejected = Some(TreeCopyPlanError::PathBudget);
            return;
        };
        remaining = left;
        if !matches!(entry.kind, FileKind::Directory | FileKind::Regular) {
            rejected = Some(TreeCopyPlanError::Unsupported(entry.relative.to_owned()));
            return;
        }
        entries.push(TreeCopyEntry {
            relative: entry.relative.to_owned(),
            kind: entry.kind,
            identity: entry.identity,
        });
    })
    .map_err(TreeCopyPlanError::Walk)?;
    if let Some(error) = rejected {
        return Err(error);
    }
    if summary.incomplete {
        return Err(TreeCopyPlanError::Incomplete);
    }
    if summary.excluded_mounts != 0 {
        return Err(TreeCopyPlanError::NestedMount);
    }
    cancel.check().map_err(TreeCopyPlanError::Walk)?;
    entries.sort_unstable_by(|a, b| a.relative.as_os_str().cmp(b.relative.as_os_str()));
    Ok(TreeCopyPlan {
        root: root.to_owned(),
        identity,
        entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        ffi::OsStr,
        fs,
        os::unix::{
            ffi::OsStrExt,
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
            let root =
                std::env::temp_dir().join(format!("jwm-tree-plan-{}-{stamp}", std::process::id()));
            fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
            Self(root)
        }

        fn identity(&self) -> FileIdentity {
            DirectoryHandle::open_without_symlinks(&self.0)
                .unwrap()
                .identity()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn tree_copy_target_rechecks_identity_and_rejects_same_or_descendant() {
        let f = Fixture::new();
        let source = f.0.join("source");
        let inside = source.join("child");
        let outside = f.0.join("outside");
        fs::create_dir_all(&inside).unwrap();
        fs::create_dir(&outside).unwrap();
        let held_source = DirectoryHandle::open_without_symlinks(&source).unwrap();
        let held_inside = DirectoryHandle::open_without_symlinks(&inside).unwrap();
        let held_outside = DirectoryHandle::open_without_symlinks(&outside).unwrap();
        let cancel = Cancellation::default();
        let plan = plan_tree_copy(&source, held_source.identity(), &cancel).unwrap();
        for (folder, held) in [(&source, &held_source), (&inside, &held_inside)] {
            assert!(matches!(
                check_tree_copy_target(&plan, held, folder, &cancel),
                Err(TreeCopyTargetError::TargetInsideSource)
            ));
        }
        assert!(check_tree_copy_target(&plan, &held_outside, &outside, &cancel).is_ok());
        let moved = f.0.join("moved");
        fs::rename(&outside, &moved).unwrap();
        symlink(&inside, &outside).unwrap();
        assert!(matches!(
            check_tree_copy_target(&plan, &held_outside, &outside, &cancel),
            Err(TreeCopyTargetError::TargetUnavailable(_))
        ));
        fs::remove_file(&outside).unwrap();
        fs::create_dir(&outside).unwrap();
        assert!(matches!(
            check_tree_copy_target(&plan, &held_outside, &outside, &cancel),
            Err(TreeCopyTargetError::TargetChanged)
        ));
        fs::write(source.join("changed"), b"new").unwrap();
        assert!(matches!(
            check_tree_copy_target(&plan, &held_outside, &moved, &cancel),
            Err(TreeCopyTargetError::SourceChanged)
        ));
    }

    #[test]
    fn tree_plan_keeps_exact_names_empty_directories_and_hidden_entries() {
        let root = Fixture::new();
        let hidden = OsStr::from_bytes(b".raw%\x80");
        fs::create_dir_all(root.0.join("folder/empty")).unwrap();
        fs::write(root.0.join("folder").join(hidden), b"contents").unwrap();
        let plan = plan_tree_copy(&root.0, root.identity(), &Cancellation::default()).unwrap();
        assert_eq!(plan.entries.len(), 3);
        assert_eq!(plan.entries[0].relative, Path::new("folder"));
        assert_eq!(plan.entries[0].kind, FileKind::Directory);
        assert_eq!(plan.entries[1].relative, Path::new("folder").join(hidden));
        assert_eq!(plan.entries[1].kind, FileKind::Regular);
        assert_eq!(plan.entries[2].relative, Path::new("folder/empty"));
        assert_eq!(plan.entries[2].kind, FileKind::Directory);
    }

    #[test]
    fn tree_plan_refuses_a_link_and_returns_no_partial_plan() {
        let root = Fixture::new();
        let outside = Fixture::new();
        fs::write(root.0.join("regular"), b"inside").unwrap();
        fs::write(outside.0.join("external"), b"outside").unwrap();
        symlink(&outside.0, root.0.join("link")).unwrap();
        let result = plan_tree_copy(&root.0, root.identity(), &Cancellation::default());
        assert!(
            matches!(result, Err(TreeCopyPlanError::Unsupported(path)) if path == Path::new("link"))
        );
        assert_eq!(fs::read(outside.0.join("external")).unwrap(), b"outside");
    }

    #[test]
    fn tree_plan_rejects_fifo_and_a_replaced_root() {
        use rustix::fs::{self as unix, FileType, Mode};

        let fixture = Fixture::new();
        let root = fixture.0.join("root");
        fs::create_dir(&root).unwrap();
        let held = DirectoryHandle::open_without_symlinks(&root).unwrap();
        let expected = held.identity();
        unix::mknodat(&held.fd, "pipe", FileType::Fifo, Mode::RUSR | Mode::WUSR, 0).unwrap();
        let result = plan_tree_copy(&root, expected, &Cancellation::default());
        assert!(
            matches!(result, Err(TreeCopyPlanError::Unsupported(path)) if path == Path::new("pipe"))
        );
        let old = fixture.0.join("old");
        fs::rename(&root, &old).unwrap();
        fs::create_dir(&root).unwrap();
        assert!(matches!(
            plan_tree_copy(&root, expected, &Cancellation::default()),
            Err(TreeCopyPlanError::Walk(ListError::ReadDirectory { .. }))
        ));
        assert!(old.join("pipe").exists());
    }

    #[test]
    fn tree_plan_rejects_budgets_and_prior_cancellation_without_a_partial_result() {
        let root = Fixture::new();
        fs::write(root.0.join("first"), b"a").unwrap();
        fs::write(root.0.join("second"), b"b").unwrap();
        let identity = root.identity();
        let cancel = Cancellation::default();
        let narrow = WalkOptions {
            entries: 1,
            ..LIMIT
        };
        assert!(matches!(
            plan(&root.0, identity, &cancel, narrow, PATH_BYTES),
            Err(TreeCopyPlanError::Incomplete)
        ));
        assert!(matches!(
            plan(&root.0, identity, &cancel, LIMIT, root.0.as_os_str().len()),
            Err(TreeCopyPlanError::PathBudget)
        ));
        cancel.cancel();
        assert!(matches!(
            plan_tree_copy(&root.0, identity, &cancel),
            Err(TreeCopyPlanError::Walk(ListError::Cancelled))
        ));
    }
}
