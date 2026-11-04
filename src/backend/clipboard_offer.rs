//! What a clipboard offer is made of, judged without any window-management
//! policy.
//!
//! Every backend faces the same two questions before it reads a payload:
//! *may* this be recorded, and *which* of the advertised types should be
//! asked for. Both are answered from MIME names alone — X11 target atoms and
//! Wayland MIME strings are the same vocabulary — so they live here, below
//! the backends, rather than in the history that consumes the result. The
//! clipboard history re-exports them, so policy callers and backends decide
//! alike by construction.

/// Clipboard text payloads larger than this are ignored: a multi-megabyte
/// log is not something the picker can usefully show, and holding fifty of
/// them would be a real memory cost.
pub const MAX_TEXT_BYTES: usize = 256 * 1024;

/// PNG history payloads larger than this are ignored.
///
/// Separate from [`MAX_TEXT_BYTES`] and from the X11 serve caps: screenshots
/// and copied images are useful well past 256 KiB, but fifty uncompressed
/// desktop captures would still be a memory liability.
pub const MAX_IMAGE_HISTORY_BYTES: usize = 4 * 1024 * 1024;

/// Largest decoded image admitted to clipboard history. Encoded byte size is
/// not a memory bound: a mostly solid image can be only a few KiB on the wire
/// and expand into hundreds of MiB. Sixteen megapixels covers 4K captures
/// with room for cropping while keeping one RGBA decode near 64 MiB.
const MAX_IMAGE_HISTORY_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_IMAGE_HISTORY_DIMENSION: u32 = 16 * 1024;
const MAX_IMAGE_DECODE_BYTES: u64 = MAX_IMAGE_HISTORY_PIXELS * 4;

/// One clipboard payload a backend captured for the history to adopt.
///
/// The remote helper captures through its own profile
/// (`clipboard_x11::CopiedClipboard`), which also carries copied files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapturedClipboard {
    Text(String),
    Png(Vec<u8>),
}

/// Keep direct X11 `ChangeProperty` requests comfortably below the core
/// protocol limit. Larger payloads use ICCCM INCR, irrespective of whether a
/// particular server happens to expose BIG-REQUESTS.
// This and the MULTIPLE/INCR bounds below are read only by the X11 clipboard
// workers, never by a test here, so a build without them compiles them out.
#[cfg(any(
    feature = "backend-x11rb",
    feature = "backend-xcb",
    feature = "remote-x11"
))]
pub(crate) const X11_DIRECT_PROPERTY_BYTES: usize = 64 * 1024;

/// One INCR property payload. 240 KiB plus the 24-byte ChangeProperty header
/// remains below the plain X11 262,140-byte request ceiling, while avoiding a
/// long round-trip train for a full-width high-entropy screenshot.
#[cfg_attr(
    not(any(
        feature = "backend-x11rb",
        feature = "backend-xcb",
        feature = "remote-x11",
        test
    )),
    allow(dead_code)
)]
pub(crate) const X11_INCR_CHUNK_BYTES: usize = 240 * 1024;

/// Bound the number of conversions one MULTIPLE request may fan out into.
/// Real toolkit requests are normally one or two pairs; the larger allowance
/// keeps compatibility without letting one event monopolize the clipboard
/// worker or manufacture an unbounded set of INCR transfers.
#[cfg(any(
    feature = "backend-x11rb",
    feature = "backend-xcb",
    feature = "remote-x11"
))]
pub(crate) const X11_MAX_MULTIPLE_CONVERSIONS: usize = 64;

/// Total byte references retained by active outgoing INCR transfers. Shared
/// payloads are deliberately counted once per requestor: that conservative
/// accounting makes many stalled clients hit a deterministic ceiling even
/// though their `Arc`s point at the same allocation.
#[cfg(any(
    feature = "backend-x11rb",
    feature = "backend-xcb",
    feature = "remote-x11"
))]
pub(crate) const X11_MAX_ACTIVE_INCR_BYTES: usize = 512 * 1024 * 1024;

/// Largest single payload JWM will offer through X11. This comfortably covers
/// an uncompressed 8K RGBA frame while bounding memory retained by a corrupt
/// or accidental producer before any requestor asks for it.
pub(crate) const X11_MAX_OFFER_BYTES: usize = 512 * 1024 * 1024;

// The native clipboard tests (x11rb's, including the remote-x11 split
// setter, and xcb's) and the Wayland XWM lifecycle test are the only users;
// a build with none of them compiles no Xvfb helper rather than an unused
// one. A remote-x11-only build has the X11 clipboard but none of its tests.
#[cfg(all(
    test,
    any(
        feature = "backend-x11rb",
        feature = "backend-xcb",
        feature = "wayland-backends"
    )
))]
pub(crate) static X11_CLIPBOARD_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// One private Xvfb for a native X11 clipboard contract.
///
/// Serializes on [`X11_CLIPBOARD_TEST_LOCK`] so concurrent clipboard tests do
/// not pile up headless servers. Callers pass [`Self::name`] into
/// `Clipboard::start` / `connect` — the process `$DISPLAY` is left alone.
#[cfg(all(
    test,
    any(
        feature = "backend-x11rb",
        feature = "backend-xcb",
        feature = "wayland-backends"
    )
))]
pub(crate) struct IsolatedXvfb {
    _lock: std::sync::MutexGuard<'static, ()>,
    child: std::process::Child,
    /// Completed X11 setup connection kept open for the fixture lifetime.
    _probe: std::os::unix::net::UnixStream,
    display: String,
}

#[cfg(all(
    test,
    any(
        feature = "backend-x11rb",
        feature = "backend-xcb",
        feature = "wayland-backends"
    )
))]
struct XvfbStartupGuard(Option<std::process::Child>);

#[cfg(all(
    test,
    any(
        feature = "backend-x11rb",
        feature = "backend-xcb",
        feature = "wayland-backends"
    )
))]
impl XvfbStartupGuard {
    fn child_mut(&mut self) -> &mut std::process::Child {
        self.0.as_mut().expect("Xvfb startup child")
    }

    fn finish(mut self) -> std::process::Child {
        self.0.take().expect("Xvfb startup child")
    }

    fn terminate(&mut self) {
        if let Some(mut child) = self.0.take() {
            Self::terminate_child(&mut child);
        }
    }

