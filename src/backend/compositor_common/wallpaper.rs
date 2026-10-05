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
        // `tag` comes straight from the config file; an index past the mask
        // width (only warned about at load) must match no tag rather than
        // overflow the shift, which panics with overflow checks and otherwise
        // wraps onto tag `tag % 32`.
        let bit = 1u32.checked_shl(wt.tag).unwrap_or(0);
        if wt.path.is_empty() || active_tags & bit == 0 {
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

/// Cap on a native X11 root pixmap so a bogus screen size cannot allocate
/// gigabytes on the worker that paints compositor-off wallpaper.
pub(crate) const MAX_NATIVE_ROOT_BYTES: u64 = 512 * 1024 * 1024;

/// One decoded wallpaper image, RGBA8, used as a blit source.
pub(crate) struct NativeWallpaperImage {
    pub(crate) rgba: Vec<u8>,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

/// One output's share of a native root wallpaper.
pub(crate) struct NativeMonitorBlit {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) w: u32,
    pub(crate) h: u32,
    pub(crate) image: usize,
    pub(crate) mode: WallpaperMode,
}

/// Compose a root-sized RGBA8 wallpaper the same way both compositors place
/// an image on each output: per-monitor `Fill`/`Fit`/`Stretch`/`Center` over
/// a solid letterbox.
#[must_use]
pub(crate) fn compose_native_root(
    root_w: u32,
    root_h: u32,
    fill: [u8; 3],
    images: &[NativeWallpaperImage],
    monitors: &[NativeMonitorBlit],
) -> Option<Vec<u8>> {
    let pixels = u64::from(root_w).checked_mul(u64::from(root_h))?;
    let bytes = pixels.checked_mul(4)?;
    if bytes == 0 || bytes > MAX_NATIVE_ROOT_BYTES {
        return None;
    }
    let stride = root_w as usize * 4;
    let mut canvas = vec![0u8; bytes as usize];
    for pixel in canvas.chunks_exact_mut(4) {
        pixel[0] = fill[0];
        pixel[1] = fill[1];
        pixel[2] = fill[2];
        pixel[3] = 255;
    }
    for monitor in monitors {
        let Some(image) = images.get(monitor.image) else {
            continue;
        };
        if image.width == 0
            || image.height == 0
            || image.rgba.len() < image.width as usize * image.height as usize * 4
        {
            continue;
        }
        let dest = compute_wallpaper_rect(
            monitor.mode,
            (
                monitor.x as f32,
                monitor.y as f32,
                monitor.w as f32,
                monitor.h as f32,
            ),
            image.width,
            image.height,
        );
        blit_nearest(&mut canvas, stride, root_w, root_h, image, dest);
    }
    Some(canvas)
}

/// Copy tightly packed RGBA8 onto a root-sized canvas at `(dx, dy)`, clipping
/// to the canvas. Used after a high-quality resize so compositor-off wallpaper
/// is not nearest-neighbour sampled from the original photo.
pub(crate) fn blit_rgba_clipped(
    canvas: &mut [u8],
    root_w: u32,
    root_h: u32,
    rgba: &[u8],
    src_w: u32,
    src_h: u32,
    dx: i32,
    dy: i32,
) {
    if src_w == 0 || src_h == 0 {
        return;
    }
    let dst_stride = root_w as usize * 4;
    let src_stride = src_w as usize * 4;
    if canvas.len() < dst_stride.saturating_mul(root_h as usize)
        || rgba.len() < src_stride.saturating_mul(src_h as usize)
    {
        return;
    }
    for row in 0..src_h {
        let dest_y = dy.saturating_add(row as i32);
        if dest_y < 0 || dest_y >= root_h as i32 {
            continue;
        }
        for col in 0..src_w {
            let dest_x = dx.saturating_add(col as i32);
            if dest_x < 0 || dest_x >= root_w as i32 {
                continue;
            }
        let src = row as usize * src_stride + col as usize * 4;
            let dst = dest_y as usize * dst_stride + dest_x as usize * 4;
            let alpha = rgba[src + 3];
            if alpha == 0 {
                continue;
            }
            if alpha == 255 {
                canvas[dst..dst + 4].copy_from_slice(&rgba[src..src + 4]);
                continue;
            }
            let inv = 255 - alpha as u16;
            for channel in 0..3 {
                let over = rgba[src + channel] as u16 * alpha as u16;
                let under = canvas[dst + channel] as u16 * inv;
                canvas[dst + channel] = ((over + under + 127) / 255) as u8;
            }
            canvas[dst + 3] = 255;
        }
    }
}

fn blit_nearest(
    canvas: &mut [u8],
    stride: usize,
    root_w: u32,
    root_h: u32,
    image: &NativeWallpaperImage,
    dest: (f32, f32, f32, f32),
) {
    let (dx, dy, dw, dh) = dest;
    if !dx.is_finite() || !dy.is_finite() || !dw.is_finite() || !dh.is_finite() {
        return;
    }
    if dw <= 0.0 || dh <= 0.0 {
        return;
    }
    let left = dx.floor() as i32;
    let top = dy.floor() as i32;
    let right = (dx + dw).ceil() as i32;
    let bottom = (dy + dh).ceil() as i32;
    let clip_left = left.max(0);
    let clip_top = top.max(0);
    let clip_right = right.min(root_w as i32);
    let clip_bottom = bottom.min(root_h as i32);
    if clip_left >= clip_right || clip_top >= clip_bottom {
        return;
    }
    let src_w = image.width as f32;
    let src_h = image.height as f32;
    for y in clip_top..clip_bottom {
        let v = ((y as f32 + 0.5 - dy) / dh).clamp(0.0, 1.0 - f32::EPSILON);
        let src_y = (v * src_h).floor() as u32;
        let src_y = src_y.min(image.height - 1) as usize;
        for x in clip_left..clip_right {
            let u = ((x as f32 + 0.5 - dx) / dw).clamp(0.0, 1.0 - f32::EPSILON);
            let src_x = (u * src_w).floor() as u32;
            let src_x = src_x.min(image.width - 1) as usize;
            let src = (src_y * image.width as usize + src_x) * 4;
            let dst = y as usize * stride + x as usize * 4;
            canvas[dst..dst + 4].copy_from_slice(&image.rgba[src..src + 4]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        NativeMonitorBlit, NativeWallpaperImage, PreviewRequest, WallpaperMode, blit_rgba_clipped,
        compose_native_root, preview_request, resolve_wallpaper_for_tag,
    };
    use crate::config::{BehaviorConfig, Config, WallpaperTagConfig};

    fn behavior_with_tags(tags: &[(u32, &str)]) -> BehaviorConfig {
        let mut behavior = Config::default().behavior().clone();
        behavior.wallpaper = "/walls/global.png".to_string();
        behavior.wallpaper_mode = "fill".to_string();
        behavior.wallpaper_monitors.clear();
        behavior.wallpaper_tags = tags
            .iter()
            .map(|&(tag, path)| WallpaperTagConfig {
                tag,
                monitor: -1,
                path: path.to_string(),
                mode: String::new(),
            })
            .collect();
        behavior
    }

    #[test]
    fn a_tag_wallpaper_applies_while_its_tag_is_active() {
        let behavior = behavior_with_tags(&[(1, "/walls/two.png")]);
        assert_eq!(
            resolve_wallpaper_for_tag(&behavior, 0, 0b10),
            ("/walls/two.png", "fill")
        );
        assert_eq!(
            resolve_wallpaper_for_tag(&behavior, 0, 0b01),
            ("/walls/global.png", "fill")
        );
    }

    #[test]
    fn an_out_of_range_tag_index_matches_no_tag() {
        // Tag 32 would alias tag 0 and tag 33 tag 1 under a wrapping shift
        // (and panic with overflow checks on); neither may win over the
        // global wallpaper, whatever tags are active.
        let behavior = behavior_with_tags(&[
            (32, "/walls/bogus-32.png"),
            (33, "/walls/bogus-33.png"),
            (u32::MAX, "/walls/bogus-max.png"),
        ]);
        for active_tags in [0b01, 0b10, u32::MAX] {
            assert_eq!(
                resolve_wallpaper_for_tag(&behavior, 0, active_tags),
                ("/walls/global.png", "fill"),
                "{active_tags:#b}"
            );
        }
        // The highest valid bit still resolves.
        let behavior = behavior_with_tags(&[(31, "/walls/last.png")]);
        assert_eq!(
            resolve_wallpaper_for_tag(&behavior, 0, 1 << 31).0,
            "/walls/last.png"
        );
    }

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

    #[test]
    fn native_root_stretch_covers_the_output_and_leaves_the_letterbox() {
        let image = NativeWallpaperImage {
            rgba: vec![255, 0, 0, 255, 0, 255, 0, 255],
            width: 2,
            height: 1,
        };
        let canvas = compose_native_root(
            4,
            2,
            [0, 0, 255],
            &[image],
            &[NativeMonitorBlit {
                x: 0,
                y: 0,
                w: 2,
                h: 2,
                image: 0,
                mode: WallpaperMode::Stretch,
            }],
        )
        .expect("tiny canvas");
        // Left output is the stretched image; the unused right half keeps the fill.
        assert_eq!(&canvas[0..4], &[255, 0, 0, 255]);
        assert_eq!(&canvas[8..12], &[0, 0, 255, 255]);
        assert_eq!(&canvas[16..20], &[255, 0, 0, 255]);
    }

    #[test]
    fn native_root_refuses_an_empty_or_oversized_canvas() {
        let image = NativeWallpaperImage {
            rgba: vec![255, 255, 255, 255],
            width: 1,
            height: 1,
        };
        let slot = NativeMonitorBlit {
            x: 0,
            y: 0,
            w: 1,
            h: 1,
            image: 0,
            mode: WallpaperMode::Fill,
        };
        assert!(compose_native_root(0, 1, [0, 0, 0], &[image], &[slot]).is_none());
    }

    #[test]
    fn blit_rgba_clipped_copies_inside_the_canvas_and_ignores_overflow() {
        let mut canvas = vec![0u8; 2 * 2 * 4];
        blit_rgba_clipped(
            &mut canvas,
            2,
            2,
            &[9, 8, 7, 255],
            1,
            1,
            1,
            1,
        );
        assert_eq!(&canvas[12..16], &[9, 8, 7, 255]);
        blit_rgba_clipped(&mut canvas, 2, 2, &[1, 2, 3, 255], 1, 1, -1, 0);
        assert_eq!(&canvas[0..4], &[0, 0, 0, 0]);
    }

    #[test]
    fn blit_rgba_clipped_blends_translucent_pixels_over_the_letterbox() {
        let mut canvas = vec![0, 0, 0, 255];
        blit_rgba_clipped(&mut canvas, 1, 1, &[255, 0, 0, 128], 1, 1, 0, 0);
        assert_eq!(canvas[0], 128);
        assert_eq!(canvas[3], 255);
    }
}
