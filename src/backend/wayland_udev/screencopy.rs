/// wlr-screencopy-unstable-v1 protocol implementation for JWM.
///
/// This allows clients like `grim` to request screen content from the compositor.
/// The compositor captures the framebuffer during the render loop and copies the data
/// into the client-provided wl_shm buffer.
use crate::sync_ext::MutexExt;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use log::{debug, info, warn};

use smithay::output::Output;
use smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::utils::Transform;

// Use the canonical path that matches Display<JwmWaylandState> in backend.rs.
// `wayland_udev::state` is now a compatibility re-export of this same module,
// rather than a second compilation of the state implementation.
use crate::backend::wayland::state::JwmWaylandState;

// ---- Shared pending-copy queue ---------------------------------------------------

/// A screencopy frame waiting for the compositor to capture pixels.
pub struct PendingScreencopyFrame {
    /// The `zwlr_screencopy_frame_v1` resource to send events on.
    pub frame: ZwlrScreencopyFrameV1,
    /// The client's wl_buffer to copy pixels into.
    pub buffer: WlBuffer,
    /// The smithay `Output` to capture.
    pub output: Output,
    /// Optional sub-region (x, y, width, height) in output buffer pixels:
    /// the client's logical `capture_output_region` box after the output
    /// transform was undone and the output scale applied, so it indexes the
    /// physical mode-sized capture directly.
    pub region: Option<(i32, i32, i32, i32)>,
    /// Whether to composite the cursor onto the frame.
    pub overlay_cursor: bool,
    /// True for `copy_with_damage` requests: the protocol requires a `damage`
    /// event to be sent before `ready`.
    pub with_damage: bool,
}

// PendingScreencopyFrame contains Wayland protocol objects which are !Send.
// JWM runs everything on the main thread so this is fine.
unsafe impl Send for PendingScreencopyFrame {}

pub type PendingScreencopyQueue = Arc<Mutex<Vec<PendingScreencopyFrame>>>;

pub fn new_pending_screencopy_queue() -> PendingScreencopyQueue {
    Arc::new(Mutex::new(Vec::new()))
}

// ---- Per-frame user data ---------------------------------------------------------

/// User data stored per `zwlr_screencopy_frame_v1` object.
pub struct ScreencopyFrameData {
    /// `None` when the requested `wl_output` had no matching compositor output;
    /// the frame is initialized only so it can be failed cleanly.
    pub output: Option<Output>,
    /// Region to copy in output buffer pixels (see
    /// [`PendingScreencopyFrame::region`]); `None` for a full-output capture.
    pub region: Option<(i32, i32, i32, i32)>,
    pub overlay_cursor: bool,
    pub buffer_info: (u32, u32, u32, wl_shm::Format), // (width, height, stride, format)
    pub pending_queue: PendingScreencopyQueue,
    /// The protocol defines a frame as single-use. Without this guard, one
    /// resource can enqueue itself repeatedly before the next render drain.
    pub copy_requested: AtomicBool,
}

fn claim_copy_request(copy_requested: &AtomicBool) -> bool {
    copy_requested
        .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
}

// ---- Manager global (zwlr_screencopy_manager_v1) ----------------------------------

// Use () as global data - access pending_queue from JwmWaylandState.screencopy_pending
impl GlobalDispatch<ZwlrScreencopyManagerV1, ()> for JwmWaylandState {
    fn bind(
        state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrScreencopyManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        state.record_protocol_bind("zwlr_screencopy_manager_v1");
        data_init.init(resource, ());
    }

    /// Screen capture is privileged: a sandboxed (wp_security_context)
    /// client must not read other clients' pixels.
    fn can_view(client: Client, _global_data: &()) -> bool {
        !crate::backend::wayland::state::client_is_sandboxed(&client)
    }
}