    fn child_is_running(child: &std::process::Child) -> std::io::Result<bool> {
        // WNOWAIT keeps even an exited group leader waitable. Its PID cannot
        // be reused before terminate_child has signalled the owned group.
        loop {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    child.id(),
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result == 0 {
                return Ok(unsafe { info.si_pid() } == 0);
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    fn terminate_child(child: &mut std::process::Child) {
        // Fixture ownership is exclusive: no path reaps this private leader
        // before cleanup. If exact-child observation fails, do not signal a
        // numeric PID whose ownership we can no longer establish.
        if let Err(error) = Self::child_is_running(child) {
            if error.raw_os_error() != Some(libc::ECHILD) {
                eprintln!(
                    "cannot verify Xvfb fixture child {} for cleanup: {error}",
                    child.id()
                );
            }
            return;
        }
        // acquire() gives this fixture its own process group. Xvfb can spawn
        // xkbcomp with stdout inherited, so clean the whole owned launch tree.
        if let Ok(group) = i32::try_from(child.id())
            && group > 0
        {
            let _ = unsafe { libc::kill(-group, libc::SIGKILL) };
        }
        let _ = child.kill();
        let _ = child.wait();
    }

    fn read_display_number(
        stdout: &mut std::process::ChildStdout,
        timeout: std::time::Duration,
    ) -> std::io::Result<Vec<u8>> {
        use std::io::{Error, ErrorKind, Read as _};
        use std::os::fd::AsRawFd as _;

        let deadline = std::time::Instant::now() + timeout;
        let mut line = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(Error::new(
                    ErrorKind::TimedOut,
                    "Xvfb displayfd readiness timeout",
                ));
            }
            let mut fd = libc::pollfd {
                fd: stdout.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let timeout_ms = remaining
                .as_millis()
                .saturating_add(1)
                .min(i32::MAX as u128) as i32;
            let ready = unsafe { libc::poll(&mut fd, 1, timeout_ms) };
            if ready == 0 {
                continue;
            }
            if ready < 0 {
                let error = Error::last_os_error();
                if error.kind() == ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            // This is the pipe's only reader. POLLIN/POLLHUP guarantees this
            // single-byte read cannot wait on a helper retaining the writer.
            let mut byte = [0u8; 1];
            match stdout.read(&mut byte) {
                Ok(0) => {
                    return Err(Error::new(
                        ErrorKind::UnexpectedEof,
                        "Xvfb closed displayfd before reporting readiness",
                    ));
                }
                Ok(_) if byte[0] == b'\n' => return Ok(line),
                Ok(_) if line.len() < 16 => line.push(byte[0]),
                Ok(_) => {
                    return Err(Error::new(
                        ErrorKind::InvalidData,
                        "Xvfb display number exceeded 16 bytes",
                    ));
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }
}

#[cfg(all(
    test,
    any(
        feature = "backend-x11rb",
        feature = "backend-xcb",
        feature = "wayland-backends"
    )
))]
impl Drop for XvfbStartupGuard {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[cfg(all(
    test,
    any(
        feature = "backend-x11rb",
        feature = "backend-xcb",
        feature = "wayland-backends"
    )
))]
impl IsolatedXvfb {
    pub(crate) fn acquire() -> Self {
        use std::io::{Read as _, Write as _};
        use std::os::unix::process::CommandExt as _;
        use std::process::Stdio;

        let lock = X11_CLIPBOARD_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // `-displayfd` asks Xvfb itself to choose and bind an unused display,
        // then print its number only after the listener is ready. This cannot
        // mistake a stale socket (or somebody else's live display) for the
        // child launched here. Stdout is a private inherited pipe, so fd 1 is
        // a portable displayfd without manual descriptor manipulation.
        let child = match std::process::Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "1280x720x24",
                "-nolisten",
                "tcp",
                "-ac",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            // Preserve native startup diagnostics in CI. A displayfd timeout
            // alone cannot distinguish initialization stalls from listener or
            // driver errors, and discarding stderr made recurring failures
            // impossible to diagnose from the test log.
            .stderr(Stdio::inherit())
            .process_group(0)
            .spawn()
        {
            Ok(child) => child,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                panic!("Xvfb is required for native X11 clipboard tests; install the xvfb package");
            }
            Err(error) => panic!("spawn Xvfb: {error}"),
        };
        // Until a complete X11 setup succeeds, every early return or panic
        // must reap the private server rather than leak it into later tests.
        let mut startup = XvfbStartupGuard(Some(child));

        let mut stdout = startup
            .child_mut()
            .stdout
            .take()
            .expect("piped Xvfb displayfd");
        // Starting an external X server is not a clipboard latency check.
        // Cold CI runners can spend several seconds initializing Xvfb before
        // displayfd becomes ready. Keep that startup bounded independently
        // of the three-second X11 setup/clipboard protocol deadlines below.
        let startup_timeout = std::time::Duration::from_secs(15);
        let display_number =
            match XvfbStartupGuard::read_display_number(&mut stdout, startup_timeout) {
                Ok(number) => number,
                Err(error) => {
                    startup.terminate();
                    panic!("Xvfb displayfd failed within {startup_timeout:?}: {error}");
                }
            };
        let display_number = std::str::from_utf8(&display_number)
            .expect("Xvfb display number is UTF-8")
            .trim()
            .parse::<u32>()
            .expect("Xvfb display number is numeric");
        assert!(
            XvfbStartupGuard::child_is_running(startup.child_mut())
                .expect("query Xvfb child without reaping")
        );

        let display = format!(":{display_number}");
        let socket = format!("/tmp/.X11-unix/X{display_number}");
        let mut probe = std::os::unix::net::UnixStream::connect(&socket)
            .unwrap_or_else(|error| panic!("connect to Xvfb {display}: {error}"));
        probe
            .set_read_timeout(Some(std::time::Duration::from_secs(3)))
            .expect("set X11 setup timeout");
        probe
            .set_write_timeout(Some(std::time::Duration::from_secs(3)))
            .expect("set X11 setup timeout");
        // X11 connection setup: little-endian byte order, protocol 11.0, no
        // authentication (the fixture starts Xvfb with -ac).
        probe
            .write_all(&[b'l', 0, 11, 0, 0, 0, 0, 0, 0, 0, 0, 0])
            .expect("write X11 setup request");
        let mut prefix = [0u8; 8];
        probe
            .read_exact(&mut prefix)
            .expect("read X11 setup response");
        assert_eq!(prefix[0], 1, "Xvfb rejected X11 setup on {display}");
        let remaining = usize::from(u16::from_le_bytes([prefix[6], prefix[7]])) * 4;
        let mut setup = vec![0u8; remaining];
        probe
            .read_exact(&mut setup)
            .expect("read complete X11 setup");
        assert!(
            XvfbStartupGuard::child_is_running(startup.child_mut())
                .expect("query Xvfb child without reaping")
        );
        let child = startup.finish();

        Self {
            _lock: lock,
            child,
            _probe: probe,
            display,
        }
    }

    pub(crate) fn name(&self) -> &str {
        &self.display
    }
}

#[cfg(all(
    test,
    any(
        feature = "backend-x11rb",
        feature = "backend-xcb",
        feature = "wayland-backends"
    )
))]
impl Drop for IsolatedXvfb {
    fn drop(&mut self) {
        XvfbStartupGuard::terminate_child(&mut self.child);
    }
}

