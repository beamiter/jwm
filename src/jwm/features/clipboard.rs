//! Clipboard history.
//!
//! What the compositor remembers of the clipboard, and what it refuses to
//! remember. The backends differ completely — X11 needs XFIXES monitoring and
//! selection ownership, Wayland reads its own data device — so everything
//! they share lives here: the bounded store, deduplication, the previews and
//! the type-to-filter match the picker renders, and the rules that keep
//! secrets out.
//!
//! The history is **memory only**. It is never written to disk and does not
//! survive a restart; a clipboard manager that persisted passwords to a file
//! would be a liability, not a feature. Text and PNG shares one newest-first
//! list; JPEG/BMP and remote image payloads are out of scope.

use crate::config::CONFIG;
use std::collections::VecDeque;

/// Entries kept before the oldest is dropped.
pub const MAX_ENTRIES: usize = 50;
/// Longest preview drawn in the picker.
const MAX_PREVIEW_CHARS: usize = 72;
/// What may be recorded and which type to ask for are judged from MIME names
/// alone, so the backends share those rules rather than reimplementing them.
pub use crate::backend::clipboard_offer::{
    MAX_IMAGE_HISTORY_BYTES, MAX_TEXT_BYTES, is_secret, preferred_image_mime, preferred_text_mime,
};

/// One remembered clipboard payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardEntry {
    Text {
        text: String,
        /// Wall-clock milliseconds when it was captured.
        captured_unix_ms: u64,
    },
    Png {
        bytes: Vec<u8>,
        width: Option<u32>,
        height: Option<u32>,
        /// Wall-clock milliseconds when it was captured.
        captured_unix_ms: u64,
    },
}

impl ClipboardEntry {
    #[must_use]
    pub fn captured_unix_ms(&self) -> u64 {
        match self {
            Self::Text {
                captured_unix_ms, ..
            }
            | Self::Png {
                captured_unix_ms, ..
            } => *captured_unix_ms,
        }
    }

    fn set_captured_unix_ms(&mut self, now_unix_ms: u64) {
        match self {
            Self::Text {
                captured_unix_ms, ..
            }
            | Self::Png {
                captured_unix_ms, ..
            } => *captured_unix_ms = now_unix_ms,
        }
    }
}

/// Bounded clipboard history, newest first.
#[derive(Debug, Default)]
pub struct ClipboardHistory {
    entries: VecDeque<ClipboardEntry>,
}

impl ClipboardHistory {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a text copy, returning whether the history changed.
    ///
    /// Copying something already in the history moves it back to the top
    /// rather than adding a duplicate — the list is "what I might paste
    /// next", so recency is the useful order and repeats are noise.
    pub fn record(&mut self, text: &str, now_unix_ms: u64) -> bool {
        if text.trim().is_empty() || text.len() > MAX_TEXT_BYTES {
            return false;
        }
        if let Some(index) = self.entries.iter().position(|entry| match entry {
            ClipboardEntry::Text { text: existing, .. } => existing == text,
            ClipboardEntry::Png { .. } => false,
        }) {
            return self.reorder_existing(index, now_unix_ms);
        }
        self.entries.push_front(ClipboardEntry::Text {
            text: text.to_string(),
            captured_unix_ms: now_unix_ms,
        });
        self.trim_to_capacity();
        true
    }

    /// Record a PNG copy under the image history cap.
    ///
    /// Empty and oversized payloads are rejected. Byte-identical images are
    /// reordered like text rather than duplicated, so a Wayland screenshot
    /// that both publishes via `wl-copy` and records explicitly collapses to
    /// one newest entry.
    pub fn record_png(&mut self, bytes: &[u8], now_unix_ms: u64) -> bool {
        if bytes.is_empty() || bytes.len() > MAX_IMAGE_HISTORY_BYTES {
            return false;
        }
        if let Some(index) = self.entries.iter().position(|entry| match entry {
            ClipboardEntry::Png {
                bytes: existing, ..
            } => existing.as_slice() == bytes,
            ClipboardEntry::Text { .. } => false,
        }) {
            return self.reorder_existing(index, now_unix_ms);
        }
        let (width, height) = png_dimensions(bytes);
        self.entries.push_front(ClipboardEntry::Png {
            bytes: bytes.to_vec(),
            width,
            height,
            captured_unix_ms: now_unix_ms,
        });
        self.trim_to_capacity();
        true
    }

