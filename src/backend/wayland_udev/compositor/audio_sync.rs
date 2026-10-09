use std::collections::HashMap;
use std::time::{Duration, Instant};

pub(crate) struct AudioStreamTiming {
    pub fps: f32,
    pub buffer_latency_ms: u32,
    pub drift_ema: f32,
    pub pts_offset_ns: i64,
    pub dynamic_latency_ms: f32,
    pub frames_rendered: u64,
    pub last_frame_time: Instant,
    pub next_frame_deadline: Instant,
    pub max_observed_gap_ms: f32,
}

pub(crate) struct AudioSyncManager {
    streams: HashMap<u64, AudioStreamTiming>,
    fallback_timeout_ms: f32,
}

impl AudioSyncManager {
    pub(crate) fn new() -> Self {
        Self {
            streams: HashMap::new(),
            fallback_timeout_ms: 100.0,
        }
    }

    fn frame_deadline(now: Instant, fps: f32) -> Option<Instant> {
        if !fps.is_finite() || fps <= 0.0 {
            return None;
        }
        let interval = Duration::try_from_secs_f64(1.0 / f64::from(fps)).ok()?;
        now.checked_add(interval)
    }

    /// Register a new audio stream for the given window with initial fps and latency.
    /// Invalid or unrepresentable rates are ignored, preserving any existing stream.
    pub(crate) fn register_stream(&mut self, window_id: u64, fps: f32, latency_ms: u32) {
        let now = Instant::now();
        let Some(next_frame_deadline) = Self::frame_deadline(now, fps) else {
            return;
        };

        self.streams.insert(
            window_id,
            AudioStreamTiming {
                fps,
                buffer_latency_ms: latency_ms,
                drift_ema: 0.0,
                pts_offset_ns: 0,
                dynamic_latency_ms: latency_ms as f32,
                frames_rendered: 0,
                last_frame_time: now,
                next_frame_deadline,
                max_observed_gap_ms: 0.0,
            },
        );
    }

    /// Remove the audio stream for the given window.
    pub(crate) fn unregister_stream(&mut self, window_id: u64) {
        self.streams.remove(&window_id);
    }

    /// Update stream timing parameters and drift EMA (exponential moving average, alpha=0.1).
    /// Tracks the maximum observed gap between updates.
    /// Invalid or unrepresentable rates leave the entire previous timing state unchanged.
    pub(crate) fn update_stream(&mut self, window_id: u64, fps: f32, latency_ms: u32) {
        let now = Instant::now();
        let Some(next_frame_deadline) = Self::frame_deadline(now, fps) else {
            return;
        };
        if let Some(stream) = self.streams.get_mut(&window_id) {
            let gap_ms = now.duration_since(stream.last_frame_time).as_secs_f32() * 1000.0;

            // Track max observed gap
            if gap_ms > stream.max_observed_gap_ms {
                stream.max_observed_gap_ms = gap_ms;
            }

            // Compute drift: difference between expected and actual frame interval
            let expected_interval_ms = 1000.0 / stream.fps;
            let drift = gap_ms - expected_interval_ms;

            // Exponential moving average with alpha = 0.1
            let alpha = 0.1_f32;
            stream.drift_ema = stream.drift_ema * (1.0 - alpha) + drift * alpha;

            // Update dynamic latency based on drift
            stream.dynamic_latency_ms = latency_ms as f32 + stream.drift_ema.abs();

            stream.fps = fps;
            stream.buffer_latency_ms = latency_ms;
            stream.last_frame_time = now;

            // Recompute next frame deadline
            stream.next_frame_deadline = next_frame_deadline;
        }
    }

    /// Returns true if the window should render now (current time >= next_frame_deadline).
    pub(crate) fn should_render(&self, window_id: u64) -> bool {
        if let Some(stream) = self.streams.get(&window_id) {
            Instant::now() >= stream.next_frame_deadline
        } else {
            true // No stream registered, always render
        }
    }

    /// Mark a frame as rendered: update timing and compute the next deadline.
    pub(crate) fn mark_frame_rendered(&mut self, window_id: u64) {
        if let Some(stream) = self.streams.get_mut(&window_id) {
            let now = Instant::now();
            let Some(next_frame_deadline) = Self::frame_deadline(now, stream.fps) else {
                return;
            };
            stream.last_frame_time = now;
            stream.frames_rendered += 1;

            stream.next_frame_deadline = next_frame_deadline;
        }
    }

    /// Get a reference to the timing state for the given window.
    pub(crate) fn get_timing(&self, window_id: u64) -> Option<&AudioStreamTiming> {
        self.streams.get(&window_id)
    }

    /// Return the number of active audio streams.
    pub(crate) fn active_streams(&self) -> usize {
        self.streams.len()
    }

    /// Returns true if no timing update has been received within
    /// fallback_timeout + 3 * max_observed_gap, indicating the stream may be stalled.
    pub(crate) fn should_fallback(&self, window_id: u64) -> bool {
        if let Some(stream) = self.streams.get(&window_id) {
            let timeout_ms = self.fallback_timeout_ms + 3.0 * stream.max_observed_gap_ms;
            let elapsed_ms = stream.last_frame_time.elapsed().as_secs_f32() * 1000.0;
            elapsed_ms > timeout_ms
        } else {
            true
        }
    }
}
#[cfg(test)]
mod input_contract_tests {
    use super::*;
    const INVALID: [f32; 6] = [
        0.0,
        -1.0,
        f32::NAN,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::MIN_POSITIVE,
    ];
    #[test]
    fn invalid_registration_is_ignored_and_preserves_existing_stream() {
        let mut manager = AudioSyncManager::new();
        for fps in INVALID {
            manager.register_stream(1, fps, 50);
            assert_eq!(manager.active_streams(), 0);
            assert!(manager.should_render(1));
        }
        manager.register_stream(1, 60.0, 20);
        let original_deadline = manager.get_timing(1).unwrap().next_frame_deadline;
        for fps in INVALID {
            manager.register_stream(1, fps, 50);
            let stream = manager.get_timing(1).unwrap();
            assert_eq!(stream.fps, 60.0);
            assert_eq!(stream.buffer_latency_ms, 20);
            assert_eq!(stream.next_frame_deadline, original_deadline);
        }
        manager.unregister_stream(1);
        assert_eq!(manager.active_streams(), 0);
    }
    #[test]
    fn invalid_update_does_not_partially_change_timing() {
        let mut manager = AudioSyncManager::new();
        manager.register_stream(1, 60.0, 20);
        let original = manager.get_timing(1).unwrap();
        let deadline = original.next_frame_deadline;
        let last_frame = original.last_frame_time;
        for fps in INVALID {
            manager.update_stream(1, fps, 80);
            let stream = manager.get_timing(1).unwrap();
            assert_eq!(stream.fps, 60.0);
            assert_eq!(stream.buffer_latency_ms, 20);
            assert_eq!(stream.dynamic_latency_ms, 20.0);
            assert_eq!(stream.drift_ema, 0.0);
            assert_eq!(stream.max_observed_gap_ms, 0.0);
            assert_eq!(stream.last_frame_time, last_frame);
            assert_eq!(stream.next_frame_deadline, deadline);
        }
        manager.update_stream(1, 30.0, 40);
        assert_eq!(manager.get_timing(1).unwrap().fps, 30.0);
        assert_eq!(manager.get_timing(1).unwrap().buffer_latency_ms, 40);
        manager.mark_frame_rendered(1);
        let stream = manager.get_timing(1).unwrap();
        assert_eq!(stream.frames_rendered, 1);
        assert!(stream.next_frame_deadline > stream.last_frame_time);
    }
}
