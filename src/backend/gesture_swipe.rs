//! Backend-independent swipe recognition. Commit only on a completed gesture,
//! never on updates, so cancellation and a long swipe cannot switch twice.

use std::sync::atomic::{AtomicU64, Ordering};

/// One physical sequence, before policy turns it into a workspace preview or
/// an ordinary command. IDs survive queuing and never identify another device.
#[derive(Debug, Clone)]
pub enum SwipeInput {
    Begin {
        id: u64,
        fingers: u32,
        config_signature: String,
    },
    Update {
        id: u64,
        dx: f64,
        dy: f64,
        time: u32,
    },
    End {
        id: u64,
        cancelled: bool,
        time: u32,
    },
}

static NEXT_SWIPE_ID: AtomicU64 = AtomicU64::new(1);

pub fn next_swipe_id() -> u64 {
    NEXT_SWIPE_ID.fetch_add(1, Ordering::Relaxed)
}

/// Physical Begins already observed by a backend, including queued ones.
pub fn latest_swipe_id() -> u64 {
    NEXT_SWIPE_ID.load(Ordering::Relaxed).saturating_sub(1)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwipeAxis {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, Copy)]
struct SwipeVelocitySample {
    previous: (f64, f64),
    delta: (f64, f64),
    scale: f64,
}

impl SwipeVelocitySample {
    fn velocity(self) -> (f64, f64) {
        (
            0.4 * self.previous.0 + 0.6 * self.delta.0 * self.scale,
            0.4 * self.previous.1 + 0.6 * self.delta.1 * self.scale,
        )
    }
}

#[derive(Debug, Default, Clone)]
pub struct GestureSwipeTracker {
    pub fingers: u32,
    pub intercept: bool,
    pub config_signature: String,
    pub dx: f64,
    pub dy: f64,
    pub axis: Option<SwipeAxis>,
    last_time: Option<u32>,
    velocity_sample: Option<SwipeVelocitySample>,
}

impl GestureSwipeTracker {
    pub fn begin(&mut self, fingers: u32, intercept: bool) {
        *self = Self {
            fingers,
            intercept,
            ..Self::default()
        };
    }

    pub fn update(&mut self, dx: f64, dy: f64) {
        if self.intercept {
            self.dx += dx;
            self.dy += dy;
            if self.axis.is_none() {
                let (x, y) = (self.dx.abs(), self.dy.abs());
                self.axis = if x >= 8.0 && x >= y * 1.5 {
                    Some(SwipeAxis::Horizontal)
                } else if y >= 8.0 && y >= x * 1.5 {
                    Some(SwipeAxis::Vertical)
                } else {
                    None
                };
            }
        }
    }

    /// Device timestamps wrap at 2^32 ms. A long pause invalidates flick speed.
    pub fn update_timed(&mut self, dx: f64, dy: f64, time: u32) {
        match self.last_time.map(|previous| time.wrapping_sub(previous)) {
            Some(0) => {
                // Millisecond timestamps can be shared by several updates.
                // Use all their motion, without smoothing the same interval twice.
                if let Some(sample) = &mut self.velocity_sample {
                    sample.delta.0 += dx;
                    sample.delta.1 += dy;
                }
            }
            Some(dt @ 1..=80) => {
                self.velocity_sample = Some(SwipeVelocitySample {
                    previous: self.velocity(),
                    delta: (dx, dy),
                    scale: 1000.0 / f64::from(dt),
                });
            }
            _ => self.velocity_sample = None,
        }
        self.last_time = Some(time);
        self.update(dx, dy);
    }

    fn velocity(&self) -> (f64, f64) {
        self.velocity_sample
            .map_or((0.0, 0.0), SwipeVelocitySample::velocity)
    }

