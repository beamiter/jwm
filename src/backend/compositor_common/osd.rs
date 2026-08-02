//! Backend-neutral volume/brightness OSD state.
//!
//! One card, replace-in-place: a new event restarts the hold timer instead of
//! stacking (macOS / DMS / Noctalia behavior). Everything that is not GL —
//! the hold+fade envelope and the display strings — lives here so the two
//! compositors cannot drift.

use crate::backend::api::OsdKind;
use crate::backend::compositor_common::dynamic_island::IslandMotion;
use std::time::{Duration, Instant};

/// Time the card stays fully visible after the most recent event.
const OSD_HOLD: Duration = Duration::from_millis(1400);
/// Fade-out length after the hold expires.
const OSD_FADE_OUT: f32 = 0.25;
/// Fade-in length when the card first appears.
const OSD_FADE_IN: f32 = 0.12;

/// Height of the docked card. Shared so the toast stack can reserve room for
/// an OSD without waiting for its spring to arrive at a height.
pub(crate) const OSD_CARD_HEIGHT: f32 = 64.0;
/// Card width for the slider kinds: a fixed geometry so the bar does not
/// jump as digits change.
const SLIDER_CARD_WIDTH: f32 = 360.0;
/// Media cards carry a track title instead of a bar, so they are wider.
const MEDIA_CARD_WIDTH: f32 = 520.0;
/// Longest track label drawn; the renderer does not wrap.
const MAX_MEDIA_LABEL_CHARS: usize = 48;

#[derive(Debug, Clone)]
pub(crate) struct ActiveOsd {
    pub(crate) kind: OsdKind,
    /// 0..=100 for the bar; volume above 100% still clamps the bar full.
    pub(crate) percent: u8,
    /// Free text for kinds that show a label instead of a value, i.e. media.
    label: Option<String>,
    /// When the OSD first became visible (drives fade-in only).
    appeared: Instant,
    /// When the most recent event arrived (drives hold + fade-out).
    refreshed: Instant,
}

impl ActiveOsd {
    /// Opacity envelope at `now`: fade in from first appearance, hold from the
    /// last refresh, then fade out.
    pub(crate) fn alpha(&self, now: Instant) -> f32 {
        let since_appear = now.saturating_duration_since(self.appeared).as_secs_f32();
        let since_refresh = now.saturating_duration_since(self.refreshed).as_secs_f32();
        let fade_in = (since_appear / OSD_FADE_IN).clamp(0.0, 1.0);
        let fade_out = ((OSD_HOLD.as_secs_f32() + OSD_FADE_OUT - since_refresh) / OSD_FADE_OUT)
            .clamp(0.0, 1.0);
        fade_in.min(fade_out)
    }

    fn expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.refreshed).as_secs_f32()
            >= OSD_HOLD.as_secs_f32() + OSD_FADE_OUT
    }

    /// Icon glyph + label text the renderer shows, e.g. `("\u{f028}", "45%")`.
    pub(crate) fn icon_and_label(&self) -> (&'static str, String) {
        match self.kind {
            OsdKind::Volume => {
                let icon = if self.percent == 0 {
                    "\u{f026}" // fa-volume-off
                } else if self.percent < 50 {
                    "\u{f027}" // fa-volume-down
                } else {
                    "\u{f028}" // fa-volume-up
                };
                (icon, format!("{}%", self.percent))
            }
            // fa-volume-off: fa-volume-mute is an f6xx codepoint that common
            // Nerd Font builds lack, and it rendered as a hollow box.
            OsdKind::VolumeMuted => ("\u{f026}", "muted".into()),
            OsdKind::Brightness => ("\u{f185}", format!("{}%", self.percent)), // fa-sun
            OsdKind::Media => (
                "\u{f001}", // fa-music
                self.label.clone().unwrap_or_default(),
            ),
        }
    }

    /// Bar fill fraction, or `None` for kinds that draw no bar. Muted renders
    /// an empty bar rather than no bar, so the card keeps its shape.
    pub(crate) fn fill(&self) -> Option<f32> {
        match self.kind {
            OsdKind::Media => None,
            OsdKind::VolumeMuted => Some(0.0),
            _ => Some(f32::from(self.percent.min(100)) / 100.0),
        }
    }

    /// Card width for this kind. Lives here so both compositors lay the card
    /// out identically.
    pub(crate) fn card_width(&self) -> f32 {
        match self.kind {
            OsdKind::Media => MEDIA_CARD_WIDTH,
            _ => SLIDER_CARD_WIDTH,
        }
    }
}

