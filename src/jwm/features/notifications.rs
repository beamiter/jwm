//! Backend-neutral notification history.
//!
//! JWM renders notifications itself (see `compositor_common::toast`), so it
//! also has to remember them: a toast that faded out while the user was in a
//! fullscreen app, or one that Do-Not-Disturb suppressed, must still be
//! reachable afterwards. This module owns that history — identifier
//! allocation, replacement, bounded eviction, and the pure row/age formatting
//! the notification-center panel renders — so the freedesktop bridge, the IPC
//! surface, and the panel all agree on one representation.
//!
//! The history survives a restart (see [`HISTORY_FILE`]): loading and saving
//! are a thin file-IO shell around the pure [`serialize_history`] /
//! [`parse_history`] pair, with the writes themselves on a thread of their
//! own (see `HistoryWriter`) so the compositor never waits on an fsync.
//! Everything else here is pure: no backend, no clock of its own (callers
//! pass the timestamp), so it is exercised directly by unit tests.

use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Records kept before the oldest is evicted. Deep enough to cover a work
/// session's backlog, bounded so a chatty application cannot grow the
/// compositor's heap without limit.
pub const MAX_HISTORY: usize = 64;

/// Where the history lives across restarts, under the user's data directory.
pub const HISTORY_FILE: &str = "notification-history";

/// On-disk schema version. A file carrying anything else is not read, so a
/// future migration starts from a clean, explicit break.
const HISTORY_FILE_VERSION: u32 = 1;

/// Per-record allowance the file size check on load is built from. A record
/// holds at most three [`MAX_TEXT_CHARS`]-character texts, [`MAX_ACTIONS`]
/// [`MAX_LABEL_CHARS`]-character labels and as many
/// [`MAX_ACTION_KEY_CHARS`]-character keys, which at the widest JSON escaping
/// comes to a little over 4 KiB (`history_written_by_this_version_always_loads`
/// pins it) — so the writer can never produce a file the loader refuses.
const MAX_HISTORY_RECORD_BYTES: u64 = 8192;

/// Largest history file read back; a larger one is rejected like any other
/// special or damaged source.
const MAX_HISTORY_BYTES: u64 = MAX_HISTORY as u64 * MAX_HISTORY_RECORD_BYTES + 1024;

static HISTORY_WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Longest summary/body kept; the panel does not wrap.
const MAX_TEXT_CHARS: usize = 96;

/// Actions kept from a sender's list. The strip is one line and the card is
/// as wide as its widest line, so a client offering a dozen buttons would
/// otherwise stretch the panel off the screen.
pub const MAX_ACTIONS: usize = 6;

/// Longest action label kept, for the same reason.
const MAX_LABEL_CHARS: usize = 20;

/// Longest action key kept. A key goes back to its sender verbatim over
/// `ActionInvoked`, so an over-long one is dropped rather than truncated — a
/// truncated key would name an action the sender never offered. Keys in the
/// wild are short identifiers; the cap is what lets a single record never
/// outgrow `MAX_HISTORY_RECORD_BYTES` and take the whole persisted history
/// with it on the next load.
pub const MAX_ACTION_KEY_CHARS: usize = 64;

/// The key the specification reserves for "the notification itself was
/// activated", rather than one of its buttons.
const DEFAULT_KEY: &str = "default";

/// One button a sender offered. Defined next to [`ToastNotification`] because
/// the toast card draws it too; re-exported so the existing module paths keep
/// working.
///
/// [`ToastNotification`]: crate::backend::api::ToastNotification
pub use crate::backend::api::NotificationAction;

/// Reason a notification left the history, matching the `NotificationClosed`
/// reason codes in the freedesktop notification specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// The notification's timeout expired.
    Expired = 1,
    /// The user dismissed it.
    Dismissed = 2,
    /// A `CloseNotification` call or the `close_notification` IPC closed it.
    Requested = 3,
    /// Undefined/reserved — used when clearing the whole history.
    Undefined = 4,
}

impl CloseReason {
    #[must_use]
    pub fn code(self) -> u32 {
        self as u32
    }
}

/// A notification as the shell remembers it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationRecord {
    /// Identifier handed back to the sender; never zero.
    pub id: u32,
    /// Sending application, when it identified itself.
    pub app: String,
    pub summary: String,
    pub body: String,
    /// 0 low, 1 normal, 2 critical — same scale as [`crate::backend::api::ToastNotification`].
    pub urgency: u8,
    /// Wall-clock milliseconds since the Unix epoch when the record was posted.
    pub posted_unix_ms: u64,
    /// True when Do-Not-Disturb suppressed the toast. The record is still
    /// kept so the notification center can show what was missed.
    pub suppressed: bool,
    /// The buttons the sender offered, in the order it offered them — the
    /// specification requires that order to be the display order.
    pub actions: Vec<NotificationAction>,
}

/// A posting request, before the center assigns an identifier.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NotificationRequest {
    pub app: String,
    pub summary: String,
    pub body: String,
    pub urgency: u8,
    /// Replace this identifier in place instead of appending, when it is still
    /// in the history. Zero means "new notification", per the specification.
    pub replaces_id: u32,
    pub actions: Vec<NotificationAction>,
}

/// Bounded, ordered notification history. Oldest record first.
#[derive(Debug, Default)]
pub struct NotificationCenter {
    records: VecDeque<NotificationRecord>,
    next_id: u32,
    /// Identifiers the cap pushed out since the last [`Self::take_evicted`].
    /// Each still owes its sender a close.
    evicted: Vec<u32>,
    /// The configuration's Do-Not-Disturb value the runtime toggle was last
    /// reconciled with; `None` until the configuration has been seen.
    config_do_not_disturb: Option<bool>,
    /// Where the history is written back. `None` for an in-memory center —
    /// tests, or one that never loaded — which persists nothing.
    path: Option<PathBuf>,
    /// The thread that owns the file, started by the first save.
    writer: Option<HistoryWriter>,
}

fn sanitize(text: &str) -> String {
    sanitize_chars(text.chars(), MAX_TEXT_CHARS)
}

fn sanitize_chars(chars: impl IntoIterator<Item = char>, limit: usize) -> String {
    if limit == 0 {
        return String::new();
    }
    let mut out = String::with_capacity(limit);
    let mut output_chars = 0;
    // Whitespace cannot be emitted until a later non-whitespace character
    // proves it is internal rather than trailing. Only a bounded prefix is
    // needed: once it would cross the visible limit, one more non-whitespace
    // character is enough to decide the ellipsis.
    let mut pending_whitespace = Vec::with_capacity(limit + 1);

    for ch in chars {
        let ch = if ch.is_control() { ' ' } else { ch };
        if ch.is_whitespace() {
            if !out.is_empty() && pending_whitespace.len() < limit + 1 - output_chars {
                pending_whitespace.push(ch);
            }
            continue;
        }

        for whitespace in pending_whitespace.drain(..) {
            if push_sanitized_char(&mut out, &mut output_chars, whitespace, limit) {
                return out;
            }
        }
        if push_sanitized_char(&mut out, &mut output_chars, ch, limit) {
            return out;
        }
    }
    out
}

fn push_sanitized_char(out: &mut String, output_chars: &mut usize, ch: char, limit: usize) -> bool {
    if *output_chars == limit {
        let _ = out.pop();
        out.push('\u{2026}');
        return true;
    }
    out.push(ch);
    *output_chars += 1;
    false
}

/// Trim a sender's action list to what the panel can show and the user can
/// act on.
///
/// An action with no key is dropped: invoking it would hand the sender back
/// an empty string, which tells it nothing. A blank label falls back to the
/// key, because a chip with no text is one the user cannot aim at. Repeated
/// keys are kept, both of them: dropping one would shift every later label
/// onto the wrong chip, and a sender that offered a duplicate cannot tell
/// them apart anyway.
fn sanitize_actions(actions: &[NotificationAction]) -> Vec<NotificationAction> {
    sanitize_action_iter(actions.iter())
}

/// Whether a sender's key can be kept: trimmed, non-empty, and short enough
/// to persist. Shared by the flat-list decoder and the record sanitizer so
/// both count the same entries against the cap and the `default` rescue.
fn action_key_is_usable(trimmed_key: &str) -> bool {
    !trimmed_key.is_empty() && trimmed_key.chars().nth(MAX_ACTION_KEY_CHARS).is_none()
}

