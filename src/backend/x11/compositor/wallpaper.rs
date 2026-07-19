// Wallpaper loading and monitor setup
#[allow(unused_imports)]
use super::math::ortho;
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
use std::sync::{Condvar, Mutex, OnceLock};

use crate::backend::x11::compositor_common::wallpaper::parse_wallpaper_mode;

fn uses_global_wallpaper_fallback(
    resolved_path: &str,
    resolved_mode: WallpaperMode,
    global_path: &str,
    global_mode: WallpaperMode,
) -> bool {
    resolved_path == global_path && resolved_mode == global_mode
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WallpaperUploadFormat {
    internal: i32,
    external: u32,
    pixel_type: u32,
}

/// Decoded wallpapers are RGBA8 source images even when the compositor output
/// is 10-bit. Keep their source texture RGBA8 and let the render target perform
/// the normalized conversion. GLES 3 rejects RGB10_A2 + UNSIGNED_BYTE, while
/// changing only the type would incorrectly reinterpret the unpacked byte data
/// as packed 2:10:10:10 pixels.
fn wallpaper_upload_format() -> WallpaperUploadFormat {
    WallpaperUploadFormat {
        internal: glow::RGBA8 as i32,
        external: glow::RGBA,
        pixel_type: glow::UNSIGNED_BYTE,
    }
}

/// Process-wide gate bounding how many wallpaper images decode concurrently.
/// Each decode does `image::open` + a Lanczos3 downscale (heavy CPU, transient
/// full-image allocation); rapid wallpaper changes or per-monitor setup would
/// otherwise spawn unbounded threads at once. The mutex value is the number of
/// currently-available decode permits.
fn decode_gate() -> &'static (Mutex<usize>, Condvar) {
    static GATE: OnceLock<(Mutex<usize>, Condvar)> = OnceLock::new();
    GATE.get_or_init(|| {
        let max = std::thread::available_parallelism()
            .map(|n| n.get().min(4))
            .unwrap_or(2);
        (Mutex::new(max), Condvar::new())
    })
}

/// RAII permit for the wallpaper decode gate. Blocks until a permit is free,
/// and returns it on drop (covering early returns and panics).
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

