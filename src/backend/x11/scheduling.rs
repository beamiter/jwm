//! Shared calloop pacing policy for the x11rb and xcb backends.

use std::time::{Duration, Instant};

/// Ordinary maintenance cadence while IPC/bar readiness is still polled.
pub(crate) const IDLE_UPDATE_INTERVAL: Duration = Duration::from_millis(20);
/// Cadence for handler-owned and compositor-owned continuous frame work.
pub(crate) const ACTIVE_UPDATE_INTERVAL: Duration = Duration::from_millis(16);

#[must_use]
pub(crate) const fn update_interval(frame_work_pending: bool) -> Duration {
    if frame_work_pending {
        ACTIVE_UPDATE_INTERVAL
    } else {
        IDLE_UPDATE_INTERVAL
    }
}

/// Keep the update clock anchored to its previous deadline.
///
/// `handler.update()` may block in a vblank swap. Adding another duration
/// after it returns would turn one 16 ms swap plus one 16 ms sleep into 30Hz.
/// If rendering consumed the slot, schedule one immediate next update; do not
/// replay every missed slot.
#[must_use]
pub(crate) fn next_update_deadline(
    previous_deadline: Instant,
    now: Instant,
    frame_work_pending: bool,
) -> Instant {
    previous_deadline
        .checked_add(update_interval(frame_work_pending))
        .map_or(now, |next| next.max(now))
}

/// Timeout for the outer calloop dispatch.
///
/// X readiness renders ordinary DamageNotify work immediately after dispatch.
/// A pending signal that survives that render represents continuous visual
/// work or a retry, so both handler- and compositor-owned work use the frame
/// cadence rather than a millisecond poll. Recording supplies a deadline only
/// while no frame is already pending, avoiding a due-zero busy loop.
#[must_use]
pub(crate) fn dispatch_timeout(
    handler_needs_tick: bool,
    compositor_pending: bool,
    compositor_deadline: Option<Duration>,
) -> Option<Duration> {
    if handler_needs_tick || compositor_pending {
        Some(ACTIVE_UPDATE_INTERVAL)
    } else {
        compositor_deadline
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continuous_frame_work_uses_one_shared_cadence() {
        assert_eq!(
            dispatch_timeout(true, false, None),
            Some(ACTIVE_UPDATE_INTERVAL)
        );
        assert_eq!(
            dispatch_timeout(true, true, None),
            Some(ACTIVE_UPDATE_INTERVAL)
        );
        assert_eq!(update_interval(true), ACTIVE_UPDATE_INTERVAL);
        assert_eq!(
            dispatch_timeout(false, true, Some(Duration::from_secs(1))),
            Some(ACTIVE_UPDATE_INTERVAL)
        );
    }

    #[test]
    fn idle_dispatch_preserves_the_compositor_deadline() {
        let deadline = Duration::from_millis(37);
        assert_eq!(
            dispatch_timeout(false, false, Some(deadline)),
            Some(deadline)
        );
        assert_eq!(dispatch_timeout(false, false, None), None);
        assert_eq!(update_interval(false), IDLE_UPDATE_INTERVAL);
    }

    #[test]
    fn update_deadline_does_not_sleep_again_after_a_blocking_swap() {
        let start = Instant::now();
        assert_eq!(
            next_update_deadline(start, start + Duration::from_millis(4), true),
            start + ACTIVE_UPDATE_INTERVAL
        );
        let returned_after_vblank = start + Duration::from_millis(17);
        assert_eq!(
            next_update_deadline(start, returned_after_vblank, true),
            returned_after_vblank
        );
    }

    #[test]
    fn both_x11_transports_use_the_shared_pacing_policy() {
        for backend in [
            include_str!("../x11rb/backend.rs"),
            include_str!("../xcb/backend.rs"),
        ] {
            assert!(backend.contains("scheduling::dispatch_timeout("));
            assert!(backend.contains("scheduling::next_update_deadline("));
            assert!(backend.contains("TimeoutAction::ToInstant("));
            assert!(!backend.contains("Duration::from_millis(1)"));
            assert!(!backend.contains("TimeoutAction::ToDuration"));
        }
    }
}
