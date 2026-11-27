//! Bounded, non-transactional batches of no-replace regular-file copies.
use crate::{Cancellation, CopyDestination, CopyError, CopyReceipt, CopySource, DirectoryHandle};
use std::{collections::HashSet, path::Path};

pub const COPY_BATCH_LIMIT: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CopyBatchInputError {
    InvalidCount,
    InvalidName,
    DuplicateName,
}

#[derive(Debug)]
pub enum CopyBatchItem {
    Published(CopyReceipt),
    ExistingTarget,
    CancelledBeforePublication,
    Failed(CopyError),
    NotStarted,
}

fn run_batch(
    count: usize,
    cancel: &Cancellation,
    mut copy: impl FnMut(usize) -> Result<CopyReceipt, CopyError>,
) -> Vec<CopyBatchItem> {
    let mut items = Vec::with_capacity(count);
    for i in 0..count {
        if cancel.check().is_err() {
            break;
        }
        let outcome = match copy(i) {
            Ok(receipt) => CopyBatchItem::Published(receipt),
            Err(CopyError::DestinationExists) => CopyBatchItem::ExistingTarget,
            Err(CopyError::Cancelled) => CopyBatchItem::CancelledBeforePublication,
            Err(error) => CopyBatchItem::Failed(error),
        };
        let stop = matches!(
            outcome,
            CopyBatchItem::CancelledBeforePublication
                | CopyBatchItem::Failed(
                    CopyError::ParentChanged | CopyError::StagingUnavailable(_)
                )
        );
        items.push(outcome);
        if stop {
            break;
        }
    }
    items.resize_with(count, || CopyBatchItem::NotStarted);
    items
}