impl Dispatch<ZwlrScreencopyManagerV1, ()> for JwmWaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ZwlrScreencopyManagerV1,
        request: zwlr_screencopy_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_screencopy_manager_v1::Request::CaptureOutput {
                frame: frame_new_id,
                overlay_cursor,
                output: wl_output,
            } => {
                handle_capture(
                    state,
                    data_init,
                    frame_new_id,
                    overlay_cursor,
                    wl_output,
                    None,
                );
            }
            zwlr_screencopy_manager_v1::Request::CaptureOutputRegion {
                frame: frame_new_id,
                overlay_cursor,
                output: wl_output,
                x,
                y,
                width,
                height,
            } => {
                handle_capture(
                    state,
                    data_init,
                    frame_new_id,
                    overlay_cursor,
                    wl_output,
                    Some((x, y, width, height)),
                );
            }
            zwlr_screencopy_manager_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

fn handle_capture(
    state: &mut JwmWaylandState,
    data_init: &mut DataInit<'_, JwmWaylandState>,
    frame_new_id: New<ZwlrScreencopyFrameV1>,
    overlay_cursor: i32,
    wl_output: WlOutput,
    region: Option<(i32, i32, i32, i32)>,
) {
    // Get pending queue from state
    let pending_queue = match state.screencopy_pending.as_ref() {
        Some(q) => q.clone(),
        None => {
            warn!("[screencopy] no pending queue available");
            return;
        }
    };

    // Find the smithay Output that matches this wl_output.
    let output = Output::from_resource(&wl_output);

    let output = match output {
        Some(o) => o,
        None => {
            // No matching output → create the frame but immediately fail it.
            warn!(
                "[screencopy] no matching output for wl_output {:?}",
                wl_output.id()
            );
            let frame_data = ScreencopyFrameData {
                output: None,
                region,
                overlay_cursor: overlay_cursor != 0,
                buffer_info: (0, 0, 0, wl_shm::Format::Argb8888),
                pending_queue: pending_queue.clone(),
                copy_requested: AtomicBool::new(false),
            };
            let frame = data_init.init(frame_new_id, frame_data);
            frame.failed();
            return;
        }
    };

    // Determine output dimensions. An enabled-but-modeless output cannot be
    // captured; init the frame so we can fail it instead of panicking.
    let mode = match output.current_mode() {
        Some(m) => m,
        None => {
            warn!("[screencopy] output {} has no current mode", output.name());
            let frame_data = ScreencopyFrameData {
                output: None,
                region,
                overlay_cursor: overlay_cursor != 0,
                buffer_info: (0, 0, 0, wl_shm::Format::Argb8888),
                pending_queue: pending_queue.clone(),
                copy_requested: AtomicBool::new(false),
            };
            let frame = data_init.init(frame_new_id, frame_data);
            frame.failed();
            return;
        }
    };
    let (out_w, out_h) = (mode.size.w as u32, mode.size.h as u32);

    // For region captures, use the region size; otherwise full output. The
    // protocol gives the region in output-logical coordinates, while the
    // capture is a physical mode-sized buffer, so it is mapped to buffer
    // pixels here once; the render drain then copies from the mapped
    // rectangle as-is. Region dimensions come from i32 wire fields, so the
    // mapping also rejects empty, negative and out-of-output boxes before any
    // `as u32` could wrap into huge stride/buffer-size math.
    let buffer_region = match region {
        Some(logical) => {
            let Some(buffer_region) = logical_region_to_buffer(
                logical,
                output.current_scale().fractional_scale(),
                output.current_transform(),
                (mode.size.w, mode.size.h),
            ) else {
                let (rx, ry, rw, rh) = logical;
                warn!(
                    "[screencopy] invalid logical region ({rx},{ry} {rw}x{rh}) for output {} ({out_w}x{out_h} buffer)",
                    output.name()
                );
                let frame_data = ScreencopyFrameData {
                    output: None,
                    region,
                    overlay_cursor: overlay_cursor != 0,
                    buffer_info: (0, 0, 0, wl_shm::Format::Argb8888),
                    pending_queue: pending_queue.clone(),
                    copy_requested: AtomicBool::new(false),
                };
                let frame = data_init.init(frame_new_id, frame_data);
                frame.failed();
                return;
            };
            Some(buffer_region)
        }
        None => None,
    };
    let (cap_w, cap_h) = match buffer_region {
        Some((_, _, width, height)) => (width as u32, height as u32),
        None => (out_w, out_h),
    };

    let stride = cap_w * 4; // ARGB8888 → 4 bytes per pixel

    let frame_data = ScreencopyFrameData {
        output: Some(output.clone()),
        region: buffer_region,
        overlay_cursor: overlay_cursor != 0,
        buffer_info: (cap_w, cap_h, stride, wl_shm::Format::Argb8888),
        pending_queue,
        copy_requested: AtomicBool::new(false),
    };

    let frame = data_init.init(frame_new_id, frame_data);

    // Send buffer info to the client.
    frame.buffer(wl_shm::Format::Argb8888, cap_w, cap_h, stride);

    // Advertise a dmabuf buffer option (v3+) for the zero-copy render path.
    // Only for full-output captures — region capture into dmabuf is unsupported.
    if frame.version() >= 3 {
        if region.is_none() {
            frame.linux_dmabuf(
                smithay::backend::allocator::Fourcc::Argb8888 as u32,
                cap_w,
                cap_h,
            );
        }
        // Signal that all buffer types have been enumerated (v3).
        frame.buffer_done();
    }

    debug!(
        "[screencopy] capture_output: output={} size={}x{} region={:?} buffer_region={:?}",
        output.name(),
        cap_w,
        cap_h,
        region,
        buffer_region,
    );
}

