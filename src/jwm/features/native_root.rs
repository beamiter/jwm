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
    NativeMonitorBlit, blit_rgba_clipped, compose_native_root, compute_wallpaper_rect,
    parse_wallpaper_mode, resolve_wallpaper_for_tag,
};
use crate::config::CONFIG;
use crate::jwm::Jwm;
use crate::jwm::features::connectivity::{BackgroundJob, job_in_flight};
use crate::jwm::features::wallpaper::expand_home;

/// RGBA8 canvas ready to upload as a native root pixmap.
#[derive(Debug)]
pub struct NativeRootPixels {
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
        let Some(pixels) = result else {
            log::warn!("native root wallpaper: decode produced no image");
            return;
        };
        if let Err(error) =
            backend.install_root_wallpaper(pixels.width, pixels.height, &pixels.rgba)
        {
            log::warn!("native root wallpaper: {error}");
        }
    }

    fn native_root_request(&self) -> Option<NativeRootRequest> {
        let cfg = CONFIG.load();
        let behavior = cfg.behavior();
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        let fill = ui_theme::palette().native_letterbox_rgb();
        let root_w = self.s_w.max(1) as u32;
        let root_h = self.s_h.max(1) as u32;
        let mut images: Vec<(String, PathBuf)> = Vec::new();
        let mut monitors = Vec::new();
        let mut key = format!("{root_w}x{root_h};fill={fill:?}");
        for (idx, &mk) in self.state.monitor_order.iter().enumerate() {
            let Some(monitor) = self.state.monitors.get(mk) else {
                continue;
            };
            let (path, mode_str) =
                resolve_wallpaper_for_tag(behavior, idx as u32, monitor.get_active_tags());
            let expanded = expand_home(path, &home);
            key.push_str(&format!(
                ";{idx}:{}x{}+{}+{}:{path}:{mode_str}",
                monitor.geometry.m_w,
                monitor.geometry.m_h,
                monitor.geometry.m_x,
                monitor.geometry.m_y
            ));
            if path.trim().is_empty() {
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

fn decode_native_root(request: NativeRootRequest) -> Option<NativeRootPixels> {
    let mut decoded = HashMap::new();
    for (index, (_, path)) in request.images.iter().enumerate() {
        match image::open(path) {
            Ok(image) => {
                decoded.insert(index, image.to_rgba8());
            }
            Err(error) => {
                log::warn!("native root wallpaper: {}: {error}", path.display());
            }
        }
    }
    let remapped = request.monitors;
    let mut rgba = compose_native_root(
        request.root_w,
        request.root_h,
        request.fill,
        &[],
        &remapped,
    )?;
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
        let placed = if tw == image.width() && th == image.height() {
            image.clone()
        } else {
            image::imageops::resize(image, tw, th, image::imageops::FilterType::Triangle)
        };
        blit_rgba_clipped(
            &mut rgba,
            request.root_w,
            request.root_h,
            placed.as_raw(),
            placed.width(),
            placed.height(),
            dx.round() as i32,
            dy.round() as i32,
        );
    }
    Some(NativeRootPixels {
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
        assert_eq!(pixels.rgba, vec![1, 2, 3, 255, 1, 2, 3, 255]);
    }
}
