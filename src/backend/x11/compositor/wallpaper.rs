// Wallpaper loading and monitor setup
use super::*;
use glow::HasContext;

use crate::backend::x11::compositor_common::wallpaper::{PREVIEW_THUMB_EDGE, parse_wallpaper_mode};

#[cfg(test)]
use crate::backend::compositor_common::wallpaper::uses_global_wallpaper_fallback;

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

pub(super) use crate::backend::compositor_common::wallpaper_decode::DeferredWallpaperLoad;
impl<C: CompositorConnection> Compositor<C> {
    /// Queue a wallpaper decode. The first poll with an available permit starts
    /// its background worker; pending slots keep the compositor tick armed.
    pub(crate) fn load_wallpaper_async(
        path: &str,
        max_w: u32,
        max_h: u32,
        mode: WallpaperMode,
    ) -> DeferredWallpaperLoad {
        crate::backend::compositor_common::wallpaper_decode::load_wallpaper(
            path, max_w, max_h, mode,
        )
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
            let max_size = gl.get_parameter_i32(glow::MAX_TEXTURE_SIZE).max(0) as u32;
            if data.width > max_size || data.height > max_size {
                gl.delete_texture(tex);
                return None;
            }
            let previous_texture = gl.get_parameter_texture(glow::TEXTURE_BINDING_2D);
            let previous_unpack = gl.get_parameter_buffer(glow::PIXEL_UNPACK_BUFFER_BINDING);
            let unpack_names = [
                glow::UNPACK_ALIGNMENT,
                glow::UNPACK_ROW_LENGTH,
                glow::UNPACK_SKIP_ROWS,
                glow::UNPACK_SKIP_PIXELS,
            ];
            let unpack_values = unpack_names.map(|name| gl.get_parameter_i32(name));
            gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, None);
            for name in unpack_names {
                gl.pixel_store_i32(name, if name == glow::UNPACK_ALIGNMENT { 1 } else { 0 });
            }
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
            let upload_error = gl.get_error();
            gl.bind_texture(glow::TEXTURE_2D, previous_texture);
            gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, previous_unpack);
            for (name, value) in unpack_names.into_iter().zip(unpack_values) {
                gl.pixel_store_i32(name, value);
            }
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

    /// Decode a wallpaper picker's side-preview thumbnail on a background
    /// thread. Same worker pattern as [`Self::load_wallpaper_async`] — decode
    /// gate, Lanczos3 downscale, channel back — but bounded to a thumbnail,
    /// latest-wins, and quiet about it: an unreadable candidate is a
    /// no-preview, not a warning the user cannot act on.
    pub(crate) fn load_system_ui_preview_async(path: &str) -> DeferredWallpaperLoad {
        Self::load_wallpaper_async(
            path,
            PREVIEW_THUMB_EDGE,
            PREVIEW_THUMB_EDGE,
            WallpaperMode::Fit,
        )
    }

    /// Poll the side preview's decode: on arrival, upload and ask for the
    /// frame that draws it. A superseded request's receiver is gone, so only
    /// the latest highlight's decode can land here.
    pub(super) fn poll_system_ui_preview(&mut self) {
        let Some(rx) = &self.pending_system_ui_preview else {
            return;
        };
        match rx.try_recv() {
            Ok(data) => {
                if let Some((tex, w, h)) = Self::upload_wallpaper_texture(&self.gl, &data) {
                    self.free_system_ui_preview();
                    self.system_ui_preview = Some((self.system_ui_preview_path.clone(), tex, w, h));
                }
                self.pending_system_ui_preview = None;
                self.needs_render = true;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            // The worker finished without sending (decode failed, logged
            // there): no preview, no retry until the highlight moves.
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.pending_system_ui_preview = None;
            }
        }
    }

    /// Update monitor geometries and per-monitor wallpaper textures.
    /// Called when monitors are added/removed/changed AND when the active
    /// tag mask changes on a monitor (per-tag wallpaper resolution).
    /// `monitors`: list of (index, x, y, w, h, active_tags) for each monitor.
    pub(crate) fn set_monitor_identities(&mut self, identities: &[u64]) {
        self.monitor_identities = identities.to_vec();
    }

    pub(crate) fn set_monitors(&mut self, monitors: &[(u32, i32, i32, u32, u32, u32)]) {
        let cfg = crate::config::CONFIG.load();
        let behavior = cfg.behavior();
        let day = chrono::Datelike::num_days_from_ce(&chrono::Local::now().date_naive());
        let mut previous: Vec<_> = std::mem::take(&mut self.monitor_wallpapers)
            .into_iter()
            .map(Some)
            .collect();
        let mut pending = std::mem::take(&mut self.pending_monitor_wallpapers);
        let wanted: Vec<_> = monitors
            .iter()
            .enumerate()
            .map(|(slot, &(idx, x, y, w, h, tags))| {
                let (path, mode) = resolve_wallpaper_for_tag_at(
                    behavior,
                    if behavior.wallpaper_rotation == "daily" {
                        slot as u32
                    } else {
                        idx
                    },
                    tags,
                    day,
                );
                let mode = parse_wallpaper_mode(mode);
                let global =
                    crate::backend::compositor_common::wallpaper::uses_global_wallpaper_fallback(
                        path,
                        mode,
                        &behavior.wallpaper,
                        parse_wallpaper_mode(&behavior.wallpaper_mode),
                        behavior.wallpaper_rotation == "daily",
                    );
                (
                    self.monitor_identities.get(slot).copied(),
                    x,
                    y,
                    w,
                    h,
                    if global {
                        String::new()
                    } else {
                        path.to_owned()
                    },
                    mode,
                )
            })
            .collect();
        use crate::backend::compositor_common::wallpaper::{
            WallpaperOutputKey, wallpaper_output_plan,
        };
        let previous_keys: Vec<_> = previous
            .iter()
            .map(|old| {
                let old = old.as_ref().unwrap();
                WallpaperOutputKey {
                    identity: old.identity,
                    position: (old.mon_x, old.mon_y),
                    image: (&old.current_path, old.requested_mode, old.mon_w, old.mon_h),
                    failed: old.load_failed,
                }
            })
            .collect();
        let requested_keys: Vec<_> = wanted
            .iter()
            .map(|(identity, x, y, w, h, path, mode)| WallpaperOutputKey {
                identity: *identity,
                position: (*x, *y),
                image: (path, *mode, *w, *h),
                failed: false,
            })
            .collect();
        let plan = wallpaper_output_plan(&previous_keys, &requested_keys);
        for (slot, (identity, x, y, w, h, path, mode)) in wanted.into_iter().enumerate() {
            let (previous_index, reuse) = plan[slot];
            let old_index = previous_index.filter(|_| reuse);
            let mut wallpaper =
                previous_index
                    .and_then(|i| previous[i].take())
                    .unwrap_or(MonitorWallpaper {
                        identity,
                        mon_x: x,
                        mon_y: y,
                        mon_w: w,
                        mon_h: h,
                        texture: None,
                        mode,
                        requested_mode: mode,
                        img_w: 0,
                        img_h: 0,
                        current_path: String::new(),
                        load_failed: false,
                    });
            wallpaper.identity = identity;
            wallpaper.mon_x = x;
            wallpaper.mon_y = y;
            wallpaper.mon_w = w;
            wallpaper.mon_h = h;
            if let Some(old_index) = old_index {
                if let Some(index) = pending.iter().position(|(index, _)| *index == old_index) {
                    let (_, load) = pending.swap_remove(index);
                    self.pending_monitor_wallpapers.push((slot, load));
                }
            } else {
                wallpaper.current_path = path.clone();
                wallpaper.requested_mode = mode;
                wallpaper.load_failed = false;
                if path.is_empty() {
                    if let Some(texture) = wallpaper.texture.take() {
                        unsafe {
                            self.gl.delete_texture(texture);
                        }
                    }
                    wallpaper.mode = mode;
                    wallpaper.img_w = 0;
                    wallpaper.img_h = 0;
                } else {
                    // Keep the visible image and its mode until the replacement
                    // decode AND GPU upload have succeeded.
                    self.pending_monitor_wallpapers
                        .push((slot, Self::load_wallpaper_async(&path, w, h, mode)));
                }
            }
            self.monitor_wallpapers.push(wallpaper);
        }
        for mut wallpaper in previous.into_iter().flatten() {
            if let Some(texture) = wallpaper.texture.take() {
                unsafe {
                    self.gl.delete_texture(texture);
                }
            }
        }
        // Any unmatched request is cancelled by dropping its lazy queue slot.
        drop(pending);
        self.needs_render = true;
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
    fn daily_selection_is_independent_of_global_crossfade() {
        assert!(!uses_global_wallpaper_fallback(
            "global.png",
            WallpaperMode::Fill,
            "global.png",
            WallpaperMode::Fill,
            true,
        ));
    }

    #[test]
    fn identical_monitor_selection_reuses_global_texture() {
        assert!(uses_global_wallpaper_fallback(
            "global.png",
            WallpaperMode::Fill,
            "global.png",
            WallpaperMode::Fill,
            false,
        ));
    }

    #[test]
    fn monitor_path_or_layout_difference_is_an_override() {
        assert!(!uses_global_wallpaper_fallback(
            "monitor.png",
            WallpaperMode::Fill,
            "global.png",
            WallpaperMode::Fill,
            false,
        ));
        assert!(!uses_global_wallpaper_fallback(
            "global.png",
            WallpaperMode::Fit,
            "global.png",
            WallpaperMode::Fill,
            false,
        ));
    }
}
