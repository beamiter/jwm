use crate::protocol::invalid;
use crate::{HEADER_BYTES, Header, Hello, OutputSize};
use std::fs::{self, DirBuilder, Metadata};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// A trusted, nonblocking event-loop notification, not producer-supplied code.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// One complete frame. The payload is always tight RGBA8, independent of the
/// original wire stride recorded in `header`. Ownership passes to the caller.
#[derive(Debug)]
pub struct Frame {
    /// Unique authenticated connection that published this frame. This is local
    /// consumer metadata, never a producer-selected wire field.
    pub connection_epoch: u64,
    pub header: Header,
    pub rgba: Vec<u8>,
}

#[derive(Clone, Copy)]
struct Timeouts {
    io_poll: Duration,
    accept_poll: Duration,
    frame: Duration,
    idle: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            io_poll: Duration::from_millis(20),
            accept_poll: Duration::from_millis(10),
            frame: Duration::from_secs(2),
            idle: Duration::from_secs(30),
        }
    }
}

struct Shared {
    stop: AtomicBool,
    connected: AtomicBool,
    output: AtomicU64,
    generation: AtomicU64,
    connection_generation: AtomicU64,
    connection_epoch: AtomicU64,
    latest: Mutex<Option<(u64, Frame)>>,
    revision: AtomicU64,
    observed_revision: AtomicU64,
    waker: OnceLock<Waker>,
}

impl Shared {
    fn wake(&self) {
        self.revision.fetch_add(1, Ordering::Release);
        if let Some(waker) = self.waker.get() {
            waker();
        }
    }

    fn current(&self, generation: u64) -> bool {
        !self.stop.load(Ordering::Acquire) && generation == self.generation.load(Ordering::Acquire)
    }

    fn connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
            && self.connection_generation.load(Ordering::Acquire)
                == self.generation.load(Ordering::Acquire)
    }

    fn begin_connection(&self) -> Option<u64> {
        self.connected.store(false, Ordering::Release);
        // Exhaustion closes the endpoint rather than reusing an identity.
        // Only the receive worker advances epochs; readers only load them.
        let next = self
            .connection_epoch
            .load(Ordering::Relaxed)
            .checked_add(1)?;
        self.connection_epoch.store(next, Ordering::Release);
        Some(next)
    }

    fn try_take_latest(&self) -> Option<Frame> {
        let revision = self.revision.load(Ordering::Acquire);
        let mut latest = self.latest.try_lock().ok()?;
        self.observed_revision.store(revision, Ordering::Release);
        let (generation, frame) = latest.take()?;
        if self.connected()
            && generation == self.generation.load(Ordering::Acquire)
            && frame.connection_epoch == self.connection_epoch.load(Ordering::Acquire)
        {
            Some(frame)
        } else {
            None
        }
    }

    fn disconnect(&self) {
        self.connected.store(false, Ordering::Release);
        if let Ok(mut latest) = self.latest.lock() {
            *latest = None;
        }
        self.wake();
    }
}

/// A single-producer local endpoint. Socket and payload I/O, parsing, allocation
/// and row compaction live exclusively on its worker thread. Reconnecting is a
/// producer responsibility; each new connection gets current capabilities.
///
/// `try_take_latest`, `connected` and `set_output_size` never wait for a mutex
/// or perform I/O. `bind` and `Drop` are lifecycle operations, not render-loop
/// operations. Drop joins a worker whose socket waits are bounded to 20 ms.
pub struct Server {
    owner: SocketOwner,
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

impl Server {
    pub fn bind(path: PathBuf, output: OutputSize) -> io::Result<Self> {
        Self::bind_with_timeouts(path, output, Timeouts::default())
    }

    fn bind_with_timeouts(
        path: PathBuf,
        output: OutputSize,
        timeouts: Timeouts,
    ) -> io::Result<Self> {
        output.validate()?;
        prepare_parent(&path)?;
        remove_stale_socket(&path)?;
        let listener = UnixListener::bind(&path)?;
        let owner = SocketOwner::new(path)?;
        fs::set_permissions(&owner.path, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            connected: AtomicBool::new(false),
            output: AtomicU64::new(output.packed()),
            generation: AtomicU64::new(1),
            connection_generation: AtomicU64::new(0),
            connection_epoch: AtomicU64::new(0),
            latest: Mutex::new(None),
            revision: AtomicU64::new(0),
            observed_revision: AtomicU64::new(0),
            waker: OnceLock::new(),
        });
        let worker_shared = shared.clone();
        let worker = std::thread::Builder::new()
            .name("desktop-visual-ipc".into())
            .spawn(move || receive_loop(listener, &worker_shared, timeouts))?;
        Ok(Self {
            owner,
            shared,
            worker: Some(worker),
        })
    }

