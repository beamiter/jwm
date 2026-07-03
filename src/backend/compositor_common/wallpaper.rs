//! Backend-independent wallpaper layout helpers.

use crate::config::{BehaviorConfig, WallpaperTagConfig};

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum WallpaperMode {
    Fill,
    Fit,
    Stretch,
    Center,
}

pub(crate) struct WallpaperImageData {
    pub(crate) rgba: Vec<u8>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) mode: WallpaperMode,
}

pub(crate) fn parse_wallpaper_mode(s: &str) -> WallpaperMode {
    match s.to_ascii_lowercase().as_str() {
        "fit" => WallpaperMode::Fit,
        "stretch" => WallpaperMode::Stretch,
        "center" => WallpaperMode::Center,
        _ => WallpaperMode::Fill,
    }
}

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
            let scale = (aw / iw).max(ah / ih);
            let dw = iw * scale;
            let dh = ih * scale;
            (ax + (aw - dw) * 0.5, ay + (ah - dh) * 0.5, dw, dh)
        }
        WallpaperMode::Fit => {
            let scale = (aw / iw).min(ah / ih);
            let dw = iw * scale;
            let dh = ih * scale;
            (ax + (aw - dw) * 0.5, ay + (ah - dh) * 0.5, dw, dh)
        }
        WallpaperMode::Center => (ax + (aw - iw) * 0.5, ay + (ah - ih) * 0.5, iw, ih),
    }
}

pub(crate) fn resolve_wallpaper_for_tag(
    behavior: &BehaviorConfig,
    monitor_idx: u32,
    active_tags: u32,
) -> (&str, &str) {
    let mut best: Option<&WallpaperTagConfig> = None;
    let mut best_specific = false;
    for wt in &behavior.wallpaper_tags {
        if wt.path.is_empty() || active_tags & (1u32 << wt.tag) == 0 {
            continue;
        }
        let specific = wt.monitor == monitor_idx as i32;
        let any = wt.monitor < 0;
        if !specific && !any {
            continue;
        }
        if best.is_none() || (specific && !best_specific) {
            best = Some(wt);
            best_specific = specific;
            if specific {
                break;
            }
        }
    }
    if let Some(wt) = best {
        let mode = if wt.mode.is_empty() {
            &behavior.wallpaper_mode
        } else {
            &wt.mode
        };
        return (&wt.path, mode);
    }

    if let Some(pm) = behavior
        .wallpaper_monitors
        .iter()
        .find(|wm| wm.monitor == monitor_idx)
    {
        let path = if pm.path.is_empty() {
            &behavior.wallpaper
        } else {
            &pm.path
        };
        let mode = if pm.mode.is_empty() {
            &behavior.wallpaper_mode
        } else {
            &pm.mode
        };
        return (path, mode);
    }
    (&behavior.wallpaper, &behavior.wallpaper_mode)
}
