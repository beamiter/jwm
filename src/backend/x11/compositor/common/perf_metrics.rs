/// Performance metrics and monitoring for the compositor
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const MAX_HISTORY: usize = 120;
const RECENT_HISTORY: usize = 30;
const NANOS_PER_SECOND: u128 = 1_000_000_000;

#[derive(Debug)]
struct TimingHistory {
    samples: VecDeque<Duration>,
    total_nanos: u128,
    recent_total_nanos: u128,
    max_history: usize,
}

impl TimingHistory {
    fn new(max_history: usize) -> Self {
        assert!(max_history > 0, "timing history must retain at least one sample");
        Self {
            samples: VecDeque::with_capacity(max_history),
            total_nanos: 0,
            recent_total_nanos: 0,
            max_history,
        }
    }

    fn push(&mut self, duration: Duration) {
        let len_before_eviction = self.samples.len();
        if len_before_eviction >= self.max_history {
            let evicted_was_recent = len_before_eviction <= RECENT_HISTORY;
            if let Some(evicted) = self.samples.pop_front() {
                let evicted_nanos = evicted.as_nanos();
                self.total_nanos = self.total_nanos.saturating_sub(evicted_nanos);
                if evicted_was_recent {
                    self.recent_total_nanos =
                        self.recent_total_nanos.saturating_sub(evicted_nanos);
                }
            }
        }

        let duration_nanos = duration.as_nanos();
        self.samples.push_back(duration);
        self.total_nanos = self.total_nanos.saturating_add(duration_nanos);
        self.recent_total_nanos = self.recent_total_nanos.saturating_add(duration_nanos);

        if self.samples.len() > RECENT_HISTORY {
            let expired_index = self.samples.len() - RECENT_HISTORY - 1;
            if let Some(expired) = self.samples.get(expired_index) {
                self.recent_total_nanos = self
                    .recent_total_nanos
                    .saturating_sub(expired.as_nanos());
            }
        }
    }

    fn average(&self) -> Duration {
        average_duration(self.total_nanos, self.samples.len())
    }

    fn recent_sample_count(&self) -> usize {
        self.samples.len().min(RECENT_HISTORY)
    }

    fn recent_fps(&self) -> f32 {
        let count = self.recent_sample_count();
        if count < 2 || self.recent_total_nanos == 0 {
            return 0.0;
        }

        let elapsed_seconds = self.recent_total_nanos as f64 / NANOS_PER_SECOND as f64;
        (count as f64 / elapsed_seconds) as f32
    }

    fn clear(&mut self) {
        self.samples.clear();
        self.total_nanos = 0;
        self.recent_total_nanos = 0;
    }
}

fn average_duration(total_nanos: u128, sample_count: usize) -> Duration {
    if sample_count == 0 {
        return Duration::ZERO;
    }
    duration_from_nanos(total_nanos / sample_count as u128)
}

fn duration_from_nanos(nanos: u128) -> Duration {
    let seconds = nanos / NANOS_PER_SECOND;
    if seconds > u64::MAX as u128 {
        return Duration::MAX;
    }

    Duration::new(
        seconds as u64,
        (nanos % NANOS_PER_SECOND) as u32,
    )
}

/// Frame timing statistics
#[derive(Clone)]
pub struct PerfMetrics {
    frame_times: Arc<Mutex<TimingHistory>>,
    compositor_times: Arc<Mutex<TimingHistory>>,
    gpu_load: Arc<AtomicU32>, // 0-100
    cpu_load: Arc<AtomicU32>, // 0-100
    frame_count: Arc<AtomicU64>,
}

