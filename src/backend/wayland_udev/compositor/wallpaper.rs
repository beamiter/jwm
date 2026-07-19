// Wallpaper loading, texture upload, and rendering for the Wayland udev compositor.
// Uses raw GLES2 FFI (smithay::backend::renderer::gles::ffi) instead of glow.

#[allow(unused_imports)]
use super::*;
use crate::backend::compositor_common::wallpaper::{compute_wallpaper_rect, parse_wallpaper_mode};
use smithay::backend::renderer::gles::ffi;
use std::sync::mpsc;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Instant;

type GlScissor = [i32; 4];

/// Intersect two OpenGL scissor rectangles (`x`, `y`, `width`, `height`).
/// Calculations use i64 so malformed or extreme output coordinates cannot
/// overflow before the result is clamped back to the framebuffer range.
fn intersect_gl_scissors(a: GlScissor, b: GlScissor) -> Option<GlScissor> {
    let ax1 = i64::from(a[0]);
    let ay1 = i64::from(a[1]);
    let ax2 = ax1 + i64::from(a[2].max(0));
    let ay2 = ay1 + i64::from(a[3].max(0));
    let bx1 = i64::from(b[0]);
    let by1 = i64::from(b[1]);
    let bx2 = bx1 + i64::from(b[2].max(0));
    let by2 = by1 + i64::from(b[3].max(0));

    let x1 = ax1.max(bx1);
    let y1 = ay1.max(by1);
    let x2 = ax2.min(bx2);
    let y2 = ay2.min(by2);
    if x2 <= x1 || y2 <= y1 {
        return None;
    }

    Some([
        x1.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
        y1.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
        (x2 - x1).min(i64::from(i32::MAX)) as i32,
        (y2 - y1).min(i64::from(i32::MAX)) as i32,
    ])
}

/// Convert a top-left-origin monitor rectangle to a framebuffer-bounded GL
/// scissor, optionally intersecting the compositor's outer damage scissor.
fn monitor_gl_scissor(
    screen_w: u32,
    screen_h: u32,
    mon_x: i32,
    mon_y: i32,
    mon_w: u32,
    mon_h: u32,
    outer_scissor: Option<GlScissor>,
) -> Option<GlScissor> {
    let screen_w = screen_w.min(i32::MAX as u32) as i32;
    let screen_h = screen_h.min(i32::MAX as u32) as i32;
    let mon_w = mon_w.min(i32::MAX as u32) as i32;
    let mon_h = mon_h.min(i32::MAX as u32) as i32;
    let monitor = [
        mon_x,
        screen_h.saturating_sub(mon_y).saturating_sub(mon_h),
        mon_w,
        mon_h,
    ];
    let monitor = intersect_gl_scissors(monitor, [0, 0, screen_w, screen_h])?;
    match outer_scissor {
        Some(outer) => intersect_gl_scissors(monitor, outer),
        None => Some(monitor),
    }
}

/// Crossfade only applies when the monitor uses the global wallpaper. A
/// per-monitor override is independent and must remain fully opaque.
fn monitor_crossfade_layers(
    has_monitor_override: bool,
    has_old_global: bool,
    crossfade_alpha: f32,
) -> (bool, f32) {
    let active = !has_monitor_override && has_old_global && crossfade_alpha < 1.0;
    (active, if active { crossfade_alpha } else { 1.0 })
}

/// Process-wide gate bounding how many wallpaper images decode concurrently.
/// Each decode does `image::open` + a Lanczos3 downscale, which is heavy on CPU
/// and transiently holds a full decoded image in memory. Rapid wallpaper changes
/// or output hotplug (one decode per monitor in `set_monitors`) would otherwise
/// spawn an unbounded number of such threads at once. The value held in the
/// mutex is the number of currently-available decode permits.
fn decode_gate() -> &'static (Mutex<usize>, Condvar) {
    static GATE: OnceLock<(Mutex<usize>, Condvar)> = OnceLock::new();
    GATE.get_or_init(|| {
        let max = std::thread::available_parallelism()
            .map(|n| n.get().min(4))
            .unwrap_or(2);
        (Mutex::new(max), Condvar::new())
    })
}

/// RAII permit for the wallpaper decode gate. Blocks on construction until a
/// permit is free, and returns it on drop (covering early returns and panics).
struct DecodePermit;

