//! Backend-neutral volume/brightness OSD state.
//!
//! One card, replace-in-place: a new event restarts the hold timer instead of
//! stacking (macOS / DMS / Noctalia behavior). Everything that is not GL —
//! the hold+fade envelope and the display strings — lives here so the two
//! compositors cannot drift.

use crate::backend::api::OsdKind;
use std::time::{Duration, Instant};

/// Time the card stays fully visible after the most recent event.
const OSD_HOLD: Duration = Duration::from_millis(1400);
/// Fade-out length after the hold expires.
const OSD_FADE_OUT: f32 = 0.25;
/// Fade-in length when the card first appears.
const OSD_FADE_IN: f32 = 0.12;

#[derive(Debug, Clone)]
pub(crate) struct ActiveOsd {
    pub(crate) kind: OsdKind,
    /// 0..=100 for the bar; volume above 100% still clamps the bar full.
    pub(crate) percent: u8,
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
            OsdKind::VolumeMuted => ("\u{f6a9}", "muted".into()), // fa-volume-mute
            OsdKind::Brightness => ("\u{f185}", format!("{}%", self.percent)), // fa-sun
        }
    }

    /// Bar fill fraction (muted renders an empty bar).
    pub(crate) fn fill(&self) -> f32 {
        if matches!(self.kind, OsdKind::VolumeMuted) {
            0.0
        } else {
            f32::from(self.percent.min(100)) / 100.0
        }
    }
}

/// Single-slot OSD holder used by both compositors.
#[derive(Debug, Default)]
pub(crate) struct OsdSlot {
    active: Option<ActiveOsd>,
}

impl OsdSlot {
    /// Show or refresh the OSD. A card already on screen keeps its fade-in
    /// origin so updating it does not flicker.
    pub(crate) fn show(&mut self, kind: OsdKind, percent: u8, now: Instant) {
        match &mut self.active {
            Some(osd) if !osd.expired(now) => {
                osd.kind = kind;
                osd.percent = percent;
                osd.refreshed = now;
            }
            _ => {
                self.active = Some(ActiveOsd {
                    kind,
                    percent,
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
            true
        } else {
            false
        }
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
        assert!((osd.fill() - 0.45).abs() < 1e-6);

        slot.show(OsdKind::VolumeMuted, 45, now);
        let osd = slot.get().unwrap();
        assert_eq!(osd.icon_and_label().1, "muted");
        assert_eq!(osd.fill(), 0.0);

        slot.show(OsdKind::Brightness, 130, now);
        assert_eq!(slot.get().unwrap().fill(), 1.0);
    }
}
