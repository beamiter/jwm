//! Backend-neutral toast-notification stack.
//!
//! Both compositors render toasts as styled cards in the top-right corner;
//! everything that is not GL — capacity eviction, timeout expiry, the
//! fade-in/fade-out opacity envelope, and content sanitation — lives here so
//! the two backends cannot drift.

use crate::backend::api::{NotificationAction, ToastClick, ToastNotification};
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

/// Action buttons a card shows at most: one row of chips must stay readable.
pub(crate) const MAX_TOAST_ACTIONS: usize = 3;
/// Longest button label kept after sanitation.
const MAX_ACTION_LABEL_CHARS: usize = 20;
/// Widest rasterized button label. Three chips at this width plus their
/// padding and gaps still fit the card's [`MAX_TEXT_WIDTH_PX`] ceiling.
pub(crate) const MAX_ACTION_LABEL_WIDTH_PX: u32 = 120;
/// Chip height in the action row.
pub(crate) const ACTION_BUTTON_H: f32 = 24.0;
/// Horizontal padding inside a chip, per side.
pub(crate) const ACTION_BUTTON_PAD_X: f32 = 10.0;
/// Space between two chips.
pub(crate) const ACTION_BUTTON_GAP: f32 = 8.0;
/// Gap between the text block and the action row.
pub(crate) const ACTION_ROW_TOP_GAP: f32 = 10.0;
/// Extra card height when an action row is present.
pub(crate) const ACTIONS_ROW_EXTRA_H: f32 = ACTION_ROW_TOP_GAP + ACTION_BUTTON_H;

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
    sanitize_segment(line, MAX_LINE_CHARS)
}

/// Clamp one text segment: control characters become spaces, trailing
/// whitespace is dropped, and an over-long run ends in an ellipsis.
fn sanitize_segment(text: &str, max_chars: usize) -> String {
    let cleaned: String = text
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    let trimmed = cleaned.trim_end();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(max_chars - 1).collect();
    out.push('\u{2026}');
    out
}

/// Trim a toast's action list to the chip row: a button with no key cannot be
/// invoked and is dropped before the cap is counted, a blank label falls back
/// to the key, and labels are cleaned to one short line. The key itself is
/// kept exact — it goes back out over `ActionInvoked` unchanged.
fn sanitize_actions(actions: &[NotificationAction]) -> Vec<NotificationAction> {
    actions
        .iter()
        .filter_map(|action| {
            let key = action.key.trim();
            (!key.is_empty()).then_some((key, action))
        })
        .take(MAX_TOAST_ACTIONS)
        .map(|(key, action)| {
            let label = sanitize_segment(
                action.label.lines().next().unwrap_or(""),
                MAX_ACTION_LABEL_CHARS,
            );
            let label = if label.is_empty() {
                sanitize_segment(key, MAX_ACTION_LABEL_CHARS)
            } else {
                label
            };
            NotificationAction {
                key: key.to_string(),
                label,
            }
        })
        .collect()
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
    notification.actions = sanitize_actions(&notification.actions);
}

/// One toast card's hit geometry from the last drawn frame.
///
/// Rebuilt every frame by the renderers so hover and click testing never see
/// stale geometry; shared here so the two backends cannot drift.
#[derive(Clone, Debug, Default)]
pub(crate) struct ToastRects {
    pub(crate) id: u64,
    /// Card body `[x, y, w, h]`.
    pub(crate) card: [f32; 4],
    /// Action buttons in action order, absolute coordinates like the card.
    pub(crate) buttons: Vec<[f32; 4]>,
}

/// Which part of a card a point lands on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToastHit {
    Card,
    Button(usize),
}

fn point_in(rect: &[f32; 4], x: f32, y: f32) -> bool {
    x >= rect[0] && x <= rect[0] + rect[2] && y >= rect[1] && y <= rect[1] + rect[3]
}

/// Hit-test one card's recorded geometry. Buttons sit inside the card and
/// are checked first, so a click on a chip never falls through to the body.
pub(crate) fn hit_test(rects: &ToastRects, x: f32, y: f32) -> Option<ToastHit> {
    for (index, button) in rects.buttons.iter().enumerate() {
        if point_in(button, x, y) {
            return Some(ToastHit::Button(index));
        }
    }
    point_in(&rects.card, x, y).then_some(ToastHit::Card)
}

