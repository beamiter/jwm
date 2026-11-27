//! Publication inside a unique private destination-side namespace.
use crate::{DirectoryHandle, FileIdentity};
use rustix::{
    fs::{self, AtFlags, FileType, Mode, OFlags, RenameFlags},
    io::Errno,
    process::geteuid,
};
use std::{
    ffi::{OsStr, OsString},
    io,
    sync::atomic::{AtomicUsize, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

pub(super) const STAGED_ROOT: &str = "tree";
static NEXT: AtomicUsize = AtomicUsize::new(0);

pub(super) struct StageCreateError {
    pub(super) error: io::Error,
    pub(super) residue: Option<OsString>,
}

impl From<io::Error> for StageCreateError {
    fn from(error: io::Error) -> Self {
        Self {
            error,
            residue: None,
        }
    }
}

pub(super) struct Stage<'a> {
    parent: &'a DirectoryHandle,
    wrapper: DirectoryHandle,
    name: OsString,
}

fn stat_identity(stat: &fs::Stat) -> FileIdentity {
    FileIdentity {
        device: stat.st_dev,
        inode: stat.st_ino,
    }
}

fn private_error() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "copy staging requires a private writable destination")
}

fn open_dir(parent: &DirectoryHandle, name: &OsStr) -> io::Result<DirectoryHandle> {
    let fd = fs::openat(
        &parent.fd,
        name,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    DirectoryHandle::from_directory_fd(fd)
}

impl<'a> Stage<'a> {
    pub(super) fn create(parent: &'a DirectoryHandle) -> Result<Self, StageCreateError> {
        let stat = fs::fstat(&parent.fd).map_err(io::Error::from)?;
        if stat.st_uid != geteuid().as_raw() || stat.st_mode & 0o022 != 0 {
            return Err(private_error().into());
        }
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        for _ in 0..16 {
            let name = OsString::from(format!(
                ".jwm-files-copy-{}-{stamp}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::mkdirat(&parent.fd, &name, Mode::RWXU) {
                Ok(()) => {}
                Err(Errno::EXIST) => continue,
                Err(error) => return Err(io::Error::from(error).into()),
            }
            // If this cannot be validated, leave the namespace untouched.
            let after_mkdir = |error| StageCreateError {
                error,
                residue: Some(name.clone()),
            };
            let wrapper = open_dir(parent, &name).map_err(&after_mkdir)?;
            let current = fs::fstat(&wrapper.fd)
                .map_err(io::Error::from)
                .map_err(&after_mkdir)?;
            if current.st_uid != geteuid().as_raw() || current.st_mode & 0o777 != 0o700 {
                return Err(after_mkdir(private_error()));
            }
            return Ok(Self {
                parent,
                wrapper,
                name,
            });
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "no private copy staging name available",
        )
        .into())
    }

    pub(super) fn name(&self) -> &OsStr {
        &self.name
    }

    pub(super) fn make_tree(&self) -> io::Result<DirectoryHandle> {
        fs::mkdirat(&self.wrapper.fd, STAGED_ROOT, Mode::RWXU)?;
        open_dir(&self.wrapper, OsStr::new(STAGED_ROOT))
    }

    fn check_wrapper(&self) -> io::Result<()> {
        let parent = fs::fstat(&self.parent.fd)?;
        if parent.st_uid != geteuid().as_raw() || parent.st_mode & 0o022 != 0 {
            return Err(private_error());
        }
        let current = fs::statat(&self.parent.fd, &self.name, AtFlags::SYMLINK_NOFOLLOW)?;
        if FileType::from_raw_mode(current.st_mode) != FileType::Directory
            || stat_identity(&current) != self.wrapper.identity()
            || current.st_uid != geteuid().as_raw()
            || current.st_mode & 0o777 != 0o700
        {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "copy staging folder changed"));
        }
        Ok(())
    }

    pub(super) fn publish(
        &self,
        tree: &DirectoryHandle,
        final_name: &OsStr,
    ) -> io::Result<()> {
        if !crate::mkdir::single_name(final_name) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "expected a single final name"));
        }
        let current = fs::statat(&self.wrapper.fd, STAGED_ROOT, AtFlags::SYMLINK_NOFOLLOW)?;
        if FileType::from_raw_mode(current.st_mode) != FileType::Directory
            || stat_identity(&current) != tree.identity()
        {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "staged folder changed"));
        }
        self.check_wrapper()?;
        fs::renameat_with(
            &self.wrapper.fd,
            STAGED_ROOT,
            &self.parent.fd,
            final_name,
            RenameFlags::NOREPLACE,
        )?;
        Ok(())
    }

    pub(super) fn remove_empty_tree(&self, tree: &DirectoryHandle) -> io::Result<()> {
        let current = fs::statat(&self.wrapper.fd, STAGED_ROOT, AtFlags::SYMLINK_NOFOLLOW)?;
        if FileType::from_raw_mode(current.st_mode) != FileType::Directory
            || stat_identity(&current) != tree.identity()
        {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "staged folder changed"));
        }
        fs::unlinkat(&self.wrapper.fd, STAGED_ROOT, AtFlags::REMOVEDIR)?;
        Ok(())
    }

    pub(super) fn remove_empty_wrapper(&self) -> io::Result<()> {
        self.check_wrapper()?;
        fs::unlinkat(&self.parent.fd, &self.name, AtFlags::REMOVEDIR)?;
        Ok(())
    }
}
