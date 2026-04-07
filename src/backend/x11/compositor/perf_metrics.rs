/// Performance metrics and monitoring for the compositor
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Frame timing statistics
#[derive(Clone)]
pub struct PerfMetrics {
    frame_times: Arc<Mutex<VecDeque<Duration>>>,
    compositor_times: Arc<Mutex<VecDeque<Duration>>>,
    gpu_load: Arc<AtomicU32>,  // 0-100
    cpu_load: Arc<AtomicU32>,  // 0-100
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
        self.frame_times.lock()
            .ok()
            .and_then(|t| t.iter().copied().max())
            .unwrap_or_default()
    }

    /// Get min frame time (best case)
    pub fn min_frame_time(&self) -> Duration {
        self.frame_times.lock()
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
