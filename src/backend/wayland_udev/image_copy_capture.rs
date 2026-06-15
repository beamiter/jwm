/// ext-image-copy-capture-v1 + ext-image-capture-source-v1 protocol implementation for JWM.
///
/// Replaces the deprecated wlr-screencopy protocol. Allows modern screen capture
/// tools (OBS, portals, grim v2) to capture output and toplevel content.

use crate::sync_ext::MutexExt;
use std::sync::{Arc, Mutex};

use log::{debug, info, warn};

use smithay::output::Output;
use smithay::reexports::wayland_protocols::ext::image_capture_source::v1::server::{
    ext_image_capture_source_v1::{self, ExtImageCaptureSourceV1},
    ext_output_image_capture_source_manager_v1::{
        self, ExtOutputImageCaptureSourceManagerV1,
    },
};
use smithay::reexports::wayland_protocols::ext::image_copy_capture::v1::server::{
    ext_image_copy_capture_manager_v1::{self, ExtImageCopyCaptureManagerV1},
    ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
    ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
    ext_image_copy_capture_cursor_session_v1::{self, ExtImageCopyCaptureCursorSessionV1},
};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};

use crate::backend::wayland::state::JwmWaylandState;

// --- Source types ---

#[derive(Clone)]
pub enum CaptureSource {
    Output(Output),
    Toplevel(crate::backend::common_define::WindowId),
}

pub struct ImageCaptureSourceData {
    pub source: CaptureSource,
}
unsafe impl Send for ImageCaptureSourceData {}

pub struct OutputSourceManagerData;
unsafe impl Send for OutputSourceManagerData {}

pub struct ToplevelSourceManagerData;
unsafe impl Send for ToplevelSourceManagerData {}

pub struct CaptureManagerData;
unsafe impl Send for CaptureManagerData {}

pub struct CaptureSessionData {
    pub source: CaptureSource,
    pub paint_cursors: bool,
}
unsafe impl Send for CaptureSessionData {}

pub struct CaptureFrameData {
    pub source: CaptureSource,
    pub paint_cursors: bool,
    // Buffer/damage are stashed here on attach_buffer/damage_buffer and only
    // moved into the pending queue on the capture request, matching the
    // protocol's attach → damage* → capture ordering.
    pub buffer: Mutex<Option<WlBuffer>>,
    pub damage: Mutex<Vec<(i32, i32, i32, i32)>>,
    pub pending_queue: PendingImageCaptureQueue,
}
unsafe impl Send for CaptureFrameData {}

pub struct CursorSessionData {
    pub source: CaptureSource,
}
unsafe impl Send for CursorSessionData {}

// --- Pending capture queue (drained during render) ---

pub struct PendingImageCapture {
    pub frame: ExtImageCopyCaptureFrameV1,
    pub buffer: WlBuffer,
    pub source: CaptureSource,
    pub paint_cursors: bool,
    pub damage: Vec<(i32, i32, i32, i32)>,
}
unsafe impl Send for PendingImageCapture {}

pub type PendingImageCaptureQueue = Arc<Mutex<Vec<PendingImageCapture>>>;

pub fn new_pending_image_capture_queue() -> PendingImageCaptureQueue {
    Arc::new(Mutex::new(Vec::new()))
}

/// Initialize the ext-image-copy-capture globals.
pub fn init_image_copy_capture(dh: &DisplayHandle) -> PendingImageCaptureQueue {
    dh.create_global::<JwmWaylandState, ExtOutputImageCaptureSourceManagerV1, _>(
        1,
        OutputSourceManagerData,
    );
    dh.create_global::<JwmWaylandState, ExtImageCopyCaptureManagerV1, _>(1, CaptureManagerData);
    info!("[udev/wayland] ext-image-copy-capture-v1 + ext-image-capture-source-v1 globals registered");
    new_pending_image_capture_queue()
}

// =============================================================================
// ext_output_image_capture_source_manager_v1
// =============================================================================

impl GlobalDispatch<ExtOutputImageCaptureSourceManagerV1, OutputSourceManagerData>
    for JwmWaylandState
{
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ExtOutputImageCaptureSourceManagerV1>,
        _global_data: &OutputSourceManagerData,
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, OutputSourceManagerData);
    }
}

