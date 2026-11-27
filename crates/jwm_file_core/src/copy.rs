//! Staged, no-replace copies of one regular local file.
use crate::copy_names::{COPY_NAMES, sibling_copy_name};
use crate::{Cancellation, DirectoryHandle, FileIdentity};
use rustix::{
    fd::AsRawFd,
    fs::{self, AtFlags, CWD, FileType, Mode, OFlags, SeekFrom, Stat},
    io::Errno,
};
use std::{
    ffi::{OsStr, OsString},
    fmt,
    fs::File,
    io,
    io::{Read, Write},
    path::Path,
};

#[cfg(test)]
pub(crate) mod publication_probe {
    use std::{cell::RefCell, ffi::OsStr};

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(crate) enum Point {
        Before,
        After,
    }

    type Probe = Box<dyn FnMut(Point, &OsStr)>;
    thread_local! {
        static ACTIVE: RefCell<Option<Probe>> = const { RefCell::new(None) };
    }

    pub(crate) fn with_probe<T>(
        probe: impl FnMut(Point, &OsStr) + 'static,
        run: impl FnOnce() -> T,
    ) -> T {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                ACTIVE.with(|cell| cell.borrow_mut().take());
            }
        }
        ACTIVE.with(|cell| {
            let mut slot = cell.borrow_mut();
            assert!(slot.is_none(), "only one test probe per thread");
            *slot = Some(Box::new(probe));
        });
        let _reset = Reset;
        run()
    }

    pub(super) fn emit(point: Point, name: &OsStr) {
        ACTIVE.with(|cell| {
            if let Some(probe) = cell.borrow_mut().as_mut() {
                probe(point, name);
            }
        });
    }
}

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

#[derive(Debug, Eq, PartialEq)]
pub struct CopySiblingReceipt {
    pub name: OsString,
    pub bytes: u64,
}

#[derive(Clone, Copy)]
enum PublishName<'a> {
    Literal(&'a OsStr),
    Sibling,
}

#[derive(Debug)]
pub enum CopyError {
    InvalidName,
    ParentChanged,
    SourceChanged,
    NotRegular,
    Cancelled,
    DestinationExists,
    SiblingNamesExhausted,
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
            Self::SiblingNamesExhausted => f.write_str("no available sibling copy name"),
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

    /// Copy once to anonymous staging and attempt at most 32 sibling names.
    /// Every publication is no-replace; occupied names are never followed.
    pub fn copy_regular_keep_both(
        &self,
        source: CopySource<'_>,
        displayed_parent: &Path,
        cancel: &Cancellation,
    ) -> Result<CopySiblingReceipt, CopyError> {
        self.copy_with_naming(source, displayed_parent, PublishName::Sibling, cancel, None)
    }

    pub(crate) fn copy_with_observation(
        &self,
        source: CopySource<'_>,
        destination: CopyDestination<'_>,
        cancel: &Cancellation,
        expected: Option<[i128; 5]>,
    ) -> Result<CopyReceipt, CopyError> {
        self.copy_with_naming(
            source,
            destination.displayed_parent,
            PublishName::Literal(destination.name),
            cancel,
            expected,
        )
        .map(|receipt| CopyReceipt {
            bytes: receipt.bytes,
        })
    }

    fn copy_with_naming(
        &self,
        source: CopySource<'_>,
        displayed_parent: &Path,
        naming: PublishName<'_>,
        cancel: &Cancellation,
        expected: Option<[i128; 5]>,
    ) -> Result<CopySiblingReceipt, CopyError> {
        check(cancel)?;
        if !crate::mkdir::single_name(source.name)
            || matches!(naming, PublishName::Literal(name) if !crate::mkdir::single_name(name))
        {
            return Err(CopyError::InvalidName);
        }
        verify_parent(source.parent, source.displayed_parent)?;
        verify_parent(self, displayed_parent)?;
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
        let publish = |name: &OsStr| {
            verify_parent(source.parent, source.displayed_parent)?;
            verify_parent(self, displayed_parent)?;
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
            #[cfg(test)]
            publication_probe::emit(publication_probe::Point::Before, name);
            check(cancel)?;
            fs::linkat(
                CWD,
                fd_path(&output),
                &self.fd,
                name,
                AtFlags::SYMLINK_FOLLOW,
            )
            .map_err(|e| {
                if e == Errno::EXIST {
                    CopyError::DestinationExists
                } else {
                    io_error(e)
                }
            })?;
            #[cfg(test)]
            publication_probe::emit(publication_probe::Point::After, name);
            Ok(())
        };
        let name = match naming {
            PublishName::Literal(name) => {
                publish(name)?;
                name.to_os_string()
            }
            PublishName::Sibling => {
                for attempt in 1..=COPY_NAMES {
                    let name = sibling_copy_name(source.name, attempt);
                    match publish(&name) {
                        Ok(()) => return Ok(CopySiblingReceipt { name, bytes }),
                        Err(CopyError::DestinationExists) => continue,
                        Err(error) => return Err(error),
                    }
                }
                return Err(CopyError::SiblingNamesExhausted);
            }
        };
        Ok(CopySiblingReceipt { name, bytes })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keep_both_preserves_existing_entries_and_is_bounded() {
        use std::os::unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, symlink},
        };

        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("jwm-copy-{}-{now}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let _cleanup = Cleanup(dir.clone());
        let original = OsStr::from_bytes(b"file-%20-\x80.txt");
        let first = sibling_copy_name(original, 1);
        let second = sibling_copy_name(original, 2);
        symlink("missing", dir.join(&first)).unwrap();
        std::fs::write(dir.join(original), b"source").unwrap();
        std::fs::write(dir.join(&second), b"old bytes").unwrap();
        let meta = std::fs::metadata(dir.join(original)).unwrap();
        let parent = DirectoryHandle::open(&dir).unwrap();
        let source = || CopySource {
            parent: &parent,
            displayed_parent: &dir,
            name: original,
            expected_file: FileIdentity {
                device: meta.dev(),
                inode: meta.ino(),
            },
        };

        let cancelled = Cancellation::default();
        cancelled.cancel();
        assert!(matches!(
            parent.copy_regular_keep_both(source(), &dir, &cancelled),
            Err(CopyError::Cancelled)
        ));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 3);
        let copy = parent
            .copy_regular_keep_both(source(), &dir, &Cancellation::default())
            .unwrap();
        assert_eq!(copy.name, sibling_copy_name(original, 3));
        assert_eq!(copy.bytes, 6);
        assert_eq!(std::fs::read(dir.join(copy.name)).unwrap(), b"source");
        assert_eq!(
            std::fs::read_link(dir.join(first)).unwrap(),
            Path::new("missing")
        );
        assert_eq!(std::fs::read(dir.join(second)).unwrap(), b"old bytes");
        for number in 4..=COPY_NAMES {
            std::fs::write(dir.join(sibling_copy_name(original, number)), b"old").unwrap();
        }
        assert!(matches!(
            parent.copy_regular_keep_both(source(), &dir, &Cancellation::default()),
            Err(CopyError::SiblingNamesExhausted)
        ));
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            usize::from(COPY_NAMES) + 1
        );
        assert_eq!(std::fs::read(dir.join(original)).unwrap(), b"source");
    }

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