impl DirectoryHandle {
    /// Copy observed regular files in input order, preserving their names.
    /// Each item publishes independently. Existing targets are skipped, not
    /// replaced. Cancellation does not roll back published files. A changed
    /// parent or an unavailable staging facility stops the rest of the batch.
    /// Call off the UI thread. This is not an on-disk log or a move.
    pub fn copy_regular_batch_no_replace(
        &self,
        sources: &[CopySource<'_>],
        destination: &Path,
        cancel: &Cancellation,
    ) -> Result<Vec<CopyBatchItem>, CopyBatchInputError> {
        if sources.is_empty() || sources.len() > COPY_BATCH_LIMIT {
            return Err(CopyBatchInputError::InvalidCount);
        }
        let mut names = HashSet::with_capacity(sources.len());
        for source in sources {
            if !crate::mkdir::single_name(source.name) || source.name.len() > 255 {
                return Err(CopyBatchInputError::InvalidName);
            }
            if !names.insert(source.name) {
                return Err(CopyBatchInputError::DuplicateName);
            }
        }
        Ok(run_batch(sources.len(), cancel, |i| {
            let source = &sources[i];
            self.copy_regular_no_replace(
                CopySource {
                    parent: source.parent,
                    displayed_parent: source.displayed_parent,
                    name: source.name,
                    expected_file: source.expected_file,
                },
                CopyDestination {
                    displayed_parent: destination,
                    name: source.name,
                },
                cancel,
            )
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        FileIdentity,
        copy::publication_probe::{Point, with_probe},
    };
    use std::{
        ffi::OsStr,
        fs,
        os::unix::{
            ffi::OsStrExt,
            fs::{DirBuilderExt, MetadataExt},
        },
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path =
                std::env::temp_dir().join(format!("jwm-copy-batch-{}-{stamp}", std::process::id()));
            fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn batch_copies_regular_files_without_replacing_conflicts_or_touching_sources() {
        let root = Fixture::new();
        let from = root.0.join("from");
        let to = root.0.join("to");
        fs::create_dir(&from).unwrap();
        fs::create_dir(&to).unwrap();
        let origin = DirectoryHandle::open(&from).unwrap();
        let target = DirectoryHandle::open(&to).unwrap();
        let first = OsStr::from_bytes(b"raw%\x80");
        let clash = OsStr::new("existing");
        let last = OsStr::new("last");
        let names = [first, clash, last];
        let make_source = |name: &'static OsStr| {
            let path = from.join(name);
            let metadata = fs::symlink_metadata(path).unwrap();
            CopySource {
                parent: &origin,
                displayed_parent: &from,
                name,
                expected_file: FileIdentity {
                    device: metadata.dev(),
                    inode: metadata.ino(),
                },
            }
        };
        for (name, value) in names.into_iter().zip([b"one", b"two", b"end"]) {
            fs::write(from.join(name), value).unwrap();
        }
        fs::write(to.join(clash), b"original").unwrap();
        let cancel = Cancellation::default();
        assert!(matches!(
            target.copy_regular_batch_no_replace(
                &[make_source(first), make_source(first)],
                &to,
                &cancel,
            ),
            Err(CopyBatchInputError::DuplicateName)
        ));
        assert!(!to.join(first).exists());
        let result = target
            .copy_regular_batch_no_replace(&names.map(make_source), &to, &cancel)
            .unwrap();
        assert!(matches!(
            result.as_slice(),
            [
                CopyBatchItem::Published(CopyReceipt { bytes: 3 }),
                CopyBatchItem::ExistingTarget,
                CopyBatchItem::Published(CopyReceipt { bytes: 3 }),
            ]
        ));
        assert_eq!(fs::read(to.join(first)).unwrap(), b"one");
        assert_eq!(fs::read(to.join(clash)).unwrap(), b"original");
        assert_eq!(fs::read(to.join(last)).unwrap(), b"end");
        for (name, value) in names.into_iter().zip([b"one", b"two", b"end"]) {
            assert_eq!(fs::read(from.join(name)).unwrap(), value);
        }
    }

    #[test]
    fn batch_cancellation_at_each_side_of_real_publication_preserves_source_bytes() {
        for cut in [Point::Before, Point::After] {
            let root = Fixture::new();
            let from = root.0.join("from");
            let to = root.0.join("to");
            fs::create_dir(&from).unwrap();
            fs::create_dir(&to).unwrap();
            let origin = DirectoryHandle::open(&from).unwrap();
            let target = DirectoryHandle::open(&to).unwrap();
            let sources = ["a", "b", "c", "d"].map(|name| {
                let path = from.join(name);
                fs::write(&path, name.as_bytes()).unwrap();
                let meta = fs::symlink_metadata(path).unwrap();
                CopySource {
                    parent: &origin,
                    displayed_parent: &from,
                    name: OsStr::new(name),
                    expected_file: FileIdentity {
                        device: meta.dev(),
                        inode: meta.ino(),
                    },
                }
            });
            fs::write(to.join("b"), b"original").unwrap();
            let cancel = Cancellation::default();
            let at_boundary = cancel.clone();
            let items = with_probe(
                move |point, name| {
                    assert_ne!(name, OsStr::new("d"), "the fourth copy must not start");
                    if point == cut && name == OsStr::new("c") {
                        at_boundary.cancel();
                    }
                },
                || {
                    target
                        .copy_regular_batch_no_replace(&sources, &to, &cancel)
                        .unwrap()
                },
            );
            assert!(matches!(items[0], CopyBatchItem::Published(_)));
            assert!(matches!(items[1], CopyBatchItem::ExistingTarget));
            assert!(matches!(items[3], CopyBatchItem::NotStarted));
            assert_eq!(fs::read(to.join("a")).unwrap(), b"a");
            assert_eq!(fs::read(to.join("b")).unwrap(), b"original");
            assert!(!to.join("d").exists());
            match cut {
                Point::Before => {
                    assert!(matches!(
                        items[2],
                        CopyBatchItem::CancelledBeforePublication
                    ));
                    assert!(!to.join("c").exists());
                }
                Point::After => {
                    assert!(matches!(items[2], CopyBatchItem::Published(_)));
                    assert_eq!(fs::read(to.join("c")).unwrap(), b"c");
                }
            }
            for name in ["a", "b", "c", "d"] {
                assert_eq!(fs::read(from.join(name)).unwrap(), name.as_bytes());
            }
        }
    }

    #[test]
    fn batch_distinguishes_conflicts_cancelled_and_not_started() {
        let cancel = Cancellation::default();
        let result = run_batch(4, &cancel, |i| match i {
            0 => Ok(CopyReceipt { bytes: 5 }),
            1 => Err(CopyError::DestinationExists),
            2 => {
                cancel.cancel();
                Err(CopyError::Cancelled)
            }
            _ => panic!("the fourth item must not start"),
        });
        assert!(matches!(
            result.as_slice(),
            [
                CopyBatchItem::Published(CopyReceipt { bytes: 5 }),
                CopyBatchItem::ExistingTarget,
                CopyBatchItem::CancelledBeforePublication,
                CopyBatchItem::NotStarted,
            ]
        ));
    }

    #[test]
    fn late_cancellation_does_not_reclassify_publication() {
        let cancel = Cancellation::default();
        let result = run_batch(2, &cancel, |i| {
            assert_eq!(i, 0);
            cancel.cancel();
            Ok(CopyReceipt { bytes: 9 })
        });
        assert!(matches!(
            result.as_slice(),
            [
                CopyBatchItem::Published(CopyReceipt { bytes: 9 }),
                CopyBatchItem::NotStarted,
            ]
        ));
    }

    #[test]
    fn initially_cancelled_batch_never_starts_an_item() {
        let cancel = Cancellation::default();
        cancel.cancel();
        let result = run_batch(2, &cancel, |_| panic!("nothing may start"));
        assert!(matches!(
            result.as_slice(),
            [CopyBatchItem::NotStarted, CopyBatchItem::NotStarted]
        ));
    }

    #[test]
    fn revalidated_parent_failure_stops_the_batch() {
        let result = run_batch(2, &Cancellation::default(), |i| {
            assert_eq!(i, 0);
            Err(CopyError::ParentChanged)
        });
        assert!(matches!(
            result.as_slice(),
            [
                CopyBatchItem::Failed(CopyError::ParentChanged),
                CopyBatchItem::NotStarted,
            ]
        ));
    }
}
