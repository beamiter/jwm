use std::collections::VecDeque;
use std::time::Duration;

const MAX_SAMPLES: usize = 300;
const RECENT_SAMPLES: usize = 30;

pub struct PerfMetrics {
    frame_times: VecDeque<Duration>,
    compositor_times: VecDeque<Duration>,
    gpu_load: u32,
    cpu_load: u32,
    frame_count: u64,
}

impl PerfMetrics {
    pub fn new() -> Self {
        Self {
            frame_times: VecDeque::with_capacity(MAX_SAMPLES),
            compositor_times: VecDeque::with_capacity(MAX_SAMPLES),
            gpu_load: 0,
            cpu_load: 0,
            frame_count: 0,
        }
    }

    pub fn record_frame(&mut self, duration: Duration) {
        if self.frame_times.len() >= MAX_SAMPLES {
            self.frame_times.pop_front();
        }
        self.frame_times.push_back(duration);
        self.frame_count += 1;
    }

    pub fn record_compositor(&mut self, duration: Duration) {
        if self.compositor_times.len() >= MAX_SAMPLES {
            self.compositor_times.pop_front();
        }
        self.compositor_times.push_back(duration);
    }

    pub fn avg_frame_time(&self) -> Duration {
        if self.frame_times.is_empty() {
            return Duration::ZERO;
        }
        let sum: Duration = self.frame_times.iter().sum();
        sum / self.frame_times.len() as u32
    }

    pub fn avg_fps(&self) -> f32 {
        let avg = self.avg_frame_time();
        if avg.is_zero() {
            return 0.0;
        }
        1.0 / avg.as_secs_f32()
    }

    pub fn recent_fps(&self) -> f32 {
        let count = self.frame_times.len().min(RECENT_SAMPLES);
        if count == 0 {
            return 0.0;
        }
        let sum: Duration = self.frame_times.iter().rev().take(count).sum();
        if sum.is_zero() {
            return 0.0;
        }
        count as f32 / sum.as_secs_f32()
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
            .iter()
            .max()
            .copied()
            .unwrap_or(Duration::ZERO)
    }

    /// Percentile of the recent frame-time window.  This intentionally copies
    /// at most 300 samples: it is only queried by IPC/HUD, never the frame
    /// hot path, and keeps the recording path allocation-free.
    pub fn frame_time_percentile(&self, percentile: f32) -> Duration {
        if self.frame_times.is_empty() {
            return Duration::ZERO;
        }
        let mut samples: Vec<Duration> = self.frame_times.iter().copied().collect();
        samples.sort_unstable();
        let p = percentile.clamp(0.0, 1.0);
        let index = ((samples.len() - 1) as f32 * p).round() as usize;
        samples[index]
    }

    pub fn min_frame_time(&self) -> Duration {
        self.frame_times
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
}
