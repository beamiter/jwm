/// ext-image-copy-capture-v1 + ext-image-capture-source-v1 protocol implementation for JWM.
///
/// Replaces the deprecated wlr-screencopy protocol. Allows modern screen capture
/// tools (OBS, portals, grim v2) to capture output and toplevel content.
use crate::sync_ext::MutexExt;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use log::{debug, info, warn};

use smithay::output::Output;
use smithay::reexports::wayland_protocols::ext::image_capture_source::v1::server::{
    ext_foreign_toplevel_image_capture_source_manager_v1::{
        self, ExtForeignToplevelImageCaptureSourceManagerV1,
    },
    ext_image_capture_source_v1::{self, ExtImageCaptureSourceV1},
    ext_output_image_capture_source_manager_v1::{self, ExtOutputImageCaptureSourceManagerV1},
};
use smithay::reexports::wayland_protocols::ext::image_copy_capture::v1::server::{
    ext_image_copy_capture_cursor_session_v1::{self, ExtImageCopyCaptureCursorSessionV1},
    ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
    ext_image_copy_capture_manager_v1::{self, ExtImageCopyCaptureManagerV1},
    ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
};
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, Weak,
};
use smithay::wayland::foreign_toplevel_list::ForeignToplevelHandle;

use crate::backend::common_define::WindowId;
use crate::backend::wayland::state::JwmWaylandState;

type CaptureDamageRect = (i32, i32, i32, i32);

// A client may send damage_buffer any number of times before capture. Keep the
// request-side state bounded; once the precise list is full, replace it with a
// bounding rectangle (or full-buffer damage when that rectangle cannot be
// represented by the protocol's signed coordinates).
const MAX_DAMAGE_RECTS_PER_FRAME: usize = 64;
const FULL_BUFFER_DAMAGE: CaptureDamageRect = (0, 0, i32::MAX, i32::MAX);

fn record_damage_rect(damage: &mut Vec<CaptureDamageRect>, rect: CaptureDamageRect) -> bool {
    let (x, y, width, height) = rect;
    if x < 0 || y < 0 || width <= 0 || height <= 0 {
        return false;
    }

    // A rectangle may satisfy the protocol's per-field constraints while its
    // far edge lies beyond every representable buffer. Preserve the valid
    // in-buffer portion without exposing an overflowing endpoint downstream.
    if i64::from(x) + i64::from(width) > i64::from(i32::MAX)
        || i64::from(y) + i64::from(height) > i64::from(i32::MAX)
    {
        damage.clear();
        damage.push(FULL_BUFFER_DAMAGE);
        return true;
    }

    // Once all addressable buffer pixels are damaged, later rectangles cannot
    // add useful information.
    if damage.as_slice() == [FULL_BUFFER_DAMAGE] {
        return true;
    }

    if damage.len() < MAX_DAMAGE_RECTS_PER_FRAME {
        damage.push(rect);
        return true;
    }

    // Work in i64 because each individually valid i32 rectangle may extend
    // beyond i32::MAX when x + width (or y + height) is evaluated.
    let mut min_x = i64::MAX;
    let mut min_y = i64::MAX;
    let mut max_x = 0_i64;
    let mut max_y = 0_i64;
    for &(x, y, width, height) in damage.iter().chain(std::iter::once(&rect)) {
        let x = i64::from(x);
        let y = i64::from(y);
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x + i64::from(width));
        max_y = max_y.max(y + i64::from(height));
    }

    let merged_width = max_x - min_x;
    let merged_height = max_y - min_y;
    let merged = match (
        i32::try_from(min_x),
        i32::try_from(min_y),
        i32::try_from(merged_width),
        i32::try_from(merged_height),
    ) {
        (Ok(x), Ok(y), Ok(width), Ok(height)) => (x, y, width, height),
        // wl_shm and linux-dmabuf buffer dimensions are signed protocol ints,
        // so this safely over-damages every pixel an attached buffer can have.
        _ => FULL_BUFFER_DAMAGE,
    };

    damage.clear();
    damage.push(merged);
    true
}

// --- Source types ---

#[derive(Clone)]
pub enum CaptureSource {
    Output(Output),
    Toplevel(WindowId),
}

pub struct ImageCaptureSourceData {
    /// What this source captures. `None` marks an inert source: the client
    /// named an output or toplevel that no longer exists (for example a
    /// closed window's stale foreign-toplevel handle, or an unplugged
    /// output's wl_output). Every session created from it is stopped at
    /// once. Substituting another output instead would hand a client that
    /// asked for one window the whole screen.
    pub source: Option<CaptureSource>,
}
unsafe impl Send for ImageCaptureSourceData {}

pub struct OutputSourceManagerData;
unsafe impl Send for OutputSourceManagerData {}

pub struct ToplevelSourceManagerData;
unsafe impl Send for ToplevelSourceManagerData {}

pub struct CaptureManagerData;
unsafe impl Send for CaptureManagerData {}

pub struct CaptureSessionData {
    /// `None` for a session on an inert source; it was sent `stopped`.
    pub source: Option<CaptureSource>,
    pub paint_cursors: bool,
    /// Set once `stopped` was sent. The event ends the session, so it goes
    /// out at most once, and every frame created afterwards fails.
    pub stopped: AtomicBool,
}
unsafe impl Send for CaptureSessionData {}

impl CaptureSessionData {
    fn new(source: Option<CaptureSource>, paint_cursors: bool) -> Self {
        Self {
            source,
            paint_cursors,
            stopped: AtomicBool::new(false),
        }
    }
}

pub struct CaptureFrameData {
    /// `None` for a frame of a stopped session; capturing it fails.
    pub source: Option<CaptureSource>,
    /// The session that created this frame, stopped once the frame shows
    /// that the session's source (output or toplevel) is gone.
    pub session: Weak<ExtImageCopyCaptureSessionV1>,
    pub paint_cursors: bool,
    // Buffer/damage are stashed here on attach_buffer/damage_buffer and only
    // moved into the pending queue on the capture request, matching the
    // protocol's attach → damage* → capture ordering.
    pub buffer: Mutex<Option<WlBuffer>>,
    pub damage: Mutex<Vec<CaptureDamageRect>>,
    pub pending_queue: PendingImageCaptureQueue,
}
unsafe impl Send for CaptureFrameData {}

pub struct CursorSessionData {
    /// `None` for a cursor session on an inert source.
    pub source: Option<CaptureSource>,
}
unsafe impl Send for CursorSessionData {}

// --- Pending capture queue (drained during render) ---

pub struct PendingImageCapture {
    pub frame: ExtImageCopyCaptureFrameV1,
    pub buffer: WlBuffer,
    pub source: CaptureSource,
    pub paint_cursors: bool,
    pub damage: Vec<CaptureDamageRect>,
}
unsafe impl Send for PendingImageCapture {}

