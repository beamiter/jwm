//! Private, bounded atomic storage for an explicit Files tab snapshot.
use crate::DirectoryHandle;
use rustix::{
    fd::OwnedFd,
    fs::{self, AtFlags, FileType, Mode, OFlags},
    io::Errno,
    process::geteuid,
};
use std::{
    fs::File,
    io::{self, ErrorKind, Read, Write},
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

const DIRECTORY: &str = "jwm-files";
const RECORD: &str = "tabs-v1";
pub const FILES_TAB_STATE_CAP: usize = 8 + 2 + 16 * (2 + 4096);
static NEXT: AtomicUsize = AtomicUsize::new(0);

fn unsafe_state() -> io::Error {
    io::Error::new(
        ErrorKind::PermissionDenied,
        "Files session state is not private",
    )
}

fn owned_state_parent(dir: DirectoryHandle) -> io::Result<DirectoryHandle> {
    let stat = fs::fstat(&dir.fd)?;
    if stat.st_uid != geteuid().as_raw() || stat.st_mode & 0o022 != 0 {
        return Err(unsafe_state());
    }
    Ok(dir)
}

/// Resolve the app's state parent without following directory symlinks.
/// An explicit XDG root must already exist. Only an explicit save may
/// create the two default components, relative to an owned, pinned home.
pub fn open_files_state_root(
    home: Option<&Path>,
    configured: Option<&Path>,
    for_save: bool,
) -> io::Result<DirectoryHandle> {
    if let Some(root) = configured.filter(|path| path.is_absolute()) {
        return owned_state_parent(DirectoryHandle::open_without_symlinks(root)?);
    }
    let home = home.filter(|path| path.is_absolute()).ok_or_else(|| {
        io::Error::new(ErrorKind::InvalidInput, "no absolute Files state directory")
    })?;
    let mut dir = owned_state_parent(DirectoryHandle::open_without_symlinks(home)?)?;
    for name in [".local", "state"] {
        if for_save {
            match fs::mkdirat(&dir.fd, name, Mode::RWXU) {
                Ok(()) | Err(Errno::EXIST) => {}
                Err(error) => return Err(error.into()),
            }
        }
        let fd = fs::openat(
            &dir.fd,
            name,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        dir = owned_state_parent(DirectoryHandle::from_directory_fd(fd)?)?;
    }
    Ok(dir)
}

fn open_private(base: &DirectoryHandle, ensure: bool) -> io::Result<OwnedFd> {
    if ensure {
        match fs::mkdirat(&base.fd, DIRECTORY, Mode::RWXU) {
            Ok(()) | Err(Errno::EXIST) => {}
            Err(error) => return Err(error.into()),
        }
    }
    let dir = fs::openat(
        &base.fd,
        DIRECTORY,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let stat = fs::fstat(&dir)?;
    if stat.st_uid != geteuid().as_raw() || stat.st_mode & 0o777 != 0o700 {
        return Err(unsafe_state());
    }
    Ok(dir)
}

fn validate_record(dir: &OwnedFd) -> io::Result<OwnedFd> {
    // O_PATH establishes type before any data open. It cannot block on a FIFO
    // and doesn't run a device open hook. NOFOLLOW pins, but rejects, a link.
    let pinned = fs::openat(
        dir,
        RECORD,
        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let stat = fs::fstat(&pinned)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
        || stat.st_uid != geteuid().as_raw()
        || stat.st_mode & 0o777 != 0o600
        || stat.st_nlink != 1
    {
        return Err(unsafe_state());
    }
    Ok(pinned)
}

/// Read from the app-owned namespace under an already pinned XDG state root.
/// This never creates state. Call on a worker and decode the framed record.
pub fn read_files_tab_state(base: &DirectoryHandle) -> io::Result<Vec<u8>> {
    use rustix::fd::AsRawFd;

    let dir = open_private(base, false)?;
    let pinned = validate_record(&dir)?;
    let read = fs::open(
        format!("/proc/self/fd/{}", pinned.as_raw_fd()),
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let mut bytes = Vec::new();
    File::from(read)
        .take(FILES_TAB_STATE_CAP as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > FILES_TAB_STATE_CAP {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "Files session state is too large",
        ));
    }
    Ok(bytes)
}

struct TempFile<'a> {
    dir: &'a OwnedFd,
    name: String,
    published: bool,
}

impl Drop for TempFile<'_> {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::unlinkat(self.dir, self.name.as_str(), AtFlags::empty());
        }
    }
}