// ---- Frame dispatch (zwlr_screencopy_frame_v1) -----------------------------------

impl Dispatch<ZwlrScreencopyFrameV1, ScreencopyFrameData> for JwmWaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwlrScreencopyFrameV1,
        request: zwlr_screencopy_frame_v1::Request,
        data: &ScreencopyFrameData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_screencopy_frame_v1::Request::Copy { buffer } => {
                if queue_copy(state, resource, &buffer, data, false) {
                    state.needs_redraw = true;
                }
            }
            zwlr_screencopy_frame_v1::Request::CopyWithDamage { buffer } => {
                if queue_copy(state, resource, &buffer, data, true) {
                    state.needs_redraw = true;
                }
            }
            zwlr_screencopy_frame_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

fn queue_copy(
    state: &mut JwmWaylandState,
    frame: &ZwlrScreencopyFrameV1,
    buffer: &WlBuffer,
    data: &ScreencopyFrameData,
    with_damage: bool,
) -> bool {
    if !claim_copy_request(&data.copy_requested) {
        frame.post_error(
            zwlr_screencopy_frame_v1::Error::AlreadyUsed,
            "a screencopy frame accepts exactly one copy request",
        );
        return false;
    }

    let output = match data.output.as_ref() {
        Some(o) => o,
        None => {
            // Frame was created for an output that no longer exists; fail cleanly.
            let mut counters = state.capture_counters.lock_safe();
            counters.note_screencopy_failed("screencopy dispatch: missing output");
            frame.failed();
            return false;
        }
    };
    debug!(
        "[screencopy] copy request queued for output {}",
        output.name()
    );
    let mut queue = data.pending_queue.lock_safe();
    queue.push(PendingScreencopyFrame {
        frame: frame.clone(),
        buffer: buffer.clone(),
        output: output.clone(),
        region: data.region,
        overlay_cursor: data.overlay_cursor,
        with_damage,
    });
    let mut counters = state.capture_counters.lock_safe();
    counters.note_screencopy_queued();
    true
}

// ---- Initialization ---------------------------------------------------------------

/// Validate a region against the output bounds. Pure helper so it can be
/// unit-tested without standing up a wayland Display. Returns true iff the
/// region is fully inside [0, out_w) × [0, out_h) with positive dimensions.
pub(crate) fn region_is_valid(rx: i32, ry: i32, rw: i32, rh: i32, out_w: u32, out_h: u32) -> bool {
    if rw <= 0 || rh <= 0 || rx < 0 || ry < 0 {
        return false;
    }
    // i32 → u32 conversion is now safe (we just bounded everything ≥0).
    let (rx, ry, rw, rh) = (rx as u32, ry as u32, rw as u32, rh as u32);
    let Some(right) = rx.checked_add(rw) else {
        return false;
    };
    let Some(bottom) = ry.checked_add(rh) else {
        return false;
    };
    right <= out_w && bottom <= out_h
}

