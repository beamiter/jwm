// Feature control methods
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

impl Compositor {
    pub(crate) fn set_mouse_position(&mut self, x: f32, y: f32) {
        self.mouse_x = x;
        self.mouse_y = y;
        if self.edge_glow {
            self.edge_glow_tick(x, y);
        }
        if self.magnifier_enabled || self.window_tilt {
            self.needs_render = true;
        }
        if self.expose_active {
            self.expose_set_hover(x, y);
        }
    }

    /// Core edge-glow state machine (called from mouse events and render tick).
    ///
    /// - Mouse at edge (unsuppressed) → activate.
    /// - Mouse away or suppressed     → deactivate immediately.
    pub(super) fn edge_glow_tick(&mut self, mx: f32, my: f32) {
        let sw = self.screen_w as f32;
        let sh = self.screen_h as f32;
        let min_dist = mx.min(sw - mx).min(my).min(sh - my);
        let at_edge = min_dist < self.edge_glow_width;

        if at_edge && !self.edge_glow_suppressed {
            if !self.edge_glow_active {
                self.edge_glow_active = true;
                self.needs_render = true;
            }
        } else if self.edge_glow_active {
            self.edge_glow_active = false;
            self.needs_render = true;
        }
    }

    /// Immediately deactivate the edge glow and suppress re-activation
    /// until the pointer leaves the window (returns to root/desktop).
    pub(crate) fn deactivate_edge_glow(&mut self) {
        if self.edge_glow {
            self.edge_glow_suppressed = true;
            if self.edge_glow_active {
                self.edge_glow_active = false;
                self.needs_render = true;
            }
        }
    }

    /// Clear the edge-glow suppression (pointer returned to desktop).
    pub(crate) fn unsuppress_edge_glow(&mut self) {
        self.edge_glow_suppressed = false;
    }

