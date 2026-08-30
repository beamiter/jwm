// src/backends/eventfd.rs
#![cfg(feature = "eventfd")]

use super::common::{RegisterOutcome, SyncBackend, WaiterGate};
use nix::errno::Errno;
use nix::fcntl::{fcntl, FcntlArg};
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::sys::socket::{
    accept4, bind, connect, getsockopt, listen, recvmsg, sendmsg, socket, sockopt, AddressFamily,
    Backlog, ControlMessage, ControlMessageOwned, MsgFlags, SockFlag, SockType, UnixAddr,
};
use nix::unistd;
use std::ffi::CString;
use std::hint;
use std::io::{Error, ErrorKind, Result};
use std::io::{IoSlice, IoSliceMut};
use std::os::fd::FromRawFd;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// UNIX sockaddr_un 路径上限为 108 字节
const UNIX_SOCK_MAX: usize = 108;
const FD_PASS_MARKER: u8 = 0xE5;
/// Linux caps one SCM_RIGHTS transfer at SCM_MAX_FD descriptors. Reserving the
/// full capacity lets us own and close every fd in an invalid oversized reply.
const SCM_RIGHTS_MAX_FDS: usize = 253;
static SOCKET_NONCE: AtomicU64 = AtomicU64::new(0);

/// `is_ready` 的取值：0 = 未发布，1 = 可连接，2 = 创建者已关闭。
const READY_PENDING: u32 = 0;
const READY_LISTENING: u32 = 1;
const READY_CLOSED: u32 = 2;

/// 等待创建者发布 socket 路径的最长时间，与 fd 接收侧的重试窗口对齐。
const READY_WAIT_TIMEOUT: Duration = Duration::from_millis(200);

/// 自旋阶段每多少次迭代检查一次超时。
const SPIN_TIMEOUT_CHECK_INTERVAL: u32 = 1024;

// ── EventFdHeader ─────────────────────────────────────────────────────────────
//
// sequence + waiters 组成 SeqCst 注册握手：无人等待时 signal 不必进入内核，
// 同时不会因读到旧 waiter 计数而丢失正在注册的唤醒。
#[repr(C, align(8))]
pub struct EventFdHeader {
    // 创建者写入路径后，把 ready 置为 true，打开者据此读取路径
    is_ready: AtomicU32,
    /// 正在 poll() 等待消息 fd 的线程数（含本进程）
    pub message_waiters: AtomicI32,
    /// 正在 poll() 等待命令 fd 的线程数
    pub command_waiters: AtomicI32,
    message_sequence: AtomicU32,
    command_sequence: AtomicU32,
    // 以 0 结尾的 C 字节串，未用完则补 0
    sock_path: [u8; UNIX_SOCK_MAX],
}

pub struct EventFdBackend {
    header: *mut EventFdHeader,

    // 本进程可用的 eventfd（消息、命令）
    local_message_fd: Option<OwnedFd>,
    local_command_fd: Option<OwnedFd>,

    // 仅创建者持有：用于关闭监听线程
    listener_stop: Option<Arc<AtomicBool>>,
    listener_thread: Option<JoinHandle<()>>,
    sock_path: Option<PathBuf>,
    sock_dir: Option<PathBuf>,
}

unsafe impl Send for EventFdBackend {}
unsafe impl Sync for EventFdBackend {}

impl EventFdBackend {
    pub fn new() -> Self {
        Self {
            header: std::ptr::null_mut(),
            local_message_fd: None,
            local_command_fd: None,
            listener_stop: None,
            listener_thread: None,
            sock_path: None,
            sock_dir: None,
        }
    }

    fn ensure_initialized(&self) -> Result<()> {
        debug_assert!(!self.header.is_null(), "eventfd backend not initialized");
        if self.header.is_null() {
            return Err(Error::new(
                ErrorKind::NotConnected,
                "eventfd backend is not initialized",
            ));
        }
        Ok(())
    }

    fn write_u64(fd: BorrowedFd<'_>, v: u64) -> Result<()> {
        let bytes = v.to_ne_bytes();
        match unistd::write(fd, &bytes) {
            Ok(8) => Ok(()),
            Ok(written) => Err(Error::new(
                ErrorKind::WriteZero,
                format!("short eventfd write: {written} bytes"),
            )),
            Err(Errno::EAGAIN) => Ok(()),
            Err(e) => Err(Error::other(e)),
        }
    }

    fn signal_if_waiting(
        fd: Option<&OwnedFd>,
        waiters: &AtomicI32,
        sequence: &AtomicU32,
    ) -> Result<()> {
        let gate = WaiterGate { sequence, waiters };
        if gate.signal() {
            if let Some(fd) = fd {
                Self::write_u64(fd.as_fd(), 1)?;
            }
        }
        Ok(())
    }

    fn consume_one_eventfd(fd: BorrowedFd<'_>) {
        // EFD_SEMAPHORE 保证每次 read 只消费一个通知，不会把其他
        // consumer 对应的累计计数一次清空。
        let mut buf = [0u8; 8];
        let _ = unistd::read(fd, &mut buf);
    }

    fn poll_fd(fd: BorrowedFd<'_>, timeout: Option<Duration>) -> Result<bool> {
        let pfd = PollFd::new(fd, PollFlags::POLLIN);
        let to = timeout.map_or(PollTimeout::NONE, |d| {
            // 毫秒数向上取整：剩余不足 1ms 时截断为 0 会让尾段退化成
            // 忙轮询。轻微过睡由外层的整体 deadline 检查兜住。
            let millis = d.as_millis() + u128::from(d.subsec_nanos() % 1_000_000 != 0);
            // 有限 Duration 绝不能因整数转换溢出而变成无限等待。
            PollTimeout::try_from(millis).unwrap_or(PollTimeout::MAX)
        });
        match poll(&mut [pfd], to) {
            Ok(0) => Ok(false),
            Ok(_) => Ok(true),
            Err(Errno::EINTR) => Err(ErrorKind::Interrupted.into()),
            Err(e) => Err(Error::other(e)),
        }
    }

