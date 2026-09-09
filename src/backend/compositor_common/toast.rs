//! Backend-neutral toast-notification stack.
//!
//! Both compositors render toasts as styled cards docked under the status
//! bar — the dynamic-island slot, centred on it — with newer cards stacking
//! downward; everything that is not GL — capacity eviction, timeout expiry,
//! the fade-in/fade-out opacity envelope, content sanitation, and the
//! stacking geometry — lives here so the two backends cannot drift.

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

/// Gap between two stacked cards, and between the reserved OSD slot and the
/// first card.
pub(crate) const STACK_GAP: f32 = 12.0;

/// Offset below the dock where the first card starts. A visible OSD owns the
/// slot directly under the bar, so the stack begins below its full reserved
/// height rather than its current sprung height — otherwise every toast below
/// would jitter while the OSD opens.
pub(crate) fn stack_start(osd_visible: bool) -> f32 {
    if osd_visible {
        super::osd::OSD_CARD_HEIGHT + STACK_GAP
    } else {
        0.0
    }
}

/// Offset of the card below one whose target height is `target_h`. The target
/// — not the current sprung height — advances the cursor, so an opening card
/// already claims its full slot and the cards beneath it never shift while
/// its spring runs.
pub(crate) fn stack_next(top: f32, target_h: f32) -> f32 {
    top + target_h.max(0.0) + STACK_GAP
}

const DEFAULT_TIMEOUT: Duration = Duration::from_millis(4000);
const MIN_TIMEOUT: Duration = Duration::from_millis(800);
const MAX_TIMEOUT: Duration = Duration::from_millis(30_000);

#[derive(Debug)]
pub(crate) struct ActiveToast {
    pub(crate) notification: ToastNotification,
    pub(crate) id: u64,
    /// When the card appeared. The fade-in reads this and nothing moves it,
    /// so a hover cannot freeze a card half-transparent and leaving cannot
    /// rewind it.
    born: Instant,
    /// Start of the countdown: shifted forward by hover pauses, and reset by
    /// a replacement, which restarts the timeout for the new text.
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
    /// The size the renderer last asked the spring to travel to, remembered
    /// so [`Self::spring_animating`] can answer without the texture set that
    /// produced the measurement.
    spring_target: (f32, f32),
}

impl ActiveToast {
    /// Opacity envelope at `now`: linear fade in, hold, linear fade out. A
    /// dismissed card ignores the envelope and fades out quickly from the
    /// opacity it had when clicked; a hovered card's countdown is frozen at
    /// its pause instant, but its arrival is not — a card the pointer reaches
    /// while it is still appearing finishes appearing.
    pub(crate) fn alpha(&self, now: Instant) -> f32 {
        if let Some((dismissed_at, dismiss_alpha)) = self.dismissed {
            let elapsed = now.saturating_duration_since(dismissed_at).as_secs_f32();
            return (dismiss_alpha * (1.0 - elapsed / TOAST_DISMISS_FADE)).max(0.0);
        }
        let since_born = now.saturating_duration_since(self.born).as_secs_f32();
        let fade_in = (since_born / TOAST_FADE_IN).clamp(0.0, 1.0);
        let effective_now = self.paused_at.unwrap_or(now);
        let age = effective_now
            .saturating_duration_since(self.created)
            .as_secs_f32();
        let timeout = self.timeout.as_secs_f32();
        let fade_out = ((timeout - age) / TOAST_FADE_OUT).clamp(0.0, 1.0);
        fade_in.min(fade_out)
    }

    /// Whether the card's on-screen state is moving at `now`: inside the
    /// fade-in, the fade-out, or a dismiss fade. A settled hold and a frozen
    /// hover both read false — their next change is a scheduled boundary
    /// ([`Self::next_envelope_change_at`]), not a running curve.
    pub(crate) fn envelope_active(&self, now: Instant) -> bool {
        if let Some((dismissed_at, _)) = self.dismissed {
            return now.saturating_duration_since(dismissed_at).as_secs_f32() < TOAST_DISMISS_FADE;
        }
        // The arrival is never frozen: a card the pointer reached while it
        // was still appearing keeps finishing its fade-in.
        if now.saturating_duration_since(self.born).as_secs_f32() < TOAST_FADE_IN {
            return true;
        }
        if self.paused_at.is_some() {
            return false;
        }
        let age = now.saturating_duration_since(self.created).as_secs_f32();
        let timeout = self.timeout.as_secs_f32();
        age >= timeout - TOAST_FADE_OUT && age < timeout
    }

