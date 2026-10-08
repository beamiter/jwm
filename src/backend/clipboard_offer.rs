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
/// Remote clipboard sharing remains text-only; PNG variants are filtered
/// before they leave the local session.
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
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
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
impl IsolatedXvfb {
    pub(crate) fn acquire() -> Self {
        use std::io::{Read as _, Write as _};
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
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let reader = std::thread::spawn(move || {
            let mut line = Vec::new();
            let result = loop {
                let mut byte = [0u8; 1];
                match stdout.read(&mut byte) {
                    Ok(0) => break Err("Xvfb closed displayfd before reporting readiness".into()),
                    Ok(_) if byte[0] == b'\n' => break Ok(line),
                    Ok(_) if line.len() < 16 => line.push(byte[0]),
                    Ok(_) => break Err("Xvfb display number exceeded 16 bytes".into()),
                    Err(error) => break Err(format!("read Xvfb displayfd: {error}")),
                }
            };
            let _ = sender.send(result);
        });
        let reported = receiver.recv_timeout(std::time::Duration::from_secs(3));
        if !matches!(reported, Ok(Ok(_))) {
            let _ = startup.child_mut().kill();
        }
        let _ = reader.join();
        let display_number = match reported {
            Ok(Ok(number)) => number,
            Ok(Err(error)) => {
                let _ = startup.child_mut().wait();
                panic!("Xvfb displayfd failed: {error}");
            }
            Err(error) => {
                let _ = startup.child_mut().wait();
                panic!("Xvfb did not report a display within 3 seconds: {error}");
            }
        };
        let display_number = std::str::from_utf8(&display_number)
            .expect("Xvfb display number is UTF-8")
            .trim()
            .parse::<u32>()
            .expect("Xvfb display number is numeric");
        assert!(
            startup
                .child_mut()
                .try_wait()
                .expect("query Xvfb child")
                .is_none()
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
            startup
                .child_mut()
                .try_wait()
                .expect("query Xvfb child")
                .is_none()
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
        let _ = self.child.kill();
        let _ = self.child.wait();
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

/// Payload JWM asks a backend-owned clipboard worker to serve.
///
/// X11 selections are lazy: the owner keeps the bytes and answers requests
/// from paste targets.  Keeping this message backend-neutral lets both X11
/// transports use their existing private clipboard connection instead of
/// launching `xclip` for screenshots.
#[derive(Debug)]
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
pub(crate) enum ClipboardOffer {
    Text(String),
    Png(Vec<u8>),
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
    if bytes.is_empty() || bytes.len() > MAX_IMAGE_HISTORY_BYTES {
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
    if png.is_empty() || png.len() > MAX_IMAGE_HISTORY_BYTES {
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

#[cfg(test)]
mod tests {
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
}