pub type PendingImageCaptureQueue = Arc<Mutex<Vec<PendingImageCapture>>>;

pub fn new_pending_image_capture_queue() -> PendingImageCaptureQueue {
    Arc::new(Mutex::new(Vec::new()))
}

// --- Session lifetime ---

/// Send `stopped` on `session` unless it was already sent. Returns whether
/// it was sent now.
fn stop_session(session: &ExtImageCopyCaptureSessionV1) -> bool {
    let Some(data) = session.data::<CaptureSessionData>() else {
        return false;
    };
    if data.stopped.swap(true, Ordering::Relaxed) {
        return false;
    }
    session.stopped();
    true
}

/// Whether `source` is gone for good, so its sessions are over:
/// - an output that is no longer one of the backend's outputs (unplugged, or
///   replaced by a KMS rebuild), which nothing renders again;
/// - a toplevel without a `window_geometry` entry, which is the liveness key
///   `send_session_constraints` and the KMS fulfilment already use. The entry
///   leaves only when the window is destroyed (an X11 window also loses it on
///   unmap, but gets a fresh `WindowId` on its next map), so a missing entry
///   never comes back for this id.
fn source_is_gone(state: &JwmWaylandState, source: &CaptureSource) -> bool {
    match source {
        CaptureSource::Output(output) => !state.outputs.contains(output),
        CaptureSource::Toplevel(win) => !state.window_geometry.contains_key(win),
    }
}

/// Live capture sessions of one output, kept in the output's user data so
/// whoever removes the output can stop them without a registry of its own.
#[derive(Default)]
struct OutputCaptureSessions(Mutex<Vec<Weak<ExtImageCopyCaptureSessionV1>>>);

/// Live capture sessions of toplevels, by window, kept on the Wayland state
/// so every path that retires a window can stop them (see
/// [`stop_toplevel_capture_sessions`]). Unlike an `Output`, a `WindowId`
/// has no user data to hold them.
pub(crate) type ToplevelCaptureSessions =
    HashMap<WindowId, Vec<Weak<ExtImageCopyCaptureSessionV1>>>;

/// Remember a new session under its source, so the source's removal can stop
/// it while it is idle.
fn track_session(
    state: &mut JwmWaylandState,
    session: &ExtImageCopyCaptureSessionV1,
    source: Option<&CaptureSource>,
) {
    match source {
        Some(CaptureSource::Output(output)) => {
            output
                .user_data()
                .insert_if_missing_threadsafe(OutputCaptureSessions::default);
            if let Some(sessions) = output.user_data().get::<OutputCaptureSessions>() {
                let mut sessions = sessions.0.lock_safe();
                // Destroyed sessions leave dead entries; drop them as new ones come.
                sessions.retain(Weak::is_alive);
                sessions.push(session.downgrade());
            }
        }
        // Only a live window: a session on a closed one is stopped at
        // creation, and nothing would ever remove its entry.
        Some(CaptureSource::Toplevel(win)) if state.window_geometry.contains_key(win) => {
            let sessions = state.toplevel_capture_sessions.entry(*win).or_default();
            sessions.retain(Weak::is_alive);
            sessions.push(session.downgrade());
        }
        Some(CaptureSource::Toplevel(_)) | None => {}
    }
}

/// Stop every capture session of `output`, which was unplugged or replaced
/// by a KMS rebuild and will not be rendered again. The protocol pairs the
/// session's `stopped` with the `failed(stopped)` its queued frames get;
/// frames the client creates afterwards fail the same way. Returns whether
/// any event was sent, so the caller can flush clients.
///
/// The udev backend's `sync_wayland_state_from_kms`, the one place a running
/// backend replaces `state.outputs`, calls it for every output that leaves
/// them; the nested backends publish their single output once at startup.
/// Without that call an idle session (no frame in flight) would learn it
/// only on its next `create_frame`, which the lazy checks still catch.
pub fn stop_output_capture_sessions(output: &Output) -> bool {
    let Some(sessions) = output.user_data().get::<OutputCaptureSessions>() else {
        return false;
    };
    let sessions = std::mem::take(&mut *sessions.0.lock_safe());
    let mut sent = false;
    for session in sessions.iter().filter_map(|weak| weak.upgrade().ok()) {
        sent |= stop_session(&session);
    }
    if sent {
        debug!(
            "[image-capture] stopped the capture sessions of removed output {}",
            output.name()
        );
    }
    sent
}

/// Stop every capture session of `win`, a window that was just retired: a
/// closed Wayland window, or an X11 window that was unmapped or destroyed
/// (its next map gets a fresh `WindowId`). The protocol sends `stopped` when
/// the source goes away; without this call a paused stream (no frame in
/// flight) would keep showing a frozen window until its next `create_frame`,
/// which the lazy checks still catch. Returns whether any event was sent, so
/// the caller can flush clients.
///
/// The caller drops the window's `window_geometry` first, so frames created
/// or captured afterwards fail through `source_is_gone`.
pub(crate) fn stop_toplevel_capture_sessions(state: &mut JwmWaylandState, win: WindowId) -> bool {
    let Some(sessions) = state.toplevel_capture_sessions.remove(&win) else {
        return false;
    };
    let mut sent = false;
    for session in sessions.iter().filter_map(|weak| weak.upgrade().ok()) {
        sent |= stop_session(&session);
    }
    if sent {
        debug!("[image-capture] stopped the capture sessions of closed toplevel {win:?}");
    }
    sent
}

/// Advertise dmabuf capture support on a session, emitting `dmabuf_device`
/// followed by one `dmabuf_format` per supported fourcc with its modifier list.
/// No-op when the compositor has no render device/formats (clients then use shm).
/// Must be called after the shm_format events and before `done()`.
fn advertise_session_dmabuf(sess: &ExtImageCopyCaptureSessionV1, state: &JwmWaylandState) {
    let Some(dev) = state.dmabuf_main_device else {
        return;
    };
    if state.dmabuf_render_formats.is_empty() {
        return;
    }
    sess.dmabuf_device(dev.to_ne_bytes().to_vec());
    for code in [
        smithay::backend::allocator::Fourcc::Argb8888,
        smithay::backend::allocator::Fourcc::Xrgb8888,
    ] {
        let mods: Vec<u8> = state
            .dmabuf_render_formats
            .iter()
            .filter(|f| f.code == code)
            .flat_map(|f| u64::from(f.modifier).to_ne_bytes())
            .collect();
        if !mods.is_empty() {
            sess.dmabuf_format(code as u32, mods);
        }
    }
}

