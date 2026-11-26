#![cfg(target_os = "linux")]

use jwm_file_core::{
    Cancellation, DirectoryHandle, FileProvider, ListOptions, LocalProvider, Location,
    MakeDirectoryError,
};
use std::ffi::OsStr;
use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        loop {
            let root = std::env::temp_dir().join(format!(
                "jwm-files-mkdir-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
            ));
            match fs::create_dir(&root) {
                Ok(()) => return Self(root),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                Err(error) => panic!("test fixture: {error}"),
            }
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn creates_one_unicode_child_without_overwriting_anything() {
    let root = Fixture::new();
    let target = root.0.join("已存在");
    fs::write(&target, b"keep").unwrap();
    let parent = DirectoryHandle::open(&root.0).unwrap();
    parent.create_directory(OsStr::new("照片🐕")).unwrap();
    assert!(root.0.join("照片🐕").is_dir());
    assert!(matches!(
        parent.create_directory(OsStr::new("已存在")),
        Err(MakeDirectoryError::Io(error)) if error.kind() == ErrorKind::AlreadyExists
    ));
    assert_eq!(fs::read(target).unwrap(), b"keep");
}

#[test]
fn invalid_or_multicomponent_names_are_rejected() {
    let root = Fixture::new();
    let parent = DirectoryHandle::open(&root.0).unwrap();
    for name in [
        "",
        ".",
        "..",
        "/",
        "/abs",
        "nested/name",
        "trailing/",
        "a\0b",
    ] {
        assert!(matches!(
            parent.create_directory(OsStr::new(name)),
            Err(MakeDirectoryError::InvalidName)
        ));
    }
    assert_eq!(fs::read_dir(&root.0).unwrap().count(), 0);
}

#[test]
fn replacing_the_parent_path_does_not_redirect_the_operation() {
    let root = Fixture::new();
    let chosen = root.0.join("chosen");
    let moved = root.0.join("moved");
    fs::create_dir(&chosen).unwrap();
    let snapshot = LocalProvider
        .list(
            &Location::Local(chosen.clone()),
            ListOptions::default(),
            &Cancellation::default(),
        )
        .unwrap();
    let parent = snapshot.local_parent.unwrap();
    fs::rename(&chosen, &moved).unwrap();
    fs::create_dir(&chosen).unwrap();
    let replacement = DirectoryHandle::open(&chosen).unwrap();
    assert_ne!(parent.identity(), replacement.identity());
    assert!(matches!(
        parent.create_directory_at(&chosen, OsStr::new("album")),
        Err(MakeDirectoryError::ParentChanged)
    ));
    assert!(!moved.join("album").exists());
    parent
        .create_directory_at(&moved, OsStr::new("album"))
        .unwrap();
    assert!(moved.join("album").is_dir());
    assert!(!chosen.join("album").exists());
}

fn scanned_child(
    root: &Fixture,
    name: &OsStr,
) -> (std::sync::Arc<DirectoryHandle>, jwm_file_core::FileIdentity) {
    let snapshot = LocalProvider
        .list(
            &Location::Local(root.0.clone()),
            ListOptions::default(),
            &Cancellation::default(),
        )
        .unwrap();
    let identity = snapshot
        .entries
        .iter()
        .find(|entry| entry.name == name)
        .unwrap()
        .identity
        .unwrap();
    (snapshot.local_parent.unwrap(), identity)
}

#[test]
fn rename_keeps_exact_bytes_and_refuses_existing_destination() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let root = Fixture::new();
    let name = OsString::from_vec(b"source-\xff".to_vec());
    fs::write(root.0.join(&name), b"source").unwrap();
    fs::write(root.0.join("already"), b"destination").unwrap();
    let (parent, identity) = scanned_child(&root, &name);
    assert!(matches!(
        parent.rename_child_no_replace(&root.0, &name, identity, OsStr::new("already")),
        Err(jwm_file_core::RenameError::Io(error)) if error.kind() == ErrorKind::AlreadyExists
    ));
    assert_eq!(fs::read(root.0.join("already")).unwrap(), b"destination");
    assert_eq!(fs::read(root.0.join(&name)).unwrap(), b"source");
    parent
        .rename_child_no_replace(&root.0, &name, identity, OsStr::new("新名"))
        .unwrap();
    assert!(!root.0.join(name).exists());
    assert_eq!(fs::read(root.0.join("新名")).unwrap(), b"source");
}

