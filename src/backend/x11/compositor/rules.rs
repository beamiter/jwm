// Config rule lookups, VRR, monitor, blur quality
#[allow(unused_imports)]
use super::*;
#[allow(unused_imports)]
use glow::HasContext;
#[allow(unused_imports)]
use std::collections::HashMap;
#[allow(unused_imports)]
use std::ffi::CString;
#[allow(unused_imports)]
use std::sync::Arc;
#[allow(unused_imports)]
use std::sync::mpsc;
#[allow(unused_imports)]
use x11rb::connection::{Connection, RequestConnection};
#[allow(unused_imports)]
use x11rb::wrapper::ConnectionExt as WrapperExt;
#[allow(unused_imports)]
use x11rb::protocol::composite::ConnectionExt as CompositeExt;
#[allow(unused_imports)]
use x11rb::protocol::damage::{self, ConnectionExt as DamageExt};
#[allow(unused_imports)]
use x11rb::protocol::xfixes::ConnectionExt as XFixesExt;
#[allow(unused_imports)]
use x11rb::protocol::xproto::{self, ConnectionExt as XProtoExt};
#[allow(unused_imports)]
use x11rb::protocol::randr::ConnectionExt as RandrExt;
#[allow(unused_imports)]
use x11rb::rust_connection::RustConnection;
#[allow(unused_imports)]
use super::math::ortho;

/// ASCII case-insensitive substring test that performs no heap allocation.
/// Window class names are ASCII identifiers in practice, so ASCII case folding
/// is sufficient and avoids the per-call `String` allocs that
/// `haystack.to_lowercase().contains(&needle.to_lowercase())` incurs.
fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
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

impl Compositor {
    /// Check if a window class matches any entry in an exclude list.
    pub(super) fn class_matches_exclude(class_name: &str, exclude_list: &[String]) -> bool {
        if class_name.is_empty() {
            return false;
        }
        // Screenshot overlays like Flameshot are full-screen translucent windows
        // that update every pointer move. Running blur/shadow/rounding on them is
        // very expensive and causes visible stutter during region selection.
        if class_name.eq_ignore_ascii_case("flameshot") {
            return true;
        }
        exclude_list.iter().any(|ex| ex.eq_ignore_ascii_case(class_name))
    }

    /// Look up per-window opacity from opacity_rules.
    pub(super) fn lookup_opacity_rule(&self, class_name: &str) -> Option<f32> {
        if class_name.is_empty() {
            return None;
        }
        for rule in &self.opacity_rules {
            if rule.class_name.eq_ignore_ascii_case(class_name) {
                return Some(rule.opacity);
            }
        }
        None
    }

    /// Look up per-window corner radius (feature 3).
    pub(super) fn lookup_corner_radius_rule(&self, class_name: &str) -> Option<f32> {
        if class_name.is_empty() {
            return None;
        }
        for rule in &self.corner_radius_rules {
            if rule.class_name.eq_ignore_ascii_case(class_name) {
                return Some(rule.radius);
            }
        }
        None
    }

    /// Look up whether a window should have frosted glass effect.
    pub(super) fn lookup_frosted_glass_rule(&self, class_name: &str) -> bool {
        if class_name.is_empty() {
            return false;
        }
        self.frosted_glass_rules.iter().any(|r| r.eq_ignore_ascii_case(class_name))
    }

    /// Detect if window is a game (for VRR)
    pub(super) fn detect_game_window(&self, class_name: &str) -> bool {
        if class_name.is_empty() {
            return false;
        }
        let cfg = crate::config::CONFIG.load();
        let behavior = cfg.behavior();

        // Check user's game_classes list (substring match).
        for game_class in &behavior.game_classes {
            if contains_ignore_case(class_name, game_class) {
                return true;
            }
        }

        // Built-in game/emulator detection (exact match).
        const BUILTIN_GAME_CLASSES: &[&str] = &[
            "steam", "steamapps", "proton", "dxvk", "lutris", "wine",
            "minecraft", "dosbox", "mgba", "pcsx2", "yuzu", "dolphin",
        ];
        BUILTIN_GAME_CLASSES
            .iter()
            .any(|g| class_name.eq_ignore_ascii_case(g))
    }

    /// Check if currently focused window is a game
    pub(crate) fn is_focused_window_game(&self, focused_win: Option<u32>) -> bool {
        match focused_win {
            Some(win) => self.is_game_window.get(&win).copied().unwrap_or(false),
            None => false,
        }
    }

    /// Update VRR state based on focused window type
    pub(crate) fn update_vrr_state(&mut self, focused_win: Option<u32>) {
        let cfg = crate::config::CONFIG.load();
        let behavior = cfg.behavior();

        if !behavior.vrr_enabled {
            self.vrr_active = false;
            return;
        }

        // Limit updates to once per second to avoid flapping
        if self.vrr_last_check.elapsed().as_secs() < 1 {
            return;
        }
        self.vrr_last_check = std::time::Instant::now();

        // Enable VRR for game windows, disable for desktop
        let should_vrr = self.is_focused_window_game(focused_win);
        if should_vrr != self.vrr_active {
            self.vrr_active = should_vrr;
            log::info!("VRR {}", if should_vrr { "enabled" } else { "disabled" });
        }
    }