impl Dispatch<ExtOutputImageCaptureSourceManagerV1, OutputSourceManagerData> for JwmWaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ExtOutputImageCaptureSourceManagerV1,
        request: ext_output_image_capture_source_manager_v1::Request,
        _data: &OutputSourceManagerData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            ext_output_image_capture_source_manager_v1::Request::CreateSource {
                source,
                output: wl_output,
            } => {
                let output = Output::from_resource(&wl_output);
                let capture_source = match output {
                    Some(o) => CaptureSource::Output(o),
                    None => {
                        let fallback = state.outputs.first().cloned();
                        match fallback {
                            Some(o) => CaptureSource::Output(o),
                            None => {
                                warn!("[image-capture] no output available for source creation");
                                return;
                            }
                        }
                    }
                };
                data_init.init(source, ImageCaptureSourceData { source: capture_source });
            }
            ext_output_image_capture_source_manager_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

// =============================================================================
// ext_image_capture_source_v1 (opaque source handle)
// =============================================================================

impl Dispatch<ExtImageCaptureSourceV1, ImageCaptureSourceData> for JwmWaylandState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ExtImageCaptureSourceV1,
        request: ext_image_capture_source_v1::Request,
        _data: &ImageCaptureSourceData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            ext_image_capture_source_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

// =============================================================================
// ext_image_copy_capture_manager_v1
// =============================================================================

impl GlobalDispatch<ExtImageCopyCaptureManagerV1, CaptureManagerData> for JwmWaylandState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ExtImageCopyCaptureManagerV1>,
        _global_data: &CaptureManagerData,
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, CaptureManagerData);
    }
}

impl Dispatch<ExtImageCopyCaptureManagerV1, CaptureManagerData> for JwmWaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ExtImageCopyCaptureManagerV1,
        request: ext_image_copy_capture_manager_v1::Request,
        _data: &CaptureManagerData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            ext_image_copy_capture_manager_v1::Request::CreateSession {
                session,
                source,
                options,
            } => {
                let capture_source = match source
                    .data::<ImageCaptureSourceData>()
                    .map(|d| d.source.clone())
                    .or_else(|| state.outputs.first().map(|o| CaptureSource::Output(o.clone())))
                {
                    Some(s) => s,
                    None => {
                        warn!("[image-capture] CreateSession with no source and no outputs; ignoring");
                        return;
                    }
                };

                let paint_cursors = options
                    .into_result()
                    .map(|o| o.contains(ext_image_copy_capture_manager_v1::Options::PaintCursors))
                    .unwrap_or(false);

                let sess = data_init.init(
                    session,
                    CaptureSessionData {
                        source: capture_source.clone(),
                        paint_cursors,
                    },
                );

                // Send buffer constraints to client.
                match &capture_source {
                    CaptureSource::Output(output) => {
                        if let Some(mode) = output.current_mode() {
                            let (w, h) = (mode.size.w as u32, mode.size.h as u32);
                            sess.buffer_size(w, h);
                            sess.shm_format(wl_shm::Format::Argb8888);
                            sess.shm_format(wl_shm::Format::Xrgb8888);
                            sess.done();
                            debug!("[image-capture] session created: output={} size={}x{}", output.name(), w, h);
                        } else {
                            sess.stopped();
                        }
                    }
                    CaptureSource::Toplevel(win) => {
                        match state.window_geometry.get(win) {
                            Some(geo) if geo.w > 0 && geo.h > 0 => {
                                sess.buffer_size(geo.w, geo.h);
                                sess.shm_format(wl_shm::Format::Argb8888);
                                sess.shm_format(wl_shm::Format::Xrgb8888);
                                sess.done();
                                debug!(
                                    "[image-capture] session created: toplevel={win:?} size={}x{}",
                                    geo.w, geo.h
                                );
                            }
                            _ => sess.stopped(),
                        }
                    }
                }
            }
            ext_image_copy_capture_manager_v1::Request::CreatePointerCursorSession {
                session,
                source,
                pointer: _,
            } => {
                let capture_source = match source
                    .data::<ImageCaptureSourceData>()
                    .map(|d| d.source.clone())
                    .or_else(|| state.outputs.first().map(|o| CaptureSource::Output(o.clone())))
                {
                    Some(s) => s,
                    None => {
                        warn!("[image-capture] CreatePointerCursorSession with no source and no outputs; ignoring");
                        return;
                    }
                };

                data_init.init(session, CursorSessionData { source: capture_source });
            }
            ext_image_copy_capture_manager_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

// =============================================================================
// ext_image_copy_capture_session_v1
// =============================================================================

