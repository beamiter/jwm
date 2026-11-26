use jwm_file_core::{
    Cancellation, FileKind, FileProvider, ListError, ListOptions, LocalProvider, Location,
};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        loop {
            let dir = std::env::temp_dir().join(format!(
                "jwm-files-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&dir) {
                Ok(()) => return Self(dir),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("create test directory: {e}"),
            }
        }
    }

    fn location(&self) -> Location {
        Location::Local(self.0.clone())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn lists_directories_first_and_keeps_unicode_paths_and_sizes() {
    let root = Fixture::new();
    fs::create_dir(root.0.join("项目")).unwrap();
    fs::write(root.0.join(".hidden"), b"secret").unwrap();
    fs::write(root.0.join("a.txt"), b"abc").unwrap();
    fs::write(root.0.join("备忘录🐕.md"), b"notes").unwrap();
    let snapshot = LocalProvider
        .list(
            &root.location(),
            ListOptions::default(),
            &Cancellation::default(),
        )
        .unwrap();
    assert_eq!(snapshot.location, root.location());
    assert_eq!(snapshot.skipped_entries, 0);
    assert_eq!(
        snapshot
            .entries
            .iter()
            .map(|e| e.name.to_str().unwrap())
            .collect::<Vec<_>>(),
        ["项目", "a.txt", "备忘录🐕.md"]
    );
    assert_eq!(snapshot.entries[0].kind, FileKind::Directory);
    assert_eq!(snapshot.entries[0].bytes, None);
    assert_eq!(snapshot.entries[1].bytes, Some(3));
    assert_eq!(
        snapshot.entries[2].location,
        Location::Local(root.0.join("备忘录🐕.md"))
    );

    let all = LocalProvider
        .list(
            &root.location(),
            ListOptions { show_hidden: true },
            &Cancellation::default(),
        )
        .unwrap();
    assert_eq!(all.entries[1].name, ".hidden");
}

#[cfg(unix)]
#[test]
fn keeps_symlinks_distinct_and_does_not_descend_into_cycles() {
    use std::os::unix::fs::symlink;
    let root = Fixture::new();
    fs::create_dir(root.0.join("dir")).unwrap();
    symlink(".", root.0.join("loop")).unwrap();
    symlink("not-present", root.0.join("broken")).unwrap();
    let snapshot = LocalProvider
        .list(
            &root.location(),
            ListOptions::default(),
            &Cancellation::default(),
        )
        .unwrap();
    assert_eq!(snapshot.entries.len(), 3);
    assert_eq!(snapshot.entries[0].kind, FileKind::Directory);
    for entry in &snapshot.entries[1..] {
        assert_eq!(entry.kind, FileKind::Symlink);
        assert_ne!(entry.identity, snapshot.entries[0].identity);
    }
}

#[cfg(unix)]
#[test]
fn preserves_non_utf8_names_and_hard_link_identity_without_collapsing_rows() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    let root = Fixture::new();
    let bad = OsString::from_vec(vec![b'a', 0xff]);
    fs::write(root.0.join(&bad), b"x").unwrap();
    fs::hard_link(root.0.join(&bad), root.0.join("alias")).unwrap();
    let snapshot = LocalProvider
        .list(
            &root.location(),
            ListOptions::default(),
            &Cancellation::default(),
        )
        .unwrap();
    assert_eq!(snapshot.entries.len(), 2);
    assert!(snapshot.entries.iter().any(|e| e.name == bad));
    assert!(snapshot.entries[0].identity.is_some());
    assert_eq!(snapshot.entries[0].identity, snapshot.entries[1].identity);
    assert_ne!(snapshot.entries[0].location, snapshot.entries[1].location);
}

#[test]
fn distinguishes_cancellation_from_an_os_error() {
    let root = Fixture::new();
    let missing = Location::Local(root.0.join("missing"));
    let cancellation = Cancellation::default();
    cancellation.clone().cancel();
    assert!(matches!(
        LocalProvider.list(&missing, ListOptions::default(), &cancellation),
        Err(ListError::Cancelled)
    ));
    let result = LocalProvider.list(&missing, ListOptions::default(), &Cancellation::default());
    assert!(
        matches!(result, Err(ListError::ReadDirectory { source, .. }) if source.kind() == std::io::ErrorKind::NotFound)
    );
}

#[test]
fn a_large_directory_does_not_silently_truncate_entries() {
    let root = Fixture::new();
    for n in 0..10_000 {
        fs::File::create(root.0.join(format!("entry-{n:05}"))).unwrap();
    }
    let snapshot = LocalProvider
        .list(
            &root.location(),
            ListOptions::default(),
            &Cancellation::default(),
        )
        .unwrap();
    assert_eq!(snapshot.entries.len(), 10_000);
    assert_eq!(snapshot.skipped_entries, 0);
    assert_eq!(snapshot.entries[0].name, "entry-00000");
    assert_eq!(snapshot.entries[9_999].name, "entry-09999");
}