#[cfg(all(
    test,
    any(
        feature = "backend-x11rb",
        feature = "backend-xcb",
        feature = "wayland-backends"
    )
))]
mod xvfb_startup_tests {
    use super::XvfbStartupGuard;
    use std::io::ErrorKind;
    use std::os::unix::process::CommandExt as _;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    fn child(script: &str) -> Child {
        let child = Command::new("/bin/sh")
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("spawn owned fixture probe");
        assert!(child.id() > 0);
        assert_eq!(
            unsafe { libc::getpgid(child.id() as i32) },
            child.id() as i32
        );
        assert_ne!(child.id() as i32, unsafe { libc::getpgrp() });
        child
    }

    #[test]
    fn readiness_line_and_normal_cleanup_leave_other_groups_alive() {
        let mut control = XvfbStartupGuard(Some(child("sleep 30")));
        let mut server = XvfbStartupGuard(Some(child("printf '42\\n'; sleep 30")));
        let mut stdout = server.child_mut().stdout.take().unwrap();
        assert_eq!(
            XvfbStartupGuard::read_display_number(&mut stdout, Duration::from_secs(2)).unwrap(),
            b"42"
        );
        server.terminate();
        assert!(server.0.is_none());
        assert!(XvfbStartupGuard::child_is_running(control.child_mut()).unwrap());
    }

    #[test]
    fn inherited_writer_cannot_extend_readiness_or_cleanup() {
        let mut server = XvfbStartupGuard(Some(child("(sleep 30) & exit 23")));
        let mut stdout = server.child_mut().stdout.take().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while XvfbStartupGuard::child_is_running(server.child_mut()).unwrap() {
            assert!(Instant::now() < deadline, "probe leader did not exit");
            std::thread::sleep(Duration::from_millis(2));
        }
        // Observing an exited leader must not reap it before group cleanup.
        assert!(!XvfbStartupGuard::child_is_running(server.child_mut()).unwrap());
        assert_eq!(
            XvfbStartupGuard::read_display_number(&mut stdout, Duration::from_millis(20))
                .unwrap_err()
                .kind(),
            ErrorKind::TimedOut
        );
        server.terminate();
        assert!(server.0.is_none());
        // The descendant no longer owns the writer: no reader thread or
        // abandoned pipe can keep the fixture alive after cleanup.
        assert_eq!(
            XvfbStartupGuard::read_display_number(&mut stdout, Duration::from_secs(2))
                .unwrap_err()
                .kind(),
            ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn partial_readiness_line_still_obeys_the_deadline() {
        let mut server = XvfbStartupGuard(Some(child("printf '4'; sleep 30")));
        let mut stdout = server.child_mut().stdout.take().unwrap();
        assert_eq!(
            XvfbStartupGuard::read_display_number(&mut stdout, Duration::from_millis(20))
                .unwrap_err()
                .kind(),
            ErrorKind::TimedOut
        );
    }

    #[test]
    fn early_exit_and_oversized_display_lines_remain_errors() {
        for (script, expected) in [
            ("exit 7", ErrorKind::UnexpectedEof),
            (
                "printf '12345678901234567'; sleep 30",
                ErrorKind::InvalidData,
            ),
        ] {
            let mut server = XvfbStartupGuard(Some(child(script)));
            let mut stdout = server.child_mut().stdout.take().unwrap();
            assert_eq!(
                XvfbStartupGuard::read_display_number(&mut stdout, Duration::from_secs(2))
                    .unwrap_err()
                    .kind(),
                expected
            );
        }
    }

    #[test]
    fn non_waitable_leader_is_not_signalled() {
        let mut child = child("exit 23");
        assert_eq!(child.wait().unwrap().code(), Some(23));
        assert!(XvfbStartupGuard::child_is_running(&child).is_err());
        XvfbStartupGuard::terminate_child(&mut child);
        assert_eq!(child.try_wait().unwrap().unwrap().code(), Some(23));
    }
}

/// Return the next byte range for an outgoing INCR transfer and whether it is
/// the required zero-length terminator.
#[cfg_attr(
    not(any(
        feature = "backend-x11rb",
        feature = "backend-xcb",
        feature = "remote-x11",
        test
    )),
    allow(dead_code)
)]
pub(crate) fn next_x11_incr_chunk_with_limit(
    total: usize,
    offset: &mut usize,
    chunk_bytes: usize,
) -> (std::ops::Range<usize>, bool) {
    if *offset >= total {
        return (total..total, true);
    }
    let start = *offset;
    let end = start.saturating_add(chunk_bytes.max(1)).min(total);
    *offset = end;
    (start..end, false)
}

/// X11 timestamps are wrapping 32-bit millisecond counters. Treat CurrentTime
/// as valid for compatibility, otherwise use the ICCCM half-range comparison
/// to reject a request made before the current ownership began.
#[cfg_attr(
    not(any(
        feature = "backend-x11rb",
        feature = "backend-xcb",
        feature = "remote-x11",
        test
    )),
    allow(dead_code)
)]
pub(crate) fn x11_selection_time_is_valid(request: u32, acquired: Option<u32>) -> bool {
    request == 0 || acquired.is_none_or(|acquired| (request.wrapping_sub(acquired) as i32) >= 0)
}

/// Stop waiting promptly after a SAVE_TARGETS refusal unless the manager
/// has begun an actual clipboard conversion during the short grace period.
#[cfg_attr(
    not(any(
        feature = "backend-x11rb",
        feature = "backend-xcb",
        feature = "remote-x11",
        test
    )),
    allow(dead_code)
)]
pub(crate) fn x11_handoff_poll_deadline(
    deadline: std::time::Instant,
    provisional_failure: Option<std::time::Instant>,
    protocol_activity: bool,
) -> std::time::Instant {
    if protocol_activity {
        deadline
    } else {
        provisional_failure
            .and_then(|failed_at| failed_at.checked_add(std::time::Duration::from_millis(100)))
            .map_or(deadline, |grace| deadline.min(grace))
    }
}

