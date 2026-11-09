use super::*;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "jwm-budget-test-{}-{:032x}",
            std::process::id(),
            rand::random::<u128>()
        ));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
    fn root(&self) -> PathBuf {
        self.0.join("root")
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn private_file(path: &Path, bytes: &[u8]) {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(bytes).unwrap();
}
// Publish result payloads atomically: exists() must mean the complete result
// is readable, not merely that create_new ran before write_all.
fn publish_result(path: &Path, bytes: &[u8]) {
    let staged = path.with_extension("pending");
    private_file(&staged, bytes);
    fs::rename(&staged, path).unwrap();
}

#[test]
fn staged_result_is_invisible_until_complete_publication() {
    let temp = Temp::new();
    let final_path = temp.0.join("result");
    let staged = final_path.with_extension("pending");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&staged)
        .unwrap();
    file.write_all(b"y").unwrap();
    assert!(!final_path.exists());
    file.write_all(b"es").unwrap();
    drop(file);
    assert!(!final_path.exists());
    fs::rename(&staged, &final_path).unwrap();
    assert_eq!(fs::read(&final_path).unwrap(), b"yes");
    let helper_path = temp.0.join("helper-result");
    publish_result(&helper_path, b"no");
    assert_eq!(fs::read(&helper_path).unwrap(), b"no");
    assert!(!helper_path.with_extension("pending").exists());
}

fn private_dir(path: &Path) {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .unwrap();
}
fn fifo(path: &Path) {
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: path is a valid terminated C string and mkfifo retains no pointer.
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
}
fn assert_controls_fail(root: &Path) {
    assert!(status(root).is_err());
    assert!(set_limit(root, 100).is_err());
    assert!(disable(root).is_err());
    assert!(Session::create(root).is_err());
}

#[test]
fn exact_cap_and_overflow_are_atomic() {
    let temp = Temp::new();
    let root = temp.root();
    set_limit(&root, 10).unwrap();
    let session = Session::create(&root).unwrap();
    assert!(session.reserve(1, 10).unwrap().is_dir());
    assert_eq!(
        session.reserve(2, 1).unwrap_err().kind(),
        io::ErrorKind::StorageFull
    );
    assert!(!session.path().join("2").exists());
    assert_eq!(status(&root).unwrap().reserved_bytes, 10);
    session.delete(1).unwrap();
    disable(&root).unwrap();
    session.reserve(2, u64::MAX).unwrap();
    assert!(session.reserve(3, 1).is_err());
    assert!(!session.path().join("3").exists());
    assert_eq!(status(&root).unwrap().reserved_bytes, u64::MAX);
}

#[test]
fn disabled_tracking_can_enable_with_live_sessions_without_losing_files() {
    let temp = Temp::new();
    let root = temp.root();
    let first = Session::create(&root).unwrap();
    let second = Session::create(&root).unwrap();
    let copy = first.reserve(1, 40).unwrap();
    private_file(&copy.join("payload"), b"unchanged");
    second.reserve(1, 60).unwrap();
    assert_eq!(
        status(&root).unwrap(),
        Status {
            initialized: true,
            limit_bytes: None,
            reserved_bytes: 100,
            reservations: 2
        }
    );
    let before = fs::read(root.join(LEDGER)).unwrap();
    assert!(set_limit(&root, 99).is_err());
    assert!(set_limit(&root, 0).is_err());
    assert_eq!(fs::read(root.join(LEDGER)).unwrap(), before);
    set_limit(&root, 100).unwrap();
    assert!(second.reserve(2, 1).is_err());
    let disabled = disable(&root).unwrap();
    assert_eq!(disabled.reserved_bytes, 100);
    assert_eq!(disabled.limit_bytes, None);
    assert_eq!(fs::read(copy.join("payload")).unwrap(), b"unchanged");
    second.reserve(2, 1).unwrap();
    first.remove_if_empty().unwrap();
    assert!(first.path().exists());
    first.delete(1).unwrap();
    first.remove_if_empty().unwrap();
    assert!(!first.path().exists());
    assert_eq!(status(&root).unwrap().reserved_bytes, 61);
}

