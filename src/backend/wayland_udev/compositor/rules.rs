// ---------------------------------------------------------------------------
// Per-window rule engine for the Wayland udev backend compositor.
// Handles opacity rules, corner radius rules, scale rules, frosted glass,
// exclusion lists, VRR detection, temporal blur reuse, and adaptive blur quality.
// ---------------------------------------------------------------------------

use super::*;
use crate::config::CONFIG;

// BlurQuality is defined in super (mod.rs)

/// ASCII case-insensitive substring test that performs no heap allocation.
///
/// Window class names / app_ids are ASCII identifiers in practice, so ASCII
/// case folding is sufficient and avoids the per-call `String` allocation that
/// `haystack.to_lowercase().contains(&needle.to_lowercase())` incurs. This runs
/// per-window per-frame in the render loop, so the allocations mattered.
pub(crate) fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    let n = needle.len();
    if n == 0 {
        return true;
    }
    let h = haystack.as_bytes();
    let ne = needle.as_bytes();
    if h.len() < n {
        return false;
    }
    let first = ne[0].to_ascii_lowercase();
    for start in 0..=h.len() - n {
        if h[start].to_ascii_lowercase() != first {
            continue;
        }
        if h[start..start + n]
            .iter()
            .zip(ne)
            .all(|(a, b)| a.to_ascii_lowercase() == b.to_ascii_lowercase())
        {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Exclusion and rule matching
// ---------------------------------------------------------------------------

impl WaylandCompositor {
    /// Check if `class_name` matches any pattern in `list` (case-insensitive substring).
    /// Always returns true for "flameshot" regardless of the list contents.
    pub(crate) fn class_matches_exclude(class_name: &str, list: &[String]) -> bool {
        // Always exclude flameshot (screenshot tool overlays).
        if contains_ignore_case(class_name, "flameshot") {
            return true;
        }

        list.iter()
            .any(|pattern| contains_ignore_case(class_name, pattern))
    }

    // -----------------------------------------------------------------------
    // Opacity rules
    // -----------------------------------------------------------------------

    /// Lookup the first matching opacity rule for the given window class.
    /// Returns the opacity as a fraction 0.0..1.0 (rules are stored as 0..100 percent).
    pub(crate) fn lookup_opacity_rule(&self, class_name: &str) -> Option<f32> {
        for (opacity, pattern) in &self.opacity_rules {
            if contains_ignore_case(class_name, pattern) {
                return Some(*opacity);
            }
        }
        None
    }

    /// Parse opacity rules from config format: ["85:firefox", "90:Alacritty"].
    /// Returns Vec<(opacity_fraction, class_pattern)>.
    pub(crate) fn parse_opacity_rules(rules: &[String]) -> Vec<(f32, String)> {
        let mut result = Vec::with_capacity(rules.len());
        for rule in rules {
            if let Some((pct_str, class)) = rule.split_once(':') {
                if let Ok(pct) = pct_str.trim().parse::<f32>() {
                    let opacity = (pct / 100.0).clamp(0.0, 1.0);
                    result.push((opacity, class.trim().to_string()));
                }
            }
        }
        result
    }

    // -----------------------------------------------------------------------
    // Corner radius rules
    // -----------------------------------------------------------------------

    /// Lookup the first matching corner radius rule for the given window class.
    pub(crate) fn lookup_corner_radius_rule(&self, class_name: &str) -> Option<f32> {
        for (radius, pattern) in &self.corner_radius_rules {
            if contains_ignore_case(class_name, pattern) {
                return Some(*radius);
            }
        }
        None
    }

    /// Parse corner radius rules from config format: ["12.0:kitty", "0:Alacritty"].
    /// Returns Vec<(radius_px, class_pattern)>.
    pub(crate) fn parse_corner_radius_rules(rules: &[String]) -> Vec<(f32, String)> {
        let mut result = Vec::with_capacity(rules.len());
        for rule in rules {
            if let Some((radius_str, class)) = rule.split_once(':') {
                if let Ok(radius) = radius_str.trim().parse::<f32>() {
                    result.push((radius.max(0.0), class.trim().to_string()));
                }
            }
        }
        result
    }

    // -----------------------------------------------------------------------
    // Frosted glass rules
    // -----------------------------------------------------------------------

    /// Check if `class_name` matches a frosted glass rule.
    /// Returns the strength (0.0-1.0) if matched, None otherwise.
    pub(crate) fn lookup_frosted_glass_rule(&self, class_name: &str) -> Option<f32> {
        for (pattern, strength) in &self.frosted_glass_rules {
            if contains_ignore_case(class_name, pattern) {
                return Some(*strength);
            }
        }
        None
    }

    /// Parse frosted glass rules. Supports:
    /// - `"class_name"` → strength 1.0
    /// - `"0.7:class_name"` → strength 0.7
    pub(crate) fn parse_frosted_glass_rules(rules: &[String]) -> Vec<(String, f32)> {
        let mut result = Vec::with_capacity(rules.len());
        for rule in rules {
            if let Some((strength_str, class)) = rule.split_once(':') {
                if let Ok(strength) = strength_str.trim().parse::<f32>() {
                    result.push((class.trim().to_string(), strength.clamp(0.0, 1.0)));
                    continue;
                }
            }
            // No colon or unparseable strength: treat entire string as class, strength=1.0
            result.push((rule.trim().to_string(), 1.0));
        }
        result
    }

    // -----------------------------------------------------------------------
    // Scale rules
    // -----------------------------------------------------------------------

    /// Lookup the first matching scale rule for the given window class.
    /// Returns the scale as a fraction (e.g. 0.9 for 90%).
    pub(crate) fn lookup_scale_rule(&self, class_name: &str) -> Option<f32> {
        for (scale, pattern) in &self.scale_rules {
            if contains_ignore_case(class_name, pattern) {
                return Some(*scale);
            }
        }
        None
    }

    /// Parse scale rules from config format: ["90:obs", "75:mpv"].
    /// Returns Vec<(scale_fraction, class_pattern)>.
    pub(crate) fn parse_scale_rules(rules: &[String]) -> Vec<(f32, String)> {
        let mut result = Vec::with_capacity(rules.len());
        for rule in rules {
            if let Some((pct_str, class)) = rule.split_once(':') {
                if let Ok(pct) = pct_str.trim().parse::<f32>() {
                    let scale = (pct / 100.0).clamp(0.01, 10.0);
                    result.push((scale, class.trim().to_string()));
                }
            }
        }
        result
    }

    // -----------------------------------------------------------------------
    // Game window detection (for VRR)
    // -----------------------------------------------------------------------

    /// Built-in set of known game/emulator window classes.
    const BUILTIN_GAME_CLASSES: &'static [&'static str] = &[
        "steam",
        "gamescope",
        "proton",
        "dxvk",
        "lutris",
        "wine",
        "minecraft",
        "dosbox",
        "mgba",
        "pcsx2",
        "yuzu",
        "dolphin-emu",
        "retroarch",
        "citra",
        "rpcs3",
    ];

    /// Detect whether the given window class belongs to a game or emulator.
    /// Checks against both the built-in set and user-configured game_classes.
    pub(crate) fn detect_game_window(class_name: &str) -> bool {
        // Check built-in list.
        for &game_class in Self::BUILTIN_GAME_CLASSES {
            if contains_ignore_case(class_name, game_class) {
                return true;
            }
        }

        // Check user-configured game classes from CONFIG.
        let cfg = CONFIG.load();
        let b = cfg.behavior();
        b.game_classes
            .iter()
            .any(|user_class| contains_ignore_case(class_name, user_class))
    }

    // -----------------------------------------------------------------------
    // VRR (Variable Refresh Rate) state management
    // -----------------------------------------------------------------------

    /// Update VRR active state based on the currently focused window.
    /// Gated by a 1-second cooldown to avoid excessive polling.
    pub(crate) fn update_vrr_state(&mut self, focused: Option<u64>) {
        let now = Instant::now();
        if now.duration_since(self.vrr_last_check) < Duration::from_secs(1) {
            return;
        }
        self.vrr_last_check = now;

        let cfg = CONFIG.load();
        let b = cfg.behavior();

        if !b.vrr_enabled {
            if self.vrr_active {
                log::debug!("VRR disabled by config, deactivating");
                self.vrr_active = false;
            }
            return;
        }

        let is_game = if let Some(wid) = focused {
            // Check cache first.
            if let Some(&cached) = self.is_game_window.get(&wid) {
                cached
            } else {
                // Lookup class name from window state.
                let detected = self
                    .windows
                    .get(&wid)
                    .map(|ws| Self::detect_game_window(&ws.class_name))
                    .unwrap_or(false);
                self.is_game_window.insert(wid, detected);
                detected
            }
        } else {
            false
        };

        if is_game != self.vrr_active {
            log::debug!(
                "VRR state changed: {} -> {} (focused: {:?})",
                self.vrr_active,
                is_game,
                focused
            );
            self.vrr_active = is_game;
        }
    }

    #[allow(dead_code)]
    pub(crate) fn get_vrr_refresh_rate(&self) -> u32 {
        if self.vrr_active {
            let cfg = CONFIG.load();
            let b = cfg.behavior();
            b.vrr_max_fps
        } else {
            60
        }
    }

    // -----------------------------------------------------------------------
    // Temporal blur reuse
    // -----------------------------------------------------------------------

    /// Compute a hash of all visible (non-fading-out) window positions and opacities.
    /// Uses FNV-1a inspired hash for speed and reasonable distribution.
    pub(crate) fn compute_window_positions_hash(&self) -> u64 {
        const FNV_OFFSET: u64 = 0xcbf29ce484222325;
        const FNV_PRIME: u64 = 0x100000001b3;

        let mut hash = FNV_OFFSET;

        // Use prev_scene which captures the actual rendered layout (id, x, y, w, h).
        for &(id, x, y, w, h) in &self.prev_scene {
            hash ^= id;
            hash = hash.wrapping_mul(FNV_PRIME);
            hash ^= x as u64;
            hash = hash.wrapping_mul(FNV_PRIME);
            hash ^= y as u64;
            hash = hash.wrapping_mul(FNV_PRIME);
            hash ^= w as u64;
            hash = hash.wrapping_mul(FNV_PRIME);
            hash ^= h as u64;
            hash = hash.wrapping_mul(FNV_PRIME);
        }

        // Also mix in opacity for fading windows.
        for &(id, _, _, _, _) in &self.prev_scene {
            if let Some(ws) = self.windows.get(&id) {
                let opacity_bits = (ws.fade_opacity * 1000.0) as u64;
                hash ^= opacity_bits;
                hash = hash.wrapping_mul(FNV_PRIME);
            }
        }

        hash
    }

    /// Apply temporal blur mixing: if window positions have not changed since the
    /// Copy a blur texture into the prev_blur_fbo cache for temporal reuse.
    /// Allocates the FBO on first call.
    ///
    /// # Safety
    /// Caller must ensure `gl` is valid and `current_blur_tex` is a valid texture.
    pub(crate) unsafe fn copy_blur_to_prev_fbo(
        &mut self,
        gl: &ffi::Gles2,
        current_blur_tex: u32,
    ) {
        unsafe {
            let prev_fbo = match self.prev_blur_fbo {
                Some((fbo, _tex)) => fbo,
                None => {
                    let mut tex = 0u32;
                    gl.GenTextures(1, &mut tex);
                    gl.BindTexture(ffi::TEXTURE_2D, tex);
                    gl.TexImage2D(
                        ffi::TEXTURE_2D,
                        0,
                        ffi::RGBA8 as i32,
                        self.screen_w as i32,
                        self.screen_h as i32,
                        0,
                        ffi::RGBA,
                        ffi::UNSIGNED_BYTE,
                        std::ptr::null(),
                    );
                    gl.TexParameteri(
                        ffi::TEXTURE_2D,
                        ffi::TEXTURE_MIN_FILTER,
                        ffi::LINEAR as i32,
                    );
                    gl.TexParameteri(
                        ffi::TEXTURE_2D,
                        ffi::TEXTURE_MAG_FILTER,
                        ffi::LINEAR as i32,
                    );
                    gl.TexParameteri(
                        ffi::TEXTURE_2D,
                        ffi::TEXTURE_WRAP_S,
                        ffi::CLAMP_TO_EDGE as i32,
                    );
                    gl.TexParameteri(
                        ffi::TEXTURE_2D,
                        ffi::TEXTURE_WRAP_T,
                        ffi::CLAMP_TO_EDGE as i32,
                    );

                    let mut fbo = 0u32;
                    gl.GenFramebuffers(1, &mut fbo);
                    gl.BindFramebuffer(ffi::FRAMEBUFFER, fbo);
                    gl.FramebufferTexture2D(
                        ffi::FRAMEBUFFER,
                        ffi::COLOR_ATTACHMENT0,
                        ffi::TEXTURE_2D,
                        tex,
                        0,
                    );

                    self.prev_blur_fbo = Some((fbo, tex));
                    fbo
                }
            };

            let mut src_fbo = 0u32;
            gl.GenFramebuffers(1, &mut src_fbo);
            gl.BindFramebuffer(ffi::READ_FRAMEBUFFER, src_fbo);
            gl.FramebufferTexture2D(
                ffi::READ_FRAMEBUFFER,
                ffi::COLOR_ATTACHMENT0,
                ffi::TEXTURE_2D,
                current_blur_tex,
                0,
            );

            gl.BindFramebuffer(ffi::DRAW_FRAMEBUFFER, prev_fbo);
            gl.BlitFramebuffer(
                0,
                0,
                self.screen_w as i32,
                self.screen_h as i32,
                0,
                0,
                self.screen_w as i32,
                self.screen_h as i32,
                ffi::COLOR_BUFFER_BIT,
                ffi::NEAREST,
            );

            gl.DeleteFramebuffers(1, &src_fbo);
            gl.BindFramebuffer(ffi::FRAMEBUFFER, 0);
        }
    }

    // -----------------------------------------------------------------------
    // Adaptive blur quality
    // -----------------------------------------------------------------------

    /// Compute the blur quality level for a given window, based on:
    /// - Whether adaptive quality is enabled (`blur_quality_auto`)
    /// - Per-monitor quality overrides
    #[allow(dead_code)]
    pub(crate) fn compute_window_blur_quality(
        &self,
        window_id: u64,
        focused: Option<u64>,
    ) -> BlurQuality {
        // If auto-quality is disabled, return the global setting.
        if !self.blur_quality_auto {
            return self.blur_quality;
        }

        // Check per-monitor override: find which monitor contains this window.
        if let Some(ws) = self.windows.get(&window_id) {
            let win_cx = ws.width / 2;
            let win_cy = ws.height / 2;

            for &(mon_id, mx, my, mw, mh) in &self.monitors {
                let contains_x =
                    (win_cx as i32) >= mx && (win_cx as i32) < mx + mw as i32;
                let contains_y =
                    (win_cy as i32) >= my && (win_cy as i32) < my + mh as i32;
                if contains_x && contains_y {
                    if let Some(&quality) = self.blur_quality_by_monitor.get(&mon_id) {
                        return quality;
                    }
                    break;
                }
            }
        }

        // Focused window always gets full quality.
        if focused == Some(window_id) {
            return BlurQuality::Full;
        }

        // Adaptive: estimate GPU load from frame times.
        // last_gpu_load is a percentage 0..100.
        if self.last_gpu_load >= 80 {
            BlurQuality::Minimal
        } else if self.last_gpu_load >= 70 {
            BlurQuality::Reduced
        } else {
            BlurQuality::Full
        }
    }

    // -----------------------------------------------------------------------
    // Parsing helpers for blur quality/strength configuration
    // -----------------------------------------------------------------------

    /// Parse per-monitor blur quality from config string.
    /// Format: "primary:Full,secondary:Reduced,tertiary:Minimal"
    /// Monitor names map to indices: primary=0, secondary=1, tertiary=2, etc.
    pub(crate) fn parse_blur_quality_by_monitor(s: &str) -> HashMap<u32, BlurQuality> {
        let mut map = HashMap::new();

        if s.is_empty() {
            return map;
        }

        for entry in s.split(',') {
            let entry = entry.trim();
            if let Some((name, quality_str)) = entry.split_once(':') {
                let mon_id = match name.trim().to_lowercase().as_str() {
                    "primary" => 0u32,
                    "secondary" => 1,
                    "tertiary" => 2,
                    "quaternary" => 3,
                    other => other.parse::<u32>().unwrap_or(0),
                };

                let quality = match quality_str.trim().to_lowercase().as_str() {
                    "full" => BlurQuality::Full,
                    "reduced" => BlurQuality::Reduced,
                    "minimal" => BlurQuality::Minimal,
                    _ => BlurQuality::Full,
                };

                map.insert(mon_id, quality);
            }
        }

        map
    }

    /// Parse dynamic blur strength by monitor Hz from config string.
    /// Format: "60:2,144:3,240:4"
    /// Returns Vec<(hz, strength)> sorted by Hz ascending.
    pub(crate) fn parse_blur_strength_by_hz(s: &str) -> Vec<(u32, u32)> {
        let mut result = Vec::new();

        if s.is_empty() {
            return result;
        }

        for entry in s.split(',') {
            let entry = entry.trim();
            if let Some((hz_str, strength_str)) = entry.split_once(':') {
                if let (Ok(hz), Ok(strength)) = (
                    hz_str.trim().parse::<u32>(),
                    strength_str.trim().parse::<u32>(),
                ) {
                    result.push((hz, strength));
                }
            }
        }

        result.sort_by_key(|&(hz, _)| hz);
        result
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_class_matches_exclude_basic() {
        let list = vec!["firefox".to_string(), "chrome".to_string()];
        assert!(WaylandCompositor::class_matches_exclude("Firefox", &list));
        assert!(WaylandCompositor::class_matches_exclude("Google-Chrome", &list));
        assert!(!WaylandCompositor::class_matches_exclude("Alacritty", &list));
    }

    #[test]
    fn test_class_matches_exclude_flameshot() {
        let list: Vec<String> = vec![];
        assert!(WaylandCompositor::class_matches_exclude("flameshot", &list));
        assert!(WaylandCompositor::class_matches_exclude("Flameshot", &list));
    }

    #[test]
    fn test_parse_opacity_rules() {
        let rules = vec!["85:firefox".to_string(), "90:Alacritty".to_string()];
        let parsed = WaylandCompositor::parse_opacity_rules(&rules);
        assert_eq!(parsed.len(), 2);
        assert!((parsed[0].0 - 0.85).abs() < 0.001);
        assert_eq!(parsed[0].1, "firefox");
        assert!((parsed[1].0 - 0.90).abs() < 0.001);
        assert_eq!(parsed[1].1, "Alacritty");
    }

    #[test]
    fn test_parse_corner_radius_rules() {
        let rules = vec!["12.0:kitty".to_string(), "0:mpv".to_string()];
        let parsed = WaylandCompositor::parse_corner_radius_rules(&rules);
        assert_eq!(parsed.len(), 2);
        assert!((parsed[0].0 - 12.0).abs() < 0.001);
        assert_eq!(parsed[0].1, "kitty");
        assert!((parsed[1].0 - 0.0).abs() < 0.001);
    }

    #[test]
    fn test_parse_scale_rules() {
        let rules = vec!["90:obs".to_string(), "50:pip".to_string()];
        let parsed = WaylandCompositor::parse_scale_rules(&rules);
        assert_eq!(parsed.len(), 2);
        assert!((parsed[0].0 - 0.9).abs() < 0.001);
        assert!((parsed[1].0 - 0.5).abs() < 0.001);
    }

    #[test]
    fn test_parse_blur_quality_by_monitor() {
        let s = "primary:Full,secondary:Reduced,tertiary:Minimal";
        let map = WaylandCompositor::parse_blur_quality_by_monitor(s);
        assert_eq!(map.get(&0), Some(&BlurQuality::Full));
        assert_eq!(map.get(&1), Some(&BlurQuality::Reduced));
        assert_eq!(map.get(&2), Some(&BlurQuality::Minimal));
    }

    #[test]
    fn test_parse_blur_strength_by_hz() {
        let s = "60:2,144:3,240:4";
        let parsed = WaylandCompositor::parse_blur_strength_by_hz(s);
        assert_eq!(parsed, vec![(60, 2), (144, 3), (240, 4)]);
    }

    #[test]
    fn test_parse_blur_strength_by_hz_empty() {
        let parsed = WaylandCompositor::parse_blur_strength_by_hz("");
        assert!(parsed.is_empty());
    }

    #[test]
    fn test_detect_game_window_builtin() {
        assert!(WaylandCompositor::detect_game_window("Steam"));
        assert!(WaylandCompositor::detect_game_window("gamescope"));
        assert!(WaylandCompositor::detect_game_window("RetroArch"));
        assert!(WaylandCompositor::detect_game_window("dolphin-emu"));
        assert!(!WaylandCompositor::detect_game_window("firefox"));
        assert!(!WaylandCompositor::detect_game_window("Alacritty"));
    }
}