/// Map a `capture_output_region` box to the buffer-pixel rectangle the
/// capture copies from, or `None` when the box is empty or leaves the output.
///
/// The protocol gives the box in output-logical coordinates: the space
/// xdg-output advertises, i.e. the mode size divided by the output scale and
/// then rotated by the output transform. The capture itself is a physical
/// mode-sized buffer. Like wlroots, the box is validated against the
/// advertised logical size, scaled to physical pixels (rounded outward so a
/// fractional scale never drops an edge pixel) and then brought back into
/// buffer orientation by undoing the output transform. Treating the logical
/// box as raw buffer pixels captured a shrunken, shifted area on any output
/// whose scale is not 1.
pub(crate) fn logical_region_to_buffer(
    region: (i32, i32, i32, i32),
    scale: f64,
    transform: Transform,
    mode_size: (i32, i32),
) -> Option<(i32, i32, i32, i32)> {
    let (mode_w, mode_h) = mode_size;
    if mode_w <= 0 || mode_h <= 0 || !scale.is_finite() || scale <= 0.0 {
        return None;
    }
    // Physical output size in the client's (transformed) orientation.
    let (area_w, area_h) = if transform_swaps_axes(transform) {
        (mode_h, mode_w)
    } else {
        (mode_w, mode_h)
    };
    // Same rounding smithay uses for the advertised xdg-output logical size.
    let logical_w = (f64::from(area_w) / scale).round();
    let logical_h = (f64::from(area_h) / scale).round();
    if logical_w < 1.0 || logical_h < 1.0 {
        return None;
    }
    let (rx, ry, rw, rh) = region;
    if !region_is_valid(rx, ry, rw, rh, logical_w as u32, logical_h as u32) {
        return None;
    }

    // Edges are summed in i64: a scale below 1 makes the logical output
    // larger than the mode, so `rx + rw` is not bounded by an i32 there.
    let to_physical = |logical: i64, round: fn(f64) -> f64, limit: i32| -> i32 {
        (round(logical as f64 * scale) as i64).clamp(0, i64::from(limit)) as i32
    };
    let left = to_physical(i64::from(rx), f64::floor, area_w);
    let top = to_physical(i64::from(ry), f64::floor, area_h);
    let right = to_physical(i64::from(rx) + i64::from(rw), f64::ceil, area_w);
    let bottom = to_physical(i64::from(ry) + i64::from(rh), f64::ceil, area_h);
    if right <= left || bottom <= top {
        return None;
    }
    Some(untransform_box(
        (left, top, right - left, bottom - top),
        transform,
        (area_w, area_h),
    ))
}

fn transform_swaps_axes(transform: Transform) -> bool {
    matches!(
        transform,
        Transform::_90 | Transform::_270 | Transform::Flipped90 | Transform::Flipped270
    )
}

/// Undo `transform` for a box inside an `(area_w, area_h)` area given in the
/// transformed orientation. This is wlroots' `wlr_box_transform` applied with
/// the inverted output transform, because capture clients such as grim
/// rotate the returned buffer by the advertised `wl_output` transform and so
/// expect buffer orientation: rotations swap 90 and 270, while every flipped
/// transform is a reflection and therefore its own inverse. Smithay's
/// `Transform::invert` maps `Flipped90` to `Flipped270`, which does not match
/// that convention, so it is not used here.
fn untransform_box(
    (x, y, w, h): (i32, i32, i32, i32),
    transform: Transform,
    (area_w, area_h): (i32, i32),
) -> (i32, i32, i32, i32) {
    match transform {
        Transform::Normal => (x, y, w, h),
        Transform::_90 => (y, area_w - x - w, h, w),
        Transform::_180 => (area_w - x - w, area_h - y - h, w, h),
        Transform::_270 => (area_h - y - h, x, h, w),
        Transform::Flipped => (area_w - x - w, y, w, h),
        Transform::Flipped90 => (y, x, h, w),
        Transform::Flipped180 => (x, area_h - y - h, w, h),
        Transform::Flipped270 => (area_h - y - h, area_w - x - w, h, w),
    }
}