    /// Install the event-loop waker once. Calling again leaves the first waker
    /// installed. A wake is issued immediately, so early publications aren't
    /// lost when the compositor obtains its loop signal after construction.
    pub fn set_waker(&self, waker: Waker) {
        let _ = self.shared.waker.set(waker);
        self.shared.wake();
    }

    /// Invalidate the old canvas immediately. The worker closes its connection
    /// within its bounded I/O poll; a new connection receives the new size.
    /// Unchanged sizes do not disconnect. Zero dimensions are rejected.
    pub fn set_output_size(&self, output: OutputSize) -> io::Result<()> {
        output.validate()?;
        if self.shared.output.swap(output.packed(), Ordering::AcqRel) != output.packed() {
            self.shared.generation.fetch_add(1, Ordering::AcqRel);
        }
        Ok(())
    }

    /// True for unseen frames or connection-state changes, including disconnects.
    /// Taking (or attempting to take) a frame acknowledges the observed state.
    pub fn has_pending(&self) -> bool {
        self.shared.revision.load(Ordering::Acquire)
            != self.shared.observed_revision.load(Ordering::Acquire)
    }

    pub fn connected(&self) -> bool {
        self.shared.connected()
    }

    /// Changes on every authenticated accept, including same-size reconnects.
    /// Consumers must tag retained GPU textures with `Frame::connection_epoch`
    /// and hide/drop them when this value differs, even if `connected()` is true.
    pub fn connection_epoch(&self) -> u64 {
        self.shared.connection_epoch.load(Ordering::Acquire)
    }

    /// Latest-wins handoff. Contention returns None instead of stalling the
    /// compositor. A subsequent poll takes the still-pending frame. A frame
    /// from an obsolete output generation is never handed to the renderer.
    pub fn try_take_latest(&self) -> Option<Frame> {
        self.shared.try_take_latest()
    }

    pub fn socket_path(&self) -> &Path {
        &self.owner.path
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        // SocketOwner removes only the exact socket inode created by bind.
        // No pathname connection, producer signal, or process-name kill occurs.
    }
}

fn receive_loop(listener: UnixListener, shared: &Shared, timeouts: Timeouts) {
    while !shared.stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                if !peer_is_current_user(&stream) {
                    continue;
                }
                let Some(connection_epoch) = shared.begin_connection() else {
                    break;
                };
                let generation = shared.generation.load(Ordering::Acquire);
                let output = OutputSize::unpack(shared.output.load(Ordering::Acquire));
                let _ = receive_connection(
                    &mut stream,
                    shared,
                    generation,
                    connection_epoch,
                    output,
                    timeouts,
                );
                let _ = stream.shutdown(std::net::Shutdown::Both);
                shared.disconnect();
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(timeouts.accept_poll);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    shared.disconnect();
}

fn receive_connection(
    stream: &mut UnixStream,
    shared: &Shared,
    generation: u64,
    connection_epoch: u64,
    output: OutputSize,
    timeouts: Timeouts,
) -> io::Result<()> {
    stream.set_read_timeout(Some(timeouts.io_poll))?;
    stream.set_write_timeout(Some(timeouts.io_poll))?;
    stream.write_all(&Hello { output }.encode()?)?;
    if !shared.current(generation) {
        return Ok(());
    }
    shared
        .connection_generation
        .store(generation, Ordering::Release);
    shared.connected.store(true, Ordering::Release);
    shared.wake();
    let mut previous_sequence = 0;
    loop {
        let idle_start = Instant::now();
        let mut frame_start = None;
        let mut bytes = [0; HEADER_BYTES];
        read_bounded(
            stream,
            &mut bytes,
            shared,
            generation,
            idle_start,
            &mut frame_start,
            timeouts,
        )?;
        let header = Header::parse(&bytes)?;
        if header.sequence <= previous_sequence {
            return Err(invalid("sequence must strictly increase on a connection"));
        }
        let length = header.payload_len()?;
        let mut rgba = Vec::new();
        rgba.try_reserve_exact(length)
            .map_err(|_| io::Error::other("frame allocation failed"))?;
        rgba.resize(length, 0);
        read_bounded(
            stream,
            &mut rgba,
            shared,
            generation,
            idle_start,
            &mut frame_start,
            timeouts,
        )?;
        if !shared.current(generation) {
            return Ok(());
        }
        let tight_stride = header.width as usize * 4;
        if header.stride as usize != tight_stride {
            for row in 1..header.height as usize {
                let source = row * header.stride as usize;
                rgba.copy_within(source..source + tight_stride, row * tight_stride);
            }
            rgba.truncate(tight_stride * header.height as usize);
        }
        previous_sequence = header.sequence;
        let frame = Frame {
            connection_epoch,
            header,
            rgba,
        };
        // Only the worker blocks on this lock; the render thread uses try_lock.
        // Replacing rather than queueing bounds memory under a slow compositor.
        if let Ok(mut latest) = shared.latest.lock() {
            *latest = Some((generation, frame));
        } else {
            return Err(io::Error::other("frame handoff poisoned"));
        }
        shared.wake();
    }
}