#[test]
fn status_and_disable_do_not_initialize_missing_root() {
    let temp = Temp::new();
    assert!(!status(&temp.root()).unwrap().initialized);
    assert!(!disable(&temp.root()).unwrap().initialized);
    assert!(!temp.root().exists());
}

#[test]
fn missing_malformed_and_unsafe_metadata_fail_closed() {
    for name in [MARKER, LEDGER] {
        for damage in [
            "missing",
            "malformed",
            "symlink",
            "hardlink",
            "fifo",
            "public",
            "oversized",
        ] {
            let temp = Temp::new();
            let root = temp.root();
            set_limit(&root, 100).unwrap();
            let path = root.join(name);
            let original = fs::read(&path).unwrap();
            fs::remove_file(&path).unwrap();
            match damage {
                "missing" => {}
                "malformed" => private_file(&path, b"invalid"),
                "symlink" => {
                    let target = temp.0.join("target");
                    private_file(&target, &original);
                    symlink(target, &path).unwrap();
                }
                "hardlink" => {
                    let target = temp.0.join("target");
                    private_file(&target, &original);
                    fs::hard_link(target, &path).unwrap();
                }
                "fifo" => fifo(&path),
                "public" => {
                    private_file(&path, &original);
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
                }
                "oversized" => private_file(&path, &vec![b'x'; MAX_METADATA_BYTES + 1]),
                _ => unreachable!(),
            }
            assert_controls_fail(&root);
        }
    }
}

#[test]
fn malformed_ledger_schema_and_arithmetic_fail_closed() {
    for damage in [
        "version",
        "overflow",
        "duplicate",
        "zero-copy",
        "zero-limit",
        "over-limit",
        "root",
    ] {
        let temp = Temp::new();
        let root = temp.root();
        let session = Session::create(&root).unwrap();
        session.reserve(1, 10).unwrap();
        let mut ledger: Ledger =
            serde_json::from_slice(&fs::read(root.join(LEDGER)).unwrap()).unwrap();
        match damage {
            "version" => ledger.version = 2,
            "overflow" => {
                ledger.sessions[0].copies[0].bytes = u64::MAX;
                ledger.sessions[0].copies.push(Reservation {
                    copy: 2,
                    bytes: 1,
                    directory: None,
                });
            }
            "duplicate" => {
                let duplicate = ledger.sessions[0].copies[0].clone();
                ledger.sessions[0].copies.push(duplicate);
            }
            "zero-copy" => ledger.sessions[0].copies[0].copy = 0,
            "zero-limit" => ledger.limit_bytes = Some(0),
            "over-limit" => ledger.limit_bytes = Some(9),
            "root" => ledger.root.inode ^= 1,
            _ => unreachable!(),
        }
        fs::write(root.join(LEDGER), serde_json::to_vec(&ledger).unwrap()).unwrap();
        assert_controls_fail(&root);
    }
}

fn legacy(temp: &Temp, pid: u32) -> PathBuf {
    let copy = temp.root().join(format!("s{pid}-12345678")).join("1");
    private_dir(&copy);
    copy
}
fn dead_pid() -> u32 {
    // Linux PIDs are signed ints, so this parsed u32 cannot name a process.
    let pid = u32::MAX;
    assert!(definitely_exited(pid));
    pid
}

#[test]
fn inactive_legacy_import_counts_archive_headers_and_relative_names() {
    let temp = Temp::new();
    let copy = legacy(&temp, dead_pid());
    private_file(&copy.join("a"), b"abc");
    private_dir(&copy.join("dir"));
    private_file(&copy.join("dir/b"), b"12345");
    let expected = (23 + 1 + 3) + (23 + 3) + (23 + 5 + 5);
    let before = status(&temp.root()).unwrap();
    assert!(!before.initialized);
    assert_eq!(before.reserved_bytes, expected);
    assert!(!temp.root().join(MARKER).exists());
    assert_eq!(
        set_limit(&temp.root(), expected).unwrap().reserved_bytes,
        expected
    );
    assert_eq!(fs::read(copy.join("a")).unwrap(), b"abc");
}