impl<C: CompositorConnection> Compositor<C> {
    /// Decode a wallpaper image on a background thread.
    /// Returns a receiver that will deliver the decoded RGBA data.
    pub(super) fn load_wallpaper_async(
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
                    log::warn!("compositor: failed to load wallpaper '{}': {}", path, e);
                    return;
                }
            };

            let img = if max_w > 0 && max_h > 0 && (img.width() > max_w || img.height() > max_h) {
                log::info!(
                    "compositor: downscaling wallpaper '{}' from {}x{} to fit {}x{}",
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
            log::info!("compositor: decoded wallpaper '{}' ({}x{})", path, w, h);

            let _ = tx.send(WallpaperImageData {
                rgba: rgba.into_raw(),
                width: w,
                height: h,
                mode,
            });
        });
        rx
    }

    /// Upload decoded wallpaper RGBA data to a GL texture.
    pub(super) fn upload_wallpaper_texture(
        gl: &glow::Context,
        data: &WallpaperImageData,
    ) -> Option<(glow::Texture, u32, u32)> {
        let expected_len = usize::try_from(data.width)
            .ok()?
            .checked_mul(usize::try_from(data.height).ok()?)?
            .checked_mul(4)?;
        if data.width == 0 || data.height == 0 || data.rgba.len() != expected_len {
            log::warn!(
                "compositor: invalid wallpaper image data ({}x{}, {} bytes)",
                data.width,
                data.height,
                data.rgba.len()
            );
            return None;
        }

        unsafe {
            // Attribute the error check below to this upload rather than an
            // unrelated, already-reported operation.
            for _ in 0..8 {
                if gl.get_error() == glow::NO_ERROR {
                    break;
                }
            }
            let tex = match gl.create_texture() {
                Ok(t) => t,
                Err(e) => {
                    log::warn!("compositor: failed to create wallpaper texture: {}", e);
                    return None;
                }
            };
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            let upload = wallpaper_upload_format();
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                upload.internal,
                data.width as i32,
                data.height as i32,
                0,
                upload.external,
                upload.pixel_type,
                glow::PixelUnpackData::Slice(Some(&data.rgba)),
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                glow::LINEAR as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MAG_FILTER,
                glow::LINEAR as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_S,
                glow::CLAMP_TO_EDGE as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_T,
                glow::CLAMP_TO_EDGE as i32,
            );
            gl.bind_texture(glow::TEXTURE_2D, None);
            let upload_error = gl.get_error();
            if upload_error != glow::NO_ERROR {
                gl.delete_texture(tex);
                log::warn!(
                    "compositor: wallpaper texture upload failed with GL error 0x{upload_error:x}"
                );
                return None;
            }
            log::info!(
                "compositor: uploaded wallpaper texture ({}x{})",
                data.width,
                data.height
            );
            Some((tex, data.width, data.height))
        }
    }

    /// Update monitor geometries and per-monitor wallpaper textures.
    /// Called when monitors are added/removed/changed AND when the active
    /// tag mask changes on a monitor (per-tag wallpaper resolution).
    /// `monitors`: list of (index, x, y, w, h, active_tags) for each monitor.
    pub(crate) fn set_monitors(&mut self, monitors: &[(u32, i32, i32, u32, u32, u32)]) {
        // Detect topology change: if monitor count or geometry differs we have
        // to tear down existing per-monitor wallpaper textures. If only
        // `active_tags` changed (typical view/toggleview path), keep existing
        // textures and just re-resolve paths.
        let geometry_changed = self.monitor_wallpapers.len() != monitors.len()
            || self
                .monitor_wallpapers
                .iter()
                .zip(monitors.iter())
                .any(|(mw, b)| (mw.mon_x, mw.mon_y, mw.mon_w, mw.mon_h) != (b.1, b.2, b.3, b.4));

        if geometry_changed {
            // Clean up old per-monitor textures
            unsafe {
                for mw in self.monitor_wallpapers.drain(..) {
                    if let Some(tex) = mw.texture {
                        self.gl.delete_texture(tex);
                    }
                }
            }
            self.pending_monitor_wallpapers.clear();
        }

        let cfg = crate::config::CONFIG.load();
        let behavior = cfg.behavior();

        for &(idx, x, y, w, h, active_tags) in monitors {
            let (resolved_path, mode_str) = resolve_wallpaper_for_tag(behavior, idx, active_tags);
            let mode = parse_wallpaper_mode(mode_str);
            let uses_global_fallback = uses_global_wallpaper_fallback(
                resolved_path,
                mode,
                &behavior.wallpaper,
                parse_wallpaper_mode(&behavior.wallpaper_mode),
            );
            // `mw.texture` is reserved for an actual monitor/tag override.
            // Outputs whose resolved configuration equals the global default
            // share `wallpaper_texture`, which also makes global crossfades
            // apply consistently without duplicate decodes.
            let override_path = if uses_global_fallback {
                String::new()
            } else {
                resolved_path.to_string()
            };

            if geometry_changed {
                let mon_idx = self.monitor_wallpapers.len();
                if !override_path.is_empty() {
                    let rx = Self::load_wallpaper_async(
                        &override_path,
                        self.screen_w,
                        self.screen_h,
                        mode,
                    );
                    self.pending_monitor_wallpapers.push((mon_idx, rx));
                }
                self.monitor_wallpapers.push(MonitorWallpaper {
                    mon_x: x,
                    mon_y: y,
                    mon_w: w,
                    mon_h: h,
                    texture: None,
                    mode,
                    img_w: 0,
                    img_h: 0,
                    current_path: override_path,
                });
            } else if let Some(mw) = self.monitor_wallpapers.get_mut(idx as usize) {
                if mw.current_path != override_path || mw.mode != mode {
                    mw.mode = mode;
                    mw.current_path.clone_from(&override_path);
                    // Only the newest request for a monitor may publish. A
                    // slower decode of the previous tag's wallpaper must not
                    // race in afterward and overwrite the current selection.
                    self.pending_monitor_wallpapers
                        .retain(|(pending_idx, _)| *pending_idx != idx as usize);
                    if !override_path.is_empty() {
                        let rx = Self::load_wallpaper_async(
                            &override_path,
                            self.screen_w,
                            self.screen_h,
                            mode,
                        );
                        self.pending_monitor_wallpapers.push((idx as usize, rx));
                    } else if let Some(texture) = mw.texture.take() {
                        unsafe {
                            self.gl.delete_texture(texture);
                        }
                        mw.img_w = 0;
                        mw.img_h = 0;
                    }
                }
            }
        }

        self.needs_render = true;
        if geometry_changed {
            log::info!(
                "compositor: set_monitors: {} monitors, {} monitor / {} tag wallpaper overrides",
                monitors.len(),
                behavior.wallpaper_monitors.len(),
                behavior.wallpaper_tags.len(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{uses_global_wallpaper_fallback, wallpaper_upload_format};
    use crate::backend::compositor_common::wallpaper::WallpaperMode;

    #[test]
    fn rgba8_wallpaper_source_format_is_valid_for_gles_and_hdr_outputs() {
        let format = wallpaper_upload_format();
        assert_eq!(format.internal, glow::RGBA8 as i32);
        assert_eq!(format.external, glow::RGBA);
        assert_eq!(format.pixel_type, glow::UNSIGNED_BYTE);
    }

    #[test]
    fn identical_monitor_selection_reuses_global_texture() {
        assert!(uses_global_wallpaper_fallback(
            "global.png",
            WallpaperMode::Fill,
            "global.png",
            WallpaperMode::Fill,
        ));
    }

    #[test]
    fn monitor_path_or_layout_difference_is_an_override() {
        assert!(!uses_global_wallpaper_fallback(
            "monitor.png",
            WallpaperMode::Fill,
            "global.png",
            WallpaperMode::Fill,
        ));
        assert!(!uses_global_wallpaper_fallback(
            "global.png",
            WallpaperMode::Fit,
            "global.png",
            WallpaperMode::Fill,
        ));
    }
}