/// Create the zwlr_screencopy_manager_v1 global and return the shared pending queue.
pub fn init_screencopy_manager(dh: &DisplayHandle) -> PendingScreencopyQueue {
    let queue = new_pending_screencopy_queue();
    // Version 3 – includes buffer_done, linux_dmabuf, copy_with_damage.
    dh.create_global::<JwmWaylandState, ZwlrScreencopyManagerV1, _>(3, ());
    info!("[screencopy] zwlr_screencopy_manager_v1 global created (v3)");
    queue
}

#[cfg(test)]
mod tests {
    use super::{claim_copy_request, logical_region_to_buffer, region_is_valid};
    use smithay::utils::Transform;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn a_frame_accepts_exactly_one_copy_request_under_a_flood() {
        let copy_requested = AtomicBool::new(false);
        assert!(claim_copy_request(&copy_requested));
        for _ in 0..10_000 {
            assert!(!claim_copy_request(&copy_requested));
        }
    }

    #[test]
    fn negative_dims_are_rejected() {
        assert!(!region_is_valid(0, 0, -1, 100, 1920, 1080));
        assert!(!region_is_valid(0, 0, 100, -1, 1920, 1080));
    }

    #[test]
    fn zero_dims_are_rejected() {
        assert!(!region_is_valid(0, 0, 0, 100, 1920, 1080));
        assert!(!region_is_valid(0, 0, 100, 0, 1920, 1080));
    }

    #[test]
    fn negative_origin_rejected() {
        // Per spec the region coords are in output logical pixels — negative
        // origins make no sense and would alias to huge u32 if we cast.
        assert!(!region_is_valid(-1, 0, 10, 10, 1920, 1080));
        assert!(!region_is_valid(0, -1, 10, 10, 1920, 1080));
    }

    #[test]
    fn region_extending_past_output_rejected() {
        assert!(!region_is_valid(1900, 0, 100, 100, 1920, 1080));
        assert!(!region_is_valid(0, 1000, 100, 100, 1920, 1080));
    }

    #[test]
    fn region_flush_with_output_edges_accepted() {
        assert!(region_is_valid(0, 0, 1920, 1080, 1920, 1080));
        assert!(region_is_valid(1820, 980, 100, 100, 1920, 1080));
    }

    #[test]
    fn region_inside_output_accepted() {
        assert!(region_is_valid(100, 100, 800, 600, 1920, 1080));
    }

    #[test]
    fn overflow_in_right_edge_rejected() {
        // rx + rw overflows u32 — must be caught, not silently wrapped.
        assert!(!region_is_valid(i32::MAX, 0, i32::MAX, 100, 1920, 1080));
    }

    const FHD: (i32, i32) = (1920, 1080);

    #[test]
    fn unscaled_normal_output_maps_regions_one_to_one() {
        for region in [
            (0, 0, 1920, 1080),
            (100, 100, 800, 600),
            (1820, 980, 100, 100),
        ] {
            assert_eq!(
                logical_region_to_buffer(region, 1.0, Transform::Normal, FHD),
                Some(region)
            );
        }
    }

    #[test]
    fn scaled_output_regions_are_logical_not_buffer_pixels() {
        // Regression: a 1920x1080 panel at scale 2 is a 960x540 logical
        // output. The lower-right quarter used to be copied from buffer
        // pixels (480,270)..(960,540) at half size, and the whole output
        // captured only its top-left quarter.
        assert_eq!(
            logical_region_to_buffer((480, 270, 480, 270), 2.0, Transform::Normal, FHD),
            Some((960, 540, 960, 540))
        );
        assert_eq!(
            logical_region_to_buffer((0, 0, 960, 540), 2.0, Transform::Normal, FHD),
            Some((0, 0, 1920, 1080))
        );
        // Anything past the logical edge is outside the output, even though
        // it would still fit inside the physical mode.
        assert_eq!(
            logical_region_to_buffer((960, 540, 960, 540), 2.0, Transform::Normal, FHD),
            None
        );
    }

