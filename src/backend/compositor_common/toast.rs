//! Backend-neutral toast-notification stack.
//!
//! Both compositors render toasts as styled cards in the top-right corner;
//! everything that is not GL — capacity eviction, timeout expiry, the
//! fade-in/fade-out opacity envelope, and content sanitation — lives here so
//! the two backends cannot drift.

use crate::backend::api::ToastNotification;
use crate::backend::compositor_common::dynamic_island::IslandMotion;
use std::time::{Duration, Instant};

/// Visible cards are capped; older toasts are evicted first.
pub(crate) const MAX_TOASTS: usize = 4;
/// Seconds a card takes to fade in after being pushed.
pub(crate) const TOAST_FADE_IN: f32 = 0.18;
/// Seconds of fade-out before the timeout expires.
pub(crate) const TOAST_FADE_OUT: f32 = 0.30;
/// Seconds a clicked-away card takes to fade out from its current opacity.
pub(crate) const TOAST_DISMISS_FADE: f32 = 0.12;
/// Longest line kept after sanitation; the renderer does not wrap.
const MAX_LINE_CHARS: usize = 80;
/// Body lines kept after sanitation.
const MAX_BODY_LINES: usize = 3;
/// Widest rasterized title/body line. The card renderer uses the same ceiling;
/// fitting before upload avoids allocating a giant texture only to draw it
/// outside a 440 px card.
pub(crate) const MAX_TEXT_WIDTH_PX: u32 = 440;

const DEFAULT_TIMEOUT: Duration = Duration::from_millis(4000);
const MIN_TIMEOUT: Duration = Duration::from_millis(800);
const MAX_TIMEOUT: Duration = Duration::from_millis(30_000);

#[derive(Debug)]
pub(crate) struct ActiveToast {
    pub(crate) notification: ToastNotification,
    pub(crate) id: u64,
    pub(crate) created: Instant,
    pub(crate) timeout: Duration,
    /// Hover pause: while `Some`, the card's age stays frozen at this instant.
    paused_at: Option<Instant>,
    /// Click-to-dismiss: the dismiss instant and the alpha the card had when
    /// clicked, so the quick fade-out starts from what was on screen.
    dismissed: Option<(Instant, f32)>,
    /// Open spring for the docked card, so each notification drops out of the
    /// bar on its own rather than the whole stack sliding as one.
    motion: IslandMotion,
}

impl ActiveToast {
    /// Opacity envelope at `now`: linear fade in, hold, linear fade out. A
    /// dismissed card ignores the envelope and fades out quickly from the
    /// opacity it had when clicked; a hovered card's age is frozen at its
    /// pause instant.
    pub(crate) fn alpha(&self, now: Instant) -> f32 {
        if let Some((dismissed_at, dismiss_alpha)) = self.dismissed {
            let elapsed = now.saturating_duration_since(dismissed_at).as_secs_f32();
            return (dismiss_alpha * (1.0 - elapsed / TOAST_DISMISS_FADE)).max(0.0);
        }
        let effective_now = self.paused_at.unwrap_or(now);
        let age = effective_now
            .saturating_duration_since(self.created)
            .as_secs_f32();
        let timeout = self.timeout.as_secs_f32();
        let fade_in = (age / TOAST_FADE_IN).clamp(0.0, 1.0);
        let fade_out = ((timeout - age) / TOAST_FADE_OUT).clamp(0.0, 1.0);
        fade_in.min(fade_out)
    }

