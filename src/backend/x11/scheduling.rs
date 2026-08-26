//! Shared calloop pacing policy for the x11rb and xcb backends.

use std::time::{Duration, Instant};

/// Safety cadence retained whenever async readiness is unavailable or fails.
pub(crate) const IDLE_UPDATE_INTERVAL: Duration = Duration::from_millis(20);
/// Cadence for handler-owned and compositor-owned continuous frame work.
pub(crate) const ACTIVE_UPDATE_INTERVAL: Duration = Duration::from_millis(16);

#[must_use]
pub(crate) const fn update_interval(
    frame_work_pending: bool,
    idle_poll_required: bool,
) -> Option<Duration> {
    if frame_work_pending {
        Some(ACTIVE_UPDATE_INTERVAL)
    } else if idle_poll_required {
        Some(IDLE_UPDATE_INTERVAL)
    } else {
        None
    }
}

/// Only a native, fully readiness-driven session may drop the safety poll.
///
/// Composited sessions intentionally retain their existing idle cadence.
/// Registration failure and a notifier whose write/drain path failed both
/// restore the old 20 ms timer immediately on the next scheduling decision.
#[must_use]
pub(crate) const fn idle_poll_required(
    compositor_active: bool,
    handler_readiness_registered: bool,
    async_update_readiness_healthy: bool,
) -> bool {
    compositor_active || !handler_readiness_registered || !async_update_readiness_healthy
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
    idle_poll_required: bool,
    handler_wakeup: Option<Duration>,
) -> Instant {
    let cadence = update_interval(frame_work_pending, idle_poll_required).map(|interval| {
        previous_deadline
            .checked_add(interval)
            .map_or(now, |next| next.max(now))
    });
    joined_deadline(cadence, handler_wakeup, now)
}

/// Fresh candidate after a non-timer event or a readiness-driven update.
#[must_use]
pub(crate) fn requested_update_deadline(
    now: Instant,
    frame_work_pending: bool,
    idle_poll_required: bool,
    handler_wakeup: Option<Duration>,
) -> Instant {
    let cadence = update_interval(frame_work_pending, idle_poll_required)
        .map(|interval| now.checked_add(interval).unwrap_or(now));
    joined_deadline(cadence, handler_wakeup, now)
}

fn joined_deadline(
    cadence: Option<Instant>,
    handler_wakeup: Option<Duration>,
    now: Instant,
) -> Instant {
    let handler = handler_wakeup.and_then(|delay| now.checked_add(delay));
    match (cadence, handler) {
        (Some(cadence), Some(handler)) => cadence.min(handler),
        (Some(cadence), None) => cadence,
        (None, Some(handler)) => handler,
        // Generic handlers that advertise neither readiness nor a deadline
        // are kept safe even if a caller supplied an inconsistent policy.
        (None, None) => now.checked_add(IDLE_UPDATE_INTERVAL).unwrap_or(now),
    }
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
        assert_eq!(update_interval(true, false), Some(ACTIVE_UPDATE_INTERVAL));
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
        assert_eq!(update_interval(false, true), Some(IDLE_UPDATE_INTERVAL));
    }

    #[test]
    fn update_deadline_does_not_sleep_again_after_a_blocking_swap() {
        let start = Instant::now();
        assert_eq!(
            next_update_deadline(start, start + Duration::from_millis(4), true, false, None),
            start + ACTIVE_UPDATE_INTERVAL
        );
        let returned_after_vblank = start + Duration::from_millis(17);
        assert_eq!(
            next_update_deadline(start, returned_after_vblank, true, false, None),
            returned_after_vblank
        );
    }

    #[test]
    fn handler_deadlines_join_the_absolute_frame_clock() {
        let now = Instant::now();
        assert_eq!(
            requested_update_deadline(now, false, true, Some(Duration::from_millis(7))),
            now + Duration::from_millis(7)
        );
        assert_eq!(
            requested_update_deadline(now, true, false, Some(Duration::from_secs(1))),
            now + ACTIVE_UPDATE_INTERVAL
        );
        assert_eq!(
            next_update_deadline(
                now,
                now + Duration::from_millis(4),
                false,
                true,
                Some(Duration::from_millis(3)),
            ),
            now + Duration::from_millis(7)
        );
    }

    #[test]
    fn fully_ready_native_idle_uses_the_real_maintenance_deadline() {
        let now = Instant::now();
        assert!(!idle_poll_required(false, true, true));
        assert_eq!(update_interval(false, false), None);
        assert_eq!(
            requested_update_deadline(now, false, false, Some(Duration::from_secs(3))),
            now + Duration::from_secs(3)
        );
        assert_eq!(
            next_update_deadline(
                now,
                now + Duration::from_millis(4),
                false,
                false,
                Some(Duration::from_secs(3)),
            ),
            now + Duration::from_secs(3) + Duration::from_millis(4)
        );
    }

    #[test]
    fn compositor_or_readiness_failure_restores_the_idle_safety_poll() {
        for required in [
            idle_poll_required(true, true, true),
            idle_poll_required(false, false, true),
            idle_poll_required(false, true, false),
        ] {
            assert!(required);
        }
        let now = Instant::now();
        assert_eq!(
            requested_update_deadline(now, false, true, Some(Duration::from_secs(3))),
            now + IDLE_UPDATE_INTERVAL
        );
    }

    #[test]
    fn bar_worker_health_loss_rearms_the_idle_safety_poll() {
        let now = Instant::now();
        let long_idle_deadline = now + Duration::from_secs(3);
        let poll_required = idle_poll_required(false, true, false);
        let fallback =
            requested_update_deadline(now, false, poll_required, Some(Duration::from_secs(3)));

        assert_eq!(fallback, now + IDLE_UPDATE_INTERVAL);
        assert_eq!(
            timer_rearm_deadline(Some(long_idle_deadline), fallback, false),
            Some(fallback),
            "an observed worker failure must tighten an already-armed maintenance timer"
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
            assert!(backend.contains("scheduling::idle_poll_required("));
            assert!(backend.contains("scheduling::timer_rearm_deadline("));
            assert!(backend.contains("register_dispatcher("));
            assert!(backend.contains("TimeoutAction::ToInstant("));
            assert!(backend.contains("handler.duplicate_update_readiness_fd()"));
            assert_eq!(
                backend
                    .matches("handler.async_update_readiness_healthy()")
                    .count(),
                2,
                "timer callbacks and post-dispatch rearming must both observe health"
            );
            assert!(backend.contains("data.update_requested = true"));
            assert!(!backend.contains("Duration::from_millis(1)"));
            assert!(!backend.contains("TimeoutAction::ToDuration"));
        }
    }
}