fn sanitize_action_iter<'a>(
    actions: impl IntoIterator<Item = &'a NotificationAction>,
) -> Vec<NotificationAction> {
    let mut kept = Vec::with_capacity(MAX_ACTIONS);
    for action in actions {
        let key = action.key.trim();
        if !action_key_is_usable(key) {
            continue;
        }

        // Once the visible slots are full, only the first later `default`
        // can affect the result. Do not sanitize or allocate ignored labels.
        if kept.len() == MAX_ACTIONS && key != DEFAULT_KEY {
            continue;
        }
        let mut label = sanitize_label(&action.label);
        if label.is_empty() {
            // The key remains exact for ActionInvoked, but its fallback label
            // has the same one-line 20-character contract as any other label.
            label = sanitize_label(key);
        }
        let action = NotificationAction {
            key: key.to_string(),
            label,
        };
        if kept.len() < MAX_ACTIONS {
            let is_default = action.key == DEFAULT_KEY;
            kept.push(action);
            if kept.len() == MAX_ACTIONS
                && (is_default || kept.iter().any(|action| action.key == DEFAULT_KEY))
            {
                return kept;
            }
        } else {
            kept[MAX_ACTIONS - 1] = action;
            return kept;
        }
    }
    kept
}

fn sanitize_label(text: &str) -> String {
    sanitize_chars(text.chars(), MAX_LABEL_CHARS)
}

/// Decode the `actions` argument of a `notify` request.
///
/// D-Bus hands the list over flat — `[key, label, key, label, …]` — and this
/// is the only place that layout is understood. A trailing key with no label
/// is kept: a lone malformed `["open"]` must still be invokable.
///
/// Falls back to the older `default_action` string when no list is offered,
/// so a `jwm-bridge` installed before this change keeps working: the bridge
/// is installed separately from the compositor and mismatched pairs are
/// normal.
#[must_use]
pub fn parse_action_args(args: &serde_json::Value) -> Vec<NotificationAction> {
    if let Some(items) = args
        .get("actions")
        .and_then(serde_json::Value::as_array)
        .filter(|items| !items.is_empty())
    {
        return parse_flat_actions(items);
    }
    args.get("default_action")
        .and_then(serde_json::Value::as_str)
        .filter(|key| !key.trim().is_empty())
        .map(|key| {
            vec![NotificationAction {
                key: key.to_string(),
                label: String::new(),
            }]
        })
        .unwrap_or_default()
}

fn parse_flat_actions(items: &[serde_json::Value]) -> Vec<NotificationAction> {
    let mut kept = Vec::with_capacity(MAX_ACTIONS);
    for pair in items.chunks(2) {
        let key = pair[0].as_str().unwrap_or_default();
        let trimmed_key = key.trim();
        if !action_key_is_usable(trimmed_key) {
            continue;
        }
        if kept.len() == MAX_ACTIONS && trimmed_key != DEFAULT_KEY {
            continue;
        }
        let action = NotificationAction {
            key: key.to_string(),
            label: pair
                .get(1)
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
        };
        if kept.len() < MAX_ACTIONS {
            let is_default = trimmed_key == DEFAULT_KEY;
            kept.push(action);
            if kept.len() == MAX_ACTIONS
                && (is_default || kept.iter().any(|action| action.key.trim() == DEFAULT_KEY))
            {
                return kept;
            }
        } else {
            kept[MAX_ACTIONS - 1] = action;
            return kept;
        }
    }
    kept
}

/// Where a row's action cursor starts.
///
/// The reserved `default` key wherever it sits, else the first action. This
/// is the whole of the rule the bridge used to apply before it threw the rest
/// of the list away, so a notification offering one action, or offering an
/// explicit `default`, behaves exactly as it did.
#[must_use]
pub fn default_action_index(actions: &[NotificationAction]) -> usize {
    actions
        .iter()
        .position(|action| action.key == DEFAULT_KEY)
        .unwrap_or(0)
}

/// The chip line drawn under the selected row: numbered labels, with the one
/// under the cursor marked.
#[must_use]
pub fn action_strip(actions: &[NotificationAction], cursor: usize) -> String {
    let chips: Vec<String> = actions
        .iter()
        .enumerate()
        .map(|(index, action)| {
            let marker = if index == cursor { "\u{f00c}" } else { " " };
            format!("{marker}{} {}", index + 1, action.label)
        })
        .collect();
    format!("      \u{f0a9} {}", chips.join("   "))
}

/// Where the runtime Do-Not-Disturb toggle lands once the configuration is
/// (re)applied. The toggle is the user's most recent word and survives a
/// reload that did not touch the setting; a configuration whose value moved
/// — an edit to the file, a `set_config` — is newer than the toggle and wins.
/// With no earlier value to compare against, the configuration is adopted.
#[must_use]
pub fn do_not_disturb_after_config_apply(
    runtime: bool,
    previous_config: Option<bool>,
    config: bool,
) -> bool {
    match previous_config {
        Some(previous) if previous == config => runtime,
        _ => config,
    }
}

/// Whether one of jwm's own toasts — a reload result, a recording state —
/// may be shown. Do-Not-Disturb holds back everything but critical news: the
/// user asked for a quiet screen, and a reload that failed is the one thing
/// they would rather hear now than find in a log.
#[must_use]
pub fn system_toast_allowed(do_not_disturb: bool, urgency: u8) -> bool {
    !do_not_disturb || urgency >= 2
}

