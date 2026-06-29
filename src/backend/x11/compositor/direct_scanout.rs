/// Direct Scanout - Bypass compositor for fullscreen windows
///
/// When a fullscreen opaque window with no effects is active,
/// bypass the compositor and present directly to reduce latency
use std::time::Instant;

/// Window properties for scanout eligibility check
#[derive(Debug, Clone, Copy)]
pub struct WindowScanoutInfo {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub is_fullscreen: bool,
    pub has_alpha: bool,
    pub has_blur: bool,
    pub has_shadow: bool,
    pub has_corner_radius: bool,
    pub opacity: f32,
}

/// Direct scanout manager
pub struct DirectScanoutManager {
    /// Currently scanned-out window (if any)
    current_scanout: Option<u32>,
    /// Time when scanout started
    scanout_start: Option<Instant>,
    /// Screen dimensions for fullscreen check
    screen_width: u32,
    screen_height: u32,
    /// Enable/disable direct scanout
    enabled: bool,
    /// Statistics
    scanout_count: u64,
    bypass_time_ms: u64,
}

impl DirectScanoutManager {
    pub fn new(screen_width: u32, screen_height: u32) -> Self {
        Self {
            current_scanout: None,
            scanout_start: None,
            screen_width,
            screen_height,
            enabled: true,
            scanout_count: 0,
            bypass_time_ms: 0,
        }
    }

    /// Update screen dimensions
    pub fn update_screen_size(&mut self, width: u32, height: u32) {
        self.screen_width = width;
        self.screen_height = height;
        // Clear scanout if screen size changed
        if self.current_scanout.is_some() {
            self.end_scanout();
        }
    }

    /// Check if a window is eligible for direct scanout
    pub fn is_scanout_eligible(&self, info: &WindowScanoutInfo) -> bool {
        if !self.enabled {
            return false;
        }

        // Must be flagged as fullscreen
        if !info.is_fullscreen {
            return false;
        }

        // Must cover entire screen
        if info.x != 0 || info.y != 0 {
            return false;
        }
        if info.width != self.screen_width || info.height != self.screen_height {
            return false;
        }

        // Must be fully opaque
        if info.has_alpha || info.opacity < 1.0 {
            return false;
        }

        // Must have no effects
        if info.has_blur || info.has_shadow || info.has_corner_radius {
            return false;
        }

        true
    }

    /// Check if we should use direct scanout for the current scene
    /// Returns (should_scanout, window_id)
    pub fn check_scene(
        &mut self,
        scene: &[(u32, WindowScanoutInfo)],
        focused: Option<u32>,
    ) -> (bool, Option<u32>) {
        // Only consider if there's exactly one fullscreen window
        if scene.len() != 1 {
            if self.current_scanout.is_some() {
                self.end_scanout();
            }
            return (false, None);
        }

        let (window_id, info) = scene[0];

        // Window must be focused
        if Some(window_id) != focused {
            if self.current_scanout.is_some() {
                self.end_scanout();
            }
            return (false, None);
        }

        // Check eligibility
        if !self.is_scanout_eligible(&info) {
            if self.current_scanout.is_some() {
                self.end_scanout();
            }
            return (false, None);
        }

        // Start or continue scanout
        if self.current_scanout != Some(window_id) {
            self.begin_scanout(window_id);
        }

        (true, Some(window_id))
    }

    /// Begin direct scanout for a window
    fn begin_scanout(&mut self, window_id: u32) {
        if self.current_scanout.is_some() {
            self.end_scanout();
        }

        self.current_scanout = Some(window_id);
        self.scanout_start = Some(Instant::now());
        self.scanout_count += 1;

        log::info!(
            "[direct_scanout] Enabled for window 0x{:x} (bypass #{}, expect -8-12ms latency)",
            window_id,
            self.scanout_count
        );
    }

    /// End direct scanout
    fn end_scanout(&mut self) {
        if let Some(window_id) = self.current_scanout.take() {
            if let Some(start) = self.scanout_start.take() {
                let duration_ms = start.elapsed().as_millis() as u64;
                self.bypass_time_ms += duration_ms;

                log::info!(
                    "[direct_scanout] Disabled for window 0x{:x} (duration: {}ms, total bypass: {}ms)",
                    window_id,
                    duration_ms,
                    self.bypass_time_ms
                );
            }
        }
    }

    /// Get current scanned-out window
    pub fn current_scanout(&self) -> Option<u32> {
        self.current_scanout
    }

    /// Check if direct scanout is active
    pub fn is_active(&self) -> bool {
        self.current_scanout.is_some()
    }