#[test]
fn live_legacy_writer_refuses_activation_but_uncapped_writers_continue() {
    let temp = Temp::new();
    let copy = legacy(&temp, std::process::id());
    private_file(&copy.join("a"), b"retain");
    assert!(status(&temp.root()).is_err());
    assert!(set_limit(&temp.root(), 100).is_err());
    assert!(!temp.root().join(MARKER).exists());
    let session = Session::create(&temp.root()).unwrap();
    session.reserve(1, 20).unwrap();
    let before = fs::read(temp.root().join(LEDGER)).unwrap();
    assert!(status(&temp.root()).is_err());
    assert!(set_limit(&temp.root(), 100).is_err());
    assert_eq!(fs::read(temp.root().join(LEDGER)).unwrap(), before);
    session.reserve(2, 30).unwrap();
    let ledger: Ledger =
        serde_json::from_slice(&fs::read(temp.root().join(LEDGER)).unwrap()).unwrap();
    assert!(ledger.legacy_pending);
    assert_eq!(ledger.status(true).unwrap().reserved_bytes, 50);
    assert_eq!(fs::read(copy.join("a")).unwrap(), b"retain");
    fs::remove_dir_all(copy.parent().unwrap()).unwrap();
    set_limit(&temp.root(), 100).unwrap();
    disable(&temp.root()).unwrap();
    let ledger: Ledger =
        serde_json::from_slice(&fs::read(temp.root().join(LEDGER)).unwrap()).unwrap();
    assert!(!ledger.legacy_pending);
    // Disabling an activated root must not reopen legacy acceptance.
    legacy(&temp, std::process::id());
    assert_controls_fail(&temp.root());
}

#[test]
fn unsafe_legacy_entries_are_not_followed_or_imported() {
    for kind in [
        "symlink",
        "hardlink",
        "fifo",
        "copy-symlink",
        "session-symlink",
    ] {
        let temp = Temp::new();
        let copy = legacy(&temp, dead_pid());
        let target = temp.0.join("outside");
        private_file(&target, b"outside remains");
        match kind {
            "symlink" => symlink(&target, copy.join("a")).unwrap(),
            "hardlink" => fs::hard_link(&target, copy.join("a")).unwrap(),
            "fifo" => fifo(&copy.join("a")),
            "copy-symlink" => {
                fs::remove_dir(&copy).unwrap();
                symlink(&temp.0, &copy).unwrap();
            }
            "session-symlink" => {
                fs::remove_dir(&copy).unwrap();
                let session = copy.parent().unwrap();
                fs::remove_dir(session).unwrap();
                symlink(&temp.0, session).unwrap();
            }
            _ => unreachable!(),
        }
        assert_controls_fail(&temp.root());
        assert_eq!(fs::read(target).unwrap(), b"outside remains");
    }
}

#[test]
fn unsafe_root_and_symlink_ancestor_are_rejected() {
    let temp = Temp::new();
    private_dir(&temp.root());
    fs::set_permissions(temp.root(), fs::Permissions::from_mode(0o755)).unwrap();
    assert_controls_fail(&temp.root());
    fs::set_permissions(temp.root(), fs::Permissions::from_mode(0o700)).unwrap();
    let link = temp.0.join("link");
    symlink(temp.root(), &link).unwrap();
    assert_controls_fail(&link);
    assert!(Session::create(&link.join("child")).is_err());
    assert!(!temp.root().join("child").exists());
}

#[test]
fn pinned_session_refuses_replaced_root() {
    let temp = Temp::new();
    let root = temp.root();
    let session = Session::create(&root).unwrap();
    session.reserve(1, 20).unwrap();
    let moved = temp.0.join("moved");
    fs::rename(&root, &moved).unwrap();
    set_limit(&root, 100).unwrap();
    assert!(session.reserve(2, 1).is_err());
    assert!(session.delete(1).is_err());
    assert!(session.remove_if_empty().is_err());
    assert_eq!(status(&root).unwrap().reserved_bytes, 0);
    assert!(moved.join(&session.name).join("1").exists());
}