    fn expired(&self, now: Instant) -> bool {
        // Dismiss wins over hover: a dismissed card expires on the dismiss
        // clock even while it is still hovered.
        if let Some((dismissed_at, _)) = self.dismissed {
            return now.saturating_duration_since(dismissed_at).as_secs_f32() >= TOAST_DISMISS_FADE;
        }
        // A hovered card never expires; its age is frozen at `paused_at`.
        if self.paused_at.is_some() {
            return false;
        }
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
            paused_at: None,
            dismissed: None,
            motion: IslandMotion::default(),
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

    /// Mark the hovered card (at most one at a time): its age freezes at
    /// `now` until the hover moves elsewhere or ends, and the card the hover
    /// left has the paused span credited back to `created` so its envelope
    /// resumes from the frozen point. Dismissed cards are left to fade out.
    pub(crate) fn set_hovered(&mut self, id: Option<u64>, now: Instant) {
        for toast in &mut self.toasts {
            let hovered = Some(toast.id) == id;
            match (hovered, toast.paused_at) {
                (true, None) => {
                    if toast.dismissed.is_none() && !toast.expired(now) {
                        toast.paused_at = Some(now);
                    }
                }
                (false, Some(paused_at)) => {
                    toast.created += now.saturating_duration_since(paused_at);
                    toast.paused_at = None;
                }
                _ => {}
            }
        }
    }

    /// Click-to-dismiss: the card fades out quickly from its current opacity
    /// and is then pruned like an expired one. Returns false when the id is
    /// unknown or the card is already dismissed.
    pub(crate) fn dismiss(&mut self, id: u64, now: Instant) -> bool {
        let Some(toast) = self.toasts.iter_mut().find(|toast| toast.id == id) else {
            return false;
        };
        if toast.dismissed.is_some() {
            return false;
        }
        let alpha = toast.alpha(now);
        toast.dismissed = Some((now, alpha));
        true
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.toasts.is_empty()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &ActiveToast> {
        self.toasts.iter()
    }

    /// One card's open spring, for the renderer to advance once its measured
    /// size is known.
    pub(crate) fn motion_for(&mut self, id: u64) -> Option<&mut IslandMotion> {
        self.toasts
            .iter_mut()
            .find(|toast| toast.id == id)
            .map(|toast| &mut toast.motion)
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

    #[test]
    fn dismiss_fades_out_quickly_and_prunes() {
        let now = Instant::now();
        let mut stack = ToastStack::default();
        stack.push(toast("bye", 4000), now);
        // Fully faded in by the time the click lands.
        let click = now + Duration::from_millis(500);
        assert!(stack.dismiss(0, click));
        let active = stack.iter().next().unwrap();
        assert_eq!(active.alpha(click), 1.0);
        let half = active.alpha(click + Duration::from_millis(60));
        assert!(half > 0.4 && half < 0.6);
        assert_eq!(active.alpha(click + Duration::from_millis(120)), 0.0);
        // The card stays until the dismiss fade has run, then prune reports
        // the id so the renderer frees its textures.
        assert!(stack.prune(click + Duration::from_millis(119)).is_empty());
        assert_eq!(stack.prune(click + Duration::from_millis(120)), vec![0]);
        assert!(stack.is_empty());
    }

    #[test]
    fn dismiss_unknown_or_dismissed_id_is_a_no_op() {
        let now = Instant::now();
        let mut stack = ToastStack::default();
        stack.push(toast("a", 4000), now);
        assert!(!stack.dismiss(7, now));
        assert!(stack.dismiss(0, now));
        assert!(!stack.dismiss(0, now + Duration::from_millis(10)));
    }

    #[test]
    fn hover_pause_freezes_alpha_and_expiry() {
        let now = Instant::now();
        let mut stack = ToastStack::default();
        stack.push(toast("pause", 1000), now);
        stack.set_hovered(Some(0), now + Duration::from_millis(500));
        let frozen = stack
            .iter()
            .next()
            .unwrap()
            .alpha(now + Duration::from_millis(500));
        assert_eq!(frozen, 1.0);
        // Long past the timeout the hovered card neither fades nor expires.
        let later = now + Duration::from_millis(5000);
        assert_eq!(stack.iter().next().unwrap().alpha(later), frozen);
        assert!(stack.prune(later).is_empty());
    }

    #[test]
    fn unhover_resumes_age_from_the_frozen_point() {
        let now = Instant::now();
        let mut stack = ToastStack::default();
        stack.push(toast("pause", 1000), now);
        stack.set_hovered(Some(0), now + Duration::from_millis(500));
        stack.set_hovered(None, now + Duration::from_millis(2000));
        // The 1.5 s hover is credited back: the card ages as if it had been
        // pushed 1.5 s later, so the original timeout lands 1.5 s late.
        let active = stack.iter().next().unwrap();
        assert!(active.alpha(now + Duration::from_millis(2000)) >= 0.99);
        assert!(stack.prune(now + Duration::from_millis(2499)).is_empty());
        assert_eq!(stack.prune(now + Duration::from_millis(2500)), vec![0]);
    }

    #[test]
    fn dismiss_wins_over_hover_pause() {
        let now = Instant::now();
        let mut stack = ToastStack::default();
        stack.push(toast("paused then clicked", 4000), now);
        stack.set_hovered(Some(0), now + Duration::from_millis(500));
        let click = now + Duration::from_millis(1000);
        assert!(stack.dismiss(0, click));
        // Still hovered, but the card fades out and expires on the dismiss
        // clock rather than staying frozen.
        let gone = click + Duration::from_millis(120);
        assert_eq!(stack.iter().next().unwrap().alpha(gone), 0.0);
        assert_eq!(stack.prune(gone), vec![0]);
    }

    #[test]
    fn hover_switch_replaces_the_paused_card() {
        let now = Instant::now();
        let mut stack = ToastStack::default();
        stack.push(toast("first", 2000), now);
        stack.push(toast("second", 2000), now);
        stack.set_hovered(Some(0), now + Duration::from_millis(400));
        stack.set_hovered(Some(1), now + Duration::from_millis(900));
        // The first card resumed when the hover moved and burns down its
        // remaining 1.6 s; the second is frozen and survives.
        let removed = stack.prune(now + Duration::from_millis(5000));
        assert_eq!(removed, vec![0]);
        assert_eq!(stack.iter().next().unwrap().id, 1);
    }
}
