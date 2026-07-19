//! Protocol-independent helpers for time-based compositor effects.
//!
//! Render loops are not guaranteed to run at 60 Hz: a display can refresh at
//! another rate and a compositor can resume after being idle for an arbitrary
//! amount of time.  Keep the small pieces of time normalization used by both
//! compositor backends here so effects advance consistently and cannot explode
//! after a long frame.

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
#[inline]
pub fn motion_trail_lifetime(samples: u32) -> Duration {
    let frames = motion_trail_capacity(samples).max(1) as f32;
    Duration::from_secs_f32(frames / 60.0)
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
        assert_eq!(motion_trail_lifetime(6), Duration::from_secs_f32(0.1));
        assert_eq!(particle_burst_count(u32::MAX), 4096);
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