#[test]
fn durable_reservation_without_mkdir_stays_charged() {
    let temp = Temp::new();
    let root = temp.root();
    let session = Session::create(&root).unwrap();
    {
        let store = LockedRoot::open(&root, false, None).unwrap();
        let mut ledger = store.load().unwrap().unwrap();
        ledger.sessions[0].copies.push(Reservation {
            copy: 1,
            bytes: 50,
            directory: None,
        });
        store.save(&ledger).unwrap();
    }
    assert!(!session.path().join("1").exists());
    assert_eq!(status(&root).unwrap().reserved_bytes, 50);
    assert!(set_limit(&root, 49).is_err());
    set_limit(&root, 50).unwrap();
    assert!(session.reserve(2, 1).is_err());
    session.delete(1).unwrap();
    assert_eq!(status(&root).unwrap().reserved_bytes, 0);
}

#[test]
fn failed_deletion_preserves_charge_and_external_target() {
    let temp = Temp::new();
    let root = temp.root();
    let session = Session::create(&root).unwrap();
    let copy = session.reserve(1, 50).unwrap();
    let target = temp.0.join("target");
    private_file(&target, b"retained");
    symlink(&target, copy.join("unsafe")).unwrap();
    assert!(session.delete(1).is_err());
    assert_eq!(status(&root).unwrap().reserved_bytes, 50);
    assert_eq!(fs::read(&target).unwrap(), b"retained");
    fs::remove_file(copy.join("unsafe")).unwrap();
    session.delete(1).unwrap();
    assert_eq!(status(&root).unwrap().reserved_bytes, 0);
}

struct ChildGuard(Child);
impl ChildGuard {
    fn wait(&mut self) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "budget child exceeded deadline");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}
fn child(temp: &Temp, mode: &str, tag: &str) -> ChildGuard {
    let module = module_path!().split_once("::").unwrap().1;
    ChildGuard(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("{module}::subprocess_helper"),
                "--nocapture",
            ])
            .env("JWM_STAGING_TEST_ROOT", temp.root())
            .env("JWM_STAGING_TEST_CONTROL", &temp.0)
            .env("JWM_STAGING_TEST_MODE", mode)
            .env("JWM_STAGING_TEST_TAG", tag)
            .stdin(Stdio::null())
            .spawn()
            .unwrap(),
    )
}
fn wait_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn retry_busy<T>(mut operation: impl FnMut() -> io::Result<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match operation() {
            Ok(value) => return value,
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(5))
            }
            Err(error) => panic!("operation failed: {error}"),
        }
    }
}

