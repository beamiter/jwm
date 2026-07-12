use std::collections::VecDeque;
use std::time::Duration;

const MAX_SAMPLES: usize = 300;
const RECENT_SAMPLES: usize = 30;
const NANOS_PER_SECOND: u128 = 1_000_000_000;

struct TimingHistory {
    samples: VecDeque<Duration>,
    total_nanos: u128,
    recent_total_nanos: u128,
}

impl TimingHistory {
    fn new() -> Self {
        Self {
            samples: VecDeque::with_capacity(MAX_SAMPLES),
            total_nanos: 0,
            recent_total_nanos: 0,
        }
    }

    fn push(&mut self, duration: Duration) {
        let len_before_eviction = self.samples.len();
        if len_before_eviction >= MAX_SAMPLES {
            let evicted_was_recent = len_before_eviction <= RECENT_SAMPLES;
            if let Some(evicted) = self.samples.pop_front() {
                let evicted_nanos = evicted.as_nanos();
                self.total_nanos = self.total_nanos.saturating_sub(evicted_nanos);
                if evicted_was_recent {
                    self.recent_total_nanos = self.recent_total_nanos.saturating_sub(evicted_nanos);
                }
            }
        }

        let duration_nanos = duration.as_nanos();
        self.samples.push_back(duration);
        self.total_nanos = self.total_nanos.saturating_add(duration_nanos);
        self.recent_total_nanos = self.recent_total_nanos.saturating_add(duration_nanos);

        if self.samples.len() > RECENT_SAMPLES {
            let expired_index = self.samples.len() - RECENT_SAMPLES - 1;
            if let Some(expired) = self.samples.get(expired_index) {
                self.recent_total_nanos =
                    self.recent_total_nanos.saturating_sub(expired.as_nanos());
            }
        }
    }

    fn average(&self) -> Duration {
        average_duration(self.total_nanos, self.samples.len())
    }

    fn recent_fps(&self) -> f32 {
        let count = self.samples.len().min(RECENT_SAMPLES);
        if count == 0 || self.recent_total_nanos == 0 {
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

    Duration::new(seconds as u64, (nanos % NANOS_PER_SECOND) as u32)
}

pub struct PerfMetrics {
    frame_times: TimingHistory,
    compositor_times: TimingHistory,
    gpu_load: u32,
    cpu_load: u32,
    frame_count: u64,
}

impl PerfMetrics {
    pub fn new() -> Self {
        Self {
            frame_times: TimingHistory::new(),
            compositor_times: TimingHistory::new(),
            gpu_load: 0,
            cpu_load: 0,
            frame_count: 0,
        }
    }

    pub fn record_frame(&mut self, duration: Duration) {
        self.frame_times.push(duration);
        self.frame_count += 1;
    }

    pub fn record_compositor(&mut self, duration: Duration) {
        self.compositor_times.push(duration);
    }

    pub fn avg_frame_time(&self) -> Duration {
        self.frame_times.average()
    }

    pub fn avg_fps(&self) -> f32 {
        let avg = self.avg_frame_time();
        if avg.is_zero() {
            return 0.0;
        }
        1.0 / avg.as_secs_f32()
    }

    pub fn recent_fps(&self) -> f32 {
        self.frame_times.recent_fps()
    }

    pub fn set_gpu_load(&mut self, load: u32) {
        self.gpu_load = load;
    }

    pub fn gpu_load(&self) -> u32 {
        self.gpu_load
    }

    pub fn set_cpu_load(&mut self, load: u32) {
        self.cpu_load = load;
    }

    pub fn cpu_load(&self) -> u32 {
        self.cpu_load
    }

    pub fn frame_count(&self) -> u64 {
        self.frame_count
    }

    pub fn max_frame_time(&self) -> Duration {
        self.frame_times
            .samples
            .iter()
            .max()
            .copied()
            .unwrap_or(Duration::ZERO)
    }

    /// Percentile of the recent frame-time window. This intentionally copies
    /// at most 300 samples: it is only queried by IPC/HUD, never the frame
    /// hot path, and keeps the recording path allocation-free.
    pub fn frame_time_percentile(&self, percentile: f32) -> Duration {
        if self.frame_times.samples.is_empty() {
            return Duration::ZERO;
        }
        let mut samples: Vec<Duration> = self.frame_times.samples.iter().copied().collect();
        samples.sort_unstable();
        let p = percentile.clamp(0.0, 1.0);
        let index = ((samples.len() - 1) as f32 * p).round() as usize;
        samples[index]
    }

    pub fn min_frame_time(&self) -> Duration {
        self.frame_times
            .samples
            .iter()
            .min()
            .copied()
            .unwrap_or(Duration::ZERO)
    }

    pub fn estimate_gpu_load(&self, target_fps: f32) -> u32 {
        if target_fps <= 0.0 {
            return 0;
        }
        let avg_ms = self.avg_frame_time().as_secs_f32() * 1000.0;
        let target_ms = 1000.0 / target_fps;
        ((avg_ms / target_ms) * 100.0) as u32
    }

    pub fn clear(&mut self) {
        self.frame_times.clear();
        self.compositor_times.clear();
        self.gpu_load = 0;
        self.cpu_load = 0;
        self.frame_count = 0;
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
    fn percentile_uses_bounded_recent_samples() {
        let mut metrics = PerfMetrics::new();
        for ms in 1..=100 {
            metrics.record_frame(Duration::from_millis(ms));
        }

        assert_eq!(metrics.frame_time_percentile(0.0), Duration::from_millis(1));
        assert_eq!(
            metrics.frame_time_percentile(1.0),
            Duration::from_millis(100)
        );
        assert_eq!(
            metrics.frame_time_percentile(0.95),
            Duration::from_millis(95)
        );
    }

    #[test]
    fn rolling_average_evicts_oldest_sample() {
        let mut metrics = PerfMetrics::new();
        for _ in 0..MAX_SAMPLES {
            metrics.record_frame(Duration::from_millis(10));
        }
        metrics.record_frame(Duration::from_millis(310));

        assert_eq!(metrics.avg_frame_time(), Duration::from_millis(11));
    }

    #[test]
    fn recent_fps_excludes_older_slow_frames() {
        let mut metrics = PerfMetrics::new();
        for _ in 0..100 {
            metrics.record_frame(Duration::from_millis(100));
        }
        for _ in 0..RECENT_SAMPLES {
            metrics.record_frame(Duration::from_millis(10));
        }

        let fps = metrics.recent_fps();
        assert!((fps - 100.0).abs() < 1.0, "expected ~100fps, got {fps}");
    }

    #[test]
    fn clear_resets_cached_totals() {
        let mut metrics = PerfMetrics::new();
        metrics.record_frame(Duration::from_millis(16));
        metrics.record_compositor(Duration::from_millis(4));
        metrics.clear();

        assert_eq!(metrics.avg_frame_time(), Duration::ZERO);
        assert_eq!(metrics.recent_fps(), 0.0);
        assert_eq!(metrics.frame_count(), 0);
    }
}
