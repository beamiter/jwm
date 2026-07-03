//! Backend-independent wallpaper layout helpers.

use crate::config::{BehaviorConfig, WallpaperTagConfig};

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum WallpaperMode {
    Fill,
    Fit,
    Stretch,
    Center,
}

/// Decoded wallpaper image data ready for backend-specific GPU upload.
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

/// Resolve wallpaper (path, mode) for a monitor given its currently-active tag mask.
/// Priority: tag-specific (this monitor) > tag-specific (any monitor) >
/// monitor override > global.
pub(crate) fn resolve_wallpaper_for_tag(
    behavior: &BehaviorConfig,
    monitor_idx: u32,
    active_tags: u32,
) -> (&str, &str) {
    let mut best: Option<&WallpaperTagConfig> = None;
    let mut best_specific = false;
    for wt in &behavior.wallpaper_tags {
        if wt.path.is_empty() {
            continue;
        }
        if active_tags & (1u32 << wt.tag) == 0 {
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

#[cfg(test)]
mod tests {
    use super::{
        WallpaperMode, compute_wallpaper_rect, parse_wallpaper_mode, resolve_wallpaper_for_tag,
    };
    use crate::config::{Config, WallpaperMonitorConfig, WallpaperTagConfig};

    fn screen() -> (f32, f32, f32, f32) {
        (0.0, 0.0, 1920.0, 1080.0)
    }

    #[test]
    fn parse_mode_defaults_to_fill() {
        assert_eq!(parse_wallpaper_mode("fill"), WallpaperMode::Fill);
        assert_eq!(parse_wallpaper_mode(""), WallpaperMode::Fill);
        assert_eq!(parse_wallpaper_mode("unknown"), WallpaperMode::Fill);
    }

    #[test]
    fn parse_mode_variants() {
        assert_eq!(parse_wallpaper_mode("fit"), WallpaperMode::Fit);
        assert_eq!(parse_wallpaper_mode("FIT"), WallpaperMode::Fit);
        assert_eq!(parse_wallpaper_mode("stretch"), WallpaperMode::Stretch);
        assert_eq!(parse_wallpaper_mode("Stretch"), WallpaperMode::Stretch);
        assert_eq!(parse_wallpaper_mode("center"), WallpaperMode::Center);
        assert_eq!(parse_wallpaper_mode("Center"), WallpaperMode::Center);
    }

    #[test]
    fn stretch_fills_area() {
        let (x, y, w, h) = compute_wallpaper_rect(WallpaperMode::Stretch, screen(), 800, 600);
        assert!((x - 0.0).abs() < 1.0);
        assert!((y - 0.0).abs() < 1.0);
        assert!((w - 1920.0).abs() < 1.0);
        assert!((h - 1080.0).abs() < 1.0);
    }

    #[test]
    fn fill_covers_area() {
        let (_, _, w, h) = compute_wallpaper_rect(WallpaperMode::Fill, screen(), 3840, 1080);
        assert!(w >= 1920.0 - 1.0);
        assert!(h >= 1080.0 - 1.0);
    }

    #[test]
    fn fill_centers_scaled_image() {
        let (x, y, w, h) = compute_wallpaper_rect(WallpaperMode::Fill, screen(), 960, 540);
        assert!((w - 1920.0).abs() < 1.0);
        assert!((h - 1080.0).abs() < 1.0);
        assert!((x - 0.0).abs() < 1.0);
        assert!((y - 0.0).abs() < 1.0);
    }

    #[test]
    fn fit_preserves_aspect_ratio() {
        let (_, y, w, h) = compute_wallpaper_rect(WallpaperMode::Fit, screen(), 1920, 400);
        assert!((w - 1920.0).abs() < 1.0);
        assert!((h - 400.0).abs() < 1.0);
        assert!((y - 340.0).abs() < 1.0);
    }

    #[test]
    fn center_preserves_image_size() {
        let (x, y, w, h) = compute_wallpaper_rect(WallpaperMode::Center, screen(), 800, 600);
        assert!((w - 800.0).abs() < 1.0);
        assert!((h - 600.0).abs() < 1.0);
        assert!((x - 560.0).abs() < 1.0);
        assert!((y - 240.0).abs() < 1.0);
    }

    #[test]
    fn center_large_image_extends_beyond_area() {
        let (x, _, w, h) = compute_wallpaper_rect(WallpaperMode::Center, screen(), 2560, 1440);
        assert!((w - 2560.0).abs() < 1.0);
        assert!((h - 1440.0).abs() < 1.0);
        assert!((x - (-320.0)).abs() < 1.0);
    }

    #[test]
    fn zero_image_size_returns_area() {
        let area = (10.0, 20.0, 400.0, 300.0);
        let (x, y, w, h) = compute_wallpaper_rect(WallpaperMode::Fill, area, 0, 0);
        assert!((x - 10.0).abs() < 1.0);
        assert!((y - 20.0).abs() < 1.0);
        assert!((w - 400.0).abs() < 1.0);
        assert!((h - 300.0).abs() < 1.0);
    }

    #[test]
    fn non_zero_origin_area_is_preserved() {
        let area = (1920.0, 0.0, 1920.0, 1080.0);
        let (x, y, w, h) = compute_wallpaper_rect(WallpaperMode::Stretch, area, 800, 600);
        assert!((x - 1920.0).abs() < 1.0);
        assert!((y - 0.0).abs() < 1.0);
        assert!((w - 1920.0).abs() < 1.0);
        assert!((h - 1080.0).abs() < 1.0);
    }

    #[test]
    fn resolve_wallpaper_prefers_specific_tag_then_any_tag_then_monitor() {
        let mut behavior = Config::default().behavior().clone();
        behavior.wallpaper = "global.png".to_string();
        behavior.wallpaper_mode = "fill".to_string();
        behavior.wallpaper_monitors = vec![WallpaperMonitorConfig {
            monitor: 1,
            path: "monitor.png".to_string(),
            mode: "fit".to_string(),
        }];
        behavior.wallpaper_tags = vec![
            WallpaperTagConfig {
                tag: 2,
                monitor: -1,
                path: "any-tag.png".to_string(),
                mode: "center".to_string(),
            },
            WallpaperTagConfig {
                tag: 2,
                monitor: 1,
                path: "specific-tag.png".to_string(),
                mode: String::new(),
            },
        ];

        assert_eq!(
            resolve_wallpaper_for_tag(&behavior, 1, 1 << 2),
            ("specific-tag.png", "fill")
        );
        assert_eq!(
            resolve_wallpaper_for_tag(&behavior, 0, 1 << 2),
            ("any-tag.png", "center")
        );
        assert_eq!(
            resolve_wallpaper_for_tag(&behavior, 1, 1 << 3),
            ("monitor.png", "fit")
        );
        assert_eq!(
            resolve_wallpaper_for_tag(&behavior, 0, 1 << 3),
            ("global.png", "fill")
        );
    }
}