#[test]
fn subprocess_helper() {
    let Ok(mode) = std::env::var("JWM_STAGING_TEST_MODE") else {
        return;
    };
    let root = PathBuf::from(std::env::var_os("JWM_STAGING_TEST_ROOT").unwrap());
    let control = PathBuf::from(std::env::var_os("JWM_STAGING_TEST_CONTROL").unwrap());
    let tag = std::env::var("JWM_STAGING_TEST_TAG").unwrap();
    if mode == "lock" {
        let _lock = LockedRoot::open(&root, false, None).unwrap();
        private_file(&control.join(format!("ready-{tag}")), b"");
        wait_file(&control.join("release"));
        return;
    }
    let session = retry_busy(|| Session::create(&root));
    if mode == "crash-before-mkdir" {
        let store = LockedRoot::open(&root, false, None).unwrap();
        let mut ledger = store.load().unwrap().unwrap();
        ledger
            .sessions
            .iter_mut()
            .find(|s| s.name == session.name)
            .unwrap()
            .copies
            .push(Reservation {
                copy: 1,
                bytes: 60,
                directory: None,
            });
        store.save(&ledger).unwrap();
        std::process::exit(0);
    }
    if matches!(
        mode.as_str(),
        "crash-after-mkdir" | "crash-after-payload" | "crash-after-unlink" | "crash-after-delete"
    ) {
        let copy = session.reserve(1, 60).unwrap();
        if mode != "crash-after-mkdir" {
            private_file(&copy.join("payload"), b"complete payload");
            File::open(copy.join("payload"))
                .unwrap()
                .sync_all()
                .unwrap();
            open_directory(&copy).unwrap().sync_all().unwrap();
        }
        if mode == "crash-after-unlink" {
            let identity = Identity::file(&open_directory(&copy).unwrap()).unwrap();
            remove_tree(&session.directory, "1", Some(identity), &mut 0).unwrap();
            session.directory.sync_all().unwrap();
            verify_absent(&copy).unwrap();
        }
        if mode == "crash-after-delete" {
            session.delete(1).unwrap();
            assert!(!copy.exists());
        }
        std::process::exit(0);
    }
    if mode == "crash-after-save-failure" {
        let store = LockedRoot::open(&root, false, None).unwrap();
        let mut ledger = store.load().unwrap().unwrap();
        ledger
            .sessions
            .iter_mut()
            .find(|s| s.name == session.name)
            .unwrap()
            .copies
            .push(Reservation {
                copy: 1,
                bytes: 60,
                directory: None,
            });
        // Simulate the exact reservation commit before mkdir. An occupied
        // directory at the atomic-rename destination deterministically fails
        // save, including when these tests run as root, without chmod or
        // process-global resource-limit changes.
        let metadata = root.join(LEDGER);
        let backup = control.join("saved-ledger");
        fs::rename(&metadata, &backup).unwrap();
        private_dir(&metadata);
        private_file(&metadata.join("occupied"), b"do not replace");
        assert!(store.save(&ledger).is_err());
        assert!(!session.path().join("1").exists());
        // Restore the original committed state before exiting, so the parent
        // can distinguish a rejected reservation from corrupt accounting.
        fs::remove_file(metadata.join("occupied")).unwrap();
        fs::remove_dir(&metadata).unwrap();
        fs::rename(&backup, &metadata).unwrap();
        store.directory.sync_all().unwrap();
        assert_eq!(
            store
                .load()
                .unwrap()
                .unwrap()
                .status(true)
                .unwrap()
                .reserved_bytes,
            0
        );
        std::process::exit(0);
    }
    assert_eq!(mode, "compete");
    private_file(&control.join(format!("ready-{tag}")), b"");
    wait_file(&control.join("go"));
    let admitted = retry_busy(|| match session.reserve(1, 60) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::StorageFull => Ok(false),
        Err(error) => Err(error),
    });
    publish_result(
        &control.join(format!("result-{tag}")),
        if admitted { b"yes" } else { b"no" },
    );
    wait_file(&control.join("release"));
}

