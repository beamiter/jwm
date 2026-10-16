//! Backend-independent swipe recognition. Commit only on a completed gesture,
//! never on updates, so cancellation and a long swipe cannot switch twice.

#[derive(Debug, Default, Clone)]
pub struct GestureSwipeTracker {
    pub fingers: u32,
    pub intercept: bool,
    pub config_signature: String,
    pub dx: f64,
    pub dy: f64,
}

impl GestureSwipeTracker {
    pub fn begin(&mut self, fingers: u32, intercept: bool) {
        *self = Self {
            fingers,
            intercept,
            config_signature: String::new(),
            dx: 0.0,
            dy: 0.0,
        };
    }

    pub fn update(&mut self, dx: f64, dy: f64) {
        if self.intercept {
            self.dx += dx;
            self.dy += dy;
        }
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
}
