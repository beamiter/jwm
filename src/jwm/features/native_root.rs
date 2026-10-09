//! Compositor-off root wallpaper.
//!
//! The GPU compositor owns `behavior.wallpaper` while it is running. When it
//! is off the same picture is decoded off-thread, laid out per output with
//! the same Fill/Fit/Stretch/Center rules, and installed as the X11 root
//! pixmap (`_XROOTPMAP_ID`) so gaps and bars see the desktop instead of a
//! solid black root.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::backend::api::Backend;
use crate::backend::compositor_common::ui_theme;
use crate::backend::compositor_common::wallpaper::{
    MAX_NATIVE_ROOT_BYTES, NativeMonitorBlit, blit_rgba_clipped, compose_native_root,
    compute_wallpaper_rect, parse_wallpaper_mode, resolve_wallpaper_for_tag,
};
use crate::config::CONFIG;
use crate::jwm::Jwm;
use crate::jwm::features::connectivity::{BackgroundJob, job_in_flight};
use crate::jwm::features::wallpaper::expand_home;

/// RGBA8 canvas ready to upload as a native root pixmap.
#[derive(Debug)]
pub struct NativeRootPixels {
    pub(crate) key: String,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) rgba: Vec<u8>,
}

struct NativeRootRequest {
    key: String,
    root_w: u32,
    root_h: u32,
    fill: [u8; 3],
    images: Vec<(String, PathBuf)>,
    monitors: Vec<NativeMonitorBlit>,
}

impl Jwm {
    /// Start or skip a native wallpaper decode when the compositor is off.
    pub(crate) fn ensure_native_root_wallpaper(&mut self, backend: &mut dyn Backend) {
        if backend.has_compositor() || !backend.native_root_wallpaper_supported() {
            return;
        }
        let Some(request) = self.native_root_request() else {
            return;
        };
        if self.features.native_root_key == request.key {
            if job_in_flight(self.features.native_root_job.as_ref()) {
                return;
            }
            if self.features.native_root_job.is_none() {
                return;
            }
        }
        self.features.native_root_key = request.key.clone();
        if request.images.is_empty() {
            // A solid letterbox is a few kilobytes of fill — do it on this
            // tick so compositor-off without `behavior.wallpaper` is themed
            // immediately instead of waiting on a worker.
            self.features.native_root_job = None;
            install_native_root_pixels(
                backend,
                &self.features.native_root_key,
                decode_native_root(request),
            );
            return;
        }
        let job = BackgroundJob::spawn(move || decode_native_root(request));
        self.features.native_root_job = job.started().then(|| self.track_background_job(job));
        if self.features.native_root_job.is_none() {
            self.features.native_root_key.clear();
        }
    }

    /// Adopt a finished native wallpaper decode.
    pub(crate) fn poll_native_root_wallpaper(&mut self, backend: &mut dyn Backend) {
        let Some(result) = self
            .features
            .native_root_job
            .as_ref()
            .and_then(BackgroundJob::take)
        else {
            if self
                .features
                .native_root_job
                .as_ref()
                .is_some_and(|job| !job.started())
            {
                self.features.native_root_job = None;
            }
            return;
        };
        self.features.native_root_job = None;
        if backend.has_compositor() {
            return;
        }
        install_native_root_pixels(backend, &self.features.native_root_key, result);
    }

