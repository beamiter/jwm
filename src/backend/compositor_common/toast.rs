//! Backend-neutral toast-notification stack.
//!
//! Both compositors render toasts as styled cards in the top-right corner;
//! everything that is not GL — capacity eviction, timeout expiry, the
//! fade-in/fade-out opacity envelope, and content sanitation — lives here so
//! the two backends cannot drift.

use crate::backend::api::ToastNotification;
use std::time::{Duration, Instant};

/// Visible cards are capped; older toasts are evicted first.
pub(crate) const MAX_TOASTS: usize = 4;
/// Seconds a card takes to fade in after being pushed.
pub(crate) const TOAST_FADE_IN: f32 = 0.18;
/// Seconds of fade-out before the timeout expires.
pub(crate) const TOAST_FADE_OUT: f32 = 0.30;
/// Longest line kept after sanitation; the renderer does not wrap.
const MAX_LINE_CHARS: usize = 80;
/// Body lines kept after sanitation.
const MAX_BODY_LINES: usize = 3;

const DEFAULT_TIMEOUT: Duration = Duration::from_millis(4000);
const MIN_TIMEOUT: Duration = Duration::from_millis(800);
const MAX_TIMEOUT: Duration = Duration::from_millis(30_000);

#[derive(Debug)]
pub(crate) struct ActiveToast {
    pub(crate) notification: ToastNotification,
    pub(crate) id: u64,
    pub(crate) created: Instant,
    pub(crate) timeout: Duration,
}

impl ActiveToast {
    /// Opacity envelope at `now`: linear fade in, hold, linear fade out.
    pub(crate) fn alpha(&self, now: Instant) -> f32 {
        let age = now.saturating_duration_since(self.created).as_secs_f32();
        let timeout = self.timeout.as_secs_f32();
        let fade_in = (age / TOAST_FADE_IN).clamp(0.0, 1.0);
        let fade_out = ((timeout - age) / TOAST_FADE_OUT).clamp(0.0, 1.0);
        fade_in.min(fade_out)
    }

    fn expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.created) >= self.timeout
    }
}

fn sanitize_line(line: &str) -> String {
    let cleaned: String = line
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    let trimmed = cleaned.trim_end();
    if trimmed.chars().count() <= MAX_LINE_CHARS {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(MAX_LINE_CHARS - 1).collect();
    out.push('\u{2026}');
    out
}

/// Clamp text to renderer-safe shape: control characters stripped, lines
/// truncated with an ellipsis, the body capped to a few lines.
fn sanitize_notification(notification: &mut ToastNotification) {
    notification.title = sanitize_line(notification.title.lines().next().unwrap_or(""));
    notification.body = notification
        .body
        .lines()
        .take(MAX_BODY_LINES)
        .map(sanitize_line)
        .collect::<Vec<_>>()
        .join("\n");
    notification.urgency = notification.urgency.min(2);
}

#[derive(Debug, Default)]
pub(crate) struct ToastStack {
    toasts: Vec<ActiveToast>,
    next_id: u64,
}

impl ToastStack {
    /// Append a toast, evicting expired cards and then the oldest cards
    /// beyond the visible cap. Returns ids whose resources can be freed.
    pub(crate) fn push(&mut self, mut notification: ToastNotification, now: Instant) -> Vec<u64> {
        sanitize_notification(&mut notification);
        let timeout = if notification.timeout_ms == 0 {
            DEFAULT_TIMEOUT
        } else {
            Duration::from_millis(u64::from(notification.timeout_ms))
                .clamp(MIN_TIMEOUT, MAX_TIMEOUT)
        };
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.toasts.push(ActiveToast {
            notification,
            id,
            created: now,
            timeout,
        });

        let mut removed = self.prune(now);
        while self.toasts.len() > MAX_TOASTS {
            removed.push(self.toasts.remove(0).id);
        }
        removed
    }

    /// Drop expired toasts, returning their ids for resource cleanup.
    pub(crate) fn prune(&mut self, now: Instant) -> Vec<u64> {
        let mut removed = Vec::new();
        self.toasts.retain(|toast| {
            if toast.expired(now) {
                removed.push(toast.id);
                false
            } else {
                true
            }
        });
        removed
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.toasts.is_empty()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &ActiveToast> {
        self.toasts.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toast(title: &str, timeout_ms: u32) -> ToastNotification {
        ToastNotification {
            title: title.into(),
            body: String::new(),
            urgency: 1,
            timeout_ms,
        }
    }

    #[test]
    fn capacity_evicts_oldest_and_reports_freed_ids() {
        let now = Instant::now();
        let mut stack = ToastStack::default();
        for i in 0..MAX_TOASTS {
            assert!(stack.push(toast(&format!("t{i}"), 0), now).is_empty());
        }
        let removed = stack.push(toast("extra", 0), now);
        assert_eq!(removed, vec![0]);
        assert_eq!(stack.iter().count(), MAX_TOASTS);
        assert_eq!(stack.iter().next().unwrap().notification.title, "t1");
    }

    #[test]
    fn expiry_prunes_and_alpha_envelope_fades() {
        let now = Instant::now();
        let mut stack = ToastStack::default();
        stack.push(toast("short", 1000), now);
        let active = stack.iter().next().unwrap();
        assert_eq!(active.alpha(now), 0.0);
        assert_eq!(active.alpha(now + Duration::from_millis(500)), 1.0);
        assert!(active.alpha(now + Duration::from_millis(950)) < 0.2);
        assert!(stack.prune(now + Duration::from_millis(999)).is_empty());
        assert_eq!(stack.prune(now + Duration::from_millis(1000)), vec![0]);
        assert!(stack.is_empty());
    }

    #[test]
    fn sanitation_bounds_lines_and_length() {
        let now = Instant::now();
        let mut stack = ToastStack::default();
        stack.push(
            ToastNotification {
                title: format!("a\tb\n second line ignored"),
                body: (0..6)
                    .map(|i| "x".repeat(120 + i))
                    .collect::<Vec<_>>()
                    .join("\n"),
                urgency: 9,
                timeout_ms: 100,
            },
            now,
        );
        let active = stack.iter().next().unwrap();
        assert_eq!(active.notification.title, "a b");
        let body_lines: Vec<&str> = active.notification.body.lines().collect();
        assert_eq!(body_lines.len(), 3);
        assert!(body_lines.iter().all(|l| l.chars().count() == 80));
        assert!(body_lines.iter().all(|l| l.ends_with('\u{2026}')));
        assert_eq!(active.notification.urgency, 2);
        assert_eq!(active.timeout, Duration::from_millis(800));
    }
}