/// Total width of the action row for `label_widths` measured chip texts.
/// Used to widen the card when the buttons are its widest content.
pub(crate) fn action_row_width(label_widths: &[f32]) -> f32 {
    if label_widths.is_empty() {
        return 0.0;
    }
    label_widths
        .iter()
        .map(|w| w + 2.0 * ACTION_BUTTON_PAD_X)
        .sum::<f32>()
        + ACTION_BUTTON_GAP * (label_widths.len() - 1) as f32
}

/// Chip rects for the action row: one chip per measured label width,
/// left-aligned at `x` on the row at `y`. Index order matches the toast's
/// action order, which is what click dispatch reports back.
pub(crate) fn action_row_layout(label_widths: &[f32], x: f32, y: f32) -> Vec<[f32; 4]> {
    let mut rects = Vec::with_capacity(label_widths.len());
    let mut chip_x = x;
    for width in label_widths {
        let chip_w = width + 2.0 * ACTION_BUTTON_PAD_X;
        rects.push([chip_x, y, chip_w, ACTION_BUTTON_H]);
        chip_x += chip_w + ACTION_BUTTON_GAP;
    }
    rects
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

    pub(crate) fn get(&self, id: u64) -> Option<&ActiveToast> {
        self.toasts.iter().find(|toast| toast.id == id)
    }

    /// Resolve a left-click at `(x, y)` against the geometry recorded for the
    /// last drawn frame. Any hit dismisses the card (it fades out on the
    /// dismiss clock); a button hit additionally reports the action's key and
    /// the notification record it belongs to, so the WM can invoke it. A
    /// button on a standalone toast — no record — degrades to a plain
    /// dismissal, and so does any click on a card already fading out: the
    /// click is swallowed but the action is never invoked twice.
    pub(crate) fn click(
        &mut self,
        rects: &[ToastRects],
        x: f32,
        y: f32,
        now: Instant,
    ) -> ToastClick {
        let Some((id, hit)) = rects
            .iter()
            .find_map(|rects| hit_test(rects, x, y).map(|hit| (rects.id, hit)))
        else {
            return ToastClick::Miss;
        };
        if !self.dismiss(id, now) {
            return ToastClick::Dismissed;
        }
        let action = match hit {
            ToastHit::Card => None,
            ToastHit::Button(index) => self.get(id).and_then(|toast| {
                let notification = &toast.notification;
                if notification.notification_id == 0 {
                    return None;
                }
                notification
                    .actions
                    .get(index)
                    .map(|action| (notification.notification_id, action.key.clone()))
            }),
        };
        match action {
            Some((notification_id, action_key)) => ToastClick::Action {
                notification_id,
                action_key,
            },
            None => ToastClick::Dismissed,
        }
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
            ..Default::default()
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
                ..Default::default()
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

    #[test]
    fn sanitation_bounds_actions() {
        let now = Instant::now();
        let mut stack = ToastStack::default();
        let action = |key: &str, label: &str| NotificationAction {
            key: key.into(),
            label: label.into(),
        };
        stack.push(
            ToastNotification {
                title: "actions".into(),
                actions: vec![
                    // Dropped: an empty key cannot be invoked.
                    action("  ", "no key"),
                    action("reply", "Re\tply\non two lines"),
                    // Blank label falls back to the key.
                    action("open", "  "),
                    action("later", &"x".repeat(40)),
                    // Beyond the row cap.
                    action("extra", "Extra"),
                ],
                ..Default::default()
            },
            now,
        );
        let actions = &stack.iter().next().unwrap().notification.actions;
        assert_eq!(actions.len(), MAX_TOAST_ACTIONS);
        assert_eq!(actions[0].key, "reply");
        assert_eq!(actions[0].label, "Re ply");
        assert_eq!(actions[1].key, "open");
        assert_eq!(actions[1].label, "open");
        assert_eq!(actions[2].key, "later");
        assert_eq!(actions[2].label.chars().count(), MAX_ACTION_LABEL_CHARS);
        assert!(actions[2].label.ends_with('\u{2026}'));
    }

    #[test]
    fn hit_test_prefers_buttons_then_card_then_miss() {
        let rects = ToastRects {
            id: 7,
            card: [100.0, 50.0, 300.0, 120.0],
            buttons: vec![[130.0, 130.0, 80.0, 24.0], [218.0, 130.0, 80.0, 24.0]],
        };
        assert_eq!(hit_test(&rects, 150.0, 140.0), Some(ToastHit::Button(0)));
        assert_eq!(hit_test(&rects, 230.0, 140.0), Some(ToastHit::Button(1)));
        // Card body outside the chips, still inside the card.
        assert_eq!(hit_test(&rects, 150.0, 60.0), Some(ToastHit::Card));
        // The gap between two chips is card body.
        assert_eq!(hit_test(&rects, 214.0, 140.0), Some(ToastHit::Card));
        assert_eq!(hit_test(&rects, 50.0, 60.0), None);
        assert_eq!(hit_test(&rects, 150.0, 200.0), None);
    }

    #[test]
    fn action_row_layout_sizes_and_spacing() {
        let widths = [40.0, 60.0, 30.0];
        let rects = action_row_layout(&widths, 30.0, 100.0);
        assert_eq!(rects.len(), 3);
        let chip_w = |i: usize| widths[i] + 2.0 * ACTION_BUTTON_PAD_X;
        assert_eq!(rects[0], [30.0, 100.0, chip_w(0), ACTION_BUTTON_H]);
        assert_eq!(
            rects[1],
            [
                30.0 + chip_w(0) + ACTION_BUTTON_GAP,
                100.0,
                chip_w(1),
                ACTION_BUTTON_H
            ]
        );
        // The row's total width matches the helper the card sizing uses.
        let right = rects[2][0] + rects[2][2];
        assert_eq!(right - 30.0, action_row_width(&widths));
        assert_eq!(action_row_width(&[]), 0.0);
    }

    #[test]
    fn click_dispatches_action_dismiss_and_miss() {
        let now = Instant::now();
        let mut stack = ToastStack::default();
        let action = |key: &str, label: &str| NotificationAction {
            key: key.into(),
            label: label.into(),
        };
        stack.push(
            ToastNotification {
                title: "with actions".into(),
                timeout_ms: 4000,
                notification_id: 42,
                actions: vec![action("reply", "Reply"), action("open", "Open")],
                ..Default::default()
            },
            now,
        );
        stack.push(
            ToastNotification {
                title: "standalone".into(),
                timeout_ms: 4000,
                // No record: buttons on a standalone toast only dismiss.
                actions: vec![action("noop", "No-op")],
                ..Default::default()
            },
            now,
        );
        let rects = vec![
            ToastRects {
                id: 0,
                card: [100.0, 0.0, 300.0, 100.0],
                buttons: vec![[130.0, 66.0, 80.0, 24.0], [218.0, 66.0, 80.0, 24.0]],
            },
            ToastRects {
                id: 1,
                card: [100.0, 112.0, 300.0, 100.0],
                buttons: vec![[130.0, 178.0, 80.0, 24.0]],
            },
        ];

        // A miss touches nothing.
        assert_eq!(stack.click(&rects, 10.0, 10.0, now), ToastClick::Miss);
        assert_eq!(stack.iter().count(), 2);

        // A button hit dismisses the card and reports the record and key.
        let click = now + Duration::from_millis(500);
        assert_eq!(
            stack.click(&rects, 230.0, 70.0, click),
            ToastClick::Action {
                notification_id: 42,
                action_key: "open".into(),
            }
        );
        // While the card fades out its rects are still on screen: a second
        // click on the same chip is swallowed but never invokes twice.
        assert_eq!(
            stack.click(&rects, 230.0, 70.0, click + Duration::from_millis(60)),
            ToastClick::Dismissed
        );
        assert_eq!(stack.prune(click + Duration::from_millis(120)), vec![0]);

        // A button on a standalone toast has no record to invoke.
        let click = click + Duration::from_millis(200);
        assert_eq!(
            stack.click(&rects, 150.0, 180.0, click),
            ToastClick::Dismissed
        );
        assert_eq!(stack.prune(click + Duration::from_millis(120)), vec![1]);
        assert!(stack.is_empty());
    }

    #[test]
    fn click_on_card_body_dismisses_without_action() {
        let now = Instant::now();
        let mut stack = ToastStack::default();
        stack.push(
            ToastNotification {
                title: "body click".into(),
                timeout_ms: 4000,
                notification_id: 9,
                actions: vec![NotificationAction {
                    key: "reply".into(),
                    label: "Reply".into(),
                }],
                ..Default::default()
            },
            now,
        );
        let rects = vec![ToastRects {
            id: 0,
            card: [100.0, 0.0, 300.0, 100.0],
            buttons: vec![[130.0, 66.0, 80.0, 24.0]],
        }];
        assert_eq!(stack.click(&rects, 150.0, 20.0, now), ToastClick::Dismissed);
        // A repeat hit while the card fades out is still swallowed and
        // reports no action.
        assert_eq!(
            stack.click(&rects, 150.0, 20.0, now + Duration::from_millis(60)),
            ToastClick::Dismissed
        );
        assert_eq!(stack.prune(now + Duration::from_millis(120)), vec![0]);
    }
}
