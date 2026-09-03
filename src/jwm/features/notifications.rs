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
//! [`parse_history`] pair. Everything else here is pure: no backend, no clock
//! of its own (callers pass the timestamp), so it is exercised directly by
//! unit tests.

use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

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

/// Generous per-record allowance, used only to size the file check on load:
/// a record holds at most three [`MAX_TEXT_CHARS`]-character texts, six
/// actions, and small scalar fields, so this mostly bounds sender-controlled
/// action keys.
const MAX_HISTORY_RECORD_BYTES: u64 = 4096;

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

fn sanitize_action_iter<'a>(
    actions: impl IntoIterator<Item = &'a NotificationAction>,
) -> Vec<NotificationAction> {
    let mut kept = Vec::with_capacity(MAX_ACTIONS);
    for action in actions {
        let key = action.key.trim();
        if key.is_empty() {
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
        if trimmed_key.is_empty() {
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
            self.records.pop_front();
        }
        id
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
    #[must_use]
    pub fn load() -> Self {
        Self::load_from_path(&history_path())
    }

    fn load_from_path(path: &Path) -> Self {
        match read_history(path) {
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
        }
    }

    /// Write the history back. Failures are logged and dropped — losing a
    /// snapshot is not worth interrupting a notification over.
    pub fn save(&self) {
        let path = history_path();
        let serialized = serialize_history(&self.records, self.next_id);
        if let Err(error) = atomic_write_history(&path, serialized.as_bytes()) {
            log::debug!("notifications: {}: {error}", path.display());
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
    Some(NotificationCenter { records, next_id })
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

        self.broadcast_ipc_event(
            "notification/posted",
            serde_json::json!({
                "id": id,
                "app": request.app,
                "summary": request.summary,
                "body": request.body,
                "urgency": request.urgency.min(2),
                "suppressed": suppressed,
            }),
        );
        // A center left open while a notification arrives would otherwise show
        // a stale list.
        self.refresh_open_notification_center();
        self.refresh_open_control_center();
        id
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
        count
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
}
