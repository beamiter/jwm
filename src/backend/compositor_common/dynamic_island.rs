//! Geometry and motion for JWM's Dynamic-Island-style panels.
//!
//! The compositor's own transient surfaces — the volume/brightness/media OSD
//! and the toast stack — used to sit wherever there was room: the OSD floated
//! near the bottom of the screen, toasts piled up in the top-right corner.
//! Neither had any relationship to the status bar, so the desktop read as a
//! screen with things scattered on it rather than one surface.
//!
//! Docking them to the bar's bottom edge is what makes them read as one piece:
//! the panel's top edge is flush with the strip it drops out of, its top
//! corners are square so the two shapes merge, and it springs open from a
//! narrow seed rather than fading in at full size. The bar is the notch.
//!
//! Everything here is geometry and motion only — no GL — so the X11 and
//! Wayland compositors place their panels identically.

use crate::backend::compositor_common::effects::{clamp_effect_dt, finite_clamp};
use std::time::Instant;

/// Gap between the bar's bottom edge and a docked panel.
///
/// Zero: the point of the effect is that the two shapes touch. It is named
/// rather than inlined because "flush" is a deliberate choice, not an
/// oversight, and a themed build may want a hair of separation.
pub(crate) const DOCK_GAP: f32 = 0.0;

/// Fallback distance from the top of the screen when no bar is on screen.
const NO_BAR_TOP_MARGIN: f32 = 12.0;

/// Width the panel springs open from.
///
/// macOS grows its island out of the notch, whose width is fixed by the
/// hardware. JWM's bar spans the whole output, so there is no notch to inherit
/// a width from; a narrow pill centred on the bar reads the same way.
const SEED_WIDTH: f32 = 120.0;

/// Spring constants for the open/morph motion. The damping ratio works out
/// near 0.75, so the panel overshoots its target slightly and settles inside
/// about a third of a second — the difference between "pops open" and
/// "inflates".
const SPRING_STIFFNESS: f32 = 260.0;
const SPRING_DAMPING: f32 = 24.0;

/// Largest spring step before the integrator is substepped.
const MAX_SPRING_STEP: f32 = 1.0 / 240.0;
const MAX_SPRING_SUBSTEPS: usize = 32;

/// A panel is close enough to its target to stop animating.
const SETTLE_DISTANCE: f32 = 0.4;
const SETTLE_VELOCITY: f32 = 4.0;

/// Seconds a pointer-hover cue takes to ease in once its target appears,
/// from the toast family's quick fades (`TOAST_DISMISS_FADE` is 0.12): long
/// enough to read as motion next to the keyboard's springing pill, short
/// enough that the pointer never waits on its own feedback.
const HOVER_EASE_IN: f32 = 0.12;

/// Where docked panels attach: the strip they drop out of.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct IslandDock {
    /// Horizontal centre panels are centred on.
    pub(crate) centre_x: f32,
    /// The y a docked panel's top edge sits on.
    pub(crate) top_y: f32,
    /// Whether a bar was actually found to merge with.
    ///
    /// Squaring a panel's top corners is only right when there is something
    /// directly above for it to merge into. Hanging a flat-topped card in open
    /// space just looks like a card with a bug in its corner radius.
    merges_with_bar: bool,
    /// Output bounds used for the no-bar fallback and final containment.
    viewport: [f32; 4],
}

impl IslandDock {
    /// Dock under the status bar at `bar` (`[x, y, w, h]`), or under the top of
    /// the screen when the bar is hidden or lives on another output.
    ///
    /// The bar's own centre is used rather than the screen's: JWM's bar can be
    /// inset from the edges, and a panel centred on the screen under a bar that
    /// is not would visibly fail to line up with it.
    #[must_use]
    pub(crate) fn for_bar(bar: Option<[f32; 4]>, viewport: [f32; 4]) -> Self {
        let viewport = normalized_viewport(viewport);
        let [viewport_x, viewport_y, viewport_w, viewport_h] = viewport;
        match bar.and_then(|bar| clip_bar_to_viewport(bar, viewport)) {
            Some([x, y, w, h]) if w > 0.0 && h > 0.0 => Self {
                centre_x: finite_clamp(
                    x + w * 0.5,
                    viewport_x,
                    viewport_x + viewport_w,
                    viewport_x + viewport_w * 0.5,
                ),
                top_y: finite_clamp(
                    y + h + DOCK_GAP,
                    viewport_y,
                    viewport_y + viewport_h,
                    viewport_y + NO_BAR_TOP_MARGIN,
                ),
                merges_with_bar: true,
                viewport,
            },
            _ => Self {
                centre_x: viewport_x + viewport_w * 0.5,
                top_y: (viewport_y + NO_BAR_TOP_MARGIN).min(viewport_y + viewport_h),
                merges_with_bar: false,
                viewport,
            },
        }
    }

    /// The rect a panel of `width` x `height` occupies, centred on the dock.
    #[must_use]
    pub(crate) fn rect(&self, width: f32, height: f32, y_offset: f32) -> [f32; 4] {
        [
            self.centre_x - width * 0.5,
            self.top_y + y_offset,
            width,
            height,
        ]
    }