/// Collapse whitespace, drop control characters, and ellipsize a track label.
fn sanitize_label(label: &str) -> String {
    let cleaned: String = label
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.chars().count() <= MAX_MEDIA_LABEL_CHARS {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(MAX_MEDIA_LABEL_CHARS - 1).collect();
    out.push('\u{2026}');
    out
}

/// Single-slot OSD holder used by both compositors.
#[derive(Debug, Default)]
pub(crate) struct OsdSlot {
    active: Option<ActiveOsd>,
    /// Open/morph spring for the docked card. Living here rather than in each
    /// compositor is what keeps a card that changes kind mid-flight — volume
    /// replaced by a wider media card — morphing rather than restarting.
    motion: IslandMotion,
}

impl OsdSlot {
    /// Show or refresh the OSD. A card already on screen keeps its fade-in
    /// origin so updating it does not flicker.
    pub(crate) fn show(&mut self, kind: OsdKind, percent: u8, now: Instant) {
        self.show_labeled(kind, percent, None, now);
    }

    /// Show a media card: the track label replaces the value, and the card
    /// carries no bar.
    pub(crate) fn show_media(&mut self, label: &str, now: Instant) {
        self.show_labeled(OsdKind::Media, 0, Some(sanitize_label(label)), now);
    }

    fn show_labeled(&mut self, kind: OsdKind, percent: u8, label: Option<String>, now: Instant) {
        match &mut self.active {
            Some(osd) if !osd.expired(now) => {
                osd.kind = kind;
                osd.percent = percent;
                osd.label = label;
                osd.refreshed = now;
            }
            _ => {
                // A card that fully expired earlier springs open again rather
                // than resuming the geometry it died at.
                self.motion.close();
                self.active = Some(ActiveOsd {
                    kind,
                    percent,
                    label,
                    appeared: now,
                    refreshed: now,
                });
            }
        }
    }

    /// Drop the card once fully faded. Returns `true` when a card was removed
    /// (so the caller can free any cached texture).
    pub(crate) fn prune(&mut self, now: Instant) -> bool {
        if self.active.as_ref().is_some_and(|osd| osd.expired(now)) {
            self.active = None;
            self.motion.close();
            true
        } else {
            false
        }
    }

    /// The card's open/morph spring, for the renderer to advance.
    pub(crate) fn motion_mut(&mut self) -> &mut IslandMotion {
        &mut self.motion
    }

    pub(crate) fn get(&self) -> Option<&ActiveOsd> {
        self.active.as_ref()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.active.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_extends_hold_without_restarting_fade_in() {
        let start = Instant::now();
        let mut slot = OsdSlot::default();
        slot.show(OsdKind::Volume, 40, start);
        assert_eq!(slot.get().unwrap().alpha(start), 0.0);

        let later = start + Duration::from_millis(500);
        assert_eq!(slot.get().unwrap().alpha(later), 1.0);

        // Refresh near expiry: alpha stays 1.0 (no fade-in restart), expiry moves.
        let near_expiry = start + Duration::from_millis(1500);
        slot.show(OsdKind::Volume, 45, near_expiry);
        assert_eq!(slot.get().unwrap().alpha(near_expiry), 1.0);
        assert!(!slot.prune(near_expiry + Duration::from_millis(1000)));
        assert!(slot.prune(near_expiry + Duration::from_millis(1650)));
        assert!(slot.is_empty());
    }

    #[test]
    fn labels_and_fill_follow_kind() {
        let now = Instant::now();
        let mut slot = OsdSlot::default();
        slot.show(OsdKind::Volume, 45, now);
        let osd = slot.get().unwrap();
        assert_eq!(osd.icon_and_label().1, "45%");
        assert!((osd.fill().unwrap() - 0.45).abs() < 1e-6);

        slot.show(OsdKind::VolumeMuted, 45, now);
        let osd = slot.get().unwrap();
        assert_eq!(osd.icon_and_label().1, "muted");
        assert_eq!(osd.fill(), Some(0.0));

        slot.show(OsdKind::Brightness, 130, now);
        assert_eq!(slot.get().unwrap().fill(), Some(1.0));
    }

    #[test]
    fn a_media_card_carries_a_label_and_no_bar() {
        let now = Instant::now();
        let mut slot = OsdSlot::default();
        slot.show_media("Blue in Green \u{2014} Miles Davis", now);
        let osd = slot.get().unwrap();

        assert_eq!(osd.icon_and_label().1, "Blue in Green \u{2014} Miles Davis");
        assert_eq!(osd.fill(), None);
        assert!(osd.card_width() > SLIDER_CARD_WIDTH);
    }

    #[test]
    fn media_labels_are_sanitized_and_ellipsized() {
        let now = Instant::now();
        let mut slot = OsdSlot::default();
        slot.show_media(&format!("  line\nbreak {}", "x".repeat(80)), now);
        let label = slot.get().unwrap().icon_and_label().1;

        assert!(label.starts_with("line break"));
        assert_eq!(label.chars().count(), MAX_MEDIA_LABEL_CHARS);
        assert!(label.ends_with('\u{2026}'));
    }

    #[test]
    fn a_slider_card_replaces_a_media_card_in_place() {
        let now = Instant::now();
        let mut slot = OsdSlot::default();
        slot.show_media("Some Track", now);
        slot.show(OsdKind::Volume, 30, now + Duration::from_millis(100));
        let osd = slot.get().unwrap();

        // The stale label must not survive onto the volume card.
        assert_eq!(osd.icon_and_label().1, "30%");
        assert_eq!(osd.card_width(), SLIDER_CARD_WIDTH);
    }
}
