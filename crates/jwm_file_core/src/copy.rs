//! Staged, no-replace copies of one regular local file.
use crate::{Cancellation, DirectoryHandle, FileIdentity};
use rustix::{
    fd::AsRawFd,
    fs::{self, AtFlags, CWD, FileType, Mode, OFlags, SeekFrom, Stat},
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

pub(crate) fn content_observation(stat: &Stat) -> [i128; 5] {
    [
        i128::from(stat.st_size),
        i128::from(stat.st_ctime),
        i128::from(stat.st_ctime_nsec),
        i128::from(stat.st_mtime),
        i128::from(stat.st_mtime_nsec),
    ]
}

fn same_content_observation(before: &Stat, after: &Stat) -> bool {
    content_observation(before) == content_observation(after)
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

fn sparse_seek_unsupported(error: Errno) -> bool {
    matches!(error, Errno::INVAL | Errno::OPNOTSUPP | Errno::NOSYS)
}

fn copy_reported_extents(
    input: &File,
    output: &File,
    size: u64,
    cancel: &Cancellation,
) -> Result<bool, CopyError> {
    let mut cursor = 0;
    let mut buffer = vec![0u8; 128 * 1024];
    while cursor < size {
        check(cancel)?;
        let data = match fs::seek(input, SeekFrom::Data(cursor)) {
            Ok(data) if (cursor..size).contains(&data) => data,
            Ok(_) => return Err(CopyError::SourceChanged),
            Err(Errno::NXIO) => break, // Remaining bytes are a reported hole.
            Err(error) if sparse_seek_unsupported(error) => return Ok(false),
            Err(error) => return Err(io_error(error)),
        };
        let end = match fs::seek(input, SeekFrom::Hole(data)) {
            Ok(end) if (data + 1..=size).contains(&end) => end,
            Ok(_) | Err(Errno::NXIO) => return Err(CopyError::SourceChanged),
            Err(error) if sparse_seek_unsupported(error) => return Ok(false),
            Err(error) => return Err(io_error(error)),
        };
        fs::seek(input, SeekFrom::Start(data)).map_err(io_error)?;
        fs::seek(output, SeekFrom::Start(data)).map_err(io_error)?;
        let mut left = end - data;
        let mut input = input;
        let mut output = output;
        while left != 0 {
            check(cancel)?;
            let count = usize::try_from(left.min(buffer.len() as u64)).unwrap();
            let read = match input.read(&mut buffer[..count]) {
                Ok(0) => return Err(CopyError::SourceChanged),
                Ok(read) => read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(CopyError::Io(error)),
            };
            output.write_all(&buffer[..read]).map_err(CopyError::Io)?;
            left -= read as u64;
        }
        cursor = end;
    }
    check(cancel)?;
    // This creates any trailing hole and handles a completely sparse file.
    output.set_len(size).map_err(CopyError::Io)?;
    Ok(true)
}

fn copy_payload(
    input: &File,
    output: &File,
    size: u64,
    cancel: &Cancellation,
) -> Result<u64, CopyError> {
    if size == 0 {
        // Some virtual files report zero bytes but produce data. Keep the
        // existing exact-size check instead of claiming an empty copy.
        return copy_chunks(input, output, size, cancel);
    }
    if copy_reported_extents(input, output, size, cancel)? {
        return Ok(size);
    }
    check(cancel)?;
    // Some providers cannot enumerate extents. Reset private staging before
    // repeating the ordinary capped copy, including on a late unsupported seek.
    output.set_len(0).map_err(CopyError::Io)?;
    fs::seek(input, SeekFrom::Start(0)).map_err(io_error)?;
    fs::seek(output, SeekFrom::Start(0)).map_err(io_error)?;
    copy_chunks(input, output, size, cancel)
}

impl DirectoryHandle {
    /// Copy one observed regular source file into this destination directory.
    /// Data is staged in an anonymous inode. Publication is one no-replace
    /// link, after the bytes are synced and source observations still match.
    /// Reported sparse holes are not written. No source deletion, permission
    /// preservation, or crash-durable directory receipt is implied.
    /// Cancellation is cooperative until publication.
    pub fn copy_regular_no_replace(
        &self,
        source: CopySource<'_>,
        destination: CopyDestination<'_>,
        cancel: &Cancellation,
    ) -> Result<CopyReceipt, CopyError> {
        self.copy_with_observation(source, destination, cancel, None)
    }

    pub(crate) fn copy_with_observation(
        &self,
        source: CopySource<'_>,
        destination: CopyDestination<'_>,
        cancel: &Cancellation,
        expected: Option<[i128; 5]>,
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
        if expected.is_some_and(|expected| content_observation(&before) != expected) {
            return Err(CopyError::SourceChanged);
        }
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
        let bytes = copy_payload(&input, &output, size, cancel)?;
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