    fn native_root_request(&self) -> Option<NativeRootRequest> {
        let cfg = CONFIG.load();
        let behavior = cfg.behavior();
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        let fill = ui_theme::palette().native_letterbox_rgb();
        let theme = cfg.ui_theme();
        let (root_w, root_h) = native_root_canvas_size(self)?;
        let mut images: Vec<(String, PathBuf)> = Vec::new();
        let mut monitors = Vec::new();
        let mut key = format!("{root_w}x{root_h};theme={theme};fill={fill:?}");
        for (idx, &mk) in self.state.monitor_order.iter().enumerate() {
            let Some(monitor) = self.state.monitors.get(mk) else {
                continue;
            };
            let (path, mode_str) =
                resolve_wallpaper_for_tag(behavior, idx as u32, monitor.get_active_tags());
            let expanded = expand_home(path, &home);
            let token = wallpaper_file_token(&expanded);
            key.push_str(&format!(
                ";{idx}:{}x{}+{}+{}:{token}:{mode_str}",
                monitor.geometry.m_w,
                monitor.geometry.m_h,
                monitor.geometry.m_x,
                monitor.geometry.m_y
            ));
            if path.trim().is_empty() || !expanded.is_file() {
                continue;
            }
            let image_index =
                if let Some(index) = images.iter().position(|(stored, _)| stored == path) {
                    index
                } else {
                    images.push((path.to_string(), expanded));
                    images.len() - 1
                };
            monitors.push(NativeMonitorBlit {
                x: monitor.geometry.m_x,
                y: monitor.geometry.m_y,
                w: monitor.geometry.m_w.max(1) as u32,
                h: monitor.geometry.m_h.max(1) as u32,
                image: image_index,
                mode: parse_wallpaper_mode(mode_str),
            });
        }
        if monitors.is_empty() && images.is_empty() {
            // Still paint the theme letterbox so compositor-off is not the
            // historical solid black root.
            monitors.push(NativeMonitorBlit {
                x: 0,
                y: 0,
                w: root_w,
                h: root_h,
                image: usize::MAX,
                mode: parse_wallpaper_mode("fill"),
            });
        }
        Some(NativeRootRequest {
            key,
            root_w,
            root_h,
            fill,
            images,
            monitors,
        })
    }
}

fn native_root_canvas_size(jwm: &Jwm) -> Option<(u32, u32)> {
    let mut width = jwm.s_w.max(0) as u32;
    let mut height = jwm.s_h.max(0) as u32;
    for &mk in &jwm.state.monitor_order {
        let Some(monitor) = jwm.state.monitors.get(mk) else {
            continue;
        };
        let right = monitor
            .geometry
            .m_x
            .saturating_add(monitor.geometry.m_w)
            .max(0) as u32;
        let bottom = monitor
            .geometry
            .m_y
            .saturating_add(monitor.geometry.m_h)
            .max(0) as u32;
        width = width.max(right);
        height = height.max(bottom);
    }
    if width == 0 || height == 0 {
        return None;
    }
    if width > u16::MAX as u32 || height > u16::MAX as u32 {
        return None;
    }
    Some((width, height))
}

fn wallpaper_file_token(path: &PathBuf) -> String {
    match std::fs::metadata(path) {
        Ok(meta) => {
            let modified = meta
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs())
                .unwrap_or(0);
            format!("{}:{}:{modified}", path.display(), meta.len())
        }
        Err(_) => format!("{}:missing", path.display()),
    }
}

fn load_native_wallpaper(path: &PathBuf) -> Option<image::RgbaImage> {
    let mut reader = image::ImageReader::open(path).ok()?;
    reader = reader.with_guessed_format().ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(MAX_NATIVE_ROOT_BYTES);
    reader.limits(limits);
    reader.decode().ok().map(|image| image.into_rgba8())
}

fn install_native_root_pixels(
    backend: &mut dyn Backend,
    expected_key: &str,
    pixels: Option<NativeRootPixels>,
) {
    let Some(pixels) = pixels else {
        log::warn!("native root wallpaper: decode produced no image");
        return;
    };
    if pixels.key != expected_key {
        return;
    }
    if let Err(error) = backend.install_root_wallpaper(pixels.width, pixels.height, &pixels.rgba) {
        log::warn!("native root wallpaper: {error}");
    }
}