    /// Get current VRR refresh rate target (Hz)
    pub(crate) fn get_vrr_refresh_rate(&self) -> u32 {
        if self.vrr_active {
            let cfg = crate::config::CONFIG.load();
            let behavior = cfg.behavior();
            behavior.vrr_max_fps
        } else {
            60  // Default refresh rate for non-game windows
        }
    }

    /// Record input event for latency tracking (Task 8)
    pub(crate) fn record_input_event(&mut self) {
        self.frame_stats.last_input_time = Some(std::time::Instant::now());
    }

    /// Compute and record input→display latency when frame is rendered
    pub(super) fn record_latency_sample(&mut self) {
        if let Some(input_time) = self.frame_stats.last_input_time {
            let now = std::time::Instant::now();
            let input_to_render_ms = now.duration_since(input_time).as_secs_f32() * 1000.0;

            // Estimate GPU→display latency using OML sync or vblank period
            let gpu_to_display_ms = if let Some(oml) = &self.oml {
                // If OML available, estimate remaining time to vblank
                if let Some((ust, _msc, _sbc)) = oml.get_sync_values() {
                    // Assume 60Hz default; in practice, would query RandR for actual refresh rate
                    let vblank_interval_ns = 16_666_667u64;  // 60Hz = 16.67ms
                    let frame_age_ns = ust % vblank_interval_ns;
                    let time_to_next_vblank_ns = vblank_interval_ns - frame_age_ns;
                    (time_to_next_vblank_ns as f32 / 1_000_000.0) + 1.0  // +1ms buffer for display pipeline
                } else {
                    // Fallback: assume 1 frame time (~16.67ms at 60Hz)
                    16.67
                }
            } else {
                // Fallback without OML: assume 1-2 frames of pipeline latency
                33.33
            };

            let total_latency_ms = input_to_render_ms + gpu_to_display_ms;

            self.frame_stats.latency_samples.push_back(total_latency_ms);
            // Ring buffer: keep最多 300 samples (~5 seconds at 60fps).
            // VecDeque::pop_front is O(1); Vec::remove(0) was an O(N) memmove.
            if self.frame_stats.latency_samples.len() > 300 {
                self.frame_stats.latency_samples.pop_front();
            }

            // Diagnostic logging for high latency
            if total_latency_ms > 100.0 {
                log::warn!(
                    "compositor: high input latency detected: {:.1}ms (input→render: {:.1}ms, gpu→display: {:.1}ms)",
                    total_latency_ms, input_to_render_ms, gpu_to_display_ms
                );
            } else if total_latency_ms > 50.0 {
                log::debug!(
                    "compositor: elevated input latency: {:.1}ms (input→render: {:.1}ms, gpu→display: {:.1}ms)",
                    total_latency_ms, input_to_render_ms, gpu_to_display_ms
                );
            }

            // Clear the input timestamp after recording
            self.frame_stats.last_input_time = None;
        }
    }

    /// Compute latency statistics (p50, p95, p99)
    pub(super) fn compute_latency_stats(&self) -> (f32, f32, f32, f32) {
        if self.frame_stats.latency_samples.is_empty() {
            return (0.0, 0.0, 0.0, 0.0);
        }

        let mut sorted: Vec<f32> = self.frame_stats.latency_samples.iter().copied().collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let len = sorted.len();
        let p50_idx = (len * 50 / 100).min(len - 1);
        let p95_idx = (len * 95 / 100).min(len - 1);
        let p99_idx = (len * 99 / 100).min(len - 1);

        let avg = sorted.iter().sum::<f32>() / len as f32;
        (avg, sorted[p50_idx], sorted[p95_idx], sorted[p99_idx])
    }

    /// P5B: Build monitor rectangles from RandR outputs
    pub(super) fn build_monitor_rects(conn: &Arc<RustConnection>, root: u32) -> Vec<(u32, i32, i32, u32, u32)> {
        // Query RandR for outputs to get monitor positions and dimensions
        let mut rects = Vec::new();

        // Try RandR 1.5 get_monitors API first
        if let Ok(ver_cookie) = conn.as_ref().randr_query_version(1, 5) {
            if let Ok(ver) = ver_cookie.reply() {
                if ver.major_version > 1 || (ver.major_version == 1 && ver.minor_version >= 5) {
                    if let Ok(mon_cookie) = conn.as_ref().randr_get_monitors(root, true) {
                        if let Ok(reply) = mon_cookie.reply() {
                            for (idx, mon) in reply.monitors.iter().enumerate() {
                                if mon.width > 0 && mon.height > 0 {
                                    rects.push((idx as u32, mon.x as i32, mon.y as i32, mon.width as u32, mon.height as u32));
                                }
                            }
                            if !rects.is_empty() {
                                return rects;
                            }
                        }
                    }
                }
            }
        }

        // Fallback: use screen resources (older RandR)
        if let Ok(res_cookie) = conn.as_ref().randr_get_screen_resources(root) {
            if let Ok(resources) = res_cookie.reply() {
                for (idx, crtc_id) in resources.crtcs.iter().enumerate() {
                    if let Ok(info_cookie) = conn.as_ref().randr_get_crtc_info(*crtc_id, 0) {
                        if let Ok(info) = info_cookie.reply() {
                            if info.width > 0 && info.height > 0 {
                                rects.push((idx as u32, info.x as i32, info.y as i32, info.width as u32, info.height as u32));
                            }
                        }
                    }
                }
                if !rects.is_empty() {
                    return rects;
                }
            }
        }

        // Fallback: return empty vector (will use center-point detection with fallback to monitor 0)
        rects
    }

