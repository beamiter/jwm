//! Hold an explicit local directory until the terminal's child accepts it.
use crate::{DirectoryHandle, FileIdentity};
use rustix::{
    fd::{AsRawFd, OwnedFd},
    fs::{self, FileType, Mode, OFlags},
    io::{self as rio, Errno},
    pipe::{PipeFlags, pipe_with},
};
use std::{
    io::{self, ErrorKind},
    os::fd::RawFd,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

fn receipt(identity: FileIdentity) -> [u8; 16] {
    let mut bytes = [0; 16];
    bytes[..8].copy_from_slice(&identity.device.to_le_bytes());
    bytes[8..].copy_from_slice(&identity.inode.to_le_bytes());
    bytes
}

#[derive(Clone, Copy, Debug)]
pub struct TerminalLocator {
    pub owner: u32,
    pub directory: RawFd,
    pub reply: RawFd,
    pub expected: FileIdentity,
}

pub struct TerminalHandoff {
    directory: DirectoryHandle,
    read: OwnedFd,
    write: Option<OwnedFd>,
}

impl TerminalHandoff {
    /// Bound to the current view's expected directory before any launcher runs.
    pub fn new(path: &Path, expected: FileIdentity) -> io::Result<Self> {
        if !path.is_absolute() {
            return Err(io::Error::new(ErrorKind::InvalidInput, "expected absolute directory"));
        }
        let directory = DirectoryHandle::open(path)?;
        if directory.identity() != expected {
            return Err(io::Error::new(ErrorKind::NotFound, "the directory has changed"));
        }
        let (read, write) = pipe_with(PipeFlags::CLOEXEC | PipeFlags::NONBLOCK)?;
        Ok(Self {
            directory,
            read,
            write: Some(write),
        })
    }

    pub fn locator(&self) -> TerminalLocator {
        TerminalLocator {
            owner: std::process::id(),
            directory: self.directory.fd.as_raw_fd(),
            reply: self.write.as_ref().expect("live handoff").as_raw_fd(),
            expected: self.directory.identity(),
        }
    }

    /// On a worker, await the final helper's receipt without blocking GTK.
    /// Dropping this guard closes all the original descriptors.
    pub fn wait_for_child(mut self, timeout: Duration) -> io::Result<()> {
        let start = Instant::now();
        let expected = receipt(self.directory.identity());
        let mut buffer = [0; 17];
        let mut filled = 0;
        loop {
            match rio::read(&self.read, &mut buffer[filled..]) {
                Ok(0) if filled == expected.len() => return Ok(()),
                Ok(0) => return Err(io::Error::other("terminal helper closed early")),
                Ok(n) => {
                    filled += n;
                    if filled > expected.len() || buffer[..filled] != expected[..filled] {
                        return Err(io::Error::other("terminal helper could not execute the shell"));
                    }
                    if filled == expected.len() {
                        // The helper acquired its own CLOEXEC writer. Drop
                        // ours so successful exec closes the last writer.
                        drop(self.write.take());
                    }
                }
                Err(Errno::AGAIN | Errno::INTR) => {}
                Err(error) => return Err(error.into()),
            }
            let remaining = timeout.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                return Err(io::Error::new(ErrorKind::TimedOut, "terminal did not reply"));
            }
            std::thread::sleep(remaining.min(Duration::from_millis(8)));
        }
    }
}

pub struct TerminalChildSetup {
    directory: DirectoryHandle,
    write: OwnedFd,
}

impl TerminalChildSetup {
    /// No original pathname or installed terminal's cwd is accepted here.
    /// Both descriptors must still exist in the owner's process.
    pub fn reopen(locator: TerminalLocator) -> io::Result<Self> {
        if locator.owner <= 1 || locator.directory < 3 || locator.reply < 3 {
            return Err(io::Error::new(ErrorKind::InvalidInput, "invalid terminal locator"));
        }
        let directory = DirectoryHandle::open(Path::new(&format!(
            "/proc/{}/fd/{}",
            locator.owner, locator.directory,
        )))?;
        if directory.identity() != locator.expected {
            return Err(io::Error::new(ErrorKind::InvalidData, "terminal directory changed"));
        }
        let pinned = fs::open(
            format!("/proc/{}/fd/{}", locator.owner, locator.reply),
            OFlags::PATH | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        if FileType::from_raw_mode(fs::fstat(&pinned)?.st_mode) != FileType::Fifo {
            return Err(io::Error::new(ErrorKind::InvalidData, "terminal reply is not a pipe"));
        }
        let write = fs::open(
            format!("/proc/self/fd/{}", pinned.as_raw_fd()),
            OFlags::WRONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        Ok(Self { directory, write })
    }

    /// Use this with Command::current_dir before exec closes the held FD.
    pub fn process_cwd(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.directory.fd.as_raw_fd()))
    }

    /// Report the held directory immediately before executing the intended shell.
    pub fn prepared(&self) -> io::Result<()> {
        let data = receipt(self.directory.identity());
        if rio::write(&self.write, &data)? != data.len() {
            return Err(io::Error::new(ErrorKind::WriteZero, "partial terminal receipt"));
        }
        Ok(())
    }

    /// If exec returns, the CLOEXEC receipt descriptor is still open.
    pub fn reject_exec(&self) {
        let _ = rio::write(&self.write, b"!");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handoff_wait_is_bounded_and_requires_exact_identity() {
        let current = std::env::current_dir().unwrap();
        let id = DirectoryHandle::open(&current).unwrap().identity();
        let unused = TerminalHandoff::new(&current, id).unwrap();
        let error = unused.wait_for_child(Duration::from_millis(20)).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::TimedOut);

        let handoff = TerminalHandoff::new(&current, id).unwrap();
        let locator = handoff.locator();
        let wrong = TerminalLocator {
            expected: FileIdentity {
                inode: id.inode ^ 1,
                ..id
            },
            ..locator
        };
        assert!(TerminalChildSetup::reopen(wrong).is_err());
        let child = TerminalChildSetup::reopen(locator).unwrap();
        let held = DirectoryHandle::open(&child.process_cwd()).unwrap();
        assert_eq!(held.identity(), id);
        child.prepared().unwrap();
        drop(child); // In production exec closes this CLOEXEC writer.
        handoff.wait_for_child(Duration::from_millis(20)).unwrap();

        let handoff = TerminalHandoff::new(&current, id).unwrap();
        let failed = TerminalChildSetup::reopen(handoff.locator()).unwrap();
        failed.prepared().unwrap();
        failed.reject_exec();
        drop(failed);
        assert!(handoff.wait_for_child(Duration::from_millis(20)).is_err());
    }
}
