//! Backend-neutral timing and geometry for Dock/Genie effects.

use crate::backend::api::CompositorRect;

pub(crate) const PREVIEW_MAX_WIDTH: f32 = 320.0;
const PREVIEW_MAX_HEIGHT: f32 = 240.0;
const PREVIEW_GAP: f32 = 10.0;
const PREVIEW_DURATION_SECS: f32 = 0.22;
pub(crate) const MAX_MINIMIZED_VISUALS: usize = 32;
pub(crate) const MAX_MINIMIZED_VISUAL_BYTES: u64 = 128 * 1024 * 1024;

#[must_use]
pub(crate) fn estimated_visual_bytes(width: f32, height: f32) -> u64 {
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        return 0;
    }
    (width.ceil() as u64)
        .saturating_mul(height.ceil() as u64)
        .saturating_mul(4)
}

/// Preserve at least the newest real preview even when one unusually large
/// surface exceeds the byte budget by itself. Further entries obey both the
/// count and estimated RGBA storage ceilings.
#[must_use]
pub(crate) fn minimized_cache_over_budget(entry_count: usize, estimated_bytes: u64) -> bool {
    entry_count > MAX_MINIMIZED_VISUALS
        || (entry_count > 1 && estimated_bytes > MAX_MINIMIZED_VISUAL_BYTES)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GenieDirection {
    Minimize,
    Restore,
}

impl GenieDirection {
    #[must_use]
    pub(crate) const fn end_progress(self) -> f32 {
        match self {
            Self::Minimize => 1.0,
            Self::Restore => 0.0,
        }
    }
}

/// Advance a reversible animation without changing speed when it reverses
/// halfway through.  The shader owns the spatial easing; this timeline stays
/// linear so a reversal is position-continuous.
#[must_use]
pub(crate) fn genie_progress(
    start_progress: f32,
    direction: GenieDirection,
    elapsed_secs: f32,
    full_duration_secs: f32,
) -> (f32, bool) {
    let start = start_progress.clamp(0.0, 1.0);
    let end = direction.end_progress();
    let distance = (end - start).abs();
    if distance <= f32::EPSILON {
        return (end, true);
    }
    let duration = full_duration_secs.max(0.001) * distance;
    let fraction = (elapsed_secs.max(0.0) / duration).clamp(0.0, 1.0);
    (start + (end - start) * fraction, fraction >= 1.0)
}

/// Sample an in-flight animation and retarget its timeline in place.
///
/// Both compositor backends use this when a window is restored and minimized
/// again before the reverse Genie has finished.  Sampling before changing the
/// direction keeps the first frame after the reversal at exactly the same
/// mesh position; only the velocity changes sign.
pub(crate) fn retarget_genie_timeline(
    start: &mut std::time::Instant,
    start_progress: &mut f32,
    direction: &mut GenieDirection,
    new_direction: GenieDirection,
    now: std::time::Instant,
    full_duration_secs: f32,
) -> f32 {
    let sampled = genie_progress(
        *start_progress,
        *direction,
        now.saturating_duration_since(*start).as_secs_f32(),
        full_duration_secs,
    )
    .0;
    *start = now;
    *start_progress = sampled;
    *direction = new_direction;
    sampled
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PreviewDirection {
    Show,
    Hide,
}

/// Return the remaining time on a visible preview lease. Hidden previews do
/// not own a lease; a zero duration means the compositor must start the exit
/// transition on its next scheduler tick.
#[must_use]
pub(crate) fn preview_lease_timeout(
    direction: PreviewDirection,
    now: std::time::Instant,
    deadline: std::time::Instant,
) -> Option<std::time::Duration> {
    (direction == PreviewDirection::Show).then(|| deadline.saturating_duration_since(now))
}

/// Sample a compact, slightly overshooting Dock preview entrance.  The
/// decaying sine gives the scale a spring response while opacity remains
/// monotonic and therefore cannot flash on rapid enter/leave transitions.
#[must_use]
pub(crate) fn preview_motion(
    start_opacity: f32,
    start_scale: f32,
    direction: PreviewDirection,
    elapsed_secs: f32,
) -> (f32, f32, bool) {
    let t = (elapsed_secs.max(0.0) / PREVIEW_DURATION_SECS).clamp(0.0, 1.0);
    let eased = t * t * (3.0 - 2.0 * t);
    let (target_opacity, target_scale) = match direction {
        PreviewDirection::Show => (1.0, 1.0),
        PreviewDirection::Hide => (0.0, 0.92),
    };
    if t >= 1.0 {
        return (target_opacity, target_scale, true);
    }
    let opacity = start_opacity + (target_opacity - start_opacity) * eased;
    let mut scale = start_scale + (target_scale - start_scale) * eased;
    if direction == PreviewDirection::Show {
        scale += (t * std::f32::consts::TAU * 1.25).sin() * (-7.0 * t).exp() * 0.035;
    }
    (opacity.clamp(0.0, 1.0), scale.max(0.0), false)
}

/// Move only the hovered window's static Dock thumbnail from its persistent
/// slot to the bar's transformed hover anchor.  The persistent target remains
/// untouched so Genie minimize/restore animations always land at the stable
/// slot and a Hide transition naturally returns there as opacity falls.
#[must_use]
pub(crate) fn dock_item_preview_target(
    window_id: u64,
    stable_target: CompositorRect,
    preview: Option<(u64, CompositorRect, f32)>,
) -> Option<CompositorRect> {
    let stable_target = stable_target.normalized()?;
    let Some((preview_window, preview_anchor, opacity)) = preview else {
        return Some(stable_target);
    };
    if preview_window != window_id {
        return Some(stable_target);
    }
    let Some(preview_anchor) = preview_anchor.normalized() else {
        return Some(stable_target);
    };
    let progress = if opacity.is_finite() {
        opacity.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let interpolate = |from: f32, to: f32| from + (to - from) * progress;
    CompositorRect::new(
        interpolate(stable_target.x, preview_anchor.x),
        interpolate(stable_target.y, preview_anchor.y),
        interpolate(stable_target.width, preview_anchor.width),
        interpolate(stable_target.height, preview_anchor.height),
    )
    .normalized()
    .or(Some(stable_target))
}

/// Place an aspect-preserving preview below its bar anchor, flipping above
/// only when the lower side cannot hold it.  The result is clamped to the
/// compositor framebuffer so multi-monitor edge slots remain fully visible.
#[must_use]
pub(crate) fn preview_rect(
    anchor: CompositorRect,
    source_width: f32,
    source_height: f32,
    output_bounds: CompositorRect,
    scale: f32,
) -> Option<CompositorRect> {
    let anchor = anchor.normalized()?;
    let output_bounds = output_bounds.normalized()?;
    if !source_width.is_finite()
        || !source_height.is_finite()
        || source_width <= 0.0
        || source_height <= 0.0
    {
        return None;
    }
    let fit = (PREVIEW_MAX_WIDTH / source_width)
        .min(PREVIEW_MAX_HEIGHT / source_height)
        .min(output_bounds.width / source_width)
        .min(output_bounds.height / source_height)
        .min(1.0);
    let width = (source_width * fit * scale)
        .max(1.0)
        .min(output_bounds.width.max(1.0));
    let height = (source_height * fit * scale)
        .max(1.0)
        .min(output_bounds.height.max(1.0));
    let center_x = anchor.x + anchor.width * 0.5;
    let below_y = anchor.y + anchor.height + PREVIEW_GAP;
    let output_right = output_bounds.x + output_bounds.width;
    let output_bottom = output_bounds.y + output_bounds.height;
    let y = if below_y + height <= output_bottom {
        below_y
    } else {
        (anchor.y - PREVIEW_GAP - height).max(output_bounds.y)
    };
    let max_x = (output_right - width).max(output_bounds.x);
    let max_y = (output_bottom - height).max(output_bounds.y);
    let x = (center_x - width * 0.5).clamp(output_bounds.x, max_x);
    Some(CompositorRect::new(
        x,
        y.clamp(output_bounds.y, max_y),
        width,
        height,
    ))
}

/// Resolve the output containing an anchor without assuming the compositor's
/// global space starts at `(0, 0)`. If the anchor sits in a topology gap, use
/// the nearest output; an empty/invalid topology falls back to the supplied
/// compositor bounds.
#[must_use]
pub(crate) fn output_bounds_for_anchor(
    anchor: CompositorRect,
    outputs: impl IntoIterator<Item = CompositorRect>,
    fallback: CompositorRect,
) -> CompositorRect {
    let (center_x, center_y) = anchor.center();
    let fallback = fallback.normalized().unwrap_or(CompositorRect::new(
        anchor.x,
        anchor.y,
        anchor.width.max(1.0),
        anchor.height.max(1.0),
    ));
    let mut nearest = fallback;
    let mut nearest_distance = f32::INFINITY;
    for output in outputs {
        let Some(output) = output.normalized() else {
            continue;
        };
        let right = output.x + output.width;
        let bottom = output.y + output.height;
        let dx = if center_x < output.x {
            output.x - center_x
        } else if center_x > right {
            center_x - right
        } else {
            0.0
        };
        let dy = if center_y < output.y {
            output.y - center_y
        } else if center_y > bottom {
            center_y - bottom
        } else {
            0.0
        };
        if dx == 0.0 && dy == 0.0 {
            return output;
        }
        let distance = dx * dx + dy * dy;
        if distance < nearest_distance {
            nearest = output;
            nearest_distance = distance;
        }
    }
    nearest
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reversing_halfway_keeps_progress_continuous_and_scales_duration() {
        let (half, done) = genie_progress(0.0, GenieDirection::Minimize, 0.15, 0.3);
        assert!(!done);
        assert!((half - 0.5).abs() < 0.001);
        let (same, _) = genie_progress(half, GenieDirection::Restore, 0.0, 0.3);
        assert_eq!(same, half);
        let (restored, done) = genie_progress(half, GenieDirection::Restore, 0.15, 0.3);
        assert!(done);
        assert_eq!(restored, 0.0);
    }

    #[test]
    fn retargeting_restore_back_to_minimize_is_position_continuous() {
        use std::time::{Duration, Instant};

        let now = Instant::now();
        let mut start = now - Duration::from_millis(150);
        let mut start_progress = 1.0;
        let mut direction = GenieDirection::Restore;

        let sampled = retarget_genie_timeline(
            &mut start,
            &mut start_progress,
            &mut direction,
            GenieDirection::Minimize,
            now,
            0.3,
        );

        assert!((sampled - 0.5).abs() < 0.01);
        assert_eq!(start, now);
        assert_eq!(start_progress, sampled);
        assert_eq!(direction, GenieDirection::Minimize);
        assert_eq!(
            genie_progress(start_progress, direction, 0.0, 0.3).0,
            sampled
        );
        let (finished, done) = genie_progress(start_progress, direction, 0.15, 0.3);
        assert!(done);
        assert_eq!(finished, 1.0);
    }

    #[test]
    fn preview_is_aspect_preserving_bounded_and_flips_at_bottom_edge() {
        let top = preview_rect(
            CompositorRect::new(1800.0, 0.0, 40.0, 38.0),
            1600.0,
            900.0,
            CompositorRect::new(0.0, 0.0, 1920.0, 1080.0),
            1.0,
        )
        .unwrap();
        assert!(top.width <= PREVIEW_MAX_WIDTH && top.x + top.width <= 1920.0);
        assert!((top.width / top.height - 16.0 / 9.0).abs() < 0.01);

        let bottom = preview_rect(
            CompositorRect::new(900.0, 1040.0, 40.0, 40.0),
            800.0,
            600.0,
            CompositorRect::new(0.0, 0.0, 1920.0, 1080.0),
            1.0,
        )
        .unwrap();
        assert!(bottom.y < 1040.0);
    }

    #[test]
    fn preview_clamps_to_anchor_output_in_negative_and_right_hand_spaces() {
        let outputs = [
            CompositorRect::new(-1280.0, 0.0, 1280.0, 1024.0),
            CompositorRect::new(0.0, 0.0, 1920.0, 1080.0),
            CompositorRect::new(1920.0, -120.0, 2560.0, 1440.0),
        ];
        let fallback = CompositorRect::new(0.0, 0.0, 4480.0, 1440.0);

        let left_anchor = CompositorRect::new(-42.0, 980.0, 36.0, 26.0);
        let left_bounds = output_bounds_for_anchor(left_anchor, outputs, fallback);
        assert_eq!(left_bounds, outputs[0]);
        let left = preview_rect(left_anchor, 1600.0, 900.0, left_bounds, 1.0).unwrap();
        assert!(left.x >= -1280.0 && left.x + left.width <= 0.0);

        let right_anchor = CompositorRect::new(4380.0, 1200.0, 36.0, 26.0);
        let right_bounds = output_bounds_for_anchor(right_anchor, outputs, fallback);
        assert_eq!(right_bounds, outputs[2]);
        let right = preview_rect(right_anchor, 800.0, 600.0, right_bounds, 1.0).unwrap();
        assert!(right.x >= 1920.0 && right.x + right.width <= 4480.0);
        assert!(right.y >= -120.0 && right.y + right.height <= 1320.0);
    }

    #[test]
    fn preview_motion_finishes_at_exact_stable_values() {
        let (opacity, scale, done) = preview_motion(0.0, 0.86, PreviewDirection::Show, 1.0);
        assert!(done);
        assert_eq!(opacity, 1.0);
        assert_eq!(scale, 1.0);
        let (opacity, scale, done) = preview_motion(1.0, 1.0, PreviewDirection::Hide, 1.0);
        assert!(done);
        assert_eq!(opacity, 0.0);
        assert_eq!(scale, 0.92);
    }

    #[test]
    fn dock_item_target_tracks_hover_anchor_at_zero_half_and_full_opacity() {
        let stable = CompositorRect::new(10.0, 20.0, 30.0, 20.0);
        let anchor = CompositorRect::new(2.0, 8.0, 46.0, 32.0);

        assert_eq!(
            dock_item_preview_target(7, stable, Some((7, anchor, 0.0))),
            Some(stable)
        );
        assert_eq!(
            dock_item_preview_target(7, stable, Some((7, anchor, 0.5))),
            Some(CompositorRect::new(6.0, 14.0, 38.0, 26.0))
        );
        assert_eq!(
            dock_item_preview_target(7, stable, Some((7, anchor, 1.0))),
            Some(anchor)
        );
    }

    #[test]
    fn dock_item_target_does_not_move_a_non_previewed_window() {
        let stable = CompositorRect::new(10.0, 20.0, 30.0, 20.0);
        let anchor = CompositorRect::new(2.0, 8.0, 46.0, 32.0);

        assert_eq!(
            dock_item_preview_target(8, stable, Some((7, anchor, 1.0))),
            Some(stable)
        );
    }

    #[test]
    fn dock_item_target_is_finite_and_clamps_preview_progress() {
        let stable = CompositorRect::new(10.0, 20.0, 30.0, 20.0);
        let anchor = CompositorRect::new(2.0, 8.0, 46.0, 32.0);

        assert_eq!(
            dock_item_preview_target(7, stable, Some((7, anchor, -4.0))),
            Some(stable)
        );
        assert_eq!(
            dock_item_preview_target(7, stable, Some((7, anchor, 4.0))),
            Some(anchor)
        );
        assert_eq!(
            dock_item_preview_target(7, stable, Some((7, anchor, f32::NAN))),
            Some(stable)
        );
        assert_eq!(
            dock_item_preview_target(
                7,
                stable,
                Some((7, CompositorRect::new(f32::INFINITY, 8.0, 46.0, 32.0), 1.0))
            ),
            Some(stable)
        );
        assert_eq!(
            dock_item_preview_target(
                7,
                CompositorRect::new(10.0, 20.0, f32::NAN, 20.0),
                Some((7, anchor, 1.0))
            ),
            None
        );
    }

    #[test]
    fn preview_lease_has_a_scheduler_deadline_and_expires_to_zero() {
        let now = std::time::Instant::now();
        let deadline = now + std::time::Duration::from_secs(4);
        assert_eq!(
            preview_lease_timeout(PreviewDirection::Show, now, deadline),
            Some(std::time::Duration::from_secs(4))
        );
        assert_eq!(
            preview_lease_timeout(
                PreviewDirection::Show,
                deadline + std::time::Duration::from_millis(1),
                deadline,
            ),
            Some(std::time::Duration::ZERO)
        );
        assert_eq!(
            preview_lease_timeout(PreviewDirection::Hide, now, deadline),
            None
        );
    }

    #[test]
    fn minimized_cache_obeys_count_and_byte_budgets_but_keeps_latest_one() {
        assert!(!minimized_cache_over_budget(
            1,
            MAX_MINIMIZED_VISUAL_BYTES * 2
        ));
        assert!(minimized_cache_over_budget(
            2,
            MAX_MINIMIZED_VISUAL_BYTES + 1
        ));
        assert!(minimized_cache_over_budget(MAX_MINIMIZED_VISUALS + 1, 0));
        assert!(!minimized_cache_over_budget(
            MAX_MINIMIZED_VISUALS,
            MAX_MINIMIZED_VISUAL_BYTES
        ));
        assert_eq!(estimated_visual_bytes(3840.0, 2160.0), 33_177_600);
    }
}