#[test]
fn inherited_descriptor_cannot_extend_a_finished_root_transaction() {
    let temp = Temp::new();
    set_limit(&temp.root(), 100).unwrap();
    let transaction = LockedRoot::open(&temp.root(), false, None).unwrap();
    // dup, like fork, keeps the same open-file description alive. This models
    // an unrelated process spawned while another thread owns the transaction,
    // without depending on the small fork-to-exec scheduling window.
    let inherited = transaction.directory.try_clone().unwrap();
    assert_eq!(
        status(&temp.root()).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    drop(transaction);
    assert_eq!(status(&temp.root()).unwrap().limit_bytes, Some(100));
    // The old duplicate must also not unlock a later independent transaction.
    let next = LockedRoot::open(&temp.root(), false, None).unwrap();
    drop(inherited);
    assert_eq!(
        status(&temp.root()).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    drop(next);
    assert!(status(&temp.root()).is_ok());
}

#[test]
fn root_lock_contention_is_bounded_and_does_not_mutate() {
    let temp = Temp::new();
    set_limit(&temp.root(), 100).unwrap();
    let before = fs::read(temp.root().join(LEDGER)).unwrap();
    let mut process = child(&temp, "lock", "lock");
    wait_file(&temp.0.join("ready-lock"));
    assert_eq!(
        status(&temp.root()).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    assert_eq!(
        set_limit(&temp.root(), 200).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    assert!(Session::create(&temp.root()).is_err());
    assert_eq!(fs::read(temp.root().join(LEDGER)).unwrap(), before);
    private_file(&temp.0.join("release"), b"");
    assert!(process.wait().success());
    assert_eq!(status(&temp.root()).unwrap().limit_bytes, Some(100));
}

#[test]
fn competing_process_admissions_never_exceed_cap() {
    let temp = Temp::new();
    set_limit(&temp.root(), 100).unwrap();
    let mut first = child(&temp, "compete", "a");
    let mut second = child(&temp, "compete", "b");
    wait_file(&temp.0.join("ready-a"));
    wait_file(&temp.0.join("ready-b"));
    private_file(&temp.0.join("go"), b"");
    wait_file(&temp.0.join("result-a"));
    wait_file(&temp.0.join("result-b"));
    let results = [
        fs::read(temp.0.join("result-a")).unwrap(),
        fs::read(temp.0.join("result-b")).unwrap(),
    ];
    for (tag, result) in ["a", "b"].into_iter().zip(&results) {
        assert!(
            matches!(result.as_slice(), b"yes" | b"no"),
            "child {tag} published an invalid admission result: {result:?}"
        );
    }
    assert_eq!(results.iter().filter(|v| v.as_slice() == b"yes").count(), 1);
    assert_eq!(results.iter().filter(|v| v.as_slice() == b"no").count(), 1);
    assert_eq!(status(&temp.root()).unwrap().reserved_bytes, 60);
    private_file(&temp.0.join("release"), b"");
    assert!(first.wait().success());
    assert!(second.wait().success());
    assert_eq!(status(&temp.root()).unwrap().reserved_bytes, 60);
}

#[test]
fn abrupt_process_exit_keeps_reservations_until_verified_cleanup() {
    for mode in [
        "crash-before-mkdir",
        "crash-after-mkdir",
        "crash-after-payload",
        "crash-after-unlink",
    ] {
        let temp = Temp::new();
        set_limit(&temp.root(), 100).unwrap();
        let mut process = child(&temp, mode, "crash");
        assert!(process.wait().success());
        assert_eq!(status(&temp.root()).unwrap().reserved_bytes, 60);
        let ledger: Ledger =
            serde_json::from_slice(&fs::read(temp.root().join(LEDGER)).unwrap()).unwrap();
        let copy = temp.root().join(&ledger.sessions[0].name).join("1");
        if mode == "crash-after-payload" {
            assert_eq!(fs::read(copy.join("payload")).unwrap(), b"complete payload");
        }
        if matches!(mode, "crash-before-mkdir" | "crash-after-unlink") {
            assert!(!copy.exists());
        }
        assert!(set_limit(&temp.root(), 59).is_err());
        assert_eq!(disable(&temp.root()).unwrap().reserved_bytes, 60);
        let session = Session::create(&temp.root()).unwrap();
        assert_eq!(status(&temp.root()).unwrap().reserved_bytes, 0);
        session.remove_if_empty().unwrap();
    }
}

#[test]
fn abrupt_exit_after_committed_delete_or_failed_reservation_has_no_charge() {
    for mode in ["crash-after-delete", "crash-after-save-failure"] {
        let temp = Temp::new();
        set_limit(&temp.root(), 100).unwrap();
        let mut process = child(&temp, mode, "released");
        assert!(process.wait().success());
        let snapshot = status(&temp.root()).unwrap();
        assert_eq!(snapshot.reserved_bytes, 0);
        assert_eq!(snapshot.reservations, 0);
        assert_eq!(snapshot.limit_bytes, Some(100));
        let ledger: Ledger =
            serde_json::from_slice(&fs::read(temp.root().join(LEDGER)).unwrap()).unwrap();
        assert!(
            !temp
                .root()
                .join(&ledger.sessions[0].name)
                .join("1")
                .exists()
        );
    }
}

#[test]
fn unknown_fields_are_rejected_at_every_ledger_level() {
    for level in [
        "ledger",
        "root",
        "session",
        "session-directory",
        "reservation",
        "copy-directory",
    ] {
        let temp = Temp::new();
        let root = temp.root();
        let session = Session::create(&root).unwrap();
        session.reserve(1, 1).unwrap();
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join(LEDGER)).unwrap()).unwrap();
        let target = match level {
            "ledger" => &mut value,
            "root" => &mut value["root"],
            "session" => &mut value["sessions"][0],
            "session-directory" => &mut value["sessions"][0]["directory"],
            "reservation" => &mut value["sessions"][0]["copies"][0],
            "copy-directory" => &mut value["sessions"][0]["copies"][0]["directory"],
            _ => unreachable!(),
        };
        target
            .as_object_mut()
            .unwrap()
            .insert("unknown_future_field".into(), serde_json::Value::Bool(true));
        fs::write(root.join(LEDGER), serde_json::to_vec(&value).unwrap()).unwrap();
        assert_controls_fail(&root);
    }
}

#[test]
fn ledger_session_and_reservation_bounds_accept_exactly_the_limit() {
    let identity = Identity {
        device: 1,
        inode: 1,
    };
    let mut ledger = Ledger {
        version: 1,
        root: identity,
        limit_bytes: None,
        legacy_pending: false,
        sessions: (0..MAX_SESSIONS)
            .map(|index| SessionRecord {
                name: format!("s1-{index:08x}"),
                directory: identity,
                copies: Vec::new(),
            })
            .collect(),
    };
    assert!(ledger.status(true).is_ok());
    ledger.sessions.push(SessionRecord {
        name: format!("s1-{:08x}", MAX_SESSIONS),
        directory: identity,
        copies: Vec::new(),
    });
    assert!(ledger.status(true).is_err());
    ledger.sessions.truncate(1);
    ledger.sessions[0].copies = (1..=MAX_RESERVATIONS)
        .map(|copy| Reservation {
            copy: u32::try_from(copy).unwrap(),
            bytes: 1,
            directory: None,
        })
        .collect();
    assert_eq!(ledger.status(true).unwrap().reservations, MAX_RESERVATIONS);
    ledger.sessions[0].copies.push(Reservation {
        copy: u32::try_from(MAX_RESERVATIONS + 1).unwrap(),
        bytes: 1,
        directory: None,
    });
    assert!(ledger.status(true).is_err());
}

#[test]
fn root_inventory_limit_is_bounded_without_importing_entries() {
    let temp = Temp::new();
    private_dir(&temp.root());
    let store = LockedRoot::open(&temp.root(), false, None).unwrap();
    for index in 0..MAX_ROOT_ENTRIES {
        // root_names validates the grammar but deliberately leaves types to
        // its callers. Cheap private files suffice to exercise its bound.
        private_file(&temp.root().join(format!("s1-{index:08x}")), b"");
    }
    assert_eq!(store.root_names().unwrap().len(), MAX_ROOT_ENTRIES);
    private_file(
        &temp.root().join(format!("s1-{:08x}", MAX_ROOT_ENTRIES)),
        b"",
    );
    assert!(store.root_names().is_err());
}

#[test]
fn tree_depth_entry_and_relative_name_limits_fail_closed() {
    let temp = Temp::new();
    private_dir(&temp.root());
    let directory = open_directory(&temp.root()).unwrap();
    assert_eq!(
        inspect_tree(&directory, MAX_TREE_DEPTH, 0, &mut 0).unwrap(),
        0
    );
    assert!(inspect_tree(&directory, MAX_TREE_DEPTH + 1, 0, &mut 0).is_err());
    private_file(&temp.root().join("x"), b"");
    let mut scanned = MAX_TREE_ENTRIES - 1;
    assert_eq!(inspect_tree(&directory, 0, 0, &mut scanned).unwrap(), 24);
    assert_eq!(scanned, MAX_TREE_ENTRIES);
    assert!(inspect_tree(&directory, 0, 0, &mut scanned).is_err());
    assert_eq!(
        inspect_tree(&directory, 0, MAX_RELATIVE_BYTES - 1, &mut 0).unwrap(),
        23 + MAX_RELATIVE_BYTES as u64
    );
    assert!(inspect_tree(&directory, 0, MAX_RELATIVE_BYTES, &mut 0).is_err());
    fs::remove_file(temp.root().join("x")).unwrap();
    let mut path = temp.root();
    for _ in 0..MAX_TREE_DEPTH {
        path.push("d");
        private_dir(&path);
    }
    assert!(inspect_tree(&directory, 0, 0, &mut 0).is_ok());
    path.push("d");
    private_dir(&path);
    assert!(inspect_tree(&directory, 0, 0, &mut 0).is_err());
}