/// Payload JWM asks a backend-owned clipboard worker to serve.
///
/// X11 selections are lazy: the owner keeps the bytes and answers requests
/// from paste targets.  Keeping this message backend-neutral lets both X11
/// transports use their existing private clipboard connection instead of
/// launching `xclip` for screenshots.
// Only a backend's clipboard worker offers `Text`; tests, like JWM's own
// screenshot path, only ever send `Png`. So a test build without a backend
// has the same unconstructed `Text` a plain build without one has, and must
// not drop the allowance just because it is a test build.
#[cfg_attr(
    not(any(
        feature = "backend-x11rb",
        feature = "backend-xcb",
        feature = "remote-x11",
        feature = "wayland-backends"
    )),
    allow(dead_code)
)]
#[derive(Clone, Debug)]
pub struct ClipboardReceipt(std::sync::Arc<std::sync::atomic::AtomicU8>);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardReceiptStatus {
    Pending,
    Owned,
    Failed,
}
impl ClipboardReceipt {
    pub(crate) fn pending() -> Self {
        Self(std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)))
    }
    pub fn status(&self) -> ClipboardReceiptStatus {
        match self.0.load(std::sync::atomic::Ordering::Acquire) {
            1 => ClipboardReceiptStatus::Owned,
            2 => ClipboardReceiptStatus::Failed,
            _ => ClipboardReceiptStatus::Pending,
        }
    }
    pub(crate) fn complete(&self, owned: bool) {
        let _ = self.0.compare_exchange(
            0,
            if owned { 1 } else { 2 },
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        );
    }
}
#[derive(Debug)]
pub(crate) struct ClipboardReceiptGuard(ClipboardReceipt);
impl Drop for ClipboardReceiptGuard {
    fn drop(&mut self) {
        self.0.complete(false);
    }
}
#[allow(dead_code)]
impl ClipboardReceiptGuard {
    pub(crate) fn owned(&self) {
        self.0.complete(true);
    }
}

#[derive(Debug)]
#[allow(dead_code)]
pub(crate) enum ClipboardOffer {
    Text(String),
    Png(Vec<u8>),
    Tracked(Box<ClipboardOffer>, ClipboardReceiptGuard),
}
#[allow(dead_code)]
impl ClipboardOffer {
    pub(crate) fn tracked(offer: Self) -> (Self, ClipboardReceipt) {
        let receipt = ClipboardReceipt::pending();
        (
            Self::Tracked(Box::new(offer), ClipboardReceiptGuard(receipt.clone())),
            receipt,
        )
    }
    pub(crate) fn split_receipt(self) -> (Self, Option<ClipboardReceiptGuard>) {
        match self {
            Self::Tracked(offer, guard) => (*offer, Some(guard)),
            offer => (offer, None),
        }
    }
}

/// Cloneable, thread-safe route into a backend's native image clipboard.
///
/// Screenshot encoding finishes on a worker thread, after the command that
/// started the capture has returned.  This handle lets that worker transfer
/// ownership of the completed PNG to the backend clipboard thread without
/// retaining a reference to the backend or polling the window-manager loop.
#[derive(Clone, Debug)]
pub struct ClipboardImageSender {
    sender: std::sync::mpsc::Sender<ClipboardOffer>,
    wake: Option<crate::backend::update_notifier::AsyncUpdateNotifier>,
}

impl ClipboardImageSender {
    #[cfg(test)]
    pub(crate) fn new(sender: std::sync::mpsc::Sender<ClipboardOffer>) -> Self {
        Self { sender, wake: None }
    }

    #[cfg(any(feature = "backend-x11rb", feature = "backend-xcb"))]
    pub(crate) fn new_with_wake(
        sender: std::sync::mpsc::Sender<ClipboardOffer>,
        wake: crate::backend::update_notifier::AsyncUpdateNotifier,
    ) -> Self {
        Self {
            sender,
            wake: Some(wake),
        }
    }

    /// Queue a complete PNG for native clipboard ownership.
    #[must_use]
    pub fn send_png(&self, png: Vec<u8>) -> bool {
        if png.len() > X11_MAX_OFFER_BYTES {
            return false;
        }
        if self.wake.as_ref().is_some_and(|wake| !wake.is_healthy()) {
            return false;
        }
        let sent = self.sender.send(ClipboardOffer::Png(png)).is_ok();
        sent && self.wake.as_ref().is_none_or(|wake| wake.notify())
    }
}

/// MIME types by which an application asks clipboard managers not to store
/// what it just copied.
///
/// `x-kde-passwordManagerHint` is the de-facto standard — password managers
/// offer it alongside the text, and every clipboard manager that respects
/// privacy checks for it. Honoring it is the difference between a history
/// and a password leak.
const SECRET_HINTS: [&str; 3] = [
    "x-kde-passwordManagerHint",
    "application/x-secret",
    "x-secret",
];

/// Whether an offer is marked as a secret and must not be recorded.
///
/// The comparison is case-insensitive and matches on a suffix, because the
/// hint travels both bare and with a vendor prefix depending on the toolkit.
#[must_use]
pub fn is_secret(mime_types: &[String]) -> bool {
    mime_types.iter().any(|mime| {
        let mime = mime.to_ascii_lowercase();
        SECRET_HINTS
            .iter()
            .any(|hint| mime.ends_with(&hint.to_ascii_lowercase()))
    })
}

/// Pick the text-ish MIME type to ask for, preferring UTF-8.
///
/// Returns `None` when the offer holds no text the history can store. Image
/// offers fall through to [`preferred_image_mime`] under the capture policy
/// (text wins when both are present).
#[must_use]
pub fn preferred_text_mime(mime_types: &[String]) -> Option<String> {
    const PREFERRED: [&str; 4] = [
        "text/plain;charset=utf-8",
        "UTF8_STRING",
        "text/plain",
        "STRING",
    ];
    for wanted in PREFERRED {
        if let Some(found) = mime_types
            .iter()
            .find(|mime| mime.eq_ignore_ascii_case(wanted))
        {
            return Some(found.clone());
        }
    }
    None
}

/// Pick an image offer for history capture.
///
/// Prefers `image/png`, then `image/jpeg`, then `image/webp`, then
/// `image/gif`, then `image/bmp`, then `image/tiff`, then `image/avif`.
/// Non-PNG offers are decoded into PNG under [`MAX_IMAGE_HISTORY_BYTES`]
/// before the history stores them (see [`image_offer_to_history_png`]).
/// Callers still prefer text when [`preferred_text_mime`] finds one.
///
/// HEIC/HEIF and JPEG XL (`image/heic`, `image/heif`, `image/jxl`) are
/// intentionally omitted: the bundled `image` crate has no decoder for
/// those formats, so accepting the MIME would only drop every offer.
#[must_use]
pub fn preferred_image_mime(mime_types: &[String]) -> Option<String> {
    const PREFERRED: [&str; 7] = [
        "image/png",
        "image/jpeg",
        "image/webp",
        "image/gif",
        "image/bmp",
        "image/tiff",
        "image/avif",
    ];
    for wanted in PREFERRED {
        if let Some(found) = mime_types
            .iter()
            .find(|mime| mime.eq_ignore_ascii_case(wanted))
        {
            return Some(found.clone());
        }
    }
    None
}