impl Dispatch<ExtImageCopyCaptureSessionV1, CaptureSessionData> for JwmWaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ExtImageCopyCaptureSessionV1,
        request: ext_image_copy_capture_session_v1::Request,
        data: &CaptureSessionData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            ext_image_copy_capture_session_v1::Request::CreateFrame { frame } => {
                let pending_queue = state
                    .image_capture_pending
                    .clone()
                    .unwrap_or_else(new_pending_image_capture_queue);

                data_init.init(
                    frame,
                    CaptureFrameData {
                        source: data.source.clone(),
                        paint_cursors: data.paint_cursors,
                        buffer: Mutex::new(None),
                        damage: Mutex::new(Vec::new()),
                        pending_queue,
                    },
                );
            }
            ext_image_copy_capture_session_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

// =============================================================================
// ext_image_copy_capture_frame_v1
// =============================================================================

impl Dispatch<ExtImageCopyCaptureFrameV1, CaptureFrameData> for JwmWaylandState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        resource: &ExtImageCopyCaptureFrameV1,
        request: ext_image_copy_capture_frame_v1::Request,
        data: &CaptureFrameData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            ext_image_copy_capture_frame_v1::Request::AttachBuffer { buffer } => {
                // Stash until capture; the new buffer replaces any previous one.
                *data.buffer.lock_safe() = Some(buffer);
            }
            ext_image_copy_capture_frame_v1::Request::DamageBuffer {
                x,
                y,
                width,
                height,
            } => {
                if x >= 0 && y >= 0 && width > 0 && height > 0 {
                    data.damage.lock_safe().push((x, y, width, height));
                }
            }
            ext_image_copy_capture_frame_v1::Request::Capture => {
                // Move the attached buffer into the pending queue; the render
                // loop fulfills it on the next frame for the source output.
                let buffer = data.buffer.lock_safe().take();
                match buffer {
                    Some(buffer) => {
                        let damage = std::mem::take(&mut *data.damage.lock_safe());
                        data.pending_queue.lock_safe().push(PendingImageCapture {
                            frame: resource.clone(),
                            buffer,
                            source: data.source.clone(),
                            paint_cursors: data.paint_cursors,
                            damage,
                        });
                        debug!("[image-capture] frame capture queued");
                    }
                    None => {
                        // capture without attach_buffer: fail rather than hang.
                        resource.failed(ext_image_copy_capture_frame_v1::FailureReason::Unknown);
                    }
                }
            }
            ext_image_copy_capture_frame_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

// =============================================================================
// ext_image_copy_capture_cursor_session_v1
// =============================================================================

impl Dispatch<ExtImageCopyCaptureCursorSessionV1, CursorSessionData> for JwmWaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ExtImageCopyCaptureCursorSessionV1,
        request: ext_image_copy_capture_cursor_session_v1::Request,
        data: &CursorSessionData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            ext_image_copy_capture_cursor_session_v1::Request::GetCaptureSession { session } => {
                // Create a sub-session for cursor capture.
                let capture_source = data.source.clone();
                let sess = data_init.init(
                    session,
                    CaptureSessionData {
                        source: capture_source.clone(),
                        paint_cursors: true,
                    },
                );

                // A capture session is unusable until the client receives buffer
                // constraints followed by `done`; without them a cursor-capture
                // client stalls forever. Mirror the regular-session sizing so the
                // client's buffer matches what the copy path writes.
                match &capture_source {
                    CaptureSource::Output(output) => {
                        if let Some(mode) = output.current_mode() {
                            sess.buffer_size(mode.size.w as u32, mode.size.h as u32);
                            sess.shm_format(wl_shm::Format::Argb8888);
                            sess.shm_format(wl_shm::Format::Xrgb8888);
                            sess.done();
                        } else {
                            sess.stopped();
                        }
                    }
                    CaptureSource::Toplevel(win) => match state.window_geometry.get(win) {
                        Some(geo) if geo.w > 0 && geo.h > 0 => {
                            sess.buffer_size(geo.w, geo.h);
                            sess.shm_format(wl_shm::Format::Argb8888);
                            sess.shm_format(wl_shm::Format::Xrgb8888);
                            sess.done();
                        }
                        _ => sess.stopped(),
                    },
                }
            }
            ext_image_copy_capture_cursor_session_v1::Request::Destroy => {}
            _ => {}
        }
    }
}