#[test]
fn rename_refuses_an_item_replaced_since_selection() {
    let root = Fixture::new();
    let name = OsStr::new("selected");
    fs::write(root.0.join(name), b"original").unwrap();
    let (parent, identity) = scanned_child(&root, name);
    fs::rename(root.0.join(name), root.0.join("retired")).unwrap();
    fs::write(root.0.join(name), b"replacement").unwrap();
    assert!(matches!(
        parent.rename_child_no_replace(&root.0, name, identity, OsStr::new("new")),
        Err(jwm_file_core::RenameError::SourceChanged)
    ));
    assert_eq!(fs::read(root.0.join(name)).unwrap(), b"replacement");
    assert!(!root.0.join("new").exists());
}

#[test]
fn directory_watch_coalesces_changes_and_reports_moved_parent() {
    use jwm_file_core::{DirectoryChange, DirectoryWatch};

    let root = Fixture::new();
    let child = root.0.join("origin");
    fs::create_dir(&child).unwrap();
    let parent = DirectoryHandle::open(&child).unwrap();
    let watcher = DirectoryWatch::new(&parent).unwrap();
    assert_eq!(watcher.read_change().unwrap(), DirectoryChange::Unchanged);
    fs::write(child.join("new"), b"new").unwrap();
    fs::rename(child.join("new"), child.join("renamed")).unwrap();
    assert_eq!(watcher.read_change().unwrap(), DirectoryChange::Dirty);
    assert_eq!(watcher.read_change().unwrap(), DirectoryChange::Unchanged);
    fs::rename(&child, root.0.join("moved")).unwrap();
    fs::create_dir(&child).unwrap();
    assert_eq!(watcher.read_change().unwrap(), DirectoryChange::Invalidated);
}