/// Persist only an explicitly encoded snapshot. On an error after rename,
/// durability is uncertain; callers should not say the former file survived.
pub fn write_files_tab_state(base: &DirectoryHandle, bytes: &[u8]) -> io::Result<()> {
    if bytes.is_empty() || bytes.len() > FILES_TAB_STATE_CAP {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "invalid Files session state size",
        ));
    }
    let dir = open_private(base, true)?;
    match validate_record(&dir) {
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let name = format!(
        ".tabs-{}-{now}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let fd = fs::openat(
        &dir,
        name.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )?;
    let mut guard = TempFile {
        dir: &dir,
        name,
        published: false,
    };
    let mut file = File::from(fd);
    fs::fchmod(&file, Mode::RUSR | Mode::WUSR)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::renameat(&dir, guard.name.as_str(), &dir, RECORD)?;
    guard.published = true;
    fs::fsync(&dir)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::fs::{DirBuilderExt, PermissionsExt, symlink},
        path::PathBuf,
    };

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("jwm-file-session-{}-{stamp}", std::process::id()));
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&path)
                .unwrap();
            Self(path)
        }

        fn base(&self) -> DirectoryHandle {
            DirectoryHandle::open_without_symlinks(&self.0).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn default_state_components_are_created_only_for_explicit_save() {
        let root = Fixture::new();
        assert!(open_files_state_root(Some(&root.0), None, false).is_err());
        assert!(!root.0.join(".local").exists());
        let pinned = open_files_state_root(Some(&root.0), None, true).unwrap();
        assert_eq!(
            pinned.identity(),
            DirectoryHandle::open(&root.0.join(".local/state"))
                .unwrap()
                .identity()
        );
        let configured = root.0.join("unexpected");
        assert!(open_files_state_root(Some(&root.0), Some(&configured), true).is_err());
        assert!(!configured.exists());
    }

    #[test]
    fn does_not_create_state_on_read_and_replaces_only_a_private_file() {
        let root = Fixture::new();
        let base = root.base();
        assert_eq!(
            read_files_tab_state(&base).unwrap_err().kind(),
            ErrorKind::NotFound
        );
        assert!(!root.0.join(DIRECTORY).exists());
        write_files_tab_state(&base, b"first contents").unwrap();
        let path = root.0.join(DIRECTORY).join(RECORD);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        write_files_tab_state(&base, b"second").unwrap();
        assert_eq!(read_files_tab_state(&base).unwrap(), b"second");
        assert_eq!(
            std::fs::read_dir(root.0.join(DIRECTORY)).unwrap().count(),
            1
        );
        assert!(write_files_tab_state(&base, &vec![b'x'; FILES_TAB_STATE_CAP + 1]).is_err());
        assert_eq!(read_files_tab_state(&base).unwrap(), b"second");
    }

    #[test]
    fn refuses_symlink_replacement_and_does_not_touch_a_foreign_file() {
        let root = Fixture::new();
        let base = root.base();
        let sentinel = root.0.join("keep");
        std::fs::write(&sentinel, b"foreign").unwrap();
        let dir = root.0.join(DIRECTORY);
        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        let record = dir.join(RECORD);
        symlink(&sentinel, &record).unwrap();
        assert_eq!(
            read_files_tab_state(&base).unwrap_err().kind(),
            ErrorKind::PermissionDenied
        );
        assert_eq!(
            write_files_tab_state(&base, b"not allowed")
                .unwrap_err()
                .kind(),
            ErrorKind::PermissionDenied
        );
        assert!(
            std::fs::symlink_metadata(record)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(sentinel).unwrap(), b"foreign");
    }

    #[test]
    fn validates_kind_before_opening_the_record_for_data() {
        let root = Fixture::new();
        let base = root.base();
        let dir = root.0.join(DIRECTORY);
        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        let fd = open_private(&base, false).unwrap();
        fs::mknodat(&fd, RECORD, FileType::Fifo, Mode::RUSR | Mode::WUSR, 0).unwrap();
        assert_eq!(
            read_files_tab_state(&base).unwrap_err().kind(),
            ErrorKind::PermissionDenied
        );
        assert_eq!(
            write_files_tab_state(&base, b"not allowed")
                .unwrap_err()
                .kind(),
            ErrorKind::PermissionDenied
        );
    }
}
