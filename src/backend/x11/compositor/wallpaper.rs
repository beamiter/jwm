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
use std::sync::atomic::{AtomicU64, Ordering};
#[allow(unused_imports)]
use std::sync::mpsc;
use std::sync::{Condvar, Mutex, OnceLock};

use crate::backend::x11::compositor_common::wallpaper::{PREVIEW_THUMB_EDGE, parse_wallpaper_mode};

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

/// Counting gate bounding how many wallpaper images decode concurrently.
/// Each decode does `image::open` + a Lanczos3 downscale (heavy CPU, transient
/// full-image allocation); rapid wallpaper changes or per-monitor setup would
/// otherwise run unbounded decodes at once.
struct DecodeGate {
    /// The number of currently-available decode permits.
    available: Mutex<usize>,
    freed: Condvar,
}

impl DecodeGate {
    const fn new(permits: usize) -> Self {
        Self {
            available: Mutex::new(permits),
            freed: Condvar::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, usize> {
        self.available.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Block until a permit is free.
    fn acquire(&self) -> DecodePermit<'_> {
        let mut available = self.lock();
        while *available == 0 {
            available = self
                .freed
                .wait(available)
                .unwrap_or_else(|e| e.into_inner());
        }
        *available -= 1;
        DecodePermit { gate: self }
    }

    /// Block until a permit is free, or give up once `cancelled` reports
    /// true. The token is re-read on every wake-up; [`Self::wake_all`] is how
    /// a canceller makes blocked waiters look at it.
    fn acquire_unless(&self, cancelled: impl Fn() -> bool) -> Option<DecodePermit<'_>> {
        let mut available = self.lock();
        loop {
            if cancelled() {
                // `notify_one` may have picked this waiter for a freed permit
                // it no longer wants; hand the wake-up on, or a waiter that
                // still wants the permit sleeps beside a free one.
                if *available > 0 {
                    self.freed.notify_one();
                }
                return None;
            }
            if *available > 0 {
                *available -= 1;
                return Some(DecodePermit { gate: self });
            }
            available = self
                .freed
                .wait(available)
                .unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Wake every blocked waiter so cancellable ones re-check their token;
    /// the others find no permit and wait again. Taking the lock orders the
    /// wake-up after a waiter's check, so none can miss it.
    fn wake_all(&self) {
        let _available = self.lock();
        self.freed.notify_all();
    }
}

/// The process-wide decode gate shared by wallpaper loads and side previews.
fn decode_gate() -> &'static DecodeGate {
    static GATE: OnceLock<DecodeGate> = OnceLock::new();
    GATE.get_or_init(|| {
        let max = std::thread::available_parallelism()
            .map(|n| n.get().min(4))
            .unwrap_or(2);
        DecodeGate::new(max)
    })
}

/// RAII permit for a [`DecodeGate`]; returns the permit on drop (covering
/// early returns and panics).
struct DecodePermit<'a> {
    gate: &'a DecodeGate,
}

impl Drop for DecodePermit<'_> {
    fn drop(&mut self) {
        let mut available = self.gate.lock();
        *available += 1;
        self.gate.freed.notify_one();
    }
}

/// Latest-wins tickets for the wallpaper picker's side preview.
///
/// A held arrow key moves the highlight faster than a full-size image
/// decodes. Dropping a superseded request's receiver only discards its
/// result; without a ticket every superseded worker still queued on the
/// shared gate, decoded its whole image and delayed both the preview the user
/// is looking at and any real wallpaper change behind it.
struct PreviewRequests {
    latest: AtomicU64,
}

impl PreviewRequests {
    const fn new() -> Self {
        Self {
            latest: AtomicU64::new(0),
        }
    }

    /// Issue the ticket for a new request, superseding every earlier one, and
    /// wake the decodes blocked on `gate` so the superseded ones leave now.
    fn supersede(&self, gate: &DecodeGate) -> u64 {
        let ticket = self.latest.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
        gate.wake_all();
        ticket
    }

    fn is_current(&self, ticket: u64) -> bool {
        self.latest.load(Ordering::Acquire) == ticket
    }
}

static SIDE_PREVIEW_REQUESTS: PreviewRequests = PreviewRequests::new();

