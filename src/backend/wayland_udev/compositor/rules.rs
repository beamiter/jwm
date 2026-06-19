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
        // The blur result lives in blur_fbos[0], which is half-resolution.
        // prev_blur_fbo must match those dims so the temporal-mix pass samples
        // both history and current at identical resolution.
        let (bw, bh) = match self.blur_fbos.first() {
            Some(l) => (l.width, l.height),
            None => return,
        };
        unsafe {
            let prev_fbo = match self.prev_blur_fbo {
                Some((fbo, _tex)) => fbo,
                None => {
                    let (fbo, tex) = if self.hdr_enabled {
                        super::create_fbo_texture_10bit(gl, bw, bh)
                    } else {
                        super::create_fbo_texture(gl, bw, bh)
                    };
                    self.prev_blur_fbo = Some((fbo, tex));
                    fbo
                }
            };

            // Reuse a single read-framebuffer across frames; re-attaching a
            // texture is cheap, gen/deleting an FBO every frame is not.
            if self.blur_blit_src_fbo == 0 {
                gl.GenFramebuffers(1, &mut self.blur_blit_src_fbo);
            }
            gl.BindFramebuffer(ffi::READ_FRAMEBUFFER, self.blur_blit_src_fbo);
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
                bw as i32,
                bh as i32,
                0,
                0,
                bw as i32,
                bh as i32,
                ffi::COLOR_BUFFER_BIT,
                ffi::NEAREST,
            );

            gl.BindFramebuffer(ffi::FRAMEBUFFER, 0);
        }
    }

    /// Compute a motion-aware temporal-mix ratio for the current frame.
    ///
    /// Returns the configured `temporal_blur_mix_ratio` when the scene is static
    /// (so the blur is maximally stabilized), decaying toward 0 as the aggregate
    /// per-window displacement since the previous frame grows. Returning ~0 on
    /// motion means the displayed blur is essentially the fresh current frame,
    /// which avoids ghosting/smearing while windows move.
    ///
    /// Side effect: records the current positions for next frame's comparison.
    pub(crate) fn temporal_mix_ratio_for_motion(
        &mut self,
        scene: &[(u64, i32, i32, u32, u32)],
    ) -> f32 {
        let mut total_disp: u64 = 0;
        for &(id, x, y, _, _) in scene {
            if let Some(&(_, px, py)) =
                self.prev_motion_positions.iter().find(|&&(pid, _, _)| pid == id)
            {
                total_disp +=
                    u64::from((x - px).unsigned_abs()) + u64::from((y - py).unsigned_abs());
            }
        }

        // Record current positions for the next frame.
        self.prev_motion_positions.clear();
        self.prev_motion_positions
            .extend(scene.iter().map(|&(id, x, y, _, _)| (id, x, y)));

        let base = self.temporal_blur_mix_ratio;
        if total_disp == 0 {
            return base;
        }
        // Linear attenuation: ~16px of aggregate motion fully suppresses history.
        let atten = (total_disp as f32 / 16.0).min(1.0);
        base * (1.0 - atten)
    }

    /// Blend the current blur result with the cached previous blur into
    /// `temporal_mix_fbo` (allocated lazily at half-res) and return the mixed
    /// texture. Inputs are read-only; output is a distinct FBO to avoid a
    /// read/write hazard.
    ///
    /// # Safety
    /// Caller must ensure `gl` is valid and both textures are valid half-res blur textures.
    pub(crate) unsafe fn run_temporal_mix(
        &mut self,
        gl: &ffi::Gles2,
        current_tex: u32,
        previous_tex: u32,
        ratio: f32,
    ) -> u32 {
        let (bw, bh) = match self.blur_fbos.first() {
            Some(l) => (l.width, l.height),
            None => return current_tex,
        };
        unsafe {
            let (fbo, tex) = match self.temporal_mix_fbo {
                Some(p) => p,
                None => {
                    let p = if self.hdr_enabled {
                        super::create_fbo_texture_10bit(gl, bw, bh)
                    } else {
                        super::create_fbo_texture(gl, bw, bh)
                    };
                    self.temporal_mix_fbo = Some(p);
                    p
                }
            };

            gl.BindFramebuffer(ffi::FRAMEBUFFER, fbo);
            gl.Viewport(0, 0, bw as i32, bh as i32);
            gl.Disable(ffi::BLEND);

            gl.UseProgram(self.temporal_blur_mix_program);
            gl.Uniform4f(self.temporal_blur_mix_uniforms.rect, 0.0, 0.0, bw as f32, bh as f32);
            let proj = ortho(0.0, bw as f32, bh as f32, 0.0);
            gl.UniformMatrix4fv(
                self.temporal_blur_mix_uniforms.projection,
                1,
                ffi::FALSE as u8,
                proj.as_ptr(),
            );
            gl.Uniform1f(self.temporal_blur_mix_uniforms.mix, ratio);

            gl.ActiveTexture(ffi::TEXTURE0);
            gl.BindTexture(ffi::TEXTURE_2D, current_tex);
            gl.Uniform1i(self.temporal_blur_mix_uniforms.current, 0);

            gl.ActiveTexture(ffi::TEXTURE1);
            gl.BindTexture(ffi::TEXTURE_2D, previous_tex);
            gl.Uniform1i(self.temporal_blur_mix_uniforms.previous, 1);

            gl.BindVertexArray(self.quad_vao);
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);

            // Restore default texture unit to avoid leaking unit 1 state.
            gl.ActiveTexture(ffi::TEXTURE0);
            gl.Enable(ffi::BLEND);

            tex
        }
    }

    // -----------------------------------------------------------------------
    // Adaptive blur quality
    // -----------------------------------------------------------------------

    /// Compute the effective blur quality for the screen-wide blur pass.
    ///
    /// The Wayland backend runs a single global dual-Kawase pass (unlike the
    /// X11 backend, which blurs per-window), so quality is computed globally:
    /// - When `blur_quality_auto` is off, the user's `blur_quality` is used as-is.
    /// - When on, recent GPU load can degrade quality *below* that baseline, but
    ///   never raise it above (auto may only reduce cost, never add it).
    pub(crate) fn compute_global_blur_quality(&self) -> BlurQuality {
        if !self.blur_quality_auto {
            return self.blur_quality;
        }

        // Adaptive: estimate from recent GPU load (percentage 0..100).
        let load_quality = if self.last_gpu_load >= 80 {
            BlurQuality::Minimal
        } else if self.last_gpu_load >= 70 {
            BlurQuality::Reduced
        } else {
            BlurQuality::Full
        };

        Self::more_reduced_blur_quality(load_quality, self.blur_quality)
    }

    /// Per-window blur quality (mirrors X11's `compute_window_blur_quality`).
    ///
    /// Wayland still runs ONE global blur pass per frame, but this lets us pick
    /// quality based on the most-demanding visible frosted window (focused +
    /// onscreen wins). Caller should `max` across visible frosted windows.
    pub(crate) fn compute_window_blur_quality(
        &self,
        class_name: &str,
        x: i32, y: i32, w: u32, h: u32,
        is_focused: bool,
    ) -> BlurQuality {
        if !self.blur_quality_auto {
            return self.blur_quality;
        }
        let max_quality = self.blur_quality;

        // Status bar: never adapt (matches X11).
        let cfg = crate::config::CONFIG.load();
        let status_bar_name = cfg.status_bar_name();
        if !status_bar_name.is_empty()
            && (class_name == status_bar_name || class_name.contains(status_bar_name))
        {
            return self.blur_quality;
        }

        // Per-monitor override (precedence over GPU load).
        let monitor_id = self.find_window_monitor_id(x, y, w, h);
        if let Some(&override_quality) = self.blur_quality_by_monitor.get(&monitor_id) {
            return BlurQuality::min(override_quality, max_quality);
        }

        let is_onscreen = x + w as i32 > 0
            && y + h as i32 > 0
            && (x as i64) < self.screen_w as i64
            && (y as i64) < self.screen_h as i64;

        let load = self.last_gpu_load;
        let per_window = if load > 80 {
            if is_focused { BlurQuality::Full } else { BlurQuality::Minimal }
        } else if load > 70 {
            if is_focused {
                BlurQuality::Full
            } else if !is_onscreen {
                BlurQuality::Minimal
            } else {
                BlurQuality::Reduced
            }
        } else if is_focused {
            BlurQuality::Full
        } else if !is_onscreen {
            BlurQuality::Minimal
        } else {
            BlurQuality::Reduced
        };

        BlurQuality::min(per_window, max_quality)
    }

    /// Compute the highest blur quality needed across all visible frosted
    /// windows. Used to pick blur levels for the single global blur pass:
    /// the focused/onscreen frosted window drives quality, off-screen and
    /// unfocused frosted windows do not pull it down.
    pub(crate) fn compute_max_visible_blur_quality(
        &self,
        visible_scene: &[(u64, i32, i32, u32, u32)],
        focused: Option<u64>,
    ) -> BlurQuality {
        let mut best: Option<BlurQuality> = None;
        for &(win_id, x, y, w, h) in visible_scene {
            let win = match self.windows.get(&win_id) {
                Some(w) => w,
                None => continue,
            };
            if !win.is_frosted {
                continue;
            }
            let is_focused = focused == Some(win_id);
            let q = self.compute_window_blur_quality(
                &win.class_name, x, y, w, h, is_focused,
            );
            best = Some(match best {
                Some(prev) => BlurQuality::max(prev, q),
                None => q,
            });
        }
        best.unwrap_or_else(|| self.compute_global_blur_quality())
    }

    /// Find which monitor a window's center belongs to. Returns 0 if no match
    /// (treated as primary).
    fn find_window_monitor_id(&self, x: i32, y: i32, w: u32, h: u32) -> u32 {
        let cx = x + w as i32 / 2;
        let cy = y + h as i32 / 2;
        for &(id, mx, my, mw, mh, _) in &self.monitors {
            if cx >= mx
                && cx < mx + mw as i32
                && cy >= my
                && cy < my + mh as i32
            {
                return id;
            }
        }
        0
    }

    /// Return the more aggressively reduced of two qualities
    /// (Full = most blur levels, Minimal = fewest). Used so the global
    /// `blur_quality` setting bounds how much adaptive auto-quality may add.
    fn more_reduced_blur_quality(a: BlurQuality, b: BlurQuality) -> BlurQuality {
        fn rank(q: BlurQuality) -> u8 {
            match q {
                BlurQuality::Full => 0,
                BlurQuality::Reduced => 1,
                BlurQuality::Minimal => 2,
            }
        }
        if rank(a) >= rank(b) { a } else { b }
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

    /// Find the configured blur strength for a refresh rate: exact match, else
    /// the closest entry below `hz`, else the lowest entry. `table` is sorted
    /// ascending by `parse_blur_strength_by_hz`. Mirrors the X11 backend lookup.
    pub(crate) fn blur_strength_for_hz(table: &[(u32, u32)], hz: u32) -> Option<u32> {
        if table.is_empty() {
            return None;
        }
        for (i, &(config_hz, strength)) in table.iter().enumerate() {
            if config_hz == hz {
                return Some(strength);
            }
            if config_hz > hz {
                return Some(if i > 0 { table[i - 1].1 } else { strength });
            }
        }
        table.last().map(|p| p.1)
    }

    /// Apply Hz-aware blur strength from the primary monitor's refresh rate.
    /// Called on init and on every monitor-layout change (hotplug / mode change)
    /// so `blur_strength_by_hz` tracks the live refresh rate instead of staying
    /// fixed at the config default.
    pub(crate) fn apply_dynamic_blur_strength(&mut self, primary_hz: u32) {
        // Record the primary refresh rate for parity with the X11 backend's
        // monitor_refresh_rates map (id 0 == primary).
        self.monitor_refresh_rates.insert(0, primary_hz);
        if let Some(strength) = Self::blur_strength_for_hz(&self.blur_strength_by_hz, primary_hz) {
            if strength != self.blur_strength {
                log::info!(
                    "compositor: dynamic blur strength at {}Hz: {} (was {})",
                    primary_hz,
                    strength,
                    self.blur_strength
                );
                self.blur_strength = strength;
                self.needs_render = true;
            }
        }
    }

    /// Record per-monitor refresh rates and pick blur strength from the highest
    /// Hz across the live output list.
    ///
    /// Why max-Hz: Wayland blur is a single screen-wide dual-Kawase pass shared
    /// by every monitor (see [[project_wayland_effect_gaps]]), so we cannot
    /// vary strength per-output the way X11 does. Picking max means the
    /// highest-Hz display gets a strength that fits its frame budget; lower-Hz
    /// outputs simply use that same blur — never a quality regression on the
    /// fast display, never a frame-time blow-up on the slow one.
    pub(crate) fn apply_per_monitor_refresh_rates(&mut self, hz_pairs: &[(u32, u32)]) {
        self.monitor_refresh_rates.clear();
        for &(id, hz) in hz_pairs {
            self.monitor_refresh_rates.insert(id, hz);
        }
        let max_hz = hz_pairs.iter().map(|&(_, hz)| hz).max().unwrap_or(60);
        self.apply_dynamic_blur_strength(max_hz);
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
    fn test_more_reduced_blur_quality_picks_fewer_levels() {
        use BlurQuality::*;
        // Full = most levels, Minimal = fewest. The "more reduced" of two
        // qualities is the one with fewer levels.
        assert_eq!(WaylandCompositor::more_reduced_blur_quality(Full, Minimal), Minimal);
        assert_eq!(WaylandCompositor::more_reduced_blur_quality(Reduced, Full), Reduced);
        assert_eq!(WaylandCompositor::more_reduced_blur_quality(Reduced, Minimal), Minimal);
        assert_eq!(WaylandCompositor::more_reduced_blur_quality(Full, Full), Full);
        // Symmetric.
        assert_eq!(WaylandCompositor::more_reduced_blur_quality(Minimal, Full), Minimal);
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
    fn test_blur_strength_for_hz() {
        let table = WaylandCompositor::parse_blur_strength_by_hz("30:1,60:2,120:3,144:4,240:5");
        // exact match
        assert_eq!(WaylandCompositor::blur_strength_for_hz(&table, 60), Some(2));
        assert_eq!(WaylandCompositor::blur_strength_for_hz(&table, 144), Some(4));
        // closest entry below
        assert_eq!(WaylandCompositor::blur_strength_for_hz(&table, 75), Some(2));
        assert_eq!(WaylandCompositor::blur_strength_for_hz(&table, 165), Some(4));
        // below lowest entry -> lowest entry
        assert_eq!(WaylandCompositor::blur_strength_for_hz(&table, 24), Some(1));
        // above highest entry -> highest entry
        assert_eq!(WaylandCompositor::blur_strength_for_hz(&table, 360), Some(5));
        // empty table -> None
        assert_eq!(WaylandCompositor::blur_strength_for_hz(&[], 60), None);
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