    /// Wait for one socket readiness edge without ever extending `deadline`.
    ///
    /// `poll(2)` can return early for a signal, and its millisecond timeout is
    /// rounded up, so every iteration rechecks the same monotonic deadline.
    fn poll_socket_until(fd: BorrowedFd<'_>, events: PollFlags, deadline: Instant) -> Result<bool> {
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Ok(false);
            }
            let remaining = deadline - now;
            let millis =
                remaining.as_millis() + u128::from(remaining.subsec_nanos() % 1_000_000 != 0);
            let timeout = PollTimeout::try_from(millis).unwrap_or(PollTimeout::MAX);
            let mut descriptor = PollFd::new(fd, events);
            match poll(std::slice::from_mut(&mut descriptor), timeout) {
                Ok(0) => {
                    if Instant::now() >= deadline {
                        return Ok(false);
                    }
                }
                Ok(_) if descriptor.revents().is_some_and(|ready| !ready.is_empty()) => {
                    return Ok(true);
                }
                Ok(_) => {}
                Err(Errno::EINTR) => continue,
                Err(error) => return Err(Error::other(error)),
            }
        }
    }

    fn set_header_sock_path(header: *mut EventFdHeader, path: &Path) -> Result<()> {
        let cstr = CString::new(
            path.as_os_str()
                .to_str()
                .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "Invalid socket path"))?,
        )
        .map_err(|e| Error::new(ErrorKind::InvalidInput, format!("Bad path: {e}")))?;
        let bytes = cstr.as_bytes_with_nul();
        if bytes.len() > UNIX_SOCK_MAX {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Socket path too long for sockaddr_un",
            ));
        }
        unsafe {
            let path_ptr = std::ptr::addr_of_mut!((*header).sock_path).cast::<u8>();
            std::ptr::write_bytes(path_ptr, 0, UNIX_SOCK_MAX);
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), path_ptr, bytes.len());
            (*header).is_ready.store(READY_LISTENING, Ordering::Release);
        }
        Ok(())
    }

    fn get_header_sock_path(header: *mut EventFdHeader) -> Result<PathBuf> {
        unsafe {
            match (*header).is_ready.load(Ordering::Acquire) {
                READY_PENDING => {
                    return Err(Error::new(
                        ErrorKind::WouldBlock,
                        "Backend not ready (socket path not published)",
                    ));
                }
                READY_LISTENING => {}
                READY_CLOSED => {
                    return Err(Error::new(
                        ErrorKind::NotConnected,
                        "eventfd creator has shut down; new openers cannot obtain the eventfds",
                    ));
                }
                value => {
                    return Err(Error::new(
                        ErrorKind::InvalidData,
                        format!("Invalid eventfd ready state: {value}"),
                    ));
                }
            }
            Self::decode_header_sock_path(header)
        }
    }

    unsafe fn decode_header_sock_path(header: *mut EventFdHeader) -> Result<PathBuf> {
        // SAFETY: callers provide a live, fully initialized EventFdHeader.
        let buf = unsafe { &(*header).sock_path };
        let len = buf.iter().position(|&byte| byte == 0).ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidData,
                "eventfd socket path is not NUL-terminated",
            )
        })?;
        if len == 0 {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "eventfd socket path is empty",
            ));
        }
        let path = PathBuf::from(
            std::str::from_utf8(&buf[..len])
                .map_err(|error| Error::new(ErrorKind::InvalidData, error))?,
        );
        if !path.is_absolute() {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "eventfd socket path is not absolute",
            ));
        }
        Ok(path)
    }

    /// Best-effort removal of the filesystem socket left by an abruptly
    /// exited creator.
    ///
    /// The path comes from shared memory, so validate the exact directory
    /// shape, ownership, modes, and socket file type before deleting anything.
    ///
    /// # Safety
    ///
    /// `backend_ptr` must address a fully initialized `EventFdHeader` that
    /// remains mapped for the duration of this call.
    pub(crate) unsafe fn cleanup_stale_socket(
        backend_ptr: *mut u8,
        creator_pid: u32,
    ) -> Result<()> {
        if backend_ptr.is_null() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "eventfd backend pointer is null",
            ));
        }
        let header = backend_ptr.cast::<EventFdHeader>();
        // SAFETY: guaranteed by the caller contract above.
        let ready = unsafe { (*header).is_ready.load(Ordering::Acquire) };
        if !matches!(ready, READY_LISTENING | READY_CLOSED) {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "stale eventfd backend has no published socket",
            ));
        }
        // SAFETY: guaranteed by the caller contract above.
        let socket_path = unsafe { Self::decode_header_sock_path(header)? };
        if socket_path.file_name().and_then(|name| name.to_str()) != Some("fd.sock") {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "stale eventfd socket has an unexpected filename",
            ));
        }
        let socket_dir = socket_path.parent().ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidData,
                "stale eventfd socket has no parent directory",
            )
        })?;
        let directory_name = socket_dir
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::InvalidData,
                    "stale eventfd socket directory has an invalid name",
                )
            })?;
        let suffix = directory_name
            .strip_prefix(&format!("srb-{creator_pid}-"))
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::InvalidData,
                    "stale eventfd socket directory does not match its creator",
                )
            })?;
        let mut components = suffix.split('-');
        let timestamp_is_valid = components
            .next()
            .is_some_and(|value| value.parse::<u128>().is_ok());
        let nonce_is_valid = components
            .next()
            .is_some_and(|value| value.parse::<u64>().is_ok());
        if !timestamp_is_valid || !nonce_is_valid || components.next().is_some() {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "stale eventfd socket directory has an invalid generated name",
            ));
        }

        let effective_uid = unsafe { libc::geteuid() };
        let directory_metadata = match std::fs::symlink_metadata(socket_dir) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        if !directory_metadata.is_dir()
            || directory_metadata.uid() != effective_uid
            || directory_metadata.mode() & 0o777 != 0o700
        {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "stale eventfd socket directory failed ownership or mode validation",
            ));
        }

        match std::fs::symlink_metadata(&socket_path) {
            Ok(metadata)
                if metadata.file_type().is_socket()
                    && metadata.uid() == effective_uid
                    && metadata.mode() & 0o777 == 0o600 =>
            {
                std::fs::remove_file(&socket_path)?;
            }
            Ok(_) => {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "stale eventfd socket failed type, ownership, or mode validation",
                ));
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }

        match std::fs::remove_dir(socket_dir) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// 在属主私有（0700）目录内创建 socket 路径。
    ///
    /// 目录级隔离消除两个多用户机器上的风险：bind 与 chmod 之间的
    /// 权限窗口，以及可预测路径被他人抢先 bind 的拒绝服务。优先使用
    /// `$XDG_RUNTIME_DIR`（本就是 0700 的属主目录），退化到 `/tmp`。
    /// 返回 (目录, socket 路径)。
    fn create_socket_dir() -> Result<(PathBuf, PathBuf)> {
        use std::os::unix::fs::DirBuilderExt;

        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|base| base.is_absolute() && base.is_dir())
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        let pid = std::process::id();
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let nonce = SOCKET_NONCE.fetch_add(1, Ordering::Relaxed);
        let dir = base.join(format!("srb-{pid}-{ts}-{nonce}"));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .map_err(|error| {
                Error::new(
                    error.kind(),
                    format!("failed to create private eventfd socket directory: {error}"),
                )
            })?;
        if let Err(error) = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)) {
            let _ = std::fs::remove_dir(&dir);
            return Err(Error::new(
                error.kind(),
                format!("failed to secure eventfd socket directory: {error}"),
            ));
        }
        let sock_path = dir.join("fd.sock");
        Ok((dir, sock_path))
    }

    /// 校验对端与本进程属于同一有效 UID，防止其他用户拿到 eventfd 后
    /// 制造虚假唤醒或抢先消费通知 token。
    fn peer_is_same_user(cli_fd: &OwnedFd) -> bool {
        use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
        match getsockopt(cli_fd, PeerCredentials) {
            Ok(credentials) => credentials.uid() == unsafe { libc::geteuid() },
            Err(error) => {
                log::warn!("SO_PEERCRED failed, rejecting fd-pass connection: {error}");
                false
            }
        }
    }

    fn send_eventfds(cli_fd: RawFd, message_fd: RawFd, command_fd: RawFd) -> nix::Result<usize> {
        let iov = [IoSlice::new(&[FD_PASS_MARKER])];
        let fds = [message_fd, command_fd];
        let cmsg = [ControlMessage::ScmRights(&fds)];
        // The opener may disconnect after accept(); report EPIPE to the
        // listener instead of letting SIGPIPE terminate the creator process.
        sendmsg::<nix::sys::socket::UnixAddr>(cli_fd, &iov, &cmsg, MsgFlags::MSG_NOSIGNAL, None)
    }

    fn create_eventfd_owned() -> Result<OwnedFd> {
        let flags = libc::EFD_NONBLOCK | libc::EFD_CLOEXEC | libc::EFD_SEMAPHORE;
        let fd = unsafe { libc::eventfd(0, flags) };
        if fd < 0 {
            return Err(Error::last_os_error());
        }
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    fn dup_cloexec(fd: &OwnedFd) -> Result<OwnedFd> {
        let duplicated = fcntl(fd, FcntlArg::F_DUPFD_CLOEXEC(0)).map_err(Error::other)?;
        Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
    }

    /// Validate the exact descriptor contract assumed by wait/signal paths.
    /// In particular, `consume_one_eventfd` relies on EFD_SEMAPHORE so one
    /// waiter cannot drain notifications reserved for other waiters.
    fn validate_received_eventfd(fd: &OwnedFd) -> Result<u64> {
        let status_flags = fcntl(fd, FcntlArg::F_GETFL).map_err(Error::other)?;
        if status_flags & libc::O_NONBLOCK == 0 {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "received eventfd is not non-blocking",
            ));
        }
        let descriptor_flags = fcntl(fd, FcntlArg::F_GETFD).map_err(Error::other)?;
        if descriptor_flags & libc::FD_CLOEXEC == 0 {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "received eventfd is not close-on-exec",
            ));
        }

        let fdinfo_path = Path::new("/proc/self/fdinfo").join(fd.as_raw_fd().to_string());
        let fdinfo = std::fs::read_to_string(&fdinfo_path).map_err(|error| {
            Error::new(
                ErrorKind::InvalidData,
                format!("failed to inspect received eventfd: {error}"),
            )
        })?;
        let uses_semaphore_mode = fdinfo.lines().any(|line| {
            line.split_once(':').is_some_and(|(key, value)| {
                key.trim() == "eventfd-semaphore" && value.trim() == "1"
            })
        });
        if !uses_semaphore_mode {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "received descriptor is not an EFD_SEMAPHORE eventfd",
            ));
        }
        fdinfo
            .lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                if key.trim() == "eventfd-id" {
                    value.trim().parse::<u64>().ok()
                } else {
                    None
                }
            })
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::InvalidData,
                    "received eventfd has no valid kernel identity",
                )
            })
    }

    fn spawn_listener_thread(
        sock_path: PathBuf,
        msg_fd: OwnedFd,
        cmd_fd: OwnedFd,
        stop: Arc<AtomicBool>,
    ) -> Result<JoinHandle<()>> {
        let backlog = Backlog::new(8).map_err(Error::other)?;
        let srv = socket(
            AddressFamily::Unix,
            SockType::Stream,
            SockFlag::SOCK_CLOEXEC | SockFlag::SOCK_NONBLOCK,
            None,
        )
        .map_err(Error::other)?;

        let addr = UnixAddr::new(&sock_path).map_err(Error::other)?;
        bind(srv.as_raw_fd(), &addr).map_err(Error::other)?;
        if let Err(error) =
            std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o600))
        {
            let _ = std::fs::remove_file(&sock_path);
            return Err(error);
        }
        if let Err(error) = listen(&srv, backlog).map_err(Error::other) {
            let _ = std::fs::remove_file(&sock_path);
            return Err(error);
        }

        let listener_path = sock_path.clone();
        let listener = std::thread::Builder::new()
            .name("srb_eventfd_fdpass".to_string())
            .spawn(move || {
                let msg_fd_raw = msg_fd.as_raw_fd();
                let cmd_fd_raw = cmd_fd.as_raw_fd();

                while !stop.load(Ordering::Acquire) {
                    match accept4(srv.as_raw_fd(), SockFlag::SOCK_CLOEXEC) {
                        Ok(cli_fd) => {
                            let cli_fd = unsafe { OwnedFd::from_raw_fd(cli_fd) };
                            if !Self::peer_is_same_user(&cli_fd) {
                                log::warn!("rejected eventfd fd-pass connection from another user");
                                continue;
                            }
                            if let Err(e) =
                                Self::send_eventfds(cli_fd.as_raw_fd(), msg_fd_raw, cmd_fd_raw)
                            {
                                log::warn!("sendmsg(SCM_RIGHTS) failed: {e}");
                            }
                        }
                        Err(Errno::EAGAIN) => {
                            // ── 优化：用 poll(1ms) 代替 sleep(10ms) ──────────────
                            // 既能快速响应新连接（POLLIN），又给 stop 标志留出
                            // 1ms 内的检查窗口，使 cleanup 的等待时间从 20ms→3ms。
                            let borrowed = unsafe { BorrowedFd::borrow_raw(srv.as_raw_fd()) };
                            let mut pfd = PollFd::new(borrowed, PollFlags::POLLIN);
                            let _ = poll(
                                std::slice::from_mut(&mut pfd),
                                nix::poll::PollTimeout::from(1u8),
                            );
                        }
                        Err(e) => {
                            log::warn!("eventfd listener accept error: {e}");
                            std::thread::sleep(Duration::from_millis(50));
                        }
                    }
                }

                let _ = std::fs::remove_file(&listener_path);
            });

        match listener {
            Ok(listener) => Ok(listener),
            Err(error) => {
                let _ = std::fs::remove_file(&sock_path);
                Err(Error::other(error))
            }
        }
    }

    fn poke_listener(sock_path: &Path) {
        let Ok(addr) = UnixAddr::new(sock_path) else {
            return;
        };
        let Ok(client) = socket(
            AddressFamily::Unix,
            SockType::Stream,
            SockFlag::SOCK_CLOEXEC | SockFlag::SOCK_NONBLOCK,
            None,
        ) else {
            return;
        };
        let _ = connect(client.as_raw_fd(), &addr);
    }

    fn receive_fds_from_server(sock_path: &Path) -> Result<(OwnedFd, OwnedFd)> {
        let deadline = Instant::now() + READY_WAIT_TIMEOUT;
        let addr = UnixAddr::new(sock_path).map_err(Error::other)?;
        let mut last_connect_error = None;
        let cli = loop {
            if Instant::now() >= deadline {
                let detail = last_connect_error
                    .as_ref()
                    .map_or_else(String::new, |error: &Error| format!(": {error}"));
                return Err(Error::new(
                    ErrorKind::TimedOut,
                    format!("timed out connecting to the eventfd fd-pass socket{detail}"),
                ));
            }

            let cli = socket(
                AddressFamily::Unix,
                SockType::Stream,
                SockFlag::SOCK_CLOEXEC | SockFlag::SOCK_NONBLOCK,
                None,
            )
            .map_err(Error::other)?;
            match connect(cli.as_raw_fd(), &addr) {
                Ok(()) | Err(Errno::EISCONN) => break cli,
                Err(Errno::EINPROGRESS | Errno::EALREADY | Errno::EINTR) => {
                    if !Self::poll_socket_until(cli.as_fd(), PollFlags::POLLOUT, deadline)? {
                        return Err(Error::new(
                            ErrorKind::TimedOut,
                            "timed out connecting to the eventfd fd-pass socket",
                        ));
                    }
                    let pending = getsockopt(&cli, sockopt::SocketError).map_err(Error::other)?;
                    if pending == 0 {
                        break cli;
                    }
                    last_connect_error = Some(Error::from_raw_os_error(pending));
                }
                Err(error) => last_connect_error = Some(Error::other(error)),
            }

            let now = Instant::now();
            if now < deadline {
                std::thread::sleep((deadline - now).min(Duration::from_millis(10)));
            }
        };

        loop {
            if Instant::now() >= deadline {
                return Err(Error::new(
                    ErrorKind::TimedOut,
                    "timed out waiting for eventfds from the fd-pass server",
                ));
            }
            match Self::try_receive_fds(cli.as_raw_fd()) {
                Ok(Some(fds)) => return Ok(fds),
                Ok(None) => {
                    if !Self::poll_socket_until(cli.as_fd(), PollFlags::POLLIN, deadline)? {
                        return Err(Error::new(
                            ErrorKind::TimedOut,
                            "timed out waiting for eventfds from the fd-pass server",
                        ));
                    }
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }

    /// Perform one non-blocking SCM_RIGHTS receive attempt.
    ///
    /// `Ok(None)` is the ordinary EAGAIN state; EINTR remains distinct so the
    /// caller can retry while checking the shared connection deadline.
    fn try_receive_fds(fd: RawFd) -> Result<Option<(OwnedFd, OwnedFd)>> {
        let mut buf = [0u8; 1];
        let mut iov = [IoSliceMut::new(&mut buf)];
        let mut cmsgspace = nix::cmsg_space!([RawFd; SCM_RIGHTS_MAX_FDS]);

        let msg = match recvmsg::<UnixAddr>(
            fd,
            &mut iov,
            Some(&mut cmsgspace),
            MsgFlags::MSG_CMSG_CLOEXEC | MsgFlags::MSG_DONTWAIT,
        ) {
            Ok(message) => message,
            Err(Errno::EAGAIN) => return Ok(None),
            Err(Errno::EINTR) => return Err(ErrorKind::Interrupted.into()),
            Err(error) => return Err(Error::other(error)),
        };

        // 缓冲覆盖 Linux 单次 SCM_RIGHTS 的完整上限，因此内核安装的描述符
        // 都可被迭代。立即转为 OwnedFd，使数量或标志校验失败时也能全部关闭。
        let mut received_fds: Vec<OwnedFd> = Vec::new();
        let cmsgs = msg.cmsgs().map_err(Error::other)?;
        for cmsg in cmsgs {
            if let ControlMessageOwned::ScmRights(raw_fds) = cmsg {
                received_fds.extend(
                    raw_fds
                        .into_iter()
                        .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) }),
                );
            }
        }

        if msg.bytes == 0 {
            return Err(Error::new(
                ErrorKind::UnexpectedEof,
                "server closed before sending fds",
            ));
        }
        if msg.flags.contains(MsgFlags::MSG_CTRUNC) {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "ancillary data truncated",
            ));
        }
        if buf[0] != FD_PASS_MARKER {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!("invalid eventfd fd-pass marker: 0x{:02x}", buf[0]),
            ));
        }

        if received_fds.len() != 2 {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!(
                    "Expected exactly 2 fds from server, received {}",
                    received_fds.len()
                ),
            ));
        }

        let message_eventfd_id = Self::validate_received_eventfd(&received_fds[0])?;
        let command_eventfd_id = Self::validate_received_eventfd(&received_fds[1])?;
        if message_eventfd_id == command_eventfd_id {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "message and command channels alias the same eventfd",
            ));
        }

        let mut received_fds = received_fds.into_iter();
        let owned_msg = received_fds.next().expect("length checked above");
        let owned_cmd = received_fds.next().expect("length checked above");
        Ok(Some((owned_msg, owned_cmd)))
    }

    fn wait_on_eventfd(
        &self,
        is_message: bool,
        has_data: impl Fn() -> bool,
        adaptive_poll_spins: u32,
        timeout: Option<Duration>,
    ) -> Result<bool> {
        self.ensure_initialized()?;
        let started = std::time::Instant::now();
        if timeout != Some(Duration::ZERO) {
            for spin in 0..adaptive_poll_spins {
                if has_data() {
                    return Ok(true);
                }
                if spin % SPIN_TIMEOUT_CHECK_INTERVAL == SPIN_TIMEOUT_CHECK_INTERVAL - 1
                    && timeout.is_some_and(|limit| started.elapsed() >= limit)
                {
                    break;
                }
                hint::spin_loop();
            }
        }
        if has_data() {
            return Ok(true);
        }

        let opt_fd = if is_message {
            self.local_message_fd.as_ref()
        } else {
            self.local_command_fd.as_ref()
        };
        // fd 缺失说明 init 未完成或已失败；如实报错，绝不把
        // 「无法等待」伪装成一次超时（尤其 timeout=None 语义是无限阻塞）。
        let Some(ofd) = opt_fd else {
            return Err(Error::new(
                ErrorKind::NotConnected,
                "eventfd backend has no local eventfd; initialization did not complete",
            ));
        };

        let gate = unsafe {
            if is_message {
                WaiterGate {
                    sequence: &(*self.header).message_sequence,
                    waiters: &(*self.header).message_waiters,
                }
            } else {
                WaiterGate {
                    sequence: &(*self.header).command_sequence,
                    waiters: &(*self.header).command_waiters,
                }
            }
        };

        loop {
            if has_data() {
                return Ok(true);
            }
            if timeout.is_some_and(|limit| started.elapsed() >= limit) {
                return Ok(false);
            }

            match gate.register(&has_data)? {
                RegisterOutcome::DataReady => return Ok(true),
                RegisterOutcome::Retry => continue,
                RegisterOutcome::Registered { registration, .. } => {
                    let remaining = timeout.map(|limit| limit.saturating_sub(started.elapsed()));
                    let poll_result = Self::poll_fd(ofd.as_fd(), remaining);
                    drop(registration);
                    match poll_result {
                        Ok(true) => {
                            // EFD_SEMAPHORE 保证每次只消费一个 token。若该
                            // token 已过时，沿用原 deadline 重新注册并继续等待。
                            Self::consume_one_eventfd(ofd.as_fd());
                            if has_data() {
                                return Ok(true);
                            }
                            if timeout.is_some_and(|limit| started.elapsed() >= limit) {
                                return Ok(false);
                            }
                        }
                        Ok(false) => {
                            if has_data() {
                                return Ok(true);
                            }
                            // PollTimeout::MAX 是有限超长 timeout 的分段，不能
                            // 把这个分段到期误当作整个 Duration 已经到期。
                            if timeout.is_some_and(|limit| started.elapsed() < limit) {
                                continue;
                            }
                            return Ok(false);
                        }
                        Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                        Err(error) => {
                            log::warn!("poll(eventfd) error: {error}. Fallback to check state.");
                            return Ok(has_data());
                        }
                    }
                }
            }
        }
    }
}

