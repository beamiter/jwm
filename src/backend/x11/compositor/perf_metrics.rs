/// Performance metrics and monitoring for the compositor
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Frame timing statistics
#[derive(Clone)]
pub struct PerfMetrics {
    frame_times: Arc<Mutex<VecDeque<Duration>>>,
    compositor_times: Arc<Mutex<VecDeque<Duration>>>,
    gpu_load: Arc<AtomicU32>, // 0-100
    cpu_load: Arc<AtomicU32>, // 0-100
    frame_count: Arc<AtomicU64>,
    max_history: usize,
}

impl PerfMetrics {
    pub fn new() -> Self {
        Self {
            frame_times: Arc::new(Mutex::new(VecDeque::with_capacity(120))),
            compositor_times: Arc::new(Mutex::new(VecDeque::with_capacity(120))),
            gpu_load: Arc::new(AtomicU32::new(0)),
            cpu_load: Arc::new(AtomicU32::new(0)),
            frame_count: Arc::new(AtomicU64::new(0)),
            max_history: 120,
        }
    }

    /// Record a frame time
    pub fn record_frame(&self, duration: Duration) {
        if let Ok(mut times) = self.frame_times.lock() {
            times.push_back(duration);
            if times.len() > self.max_history {
                times.pop_front();
            }
        }
        self.frame_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record compositor processing time
    pub fn record_compositor(&self, duration: Duration) {
        if let Ok(mut times) = self.compositor_times.lock() {
            times.push_back(duration);
            if times.len() > self.max_history {
                times.pop_front();
            }
        }
    }

    /// Get average frame time
    pub fn avg_frame_time(&self) -> Duration {
        if let Ok(times) = self.frame_times.lock() {
            if times.is_empty() {
                return Duration::ZERO;
            }
            let sum: Duration = times.iter().sum();
            sum / times.len() as u32
        } else {
            Duration::ZERO
        }
    }

    /// Get average FPS
    pub fn avg_fps(&self) -> f32 {
        let avg = self.avg_frame_time();
        if avg.is_zero() {
            return 0.0;
        }
        1.0 / avg.as_secs_f32()
    }

    /// Get recent FPS (last 30 frames)
    pub fn recent_fps(&self) -> f32 {
        if let Ok(times) = self.frame_times.lock() {
            if times.len() < 2 {
                return 0.0;
            }
            let recent: Vec<_> = times.iter().rev().take(30).copied().collect();
            if recent.is_empty() {
                return 0.0;
            }
            let sum: Duration = recent.iter().sum();
            1.0 / (sum / recent.len() as u32).as_secs_f32()
        } else {
            0.0
        }
    }

    /// Set estimated GPU load (0-100)
    pub fn set_gpu_load(&self, load: u32) {
        self.gpu_load.store(load.min(100), Ordering::Relaxed);
    }

    /// Get GPU load
    pub fn gpu_load(&self) -> u32 {
        self.gpu_load.load(Ordering::Relaxed)
    }

    /// Set estimated CPU load (0-100)
    pub fn set_cpu_load(&self, load: u32) {
        self.cpu_load.store(load.min(100), Ordering::Relaxed);
    }

    /// Get CPU load
    pub fn cpu_load(&self) -> u32 {
        self.cpu_load.load(Ordering::Relaxed)
    }

    /// Get total frame count
    pub fn frame_count(&self) -> u64 {
        self.frame_count.load(Ordering::Relaxed)
    }

    /// Get max frame time (worst case)
    pub fn max_frame_time(&self) -> Duration {
        self.frame_times
            .lock()
            .ok()
            .and_then(|t| t.iter().copied().max())
            .unwrap_or_default()
    }

    /// Get min frame time (best case)
    pub fn min_frame_time(&self) -> Duration {
        self.frame_times
            .lock()
            .ok()
            .and_then(|t| t.iter().copied().min())
            .unwrap_or_default()
    }

    /// Estimate GPU load based on frame times and target FPS
    pub fn estimate_gpu_load(&self, target_fps: f32) -> u32 {
        let target_frame_time = 1.0 / target_fps;
        let actual_frame_time = self.avg_frame_time().as_secs_f32();
        let load = (actual_frame_time / target_frame_time * 100.0) as u32;
        load.min(100)
    }

    /// Clear all metrics
    pub fn clear(&self) {
        if let Ok(mut times) = self.frame_times.lock() {
            times.clear();
        }

        if let Ok(mut times) = self.compositor_times.lock() {
            times.clear();
        }
    }
}

impl Default for PerfMetrics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_metrics_are_empty() {
        let m = PerfMetrics::new();
        assert_eq!(m.frame_count(), 0);
        assert_eq!(m.avg_fps(), 0.0);
        assert_eq!(m.avg_frame_time(), Duration::ZERO);
        assert_eq!(m.recent_fps(), 0.0);
        assert_eq!(m.max_frame_time(), Duration::ZERO);
        assert_eq!(m.min_frame_time(), Duration::ZERO);
        assert_eq!(m.gpu_load(), 0);
        assert_eq!(m.cpu_load(), 0);
    }

    #[test]
    fn test_record_frame_increments_count() {
        let m = PerfMetrics::new();
        m.record_frame(Duration::from_millis(16));
        m.record_frame(Duration::from_millis(16));
        assert_eq!(m.frame_count(), 2);
    }

    #[test]
    fn test_avg_frame_time_single() {
        let m = PerfMetrics::new();
        m.record_frame(Duration::from_millis(20));
        assert_eq!(m.avg_frame_time(), Duration::from_millis(20));
    }

