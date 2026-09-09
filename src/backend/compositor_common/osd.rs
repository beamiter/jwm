//! Backend-neutral volume/brightness OSD state.
//!
//! One card, replace-in-place: a new event restarts the hold timer instead of
//! stacking (macOS / DMS / Noctalia behavior). Everything that is not GL —
//! the hold+fade envelope, the display strings, and the frame-scheduling
//! queries the compositors pace themselves with — lives here so the two
//! compositors cannot drift. A settled hold draws nothing until its next
//! envelope boundary; only the fades, the open/morph spring, and the final
//! pruning frame ask for compositor frames.

use crate::backend::api::OsdKind;
use crate::backend::compositor_common::dynamic_island::IslandMotion;
use std::time::{Duration, Instant};

/// Time the card stays fully visible after the most recent event.
const OSD_HOLD: Duration = Duration::from_millis(1400);
/// Fade-out length after the hold expires.
const OSD_FADE_OUT: f32 = 0.25;
/// Hold plus fade-out (1650 ms): how long after the last event the card may
/// still be on screen. jwm's control-feedback path reads it to tell "refresh
/// the visible card" from "the card is gone; showing now would pop a new
/// one".
pub(crate) const OSD_VISIBLE_WINDOW: Duration = Duration::from_millis(1650);
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

    /// Whether the card's on-screen state is moving at `now`: inside the
    /// fade-in or the fade-out. A settled hold reads false — its next change
    /// is a scheduled boundary ([`Self::next_envelope_change_at`]), not a
    /// running curve.
    pub(crate) fn envelope_active(&self, now: Instant) -> bool {
        if now.saturating_duration_since(self.appeared).as_secs_f32() < OSD_FADE_IN {
            return true;
        }
        let since_refresh = now.saturating_duration_since(self.refreshed).as_secs_f32();
        let hold = OSD_HOLD.as_secs_f32();
        since_refresh >= hold && since_refresh < hold + OSD_FADE_OUT
    }

    /// Whether the compositor owes this card frames right now: the envelope
    /// is moving, or the card reached its end and is owed the frame that
    /// prunes it and erases its pixels.
    pub(crate) fn needs_frames(&self, now: Instant) -> bool {
        self.envelope_active(now) || self.expired(now)
    }

    /// The next instant the card's on-screen state changes without further
    /// input: the fade-in completing, the hold ending and the fade-out
    /// beginning, or the card expiring. Unlike a toast the card has no
    /// pointer interaction — it is a transient value display with nothing to
    /// hover or click — so there is no frozen state that could answer `None`
    /// while the card lives; past the expiry nothing is scheduled because
    /// [`Self::needs_frames`] already reports the owed pruning frame.
    pub(crate) fn next_envelope_change_at(&self, now: Instant) -> Option<Instant> {
        let fade_in_end = self.appeared + Duration::from_secs_f32(OSD_FADE_IN);
        let fade_out_start = self.refreshed + OSD_HOLD;
        let expiry = self.refreshed + OSD_HOLD + Duration::from_secs_f32(OSD_FADE_OUT);
        [fade_in_end, fade_out_start, expiry]
            .into_iter()
            .filter(|&at| at > now)
            .min()
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
            // Every glyph below is a FontAwesome-4 codepoint for the same
            // reason as `VolumeMuted`: FA-5-era f6xx glyphs render as hollow
            // boxes on common Nerd Font builds. The off states reuse the
            // panel's own vocabulary — fa-ban is what the network row draws
            // for a switched-off radio.
            OsdKind::DoNotDisturb(true) => ("\u{f1f7}", "Do Not Disturb On".into()), // fa-bell-slash
            OsdKind::DoNotDisturb(false) => ("\u{f0f3}", "Do Not Disturb Off".into()), // fa-bell
            OsdKind::Caffeine(true) => ("\u{f0f4}", "Caffeine On".into()),           // fa-coffee
            OsdKind::Caffeine(false) => ("\u{f0f4}", "Caffeine Off".into()),
            OsdKind::NightLight(true) => ("\u{f186}", "Night Light On".into()), // fa-moon-o
            OsdKind::NightLight(false) => ("\u{f185}", "Night Light Off".into()), // fa-sun
            OsdKind::Wifi(true) => ("\u{f1eb}", "Wi-Fi On".into()),             // fa-wifi
            OsdKind::Wifi(false) => ("\u{f05e}", "Wi-Fi Off".into()),           // fa-ban
            OsdKind::Bluetooth(true) => ("\u{f293}", "Bluetooth On".into()),    // fa-bluetooth
            OsdKind::Bluetooth(false) => ("\u{f293}", "Bluetooth Off".into()),
            // fa-microphone-slash / fa-microphone: both sit in the FA-4
            // range for the same reason as `VolumeMuted` above.
            OsdKind::MicMute(true) => ("\u{f131}", "Microphone Muted".into()),
            OsdKind::MicMute(false) => ("\u{f130}", "Microphone Unmuted".into()),
        }
    }

    /// Bar fill fraction, or `None` for kinds that draw no bar. Muted renders
    /// an empty bar rather than no bar, so the card keeps its shape.
    pub(crate) fn fill(&self) -> Option<f32> {
        match self.kind {
            OsdKind::Media
            | OsdKind::DoNotDisturb(_)
            | OsdKind::Caffeine(_)
            | OsdKind::NightLight(_)
            | OsdKind::Wifi(_)
            | OsdKind::Bluetooth(_)
            | OsdKind::MicMute(_) => None,
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

    /// Whether the open/morph spring is still travelling toward the current
    /// card's size. The target is derived from the card itself — both
    /// renderers advance the spring toward exactly
    /// `(card_width(), OSD_CARD_HEIGHT)` — so a replacement that changed the
    /// card's width (a volume slider swapping to a wider media card) reports
    /// animating until the morph arrives, with no queued state of its own.
    pub(crate) fn spring_animating(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|osd| self.motion.animating(osd.card_width(), OSD_CARD_HEIGHT))
    }

    /// Whether the compositor owes the card frames right now: the envelope
    /// or the open/morph spring is moving, or the card reached its end and
    /// is owed the frame that prunes it. A fully settled hold answers false —
    /// its next change is a scheduled boundary
    /// ([`Self::next_envelope_change_at`]), not a running curve.
    pub(crate) fn needs_frames(&self, now: Instant) -> bool {
        self.spring_animating()
            || self
                .active
                .as_ref()
                .is_some_and(|osd| osd.needs_frames(now))
    }

    /// The next instant the card's on-screen state changes without input, so
    /// the event loop can sleep until then and still start the fade-out on
    /// time. `None` when the slot is empty — a `show` is an input event that
    /// makes its own frame.
    pub(crate) fn next_envelope_change_at(&self, now: Instant) -> Option<Instant> {
        self.active
            .as_ref()
            .and_then(|osd| osd.next_envelope_change_at(now))
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
    fn a_settled_hold_needs_no_frames_but_its_boundaries_do() {
        let start = Instant::now();
        let mut slot = OsdSlot::default();
        slot.show(OsdKind::Volume, 40, start);
        {
            let osd = slot.get().unwrap();
            // Fade-in: the envelope is moving and owes frames.
            assert!(osd.envelope_active(start));
            assert!(osd.needs_frames(start));
            assert!(osd.envelope_active(start + Duration::from_millis(119)));
            // Settled hold: nothing moves, nothing is owed.
            let hold = start + Duration::from_millis(500);
            assert!(!osd.envelope_active(hold));
            assert!(!osd.needs_frames(hold));
            // The fade-out begins when the 1400 ms hold ends ...
            assert!(!osd.envelope_active(start + Duration::from_millis(1399)));
            let fade_out = start + Duration::from_millis(1401);
            assert!(osd.envelope_active(fade_out));
            assert!(osd.needs_frames(fade_out));
            // ... and once the fade has run the card is owed its pruning frame.
            let expiry = start + Duration::from_millis(1651);
            assert!(!osd.envelope_active(expiry));
            assert!(osd.needs_frames(expiry));
        }
        // Pruned, the slot goes quiet.
        assert!(slot.prune(start + Duration::from_millis(1651)));
        assert!(!slot.needs_frames(start + Duration::from_millis(1651)));
        assert_eq!(slot.next_envelope_change_at(start), None);
    }

    #[test]
    fn next_envelope_change_walks_the_boundaries() {
        let start = Instant::now();
        let mut slot = OsdSlot::default();
        slot.show(OsdKind::Volume, 40, start);
        let osd = slot.get().unwrap();
        // While fading in, the next change is the fade-in completing.
        assert_eq!(
            osd.next_envelope_change_at(start),
            Some(start + Duration::from_secs_f32(OSD_FADE_IN))
        );
        // In the hold it is the fade-out's start, when the hold expires.
        assert_eq!(
            osd.next_envelope_change_at(start + Duration::from_millis(500)),
            Some(start + OSD_HOLD)
        );
        // Once the fade-out runs, only the expiry is left.
        assert_eq!(
            osd.next_envelope_change_at(start + OSD_HOLD),
            Some(start + OSD_HOLD + Duration::from_secs_f32(OSD_FADE_OUT))
        );
        // Past the expiry nothing is scheduled: the card is owed a prune,
        // which `needs_frames` already reports.
        assert_eq!(
            osd.next_envelope_change_at(start + Duration::from_millis(1651)),
            None
        );
        // The slot-level query follows the card, and an empty slot schedules
        // nothing: a `show` is an input event that makes its own frame.
        assert_eq!(
            slot.next_envelope_change_at(start + Duration::from_millis(500)),
            Some(start + OSD_HOLD)
        );
        assert_eq!(OsdSlot::default().next_envelope_change_at(start), None);
    }

    #[test]
    fn a_refresh_moves_the_fade_out_boundary_without_new_frames() {
        let start = Instant::now();
        let mut slot = OsdSlot::default();
        slot.show(OsdKind::Volume, 40, start);
        // A held volume key repeats `show`: each repeat is an input event
        // that arms its own frame, restarts the hold, and — the spring being
        // settled at an unchanged width — asks for nothing further.
        let repeat = start + Duration::from_millis(500);
        slot.show(OsdKind::Volume, 45, repeat);
        assert!(!slot.needs_frames(repeat));
        assert_eq!(
            slot.next_envelope_change_at(repeat),
            Some(repeat + OSD_HOLD)
        );
    }

    #[test]
    fn the_open_spring_keeps_frames_coming_until_it_settles() {
        let start = Instant::now();
        let mut slot = OsdSlot::default();
        slot.show(OsdKind::Volume, 40, start);
        // Mid-hold, so the envelope itself is quiet and only the spring can
        // ask for frames.
        let hold = start + Duration::from_millis(500);
        // Before the renderer's first advance the spring has never opened;
        // the size it reports is the seed, nowhere near the card's target.
        let (w, h) =
            slot.motion_mut()
                .advance_with_motion(hold, SLIDER_CARD_WIDTH, OSD_CARD_HEIGHT, true);
        assert!((w, h) != (SLIDER_CARD_WIDTH, OSD_CARD_HEIGHT));
        assert!(slot.spring_animating());
        assert!(slot.needs_frames(hold), "a travelling spring owes frames");
        // Motion disabled snaps straight to the target: nothing left to draw.
        let (w, h) =
            slot.motion_mut()
                .advance_with_motion(hold, SLIDER_CARD_WIDTH, OSD_CARD_HEIGHT, false);
        assert_eq!((w, h), (SLIDER_CARD_WIDTH, OSD_CARD_HEIGHT));
        assert!(!slot.spring_animating());
        assert!(
            !slot.needs_frames(hold),
            "a snapped spring settles on the spot"
        );
    }

    #[test]
    fn a_replacement_morph_keeps_frames_coming_until_the_new_width_arrives() {
        let start = Instant::now();
        let mut slot = OsdSlot::default();
        slot.show(OsdKind::Volume, 40, start);
        // Open the slider card and run the spring to rest, mid-hold.
        let mut t = start;
        slot.motion_mut()
            .advance_with_motion(t, SLIDER_CARD_WIDTH, OSD_CARD_HEIGHT, true);
        while slot.spring_animating() {
            t += Duration::from_millis(16);
            slot.motion_mut()
                .advance_with_motion(t, SLIDER_CARD_WIDTH, OSD_CARD_HEIGHT, true);
        }
        let hold = start + Duration::from_millis(500);
        assert!(
            !slot.needs_frames(hold),
            "a settled card with a settled spring is quiet"
        );

        // A wider media card replaces the slider in place: the swap has no
        // queued state of its own — the spring simply owes the morph to the
        // new target, and the hold restarts on the new card.
        slot.show_media("Blue in Green", hold);
        assert!(
            slot.spring_animating(),
            "the morph to the wider card owes frames"
        );
        assert!(slot.needs_frames(hold));
        assert_eq!(slot.next_envelope_change_at(hold), Some(hold + OSD_HOLD));

        // The morph arrived and the envelope quiet, the slot goes quiet again.
        slot.motion_mut()
            .advance_with_motion(hold, MEDIA_CARD_WIDTH, OSD_CARD_HEIGHT, false);
        assert!(!slot.spring_animating());
        assert!(!slot.needs_frames(hold + Duration::from_millis(100)));
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

    #[test]
    fn toggle_kinds_draw_labeled_cards_without_bars() {
        let now = Instant::now();
        // Each kind carries its new state in the label, an icon in the
        // FontAwesome-4 range common Nerd Font builds actually carry (an
        // f6xx glyph renders as a hollow box), no bar, and the slider
        // card's width.
        for (kind, icon, label) in [
            (OsdKind::DoNotDisturb(true), "\u{f1f7}", "Do Not Disturb On"),
            (
                OsdKind::DoNotDisturb(false),
                "\u{f0f3}",
                "Do Not Disturb Off",
            ),
            (OsdKind::Caffeine(true), "\u{f0f4}", "Caffeine On"),
            (OsdKind::Caffeine(false), "\u{f0f4}", "Caffeine Off"),
            (OsdKind::NightLight(true), "\u{f186}", "Night Light On"),
            (OsdKind::NightLight(false), "\u{f185}", "Night Light Off"),
            (OsdKind::Wifi(true), "\u{f1eb}", "Wi-Fi On"),
            (OsdKind::Wifi(false), "\u{f05e}", "Wi-Fi Off"),
            (OsdKind::Bluetooth(true), "\u{f293}", "Bluetooth On"),
            (OsdKind::Bluetooth(false), "\u{f293}", "Bluetooth Off"),
            (OsdKind::MicMute(true), "\u{f131}", "Microphone Muted"),
            (OsdKind::MicMute(false), "\u{f130}", "Microphone Unmuted"),
        ] {
            let mut slot = OsdSlot::default();
            slot.show(kind, 0, now);
            let osd = slot.get().unwrap();
            assert_eq!(osd.icon_and_label(), (icon, label.to_string()), "{kind:?}");
            assert_eq!(osd.fill(), None, "{kind:?} draws no bar");
            assert_eq!(osd.card_width(), SLIDER_CARD_WIDTH, "{kind:?}");
            for ch in icon
                .chars()
                .filter(|ch| ('\u{f000}'..'\u{f900}').contains(ch))
            {
                assert!(
                    (ch as u32) < 0xf600,
                    "{kind:?}'s icon {ch:?} (U+{:04X}) is outside the FontAwesome-4 range",
                    ch as u32
                );
            }
        }
    }
}