    /// As [`Self::rect`], constrained to the output this dock belongs to.
    /// Modal system UI uses this path; transient OSD/toast stacks retain their
    /// existing placement semantics through [`Self::rect`].
    #[must_use]
    pub(crate) fn contained_rect(&self, width: f32, height: f32, y_offset: f32) -> [f32; 4] {
        let [viewport_x, viewport_y, viewport_w, viewport_h] = self.viewport;
        let width = finite_clamp(width, 0.0, f32::MAX, 0.0);
        let height = finite_clamp(height, 0.0, f32::MAX, 0.0);
        let max_x = (viewport_x + viewport_w - width).max(viewport_x);
        let max_y = (viewport_y + viewport_h - height).max(viewport_y);
        [
            (self.centre_x - width * 0.5).clamp(viewport_x, max_x),
            (self.top_y + y_offset).clamp(viewport_y, max_y),
            width,
            height,
        ]
    }

    /// Corner radii for a panel of `height` hanging at `y_offset` below the
    /// dock: `(top, bottom)`.
    ///
    /// Only a panel touching the bar squares off against it. One stacked below
    /// another, or hanging from a screen edge with no bar, stays a normal
    /// rounded card.
    #[must_use]
    pub(crate) fn radii(&self, height: f32, radius: f32, y_offset: f32) -> (f32, f32) {
        if self.merges_with_bar && y_offset <= 0.0 {
            island_radii(height, radius)
        } else {
            let r = finite_clamp(radius, 0.0, 512.0, 0.0)
                .min(finite_clamp(height, 0.0, f32::MAX, 0.0) * 0.5);
            (r, r)
        }
    }
}

fn normalized_viewport(viewport: [f32; 4]) -> [f32; 4] {
    let [x, y, width, height] = viewport;
    [
        if x.is_finite() { x } else { 0.0 },
        if y.is_finite() { y } else { 0.0 },
        finite_clamp(width, 1.0, f32::MAX, 1.0),
        finite_clamp(height, 1.0, f32::MAX, 1.0),
    ]
}

/// Portion of a bar belonging to `viewport`. Clipping rather than merely
/// testing the centre also supports one bar spanning the whole virtual
/// desktop: each monitor docks to the segment actually above it.
#[must_use]
pub(crate) fn clip_bar_to_viewport(bar: [f32; 4], viewport: [f32; 4]) -> Option<[f32; 4]> {
    let [bx, by, bw, bh] = bar;
    if !bar.into_iter().all(f32::is_finite) || bw <= 0.0 || bh <= 0.0 {
        return None;
    }
    let [vx, vy, vw, vh] = normalized_viewport(viewport);
    let left = bx.max(vx);
    let top = by.max(vy);
    let right = (bx + bw).min(vx + vw);
    let bottom = (by + bh).min(vy + vh);
    (right > left && bottom > top).then_some([left, top, right - left, bottom - top])
}

/// One spring, integrated with the same discipline as the wobbly grid: a
/// clamped step, substeps sized to the spring's period, and exponential
/// damping so a large configured value cannot make it explode.
#[derive(Clone, Copy, Debug, Default)]
struct Spring {
    value: f32,
    velocity: f32,
}

impl Spring {
    fn at(value: f32) -> Self {
        Self {
            value,
            velocity: 0.0,
        }
    }

    fn advance(&mut self, target: f32, dt: f32) {
        let dt = clamp_effect_dt(dt);
        if dt <= f32::EPSILON {
            return;
        }
        let substeps = ((dt / MAX_SPRING_STEP).ceil() as usize).clamp(1, MAX_SPRING_SUBSTEPS);
        let step = dt / substeps as f32;
        let decay = (-SPRING_DAMPING * step).exp();
        for _ in 0..substeps {
            self.velocity += (target - self.value) * SPRING_STIFFNESS * step;
            self.value += self.velocity * step;
            self.velocity *= decay;
        }
        self.value = finite_clamp(self.value, 0.0, f32::MAX, target);
        self.velocity = finite_clamp(self.velocity, -1.0e6, 1.0e6, 0.0);
    }

    /// As [`Self::advance`], but for a value that is allowed to be negative —
    /// a screen coordinate rather than a size.
    fn advance_signed(&mut self, target: f32, dt: f32) {
        let dt = clamp_effect_dt(dt);
        if dt <= f32::EPSILON {
            return;
        }
        let substeps = ((dt / MAX_SPRING_STEP).ceil() as usize).clamp(1, MAX_SPRING_SUBSTEPS);
        let step = dt / substeps as f32;
        let decay = (-SPRING_DAMPING * step).exp();
        for _ in 0..substeps {
            self.velocity += (target - self.value) * SPRING_STIFFNESS * step;
            self.value += self.velocity * step;
            self.velocity *= decay;
        }
        self.value = finite_clamp(self.value, f32::MIN, f32::MAX, target);
        self.velocity = finite_clamp(self.velocity, -1.0e6, 1.0e6, 0.0);
    }

    fn settled(&self, target: f32) -> bool {
        (self.value - target).abs() < SETTLE_DISTANCE && self.velocity.abs() < SETTLE_VELOCITY
    }
}