    /// Whether the open spring is still travelling toward the size the
    /// renderer last measured for this card.
    pub(crate) fn spring_animating(&self) -> bool {
        self.motion
            .animating(self.spring_target.0, self.spring_target.1)
    }

    /// Whether the compositor owes this card frames right now: the envelope
    /// or the open spring is moving, or the card reached its end and is owed
    /// the frame that prunes it and erases its pixels.
    pub(crate) fn needs_frames(&self, now: Instant) -> bool {
        self.envelope_active(now) || self.spring_animating() || self.expired(now)
    }

    /// The next instant the card's on-screen state changes without further
    /// input: the fade-in completing, the hold ending and the fade-out
    /// beginning, the card expiring, or a dismiss fade finishing. `None`
    /// while the countdown is frozen by a hover — the pointer leaving is
    /// what re-arms the clock, and that event makes its own frame.
    pub(crate) fn next_envelope_change_at(&self, now: Instant) -> Option<Instant> {
        if let Some((dismissed_at, _)) = self.dismissed {
            let end = dismissed_at + Duration::from_secs_f32(TOAST_DISMISS_FADE);
            return (end > now).then_some(end);
        }
        let fade_in_end = self.born + Duration::from_secs_f32(TOAST_FADE_IN);
        if self.paused_at.is_some() {
            return (fade_in_end > now).then_some(fade_in_end);
        }
        let expiry = self.created + self.timeout;
        let fade_out_start = self.created
            + self
                .timeout
                .saturating_sub(Duration::from_secs_f32(TOAST_FADE_OUT));
        [fade_in_end, fade_out_start, expiry]
            .into_iter()
            .filter(|&at| at > now)
            .min()
    }

