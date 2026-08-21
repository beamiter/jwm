//! Shared calloop pacing policy for the x11rb and xcb backends.

use std::time::{Duration, Instant};

/// Safety cadence while clipboard and generic worker completion still poll.
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
    handler_wakeup: Option<Duration>,
) -> Instant {
    let cadence = previous_deadline
        .checked_add(update_interval(frame_work_pending))
        .map_or(now, |next| next.max(now));
    handler_wakeup
        .and_then(|delay| now.checked_add(delay))
        .map_or(cadence, |deadline| cadence.min(deadline))
}

/// Fresh candidate after a non-timer event or a readiness-driven update.
#[must_use]
pub(crate) fn requested_update_deadline(
    now: Instant,
    frame_work_pending: bool,
    handler_wakeup: Option<Duration>,
) -> Instant {
    let cadence = now
        .checked_add(update_interval(frame_work_pending))
        .unwrap_or(now);
    handler_wakeup
        .and_then(|delay| now.checked_add(delay))
        .map_or(cadence, |deadline| cadence.min(deadline))
}

/// Return a deadline that needs to be installed in the timer.
///
/// Events that have not run `handler.update()` may only make a promise earlier;
/// a completed update starts a new generation and may reset it in either
/// direction.
#[must_use]
pub(crate) fn timer_rearm_deadline(
    current: Option<Instant>,
    candidate: Instant,
    reset_after_update: bool,
) -> Option<Instant> {
    match current {
        None => Some(candidate),
        Some(current) if reset_after_update && current != candidate => Some(candidate),
        Some(current) if candidate < current => Some(candidate),
        Some(_) => None,
    }
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
            next_update_deadline(start, start + Duration::from_millis(4), true, None),
            start + ACTIVE_UPDATE_INTERVAL
        );
        let returned_after_vblank = start + Duration::from_millis(17);
        assert_eq!(
            next_update_deadline(start, returned_after_vblank, true, None),
            returned_after_vblank
        );
    }

    #[test]
    fn handler_deadlines_join_the_absolute_frame_clock() {
        let now = Instant::now();
        assert_eq!(
            requested_update_deadline(now, false, Some(Duration::from_millis(7))),
            now + Duration::from_millis(7)
        );
        assert_eq!(
            requested_update_deadline(now, true, Some(Duration::from_secs(1))),
            now + ACTIVE_UPDATE_INTERVAL
        );
        assert_eq!(
            next_update_deadline(
                now,
                now + Duration::from_millis(4),
                false,
                Some(Duration::from_millis(3)),
            ),
            now + Duration::from_millis(7)
        );
    }

    #[test]
    fn events_only_tighten_but_completed_updates_can_reset() {
        let now = Instant::now();
        let current = now + Duration::from_millis(8);
        assert_eq!(
            timer_rearm_deadline(Some(current), now + Duration::from_millis(12), false),
            None
        );
        assert_eq!(
            timer_rearm_deadline(Some(current), now + Duration::from_millis(4), false),
            Some(now + Duration::from_millis(4))
        );
        assert_eq!(
            timer_rearm_deadline(Some(current), now + Duration::from_secs(1), true),
            Some(now + Duration::from_secs(1))
        );
    }

    #[test]
    fn dispatcher_reregistration_makes_an_existing_timer_earlier() {
        use calloop::timer::{TimeoutAction, Timer};
        use calloop::{Dispatcher, EventLoop};

        let mut event_loop: EventLoop<usize> = EventLoop::try_new().unwrap();
        let handle = event_loop.handle();
        let timer = Dispatcher::new(
            Timer::from_duration(Duration::from_secs(1)),
            |_, _, fired: &mut usize| {
                *fired += 1;
                TimeoutAction::Drop
            },
        );
        let token = handle.register_dispatcher(timer.clone()).unwrap();
        {
            timer
                .as_source_mut()
                .set_deadline(Instant::now() + Duration::from_millis(2));
        }
        handle.update(&token).unwrap();

        let mut fired = 0;
        event_loop
            .dispatch(Some(Duration::from_millis(100)), &mut fired)
            .unwrap();
        assert_eq!(fired, 1);
    }

    #[test]
    fn both_x11_transports_use_the_shared_pacing_policy() {
        for backend in [
            include_str!("../x11rb/backend.rs"),
            include_str!("../xcb/backend.rs"),
        ] {
            assert!(backend.contains("scheduling::dispatch_timeout("));
            assert!(backend.contains("scheduling::next_update_deadline("));
            assert!(backend.contains("scheduling::timer_rearm_deadline("));
            assert!(backend.contains("register_dispatcher("));
            assert!(backend.contains("TimeoutAction::ToInstant("));
            assert!(backend.contains("handler.duplicate_update_readiness_fd()"));
            assert!(backend.contains("data.update_requested = true"));
            assert!(!backend.contains("Duration::from_millis(1)"));
            assert!(!backend.contains("TimeoutAction::ToDuration"));
        }
    }
}