/// The open/morph motion of one docked panel.
///
/// A panel that has never been on screen springs open from [`SEED_WIDTH`] at
/// zero height. One already on screen whose content changed size — the OSD
/// switching from a volume slider to a wider media card — springs from where
/// it is to the new size, which is the morph.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct IslandMotion {
    width: Spring,
    height: Spring,
    open: bool,
    last_tick: Option<Instant>,
}

impl IslandMotion {
    /// Advance to `now` and return the current `(width, height)`.
    ///
    /// The first frame of an appearance deliberately consumes no time: the
    /// compositor can have been idle for minutes before an OSD event, and
    /// handing the spring that interval would finish the animation before its
    /// first draw.
    #[cfg(test)]
    pub(crate) fn advance(
        &mut self,
        now: Instant,
        target_width: f32,
        target_height: f32,
    ) -> (f32, f32) {
        self.advance_with_motion(now, target_width, target_height, true)
    }

    /// Advance the spring, or place the panel directly at its target when
    /// motion is disabled.
    ///
    /// JWM's global animation switch applies to compositor-owned UI as well as
    /// client geometry.  Snapping the spring's internal state (rather than
    /// merely returning the target) also makes [`Self::animating`] false, so a
    /// reduced-motion panel does not keep requesting invisible follow-up
    /// frames.
    pub(crate) fn advance_with_motion(
        &mut self,
        now: Instant,
        target_width: f32,
        target_height: f32,
        motion_enabled: bool,
    ) -> (f32, f32) {
        let target_width = finite_clamp(target_width, 0.0, f32::MAX, 0.0);
        let target_height = finite_clamp(target_height, 0.0, f32::MAX, 0.0);

        if !motion_enabled {
            self.open = true;
            self.width = Spring::at(target_width);
            self.height = Spring::at(target_height);
            self.last_tick = Some(now);
            return (target_width, target_height);
        }

        if !self.open {
            self.open = true;
            self.width = Spring::at(SEED_WIDTH.min(target_width));
            self.height = Spring::at(0.0);
            self.last_tick = Some(now);
            return (self.width.value, self.height.value);
        }

        let dt = self.last_tick.replace(now).map_or(0.0, |last| {
            now.saturating_duration_since(last).as_secs_f32()
        });
        self.width.advance(target_width, dt);
        self.height.advance(target_height, dt);
        (self.width.value, self.height.value)
    }

    /// Forget the current geometry so the next appearance springs open again.
    pub(crate) fn close(&mut self) {
        *self = Self::default();
    }

    /// Whether the panel is still moving toward the given target.
    #[must_use]
    pub(crate) fn animating(&self, target_width: f32, target_height: f32) -> bool {
        self.open && !(self.width.settled(target_width) && self.height.settled(target_height))
    }
}

/// The fade-in envelope a modal surface's dim rides while its panel opens:
/// the eased square of how far the open spring has travelled toward the
/// panel's final width.
///
/// opened² rather than opened — the dim lingers low while the panel is still
/// a seed and catches up quickly as the spring arrives, and the clamp keeps
/// the spring's slight overshoot from pushing the alpha past full. The
/// docked card's contents and scrim introduced the curve; the layout
/// filmstrip and the tags grid, which never spring their own geometry, dim
/// their scrims with the very same one. It lives here rather than in a
/// renderer because all three panels and both compositors must agree on it
/// exactly.
#[must_use]
pub(crate) fn open_envelope(spring_width: f32, panel_width: f32) -> f32 {
    let opened = (spring_width / panel_width.max(1.0)).clamp(0.0, 1.0);
    opened * opened
}

/// The selection pill's travel between rows of a list panel.
///
/// The pill used to be placed straight from the selected index, so it
/// teleported: press Down and it is simply somewhere else. Sliding it is what
/// makes a list read as one object being moved through rather than a set of
/// rows taking turns being lit.
///
/// It deliberately does *not* slide on its first appearance, or when the list
/// underneath it changes identity — sliding in from a row of a different
/// panel would be motion that describes nothing.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RowHighlight {
    y: Spring,
    height: Spring,
    shown: bool,
    last_tick: Option<Instant>,
}

impl RowHighlight {
    /// Advance to `now` and return the pill to draw for `target`
    /// (`[x, y, w, h]`). Only the vertical axis moves: the pill always spans
    /// the card's content width, so animating x or width would be motion the
    /// user cannot see.
    #[cfg(test)]
    pub(crate) fn advance(&mut self, now: Instant, target: [f32; 4]) -> [f32; 4] {
        self.advance_with_motion(now, target, true)
    }