impl NotificationCenter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocate the next identifier. Zero is reserved by the specification for
    /// "not a notification", so the counter skips it on wrap.
    fn allocate_id(&mut self) -> u32 {
        self.next_id = self.next_id.wrapping_add(1);
        if self.next_id == 0 {
            self.next_id = 1;
        }
        self.next_id
    }

    /// Record a notification and return its identifier.
    ///
    /// `replaces_id` updates that record in place, keeping its position and
    /// identifier, which is how progress notifications stay a single row.
    /// Otherwise the record is appended and the oldest is evicted once the
    /// history is full.
    pub fn push(
        &mut self,
        request: &NotificationRequest,
        posted_unix_ms: u64,
        suppressed: bool,
    ) -> u32 {
        if request.replaces_id != 0
            && let Some(existing) = self
                .records
                .iter_mut()
                .find(|record| record.id == request.replaces_id)
        {
            existing.app = sanitize(&request.app);
            existing.summary = sanitize(&request.summary);
            existing.body = sanitize(&request.body);
            existing.urgency = request.urgency.min(2);
            existing.posted_unix_ms = posted_unix_ms;
            existing.suppressed = suppressed;
            // Replacement overwrites the buttons too: a progress
            // notification that stops offering Cancel stops showing it.
            existing.actions = sanitize_actions(&request.actions);
            return existing.id;
        }

        let id = self.allocate_id();
        self.records.push_back(NotificationRecord {
            id,
            app: sanitize(&request.app),
            summary: sanitize(&request.summary),
            body: sanitize(&request.body),
            urgency: request.urgency.min(2),
            posted_unix_ms,
            suppressed,
            actions: sanitize_actions(&request.actions),
        });
        while self.records.len() > MAX_HISTORY {
            if let Some(evicted) = self.records.pop_front() {
                self.evicted.push(evicted.id);
            }
        }
        id
    }

    /// Identifiers the history cap evicted since the last call. None of them
    /// was closed on its way out — a toast expiring closes nothing — and the
    /// executor owes each sender the one `notification/closed` the contract
    /// promises.
    pub fn take_evicted(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.evicted)
    }

    /// Reconcile the runtime Do-Not-Disturb toggle with a freshly applied
    /// configuration (see [`do_not_disturb_after_config_apply`]) and remember
    /// the configuration's value as the baseline for the next apply.
    pub fn reconcile_do_not_disturb(&mut self, runtime: bool, config: bool) -> bool {
        let next = do_not_disturb_after_config_apply(runtime, self.config_do_not_disturb, config);
        self.config_do_not_disturb = Some(config);
        next
    }

    /// Drop one record. Returns false when the identifier is unknown, which
    /// the IPC surface reports back instead of silently succeeding.
    pub fn close(&mut self, id: u32) -> bool {
        let Some(index) = self.records.iter().position(|record| record.id == id) else {
            return false;
        };
        self.records.remove(index);
        true
    }

    /// Drop every record, returning how many were removed.
    pub fn clear(&mut self) -> usize {
        let count = self.records.len();
        self.records.clear();
        count
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    #[must_use]
    pub fn get(&self, id: u32) -> Option<&NotificationRecord> {
        self.records.iter().find(|record| record.id == id)
    }

    /// Newest first — the order the notification center lists them.
    pub fn recent(&self) -> impl Iterator<Item = &NotificationRecord> {
        self.records.iter().rev()
    }

    /// Read the history back from disk. A missing file is an empty history,
    /// and so is an unreadable or malformed one — with a warning, because
    /// the notification center must work regardless of what is on disk.
    ///
    /// Also notes the configuration's Do-Not-Disturb value: the runtime
    /// toggle starts on it (`Jwm::new`), and a later configuration apply
    /// must compare against what the toggle started from.
    #[must_use]
    pub fn load() -> Self {
        let mut center = Self::load_from_path(&history_path());
        let config = crate::config::CONFIG.load();
        center.config_do_not_disturb = Some(config.behavior().do_not_disturb);
        center
    }

    /// The center writes back to where it was read from.
    fn load_from_path(path: &Path) -> Self {
        let mut center = match read_history(path) {
            Ok(text) => parse_history(&text).unwrap_or_else(|| {
                log::warn!(
                    "notification history at {} is not a v{HISTORY_FILE_VERSION} file; starting empty",
                    path.display()
                );
                Self::default()
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Self::default(),
            Err(error) => {
                log::warn!(
                    "notification history at {} cannot be read: {error}; starting empty",
                    path.display()
                );
                Self::default()
            }
        };
        center.path = Some(path.to_path_buf());
        center
    }

    /// Queue the history for its writer thread. A write is a rename plus an
    /// fsync of the file and of its directory — tens of milliseconds on a
    /// busy disk — and the posting path used to run one inline, on the
    /// compositor thread, for every notification, close and clear. Now it
    /// only copies the records: the thread folds a burst (a progress
    /// notification updating ten times a second) into one write per
    /// `HISTORY_WRITE_WINDOW`, and [`Self::flush`] lands the last one at
    /// shutdown. An in-memory center has nowhere to write and does nothing.
    pub fn save(&mut self) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let snapshot = HistorySnapshot {
            records: self.records.iter().cloned().collect(),
            next_id: self.next_id,
        };
        if self.writer.is_none() {
            match HistoryWriter::spawn(path.clone()) {
                Ok(writer) => self.writer = Some(writer),
                Err(error) => {
                    log::warn!("notifications: no history writer thread ({error}); writing inline");
                    write_history_now(&path, &snapshot);
                    return;
                }
            }
        }
        let rejected = match self.writer.as_ref() {
            Some(writer) => writer.send(snapshot).err(),
            None => Some(snapshot),
        };
        if let Some(snapshot) = rejected {
            // The thread only ends when its channel closes, so this is one
            // that died. Do not lose the change; the next save starts anew.
            self.writer = None;
            write_history_now(&path, &snapshot);
        }
    }

    /// Land whatever the writer thread still holds. Shutdown and restart call
    /// this so a notification posted a moment earlier is in the file the next
    /// process reads.
    pub fn flush(&mut self) {
        if let Some(writer) = self.writer.take() {
            let written = writer.finish();
            log::debug!("notifications: history writer landed {written} snapshot(s)");
        }
    }
}

/// Compact age label: `now`, `4m`, `2h`, `3d`. Clock jumps backwards (NTP
/// steps, suspend) read as `now` rather than a negative age.
#[must_use]
pub fn format_age(now_unix_ms: u64, posted_unix_ms: u64) -> String {
    let seconds = now_unix_ms.saturating_sub(posted_unix_ms) / 1000;
    if seconds < 60 {
        return "now".to_string();
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours}h");
    }
    format!("{}d", hours / 24)
}

/// Icon for an urgency level, matching the toast accent stripe.
#[must_use]
pub fn urgency_icon(urgency: u8) -> &'static str {
    match urgency {
        0 => "\u{f0f3}", // fa-bell, low
        2 => "\u{f071}", // fa-exclamation-triangle, critical
        _ => "\u{f0a2}", // fa-bell-o, normal
    }
}

/// One notification-center row: icon, app/summary, body preview, and age.
#[must_use]
pub fn panel_row(record: &NotificationRecord, now_unix_ms: u64) -> String {
    let icon = urgency_icon(record.urgency);
    let age = format_age(now_unix_ms, record.posted_unix_ms);
    let headline = if record.summary.is_empty() {
        record.body.clone()
    } else {
        record.summary.clone()
    };
    let detail = if record.summary.is_empty() {
        String::new()
    } else if record.body.is_empty() {
        String::new()
    } else {
        format!("  \u{2014}  {}", record.body)
    };
    let app = if record.app.is_empty() {
        String::new()
    } else {
        format!("[{}] ", record.app)
    };
    let muted = if record.suppressed { " \u{f1f6}" } else { "" };
    // Before the age, so a row with buttons is discoverable without having to
    // select it and see whether a strip appears.
    let has_actions = if record.actions.is_empty() {
        ""
    } else {
        " \u{f0a9}"
    };
    format!("{icon}  {app}{headline}{detail}{muted}{has_actions}   {age}")
}

/// The `notification/posted` payload: the record as the history keeps it,
/// never the request's raw text. The event fans out to every subscriber, and
/// a body near the IPC message limit would push each of their outbound
/// buffers over it and disconnect the bar and the bridge alike.
#[must_use]
pub fn posted_event_payload(record: &NotificationRecord) -> serde_json::Value {
    serde_json::json!({
        "id": record.id,
        "app": record.app,
        "summary": record.summary,
        "body": record.body,
        "urgency": record.urgency,
        "suppressed": record.suppressed,
    })
}

/// Wall-clock milliseconds since the Unix epoch, saturating at zero if the
/// system clock predates the epoch.
#[must_use]
pub fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

// -------------------------------------------------------------------------
// Persistence
// -------------------------------------------------------------------------

/// The history on disk: oldest record first, matching the in-memory order,
/// with the identifier counter so records restored from a previous run and
/// notifications posted after it can never share an identifier.
#[derive(Serialize, Deserialize)]
struct HistoryFile {
    version: u32,
    next_id: u32,
    notifications: Vec<NotificationRecord>,
}

/// Serialize the history as a versioned JSON document, keeping at most the
/// newest [`MAX_HISTORY`] records.
#[must_use]
pub fn serialize_history<'a>(
    records: impl IntoIterator<Item = &'a NotificationRecord>,
    next_id: u32,
) -> String {
    let mut kept: Vec<&NotificationRecord> = records.into_iter().collect();
    let overflow = kept.len().saturating_sub(MAX_HISTORY);
    kept.drain(..overflow);
    let file = HistoryFile {
        version: HISTORY_FILE_VERSION,
        next_id,
        notifications: kept.into_iter().cloned().collect(),
    };
    serde_json::to_string(&file).unwrap_or_else(|_| {
        // Strings, integers and vectors cannot fail to serialize; should that
        // ever change, an empty history still beats not writing at all.
        format!("{{\"version\":{HISTORY_FILE_VERSION},\"next_id\":{next_id},\"notifications\":[]}}")
    })
}

/// Parse a history file back into a center. Anything that is not a
/// well-formed v1 document — corrupt JSON, a missing or mismatched version —
/// is `None`, which the loader reports as an empty history.
///
/// Records are re-checked against the posting-time bounds rather than
/// trusted: the file is user-writable, so what lands in memory must satisfy
/// the same invariants as a record that arrived over IPC.
#[must_use]
pub fn parse_history(text: &str) -> Option<NotificationCenter> {
    let file: HistoryFile = serde_json::from_str(text).ok()?;
    if file.version != HISTORY_FILE_VERSION {
        return None;
    }
    // Oldest first on disk too, so an over-cap file keeps the newest records.
    let skip = file.notifications.len().saturating_sub(MAX_HISTORY);
    let mut next_id = file.next_id;
    let mut records = VecDeque::with_capacity(file.notifications.len() - skip);
    for mut record in file.notifications.into_iter().skip(skip) {
        // Zero is the specification's "not a notification"; a record holding
        // it would answer to lookups no client was ever handed.
        if record.id == 0 {
            continue;
        }
        // The counter must stay ahead of every restored identifier, whatever
        // the file claims.
        next_id = next_id.max(record.id);
        record.app = sanitize(&record.app);
        record.summary = sanitize(&record.summary);
        record.body = sanitize(&record.body);
        record.urgency = record.urgency.min(2);
        record.actions = sanitize_actions(&record.actions);
        records.push_back(record);
    }
    Some(NotificationCenter {
        records,
        next_id,
        ..NotificationCenter::default()
    })
}