/// Send a new session's buffer constraints followed by `done`, or `stopped`
/// when there is nothing to capture: an inert source, an output without a
/// current mode or no longer among the backend's outputs, or a toplevel that
/// is gone or has no size yet.
fn send_session_constraints(
    sess: &ExtImageCopyCaptureSessionV1,
    state: &JwmWaylandState,
    source: Option<&CaptureSource>,
) {
    let size = match source {
        Some(CaptureSource::Output(output)) if state.outputs.contains(output) => output
            .current_mode()
            .map(|mode| (mode.size.w as u32, mode.size.h as u32)),
        Some(CaptureSource::Output(_)) => None,
        Some(CaptureSource::Toplevel(win)) => state
            .window_geometry
            .get(win)
            .filter(|geo| geo.w > 0 && geo.h > 0)
            .map(|geo| (geo.w, geo.h)),
        None => None,
    };
    let Some((w, h)) = size else {
        stop_session(sess);
        debug!("[image-capture] session stopped at creation: nothing to capture");
        return;
    };
    sess.buffer_size(w, h);
    sess.shm_format(wl_shm::Format::Argb8888);
    sess.shm_format(wl_shm::Format::Xrgb8888);
    advertise_session_dmabuf(sess, state);
    sess.done();
    match source {
        Some(CaptureSource::Output(output)) => {
            debug!(
                "[image-capture] session created: output={} size={w}x{h}",
                output.name()
            );
        }
        Some(CaptureSource::Toplevel(win)) => {
            debug!("[image-capture] session created: toplevel={win:?} size={w}x{h}");
        }
        None => {}
    }
}

/// Global filter for every capture global here: a sandboxed
/// (wp_security_context) client must not capture outputs or other clients'
/// windows.
fn capture_global_visible(client: &Client) -> bool {
    !crate::backend::wayland::state::client_is_sandboxed(client)
}

/// Initialize the ext-image-copy-capture globals.
pub fn init_image_copy_capture(dh: &DisplayHandle) -> PendingImageCaptureQueue {
    dh.create_global::<JwmWaylandState, ExtOutputImageCaptureSourceManagerV1, _>(
        1,
        OutputSourceManagerData,
    );
    dh.create_global::<JwmWaylandState, ExtForeignToplevelImageCaptureSourceManagerV1, _>(
        1,
        ToplevelSourceManagerData,
    );
    dh.create_global::<JwmWaylandState, ExtImageCopyCaptureManagerV1, _>(1, CaptureManagerData);
    info!(
        "[udev/wayland] ext-image-copy-capture-v1 + ext-image-capture-source-v1 globals registered \
         (output + foreign-toplevel sources)"
    );
    new_pending_image_capture_queue()
}

// =============================================================================
// ext_output_image_capture_source_manager_v1
// =============================================================================

impl GlobalDispatch<ExtOutputImageCaptureSourceManagerV1, OutputSourceManagerData>
    for JwmWaylandState
{
    fn bind(
        state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ExtOutputImageCaptureSourceManagerV1>,
        _global_data: &OutputSourceManagerData,
        data_init: &mut DataInit<'_, Self>,
    ) {
        state.record_protocol_bind("ext_output_image_capture_source_manager_v1");
        data_init.init(resource, OutputSourceManagerData);
    }

    fn can_view(client: Client, _global_data: &OutputSourceManagerData) -> bool {
        capture_global_visible(&client)
    }
}

impl Dispatch<ExtOutputImageCaptureSourceManagerV1, OutputSourceManagerData> for JwmWaylandState {
    fn request(
        _state: &mut Self,
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
                // The new_id must always be initialized: returning without it
                // aborts the compositor inside wayland-backend.
                let capture_source = Output::from_resource(&wl_output).map(CaptureSource::Output);
                if capture_source.is_none() {
                    warn!(
                        "[image-capture] output source requested for a wl_output that no longer \
                         exists; the source is inert"
                    );
                }
                data_init.init(
                    source,
                    ImageCaptureSourceData {
                        source: capture_source,
                    },
                );
            }
            ext_output_image_capture_source_manager_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

// =============================================================================
// ext_foreign_toplevel_image_capture_source_manager_v1
// =============================================================================

impl GlobalDispatch<ExtForeignToplevelImageCaptureSourceManagerV1, ToplevelSourceManagerData>
    for JwmWaylandState
{
    fn bind(
        state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ExtForeignToplevelImageCaptureSourceManagerV1>,
        _global_data: &ToplevelSourceManagerData,
        data_init: &mut DataInit<'_, Self>,
    ) {
        state.record_protocol_bind("ext_foreign_toplevel_image_capture_source_manager_v1");
        data_init.init(resource, ToplevelSourceManagerData);
    }

    fn can_view(client: Client, _global_data: &ToplevelSourceManagerData) -> bool {
        capture_global_visible(&client)
    }
}

impl Dispatch<ExtForeignToplevelImageCaptureSourceManagerV1, ToplevelSourceManagerData>
    for JwmWaylandState
{
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ExtForeignToplevelImageCaptureSourceManagerV1,
        request: ext_foreign_toplevel_image_capture_source_manager_v1::Request,
        _data: &ToplevelSourceManagerData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            ext_foreign_toplevel_image_capture_source_manager_v1::Request::CreateSource {
                source,
                toplevel_handle,
            } => {
                // Look up which WindowId owns this handle. Match by identifier so we
                // don't depend on `ForeignToplevelHandle` being `PartialEq`-comparable
                // across crate boundaries; the identifier is the stable id the protocol
                // already sends to clients.
                let win = match ForeignToplevelHandle::from_resource(&toplevel_handle) {
                    Some(handle) => {
                        let target_id = handle.identifier();
                        let win = state
                            .foreign_toplevel_handles
                            .iter()
                            .find(|(_, h)| h.identifier() == target_id)
                            .map(|(w, _)| *w);
                        if win.is_none() {
                            // The window closed before the client asked for
                            // it. Its sessions are stopped; capturing any
                            // other surface would leak content the client
                            // never selected.
                            warn!(
                                "[image-capture] toplevel handle identifier={target_id} is not \
                                 a live window; the source is inert"
                            );
                        }
                        win
                    }
                    None => {
                        warn!(
                            "[image-capture] toplevel handle has no smithay user data; the source \
                             is inert"
                        );
                        None
                    }
                };
                // Always initialize the new_id; see the output manager above.
                data_init.init(
                    source,
                    ImageCaptureSourceData {
                        source: win.map(CaptureSource::Toplevel),
                    },
                );
            }
            ext_foreign_toplevel_image_capture_source_manager_v1::Request::Destroy => {}
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
        state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ExtImageCopyCaptureManagerV1>,
        _global_data: &CaptureManagerData,
        data_init: &mut DataInit<'_, Self>,
    ) {
        state.record_protocol_bind("ext_image_copy_capture_manager_v1");
        data_init.init(resource, CaptureManagerData);
    }

