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
    last_candidate_count: usize,
    last_reason: String,
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
            last_candidate_count: 0,
            last_reason: "not evaluated yet".into(),
        }
    }

    fn rejection_reason(&self, info: &WindowScanoutInfo) -> Option<String> {
        if !info.is_fullscreen {
            return Some("window is not fullscreen".into());
        }
        if info.has_alpha {
            return Some("window buffer has alpha".into());
        }
        if info.has_blur {
            return Some("window has blur/frosted effect".into());
        }
        if info.has_shadow {
            return Some("window shadow is enabled".into());
        }
        if info.corner_radius != 0.0 {
            return Some(format!("window corner radius is {}", info.corner_radius));
        }
        if info.opacity < 1.0 {
            return Some(format!("window opacity is {:.3}", info.opacity));
        }
        if info.x != 0 || info.y != 0 {
            return Some(format!("window origin is {},{}", info.x, info.y));
        }
        if info.width != self.screen_w || info.height != self.screen_h {
            return Some(format!(
                "window size {}x{} does not match screen {}x{}",
                info.width, info.height, self.screen_w, self.screen_h
            ));
        }
        None
    }

    /// Returns true if the window is eligible for direct scanout bypass.
    /// Eligible means: fullscreen, no alpha, no blur, no shadow, zero corner radius,
    /// fully opaque, positioned at (0,0), and matching the screen dimensions.
    pub(crate) fn is_scanout_eligible(&self, info: &WindowScanoutInfo) -> bool {
        self.rejection_reason(info).is_none()
    }

    /// Check the current scene to determine if direct scanout is possible.
    /// Returns (can_scanout, window_id).
    /// Can scanout if exactly one visible window on top and it is eligible.
    pub(crate) fn check_scene(
        &mut self,
        windows: &[(u64, WindowScanoutInfo)],
        _focused: Option<u64>,
    ) -> (bool, Option<u64>) {
        self.last_candidate_count = windows.len();
        if !self.enabled {
            self.end_scanout();
            self.last_reason = "direct scanout disabled".into();
            return (false, None);
        }

        // Direct scanout requires exactly one visible window
        if windows.len() != 1 {
            self.end_scanout();
            self.last_reason =
                format!("expected exactly 1 candidate window, got {}", windows.len());
            return (false, None);
        }

        let (window_id, ref info) = windows[0];

        if let Some(reason) = self.rejection_reason(info) {
            self.end_scanout();
            self.last_reason = reason;
            (false, None)
        } else {
            // Start or continue scanout
            if self.current_scanout != Some(window_id) {
                self.current_scanout = Some(window_id);
                self.scanout_start = Some(Instant::now());
                self.stats.scanout_count += 1;
                self.stats.current_window = Some(window_id);
            }
            self.last_reason = "eligible".into();
            (true, Some(window_id))
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

    pub(crate) fn candidate_count(&self) -> usize {
        self.last_candidate_count
    }

    pub(crate) fn last_reason(&self) -> &str {
        &self.last_reason
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fullscreen_info() -> WindowScanoutInfo {
        WindowScanoutInfo {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            is_fullscreen: true,
            has_alpha: false,
            has_blur: false,
            has_shadow: false,
            corner_radius: 0.0,
            opacity: 1.0,
        }
    }

    #[test]
    fn records_eligible_reason_and_current_window() {
        let mut mgr = DirectScanoutManager::new(1920, 1080);

        let (ok, win) = mgr.check_scene(&[(42, fullscreen_info())], Some(42));

        assert!(ok);
        assert_eq!(win, Some(42));
        assert_eq!(mgr.current_scanout(), Some(42));
        assert_eq!(mgr.candidate_count(), 1);
        assert_eq!(mgr.last_reason(), "eligible");
    }

    #[test]
    fn records_multiple_window_rejection() {
        let mut mgr = DirectScanoutManager::new(1920, 1080);

        let (ok, win) = mgr.check_scene(&[(1, fullscreen_info()), (2, fullscreen_info())], Some(1));

        assert!(!ok);
        assert_eq!(win, None);
        assert_eq!(mgr.candidate_count(), 2);
        assert_eq!(
            mgr.last_reason(),
            "expected exactly 1 candidate window, got 2"
        );
    }

    #[test]
    fn records_first_window_property_rejection() {
        let mut mgr = DirectScanoutManager::new(1920, 1080);
        let mut info = fullscreen_info();
        info.has_blur = true;

        let (ok, _) = mgr.check_scene(&[(7, info)], Some(7));

        assert!(!ok);
        assert_eq!(mgr.last_reason(), "window has blur/frosted effect");
    }

    #[test]
    fn records_size_mismatch_rejection() {
        let mut mgr = DirectScanoutManager::new(1920, 1080);
        let mut info = fullscreen_info();
        info.width = 1280;

        let (ok, _) = mgr.check_scene(&[(7, info)], Some(7));

        assert!(!ok);
        assert_eq!(
            mgr.last_reason(),
            "window size 1280x1080 does not match screen 1920x1080"
        );
    }
}