    /// Enable/disable direct scanout
    pub fn set_enabled(&mut self, enabled: bool) {
        if !enabled && self.enabled {
            // Disabling - end any active scanout
            self.end_scanout();
        }
        self.enabled = enabled;
    }

    /// Check if enabled
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Get statistics
    pub fn stats(&self) -> DirectScanoutStats {
        DirectScanoutStats {
            scanout_count: self.scanout_count,
            total_bypass_time_ms: self.bypass_time_ms,
            currently_active: self.is_active(),
            current_window: self.current_scanout,
        }
    }

    /// Reset statistics
    pub fn reset_stats(&mut self) {
        self.scanout_count = 0;
        self.bypass_time_ms = 0;
    }
}

/// Direct scanout statistics
#[derive(Debug, Clone, Copy)]
pub struct DirectScanoutStats {
    pub scanout_count: u64,
    pub total_bypass_time_ms: u64,
    pub currently_active: bool,
    pub current_window: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_fullscreen_window(window_id: u32) -> (u32, WindowScanoutInfo) {
        (
            window_id,
            WindowScanoutInfo {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
                is_fullscreen: true,
                has_alpha: false,
                has_blur: false,
                has_shadow: false,
                has_corner_radius: false,
                opacity: 1.0,
            },
        )
    }

    #[test]
    fn test_scanout_creation() {
        let mgr = DirectScanoutManager::new(1920, 1080);
        assert!(!mgr.is_active());
        assert!(mgr.is_enabled());
    }

    #[test]
    fn test_fullscreen_eligible() {
        let mgr = DirectScanoutManager::new(1920, 1080);
        let (_, info) = make_fullscreen_window(1);

        assert!(mgr.is_scanout_eligible(&info));
    }

    #[test]
    fn test_not_fullscreen_ineligible() {
        let mgr = DirectScanoutManager::new(1920, 1080);
        let mut info = make_fullscreen_window(1).1;
        info.is_fullscreen = false;

        assert!(!mgr.is_scanout_eligible(&info));
    }

    #[test]
    fn test_partial_size_ineligible() {
        let mgr = DirectScanoutManager::new(1920, 1080);
        let mut info = make_fullscreen_window(1).1;
        info.width = 1280; // Not full width

        assert!(!mgr.is_scanout_eligible(&info));
    }

    #[test]
    fn test_with_alpha_ineligible() {
        let mgr = DirectScanoutManager::new(1920, 1080);
        let mut info = make_fullscreen_window(1).1;
        info.has_alpha = true;

        assert!(!mgr.is_scanout_eligible(&info));
    }

    #[test]
    fn test_with_blur_ineligible() {
        let mgr = DirectScanoutManager::new(1920, 1080);
        let mut info = make_fullscreen_window(1).1;
        info.has_blur = true;

        assert!(!mgr.is_scanout_eligible(&info));
    }

    #[test]
    fn test_scene_check_single_fullscreen() {
        let mut mgr = DirectScanoutManager::new(1920, 1080);
        let scene = vec![make_fullscreen_window(100)];

        let (should_scanout, window) = mgr.check_scene(&scene, Some(100));

        assert!(should_scanout);
        assert_eq!(window, Some(100));
        assert!(mgr.is_active());
    }

    #[test]
    fn test_scene_check_multiple_windows() {
        let mut mgr = DirectScanoutManager::new(1920, 1080);
        let scene = vec![make_fullscreen_window(100), make_fullscreen_window(101)];

        let (should_scanout, _) = mgr.check_scene(&scene, Some(100));

        assert!(!should_scanout);
        assert!(!mgr.is_active());
    }

    #[test]
    fn test_scene_check_unfocused() {
        let mut mgr = DirectScanoutManager::new(1920, 1080);
        let scene = vec![make_fullscreen_window(100)];

        let (should_scanout, _) = mgr.check_scene(&scene, Some(101)); // Different window focused

        assert!(!should_scanout);
        assert!(!mgr.is_active());
    }

    #[test]
    fn test_enable_disable() {
        let mut mgr = DirectScanoutManager::new(1920, 1080);
        let scene = vec![make_fullscreen_window(100)];

        mgr.check_scene(&scene, Some(100));
        assert!(mgr.is_active());

        mgr.set_enabled(false);
        assert!(!mgr.is_active());
        assert!(!mgr.is_enabled());
    }

    #[test]
    fn test_stats() {
        let mut mgr = DirectScanoutManager::new(1920, 1080);
        let scene = vec![make_fullscreen_window(100)];

        mgr.check_scene(&scene, Some(100));

        let stats = mgr.stats();
        assert_eq!(stats.scanout_count, 1);
        assert!(stats.currently_active);
        assert_eq!(stats.current_window, Some(100));
    }
}