fn decode_native_root(request: NativeRootRequest) -> Option<NativeRootPixels> {
    let mut decoded = HashMap::new();
    for (index, (_, path)) in request.images.iter().enumerate() {
        match load_native_wallpaper(path) {
            Some(image) => {
                decoded.insert(index, image);
            }
            None => {
                log::warn!("native root wallpaper: failed to decode {}", path.display());
            }
        }
    }
    let remapped = request.monitors;
    let mut rgba =
        compose_native_root(request.root_w, request.root_h, request.fill, &[], &remapped)?;
    for slot in &remapped {
        let Some(image) = decoded.get(&slot.image) else {
            continue;
        };
        let (dx, dy, dw, dh) = compute_wallpaper_rect(
            slot.mode,
            (slot.x as f32, slot.y as f32, slot.w as f32, slot.h as f32),
            image.width(),
            image.height(),
        );
        if !dw.is_finite() || !dh.is_finite() || dw <= 0.0 || dh <= 0.0 {
            continue;
        }
        let tw = dw.round().max(1.0) as u32;
        let th = dh.round().max(1.0) as u32;
        if u64::from(tw)
            .saturating_mul(u64::from(th))
            .saturating_mul(4)
            > MAX_NATIVE_ROOT_BYTES
        {
            continue;
        }
        let (pixels, width, height) = if tw == image.width() && th == image.height() {
            (image.as_raw().as_slice(), image.width(), image.height())
        } else {
            let placed =
                image::imageops::resize(image, tw, th, image::imageops::FilterType::Triangle);
            blit_rgba_clipped(
                &mut rgba,
                request.root_w,
                request.root_h,
                placed.as_raw(),
                placed.width(),
                placed.height(),
                dx.round() as i32,
                dy.round() as i32,
                (slot.x, slot.y, slot.w, slot.h),
            );
            continue;
        };
        blit_rgba_clipped(
            &mut rgba,
            request.root_w,
            request.root_h,
            pixels,
            width,
            height,
            dx.round() as i32,
            dy.round() as i32,
            (slot.x, slot.y, slot.w, slot.h),
        );
    }
    Some(NativeRootPixels {
        key: request.key,
        width: request.root_w,
        height: request.root_h,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::NativeRootRequest;
    use super::decode_native_root;
    use crate::backend::compositor_common::wallpaper::{NativeMonitorBlit, WallpaperMode};

    #[test]
    fn a_missing_wallpaper_file_still_fills_the_letterbox() {
        let pixels = decode_native_root(NativeRootRequest {
            key: "test".into(),
            root_w: 2,
            root_h: 1,
            fill: [1, 2, 3],
            images: vec![(
                "missing".into(),
                std::path::PathBuf::from("/no/such/wallpaper.png"),
            )],
            monitors: vec![NativeMonitorBlit {
                x: 0,
                y: 0,
                w: 2,
                h: 1,
                image: 0,
                mode: WallpaperMode::Fill,
            }],
        })
        .expect("letterbox canvas");
        assert_eq!(pixels.key, "test");
        assert_eq!(pixels.rgba, vec![1, 2, 3, 255, 1, 2, 3, 255]);
    }

    #[test]
    fn an_empty_image_list_still_fills_the_letterbox() {
        let pixels = decode_native_root(NativeRootRequest {
            key: "fill".into(),
            root_w: 1,
            root_h: 1,
            fill: [9, 8, 7],
            images: vec![],
            monitors: vec![NativeMonitorBlit {
                x: 0,
                y: 0,
                w: 1,
                h: 1,
                image: usize::MAX,
                mode: WallpaperMode::Fill,
            }],
        })
        .expect("letterbox canvas");
        assert_eq!(pixels.key, "fill");
        assert_eq!(pixels.rgba, vec![9, 8, 7, 255]);
    }

    #[test]
    fn wallpaper_file_token_marks_a_missing_path() {
        assert!(
            super::wallpaper_file_token(&std::path::PathBuf::from("/no/such/wallpaper.png"))
                .ends_with(":missing")
        );
    }
}
