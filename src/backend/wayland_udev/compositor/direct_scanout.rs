use std::time::Instant;

pub(crate) struct WindowScanoutInfo {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub is_fullscreen: bool,
    pub has_alpha: bool,
    pub has_blur: bool,
    pub has_shadow: bool,
    pub corner_radius: f32,
    pub opacity: f32,
}

pub(crate) struct DirectScanoutStats {
    pub scanout_count: u64,
    pub bypass_time_ms: u64,
    pub current_window: Option<u64>,
}

pub(crate) struct DirectScanoutManager {
    screen_w: u32,
    screen_h: u32,
    enabled: bool,
    current_scanout: Option<u64>,
    scanout_start: Option<Instant>,
    stats: DirectScanoutStats,
}

impl DirectScanoutManager {
    pub(crate) fn new(screen_w: u32, screen_h: u32) -> Self {
        Self {
            screen_w,
            screen_h,
            enabled: true,
            current_scanout: None,
            scanout_start: None,
            stats: DirectScanoutStats {
                scanout_count: 0,
                bypass_time_ms: 0,
                current_window: None,
            },
        }
    }

    /// Returns true if the window is eligible for direct scanout bypass.
    /// Eligible means: fullscreen, no alpha, no blur, no shadow, zero corner radius,
    /// fully opaque, positioned at (0,0), and matching the screen dimensions.
    pub(crate) fn is_scanout_eligible(&self, info: &WindowScanoutInfo) -> bool {
        info.is_fullscreen
            && !info.has_alpha
            && !info.has_blur
            && !info.has_shadow
            && info.corner_radius == 0.0
            && info.opacity >= 1.0
            && info.x == 0
            && info.y == 0
            && info.width == self.screen_w
            && info.height == self.screen_h
    }

    /// Check the current scene to determine if direct scanout is possible.
    /// Returns (can_scanout, window_id).
    /// Can scanout if exactly one visible window on top and it is eligible.
    pub(crate) fn check_scene(
        &mut self,
        windows: &[(u64, WindowScanoutInfo)],
        _focused: Option<u64>,
    ) -> (bool, Option<u64>) {
        if !self.enabled {
            self.end_scanout();
            return (false, None);
        }

        // Direct scanout requires exactly one visible window
        if windows.len() != 1 {
            self.end_scanout();
            return (false, None);
        }

        let (window_id, ref info) = windows[0];

        if self.is_scanout_eligible(info) {
            // Start or continue scanout
            if self.current_scanout != Some(window_id) {
                self.current_scanout = Some(window_id);
                self.scanout_start = Some(Instant::now());
                self.stats.scanout_count += 1;
                self.stats.current_window = Some(window_id);
            }
            (true, Some(window_id))
        } else {
            self.end_scanout();
            (false, None)
        }
    }

    /// End the current scanout session, updating bypass time stats.
    fn end_scanout(&mut self) {
        if let Some(start) = self.scanout_start.take() {
            self.stats.bypass_time_ms += start.elapsed().as_millis() as u64;
        }
        self.current_scanout = None;
        self.stats.current_window = None;
    }

    pub(crate) fn current_scanout(&self) -> Option<u64> {
        self.current_scanout
    }

    pub(crate) fn is_active(&self) -> bool {
        self.current_scanout.is_some()
    }

    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.end_scanout();
        }
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub(crate) fn stats(&self) -> &DirectScanoutStats {
        &self.stats
    }

    pub(crate) fn reset_stats(&mut self) {
        self.stats.scanout_count = 0;
        self.stats.bypass_time_ms = 0;
        // current_window is live state, not reset
    }

    pub(crate) fn update_screen_size(&mut self, w: u32, h: u32) {
        self.screen_w = w;
        self.screen_h = h;
        // Invalidate current scanout since screen dimensions changed
        self.end_scanout();
    }
}
