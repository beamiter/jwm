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
//! Everything here is pure: no backend, no clock of its own (callers pass the
//! timestamp), so it is exercised directly by unit tests.

use std::collections::VecDeque;

/// Records kept before the oldest is evicted. Deep enough to cover a work
/// session's backlog, bounded so a chatty application cannot grow the
/// compositor's heap without limit.
pub const MAX_HISTORY: usize = 64;

/// Longest summary/body kept; the panel does not wrap.
const MAX_TEXT_CHARS: usize = 96;

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
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Key of the sender's default action, when it offered one. Activating
    /// the row in the panel invokes it.
    pub default_action: Option<String>,
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
    pub default_action: Option<String>,
}

/// Bounded, ordered notification history. Oldest record first.
#[derive(Debug, Default)]
pub struct NotificationCenter {
    records: VecDeque<NotificationRecord>,
    next_id: u32,
}

fn sanitize(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.chars().count() <= MAX_TEXT_CHARS {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(MAX_TEXT_CHARS - 1).collect();
    out.push('\u{2026}');
    out
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
            existing.default_action = request.default_action.clone();
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
            default_action: request.default_action.clone(),
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
    format!("{icon}  {app}{headline}{detail}{muted}   {age}")
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

        if !suppressed {
            let title = if request.summary.trim().is_empty() {
                request.app.clone()
            } else {
                request.summary.clone()
            };
            backend.compositor_push_toast(crate::backend::api::ToastNotification {
                title,
                body: request.body.clone(),
                urgency: request.urgency.min(2),
                timeout_ms,
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
        id
    }

    /// Drop one notification from the history and tell subscribers why, so the
    /// freedesktop bridge can emit `NotificationClosed` with the same reason.
    pub(crate) fn close_notification(&mut self, id: u32, reason: CloseReason) -> bool {
        if !self.features.notifications.close(id) {
            return false;
        }
        self.broadcast_ipc_event(
            "notification/closed",
            serde_json::json!({ "id": id, "reason": reason.code() }),
        );
        self.features.system_ui.remove_notification(id);
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
        for id in ids {
            self.broadcast_ipc_event(
                "notification/closed",
                serde_json::json!({ "id": id, "reason": CloseReason::Undefined.code() }),
            );
        }
        self.features.system_ui.clear_notifications();
        count
    }

    /// Report a row activation so the sending application can run its default
    /// action. The notification is closed the way the specification expects
    /// once its action was invoked.
    pub(crate) fn invoke_notification_action(&mut self, id: u32, action: &str) {
        self.broadcast_ipc_event(
            "notification/action",
            serde_json::json!({ "id": id, "action": action }),
        );
        self.close_notification(id, CloseReason::Dismissed);
    }

    /// Rebuild an open notification center against the live history.
    fn refresh_open_notification_center(&mut self) {
        if self.features.system_ui.is_notification_center() {
            self.features.system_ui = crate::jwm::features::SystemUiState::notification_center(
                &self.features.notifications,
                now_unix_ms(),
            );
        }
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
                    "default_action": record.default_action,
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
            default_action: None,
        }
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
}