    fn can_view(client: Client, _global_data: &CaptureManagerData) -> bool {
        capture_global_visible(&client)
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
                // A source without our data or with an inert target yields a
                // session that is stopped right away, never a substitute output.
                let capture_source = source
                    .data::<ImageCaptureSourceData>()
                    .and_then(|d| d.source.clone());

                let paint_cursors = options
                    .into_result()
                    .map(|o| o.contains(ext_image_copy_capture_manager_v1::Options::PaintCursors))
                    .unwrap_or(false);

                let sess = data_init.init(
                    session,
                    CaptureSessionData::new(capture_source.clone(), paint_cursors),
                );
                track_session(state, &sess, capture_source.as_ref());
                {
                    let mut counters = state.capture_counters.lock_safe();
                    counters.note_image_copy_session();
                }

                // Send buffer constraints to client.
                send_session_constraints(&sess, state, capture_source.as_ref());
            }
            ext_image_copy_capture_manager_v1::Request::CreatePointerCursorSession {
                session,
                source,
                pointer: _,
            } => {
                // Same rule as CreateSession: an inert source stays inert, and
                // the cursor session's capture sub-session is stopped.
                let capture_source = source
                    .data::<ImageCaptureSourceData>()
                    .and_then(|d| d.source.clone());

                data_init.init(
                    session,
                    CursorSessionData {
                        source: capture_source,
                    },
                );
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
        resource: &ExtImageCopyCaptureSessionV1,
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

                // A session whose output or window went away since it
                // started is over: stop it (once) and let this frame fail on
                // capture.
                if data
                    .source
                    .as_ref()
                    .is_some_and(|source| source_is_gone(state, source))
                {
                    stop_session(resource);
                }
                let source = if data.stopped.load(Ordering::Relaxed) {
                    None
                } else {
                    data.source.clone()
                };
                data_init.init(
                    frame,
                    CaptureFrameData {
                        source,
                        session: resource.downgrade(),
                        paint_cursors: data.paint_cursors,
                        buffer: Mutex::new(None),
                        // Allocate lazily so idle frame objects stay cheap; the
                        // accumulator below still prevents growth past its cap.
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
        state: &mut Self,
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
                if !record_damage_rect(&mut data.damage.lock_safe(), (x, y, width, height)) {
                    resource.post_error(
                        ext_image_copy_capture_frame_v1::Error::InvalidBufferDamage,
                        "damage_buffer requires non-negative coordinates and positive dimensions",
                    );
                }
            }
            ext_image_copy_capture_frame_v1::Request::Capture => {
                // Move the attached buffer into the pending queue; the render
                // loop fulfills it on the next frame for the source output.
                let buffer = data.buffer.lock_safe().take();
                let source = match data.source.clone() {
                    Some(source) if source_is_gone(state, &source) => {
                        // The output or window went away after this frame
                        // was created: its session is over as well.
                        stop_frame_session(data);
                        None
                    }
                    source => source,
                };
                let Some(source) = source else {
                    // The session was stopped, so nothing will ever fill
                    // this frame.
                    data.damage.lock_safe().clear();
                    let mut counters = state.capture_counters.lock_safe();
                    counters.note_image_copy_failed(
                        "image-copy dispatch: capture on a stopped session",
                    );
                    resource.failed(ext_image_copy_capture_frame_v1::FailureReason::Stopped);
                    return;
                };
                match buffer {
                    Some(buffer) => {
                        let damage = std::mem::take(&mut *data.damage.lock_safe());
                        {
                            let mut counters = state.capture_counters.lock_safe();
                            match &source {
                                CaptureSource::Output(_) => {
                                    counters.note_image_copy_queued("output");
                                }
                                CaptureSource::Toplevel(_) => {
                                    counters.note_image_copy_queued("toplevel");
                                }
                            }
                        }
                        data.pending_queue.lock_safe().push(PendingImageCapture {
                            frame: resource.clone(),
                            buffer,
                            source,
                            paint_cursors: data.paint_cursors,
                            damage,
                        });
                        state.needs_redraw = true;
                        debug!("[image-capture] frame capture queued");
                    }
                    None => {
                        // capture without attach_buffer: fail rather than hang.
                        let mut counters = state.capture_counters.lock_safe();
                        counters
                            .note_image_copy_failed("image-copy dispatch: capture without buffer");
                        resource.failed(ext_image_copy_capture_frame_v1::FailureReason::Unknown);
                    }
                }
            }
            ext_image_copy_capture_frame_v1::Request::Destroy => {}
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        _resource: &ExtImageCopyCaptureFrameV1,
        data: &CaptureFrameData,
    ) {
        // The renderer fails a queued frame of a removed output or closed
        // window with `failed(stopped)`, and the client then destroys it.
        // The session gets its matching `stopped` here at the latest.
        if data
            .source
            .as_ref()
            .is_some_and(|source| source_is_gone(state, source))
        {
            stop_frame_session(data);
        }
    }
}

/// Stop the session that created the frame behind `data`, if it is alive.
fn stop_frame_session(data: &CaptureFrameData) {
    if let Ok(session) = data.session.upgrade() {
        stop_session(&session);
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
                    CaptureSessionData::new(capture_source.clone(), true),
                );
                track_session(state, &sess, capture_source.as_ref());

                // A capture session is unusable until the client receives buffer
                // constraints followed by `done`; without them a cursor-capture
                // client stalls forever. Mirror the regular-session sizing so the
                // client's buffer matches what the copy path writes.
                send_session_constraints(&sess, state, capture_source.as_ref());
            }
            ext_image_copy_capture_cursor_session_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

/// Raw-socket Wayland client for this directory's protocol tests.
///
/// The crate has no wayland-client dependency, so the client speaks the wire
/// format directly against a headless `JwmWaylandState`. Requests therefore
/// reach the real `Dispatch` impls exactly as they would from a connected
/// client, including wayland-backend's own checks (such as the abort when a
/// handler leaves a new_id uninitialized).
#[cfg(test)]
pub(crate) mod wire_test_client {
    use crate::backend::api::BackendEvent;
    use crate::backend::wayland::state::{JwmClientState, JwmWaylandState};
    use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
    use smithay::reexports::calloop::{EventLoop, channel};
    use smithay::reexports::wayland_server::Display;
    use std::collections::VecDeque;
    use std::io::{ErrorKind, Read, Write};
    use std::os::fd::{AsRawFd, RawFd};
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    /// A headless compositor: the display, the shared protocol state, and the
    /// backend-event queue its protocol handlers push into.
    pub(crate) struct Server {
        pub(crate) display: Display<JwmWaylandState>,
        pub(crate) state: JwmWaylandState,
        pub(crate) backend_events: Arc<Mutex<VecDeque<BackendEvent>>>,
        _event_loop: EventLoop<'static, JwmWaylandState>,
    }

