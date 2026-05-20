// src/backends/eventfd.rs
#![cfg(feature = "eventfd")]

use super::common::SyncBackend;
use log::info;
use nix::errno::Errno;
use nix::fcntl::{fcntl, FcntlArg, FdFlag, OFlag};
use nix::poll::{poll, PollFd, PollFlags};
use nix::sys::socket::{
    accept, bind, connect, listen, recvmsg, sendmsg, socket, AddressFamily, Backlog,
    ControlMessage, ControlMessageOwned, MsgFlags, SockFlag, SockType, UnixAddr,
};
use nix::unistd;
use std::ffi::CString;
use std::hint;
use std::io::{Error, ErrorKind, Result};
use std::io::{IoSlice, IoSliceMut};
use std::os::fd::FromRawFd;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// UNIX sockaddr_un 路径上限为 108 字节
const UNIX_SOCK_MAX: usize = 108;

// ── EventFdHeader ─────────────────────────────────────────────────────────────
//
// 核心优化：新增 message_waiters / command_waiters 计数器（位于共享内存中），
// signal 路径在 waiters == 0 时跳过 write(eventfd) syscall。
// 改动前：每次 try_write_message 都触发一次 write() 内核调用（≈200–500 ns）。
// 改动后：只有对端真正在 poll() 等待时才发出 write()。
#[repr(C, align(8))]
pub struct EventFdHeader {
    // 创建者写入路径后，把 ready 置为 true，打开者据此读取路径
    is_ready: AtomicBool,
    _pad0: [u8; 3],
    /// 正在 poll() 等待消息 fd 的线程数（含本进程）
    pub message_waiters: AtomicI32,
    /// 正在 poll() 等待命令 fd 的线程数
    pub command_waiters: AtomicI32,
    _pad1: [u8; 4],
    // 以 0 结尾的 C 字节串，未用完则补 0
    sock_path: [u8; UNIX_SOCK_MAX],
}

pub struct EventFdBackend {
    header: *mut EventFdHeader,

    // 本进程可用的 eventfd（消息、命令）
    local_message_fd: Option<OwnedFd>,
    local_command_fd: Option<OwnedFd>,

    // 仅创建者持有：用于关闭监听线程
    is_creator: bool,
    listener_stop: Option<Arc<AtomicBool>>,
    sock_path: Option<PathBuf>,
}

unsafe impl Send for EventFdBackend {}
unsafe impl Sync for EventFdBackend {}

impl EventFdBackend {
    pub fn new() -> Self {
        Self {
            header: std::ptr::null_mut(),
            local_message_fd: None,
            local_command_fd: None,
            is_creator: false,
            listener_stop: None,
            sock_path: None,
        }
    }

    fn write_u64(fd: BorrowedFd<'_>, v: u64) -> Result<()> {
        let bytes = v.to_ne_bytes();
        match unistd::write(fd, &bytes) {
            Ok(_) => Ok(()),
            Err(Errno::EAGAIN) => Ok(()),
            Err(e) => Err(Error::new(ErrorKind::Other, e)),
        }
    }

    fn drain_eventfd(fd: BorrowedFd<'_>) {
        // fd 是 EFD_NONBLOCK，读不到数据时返回 EAGAIN，此处忽略即可
        let mut buf = [0u8; 8];
        let _ = unistd::read(fd, &mut buf);
    }

    fn poll_fd(fd: RawFd, timeout: Option<Duration>) -> Result<bool> {
        use nix::poll::PollTimeout;
        let borrowed_fd = unsafe { BorrowedFd::borrow_raw(fd) };
        let pfd = PollFd::new(borrowed_fd, PollFlags::POLLIN);
        let to = timeout.map_or(PollTimeout::NONE, |d| {
            nix::poll::PollTimeout::try_from(d.as_millis()).unwrap_or(PollTimeout::NONE)
        });
        match poll(&mut [pfd], to) {
            Ok(0) => Ok(false),
            Ok(_) => Ok(true),
            Err(e) => Err(Error::new(ErrorKind::Other, e)),
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
            (*header).sock_path.fill(0);
            (&mut (*header).sock_path)[..bytes.len()].copy_from_slice(bytes);
            (*header).is_ready.store(true, Ordering::Release);
        }
        Ok(())
    }

    fn get_header_sock_path(header: *mut EventFdHeader) -> Result<PathBuf> {
        unsafe {
            if !(*header).is_ready.load(Ordering::Acquire) {
                return Err(Error::new(
                    ErrorKind::WouldBlock,
                    "Backend not ready (socket path not published)",
                ));
            }
            let buf = &(*header).sock_path;
            let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            let s = std::str::from_utf8(&buf[..len])
                .map_err(|e| Error::new(ErrorKind::InvalidData, e))?;
            Ok(PathBuf::from(s))
        }
    }

    fn generate_socket_path() -> PathBuf {
        let pid = std::process::id();
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        PathBuf::from(format!("/tmp/srb-{}-{}.sock", pid, ts))
    }