    #[test]
    fn test_avg_frame_time_multiple() {
        let m = PerfMetrics::new();
        m.record_frame(Duration::from_millis(10));
        m.record_frame(Duration::from_millis(30));
        // avg = 20ms
        assert_eq!(m.avg_frame_time(), Duration::from_millis(20));
    }

    #[test]
    fn test_avg_fps_from_frame_times() {
        let m = PerfMetrics::new();
        // 16.666ms ≈ 60fps
        m.record_frame(Duration::from_micros(16_667));
        let fps = m.avg_fps();
        assert!((fps - 60.0).abs() < 1.0, "expected ~60fps, got {fps}");
    }

    #[test]
    fn test_recent_fps_requires_at_least_two_samples() {
        let m = PerfMetrics::new();
        // 0 samples
        assert_eq!(m.recent_fps(), 0.0);
        // 1 sample still returns 0
        m.record_frame(Duration::from_millis(16));
        assert_eq!(m.recent_fps(), 0.0);
        // 2 samples → valid fps
        m.record_frame(Duration::from_millis(16));
        assert!(m.recent_fps() > 0.0);
    }

    #[test]
    fn test_recent_fps_uses_last_30_frames() {
        let m = PerfMetrics::new();
        // 50 frames at 100ms → 10fps
        for _ in 0..50 {
            m.record_frame(Duration::from_millis(100));
        }
        let fps = m.recent_fps();
        assert!((fps - 10.0).abs() < 1.0, "expected ~10fps, got {fps}");
    }

    #[test]
    fn test_gpu_load_clamped_to_100() {
        let m = PerfMetrics::new();
        m.set_gpu_load(150);
        assert_eq!(m.gpu_load(), 100);
    }

    #[test]
    fn test_gpu_load_round_trip() {
        let m = PerfMetrics::new();
        m.set_gpu_load(75);
        assert_eq!(m.gpu_load(), 75);
    }

    #[test]
    fn test_cpu_load_clamped_to_100() {
        let m = PerfMetrics::new();
        m.set_cpu_load(999);
        assert_eq!(m.cpu_load(), 100);
    }

    #[test]
    fn test_cpu_load_round_trip() {
        let m = PerfMetrics::new();
        m.set_cpu_load(42);
        assert_eq!(m.cpu_load(), 42);
    }

    #[test]
    fn test_max_frame_time() {
        let m = PerfMetrics::new();
        m.record_frame(Duration::from_millis(10));
        m.record_frame(Duration::from_millis(50));
        m.record_frame(Duration::from_millis(20));
        assert_eq!(m.max_frame_time(), Duration::from_millis(50));
    }

    #[test]
    fn test_min_frame_time() {
        let m = PerfMetrics::new();
        m.record_frame(Duration::from_millis(10));
        m.record_frame(Duration::from_millis(50));
        m.record_frame(Duration::from_millis(20));
        assert_eq!(m.min_frame_time(), Duration::from_millis(10));
    }

    #[test]
    fn test_record_compositor_time() {
        let m = PerfMetrics::new();
        // should not panic
        m.record_compositor(Duration::from_millis(5));
        m.record_compositor(Duration::from_millis(8));
    }

    #[test]
    fn test_estimate_gpu_load_at_60fps() {
        let m = PerfMetrics::new();
        // At 60fps target, a 16.67ms frame → 100% load
        m.record_frame(Duration::from_micros(16_667));
        let load = m.estimate_gpu_load(60.0);
        assert!((load as i32 - 100).abs() <= 5, "expected ~100%, got {load}");
    }

    #[test]
    fn test_estimate_gpu_load_half_target() {
        let m = PerfMetrics::new();
        // At 120fps target, a 16.67ms frame → 200%, clamped to 100%
        m.record_frame(Duration::from_micros(16_667));
        let load = m.estimate_gpu_load(120.0);
        assert!(load <= 100);
    }

    #[test]
    fn test_estimate_gpu_load_empty_returns_zero() {
        let m = PerfMetrics::new();
        assert_eq!(m.estimate_gpu_load(60.0), 0);
    }

    #[test]
    fn test_clear_resets_frame_times() {
        let m = PerfMetrics::new();
        m.record_frame(Duration::from_millis(16));
        m.record_compositor(Duration::from_millis(5));
        m.clear();
        assert_eq!(m.avg_frame_time(), Duration::ZERO);
        assert_eq!(m.avg_fps(), 0.0);
    }

    #[test]
    fn test_clear_does_not_reset_frame_count() {
        // frame_count uses an atomic that is not cleared by clear()
        let m = PerfMetrics::new();
        m.record_frame(Duration::from_millis(16));
        m.clear();
        assert_eq!(m.frame_count(), 1);
    }

    #[test]
    fn test_clone_shares_state() {
        let m = PerfMetrics::new();
        let m2 = m.clone();
        m.record_frame(Duration::from_millis(16));
        // clone shares Arc, so both see the update
        assert_eq!(m2.frame_count(), 1);
        assert_eq!(m2.avg_frame_time(), Duration::from_millis(16));
    }

    #[test]
    fn test_history_capped_at_max() {
        let m = PerfMetrics::new();
        // max_history = 120; insert 200 frames
        for _ in 0..200 {
            m.record_frame(Duration::from_millis(16));
        }
        assert_eq!(m.frame_count(), 200);
        // avg should still be ~16ms (all equal)
        let avg = m.avg_frame_time();
        assert!((avg.as_millis() as i64 - 16).abs() <= 1);
    }

    #[test]
    fn test_default_equals_new() {
        let m1 = PerfMetrics::new();
        let m2 = PerfMetrics::default();
        assert_eq!(m1.frame_count(), m2.frame_count());
        assert_eq!(m1.gpu_load(), m2.gpu_load());
    }
}
