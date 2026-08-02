//! Protocol-independent helpers for time-based compositor effects.
//!
//! Render loops are not guaranteed to run at 60 Hz: a display can refresh at
//! another rate and a compositor can resume after being idle for an arbitrary
//! amount of time.  Keep the small pieces of time normalization used by both
//! compositor backends here so effects advance consistently and cannot explode
//! after a long frame.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Largest simulation step accepted by inexpensive visual effects.
///
/// Dropping excess elapsed time after a stall is preferable to a particle or
/// spring system jumping across the screen on the first frame after resume.
pub const MAX_EFFECT_FRAME_DT: f32 = 0.05;

/// Maximum wobbly-window mesh supported by the shaders (15 × 15 nodes).
///
/// 225 vec2 uniforms plus the other vertex uniforms remain below GLES 3's
/// minimum guarantee of 256 vertex-uniform vectors.
pub const MAX_WOBBLY_SUBDIVISIONS: u32 = 14;

/// Defensive CPU/GPU work limit for one close-particle burst.
pub const MAX_PARTICLES_PER_BURST: u32 = 4096;

/// Bound the number of concurrent close bursts after rapid window teardown.
pub const MAX_PARTICLE_SYSTEMS: usize = 8;

/// Defensive history limit for motion-trail ghost draws.
pub const MAX_MOTION_TRAIL_SAMPLES: u32 = 64;

/// How much smaller the oldest ghost is drawn than the live window.
///
/// Identically sized ghosts stacked on a slow drag accumulate into one solid
/// dark rectangle. Receding them slightly keeps the tail readable as motion.
const GHOST_SHRINK: f32 = 0.12;

/// One historical window position used by the drag motion-trail effect.
#[derive(Clone, Copy, Debug)]
pub struct MotionTrailSample {
    pub x: i32,
    pub y: i32,
    created_at: Instant,
}

impl MotionTrailSample {
    #[inline]
    pub fn new(x: i32, y: i32) -> Self {
        Self {
            x,
            y,
            created_at: Instant::now(),
        }
    }

    #[inline]
    pub fn opacity_at(self, now: Instant, lifetime: Duration) -> f32 {
        if lifetime.is_zero() {
            return 0.0;
        }
        let age = now.saturating_duration_since(self.created_at).as_secs_f32();
        (1.0 - age / lifetime.as_secs_f32()).clamp(0.0, 1.0)
    }
}

/// Tuning for one window's motion trail, derived from config plus its size.
#[derive(Clone, Copy, Debug)]
pub struct MotionTrailParams {
    capacity: usize,
    lifetime: Duration,
    spacing: f32,
    base_opacity: f32,
}

impl MotionTrailParams {
    pub fn new(frames: u32, opacity: f32, width: f32, height: f32) -> Self {
        Self {
            capacity: motion_trail_capacity(frames),
            lifetime: motion_trail_lifetime(frames),
            spacing: motion_trail_spacing(width, height),
            base_opacity: finite_clamp(opacity, 0.0, 1.0, 0.3),
        }
    }

    #[inline]
    pub fn draws_nothing(&self) -> bool {
        self.capacity == 0 || self.base_opacity <= 0.0
    }

    #[inline]
    pub fn lifetime(&self) -> Duration {
        self.lifetime
    }
}

/// One ghost draw, already positioned and sized for the render pass.
#[derive(Clone, Copy, Debug)]
pub struct MotionGhost {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub opacity: f32,
}

/// Ring buffer of historical window positions, sampled by distance travelled.
///
/// Recording a sample per input event ties the trail to the pointer's polling
/// rate: a 1000 Hz mouse fills the whole history within a few milliseconds of
/// travel, so the ghosts pile up underneath the window as one dark blob
/// instead of trailing behind it, while a slow drag spaces them a pixel apart
/// and produces the same blob for the opposite reason. Sampling once per
/// distance interval makes the trail describe how far the window moved, which
/// is what the effect is meant to show, and caps the ghost overdraw at a few
/// well-separated copies.
#[derive(Debug, Default)]
pub struct MotionTrail {
    samples: VecDeque<MotionTrailSample>,
    /// Logical drag position advanced by move deltas. A configure event can
    /// arrive before or after the move hook, so deriving the previous position
    /// from the window rect would intermittently duplicate or skip samples.
    cursor: Option<(f32, f32)>,
}

