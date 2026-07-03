//! Backend-independent compositor frame timing state.

use std::collections::VecDeque;
use std::time::Instant;

/// Frame timing statistics for debug HUDs and lightweight telemetry.
pub(crate) struct FrameStats {
    pub(crate) frame_count: u64,
    pub(crate) last_fps_update: Instant,
    pub(crate) fps: f32,
    pub(crate) frame_times: VecDeque<f32>,
    pub(crate) last_frame_time: Instant,
    pub(crate) draw_calls: u32,
    pub(crate) texture_memory_bytes: u64,
    pub(crate) blur_cache_hits: u64,
    pub(crate) blur_cache_misses: u64,
    pub(crate) last_input_time: Option<Instant>,
    pub(crate) latency_samples: VecDeque<f32>,
}

impl FrameStats {
    pub(crate) fn new() -> Self {
        let now = Instant::now();
        Self {
            frame_count: 0,
            last_fps_update: now,
            fps: 0.0,
            frame_times: VecDeque::with_capacity(120),
            last_frame_time: now,
            draw_calls: 0,
            texture_memory_bytes: 0,
            blur_cache_hits: 0,
            blur_cache_misses: 0,
            last_input_time: None,
            latency_samples: VecDeque::with_capacity(300),
        }
    }
}

impl Default for FrameStats {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::FrameStats;

    #[test]
    fn new_stats_start_empty_with_expected_capacities() {
        let stats = FrameStats::new();
        assert_eq!(stats.frame_count, 0);
        assert_eq!(stats.fps, 0.0);
        assert_eq!(stats.frame_times.capacity(), 120);
        assert_eq!(stats.latency_samples.capacity(), 300);
        assert!(stats.last_input_time.is_none());
    }
}
