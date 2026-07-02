use std::time::Instant;

/// Per-window OML sync state.
pub struct OmlSyncWindow {
    pub x11_win: u32,
    pub last_msc: u64,
    pub last_ust: u64,
    pub last_update: Instant,
    pub frame_delay_ns: u64,
}

impl OmlSyncWindow {
    pub fn new(x11_win: u32, fps: f32) -> Self {
        let frame_delay_ns = if fps > 0.0 {
            (1_000_000_000.0 / fps as f64).round() as u64
        } else {
            16_666_667
        };

        Self {
            x11_win,
            last_msc: 0,
            last_ust: 0,
            last_update: Instant::now(),
            frame_delay_ns,
        }
    }

    pub fn set_fps(&mut self, fps: f32) {
        self.frame_delay_ns = if fps > 0.0 {
            (1_000_000_000.0 / fps as f64).round() as u64
        } else {
            16_666_667
        };
    }

    /// Estimate next MSC when this window should present.
    pub fn estimate_next_msc(&self) -> u64 {
        if self.last_msc == 0 {
            return 0;
        }

        let elapsed = self.last_update.elapsed();
        let elapsed_ns = elapsed.as_nanos() as u64;
        let frames_elapsed = elapsed_ns / self.frame_delay_ns;

        self.last_msc.saturating_add(frames_elapsed.max(1))
    }
}