    pub fn direction(&self) -> Option<&'static str> {
        if !self.dx.is_finite() || !self.dy.is_finite() {
            return None;
        }
        match self.axis {
            Some(SwipeAxis::Horizontal) if self.dx != 0.0 => {
                Some(if self.dx < 0.0 { "left" } else { "right" })
            }
            Some(SwipeAxis::Vertical) if self.dy != 0.0 => {
                Some(if self.dy < 0.0 { "up" } else { "down" })
            }
            _ => None,
        }
    }

    /// A quick short flick may commit, but tiny noise and a flick followed by
    /// a pause/reversal cannot. Custom actions still execute once, at End.
    pub fn finish_timed(
        &mut self,
        cancelled: bool,
        threshold: f64,
        time: u32,
    ) -> Option<(u32, &'static str)> {
        let direction = self.direction();
        let (vx, vy) = self.velocity();
        let (distance, velocity) = match self.axis {
            Some(SwipeAxis::Horizontal) => (self.dx, vx),
            Some(SwipeAxis::Vertical) => (self.dy, vy),
            None => (0.0, 0.0),
        };
        let fresh = self
            .last_time
            .is_some_and(|last| time.wrapping_sub(last) <= 80);
        let flick = fresh
            && distance.abs() >= threshold * 0.35
            && velocity.abs() >= 900.0
            && distance.signum() == velocity.signum();
        let reversing = fresh && velocity.abs() >= 900.0 && distance.signum() != velocity.signum();
        let result = !cancelled
            && !reversing
            && self.intercept
            && threshold.is_finite()
            && threshold > 0.0
            && distance.is_finite()
            && (distance.abs() >= threshold || flick);
        let fingers = self.fingers;
        *self = Self::default();
        result
            .then_some(direction)
            .flatten()
            .map(|direction| (fingers, direction))
    }

    /// Consume ownership even on cancellation, invalid input or short swipes.
    /// A 1.5:1 axis margin rejects ambiguous diagonal movements.
    pub fn finish(&mut self, cancelled: bool, threshold: f64) -> Option<(u32, &'static str)> {
        let gesture = std::mem::take(self);
        if !gesture.intercept
            || cancelled
            || !threshold.is_finite()
            || threshold <= 0.0
            || !gesture.dx.is_finite()
            || !gesture.dy.is_finite()
        {
            return None;
        }
        let x = gesture.dx.abs();
        let y = gesture.dy.abs();
        let direction = if x >= threshold && x >= y * 1.5 {
            if gesture.dx < 0.0 { "left" } else { "right" }
        } else if y >= threshold && y >= x * 1.5 {
            if gesture.dy < 0.0 { "up" } else { "down" }
        } else {
            return None;
        };
        Some((gesture.fingers, direction))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn swipe(dx: f64, dy: f64) -> Option<(u32, &'static str)> {
        let mut tracker = GestureSwipeTracker::default();
        tracker.begin(3, true);
        tracker.update(dx, dy);
        tracker.finish(false, 80.0)
    }

    #[test]
    fn horizontal_and_vertical_directions_require_clear_axis_and_threshold() {
        assert_eq!(swipe(-80.0, 10.0), Some((3, "left")));
        assert_eq!(swipe(80.0, -10.0), Some((3, "right")));
        assert_eq!(swipe(10.0, -80.0), Some((3, "up")));
        assert_eq!(swipe(10.0, 80.0), Some((3, "down")));
        assert_eq!(swipe(79.9, 0.0), None);
        assert_eq!(swipe(100.0, 90.0), None);
        assert_eq!(swipe(-90.0, -100.0), None);
    }

    #[test]
    fn completed_swipe_commits_once_and_cancel_never_commits() {
        let mut tracker = GestureSwipeTracker::default();
        tracker.begin(3, true);
        for _ in 0..100 {
            tracker.update(-10.0, 0.0);
        }
        assert_eq!(tracker.finish(false, 80.0), Some((3, "left")));
        assert_eq!(tracker.finish(false, 80.0), None);
        tracker.begin(3, true);
        tracker.update(1000.0, 0.0);
        assert_eq!(tracker.finish(true, 80.0), None);
        assert_eq!(tracker.finish(false, 80.0), None);
    }

    #[test]
    fn new_begin_discards_stale_ownership_and_unclaimed_updates() {
        let mut tracker = GestureSwipeTracker::default();
        tracker.begin(3, true);
        tracker.update(100.0, 0.0);
        tracker.begin(2, false);
        tracker.update(1000.0, 0.0);
        assert_eq!(tracker.finish(false, 80.0), None);
        tracker.begin(3, true);
        assert_eq!(tracker.finish(false, 80.0), None);
    }

    #[test]
    fn non_finite_input_and_invalid_thresholds_are_rejected() {
        assert_eq!(swipe(f64::NAN, 0.0), None);
        assert_eq!(swipe(0.0, f64::INFINITY), None);
        for threshold in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let mut tracker = GestureSwipeTracker::default();
            tracker.begin(3, true);
            tracker.update(100.0, 0.0);
            assert_eq!(tracker.finish(false, threshold), None);
        }
    }

    #[test]
    fn timed_swipe_locks_an_axis_and_can_be_pulled_back_before_release() {
        let mut tracker = GestureSwipeTracker::default();
        tracker.begin(3, true);
        tracker.update_timed(-30.0, 1.0, 10);
        tracker.update_timed(-60.0, 55.0, 110);
        assert_eq!(tracker.axis, Some(SwipeAxis::Horizontal));
        assert_eq!(tracker.direction(), Some("left"));
        tracker.update_timed(80.0, 0.0, 210);
        assert_eq!(tracker.finish_timed(false, 80.0, 220), None);
        assert_eq!(tracker.finish_timed(false, 80.0, 220), None);
    }

    #[test]
    fn a_short_fast_flick_needs_distance_freshness_and_matching_velocity() {
        let mut tracker = GestureSwipeTracker::default();
        tracker.begin(3, true);
        tracker.update_timed(-15.0, 0.0, 100);
        tracker.update_timed(-20.0, 0.0, 108);
        assert_eq!(
            tracker.clone().finish_timed(false, 80.0, 110),
            Some((3, "left"))
        );
        assert_eq!(tracker.clone().finish_timed(true, 80.0, 110), None);
        assert_eq!(tracker.finish_timed(false, 80.0, 200), None);
        let mut tracker = GestureSwipeTracker::default();
        tracker.begin(3, true);
        tracker.update_timed(-15.0, 0.0, u32::MAX - 5);
        tracker.update_timed(-20.0, 0.0, 2);
        assert_eq!(tracker.finish_timed(false, 80.0, 4), Some((3, "left")));
    }

    #[test]
    fn same_timestamp_updates_preserve_short_flick_and_reversal_speed() {
        let mut tracker = GestureSwipeTracker::default();
        tracker.begin(3, true);
        tracker.update_timed(-16.0, 0.0, u32::MAX - 9);
        let mut combined = tracker.clone();
        tracker.update_timed(-20.0, 0.0, 10);
        tracker.update_timed(-20.0, 0.0, 10);
        combined.update_timed(-40.0, 0.0, 10);
        assert_eq!(tracker.dx, combined.dx);
        assert_eq!(tracker.velocity(), combined.velocity());
        assert_eq!(tracker.finish_timed(false, 80.0, 10), Some((3, "left")));
        assert_eq!(combined.finish_timed(false, 80.0, 10), Some((3, "left")));

        tracker.begin(3, true);
        tracker.update_timed(-160.0, 0.0, 100);
        let mut combined = tracker.clone();
        tracker.update_timed(-60.0, 0.0, 120);
        tracker.update_timed(100.0, 0.0, 120);
        combined.update_timed(40.0, 0.0, 120);
        assert_eq!(tracker.dx, -120.0);
        assert_eq!(tracker.dx, combined.dx);
        assert_eq!(tracker.velocity(), combined.velocity());
        assert_eq!(tracker.finish_timed(false, 80.0, 120), None);
        assert_eq!(combined.finish_timed(false, 80.0, 120), None);
    }

    #[test]
    fn same_timestamp_updates_cannot_invent_speed_after_missing_or_stale_interval() {
        let mut tracker = GestureSwipeTracker::default();
        tracker.begin(3, true);
        tracker.update_timed(-20.0, 0.0, 10);
        tracker.update_timed(-20.0, 0.0, 10);
        assert_eq!(tracker.velocity(), (0.0, 0.0));
        assert_eq!(tracker.finish_timed(false, 80.0, 10), None);

        tracker.begin(3, true);
        tracker.update_timed(-12.0, 0.0, 10);
        tracker.update_timed(-8.0, 0.0, 12);
        assert!(tracker.velocity().0 < -900.0);
        tracker.update_timed(-12.0, 0.0, 100);
        tracker.update_timed(-24.0, 0.0, 100);
        assert_eq!(tracker.velocity(), (0.0, 0.0));
        assert_eq!(tracker.finish_timed(false, 80.0, 100), None);
    }

    #[test]
    fn a_fast_tiny_twitch_or_invalid_delta_cannot_commit() {
        let mut tracker = GestureSwipeTracker::default();
        tracker.begin(3, true);
        tracker.update_timed(-9.0, 0.0, 10);
        tracker.update_timed(-9.0, 0.0, 11);
        assert_eq!(tracker.finish_timed(false, 80.0, 12), None);
        tracker.begin(3, true);
        tracker.update_timed(-100.0, f64::NAN, 20);
        assert_eq!(tracker.finish_timed(false, 80.0, 21), None);
    }

    #[test]
    fn a_deliberate_fast_reversal_cancels_even_past_the_distance_threshold() {
        let mut tracker = GestureSwipeTracker::default();
        tracker.begin(3, true);
        tracker.update_timed(-110.0, 0.0, 10);
        tracker.update_timed(20.0, 0.0, 20);
        assert_eq!(tracker.dx, -90.0);
        assert_eq!(tracker.finish_timed(false, 80.0, 21), None);
    }
}
