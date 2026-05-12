use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
/// Frame rate limiter and VSync control
use std::time::{Duration, Instant};

/// Controls frame timing and VSync behavior
pub struct FrameRateLimiter {
    target_fps: Arc<AtomicU32>,
    enable_vsync: Arc<AtomicBool>,
    last_frame: Arc<std::sync::Mutex<Instant>>,
    frame_budget: Arc<std::sync::Mutex<Duration>>,
}

impl FrameRateLimiter {
    pub fn new(target_fps: u32) -> Self {
        let frame_budget = 1_000_000_000.0 / target_fps as f64;
        Self {
            target_fps: Arc::new(AtomicU32::new(target_fps)),
            enable_vsync: Arc::new(AtomicBool::new(true)),
            last_frame: Arc::new(std::sync::Mutex::new(Instant::now())),
            frame_budget: Arc::new(std::sync::Mutex::new(Duration::from_nanos(
                frame_budget as u64,
            ))),
        }
    }

    /// Set target FPS
    pub fn set_target_fps(&self, fps: u32) {
        let fps = fps.max(1).min(300); // Clamp to reasonable range
        self.target_fps.store(fps, Ordering::Relaxed);

        let frame_budget = 1_000_000_000.0 / fps as f64;
        if let Ok(mut budget) = self.frame_budget.lock() {
            *budget = Duration::from_nanos(frame_budget as u64);
        }
    }

    /// Get current target FPS
    pub fn target_fps(&self) -> u32 {
        self.target_fps.load(Ordering::Relaxed)
    }

    /// Set VSync enabled/disabled
    pub fn set_vsync(&self, enabled: bool) {
        self.enable_vsync.store(enabled, Ordering::Relaxed);
    }

    /// Get VSync status
    pub fn vsync_enabled(&self) -> bool {
        self.enable_vsync.load(Ordering::Relaxed)
    }

    /// Sleep until next frame if needed
    pub fn sleep_until_next_frame(&self) {
        if let Ok(budget) = self.frame_budget.lock() {
            if let Ok(mut last) = self.last_frame.lock() {
                let elapsed = last.elapsed();
                if elapsed < *budget {
                    let sleep_duration = *budget - elapsed;
                    std::thread::sleep(sleep_duration);
                }
                *last = Instant::now();
            }
        }
    }

    /// Mark current frame time
    pub fn mark_frame(&self) {
        if let Ok(mut last) = self.last_frame.lock() {
            *last = Instant::now();
        }
    }

    /// Get frame time budget
    pub fn frame_budget(&self) -> Duration {
        self.frame_budget
            .lock()
            .ok()
            .map(|f| *f)
            .unwrap_or_else(|| Duration::from_millis(16))
    }

    /// Get time since last frame
    pub fn time_since_last_frame(&self) -> Duration {
        self.last_frame
            .lock()
            .ok()
            .map(|t| t.elapsed())
            .unwrap_or_default()
    }

    /// Check if we should render this frame (adaptive)
    /// Returns true if enough time has passed since last frame
    pub fn should_render(&self) -> bool {
        if !self.enable_vsync.load(Ordering::Relaxed) {
            return true;
        }

        if let Ok(budget) = self.frame_budget.lock() {
            self.time_since_last_frame() >= *budget
        } else {
            true
        }
    }

    /// Reset frame timer
    pub fn reset(&self) {
        if let Ok(mut last) = self.last_frame.lock() {
            *last = Instant::now();
        }
    }
}

impl Clone for FrameRateLimiter {
    fn clone(&self) -> Self {
        Self {
            target_fps: self.target_fps.clone(),
            enable_vsync: self.enable_vsync.clone(),
            last_frame: self.last_frame.clone(),
            frame_budget: self.frame_budget.clone(),
        }
    }
}

impl Default for FrameRateLimiter {
    fn default() -> Self {
        Self::new(60)
    }
}

/// Adaptive frame rate that adjusts based on GPU/CPU load
pub struct AdaptiveFrameRate {
    limiter: FrameRateLimiter,
    min_fps: u32,
    max_fps: u32,
    current_load: Arc<std::sync::atomic::AtomicU32>,
}

impl AdaptiveFrameRate {
    pub fn new(min_fps: u32, max_fps: u32) -> Self {
        Self {
            limiter: FrameRateLimiter::new((min_fps + max_fps) / 2),
            min_fps,
            max_fps,
            current_load: Arc::new(std::sync::atomic::AtomicU32::new(50)),
        }
    }

    /// Update frame rate based on GPU/CPU load
    /// load: 0-100, higher means busier system
    pub fn update_load(&self, load: u32) {
        let load = load.min(100);
        self.current_load.store(load, Ordering::Relaxed);

        // Adaptive FPS: reduce when busy, increase when idle
        let new_fps = if load > 90 {
            self.min_fps
        } else if load > 75 {
            (self.min_fps + self.max_fps) / 4
        } else if load < 30 {
            self.max_fps
        } else if load < 50 {
            (self.min_fps + self.max_fps) * 3 / 4
        } else {
            (self.min_fps + self.max_fps) / 2
        };

        self.limiter.set_target_fps(new_fps);
    }

    /// Get current load
    pub fn current_load(&self) -> u32 {
        self.current_load.load(Ordering::Relaxed)
    }

    /// Get the underlying frame rate limiter
    pub fn limiter(&self) -> &FrameRateLimiter {
        &self.limiter
    }
}