/// Decode a captured image offer into PNG bytes for the history.
///
/// PNG offers pass through when they fit the encoded-byte and decoded-pixel
/// budgets. Their dimensions are inspected without allocating the raster.
/// JPEG/WebP/GIF/BMP/TIFF/AVIF offers are decoded and re-encoded as PNG;
/// empty, undecodable, or oversized results are dropped. The history stores
/// and re-offers PNG only.
#[must_use]
pub fn image_offer_to_history_png(bytes: &[u8], mime: &str) -> Option<Vec<u8>> {
    image_offer_to_png_within(bytes, mime, MAX_IMAGE_HISTORY_BYTES)
}

/// [`image_offer_to_history_png`] with a caller-chosen encoded-byte ceiling.
///
/// The decoded-pixel budget is the same for every caller; only how many
/// encoded bytes are worth keeping differs. The remote helper shares one image
/// at a time and may carry a larger screenshot than the fifty-entry history.
#[must_use]
pub fn image_offer_to_png_within(bytes: &[u8], mime: &str, max_bytes: usize) -> Option<Vec<u8>> {
    if bytes.is_empty() || bytes.len() > max_bytes {
        return None;
    }
    let mime = mime.to_ascii_lowercase();
    if mime != "image/png"
        && mime != "image/jpeg"
        && mime != "image/jpg"
        && mime != "image/webp"
        && mime != "image/gif"
        && mime != "image/bmp"
        && mime != "image/tiff"
        && mime != "image/tif"
        && mime != "image/avif"
    {
        return None;
    }
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let actual_format = reader.format();
    let limits = image_history_decode_limits();
    reader.limits(limits.clone());
    let (width, height) = reader.into_dimensions().ok()?;
    if !image_dimensions_fit_history(width, height) {
        return None;
    }
    if mime == "image/png" {
        return (actual_format == Some(image::ImageFormat::Png)).then(|| bytes.to_vec());
    }

    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    // The dimension reader consumed its decoder, so apply the same limits to
    // the fresh reader that performs the allocation.
    reader.limits(limits);
    let image = reader.decode().ok()?;
    let mut png = Vec::new();
    {
        let mut cursor = std::io::Cursor::new(&mut png);
        image.write_to(&mut cursor, image::ImageFormat::Png).ok()?;
    }
    if png.is_empty() || png.len() > max_bytes {
        return None;
    }
    Some(png)
}

fn image_history_decode_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_HISTORY_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_HISTORY_DIMENSION);
    limits.max_alloc = Some(MAX_IMAGE_DECODE_BYTES);
    limits
}

fn image_dimensions_fit_history(width: u32, height: u32) -> bool {
    width <= MAX_IMAGE_HISTORY_DIMENSION
        && height <= MAX_IMAGE_HISTORY_DIMENSION
        && u64::from(width).saturating_mul(u64::from(height)) <= MAX_IMAGE_HISTORY_PIXELS
}

/// What the history should request from an offer, after secret filtering.
///
/// Policy: text wins when present; otherwise PNG / JPEG / WebP / GIF / BMP /
/// TIFF / AVIF. Everything else is skipped.
#[must_use]
pub fn preferred_history_mime(mime_types: &[String]) -> Option<String> {
    preferred_text_mime(mime_types).or_else(|| preferred_image_mime(mime_types))
}

/// File managers copy files as a list of `file://` URIs. GNOME, Thunar,
/// PCManFM, Nemo and Caja put `copy` or `cut` on a first line of
/// `x-special/gnome-copied-files`; everything (Qt included) also offers the
/// plain RFC 2483 `text/uri-list`.
#[cfg(feature = "remote-x11")]
pub(crate) const GNOME_COPIED_FILES_MIME: &str = "x-special/gnome-copied-files";

/// RFC 2483 URI list; see [`GNOME_COPIED_FILES_MIME`].
#[cfg(feature = "remote-x11")]
pub(crate) const URI_LIST_MIME: &str = "text/uri-list";

/// Most entries one copied file list may name. A selection of thousands of
/// top-level items is far past what a person picks by hand.
#[cfg(feature = "remote-x11")]
pub(crate) const MAX_FILE_LIST_ITEMS: usize = 4096;

/// Parse a copied file list into local absolute paths.
///
/// `None` means "this is not a local file copy": an empty list, a non-`file`
/// URI (a browser's link list), a remote host, malformed escapes, or a path
/// containing NUL. The caller then falls back to the plain-text form the same
/// owner usually offers, so a copied web link still arrives as text.
#[cfg(feature = "remote-x11")]
pub(crate) fn parse_file_list(mime: &str, bytes: &[u8]) -> Option<Vec<std::path::PathBuf>> {
    use std::os::unix::ffi::OsStringExt as _;
    let text = std::str::from_utf8(bytes).ok()?.trim_end_matches('\0');
    let mut lines = text.split('\n').map(|line| line.trim_end_matches('\r'));
    if mime == GNOME_COPIED_FILES_MIME {
        // The first line is the operation. A cut is shared as a copy: the
        // other machine never deletes the originals here.
        match lines.next()? {
            "copy" | "cut" => {}
            _ => return None,
        }
    } else if mime != URI_LIST_MIME {
        return None;
    }
    let mut paths = Vec::new();
    for line in lines {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if paths.len() == MAX_FILE_LIST_ITEMS {
            return None;
        }
        let path = local_path_from_file_uri(line)?;
        paths.push(std::path::PathBuf::from(std::ffi::OsString::from_vec(path)));
    }
    (!paths.is_empty()).then_some(paths)
}

/// Decode `file:///abs/path` or `file://localhost/abs/path` into path bytes.
#[cfg(feature = "remote-x11")]
fn local_path_from_file_uri(uri: &str) -> Option<Vec<u8>> {
    let scheme_len = "file:".len();
    if uri.len() < scheme_len || !uri[..scheme_len].eq_ignore_ascii_case("file:") {
        return None;
    }
    let rest = &uri[scheme_len..];
    let path = if let Some(authority_and_path) = rest.strip_prefix("//") {
        let slash = authority_and_path.find('/')?;
        let host = &authority_and_path[..slash];
        if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
            return None;
        }
        &authority_and_path[slash..]
    } else if rest.starts_with('/') {
        // `file:/path`, as some KDE versions write it.
        rest
    } else {
        return None;
    };
    // A query or fragment has no meaning for a local file.
    if path.contains(['?', '#']) {
        return None;
    }
    let decoded = percent_decode(path.as_bytes())?;
    (decoded.first() == Some(&b'/') && !decoded.contains(&0)).then_some(decoded)
}