fn read_history(path: &Path) -> io::Result<String> {
    // O_NONBLOCK matters for a path unexpectedly replaced with a FIFO: the
    // regular-file check can then reject it without waiting for a writer.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "notification history source is not a regular file: {}",
                path.display()
            ),
        ));
    }
    if metadata.len() > MAX_HISTORY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "notification history file exceeds its size limit",
        ));
    }

    // Keep the descriptor bounded too: metadata may be stale by the time the
    // read starts. The sentinel byte distinguishes an exact-limit file.
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_HISTORY_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_HISTORY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "notification history file exceeds its size limit",
        ));
    }
    String::from_utf8(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn atomic_write_history(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let sequence = HISTORY_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".{HISTORY_FILE}.tmp-{}-{sequence}",
        std::process::id()
    ));

    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        // `mode` is still filtered through the process umask. Set the final
        // private mode explicitly before the inode becomes visible at `path`.
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Longest a change waits to reach the disk. Changes inside one window share
/// a write, so a burst costs the disk one rename-and-fsync rather than one
/// per notification; a crash inside the window loses at most that much.
const HISTORY_WRITE_WINDOW: Duration = Duration::from_secs(1);

/// The records as handed to the writer thread.
#[derive(Debug)]
struct HistorySnapshot {
    records: Vec<NotificationRecord>,
    next_id: u32,
}

/// The thread that owns the history file. It takes snapshots over a channel,
/// collects whatever else arrives within [`HISTORY_WRITE_WINDOW`] of the
/// first, and writes only the newest; a closed channel ends the wait early
/// and the final snapshot lands before the thread exits, which is how
/// [`NotificationCenter::flush`] — and a plain drop — flush.
#[derive(Debug)]
struct HistoryWriter {
    sender: Option<mpsc::Sender<HistorySnapshot>>,
    thread: Option<thread::JoinHandle<()>>,
    /// Snapshots that reached the disk.
    writes: Arc<AtomicU64>,
}