impl SyncBackend for EventFdBackend {
    fn init(&mut self, is_creator: bool, backend_ptr: *mut u8) -> Result<()> {
        self.header = backend_ptr as *mut EventFdHeader;

        if is_creator {
            // 共享映射中的原子对象尚未构造；在发布映射前用
            // ptr::write + Atomic*::new 正式开始它们的生命期。
            unsafe {
                self.header.write(EventFdHeader {
                    is_ready: AtomicU32::new(0),
                    message_waiters: AtomicI32::new(0),
                    command_waiters: AtomicI32::new(0),
                    message_sequence: AtomicU32::new(0),
                    command_sequence: AtomicU32::new(0),
                    sock_path: [0; UNIX_SOCK_MAX],
                });
            }

            let msg_fd = Self::create_eventfd_owned()?;
            let cmd_fd = Self::create_eventfd_owned()?;

            let msg_fd_for_send = Self::dup_cloexec(&msg_fd)?;
            let cmd_fd_for_send = Self::dup_cloexec(&cmd_fd)?;

            let (sock_dir, sock_path) = Self::create_socket_dir()?;
            let stop = Arc::new(AtomicBool::new(false));
            let listener = match Self::spawn_listener_thread(
                sock_path.clone(),
                msg_fd_for_send,
                cmd_fd_for_send,
                stop.clone(),
            ) {
                Ok(listener) => listener,
                Err(error) => {
                    let _ = std::fs::remove_dir(&sock_dir);
                    return Err(error);
                }
            };

            // 只有 bind/chmod/listen/thread spawn 全部成功后才发布路径。
            if let Err(error) = Self::set_header_sock_path(self.header, &sock_path) {
                stop.store(true, Ordering::Release);
                Self::poke_listener(&sock_path);
                let _ = listener.join();
                let _ = std::fs::remove_file(&sock_path);
                let _ = std::fs::remove_dir(&sock_dir);
                return Err(error);
            }

            self.local_message_fd = Some(msg_fd);
            self.local_command_fd = Some(cmd_fd);
            self.listener_stop = Some(stop);
            self.listener_thread = Some(listener);
            self.sock_path = Some(sock_path);
            self.sock_dir = Some(sock_dir);
        } else {
            // 创建者发布路径与 opener 打开映射之间存在调度间隙；用带
            // deadline 的重试而不是固定自旋数，避免负载高时随机失败。
            let ready_deadline = std::time::Instant::now() + READY_WAIT_TIMEOUT;
            for _ in 0..10_000 {
                unsafe {
                    if (*self.header).is_ready.load(Ordering::Acquire) != READY_PENDING {
                        break;
                    }
                }
                hint::spin_loop();
            }
            while unsafe { (*self.header).is_ready.load(Ordering::Acquire) == READY_PENDING }
                && std::time::Instant::now() < ready_deadline
            {
                std::thread::sleep(Duration::from_millis(1));
            }

            let sock_path = Self::get_header_sock_path(self.header)?;
            let (msg_fd, cmd_fd) = Self::receive_fds_from_server(&sock_path)?;
            self.local_message_fd = Some(msg_fd);
            self.local_command_fd = Some(cmd_fd);
        }

        Ok(())
    }