    impl Server {
        pub(crate) fn new() -> Self {
            let event_loop: EventLoop<'static, JwmWaylandState> =
                EventLoop::try_new().expect("create test event loop");
            let display = Display::<JwmWaylandState>::new().expect("create test display");
            let backend_events = Arc::new(Mutex::new(VecDeque::new()));
            let (flush_tx, _flush_rx) = channel::channel();
            let (state, socket_name) = JwmWaylandState::init(
                &display.handle(),
                event_loop.handle(),
                backend_events.clone(),
                flush_tx,
                Arc::new(AtomicBool::new(false)),
                "wire-test-seat".to_owned(),
                false,
                false,
            )
            .expect("initialize headless Wayland state");
            assert!(socket_name.is_none());
            Self {
                display,
                state,
                backend_events,
                _event_loop: event_loop,
            }
        }

        /// Dispatch every request the clients have sent and flush the replies.
        pub(crate) fn roundtrip(&mut self) {
            self.display
                .dispatch_clients(&mut self.state)
                .expect("dispatch client requests");
            self.display.flush_clients().expect("flush client events");
        }

        /// Connect a raw client and collect the globals it is advertised.
        pub(crate) fn connect(&mut self) -> Client {
            let (server_end, peer) = UnixStream::pair().expect("create Wayland socket pair");
            self.display
                .handle()
                .insert_client(server_end, Arc::new(JwmClientState::default()))
                .expect("insert raw Wayland client");
            peer.set_nonblocking(true)
                .expect("make the test peer non-blocking");
            // Client object ids must be allocated without gaps; 2 is the
            // registry.
            let mut client = Client {
                peer,
                globals: Vec::new(),
                next_id: 2,
            };
            // wl_display.get_registry(new_id=2).
            client.request(1, 1, &[2]);
            self.roundtrip();
            for event in client.events() {
                if event.sender == 2 && event.opcode == 0 {
                    client.globals.push(event.global());
                }
            }
            client
        }
    }

    /// One server event, with its payload split into 32-bit words.
    pub(crate) struct Event {
        pub(crate) sender: u32,
        pub(crate) opcode: u16,
        pub(crate) args: Vec<u32>,
    }

    impl Event {
        /// Decode a wl_registry.global event: (name, interface, version).
        fn global(&self) -> (u32, String, u32) {
            let len = self.args[1] as usize;
            let bytes: Vec<u8> = self.args[2..]
                .iter()
                .flat_map(|word| word.to_ne_bytes())
                .collect();
            let interface = std::str::from_utf8(&bytes[..len - 1])
                .expect("registry interface is UTF-8")
                .to_owned();
            let version = self.args[2 + len.div_ceil(4)];
            (self.args[0], interface, version)
        }
    }

    pub(crate) struct Client {
        peer: UnixStream,
        globals: Vec<(u32, String, u32)>,
        next_id: u32,
    }

    impl Client {
        /// Allocate the next client-side object id.
        pub(crate) fn new_id(&mut self) -> u32 {
            self.next_id += 1;
            self.next_id
        }

        /// Bind the most recently advertised global implementing `interface`.
        pub(crate) fn bind(&mut self, interface: &str, version: u32) -> u32 {
            let (name, advertised) = self
                .globals
                .iter()
                .rev()
                .find(|(_, advertised, _)| advertised == interface)
                .map(|(name, _, version)| (*name, *version))
                .unwrap_or_else(|| panic!("{interface} is not advertised"));
            let id = self.new_id();
            let mut interface_bytes = interface.as_bytes().to_vec();
            interface_bytes.push(0);
            let interface_len = interface_bytes.len() as u32;
            interface_bytes.resize(interface_bytes.len().next_multiple_of(4), 0);
            let mut args = vec![name, interface_len];
            args.extend(
                interface_bytes
                    .chunks_exact(4)
                    .map(|word| u32::from_ne_bytes([word[0], word[1], word[2], word[3]])),
            );
            args.extend([version.min(advertised), id]);
            self.request(2, 0, &args);
            id
        }

        /// Send one request whose arguments are all 32-bit words.
        pub(crate) fn request(&mut self, object: u32, opcode: u16, args: &[u32]) {
            let message = message(object, opcode, args);
            self.peer.write_all(&message).expect("send Wayland request");
        }

        /// Send one request carrying a single fd argument (and nothing else).
        pub(crate) fn request_with_fd(&mut self, object: u32, opcode: u16, fd: RawFd) {
            let message = message(object, opcode, &[]);
            let mut iov = libc::iovec {
                iov_base: message.as_ptr() as *mut libc::c_void,
                iov_len: message.len(),
            };
            let fd_len = std::mem::size_of::<RawFd>() as u32;
            // SAFETY: CMSG_SPACE/CMSG_LEN are pure size computations.
            let (space, len) = unsafe { (libc::CMSG_SPACE(fd_len), libc::CMSG_LEN(fd_len)) };
            let mut control = vec![0u8; space as usize];
            // SAFETY: an all-zero msghdr is a valid empty header.
            let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
            header.msg_iov = &mut iov;
            header.msg_iovlen = 1;
            header.msg_control = control.as_mut_ptr().cast();
            header.msg_controllen = control.len() as _;
            // SAFETY: `control` is CMSG_SPACE bytes, large enough for the
            // single SCM_RIGHTS header and fd written here, and outlives the
            // sendmsg call together with `iov` and `message`.
            let sent = unsafe {
                let cmsg = libc::CMSG_FIRSTHDR(&header);
                assert!(!cmsg.is_null());
                (*cmsg).cmsg_level = libc::SOL_SOCKET;
                (*cmsg).cmsg_type = libc::SCM_RIGHTS;
                (*cmsg).cmsg_len = len as _;
                std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<RawFd>(), fd);
                libc::sendmsg(self.peer.as_raw_fd(), &header, 0)
            };
            assert_eq!(sent, message.len() as isize, "send fd-carrying request");
        }

