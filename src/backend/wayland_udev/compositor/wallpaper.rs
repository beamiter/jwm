// Wallpaper loading, texture upload, and rendering for the Wayland udev compositor.
// Uses raw GLES2 FFI (smithay::backend::renderer::gles::ffi) instead of glow.

#[allow(unused_imports)]
use super::*;
use smithay::backend::renderer::gles::ffi;
use std::sync::mpsc;
use std::time::Instant;

impl WaylandCompositor {
    // =========================================================================
    // 1. Asynchronous wallpaper loading (background thread)
    // =========================================================================

    /// Decode a wallpaper image on a background thread.
    /// Returns a receiver that will deliver the decoded RGBA data once ready.
    /// If `max_w`/`max_h` are non-zero and the image exceeds those dimensions,
    /// it is downscaled using Lanczos3 to fit within the bounds while
    /// preserving the aspect ratio.
    pub(crate) fn load_wallpaper_async(
        path: &str,
        max_w: u32,
        max_h: u32,
        mode: WallpaperMode,
    ) -> mpsc::Receiver<WallpaperImageData> {
        let (tx, rx) = mpsc::channel();
        let path = path.to_string();
        std::thread::spawn(move || {
            let img = match image::open(&path) {
                Ok(img) => img,
                Err(e) => {
                    log::warn!(
                        "[wallpaper] failed to load '{}': {}",
                        path, e
                    );
                    return;
                }
            };

            let img = if max_w > 0
                && max_h > 0
                && (img.width() > max_w || img.height() > max_h)
            {
                log::info!(
                    "[wallpaper] downscaling '{}' from {}x{} to fit {}x{}",
                    path,
                    img.width(),
                    img.height(),
                    max_w,
                    max_h,
                );
                img.resize(max_w, max_h, image::imageops::FilterType::Lanczos3)
            } else {
                img
            };

            let rgba = img.to_rgba8();
            let (w, h) = (rgba.width(), rgba.height());
            log::info!("[wallpaper] decoded '{}' ({}x{})", path, w, h);

            let _ = tx.send(WallpaperImageData {
                rgba: rgba.into_raw(),
                width: w,
                height: h,
                mode,
            });
        });
        rx
    }

    // =========================================================================
    // 2. Upload wallpaper texture via GLES2 FFI
    // =========================================================================