fn read_bounded(
    stream: &mut impl Read,
    mut destination: &mut [u8],
    shared: &Shared,
    generation: u64,
    idle_start: Instant,
    frame_start: &mut Option<Instant>,
    timeouts: Timeouts,
) -> io::Result<()> {
    while !destination.is_empty() {
        if !shared.current(generation) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "endpoint stopped or resized",
            ));
        }
        if frame_start.is_some_and(|start| start.elapsed() >= timeouts.frame)
            || (frame_start.is_none() && idle_start.elapsed() >= timeouts.idle)
        {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "producer frame/idle deadline",
            ));
        }
        match stream.read(destination) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "producer disconnected",
                ));
            }
            Ok(count) => {
                if frame_start.is_none() {
                    *frame_start = Some(Instant::now());
                }
                destination = &mut destination[count..];
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error),
        }
    }
    // A final successful read may have crossed the deadline. Never publish it.
    if frame_start.is_some_and(|start| start.elapsed() >= timeouts.frame) {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "producer frame deadline",
        ));
    }
    Ok(())
}

struct SocketOwner {
    path: PathBuf,
    identity: (u64, u64),
}

impl SocketOwner {
    fn new(path: PathBuf) -> io::Result<Self> {
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.file_type().is_socket() || metadata.uid() != current_uid() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "socket ownership changed during bind",
            ));
        }
        Ok(Self {
            path,
            identity: identity(&metadata),
        })
    }
}

impl Drop for SocketOwner {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path)
            .ok()
            .is_some_and(|metadata| {
                metadata.file_type().is_socket() && identity(&metadata) == self.identity
            })
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn identity(metadata: &Metadata) -> (u64, u64) {
    (metadata.dev(), metadata.ino())
}

fn current_uid() -> u32 {
    // SAFETY: geteuid has no arguments or memory-access preconditions.
    unsafe { libc::geteuid() }
}

fn prepare_parent(path: &Path) -> io::Result<()> {
    if !path.is_absolute() || path.file_name().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket requires an absolute pathname",
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "socket has no parent"))?;
    match fs::symlink_metadata(parent) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            // Only create the final private directory, not a chain whose
            // intermediate ownership and permissions would be ambiguous.
            DirBuilder::new().mode(0o700).create(parent)?;
        }
        Err(error) => return Err(error),
    }
    let metadata = fs::symlink_metadata(parent)?;
    if !metadata.file_type().is_dir()
        || metadata.uid() != current_uid()
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "socket parent must be a private same-user directory (0700), not a symlink",
        ));
    }
    Ok(())
}

fn remove_stale_socket(path: &Path) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_socket() || metadata.uid() != current_uid() {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "endpoint is not a same-user stale socket",
        ));
    }
    if !socket_is_definitely_stale(path)? {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "another visual consumer owns the endpoint",
        ));
    }
    if identity(&fs::symlink_metadata(path)?) != identity(&metadata) {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "socket replaced during stale check",
        ));
    }
    fs::remove_file(path)
}

/// A nonblocking connect is enough to prove ECONNREFUSED. In-progress, full
/// backlog, permission/resource failures and timeouts never prove staleness.
fn socket_is_definitely_stale(path: &Path) -> io::Result<bool> {
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: zero is a valid initial representation of sockaddr_un.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.len() >= address.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket pathname too long",
        ));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (destination, source) in address.sun_path.iter_mut().zip(bytes) {
        *destination = *source as libc::c_char;
    }
    // SAFETY: socket has no memory-access preconditions and returns a new fd.
    let raw = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: raw is newly returned, valid and exclusively owned here.
    let socket = unsafe { OwnedFd::from_raw_fd(raw) };
    let length =
        (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as libc::socklen_t;
    // SAFETY: address is initialized and length fits its allocated storage.
    let result = unsafe {
        libc::connect(
            socket.as_raw_fd(),
            (&address as *const libc::sockaddr_un).cast(),
            length,
        )
    };
    if result == 0 {
        return Ok(false);
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ECONNREFUSED) => Ok(true),
        Some(libc::EINPROGRESS | libc::EAGAIN | libc::EALREADY) => Ok(false),
        _ => Err(error),
    }
}

fn peer_is_current_user(stream: &UnixStream) -> bool {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: pointers identify writable storage of the stated length; the fd
    // is borrowed from a live UnixStream for the duration of getsockopt.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    result == 0
        && length as usize == std::mem::size_of::<libc::ucred>()
        && credentials.uid == current_uid()
}

#[cfg(test)]
mod tests;