    pub(crate) fn set_window_urgent(&mut self, x11_win: u32, urgent: bool) {
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            wt.is_urgent = urgent;
            self.needs_render = true;
        }
    }

    pub(crate) fn set_window_pip(&mut self, x11_win: u32, pip: bool) {
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            wt.is_pip = pip;
            self.needs_render = true;
        }
    }

    /// Notify the compositor about audio stream timing for a window.
    /// This lets the compositor schedule frame presentation to match
    /// each window's independent audio clock, preventing desync.
    pub(crate) fn notify_audio_timing(&mut self, x11_win: u32, fps: f32, buffer_latency_ms: u32) {
        self.audio_sync.register_stream(x11_win, fps, buffer_latency_ms);
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            wt.audio_sync_target = Some(fps);
        }
        // Register with OML for per-window vblank timing too
        if let Some(oml) = &mut self.oml {
            oml.register_window(x11_win, fps);
        }
    }

    /// Register a window for Present extension support
    #[allow(dead_code)]
    pub(crate) fn register_window_present(&mut self, x11_win: u32) {
        if let Some(present_mgr) = &mut self.present_mgr {
            match present_mgr.register_window(x11_win) {
                Ok(()) => {
                    log::debug!("compositor: window 0x{:x} registered with Present", x11_win);
                }
                Err(e) => {
                    log::warn!("compositor: failed to register 0x{:x} with Present: {}", x11_win, e);
                }
            }
        }
    }

    /// Present a window's pixmap at a specific MSC (for Present-enabled windows)
    #[allow(dead_code)]
    pub(crate) fn present_pixmap(
        &self,
        x11_win: u32,
        pixmap: u32,
        target_msc: u64,
        serial: u32,
    ) {
        if let Some(present_mgr) = &self.present_mgr {
            match present_mgr.present_pixmap(x11_win, pixmap, target_msc, serial) {
                Ok(()) => {
                    log::debug!(
                        "compositor: presented pixmap for 0x{:x} (serial={}, msc={})",
                        x11_win, serial, target_msc
                    );
                }
                Err(e) => {
                    log::debug!("compositor: present_pixmap failed for 0x{:x}: {}", x11_win, e);
                }
            }
        }
    }

    pub(crate) fn set_magnifier(&mut self, enabled: bool) {
        self.magnifier_enabled = enabled;
        self.ensure_postprocess_fbo();
        self.needs_render = true;
    }

    pub(crate) fn set_overview_monitor(&mut self, x: i32, y: i32, w: u32, h: u32) {
        self.overview_mon_x = x;
        self.overview_mon_y = y;
        self.overview_mon_w = w;
        self.overview_mon_h = h;
    }

    pub(crate) fn set_overview_mode(&mut self, active: bool, windows: Vec<(u32, f32, f32, f32, f32, bool, String)>) {
        if !active && self.overview_active && !self.overview_closing {
            // Begin exit animation — don't clear state yet
            self.overview_closing = true;
            self.overview_exit_progress = 1.0;
            self.needs_render = true;
            return;
        }
        self.clear_overview_snapshots();
        self.clear_overview_title_textures();
        self.overview_active = active;
        self.overview_closing = false;
        let n = windows.len();
        let face_w = self.screen_w as f32 * 0.8;
        let face_h = self.screen_h as f32 * 0.8;
        self.overview_windows = windows.into_iter().enumerate().map(|(i, (win, _x, _y, _w, _h, sel, title))| {
            OverviewEntry {
                x11_win: win,
                target_w: face_w,
                target_h: face_h,
                is_selected: sel,
                snapshot_texture: None,
                title,
                title_texture: None,
                face_index: i.min(5),
            }
        }).collect();
        self.overview_total_clients = n;
        self.overview_slide_offset = 0;
        self.overview_prism_target_angle = 0.0;
        self.overview_prism_current_angle = 0.0;
        self.overview_prism_last_tick = None;
        if active {
            self.refresh_overview_snapshots();
            self.create_overview_title_textures();
            self.overview_entry_progress = 0.0;
            self.overview_exit_progress = 1.0;
            self.overview_opacity = 0.0;
        } else {
            self.overview_entry_progress = 1.0;
            self.overview_exit_progress = 1.0;
            self.overview_opacity = 0.0;
        }
        self.needs_render = true;
    }

    pub(crate) fn set_overview_selection(&mut self, x11_win: u32) {
        let mut selected_face = 0usize;
        for entry in &mut self.overview_windows {
            let sel = entry.x11_win == x11_win;
            entry.is_selected = sel;
            if sel {
                selected_face = entry.face_index;
            }
        }
        // Rotate prism so selected face faces the camera.
        let new_target = -(selected_face as f32) * std::f32::consts::FRAC_PI_3;
        // Normalize angular difference to shortest path (within -PI..PI).
        let mut diff = new_target - self.overview_prism_target_angle;
        while diff > std::f32::consts::PI { diff -= 2.0 * std::f32::consts::PI; }
        while diff < -std::f32::consts::PI { diff += 2.0 * std::f32::consts::PI; }
        self.overview_prism_target_angle += diff;
        self.needs_render = true;
    }

    pub(crate) fn notify_window_move_start(&mut self, x11_win: u32) {
        if !self.wobbly_windows { return; }
        let grid_n = (self.wobbly_grid_size as usize + 1).min(17);
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            // Determine anchor node: closest grid node to mouse position
            let rel_x = ((self.mouse_x - wt.x as f32).max(0.0)).min(wt.w as f32);
            let rel_y = ((self.mouse_y - wt.y as f32).max(0.0)).min(wt.h as f32);
            let anchor_col = ((rel_x / wt.w as f32) * (grid_n - 1) as f32).round() as usize;
            let anchor_row = ((rel_y / wt.h as f32) * (grid_n - 1) as f32).round() as usize;

            let count = grid_n * grid_n;
            wt.wobbly = Some(WobblyState {
                grid_n,
                offsets: vec![[0.0; 2]; count],
                velocities: vec![[0.0; 2]; count],
                dragging: true,
                anchor_row: anchor_row.min(grid_n - 1),
                anchor_col: anchor_col.min(grid_n - 1),
                last_tick: std::time::Instant::now(),
            });
        } else {
            log::warn!("[wobbly] move_start: window 0x{:x} not tracked by compositor", x11_win);
        }
    }

    pub(crate) fn notify_window_move_delta(&mut self, x11_win: u32, dx: f32, dy: f32) {
        // Phase 3.1: Record position for motion trail
        if self.motion_trail_enabled {
            if let Some(wt) = self.windows.get(&x11_win) {
                let cur_x = wt.x;
                let cur_y = wt.y;
                self.update_motion_trail(x11_win, cur_x, cur_y);
            }
        }

        if self.wobbly_windows {
            if let Some(wt) = self.windows.get_mut(&x11_win) {
                if let Some(ref mut w) = wt.wobbly {
                    // The window has already moved to the new position.
                    // Anchor node stays at [0,0] (moves with the window).
                    // All OTHER nodes get a reverse impulse to simulate inertia.
                    let n = w.grid_n;
                    let ar = w.anchor_row;
                    let ac = w.anchor_col;
                    for row in 0..n {
                        for col in 0..n {
                            if row == ar && col == ac { continue; }
                            let idx = row * n + col;
                            w.offsets[idx][0] -= dx;
                            w.offsets[idx][1] -= dy;
                        }
                    }
                    // Ensure anchor stays pinned at zero
                    let ai = ar * n + ac;
                    w.offsets[ai] = [0.0, 0.0];
                    w.velocities[ai] = [0.0, 0.0];
                }
            }
        }

        // During interactive move/resize, request full-frame redraw when backdrop
        // blur is active so translucent windows see real-time updated background.
        let blur_active = self.blur_enabled
            && self.scene_fbo.is_some()
            && !self.blur_fbos.is_empty()
            && {
                let cfg = crate::config::CONFIG.load();
                let status_bar_name = cfg.status_bar_name();
                self.windows.values().any(|wt| self.needs_backdrop_blur(wt, status_bar_name))
            };
        if blur_active {
            self.damage_tracker.mark_all_dirty();
            self.dirty_region_tracker.mark_all_dirty();  // P5C: Sync rect tracker
        }
        self.needs_render = true;
    }

    pub(crate) fn notify_window_move_end(&mut self, x11_win: u32) {
        // Phase 3.1: Clear motion trail
        self.clear_motion_trail(x11_win);

        // Release anchor — let all nodes spring back via tick_wobbly
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            if let Some(ref mut w) = wt.wobbly {
                w.dragging = false;
            }
        }
    }

    #[allow(dead_code)]
    pub(crate) fn tracked_window_count(&self) -> usize {
        self.windows.len()
    }

    /// Set dock/taskbar position for genie minimize target.
    pub(crate) fn set_dock_position(&mut self, x: f32, y: f32) {
        self.dock_position = (x, y);
    }

    #[allow(dead_code)]
    pub(crate) fn has_window(&self, x11_win: u32) -> bool {
        self.windows.contains_key(&x11_win)
    }

    // =====================================================================
    // Phase 6: Accessibility & Utility
    // =====================================================================

    pub(crate) fn set_colorblind_mode(&mut self, mode: &str) {
        let m = match mode {
            "deuteranopia" => 1,
            "protanopia" => 2,
            "tritanopia" => 3,
            _ => 0,
        };
        if self.colorblind_mode != m {
            self.colorblind_mode = m;
            self.ensure_postprocess_fbo();
            self.needs_render = true;
        }
    }

    pub(crate) fn zoom_to_fit(&mut self, window: Option<u32>) {
        if let Some(win) = window {
            if self.zoom_to_fit_window == Some(win) {
                self.zoom_to_fit_window = None;
                self.zoom_to_fit_target = 1.0;
            } else {
                self.zoom_to_fit_window = Some(win);
                if let Some(wt) = self.windows.get(&win) {
                    if wt.w > 0 && wt.h > 0 {
                        let sx = self.screen_w as f32 / wt.w as f32;
                        let sy = self.screen_h as f32 / wt.h as f32;
                        self.zoom_to_fit_target = sx.min(sy);
                    }
                }
            }
            self.needs_render = true;
        } else {
            self.zoom_to_fit_window = None;
            self.zoom_to_fit_target = 1.0;
            self.needs_render = true;
        }
    }

    // =====================================================================
    // Phase 7: Diagnostics
    // =====================================================================

    pub(crate) fn reload_shader_from_file(&mut self, name: &str, path: &std::path::Path) -> Result<(), String> {
        let file_content = std::fs::read_to_string(path)
            .map_err(|e| format!("read shader file: {e}"))?;

        let (vs_src, fs_src) = match name {
            "window" => (shaders::VERTEX_SHADER, file_content.as_str()),
            "shadow" => (shaders::VERTEX_SHADER, file_content.as_str()),
            "border" => (shaders::VERTEX_SHADER, file_content.as_str()),
            "blur_down" => (shaders::BLUR_DOWN_VERTEX, file_content.as_str()),
            "blur_up" => (shaders::BLUR_DOWN_VERTEX, file_content.as_str()),
            "box_blur" => (shaders::BLUR_DOWN_VERTEX, file_content.as_str()),
            "postprocess" => (shaders::BLUR_DOWN_VERTEX, file_content.as_str()),
            "hud" => (shaders::VERTEX_SHADER, file_content.as_str()),
            "hud_text" => (shaders::VERTEX_SHADER, file_content.as_str()),
            "transition" => (shaders::BLUR_DOWN_VERTEX, file_content.as_str()),
            "cube" => (shaders::CUBE_VERTEX_SHADER, file_content.as_str()),
            "portal" => (shaders::BLUR_DOWN_VERTEX, file_content.as_str()),
            "edge_glow" => (shaders::VERTEX_SHADER, file_content.as_str()),
            "tilt" => (shaders::TILT_VERTEX_SHADER, file_content.as_str()),
            "wobbly" => (shaders::WOBBLY_VERTEX_SHADER, file_content.as_str()),
            "particle" => (shaders::PARTICLE_VERTEX_SHADER, file_content.as_str()),
            "genie" => (shaders::GENIE_VERTEX_SHADER, file_content.as_str()),
            "overview_bg" => (shaders::VERTEX_SHADER, file_content.as_str()),
            _ if name.ends_with("_vs") => {
                log::warn!("compositor: shader reload requires both vertex and fragment shaders to be specified");
                return Err(format!("shader {} needs corresponding fragment shader", name));
            }
            _ => return Err(format!("unknown shader: {name}")),
        };

        match unsafe { Self::create_program(&self.gl, vs_src, fs_src) } {
            Ok(new_program) => {
                unsafe {
                    match name {
                        "window" => { self.gl.delete_program(self.program); self.program = new_program; }
                        "shadow" => { self.gl.delete_program(self.shadow_program); self.shadow_program = new_program; }
                        "border" => { self.gl.delete_program(self.border_program); self.border_program = new_program; }
                        "blur_down" => { self.gl.delete_program(self.blur_down_program); self.blur_down_program = new_program; }
                        "blur_up" => { self.gl.delete_program(self.blur_up_program); self.blur_up_program = new_program; }
                        "box_blur" => { /* no separate program, used in blur_optimize */ }
                        "postprocess" => { self.gl.delete_program(self.postprocess_program); self.postprocess_program = new_program; }
                        "hud" => { self.gl.delete_program(self.hud_program); self.hud_program = new_program; }
                        "hud_text" => { self.gl.delete_program(self.hud_text_program); self.hud_text_program = new_program; }
                        "transition" => { self.gl.delete_program(self.transition_program); self.transition_program = new_program; }
                        "cube" => { self.gl.delete_program(self.cube_program); self.cube_program = new_program; }
                        "portal" => { self.gl.delete_program(self.portal_program); self.portal_program = new_program; }
                        "edge_glow" => { self.gl.delete_program(self.edge_glow_program); self.edge_glow_program = new_program; }
                        "tilt" => { self.gl.delete_program(self.tilt_program); self.tilt_program = new_program; }
                        "wobbly" => { self.gl.delete_program(self.wobbly_program); self.wobbly_program = new_program; }
                        "particle" => { self.gl.delete_program(self.particle_program); self.particle_program = new_program; }
                        "genie" => { self.gl.delete_program(self.genie_program); self.genie_program = new_program; }
                        "overview_bg" => { self.gl.delete_program(self.overview_bg_program); self.overview_bg_program = new_program; }
                        _ => { self.gl.delete_program(new_program); }
                    }
                }
                self.needs_render = true;
                log::info!("compositor: shader reload succeeded for {name}");
                Ok(())
            }
            Err(e) => {
                log::warn!("compositor: shader reload failed for {name}: {e}");
                Err(e)
            }
        }
    }

    pub(crate) fn enable_shader_hot_reload(&mut self, shader_dir: &str) {
        if shader_dir.is_empty() {
            log::warn!("compositor: shader_dir is empty, cannot enable hot-reload");
            return;
        }
        let dir = std::path::PathBuf::from(shader_dir);
        if !dir.exists() {
            if let Err(e) = std::fs::create_dir_all(&dir) {
                log::warn!("compositor: failed to create shader_dir '{shader_dir}': {e}");
                return;
            }
        }
        self.shader_hot_reload_enabled = true;
        self.shader_dir = shader_dir.to_string();
        self.shader_file_mtimes.clear();
        log::info!("compositor: shader hot-reload enabled, watching '{shader_dir}'");
    }

    pub(crate) fn poll_shader_hot_reload(&mut self) {
        if !self.shader_hot_reload_enabled || self.shader_dir.is_empty() {
            return;
        }

        const SHADER_NAMES: &[&str] = &[
            "window", "shadow", "border", "blur_down", "blur_up", "box_blur",
            "postprocess", "hud", "hud_text", "transition", "cube", "portal",
            "edge_glow", "tilt", "wobbly", "particle", "genie", "overview_bg",
        ];

        let dir = std::path::PathBuf::from(&self.shader_dir);
        let mut to_reload: Vec<(String, std::path::PathBuf)> = Vec::new();

        for &name in SHADER_NAMES {
            let path = dir.join(format!("{name}.frag"));
            if !path.exists() { continue; }
            let mtime = match std::fs::metadata(&path).and_then(|m| m.modified()) {
                Ok(t) => t,
                Err(_) => continue,
            };
            let changed = match self.shader_file_mtimes.get(name) {
                Some(&prev) => mtime != prev,
                None => true,
            };
            if changed {
                self.shader_file_mtimes.insert(name.to_string(), mtime);
                to_reload.push((name.to_string(), path));
            }
        }

        for (name, path) in to_reload {
            match self.reload_shader_from_file(&name, &path) {
                Ok(()) => log::info!("compositor: hot-reloaded shader '{name}'"),
                Err(e) => log::warn!("compositor: hot-reload failed for '{name}': {e}"),
            }
        }
    }

    pub(crate) fn start_recording(&mut self, output_path: &str) {
        if self.recording_active { return; }
        let w = self.screen_w;
        let h = self.screen_h;
        let fps = self.recording_fps;

        let stderr_file = std::fs::File::create("/tmp/jwm-ffmpeg.log")
            .unwrap_or_else(|_| std::fs::File::create("/dev/null").unwrap());

        // Select encoder: respect config or auto-probe (NVENC > VAAPI > SW).
        let probe = |args: &[&str]| -> bool {
            std::process::Command::new("ffmpeg")
                .args(args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };

        enum Encoder { Nvenc, Vaapi, Sw }
        let encoder = match self.recording_encoder.as_str() {
            "nvenc" => Encoder::Nvenc,
            "vaapi" => Encoder::Vaapi,
            "software" => Encoder::Sw,
            _ => {
                // auto: probe NVENC > VAAPI > SW
                if probe(&["-f", "lavfi", "-i", "nullsrc=s=64x64", "-frames:v", "1", "-c:v", "h264_nvenc", "-f", "null", "-"]) {
                    Encoder::Nvenc
                } else if std::path::Path::new("/dev/dri/renderD128").exists()
                    && probe(&["-vaapi_device", "/dev/dri/renderD128", "-f", "lavfi", "-i", "nullsrc=s=64x64", "-frames:v", "1", "-f", "null", "-"])
                {
                    Encoder::Vaapi
                } else {
                    Encoder::Sw
                }
            }
        };

        let codec_name = match encoder { Encoder::Nvenc => "h264_nvenc", Encoder::Vaapi => "h264_vaapi", Encoder::Sw => "libopenh264" };
        let bitrate = &self.recording_bitrate;
        let quality_str = self.recording_quality.to_string();
        log::info!("compositor: recording encoder={codec_name}, size={w}x{h}, fps={fps}, bitrate={bitrate}, qp={quality_str}, output={output_path}");

        let size_str = format!("{w}x{h}");
        let fps_str = fps.to_string();
        let mut args: Vec<&str> = Vec::new();

        if matches!(encoder, Encoder::Vaapi) {
            args.extend_from_slice(&["-vaapi_device", "/dev/dri/renderD128"]);
        }
        // Input: use wall clock timestamps so video duration matches real time.
        // The nominal `-r` is moved to the output side; ffmpeg duplicates/drops
        // frames automatically to produce a constant-frame-rate file.
        args.extend_from_slice(&[
            "-use_wallclock_as_timestamps", "1",
            "-f", "rawvideo",
            "-pix_fmt", "rgba",
            "-s", &size_str,
            "-i", "pipe:0",
        ]);
        match encoder {
            Encoder::Nvenc => args.extend_from_slice(&["-vf", "vflip"]),
            Encoder::Vaapi => args.extend_from_slice(&["-vf", "vflip,format=nv12,hwupload"]),
            Encoder::Sw => args.extend_from_slice(&["-vf", "vflip"]),
        }
        args.push("-c:v"); args.push(codec_name);
        match encoder {
            Encoder::Vaapi => args.extend_from_slice(&["-rc_mode", "CQP", "-qp", &quality_str]),
            _ => args.extend_from_slice(&["-b:v", bitrate]),
        }
        args.extend_from_slice(&["-r", &fps_str, "-y", output_path]);

        let child = match std::process::Command::new("ffmpeg")
            .args(&args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(stderr_file)
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                log::warn!("compositor: failed to start ffmpeg: {e}");
                return;
            }
        };

        unsafe {
            for pbo in &mut self.recording_pbo {
                if let Ok(buf) = self.gl.create_buffer() {
                    self.gl.bind_buffer(glow::PIXEL_PACK_BUFFER, Some(buf));
                    self.gl.buffer_data_size(
                        glow::PIXEL_PACK_BUFFER,
                        (w * h * 4) as i32,
                        glow::STREAM_READ,
                    );
                    self.gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
                    *pbo = Some(buf);
                }
            }
        }

        self.recording_process = Some(child);
        self.recording_active = true;
        self.recording_last_frame = None;
        log::info!("compositor: recording started to {output_path}");
    }

    pub(crate) fn stop_recording(&mut self) {
        if !self.recording_active { return; }
        self.recording_active = false;

        unsafe {
            for pbo in &mut self.recording_pbo {
                if let Some(buf) = pbo.take() {
                    self.gl.delete_buffer(buf);
                }
            }
        }

        if let Some(mut child) = self.recording_process.take() {
            drop(child.stdin.take());
            let _ = child.wait();
        }
        log::info!("compositor: recording stopped");
    }

    pub(super) fn capture_recording_frame(&mut self) {
        if !self.recording_active { return; }

        let now = std::time::Instant::now();
        let min_interval = std::time::Duration::from_secs_f32(1.0 / self.recording_fps as f32);
        if let Some(last) = self.recording_last_frame {
            if now.duration_since(last) < min_interval {
                return;
            }
        }
        self.recording_last_frame = Some(now);

        let w = self.screen_w;
        let h = self.screen_h;
        let buf_size = (w * h * 4) as usize;

        // Simple single-buffer approach: read_pixels into PBO, map, write to ffmpeg.
        if let Some(pbo) = self.recording_pbo[0] {
            unsafe {
                self.gl.bind_buffer(glow::PIXEL_PACK_BUFFER, Some(pbo));
                self.gl.read_pixels(
                    0, 0, w as i32, h as i32,
                    glow::RGBA, glow::UNSIGNED_BYTE,
                    glow::PixelPackData::BufferOffset(0),
                );

                let ptr = self.gl.map_buffer_range(
                    glow::PIXEL_PACK_BUFFER,
                    0,
                    buf_size as i32,
                    glow::MAP_READ_BIT,
                );
                if !ptr.is_null() {
                    let pixels = std::slice::from_raw_parts(ptr as *const u8, buf_size);
                    if let Some(ref mut child) = self.recording_process {
                        if let Some(ref mut stdin) = child.stdin {
                            use std::io::Write;
                            if let Err(e) = stdin.write_all(pixels) {
                                log::warn!("compositor: recording write failed: {e}, stopping");
                                self.gl.unmap_buffer(glow::PIXEL_PACK_BUFFER);
                                self.gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
                                self.recording_active = false;
                                return;
                            }
                        }
                    }
                    self.gl.unmap_buffer(glow::PIXEL_PACK_BUFFER);
                } else {
                    log::warn!("compositor: recording PBO map returned null");
                }
                self.gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
            }
        }
    }

    /// P6A: Process deferred X11 operations
    /// Called at start of render_frame to batch operations
    pub(super) fn process_deferred_x11_ops(&mut self) {
        while let Some(op) = self.deferred_ops_queue.pop() {
            match op.op_type.as_str() {
                "name_pixmap" => {
                    // Deferred NameWindowPixmap operation
                    // This was originally in event handler, now batched in render thread
                    log::debug!("compositor: processing deferred name_pixmap for window 0x{:x}", op.window_id);
                    // Implementation would go here (currently placeholder)
                }
                "destroy_pixmap" => {
                    // Deferred pixmap destruction
                    log::debug!("compositor: processing deferred destroy_pixmap for window 0x{:x}", op.window_id);
                }
                _ => {
                    log::warn!("compositor: unknown deferred op type: {}", op.op_type);
                }
            }
        }
    }

}
