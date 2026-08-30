//! Clipboard history.
//!
//! What the compositor remembers of the clipboard, and what it refuses to
//! remember. The backends differ completely — X11 needs XFIXES monitoring and
//! selection ownership, Wayland reads its own data device — so everything
//! they share lives here: the bounded store, deduplication, the previews the
//! picker renders, and the rules that keep secrets out.
//!
//! The history is **memory only**. It is never written to disk and does not
//! survive a restart; a clipboard manager that persisted passwords to a file
//! would be a liability, not a feature.

use crate::config::CONFIG;
use std::collections::VecDeque;

/// Entries kept before the oldest is dropped.
pub const MAX_ENTRIES: usize = 50;
/// Longest preview drawn in the picker.
const MAX_PREVIEW_CHARS: usize = 72;
/// What may be recorded and which type to ask for are judged from MIME names
/// alone, so the backends share those rules rather than reimplementing them.
pub use crate::backend::clipboard_offer::{MAX_TEXT_BYTES, is_secret, preferred_text_mime};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardEntry {
    pub text: String,
    /// Wall-clock milliseconds when it was captured.
    pub captured_unix_ms: u64,
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

    /// Record a copy, returning whether the history changed.
    ///
    /// Copying something already in the history moves it back to the top
    /// rather than adding a duplicate — the list is "what I might paste
    /// next", so recency is the useful order and repeats are noise.
    pub fn record(&mut self, text: &str, now_unix_ms: u64) -> bool {
        if text.trim().is_empty() || text.len() > MAX_TEXT_BYTES {
            return false;
        }
        if let Some(index) = self.entries.iter().position(|entry| entry.text == text) {
            if index == 0 {
                // Already the most recent: nothing to reorder.
                if let Some(entry) = self.entries.front_mut() {
                    entry.captured_unix_ms = now_unix_ms;
                }
                return false;
            }
            let Some(mut entry) = self.entries.remove(index) else {
                return false;
            };
            entry.captured_unix_ms = now_unix_ms;
            self.entries.push_front(entry);
            return true;
        }
        self.entries.push_front(ClipboardEntry {
            text: text.to_string(),
            captured_unix_ms: now_unix_ms,
        });
        while self.entries.len() > MAX_ENTRIES {
            self.entries.pop_back();
        }
        true
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

/// One picker row: position, a hint of how much was copied, and the preview.
#[must_use]
pub fn picker_row(entry: &ClipboardEntry, index: usize) -> String {
    let lines = entry.text.lines().count();
    let shape = if lines > 1 {
        format!("{lines}L")
    } else {
        format!("{}c", entry.text.chars().count())
    };
    format!(
        "\u{f0ea} {:>2}  {:<6} {}",
        index + 1,
        shape,
        preview(&entry.text)
    )
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
            self.features
                .system_ui
                .refresh_clipboard(&self.features.clipboard);
            self.broadcast_ipc_event(
                "clipboard/changed",
                serde_json::json!({ "count": self.features.clipboard.len() }),
            );
            self.refresh_open_control_center();
        }
        changed
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
    /// Previews only: the full text of every copy is exactly what a
    /// compromised IPC client should not be handed in one request.
    pub(crate) fn clipboard_json(&self) -> serde_json::Value {
        let items: Vec<serde_json::Value> = self
            .features
            .clipboard
            .entries()
            .enumerate()
            .map(|(index, entry)| {
                serde_json::json!({
                    "index": index,
                    "preview": preview(&entry.text),
                    "chars": entry.text.chars().count(),
                    "captured_unix_ms": entry.captured_unix_ms,
                })
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

    #[test]
    fn copies_are_recorded_newest_first() {
        let mut history = ClipboardHistory::new();
        assert!(history.record("first", 1_000));
        assert!(history.record("second", 2_000));

        let texts: Vec<&str> = history.entries().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, ["second", "first"]);
    }

    #[test]
    fn recopying_moves_an_entry_back_to_the_top() {
        let mut history = ClipboardHistory::new();
        history.record("a", 1_000);
        history.record("b", 2_000);
        history.record("c", 3_000);

        assert!(history.record("a", 4_000));
        let texts: Vec<&str> = history.entries().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, ["a", "c", "b"], "no duplicate, just reordered");
        assert_eq!(history.len(), 3);
        assert_eq!(history.get(0).unwrap().captured_unix_ms, 4_000);
    }

    #[test]
    fn recopying_the_newest_entry_changes_nothing() {
        let mut history = ClipboardHistory::new();
        history.record("a", 1_000);
        // The same text copied twice in a row is the common case; it must not
        // report a change and churn the panel.
        assert!(!history.record("a", 2_000));
        assert_eq!(history.len(), 1);
        assert_eq!(history.get(0).unwrap().captured_unix_ms, 2_000);
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
            history.get(0).unwrap().text,
            format!("entry {}", MAX_ENTRIES + 9)
        );
    }

    #[test]
    fn entries_can_be_removed_and_cleared() {
        let mut history = ClipboardHistory::new();
        history.record("a", 1);
        history.record("b", 2);

        assert!(history.remove(0));
        assert_eq!(history.get(0).unwrap().text, "a");
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
    fn an_offer_without_text_is_skipped() {
        // A copied image is a legitimate payload this history cannot show.
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
        let single = ClipboardEntry {
            text: "hello".to_string(),
            captured_unix_ms: 0,
        };
        let row = picker_row(&single, 0);
        assert!(row.contains(" 1"));
        assert!(row.contains("5c"), "single-line copies show a length");
        assert!(row.contains("hello"));

        let multi = ClipboardEntry {
            text: "one\ntwo\nthree".to_string(),
            captured_unix_ms: 0,
        };
        let row = picker_row(&multi, 1);
        assert!(row.contains("3L"), "multi-line copies show a line count");
        assert!(row.contains("one two three"));
    }

    #[test]
    fn every_glyph_stays_in_the_widely_available_range() {
        let entry = ClipboardEntry {
            text: "hello".to_string(),
            captured_unix_ms: 0,
        };
        let row = picker_row(&entry, 0);
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