#[cfg(feature = "remote-x11")]
fn percent_decode(bytes: &[u8]) -> Option<Vec<u8>> {
    fn hex(digit: u8) -> Option<u8> {
        match digit {
            b'0'..=b'9' => Some(digit - b'0'),
            b'a'..=b'f' => Some(digit - b'a' + 10),
            b'A'..=b'F' => Some(digit - b'A' + 10),
            _ => None,
        }
    }
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = hex(*bytes.get(index + 1)?)?;
            let low = hex(*bytes.get(index + 2)?)?;
            decoded.push(high << 4 | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    Some(decoded)
}

/// `file://` URI for an absolute local path.
///
/// Every byte outside RFC 3986's unreserved set (and `/`) is escaped, so a
/// name with spaces, `#`, `%` or non-UTF-8 bytes survives the round trip.
#[cfg(feature = "remote-x11")]
pub(crate) fn file_uri(path: &std::path::Path) -> String {
    use std::fmt::Write as _;
    use std::os::unix::ffi::OsStrExt as _;
    let mut uri = String::from("file://");
    for &byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'.' | b'_' | b'~') {
            uri.push(char::from(byte));
        } else {
            let _ = write!(uri, "%{byte:02X}");
        }
    }
    uri
}

/// The three encodings a file list is offered in on X11.
#[cfg(feature = "remote-x11")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileListPayloads {
    /// `x-special/gnome-copied-files`: `copy` then one URI per line.
    pub(crate) gnome: Vec<u8>,
    /// `text/uri-list`: CRLF-terminated URIs.
    pub(crate) uri_list: Vec<u8>,
    /// Plain text: one path per line, for pasting into a terminal or editor.
    pub(crate) text: Vec<u8>,
}

