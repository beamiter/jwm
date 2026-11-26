#![cfg(target_os = "linux")]

use jwm_file_core::{Cancellation, FileKind, list_home_trash};
use std::{
    fs,
    os::unix::{
        ffi::OsStrExt,
        fs::{PermissionsExt, symlink},
    },
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("jwm-trash-test-{}-{nonce}", std::process::id()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn populate(&self) {
        fs::create_dir(self.0.join("Trash")).unwrap();
        for part in ["files", "info"] {
            fs::create_dir(self.0.join("Trash").join(part)).unwrap();
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn same_path_and_second_remain_two_exact_trash_entries() {
    let data = Fixture::new();
    data.populate();
    let info = b"[Trash Info]\nPath=relative/test%25-%E6%B5%8B%E8%AF%95-%80.txt\nDeletionDate=2026-11-26T12:26:00\n";
    for (name, payload) in [("slot-A", b"old"), ("slot-B", b"new")] {
        fs::write(data.0.join("Trash/files").join(name), payload).unwrap();
        fs::write(
            data.0.join("Trash/info").join(format!("{name}.trashinfo")),
            info,
        )
        .unwrap();
    }
    let result = list_home_trash(&data.0, &Cancellation::default()).unwrap();
    assert_eq!(result.entries.len(), 2);
    assert_eq!(result.entries[0].name, "slot-A");
    assert_eq!(result.entries[1].name, "slot-B");
    assert_ne!(result.entries[0].file, result.entries[1].file);
    assert_eq!(result.entries[0].info, result.entries[1].info);
    let original = &result.entries[0].info.as_ref().unwrap().original;
    assert!(
        original
            .as_os_str()
            .as_bytes()
            .ends_with(b"relative/test%-\xe6\xb5\x8b\xe8\xaf\x95-\x80.txt")
    );
    assert!(!result.incomplete);
    assert_eq!(fs::read(data.0.join("Trash/files/slot-A")).unwrap(), b"old");
    assert_eq!(fs::read(data.0.join("Trash/files/slot-B")).unwrap(), b"new");
}

#[test]
fn lists_orphans_as_unknown_without_following_info_or_payload_links() {
    let data = Fixture::new();
    data.populate();
    let original = data.0.join("outside");
    fs::write(
        &original,
        b"[Trash Info]\nPath=/would-be-misread\nDeletionDate=2026-11-26T12:26:00",
    )
    .unwrap();
    symlink(&original, data.0.join("Trash/info/link.trashinfo")).unwrap();
    symlink(&original, data.0.join("Trash/files/link")).unwrap();
    fs::write(data.0.join("Trash/files/orphan"), b"retained").unwrap();

    let result = list_home_trash(&data.0, &Cancellation::default()).unwrap();
    assert_eq!(result.entries.len(), 2);
    assert_eq!(result.entries[0].name, "link");
    assert_eq!(result.entries[0].kind, FileKind::Symlink);
    assert!(result.entries[0].info.is_none());
    assert_eq!(result.entries[1].name, "orphan");
    assert!(result.entries[1].info.is_none());
}

#[test]
fn refuses_a_link_or_shared_writable_trash_directory_and_supports_cancellation() {
    let data = Fixture::new();
    let absent = list_home_trash(&data.0, &Cancellation::default()).unwrap();
    assert!(absent.entries.is_empty());
    fs::create_dir(data.0.join("external")).unwrap();
    symlink(data.0.join("external"), data.0.join("Trash")).unwrap();
    assert!(list_home_trash(&data.0, &Cancellation::default()).is_err());
    fs::remove_file(data.0.join("Trash")).unwrap();
    data.populate();
    fs::set_permissions(data.0.join("Trash/info"), fs::Permissions::from_mode(0o777)).unwrap();
    assert_eq!(
        list_home_trash(&data.0, &Cancellation::default())
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::PermissionDenied
    );
    let cancel = Cancellation::default();
    cancel.cancel();
    assert_eq!(
        list_home_trash(&data.0, &cancel).unwrap_err().kind(),
        std::io::ErrorKind::Interrupted
    );
}

#[test]
fn trash_snapshot_bounds_enumeration_and_reports_incomplete_results() {
    let data = Fixture::new();
    data.populate();
    for index in 0..=jwm_file_core::HOME_TRASH_INSPECT_LIMIT {
        fs::File::create_new(data.0.join("Trash/files").join(format!("slot-{index:05}"))).unwrap();
    }
    let snapshot = list_home_trash(&data.0, &Cancellation::default()).unwrap();
    assert_eq!(
        snapshot.entries.len(),
        jwm_file_core::HOME_TRASH_INSPECT_LIMIT
    );
    assert!(snapshot.incomplete);
    assert!(snapshot.entries.iter().all(|row| row.info.is_none()));
}

#[test]
fn recovery_uses_exact_trash_slot_and_keeps_both_revisions_on_conflict_and_success() {
    use jwm_file_core::{CopyError, RecoverCopyError, recover_home_trash_copy};
    use std::ffi::OsStr;
    let data = Fixture::new();
    data.populate();
    fs::create_dir(data.0.join("working")).unwrap();
    let name = OsStr::from_bytes(b"doc-%-\x80");
    let target = data.0.join("working").join(name);
    let info = b"[Trash Info]\nPath=working/doc-%25-%80\nDeletionDate=2026-11-26T12:26:00";
    for (slot, bytes) in [("A", b"older"), ("B", b"newer")] {
        fs::write(data.0.join("Trash/files").join(slot), bytes).unwrap();
        fs::write(
            data.0.join("Trash/info").join(format!("{slot}.trashinfo")),
            info,
        )
        .unwrap();
    }
    let snapshot = list_home_trash(&data.0, &Cancellation::default()).unwrap();
    let selected = snapshot.entries.iter().find(|e| e.name == "B").unwrap();
    fs::write(&target, b"occupied").unwrap();
    assert!(matches!(
        recover_home_trash_copy(&data.0, selected, &Cancellation::default()),
        Err(RecoverCopyError::Copy(CopyError::DestinationExists))
    ));
    assert_eq!(fs::read(&target).unwrap(), b"occupied");
    assert_eq!(fs::read(data.0.join("Trash/files/A")).unwrap(), b"older");
    assert_eq!(fs::read(data.0.join("Trash/files/B")).unwrap(), b"newer");
    fs::remove_file(&target).unwrap();
    let receipt = recover_home_trash_copy(&data.0, selected, &Cancellation::default()).unwrap();
    assert_eq!(receipt.bytes, 5);
    assert_eq!(fs::read(&target).unwrap(), b"newer");
    assert_eq!(fs::read(data.0.join("Trash/files/A")).unwrap(), b"older");
    assert_eq!(fs::read(data.0.join("Trash/files/B")).unwrap(), b"newer");
    assert_eq!(
        fs::read(data.0.join("Trash/info/A.trashinfo")).unwrap(),
        info
    );
    assert_eq!(
        fs::read(data.0.join("Trash/info/B.trashinfo")).unwrap(),
        info
    );
}

#[test]
fn recovery_refuses_changed_slot_or_symlinked_destination_parent() {
    use jwm_file_core::{RecoverCopyError, recover_home_trash_copy};
    let data = Fixture::new();
    data.populate();
    fs::create_dir(data.0.join("outside")).unwrap();
    symlink(data.0.join("outside"), data.0.join("via-link")).unwrap();
    let source = data.0.join("Trash/files/slot");
    fs::write(&source, b"untouched").unwrap();
    fs::write(
        data.0.join("Trash/info/slot.trashinfo"),
        b"[Trash Info]\nPath=via-link/file\nDeletionDate=2026-11-26T12:26:00",
    )
    .unwrap();
    let snapshot = list_home_trash(&data.0, &Cancellation::default()).unwrap();
    let selected = &snapshot.entries[0];
    assert!(matches!(
        recover_home_trash_copy(&data.0, selected, &Cancellation::default()),
        Err(RecoverCopyError::Io(_))
    ));
    assert!(!data.0.join("outside/file").exists());
    fs::write(&source, b"replaced bytes").unwrap();
    assert!(matches!(
        recover_home_trash_copy(&data.0, selected, &Cancellation::default()),
        Err(RecoverCopyError::EntryChanged)
    ));
    assert!(!data.0.join("outside/file").exists());
}