    fn reorder_existing(&mut self, index: usize, now_unix_ms: u64) -> bool {
        if index == 0 {
            // Already the most recent: nothing to reorder.
            if let Some(entry) = self.entries.front_mut() {
                entry.set_captured_unix_ms(now_unix_ms);
            }
            return false;
        }
        let Some(mut entry) = self.entries.remove(index) else {
            return false;
        };
        entry.set_captured_unix_ms(now_unix_ms);
        self.entries.push_front(entry);
        true
    }

    fn trim_to_capacity(&mut self) {
        while self.entries.len() > MAX_ENTRIES {
            self.entries.pop_back();
        }
    }

    /// Newest first — the order the picker lists them.
    pub fn entries(&self) -> impl Iterator<Item = &ClipboardEntry> {
        self.entries.iter()
    }

    #[must_use]
    pub fn get(&self, index: usize) -> Option<&ClipboardEntry> {
        self.entries.get(index)
    }

    /// Drop one entry, returning whether it existed.
    pub fn remove(&mut self, index: usize) -> bool {
        self.entries.remove(index).is_some()
    }

    /// Drop everything, returning how many entries were removed.
    pub fn clear(&mut self) -> usize {
        let count = self.entries.len();
        self.entries.clear();
        count
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// One line, whitespace collapsed, ellipsized — clipboard text is routinely
/// multi-line and the panel does not wrap.
#[must_use]
pub fn preview(text: &str) -> String {
    preview_chars(text.chars())
}

fn preview_chars(chars: impl IntoIterator<Item = char>) -> String {
    let mut out = String::with_capacity(MAX_PREVIEW_CHARS);
    let mut output_chars = 0;
    let mut pending_space = false;

    for ch in chars {
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            if push_preview_char(&mut out, &mut output_chars, ' ') {
                return out;
            }
            pending_space = false;
        }
        if push_preview_char(&mut out, &mut output_chars, ch) {
            return out;
        }
    }
    out
}

/// Append one collapsed character. The first character beyond the visible
/// limit proves truncation is needed, so replace the last visible character
/// with an ellipsis and let the caller stop consuming the source immediately.
fn push_preview_char(out: &mut String, output_chars: &mut usize, ch: char) -> bool {
    if *output_chars == MAX_PREVIEW_CHARS {
        let _ = out.pop();
        out.push('\u{2026}');
        return true;
    }
    out.push(ch);
    *output_chars += 1;
    false
}

/// Compact byte size for PNG picker labels (`1.2M`, `512K`, `42B`).
#[must_use]
pub fn format_byte_size(bytes: usize) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    if bytes < 1024 {
        format!("{bytes}B")
    } else if (bytes as f64) < MIB {
        let kib = bytes as f64 / KIB;
        if kib < 10.0 {
            format!("{kib:.1}K")
        } else {
            format!("{kib:.0}K")
        }
    } else {
        format!("{:.1}M", bytes as f64 / MIB)
    }
}

/// Human label for a PNG history entry (also the IPC preview).
#[must_use]
pub fn png_preview_label(bytes: usize, width: Option<u32>, height: Option<u32>) -> String {
    let size = format_byte_size(bytes);
    match (width, height) {
        (Some(w), Some(h)) => format!("PNG {w}\u{00d7}{h}  {size}"),
        _ => format!("PNG \u{00b7} {size}"),
    }
}

/// Read width/height from a PNG IHDR when the leading bytes look valid.
#[must_use]
pub fn png_dimensions(bytes: &[u8]) -> (Option<u32>, Option<u32>) {
    // signature(8) + length(4) + type(4) + width(4) + height(4)
    const IHDR_PREFIX: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR";
    if bytes.len() < 24 || !bytes.starts_with(IHDR_PREFIX) {
        return (None, None);
    }
    let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    if width == 0 || height == 0 {
        return (None, None);
    }
    (Some(width), Some(height))
}

/// One picker row: position, a hint of how much was copied, and the preview.
///
/// PNG rows are text labels only — no thumbnails and no per-row image icons
/// beyond the shared FontAwesome-4 glyph.
#[must_use]
pub fn picker_row(entry: &ClipboardEntry, index: usize) -> String {
    match entry {
        ClipboardEntry::Text { text, .. } => {
            let lines = text.lines().count();
            let shape = if lines > 1 {
                format!("{lines}L")
            } else {
                format!("{}c", text.chars().count())
            };
            format!("\u{f0ea} {:>2}  {:<6} {}", index + 1, shape, preview(text))
        }
        ClipboardEntry::Png {
            bytes,
            width,
            height,
            ..
        } => {
            // FA4 `file-image-o` stays below the 0xf600 FA5 floor the glyph
            // pin test enforces.
            format!(
                "\u{f1c5} {:>2}  {}",
                index + 1,
                png_preview_label(bytes.len(), *width, *height)
            )
        }
    }
}