#[cfg(feature = "remote-x11")]
pub(crate) fn file_list_payloads(paths: &[std::path::PathBuf]) -> FileListPayloads {
    let uris: Vec<String> = paths.iter().map(|path| file_uri(path)).collect();
    let mut gnome = b"copy".to_vec();
    let mut uri_list = Vec::new();
    for uri in &uris {
        gnome.push(b'\n');
        gnome.extend_from_slice(uri.as_bytes());
        uri_list.extend_from_slice(uri.as_bytes());
        uri_list.extend_from_slice(b"\r\n");
    }
    let text = paths
        .iter()
        .map(|path| path.to_string_lossy())
        .collect::<Vec<_>>()
        .join("\n")
        .into_bytes();
    FileListPayloads {
        gnome,
        uri_list,
        text,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn refused_handoff_poll_wakes_at_grace_deadline() {
        let now = std::time::Instant::now();
        let deadline = now + std::time::Duration::from_secs(10);
        assert_eq!(
            super::x11_handoff_poll_deadline(deadline, Some(now), false),
            now + std::time::Duration::from_millis(100)
        );
        assert_eq!(
            super::x11_handoff_poll_deadline(deadline, Some(now), true),
            deadline
        );
        assert_eq!(
            super::x11_handoff_poll_deadline(deadline, None, false),
            deadline
        );
        let sooner = now + std::time::Duration::from_millis(20);
        assert_eq!(
            super::x11_handoff_poll_deadline(sooner, Some(now), false),
            sooner
        );
    }

    use super::*;

    #[test]
    fn outgoing_x11_incr_covers_payload_then_emits_empty_terminator() {
        let total = X11_INCR_CHUNK_BYTES * 2 + 17;
        let mut offset = 0;
        let mut ranges = Vec::new();
        loop {
            let (range, terminal) =
                next_x11_incr_chunk_with_limit(total, &mut offset, X11_INCR_CHUNK_BYTES);
            ranges.push((range, terminal));
            if terminal {
                break;
            }
        }

        assert_eq!(ranges[0], (0..X11_INCR_CHUNK_BYTES, false));
        assert_eq!(
            ranges[1],
            (X11_INCR_CHUNK_BYTES..X11_INCR_CHUNK_BYTES * 2, false)
        );
        assert_eq!(ranges[2], (X11_INCR_CHUNK_BYTES * 2..total, false));
        assert_eq!(ranges[3], (total..total, true));
        assert_eq!(offset, total);
    }

    #[test]
    fn empty_x11_incr_payload_is_only_a_terminator() {
        let mut offset = 0;
        assert_eq!(
            next_x11_incr_chunk_with_limit(0, &mut offset, X11_INCR_CHUNK_BYTES),
            (0..0, true)
        );
    }

    #[test]
    fn outgoing_x11_incr_respects_a_small_server_request_limit() {
        let mut offset = 0;
        assert_eq!(
            next_x11_incr_chunk_with_limit(10, &mut offset, 4),
            (0..4, false)
        );
        assert_eq!(
            next_x11_incr_chunk_with_limit(10, &mut offset, 4),
            (4..8, false)
        );
        assert_eq!(
            next_x11_incr_chunk_with_limit(10, &mut offset, 4),
            (8..10, false)
        );
    }

    #[test]
    fn x11_selection_time_validation_handles_current_time_and_wraparound() {
        assert!(x11_selection_time_is_valid(0, Some(100)));
        assert!(x11_selection_time_is_valid(100, Some(100)));
        assert!(x11_selection_time_is_valid(101, Some(100)));
        assert!(!x11_selection_time_is_valid(99, Some(100)));

        let acquired = u32::MAX - 2;
        assert!(x11_selection_time_is_valid(1, Some(acquired)));
        assert!(!x11_selection_time_is_valid(acquired - 1, Some(acquired)));
        assert!(x11_selection_time_is_valid(42, None));
    }

    #[test]
    fn image_history_accepts_png_jpeg_webp_gif_bmp_tiff_and_avif_mimes() {
        assert_eq!(
            preferred_image_mime(&["image/png".to_string()]).as_deref(),
            Some("image/png")
        );
        assert_eq!(
            preferred_image_mime(&["IMAGE/PNG".to_string()]).as_deref(),
            Some("IMAGE/PNG")
        );
        assert_eq!(
            preferred_image_mime(&["image/jpeg".to_string(), "image/bmp".to_string()]).as_deref(),
            Some("image/jpeg")
        );
        assert_eq!(
            preferred_image_mime(&["image/webp".to_string(), "image/bmp".to_string()]).as_deref(),
            Some("image/webp")
        );
        assert_eq!(
            preferred_image_mime(&["image/gif".to_string()]).as_deref(),
            Some("image/gif")
        );
        assert_eq!(
            preferred_image_mime(&["image/bmp".to_string()]).as_deref(),
            Some("image/bmp")
        );
        assert_eq!(
            preferred_image_mime(&["image/tiff".to_string()]).as_deref(),
            Some("image/tiff")
        );
        assert_eq!(
            preferred_image_mime(&["image/avif".to_string()]).as_deref(),
            Some("image/avif")
        );
        assert_eq!(
            preferred_image_mime(&["image/tiff".to_string(), "image/avif".to_string()]).as_deref(),
            Some("image/tiff")
        );
        // PNG wins over JPEG when both are advertised.
        assert_eq!(
            preferred_image_mime(&[
                "image/jpeg".to_string(),
                "image/png".to_string(),
                "image/bmp".to_string(),
            ])
            .as_deref(),
            Some("image/png")
        );
        // JPEG wins over WebP when both are advertised (preference order).
        assert_eq!(
            preferred_image_mime(&["image/webp".to_string(), "image/jpeg".to_string()]).as_deref(),
            Some("image/jpeg")
        );
        // HEIC / JXL: no decoder in the bundled `image` crate — never prefer.
        assert!(
            preferred_image_mime(&[
                "image/heic".to_string(),
                "image/heif".to_string(),
                "image/jxl".to_string(),
            ])
            .is_none()
        );
        assert_eq!(
            preferred_image_mime(&["image/heic".to_string(), "image/png".to_string()]).as_deref(),
            Some("image/png")
        );
    }

    #[test]
    fn history_mime_prefers_text_over_images() {
        let both = vec![
            "image/png".to_string(),
            "text/plain;charset=utf-8".to_string(),
        ];
        assert_eq!(
            preferred_history_mime(&both).as_deref(),
            Some("text/plain;charset=utf-8")
        );
        assert_eq!(
            preferred_history_mime(&["image/png".to_string()]).as_deref(),
            Some("image/png")
        );
        assert_eq!(
            preferred_history_mime(&["image/jpeg".to_string(), "text/uri-list".to_string()])
                .as_deref(),
            Some("image/jpeg")
        );
    }

    #[test]
    fn jpeg_offer_decodes_to_png_under_the_image_cap() {
        // 1×1 red JPEG.
        let jpeg = {
            let img = image::RgbImage::from_pixel(1, 1, image::Rgb([255, 0, 0]));
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgb8(img)
                .write_to(
                    &mut std::io::Cursor::new(&mut bytes),
                    image::ImageFormat::Jpeg,
                )
                .expect("encode jpeg");
            bytes
        };
        let png = image_offer_to_history_png(&jpeg, "image/jpeg").expect("jpeg→png");
        assert!(png.starts_with(b"\x89PNG"), "must be a PNG");
        assert!(png.len() <= MAX_IMAGE_HISTORY_BYTES);
        // Round-trip through history PNG path is a no-op copy.
        let again = image_offer_to_history_png(&png, "image/png").expect("png passthrough");
        assert_eq!(again, png);
    }

    #[test]
    fn png_passthrough_requires_png_bytes() {
        let png = {
            let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([1, 2, 3, 255]));
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgba8(img)
                .write_to(
                    &mut std::io::Cursor::new(&mut bytes),
                    image::ImageFormat::Png,
                )
                .unwrap();
            bytes
        };
        assert_eq!(image_offer_to_history_png(&png, "image/png"), Some(png));

        let jpeg = {
            let img = image::RgbImage::from_pixel(1, 1, image::Rgb([4, 5, 6]));
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgb8(img)
                .write_to(
                    &mut std::io::Cursor::new(&mut bytes),
                    image::ImageFormat::Jpeg,
                )
                .unwrap();
            bytes
        };
        assert!(image_offer_to_history_png(&jpeg, "image/png").is_none());
    }

    #[test]
    fn image_dimension_budget_includes_its_edges() {
        assert!(image_dimensions_fit_history(
            MAX_IMAGE_HISTORY_DIMENSION,
            (MAX_IMAGE_HISTORY_PIXELS / u64::from(MAX_IMAGE_HISTORY_DIMENSION)) as u32,
        ));
        assert!(!image_dimensions_fit_history(
            MAX_IMAGE_HISTORY_DIMENSION + 1,
            1
        ));
        assert!(!image_dimensions_fit_history(4097, 4097));
    }

    #[test]
    fn bmp_offer_decodes_to_png_under_the_image_cap() {
        let bmp = {
            let img = image::RgbImage::from_pixel(2, 2, image::Rgb([0, 128, 255]));
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgb8(img)
                .write_to(
                    &mut std::io::Cursor::new(&mut bytes),
                    image::ImageFormat::Bmp,
                )
                .expect("encode bmp");
            bytes
        };
        let png = image_offer_to_history_png(&bmp, "image/bmp").expect("bmp→png");
        assert!(png.starts_with(b"\x89PNG"));
    }

    #[test]
    fn webp_offer_decodes_to_png_under_the_image_cap() {
        let webp = {
            let img = image::RgbImage::from_pixel(2, 1, image::Rgb([10, 20, 30]));
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgb8(img)
                .write_to(
                    &mut std::io::Cursor::new(&mut bytes),
                    image::ImageFormat::WebP,
                )
                .expect("encode webp");
            bytes
        };
        let png = image_offer_to_history_png(&webp, "image/webp").expect("webp→png");
        assert!(png.starts_with(b"\x89PNG"));
        assert!(png.len() <= MAX_IMAGE_HISTORY_BYTES);
    }

    #[test]
    fn gif_offer_decodes_to_png_under_the_image_cap() {
        let gif = {
            let img = image::RgbImage::from_pixel(1, 2, image::Rgb([200, 100, 50]));
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgb8(img)
                .write_to(
                    &mut std::io::Cursor::new(&mut bytes),
                    image::ImageFormat::Gif,
                )
                .expect("encode gif");
            bytes
        };
        let png = image_offer_to_history_png(&gif, "image/gif").expect("gif→png");
        assert!(png.starts_with(b"\x89PNG"));
    }

    #[test]
    fn tiff_offer_decodes_to_png_under_the_image_cap() {
        let tiff = {
            let img = image::RgbImage::from_pixel(2, 1, image::Rgb([10, 20, 30]));
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgb8(img)
                .write_to(
                    &mut std::io::Cursor::new(&mut bytes),
                    image::ImageFormat::Tiff,
                )
                .expect("encode tiff");
            bytes
        };
        let png = image_offer_to_history_png(&tiff, "image/tiff").expect("tiff→png");
        assert!(png.starts_with(b"\x89PNG"));
        assert!(png.len() <= MAX_IMAGE_HISTORY_BYTES);
    }

    #[test]
    fn avif_mime_is_accepted_and_undecodable_payloads_drop() {
        // Default `image` builds encode AVIF but need `avif-native` to decode.
        // A garbage payload must still drop rather than panic; a real decoder
        // (when present) re-encodes under the image cap like TIFF/JPEG.
        assert!(image_offer_to_history_png(b"not-avif", "image/avif").is_none());
        let avif = {
            let img = image::RgbImage::from_pixel(1, 1, image::Rgb([1, 2, 3]));
            let mut bytes = Vec::new();
            let encoded = image::DynamicImage::ImageRgb8(img).write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Avif,
            );
            match encoded {
                Ok(()) => Some(bytes),
                Err(_) => None,
            }
        };
        if let Some(avif) = avif {
            let _ = image_offer_to_history_png(&avif, "image/avif");
        }
    }

    #[test]
    fn oversized_or_empty_image_offers_are_dropped() {
        assert!(image_offer_to_history_png(&[], "image/png").is_none());
        let huge = vec![0u8; MAX_IMAGE_HISTORY_BYTES + 1];
        assert!(image_offer_to_history_png(&huge, "image/png").is_none());
        assert!(image_offer_to_history_png(b"not-an-image", "image/jpeg").is_none());
    }

    #[test]
    fn compressed_image_dimensions_cannot_exceed_the_decode_budget() {
        // A BMP header can advertise a huge, mostly absent raster in only 54
        // bytes. Dimension inspection must reject it before decode allocates
        // the advertised 100-million-pixel output buffer.
        let mut bmp = vec![0u8; 54];
        bmp[0..2].copy_from_slice(b"BM");
        bmp[2..6].copy_from_slice(&54u32.to_le_bytes());
        bmp[10..14].copy_from_slice(&54u32.to_le_bytes());
        bmp[14..18].copy_from_slice(&40u32.to_le_bytes());
        bmp[18..22].copy_from_slice(&10_000i32.to_le_bytes());
        bmp[22..26].copy_from_slice(&10_000i32.to_le_bytes());
        bmp[26..28].copy_from_slice(&1u16.to_le_bytes());
        bmp[28..30].copy_from_slice(&24u16.to_le_bytes());

        assert!(bmp.len() < 100);
        let dimensions = image::ImageReader::new(std::io::Cursor::new(&bmp))
            .with_guessed_format()
            .unwrap()
            .into_dimensions()
            .unwrap();
        assert_eq!(dimensions, (10_000, 10_000));
        assert!(image_offer_to_history_png(&bmp, "image/bmp").is_none());
    }

    #[test]
    fn image_history_cap_dwarfs_the_text_cap() {
        assert!(MAX_IMAGE_HISTORY_BYTES > MAX_TEXT_BYTES);
        assert_eq!(MAX_IMAGE_HISTORY_BYTES, 4 * 1024 * 1024);
    }

    #[cfg(feature = "remote-x11")]
    #[test]
    fn nautilus_file_copies_parse_to_local_paths() {
        let gnome = b"copy\nfile:///home/u/My%20Doc.pdf\nfile:///home/u/%E4%B8%AD%E6%96%87";
        assert_eq!(
            parse_file_list(GNOME_COPIED_FILES_MIME, gnome).unwrap(),
            vec![
                std::path::PathBuf::from("/home/u/My Doc.pdf"),
                std::path::PathBuf::from("/home/u/\u{4e2d}\u{6587}"),
            ]
        );
        // A cut is shared as a copy, with the trailing newline and NUL some
        // toolkits append.
        assert_eq!(
            parse_file_list(GNOME_COPIED_FILES_MIME, b"cut\nfile:///tmp/a\n\0").unwrap(),
            vec![std::path::PathBuf::from("/tmp/a")]
        );
        let uri_list = b"# comment\r\nfile://localhost/etc/hosts\r\nfile:/tmp/b\r\n";
        assert_eq!(
            parse_file_list(URI_LIST_MIME, uri_list).unwrap(),
            vec![
                std::path::PathBuf::from("/etc/hosts"),
                std::path::PathBuf::from("/tmp/b"),
            ]
        );
    }

    #[cfg(feature = "remote-x11")]
    #[test]
    fn non_local_or_malformed_file_lists_fall_back_to_text() {
        for (mime, bytes) in [
            (URI_LIST_MIME, &b"https://example.com/page"[..]),
            (URI_LIST_MIME, b"file:///tmp/a\r\nhttps://example.com/"),
            (URI_LIST_MIME, b"file://otherhost/tmp/a"),
            (URI_LIST_MIME, b"file:///tmp/%zz"),
            (URI_LIST_MIME, b"file:///tmp/%00nul"),
            (URI_LIST_MIME, b"file:///tmp/a?query"),
            (URI_LIST_MIME, b"file:relative"),
            (URI_LIST_MIME, b"\r\n# only comments\r\n"),
            (GNOME_COPIED_FILES_MIME, b"file:///tmp/a"),
            (GNOME_COPIED_FILES_MIME, b"link\nfile:///tmp/a"),
            ("text/plain", b"file:///tmp/a"),
            (URI_LIST_MIME, &[0xff, 0xfe]),
        ] {
            assert_eq!(parse_file_list(mime, bytes), None, "{mime}: {bytes:?}");
        }
        let too_many = (0..=MAX_FILE_LIST_ITEMS)
            .map(|index| format!("file:///tmp/{index}\r\n"))
            .collect::<String>();
        assert_eq!(parse_file_list(URI_LIST_MIME, too_many.as_bytes()), None);
    }

    #[cfg(feature = "remote-x11")]
    #[test]
    fn served_file_lists_round_trip_through_the_parser() {
        use std::os::unix::ffi::OsStrExt as _;
        let paths = vec![
            std::path::PathBuf::from("/home/u/.cache/jwm/a b#c%d.txt"),
            std::path::PathBuf::from(std::ffi::OsStr::from_bytes(b"/tmp/raw\xff\xfename")),
            std::path::PathBuf::from("/tmp/\u{4e2d}\u{6587}/dir"),
        ];
        let payloads = file_list_payloads(&paths);
        assert!(
            payloads
                .gnome
                .starts_with(b"copy\nfile:///home/u/.cache/jwm/a%20b%23c%25d.txt")
        );
        assert_eq!(
            parse_file_list(GNOME_COPIED_FILES_MIME, &payloads.gnome).unwrap(),
            paths
        );
        assert_eq!(
            parse_file_list(URI_LIST_MIME, &payloads.uri_list).unwrap(),
            paths
        );
        assert!(payloads.uri_list.ends_with(b"/dir\r\n"));
        assert_eq!(
            String::from_utf8(payloads.text).unwrap().lines().count(),
            paths.len()
        );
    }

    #[test]
    fn image_ceiling_is_chosen_by_the_caller() {
        let png = {
            let img = image::RgbImage::from_pixel(2, 2, image::Rgb([0, 128, 255]));
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgb8(img)
                .write_to(
                    &mut std::io::Cursor::new(&mut bytes),
                    image::ImageFormat::Png,
                )
                .unwrap();
            bytes
        };
        assert_eq!(
            image_offer_to_png_within(&png, "image/png", png.len()).as_deref(),
            Some(&png[..])
        );
        assert_eq!(
            image_offer_to_png_within(&png, "image/png", png.len() - 1),
            None
        );
    }
}