    fn wait_for_message(
        &self,
        has_data: impl Fn() -> bool,
        spins: u32,
        timeout: Option<Duration>,
    ) -> Result<bool> {
        self.wait_on_eventfd(true, has_data, spins, timeout)
    }

    fn wait_for_command(
        &self,
        has_data: impl Fn() -> bool,
        spins: u32,
        timeout: Option<Duration>,
    ) -> Result<bool> {
        self.wait_on_eventfd(false, has_data, spins, timeout)
    }

    fn signal_message(&self) -> Result<()> {
        self.ensure_initialized()?;
        unsafe {
            Self::signal_if_waiting(
                self.local_message_fd.as_ref(),
                &(*self.header).message_waiters,
                &(*self.header).message_sequence,
            )
        }
    }

    fn signal_command(&self) -> Result<()> {
        self.ensure_initialized()?;
        unsafe {
            Self::signal_if_waiting(
                self.local_command_fd.as_ref(),
                &(*self.header).command_waiters,
                &(*self.header).command_sequence,
            )
        }
    }

    fn wake_all(&self) -> Result<()> {
        self.ensure_initialized()?;
        let (message_count, command_count) = unsafe {
            let message_gate = WaiterGate {
                sequence: &(*self.header).message_sequence,
                waiters: &(*self.header).message_waiters,
            };
            let command_gate = WaiterGate {
                sequence: &(*self.header).command_sequence,
                waiters: &(*self.header).command_waiters,
            };
            (
                message_gate.broadcast_count() as u64,
                command_gate.broadcast_count() as u64,
            )
        };
        let message_result = if message_count == 0 {
            Ok(())
        } else {
            self.local_message_fd
                .as_ref()
                .map_or(Ok(()), |fd| Self::write_u64(fd.as_fd(), message_count))
        };
        let command_result = if command_count == 0 {
            Ok(())
        } else {
            self.local_command_fd
                .as_ref()
                .map_or(Ok(()), |fd| Self::write_u64(fd.as_fd(), command_count))
        };
        message_result.and(command_result)
    }