    /// Advance the highlight, or place it immediately when motion is
    /// disabled. As with [`IslandMotion::advance_with_motion`], snapping the
    /// stored springs prevents needless animation frames after the global
    /// animation switch is turned off at runtime.
    pub(crate) fn advance_with_motion(
        &mut self,
        now: Instant,
        target: [f32; 4],
        motion_enabled: bool,
    ) -> [f32; 4] {
        let [x, y, w, h] = target;
        let y = finite_clamp(y, f32::MIN, f32::MAX, 0.0);
        let h = finite_clamp(h, 0.0, f32::MAX, 0.0);

        if !motion_enabled {
            self.shown = true;
            self.y = Spring::at(y);
            self.height = Spring::at(h);
            self.last_tick = Some(now);
            return [x, y, w, h];
        }

        if !self.shown {
            self.shown = true;
            self.y = Spring::at(y);
            self.height = Spring::at(h);
            self.last_tick = Some(now);
            return [x, y, w, h];
        }

        let dt = self.last_tick.replace(now).map_or(0.0, |last| {
            now.saturating_duration_since(last).as_secs_f32()
        });
        // The springs clamp their value at zero, which is right for a size but
        // wrong for a screen coordinate, so y travels as an offset from the
        // target and is added back.
        self.y.advance_signed(y, dt);
        self.height.advance(h, dt);
        [x, self.y.value, w, self.height.value]
    }

    /// Whether the pill is still travelling toward `target`.
    #[must_use]
    pub(crate) fn animating(&self, target: [f32; 4]) -> bool {
        self.shown && !(self.y.settled(target[1]) && self.height.settled(target[3]))
    }

    /// Forget where the pill was, so the next one appears where it belongs
    /// instead of sliding in from another panel's row.
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Ease-out curve for the hover envelope: the cue reads immediately and then
/// settles, rather than creeping up on the pointer.
fn hover_ease_out(t: f32) -> f32 {
    let t = finite_clamp(t, 0.0, 1.0, 0.0);
    1.0 - (1.0 - t) * (1.0 - t)
}

/// Fade-in envelope for a pointer-hover cue, keyed to the hovered target.
///
/// Pointer hover used to flip on in a single frame while the keyboard's
/// selection pill springs smoothly beside it, so one panel read as two kinds
/// of motion at once. This is the hover half of that bargain: when the
/// hovered target (a list row, an expose cell, a tab) appears or changes, the
/// cue eases in over [`HOVER_EASE_IN`]; when the hover leaves, the cue is
/// gone the same frame — JWM draws no fade-outs.
///
/// A key change restarts the envelope from zero rather than travelling from
/// the old target: the cue is a property of what is under the pointer, not a
/// thing that moves. Both compositors advance through this one type so the
/// two backends cannot drift apart on any of it.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct HoverEase<K> {
    key: Option<K>,
    progress: f32,
    last_tick: Option<Instant>,
}

impl<K: Copy + PartialEq> HoverEase<K> {
    /// Advance to `now` for the currently hovered `key` and return the eased
    /// `0.0..=1.0` strength the cue should draw with.
    #[cfg(test)]
    pub(crate) fn advance(&mut self, now: Instant, hover: Option<K>) -> f32 {
        self.advance_with_motion(now, hover, true)
    }

    /// Advance the envelope, or snap it when motion is disabled.
    ///
    /// `None` — the hover left — clears the envelope outright: the cue
    /// disappears on the very frame that reports it, like every other cue in
    /// the compositor. With motion off a present hover draws at full strength
    /// immediately and [`Self::animating`] stays false, so a reduced-motion
    /// desktop renders one frame and no more.
    pub(crate) fn advance_with_motion(
        &mut self,
        now: Instant,
        hover: Option<K>,
        motion_enabled: bool,
    ) -> f32 {
        let Some(key) = hover else {
            self.clear();
            return 0.0;
        };
        if self.key != Some(key) {
            // A new target starts the envelope over. The first frame consumes
            // no time — the discipline `IslandMotion` keeps — so a long idle
            // gap before it cannot finish the fade before it is drawn.
            self.key = Some(key);
            self.progress = if motion_enabled { 0.0 } else { 1.0 };
            self.last_tick = Some(now);
            return hover_ease_out(self.progress);
        }
        if !motion_enabled {
            // Snap the stored envelope as well as the returned value, so
            // `animating` is false and no invisible follow-up frames tick.
            self.progress = 1.0;
            self.last_tick = Some(now);
            return 1.0;
        }
        let dt = self.last_tick.replace(now).map_or(0.0, |last| {
            now.saturating_duration_since(last).as_secs_f32()
        });
        let dt = clamp_effect_dt(dt);
        self.progress = finite_clamp(self.progress + dt / HOVER_EASE_IN, 0.0, 1.0, 1.0);
        hover_ease_out(self.progress)
    }

    /// Whether the envelope is still easing toward full strength.
    #[must_use]
    pub(crate) fn animating(&self) -> bool {
        self.key.is_some() && self.progress < 1.0
    }

    /// Forget the current target, so the next hover eases in from nothing
    /// instead of resuming where a previous surface left off.
    pub(crate) fn clear(&mut self) {
        self.key = None;
        self.progress = 0.0;
        self.last_tick = None;
    }
}

