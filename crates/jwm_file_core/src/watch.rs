use crate::DirectoryHandle;
use rustix::fd::{AsRawFd, OwnedFd};
use rustix::fs::inotify::{self, CreateFlags, ReadFlags, Reader, WatchFlags};
use std::io;
use std::mem::MaybeUninit;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirectoryChange {
    Unchanged,
    /// At least one change occurred, or the queue overflowed. Re-scan.
    Dirty,
    /// The watched directory moved, was deleted, or became unavailable.
    Invalidated,
}

/// One non-recursive Linux watch anchored to an already-selected directory.
/// Both setup and pumping belong on a worker, not on the GTK thread.
#[derive(Debug)]
pub struct DirectoryWatch(OwnedFd);

impl DirectoryWatch {
    pub fn new(parent: &DirectoryHandle) -> io::Result<Self> {
        let fd = inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK)?;
        // The procfs descriptor points to the pinned object even if someone
        // replaces the displayed path before the watch is installed.
        let pinned = format!("/proc/self/fd/{}", parent.fd.as_raw_fd());
        inotify::add_watch(
            &fd,
            pinned.as_str(),
            WatchFlags::ATTRIB
                | WatchFlags::CLOSE_WRITE
                | WatchFlags::CREATE
                | WatchFlags::DELETE
                | WatchFlags::DELETE_SELF
                | WatchFlags::MOVE_SELF
                | WatchFlags::MOVED_FROM
                | WatchFlags::MOVED_TO
                | WatchFlags::ONLYDIR,
        )?;
        Ok(Self(fd))
    }

    /// Drain complete read batches and summarize changes, without maintaining
    /// a path delta from untrusted event names. The bound applies between
    /// kernel reads; any unread kernel events remain for the next call.
    pub fn read_change(&self) -> io::Result<DirectoryChange> {
        let mut buf = [MaybeUninit::uninit(); 4096];
        let mut reader = Reader::new(&self.0, &mut buf);
        let mut change = DirectoryChange::Unchanged;
        let mut seen = 0usize;
        while seen < 256 || !reader.is_buffer_empty() {
            match reader.next() {
                Ok(event) => {
                    seen += 1;
                    let flags = event.events();
                    if flags.intersects(
                        ReadFlags::DELETE_SELF
                            | ReadFlags::MOVE_SELF
                            | ReadFlags::UNMOUNT
                            | ReadFlags::IGNORED,
                    ) {
                        change = DirectoryChange::Invalidated;
                    } else if change == DirectoryChange::Unchanged {
                        change = DirectoryChange::Dirty;
                    }
                }
                Err(rustix::io::Errno::WOULDBLOCK) => return Ok(change),
                Err(error) => return Err(error.into()),
            }
        }
        Ok(change)
    }
}