    fn cleanup(&mut self, is_creator: bool) {
        if is_creator {
            // 先把 ready 状态标记为「已关闭」：此后到达的新 opener 得到明确的
            // NotConnected 错误，而不是对已关闭 socket 反复重连 200ms。
            // cleanup 在共享映射解除之前被调用（SharedRingBuffer::drop 的
            // 显式调用点），此时 header 指针仍然有效。
            if !self.header.is_null() {
                unsafe {
                    (*self.header)
                        .is_ready
                        .store(READY_CLOSED, Ordering::Release);
                }
            }
            if let Some(stop) = &self.listener_stop {
                stop.store(true, Ordering::Release);

                // ── 优化：主动连接 socket 唤醒监听线程，避免等满 poll 超时 ─────
                // 监听线程 poll(1ms)，正常情况下 3ms 内必定退出。
                // 改动前：sleep(20ms) 硬等。
                if let Some(path) = &self.sock_path {
                    Self::poke_listener(path);
                }
            }
            if let Some(listener) = self.listener_thread.take() {
                if listener.join().is_err() {
                    log::warn!("eventfd listener thread panicked during cleanup");
                }
            }
            if let Some(path) = &self.sock_path {
                let _ = std::fs::remove_file(path);
            }
            if let Some(dir) = &self.sock_dir {
                let _ = std::fs::remove_dir(dir);
            }
        }
        // OwnedFd 在 drop 时自动关闭
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::MaybeUninit;
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc;

    #[test]
    fn socket_directory_permissions_ignore_restrictive_umask() {
        const CHILD_MARKER_ENV: &str = "SHARED_STRUCTURES_EVENTFD_UMASK_MARKER";

        if let Some(marker) = std::env::var_os(CHILD_MARKER_ENV) {
            // umask is process-global, so exercise it only in this exact child
            // test, isolated from the parallel parent harness.
            let previous_umask = unsafe { libc::umask(0o777) };
            let created = EventFdBackend::create_socket_dir();
            unsafe { libc::umask(previous_umask) };

            let (directory, socket_path) = created.unwrap();
            let mode = std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777;
            let listener = UnixListener::bind(&socket_path);
            let bind_error = listener.as_ref().err().map(ToString::to_string);

            // Restore access before assertions so a failing regression does
            // not leave an unremovable test directory behind.
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
            drop(listener);
            let _ = std::fs::remove_file(&socket_path);
            std::fs::remove_dir(&directory).unwrap();

            assert_eq!(mode, 0o700);
            assert!(bind_error.is_none(), "socket bind failed: {bind_error:?}");
            std::fs::write(marker, b"completed").unwrap();
            return;
        }

        let marker = std::env::temp_dir().join(format!(
            "srb-eventfd-umask-marker-{}-{}",
            std::process::id(),
            SOCKET_NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        let test_name =
            "backends::eventfd::tests::socket_directory_permissions_ignore_restrictive_umask";
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg(test_name)
            .arg("--nocapture")
            .env(CHILD_MARKER_ENV, &marker)
            .status()
            .unwrap();
        let child_completed = marker.is_file();
        let _ = std::fs::remove_file(marker);

        assert!(status.success(), "restrictive-umask child failed: {status}");
        assert!(child_completed, "restrictive-umask child did not run");
    }

    #[test]
    fn signal_without_waiters_does_not_accumulate_tokens() {
        let mut storage = MaybeUninit::<EventFdHeader>::uninit();
        let header = storage.as_mut_ptr();
        let mut backend = EventFdBackend::new();
        backend.init(true, header.cast()).unwrap();

        backend.signal_message().unwrap();
        backend.signal_message().unwrap();
        let sequence = unsafe { (*header).message_sequence.load(Ordering::SeqCst) };
        let mut bytes = [0u8; 8];
        let read_result = unistd::read(
            backend.local_message_fd.as_ref().unwrap().as_fd(),
            &mut bytes,
        );
        backend.cleanup(true);

        assert_eq!(read_result, Err(Errno::EAGAIN));
        assert_eq!(sequence, 2);
    }

    #[test]
    fn fd_receive_timeout_covers_a_server_that_accepts_but_never_sends() {
        let socket_dir = std::env::temp_dir().join(format!(
            "srb-eventfd-timeout-test-{}-{}",
            std::process::id(),
            SOCKET_NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&socket_dir).unwrap();
        let socket_path = socket_dir.join("fd.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let (accepted_tx, accepted_rx) = mpsc::sync_channel(0);
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        let server = std::thread::spawn(move || {
            let (accepted, _) = listener.accept().unwrap();
            accepted_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(2));
            drop(accepted);
        });

        let client_path = socket_path.clone();
        let (result_tx, result_rx) = mpsc::sync_channel(0);
        let client = std::thread::spawn(move || {
            let started = Instant::now();
            let result = EventFdBackend::receive_fds_from_server(&client_path);
            result_tx.send((result, started.elapsed())).unwrap();
        });

        accepted_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("fake fd-pass server did not accept the client");
        let outcome = result_rx.recv_timeout(Duration::from_secs(1));
        let _ = release_tx.send(());
        server.join().unwrap();

        let (result, elapsed) = match outcome {
            Ok(outcome) => {
                client.join().unwrap();
                outcome
            }
            Err(error) => {
                // Do not let a regression that restores the unbounded recvmsg
                // block this test process while reporting the bounded failure.
                drop(client);
                panic!("fd receive did not honor its deadline: {error}");
            }
        };
        let error = result.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::TimedOut);
        assert!(
            elapsed >= READY_WAIT_TIMEOUT.saturating_sub(Duration::from_millis(20)),
            "fd receive timed out too early: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "fd receive exceeded its bounded deadline: {elapsed:?}"
        );

        let _ = std::fs::remove_file(&socket_path);
        let _ = std::fs::remove_dir(&socket_dir);
    }

    #[test]
    fn truncated_fd_pass_does_not_leak_received_descriptors() {
        const CHILD_MARKER_ENV: &str = "SHARED_STRUCTURES_EVENTFD_CTRUNC_MARKER";

        if let Some(marker) = std::env::var_os(CHILD_MARKER_ENV) {
            let (sender, receiver) = nix::sys::socket::socketpair(
                AddressFamily::Unix,
                SockType::Stream,
                None,
                SockFlag::SOCK_CLOEXEC,
            )
            .unwrap();
            let sent_fds = [
                EventFdBackend::create_eventfd_owned().unwrap(),
                EventFdBackend::create_eventfd_owned().unwrap(),
                EventFdBackend::create_eventfd_owned().unwrap(),
            ];
            let raw_fds = sent_fds.each_ref().map(AsRawFd::as_raw_fd);
            let before = std::fs::read_dir("/proc/self/fd").unwrap().count();

            for _ in 0..8 {
                let iov = [IoSlice::new(&[FD_PASS_MARKER])];
                let cmsg = [ControlMessage::ScmRights(&raw_fds)];
                let written = sendmsg::<UnixAddr>(
                    sender.as_raw_fd(),
                    &iov,
                    &cmsg,
                    MsgFlags::MSG_NOSIGNAL,
                    None,
                )
                .unwrap();
                assert_eq!(written, 1);
                let error = EventFdBackend::try_receive_fds(receiver.as_raw_fd()).unwrap_err();
                assert_eq!(error.kind(), ErrorKind::InvalidData);
            }

            let after = std::fs::read_dir("/proc/self/fd").unwrap().count();
            assert_eq!(
                after,
                before,
                "truncated SCM_RIGHTS leaked {} descriptors",
                after.saturating_sub(before)
            );
            std::fs::write(marker, b"completed").unwrap();
            return;
        }

        let marker = std::env::temp_dir().join(format!(
            "srb-eventfd-ctrunc-marker-{}-{}",
            std::process::id(),
            SOCKET_NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&marker);
        let test_name =
            "backends::eventfd::tests::truncated_fd_pass_does_not_leak_received_descriptors";
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg(test_name)
            .arg("--nocapture")
            .env(CHILD_MARKER_ENV, &marker)
            .status()
            .unwrap();
        let child_completed = marker.is_file();
        let _ = std::fs::remove_file(marker);

        assert!(status.success(), "eventfd CTRUNC child failed: {status}");
        assert!(child_completed, "eventfd CTRUNC child did not run");
    }

    #[test]
    fn fd_pass_rejects_an_invalid_protocol_marker() {
        let (sender, receiver) = nix::sys::socket::socketpair(
            AddressFamily::Unix,
            SockType::Stream,
            None,
            SockFlag::SOCK_CLOEXEC,
        )
        .unwrap();
        let message_fd = EventFdBackend::create_eventfd_owned().unwrap();
        let command_fd = EventFdBackend::create_eventfd_owned().unwrap();
        let raw_fds = [message_fd.as_raw_fd(), command_fd.as_raw_fd()];
        let iov = [IoSlice::new(&[0x00])];
        let cmsg = [ControlMessage::ScmRights(&raw_fds)];

        let written = sendmsg::<UnixAddr>(
            sender.as_raw_fd(),
            &iov,
            &cmsg,
            MsgFlags::MSG_NOSIGNAL,
            None,
        )
        .unwrap();
        assert_eq!(written, 1);

        let error = EventFdBackend::try_receive_fds(receiver.as_raw_fd()).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn fd_pass_rejects_eventfds_without_semaphore_mode() {
        let (sender, receiver) = nix::sys::socket::socketpair(
            AddressFamily::Unix,
            SockType::Stream,
            None,
            SockFlag::SOCK_CLOEXEC,
        )
        .unwrap();
        let create_plain_eventfd = || {
            let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
            assert!(
                fd >= 0,
                "eventfd creation failed: {}",
                Error::last_os_error()
            );
            unsafe { OwnedFd::from_raw_fd(fd) }
        };
        let message_fd = create_plain_eventfd();
        let command_fd = create_plain_eventfd();
        let raw_fds = [message_fd.as_raw_fd(), command_fd.as_raw_fd()];
        let iov = [IoSlice::new(&[FD_PASS_MARKER])];
        let cmsg = [ControlMessage::ScmRights(&raw_fds)];

        let written = sendmsg::<UnixAddr>(
            sender.as_raw_fd(),
            &iov,
            &cmsg,
            MsgFlags::MSG_NOSIGNAL,
            None,
        )
        .unwrap();
        assert_eq!(written, 1);

        let error = EventFdBackend::try_receive_fds(receiver.as_raw_fd()).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn fd_pass_rejects_aliased_message_and_command_eventfds() {
        let (sender, receiver) = nix::sys::socket::socketpair(
            AddressFamily::Unix,
            SockType::Stream,
            None,
            SockFlag::SOCK_CLOEXEC,
        )
        .unwrap();
        let shared_fd = EventFdBackend::create_eventfd_owned().unwrap();
        let raw_fds = [shared_fd.as_raw_fd(), shared_fd.as_raw_fd()];
        let iov = [IoSlice::new(&[FD_PASS_MARKER])];
        let cmsg = [ControlMessage::ScmRights(&raw_fds)];

        let written = sendmsg::<UnixAddr>(
            sender.as_raw_fd(),
            &iov,
            &cmsg,
            MsgFlags::MSG_NOSIGNAL,
            None,
        )
        .unwrap();
        assert_eq!(written, 1);

        let error = EventFdBackend::try_receive_fds(receiver.as_raw_fd()).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn listener_poke_does_not_block_on_a_full_backlog() {
        let socket_dir = PathBuf::from(format!(
            "/tmp/srb-eventfd-poke-test-{}-{}",
            std::process::id(),
            SOCKET_NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&socket_dir).unwrap();
        let socket_path = socket_dir.join("fd.sock");
        let address = UnixAddr::new(&socket_path).unwrap();
        let server = socket(
            AddressFamily::Unix,
            SockType::Stream,
            SockFlag::SOCK_CLOEXEC | SockFlag::SOCK_NONBLOCK,
            None,
        )
        .unwrap();
        bind(server.as_raw_fd(), &address).unwrap();
        listen(&server, Backlog::new(1).unwrap()).unwrap();

        let mut clients = Vec::new();
        let mut backlog_is_full = false;
        for _ in 0..64 {
            let client = socket(
                AddressFamily::Unix,
                SockType::Stream,
                SockFlag::SOCK_CLOEXEC | SockFlag::SOCK_NONBLOCK,
                None,
            )
            .unwrap();
            match connect(client.as_raw_fd(), &address) {
                Ok(()) | Err(Errno::EINPROGRESS | Errno::EALREADY) => clients.push(client),
                Err(Errno::EAGAIN) => {
                    backlog_is_full = true;
                    break;
                }
                Err(error) => panic!("failed to saturate Unix socket backlog: {error}"),
            }
        }
        assert!(backlog_is_full, "test did not saturate the listen backlog");

        let poke_path = socket_path.clone();
        let (finished_tx, finished_rx) = mpsc::sync_channel(0);
        let poke = std::thread::spawn(move || {
            EventFdBackend::poke_listener(&poke_path);
            finished_tx.send(()).unwrap();
        });
        let returned_before_drain = finished_rx.recv_timeout(Duration::from_millis(100)).is_ok();
        if !returned_before_drain {
            // Unblock the pre-fix implementation so a failing regression does
            // not strand this test process or leave filesystem residue.
            let accepted = accept4(server.as_raw_fd(), SockFlag::SOCK_CLOEXEC).unwrap();
            let accepted = unsafe { OwnedFd::from_raw_fd(accepted) };
            finished_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("blocked listener poke did not resume after backlog drain");
            drop(accepted);
        }
        poke.join().unwrap();

        drop(clients);
        drop(server);
        std::fs::remove_file(&socket_path).unwrap();
        std::fs::remove_dir(&socket_dir).unwrap();
        assert!(
            returned_before_drain,
            "listener poke blocked while the listen backlog was full"
        );
    }

    #[test]
    fn fd_pass_does_not_raise_sigpipe_when_opener_disconnects() {
        const CHILD_MARKER_ENV: &str = "SHARED_STRUCTURES_EVENTFD_SIGPIPE_MARKER";

        if let Some(marker) = std::env::var_os(CHILD_MARKER_ENV) {
            // Applications may restore SIGPIPE's default disposition. The
            // backend must suppress it per send instead of relying on Rust's
            // process-wide startup choice.
            unsafe {
                libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            }
            let (sender, peer) = nix::sys::socket::socketpair(
                AddressFamily::Unix,
                SockType::Stream,
                None,
                SockFlag::SOCK_CLOEXEC,
            )
            .unwrap();
            drop(peer);
            let message_fd = EventFdBackend::create_eventfd_owned().unwrap();
            let command_fd = EventFdBackend::create_eventfd_owned().unwrap();

            let error = EventFdBackend::send_eventfds(
                sender.as_raw_fd(),
                message_fd.as_raw_fd(),
                command_fd.as_raw_fd(),
            )
            .unwrap_err();
            assert_eq!(error, Errno::EPIPE);
            std::fs::write(marker, b"completed").unwrap();
            return;
        }

        let marker = PathBuf::from(format!(
            "/tmp/srb-eventfd-sigpipe-{}-{}",
            std::process::id(),
            SOCKET_NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&marker);
        let test_name =
            "backends::eventfd::tests::fd_pass_does_not_raise_sigpipe_when_opener_disconnects";
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg(test_name)
            .arg("--nocapture")
            .env(CHILD_MARKER_ENV, &marker)
            .status()
            .unwrap();
        let child_completed = marker.is_file();
        let _ = std::fs::remove_file(marker);

        assert!(status.success(), "eventfd SIGPIPE child failed: {status}");
        assert!(child_completed, "eventfd SIGPIPE child did not run");
    }
}
