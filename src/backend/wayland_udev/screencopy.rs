/// wlr-screencopy-unstable-v1 protocol implementation for JWM.
///
/// This allows clients like `grim` to request screen content from the compositor.
/// The compositor captures the framebuffer during the render loop and copies the data
/// into the client-provided wl_shm buffer.
use crate::sync_ext::MutexExt;
use std::sync::{Arc, Mutex};

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

// IMPORTANT: Use the canonical path that matches Display<JwmWaylandState> in backend.rs.
// Do NOT use `super::state::JwmWaylandState` — that's a different type due to the #[path]
// attribute in wayland.rs re-exporting the same file under a different module path.
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
    /// Optional sub-region (x, y, width, height) in output logical coords.
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
    pub region: Option<(i32, i32, i32, i32)>,
    pub overlay_cursor: bool,
    pub buffer_info: (u32, u32, u32, wl_shm::Format), // (width, height, stride, format)
    pub pending_queue: PendingScreencopyQueue,
}

// ---- Manager global (zwlr_screencopy_manager_v1) ----------------------------------

// Use () as global data - access pending_queue from JwmWaylandState.screencopy_pending
impl GlobalDispatch<ZwlrScreencopyManagerV1, ()> for JwmWaylandState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrScreencopyManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
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
            };
            let frame = data_init.init(frame_new_id, frame_data);
            frame.failed();
            return;
        }
    };
    let (out_w, out_h) = (mode.size.w as u32, mode.size.h as u32);

    // For region captures, use the region size; otherwise full output. Region
    // dimensions come from i32 wire fields — without validation, `as u32` on a
    // negative number wraps to ~2^32 and the resulting stride/buffer-size math
    // overflows. Reject zero/negative dims and require the region to lie within
    // the output rectangle.
    let (cap_w, cap_h) = if let Some((rx, ry, rw, rh)) = region {
        if !region_is_valid(rx, ry, rw, rh, out_w, out_h) {
            warn!(
                "[screencopy] invalid region ({rx},{ry} {rw}x{rh}) for output {} ({out_w}x{out_h})",
                output.name()
            );
            let frame_data = ScreencopyFrameData {
                output: None,
                region,
                overlay_cursor: overlay_cursor != 0,
                buffer_info: (0, 0, 0, wl_shm::Format::Argb8888),
                pending_queue: pending_queue.clone(),
            };
            let frame = data_init.init(frame_new_id, frame_data);
            frame.failed();
            return;
        }
        (rw as u32, rh as u32)
    } else {
        (out_w, out_h)
    };

    let stride = cap_w * 4; // ARGB8888 → 4 bytes per pixel

    let frame_data = ScreencopyFrameData {
        output: Some(output.clone()),
        region,
        overlay_cursor: overlay_cursor != 0,
        buffer_info: (cap_w, cap_h, stride, wl_shm::Format::Argb8888),
        pending_queue,
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
        "[screencopy] capture_output: output={} size={}x{} region={:?}",
        output.name(),
        cap_w,
        cap_h,
        region,
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
                queue_copy(resource, &buffer, data, false);
                state.needs_redraw = true;
            }
            zwlr_screencopy_frame_v1::Request::CopyWithDamage { buffer } => {
                queue_copy(resource, &buffer, data, true);
                state.needs_redraw = true;
            }
            zwlr_screencopy_frame_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

fn queue_copy(
    frame: &ZwlrScreencopyFrameV1,
    buffer: &WlBuffer,
    data: &ScreencopyFrameData,
    with_damage: bool,
) {
    let output = match data.output.as_ref() {
        Some(o) => o,
        None => {
            // Frame was created for an output that no longer exists; fail cleanly.
            frame.failed();
            return;
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
    use super::region_is_valid;

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
}