    /// Swap in a newer notification for the same record. The countdown
    /// restarts on the new timeout (a hovered card stays frozen, at zero),
    /// the open spring keeps running, and the fade-in resumes from the
    /// opacity on screen rather than from nothing, so an update landing on a
    /// half-faded card lifts it back without a blink.
    fn replace(&mut self, notification: ToastNotification, timeout: Duration, now: Instant) {
        let alpha = self.alpha(now);
        self.notification = notification;
        self.timeout = timeout;
        self.created = now;
        if self.paused_at.is_some() {
            self.paused_at = Some(now);
        }
        // The fade-in reads `born`; move it only when the card was already
        // past its fade-in and fading out, so the curve passes through the
        // opacity on screen. A card still fading in simply continues.
        let fade_in_elapsed = Duration::from_secs_f32(alpha * TOAST_FADE_IN);
        if now.saturating_duration_since(self.born) > fade_in_elapsed {
            self.born = now.checked_sub(fade_in_elapsed).unwrap_or(self.born);
        }
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
/// truncated with an ellipsis, the body capped to a few lines. Lines that
/// are blank once cleaned are skipped before the caps are counted: a body
/// that opens with newlines — common in app-formatted text — would otherwise
/// spend its whole line budget on nothing and never show its text, and a
/// title that opens with one would render empty.
fn sanitize_notification(notification: &mut ToastNotification) {
    notification.title = notification
        .title
        .lines()
        .map(sanitize_line)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    // The sender gets the title's one-line shape and length cap: an empty
    // result means the card draws no attribution line at all.
    notification.app = notification
        .app
        .lines()
        .map(sanitize_line)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    notification.body = notification
        .body
        .lines()
        .map(sanitize_line)
        .filter(|line| !line.is_empty())
        .take(MAX_BODY_LINES)
        .collect::<Vec<_>>()
        .join("\n");
    notification.urgency = notification.urgency.min(2);
    notification.actions = sanitize_actions(&notification.actions);
}

/// Transparent rows between the sender band and the title band of a merged
/// title texture. With the rasterizer's own 2 px margins on both bands this
/// lands the sender the same 6 px above the title that the body sits below
/// it.
pub(crate) const SENDER_TITLE_GAP_PX: u32 = 2;

/// The sender line's ink: the body's label ink at a reduced alpha, so the
/// attribution reads quieter than the message in every theme without a new
/// palette tone. The card's type scale is one size with brightness as the
/// hierarchy — the title is the brightest ink, the body one step down, and
/// the sender sits at the dim end.
pub(crate) fn sender_ink(label_ink: [u8; 4]) -> [u8; 4] {
    [
        label_ink[0],
        label_ink[1],
        label_ink[2],
        (f32::from(label_ink[3]) * 0.72).round() as u8,
    ]
}

/// Stack rasterized RGBA text bands into one texture buffer, top band first,
/// with `gap_px` fully transparent rows between bands. The card bakes its
/// dim sender line and bright title into the single title texture this way,
/// so the draw and layout code needs no sender case — it reads everything
/// from the merged texture's dimensions, and a card without a sender keeps
/// the exact pixels, and therefore geometry, it has always had. Returns
/// `(pixels, width, height)`; `(vec![], 0, 0)` for no bands.
pub(crate) fn merge_text_bands(bands: &[(&[u8], u32, u32)], gap_px: u32) -> (Vec<u8>, u32, u32) {
    if bands.is_empty() {
        return (Vec::new(), 0, 0);
    }
    let width = bands.iter().map(|&(_, w, _)| w).max().unwrap_or(0);
    let height = bands
        .iter()
        .map(|&(_, _, h)| h)
        .sum::<u32>()
        .saturating_add(gap_px.saturating_mul(bands.len().saturating_sub(1) as u32));
    let Some(len) = (width as usize)
        .checked_mul(height as usize)
        .and_then(|px| px.checked_mul(4))
    else {
        return (Vec::new(), 0, 0);
    };
    if len == 0 {
        return (Vec::new(), 0, 0);
    }
    let mut pixels = vec![0u8; len];
    let mut top = 0usize;
    for (index, &(band, band_w, band_h)) in bands.iter().enumerate() {
        if index > 0 {
            top += gap_px as usize;
        }
        for row in 0..band_h as usize {
            let dst = ((top + row) * width as usize) * 4;
            let src = row * band_w as usize * 4;
            let span = band_w as usize * 4;
            if src + span <= band.len() && dst + span <= pixels.len() {
                pixels[dst..dst + span].copy_from_slice(&band[src..src + span]);
            }
        }
        top += band_h as usize;
    }
    (pixels, width, height)
}

/// One toast card's hit geometry from the last drawn frame.
///
/// Rebuilt every frame by the renderers so hover and click testing never see
/// stale geometry; shared here so the two backends cannot drift. Compared by
/// value, so a backend republishing the list to another thread can tell an
/// unchanged frame from a moved card and skip the handover.
#[derive(Clone, Debug, Default, PartialEq)]
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
    /// Show a toast. A notification already on screen — a live card carrying
    /// the same non-zero `notification_id` — is updated in place, keeping its
    /// slot, its open spring and its hover, which is how a progress
    /// notification stays one card instead of filling the stack with stale
    /// copies of itself. Anything else is appended; expired cards and then the
    /// oldest beyond the visible cap are evicted.
    ///
    /// Returns the ids whose rasterized resources are stale and must be freed:
    /// the evicted cards', and a replaced card's own — that card stays in the
    /// stack, and the renderer rasterizes its new text on the next frame
    /// because it finds no textures for it.
    pub(crate) fn push(&mut self, mut notification: ToastNotification, now: Instant) -> Vec<u64> {
        sanitize_notification(&mut notification);
        let timeout = if notification.timeout_ms == 0 {
            DEFAULT_TIMEOUT
        } else {
            Duration::from_millis(u64::from(notification.timeout_ms))
                .clamp(MIN_TIMEOUT, MAX_TIMEOUT)
        };
        let mut removed = Vec::new();
        match self.replacement_index(notification.notification_id) {
            Some(index) => {
                let existing = &mut self.toasts[index];
                existing.replace(notification, timeout, now);
                removed.push(existing.id);
            }
            None => {
                let id = self.next_id;
                self.next_id = self.next_id.wrapping_add(1);
                self.toasts.push(ActiveToast {
                    notification,
                    id,
                    born: now,
                    created: now,
                    timeout,
                    paused_at: None,
                    dismissed: None,
                    motion: IslandMotion::default(),
                    spring_target: (0.0, 0.0),
                });
            }
        }

        removed.extend(self.prune(now));
        while self.toasts.len() > MAX_TOASTS {
            removed.push(self.toasts.remove(0).id);
        }
        removed
    }

    /// The card a replacement lands on: the live card for the same record.
    /// A standalone toast (record zero) is never one, and neither is a card
    /// the user already clicked away — it finishes fading and the update
    /// gets a card of its own.
    fn replacement_index(&self, notification_id: u32) -> Option<usize> {
        if notification_id == 0 {
            return None;
        }
        self.toasts.iter().position(|toast| {
            toast.notification.notification_id == notification_id && toast.dismissed.is_none()
        })
    }

    /// Drop expired toasts, returning their ids for resource cleanup.
    ///
    /// The ids are the stack's own card keys, not notification records, and a
    /// card reaching the end of its timeout closes nothing: the record stays
    /// in the notification center, where its buttons still work, until it is
    /// dismissed, closed, or evicted. `docs/notifications.md` carries that
    /// contract and the reasoning; reporting expiry as a `NotificationClosed`
    /// would tell senders to forget a notification the center still offers.
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

    /// Whether any card needs compositor frames right now: an envelope or
    /// open spring still moving, or a card owed the frame that prunes it. A
    /// fully settled hold answers false — its next change is a scheduled
    /// boundary ([`Self::next_envelope_change_at`]), not a running curve.
    pub(crate) fn needs_frames(&self, now: Instant) -> bool {
        self.toasts.iter().any(|toast| toast.needs_frames(now))
    }

    /// The nearest instant some card's on-screen state next changes without
    /// input, so the event loop can sleep until then and still start the
    /// fade-out on time. `None` when every card is settled or hover-frozen —
    /// pointer and click events make their own frames.
    pub(crate) fn next_envelope_change_at(&self, now: Instant) -> Option<Instant> {
        self.toasts
            .iter()
            .filter_map(|toast| toast.next_envelope_change_at(now))
            .min()
    }

    /// Advance one card's open spring to `now` toward the renderer's measured
    /// target, remembering that target so [`ActiveToast::spring_animating`]
    /// can later answer without the texture set that produced it.
    pub(crate) fn advance_motion(
        &mut self,
        id: u64,
        now: Instant,
        target_w: f32,
        target_h: f32,
        motion_enabled: bool,
    ) -> (f32, f32) {
        self.toasts
            .iter_mut()
            .find(|toast| toast.id == id)
            .map_or((target_w, target_h), |toast| {
                toast.spring_target = (target_w, target_h);
                toast
                    .motion
                    .advance_with_motion(now, target_w, target_h, motion_enabled)
            })
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
    fn a_settled_hold_needs_no_frames_but_its_boundaries_do() {
        let now = Instant::now();
        let mut stack = ToastStack::default();
        stack.push(toast("hold", 4000), now);
        {
            let active = stack.iter().next().unwrap();
            // Fade-in: the envelope is moving and owes frames.
            assert!(active.envelope_active(now));
            assert!(active.needs_frames(now));
            assert!(active.envelope_active(now + Duration::from_millis(179)));
            // Settled hold: nothing moves, nothing is owed.
            let hold = now + Duration::from_millis(1000);
            assert!(!active.envelope_active(hold));
            assert!(!active.needs_frames(hold));
            // The fade-out begins 300 ms before the timeout ...
            assert!(!active.envelope_active(now + Duration::from_millis(3699)));
            let fade_out = now + Duration::from_millis(3701);
            assert!(active.envelope_active(fade_out));
            assert!(active.needs_frames(fade_out));
            // ... and at the timeout the card is owed its pruning frame.
            let expiry = now + Duration::from_millis(4001);
            assert!(!active.envelope_active(expiry));
            assert!(active.needs_frames(expiry));
        }
        // Pruned, the stack goes quiet.
        assert_eq!(stack.prune(now + Duration::from_millis(4001)), vec![0]);
        assert!(!stack.needs_frames(now + Duration::from_millis(4001)));
    }

    #[test]
    fn next_envelope_change_walks_the_boundaries() {
        let now = Instant::now();
        let mut stack = ToastStack::default();
        stack.push(toast("walk", 4000), now);
        let active = stack.iter().next().unwrap();
        // While fading in, the next change is the fade-in completing.
        assert_eq!(
            active.next_envelope_change_at(now),
            Some(now + Duration::from_secs_f32(TOAST_FADE_IN))
        );
        // In the hold it is the fade-out's start, 300 ms before the timeout.
        let fade_out_start = now
            + Duration::from_millis(4000).saturating_sub(Duration::from_secs_f32(TOAST_FADE_OUT));
        assert_eq!(
            active.next_envelope_change_at(now + Duration::from_millis(1000)),
            Some(fade_out_start)
        );
        // Once the fade-out runs, only the expiry is left.
        assert_eq!(
            active.next_envelope_change_at(fade_out_start),
            Some(now + Duration::from_millis(4000))
        );
        // Past the timeout nothing is scheduled: the card is owed a prune,
        // which `needs_frames` already reports.
        assert_eq!(
            active.next_envelope_change_at(now + Duration::from_millis(4001)),
            None
        );
    }

    #[test]
    fn a_hover_pause_reports_no_boundary_but_keeps_the_arrival() {
        let now = Instant::now();
        let mut stack = ToastStack::default();
        stack.push(toast("frozen", 4000), now);
        // Hovered mid-hold: the countdown is frozen, so there is no boundary
        // to schedule — the pointer leaving makes its own frame.
        stack.set_hovered(Some(0), now + Duration::from_millis(1000));
        let active = stack.iter().next().unwrap();
        assert_eq!(
            active.next_envelope_change_at(now + Duration::from_millis(1000)),
            None
        );
        assert!(!active.envelope_active(now + Duration::from_millis(1000)));
        assert!(!active.needs_frames(now + Duration::from_millis(5000)));

        // Leaving credits the hover back: the fade-out boundary rides the
        // shifted countdown (created moved 4 s forward).
        stack.set_hovered(None, now + Duration::from_millis(5000));
        let active = stack.iter().next().unwrap();
        let shifted_fade_out = now
            + Duration::from_millis(4000)
            + Duration::from_millis(4000).saturating_sub(Duration::from_secs_f32(TOAST_FADE_OUT));
        assert_eq!(
            active.next_envelope_change_at(now + Duration::from_millis(5000)),
            Some(shifted_fade_out)
        );

        // A hover during the fade-in freezes the countdown but not the
        // arrival: the only scheduled change is the fade-in completing.
        let mut stack = ToastStack::default();
        stack.push(toast("early", 4000), now);
        stack.set_hovered(Some(0), now + Duration::from_millis(50));
        let active = stack.iter().next().unwrap();
        assert!(active.envelope_active(now + Duration::from_millis(100)));
        assert_eq!(
            active.next_envelope_change_at(now + Duration::from_millis(50)),
            Some(now + Duration::from_secs_f32(TOAST_FADE_IN))
        );
    }

    #[test]
    fn a_dismiss_rides_its_own_clock_to_the_pruning_frame() {
        let now = Instant::now();
        let mut stack = ToastStack::default();
        stack.push(toast("bye", 4000), now);
        let click = now + Duration::from_millis(500);
        assert!(stack.dismiss(0, click));
        let active = stack.iter().next().unwrap();
        assert_eq!(
            active.next_envelope_change_at(click),
            Some(click + Duration::from_secs_f32(TOAST_DISMISS_FADE))
        );
        assert!(active.envelope_active(click + Duration::from_millis(60)));
        // The fade over, the envelope is quiet but the card is still owed
        // the frame that prunes it; nothing further is ever scheduled.
        let end = click + Duration::from_millis(121);
        assert!(!active.envelope_active(end));
        assert!(active.needs_frames(end));
        assert_eq!(active.next_envelope_change_at(end), None);
    }

    #[test]
    fn the_open_spring_keeps_frames_coming_until_it_settles() {
        let now = Instant::now();
        let mut stack = ToastStack::default();
        stack.push(toast("spring", 4000), now);
        // Mid-hold, so the envelope itself is quiet and only the spring can
        // ask for frames.
        let hold = now + Duration::from_millis(1000);
        // Before the renderer's first advance the spring has never opened;
        // the size it reports is the seed, nowhere near the measured target.
        let (w, h) = stack.advance_motion(0, hold, 300.0, 80.0, true);
        assert!((w, h) != (300.0, 80.0));
        assert!(stack.needs_frames(hold), "a travelling spring owes frames");
        // Motion disabled snaps straight to the target: nothing left to draw.
        let (w, h) = stack.advance_motion(0, hold, 300.0, 80.0, false);
        assert_eq!((w, h), (300.0, 80.0));
        assert!(
            !stack.needs_frames(hold),
            "a snapped spring settles on the spot"
        );
    }

    #[test]
    fn the_stack_schedules_the_nearest_boundary_of_any_card() {
        let now = Instant::now();
        let mut stack = ToastStack::default();
        stack.push(toast("first", 4000), now);
        stack.push(toast("second", 4000), now + Duration::from_millis(1000));
        assert_eq!(
            stack.next_envelope_change_at(now),
            Some(now + Duration::from_secs_f32(TOAST_FADE_IN))
        );
        // Past the first card's fade-in, the second card's is the nearest.
        assert_eq!(
            stack.next_envelope_change_at(now + Duration::from_millis(200)),
            Some(now + Duration::from_millis(1000) + Duration::from_secs_f32(TOAST_FADE_IN))
        );
        // In the double hold, the first card's fade-out start wins.
        assert_eq!(
            stack.next_envelope_change_at(now + Duration::from_millis(2000)),
            Some(
                now + Duration::from_millis(4000)
                    .saturating_sub(Duration::from_secs_f32(TOAST_FADE_OUT))
            )
        );
    }

    #[test]
    fn sanitation_gives_the_sender_the_titles_one_line_shape() {
        let now = Instant::now();
        let mut stack = ToastStack::default();
        stack.push(
            ToastNotification {
                title: "Build finished".into(),
                app: "dusk\tbuilder\nextra line ignored".into(),
                timeout_ms: 100,
                ..Default::default()
            },
            now,
        );
        assert_eq!(
            stack.iter().next().unwrap().notification.app,
            "dusk builder"
        );

        // The length cap matches the title's: 80 characters, ellipsis ended.
        let mut stack = ToastStack::default();
        stack.push(
            ToastNotification {
                title: "t".into(),
                app: "x".repeat(120),
                timeout_ms: 100,
                ..Default::default()
            },
            now,
        );
        let app = &stack.iter().next().unwrap().notification.app;
        assert_eq!(app.chars().count(), 80);
        assert!(app.ends_with('\u{2026}'));

        // A sender that is blank once cleaned is no sender: the card draws
        // no attribution line for it.
        let mut stack = ToastStack::default();
        stack.push(
            ToastNotification {
                title: "t".into(),
                app: " \t ".into(),
                timeout_ms: 100,
                ..Default::default()
            },
            now,
        );
        assert!(stack.iter().next().unwrap().notification.app.is_empty());
    }

    #[test]
    fn a_card_without_a_sender_sanitizes_exactly_as_before() {
        // Unknown sender: the notification — and therefore the card, which
        // draws a sender line only for a non-empty app — is byte-identical
        // to what it has always been.
        let now = Instant::now();
        let mut stack = ToastStack::default();
        stack.push(
            ToastNotification {
                title: "a\tb\n second line ignored".into(),
                body: (0..4)
                    .map(|i| "x".repeat(90 + i))
                    .collect::<Vec<_>>()
                    .join("\n"),
                urgency: 9,
                timeout_ms: 100,
                ..Default::default()
            },
            now,
        );
        let card = &stack.iter().next().unwrap().notification;
        assert_eq!(card.title, "a b");
        assert!(card.app.is_empty());
        let body_lines: Vec<&str> = card.body.lines().collect();
        assert_eq!(body_lines.len(), 3);
        assert!(body_lines.iter().all(|l| l.chars().count() == 80));
        assert_eq!(card.urgency, 2);
    }

    #[test]
    fn the_sender_ink_only_dims_the_alpha() {
        let label = [136, 148, 172, 255];
        assert_eq!(sender_ink(label), [136, 148, 172, 184]);
        // Zero alpha is a fixed point; the hue never moves.
        assert_eq!(sender_ink([10, 20, 30, 0]), [10, 20, 30, 0]);
        assert_eq!(sender_ink([0, 0, 0, 128]), [0, 0, 0, 92]);
    }

    #[test]
    fn merge_text_bands_stacks_bands_over_a_transparent_gap() {
        // One 2x1 band over one 1x1 band, gap 2: width follows the widest,
        // the gap rows and the narrow band's padding stay zeroed.
        let top: &[u8] = &[255, 0, 0, 255, 0, 255, 0, 255];
        let bottom: &[u8] = &[1, 2, 3, 4];
        let (pixels, w, h) = merge_text_bands(&[(top, 2, 1), (bottom, 1, 1)], 2);
        assert_eq!((w, h), (2, 4));
        assert_eq!(
            pixels,
            vec![
                255, 0, 0, 255, 0, 255, 0, 255, // top band
                0, 0, 0, 0, 0, 0, 0, 0, // gap
                0, 0, 0, 0, 0, 0, 0, 0, // gap
                1, 2, 3, 4, 0, 0, 0, 0, // bottom band, right-padded
            ]
        );

        // A single band is copied through with no gap rows — this is the
        // shape a sender-only title texture takes.
        let (pixels, w, h) = merge_text_bands(&[(top, 2, 1)], 2);
        assert_eq!((w, h), (2, 1));
        assert_eq!(pixels.as_slice(), top);
    }

    #[test]
    fn merge_text_bands_refuses_empty_and_degenerate_input() {
        assert_eq!(merge_text_bands(&[], 2), (Vec::new(), 0, 0));
        let band: &[u8] = &[1, 2, 3, 4];
        assert_eq!(merge_text_bands(&[(band, 0, 1)], 2), (Vec::new(), 0, 0));
        assert_eq!(merge_text_bands(&[(band, 1, 0)], 2), (Vec::new(), 0, 0));
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
    fn stack_offsets_rise_monotonically_without_overlap() {
        // Two, three, and four cards (the visible cap), with and without the
        // OSD's reserved slot: every card starts strictly below the previous
        // card's bottom edge plus the gap.
        let height_sets: Vec<Vec<f32>> = vec![
            vec![84.0; 2],
            vec![84.0, 118.0, 72.0],
            vec![84.0, 118.0, 72.0, 150.0],
        ];
        for heights in height_sets {
            assert!(heights.len() <= MAX_TOASTS);
            for osd_visible in [false, true] {
                let mut top = stack_start(osd_visible);
                let mut spans = Vec::with_capacity(heights.len());
                for h in &heights {
                    spans.push((top, top + h));
                    top = stack_next(top, *h);
                }
                for pair in spans.windows(2) {
                    let (a_top, a_bottom) = pair[0];
                    let (b_top, _) = pair[1];
                    assert!(
                        b_top > a_top,
                        "offsets must rise monotonically: {a_top} then {b_top}"
                    );
                    assert!(
                        b_top >= a_bottom + STACK_GAP,
                        "cards overlap: [{a_top}, {a_bottom}) then {b_top}"
                    );
                }
            }
        }
    }

    #[test]
    fn stack_start_reserves_the_full_osd_card() {
        assert_eq!(stack_start(false), 0.0);
        assert_eq!(
            stack_start(true),
            super::super::osd::OSD_CARD_HEIGHT + STACK_GAP
        );
        // A zero-height card still advances the cursor by the gap, so two
        // cards mid-open can never sit on the same line.
        assert!(stack_next(0.0, 0.0) > 0.0);
        // A bogus negative height cannot drag the cursor upward.
        assert_eq!(stack_next(10.0, -5.0), 10.0 + STACK_GAP);
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

    fn progress(title: &str, notification_id: u32) -> ToastNotification {
        ToastNotification {
            title: title.into(),
            timeout_ms: 1000,
            notification_id,
            actions: vec![NotificationAction {
                key: "cancel".into(),
                label: "Cancel".into(),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn a_replacement_updates_the_live_card_in_place() {
        let now = Instant::now();
        let mut stack = ToastStack::default();
        assert!(stack.push(progress("Copying 12%", 7), now).is_empty());
        stack.set_hovered(Some(0), now + Duration::from_millis(500));

        let mut update = progress("Copying 13%", 7);
        update.actions.clear();
        let removed = stack.push(update, now + Duration::from_millis(900));
        // The card keeps its id — the renderer's key for hover, motion and
        // hit geometry — and hands it back only so the stale text textures
        // are rebuilt.
        assert_eq!(removed, vec![0]);
        assert_eq!(stack.iter().count(), 1);
        let card = stack.iter().next().unwrap();
        assert_eq!(card.id, 0);
        assert_eq!(card.notification.title, "Copying 13%");
        assert!(
            card.notification.actions.is_empty(),
            "the Cancel chip the update dropped must not linger"
        );
        // Still hovered: the countdown stays frozen.
        assert!(stack.prune(now + Duration::from_millis(5000)).is_empty());
        // Leaving runs the update's full timeout from here, not the first
        // push's remainder.
        stack.set_hovered(None, now + Duration::from_millis(5000));
        assert!(stack.prune(now + Duration::from_millis(5999)).is_empty());
        assert_eq!(stack.prune(now + Duration::from_millis(6000)), vec![0]);
    }

    #[test]
    fn a_replacement_lifts_a_fading_card_without_a_blink() {
        let now = Instant::now();
        let mut stack = ToastStack::default();
        stack.push(progress("Copying 98%", 7), now);
        let late = now + Duration::from_millis(900);
        let before = stack.iter().next().unwrap().alpha(late);
        assert!(before < 0.5, "the card should be well into its fade-out");

        stack.push(progress("Copying 99%", 7), late);
        let card = stack.iter().next().unwrap();
        let after = card.alpha(late);
        assert!(
            (after - before).abs() < 0.02,
            "the envelope must continue from {before}, not restart: {after}"
        );
        assert_eq!(card.alpha(late + Duration::from_millis(200)), 1.0);
    }

    #[test]
    fn standalone_and_dismissed_cards_are_never_replaced() {
        let now = Instant::now();
        let mut stack = ToastStack::default();
        // Record zero marks a toast with no notification behind it: two of
        // them are two cards.
        stack.push(progress("a", 0), now);
        stack.push(progress("b", 0), now);
        assert_eq!(stack.iter().count(), 2);
        // A card the user clicked away finishes fading; the update that
        // follows gets a card of its own rather than reviving it.
        stack.push(progress("Copying 12%", 7), now);
        assert!(stack.dismiss(2, now + Duration::from_millis(500)));
        let removed = stack.push(progress("Copying 13%", 7), now + Duration::from_millis(510));
        assert!(removed.is_empty());
        assert_eq!(stack.iter().count(), 4);
        assert_eq!(stack.iter().last().unwrap().id, 3);
    }

    #[test]
    fn a_hover_during_the_fade_in_lets_the_card_finish_appearing() {
        let now = Instant::now();
        let mut stack = ToastStack::default();
        stack.push(toast("early hover", 1000), now);
        let hover = now + Duration::from_millis(50);
        stack.set_hovered(Some(0), hover);
        let card = stack.iter().next().unwrap();
        assert!(card.alpha(hover) < 0.5);
        // The countdown is frozen; the arrival is not.
        assert_eq!(card.alpha(now + Duration::from_millis(1000)), 1.0);
        assert!(stack.prune(now + Duration::from_millis(5000)).is_empty());
        // Leaving keeps the card opaque — the fade-in does not rewind to the
        // frozen 50 ms — and the timeout runs its course from here.
        let leave = now + Duration::from_millis(5000);
        stack.set_hovered(None, leave);
        assert_eq!(stack.iter().next().unwrap().alpha(leave), 1.0);
        assert!(stack.prune(leave + Duration::from_millis(949)).is_empty());
        assert_eq!(stack.prune(leave + Duration::from_millis(950)), vec![0]);
    }

    #[test]
    fn card_geometry_compares_by_value() {
        // The udev backend hands this list to the input thread on every
        // composited frame while a card is up; comparing the new list against
        // the published one is what lets it skip the lock and the clone when
        // nothing moved, so the geometry must answer equality by value.
        let rects = |x: f32| ToastRects {
            id: 7,
            card: [x, 20.0, 300.0, 80.0],
            buttons: vec![[x + 8.0, 84.0, 60.0, ACTION_BUTTON_H]],
        };
        assert_eq!(rects(10.0), rects(10.0));
        assert_ne!(rects(10.0), rects(11.0), "a moved card is a new frame");
        let mut without_chip = rects(10.0);
        without_chip.buttons.clear();
        assert_ne!(
            rects(10.0),
            without_chip,
            "an action row that came or went must not compare equal"
        );
    }

    #[test]
    fn leading_blank_lines_do_not_consume_the_line_budget() {
        let now = Instant::now();
        let mut stack = ToastStack::default();
        stack.push(
            ToastNotification {
                title: "\n  \nAlert".into(),
                body: "\n\n\nDisk almost full\n\nsecond".into(),
                ..Default::default()
            },
            now,
        );
        let card = &stack.iter().next().unwrap().notification;
        assert_eq!(card.title, "Alert");
        assert_eq!(card.body, "Disk almost full\nsecond");
    }
}
