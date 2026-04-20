use std::collections::HashMap;
use std::time::Instant;

/// Per-window audio stream timing information
#[derive(Clone, Debug)]
pub struct AudioStreamTiming {
    pub fps: f32,
    pub buffer_latency_ms: u32,
    pub registered_at: Instant,
    pub last_update: Instant,
    pub frames_rendered: u64,
    /// Exponential moving average of clock drift (for dynamic buffer adjustment)
    pub drift_ema: f32,
    /// PTS offset between audio and display clock (nanoseconds)
    pub pts_offset_ns: i64,
    /// Dynamically adjusted buffer latency
    pub dynamic_latency_ms: f32,
}

impl AudioStreamTiming {
    pub fn new(fps: f32, buffer_latency_ms: u32) -> Self {
        let now = Instant::now();
        Self {
            fps,
            buffer_latency_ms,
            registered_at: now,
            last_update: now,
            frames_rendered: 0,
            drift_ema: 0.0,
            pts_offset_ns: 0,
            dynamic_latency_ms: buffer_latency_ms as f32,
        }
    }

    /// Update drift estimation for dynamic buffer adjustment
    pub fn update_drift(&mut self, actual_delta_ms: f32, expected_delta_ms: f32) {
        let drift = actual_delta_ms - expected_delta_ms;
        // EMA with alpha=0.1 (smooth but responsive)
        self.drift_ema = self.drift_ema * 0.9 + drift * 0.1;

        // Adjust buffer latency based on drift
        if self.drift_ema.abs() > 5.0 {
            // Significant drift: adjust buffer
            self.dynamic_latency_ms = (self.buffer_latency_ms as f32 + self.drift_ema)
                .clamp(10.0, self.buffer_latency_ms as f32 * 2.0);
        }
    }

    /// Calculate ideal presentation time for the next frame
    pub fn next_frame_deadline(&self) -> Instant {
        if self.fps <= 0.0 {
            return Instant::now();
        }
        let frame_duration_ms = 1000.0 / self.fps;
        let elapsed_ms = self.last_update.elapsed().as_secs_f32() * 1000.0;
        let next_frame_ms = ((self.frames_rendered as f32 + 1.0) * frame_duration_ms)
            - elapsed_ms
            - self.dynamic_latency_ms; // Use dynamic instead of static latency

        if next_frame_ms > 0.0 {
            Instant::now() + std::time::Duration::from_secs_f32(next_frame_ms / 1000.0)
        } else {
            Instant::now()
        }
    }

    /// Check if enough time has passed to render the next frame
    pub fn should_render_frame(&self) -> bool {
        Instant::now() >= self.next_frame_deadline()
    }
}

/// Global audio sync manager
pub struct AudioSyncManager {
    audio_streams: HashMap<u32, AudioStreamTiming>,
    fallback_timeout_ms: u64,
    max_observed_gap_ms: u64,
}

impl AudioSyncManager {
    pub fn new() -> Self {
        Self {
            audio_streams: HashMap::new(),
            fallback_timeout_ms: 1000, // Base timeout: 1 second
            max_observed_gap_ms: 0,
        }
    }

    /// Get adaptive timeout based on observed gaps
    fn adaptive_timeout(&self) -> u64 {
        self.fallback_timeout_ms + 3 * self.max_observed_gap_ms
    }

    /// Register an audio stream for a window
    pub fn register_stream(&mut self, x11_win: u32, fps: f32, buffer_latency_ms: u32) {
        let timing = AudioStreamTiming::new(fps, buffer_latency_ms);
        log::info!(
            "audio_sync: registered window 0x{:x} with {} fps, {} ms latency",
            x11_win, fps, buffer_latency_ms
        );
        self.audio_streams.insert(x11_win, timing);
    }

    /// Unregister an audio stream
    pub fn unregister_stream(&mut self, x11_win: u32) {
        if self.audio_streams.remove(&x11_win).is_some() {
            log::info!("audio_sync: unregistered window 0x{:x}", x11_win);
        }
    }

    /// Update audio timing for a window
    pub fn update_stream(&mut self, x11_win: u32, fps: f32, buffer_latency_ms: u32) {
        if let Some(timing) = self.audio_streams.get_mut(&x11_win) {
            let now = Instant::now();
            // Track inter-update gap for adaptive timeout
            let gap_ms = now.duration_since(timing.last_update).as_millis() as u64;
            if gap_ms > self.max_observed_gap_ms && gap_ms < 5000 {
                self.max_observed_gap_ms = gap_ms;
            }
            // Update drift estimation
            if timing.fps > 0.0 {
                let expected_ms = 1000.0 / timing.fps;
                let actual_ms = gap_ms as f32;
                timing.update_drift(actual_ms, expected_ms);
            }
            timing.fps = fps;
            timing.buffer_latency_ms = buffer_latency_ms;
            timing.last_update = now;
        }
    }

    /// Check if a window should render based on audio timing
    pub fn should_render(&self, x11_win: u32) -> bool {
        match self.audio_streams.get(&x11_win) {
            Some(timing) => timing.should_render_frame(),
            None => true, // Windows without audio sync always render
        }
    }

    /// Get audio timing for a window
    pub fn get_timing(&self, x11_win: u32) -> Option<&AudioStreamTiming> {
        self.audio_streams.get(&x11_win)
    }

    /// Mark frame rendered for a window
    pub fn mark_frame_rendered(&mut self, x11_win: u32) {
        if let Some(timing) = self.audio_streams.get_mut(&x11_win) {
            timing.frames_rendered += 1;
            timing.last_update = Instant::now();
        }
    }

    /// Get count of active audio streams
    pub fn active_streams(&self) -> usize {
        self.audio_streams.len()
    }

    /// Check if stream should fall back (no update for too long)
    pub fn should_fallback(&self, x11_win: u32) -> bool {
        match self.audio_streams.get(&x11_win) {
            Some(timing) => {
                // Use adaptive timeout instead of hardcoded 1 second
                let timeout = self.adaptive_timeout();
                timing.last_update.elapsed().as_millis() as u64 > timeout
            }
            None => false,
        }
    }
}

impl Default for AudioSyncManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_audio_sync_registration() {
        let mut manager = AudioSyncManager::new();
        manager.register_stream(1, 30.0, 50);
        assert_eq!(manager.active_streams(), 1);

        manager.unregister_stream(1);
        assert_eq!(manager.active_streams(), 0);
    }

    #[test]
    fn test_frame_deadline_calculation() {
        let timing = AudioStreamTiming::new(30.0, 50);
        // At 30fps, frame time is ~33.3ms
        let deadline = timing.next_frame_deadline();
        assert!(deadline > Instant::now());
    }

    #[test]
    fn test_should_render_timing() {
        let mut timing = AudioStreamTiming::new(30.0, 0);
        timing.last_update = Instant::now() - std::time::Duration::from_millis(100);
        // After 100ms delay, should be ready to render
        assert!(timing.should_render_frame());
    }

    #[test]
    fn test_fallback_detection() {
        let mut manager = AudioSyncManager::new();
        manager.register_stream(1, 30.0, 50);

        // Immediately after registration, shouldn't fall back
        assert!(!manager.should_fallback(1));

        // Simulate old timestamp
        if let Some(timing) = manager.audio_streams.get_mut(&1) {
            timing.last_update = Instant::now() - std::time::Duration::from_secs(2);
        }
        assert!(manager.should_fallback(1));
    }
}