impl MotionTrail {
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    #[inline]
    pub fn clear(&mut self) {
        self.samples.clear();
        self.cursor = None;
    }

    /// Start a drag from `position`, dropping any history from the last one.
    #[inline]
    pub fn begin_drag(&mut self, x: f32, y: f32) {
        self.samples.clear();
        self.cursor = Some((x, y));
    }

    /// Stop tracking, leaving the recorded ghosts to fade out on their own.
    #[inline]
    pub fn end_drag(&mut self) {
        self.cursor = None;
    }

    /// Follow a window that moved without recording anything.
    #[inline]
    pub fn sync_position(&mut self, x: f32, y: f32) {
        self.cursor = Some((x, y));
    }

    /// Record a move expressed as a delta, as the X11 move hook reports it.
    ///
    /// `fallback` is the current window origin, used to seed the logical
    /// cursor when a drag was not announced.
    pub fn record_delta(
        &mut self,
        dx: f32,
        dy: f32,
        fallback: (f32, f32),
        params: &MotionTrailParams,
    ) {
        if params.draws_nothing() {
            self.clear();
            return;
        }
        let (previous_x, previous_y) = self.cursor.unwrap_or(fallback);
        self.cursor = Some((previous_x + dx, previous_y + dy));
        self.push_spaced(previous_x, previous_y, params);
    }

    /// Record a move expressed as an absolute origin, as the Wayland scene
    /// reports it. The position the window is leaving becomes the ghost.
    pub fn record_position(&mut self, x: f32, y: f32, params: &MotionTrailParams) {
        if params.draws_nothing() {
            self.clear();
            return;
        }
        if let Some((previous_x, previous_y)) = self.cursor.replace((x, y))
            && (previous_x, previous_y) != (x, y)
        {
            self.push_spaced(previous_x, previous_y, params);
        }
    }

    fn push_spaced(&mut self, x: f32, y: f32, params: &MotionTrailParams) {
        let x = finite_clamp(x, i32::MIN as f32, i32::MAX as f32, 0.0).round() as i32;
        let y = finite_clamp(y, i32::MIN as f32, i32::MAX as f32, 0.0).round() as i32;
        if let Some(last) = self.samples.back() {
            let dx = (x - last.x) as f32;
            let dy = (y - last.y) as f32;
            if dx * dx + dy * dy < params.spacing * params.spacing {
                return;
            }
        }
        self.samples.push_back(MotionTrailSample::new(x, y));
        while self.samples.len() > params.capacity {
            self.samples.pop_front();
        }
    }

    /// Drop faded-out ghosts. Returns whether any are still visible.
    pub fn retain_live(&mut self, now: Instant, lifetime: Duration) -> bool {
        self.samples
            .retain(|sample| sample.opacity_at(now, lifetime) > 0.0);
        !self.samples.is_empty()
    }

    /// Ghost draws for a window of `width` x `height`, oldest first.
    pub fn ghosts(
        &self,
        now: Instant,
        params: &MotionTrailParams,
        width: f32,
        height: f32,
    ) -> impl Iterator<Item = MotionGhost> + '_ {
        let MotionTrailParams {
            lifetime,
            base_opacity,
            ..
        } = *params;
        self.samples.iter().filter_map(move |sample| {
            let fade = sample.opacity_at(now, lifetime);
            // Age alone drives the fade. Weighting by buffer index as well
            // made the trail's length depend on how many samples happened to
            // be buffered rather than on how long ago they were recorded.
            let opacity = base_opacity * fade * fade;
            if opacity <= 0.001 {
                return None;
            }
            let scale = 1.0 - (1.0 - fade) * GHOST_SHRINK;
            let ghost_w = width * scale;
            let ghost_h = height * scale;
            Some(MotionGhost {
                x: sample.x as f32 + (width - ghost_w) * 0.5,
                y: sample.y as f32 + (height - ghost_h) * 0.5,
                width: ghost_w,
                height: ghost_h,
                opacity,
            })
        })
    }
}

/// Clamp elapsed frame time to the range effects can safely integrate.
#[inline]
pub fn clamp_effect_dt(dt: f32) -> f32 {
    if dt.is_finite() {
        dt.clamp(0.0, MAX_EFFECT_FRAME_DT)
    } else {
        0.0
    }
}

