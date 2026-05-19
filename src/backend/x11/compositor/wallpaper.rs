// Wallpaper loading and monitor setup
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
                    path, img.width(), img.height(), max_w, max_h,
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
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
            gl.bind_texture(glow::TEXTURE_2D, None);
            log::info!("compositor: uploaded wallpaper texture ({}x{})", data.width, data.height);
            Some((tex, data.width, data.height))
        }
    }


    /// Compute the draw rect (x, y, w, h) for a wallpaper within a target area.
    /// `area`: (x, y, w, h) of the target area in screen coords.
    /// `img_w`, `img_h`: source image dimensions.
    pub(super) fn compute_wallpaper_rect(
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
                let scale = (aw / iw).max(ah / ih);
                let dw = iw * scale;
                let dh = ih * scale;
                let dx = ax + (aw - dw) * 0.5;
                let dy = ay + (ah - dh) * 0.5;
                (dx, dy, dw, dh)
            }
            WallpaperMode::Fit => {
                let scale = (aw / iw).min(ah / ih);
                let dw = iw * scale;
                let dh = ih * scale;
                let dx = ax + (aw - dw) * 0.5;
                let dy = ay + (ah - dh) * 0.5;
                (dx, dy, dw, dh)
            }
            WallpaperMode::Center => {
                let dx = ax + (aw - iw) * 0.5;
                let dy = ay + (ah - ih) * 0.5;
                (dx, dy, iw, ih)
            }
        }
    }

    pub(super) fn parse_wallpaper_mode(s: &str) -> WallpaperMode {
        match s {
            "fit" => WallpaperMode::Fit,
            "stretch" => WallpaperMode::Stretch,
            "center" => WallpaperMode::Center,
            _ => WallpaperMode::Fill,
        }
    }

    /// Update monitor geometries and per-monitor wallpaper textures.
    /// Called when monitors are added/removed/changed.
    /// `monitors`: list of (index, x, y, w, h) for each monitor.
    pub(crate) fn set_monitors(&mut self, monitors: &[(u32, i32, i32, u32, u32)]) {
        // Phase 3.5: Save old wallpaper texture for crossfade
        if self.wallpaper_crossfade && self.wallpaper_texture.is_some() {
            if let Some(old) = self.old_wallpaper_texture.take() {
                unsafe { self.gl.delete_texture(old); }
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

        let cfg = crate::config::CONFIG.load();
        let behavior = cfg.behavior();

        // Clear any previous pending monitor wallpaper loads
        self.pending_monitor_wallpapers.clear();

        for &(idx, x, y, w, h) in monitors {
            // Check if there's a per-monitor config for this index
            let per_mon = behavior.wallpaper_monitors.iter().find(|wm| wm.monitor == idx);

            let (path, mode_str) = if let Some(pm) = per_mon {
                (
                    if pm.path.is_empty() { &behavior.wallpaper } else { &pm.path },
                    if pm.mode.is_empty() { &behavior.wallpaper_mode } else { &pm.mode },
                )
            } else {
                (&behavior.wallpaper, &behavior.wallpaper_mode)
            };

            let mode = Self::parse_wallpaper_mode(mode_str);
            let mon_idx = self.monitor_wallpapers.len();

            // Spawn async decode for per-monitor wallpaper
            if !path.is_empty() {
                let rx = Self::load_wallpaper_async(path, self.screen_w, self.screen_h, mode);
                self.pending_monitor_wallpapers.push((mon_idx, rx));
            }

            self.monitor_wallpapers.push(MonitorWallpaper {
                mon_x: x,
                mon_y: y,
                mon_w: w,
                mon_h: h,
                texture: None, // will be filled when async load completes
                mode,
                img_w: 0,
                img_h: 0,
            });
        }

        self.needs_render = true;
        log::info!(
            "compositor: set_monitors: {} monitors, {} with wallpaper overrides",
            monitors.len(),
            behavior.wallpaper_monitors.len(),
        );
    }

}