/// The picker's type-to-filter. Text matches a case-insensitive substring of
/// the full payload; PNG matches a synthetic haystack (`png`, `image`, and
/// dimension tokens). An empty query matches everything.
#[must_use]
pub fn matches_query(entry: &ClipboardEntry, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let needle = query.to_lowercase();
    match entry {
        ClipboardEntry::Text { text, .. } => text.to_lowercase().contains(&needle),
        ClipboardEntry::Png { width, height, .. } => {
            let mut haystack = String::from("png image");
            if let (Some(w), Some(h)) = (*width, *height) {
                haystack.push(' ');
                haystack.push_str(&w.to_string());
                haystack.push(' ');
                haystack.push_str(&h.to_string());
                haystack.push(' ');
                haystack.push_str(&format!("{w}x{h}"));
                haystack.push(' ');
                haystack.push_str(&format!("{w}\u{00d7}{h}"));
            }
            haystack.to_lowercase().contains(&needle)
        }
    }
}

/// Wall-clock milliseconds, shared with the notification history.
#[must_use]
fn now_unix_ms() -> u64 {
    crate::jwm::features::notifications::now_unix_ms()
}

impl crate::jwm::Jwm {
    /// Record a copy the backend captured. Offers marked secret never reach
    /// this: the backends drop them before reading the payload, so a password
    /// is not copied into the compositor's memory only to be discarded.
    pub(crate) fn record_clipboard(&mut self, text: &str) -> bool {
        if !CONFIG.load().behavior().clipboard_history {
            return false;
        }
        let changed = self.features.clipboard.record(text, now_unix_ms());
        if changed {
            self.on_clipboard_history_changed();
        }
        changed
    }

    /// Record a PNG the backend captured or that a screenshot published.
    pub(crate) fn record_clipboard_png(&mut self, bytes: &[u8]) -> bool {
        if !CONFIG.load().behavior().clipboard_history {
            return false;
        }
        let changed = self.features.clipboard.record_png(bytes, now_unix_ms());
        if changed {
            self.on_clipboard_history_changed();
        }
        changed
    }

    fn on_clipboard_history_changed(&mut self) {
        self.features
            .system_ui
            .refresh_clipboard(&self.features.clipboard);
        self.broadcast_ipc_event(
            "clipboard/changed",
            serde_json::json!({ "count": self.features.clipboard.len() }),
        );
        self.refresh_open_control_center();
    }

    /// Re-offer a PNG through the backend's native owner when available.
    ///
    /// Preference: X11 `clipboard_image_sender`, then Wayland
    /// [`Backend::set_clipboard_png`](crate::backend::api::Backend::set_clipboard_png),
    /// then `wl-copy` as a last-resort platform helper.
    pub(crate) fn offer_clipboard_png(
        &self,
        backend: &mut dyn crate::backend::api::Backend,
        png: Vec<u8>,
    ) -> bool {
        if let Some(sender) = backend.clipboard_image_sender() {
            return sender.send_png(png);
        }
        if backend.set_clipboard_png(png.clone()) {
            return true;
        }
        Self::publish_png_bytes_via_wl_copy(&png)
    }

    /// Drop the whole history.
    pub(crate) fn clear_clipboard_history(&mut self) -> usize {
        let cleared = self.features.clipboard.clear();
        self.features
            .system_ui
            .refresh_clipboard(&self.features.clipboard);
        if cleared > 0 {
            self.broadcast_ipc_event(
                "clipboard/changed",
                serde_json::json!({ "count": 0, "cleared": cleared }),
            );
            self.refresh_open_control_center();
        }
        cleared
    }

