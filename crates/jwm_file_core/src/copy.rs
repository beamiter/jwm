//! Staged, no-replace copies of one regular local file.
use crate::{Cancellation, DirectoryHandle, FileIdentity};
use rustix::{
    fd::AsRawFd,
    fs::{self, AtFlags, CWD, FileType, Mode, OFlags, Stat},
    io::Errno,
};
use std::{
    ffi::OsStr,
    fmt,
    fs::File,
    io,
    io::{Read, Write},
    path::Path,
};

pub struct CopySource<'a> {
    pub parent: &'a DirectoryHandle,
    pub displayed_parent: &'a Path,
    pub name: &'a OsStr,
    pub expected_file: FileIdentity,
}

pub struct CopyDestination<'a> {
    pub displayed_parent: &'a Path,
    pub name: &'a OsStr,
}

#[derive(Debug, Eq, PartialEq)]
pub struct CopyReceipt {
    pub bytes: u64,
}

#[derive(Debug)]
pub enum CopyError {
    InvalidName,
    ParentChanged,
    SourceChanged,
    NotRegular,
    Cancelled,
    DestinationExists,
    StagingUnavailable(io::Error),
    Io(io::Error),
}

impl fmt::Display for CopyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName => f.write_str("expected one child name for each path"),
            Self::ParentChanged => f.write_str("source or destination directory changed"),
            Self::SourceChanged => f.write_str("source changed during copy"),
            Self::NotRegular => f.write_str("only regular files can be copied"),
            Self::Cancelled => f.write_str("copy cancelled before publication"),
            Self::DestinationExists => f.write_str("destination already exists"),
            Self::StagingUnavailable(error) => {
                write!(f, "private copy staging is unavailable: {error}")
            }
            Self::Io(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for CopyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::StagingUnavailable(error) | Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

fn io_error(error: Errno) -> CopyError {
    CopyError::Io(error.into())
}

fn verify_parent(handle: &DirectoryHandle, path: &Path) -> Result<(), CopyError> {
    let current = DirectoryHandle::open(path).map_err(|err| {
        if matches!(
            err.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
        ) {
            CopyError::ParentChanged
        } else {
            CopyError::Io(err)
        }
    })?;
    if current.identity() == handle.identity() {
        Ok(())
    } else {
        Err(CopyError::ParentChanged)
    }
}

fn verify_source(stat: &Stat, expected: FileIdentity) -> Result<(), CopyError> {
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(CopyError::NotRegular);
    }
    if stat.st_dev == expected.device && stat.st_ino == expected.inode {
        Ok(())
    } else {
        Err(CopyError::SourceChanged)
    }
}

fn same_content_observation(before: &Stat, after: &Stat) -> bool {
    before.st_size == after.st_size
        && before.st_mtime == after.st_mtime
        && before.st_mtime_nsec == after.st_mtime_nsec
        && before.st_ctime == after.st_ctime
        && before.st_ctime_nsec == after.st_ctime_nsec
}

fn check(cancel: &Cancellation) -> Result<(), CopyError> {
    cancel.check().map_err(|_| CopyError::Cancelled)
}

fn fd_path(file: &impl AsRawFd) -> String {
    format!("/proc/self/fd/{}", file.as_raw_fd())
}

fn copy_chunks(
    mut source: impl Read,
    mut destination: impl Write,
    expected: u64,
    cancel: &Cancellation,
) -> Result<u64, CopyError> {
    let mut buffer = vec![0u8; 128 * 1024];
    let mut done = 0;
    loop {
        check(cancel)?;
        let n = match source.read(&mut buffer) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => result.map_err(CopyError::Io)?,
        };
        if n == 0 {
            break;
        }
        done += n as u64;
        if done > expected {
            return Err(CopyError::SourceChanged);
        }
        destination.write_all(&buffer[..n]).map_err(CopyError::Io)?;
    }
    if done == expected {
        Ok(done)
    } else {
        Err(CopyError::SourceChanged)
    }
}

impl DirectoryHandle {
    /// Copy one observed regular source file into this destination directory.
    /// Data is staged in an anonymous inode. Publication is one no-replace
    /// link, after the bytes are synced and source observations still match.
    /// No source deletion, metadata preservation, or crash-durable directory
    /// receipt is implied. Cancellation is cooperative until publication.
    pub fn copy_regular_no_replace(
        &self,
        source: CopySource<'_>,
        destination: CopyDestination<'_>,
        cancel: &Cancellation,
    ) -> Result<CopyReceipt, CopyError> {
        check(cancel)?;
        if !crate::mkdir::single_name(source.name) || !crate::mkdir::single_name(destination.name) {
            return Err(CopyError::InvalidName);
        }
        verify_parent(source.parent, source.displayed_parent)?;
        verify_parent(self, destination.displayed_parent)?;
        let pin = fs::openat(
            &source.parent.fd,
            source.name,
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| {
            if e == Errno::NOENT {
                CopyError::SourceChanged
            } else {
                io_error(e)
            }
        })?;
        let before = fs::fstat(&pin).map_err(io_error)?;
        verify_source(&before, source.expected_file)?;
        // Follow only our own pinned FD, never reopen the untrusted filename.
        let input = File::from(
            fs::open(
                fd_path(&pin),
                OFlags::RDONLY | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(io_error)?,
        );
        let output = File::from(
            fs::openat(
                &self.fd,
                ".",
                OFlags::TMPFILE | OFlags::WRONLY | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(|e| CopyError::StagingUnavailable(e.into()))?,
        );
        let size = u64::try_from(before.st_size).map_err(|_| CopyError::SourceChanged)?;
        let bytes = copy_chunks(&input, &output, size, cancel)?;
        check(cancel)?;
        output.sync_all().map_err(CopyError::Io)?;
        let after = fs::fstat(&pin).map_err(io_error)?;
        if !same_content_observation(&before, &after) {
            return Err(CopyError::SourceChanged);
        }
        verify_parent(source.parent, source.displayed_parent)?;
        verify_parent(self, destination.displayed_parent)?;
        let observed = fs::statat(&source.parent.fd, source.name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|e| {
                if e == Errno::NOENT {
                    CopyError::SourceChanged
                } else {
                    io_error(e)
                }
            })?;
        verify_source(&observed, source.expected_file)?;
        if !same_content_observation(&before, &observed) {
            return Err(CopyError::SourceChanged);
        }
        check(cancel)?;
        fs::linkat(
            CWD,
            fd_path(&output),
            &self.fd,
            destination.name,
            AtFlags::SYMLINK_FOLLOW,
        )
        .map_err(|e| {
            if e == Errno::EXIST {
                CopyError::DestinationExists
            } else {
                io_error(e)
            }
        })?;
        Ok(CopyReceipt { bytes })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_between_chunks_and_source_size_changes_stop_copy() {
        struct Cancelling<'a>(&'a Cancellation);
        impl Read for Cancelling<'_> {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                self.0.cancel();
                output[0] = 7;
                Ok(1)
            }
        }
        let cancel = Cancellation::default();
        let mut output = Vec::new();
        assert!(matches!(
            copy_chunks(Cancelling(&cancel), &mut output, 2, &cancel),
            Err(CopyError::Cancelled)
        ));
        assert_eq!(output, [7]);
        for (input, expected) in [(b"a".as_slice(), 2), (b"ab".as_slice(), 1)] {
            assert!(matches!(
                copy_chunks(input, io::sink(), expected, &Cancellation::default()),
                Err(CopyError::SourceChanged)
            ));
        }
    }
}