    #[test]
    fn fractional_scale_rounds_the_buffer_region_outward() {
        assert_eq!(
            logical_region_to_buffer((1, 1, 3, 3), 1.5, Transform::Normal, FHD),
            Some((1, 1, 5, 5))
        );
        // 1366 / 1.5 = 910.67 is advertised as 911; the full logical output
        // must still map onto exactly the physical mode.
        assert_eq!(
            logical_region_to_buffer((0, 0, 911, 512), 1.5, Transform::Normal, (1366, 768)),
            Some((0, 0, 1366, 768))
        );
    }

    #[test]
    fn rotated_output_regions_are_mapped_back_into_buffer_orientation() {
        // Transform 90 advertises a 1080x1920 logical output. This region
        // used to be rejected because y + h exceeded the 1080 buffer height.
        assert_eq!(
            logical_region_to_buffer((0, 1500, 100, 100), 1.0, Transform::_90, FHD),
            Some((1500, 980, 100, 100))
        );
        assert_eq!(
            logical_region_to_buffer((0, 0, 1080, 1920), 1.0, Transform::_90, FHD),
            Some((0, 0, 1920, 1080))
        );
        assert_eq!(
            logical_region_to_buffer((0, 0, 100, 50), 1.0, Transform::_270, FHD),
            Some((1870, 0, 50, 100))
        );
        assert_eq!(
            logical_region_to_buffer((0, 0, 100, 50), 1.0, Transform::_180, FHD),
            Some((1820, 1030, 100, 50))
        );
        assert_eq!(
            logical_region_to_buffer((10, 20, 30, 40), 2.0, Transform::_90, FHD),
            Some((40, 1000, 80, 60))
        );
    }

    #[test]
    fn flipped_transforms_are_their_own_inverse() {
        // Every flipped transform is a reflection: applying the mapping to
        // the buffer box again (now inside the mode-sized buffer) must give
        // the logical box back.
        for transform in [
            Transform::Flipped,
            Transform::Flipped90,
            Transform::Flipped180,
            Transform::Flipped270,
        ] {
            let logical = (10, 20, 30, 40);
            let buffer = logical_region_to_buffer(logical, 1.0, transform, FHD)
                .expect("region inside the flipped output");
            assert!(
                buffer.0 >= 0
                    && buffer.1 >= 0
                    && buffer.0 + buffer.2 <= FHD.0
                    && buffer.1 + buffer.3 <= FHD.1,
                "{transform:?} produced {buffer:?}, outside the buffer"
            );
            assert_eq!(
                super::untransform_box(buffer, transform, FHD),
                logical,
                "{transform:?} is not its own inverse"
            );
        }
    }

    #[test]
    fn invalid_regions_and_scales_are_rejected() {
        for region in [
            (0, 0, 0, 10),
            (0, 0, 10, -1),
            (-1, 0, 10, 10),
            (i32::MAX, 0, i32::MAX, 10),
        ] {
            assert_eq!(
                logical_region_to_buffer(region, 1.0, Transform::Normal, FHD),
                None
            );
        }
        for scale in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(
                logical_region_to_buffer((0, 0, 10, 10), scale, Transform::Normal, FHD),
                None
            );
        }
        assert_eq!(
            logical_region_to_buffer((0, 0, 10, 10), 1.0, Transform::Normal, (0, 1080)),
            None
        );
        // A scale below 1 enlarges the logical output past i32 edge sums.
        assert_eq!(
            logical_region_to_buffer(
                (i32::MAX - 1, 0, i32::MAX - 1, 1),
                0.001,
                Transform::Normal,
                (i32::MAX, 1080)
            ),
            Some((2_147_483, 0, 2_147_485, 1))
        );
    }
}