    /// JSON snapshot for the `get_clipboard` query.
    ///
    /// Previews only: the full text of every copy (and never raw image bytes)
    /// is exactly what a compromised IPC client should not be handed in one
    /// request.
    pub(crate) fn clipboard_json(&self) -> serde_json::Value {
        let items: Vec<serde_json::Value> = self
            .features
            .clipboard
            .entries()
            .enumerate()
            .map(|(index, entry)| match entry {
                ClipboardEntry::Text {
                    text,
                    captured_unix_ms,
                } => serde_json::json!({
                    "index": index,
                    "kind": "text",
                    "preview": preview(text),
                    "chars": text.chars().count(),
                    "captured_unix_ms": captured_unix_ms,
                }),
                ClipboardEntry::Png {
                    bytes,
                    width,
                    height,
                    captured_unix_ms,
                } => {
                    let mut item = serde_json::json!({
                        "index": index,
                        "kind": "png",
                        "preview": png_preview_label(bytes.len(), *width, *height),
                        "bytes": bytes.len(),
                        "captured_unix_ms": captured_unix_ms,
                    });
                    if let Some(width) = width {
                        item["width"] = serde_json::json!(width);
                    }
                    if let Some(height) = height {
                        item["height"] = serde_json::json!(height);
                    }
                    item
                }
            })
            .collect();
        serde_json::json!({
            "enabled": CONFIG.load().behavior().clipboard_history,
            "count": items.len(),
            "capacity": MAX_ENTRIES,
            "entries": items,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_entry(text: &str) -> ClipboardEntry {
        ClipboardEntry::Text {
            text: text.to_string(),
            captured_unix_ms: 0,
        }
    }

    fn sample_png(width: u32, height: u32, pad: usize) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(24 + pad);
        bytes.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.resize(24 + pad, 0);
        bytes
    }

    #[test]
    fn copies_are_recorded_newest_first() {
        let mut history = ClipboardHistory::new();
        assert!(history.record("first", 1_000));
        assert!(history.record("second", 2_000));

        let texts: Vec<&str> = history
            .entries()
            .map(|e| match e {
                ClipboardEntry::Text { text, .. } => text.as_str(),
                ClipboardEntry::Png { .. } => panic!("expected text"),
            })
            .collect();
        assert_eq!(texts, ["second", "first"]);
    }

    #[test]
    fn recopying_moves_an_entry_back_to_the_top() {
        let mut history = ClipboardHistory::new();
        history.record("a", 1_000);
        history.record("b", 2_000);
        history.record("c", 3_000);

        assert!(history.record("a", 4_000));
        let texts: Vec<&str> = history
            .entries()
            .map(|e| match e {
                ClipboardEntry::Text { text, .. } => text.as_str(),
                ClipboardEntry::Png { .. } => panic!("expected text"),
            })
            .collect();
        assert_eq!(texts, ["a", "c", "b"], "no duplicate, just reordered");
        assert_eq!(history.len(), 3);
        assert_eq!(history.get(0).unwrap().captured_unix_ms(), 4_000);
    }

    #[test]
    fn recopying_the_newest_entry_changes_nothing() {
        let mut history = ClipboardHistory::new();
        history.record("a", 1_000);
        // The same text copied twice in a row is the common case; it must not
        // report a change and churn the panel.
        assert!(!history.record("a", 2_000));
        assert_eq!(history.len(), 1);
        assert_eq!(history.get(0).unwrap().captured_unix_ms(), 2_000);
    }

    #[test]
    fn empty_and_whitespace_copies_are_ignored() {
        let mut history = ClipboardHistory::new();
        assert!(!history.record("", 1_000));
        assert!(!history.record("   \n\t ", 1_000));
        assert!(history.is_empty());
    }

    #[test]
    fn oversized_payloads_are_ignored() {
        let mut history = ClipboardHistory::new();
        let huge = "x".repeat(MAX_TEXT_BYTES + 1);
        assert!(!history.record(&huge, 1_000));
        assert!(history.is_empty());

        // Exactly at the limit is still fine.
        assert!(history.record(&"y".repeat(MAX_TEXT_BYTES), 1_000));
    }

    #[test]
    fn the_history_is_bounded() {
        let mut history = ClipboardHistory::new();
        for index in 0..(MAX_ENTRIES + 10) {
            history.record(&format!("entry {index}"), index as u64);
        }
        assert_eq!(history.len(), MAX_ENTRIES);
        assert_eq!(
            match history.get(0).unwrap() {
                ClipboardEntry::Text { text, .. } => text.as_str(),
                ClipboardEntry::Png { .. } => panic!("expected text"),
            },
            format!("entry {}", MAX_ENTRIES + 9)
        );
    }

    #[test]
    fn png_copies_share_the_newest_first_list_with_text() {
        let mut history = ClipboardHistory::new();
        assert!(history.record("note", 1_000));
        let png = sample_png(1920, 1080, 8);
        assert!(history.record_png(&png, 2_000));
        assert!(history.record("later", 3_000));

        let kinds: Vec<&str> = history
            .entries()
            .map(|entry| match entry {
                ClipboardEntry::Text { .. } => "text",
                ClipboardEntry::Png { .. } => "png",
            })
            .collect();
        assert_eq!(kinds, ["text", "png", "text"]);
    }

    #[test]
    fn identical_pngs_are_reordered_not_duplicated() {
        let mut history = ClipboardHistory::new();
        let png = sample_png(64, 48, 4);
        assert!(history.record_png(&png, 1_000));
        history.record("interrupt", 2_000);
        assert!(history.record_png(&png, 3_000));
        assert_eq!(history.len(), 2);
        assert!(matches!(history.get(0), Some(ClipboardEntry::Png { .. })));
        assert_eq!(history.get(0).unwrap().captured_unix_ms(), 3_000);
        // Same bytes already newest: timestamp updates, no churn flag.
        assert!(!history.record_png(&png, 4_000));
        assert_eq!(history.get(0).unwrap().captured_unix_ms(), 4_000);
    }

    #[test]
    fn empty_and_oversized_pngs_are_ignored() {
        let mut history = ClipboardHistory::new();
        assert!(!history.record_png(&[], 1_000));
        let huge = vec![0u8; MAX_IMAGE_HISTORY_BYTES + 1];
        assert!(!history.record_png(&huge, 1_000));
        assert!(history.is_empty());
        let exact = vec![1u8; MAX_IMAGE_HISTORY_BYTES];
        assert!(history.record_png(&exact, 1_000));
    }

    #[test]
    fn entries_can_be_removed_and_cleared() {
        let mut history = ClipboardHistory::new();
        history.record("a", 1);
        history.record("b", 2);

        assert!(history.remove(0));
        assert_eq!(
            match history.get(0).unwrap() {
                ClipboardEntry::Text { text, .. } => text.as_str(),
                ClipboardEntry::Png { .. } => panic!("expected text"),
            },
            "a"
        );
        assert!(!history.remove(5));

        history.record("c", 3);
        assert_eq!(history.clear(), 2);
        assert!(history.is_empty());
        assert_eq!(history.clear(), 0);
    }

    #[test]
    fn password_manager_hints_mark_a_secret() {
        assert!(is_secret(&["x-kde-passwordManagerHint".to_string()]));
        // Toolkits prefix it in different ways; the suffix is what matters.
        assert!(is_secret(&[
            "text/plain".to_string(),
            "application/x-kde-passwordManagerHint".to_string()
        ]));
        assert!(is_secret(&["X-KDE-PASSWORDMANAGERHINT".to_string()]));
        assert!(is_secret(&["x-secret".to_string()]));
    }

    #[test]
    fn ordinary_offers_are_not_secret() {
        assert!(!is_secret(&[]));
        assert!(!is_secret(&[
            "text/plain;charset=utf-8".to_string(),
            "UTF8_STRING".to_string()
        ]));
    }

    #[test]
    fn utf8_text_is_preferred_over_plain_bytes() {
        let offer = vec![
            "STRING".to_string(),
            "text/plain".to_string(),
            "text/plain;charset=utf-8".to_string(),
        ];
        assert_eq!(
            preferred_text_mime(&offer).as_deref(),
            Some("text/plain;charset=utf-8")
        );

        assert_eq!(
            preferred_text_mime(&["UTF8_STRING".to_string()]).as_deref(),
            Some("UTF8_STRING")
        );
    }

    #[test]
    fn an_offer_without_text_is_skipped_by_text_policy() {
        assert_eq!(
            preferred_text_mime(&["image/png".to_string(), "image/bmp".to_string()]),
            None
        );
        assert_eq!(preferred_text_mime(&[]), None);
    }

    #[test]
    fn previews_collapse_whitespace_to_one_line() {
        assert_eq!(preview("hello\n\tworld  again"), "hello world again");
    }

    #[test]
    fn long_previews_are_ellipsized() {
        let preview = preview(&"x".repeat(MAX_PREVIEW_CHARS + 20));
        assert_eq!(preview.chars().count(), MAX_PREVIEW_CHARS);
        assert!(preview.ends_with('\u{2026}'));
    }

    #[test]
    fn preview_stops_consuming_once_the_ellipsis_is_decided() {
        let exact = "x".repeat(MAX_PREVIEW_CHARS);
        assert_eq!(preview(&exact), exact);

        let source =
            std::iter::repeat('x')
                .take(MAX_PREVIEW_CHARS + 1)
                .chain(std::iter::once_with(|| {
                    panic!("preview consumed input after truncation was decided")
                }));
        let shortened = preview_chars(source);
        assert_eq!(shortened.chars().count(), MAX_PREVIEW_CHARS);
        assert!(shortened.ends_with('\u{2026}'));
    }

    #[test]
    fn rows_show_the_position_and_the_shape_of_what_was_copied() {
        let single = text_entry("hello");
        let row = picker_row(&single, 0);
        assert!(row.contains(" 1"));
        assert!(row.contains("5c"), "single-line copies show a length");
        assert!(row.contains("hello"));

        let multi = text_entry("one\ntwo\nthree");
        let row = picker_row(&multi, 1);
        assert!(row.contains("3L"), "multi-line copies show a line count");
        assert!(row.contains("one two three"));
    }

    #[test]
    fn png_rows_are_text_labels_with_dims_and_size() {
        let png = sample_png(1920, 1080, 100);
        let entry = ClipboardEntry::Png {
            bytes: png,
            width: Some(1920),
            height: Some(1080),
            captured_unix_ms: 0,
        };
        let row = picker_row(&entry, 3);
        assert!(row.contains(" 4"));
        assert!(row.contains("PNG 1920\u{00d7}1080"));
        assert!(row.contains('\u{f1c5}'));

        let unknown = ClipboardEntry::Png {
            bytes: vec![1, 2, 3, 4],
            width: None,
            height: None,
            captured_unix_ms: 0,
        };
        let row = picker_row(&unknown, 0);
        assert!(row.contains("PNG \u{00b7} 4B"));
    }

    #[test]
    fn the_filter_matches_a_case_insensitive_substring() {
        assert!(matches_query(&text_entry("Hello, World"), "hello"));
        assert!(matches_query(&text_entry("Hello, World"), "WORLD"));
        assert!(matches_query(
            &text_entry("https://example.com/docs"),
            "example.COm"
        ));
        // A match can live past what the one-line preview shows.
        assert!(matches_query(&text_entry("start\nmiddle\nend"), "middle"));
        assert!(!matches_query(&text_entry("Hello, World"), "goodbye"));
    }

    #[test]
    fn png_filter_matches_synthetic_tokens() {
        let entry = ClipboardEntry::Png {
            bytes: sample_png(800, 600, 0),
            width: Some(800),
            height: Some(600),
            captured_unix_ms: 0,
        };
        assert!(matches_query(&entry, ""));
        assert!(matches_query(&entry, "png"));
        assert!(matches_query(&entry, "IMAGE"));
        assert!(matches_query(&entry, "800"));
        assert!(matches_query(&entry, "600"));
        assert!(matches_query(&entry, "800x600"));
        assert!(!matches_query(&entry, "jpeg"));
    }

    #[test]
    fn an_empty_query_keeps_every_entry() {
        assert!(matches_query(&text_entry("anything"), ""));
        assert!(matches_query(&text_entry(""), ""));
    }

    #[test]
    fn the_filter_lowercases_both_sides_for_unicode() {
        assert!(matches_query(&text_entry("Café au lait"), "CAFÉ"));
        assert!(matches_query(&text_entry("RÉSUMÉ.md"), "résumé"));
    }

    #[test]
    fn every_glyph_stays_in_the_widely_available_range() {
        let text = text_entry("hello");
        let png = ClipboardEntry::Png {
            bytes: sample_png(1, 1, 0),
            width: Some(1),
            height: Some(1),
            captured_unix_ms: 0,
        };
        for entry in [&text, &png] {
            let row = picker_row(entry, 0);
            for ch in row
                .chars()
                .filter(|ch| ('\u{f000}'..'\u{f900}').contains(ch))
            {
                assert!(
                    (ch as u32) < 0xf600,
                    "{ch:?} is outside the FontAwesome-4 range"
                );
            }
        }
    }

    #[test]
    fn png_ihdr_dimensions_are_parsed_when_present() {
        let png = sample_png(1280, 720, 0);
        assert_eq!(png_dimensions(&png), (Some(1280), Some(720)));
        assert_eq!(png_dimensions(&[0, 1, 2]), (None, None));
    }

    #[test]
    fn clipboard_json_exposes_png_metadata_never_bytes() {
        let mut history = ClipboardHistory::new();
        history.record("hello", 1_000);
        let png = sample_png(10, 20, 5);
        history.record_png(&png, 2_000);

        // Exercise the JSON shape helpers used by `clipboard_json` without
        // constructing a full `Jwm`.
        let items: Vec<serde_json::Value> = history
            .entries()
            .enumerate()
            .map(|(index, entry)| match entry {
                ClipboardEntry::Text {
                    text,
                    captured_unix_ms,
                } => serde_json::json!({
                    "index": index,
                    "kind": "text",
                    "preview": preview(text),
                    "chars": text.chars().count(),
                    "captured_unix_ms": captured_unix_ms,
                }),
                ClipboardEntry::Png {
                    bytes,
                    width,
                    height,
                    captured_unix_ms,
                } => serde_json::json!({
                    "index": index,
                    "kind": "png",
                    "preview": png_preview_label(bytes.len(), *width, *height),
                    "bytes": bytes.len(),
                    "width": width,
                    "height": height,
                    "captured_unix_ms": captured_unix_ms,
                }),
            })
            .collect();

        assert_eq!(items[0]["kind"], "png");
        assert_eq!(items[0]["width"], 10);
        assert_eq!(items[0]["height"], 20);
        assert!(items[0].get("data").is_none());
        assert!(items[0].get("png").is_none());
        assert_eq!(items[1]["kind"], "text");
        assert_eq!(items[1]["chars"], 5);
    }

    /// Minimal backend that records PNG offers through `set_clipboard_png`.
    struct PngOfferBackend {
        window_ops: crate::backend::wayland_dummy_ops::DummyWindowOps,
        input_ops: crate::backend::wayland_dummy_ops::DummyInputOps,
        property_ops: crate::backend::wayland_dummy_ops::DummyPropertyOps,
        output_ops: crate::backend::wayland_dummy_ops::DummyOutputOps,
        key_ops: crate::backend::wayland_dummy_ops::DummyKeyOps,
        cursor_provider: crate::backend::wayland_dummy_ops::DummyCursorProvider,
        color_allocator: crate::backend::wayland_dummy_ops::DummyColorAllocator,
        offered_png: Option<Vec<u8>>,
        image_sender: Option<crate::backend::clipboard_offer::ClipboardImageSender>,
        set_clipboard_png_calls: usize,
    }

    impl PngOfferBackend {
        fn with_set_png() -> Self {
            Self {
                window_ops: crate::backend::wayland_dummy_ops::DummyWindowOps,
                input_ops: crate::backend::wayland_dummy_ops::DummyInputOps,
                property_ops: crate::backend::wayland_dummy_ops::DummyPropertyOps,
                output_ops: crate::backend::wayland_dummy_ops::DummyOutputOps,
                key_ops: crate::backend::wayland_dummy_ops::DummyKeyOps,
                cursor_provider: crate::backend::wayland_dummy_ops::DummyCursorProvider,
                color_allocator: crate::backend::wayland_dummy_ops::DummyColorAllocator,
                offered_png: None,
                image_sender: None,
                set_clipboard_png_calls: 0,
            }
        }

        fn with_image_sender(
            sender: crate::backend::clipboard_offer::ClipboardImageSender,
        ) -> Self {
            let mut backend = Self::with_set_png();
            backend.image_sender = Some(sender);
            backend
        }
    }

    impl crate::backend::api::CompositorBenchmark for PngOfferBackend {}
    impl crate::backend::api::BackendDiagnostics for PngOfferBackend {}
    impl crate::backend::api::CompositorControl for PngOfferBackend {}
    impl crate::backend::api::CompositorMedia for PngOfferBackend {}
    impl crate::backend::api::CompositorWorkspaceEffects for PngOfferBackend {}
    impl crate::backend::api::CompositorWindowEffects for PngOfferBackend {}
    impl crate::backend::api::CompositorAnnotation for PngOfferBackend {}
    impl crate::backend::api::DisplayControl for PngOfferBackend {}
    impl crate::backend::api::RenderScheduler for PngOfferBackend {}

    impl crate::backend::api::Backend for PngOfferBackend {
        fn set_clipboard_png(&mut self, png: Vec<u8>) -> bool {
            self.set_clipboard_png_calls += 1;
            self.offered_png = Some(png);
            true
        }

        fn clipboard_image_sender(
            &self,
        ) -> Option<crate::backend::clipboard_offer::ClipboardImageSender> {
            self.image_sender.clone()
        }

        fn capabilities(&self) -> crate::backend::api::Capabilities {
            crate::backend::api::Capabilities::default()
        }

        fn root_window(&self) -> Option<crate::backend::common_define::WindowId> {
            Some(crate::backend::common_define::WindowId::from_raw(0))
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        fn check_existing_wm(&self) -> Result<(), crate::backend::error::BackendError> {
            Ok(())
        }

        fn window_ops(&self) -> &dyn crate::backend::api::WindowOps {
            &self.window_ops
        }

        fn input_ops(&self) -> &dyn crate::backend::api::InputOps {
            &self.input_ops
        }

        fn property_ops(&self) -> &dyn crate::backend::api::PropertyOps {
            &self.property_ops
        }

        fn output_ops(&self) -> &dyn crate::backend::api::OutputOps {
            &self.output_ops
        }

        fn key_ops(&self) -> &dyn crate::backend::api::KeyOps {
            &self.key_ops
        }

        fn key_ops_mut(&mut self) -> &mut dyn crate::backend::api::KeyOps {
            &mut self.key_ops
        }

        fn cursor_provider(&mut self) -> &mut dyn crate::backend::api::CursorProvider {
            &mut self.cursor_provider
        }

        fn color_allocator(&mut self) -> &mut dyn crate::backend::api::ColorAllocator {
            &mut self.color_allocator
        }

        fn run(
            &mut self,
            _handler: &mut dyn crate::backend::api::EventHandler,
        ) -> Result<(), crate::backend::error::BackendError> {
            Ok(())
        }
    }

    #[test]
    fn offer_clipboard_png_prefers_set_clipboard_png_when_sender_absent() {
        let mut backend = PngOfferBackend::with_set_png();
        let jwm = crate::Jwm::new_with_runtime_backend(&mut backend, "test").expect("test jwm");
        let png = sample_png(8, 8, 2);
        assert!(jwm.offer_clipboard_png(&mut backend, png.clone()));
        assert_eq!(backend.set_clipboard_png_calls, 1);
        assert_eq!(backend.offered_png.as_deref(), Some(png.as_slice()));
    }

    #[test]
    fn offer_clipboard_png_prefers_image_sender_over_set_clipboard_png() {
        let (tx, rx) = std::sync::mpsc::channel();
        let sender = crate::backend::clipboard_offer::ClipboardImageSender::new(tx);
        let mut backend = PngOfferBackend::with_image_sender(sender);
        let jwm = crate::Jwm::new_with_runtime_backend(&mut backend, "test").expect("test jwm");
        let png = sample_png(4, 4, 1);
        assert!(jwm.offer_clipboard_png(&mut backend, png.clone()));
        assert_eq!(backend.set_clipboard_png_calls, 0);
        match rx.try_recv() {
            Ok(crate::backend::clipboard_offer::ClipboardOffer::Png(got)) => {
                assert_eq!(got, png);
            }
            other => panic!("expected PNG offer on image sender, got {other:?}"),
        }
    }

    #[test]
    fn activate_and_ipc_png_paths_call_offer_clipboard_png() {
        const TOGGLES: &str = include_str!("toggles.rs");
        const IPC: &str = include_str!("../ipc_handler.rs");
        assert!(
            TOGGLES.contains("self.offer_clipboard_png(backend, bytes.clone())"),
            "picker activate must re-offer PNG through offer_clipboard_png"
        );
        assert!(
            IPC.contains("self.offer_clipboard_png(backend, bytes.clone())"),
            "clipboard_copy IPC must re-offer PNG through offer_clipboard_png"
        );
        const OFFER: &str = include_str!("clipboard.rs");
        let offer = OFFER
            .split_once("fn offer_clipboard_png(")
            .expect("offer_clipboard_png")
            .1;
        assert!(
            offer.contains("clipboard_image_sender()")
                && offer.contains("set_clipboard_png(")
                && offer.contains("publish_png_bytes_via_wl_copy"),
            "PNG offer must prefer sender, then set_clipboard_png, then wl-copy"
        );
        let sender_at = offer.find("clipboard_image_sender()").expect("sender");
        let set_at = offer.find("set_clipboard_png(").expect("set_clipboard_png");
        let wl_at = offer
            .find("publish_png_bytes_via_wl_copy")
            .expect("wl-copy");
        assert!(sender_at < set_at && set_at < wl_at);
    }
}