impl Clone for AdaptiveFrameRate {
    fn clone(&self) -> Self {
        Self {
            limiter: self.limiter.clone(),
            min_fps: self.min_fps,
            max_fps: self.max_fps,
            current_load: self.current_load.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frame_rate_limiter_new() {
        let limiter = FrameRateLimiter::new(60);
        assert_eq!(limiter.target_fps(), 60);
        assert!(limiter.vsync_enabled());
    }

    #[test]
    fn test_frame_rate_limiter_default() {
        let limiter = FrameRateLimiter::default();
        assert_eq!(limiter.target_fps(), 60);
        assert!(limiter.vsync_enabled());
    }

    #[test]
    fn test_set_target_fps() {
        let limiter = FrameRateLimiter::new(60);
        limiter.set_target_fps(120);
        assert_eq!(limiter.target_fps(), 120);

        limiter.set_target_fps(30);
        assert_eq!(limiter.target_fps(), 30);
    }

    #[test]
    fn test_target_fps_clamping() {
        let limiter = FrameRateLimiter::new(60);

        limiter.set_target_fps(0);
        assert_eq!(limiter.target_fps(), 1, "FPS should be clamped to minimum of 1");

        limiter.set_target_fps(500);
        assert_eq!(limiter.target_fps(), 300, "FPS should be clamped to maximum of 300");
    }

    #[test]
    fn test_frame_budget_calculation() {
        let limiter = FrameRateLimiter::new(60);
        let budget = limiter.frame_budget();

        let expected_nanos = 1_000_000_000u64 / 60;
        assert!(
            (budget.as_nanos() as u64).abs_diff(expected_nanos) < 1000,
            "Frame budget for 60 FPS should be ~16.67ms"
        );
    }

    #[test]
    fn test_vsync_control() {
        let limiter = FrameRateLimiter::new(60);
        assert!(limiter.vsync_enabled());

        limiter.set_vsync(false);
        assert!(!limiter.vsync_enabled());

        limiter.set_vsync(true);
        assert!(limiter.vsync_enabled());
    }

    #[test]
    fn test_should_render_with_vsync_disabled() {
        let limiter = FrameRateLimiter::new(60);
        limiter.set_vsync(false);
        assert!(limiter.should_render(), "Should always render when VSync is disabled");
    }

    #[test]
    fn test_mark_frame() {
        let limiter = FrameRateLimiter::new(60);
        limiter.mark_frame();

        let time_since = limiter.time_since_last_frame();
        assert!(time_since.as_millis() < 10, "Time since frame should be very small");
    }

    #[test]
    fn test_reset() {
        let limiter = FrameRateLimiter::new(60);
        let _ = std::thread::sleep(Duration::from_millis(5));
        limiter.reset();

        let time_since = limiter.time_since_last_frame();
        assert!(time_since.as_millis() < 10, "Time since reset should be small");
    }

    #[test]
    fn test_frame_rate_limiter_clone() {
        let limiter1 = FrameRateLimiter::new(60);
        limiter1.set_target_fps(120);

        let limiter2 = limiter1.clone();
        assert_eq!(limiter2.target_fps(), 120, "Clone should share state");
        assert_eq!(limiter1.target_fps(), limiter2.target_fps());
    }

    #[test]
    fn test_adaptive_frame_rate_new() {
        let adaptive = AdaptiveFrameRate::new(30, 120);
        assert_eq!(adaptive.current_load(), 50);

        let fps = adaptive.limiter().target_fps();
        assert_eq!(fps, 75, "Initial FPS should be average of min and max");
    }

    #[test]
    fn test_adaptive_frame_rate_high_load() {
        let adaptive = AdaptiveFrameRate::new(30, 120);

        adaptive.update_load(95);
        assert_eq!(adaptive.current_load(), 95);
        assert_eq!(
            adaptive.limiter().target_fps(),
            30,
            "High load should reduce FPS to minimum"
        );
    }

    #[test]
    fn test_adaptive_frame_rate_medium_high_load() {
        let adaptive = AdaptiveFrameRate::new(30, 120);

        adaptive.update_load(80);
        assert_eq!(adaptive.current_load(), 80);
        let fps = adaptive.limiter().target_fps();
        assert!(fps >= 30 && fps < 75, "Medium-high load should reduce FPS");
    }

    #[test]
    fn test_adaptive_frame_rate_low_load() {
        let adaptive = AdaptiveFrameRate::new(30, 120);

        adaptive.update_load(10);
        assert_eq!(adaptive.current_load(), 10);
        assert_eq!(
            adaptive.limiter().target_fps(),
            120,
            "Low load should maximize FPS"
        );
    }

    #[test]
    fn test_adaptive_frame_rate_medium_low_load() {
        let adaptive = AdaptiveFrameRate::new(30, 120);

        adaptive.update_load(40);
        assert_eq!(adaptive.current_load(), 40);
        let fps = adaptive.limiter().target_fps();
        assert!(fps > 75, "Medium-low load should increase FPS towards maximum");
    }

    #[test]
    fn test_adaptive_frame_rate_load_clamping() {
        let adaptive = AdaptiveFrameRate::new(30, 120);

        adaptive.update_load(150);
        assert_eq!(adaptive.current_load(), 100, "Load should be clamped to 100");
    }

    #[test]
    fn test_adaptive_frame_rate_clone() {
        let adaptive1 = AdaptiveFrameRate::new(30, 120);
        adaptive1.update_load(75);

        let adaptive2 = adaptive1.clone();
        assert_eq!(adaptive2.current_load(), 75, "Clone should share load state");
    }

    #[test]
    fn test_frame_budget_different_fps() {
        let limiter_60 = FrameRateLimiter::new(60);
        let limiter_120 = FrameRateLimiter::new(120);

        let budget_60 = limiter_60.frame_budget();
        let budget_120 = limiter_120.frame_budget();

        assert!(
            budget_60 > budget_120,
            "60 FPS should have larger frame budget than 120 FPS"
        );
        assert!(budget_60.as_nanos() > budget_120.as_nanos() * 1_900_000_000 / 1_000_000_000);
    }
}