    /// Create a GLES2 texture from decoded RGBA image data.
    /// Returns `(texture_id, width, height)` on success, or `None` on failure.
    /// Uses RGBA8 internal format with LINEAR filtering and CLAMP_TO_EDGE wrapping.
    pub(crate) unsafe fn upload_wallpaper_texture_gles(
        gl: &ffi::Gles2,
        data: &WallpaperImageData,
    ) -> Option<(u32, u32, u32)> {
        if data.rgba.is_empty() || data.width == 0 || data.height == 0 {
            log::warn!("[wallpaper] upload skipped: empty image data");
            return None;
        }

        unsafe {
            let mut tex: u32 = 0;
            gl.GenTextures(1, &mut tex);
            if tex == 0 {
                log::warn!("[wallpaper] GenTextures returned 0");
                return None;
            }

            gl.BindTexture(ffi::TEXTURE_2D, tex);

            gl.TexImage2D(
                ffi::TEXTURE_2D,
                0,
                ffi::RGBA8 as i32,
                data.width as i32,
                data.height as i32,
                0,
                ffi::RGBA,
                ffi::UNSIGNED_BYTE,
                data.rgba.as_ptr() as *const _,
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

            gl.BindTexture(ffi::TEXTURE_2D, 0);

            log::info!(
                "[wallpaper] uploaded texture id={} ({}x{})",
                tex,
                data.width,
                data.height
            );
            Some((tex, data.width, data.height))
        }
    }

    // =========================================================================
    // 3. Compute wallpaper draw rect (pure geometry)
    // =========================================================================

    /// Compute the draw rectangle (x, y, w, h) for a wallpaper within a target area.
    ///
    /// Modes:
    /// - `Fill`: scale to cover the entire area (max scale), crop centered.
    /// - `Fit`: scale to fit entirely within the area (min scale), letterboxed.
    /// - `Stretch`: fill the area exactly, ignoring aspect ratio.
    /// - `Center`: draw at 1:1 pixel size, centered in the area.
    pub(crate) fn compute_wallpaper_rect(
        mode: WallpaperMode,
        area: (f32, f32, f32, f32),
        img_w: u32,
        img_h: u32,
    ) -> (f32, f32, f32, f32) {
        let (ax, ay, aw, ah) = area;
        let iw = img_w as f32;
        let ih = img_h as f32;

        if iw <= 0.0 || ih <= 0.0 {
            return (ax, ay, aw, ah);
        }

        match mode {
            WallpaperMode::Stretch => (ax, ay, aw, ah),
            WallpaperMode::Fill => {
                // Scale up so image covers the entire area; excess is cropped.
                let scale = (aw / iw).max(ah / ih);
                let dw = iw * scale;
                let dh = ih * scale;
                let dx = ax + (aw - dw) * 0.5;
                let dy = ay + (ah - dh) * 0.5;
                (dx, dy, dw, dh)
            }
            WallpaperMode::Fit => {
                // Scale down so entire image fits within area; letterboxed.
                let scale = (aw / iw).min(ah / ih);
                let dw = iw * scale;
                let dh = ih * scale;
                let dx = ax + (aw - dw) * 0.5;
                let dy = ay + (ah - dh) * 0.5;
                (dx, dy, dw, dh)
            }
            WallpaperMode::Center => {
                // Draw at native resolution, centered.
                let dx = ax + (aw - iw) * 0.5;
                let dy = ay + (ah - ih) * 0.5;
                (dx, dy, iw, ih)
            }
        }
    }

    // =========================================================================
    // 4. Poll pending wallpaper loads
    // =========================================================================

    /// Check whether any background wallpaper decode has completed.
    /// On success, uploads the texture to the GPU, optionally setting up
    /// crossfade state if `wallpaper_crossfade` is enabled.
    pub(crate) unsafe fn poll_pending_wallpapers(&mut self, gl: &ffi::Gles2) {
        // --- Global wallpaper ---
        if let Some(ref rx) = self.pending_wallpaper {
            match rx.try_recv() {
                Ok(data) => {
                    // Crossfade: save old texture before replacing
                    if self.wallpaper_crossfade {
                        if let Some(old) = self.old_wallpaper_texture.take() {
                            unsafe {
                                gl.DeleteTextures(1, &old);
                            }
                        }
                        self.old_wallpaper_texture = self.wallpaper_texture.take();
                        self.wallpaper_transition_start = Some(Instant::now());
                    } else {
                        // No crossfade: just delete old texture
                        if let Some(old) = self.wallpaper_texture.take() {
                            unsafe {
                                gl.DeleteTextures(1, &old);
                            }
                        }
                    }

                    // Upload new texture
                    if let Some((tex, w, h)) =
                        unsafe { Self::upload_wallpaper_texture_gles(gl, &data) }
                    {
                        self.wallpaper_texture = Some(tex);
                        self.wallpaper_img_w = w;
                        self.wallpaper_img_h = h;
                        self.wallpaper_mode = data.mode;
                    }

                    self.pending_wallpaper = None;
                    self.needs_render = true;
                }
                Err(mpsc::TryRecvError::Empty) => {
                    // Still loading, keep waiting.
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Thread finished without sending (error logged in thread).
                    self.pending_wallpaper = None;
                }
            }
        }

        // --- Per-monitor wallpapers ---
        let mut completed: Vec<usize> = Vec::new();
        for (i, (mon_idx, rx)) in self.pending_monitor_wallpapers.iter().enumerate() {
            match rx.try_recv() {
                Ok(data) => {
                    if let Some(mw) = self.monitor_wallpapers.get_mut(*mon_idx) {
                        // Delete old per-monitor texture if any
                        if let Some(old) = mw.texture.take() {
                            unsafe {
                                gl.DeleteTextures(1, &old);
                            }
                        }

                        // Upload new texture
                        if let Some((tex, w, h)) =
                            unsafe { Self::upload_wallpaper_texture_gles(gl, &data) }
                        {
                            mw.texture = Some(tex);
                            mw.img_w = w;
                            mw.img_h = h;
                            mw.mode = data.mode;
                        }
                    }
                    completed.push(i);
                    self.needs_render = true;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    completed.push(i);
                }
            }
        }

        // Remove completed receivers (iterate in reverse to preserve indices)
        for i in completed.into_iter().rev() {
            self.pending_monitor_wallpapers.remove(i);
        }
    }

    // =========================================================================
    // 5. Render wallpaper
    // =========================================================================

    /// Render the wallpaper for each monitor (or the global wallpaper) into the
    /// currently bound framebuffer. Uses the window shader program with
    /// opacity=1.0, no corner radius, no dim.
    ///
    /// If crossfade is active (transition_start is set), draws the old wallpaper
    /// first at decreasing alpha, then the new wallpaper at increasing alpha.
    pub(crate) unsafe fn render_wallpaper(&self, gl: &ffi::Gles2, projection: &[f32; 16]) {
        // Determine crossfade progress (0.0 = just started, 1.0 = complete)
        let crossfade_alpha = if let Some(start) = self.wallpaper_transition_start {
            let elapsed_ms = start.elapsed().as_millis() as u64;
            let duration = self.wallpaper_crossfade_duration_ms.max(1);
            let t = (elapsed_ms as f32) / (duration as f32);
            t.clamp(0.0, 1.0)
        } else {
            1.0
        };

        unsafe {
            gl.UseProgram(self.program);
            gl.BindVertexArray(self.quad_vao);
            gl.ActiveTexture(ffi::TEXTURE0);
            gl.Uniform1i(self.win_uniforms.texture, 0);

            // Set projection
            gl.UniformMatrix4fv(
                self.win_uniforms.projection,
                1,
                ffi::FALSE as u8,
                projection.as_ptr(),
            );

            // No corner radius, no dim, no ripple for wallpaper
            gl.Uniform1f(self.win_uniforms.radius, 0.0);
            gl.Uniform1f(self.win_uniforms.dim, 0.0);
            gl.Uniform1f(self.win_uniforms.ripple_progress, 0.0);
            gl.Uniform1f(self.win_uniforms.ripple_amplitude, 0.0);
            // Full UV rect (use entire texture)
            gl.Uniform4f(self.win_uniforms.uv_rect, 0.0, 0.0, 1.0, 1.0);

            // Iterate over monitors
            for mw in &self.monitor_wallpapers {
                let area = (
                    mw.mon_x as f32,
                    mw.mon_y as f32,
                    mw.mon_w as f32,
                    mw.mon_h as f32,
                );

                // Determine which texture and dimensions to use for this monitor
                let (tex, img_w, img_h, mode) = if let Some(t) = mw.texture {
                    (t, mw.img_w, mw.img_h, mw.mode)
                } else if let Some(t) = self.wallpaper_texture {
                    (t, self.wallpaper_img_w, self.wallpaper_img_h, self.wallpaper_mode)
                } else {
                    // No wallpaper available for this monitor
                    continue;
                };

                if img_w == 0 || img_h == 0 {
                    continue;
                }

                let (rx, ry, rw, rh) = Self::compute_wallpaper_rect(mode, area, img_w, img_h);

                // Set size uniform for the shader
                gl.Uniform2f(self.win_uniforms.size, rw, rh);

                // --- Draw old wallpaper (crossfade out) ---
                if crossfade_alpha < 1.0 {
                    if let Some(old_tex) = self.old_wallpaper_texture {
                        // Compute rect for old wallpaper using same area
                        let (orx, ory, orw, orh) = Self::compute_wallpaper_rect(
                            self.wallpaper_mode,
                            area,
                            self.wallpaper_img_w,
                            self.wallpaper_img_h,
                        );
                        gl.Uniform4f(self.win_uniforms.rect, orx, ory, orw, orh);
                        gl.Uniform2f(self.win_uniforms.size, orw, orh);
                        gl.Uniform1f(self.win_uniforms.opacity, 1.0 - crossfade_alpha);
                        gl.BindTexture(ffi::TEXTURE_2D, old_tex);
                        gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
                    }
                }

                // --- Draw current wallpaper ---
                gl.Uniform4f(self.win_uniforms.rect, rx, ry, rw, rh);
                gl.Uniform2f(self.win_uniforms.size, rw, rh);
                gl.Uniform1f(
                    self.win_uniforms.opacity,
                    if crossfade_alpha < 1.0 { crossfade_alpha } else { 1.0 },
                );
                gl.BindTexture(ffi::TEXTURE_2D, tex);
                gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
            }

            // If no per-monitor wallpapers were drawn, draw the global wallpaper
            // across the entire screen as a fallback.
            if self.monitor_wallpapers.is_empty() {
                if let Some(tex) = self.wallpaper_texture {
                    if self.wallpaper_img_w > 0 && self.wallpaper_img_h > 0 {
                        let area = (0.0, 0.0, self.screen_w as f32, self.screen_h as f32);
                        let (rx, ry, rw, rh) = Self::compute_wallpaper_rect(
                            self.wallpaper_mode,
                            area,
                            self.wallpaper_img_w,
                            self.wallpaper_img_h,
                        );

                        gl.Uniform2f(self.win_uniforms.size, rw, rh);

                        // Old wallpaper crossfade
                        if crossfade_alpha < 1.0 {
                            if let Some(old_tex) = self.old_wallpaper_texture {
                                let (orx, ory, orw, orh) = Self::compute_wallpaper_rect(
                                    self.wallpaper_mode,
                                    area,
                                    self.wallpaper_img_w,
                                    self.wallpaper_img_h,
                                );
                                gl.Uniform4f(self.win_uniforms.rect, orx, ory, orw, orh);
                                gl.Uniform2f(self.win_uniforms.size, orw, orh);
                                gl.Uniform1f(self.win_uniforms.opacity, 1.0 - crossfade_alpha);
                                gl.BindTexture(ffi::TEXTURE_2D, old_tex);
                                gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
                            }
                        }

                        gl.Uniform4f(self.win_uniforms.rect, rx, ry, rw, rh);
                        gl.Uniform2f(self.win_uniforms.size, rw, rh);
                        gl.Uniform1f(
                            self.win_uniforms.opacity,
                            if crossfade_alpha < 1.0 { crossfade_alpha } else { 1.0 },
                        );
                        gl.BindTexture(ffi::TEXTURE_2D, tex);
                        gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
                    }
                }
            }

            gl.BindTexture(ffi::TEXTURE_2D, 0);
            gl.BindVertexArray(0);
        }
    }

    // =========================================================================
    // 6. Set wallpaper (user-facing entry point)
    // =========================================================================

    /// Initiate a wallpaper change. Parses the mode string, stores the path,
    /// and spawns a background thread to decode the image. The texture will be
    /// uploaded on the next frame via `poll_pending_wallpapers`.
    pub(crate) fn set_wallpaper(&mut self, path: &str, mode: &str) {
        let wp_mode = Self::parse_wallpaper_mode(mode);
        self.wallpaper_path = path.to_string();
        self.wallpaper_mode = wp_mode;

        log::info!(
            "[wallpaper] set_wallpaper path='{}' mode={:?}",
            path,
            wp_mode
        );

        // Determine max decode size from screen dimensions
        let max_w = self.screen_w;
        let max_h = self.screen_h;

        let rx = Self::load_wallpaper_async(path, max_w, max_h, wp_mode);
        self.pending_wallpaper = Some(rx);
        self.needs_render = true;
    }

    // =========================================================================
    // 7. Parse wallpaper mode string
    // =========================================================================

    /// Parse a mode string into a `WallpaperMode` enum value.
    /// Recognized values: "fill", "fit", "stretch", "center".
    /// Defaults to `Fill` for unrecognized strings.
    pub(crate) fn parse_wallpaper_mode(s: &str) -> WallpaperMode {
        match s.to_ascii_lowercase().as_str() {
            "fit" => WallpaperMode::Fit,
            "stretch" => WallpaperMode::Stretch,
            "center" => WallpaperMode::Center,
            _ => WallpaperMode::Fill,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_wallpaper_mode_variants() {
        assert_eq!(
            WaylandCompositor::parse_wallpaper_mode("fill"),
            WallpaperMode::Fill
        );
        assert_eq!(
            WaylandCompositor::parse_wallpaper_mode("fit"),
            WallpaperMode::Fit
        );
        assert_eq!(
            WaylandCompositor::parse_wallpaper_mode("stretch"),
            WallpaperMode::Stretch
        );
        assert_eq!(
            WaylandCompositor::parse_wallpaper_mode("center"),
            WallpaperMode::Center
        );
    }

    #[test]
    fn test_parse_wallpaper_mode_case_insensitive() {
        assert_eq!(
            WaylandCompositor::parse_wallpaper_mode("Fill"),
            WallpaperMode::Fill
        );
        assert_eq!(
            WaylandCompositor::parse_wallpaper_mode("FIT"),
            WallpaperMode::Fit
        );
        assert_eq!(
            WaylandCompositor::parse_wallpaper_mode("STRETCH"),
            WallpaperMode::Stretch
        );
        assert_eq!(
            WaylandCompositor::parse_wallpaper_mode("Center"),
            WallpaperMode::Center
        );
    }

    #[test]
    fn test_parse_wallpaper_mode_unknown_defaults_fill() {
        assert_eq!(
            WaylandCompositor::parse_wallpaper_mode(""),
            WallpaperMode::Fill
        );
        assert_eq!(
            WaylandCompositor::parse_wallpaper_mode("unknown"),
            WallpaperMode::Fill
        );
        assert_eq!(
            WaylandCompositor::parse_wallpaper_mode("tile"),
            WallpaperMode::Fill
        );
    }

    #[test]
    fn test_compute_rect_stretch() {
        let area = (0.0, 0.0, 1920.0, 1080.0);
        let (x, y, w, h) =
            WaylandCompositor::compute_wallpaper_rect(WallpaperMode::Stretch, area, 800, 600);
        assert!((x - 0.0).abs() < 0.01);
        assert!((y - 0.0).abs() < 0.01);
        assert!((w - 1920.0).abs() < 0.01);
        assert!((h - 1080.0).abs() < 0.01);
    }

    #[test]
    fn test_compute_rect_fill_covers_area() {
        let area = (0.0, 0.0, 1920.0, 1080.0);
        let (_, _, w, h) =
            WaylandCompositor::compute_wallpaper_rect(WallpaperMode::Fill, area, 3840, 1080);
        // Fill must cover the area entirely
        assert!(w >= 1920.0 - 0.01);
        assert!(h >= 1080.0 - 0.01);
    }

    #[test]
    fn test_compute_rect_fill_centered() {
        let area = (0.0, 0.0, 1920.0, 1080.0);
        // Uniform aspect ratio image (half size) -> scale 2x -> exact fit
        let (x, y, w, h) =
            WaylandCompositor::compute_wallpaper_rect(WallpaperMode::Fill, area, 960, 540);
        assert!((w - 1920.0).abs() < 0.01);
        assert!((h - 1080.0).abs() < 0.01);
        assert!((x - 0.0).abs() < 0.01);
        assert!((y - 0.0).abs() < 0.01);
    }

    #[test]
    fn test_compute_rect_fit_letterboxed() {
        let area = (0.0, 0.0, 1920.0, 1080.0);
        // Wide image 1920x400 -> scale = min(1.0, 2.7) = 1.0
        // dw = 1920, dh = 400
        let (_, y, w, h) =
            WaylandCompositor::compute_wallpaper_rect(WallpaperMode::Fit, area, 1920, 400);
        assert!((w - 1920.0).abs() < 0.01);
        assert!((h - 400.0).abs() < 0.01);
        // Centered vertically: (1080-400)/2 = 340
        assert!((y - 340.0).abs() < 0.01);
    }

    #[test]
    fn test_compute_rect_center_native_size() {
        let area = (0.0, 0.0, 1920.0, 1080.0);
        let (x, y, w, h) =
            WaylandCompositor::compute_wallpaper_rect(WallpaperMode::Center, area, 800, 600);
        assert!((w - 800.0).abs() < 0.01);
        assert!((h - 600.0).abs() < 0.01);
        // Centered: (1920-800)/2=560, (1080-600)/2=240
        assert!((x - 560.0).abs() < 0.01);
        assert!((y - 240.0).abs() < 0.01);
    }

    #[test]
    fn test_compute_rect_center_large_image_overflows() {
        let area = (0.0, 0.0, 1920.0, 1080.0);
        let (x, y, w, h) =
            WaylandCompositor::compute_wallpaper_rect(WallpaperMode::Center, area, 2560, 1440);
        assert!((w - 2560.0).abs() < 0.01);
        assert!((h - 1440.0).abs() < 0.01);
        // Negative offsets (extends beyond area)
        assert!((x - (-320.0)).abs() < 0.01);
        assert!((y - (-180.0)).abs() < 0.01);
    }

    #[test]
    fn test_compute_rect_zero_image_returns_area() {
        let area = (10.0, 20.0, 400.0, 300.0);
        let (x, y, w, h) =
            WaylandCompositor::compute_wallpaper_rect(WallpaperMode::Fill, area, 0, 0);
        assert!((x - 10.0).abs() < 0.01);
        assert!((y - 20.0).abs() < 0.01);
        assert!((w - 400.0).abs() < 0.01);
        assert!((h - 300.0).abs() < 0.01);
    }

    #[test]
    fn test_compute_rect_non_origin_area() {
        // Second monitor at (1920, 0)
        let area = (1920.0, 0.0, 1920.0, 1080.0);
        let (x, y, w, h) =
            WaylandCompositor::compute_wallpaper_rect(WallpaperMode::Stretch, area, 800, 600);
        assert!((x - 1920.0).abs() < 0.01);
        assert!((y - 0.0).abs() < 0.01);
        assert!((w - 1920.0).abs() < 0.01);
        assert!((h - 1080.0).abs() < 0.01);
    }
}
