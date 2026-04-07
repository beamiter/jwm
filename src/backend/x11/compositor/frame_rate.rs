/// Frame rate limiter and VSync control
use std::time::{Duration, Instant};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

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
            frame_budget: Arc::new(std::sync::Mutex::new(Duration::from_nanos(frame_budget as u64))),
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
        self.frame_budget.lock()
            .ok()
            .map(|f| *f)
            .unwrap_or_else(|| Duration::from_millis(16))
    }

    /// Get time since last frame
    pub fn time_since_last_frame(&self) -> Duration {
        self.last_frame.lock()
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
