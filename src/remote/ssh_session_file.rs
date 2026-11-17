//! Open the existing per-user SSH session slot without following links.
//!
//! The trust boundary is other local UIDs, not root or processes running as
//! this user. Keep the legacy pathname: changing it would bypass live locks.
use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};

fn rejected() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "unsafe SSH session lock path or permissions",
    )
}

fn name(value: &std::ffi::OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes()).map_err(|_| rejected())
}

fn open_at(parent: i32, name: &CString, flags: i32, mode: libc::mode_t) -> io::Result<File> {
    // SAFETY: name is terminated; flags and mode are supplied by this module.
    let fd = unsafe { libc::openat(parent, name.as_ptr(), flags | libc::O_CLOEXEC, mode) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a new owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn check_directory(file: &File, uid: u32, final_component: bool) -> io::Result<()> {
    let metadata = file.metadata()?;
    check_directory_metadata(metadata.mode(), metadata.uid(), uid, final_component)
}

fn check_directory_metadata(
    mode: u32,
    owner: u32,
    uid: u32,
    final_component: bool,
) -> io::Result<()> {
    let trusted = owner == uid || (!final_component && owner == 0);
    let shared_sticky = !final_component && mode & libc::S_ISVTX != 0;
    if mode & libc::S_IFMT != libc::S_IFDIR || !trusted || (mode & 0o022 != 0 && !shared_sticky) {
        return Err(rejected());
    }
    Ok(())
}

fn open_directory(path: &Path, uid: u32) -> io::Result<File> {
    if !path.is_absolute() {
        return Err(rejected());
    }
    let components: Vec<_> = path.components().collect();
    let mut directory = open_at(
        libc::AT_FDCWD,
        &CString::new("/").unwrap(),
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        0,
    )?;
    check_directory(&directory, uid, components.len() == 1)?;
    for (index, component) in components.iter().enumerate().skip(1) {
        let Component::Normal(component) = component else {
            return Err(rejected());
        };
        let component = name(component)?;
        let final_component = index + 1 == components.len();
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW;
        let next = match open_at(directory.as_raw_fd(), &component, flags, 0) {
            Ok(file) => file,
            Err(error) if final_component && error.kind() == io::ErrorKind::NotFound => {
                // SAFETY: parent fd is live, and component is a single C string.
                let result =
                    unsafe { libc::mkdirat(directory.as_raw_fd(), component.as_ptr(), 0o700) };
                if result != 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::AlreadyExists {
                        return Err(error);
                    }
                }
                open_at(directory.as_raw_fd(), &component, flags, 0)?
            }
            Err(error) => return Err(error),
        };
        check_directory(&next, uid, final_component)?;
        directory = next;
    }
    Ok(directory)
}

fn check_file(mode: u32, owner: u32, links: u64, uid: u32) -> io::Result<()> {
    if mode & libc::S_IFMT != libc::S_IFREG || owner != uid || mode & 0o022 != 0 || links != 1 {
        return Err(rejected());
    }
    Ok(())
}

fn open_in_directory(directory: &File, filename: &CString, uid: u32) -> io::Result<File> {
    let flags = libc::O_RDWR | libc::O_NOFOLLOW | libc::O_NONBLOCK;
    let file = match open_at(
        directory.as_raw_fd(),
        filename,
        flags | libc::O_CREAT | libc::O_EXCL,
        0o600,
    ) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let mut before = std::mem::MaybeUninit::<libc::stat>::uninit();
            // SAFETY: output points to sufficient storage; fd and C string live.
            if unsafe {
                libc::fstatat(
                    directory.as_raw_fd(),
                    filename.as_ptr(),
                    before.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: fstatat successfully initialized the structure.
            let before = unsafe { before.assume_init() };
            check_file(before.st_mode, before.st_uid, before.st_nlink, uid)?;
            // Reject special objects before opening, then validate the actual fd.
            let file = open_at(directory.as_raw_fd(), filename, flags, 0)?;
            let after = file.metadata()?;
            if before.st_dev != after.dev() || before.st_ino != after.ino() {
                return Err(rejected());
            }
            file
        }
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    check_file(metadata.mode(), metadata.uid(), metadata.nlink(), uid)?;
    Ok(file)
}

pub(super) fn open(path: &Path) -> io::Result<File> {
    // SAFETY: these identity queries have no preconditions.
    let uid = unsafe { libc::geteuid() };
    if uid != unsafe { libc::getuid() } {
        return Err(rejected());
    }
    let parent = path.parent().ok_or_else(rejected)?;
    let filename = name(path.file_name().ok_or_else(rejected)?)?;
    let directory = open_directory(parent, uid)?;
    open_in_directory(&directory, &filename, uid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::PathBuf;

    struct Sandbox(PathBuf);
    impl Sandbox {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "jwm-lock-test-{}-{:016x}",
                std::process::id(),
                rand::random::<u64>()
            ));
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }
        fn lock(&self) -> PathBuf {
            self.0.join("session").join("legacy.lock")
        }
        fn directory(&self) -> PathBuf {
            let path = self.0.join("session");
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            path
        }
    }
    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn new_objects_are_private_and_close_on_exec() {
        let sandbox = Sandbox::new();
        let file = open(&sandbox.lock()).unwrap();
        assert_eq!(file.metadata().unwrap().mode() & 0o777, 0o600);
        assert_eq!(
            fs::metadata(sandbox.lock().parent().unwrap())
                .unwrap()
                .mode()
                & 0o777,
            0o700
        );
        // SAFETY: descriptor is live and F_GETFD takes no third argument.
        assert_ne!(
            unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
    }

    #[test]
    fn existing_normal_permissions_preserve_inode_content_and_flock() {
        // Other parallel tests spawn processes. A fork inherits even CLOEXEC
        // descriptors until exec, temporarily extending a flock's lifetime.
        // Isolate this exact last-holder-drop assertion; do not weaken it.
        const CHILD: &str = "JWM_TEST_ISOLATED_SESSION_FLOCK";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "remote::ssh_session_file::tests::existing_normal_permissions_preserve_inode_content_and_flock", "--nocapture"])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            return;
        }
        let sandbox = Sandbox::new();
        sandbox.directory();
        fs::write(sandbox.lock(), b"existing label").unwrap();
        fs::set_permissions(sandbox.lock(), fs::Permissions::from_mode(0o644)).unwrap();
        let first = open(&sandbox.lock()).unwrap();
        let second = open(&sandbox.lock()).unwrap();
        assert_eq!(
            first.metadata().unwrap().ino(),
            second.metadata().unwrap().ino()
        );
        assert_eq!(fs::read(sandbox.lock()).unwrap(), b"existing label");
        // SAFETY: both descriptors are live, each owns a distinct open description.
        assert_eq!(
            unsafe { libc::flock(first.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        assert_eq!(
            unsafe { libc::flock(second.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            -1
        );
        assert_eq!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock);
        drop(first);
        assert_eq!(
            unsafe { libc::flock(second.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
    }

    #[test]
    fn symlink_and_hardlink_targets_remain_untouched() {
        let sandbox = Sandbox::new();
        sandbox.directory();
        let victim = sandbox.0.join("victim");
        fs::write(&victim, b"do not truncate").unwrap();
        symlink(&victim, sandbox.lock()).unwrap();
        assert!(open(&sandbox.lock()).is_err());
        fs::remove_file(sandbox.lock()).unwrap();
        fs::hard_link(&victim, sandbox.lock()).unwrap();
        assert!(open(&sandbox.lock()).is_err());
        assert_eq!(fs::read(&victim).unwrap(), b"do not truncate");
    }

    #[test]
    fn directory_and_ancestor_links_are_rejected() {
        let sandbox = Sandbox::new();
        let victim = sandbox.0.join("victim");
        fs::create_dir(&victim).unwrap();
        symlink(&victim, sandbox.0.join("session")).unwrap();
        assert!(open(&sandbox.lock()).is_err());
        assert!(open(&sandbox.0.join("session/new/lock")).is_err());
        assert_eq!(fs::read_dir(&victim).unwrap().count(), 0);
    }

    #[test]
    fn writable_directory_and_lock_modes_fail_without_repair() {
        for mode in [0o770, 0o777, 0o1777] {
            let sandbox = Sandbox::new();
            let directory = sandbox.directory();
            fs::set_permissions(&directory, fs::Permissions::from_mode(mode)).unwrap();
            assert!(open(&sandbox.lock()).is_err());
            assert!(!sandbox.lock().exists());
            assert_eq!(fs::metadata(directory).unwrap().mode() & 0o7777, mode);
        }
        for mode in [0o660, 0o666] {
            let sandbox = Sandbox::new();
            sandbox.directory();
            fs::write(sandbox.lock(), b"unchanged").unwrap();
            fs::set_permissions(sandbox.lock(), fs::Permissions::from_mode(mode)).unwrap();
            assert!(open(&sandbox.lock()).is_err());
            assert_eq!(fs::read(sandbox.lock()).unwrap(), b"unchanged");
            assert_eq!(fs::metadata(sandbox.lock()).unwrap().mode() & 0o777, mode);
        }
    }

    #[test]
    fn special_files_are_rejected_without_opening_or_blocking() {
        let sandbox = Sandbox::new();
        sandbox.directory();
        let path = name(sandbox.lock().as_os_str()).unwrap();
        // SAFETY: the path is a terminated string in an isolated test directory.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        assert!(open(&sandbox.lock()).is_err());
        fs::remove_file(sandbox.lock()).unwrap();
        // A socket inode is sufficient: this test must not depend on networking.
        // SAFETY: path is isolated; S_IFSOCK creates no listener or device.
        assert_eq!(
            unsafe { libc::mknod(path.as_ptr(), libc::S_IFSOCK | 0o600, 0) },
            0
        );
        assert!(open(&sandbox.lock()).is_err());
        fs::remove_file(sandbox.lock()).unwrap();
        fs::create_dir(sandbox.lock()).unwrap();
        assert!(open(&sandbox.lock()).is_err());
    }

    #[test]
    fn opened_directory_cannot_be_redirected_by_path_replacement() {
        let sandbox = Sandbox::new();
        let path = sandbox.directory();
        let uid = unsafe { libc::geteuid() };
        let directory = open_directory(&path, uid).unwrap();
        let original = sandbox.0.join("original");
        fs::rename(&path, &original).unwrap();
        let victim = sandbox.0.join("victim");
        fs::create_dir(&victim).unwrap();
        fs::write(victim.join("legacy.lock"), b"untouched").unwrap();
        symlink(&victim, &path).unwrap();
        let mut file =
            open_in_directory(&directory, &CString::new("legacy.lock").unwrap(), uid).unwrap();
        file.write_all(b"original directory").unwrap();
        assert_eq!(fs::read(victim.join("legacy.lock")).unwrap(), b"untouched");
        assert_eq!(
            fs::read(original.join("legacy.lock")).unwrap(),
            b"original directory"
        );
        assert!(open(&sandbox.lock()).is_err());
    }

    #[test]
    fn validation_rejects_foreign_owner_and_unsafe_ancestors() {
        let uid = unsafe { libc::geteuid() };
        assert!(check_file(libc::S_IFREG | 0o600, uid.wrapping_add(1), 1, uid).is_err());
        let sandbox = Sandbox::new();
        let directory = File::open(&sandbox.0).unwrap();
        assert!(check_directory(&directory, uid.wrapping_add(1), true).is_err());
        // Metadata branches for root-owned ancestors; no chown capability needed.
        assert!(check_directory_metadata(libc::S_IFDIR | 0o755, 0, 1000, false).is_ok());
        assert!(check_directory_metadata(libc::S_IFDIR | 0o1777, 0, 1000, false).is_ok());
        assert!(check_directory_metadata(libc::S_IFDIR | 0o777, 0, 1000, false).is_err());
        assert!(check_directory_metadata(libc::S_IFDIR | 0o755, 0, 1000, true).is_err());
        fs::set_permissions(&sandbox.0, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(open(&sandbox.lock()).is_err());
        fs::set_permissions(&sandbox.0, fs::Permissions::from_mode(0o1777)).unwrap();
        assert!(open(&sandbox.lock()).is_ok());
        assert!(open(Path::new("relative/session.lock")).is_err());
        assert!(open(&sandbox.0.join("../session.lock")).is_err());
    }

    #[test]
    fn rejected_opens_do_not_leave_directory_descriptors() {
        let sandbox = Sandbox::new();
        sandbox.directory();
        symlink("missing", sandbox.lock()).unwrap();
        for _ in 0..64 {
            assert!(open(&sandbox.lock()).is_err());
        }
        for fd in fs::read_dir("/proc/self/fd").unwrap().flatten() {
            if let Ok(target) = fs::read_link(fd.path()) {
                assert!(
                    !target.starts_with(&sandbox.0),
                    "leaked descriptor: {target:?}"
                );
            }
        }
    }
}