    /// P5B Phase 2: Build monitor refresh rates from RandR outputs
    pub(super) fn build_monitor_refresh_rates(conn: &Arc<RustConnection>, root: u32) -> HashMap<u32, u32> {
        let mut rates = HashMap::new();

        // Helper to calculate refresh rate from mode info
        fn calc_refresh_mhz(mode: &x11rb::protocol::randr::ModeInfo) -> u32 {
            if mode.htotal == 0 || mode.vtotal == 0 {
                return 60000; // 60Hz fallback
            }
            let dot_clock = mode.dot_clock as u64;
            let htotal = mode.htotal as u64;
            let vtotal = mode.vtotal as u64;
            ((dot_clock * 1000) / (htotal * vtotal)) as u32
        }

        // Try RandR 1.5 get_monitors API
        if let Ok(ver_cookie) = conn.as_ref().randr_query_version(1, 5) {
            if let Ok(ver) = ver_cookie.reply() {
                if ver.major_version > 1 || (ver.major_version == 1 && ver.minor_version >= 5) {
                    // Get screen resources for mode info
                    if let Ok(res_cookie) = conn.as_ref().randr_get_screen_resources(root) {
                        if let Ok(resources) = res_cookie.reply() {
                            let modes = resources.modes;

                            if let Ok(mon_cookie) = conn.as_ref().randr_get_monitors(root, true) {
                                if let Ok(reply) = mon_cookie.reply() {
                                    for (idx, mon) in reply.monitors.iter().enumerate() {
                                        // Get first output's current mode to determine refresh rate
                                        if let Some(&output_id) = mon.outputs.first() {
                                            if let Ok(output_cookie) = conn.as_ref().randr_get_output_info(output_id, 0) {
                                                if let Ok(output_info) = output_cookie.reply() {
                                                    if output_info.crtc != 0 {
                                                        if let Ok(crtc_cookie) = conn.as_ref().randr_get_crtc_info(output_info.crtc, 0) {
                                                            if let Ok(crtc_info) = crtc_cookie.reply() {
                                                                let refresh = modes.iter()
                                                                    .find(|m| m.id == crtc_info.mode)
                                                                    .map(calc_refresh_mhz)
                                                                    .unwrap_or(60000);
                                                                rates.insert(idx as u32, refresh / 1000); // mHz -> Hz
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    if !rates.is_empty() {
                                        return rates;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // Fallback: use screen resources directly
        if let Ok(res_cookie) = conn.as_ref().randr_get_screen_resources(root) {
            if let Ok(resources) = res_cookie.reply() {
                let modes = resources.modes;
                for (idx, crtc_id) in resources.crtcs.iter().enumerate() {
                    if let Ok(info_cookie) = conn.as_ref().randr_get_crtc_info(*crtc_id, 0) {
                        if let Ok(info) = info_cookie.reply() {
                            if info.width > 0 && info.height > 0 {
                                let refresh = modes.iter()
                                    .find(|m| m.id == info.mode)
                                    .map(calc_refresh_mhz)
                                    .unwrap_or(60000);
                                rates.insert(idx as u32, refresh / 1000); // mHz -> Hz
                            }
                        }
                    }
                }
            }
        }

        rates
    }

    /// P5B Phase 1: Map window position to monitor index using real RandR geometry.
    /// Picks the monitor with the largest rectangular overlap area; falls back
    /// to center-point containment, then to the first monitor (primary).
    pub(super) fn get_window_monitor_id(&self, window_x: i32, window_y: i32, window_w: u32, window_h: u32) -> u32 {
        if let Some(id) = Self::monitor_id_by_overlap(&self.monitor_rects, window_x, window_y, window_w, window_h) {
            return id;
        }
        self.monitor_rects.first().map(|r| r.0).unwrap_or(0)
    }

    pub(crate) fn monitor_id_by_overlap(
        monitors: &[(u32, i32, i32, u32, u32)],
        x: i32,
        y: i32,
        w: u32,
        h: u32,
    ) -> Option<u32> {
        if monitors.is_empty() {
            return None;
        }
        let wx2 = x + w as i32;
        let wy2 = y + h as i32;
        let mut best: Option<(u32, i64)> = None;
        for &(id, mx, my, mw, mh) in monitors {
            let mx2 = mx + mw as i32;
            let my2 = my + mh as i32;
            let ix = (wx2.min(mx2) - x.max(mx)).max(0) as i64;
            let iy = (wy2.min(my2) - y.max(my)).max(0) as i64;
            let area = ix * iy;
            if area > 0 && best.map_or(true, |(_, ba)| area > ba) {
                best = Some((id, area));
            }
        }
        if let Some((id, _)) = best {
            return Some(id);
        }
        let cx = x + w as i32 / 2;
        let cy = y + h as i32 / 2;
        for &(id, mx, my, mw, mh) in monitors {
            if cx >= mx && cx < mx + mw as i32 && cy >= my && cy < my + mh as i32 {
                return Some(id);
            }
        }
        None
    }

    /// P5B Phase 2: Get refresh rate for a specific monitor
    pub(super) fn get_monitor_refresh_hz(&self, monitor_id: u32) -> u32 {
        self.monitor_refresh_rates
            .get(&monitor_id)
            .copied()
            .unwrap_or(60) // Fallback to 60Hz if not found
    }

    /// Rebuild monitor geometry + refresh-rate maps from RandR after a layout
    /// change (hotplug / mode change). Both maps were previously only built once
    /// at init, so per-window blur quality and per-monitor refresh lookups went
    /// stale when displays were added/removed or modes changed.
    ///
    /// Map-only (no GL): the global Hz->blur_strength FBO rebuild is left to the
    /// config-apply path, which runs with a current GL context — recreating FBOs
    /// here (event-dispatch context) cannot assume the GL context is bound.
    pub(in crate::backend::x11) fn refresh_monitor_layout(&mut self, root: u32) {
        self.monitor_rects = Self::build_monitor_rects(&self.conn, root);
        self.monitor_refresh_rates = Self::build_monitor_refresh_rates(&self.conn, root);
        log::info!(
            "compositor: monitor layout refreshed: {} monitors",
            self.monitor_rects.len()
        );
    }

    /// Compute blur quality for a specific window (Task 10: Adaptive Blur + Per-Monitor)
    pub(super) fn compute_window_blur_quality(&self, wt: &WindowTexture, focused: Option<u32>) -> BlurQuality {
        let cfg = crate::config::CONFIG.load();
        let behavior = cfg.behavior();

        // If adaptive blur is disabled, use global quality
        if !behavior.blur_quality_auto {
            return self.blur_quality;
        }

        // Global quality acts as an upper bound (e.g. Minimal under heavy load)
        let max_quality = self.blur_quality;

        // Check if this window is focused
        let is_focused = focused.map_or(false, |f| f == wt.x11_win);

        // Check if window is visible on screen (simple heuristic)
        let is_onscreen = wt.x + (wt.w as i32) > 0
            && wt.y + (wt.h as i32) > 0
            && wt.x < self.screen_w as i32
            && wt.y < self.screen_h as i32;

        // Status bar should not have adaptive blur quality changes
        let status_bar_name = cfg.status_bar_name();
        let is_statusbar = wt.class_name == status_bar_name || wt.class_name.contains(status_bar_name);
        if is_statusbar {
            return self.blur_quality;
        }

        // P5B: Apply per-monitor quality override using real RandR geometry
        let monitor_id = self.get_window_monitor_id(wt.x, wt.y, wt.w, wt.h);
        let monitor_override = self.blur_quality_by_monitor.get(&monitor_id);

        if let Some(&override_quality) = monitor_override {
            // Per-monitor config takes precedence
            return override_quality.min(max_quality);
        }

        // Estimate GPU load from recent frame times (naive approach)
        // Assume 60Hz = 16.67ms ideal frame time; if actual is higher, GPU is under pressure
        let current_gpu_load = {
            let target_frame_time_ms = 1000.0 / 60.0;  // 60Hz baseline
            if self.frame_stats.frame_times.is_empty() {
                0  // No data yet
            } else {
                let avg_frame_time_ms = self.frame_stats.frame_times.iter().sum::<f32>()
                    / self.frame_stats.frame_times.len() as f32;
                let load = (avg_frame_time_ms / target_frame_time_ms * 100.0) as u32;
                load.min(100)
            }
        };

        // Apply hysteresis: only update if delta > 5% or elapsed > 0.5s
        // This prevents rapid quality oscillation when load hovers around thresholds
        let gpu_load = if current_gpu_load > self.last_gpu_load + 5
            || current_gpu_load + 5 < self.last_gpu_load
            || self.last_gpu_load_update.elapsed().as_millis() > 500
        {
            // Update the cached load
            // Note: We can't mutate self here, so we use current_gpu_load
            // The actual update happens in the blur rendering pass
            current_gpu_load
        } else {
            // Use previous value for stability
            self.last_gpu_load
        };

        // Under high GPU load (>80%), only focused window keeps full quality
        // Unfocused/off-screen windows degrade to minimal to reduce GPU pressure
        let per_window_quality = if gpu_load > 80 {
            // Critical load: protect focused window, minimize others
            if is_focused {
                BlurQuality::Full  // Focused: maintain full quality
            } else {
                BlurQuality::Minimal  // Unfocused/off-screen: minimal
            }
        } else if gpu_load > 70 {
            // Moderate load: reduce unfocused windows only
            if is_focused {
                BlurQuality::Full  // Focused: full quality
            } else if !is_onscreen {
                BlurQuality::Minimal  // Off-screen: minimal
            } else {
                BlurQuality::Reduced  // Inactive but visible: reduced
            }
        } else {
            // Low load: normal priority-based tiering
            if is_focused {
                BlurQuality::Full
            } else if !is_onscreen {
                BlurQuality::Minimal
            } else {
                BlurQuality::Reduced
            }
        };

        // Apply global cap (animation/overview can further reduce)
        match max_quality {
            BlurQuality::Minimal => BlurQuality::Minimal,
            BlurQuality::Reduced => match per_window_quality {
                BlurQuality::Full => BlurQuality::Reduced,
                other => other,
            },
            BlurQuality::Full => per_window_quality,
        }
    }

    /// Parse blur_strength_by_hz config string: "60:2,75:2.5,144:3.5" -> [(60, 2), (75, 2), (144, 3)]
    pub(super) fn parse_blur_strength_by_hz(config_str: &str) -> Vec<(u32, u32)> {
        let mut result = Vec::new();
        if config_str.is_empty() {
            return result;
        }
        for pair in config_str.split(',') {
            let parts: Vec<&str> = pair.trim().split(':').collect();
            if parts.len() == 2 {
                if let (Ok(hz), Ok(strength_f)) = (parts[0].trim().parse::<u32>(), parts[1].trim().parse::<f32>()) {
                    result.push((hz, strength_f as u32));
                }
            }
        }
        result.sort_by_key(|p| p.0);  // Sort by Hz ascending
        result
    }

    /// Get blur strength for a given refresh rate Hz (static version for use during init).
    /// If exact Hz not found, returns closest lower, or if none, closest higher.
    pub(super) fn new_get_blur_strength_for_hz_static(blur_strength_by_hz: &[(u32, u32)], hz: u32) -> Option<u32> {
        if blur_strength_by_hz.is_empty() {
            return None;
        }

        // Find exact match or closest lower
        for (i, &(config_hz, strength)) in blur_strength_by_hz.iter().enumerate() {
            if config_hz == hz {
                return Some(strength);
            }
            if config_hz > hz {
                // Not found exact, try previous
                if i > 0 {
                    return Some(blur_strength_by_hz[i - 1].1);
                }
                // No lower value, use this one
                return Some(strength);
            }
        }
        // All values are lower, use the last one
        blur_strength_by_hz.last().map(|p| p.1)
    }

    /// Parse blur_quality_by_monitor config string: "primary:Full,secondary:Reduced"
    pub(super) fn parse_blur_quality_by_monitor(config_str: &str) -> HashMap<u32, BlurQuality> {
        let mut result = HashMap::new();
        if config_str.is_empty() {
            return result;
        }
        let monitor_names = ["primary", "secondary", "tertiary", "quaternary", "quinary"];
        for pair in config_str.split(',') {
            let parts: Vec<&str> = pair.trim().split(':').collect();
            if parts.len() == 2 {
                let monitor_name = parts[0].trim();
                let quality_str = parts[1].trim();

                // Map monitor name to index
                if let Some(idx) = monitor_names.iter().position(|&n| n == monitor_name) {
                    let quality = match quality_str {
                        "Full" => BlurQuality::Full,
                        "Reduced" => BlurQuality::Reduced,
                        "Minimal" => BlurQuality::Minimal,
                        _ => continue,
                    };
                    result.insert(idx as u32, quality);
                }
            }
        }
        result
    }

    /// Get blur strength for a given refresh rate Hz.
    /// If exact Hz not found, returns closest lower, or if none, closest higher.
    pub(super) fn get_blur_strength_for_hz(&self, hz: u32) -> Option<u32> {
        if self.blur_strength_by_hz.is_empty() {
            return None;
        }

        // Find exact match or closest lower
        for (i, &(config_hz, strength)) in self.blur_strength_by_hz.iter().enumerate() {
            if config_hz == hz {
                return Some(strength);
            }
            if config_hz > hz {
                // Not found exact, try previous
                if i > 0 {
                    return Some(self.blur_strength_by_hz[i - 1].1);
                }
                // No lower value, use this one
                return Some(strength);
            }
        }
        // All values are lower, use the last one
        self.blur_strength_by_hz.last().map(|p| p.1)
    }

    /// Compute a hash of all visible window positions (for temporal blur reuse detection).
    /// If positions are stable (hash unchanged), we can reuse/blend previous frame's blur.
    pub(super) fn compute_window_positions_hash(&self) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        use std::hash::{Hash, Hasher};

        let mut sorted_windows: Vec<_> = self.windows.iter().collect();
        sorted_windows.sort_by_key(|(id, _)| *id);

        for (_, wt) in sorted_windows {
            if wt.dirty || wt.fading_out {
                continue;  // Skip dirty/fading windows for stability
            }
            wt.x.hash(&mut hasher);
            wt.y.hash(&mut hasher);
            wt.w.hash(&mut hasher);
            wt.h.hash(&mut hasher);
            wt.fade_opacity.to_bits().hash(&mut hasher);
        }
        hasher.finish()
    }

    /// P4: Apply temporal blur mixing: blend current with previous frame if content is stable
    /// When window positions are unchanged, we can mix current blur with previous blur for:
    /// - Higher visual stability (less flicker from blur recomputation)
    /// - Lower GPU cost (fewer blur samples needed for same quality)
    /// Lazily create prev_blur_fbo. Must be called once per frame before
    /// `apply_temporal_blur_mix` so that the mix path can run with `&self`
    /// inside loops that hold immutable borrows of self.
    pub(super) fn ensure_prev_blur_fbo(&mut self) {
        if !self.temporal_blur_enabled {
            return;
        }
        if self.prev_blur_fbo.is_none() {
            if let Ok((fbo, tex)) = unsafe { Self::create_scene_fbo(&self.gl, self.screen_w, self.screen_h) } {
                self.prev_blur_fbo = Some((fbo, tex));
            }
        }
    }

    pub(super) fn apply_temporal_blur_mix(&self, current_blur_tex: glow::Texture) -> glow::Texture {
        if !self.temporal_blur_enabled {
            return current_blur_tex;
        }

        let (prev_fbo, prev_tex) = match &self.prev_blur_fbo {
            Some((fbo, tex)) => (*fbo, *tex),
            None => return current_blur_tex,
        };

        // If we have a previous frame, blend current with previous
        let has_prev = self.prev_window_positions_hash != 0;
        if !has_prev {
            // No previous frame yet - just save current for next frame
            unsafe {
                self.gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
                self.gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(prev_fbo));
                self.gl.blit_framebuffer(
                    0, 0, self.screen_w as i32, self.screen_h as i32,
                    0, 0, self.screen_w as i32, self.screen_h as i32,
                    glow::COLOR_BUFFER_BIT, glow::NEAREST,
                );
            }
            return current_blur_tex;
        }

        // Perform temporal mix: blend current with previous
        let proj = math::ortho(0.0, self.screen_w as f32, self.screen_h as f32, 0.0, -1.0, 1.0);

        unsafe {
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(prev_fbo));
            self.gl.use_program(Some(self.temporal_blur_mix_program));
            self.gl.viewport(0, 0, self.screen_w as i32, self.screen_h as i32);

            // Bind current blur as input
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(current_blur_tex));
            self.gl.uniform_1_i32(
                self.gl.get_uniform_location(self.temporal_blur_mix_program, "u_current_blur").as_ref(),
                0,
            );

            // Bind previous blur as input
            self.gl.active_texture(glow::TEXTURE1);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(prev_tex));
            self.gl.uniform_1_i32(
                self.gl.get_uniform_location(self.temporal_blur_mix_program, "u_previous_blur").as_ref(),
                1,
            );

            // Set temporal mix ratio
            self.gl.uniform_1_f32(
                self.gl.get_uniform_location(self.temporal_blur_mix_program, "u_temporal_mix").as_ref(),
                self.temporal_blur_mix_ratio,
            );

            // Set projection and screen rect
            self.gl.uniform_matrix_4_f32_slice(
                self.temporal_blur_mix_uniforms.projection.as_ref(),
                false,
                &proj,
            );
            self.gl.uniform_4_f32(
                self.temporal_blur_mix_uniforms.rect.as_ref(),
                0.0, 0.0,
                self.screen_w as f32,
                self.screen_h as f32,
            );

            // Draw mix quad
            self.gl.bind_vertex_array(Some(self.quad_vao));
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            self.gl.bind_vertex_array(None);

            // Restore state
            self.gl.active_texture(glow::TEXTURE0);
        }

        // Return mixed texture (stored in prev_fbo now)
        prev_tex
    }

    /// P4: Finalize temporal blur state at end of render_frame
    /// Called after all blur computation to update prev_blur_fbo for next frame
    pub(super) fn finalize_temporal_blur(&mut self) {
        if !self.temporal_blur_enabled {
            return;
        }

        // On first frame (no previous state), initialize prev_blur_fbo from scene_fbo
        if self.prev_window_positions_hash == 0 {
            if let Some((_, _scene_tex)) = &self.scene_fbo {
                if self.prev_blur_fbo.is_none() {
                    if let Ok((fbo, tex)) = unsafe { Self::create_scene_fbo(&self.gl, self.screen_w, self.screen_h) } {
                        self.prev_blur_fbo = Some((fbo, tex));
                    }
                }

                // Copy scene texture to prev_blur_fbo for next frame blending
                if let Some((fbo, _)) = &self.prev_blur_fbo {
                    unsafe {
                        self.gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
                        self.gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(*fbo));
                        self.gl.blit_framebuffer(
                            0, 0, self.screen_w as i32, self.screen_h as i32,
                            0, 0, self.screen_w as i32, self.screen_h as i32,
                            glow::COLOR_BUFFER_BIT, glow::NEAREST,
                        );
                    }
                }
            }
        }
    }

    /// Whether a window should receive per-frame backdrop blur compositing.
    pub(super) fn needs_backdrop_blur(&self, wt: &WindowTexture, status_bar_name: &str) -> bool {
        // Skip backdrop blur for statusbar
        if wt.class_name == status_bar_name || wt.class_name.contains(status_bar_name) {
            return false;
        }
        if Self::class_matches_exclude(&wt.class_name, &self.blur_exclude) {
            return false;
        }
        // Skip backdrop blur for large override-redirect RGBA windows.  These
        // are typically screen-sharing overlays (e.g. Feishu/Lark) or screenshot
        // selection tools that are intentionally transparent.  Applying blur
        // behind them produces an unwanted frosted-glass effect that covers the
        // actual screen content.
        //
        // "Large" = covers at least 80 % of any single monitor in both dimensions.
        if wt.is_override_redirect && wt.has_rgba {
            let dominated = self.monitor_wallpapers.iter().any(|mw| {
                wt.w >= mw.mon_w * 4 / 5 && wt.h >= mw.mon_h * 4 / 5
            });
            if dominated {
                return false;
            }
        }
        let explicit_translucency = wt.fade_opacity < 1.0
            || wt.opacity_override.map_or(false, |o| o < 1.0);

        wt.is_frosted
            || explicit_translucency
            || wt.has_rgba
    }

    /// Look up per-window scale (feature 4).
    pub(super) fn lookup_scale_rule(&self, class_name: &str) -> Option<f32> {
        if class_name.is_empty() {
            return None;
        }
        for rule in &self.scale_rules {
            if rule.class_name.eq_ignore_ascii_case(class_name) {
                return Some(rule.scale);
            }
        }
        None
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // class_matches_exclude
    // -----------------------------------------------------------------------

    #[test]
    fn test_class_matches_exclude_empty_name() {
        let list = vec!["firefox".to_string()];
        assert!(!Compositor::class_matches_exclude("", &list));
    }

    #[test]
    fn test_class_matches_exclude_empty_list() {
        assert!(!Compositor::class_matches_exclude("firefox", &[]));
    }

    #[test]
    fn test_class_matches_exclude_exact_match() {
        let list = vec!["firefox".to_string(), "chromium".to_string()];
        assert!(Compositor::class_matches_exclude("firefox", &list));
        assert!(Compositor::class_matches_exclude("chromium", &list));
    }

    #[test]
    fn test_class_matches_exclude_case_insensitive() {
        let list = vec!["Firefox".to_string()];
        assert!(Compositor::class_matches_exclude("firefox", &list));
        assert!(Compositor::class_matches_exclude("FIREFOX", &list));
        assert!(Compositor::class_matches_exclude("FireFox", &list));
    }

    #[test]
    fn test_class_matches_exclude_not_in_list() {
        let list = vec!["firefox".to_string()];
        assert!(!Compositor::class_matches_exclude("chromium", &list));
    }

    #[test]
    fn test_class_matches_exclude_flameshot_hardcoded() {
        // Flameshot is always excluded regardless of list
        assert!(Compositor::class_matches_exclude("flameshot", &[]));
        assert!(Compositor::class_matches_exclude("Flameshot", &[]));
        assert!(Compositor::class_matches_exclude("FLAMESHOT", &[]));
    }

    // -----------------------------------------------------------------------
    // parse_blur_strength_by_hz
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_blur_strength_by_hz_empty() {
        let result = Compositor::parse_blur_strength_by_hz("");
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_blur_strength_by_hz_single() {
        let result = Compositor::parse_blur_strength_by_hz("60:2");
        assert_eq!(result, vec![(60, 2)]);
    }

    #[test]
    fn test_parse_blur_strength_by_hz_multiple() {
        let result = Compositor::parse_blur_strength_by_hz("60:2,75:2,144:3");
        assert_eq!(result, vec![(60, 2), (75, 2), (144, 3)]);
    }

    #[test]
    fn test_parse_blur_strength_by_hz_sorted_ascending() {
        // Input out of order; output must be sorted by Hz
        let result = Compositor::parse_blur_strength_by_hz("144:3,60:2,75:2");
        assert_eq!(result[0].0, 60);
        assert_eq!(result[1].0, 75);
        assert_eq!(result[2].0, 144);
    }

    #[test]
    fn test_parse_blur_strength_by_hz_float_strength_truncated() {
        // f32 cast to u32 truncates
        let result = Compositor::parse_blur_strength_by_hz("60:2.9");
        assert_eq!(result, vec![(60, 2)]);
    }

    #[test]
    fn test_parse_blur_strength_by_hz_invalid_entries_skipped() {
        let result = Compositor::parse_blur_strength_by_hz("60:2,bad,144:3");
        assert_eq!(result, vec![(60, 2), (144, 3)]);
    }

    #[test]
    fn test_parse_blur_strength_by_hz_whitespace_trimmed() {
        let result = Compositor::parse_blur_strength_by_hz(" 60 : 2 , 144 : 3 ");
        assert_eq!(result, vec![(60, 2), (144, 3)]);
    }

    // -----------------------------------------------------------------------
    // new_get_blur_strength_for_hz_static
    // -----------------------------------------------------------------------

    #[test]
    fn test_blur_strength_for_hz_empty_returns_none() {
        assert_eq!(Compositor::new_get_blur_strength_for_hz_static(&[], 60), None);
    }

    #[test]
    fn test_blur_strength_for_hz_exact_match() {
        let table = vec![(60, 2), (144, 4)];
        assert_eq!(Compositor::new_get_blur_strength_for_hz_static(&table, 60), Some(2));
        assert_eq!(Compositor::new_get_blur_strength_for_hz_static(&table, 144), Some(4));
    }

    #[test]
    fn test_blur_strength_for_hz_closest_lower() {
        let table = vec![(60, 2), (144, 4)];
        // 75Hz: no exact match; closest lower is 60Hz → strength 2
        assert_eq!(Compositor::new_get_blur_strength_for_hz_static(&table, 75), Some(2));
    }

    #[test]
    fn test_blur_strength_for_hz_below_all_uses_first() {
        let table = vec![(60, 2), (144, 4)];
        // 30Hz: below all entries → use first (60Hz)
        assert_eq!(Compositor::new_get_blur_strength_for_hz_static(&table, 30), Some(2));
    }

    #[test]
    fn test_blur_strength_for_hz_above_all_uses_last() {
        let table = vec![(60, 2), (144, 4)];
        // 240Hz: above all → use last (144Hz)
        assert_eq!(Compositor::new_get_blur_strength_for_hz_static(&table, 240), Some(4));
    }

    #[test]
    fn test_blur_strength_for_hz_single_entry() {
        let table = vec![(60, 3)];
        assert_eq!(Compositor::new_get_blur_strength_for_hz_static(&table, 30), Some(3));
        assert_eq!(Compositor::new_get_blur_strength_for_hz_static(&table, 60), Some(3));
        assert_eq!(Compositor::new_get_blur_strength_for_hz_static(&table, 120), Some(3));
    }

    // -----------------------------------------------------------------------
    // parse_blur_quality_by_monitor
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_blur_quality_by_monitor_empty() {
        let result = Compositor::parse_blur_quality_by_monitor("");
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_blur_quality_by_monitor_primary() {
        let result = Compositor::parse_blur_quality_by_monitor("primary:Full");
        assert_eq!(result.get(&0), Some(&BlurQuality::Full));
    }

    #[test]
    fn test_parse_blur_quality_by_monitor_secondary_reduced() {
        let result = Compositor::parse_blur_quality_by_monitor("secondary:Reduced");
        assert_eq!(result.get(&1), Some(&BlurQuality::Reduced));
    }

    #[test]
    fn test_parse_blur_quality_by_monitor_all_variants() {
        let result = Compositor::parse_blur_quality_by_monitor(
            "primary:Full,secondary:Reduced,tertiary:Minimal",
        );
        assert_eq!(result.get(&0), Some(&BlurQuality::Full));
        assert_eq!(result.get(&1), Some(&BlurQuality::Reduced));
        assert_eq!(result.get(&2), Some(&BlurQuality::Minimal));
    }

    #[test]
    fn test_parse_blur_quality_by_monitor_unknown_quality_skipped() {
        let result = Compositor::parse_blur_quality_by_monitor("primary:Ultra");
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_blur_quality_by_monitor_unknown_monitor_skipped() {
        let result = Compositor::parse_blur_quality_by_monitor("sixth:Full");
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_blur_quality_by_monitor_whitespace_trimmed() {
        let result = Compositor::parse_blur_quality_by_monitor(" primary : Full ");
        assert_eq!(result.get(&0), Some(&BlurQuality::Full));
    }

    // -----------------------------------------------------------------------
    // compute_latency_stats
    // -----------------------------------------------------------------------

    #[test]
    fn test_compute_latency_stats_empty() {
        // We can't construct Compositor directly; test the math inline
        // to mirror what compute_latency_stats does.
        let samples: Vec<f32> = vec![];
        let (avg, p50, p95, p99) = if samples.is_empty() {
            (0.0f32, 0.0f32, 0.0f32, 0.0f32)
        } else {
            let mut sorted = samples.clone();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let len = sorted.len();
            let avg = sorted.iter().sum::<f32>() / len as f32;
            (avg, sorted[len * 50 / 100], sorted[len * 95 / 100], sorted[len * 99 / 100])
        };
        assert_eq!(avg, 0.0);
        assert_eq!(p50, 0.0);
        assert_eq!(p95, 0.0);
        assert_eq!(p99, 0.0);
    }

    #[test]
    fn test_compute_latency_stats_uniform() {
        // All samples equal → all percentiles equal the value
        let samples: Vec<f32> = vec![20.0; 100];
        let mut sorted = samples.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let len = sorted.len();
        let avg = sorted.iter().sum::<f32>() / len as f32;
        let p50 = sorted[(len * 50 / 100).min(len - 1)];
        let p95 = sorted[(len * 95 / 100).min(len - 1)];
        let p99 = sorted[(len * 99 / 100).min(len - 1)];
        assert!((avg - 20.0).abs() < 0.001);
        assert!((p50 - 20.0).abs() < 0.001);
        assert!((p95 - 20.0).abs() < 0.001);
        assert!((p99 - 20.0).abs() < 0.001);
    }

    #[test]
    fn test_compute_latency_stats_ordered() {
        // p50 ≤ p95 ≤ p99
        let mut samples: Vec<f32> = (1..=100).map(|i| i as f32).collect();
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let len = samples.len();
        let p50 = samples[(len * 50 / 100).min(len - 1)];
        let p95 = samples[(len * 95 / 100).min(len - 1)];
        let p99 = samples[(len * 99 / 100).min(len - 1)];
        assert!(p50 <= p95);
        assert!(p95 <= p99);
    }

    #[test]
    fn test_monitor_id_by_overlap_x11_side_by_side() {
        let monitors = vec![
            (0u32, 0i32, 0i32, 1920u32, 1080u32),
            (1u32, 1920i32, 0i32, 1920u32, 1080u32),
        ];
        // Window entirely on monitor 1.
        assert_eq!(Compositor::monitor_id_by_overlap(&monitors, 2000, 100, 400, 300), Some(1));
        // Straddling, more area on monitor 0.
        assert_eq!(Compositor::monitor_id_by_overlap(&monitors, 1340, 100, 1000, 500), Some(0));
        // Off-screen below: no overlap, no center hit.
        assert_eq!(Compositor::monitor_id_by_overlap(&monitors, 100, 5000, 200, 200), None);
        // Empty monitors.
        assert_eq!(Compositor::monitor_id_by_overlap(&[], 0, 0, 100, 100), None);
    }
}