impl DecodePermit {
    fn acquire() -> Self {
        let (lock, cvar) = decode_gate();
        let mut avail = lock.lock().unwrap_or_else(|e| e.into_inner());
        while *avail == 0 {
            avail = cvar.wait(avail).unwrap_or_else(|e| e.into_inner());
        }
        *avail -= 1;
        DecodePermit
    }
}

impl Drop for DecodePermit {
    fn drop(&mut self) {
        let (lock, cvar) = decode_gate();
        let mut avail = lock.lock().unwrap_or_else(|e| e.into_inner());
        *avail += 1;
        cvar.notify_one();
    }
}

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
            // Bound concurrent decodes; released when this thread exits.
            let _permit = DecodePermit::acquire();
            let img = match image::open(&path) {
                Ok(img) => img,
                Err(e) => {
                    log::warn!("[wallpaper] failed to load '{}': {}", path, e);
                    return;
                }
            };

            let img = if max_w > 0 && max_h > 0 && (img.width() > max_w || img.height() > max_h) {
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

            gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MIN_FILTER, ffi::LINEAR as i32);
            gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MAG_FILTER, ffi::LINEAR as i32);
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
    // 4. Poll pending wallpaper loads
    // =========================================================================

    /// Check whether any background wallpaper decode has completed.
    /// On success, uploads the texture to the GPU, optionally setting up
    /// crossfade state if `wallpaper_crossfade` is enabled.
    pub(crate) unsafe fn poll_pending_wallpapers(&mut self, gl: &ffi::Gles2) {
        for texture in self.retired_wallpaper_textures.drain(..) {
            unsafe {
                gl.DeleteTextures(1, &texture);
            }
        }

        // --- Global wallpaper ---
        if let Some(ref rx) = self.pending_wallpaper {
            match rx.try_recv() {
                Ok(data) => {
                    // Keep the currently visible texture alive unless the new
                    // upload succeeds.
                    if let Some((tex, w, h)) =
                        unsafe { Self::upload_wallpaper_texture_gles(gl, &data) }
                    {
                        if self.wallpaper_crossfade && self.wallpaper_texture.is_some() {
                            if let Some(old) = self.old_wallpaper_texture.take() {
                                unsafe {
                                    gl.DeleteTextures(1, &old);
                                }
                            }
                            self.old_wallpaper_texture = self.wallpaper_texture.take();
                            self.old_wallpaper_img_w = self.wallpaper_img_w;
                            self.old_wallpaper_img_h = self.wallpaper_img_h;
                            self.old_wallpaper_mode = self.wallpaper_mode;
                            self.wallpaper_transition_start = Some(Instant::now());
                        } else {
                            if let Some(old) = self.wallpaper_texture.take() {
                                unsafe {
                                    gl.DeleteTextures(1, &old);
                                }
                            }
                            if let Some(old) = self.old_wallpaper_texture.take() {
                                unsafe {
                                    gl.DeleteTextures(1, &old);
                                }
                            }
                            self.wallpaper_transition_start = None;
                        }

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
                    if let Some((tex, w, h)) =
                        unsafe { Self::upload_wallpaper_texture_gles(gl, &data) }
                    {
                        if let Some(mw) = self.monitor_wallpapers.get_mut(*mon_idx) {
                            if let Some(old) = mw.texture.replace(tex) {
                                unsafe {
                                    gl.DeleteTextures(1, &old);
                                }
                            }
                            mw.img_w = w;
                            mw.img_h = h;
                            mw.mode = data.mode;
                        } else {
                            // The monitor disappeared while the decode was in
                            // flight; the uploaded texture has no owner.
                            unsafe {
                                gl.DeleteTextures(1, &tex);
                            }
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
    /// opaque first, then blends the new wallpaper over it at increasing alpha.
    /// Fading both layers independently over the cleared framebuffer would
    /// darken the midpoint rather than produce a linear crossfade.
    pub(crate) unsafe fn render_wallpaper(
        &mut self,
        gl: &ffi::Gles2,
        projection: &[f32; 16],
        outer_scissor: Option<GlScissor>,
    ) {
        // Determine crossfade progress (0.0 = just started, 1.0 = complete)
        let crossfade_alpha = if let Some(start) = self.wallpaper_transition_start {
            let elapsed_ms = start.elapsed().as_millis() as u64;
            let duration = self.wallpaper_crossfade_duration_ms.max(1);
            let t = (elapsed_ms as f32) / (duration as f32);
            t.clamp(0.0, 1.0)
        } else {
            1.0
        };

        // Terminate crossfade when complete: free old texture and clear state
        if crossfade_alpha >= 1.0 && self.wallpaper_transition_start.is_some() {
            self.wallpaper_transition_start = None;
            if let Some(old) = self.old_wallpaper_texture.take() {
                unsafe {
                    gl.DeleteTextures(1, &old);
                }
            }
        }

        unsafe {
            gl.UseProgram(self.program);
            self.bind_quad_vao(gl);
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
            gl.Uniform1f(self.win_uniforms.dim, 1.0);
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
                let has_monitor_override = mw.texture.is_some();
                let (tex, img_w, img_h, mode) = if let Some(t) = mw.texture {
                    (t, mw.img_w, mw.img_h, mw.mode)
                } else if let Some(t) = self.wallpaper_texture {
                    (
                        t,
                        self.wallpaper_img_w,
                        self.wallpaper_img_h,
                        self.wallpaper_mode,
                    )
                } else {
                    // No wallpaper available for this monitor
                    continue;
                };

                if img_w == 0 || img_h == 0 {
                    continue;
                }

                // Fill/center modes can extend beyond their target output.
                // Constrain every monitor wallpaper to that monitor and retain
                // the caller's partial-damage restriction.
                let Some(scissor) = monitor_gl_scissor(
                    self.screen_w,
                    self.screen_h,
                    mw.mon_x,
                    mw.mon_y,
                    mw.mon_w,
                    mw.mon_h,
                    outer_scissor,
                ) else {
                    continue;
                };
                gl.Enable(ffi::SCISSOR_TEST);
                gl.Scissor(scissor[0], scissor[1], scissor[2], scissor[3]);

                let (rx, ry, rw, rh) = compute_wallpaper_rect(mode, area, img_w, img_h);
                let (draw_old_global, current_opacity) = monitor_crossfade_layers(
                    has_monitor_override,
                    self.old_wallpaper_texture.is_some(),
                    crossfade_alpha,
                );

                // Set size uniform for the shader
                gl.Uniform2f(self.win_uniforms.size, rw, rh);

                // --- Draw old wallpaper (crossfade out) ---
                if draw_old_global {
                    if let Some(old_tex) = self.old_wallpaper_texture {
                        let (orx, ory, orw, orh) = compute_wallpaper_rect(
                            self.old_wallpaper_mode,
                            area,
                            self.old_wallpaper_img_w,
                            self.old_wallpaper_img_h,
                        );
                        gl.Uniform4f(self.win_uniforms.rect, orx, ory, orw, orh);
                        gl.Uniform2f(self.win_uniforms.size, orw, orh);
                        gl.Uniform1f(self.win_uniforms.opacity, 1.0);
                        gl.BindTexture(ffi::TEXTURE_2D, old_tex);
                        gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
                    }
                }

                // --- Draw current wallpaper ---
                gl.Uniform4f(self.win_uniforms.rect, rx, ry, rw, rh);
                gl.Uniform2f(self.win_uniforms.size, rw, rh);
                gl.Uniform1f(self.win_uniforms.opacity, current_opacity);
                gl.BindTexture(ffi::TEXTURE_2D, tex);
                gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
            }

            // If no per-monitor wallpapers were drawn, draw the global wallpaper
            // across the entire screen as a fallback.
            if self.monitor_wallpapers.is_empty() {
                if let Some(tex) = self.wallpaper_texture {
                    if self.wallpaper_img_w > 0 && self.wallpaper_img_h > 0 {
                        let area = (0.0, 0.0, self.screen_w as f32, self.screen_h as f32);
                        let (rx, ry, rw, rh) = compute_wallpaper_rect(
                            self.wallpaper_mode,
                            area,
                            self.wallpaper_img_w,
                            self.wallpaper_img_h,
                        );

                        gl.Uniform2f(self.win_uniforms.size, rw, rh);
                        let (draw_old_global, current_opacity) = monitor_crossfade_layers(
                            false,
                            self.old_wallpaper_texture.is_some(),
                            crossfade_alpha,
                        );

                        // Old wallpaper crossfade
                        if draw_old_global {
                            if let Some(old_tex) = self.old_wallpaper_texture {
                                let (orx, ory, orw, orh) = compute_wallpaper_rect(
                                    self.old_wallpaper_mode,
                                    area,
                                    self.old_wallpaper_img_w,
                                    self.old_wallpaper_img_h,
                                );
                                gl.Uniform4f(self.win_uniforms.rect, orx, ory, orw, orh);
                                gl.Uniform2f(self.win_uniforms.size, orw, orh);
                                gl.Uniform1f(self.win_uniforms.opacity, 1.0);
                                gl.BindTexture(ffi::TEXTURE_2D, old_tex);
                                gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
                            }
                        }

                        gl.Uniform4f(self.win_uniforms.rect, rx, ry, rw, rh);
                        gl.Uniform2f(self.win_uniforms.size, rw, rh);
                        gl.Uniform1f(self.win_uniforms.opacity, current_opacity);
                        gl.BindTexture(ffi::TEXTURE_2D, tex);
                        gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
                    }
                }
            }

            // Per-monitor drawing temporarily narrows the GL scissor. Restore
            // the exact outer damage state expected by the remaining passes.
            if let Some(scissor) = outer_scissor {
                gl.Enable(ffi::SCISSOR_TEST);
                gl.Scissor(scissor[0], scissor[1], scissor[2], scissor[3]);
            } else {
                gl.Disable(ffi::SCISSOR_TEST);
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
        let wp_mode = parse_wallpaper_mode(mode);
        self.wallpaper_path = path.to_string();
        self.wallpaper_mode = wp_mode;

        log::info!(
            "[wallpaper] set_wallpaper path='{}' mode={:?}",
            path,
            wp_mode
        );

        if path.is_empty() {
            self.pending_wallpaper = None;
            self.retired_wallpaper_textures
                .extend(self.wallpaper_texture.take());
            self.retired_wallpaper_textures
                .extend(self.old_wallpaper_texture.take());
            self.wallpaper_img_w = 0;
            self.wallpaper_img_h = 0;
            self.wallpaper_transition_start = None;
            self.needs_render = true;
            return;
        }

        // Determine max decode size from screen dimensions
        let max_w = self.screen_w;
        let max_h = self.screen_h;

        let rx = Self::load_wallpaper_async(path, max_w, max_h, wp_mode);
        self.pending_wallpaper = Some(rx);
        self.needs_render = true;
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
        assert_eq!(parse_wallpaper_mode("fill"), WallpaperMode::Fill);
        assert_eq!(parse_wallpaper_mode("fit"), WallpaperMode::Fit);
        assert_eq!(parse_wallpaper_mode("stretch"), WallpaperMode::Stretch);
        assert_eq!(parse_wallpaper_mode("center"), WallpaperMode::Center);
    }

    #[test]
    fn test_parse_wallpaper_mode_case_insensitive() {
        assert_eq!(parse_wallpaper_mode("Fill"), WallpaperMode::Fill);
        assert_eq!(parse_wallpaper_mode("FIT"), WallpaperMode::Fit);
        assert_eq!(parse_wallpaper_mode("STRETCH"), WallpaperMode::Stretch);
        assert_eq!(parse_wallpaper_mode("Center"), WallpaperMode::Center);
    }

    #[test]
    fn test_parse_wallpaper_mode_unknown_defaults_fill() {
        assert_eq!(parse_wallpaper_mode(""), WallpaperMode::Fill);
        assert_eq!(parse_wallpaper_mode("unknown"), WallpaperMode::Fill);
        assert_eq!(parse_wallpaper_mode("tile"), WallpaperMode::Fill);
    }

    #[test]
    fn test_compute_rect_stretch() {
        let area = (0.0, 0.0, 1920.0, 1080.0);
        let (x, y, w, h) = compute_wallpaper_rect(WallpaperMode::Stretch, area, 800, 600);
        assert!((x - 0.0).abs() < 0.01);
        assert!((y - 0.0).abs() < 0.01);
        assert!((w - 1920.0).abs() < 0.01);
        assert!((h - 1080.0).abs() < 0.01);
    }

    #[test]
    fn test_compute_rect_fill_covers_area() {
        let area = (0.0, 0.0, 1920.0, 1080.0);
        let (_, _, w, h) = compute_wallpaper_rect(WallpaperMode::Fill, area, 3840, 1080);
        // Fill must cover the area entirely
        assert!(w >= 1920.0 - 0.01);
        assert!(h >= 1080.0 - 0.01);
    }

    #[test]
    fn test_compute_rect_fill_centered() {
        let area = (0.0, 0.0, 1920.0, 1080.0);
        // Uniform aspect ratio image (half size) -> scale 2x -> exact fit
        let (x, y, w, h) = compute_wallpaper_rect(WallpaperMode::Fill, area, 960, 540);
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
        let (_, y, w, h) = compute_wallpaper_rect(WallpaperMode::Fit, area, 1920, 400);
        assert!((w - 1920.0).abs() < 0.01);
        assert!((h - 400.0).abs() < 0.01);
        // Centered vertically: (1080-400)/2 = 340
        assert!((y - 340.0).abs() < 0.01);
    }

    #[test]
    fn test_compute_rect_center_native_size() {
        let area = (0.0, 0.0, 1920.0, 1080.0);
        let (x, y, w, h) = compute_wallpaper_rect(WallpaperMode::Center, area, 800, 600);
        assert!((w - 800.0).abs() < 0.01);
        assert!((h - 600.0).abs() < 0.01);
        // Centered: (1920-800)/2=560, (1080-600)/2=240
        assert!((x - 560.0).abs() < 0.01);
        assert!((y - 240.0).abs() < 0.01);
    }

    #[test]
    fn test_compute_rect_center_large_image_overflows() {
        let area = (0.0, 0.0, 1920.0, 1080.0);
        let (x, y, w, h) = compute_wallpaper_rect(WallpaperMode::Center, area, 2560, 1440);
        assert!((w - 2560.0).abs() < 0.01);
        assert!((h - 1440.0).abs() < 0.01);
        // Negative offsets (extends beyond area)
        assert!((x - (-320.0)).abs() < 0.01);
        assert!((y - (-180.0)).abs() < 0.01);
    }

    #[test]
    fn test_compute_rect_zero_image_returns_area() {
        let area = (10.0, 20.0, 400.0, 300.0);
        let (x, y, w, h) = compute_wallpaper_rect(WallpaperMode::Fill, area, 0, 0);
        assert!((x - 10.0).abs() < 0.01);
        assert!((y - 20.0).abs() < 0.01);
        assert!((w - 400.0).abs() < 0.01);
        assert!((h - 300.0).abs() < 0.01);
    }

    #[test]
    fn test_compute_rect_non_origin_area() {
        // Second monitor at (1920, 0)
        let area = (1920.0, 0.0, 1920.0, 1080.0);
        let (x, y, w, h) = compute_wallpaper_rect(WallpaperMode::Stretch, area, 800, 600);
        assert!((x - 1920.0).abs() < 0.01);
        assert!((y - 0.0).abs() < 0.01);
        assert!((w - 1920.0).abs() < 0.01);
        assert!((h - 1080.0).abs() < 0.01);
    }

    #[test]
    fn monitor_scissor_is_bounded_and_intersects_outer_damage() {
        // The second monitor starts at x=1920 and its Fill image may overflow,
        // but drawing remains within the monitor and the damaged strip.
        assert_eq!(
            monitor_gl_scissor(3840, 1080, 1920, 0, 1920, 1080, Some([1800, 200, 400, 300]),),
            Some([1920, 200, 280, 300])
        );
    }

    #[test]
    fn monitor_scissor_handles_offset_outputs_and_empty_damage() {
        assert_eq!(
            monitor_gl_scissor(3000, 1200, -100, 100, 800, 600, None),
            Some([0, 500, 700, 600])
        );
        assert_eq!(
            monitor_gl_scissor(3000, 1200, 1000, 100, 800, 600, Some([0, 0, 500, 500])),
            None
        );
    }

    #[test]
    fn monitor_override_never_uses_global_crossfade() {
        assert_eq!(monitor_crossfade_layers(true, true, 0.25), (false, 1.0));
        assert_eq!(monitor_crossfade_layers(false, true, 0.25), (true, 0.25));
        // Without an old layer, dimming the new wallpaper would expose the
        // clear color instead of producing a crossfade.
        assert_eq!(monitor_crossfade_layers(false, false, 0.25), (false, 1.0));
    }
}
