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
#[allow(unused_imports)]
use x11rb::connection::{Connection, RequestConnection};
#[allow(unused_imports)]
use x11rb::protocol::composite::ConnectionExt as CompositeExt;
#[allow(unused_imports)]
use x11rb::protocol::damage::{self, ConnectionExt as DamageExt};
#[allow(unused_imports)]
use x11rb::protocol::randr::ConnectionExt as RandrExt;
#[allow(unused_imports)]
use x11rb::protocol::xfixes::ConnectionExt as XFixesExt;
#[allow(unused_imports)]
use x11rb::protocol::xproto::{self, ConnectionExt as XProtoExt};
#[allow(unused_imports)]
use x11rb::rust_connection::RustConnection;
#[allow(unused_imports)]
use x11rb::wrapper::ConnectionExt as WrapperExt;

use crate::backend::compositor_common::wallpaper::parse_wallpaper_mode;

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

impl Compositor {
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
        hdr_enabled: bool,
    ) -> Option<(glow::Texture, u32, u32)> {
        unsafe {
            let tex = match gl.create_texture() {
                Ok(t) => t,
                Err(e) => {
                    log::warn!("compositor: failed to create wallpaper texture: {}", e);
                    return None;
                }
            };
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            const GL_RGB10_A2: u32 = 0x8059;
            let internal_format = if hdr_enabled {
                GL_RGB10_A2 as i32
            } else {
                glow::RGBA8 as i32
            };
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                internal_format,
                data.width as i32,
                data.height as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
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
            // Phase 3.5: Save old wallpaper texture for crossfade
            if self.wallpaper_crossfade && self.wallpaper_texture.is_some() {
                if let Some(old) = self.old_wallpaper_texture.take() {
                    unsafe {
                        self.gl.delete_texture(old);
                    }
                }
                self.old_wallpaper_texture = self.wallpaper_texture;
                self.wallpaper_transition_start = Some(std::time::Instant::now());
            }

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
            let (path, mode_str) = resolve_wallpaper_for_tag(behavior, idx, active_tags);
            let path = path.to_string();
            let mode = parse_wallpaper_mode(mode_str);

            if geometry_changed {
                let mon_idx = self.monitor_wallpapers.len();
                if !path.is_empty() {
                    let rx = Self::load_wallpaper_async(&path, self.screen_w, self.screen_h, mode);
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
                    current_path: path,
                });
            } else if let Some(mw) = self.monitor_wallpapers.get_mut(idx as usize) {
                if mw.current_path != path || mw.mode != mode {
                    mw.mode = mode;
                    mw.current_path = path.clone();
                    if !path.is_empty() {
                        let rx =
                            Self::load_wallpaper_async(&path, self.screen_w, self.screen_h, mode);
                        self.pending_monitor_wallpapers.push((idx as usize, rx));
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
