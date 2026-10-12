//! Pure scheduling bounds shared by capture and viewer input polling.

use std::time::{Duration, Instant};

/// Preserve the capture poller's u16 millisecond ceiling and true zero-poll
/// behavior, but never round a positive remaining deadline down to poll(0).
pub(crate) fn capture_poll_timeout_millis(timeout: Duration) -> u16 {
    timeout
        .as_nanos()
        .div_ceil(1_000_000)
        .min(u128::from(u16::MAX)) as u16
}

/// Wait only for the *remaining* frame limit. Once eligible, never add another
/// whole idle interval: cursor position changes need not generate XDamage.
pub(crate) fn capture_rate_limit_wait(
    now: Instant,
    next_allowed: Instant,
    shutdown_slice: Duration,
) -> Option<Duration> {
    let remaining = next_allowed.saturating_duration_since(now);
    (!remaining.is_zero()).then_some(remaining.min(shutdown_slice))
}

/// Bound one local X11 drain so a continuous motion/expose stream cannot keep
/// already-collected input from reaching the remote host indefinitely.
pub(crate) struct EventPollBudget {
    remaining: usize,
}

impl EventPollBudget {
    pub(crate) fn new() -> Self {
        Self { remaining: 256 }
    }

    pub(crate) fn take(&mut self) -> bool {
        if self.remaining == 0 {
            return false;
        }
        self.remaining -= 1;
        true
    }

    pub(crate) fn exhausted(&self) -> bool {
        self.remaining == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[test]
    fn capture_poll_milliseconds_round_up_without_zero_or_overflow_regressions() {
        for (duration, expected) in [
            (Duration::ZERO, 0),
            (Duration::from_nanos(1), 1),
            (Duration::from_micros(999), 1),
            (Duration::from_millis(1), 1),
            (Duration::from_micros(33_333), 34),
            (Duration::from_millis(20), 20),
            (Duration::from_millis(u64::from(u16::MAX)), u16::MAX),
            (Duration::from_secs(100_000), u16::MAX),
            (Duration::MAX, u16::MAX),
        ] {
            assert_eq!(capture_poll_timeout_millis(duration), expected);
        }
    }

    #[test]
    fn capture_due_now_or_overdue_never_waits_another_frame_interval() {
        let base = Instant::now();
        let slice = Duration::from_millis(20);
        assert_eq!(capture_rate_limit_wait(base, base, slice), None);
        assert_eq!(capture_rate_limit_wait(base + slice, base, slice), None);
        assert_eq!(
            capture_rate_limit_wait(base, base + Duration::from_millis(83), slice),
            Some(slice)
        );
        assert_eq!(
            capture_rate_limit_wait(base, base + Duration::from_millis(3), slice),
            Some(Duration::from_millis(3))
        );
    }

    #[test]
    fn idle_cursor_polling_uses_one_rate_interval_not_two() {
        let start = Instant::now();
        for fps in [1, 12, 30, 60] {
            let interval = Duration::from_secs_f64(1.0 / f64::from(fps));
            let mut now = start;
            let mut next = start;
            let mut captures = Vec::new();
            for _ in 0..4 {
                // Model no selected X11 events: every bounded poll times out.
                while let Some(wait) = capture_rate_limit_wait(now, next, Duration::from_millis(20))
                {
                    now += wait;
                }
                captures.push(now.duration_since(start));
                next = now + interval;
            }
            assert_eq!(captures[0], Duration::ZERO);
            for pair in captures.windows(2) {
                assert_eq!(pair[1] - pair[0], interval);
            }
        }
    }

    #[test]
    fn early_x11_wakes_never_exceed_the_requested_capture_rate() {
        let start = Instant::now();
        let next = start + Duration::from_millis(33);
        for millis in 0..33 {
            assert!(
                capture_rate_limit_wait(
                    start + Duration::from_millis(millis),
                    next,
                    Duration::from_millis(20)
                )
                .is_some()
            );
        }
        assert_eq!(
            capture_rate_limit_wait(next, next, Duration::from_millis(20)),
            None
        );
    }

    #[test]
    fn event_flood_yields_without_dropping_or_reordering_input_edges() {
        // Marked edges straddle both boundaries, including a release/close.
        let expected: Vec<_> = (0..600).collect();
        let mut pending = VecDeque::from(expected.clone());
        let mut emitted = Vec::new();
        let mut batches = Vec::new();
        while !pending.is_empty() {
            let mut budget = EventPollBudget::new();
            let mut count = 0;
            while budget.take() {
                let Some(event) = pending.pop_front() else {
                    break;
                };
                emitted.push(event);
                count += 1;
            }
            batches.push(count);
        }
        assert_eq!(batches, [256, 256, 88]);
        assert_eq!(emitted, expected);
    }

    #[test]
    fn exhausted_event_budget_preserves_deferred_release_for_next_pass() {
        let mut budget = EventPollBudget::new();
        assert!(!budget.exhausted());
        for _ in 0..256 {
            assert!(budget.take());
        }
        assert!(budget.exhausted());
        assert!(!budget.take());
        // An adjacent autorepeat KeyPress may still be unread. Starting the
        // next pass restores a budget so a genuine lone release can time out.
        assert!(!EventPollBudget::new().exhausted());
    }
}