impl PerfMetrics {
    pub fn new() -> Self {
        Self {
            frame_times: Arc::new(Mutex::new(TimingHistory::new(MAX_HISTORY))),
            compositor_times: Arc::new(Mutex::new(TimingHistory::new(MAX_HISTORY))),
            gpu_load: Arc::new(AtomicU32::new(0)),
            cpu_load: Arc::new(AtomicU32::new(0)),
            frame_count: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Record a frame time.
    pub fn record_frame(&self, duration: Duration) {
        if let Ok(mut times) = self.frame_times.lock() {
            times.push(duration);
        }
        self.frame_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record compositor processing time.
    pub fn record_compositor(&self, duration: Duration) {
        if let Ok(mut times) = self.compositor_times.lock() {
            times.push(duration);
        }
    }

    /// Get average frame time in O(1).
    pub fn avg_frame_time(&self) -> Duration {
        self.frame_times
            .lock()
            .map(|times| times.average())
            .unwrap_or(Duration::ZERO)
    }

    /// Get average FPS.
    pub fn avg_fps(&self) -> f32 {
        let avg = self.avg_frame_time();
        if avg.is_zero() {
            return 0.0;
        }
        1.0 / avg.as_secs_f32()
    }

    /// Get recent FPS from the last 30 frames without allocating.
    pub fn recent_fps(&self) -> f32 {
        self.frame_times
            .lock()
            .map(|times| times.recent_fps())
            .unwrap_or(0.0)
    }

    /// Set estimated GPU load (0-100).
    pub fn set_gpu_load(&self, load: u32) {
        self.gpu_load.store(load.min(100), Ordering::Relaxed);
    }

    /// Get GPU load.
    pub fn gpu_load(&self) -> u32 {
        self.gpu_load.load(Ordering::Relaxed)
    }

    /// Set estimated CPU load (0-100).
    pub fn set_cpu_load(&self, load: u32) {
        self.cpu_load.store(load.min(100), Ordering::Relaxed);
    }

    /// Get CPU load.
    pub fn cpu_load(&self) -> u32 {
        self.cpu_load.load(Ordering::Relaxed)
    }

    /// Get total frame count.
    pub fn frame_count(&self) -> u64 {
        self.frame_count.load(Ordering::Relaxed)
    }

    /// Get max frame time (worst case).
    pub fn max_frame_time(&self) -> Duration {
        self.frame_times
            .lock()
            .ok()
            .and_then(|times| times.samples.iter().copied().max())
            .unwrap_or_default()
    }

    /// Get min frame time (best case).
    pub fn min_frame_time(&self) -> Duration {
        self.frame_times
            .lock()
            .ok()
            .and_then(|times| times.samples.iter().copied().min())
            .unwrap_or_default()
    }

    /// Estimate GPU load based on frame times and target FPS.
    pub fn estimate_gpu_load(&self, target_fps: f32) -> u32 {
        if target_fps <= 0.0 {
            return 0;
        }

        let target_frame_time = 1.0 / target_fps;
        let actual_frame_time = self.avg_frame_time().as_secs_f32();
        let load = (actual_frame_time / target_frame_time * 100.0) as u32;
        load.min(100)
    }

    /// Clear all metrics.
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
        assert_eq!(m.avg_frame_time(), Duration::from_millis(20));
    }

    #[test]
    fn test_avg_fps_from_frame_times() {
        let m = PerfMetrics::new();
        m.record_frame(Duration::from_micros(16_667));
        let fps = m.avg_fps();
        assert!((fps - 60.0).abs() < 1.0, "expected ~60fps, got {fps}");
    }

    #[test]
    fn test_recent_fps_requires_at_least_two_samples() {
        let m = PerfMetrics::new();
        assert_eq!(m.recent_fps(), 0.0);
        m.record_frame(Duration::from_millis(16));
        assert_eq!(m.recent_fps(), 0.0);
        m.record_frame(Duration::from_millis(16));
        assert!(m.recent_fps() > 0.0);
    }

    #[test]
    fn test_recent_fps_uses_last_30_frames() {
        let m = PerfMetrics::new();
        for _ in 0..50 {
            m.record_frame(Duration::from_millis(100));
        }
        let fps = m.recent_fps();
        assert!((fps - 10.0).abs() < 1.0, "expected ~10fps, got {fps}");
    }

    #[test]
    fn recent_fps_excludes_older_slow_frames() {
        let m = PerfMetrics::new();
        for _ in 0..90 {
            m.record_frame(Duration::from_millis(100));
        }
        for _ in 0..30 {
            m.record_frame(Duration::from_millis(10));
        }

        let fps = m.recent_fps();
        assert!((fps - 100.0).abs() < 1.0, "expected ~100fps, got {fps}");
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
        m.record_compositor(Duration::from_millis(5));
        m.record_compositor(Duration::from_millis(8));
    }

    #[test]
    fn test_estimate_gpu_load_at_60fps() {
        let m = PerfMetrics::new();
        m.record_frame(Duration::from_micros(16_667));
        let load = m.estimate_gpu_load(60.0);
        assert!((load as i32 - 100).abs() <= 5, "expected ~100%, got {load}");
    }

    #[test]
    fn test_estimate_gpu_load_half_target() {
        let m = PerfMetrics::new();
        m.record_frame(Duration::from_micros(16_667));
        let load = m.estimate_gpu_load(120.0);
        assert!(load <= 100);
    }

    #[test]
    fn test_estimate_gpu_load_empty_returns_zero() {
        let m = PerfMetrics::new();
        assert_eq!(m.estimate_gpu_load(60.0), 0);
        assert_eq!(m.estimate_gpu_load(0.0), 0);
    }

    #[test]
    fn test_clear_resets_frame_times() {
        let m = PerfMetrics::new();
        m.record_frame(Duration::from_millis(16));
        m.record_compositor(Duration::from_millis(5));
        m.clear();
        assert_eq!(m.avg_frame_time(), Duration::ZERO);
        assert_eq!(m.avg_fps(), 0.0);
        assert_eq!(m.recent_fps(), 0.0);
    }

    #[test]
    fn test_clear_does_not_reset_frame_count() {
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
        assert_eq!(m2.frame_count(), 1);
        assert_eq!(m2.avg_frame_time(), Duration::from_millis(16));
    }

    #[test]
    fn test_history_capped_at_max() {
        let m = PerfMetrics::new();
        for _ in 0..200 {
            m.record_frame(Duration::from_millis(16));
        }
        assert_eq!(m.frame_count(), 200);
        let avg = m.avg_frame_time();
        assert!((avg.as_millis() as i64 - 16).abs() <= 1);
    }

    #[test]
    fn rolling_average_evicts_the_oldest_sample() {
        let m = PerfMetrics::new();
        for _ in 0..MAX_HISTORY {
            m.record_frame(Duration::from_millis(10));
        }
        m.record_frame(Duration::from_millis(130));
        assert_eq!(m.avg_frame_time(), Duration::from_millis(11));
    }

    #[test]
    fn test_default_equals_new() {
        let m1 = PerfMetrics::new();
        let m2 = PerfMetrics::default();
        assert_eq!(m1.frame_count(), m2.frame_count());
        assert_eq!(m1.gpu_load(), m2.gpu_load());
    }
}