#[test]
fn scan_worker_reports_changes_only_for_its_installed_directory_watch() {
    use jwm_file_core::{DirectoryChange, ScanWorker};
    use std::time::{Duration, Instant};

    fn wait<T>(mut get: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(value) = get() {
                return value;
            }
            assert!(Instant::now() < deadline, "watch worker response timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    let root = Fixture::new();
    let first = root.0.join("first");
    let second = root.0.join("second");
    fs::create_dir(&first).unwrap();
    fs::create_dir(&second).unwrap();
    let worker = ScanWorker::start(LocalProvider).unwrap();
    let a = worker.submit(Location::Local(first.clone()), ListOptions::default());
    let reply = wait(|| worker.take_reply());
    assert_eq!(reply.id, a);
    assert!(reply.result.is_ok());
    fs::write(first.join("a"), b"one").unwrap();
    let notice = wait(|| worker.take_directory_change());
    assert_eq!((notice.id, notice.change), (a, DirectoryChange::Dirty));

    let b = worker.submit(Location::Local(second.clone()), ListOptions::default());
    let reply = wait(|| worker.take_reply());
    assert_eq!(reply.id, b);
    assert!(reply.result.is_ok());
    fs::write(first.join("retired"), b"ignore").unwrap();
    fs::write(second.join("b"), b"two").unwrap();
    let notice = wait(|| worker.take_directory_change());
    assert_eq!((notice.id, notice.change), (b, DirectoryChange::Dirty));
}

#[test]
fn tree_returns_native_directories_without_following_child_links() {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    let native = OsStr::from_bytes(b"raw\x80");
    let directory = fixture.0.join(native);
    fs::create_dir(&directory).unwrap();
    fs::create_dir(fixture.0.join(".hidden")).unwrap();
    fs::write(fixture.0.join("document"), b"text").unwrap();
    symlink(&fixture.0, fixture.0.join("cycle")).unwrap();
    fs::create_dir(directory.join("nested")).unwrap();
    let root = Location::Local(fixture.0.clone());
    let cancel = Cancellation::default();
    let snapshot = LocalProvider
        .list_tree_children(&root, None, ListOptions::default(), &cancel)
        .unwrap();
    assert_eq!(snapshot.entries.len(), 1);
    let entry = &snapshot.entries[0];
    assert_eq!(entry.name, native);
    assert_eq!(entry.location, Location::Local(directory.clone()));
    let nested = LocalProvider
        .list_tree_children(
            &entry.location,
            entry.identity,
            ListOptions::default(),
            &cancel,
        )
        .unwrap();
    assert_eq!(nested.entries[0].name, "nested");
    let shown = LocalProvider
        .list_tree_children(&root, None, ListOptions { show_hidden: true }, &cancel)
        .unwrap();
    assert_eq!(shown.entries.len(), 2);
    // Replace the entry with a link to the original inode: even though its
    // target identity matches, expansion must not follow it.
    let moved = fixture.0.join("moved");
    fs::rename(&directory, &moved).unwrap();
    symlink(&moved, &directory).unwrap();
    assert!(
        LocalProvider
            .list_tree_children(
                &entry.location,
                entry.identity,
                ListOptions::default(),
                &cancel
            )
            .is_err()
    );
    fs::remove_file(&directory).unwrap();
    fs::create_dir(&directory).unwrap();
    assert!(
        LocalProvider
            .list_tree_children(
                &entry.location,
                entry.identity,
                ListOptions::default(),
                &cancel
            )
            .is_err()
    );
}

#[test]
fn rename_undo_restores_native_name_without_replacing_a_conflict() {
    use jwm_file_core::{FileIdentity, RenameError};
    use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
    let fixture = Fixture::new();
    let old = OsStr::from_bytes(b"source-\x80");
    let renamed = OsStr::new("new name");
    fs::write(fixture.0.join(old), b"original").unwrap();
    let original = fs::metadata(fixture.0.join(old)).unwrap();
    let id = FileIdentity {
        device: original.dev(),
        inode: original.ino(),
    };
    let folder = DirectoryHandle::open(&fixture.0).unwrap();
    let receipt = folder
        .rename_child_recorded(&fixture.0, old, id, renamed)
        .unwrap()
        .unwrap();
    fs::write(fixture.0.join(old), b"conflict").unwrap();
    assert!(matches!(
        receipt.undo(&folder, &fixture.0),
        Err(RenameError::Io(e)) if e.kind() == std::io::ErrorKind::AlreadyExists
    ));
    assert_eq!(fs::read(fixture.0.join(old)).unwrap(), b"conflict");
    assert_eq!(fs::read(fixture.0.join(renamed)).unwrap(), b"original");
    fs::remove_file(fixture.0.join(old)).unwrap();
    receipt.undo(&folder, &fixture.0).unwrap();
    assert_eq!(fs::read(fixture.0.join(old)).unwrap(), b"original");
    assert!(!fixture.0.join(renamed).exists());
}

#[test]
fn rename_undo_refuses_a_different_parent_or_replacement_inode() {
    use jwm_file_core::{FileIdentity, RenameError};
    use std::os::unix::fs::MetadataExt;
    let fixture = Fixture::new();
    let old = OsStr::new("previous");
    let renamed = OsStr::new("current");
    let other = fixture.0.join("other");
    fs::create_dir(&other).unwrap();
    fs::write(fixture.0.join(old), b"original").unwrap();
    let original = fs::metadata(fixture.0.join(old)).unwrap();
    let id = FileIdentity {
        device: original.dev(),
        inode: original.ino(),
    };
    let folder = DirectoryHandle::open(&fixture.0).unwrap();
    let receipt = folder
        .rename_child_recorded(&fixture.0, old, id, renamed)
        .unwrap()
        .unwrap();
    assert!(matches!(
        receipt.undo(&DirectoryHandle::open(&other).unwrap(), &other),
        Err(RenameError::ParentChanged)
    ));
    fs::remove_file(fixture.0.join(renamed)).unwrap();
    // The receipt still pins the original inode despite the removal.
    fs::write(fixture.0.join(renamed), b"replacement").unwrap();
    assert!(matches!(
        receipt.undo(&folder, &fixture.0),
        Err(RenameError::SourceChanged)
    ));
    assert!(!fixture.0.join(old).exists());
    assert_eq!(fs::read(fixture.0.join(renamed)).unwrap(), b"replacement");
}

#[test]
fn staged_copy_publishes_exact_bytes_without_replacing_destination() {
    use jwm_file_core::{CopyDestination, CopyError, CopySource};
    use std::os::unix::ffi::OsStrExt;

    let fixture = Fixture::new();
    let from = fixture.0.join("from");
    let to = fixture.0.join("to");
    fs::create_dir(&from).unwrap();
    fs::create_dir(&to).unwrap();
    let source = OsStr::from_bytes(b"report-\x80");
    let destination = OsStr::from_bytes(b"copy-\x81");
    let bytes: Vec<_> = (0..440_000).map(|index| (index % 251) as u8).collect();
    fs::write(from.join(source), &bytes).unwrap();
    let local = LocalProvider
        .list(
            &Location::Local(from.clone()),
            ListOptions::default(),
            &Cancellation::default(),
        )
        .unwrap();
    let from_handle = local.local_parent.unwrap();
    let identity = local.entries[0].identity.unwrap();
    let to_handle = DirectoryHandle::open(&to).unwrap();
    let run = |cancel: &Cancellation| {
        to_handle.copy_regular_no_replace(
            CopySource {
                parent: &from_handle,
                displayed_parent: &from,
                name: source,
                expected_file: identity,
            },
            CopyDestination {
                displayed_parent: &to,
                name: destination,
            },
            cancel,
        )
    };

    let cancelled = Cancellation::default();
    cancelled.cancel();
    assert!(matches!(run(&cancelled), Err(CopyError::Cancelled)));
    assert_eq!(fs::read_dir(&to).unwrap().count(), 0);
    fs::write(to.join(destination), b"keep").unwrap();
    assert!(matches!(
        run(&Cancellation::default()),
        Err(CopyError::DestinationExists)
    ));
    assert_eq!(fs::read(to.join(destination)).unwrap(), b"keep");
    fs::remove_file(to.join(destination)).unwrap();
    let receipt = run(&Cancellation::default()).unwrap();
    assert_eq!(receipt.bytes, bytes.len() as u64);
    assert_eq!(fs::read(to.join(destination)).unwrap(), bytes);
    assert_eq!(fs::read(from.join(source)).unwrap(), bytes);
    assert_eq!(fs::read_dir(&to).unwrap().count(), 1);
}

#[test]
fn staged_copy_keeps_sparse_payloads_and_trailing_holes() {
    use jwm_file_core::{CopyDestination, CopySource, FileIdentity};
    use std::{
        fs::File,
        io::{Read, Seek},
        os::unix::fs::{FileExt, MetadataExt},
    };

    let fixture = Fixture::new();
    let from = fixture.0.join("from");
    let to = fixture.0.join("to");
    fs::create_dir(&from).unwrap();
    fs::create_dir(&to).unwrap();
    let from_handle = DirectoryHandle::open(&from).unwrap();
    let to_handle = DirectoryHandle::open(&to).unwrap();
    let size = 64 * 1024 * 1024;
    for name in ["two-extents", "hole-only"] {
        let source = File::create_new(from.join(name)).unwrap();
        source.set_len(size).unwrap();
        if name == "two-extents" {
            source.write_all_at(b"begin", 0).unwrap();
            source.write_all_at(b"middle", size / 2).unwrap();
        }
        source.sync_all().unwrap();
        let expected = source.metadata().unwrap();
        let input = File::open(from.join(name)).unwrap();
        let hole_reported =
            rustix::fs::seek(&input, rustix::fs::SeekFrom::Hole(0)).is_ok_and(|hole| hole < size);
        let receipt = to_handle
            .copy_regular_no_replace(
                CopySource {
                    parent: &from_handle,
                    displayed_parent: &from,
                    name: OsStr::new(name),
                    expected_file: FileIdentity {
                        device: expected.dev(),
                        inode: expected.ino(),
                    },
                },
                CopyDestination {
                    displayed_parent: &to,
                    name: OsStr::new(name),
                },
                &Cancellation::default(),
            )
            .unwrap();
        assert_eq!(receipt.bytes, size);
        let mut copied = File::open(to.join(name)).unwrap();
        let actual = copied.metadata().unwrap();
        assert_eq!(actual.len(), size);
        if hole_reported && expected.blocks() * 512 < size / 4 {
            println!(
                "sparse copy {name}: {size} logical bytes, {} -> {} allocated blocks (512 bytes each)",
                expected.blocks(),
                actual.blocks()
            );
            assert!(
                actual.blocks() * 512 < size / 4,
                "sparse source became dense: {} -> {} blocks",
                expected.blocks(),
                actual.blocks()
            );
        }

        let mut input = input;
        input.rewind().unwrap();
        let (mut original, mut clone) = (vec![0; 128 * 1024], vec![0; 128 * 1024]);
        for _ in 0..(size / original.len() as u64) {
            input.read_exact(&mut original).unwrap();
            copied.read_exact(&mut clone).unwrap();
            assert_eq!(original, clone);
        }
        assert_eq!(copied.read(&mut clone).unwrap(), 0);
    }
}

#[test]
fn staged_copy_refuses_moved_parent_and_replaced_source_link() {
    use jwm_file_core::{CopyDestination, CopyError, CopySource};
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    let from = fixture.0.join("from");
    let to = fixture.0.join("to");
    fs::create_dir(&from).unwrap();
    fs::create_dir(&to).unwrap();
    fs::write(from.join("original"), b"keep").unwrap();
    let snapshot = LocalProvider
        .list(
            &Location::Local(from.clone()),
            ListOptions::default(),
            &Cancellation::default(),
        )
        .unwrap();
    let from_handle = snapshot.local_parent.unwrap();
    let identity = snapshot.entries[0].identity.unwrap();
    let to_handle = DirectoryHandle::open(&to).unwrap();
    let run = || {
        to_handle.copy_regular_no_replace(
            CopySource {
                parent: &from_handle,
                displayed_parent: &from,
                name: OsStr::new("original"),
                expected_file: identity,
            },
            CopyDestination {
                displayed_parent: &to,
                name: OsStr::new("output"),
            },
            &Cancellation::default(),
        )
    };
    let elsewhere = from.join("elsewhere");
    fs::rename(from.join("original"), &elsewhere).unwrap();
    symlink(&elsewhere, from.join("original")).unwrap();
    assert!(matches!(run(), Err(CopyError::NotRegular)));
    assert_eq!(fs::read_dir(&to).unwrap().count(), 0);
    fs::remove_file(from.join("original")).unwrap();
    fs::rename(elsewhere, from.join("original")).unwrap();
    let moved = fixture.0.join("moved");
    fs::rename(&to, &moved).unwrap();
    fs::create_dir(&to).unwrap();
    assert!(matches!(run(), Err(CopyError::ParentChanged)));
    assert_eq!(fs::read_dir(&to).unwrap().count(), 0);
    assert_eq!(fs::read_dir(&moved).unwrap().count(), 0);
}

#[test]
fn preview_uses_selected_raw_name_and_refuses_a_replacement_link() {
    use jwm_file_core::{PreviewError, TEXT_PREVIEW_LIMIT};
    use std::os::unix::{ffi::OsStrExt, fs::symlink};
    let fixture = Fixture::new();
    let name = OsStr::from_bytes(b"literal-\x80.tsx");
    let text = "<button>你好 & hello</button>\n";
    let original = fixture.0.join(name);
    fs::write(&original, text).unwrap();
    let listing = LocalProvider
        .list(
            &Location::Local(fixture.0.clone()),
            ListOptions::default(),
            &Cancellation::default(),
        )
        .unwrap();
    let identity = listing.entries[0].identity.unwrap();
    let directory = listing.local_parent.unwrap();
    let read =
        |cancel: &Cancellation| directory.read_text_preview(&fixture.0, name, identity, cancel);

    let result = read(&Cancellation::default()).unwrap();
    assert_eq!(result.text, text);
    assert!(!result.truncated);
    fs::write(&original, "a".repeat(TEXT_PREVIEW_LIMIT + 400)).unwrap();
    let result = read(&Cancellation::default()).unwrap();
    assert!(result.truncated);
    assert_eq!(result.text.len(), TEXT_PREVIEW_LIMIT);

    let cancelled = Cancellation::default();
    cancelled.cancel();
    assert!(matches!(read(&cancelled), Err(PreviewError::Cancelled)));
    let moved = fixture.0.join("previous");
    fs::rename(&original, &moved).unwrap();
    symlink(&moved, &original).unwrap();
    assert!(matches!(
        read(&Cancellation::default()),
        Err(PreviewError::NotRegular)
    ));
}

#[test]
fn png_preview_keeps_native_filename_and_rejects_a_replacement_symlink() {
    use jwm_file_core::PreviewError;
    use std::os::unix::{ffi::OsStrExt, fs::symlink};
    let fixture = Fixture::new();
    let name = OsStr::from_bytes(b"image-\x80.PNG");
    let path = fixture.0.join(name);
    {
        let file = fs::File::create(&path).unwrap();
        let mut encoder = png::Encoder::new(file, 1, 1);
        encoder.set_color(png::ColorType::Rgba);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[1, 2, 3, 4]).unwrap();
        writer.finish().unwrap();
    }
    let cancel = Cancellation::default();
    let snapshot = LocalProvider
        .list(
            &Location::Local(fixture.0.clone()),
            ListOptions::default(),
            &cancel,
        )
        .unwrap();
    let entry = &snapshot.entries[0];
    assert_eq!(entry.name, name);
    let identity = entry.identity.unwrap();
    let parent = snapshot.local_parent.unwrap();
    let read = || parent.read_png_preview(&fixture.0, name, identity, &cancel);
    assert_eq!(read().unwrap().rgba, [1, 2, 3, 4]);
    let moved = fixture.0.join("moved");
    fs::rename(&path, &moved).unwrap();
    symlink(&moved, &path).unwrap();
    assert!(matches!(read(), Err(PreviewError::NotRegular)));
}