/// Sanitize wall-clock time used to advance non-physical animations.
///
/// Unlike [`clamp_effect_dt`], this deliberately does not cap long frames:
/// fades and normalized timelines should catch up to elapsed wall time after a
/// stall. Spring, particle, and other numerical simulations should continue to
/// use [`clamp_effect_dt`] (or fixed substeps) instead.
#[inline]
pub fn sanitize_animation_dt(dt: f32) -> f32 {
    if dt.is_finite() { dt.max(0.0) } else { 0.0 }
}

/// Advance an effect only when it already existed on the preceding frame.
///
/// A compositor can be idle for an arbitrary time before a map/unmap event
/// creates a new effect. Giving that effect the whole idle interval would make
/// it finish before its first draw.
#[inline]
pub fn continuing_effect_dt(was_active: bool, frame_dt: f32) -> f32 {
    if was_active {
        sanitize_animation_dt(frame_dt)
    } else {
        0.0
    }
}

/// Sanitize a floating-point effect parameter before it reaches simulation or
/// shader code. `f32::clamp` deliberately preserves NaN, which would otherwise
/// keep animations alive forever and poison generated vertices.
#[inline]
pub fn finite_clamp(value: f32, min: f32, max: f32, fallback: f32) -> f32 {
    debug_assert!(min.is_finite() && max.is_finite() && min <= max);
    if value.is_finite() {
        value.clamp(min, max)
    } else {
        fallback.clamp(min, max)
    }
}

/// Frame-rate-independent interpolation coefficient for exponential easing.
#[inline]
pub fn smoothing_alpha(rate: f32, dt: f32) -> f32 {
    if !rate.is_finite() || rate <= 0.0 {
        return 0.0;
    }
    1.0 - (-rate * sanitize_animation_dt(dt)).exp()
}

/// Advance a normalized animation progress value using a duration in seconds.
#[inline]
pub fn advance_progress(progress: f32, dt: f32, duration_secs: f32) -> f32 {
    if !duration_secs.is_finite() || duration_secs <= 0.0 {
        return 1.0;
    }
    let progress = finite_clamp(progress, 0.0, 1.0, 0.0);
    (progress + sanitize_animation_dt(dt) / duration_secs).clamp(0.0, 1.0)
}

/// Convert configured wobbly subdivisions into the shader's node count.
#[inline]
pub fn wobbly_node_count(subdivisions: u32) -> usize {
    subdivisions.clamp(1, MAX_WOBBLY_SUBDIVISIONS) as usize + 1
}

/// Clamp a configured motion-trail length to the render-cost limit.
#[inline]
pub fn motion_trail_capacity(samples: u32) -> usize {
    samples.min(MAX_MOTION_TRAIL_SAMPLES) as usize
}

/// Wall-clock lifetime corresponding to the configured history length.
///
/// The bounds matter more than the ratio: below roughly a seventh of a second
/// a ghost is gone before the eye registers it, and much past half a second
/// the trail lingers behind a window that already stopped.
#[inline]
pub fn motion_trail_lifetime(samples: u32) -> Duration {
    let frames = motion_trail_capacity(samples).max(1) as f32;
    Duration::from_secs_f32((frames / 20.0).clamp(0.15, 0.7))
}

/// Distance a window must travel between two recorded trail samples.
///
/// Scaled to the window so the ghosts overlap by a similar fraction whatever
/// the size: a large window needs a long stride to look like a trail, a small
/// one would be smeared across the screen by the same stride.
///
/// Together with the lifetime this sets the drag speed the effect starts at —
/// a ghost only survives to be drawn if the next one is recorded within its
/// lifetime, so `spacing / lifetime` is the threshold. Keep the stride short
/// enough that an ordinary deliberate drag crosses it; below that the window
/// is barely moving and a trail would just be a smear under it.
#[inline]
pub fn motion_trail_spacing(width: f32, height: f32) -> f32 {
    let extent = width.abs().min(height.abs());
    finite_clamp(extent * 0.05, 10.0, 48.0, 24.0)
}

/// Clamp a configured particle count to the per-burst work limit.
#[inline]
pub fn particle_burst_count(count: u32) -> usize {
    count.min(MAX_PARTICLES_PER_BURST) as usize
}