impl HistoryWriter {
    fn spawn(path: PathBuf) -> io::Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let writes = Arc::new(AtomicU64::new(0));
        let written = Arc::clone(&writes);
        let thread = thread::Builder::new()
            .name("jwm-notification-history".to_string())
            .spawn(move || write_history_snapshots(&path, &receiver, &written))?;
        Ok(Self {
            sender: Some(sender),
            thread: Some(thread),
            writes,
        })
    }

    /// Hand the thread a snapshot. It comes back when the thread is gone.
    fn send(&self, snapshot: HistorySnapshot) -> Result<(), HistorySnapshot> {
        match &self.sender {
            Some(sender) => sender.send(snapshot).map_err(|error| error.0),
            None => Err(snapshot),
        }
    }

    /// Close the channel, wait for the last snapshot to land, and report how
    /// many were written.
    fn finish(mut self) -> u64 {
        self.join();
        self.writes.load(Ordering::Relaxed)
    }

    fn join(&mut self) {
        // Dropping the sender is what ends the loop, so it goes first.
        self.sender = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for HistoryWriter {
    fn drop(&mut self) {
        self.join();
    }
}

fn write_history_snapshots(
    path: &Path,
    receiver: &mpsc::Receiver<HistorySnapshot>,
    writes: &AtomicU64,
) {
    while let Ok(first) = receiver.recv() {
        let latest = newest_snapshot_within(first, receiver, Instant::now() + HISTORY_WRITE_WINDOW);
        if write_history_now(path, &latest) {
            writes.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Keep taking snapshots until `deadline`, or until the channel closes; the
/// newest is the one worth writing.
fn newest_snapshot_within(
    mut latest: HistorySnapshot,
    receiver: &mpsc::Receiver<HistorySnapshot>,
    deadline: Instant,
) -> HistorySnapshot {
    loop {
        let now = Instant::now();
        if now >= deadline {
            return latest;
        }
        match receiver.recv_timeout(deadline - now) {
            Ok(newer) => latest = newer,
            // Timed out, or the channel closed: either way, write what we have.
            Err(_) => return latest,
        }
    }
}

/// Serialize and write one snapshot. Failures are logged and dropped —
/// losing a snapshot is not worth interrupting a notification over.
fn write_history_now(path: &Path, snapshot: &HistorySnapshot) -> bool {
    let serialized = serialize_history(&snapshot.records, snapshot.next_id);
    match atomic_write_history(path, serialized.as_bytes()) {
        Ok(()) => true,
        Err(error) => {
            log::debug!("notifications: {}: {error}", path.display());
            false
        }
    }
}

fn history_path() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(dirs::data_dir)
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
    base.join("jwm").join(HISTORY_FILE)
}

impl crate::jwm::Jwm {
    /// Record a notification and, unless Do-Not-Disturb is on, show it as a
    /// native toast. Returns the identifier the sender should use to replace
    /// or close it.
    ///
    /// This is the single entry point for both the `notify` IPC and the
    /// freedesktop bridge, so history, toast, and the `notification/posted`
    /// event can never disagree.
    pub(crate) fn post_notification(
        &mut self,
        backend: &mut dyn crate::backend::api::Backend,
        request: &NotificationRequest,
        timeout_ms: u32,
    ) -> u32 {
        let suppressed = self.do_not_disturb;
        let id = self
            .features
            .notifications
            .push(request, now_unix_ms(), suppressed);
        // The cap evicts silently in the pure history. An evicted record was
        // never closed — its toast expiring closes nothing — and its sender
        // is still owed the one `NotificationClosed` the contract promises;
        // a later `CloseNotification` for it would find nothing to close.
        for evicted in self.features.notifications.take_evicted() {
            self.broadcast_ipc_event(
                "notification/closed",
                serde_json::json!({ "id": evicted, "reason": CloseReason::Undefined.code() }),
            );
        }
        self.features.notifications.save();

        if !suppressed {
            let title = if request.summary.trim().is_empty() {
                request.app.clone()
            } else {
                request.summary.clone()
            };
            // The toast carries the record's sanitized actions, so its
            // buttons invoke the exact keys the history offers.
            let actions = self
                .features
                .notifications
                .get(id)
                .map(|record| record.actions.clone())
                .unwrap_or_default();
            backend.compositor_push_toast(crate::backend::api::ToastNotification {
                title,
                body: request.body.clone(),
                urgency: request.urgency.min(2),
                timeout_ms,
                actions,
                notification_id: id,
            });
        }

        // The record, not the request: see `posted_event_payload`.
        let payload = self
            .features
            .notifications
            .get(id)
            .map(posted_event_payload)
            .unwrap_or_else(|| serde_json::json!({ "id": id, "suppressed": suppressed }));
        self.broadcast_ipc_event("notification/posted", payload);
        // A center left open while a notification arrives would otherwise show
        // a stale list.
        self.refresh_open_notification_center();
        self.refresh_open_control_center();
        id
    }

    /// Show one of jwm's own toasts — no record, no sender — under the same
    /// Do-Not-Disturb rule a notification gets (see [`system_toast_allowed`]).
    /// Returns whether it was shown.
    pub(crate) fn push_system_toast(
        &mut self,
        backend: &mut dyn crate::backend::api::Backend,
        toast: crate::backend::api::ToastNotification,
    ) -> bool {
        if !system_toast_allowed(self.do_not_disturb, toast.urgency) {
            log::debug!("toast {:?} held back by do-not-disturb", toast.title);
            return false;
        }
        backend.compositor_push_toast(toast);
        true
    }

    /// Drop one notification from the history and tell subscribers why, so the
    /// freedesktop bridge can emit `NotificationClosed` with the same reason.
    pub(crate) fn close_notification(&mut self, id: u32, reason: CloseReason) -> bool {
        if !self.features.notifications.close(id) {
            return false;
        }
        self.features.notifications.save();
        self.broadcast_ipc_event(
            "notification/closed",
            serde_json::json!({ "id": id, "reason": reason.code() }),
        );
        self.features.system_ui.remove_notification(id);
        self.refresh_open_control_center();
        self.repaint_open_notification_center();
        true
    }

    /// Drop the whole history, emitting one close event per notification so
    /// senders waiting on `NotificationClosed` are not left hanging.
    pub(crate) fn clear_notifications(&mut self) -> usize {
        let ids: Vec<u32> = self
            .features
            .notifications
            .recent()
            .map(|record| record.id)
            .collect();
        let count = self.features.notifications.clear();
        self.features.notifications.save();
        for id in ids {
            self.broadcast_ipc_event(
                "notification/closed",
                serde_json::json!({ "id": id, "reason": CloseReason::Undefined.code() }),
            );
        }
        self.features.system_ui.clear_notifications();
        self.refresh_open_control_center();
        self.repaint_open_notification_center();
        count
    }

    /// The open notification center lost rows in memory; the frame tick must
    /// push the new list. The keyboard paths sync the panel themselves, but a
    /// close that arrives over IPC — an application cancelling its own
    /// notification — has no such follow-up, and until something else
    /// repainted, the screen kept the old rows under the old highlight while
    /// Return acted on whatever had slid into the selected slot.
    fn repaint_open_notification_center(&mut self) {
        if self.features.system_ui.is_notification_center() {
            self.mark_system_ui_dirty();
        }
    }

    /// Report an action so the sending application can run it. The
    /// notification is closed the way the specification expects once one of
    /// its actions was invoked.
    ///
    /// Returns false for an identifier that is not in the history or a key
    /// the record never offered: no client may be handed an `ActionInvoked`
    /// for an action it did not register.
    pub(crate) fn invoke_notification_action(&mut self, id: u32, action: &str) -> bool {
        let offered = self
            .features
            .notifications
            .get(id)
            .is_some_and(|record| record.actions.iter().any(|entry| entry.key == action));
        if !offered {
            log::warn!("notification {id} was not offering the action {action:?}");
            return false;
        }
        // `ActionInvoked` before `NotificationClosed`, which is the order the
        // specification expects and the bridge's single event channel keeps.
        self.broadcast_ipc_event(
            "notification/action",
            serde_json::json!({ "id": id, "action": action }),
        );
        self.close_notification(id, CloseReason::Dismissed);
        true
    }

    /// Rebuild an open notification center against the live history, keeping
    /// the user's place.
    ///
    /// A notification arriving mid-pick would otherwise reset the selection to
    /// the newest row — moving the cursor from `Later` onto some other row's
    /// `Restart now` between reading it and pressing Return.
    fn refresh_open_notification_center(&mut self) {
        if !self.features.system_ui.is_notification_center() {
            return;
        }
        let held = self.features.system_ui.selected_notification_cursor();
        self.features.system_ui = crate::jwm::features::SystemUiState::notification_center(
            &self.features.notifications,
            now_unix_ms(),
        );
        if let Some((id, cursor)) = held {
            self.features
                .system_ui
                .restore_notification_cursor(id, cursor);
        }
        // As with the control center: rebuilt here, pushed by the tick. A
        // notification arriving while the center is open was otherwise
        // invisible until the user pressed a key.
        self.mark_system_ui_dirty();
    }

    /// JSON snapshot of the history for the `get_notifications` query.
    pub(crate) fn notifications_json(&self) -> serde_json::Value {
        let now = now_unix_ms();
        let items: Vec<serde_json::Value> = self
            .features
            .notifications
            .recent()
            .map(|record| {
                serde_json::json!({
                    "id": record.id,
                    "app": record.app,
                    "summary": record.summary,
                    "body": record.body,
                    "urgency": record.urgency,
                    "posted_unix_ms": record.posted_unix_ms,
                    "age": format_age(now, record.posted_unix_ms),
                    "suppressed": record.suppressed,
                    "actions": record.actions.iter().map(|action| serde_json::json!({
                        "key": action.key,
                        "label": action.label,
                    })).collect::<Vec<_>>(),
                    // Derived, and kept so readers written against the old
                    // single-action payload still find the key Return runs.
                    "default_action": record.actions
                        .get(default_action_index(&record.actions))
                        .map(|action| action.key.clone()),
                })
            })
            .collect();
        serde_json::json!({
            "do_not_disturb": self.do_not_disturb,
            "count": items.len(),
            "capacity": MAX_HISTORY,
            "notifications": items,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(summary: &str) -> NotificationRequest {
        NotificationRequest {
            app: "test".into(),
            summary: summary.into(),
            body: "body".into(),
            urgency: 1,
            replaces_id: 0,
            actions: Vec::new(),
        }
    }

    fn action(key: &str, label: &str) -> NotificationAction {
        NotificationAction {
            key: key.into(),
            label: label.into(),
        }
    }

    #[test]
    fn a_flat_list_pairs_up_in_the_order_it_was_sent() {
        let args = serde_json::json!({
            "actions": ["open", "Open folder", "later", "Later"]
        });
        assert_eq!(
            parse_action_args(&args),
            [action("open", "Open folder"), action("later", "Later")]
        );
    }

    #[test]
    fn a_trailing_key_without_a_label_is_still_invokable() {
        let args = serde_json::json!({ "actions": ["open"] });
        assert_eq!(parse_action_args(&args), [action("open", "")]);
        // …and the center gives it the key as its own label, so the chip is
        // something the user can aim at.
        let mut center = NotificationCenter::new();
        let id = center.push(
            &NotificationRequest {
                actions: parse_action_args(&args),
                ..request("a")
            },
            1_000,
            false,
        );
        assert_eq!(
            center.get(id).expect("record").actions,
            [action("open", "open")]
        );
    }

    #[test]
    fn an_older_bridge_that_sends_only_a_default_action_still_works() {
        // The bridge is installed separately from the compositor, so a
        // mismatched pair is normal and must keep working in both directions.
        let legacy = serde_json::json!({ "default_action": "open" });
        assert_eq!(parse_action_args(&legacy), [action("open", "")]);
        // With both, the list wins and the legacy field is ignored rather
        // than appended.
        let both = serde_json::json!({
            "actions": ["reply", "Reply"],
            "default_action": "open"
        });
        assert_eq!(parse_action_args(&both), [action("reply", "Reply")]);
        assert_eq!(parse_action_args(&serde_json::json!({})), []);
    }

    #[test]
    fn flat_action_parsing_is_bounded_and_rescues_default() {
        let mut flat = Vec::new();
        for index in 0..MAX_ACTIONS + 3 {
            flat.push(serde_json::Value::String(format!("k{index}")));
            flat.push(serde_json::Value::String(format!("Label {index}")));
        }
        flat.push(serde_json::Value::String("default".into()));
        flat.push(serde_json::Value::String("Activate".into()));

        let parsed = parse_action_args(&serde_json::json!({ "actions": flat }));
        assert_eq!(parsed.len(), MAX_ACTIONS);
        assert_eq!(parsed[MAX_ACTIONS - 1], action("default", "Activate"));
    }

    #[test]
    fn an_action_with_no_key_is_dropped_and_duplicates_are_kept() {
        let kept = sanitize_actions(&[
            action("", "Nowhere"),
            action("open", "Open"),
            action("open", "Open again"),
        ]);
        // An empty key would send the sender back an empty string. A repeated
        // key is kept twice: dropping one would slide every later label onto
        // the wrong chip.
        assert_eq!(kept, [action("open", "Open"), action("open", "Open again")]);
    }

    #[test]
    fn a_label_cannot_break_the_strip_onto_a_second_line() {
        let kept = sanitize_actions(&[action("k", "one\ntwo"), action("l", &"x".repeat(60))]);
        assert!(!kept[0].label.contains('\n'));
        assert!(kept[1].label.chars().count() <= MAX_LABEL_CHARS);
    }

    #[test]
    fn the_reserved_key_survives_the_cap_and_starts_under_the_cursor() {
        let many: Vec<NotificationAction> = (0..MAX_ACTIONS + 2)
            .map(|index| action(&format!("k{index}"), &format!("Label {index}")))
            .collect();
        let mut with_default = many.clone();
        with_default[MAX_ACTIONS + 1] = action("default", "Activate");

        let kept = sanitize_actions(&with_default);
        assert_eq!(kept.len(), MAX_ACTIONS);
        assert!(
            kept.iter().any(|entry| entry.key == "default"),
            "the key Return runs was truncated away"
        );
        // The cursor starts on it wherever it ended up.
        assert_eq!(kept[default_action_index(&kept)].key, "default");

        // No reserved key: the cursor starts on the first action, which is
        // the one deliberate behaviour change — safe because the strip shows
        // which one is selected.
        assert_eq!(default_action_index(&sanitize_actions(&many)), 0);
        assert_eq!(default_action_index(&[]), 0);
    }

    #[test]
    fn action_sanitation_stops_after_rescuing_default() {
        let mut offered: Vec<NotificationAction> = (0..MAX_ACTIONS)
            .map(|index| action(&format!("k{index}"), &format!("Label {index}")))
            .collect();
        offered.push(action("default", "Activate"));

        let source = offered
            .iter()
            .chain(std::iter::once_with(|| -> &NotificationAction {
                panic!("actions were consumed after the final result was fixed")
            }));
        let kept = sanitize_action_iter(source);
        assert_eq!(kept.len(), MAX_ACTIONS);
        assert_eq!(kept[MAX_ACTIONS - 1], action("default", "Activate"));
    }

    #[test]
    fn blank_action_labels_use_a_sanitized_bounded_key() {
        let key = format!("  open\n{}  ", "界".repeat(MAX_LABEL_CHARS + 8));
        let kept = sanitize_actions(&[action(&key, " \n ")]);

        assert_eq!(kept[0].key, key.trim());
        assert_eq!(kept[0].label.chars().count(), MAX_LABEL_CHARS);
        assert!(kept[0].label.ends_with('\u{2026}'));
        assert!(!kept[0].label.chars().any(char::is_control));
    }

    #[test]
    fn the_rescued_key_is_the_sanitized_one_even_after_an_entry_was_dropped() {
        // Two traps in one input: an unkeyed action ahead of the rest, which
        // shifts the filtered list out of step with the sender's, and a
        // `default` whose label is both too long and multi-line. Rescuing by
        // the sender's index would promote the wrong action *and* put an
        // uncapped, two-line label on a one-line strip.
        let mut offered = vec![action("", "dropped")];
        offered.extend(
            (0..MAX_ACTIONS + 1)
                .map(|index| action(&format!("k{index}"), &format!("Label {index}"))),
        );
        offered.push(action("default", &format!("Restart\n{}", "x".repeat(60))));

        let kept = sanitize_actions(&offered);
        assert_eq!(kept.len(), MAX_ACTIONS);
        let rescued = &kept[MAX_ACTIONS - 1];
        assert_eq!(rescued.key, "default", "the wrong action was promoted");
        assert!(rescued.label.chars().count() <= MAX_LABEL_CHARS);
        assert!(
            !rescued.label.contains('\n'),
            "a two-line label reached the strip"
        );
        assert_eq!(kept[default_action_index(&kept)].key, "default");
        // Nothing was duplicated into the freed slot.
        assert_eq!(kept[MAX_ACTIONS - 2].key, "k4");
    }

    #[test]
    fn a_row_with_buttons_says_so_before_it_is_selected() {
        let mut center = NotificationCenter::new();
        let plain = center.push(&request("plain"), 1_000, false);
        let with = center.push(
            &NotificationRequest {
                actions: vec![action("open", "Open")],
                ..request("offered")
            },
            1_000,
            false,
        );
        let marker = '\u{f0a9}';
        let row = |id| panel_row(center.get(id).expect("record"), 1_000);
        assert!(!row(plain).contains(marker));
        assert!(row(with).contains(marker));
        // The age stays last, which the panel's column alignment depends on.
        assert!(row(with).ends_with("now"));
    }

    #[test]
    fn the_strip_numbers_the_chips_and_marks_the_one_in_use() {
        let strip = action_strip(&[action("a", "Reply"), action("b", "Later")], 1);
        assert!(strip.contains("1 Reply") && strip.contains("2 Later"));
        assert!(
            strip.contains("\u{f00c}2 Later"),
            "cursor not marked: {strip:?}"
        );
        for ch in strip
            .chars()
            .filter(|ch| ('\u{f000}'..'\u{f900}').contains(ch))
        {
            assert!((ch as u32) < 0xf600, "{ch:?} is outside FontAwesome 4");
        }
    }

    #[test]
    fn replacing_a_notification_replaces_its_buttons_too() {
        let mut center = NotificationCenter::new();
        let id = center.push(
            &NotificationRequest {
                actions: vec![action("cancel", "Cancel")],
                ..request("copying")
            },
            1_000,
            false,
        );
        // The copy finished; there is nothing left to cancel.
        center.push(
            &NotificationRequest {
                replaces_id: id,
                actions: Vec::new(),
                ..request("copied")
            },
            2_000,
            false,
        );
        assert!(center.get(id).expect("record").actions.is_empty());
    }

    #[test]
    fn identifiers_start_at_one_and_increase() {
        let mut center = NotificationCenter::new();
        assert_eq!(center.push(&request("a"), 1_000, false), 1);
        assert_eq!(center.push(&request("b"), 2_000, false), 2);
    }

    #[test]
    fn identifier_wrap_skips_zero() {
        let mut center = NotificationCenter::new();
        center.next_id = u32::MAX;
        assert_eq!(center.allocate_id(), 1);
    }

    #[test]
    fn replaces_in_place_without_growing_history() {
        let mut center = NotificationCenter::new();
        let first = center.push(&request("copying 1%"), 1_000, false);
        let mut update = request("copying 90%");
        update.replaces_id = first;
        let second = center.push(&update, 2_000, false);

        assert_eq!(first, second);
        assert_eq!(center.len(), 1);
        let record = center.get(first).expect("record kept");
        assert_eq!(record.summary, "copying 90%");
        assert_eq!(record.posted_unix_ms, 2_000);
    }

    #[test]
    fn replacing_an_unknown_id_appends_a_new_record() {
        let mut center = NotificationCenter::new();
        let mut orphan = request("late update");
        orphan.replaces_id = 4242;
        let id = center.push(&orphan, 1_000, false);

        assert_ne!(id, 4242);
        assert_eq!(center.len(), 1);
    }

    #[test]
    fn history_is_bounded_and_evicts_oldest_first() {
        let mut center = NotificationCenter::new();
        for index in 0..(MAX_HISTORY + 8) {
            center.push(&request(&format!("n{index}")), index as u64, false);
        }
        assert_eq!(center.len(), MAX_HISTORY);
        let newest = center.recent().next().expect("records present");
        assert_eq!(newest.summary, format!("n{}", MAX_HISTORY + 7));
        let oldest = center.recent().last().expect("records present");
        assert_eq!(oldest.summary, "n8");
    }

    #[test]
    fn close_removes_only_the_named_record() {
        let mut center = NotificationCenter::new();
        let first = center.push(&request("a"), 1_000, false);
        let second = center.push(&request("b"), 2_000, false);

        assert!(center.close(first));
        assert!(!center.close(first));
        assert_eq!(center.len(), 1);
        assert!(center.get(second).is_some());
    }

    #[test]
    fn clear_reports_how_many_were_dropped() {
        let mut center = NotificationCenter::new();
        center.push(&request("a"), 1_000, false);
        center.push(&request("b"), 2_000, false);
        assert_eq!(center.clear(), 2);
        assert!(center.is_empty());
        assert_eq!(center.clear(), 0);
    }

    #[test]
    fn recent_lists_newest_first() {
        let mut center = NotificationCenter::new();
        center.push(&request("old"), 1_000, false);
        center.push(&request("new"), 2_000, false);
        let summaries: Vec<_> = center.recent().map(|r| r.summary.clone()).collect();
        assert_eq!(summaries, vec!["new", "old"]);
    }

    #[test]
    fn control_characters_and_overlong_text_are_sanitized() {
        let mut center = NotificationCenter::new();
        let mut noisy = request("line\nbreak");
        noisy.body = "x".repeat(MAX_TEXT_CHARS + 20);
        let id = center.push(&noisy, 1_000, false);
        let record = center.get(id).expect("record kept");

        assert_eq!(record.summary, "line break");
        assert_eq!(record.body.chars().count(), MAX_TEXT_CHARS);
        assert!(record.body.ends_with('\u{2026}'));
    }

    #[test]
    fn sanitation_preserves_unicode_boundaries_control_mapping_and_trim() {
        let exact = "界".repeat(MAX_TEXT_CHARS);
        assert_eq!(sanitize(&exact), exact);

        let over = sanitize(&"界".repeat(MAX_TEXT_CHARS + 1));
        assert_eq!(over.chars().count(), MAX_TEXT_CHARS);
        assert!(over.ends_with('\u{2026}'));
        assert_eq!(sanitize("\u{2003}\nalpha\0beta\t \u{2003}"), "alpha beta");
    }

    #[test]
    fn sanitation_stops_consuming_after_the_ellipsis_is_decided() {
        for limit in [MAX_TEXT_CHARS, MAX_LABEL_CHARS] {
            let source = std::iter::repeat('x')
                .take(limit + 1)
                .chain(std::iter::once_with(|| {
                    panic!("sanitation consumed input after truncation was decided")
                }));
            let shortened = sanitize_chars(source, limit);
            assert_eq!(shortened.chars().count(), limit);
            assert!(shortened.ends_with('\u{2026}'));
        }
    }

    #[test]
    fn urgency_is_clamped_to_the_toast_scale() {
        let mut center = NotificationCenter::new();
        let mut shouty = request("a");
        shouty.urgency = 9;
        let id = center.push(&shouty, 1_000, false);
        assert_eq!(center.get(id).expect("record kept").urgency, 2);
    }

    #[test]
    fn age_labels_step_through_the_units() {
        assert_eq!(format_age(10_000, 10_000), "now");
        assert_eq!(format_age(59_000, 0), "now");
        assert_eq!(format_age(60_000, 0), "1m");
        assert_eq!(format_age(3_600_000, 0), "1h");
        assert_eq!(format_age(86_400_000, 0), "1d");
    }

    #[test]
    fn a_backwards_clock_reads_as_now() {
        assert_eq!(format_age(1_000, 9_000), "now");
    }

    #[test]
    fn panel_row_carries_app_summary_body_and_age() {
        let mut center = NotificationCenter::new();
        let id = center.push(&request("Build finished"), 0, false);
        let row = panel_row(center.get(id).expect("record kept"), 120_000);

        assert!(row.contains("[test]"));
        assert!(row.contains("Build finished"));
        assert!(row.contains("body"));
        assert!(row.ends_with("2m"));
    }

    #[test]
    fn suppressed_rows_are_marked() {
        let mut center = NotificationCenter::new();
        let id = center.push(&request("missed"), 0, true);
        let row = panel_row(center.get(id).expect("record kept"), 0);
        assert!(row.contains('\u{f1f6}'));
    }

    #[test]
    fn a_summary_only_record_renders_without_a_dash() {
        let mut center = NotificationCenter::new();
        let mut terse = request("only summary");
        terse.body = String::new();
        let id = center.push(&terse, 0, false);
        let row = panel_row(center.get(id).expect("record kept"), 0);
        assert!(!row.contains('\u{2014}'));
    }

    // ---------------------------------------------------------------------
    // Persistence
    // ---------------------------------------------------------------------

    fn history_temp_root(tag: &str) -> std::path::PathBuf {
        let sequence = HISTORY_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "jwm-notification-history-{tag}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn history_round_trips_through_the_disk_format() {
        let mut center = NotificationCenter::new();
        center.push(
            &NotificationRequest {
                actions: vec![action("default", "Open")],
                ..request("first")
            },
            1_000,
            false,
        );
        center.push(&request("第 二 条"), 2_000, true);

        let text = serialize_history(&center.records, center.next_id);
        let document: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(document["version"], HISTORY_FILE_VERSION);

        let mut restored = parse_history(&text).expect("a v1 file parses");
        assert_eq!(restored.records, center.records);
        assert_eq!(restored.next_id, center.next_id);
        // The identifier counter survives too, so a post-restart notification
        // cannot collide with a restored one.
        assert_eq!(restored.push(&request("after restart"), 3_000, false), 3);
        assert!(restored.get(2).is_some());
    }

    #[test]
    fn history_parse_rejects_corrupt_json_and_foreign_versions() {
        assert!(parse_history("not json").is_none());
        assert!(parse_history("{\"version\":2,\"next_id\":1,\"notifications\":[]}").is_none());
        // A missing version is not implicitly v1.
        assert!(parse_history("{\"next_id\":1,\"notifications\":[]}").is_none());
        // A truncated record is corrupt, not a record with defaults.
        assert!(
            parse_history("{\"version\":1,\"next_id\":1,\"notifications\":[{\"id\":1}]}").is_none()
        );
    }

    #[test]
    fn an_empty_history_round_trips() {
        let text = serialize_history(&Vec::<NotificationRecord>::new(), 7);
        let mut center = parse_history(&text).expect("an empty v1 file parses");
        assert!(center.is_empty());
        assert_eq!(center.push(&request("fresh"), 0, false), 8);
    }

    #[test]
    fn history_parse_keeps_only_the_newest_records() {
        let records: Vec<serde_json::Value> = (1..=(MAX_HISTORY + 5) as u32)
            .map(|id| {
                serde_json::json!({
                    "id": id,
                    "app": "app",
                    "summary": format!("n{id}"),
                    "body": "",
                    "urgency": 1,
                    "posted_unix_ms": u64::from(id),
                    "suppressed": false,
                    "actions": [],
                })
            })
            .collect();
        let text = serde_json::json!({
            "version": HISTORY_FILE_VERSION,
            "next_id": 900,
            "notifications": records,
        })
        .to_string();

        let center = parse_history(&text).expect("well-formed file");
        assert_eq!(center.len(), MAX_HISTORY);
        assert_eq!(
            center.recent().next().expect("newest").id,
            (MAX_HISTORY + 5) as u32
        );
        assert_eq!(center.recent().last().expect("oldest kept").id, 6);
        assert_eq!(center.next_id, 900);
    }

    #[test]
    fn history_serialize_keeps_only_the_newest_records() {
        let many: Vec<NotificationRecord> = (1..=(MAX_HISTORY + 5) as u32)
            .map(|id| NotificationRecord {
                id,
                app: "app".into(),
                summary: format!("n{id}"),
                body: String::new(),
                urgency: 1,
                posted_unix_ms: u64::from(id),
                suppressed: false,
                actions: Vec::new(),
            })
            .collect();

        let text = serialize_history(&many, 42);
        let document: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        let written = document["notifications"].as_array().expect("array");
        assert_eq!(written.len(), MAX_HISTORY);
        assert_eq!(written[0]["id"], 6);
        assert_eq!(document["next_id"], 42);
    }

    #[test]
    fn history_parse_rechecks_posting_time_bounds() {
        let text = serde_json::json!({
            "version": HISTORY_FILE_VERSION,
            "next_id": 3,
            "notifications": [
                {
                    "id": 0,
                    "app": "app",
                    "summary": "zero",
                    "body": "",
                    "urgency": 1,
                    "posted_unix_ms": 1,
                    "suppressed": false,
                    "actions": [],
                },
                {
                    "id": 9,
                    "app": "app",
                    "summary": format!("loud\n{}", "x".repeat(MAX_TEXT_CHARS + 20)),
                    "body": "",
                    "urgency": 9,
                    "posted_unix_ms": 2,
                    "suppressed": false,
                    "actions": [{"key": "  ", "label": "dropped"}],
                },
            ],
        })
        .to_string();

        let mut center = parse_history(&text).expect("well-formed file");
        assert_eq!(center.len(), 1, "the zero identifier is reserved");
        let record = center.get(9).expect("record kept");
        assert_eq!(record.urgency, 2);
        assert_eq!(record.summary.chars().count(), MAX_TEXT_CHARS);
        assert!(!record.summary.chars().any(char::is_control));
        assert!(record.actions.is_empty(), "a keyless action is dropped");
        // The file's counter lagged its own records; allocation must stay
        // ahead of every restored identifier.
        assert_eq!(center.push(&request("fresh"), 0, false), 10);
    }

    #[test]
    fn history_load_rejects_oversized_files_and_special_sources() {
        use std::os::unix::ffi::OsStrExt as _;

        let root = history_temp_root("load");

        let oversized = root.join("oversized");
        fs::File::create(&oversized)
            .unwrap()
            .set_len(MAX_HISTORY_BYTES + 1)
            .unwrap();
        assert!(NotificationCenter::load_from_path(&oversized).is_empty());

        let fifo = root.join("fifo");
        let fifo_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);
        // Opening a FIFO for a normal blocking read would hang here until a
        // writer appeared. The loader must reject it immediately instead.
        assert!(NotificationCenter::load_from_path(&fifo).is_empty());

        // A missing file is the normal first-run case: just an empty history.
        assert!(NotificationCenter::load_from_path(&root.join("absent")).is_empty());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_damaged_history_file_loads_as_empty() {
        let root = history_temp_root("damaged");
        let path = root.join(HISTORY_FILE);
        fs::write(
            &path,
            "{\"version\":1,\"next_id\":1,\"notifications\":[{\"id\":",
        )
        .unwrap();
        assert!(NotificationCenter::load_from_path(&path).is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn history_save_replaces_a_symlink_without_touching_its_target() {
        let root = history_temp_root("write");
        let victim = root.join("victim");
        fs::write(&victim, "unchanged").unwrap();
        let path = root.join(HISTORY_FILE);
        std::os::unix::fs::symlink(&victim, &path).unwrap();

        let mut center = NotificationCenter::new();
        center.push(&request("kept across restarts"), 1_000, false);
        atomic_write_history(
            &path,
            serialize_history(&center.records, center.next_id).as_bytes(),
        )
        .unwrap();

        assert_eq!(fs::read_to_string(&victim).unwrap(), "unchanged");
        let metadata = fs::symlink_metadata(&path).unwrap();
        assert!(metadata.is_file());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        let restored = NotificationCenter::load_from_path(&path);
        assert_eq!(restored.records, center.records);
        assert_eq!(restored.next_id, center.next_id);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn saves_are_coalesced_off_the_posting_path_and_flushed_at_the_end() {
        let root = history_temp_root("writer");
        let path = root.join(HISTORY_FILE);
        let mut center = NotificationCenter::load_from_path(&path);
        for index in 0..10 {
            center.push(&request(&format!("n{index}")), index, false);
            center.save();
        }
        // The posting path only copied the records. The thread writes at
        // most once per window, and finishing lands the newest snapshot
        // before returning.
        let written = center
            .writer
            .take()
            .expect("the first save starts the writer")
            .finish();
        assert!(
            (1..10).contains(&written),
            "ten saves inside one window wrote {written} times"
        );
        let restored = NotificationCenter::load_from_path(&path);
        assert_eq!(restored.records, center.records);
        assert_eq!(restored.next_id, center.next_id);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_in_memory_center_persists_nothing() {
        let mut center = NotificationCenter::new();
        center.push(&request("unsaved"), 0, false);
        center.save();
        assert!(center.writer.is_none());
        assert!(center.path.is_none());
    }

    #[test]
    fn an_over_long_key_is_dropped_rather_than_truncated() {
        let long = "k".repeat(MAX_ACTION_KEY_CHARS + 1);
        let exact = "k".repeat(MAX_ACTION_KEY_CHARS);
        let kept = sanitize_actions(&[action(&long, "Too long"), action(&exact, "Fits")]);
        assert_eq!(kept, [action(&exact, "Fits")]);
        // The flat decoder applies the same rule, so the two count the same
        // entries against the cap.
        let parsed = parse_action_args(&serde_json::json!({
            "actions": [long, "Too long", exact, "Fits"]
        }));
        assert_eq!(parsed, [action(&exact, "Fits")]);
    }

    #[test]
    fn history_written_by_this_version_always_loads() {
        // The widest record the sanitizers let through: four-byte characters
        // wherever text is allowed, and keys of a control character, which
        // JSON escapes to six bytes apiece.
        let text = "\u{1F600}".repeat(MAX_TEXT_CHARS);
        let label = "\u{1F600}".repeat(MAX_LABEL_CHARS);
        let key = "\u{1}".repeat(MAX_ACTION_KEY_CHARS);
        let mut center = NotificationCenter::new();
        center.next_id = u32::MAX - MAX_HISTORY as u32 - 1;
        for _ in 0..MAX_HISTORY {
            center.push(
                &NotificationRequest {
                    app: text.clone(),
                    summary: text.clone(),
                    body: text.clone(),
                    urgency: 2,
                    replaces_id: 0,
                    actions: (0..MAX_ACTIONS).map(|_| action(&key, &label)).collect(),
                },
                u64::MAX,
                true,
            );
        }
        let record = center.recent().next().expect("records present");
        assert_eq!(record.actions.len(), MAX_ACTIONS);
        assert_eq!(record.actions[0].key, key, "the widest key must survive");
        let record_bytes = serde_json::to_string(record).expect("serializable").len() as u64;
        assert!(
            record_bytes <= MAX_HISTORY_RECORD_BYTES,
            "a record can reach {record_bytes} bytes, over the {MAX_HISTORY_RECORD_BYTES} budget"
        );

        let serialized = serialize_history(&center.records, center.next_id);
        assert!(
            serialized.len() as u64 <= MAX_HISTORY_BYTES,
            "a full history can reach {} bytes, over the {MAX_HISTORY_BYTES} limit",
            serialized.len()
        );
        let restored = parse_history(&serialized).expect("a v1 file parses");
        assert_eq!(restored.records, center.records);
    }

    #[test]
    fn eviction_reports_the_identifiers_that_still_owe_a_close() {
        let mut center = NotificationCenter::new();
        let first = center.push(&request("first"), 0, false);
        for index in 0..MAX_HISTORY {
            center.push(&request(&format!("n{index}")), index as u64, false);
        }
        assert_eq!(center.take_evicted(), vec![first]);
        assert!(center.take_evicted().is_empty(), "reported once");
        assert!(center.get(first).is_none());
    }

    #[test]
    fn the_posted_event_carries_the_record_not_the_request() {
        let mut center = NotificationCenter::new();
        let mut huge = request("big");
        huge.body = "x".repeat(200 * 1024);
        let id = center.push(&huge, 1_000, true);
        let payload = posted_event_payload(center.get(id).expect("record"));
        assert_eq!(payload["id"], id);
        assert_eq!(
            payload["body"].as_str().expect("body").chars().count(),
            MAX_TEXT_CHARS
        );
        assert_eq!(payload["suppressed"], true);
    }

    #[test]
    fn a_reload_that_did_not_touch_dnd_keeps_the_runtime_toggle() {
        assert!(do_not_disturb_after_config_apply(true, Some(false), false));
        assert!(!do_not_disturb_after_config_apply(false, Some(true), true));
    }

    #[test]
    fn a_configuration_change_to_dnd_wins_over_the_toggle() {
        assert!(do_not_disturb_after_config_apply(false, Some(false), true));
        assert!(!do_not_disturb_after_config_apply(true, Some(true), false));
        // No baseline to compare against: the configuration is adopted.
        assert!(!do_not_disturb_after_config_apply(true, None, false));
    }

    #[test]
    fn reconciling_remembers_the_configuration_it_saw() {
        let mut center = NotificationCenter::new();
        // First sight: the configuration (off) is adopted.
        assert!(!center.reconcile_do_not_disturb(false, false));
        // The user toggles on; a reload that left the setting alone keeps it.
        assert!(center.reconcile_do_not_disturb(true, false));
        // `set_config` turns it on in the configuration: adopted.
        assert!(center.reconcile_do_not_disturb(true, true));
        // The user toggles off again; the next unchanged reload keeps that.
        assert!(!center.reconcile_do_not_disturb(false, true));
    }

    #[test]
    fn system_toasts_respect_dnd_except_critical_ones() {
        assert!(system_toast_allowed(false, 1));
        assert!(!system_toast_allowed(true, 0));
        assert!(!system_toast_allowed(true, 1));
        assert!(system_toast_allowed(true, 2));
    }

    #[test]
    fn the_documented_close_reasons_are_the_codes_senders_receive() {
        // `docs/notifications.md`'s table is the contract a sender reads: the
        // number in a row leaves the process unchanged — the bridge forwards
        // whatever `notification/closed` carries straight into
        // `NotificationClosed`'s reason argument. The table is parsed and
        // driven rather than restated, so this cannot be satisfied by a
        // literal of its own.
        const DOC: &str = include_str!("../../../docs/notifications.md");
        let table = DOC
            .split_once("| Reason | What closed the record |")
            .expect("docs/notifications.md carries the close-reason table")
            .1
            .split_once("\n\n")
            .expect("the table ends at a blank line")
            .0;
        let documented: Vec<(u32, String)> = table
            .lines()
            .filter_map(|line| line.trim().strip_prefix("| `"))
            .filter_map(|row| row.split_once('`'))
            .filter_map(|(code, rest)| {
                let code = code.parse::<u32>().ok()?;
                let name = rest.trim_start().split_whitespace().next()?;
                Some((code, name.to_string()))
            })
            .collect();
        assert_eq!(documented.len(), 4, "one row per reason: {documented:?}");
        for (code, name) in &documented {
            let reason = match name.as_str() {
                "expired" => CloseReason::Expired,
                "dismissed" => CloseReason::Dismissed,
                "requested" => CloseReason::Requested,
                "undefined" => CloseReason::Undefined,
                other => panic!("the table names a close reason the code lacks: {other}"),
            };
            assert_eq!(reason.code(), *code, "documented reason `{name}`");
        }
        // The cap is the backstop that bounds a `notify-send --wait`, so the
        // number the table quotes has to be the live one.
        assert!(
            table.contains(&format!("{MAX_HISTORY}-record cap")),
            "the eviction row must name the live history cap"
        );

        // The action bullet names the key cap; moving the constant without
        // the prose would tell senders a rule jwm does not apply.
        let bullet = DOC
            .split_once("**An action with no key is dropped**")
            .expect("docs/notifications.md carries the action sanitation bullet")
            .1
            .split_once("\n- ")
            .expect("the bullet ends where the next one starts")
            .0;
        assert!(
            bullet.contains(&format!("longer than {MAX_ACTION_KEY_CHARS} characters")),
            "the action bullet must name the live key cap"
        );
    }
}
