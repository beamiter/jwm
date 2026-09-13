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
#[cfg_attr(
    not(any(
        feature = "backend-x11rb",
        feature = "backend-xcb",
        feature = "remote-x11",
        test
    )),
    allow(dead_code)
)]
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
#[cfg_attr(
    not(any(
        feature = "backend-x11rb",
        feature = "backend-xcb",
        feature = "remote-x11",
        test
    )),
    allow(dead_code)
)]
pub(crate) const X11_MAX_MULTIPLE_CONVERSIONS: usize = 64;

/// Total byte references retained by active outgoing INCR transfers. Shared
/// payloads are deliberately counted once per requestor: that conservative
/// accounting makes many stalled clients hit a deterministic ceiling even
/// though their `Arc`s point at the same allocation.
#[cfg_attr(
    not(any(
        feature = "backend-x11rb",
        feature = "backend-xcb",
        feature = "remote-x11",
        test
    )),
    allow(dead_code)
)]
pub(crate) const X11_MAX_ACTIVE_INCR_BYTES: usize = 512 * 1024 * 1024;

/// Largest single payload JWM will offer through X11. This comfortably covers
/// an uncompressed 8K RGBA frame while bounding memory retained by a corrupt
/// or accidental producer before any requestor asks for it.
pub(crate) const X11_MAX_OFFER_BYTES: usize = 512 * 1024 * 1024;

#[cfg(test)]
pub(crate) static X11_CLIPBOARD_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
#[cfg_attr(
    not(any(
        feature = "backend-x11rb",
        feature = "backend-xcb",
        feature = "remote-x11",
        feature = "wayland-backends",
        test
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

/// Pick an `image/png` offer for history capture.
///
/// JPEG/BMP are intentionally ignored: the picker stores and re-offers PNG
/// only. Callers still prefer text when [`preferred_text_mime`] finds one.
#[must_use]
pub fn preferred_image_mime(mime_types: &[String]) -> Option<String> {
    mime_types
        .iter()
        .find(|mime| mime.eq_ignore_ascii_case("image/png"))
        .cloned()
}

/// What the history should request from an offer, after secret filtering.
///
/// Policy: text wins when present; otherwise PNG. Everything else is skipped.
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
    fn image_png_is_the_only_history_image_mime() {
        assert_eq!(
            preferred_image_mime(&["image/png".to_string()]).as_deref(),
            Some("image/png")
        );
        assert_eq!(
            preferred_image_mime(&["IMAGE/PNG".to_string()]).as_deref(),
            Some("IMAGE/PNG")
        );
        assert_eq!(
            preferred_image_mime(&["image/jpeg".to_string(), "image/bmp".to_string()]),
            None
        );
    }

    #[test]
    fn history_mime_prefers_text_over_png() {
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
            preferred_history_mime(&["image/jpeg".to_string(), "text/uri-list".to_string()]),
            None
        );
    }

    #[test]
    fn image_history_cap_dwarfs_the_text_cap() {
        assert!(MAX_IMAGE_HISTORY_BYTES > MAX_TEXT_BYTES);
        assert_eq!(MAX_IMAGE_HISTORY_BYTES, 4 * 1024 * 1024);
    }
}
