use std::thread;
use std::time::{Duration, Instant};

/// Frame rate limiter that enforces a target FPS via sleep-based throttling.
pub struct FrameRateLimiter {
    target_fps: u32,
    vsync_enabled: bool,
    last_frame: Instant,
    frame_budget: Duration,
}

impl FrameRateLimiter {
    /// Create a new limiter targeting the given frames per second.
    pub fn new(fps: u32) -> Self {
        let fps = fps.clamp(1, 300);
        Self {
            target_fps: fps,
            vsync_enabled: false,
            last_frame: Instant::now(),
            frame_budget: Duration::from_secs_f64(1.0 / fps as f64),
        }
    }

    /// Set target FPS, clamped to the range 1..=300.
    pub fn set_target_fps(&mut self, fps: u32) {
        let fps = fps.clamp(1, 300);
        self.target_fps = fps;
        self.frame_budget = Duration::from_secs_f64(1.0 / fps as f64);
    }

    /// Returns the current target FPS.
    pub fn target_fps(&self) -> u32 {
        self.target_fps
    }

    /// Enable or disable vsync awareness.
    pub fn set_vsync(&mut self, enabled: bool) {
        self.vsync_enabled = enabled;
    }

    /// Returns whether vsync is enabled.
    pub fn vsync_enabled(&self) -> bool {
        self.vsync_enabled
    }

    /// Sleep the calling thread until the next frame budget boundary.
    ///
    /// If the elapsed time since the last frame is already past the budget,
    /// this returns immediately without sleeping.
    pub fn sleep_until_next_frame(&self) {
        let elapsed = self.last_frame.elapsed();
        if elapsed < self.frame_budget {
            thread::sleep(self.frame_budget - elapsed);
        }
    }

    /// Mark the current instant as a completed frame.
    pub fn mark_frame(&mut self) {
        self.last_frame = Instant::now();
    }

    /// Returns `true` if enough time has passed since the last frame to
    /// warrant rendering a new one.
    pub fn should_render(&self) -> bool {
        self.last_frame.elapsed() >= self.frame_budget
    }

    /// Returns the duration of one frame at the current target FPS.
    pub fn frame_budget(&self) -> Duration {
        self.frame_budget
    }

    /// Returns the time elapsed since the last frame was marked.
    pub fn time_since_last_frame(&self) -> Duration {
        self.last_frame.elapsed()
    }

    /// Reset the limiter, treating this instant as the last frame time.
    pub fn reset(&mut self) {
        self.last_frame = Instant::now();
    }
}

/// Adaptive frame rate controller that adjusts FPS based on system load.
///
/// Load is expressed as a percentage (0-100). At 0% load the limiter runs at
/// `max_fps`; at 100% load it drops to `min_fps`. Intermediate values are
/// linearly interpolated.
pub struct AdaptiveFrameRate {
    limiter: FrameRateLimiter,
    min_fps: u32,
    max_fps: u32,
    current_load: u32,
}

impl AdaptiveFrameRate {
    /// Create an adaptive controller with the given FPS bounds.
    ///
    /// The limiter starts at `max_fps` (assuming zero load).
    pub fn new(min_fps: u32, max_fps: u32) -> Self {
        let min_fps = min_fps.clamp(1, 300);
        let max_fps = max_fps.clamp(min_fps, 300);
        Self {
            limiter: FrameRateLimiter::new(max_fps),
            min_fps,
            max_fps,
            current_load: 0,
        }
    }

    /// Update the current system load (0-100%) and adjust the target FPS
    /// accordingly.
    ///
    /// The mapping is linear: 0% load -> `max_fps`, 100% load -> `min_fps`.
    pub fn update_load(&mut self, load_percent: u32) {
        let load = load_percent.min(100);
        self.current_load = load;

        // Linear interpolation: fps = max - (max - min) * load / 100
        let range = self.max_fps - self.min_fps;
        let fps = self.max_fps - (range * load) / 100;
        self.limiter.set_target_fps(fps);
    }

    /// Returns the last reported load percentage.
    pub fn current_load(&self) -> u32 {
        self.current_load
    }

    /// Borrow the underlying frame rate limiter.
    pub fn limiter(&self) -> &FrameRateLimiter {
        &self.limiter
    }

    /// Mutably borrow the underlying frame rate limiter.
    pub fn limiter_mut(&mut self) -> &mut FrameRateLimiter {
        &mut self.limiter
    }
}
