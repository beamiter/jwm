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

/// Long-edge bound of the wallpaper picker's side-preview thumbnail, matched
/// to [`super::system_ui_panel::PREVIEW_MAX_W`] so the decode lands at very
/// nearly its drawn size. Both compositors decode through this one bound.
pub(crate) const PREVIEW_THUMB_EDGE: u32 = 480;

/// What a fresh overlay payload asks of the side preview, given the path that
/// is currently requested or decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PreviewRequest {
    /// The payload repeats the current path: keep any in-flight decode and
    /// any uploaded texture. This is what makes a re-sync cheap.
    Keep,
    /// No path in the payload (another panel, or none): drop the in-flight
    /// decode and retire the texture.
    Clear,
    /// A different path: drop the in-flight decode and texture, start over.
    Reload,
}

/// Latest-wins bookkeeping for the side preview, shared so the two
/// compositors cannot drift: a new path supersedes whatever is in flight
/// (the dropped receiver turns the superseded worker's send into a no-op),
/// and only a repeated path leaves the pipeline alone — so re-syncs of the
/// same highlight never restart a decode, and a path whose decode failed is
/// not retried until the highlight leaves and returns to it.
#[must_use]
pub(crate) fn preview_request(current_path: &str, payload: Option<&str>) -> PreviewRequest {
    let wanted = payload.unwrap_or("");
    if wanted == current_path {
        PreviewRequest::Keep
    } else if wanted.is_empty() {
        PreviewRequest::Clear
    } else {
        PreviewRequest::Reload
    }
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

#[cfg(test)]
mod tests {
    use super::{PreviewRequest, preview_request};

    #[test]
    fn a_repeated_path_keeps_the_in_flight_preview_decode() {
        assert_eq!(
            preview_request("/walls/a.png", Some("/walls/a.png")),
            PreviewRequest::Keep
        );
        // An empty request and an absent payload are the same nothing.
        assert_eq!(preview_request("", None), PreviewRequest::Keep);
        assert_eq!(preview_request("", Some("")), PreviewRequest::Keep);
    }

    #[test]
    fn a_payload_without_a_path_retires_the_preview() {
        assert_eq!(preview_request("/walls/a.png", None), PreviewRequest::Clear);
        assert_eq!(
            preview_request("/walls/a.png", Some("")),
            PreviewRequest::Clear
        );
    }

    #[test]
    fn a_new_path_supersedes_the_in_flight_preview_decode() {
        assert_eq!(
            preview_request("/walls/a.png", Some("/walls/b.png")),
            PreviewRequest::Reload
        );
        assert_eq!(
            preview_request("", Some("/walls/b.png")),
            PreviewRequest::Reload
        );
    }
}