/// Corner radii for a docked panel: square where it meets the bar, rounded
/// below.
///
/// The bottom radius is capped at half the current height so a panel that is
/// still springing open stays a capsule instead of inverting its own corners.
#[must_use]
pub(crate) fn island_radii(height: f32, radius: f32) -> (f32, f32) {
    let radius = finite_clamp(radius, 0.0, 512.0, 0.0);
    let height = finite_clamp(height, 0.0, f32::MAX, 0.0);
    (0.0, radius.min(height * 0.5))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const FRAME: Duration = Duration::from_micros(16_667);

    #[test]
    fn a_panel_docks_on_the_bar_not_the_screen() {
        // An inset bar: centring on the screen would leave the panel visibly
        // off from the strip it is supposed to grow out of.
        let dock = IslandDock::for_bar(Some([40.0, 5.0, 2400.0, 42.0]), [0.0, 0.0, 2560.0, 1440.0]);
        assert_eq!(dock.centre_x, 1240.0);
        assert_eq!(dock.top_y, 47.0);

        let rect = dock.rect(360.0, 64.0, 0.0);
        assert_eq!(rect, [1060.0, 47.0, 360.0, 64.0]);
    }

    #[test]
    fn without_a_bar_a_panel_hangs_from_the_top_of_the_screen() {
        for bar in [
            None,
            Some([0.0, 0.0, 0.0, 0.0]),
            Some([0.0, 0.0, 100.0, 0.0]),
        ] {
            let dock = IslandDock::for_bar(bar, [0.0, 0.0, 1600.0, 900.0]);
            assert_eq!(dock.centre_x, 800.0);
            assert_eq!(dock.top_y, NO_BAR_TOP_MARGIN);
        }
    }

    #[test]
    fn a_negative_origin_viewport_uses_only_its_bar_and_fallback_space() {
        let viewport = [-1920.0, 120.0, 1920.0, 1080.0];
        let spanning = [-1920.0, 120.0, 3840.0, 40.0];
        assert_eq!(
            clip_bar_to_viewport(spanning, viewport),
            Some([-1920.0, 120.0, 1920.0, 40.0])
        );
        assert_eq!(
            clip_bar_to_viewport([0.0, 0.0, 1920.0, 40.0], viewport),
            None,
            "the other monitor's bar must not become this monitor's dock"
        );

        let dock = IslandDock::for_bar(Some(spanning), viewport);
        assert_eq!(dock.centre_x, -960.0);
        assert_eq!(dock.top_y, 160.0);
        assert_eq!(
            dock.contained_rect(480.0, 120.0, 0.0),
            [-1200.0, 160.0, 480.0, 120.0]
        );

        let fallback = IslandDock::for_bar(None, viewport);
        assert_eq!(fallback.centre_x, -960.0);
        assert_eq!(fallback.top_y, 132.0);
        let bounded = fallback.contained_rect(600.0, 200.0, 5000.0);
        assert_eq!(bounded, [-1260.0, 1000.0, 600.0, 200.0]);
    }

    #[test]
    fn a_new_panel_springs_open_from_a_seed_rather_than_appearing() {
        let mut motion = IslandMotion::default();
        let start = Instant::now();

        let (w, h) = motion.advance(start, 360.0, 64.0);
        assert_eq!((w, h), (SEED_WIDTH, 0.0), "first frame is the seed");

        // A few frames in it is on its way, not there yet.
        let mid = motion.advance(start + FRAME * 4, 360.0, 64.0);
        assert!(mid.0 > SEED_WIDTH && mid.0 < 360.0, "width {}", mid.0);
        assert!(mid.1 > 0.0 && mid.1 < 64.0, "height {}", mid.1);
        assert!(motion.animating(360.0, 64.0));
    }

    #[test]
    fn the_spring_settles_on_its_target_within_a_second() {
        let mut motion = IslandMotion::default();
        let mut t = Instant::now();
        motion.advance(t, 360.0, 64.0);

        let mut frames = 0;
        while motion.animating(360.0, 64.0) && frames < 60 {
            t += FRAME;
            motion.advance(t, 360.0, 64.0);
            frames += 1;
        }
        assert!(frames < 60, "still moving after {frames} frames");

        let (w, h) = motion.advance(t, 360.0, 64.0);
        assert!((w - 360.0).abs() < SETTLE_DISTANCE, "width {w}");
        assert!((h - 64.0).abs() < SETTLE_DISTANCE, "height {h}");
    }

    #[test]
    fn a_content_change_morphs_from_the_current_width() {
        // Open a narrow volume card, let it settle, then hand it the wider
        // media card: it must travel from where it is, not restart from the
        // seed, which would read as a second panel replacing the first.
        let mut motion = IslandMotion::default();
        let mut t = Instant::now();
        motion.advance(t, 360.0, 64.0);
        for _ in 0..60 {
            t += FRAME;
            motion.advance(t, 360.0, 64.0);
        }

        t += FRAME;
        let (w, _) = motion.advance(t, 520.0, 64.0);
        assert!(w > 360.0 && w < 520.0, "morph started at {w}");
        assert!(motion.animating(520.0, 64.0));
    }

    #[test]
    fn a_closed_panel_springs_open_again_next_time() {
        let mut motion = IslandMotion::default();
        let mut t = Instant::now();
        motion.advance(t, 360.0, 64.0);
        for _ in 0..60 {
            t += FRAME;
            motion.advance(t, 360.0, 64.0);
        }
        motion.close();

        t += FRAME;
        assert_eq!(motion.advance(t, 360.0, 64.0), (SEED_WIDTH, 0.0));
    }

    #[test]
    fn disabled_motion_places_a_panel_without_requesting_more_frames() {
        let mut motion = IslandMotion::default();
        let now = Instant::now();
        assert_eq!(
            motion.advance_with_motion(now, 360.0, 64.0, false),
            (360.0, 64.0)
        );
        assert!(!motion.animating(360.0, 64.0));

        // Turning motion off while a spring is already travelling also snaps
        // its stored state, rather than hiding an animation that keeps ticking.
        motion.advance_with_motion(now + FRAME, 520.0, 80.0, true);
        assert!(motion.animating(520.0, 80.0));
        assert_eq!(
            motion.advance_with_motion(now + FRAME * 2, 520.0, 80.0, false),
            (520.0, 80.0)
        );
        assert!(!motion.animating(520.0, 80.0));
    }

    #[test]
    fn a_long_idle_gap_does_not_finish_the_animation_before_it_is_drawn() {
        let mut motion = IslandMotion::default();
        let start = Instant::now();
        motion.advance(start, 360.0, 64.0);

        // The compositor was asleep for a minute between the first and second
        // frame. The step is clamped, so the panel is still near its seed.
        let (w, _) = motion.advance(start + Duration::from_secs(60), 360.0, 64.0);
        assert!(w < 360.0, "a stall finished the animation: {w}");
    }

    #[test]
    fn only_a_panel_that_touches_the_bar_squares_off_against_it() {
        let viewport = [0.0, 0.0, 1600.0, 900.0];
        let docked = IslandDock::for_bar(Some([0.0, 0.0, 1600.0, 40.0]), viewport);
        // Touching the bar: flat above, curved below.
        assert_eq!(docked.radii(64.0, 24.0, 0.0), (0.0, 24.0));
        // Stacked below another card: nothing above to merge with.
        assert_eq!(docked.radii(64.0, 24.0, 76.0), (24.0, 24.0));

        // No bar at all: a flat-topped card hanging in open space would just
        // look like a corner-radius bug.
        let floating = IslandDock::for_bar(None, viewport);
        assert_eq!(floating.radii(64.0, 24.0, 0.0), (24.0, 24.0));
        // Still capped so a card mid-open stays a capsule.
        assert_eq!(floating.radii(20.0, 24.0, 0.0), (10.0, 10.0));
    }

    #[test]
    fn the_selection_pill_appears_where_it_belongs_and_then_slides() {
        let mut pill = RowHighlight::default();
        let mut t = Instant::now();
        let row = |i: f32| [100.0, 200.0 + i * 24.0, 400.0, 24.0];

        // First appearance is placed, not animated: there is nowhere to slide
        // from.
        assert_eq!(pill.advance(t, row(0.0)), row(0.0));
        assert!(!pill.animating(row(0.0)));

        // Moving down starts a travel that has not finished on the next frame.
        t += FRAME;
        let mid = pill.advance(t, row(4.0));
        assert!(mid[1] > row(0.0)[1] && mid[1] < row(4.0)[1], "y {}", mid[1]);
        assert!(pill.animating(row(4.0)));

        let mut frames = 0;
        while pill.animating(row(4.0)) && frames < 60 {
            t += FRAME;
            pill.advance(t, row(4.0));
            frames += 1;
        }
        assert!(frames < 60, "still moving after {frames} frames");
        assert!((pill.advance(t, row(4.0))[1] - row(4.0)[1]).abs() < SETTLE_DISTANCE);
    }

    #[test]
    fn disabled_motion_places_the_selection_without_a_follow_up_frame() {
        let mut pill = RowHighlight::default();
        let now = Instant::now();
        let first = [100.0, 200.0, 400.0, 24.0];
        let target = [100.0, 480.0, 400.0, 30.0];
        pill.advance(now, first);

        assert_eq!(pill.advance_with_motion(now + FRAME, target, false), target);
        assert!(!pill.animating(target));
    }

    #[test]
    fn the_selection_pill_keeps_the_cards_own_x_and_width() {
        // Only the vertical axis is sprung; the pill always spans the card.
        let mut pill = RowHighlight::default();
        let mut t = Instant::now();
        pill.advance(t, [100.0, 200.0, 400.0, 24.0]);
        t += FRAME;
        let drawn = pill.advance(t, [140.0, 320.0, 520.0, 24.0]);
        assert_eq!(drawn[0], 140.0);
        assert_eq!(drawn[2], 520.0);
    }

    #[test]
    fn a_reset_pill_does_not_slide_in_from_another_panels_row() {
        let mut pill = RowHighlight::default();
        let mut t = Instant::now();
        pill.advance(t, [0.0, 900.0, 400.0, 24.0]);
        pill.reset();
        t += FRAME;
        assert_eq!(
            pill.advance(t, [0.0, 120.0, 400.0, 24.0]),
            [0.0, 120.0, 400.0, 24.0]
        );
    }

    #[test]
    fn the_selection_pill_travels_upward_too() {
        // The springs used by the panel clamp at zero, which is right for a
        // size and wrong for a coordinate. A pill moving to a smaller y must
        // still arrive.
        let mut pill = RowHighlight::default();
        let mut t = Instant::now();
        pill.advance(t, [0.0, 600.0, 400.0, 24.0]);
        for _ in 0..60 {
            t += FRAME;
            pill.advance(t, [0.0, 40.0, 400.0, 24.0]);
        }
        assert!((pill.advance(t, [0.0, 40.0, 400.0, 24.0])[1] - 40.0).abs() < SETTLE_DISTANCE);
    }

    #[test]
    fn an_extreme_selection_target_stays_finite() {
        let mut pill = RowHighlight::default();
        let mut t = Instant::now();
        pill.advance(t, [0.0, f32::NAN, 400.0, f32::INFINITY]);
        for _ in 0..200 {
            t += FRAME;
            let drawn = pill.advance(t, [0.0, 1.0e9, 400.0, -5.0]);
            assert!(drawn[1].is_finite() && drawn[3].is_finite(), "{drawn:?}");
            assert!(drawn[3] >= 0.0);
        }
    }

    #[test]
    fn corners_stay_square_on_top_and_never_invert_while_opening() {
        assert_eq!(island_radii(64.0, 24.0), (0.0, 24.0));
        // Mid-open the card is shorter than the radius: it stays a capsule.
        assert_eq!(island_radii(20.0, 24.0), (0.0, 10.0));
        assert_eq!(island_radii(0.0, 24.0), (0.0, 0.0));
        assert_eq!(island_radii(f32::NAN, f32::NAN), (0.0, 0.0));
    }

    #[test]
    fn the_open_envelope_is_the_square_of_the_springs_travel() {
        assert_eq!(open_envelope(0.0, 400.0), 0.0);
        // Halfway open is a quarter dim: the fade lags the spring early and
        // catches up as it arrives.
        assert_eq!(open_envelope(200.0, 400.0), 0.25);
        assert_eq!(open_envelope(400.0, 400.0), 1.0);
        // The spring overshoots its target slightly; the dim must not follow
        // it past full alpha.
        assert_eq!(open_envelope(460.0, 400.0), 1.0);
    }

    #[test]
    fn the_open_envelope_stays_finite_for_a_degenerate_panel() {
        // The width floor keeps a zero-width target from dividing by zero;
        // any real spring width then reads as fully open.
        assert_eq!(open_envelope(0.0, 0.0), 0.0);
        assert_eq!(open_envelope(50.0, 0.0), 1.0);
        // A NaN panel width is floored away the same way; only a NaN spring
        // width could still leak through, and the springs never produce one.
        assert_eq!(open_envelope(50.0, f32::NAN), 1.0);
        assert!(open_envelope(f32::NAN, 400.0).is_nan());
    }

    #[test]
    fn a_panels_scrim_fades_in_on_the_same_curve_the_card_opens_with() {
        // The filmstrip and the tags grid advance the system-UI island toward
        // their own panel's size and dim with this envelope, so a fresh
        // appearance starts at the seed's share of the panel, squared.
        let mut motion = IslandMotion::default();
        let mut t = Instant::now();
        let (w, _) = motion.advance(t, 1200.0, 300.0);
        let first = open_envelope(w, 1200.0);
        assert_eq!(first, (SEED_WIDTH / 1200.0) * (SEED_WIDTH / 1200.0));

        t += FRAME;
        let (w, _) = motion.advance(t, 1200.0, 300.0);
        let mid = open_envelope(w, 1200.0);
        assert!(mid > first && mid < 1.0, "envelope {mid}");
        assert!(motion.animating(1200.0, 300.0));

        let mut frames = 0;
        while motion.animating(1200.0, 300.0) && frames < 60 {
            t += FRAME;
            motion.advance(t, 1200.0, 300.0);
            frames += 1;
        }
        assert!(frames < 60, "still moving after {frames} frames");
        let (w, _) = motion.advance(t, 1200.0, 300.0);
        assert!(open_envelope(w, 1200.0) > 0.99, "settled dim");

        // Closing forgets the travel, so the next appearance fades in again
        // rather than resuming at full dim.
        motion.close();
        t += FRAME;
        let (w, _) = motion.advance(t, 1200.0, 300.0);
        assert_eq!(open_envelope(w, 1200.0), first);
    }

    #[test]
    fn disabled_motion_makes_the_open_envelope_exactly_one() {
        let mut motion = IslandMotion::default();
        let now = Instant::now();
        let (w, _) = motion.advance_with_motion(now, 1200.0, 300.0, false);
        assert_eq!(open_envelope(w, 1200.0), 1.0);
        assert!(!motion.animating(1200.0, 300.0));

        // Turned off mid-travel the spring snaps, so the dim jumps to full
        // and no follow-up frames are requested.
        motion.close();
        let mut t = now;
        motion.advance_with_motion(t, 1200.0, 300.0, true);
        t += FRAME;
        motion.advance_with_motion(t, 1200.0, 300.0, true);
        assert!(motion.animating(1200.0, 300.0));
        t += FRAME;
        let (w, _) = motion.advance_with_motion(t, 1200.0, 300.0, false);
        assert_eq!(open_envelope(w, 1200.0), 1.0);
        assert!(!motion.animating(1200.0, 300.0));
    }

    #[test]
    fn extreme_targets_stay_finite() {
        let mut motion = IslandMotion::default();
        let mut t = Instant::now();
        motion.advance(t, f32::NAN, f32::INFINITY);
        for _ in 0..200 {
            t += FRAME;
            let (w, h) = motion.advance(t, 1.0e9, -5.0);
            assert!(w.is_finite() && h.is_finite(), "{w} {h}");
            assert!(h >= 0.0, "height went negative: {h}");
        }
    }

    #[test]
    fn a_hover_cue_eases_in_monotonically_and_settles_at_full_strength() {
        let mut ease = HoverEase::default();
        let mut t = Instant::now();

        // The first frame of a new target consumes no time: the cue is not
        // drawn yet, as with the island's seed frame.
        assert_eq!(ease.advance(t, Some(3usize)), 0.0);
        assert!(ease.animating());

        let mut last = 0.0;
        let mut frames = 0;
        while ease.animating() && frames < 60 {
            t += FRAME;
            let p = ease.advance(t, Some(3));
            assert!(
                p > last,
                "strength must grow monotonically: {p} after {last}"
            );
            last = p;
            frames += 1;
        }
        assert!(frames < 60, "still easing after {frames} frames");
        assert_eq!(ease.advance(t, Some(3)), 1.0);
        assert!(!ease.animating());
    }

    #[test]
    fn the_hover_envelope_reads_immediately_and_settles_gently() {
        // Ease-out: halfway through the fade the cue is already three
        // quarters drawn.
        assert_eq!(hover_ease_out(0.0), 0.0);
        assert_eq!(hover_ease_out(0.5), 0.75);
        assert_eq!(hover_ease_out(1.0), 1.0);
        // Stray out-of-range progress cannot push a cue past full strength,
        // and a NaN reads as "nothing to draw" rather than leaking through.
        assert_eq!(hover_ease_out(1.2), 1.0);
        assert_eq!(hover_ease_out(f32::NAN), 0.0);
    }

    #[test]
    fn disabled_motion_draws_hover_at_full_strength_without_follow_up_frames() {
        let mut ease = HoverEase::default();
        let now = Instant::now();
        assert_eq!(ease.advance_with_motion(now, Some(2usize), false), 1.0);
        assert!(!ease.animating());

        // Turning motion off mid-fade snaps the stored envelope too, so no
        // invisible animation keeps asking for frames.
        let mut t = now;
        ease.advance_with_motion(t, Some(5usize), true);
        t += FRAME;
        ease.advance_with_motion(t, Some(5), true);
        assert!(ease.animating());
        t += FRAME;
        assert_eq!(ease.advance_with_motion(t, Some(5), false), 1.0);
        assert!(!ease.animating());
    }

    #[test]
    fn a_departing_hover_clears_the_cue_the_same_frame() {
        let mut ease = HoverEase::default();
        let mut t = Instant::now();
        ease.advance(t, Some(1usize));
        t += FRAME;
        let p = ease.advance(t, Some(1));
        assert!(p > 0.0 && p < 1.0);

        // No fade-out anywhere in the compositor: the frame that reports the
        // leave draws nothing.
        t += FRAME;
        assert_eq!(ease.advance(t, None), 0.0);
        assert!(!ease.animating());

        // Re-hovering the same target starts over from nothing, not from
        // where the departed cue left off.
        t += FRAME;
        assert_eq!(ease.advance(t, Some(1)), 0.0);
        assert!(ease.animating());
    }

    #[test]
    fn moving_to_another_target_restarts_the_envelope_there() {
        let mut ease = HoverEase::default();
        let mut t = Instant::now();
        ease.advance(t, Some(0usize));
        for _ in 0..60 {
            t += FRAME;
            ease.advance(t, Some(0));
        }
        assert!(!ease.animating());

        t += FRAME;
        assert_eq!(
            ease.advance(t, Some(1)),
            0.0,
            "the cue eases in where the pointer is rather than travelling"
        );
        assert!(ease.animating());
    }

    #[test]
    fn a_stall_cannot_finish_the_hover_fade_before_it_is_drawn() {
        let mut ease = HoverEase::default();
        let start = Instant::now();
        ease.advance(start, Some(7usize));

        // The compositor was asleep for a minute; the clamped step keeps the
        // cue near its start instead of completing unseen.
        let p = ease.advance(start + Duration::from_secs(60), Some(7));
        assert!(p < 1.0, "a stall finished the fade: {p}");
        assert!(ease.animating());
    }

    #[test]
    fn degenerate_hover_ticks_stay_finite() {
        let mut ease = HoverEase::default();
        let t = Instant::now();
        // Zero-dt ticks (the same instant twice) neither advance nor divide
        // by zero.
        assert_eq!(ease.advance(t, Some(0usize)), 0.0);
        assert_eq!(ease.advance(t, Some(0)), 0.0);
        assert!(ease.animating());
        for i in 0..200 {
            let p = ease.advance(t + FRAME * i, Some(0));
            assert!((0.0..=1.0).contains(&p), "{p}");
        }
        assert_eq!(ease.advance(t + FRAME * 200, Some(0)), 1.0);
    }
}