/// Small deterministic noise source suitable for repeatable visual variation.
#[inline]
pub fn effect_noise(mut seed: u32) -> f32 {
    seed ^= seed >> 16;
    seed = seed.wrapping_mul(0x7feb_352d);
    seed ^= seed >> 15;
    seed = seed.wrapping_mul(0x846c_a68b);
    seed ^= seed >> 16;
    seed as f32 / u32::MAX as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_dt_rejects_non_finite_and_caps_stalls() {
        assert_eq!(clamp_effect_dt(f32::NAN), 0.0);
        assert_eq!(clamp_effect_dt(-1.0), 0.0);
        assert_eq!(clamp_effect_dt(1.0), MAX_EFFECT_FRAME_DT);
        assert_eq!(clamp_effect_dt(1.0 / 60.0), 1.0 / 60.0);
    }

    #[test]
    fn animation_dt_rejects_invalid_values_without_capping_stalls() {
        assert_eq!(sanitize_animation_dt(f32::NAN), 0.0);
        assert_eq!(sanitize_animation_dt(f32::INFINITY), 0.0);
        assert_eq!(sanitize_animation_dt(-1.0), 0.0);
        assert_eq!(sanitize_animation_dt(1.0), 1.0);
    }

    #[test]
    fn newly_started_effect_does_not_inherit_idle_time() {
        assert_eq!(continuing_effect_dt(false, 12.0), 0.0);
        assert_eq!(continuing_effect_dt(true, 0.016), 0.016);
    }

    #[test]
    fn parameter_clamp_replaces_non_finite_values() {
        assert_eq!(finite_clamp(f32::NAN, 0.0, 10.0, 3.0), 3.0);
        assert_eq!(finite_clamp(f32::INFINITY, 0.0, 10.0, 3.0), 3.0);
        assert_eq!(finite_clamp(-1.0, 0.0, 10.0, 3.0), 0.0);
        assert_eq!(finite_clamp(20.0, 0.0, 10.0, 3.0), 10.0);
    }

    #[test]
    fn exponential_smoothing_is_refresh_rate_independent() {
        let one_step = smoothing_alpha(8.0, 1.0 / 60.0);
        let half_step = smoothing_alpha(8.0, 1.0 / 120.0);
        let combined = 1.0 - (1.0 - half_step) * (1.0 - half_step);
        assert!((one_step - combined).abs() < 1e-6);

        let stalled_frame = smoothing_alpha(8.0, 0.5);
        assert!((stalled_frame - (1.0 - (-4.0f32).exp())).abs() < 1e-6);
    }

    #[test]
    fn progress_handles_invalid_durations_without_nan() {
        assert_eq!(advance_progress(0.25, 0.01, 0.0), 1.0);
        assert_eq!(advance_progress(0.25, 0.01, f32::NAN), 1.0);
        assert!((advance_progress(0.25, 0.05, 0.5) - 0.35).abs() < 1e-6);
        assert_eq!(advance_progress(f32::NAN, 0.05, 0.5), 0.1);
        assert_eq!(advance_progress(0.25, 0.5, 0.5), 1.0);
        assert_eq!(advance_progress(0.25, f32::NAN, 0.5), 0.25);
    }

    #[test]
    fn effect_work_limits_match_shader_and_draw_bounds() {
        assert_eq!(wobbly_node_count(0), 2);
        assert_eq!(wobbly_node_count(8), 9);
        assert_eq!(wobbly_node_count(u32::MAX), 15);
        assert_eq!(motion_trail_capacity(u32::MAX), 64);
        assert_eq!(motion_trail_lifetime(6), Duration::from_secs_f32(0.3));
        assert_eq!(motion_trail_lifetime(1), Duration::from_secs_f32(0.15));
        assert_eq!(
            motion_trail_lifetime(u32::MAX),
            Duration::from_secs_f32(0.7)
        );
        assert_eq!(particle_burst_count(u32::MAX), 4096);
    }

    #[test]
    fn trail_spacing_scales_with_the_window_and_stays_bounded() {
        assert_eq!(motion_trail_spacing(1600.0, 900.0), 45.0);
        assert_eq!(motion_trail_spacing(1920.0, 24.0), 10.0);
        // `f32::min` discards a NaN operand, so one bad edge still yields a
        // sane stride; only a fully unusable size falls back.
        assert_eq!(motion_trail_spacing(f32::NAN, 900.0), 45.0);
        assert_eq!(motion_trail_spacing(f32::NAN, f32::NAN), 24.0);
    }

    #[test]
    fn trail_engages_at_ordinary_drag_speeds() {
        // A ghost survives to be drawn only if the next sample lands within its
        // lifetime, so spacing/lifetime is the speed the effect starts at. A
        // deliberate drag runs a few hundred px/s; keep the threshold below it
        // for the window sizes people actually drag.
        for (w, h) in [(480.0, 320.0), (900.0, 700.0), (1600.0, 1000.0)] {
            let params = MotionTrailParams::new(5, 0.3, w, h);
            let threshold = params.spacing / params.lifetime().as_secs_f32();
            assert!(
                threshold < 200.0,
                "{w}x{h}: trail needs {threshold} px/s to appear"
            );
        }
    }

    #[test]
    fn trail_records_one_sample_per_spacing_interval_not_per_event() {
        let params = MotionTrailParams::new(8, 0.3, 500.0, 500.0);
        let mut trail = MotionTrail::default();
        trail.begin_drag(0.0, 0.0);

        // 600 px of travel delivered as 600 one-pixel pointer events, the
        // shape a high-polling-rate mouse produces.
        for _ in 0..600 {
            trail.record_delta(1.0, 0.0, (0.0, 0.0), &params);
        }

        // The buffer holds one ghost per spacing interval of travel rather than
        // 600 samples covering the last few milliseconds of it.
        let ghosts: Vec<_> = trail
            .ghosts(Instant::now(), &params, 500.0, 500.0)
            .collect();
        assert_eq!(ghosts.len(), 8);
        for pair in ghosts.windows(2) {
            let step = pair[1].x - pair[0].x;
            assert!(
                (step - params.spacing).abs() < 1.0,
                "ghosts {step} px apart, expected {}",
                params.spacing
            );
        }
    }

    #[test]
    fn trail_ghosts_fade_and_recede_with_age() {
        let params = MotionTrailParams::new(12, 0.5, 400.0, 400.0);
        let mut trail = MotionTrail::default();
        trail.begin_drag(0.0, 0.0);
        for step in 0..4 {
            if step > 0 {
                std::thread::sleep(Duration::from_millis(50));
            }
            trail.record_delta(60.0, 0.0, (0.0, 0.0), &params);
        }

        let ghosts: Vec<_> = trail
            .ghosts(Instant::now(), &params, 400.0, 400.0)
            .collect();
        assert_eq!(ghosts.len(), 4);
        let (oldest, newest) = (ghosts[0], *ghosts.last().unwrap());
        assert!(oldest.opacity < newest.opacity, "{oldest:?} vs {newest:?}");
        assert!(oldest.width < newest.width, "{oldest:?} vs {newest:?}");
        assert!(ghosts.iter().all(|g| g.opacity <= 0.5));
        assert!(ghosts.iter().all(|g| g.width >= 400.0 * (1.0 - 0.12)));
    }

    #[test]
    fn stationary_window_records_no_ghosts_and_expired_ones_retire() {
        let params = MotionTrailParams::new(6, 0.3, 300.0, 300.0);
        let mut trail = MotionTrail::default();
        trail.begin_drag(100.0, 100.0);
        trail.record_position(100.0, 100.0, &params);
        assert!(trail.is_empty());

        trail.record_position(400.0, 100.0, &params);
        assert!(!trail.is_empty());
        assert!(!trail.retain_live(Instant::now() + params.lifetime(), params.lifetime()));
    }

    #[test]
    fn disabled_trail_parameters_drop_history() {
        let params = MotionTrailParams::new(0, 0.3, 300.0, 300.0);
        let mut trail = MotionTrail::default();
        trail.begin_drag(0.0, 0.0);
        trail.record_delta(200.0, 0.0, (0.0, 0.0), &params);
        assert!(trail.is_empty());
        assert!(params.draws_nothing());
    }

    #[test]
    fn deterministic_noise_is_bounded_and_varies_by_seed() {
        let a = effect_noise(1);
        let b = effect_noise(2);
        assert!((0.0..=1.0).contains(&a));
        assert!((0.0..=1.0).contains(&b));
        assert_ne!(a, b);
        assert_eq!(a, effect_noise(1));
    }
}
