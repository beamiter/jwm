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