        /// Every event the server has flushed to this client so far.
        pub(crate) fn events(&mut self) -> Vec<Event> {
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                match self.peer.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => bytes.extend_from_slice(&chunk[..read]),
                    Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                    Err(error) => panic!("read Wayland events: {error}"),
                }
            }
            let word = |offset: usize| {
                u32::from_ne_bytes([
                    bytes[offset],
                    bytes[offset + 1],
                    bytes[offset + 2],
                    bytes[offset + 3],
                ])
            };
            let mut events = Vec::new();
            let mut offset = 0;
            while offset + 8 <= bytes.len() {
                let header = word(offset + 4);
                let size = (header >> 16) as usize;
                assert!(size >= 8 && offset + size <= bytes.len(), "truncated event");
                events.push(Event {
                    sender: word(offset),
                    opcode: header as u16,
                    args: (offset + 8..offset + size).step_by(4).map(word).collect(),
                });
                offset += size;
            }
            events
        }
    }

    /// An output with a 64x48 mode, as the backends publish them.
    pub(crate) fn test_output(name: &str) -> Output {
        let output = Output::new(
            name.to_owned(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "JWM".into(),
                model: "wire-test".into(),
                serial_number: "0".into(),
            },
        );
        let mode = Mode {
            size: (64, 48).into(),
            refresh: 60_000,
        };
        output.change_current_state(Some(mode), None, None, Some((0, 0).into()));
        output
    }

    fn message(object: u32, opcode: u16, args: &[u32]) -> Vec<u8> {
        let size = 8 + 4 * args.len() as u32;
        let mut bytes = Vec::with_capacity(size as usize);
        bytes.extend_from_slice(&object.to_ne_bytes());
        bytes.extend_from_slice(&((size << 16) | u32::from(opcode)).to_ne_bytes());
        for arg in args {
            bytes.extend_from_slice(&arg.to_ne_bytes());
        }
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::wire_test_client::{Client, Server, test_output};
    use super::{
        FULL_BUFFER_DAMAGE, MAX_DAMAGE_RECTS_PER_FRAME, init_image_copy_capture,
        record_damage_rect, stop_output_capture_sessions, stop_toplevel_capture_sessions,
    };
    use crate::backend::api::Geometry;
    use crate::backend::common_define::WindowId;
    use crate::backend::wayland::state::JwmWaylandState;
    use smithay::output::Output;

    // ext_image_copy_capture_session_v1 requests and events.
    const SESSION_CREATE_FRAME: u16 = 0;
    const SESSION_BUFFER_SIZE: u16 = 0;
    const SESSION_DONE: u16 = 4;
    const SESSION_STOPPED: u16 = 5;
    // ext_image_copy_capture_frame_v1 requests.
    const FRAME_DESTROY: u16 = 0;
    const FRAME_CAPTURE: u16 = 3;
    // ext_image_copy_capture_frame_v1.failed and its `stopped` reason.
    const FRAME_FAILED: u16 = 4;
    const FAILURE_STOPPED: u32 = 2;

    fn capture_server() -> Server {
        let mut server = Server::new();
        let queue = init_image_copy_capture(&server.display.handle());
        server.state.image_capture_pending = Some(queue);
        server
    }

    /// Publish `window` to `client`'s foreign-toplevel list. Returns the
    /// client's handle object.
    fn published_toplevel_handle(
        server: &mut Server,
        client: &mut Client,
        window: WindowId,
    ) -> u32 {
        let handle = server
            .state
            .foreign_toplevel_list_state
            .new_toplevel_with_identifier::<JwmWaylandState>(
                "private notes",
                "secret.app",
                JwmWaylandState::foreign_toplevel_identifier(window),
            );
        server.state.foreign_toplevel_handles.insert(window, handle);
        let list = client.bind("ext_foreign_toplevel_list_v1", 1);
        server.roundtrip();
        client
            .events()
            .into_iter()
            .find(|event| event.sender == list && event.opcode == 0)
            .map(|event| event.args[0])
            .expect("the list announces the window")
    }

    /// Publish a toplevel to `client`'s foreign-toplevel list, then close the
    /// window the way `remove_wayland_window` does. Returns the client's now
    /// stale handle object.
    fn stale_toplevel_handle(server: &mut Server, client: &mut Client) -> u32 {
        let window = WindowId::from_raw(0x51);
        let toplevel = published_toplevel_handle(server, client, window);

        let handle = server
            .state
            .foreign_toplevel_handles
            .remove(&window)
            .expect("published handle");
        server
            .state
            .foreign_toplevel_list_state
            .remove_toplevel(&handle);
        toplevel
    }

    /// Publish `output` (with a wl_output global) as one of the backend's
    /// outputs, and start a capture session on it from a new client.
    fn output_session(server: &mut Server, output: &Output) -> (Client, u32) {
        output.create_global::<JwmWaylandState>(&server.display.handle());
        server.state.outputs.push(output.clone());
        let mut client = server.connect();
        let wl_output = client.bind("wl_output", 4);
        let sources = client.bind("ext_output_image_capture_source_manager_v1", 1);
        let capture = client.bind("ext_image_copy_capture_manager_v1", 1);
        let source = client.new_id();
        client.request(sources, 0, &[source, wl_output]);
        let session = client.new_id();
        client.request(capture, 0, &[session, source, 0]);
        server.roundtrip();
        assert_eq!(
            session_opcodes(&mut client, session).last(),
            Some(&SESSION_DONE),
            "a live output's session is ready"
        );
        (client, session)
    }

    /// `(opcode, args)` of every event `object` received in `events`.
    fn object_events(
        events: &[super::wire_test_client::Event],
        object: u32,
    ) -> Vec<(u16, Vec<u32>)> {
        events
            .iter()
            .filter(|event| event.sender == object)
            .map(|event| (event.opcode, event.args.clone()))
            .collect()
    }

    #[test]
    fn a_removed_outputs_session_stops_once_and_its_later_frames_fail() {
        let mut server = capture_server();
        let output = test_output("CAPTURE-1");
        let (mut client, session) = output_session(&mut server, &output);

        // A KMS rebuild replaced the output. Before the fix the session
        // stayed open and each new frame waited for a render that never came.
        server.state.outputs.clear();
        let frame = client.new_id();
        client.request(session, SESSION_CREATE_FRAME, &[frame]);
        client.request(frame, FRAME_CAPTURE, &[]);
        server.roundtrip();
        let events = client.events();
        assert_eq!(object_events(&events, session), [(SESSION_STOPPED, vec![])]);
        assert_eq!(
            object_events(&events, frame),
            [(FRAME_FAILED, vec![FAILURE_STOPPED])]
        );

        // `stopped` ends the session: it is not sent again, and every later
        // frame fails the same way.
        let frame = client.new_id();
        client.request(session, SESSION_CREATE_FRAME, &[frame]);
        client.request(frame, FRAME_CAPTURE, &[]);
        server.roundtrip();
        let events = client.events();
        assert!(object_events(&events, session).is_empty());
        assert_eq!(
            object_events(&events, frame),
            [(FRAME_FAILED, vec![FAILURE_STOPPED])]
        );
        assert!(!stop_output_capture_sessions(&output));
        assert!(
            server
                .state
                .image_capture_pending
                .as_ref()
                .is_some_and(|queue| queue.lock().expect("capture queue").is_empty()),
            "nothing may be queued for the renderer"
        );
    }

    #[test]
    fn removing_an_output_stops_only_its_own_sessions() {
        let mut server = capture_server();
        let removed = test_output("CAPTURE-1");
        let kept = test_output("CAPTURE-2");
        let (mut removed_client, removed_session) = output_session(&mut server, &removed);
        let (mut kept_client, kept_session) = output_session(&mut server, &kept);

        server.state.outputs.retain(|output| *output != removed);
        assert!(stop_output_capture_sessions(&removed));
        assert!(
            !stop_output_capture_sessions(&removed),
            "a stopped session is not stopped twice"
        );
        server.roundtrip();
        assert_eq!(
            session_opcodes(&mut removed_client, removed_session),
            [SESSION_STOPPED]
        );
        assert!(session_opcodes(&mut kept_client, kept_session).is_empty());
    }

    #[test]
    fn destroying_a_frame_of_a_removed_output_stops_its_session() {
        let mut server = capture_server();
        let output = test_output("CAPTURE-1");
        let (mut client, session) = output_session(&mut server, &output);
        let frame = client.new_id();
        client.request(session, SESSION_CREATE_FRAME, &[frame]);
        server.roundtrip();
        assert!(session_opcodes(&mut client, session).is_empty());

        // The renderer failed the frame of a gone output and the client
        // destroys it: the session gets the matching `stopped`.
        server.state.outputs.clear();
        client.request(frame, FRAME_DESTROY, &[]);
        server.roundtrip();
        assert_eq!(session_opcodes(&mut client, session), [SESSION_STOPPED]);
    }

    fn session_opcodes(client: &mut Client, session: u32) -> Vec<u16> {
        client
            .events()
            .into_iter()
            .filter(|event| event.sender == session)
            .map(|event| event.opcode)
            .collect()
    }

    #[test]
    fn closed_toplevel_capture_stops_instead_of_streaming_the_first_output() {
        let mut server = capture_server();
        server.state.outputs.push(test_output("CAPTURE-1"));
        let mut client = server.connect();
        let sources = client.bind("ext_foreign_toplevel_image_capture_source_manager_v1", 1);
        let capture = client.bind("ext_image_copy_capture_manager_v1", 1);
        let toplevel = stale_toplevel_handle(&mut server, &mut client);

        let source = client.new_id();
        client.request(sources, 0, &[source, toplevel]);
        let session = client.new_id();
        client.request(capture, 0, &[session, source, 0]);
        server.roundtrip();

        assert_eq!(
            session_opcodes(&mut client, session),
            [SESSION_STOPPED],
            "a closed window's session must stop, never advertise the first output's buffer"
        );
    }

    #[test]
    fn a_closed_windows_capture_session_stops_and_its_next_frame_fails() {
        let mut server = capture_server();
        let window = WindowId::from_raw(0x52);
        server.state.window_geometry.insert(
            window,
            Geometry {
                x: 0,
                y: 0,
                w: 32,
                h: 24,
                border: 0,
            },
        );
        let mut client = server.connect();
        let sources = client.bind("ext_foreign_toplevel_image_capture_source_manager_v1", 1);
        let capture = client.bind("ext_image_copy_capture_manager_v1", 1);
        let toplevel = published_toplevel_handle(&mut server, &mut client, window);
        let source = client.new_id();
        client.request(sources, 0, &[source, toplevel]);
        let session = client.new_id();
        client.request(capture, 0, &[session, source, 0]);
        server.roundtrip();
        let events = client.events();
        let session_events = object_events(&events, session);
        assert_eq!(
            session_events.first(),
            Some(&(SESSION_BUFFER_SIZE, vec![32, 24])),
            "a live window's session advertises the window size"
        );
        assert_eq!(
            session_events.last().map(|(opcode, _)| *opcode),
            Some(SESSION_DONE)
        );

        // The window closes (every path that retires a window drops its
        // geometry). Before the fix the predicate treated toplevels as
        // never gone: the session was never stopped, and every capture was
        // queued and forced a redraw only to fail in the renderer.
        server.state.window_geometry.remove(&window);
        server.state.needs_redraw = false;
        let frame = client.new_id();
        client.request(session, SESSION_CREATE_FRAME, &[frame]);
        client.request(frame, FRAME_CAPTURE, &[]);
        server.roundtrip();
        let events = client.events();
        assert_eq!(object_events(&events, session), [(SESSION_STOPPED, vec![])]);
        assert_eq!(
            object_events(&events, frame),
            [(FRAME_FAILED, vec![FAILURE_STOPPED])]
        );
        assert!(
            server
                .state
                .image_capture_pending
                .as_ref()
                .is_some_and(|queue| queue.lock().expect("capture queue").is_empty()),
            "nothing may be queued for the renderer"
        );
        assert!(
            !server.state.needs_redraw,
            "a stopped capture forces no redraw"
        );
    }

    #[test]
    fn destroying_a_frame_of_a_closed_window_stops_its_session() {
        let mut server = capture_server();
        let window = WindowId::from_raw(0x53);
        server.state.window_geometry.insert(
            window,
            Geometry {
                x: 0,
                y: 0,
                w: 32,
                h: 24,
                border: 0,
            },
        );
        let mut client = server.connect();
        let sources = client.bind("ext_foreign_toplevel_image_capture_source_manager_v1", 1);
        let capture = client.bind("ext_image_copy_capture_manager_v1", 1);
        let toplevel = published_toplevel_handle(&mut server, &mut client, window);
        let source = client.new_id();
        client.request(sources, 0, &[source, toplevel]);
        let session = client.new_id();
        client.request(capture, 0, &[session, source, 0]);
        let frame = client.new_id();
        client.request(session, SESSION_CREATE_FRAME, &[frame]);
        server.roundtrip();
        assert_eq!(
            session_opcodes(&mut client, session).last(),
            Some(&SESSION_DONE)
        );

        // The renderer failed the queued frame of the closed window and the
        // client destroys it: the session gets the matching `stopped`.
        server.state.window_geometry.remove(&window);
        client.request(frame, FRAME_DESTROY, &[]);
        server.roundtrip();
        assert_eq!(session_opcodes(&mut client, session), [SESSION_STOPPED]);
    }

    /// Start a capture session on `window` (32x24) from `client`. Returns
    /// the session object. The window's foreign-toplevel handle is
    /// unpublished again once the session exists (the session keeps the
    /// window), so a later client's list announces only its own window.
    fn toplevel_session(server: &mut Server, client: &mut Client, window: WindowId) -> u32 {
        server.state.window_geometry.insert(
            window,
            Geometry {
                x: 0,
                y: 0,
                w: 32,
                h: 24,
                border: 0,
            },
        );
        let sources = client.bind("ext_foreign_toplevel_image_capture_source_manager_v1", 1);
        let capture = client.bind("ext_image_copy_capture_manager_v1", 1);
        let toplevel = published_toplevel_handle(server, client, window);
        let source = client.new_id();
        client.request(sources, 0, &[source, toplevel]);
        let session = client.new_id();
        client.request(capture, 0, &[session, source, 0]);
        server.roundtrip();
        assert_eq!(
            session_opcodes(client, session).last(),
            Some(&SESSION_DONE),
            "a live window's session is ready"
        );
        let handle = server
            .state
            .foreign_toplevel_handles
            .remove(&window)
            .expect("published handle");
        server
            .state
            .foreign_toplevel_list_state
            .remove_toplevel(&handle);
        session
    }

    #[test]
    fn closing_a_window_stops_its_idle_capture_session_at_once() {
        let mut server = capture_server();
        let closed = WindowId::from_raw(0x54);
        let kept = WindowId::from_raw(0x55);
        let mut closed_client = server.connect();
        let closed_session = toplevel_session(&mut server, &mut closed_client, closed);
        let mut kept_client = server.connect();
        let kept_session = toplevel_session(&mut server, &mut kept_client, kept);

        // A paused stream: no frame is in flight when the window closes.
        // Before the fix nothing told the session until its next
        // create_frame, so the consumer showed a frozen window as live.
        server.state.window_geometry.remove(&closed);
        assert!(stop_toplevel_capture_sessions(&mut server.state, closed));
        assert!(
            !stop_toplevel_capture_sessions(&mut server.state, closed),
            "a window's sessions are stopped once"
        );
        server.roundtrip();
        assert_eq!(
            session_opcodes(&mut closed_client, closed_session),
            [SESSION_STOPPED]
        );
        assert!(session_opcodes(&mut kept_client, kept_session).is_empty());
        assert_eq!(
            server
                .state
                .toplevel_capture_sessions
                .keys()
                .collect::<Vec<_>>(),
            [&kept],
            "the closed window's entry is gone"
        );

        // A frame the client creates afterwards fails without a second
        // `stopped`.
        let frame = closed_client.new_id();
        closed_client.request(closed_session, SESSION_CREATE_FRAME, &[frame]);
        closed_client.request(frame, FRAME_CAPTURE, &[]);
        server.roundtrip();
        let events = closed_client.events();
        assert!(object_events(&events, closed_session).is_empty());
        assert_eq!(
            object_events(&events, frame),
            [(FRAME_FAILED, vec![FAILURE_STOPPED])]
        );
    }

    #[test]
    fn a_session_on_an_already_closed_window_is_not_tracked() {
        let mut server = capture_server();
        let window = WindowId::from_raw(0x56);
        let mut client = server.connect();
        let sources = client.bind("ext_foreign_toplevel_image_capture_source_manager_v1", 1);
        let capture = client.bind("ext_image_copy_capture_manager_v1", 1);
        // The handle is still published, but the window has no geometry:
        // it is being retired.
        let toplevel = published_toplevel_handle(&mut server, &mut client, window);
        let source = client.new_id();
        client.request(sources, 0, &[source, toplevel]);
        let session = client.new_id();
        client.request(capture, 0, &[session, source, 0]);
        server.roundtrip();
        assert_eq!(session_opcodes(&mut client, session), [SESSION_STOPPED]);
        // Nothing would ever remove an entry for a window that is gone.
        assert!(server.state.toplevel_capture_sessions.is_empty());
    }

    #[test]
    fn stale_capture_targets_without_outputs_stop_instead_of_aborting() {
        let mut server = capture_server();
        assert!(server.state.outputs.is_empty());
        let display = server.display.handle();
        let output = test_output("UNPLUGGED-1");
        let output_global = output.create_global::<JwmWaylandState>(&display);
        let mut client = server.connect();
        let wl_output = client.bind("wl_output", 4);
        let output_sources = client.bind("ext_output_image_capture_source_manager_v1", 1);
        let toplevel_sources =
            client.bind("ext_foreign_toplevel_image_capture_source_manager_v1", 1);
        let capture = client.bind("ext_image_copy_capture_manager_v1", 1);
        let toplevel = stale_toplevel_handle(&mut server, &mut client);

        // Unplug the output: once its global and last owner are gone the
        // client's wl_output no longer resolves to an Output.
        display.remove_global::<JwmWaylandState>(output_global);
        drop(output);

        let output_source = client.new_id();
        client.request(output_sources, 0, &[output_source, wl_output]);
        let toplevel_source = client.new_id();
        client.request(toplevel_sources, 0, &[toplevel_source, toplevel]);
        let output_session = client.new_id();
        client.request(capture, 0, &[output_session, output_source, 0]);
        let toplevel_session = client.new_id();
        client.request(capture, 0, &[toplevel_session, toplevel_source, 0]);
        // A frame of a stopped session can only fail.
        let frame = client.new_id();
        client.request(output_session, 0, &[frame]);
        client.request(frame, 3, &[]);
        // Before the fix, both source requests returned without initializing
        // their new_id and wayland-backend aborted this dispatch.
        server.roundtrip();

        let events = client.events();
        let opcodes = |object: u32| {
            events
                .iter()
                .filter(|event| event.sender == object)
                .map(|event| event.opcode)
                .collect::<Vec<_>>()
        };
        assert_eq!(opcodes(output_session), [SESSION_STOPPED]);
        assert_eq!(opcodes(toplevel_session), [SESSION_STOPPED]);
        let frame_events: Vec<(u16, Vec<u32>)> = events
            .iter()
            .filter(|event| event.sender == frame)
            .map(|event| (event.opcode, event.args.clone()))
            .collect();
        assert_eq!(frame_events, [(FRAME_FAILED, vec![FAILURE_STOPPED])]);
        assert!(
            server
                .state
                .image_capture_pending
                .as_ref()
                .is_some_and(|queue| queue.lock().expect("capture queue").is_empty()),
            "nothing may be queued for the renderer"
        );
    }

    #[test]
    fn damage_requests_are_bounded_and_preserve_their_union() {
        let mut damage = Vec::new();

        for x in 0..10_000 {
            assert!(record_damage_rect(&mut damage, (x, 5, 1, 2)));
            assert!(damage.len() <= MAX_DAMAGE_RECTS_PER_FRAME);
        }

        let min_x = damage.iter().map(|rect| rect.0).min().unwrap();
        let max_x = damage
            .iter()
            .map(|rect| i64::from(rect.0) + i64::from(rect.2))
            .max()
            .unwrap();
        assert_eq!(min_x, 0);
        assert_eq!(max_x, 10_000);
    }

    #[test]
    fn unrepresentable_bounding_union_becomes_full_buffer_damage() {
        let mut damage = Vec::new();
        assert!(record_damage_rect(
            &mut damage,
            (i32::MAX - 1, 0, i32::MAX, 1),
        ));
        assert_eq!(damage, [FULL_BUFFER_DAMAGE]);

        // Once full-buffer damage is recorded, a flood remains constant-size.
        for x in 0..10_000 {
            assert!(record_damage_rect(&mut damage, (x, x, 1, 1)));
        }
        assert_eq!(damage, [FULL_BUFFER_DAMAGE]);
    }

    #[test]
    fn invalid_damage_is_rejected_without_mutating_accumulated_state() {
        let mut damage = vec![(1, 2, 3, 4)];
        for invalid in [(-1, 0, 1, 1), (0, -1, 1, 1), (0, 0, 0, 1), (0, 0, 1, 0)] {
            assert!(!record_damage_rect(&mut damage, invalid));
        }
        assert_eq!(damage, [(1, 2, 3, 4)]);
    }
}