    fn create_eventfd_owned() -> Result<OwnedFd> {
        let flags = libc::EFD_NONBLOCK | libc::EFD_CLOEXEC;
        let fd = unsafe { libc::eventfd(0, flags) };
        if fd < 0 {
            return Err(Error::last_os_error());
        }
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    fn spawn_listener_thread(
        sock_path: PathBuf,
        msg_fd: OwnedFd,
        cmd_fd: OwnedFd,
        stop: Arc<AtomicBool>,
    ) -> Result<()> {
        if sock_path.exists() {
            let _ = std::fs::remove_file(&sock_path);
        }

        let srv = socket(
            AddressFamily::Unix,
            SockType::Stream,
            SockFlag::SOCK_CLOEXEC,
            None,
        )
        .map_err(|e| Error::new(ErrorKind::Other, e))?;

        let addr = UnixAddr::new(&sock_path).map_err(|e| Error::new(ErrorKind::Other, e))?;
        bind(srv.as_raw_fd(), &addr).map_err(|e| Error::new(ErrorKind::Other, e))?;
        listen(&srv, Backlog::new(8)?).map_err(|e| Error::new(ErrorKind::Other, e))?;

        let _ = fcntl(&srv, FcntlArg::F_SETFL(OFlag::O_NONBLOCK));
        let _ = fcntl(&srv, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC));

        std::thread::Builder::new()
            .name("srb_eventfd_fdpass".to_string())
            .spawn(move || {
                let msg_fd_raw = msg_fd.as_raw_fd();
                let cmd_fd_raw = cmd_fd.as_raw_fd();

                while !stop.load(Ordering::Relaxed) {
                    match accept(srv.as_raw_fd()) {
                        Ok(cli_fd) => {
                            let iov = [IoSlice::new(&[0xE5])];
                            let fds = [msg_fd_raw, cmd_fd_raw];
                            let cmsg = [ControlMessage::ScmRights(&fds)];

                            if let Err(e) = sendmsg::<nix::sys::socket::UnixAddr>(
                                cli_fd,
                                &iov,
                                &cmsg,
                                MsgFlags::empty(),
                                None,
                            ) {
                                log::warn!("sendmsg(SCM_RIGHTS) failed: {e}");
                            }
                            let _ = unistd::close(cli_fd);
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

                let _ = std::fs::remove_file(&sock_path);
            })
            .map_err(|e| Error::new(ErrorKind::Other, e))?;

        Ok(())
    }

    fn receive_fds_from_server(sock_path: &Path) -> Result<(OwnedFd, OwnedFd)> {
        let deadline = std::time::Instant::now() + Duration::from_millis(200);
        let cli = loop {
            match socket(
                AddressFamily::Unix,
                SockType::Stream,
                SockFlag::SOCK_CLOEXEC,
                None,
            ) {
                Ok(cli) => {
                    let addr =
                        UnixAddr::new(sock_path).map_err(|e| Error::new(ErrorKind::Other, e))?;
                    match connect(cli.as_raw_fd(), &addr) {
                        Ok(()) => break cli,
                        Err(e) => {
                            if std::time::Instant::now() >= deadline {
                                return Err(Error::new(ErrorKind::Other, e));
                            }
                            std::thread::sleep(Duration::from_millis(10));
                            continue;
                        }
                    }
                }
                Err(e) => return Err(Error::new(ErrorKind::Other, e)),
            }
        };

        let mut buf = [0u8; 1];
        let mut iov = [IoSliceMut::new(&mut buf)];
        let mut cmsgspace = nix::cmsg_space!([RawFd; 2]);

        let msg = recvmsg::<UnixAddr>(
            cli.as_raw_fd(),
            &mut iov,
            Some(&mut cmsgspace),
            MsgFlags::empty(),
        )
        .map_err(|e| Error::new(ErrorKind::Other, e))?;

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

        let mut fds: Vec<RawFd> = Vec::new();
        if let Ok(mut cmsg) = msg.cmsgs() {
            while let Some(ControlMessageOwned::ScmRights(recv_fds)) = cmsg.next() {
                fds.extend(recv_fds);
            }
        }
        info!("fds: {:?}", fds);

        if fds.len() < 2 {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "Did not receive 2 fds from server",
            ));
        }

        let owned_msg = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let owned_cmd = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        Ok((owned_msg, owned_cmd))
    }

    fn wait_on_eventfd(
        &self,
        is_message: bool,
        has_data: impl Fn() -> bool,
        adaptive_poll_spins: u32,
        timeout: Option<Duration>,
    ) -> Result<bool> {
        for _ in 0..adaptive_poll_spins {
            if has_data() {
                return Ok(true);
            }
            hint::spin_loop();
        }
        if has_data() {
            return Ok(true);
        }

        let opt_fd = if is_message {
            self.local_message_fd.as_ref()
        } else {
            self.local_command_fd.as_ref()
        };
        let Some(ofd) = opt_fd else {
            std::thread::sleep(timeout.unwrap_or(Duration::from_millis(1)));
            return Ok(has_data());
        };

        // ── 核心优化：发布等待意图，signal 侧据此决定是否触发 write(fd) ────────
        let waiters = unsafe {
            if is_message {
                &(*self.header).message_waiters
            } else {
                &(*self.header).command_waiters
            }
        };
        // Release：让 signal 侧读到最新的 waiters
        waiters.fetch_add(1, Ordering::Release);

        // 进内核前再检查一次（避免 signal 在 waiters++ 前就完成，导致漏唤醒）
        if has_data() {
            waiters.fetch_sub(1, Ordering::Relaxed);
            // 可能有一个已经写入 fd 的事件，drain 掉避免下次 poll 误触发
            let borrowed = unsafe { BorrowedFd::borrow_raw(ofd.as_raw_fd()) };
            Self::drain_eventfd(borrowed);
            return Ok(true);
        }

        let result = match Self::poll_fd(ofd.as_raw_fd(), timeout) {
            Ok(true) => {
                let borrowed = unsafe { BorrowedFd::borrow_raw(ofd.as_raw_fd()) };
                Self::drain_eventfd(borrowed);
                Ok(true)
            }
            Ok(false) => Ok(has_data()),
            Err(e) => {
                log::warn!("poll(eventfd) error: {e}. Fallback to check state.");
                Ok(has_data())
            }
        };

        // Release：确保 signal 侧下次读到 waiters 已减少
        waiters.fetch_sub(1, Ordering::Release);
        result
    }
}

impl SyncBackend for EventFdBackend {
    fn init(&mut self, is_creator: bool, backend_ptr: *mut u8) -> Result<()> {
        self.header = backend_ptr as *mut EventFdHeader;
        self.is_creator = is_creator;

        if is_creator {
            info!("is creator");
            // 初始化共享内存中的 waiters 计数器
            unsafe {
                (*self.header).message_waiters.store(0, Ordering::Relaxed);
                (*self.header).command_waiters.store(0, Ordering::Relaxed);
            }

            let msg_fd = Self::create_eventfd_owned()?;
            let cmd_fd = Self::create_eventfd_owned()?;

            let msg_fd_for_send =
                nix::unistd::dup(&msg_fd).map_err(|e| Error::new(ErrorKind::Other, e))?;
            let cmd_fd_for_send =
                nix::unistd::dup(&cmd_fd).map_err(|e| Error::new(ErrorKind::Other, e))?;

            let sock_path = Self::generate_socket_path();
            Self::set_header_sock_path(self.header, &sock_path)?;
            let stop = Arc::new(AtomicBool::new(false));
            Self::spawn_listener_thread(
                sock_path.clone(),
                msg_fd_for_send,
                cmd_fd_for_send,
                stop.clone(),
            )?;

            self.local_message_fd = Some(msg_fd);
            self.local_command_fd = Some(cmd_fd);
            self.listener_stop = Some(stop);
            self.sock_path = Some(sock_path);
        } else {
            info!("is not creator");
            for _ in 0..10_000 {
                unsafe {
                    if (*self.header).is_ready.load(Ordering::Acquire) {
                        break;
                    }
                }
                hint::spin_loop();
            }
            if unsafe { !(*self.header).is_ready.load(Ordering::Acquire) } {
                std::thread::sleep(Duration::from_millis(5));
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
        unsafe {
            // ── 核心优化：无等待者时跳过 write() syscall ────────────────────────
            // Acquire：与 wait 侧的 Release fetch_add 配对，确保看到最新 waiters
            if (*self.header).message_waiters.load(Ordering::Acquire) > 0 {
                if let Some(fd) = &self.local_message_fd {
                    let borrowed = BorrowedFd::borrow_raw(fd.as_raw_fd());
                    Self::write_u64(borrowed, 1)?;
                }
            }
        }
        Ok(())
    }

    fn signal_command(&self) -> Result<()> {
        unsafe {
            if (*self.header).command_waiters.load(Ordering::Acquire) > 0 {
                if let Some(fd) = &self.local_command_fd {
                    let borrowed = BorrowedFd::borrow_raw(fd.as_raw_fd());
                    Self::write_u64(borrowed, 1)?;
                }
            }
        }
        Ok(())
    }

    fn cleanup(&mut self, is_creator: bool) {
        if is_creator {
            if let Some(stop) = &self.listener_stop {
                stop.store(true, Ordering::Relaxed);

                // ── 优化：主动连接 socket 唤醒监听线程，避免等满 poll 超时 ─────
                // 监听线程 poll(1ms)，正常情况下 3ms 内必定退出。
                // 改动前：sleep(20ms) 硬等。
                if let Some(path) = &self.sock_path {
                    if let Ok(addr) = UnixAddr::new(path.as_path()) {
                        if let Ok(cli) = socket(
                            AddressFamily::Unix,
                            SockType::Stream,
                            SockFlag::SOCK_CLOEXEC,
                            None,
                        ) {
                            // 忽略结果：仅为唤醒，失败也无妨
                            let _ = connect(cli.as_raw_fd(), &addr);
                        }
                    }
                }
                // 等待线程完成本轮 poll(1ms) 并退出（留 3ms 余量）
                std::thread::sleep(Duration::from_millis(3));
            }
            if let Some(path) = &self.sock_path {
                let _ = std::fs::remove_file(path);
            }
        }
        // OwnedFd 在 drop 时自动关闭
    }
}