/// Decode one side-preview thumbnail unless a newer request supersedes it
/// first. The check runs while waiting for a permit and again after `open`,
/// the long part: a highlight that moved on meanwhile does not need the
/// resize either.
fn decode_side_preview(
    gate: &DecodeGate,
    requests: &PreviewRequests,
    ticket: u64,
    open: impl FnOnce() -> Option<image::DynamicImage>,
) -> Option<WallpaperImageData> {
    let stale = || !requests.is_current(ticket);
    // Bound concurrent decodes; released when this returns.
    let _permit = gate.acquire_unless(stale)?;
    let img = open()?;
    if stale() {
        return None;
    }
    let img = if img.width() > PREVIEW_THUMB_EDGE || img.height() > PREVIEW_THUMB_EDGE {
        img.resize(
            PREVIEW_THUMB_EDGE,
            PREVIEW_THUMB_EDGE,
            image::imageops::FilterType::Lanczos3,
        )
    } else {
        img
    };
    let rgba = img.to_rgba8();
    let (width, height) = (rgba.width(), rgba.height());
    Some(WallpaperImageData {
        rgba: rgba.into_raw(),
        width,
        height,
        mode: WallpaperMode::Fit,
    })
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
            let _permit = decode_gate().acquire();
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

    /// Decode a wallpaper picker's side-preview thumbnail on a background
    /// thread. Same worker pattern as [`Self::load_wallpaper_async`] — decode
    /// gate, Lanczos3 downscale, channel back — but bounded to a thumbnail,
    /// latest-wins, and quiet about it: an unreadable candidate is a
    /// no-preview, not a warning the user cannot act on.
    pub(super) fn load_system_ui_preview_async(path: &str) -> mpsc::Receiver<WallpaperImageData> {
        let (tx, rx) = mpsc::channel();
        let path = path.to_string();
        let gate = decode_gate();
        // Issuing this ticket supersedes every earlier preview: a worker still
        // waiting for a permit leaves at once, and one mid-decode skips its
        // resize, so a held arrow key costs at most the decodes in flight.
        let ticket = SIDE_PREVIEW_REQUESTS.supersede(gate);
        let spawned = std::thread::Builder::new()
            .name("jwm-preview".to_string())
            .spawn(move || {
                let preview = decode_side_preview(gate, &SIDE_PREVIEW_REQUESTS, ticket, || {
                    image::open(&path)
                        .map_err(|e| {
                            log::debug!("compositor: no side preview for '{}': {}", path, e)
                        })
                        .ok()
                });
                if let Some(data) = preview {
                    let _ = tx.send(data);
                }
            });
        if let Err(error) = spawned {
            // The sender went down with the closure, so the poll sees a
            // disconnected channel: no preview until the highlight moves,
            // rather than a panic on the compositor thread.
            log::warn!("compositor: could not start a side-preview decode: {error}");
        }
        rx
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
    use super::{
        DecodeGate, PREVIEW_THUMB_EDGE, PreviewRequests, decode_side_preview,
        uses_global_wallpaper_fallback, wallpaper_upload_format,
    };
    use crate::backend::compositor_common::wallpaper::WallpaperMode;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn available(gate: &DecodeGate) -> usize {
        *gate.lock()
    }

    #[test]
    fn a_superseded_side_preview_never_decodes() {
        let gate = DecodeGate::new(1);
        let requests = PreviewRequests::new();
        let opened = AtomicUsize::new(0);
        let open = || {
            opened.fetch_add(1, Ordering::Relaxed);
            Some(image::DynamicImage::new_rgba8(960, 10))
        };

        let first = requests.supersede(&gate);
        let second = requests.supersede(&gate);
        assert!(decode_side_preview(&gate, &requests, first, open).is_none());
        assert_eq!(
            opened.load(Ordering::Relaxed),
            0,
            "a highlight the user already left must not cost a decode"
        );

        let preview = decode_side_preview(&gate, &requests, second, open)
            .expect("the latest highlight decodes");
        assert_eq!(opened.load(Ordering::Relaxed), 1);
        assert_eq!((preview.width, preview.height), (PREVIEW_THUMB_EDGE, 5));
        assert_eq!(preview.mode, WallpaperMode::Fit);
        assert_eq!(available(&gate), 1, "both requests returned the gate");
    }

    #[test]
    fn a_side_preview_superseded_mid_decode_skips_the_resize() {
        let gate = DecodeGate::new(1);
        let requests = PreviewRequests::new();
        let ticket = requests.supersede(&gate);
        let preview = decode_side_preview(&gate, &requests, ticket, || {
            // The highlight moves while the image is being read.
            requests.supersede(&gate);
            Some(image::DynamicImage::new_rgba8(960, 10))
        });
        assert!(preview.is_none());
        assert_eq!(available(&gate), 1);
    }

    #[test]
    fn superseding_releases_a_side_preview_blocked_on_the_gate() {
        let gate = DecodeGate::new(1);
        let requests = PreviewRequests::new();
        let opened = AtomicUsize::new(0);
        // A wallpaper decode holds the only permit.
        let held = gate.acquire();
        let ticket = requests.supersede(&gate);
        std::thread::scope(|scope| {
            let waiter = scope.spawn(|| {
                decode_side_preview(&gate, &requests, ticket, || {
                    opened.fetch_add(1, Ordering::Relaxed);
                    None
                })
                .is_none()
            });
            // Whether the waiter already sleeps on the gate or has yet to
            // reach it, the new ticket sends it away without the permit.
            requests.supersede(&gate);
            assert!(matches!(waiter.join(), Ok(true)));
        });
        assert_eq!(opened.load(Ordering::Relaxed), 0);
        drop(held);
        assert_eq!(available(&gate), 1);
    }

    #[test]
    fn a_cancelled_waiter_leaves_a_free_permit_for_the_next_one() {
        let gate = DecodeGate::new(1);
        assert!(gate.acquire_unless(|| true).is_none());
        assert_eq!(available(&gate), 1);
        let permit = gate.acquire_unless(|| false).expect("the permit is free");
        assert_eq!(available(&gate), 0);
        drop(permit);
        assert_eq!(available(&gate), 1);
    }

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
