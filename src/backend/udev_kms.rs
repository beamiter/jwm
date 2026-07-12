use crate::sync_ext::MutexExt;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use smithay::backend::allocator::Format as DmabufFormat;
use smithay::backend::allocator::Fourcc;
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::format::FormatSet;
use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::drm::compositor::FrameFlags;
use smithay::backend::drm::exporter::gbm::GbmFramebufferExporter;
use smithay::backend::drm::exporter::gbm::NodeFilter;
use smithay::backend::drm::output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements};
use smithay::backend::drm::{DrmDevice, DrmDeviceFd, DrmEvent, DrmEventMetadata};
use smithay::backend::egl::context::ContextPriority;
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::memory::{
    MemoryRenderBuffer, MemoryRenderBufferRenderElement,
};
use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::element::surface::{
    WaylandSurfaceRenderElement, render_elements_from_surface_tree,
};
use smithay::backend::renderer::element::texture::TextureRenderElement;
use smithay::backend::renderer::element::{AsRenderElements, Id, Kind};
use smithay::backend::renderer::gles::GlesRenderbuffer;
use smithay::backend::renderer::gles::ffi as gl_ffi;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::utils::RendererSurfaceStateUserData;
use smithay::backend::renderer::{Bind, ExportMem, Offscreen, Renderer};
use smithay::backend::session::Session;
use smithay::backend::session::libseat::LibSeatSession;
use smithay::desktop::layer_map_for_output;
use smithay::desktop::space::SurfaceTree;
use smithay::desktop::utils::send_frames_surface_tree;
use smithay::output::{Mode as WlMode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::channel::Sender;
use smithay::reexports::calloop::{LoopHandle, RegistrationToken};
use smithay::reexports::drm::control::{Device as ControlDevice, ModeTypeFlags, connector, crtc};
use smithay::reexports::rustix::fs::OFlags;
use smithay::reexports::wayland_server;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Buffer as BufferCoord, Monotonic, Size, Time};
use smithay::utils::{DeviceFd, Physical, Point, Rectangle, Scale, Transform};
use smithay::wayland::compositor::{TraversalAction, with_states, with_surface_tree_downward};
use smithay::wayland::dmabuf::get_dmabuf;
use smithay::wayland::presentation::{PresentationFeedbackCachedState, Refresh};
use smithay::wayland::shell::wlr_layer::Layer as WlrLayer;
use smithay::wayland::shell::xdg::SurfaceCachedState;

use crate::backend::common_define::StdCursorKind;

use xcursor::{
    CursorTheme,
    parser::{Image, parse_xcursor},
};

smithay::backend::renderer::element::render_elements! {
    pub KmsRenderElement<=GlesRenderer>;
    Surface=WaylandSurfaceRenderElement<GlesRenderer>,
    Solid=SolidColorRenderElement,
    Memory=MemoryRenderBufferRenderElement<GlesRenderer>,
    Texture=TextureRenderElement<GlesTexture>,
}

pub(super) type KmsHandle = Rc<RefCell<KmsState>>;

struct KmsOutputState {
    crtc: crtc::Handle,
    connector: connector::Handle,
    mode_size: (i32, i32),
    origin: (i32, i32),

    output: Output,
    drm_output:
        DrmOutput<GbmAllocator<DrmDeviceFd>, GbmFramebufferExporter<DrmDeviceFd>, (), DrmDeviceFd>,

    frame_pending: bool,
    /// When `frame_pending` was last set. If a queued page flip never produces a
    /// vblank (driver hiccup, dropped flip), `frame_pending` would otherwise stay
    /// true forever and the output stops rendering. The watchdog in `render` uses
    /// this to force-clear a stale pending flag after several refresh intervals.
    frame_pending_since: Option<std::time::Instant>,

    send_frame_callbacks: bool,
    frame_callback_roots: Vec<WlSurface>,
    frame_callback_throttle: Option<std::time::Duration>,
    frame_callback_visible: HashSet<wayland_server::Weak<WlSurface>>,

    surfaces_on_output: HashSet<wayland_server::Weak<WlSurface>>,

    last_vblank: Option<std::time::Duration>,
    last_vblank_received_at: Option<std::time::Instant>,
    refresh_interval: std::time::Duration,

    /// Cached `output.name()` — smithay's accessor allocates a fresh `String`
    /// per call, and the per-frame color-pipeline refresh would otherwise pay
    /// that allocation for every output every frame to compare against
    /// `soft_disabled_outputs`.
    output_name: String,

    /// Per-CRTC color pipeline state. Caps are probed once at output init.
    /// `installed_gamma_lut` carries `Some((blob_id, tf))` when a GAMMA_LUT
    /// blob is currently bound on the CRTC, so the activation refresh can
    /// no-op when the desired TF already matches and so teardown / DPMS-off
    /// can `destroy_property_blob` cleanly.
    color_pipeline_caps: Option<crate::backend::api::KmsColorPipelineCaps>,
    installed_gamma_lut: Option<(
        u64,
        crate::backend::wayland_udev::color_pipeline::TransferKind,
    )>,
    /// Tracked CTM blob id. The installed payload is always
    /// `rgb_to_rgb_matrix(SRGB_D65, output_primaries)` (or identity when the
    /// monitor is sRGB-primaries), derived from EDID at init — constant for
    /// the life of the `KmsOutputState`, so we only ever install once per
    /// output and skip reinstall when `installed_ctm.is_some()`.
    installed_ctm: Option<u64>,
    /// Per-output target transfer function, derived from EDID HDR caps at
    /// output init.
    output_tf: crate::backend::wayland_udev::color_pipeline::TransferKind,
    /// Per-output sRGB→output-primaries 3x3 matrix, cached at init. Pushed
    /// via `install_ctm` once `kms_color_pipeline_offload + scene_linear` are
    /// both on, so the FBO can stay uniform-sRGB while each CRTC converts to
    /// its native primaries at scanout (the trick that lets the single global
    /// FBO drive a mixed-primaries multi-output session without per-output
    /// passes).
    output_ctm: [f32; 9],
    /// `true` while DPMS is off; the LUT install path skips this output.
    dpms_off: bool,
}

/// Outcome of `refresh_color_pipeline_offload`, threaded to the renderer so the
/// shader path can disable the fragment-shader encode when the CRTC GAMMA_LUT
/// took over. `hw_ctm_active` is unused in 3.3b (identity payload only — the
/// per-surface ColorTransform target switch that consumes it lands in 3.3c),
/// but is part of the contract so callers don't need a second refactor.
#[derive(Clone, Copy)]
pub(super) struct ColorPipelineDecision {
    pub hw_encode_active: bool,
    pub shader_tf: i32,
    pub shader_gamma: f32,
    pub hw_ctm_active: bool,
}

pub(super) struct KmsState {
    #[allow(dead_code)]
    dev_path: std::path::PathBuf,
    pub(super) drm_device_fd: DrmDeviceFd,

    pub registration_token: Option<RegistrationToken>,

    flush_tx: Sender<()>,
    flush_pending: Arc<AtomicBool>,

    #[allow(dead_code)]
    drm_output_manager: DrmOutputManager<
        GbmAllocator<DrmDeviceFd>,
        GbmFramebufferExporter<DrmDeviceFd>,
        (),
        DrmDeviceFd,
    >,
    #[allow(dead_code)]
    gbm: GbmDevice<DrmDeviceFd>,
    renderer: GlesRenderer,

    pub(super) needs_render: bool,
    compositor_texture_cache: Option<(u32, GlesTexture)>,
    // Strong refs to every distinct compositor output-FBO texture we've wrapped.
    // The GL texture is owned/deleted by the compositor, so smithay's GlesTexture
    // Drop must never fire on it (double-glDeleteTextures / recycled-id risk).
    // Holding a ref keeps Drop suppressed; deduping by id bounds growth across
    // FBO swaps (replaces an unrecoverable mem::forget that leaked per swap).
    compositor_texture_keepalive: Vec<(u32, GlesTexture)>,
    background_id: Id,

    cursor_theme: CursorTheme,
    cursor_size: u32,
    cursor_images: HashMap<String, Vec<Image>>,
    cursor_cache: HashMap<(StdCursorKind, u32), CursorBitmap>,

    cursor_fallback_body_ids: Vec<Id>,
    cursor_fallback_shadow_ids: Vec<Id>,

    pending_screenshot: Option<std::path::PathBuf>,
    pending_screenshot_region: Option<(std::path::PathBuf, i32, i32, u32, u32)>,

    /// Shared queue for pending screencopy frames (from wlr-screencopy-unstable-v1).
    screencopy_pending: Option<crate::backend::wayland_udev::screencopy::PendingScreencopyQueue>,

    /// Shared queue for pending ext-image-copy-capture-v1 frames.
    image_capture_pending:
        Option<crate::backend::wayland_udev::image_copy_capture::PendingImageCaptureQueue>,

    /// Shared capture counters updated by protocol dispatch and render-drain.
    capture_counters:
        Option<std::sync::Arc<std::sync::Mutex<crate::backend::wayland::state::CaptureCounters>>>,

    outputs: Vec<KmsOutputState>,

    /// Reused offscreen renderbuffer for screencopy / image-copy-capture readback,
    /// keyed by (width, height). Continuous capture (OBS/wf-recorder) calls the
    /// fulfill paths every frame; without this each frame allocated a fresh
    /// full-screen GPU renderbuffer. Both fulfill paths share it within a frame
    /// since they run sequentially on the same output size.
    screencopy_offscreen: Option<(i32, i32, GlesRenderbuffer)>,

    /// Reused offscreen renderbuffer for ext-image-copy-capture *toplevel* (single
    /// window) capture, keyed by (width, height). Kept separate from
    /// `screencopy_offscreen` because a window's size differs from the output's,
    /// so sharing one cache would thrash reallocation between output and toplevel
    /// captures every frame.
    image_capture_toplevel_offscreen: Option<(i32, i32, GlesRenderbuffer)>,

    /// Latest vblank presentation timestamp (monotonic) for frame pacing feedback.
    last_presentation_time: Option<std::time::Instant>,

    /// Last KMS-layer direct-scanout decision per output. This complements the
    /// compositor scene eligibility: KMS can still reject because overlays,
    /// cursor, config gates, or per-output state require composition.
    last_direct_scanout_outputs: Vec<crate::backend::api::DirectScanoutOutputStatus>,
}

#[derive(Clone)]
struct CursorBitmap {
    buffer: MemoryRenderBuffer,
    xhot: i32,
    yhot: i32,
}

// A tiny software cursor (pointer arrow) expressed as a list of rectangles.
// Coordinates are relative to the cursor hotspot (tip at 0,0).
const CURSOR_RECTS: &[(i32, i32, i32, i32)] = &[
    // Triangle head (11 scanlines)
    (0, 0, 1, 1),
    (0, 1, 2, 1),
    (0, 2, 3, 1),
    (0, 3, 4, 1),
    (0, 4, 5, 1),
    (0, 5, 6, 1),
    (0, 6, 7, 1),
    (0, 7, 8, 1),
    (0, 8, 9, 1),
    (0, 9, 10, 1),
    (0, 10, 11, 1),
    // Stem
    (3, 11, 3, 7),
    // Base
    (2, 18, 5, 2),
];

fn cursor_candidates(kind: StdCursorKind) -> &'static [&'static str] {
    match kind {
        StdCursorKind::LeftPtr => &["left_ptr", "default"],
        StdCursorKind::Hand => &["hand2", "hand1", "pointer", "default"],
        StdCursorKind::XTerm => &["xterm", "text", "default"],
        StdCursorKind::Watch => &["watch", "wait", "default"],
        StdCursorKind::Crosshair => &["crosshair", "default"],
        StdCursorKind::Fleur => &["fleur", "move", "default"],
        StdCursorKind::HDoubleArrow => &["sb_h_double_arrow", "h_double_arrow", "default"],
        StdCursorKind::VDoubleArrow => &["sb_v_double_arrow", "v_double_arrow", "default"],
        StdCursorKind::TopLeftCorner => &["top_left_corner", "nw-resize", "default"],
        StdCursorKind::TopRightCorner => &["top_right_corner", "ne-resize", "default"],
        StdCursorKind::BottomLeftCorner => &["bottom_left_corner", "sw-resize", "default"],
        StdCursorKind::BottomRightCorner => &["bottom_right_corner", "se-resize", "default"],
        StdCursorKind::Sizing => &["sizing", "default"],
    }
}

#[allow(dead_code)]
#[derive(Debug)]
pub(super) enum KmsInitError {
    DeviceOpen(smithay::backend::session::libseat::Error),
    DrmInit(smithay::backend::drm::DrmError),
    GbmInit(std::io::Error),
    EglInit(smithay::backend::egl::Error),
    GlesInit(smithay::backend::renderer::gles::GlesError),
    NoConnector,
    NoCrtc,
    InitializeOutput(String),
}

impl std::fmt::Display for KmsInitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KmsInitError::DeviceOpen(e) => write!(f, "libseat open failed: {e}"),
            KmsInitError::DrmInit(e) => write!(f, "drm init failed: {e}"),
            KmsInitError::GbmInit(e) => write!(f, "gbm init failed: {e}"),
            KmsInitError::EglInit(e) => write!(f, "egl init failed: {e}"),
            KmsInitError::GlesInit(e) => write!(f, "gles init failed: {e}"),
            KmsInitError::NoConnector => write!(f, "no connected drm connector found"),
            KmsInitError::NoCrtc => write!(f, "could not pick CRTC for connector"),
            KmsInitError::InitializeOutput(e) => write!(f, "initialize_output failed: {e}"),
        }
    }
}

impl std::error::Error for KmsInitError {}

impl KmsState {
    fn deliver_frame_callbacks(
        out: &mut KmsOutputState,
        flush_tx: &Sender<()>,
        flush_pending: &AtomicBool,
        presentation_time: Option<std::time::Duration>,
    ) {
        if !out.send_frame_callbacks {
            return;
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or(std::time::Duration::ZERO);

        let throttle = out.frame_callback_throttle;
        let output = out.output.clone();
        let visible = out.frame_callback_visible.clone();
        let refresh = out.refresh_interval;
        let commit_deadline = presentation_time.map(Time::<Monotonic>::from);

        for root in &out.frame_callback_roots {
            let mut root_tree_visible = visible.contains(&root.downgrade());
            if !root_tree_visible {
                with_surface_tree_downward(
                    root,
                    (),
                    |_, _, _| TraversalAction::DoChildren(()),
                    |surface, _states, _| {
                        if visible.contains(&surface.downgrade()) {
                            root_tree_visible = true;
                        }
                    },
                    |_, _, _| true,
                );
            }

            // Send presentation feedback for wp_presentation protocol when this
            // callback is tied to an actual vblank. Empty-damage callback
            // delivery below intentionally omits presentation feedback.
            if let Some(vblank_time) = presentation_time {
                with_surface_tree_downward(
                    root,
                    (),
                    |_, _, _| TraversalAction::DoChildren(()),
                    |_surface, states, _| {
                        let mut cached =
                            states.cached_state.get::<PresentationFeedbackCachedState>();
                        let feedback = cached.current();
                        for cb in feedback.callbacks.drain(..) {
                            cb.presented(
                                &output,
                                vblank_time,
                                Refresh::fixed(refresh),
                                0,
                                smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind::Vsync
                                    | smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind::HwClock,
                            );
                        }
                    },
                    |_, _, _| true,
                );
            }

            crate::backend::wayland::state::JwmWaylandState::signal_surface_pacing_barriers(
                root,
                commit_deadline,
                true,
            );

            send_frames_surface_tree(root, &output, now, throttle, |surface, _states| {
                if (surface.id() == root.id() && root_tree_visible)
                    || visible.contains(&surface.downgrade())
                {
                    Some(output.clone())
                } else {
                    None
                }
            });
        }

        out.send_frame_callbacks = false;
        out.frame_callback_roots.clear();
        out.frame_callback_visible.clear();

        // Frame callbacks are Wayland events; flush them promptly.
        if !flush_pending.swap(true, Ordering::SeqCst) {
            let _ = flush_tx.send(());
        }
    }

    fn load_xcursor_images(&mut self, icon: &str) -> Option<&Vec<Image>> {
        if self.cursor_images.contains_key(icon) {
            return self.cursor_images.get(icon);
        }

        let images = self
            .cursor_theme
            .load_icon(icon)
            .and_then(|path| {
                let mut file = std::fs::File::open(path).ok()?;
                let mut data = Vec::new();
                file.read_to_end(&mut data).ok()?;
                parse_xcursor(&data)
            })
            .unwrap_or_default();

        self.cursor_images.insert(icon.to_string(), images);
        self.cursor_images.get(icon)
    }

    fn pick_nearest_image<'a>(images: &'a [Image], target_size: u32) -> Option<&'a Image> {
        let nearest = images
            .iter()
            .min_by_key(|img| (target_size as i32 - img.size as i32).abs())?;

        // If the cursor is animated, multiple frames share width/height.
        // We don't animate yet; pick the first frame of the nearest size.
        images
            .iter()
            .find(|img| img.width == nearest.width && img.height == nearest.height)
    }

    fn cursor_bitmap(&mut self, kind: StdCursorKind, scale: u32) -> Option<CursorBitmap> {
        let key = (kind, scale);
        if let Some(cached) = self.cursor_cache.get(&key) {
            return Some(cached.clone());
        }

        let target_size = self.cursor_size.saturating_mul(scale.max(1));

        for &name in cursor_candidates(kind) {
            let images = self.load_xcursor_images(name)?;
            if images.is_empty() {
                continue;
            }
            let img = Self::pick_nearest_image(images, target_size)?;
            if img.pixels_rgba.is_empty() || img.width == 0 || img.height == 0 {
                continue;
            }

            let buffer = MemoryRenderBuffer::from_slice(
                &img.pixels_rgba,
                Fourcc::Argb8888,
                (img.width as i32, img.height as i32),
                1,
                Transform::Normal,
                None,
            );
            let bitmap = CursorBitmap {
                buffer,
                xhot: img.xhot as i32,
                yhot: img.yhot as i32,
            };
            self.cursor_cache.insert(key, bitmap.clone());
            return Some(bitmap);
        }

        None
    }

    pub(super) fn request_render(&mut self) {
        self.needs_render = true;
    }

    pub(super) fn any_frame_pending(&self) -> bool {
        self.outputs.iter().any(|o| o.frame_pending)
    }

    /// Set the shared pending screencopy queue (called once after initialization).
    pub(super) fn set_screencopy_pending(
        &mut self,
        queue: crate::backend::wayland_udev::screencopy::PendingScreencopyQueue,
    ) {
        self.screencopy_pending = Some(queue);
    }

    /// Set the shared pending ext-image-copy-capture queue.
    pub(super) fn set_image_capture_pending(
        &mut self,
        queue: crate::backend::wayland_udev::image_copy_capture::PendingImageCaptureQueue,
    ) {
        self.image_capture_pending = Some(queue);
    }

    pub(super) fn set_capture_counters(
        &mut self,
        counters: std::sync::Arc<std::sync::Mutex<crate::backend::wayland::state::CaptureCounters>>,
    ) {
        self.capture_counters = Some(counters);
    }

    fn note_screencopy_fulfilled(
        counters: Option<
            &std::sync::Arc<std::sync::Mutex<crate::backend::wayland::state::CaptureCounters>>,
        >,
    ) {
        if let Some(counters) = counters {
            let mut counters = counters.lock_safe();
            counters.note_screencopy_fulfilled();
        }
    }

    fn note_screencopy_render_failed(
        counters: Option<
            &std::sync::Arc<std::sync::Mutex<crate::backend::wayland::state::CaptureCounters>>,
        >,
    ) {
        if let Some(counters) = counters {
            let mut counters = counters.lock_safe();
            counters.note_screencopy_render_failed("screencopy render-drain failure");
        }
    }

    fn note_image_capture_fulfilled(
        counters: Option<
            &std::sync::Arc<std::sync::Mutex<crate::backend::wayland::state::CaptureCounters>>,
        >,
    ) {
        if let Some(counters) = counters {
            let mut counters = counters.lock_safe();
            counters.note_image_copy_fulfilled();
        }
    }

    fn note_image_capture_render_failed(
        counters: Option<
            &std::sync::Arc<std::sync::Mutex<crate::backend::wayland::state::CaptureCounters>>,
        >,
    ) {
        if let Some(counters) = counters {
            let mut counters = counters.lock_safe();
            counters.note_image_copy_render_failed("image-copy render-drain failure");
        }
    }

    /// Get the size of the primary (first) output
    #[allow(dead_code)]
    pub(super) fn primary_output_size(&self) -> (u32, u32) {
        self.outputs
            .first()
            .map(|o| (o.mode_size.0 as u32, o.mode_size.1 as u32))
            .unwrap_or((1920, 1080))
    }

    /// Get the total bounding box size covering all outputs.
    pub(super) fn total_screen_size(&self) -> (u32, u32) {
        let w = self
            .outputs
            .iter()
            .map(|o| (o.origin.0 + o.mode_size.0).max(0) as u32)
            .max()
            .unwrap_or(1920);
        let h = self
            .outputs
            .iter()
            .map(|o| (o.origin.1 + o.mode_size.1).max(0) as u32)
            .max()
            .unwrap_or(1080);
        (w, h)
    }

    /// Run a closure with access to the raw GL context
    pub(super) fn with_renderer<F, R>(
        &mut self,
        f: F,
    ) -> Result<R, smithay::backend::renderer::gles::GlesError>
    where
        F: FnOnce(&smithay::backend::renderer::gles::ffi::Gles2) -> R,
    {
        self.renderer.with_context(f)
    }

    /// Run a closure with access to the GlesRenderer (for surface texture imports, etc.)
    pub(super) fn with_gles_renderer<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut GlesRenderer) -> R,
    {
        f(&mut self.renderer)
    }

    pub(super) fn request_screenshot(&mut self, path: std::path::PathBuf) {
        self.pending_screenshot = Some(path);
        self.needs_render = true;
    }

    pub(super) fn request_screenshot_region(
        &mut self,
        path: std::path::PathBuf,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
    ) {
        self.pending_screenshot_region = Some((path, x, y, w, h));
        self.needs_render = true;
    }

    /// Take the latest presentation time (returns None if not updated since last take).
    pub(super) fn take_presentation_time(&mut self) -> Option<std::time::Instant> {
        self.last_presentation_time.take()
    }

    /// Check if 10-bit rendering formats are available.
    pub(super) fn supports_10bit(&self) -> bool {
        self.dmabuf_render_formats()
            .iter()
            .any(|f| f.code == Fourcc::Argb2101010 || f.code == Fourcc::Xrgb2101010)
    }

    /// Query VRR capabilities for a given output (by index into self.outputs).
    pub(super) fn query_vrr_for_output(
        &mut self,
        output_idx: usize,
    ) -> Option<crate::backend::api::VrrCapabilities> {
        let output = self.outputs.get(output_idx)?;
        let crtc = output.crtc;
        let mgr = self.drm_output_manager.lock();
        let dev = mgr.device();
        let mut supported = false;
        let mut current_enabled = false;
        if let Ok(props) = dev.get_properties(crtc) {
            let (handles, values) = props.as_props_and_values();
            for (i, &prop_handle) in handles.iter().enumerate() {
                if let Ok(info) = dev.get_property(prop_handle) {
                    let name = info.name().to_str().unwrap_or("");
                    if name == "VRR_ENABLED" {
                        supported = true;
                        current_enabled = values[i] != 0;
                    }
                }
            }
        }
        let cfg = crate::config::CONFIG.load();
        let b = cfg.behavior();
        Some(crate::backend::api::VrrCapabilities {
            supported,
            current_enabled,
            min_refresh_hz: b.vrr_min_fps,
            max_refresh_hz: b.vrr_max_fps,
        })
    }

    /// Return the cached per-CRTC color pipeline capabilities for a given
    /// output. Caps are probed once at output init (see `KmsState::new`) and
    /// stored on `KmsOutputState.color_pipeline_caps`; this is a pure read.
    pub(super) fn query_color_pipeline_caps_for_output(
        &self,
        output_idx: usize,
    ) -> Option<crate::backend::api::KmsColorPipelineCaps> {
        self.outputs.get(output_idx)?.color_pipeline_caps.clone()
    }

    /// Set a single DRM object property. On atomic drivers this issues an atomic
    /// commit (probed with `TEST_ONLY` first); if the property cannot be set
    /// atomically (e.g. the legacy-only DPMS property on some drivers) it cleanly
    /// falls back to the legacy ioctl. The `TEST_ONLY` probe guarantees we never
    /// apply a partial/invalid atomic state, so this can never blank an output by
    /// committing an inconsistent modeset.
    fn set_drm_property<H>(
        dev: &DrmDevice,
        handle: H,
        prop: smithay::reexports::drm::control::property::Handle,
        value: u64,
    ) -> Result<(), String>
    where
        H: smithay::reexports::drm::control::ResourceHandle,
    {
        use smithay::reexports::drm::control::AtomicCommitFlags;
        use smithay::reexports::drm::control::atomic::AtomicModeReq;
        if dev.is_atomic() {
            let mut req = AtomicModeReq::new();
            req.add_raw_property(handle.into(), prop, value);
            if dev
                .atomic_commit(AtomicCommitFlags::TEST_ONLY, req.clone())
                .is_ok()
            {
                return dev
                    .atomic_commit(AtomicCommitFlags::empty(), req)
                    .map_err(|e| format!("DRM atomic_commit failed: {e:?}"));
            }
        }
        dev.set_property(handle, prop, value)
            .map_err(|e| format!("DRM set_property failed: {e:?}"))
    }

    /// Set VRR enabled/disabled for a given output (by index into self.outputs).
    pub(super) fn set_vrr_for_output(
        &mut self,
        output_idx: usize,
        enabled: bool,
    ) -> Result<(), String> {
        let output = self
            .outputs
            .get(output_idx)
            .ok_or("output index out of range")?;
        let crtc = output.crtc;
        let mgr = self.drm_output_manager.lock();
        let dev = mgr.device();
        if let Ok(props) = dev.get_properties(crtc) {
            let (handles, _values) = props.as_props_and_values();
            for &prop_handle in handles {
                if let Ok(info) = dev.get_property(prop_handle) {
                    if info.name().to_str() == Ok("VRR_ENABLED") {
                        return Self::set_drm_property(
                            dev,
                            crtc,
                            prop_handle,
                            if enabled { 1 } else { 0 },
                        );
                    }
                }
            }
        }
        Err("VRR_ENABLED property not found on CRTC".to_string())
    }

    /// Push (or clear) the HDR_OUTPUT_METADATA connector property.
    ///
    /// Pass `Some(&blob)` (32-byte CTA-861.3 HDR Static Metadata) to put the
    /// display into HDR mode, or `None` to revert to SDR (blob_id = 0).
    /// The created blob is not destroyed — kernel cleans it up at FD close.
    /// Per-output blob churn is tiny (config changes are rare), so the leak is
    /// acceptable until/unless we add bookkeeping.
    pub(super) fn set_hdr_metadata_for_output(
        &mut self,
        output_idx: usize,
        blob: Option<&[u8; 32]>,
    ) -> Result<(), String> {
        let output = self
            .outputs
            .get(output_idx)
            .ok_or("output index out of range")?;
        let conn_handle = output.connector;
        let mgr = self.drm_output_manager.lock();
        let dev = mgr.device();

        let blob_id: u64 = if let Some(bytes) = blob {
            let v = dev
                .create_property_blob(bytes)
                .map_err(|e| format!("create_property_blob failed: {e:?}"))?;
            match v {
                smithay::reexports::drm::control::property::Value::Blob(id) => id,
                _ => return Err("create_property_blob returned non-Blob value".to_string()),
            }
        } else {
            0
        };

        if let Ok(props) = dev.get_properties(conn_handle) {
            let (handles, _values) = props.as_props_and_values();
            for &prop_handle in handles {
                if let Ok(info) = dev.get_property(prop_handle) {
                    if info.name().to_str() == Ok("HDR_OUTPUT_METADATA") {
                        return Self::set_drm_property(dev, conn_handle, prop_handle, blob_id);
                    }
                }
            }
        }
        Err("HDR_OUTPUT_METADATA property not found on connector".to_string())
    }

    pub(super) fn output_index_by_name(&self, name: &str) -> Option<usize> {
        self.outputs.iter().position(|o| o.output.name() == name)
    }

    pub(super) fn set_dpms_for_output(
        &mut self,
        output_idx: usize,
        on: bool,
    ) -> Result<(), String> {
        let output = self
            .outputs
            .get(output_idx)
            .ok_or("output index out of range")?;
        let conn_handle = output.connector;
        // When powering off, drop any installed GAMMA_LUT and free its blob
        // so the CRTC isn't carrying stale color state while blanked; the next
        // render_if_needed pass after power-on will reinstall via
        // refresh_color_pipeline_offload.
        if !on && self.outputs[output_idx].installed_gamma_lut.is_some() {
            // Best-effort: log + continue. DPMS itself is more important than
            // a clean LUT teardown.
            if let Err(e) = self.uninstall_gamma_lut(output_idx) {
                log::debug!(
                    "[kms-cm] DPMS-off LUT teardown failed on {}: {e}",
                    self.outputs[output_idx].output_name
                );
            }
        }
        if !on && self.outputs[output_idx].installed_ctm.is_some() {
            if let Err(e) = self.uninstall_ctm(output_idx) {
                log::debug!(
                    "[kms-cm] DPMS-off CTM teardown failed on {}: {e}",
                    self.outputs[output_idx].output_name
                );
            }
        }
        let mgr = self.drm_output_manager.lock();
        let dev = mgr.device();
        let mut result: Result<(), String> =
            Err("DPMS property not found on connector".to_string());
        if let Ok(props) = dev.get_properties(conn_handle) {
            let (handles, _values) = props.as_props_and_values();
            for &prop_handle in handles {
                if let Ok(info) = dev.get_property(prop_handle) {
                    if info.name().to_str() == Ok("DPMS") {
                        let val = if on { 0 } else { 3 }; // 0=On, 3=Off
                        result = Self::set_drm_property(dev, conn_handle, prop_handle, val);
                        break;
                    }
                }
            }
        }
        drop(mgr);
        // Track DPMS state only on success — refresh_color_pipeline_offload
        // reads dpms_off to decide whether to skip the output. If we wrote
        // here on a failed set, the next refresh would either re-install the
        // LUT on a powered-down CRTC or skip a still-powered-on one.
        if result.is_ok() {
            self.outputs[output_idx].dpms_off = !on;
        }
        result
    }

    // ============================================================
    // KMS color pipeline activation (GAMMA_LUT)
    // ============================================================

    /// Push a `GAMMA_LUT` blob for `tf` to the output's CRTC. Creates a fresh
    /// blob, atomically sets the prop, then `destroy_property_blob`s any
    /// previously-installed blob for the same output. Stores
    /// `(blob_id, tf)` on `KmsOutputState.installed_gamma_lut`.
    pub(super) fn install_gamma_lut(
        &mut self,
        output_idx: usize,
        tf: crate::backend::wayland_udev::color_pipeline::TransferKind,
    ) -> Result<(), String> {
        let output = self
            .outputs
            .get(output_idx)
            .ok_or("output index out of range")?;
        let crtc = output.crtc;
        let caps = output
            .color_pipeline_caps
            .as_ref()
            .ok_or("no color pipeline caps cached for output")?;
        if !caps.gamma_lut_supported {
            return Err("CRTC does not advertise GAMMA_LUT".to_string());
        }
        let size = caps.gamma_lut_size as usize;
        if size < 2 {
            return Err(format!("GAMMA_LUT_SIZE={size} is below the minimum of 2"));
        }
        let old_blob = output.installed_gamma_lut.map(|(id, _)| id);

        let mut lut = crate::backend::wayland_udev::color_pipeline::build_gamma_lut(tf, size);
        let mgr = self.drm_output_manager.lock();
        let dev = mgr.device();
        // drm 0.14's `create_property_blob<T: Sized>` uses `size_of::<T>()` and
        // can't accept a variable-length slice. Smithay solves this in
        // PlaneDamageClips by calling `drm_ffi::mode::create_property_blob`
        // directly on a `&mut [u8]` view of the array.
        let new_blob_id: u64 = {
            use std::os::unix::io::AsFd;
            let bytes = unsafe {
                std::slice::from_raw_parts_mut(
                    lut.as_mut_ptr() as *mut u8,
                    std::mem::size_of::<crate::backend::wayland_udev::color_pipeline::DrmColorLut>(
                    ) * lut.len(),
                )
            };
            let blob = drm_ffi::mode::create_property_blob(dev.as_fd(), bytes)
                .map_err(|e| format!("create_property_blob(GAMMA_LUT) failed: {e:?}"))?;
            u64::from(blob.blob_id)
        };

        // Locate GAMMA_LUT property handle on the CRTC and set it.
        let mut set_result: Result<(), String> =
            Err("GAMMA_LUT property not found on CRTC".to_string());
        if let Ok(props) = dev.get_properties(crtc) {
            let (handles, _values) = props.as_props_and_values();
            for &prop_handle in handles {
                if let Ok(info) = dev.get_property(prop_handle) {
                    if info.name().to_str() == Ok("GAMMA_LUT") {
                        set_result = Self::set_drm_property(dev, crtc, prop_handle, new_blob_id);
                        break;
                    }
                }
            }
        }
        if let Err(e) = &set_result {
            // Failed atomic commit → free the just-created blob, leave state untouched.
            let _ = dev.destroy_property_blob(new_blob_id);
            return Err(e.clone());
        }
        // Replace old blob (if any) only after the new one is live.
        if let Some(old) = old_blob {
            let _ = dev.destroy_property_blob(old);
        }
        drop(mgr);

        self.outputs[output_idx].installed_gamma_lut = Some((new_blob_id, tf));
        log::info!(
            "[kms-cm] installed GAMMA_LUT on {} (size={size}, tf={tf:?})",
            self.outputs[output_idx].output_name,
        );
        Ok(())
    }

    /// Zero the output's `GAMMA_LUT` (revert to driver default) and destroy
    /// any tracked blob. No-op when nothing is installed.
    pub(super) fn uninstall_gamma_lut(&mut self, output_idx: usize) -> Result<(), String> {
        let output = self
            .outputs
            .get(output_idx)
            .ok_or("output index out of range")?;
        let blob = match output.installed_gamma_lut {
            Some((id, _)) => id,
            None => return Ok(()),
        };
        let crtc = output.crtc;
        let mgr = self.drm_output_manager.lock();
        let dev = mgr.device();
        // Set GAMMA_LUT to 0 first so the CRTC reverts before the blob is
        // destroyed. Best-effort: even if this fails we still try to free the
        // blob (a leaked blob is preferable to a dangling kernel reference).
        let mut prop_result: Result<(), String> =
            Err("GAMMA_LUT property not found on CRTC".to_string());
        if let Ok(props) = dev.get_properties(crtc) {
            let (handles, _values) = props.as_props_and_values();
            for &prop_handle in handles {
                if let Ok(info) = dev.get_property(prop_handle) {
                    if info.name().to_str() == Ok("GAMMA_LUT") {
                        prop_result = Self::set_drm_property(dev, crtc, prop_handle, 0);
                        break;
                    }
                }
            }
        }
        let _ = dev.destroy_property_blob(blob);
        drop(mgr);

        self.outputs[output_idx].installed_gamma_lut = None;
        let name = &self.outputs[output_idx].output_name;
        if let Err(e) = &prop_result {
            log::debug!("[kms-cm] uninstall GAMMA_LUT on {name}: prop clear failed: {e}");
        } else {
            log::info!("[kms-cm] uninstalled GAMMA_LUT on {name}");
        }
        Ok(())
    }

    /// Install a 3×3 CTM (color transform matrix) on the CRTC. Mirrors
    /// `install_gamma_lut`: variable-length blob via `drm_ffi::mode::
    /// create_property_blob`, atomic prop bind, free-on-failure, replace-old-
    /// after-success. 3.3b only ever passes `IDENTITY_CTM`.
    pub(super) fn install_ctm(
        &mut self,
        output_idx: usize,
        matrix: [f32; 9],
    ) -> Result<(), String> {
        let output = self
            .outputs
            .get(output_idx)
            .ok_or("output index out of range")?;
        let crtc = output.crtc;
        let caps = output
            .color_pipeline_caps
            .as_ref()
            .ok_or("no color pipeline caps cached for output")?;
        if !caps.ctm_supported {
            return Err("CRTC does not advertise CTM".to_string());
        }
        let old_blob = output.installed_ctm;

        let mut ctm = crate::backend::wayland_udev::color_pipeline::build_ctm(matrix);
        let mgr = self.drm_output_manager.lock();
        let dev = mgr.device();
        let new_blob_id: u64 = {
            use std::os::unix::io::AsFd;
            let bytes = unsafe {
                std::slice::from_raw_parts_mut(
                    &mut ctm as *mut _ as *mut u8,
                    std::mem::size_of::<crate::backend::wayland_udev::color_pipeline::DrmColorCtm>(
                    ),
                )
            };
            let blob = drm_ffi::mode::create_property_blob(dev.as_fd(), bytes)
                .map_err(|e| format!("create_property_blob(CTM) failed: {e:?}"))?;
            u64::from(blob.blob_id)
        };

        let mut set_result: Result<(), String> = Err("CTM property not found on CRTC".to_string());
        if let Ok(props) = dev.get_properties(crtc) {
            let (handles, _values) = props.as_props_and_values();
            for &prop_handle in handles {
                if let Ok(info) = dev.get_property(prop_handle) {
                    if info.name().to_str() == Ok("CTM") {
                        set_result = Self::set_drm_property(dev, crtc, prop_handle, new_blob_id);
                        break;
                    }
                }
            }
        }
        if let Err(e) = &set_result {
            let _ = dev.destroy_property_blob(new_blob_id);
            return Err(e.clone());
        }
        if let Some(old) = old_blob {
            let _ = dev.destroy_property_blob(old);
        }
        drop(mgr);

        self.outputs[output_idx].installed_ctm = Some(new_blob_id);
        log::info!(
            "[kms-cm] installed CTM on {}",
            self.outputs[output_idx].output_name,
        );
        Ok(())
    }

    /// Zero the output's `CTM` and destroy any tracked blob. No-op when
    /// nothing is installed.
    pub(super) fn uninstall_ctm(&mut self, output_idx: usize) -> Result<(), String> {
        let output = self
            .outputs
            .get(output_idx)
            .ok_or("output index out of range")?;
        let blob = match output.installed_ctm {
            Some(id) => id,
            None => return Ok(()),
        };
        let crtc = output.crtc;
        let mgr = self.drm_output_manager.lock();
        let dev = mgr.device();
        let mut prop_result: Result<(), String> = Err("CTM property not found on CRTC".to_string());
        if let Ok(props) = dev.get_properties(crtc) {
            let (handles, _values) = props.as_props_and_values();
            for &prop_handle in handles {
                if let Ok(info) = dev.get_property(prop_handle) {
                    if info.name().to_str() == Ok("CTM") {
                        prop_result = Self::set_drm_property(dev, crtc, prop_handle, 0);
                        break;
                    }
                }
            }
        }
        let _ = dev.destroy_property_blob(blob);
        drop(mgr);

        self.outputs[output_idx].installed_ctm = None;
        let name = &self.outputs[output_idx].output_name;
        if let Err(e) = &prop_result {
            log::debug!("[kms-cm] uninstall CTM on {name}: prop clear failed: {e}");
        } else {
            log::info!("[kms-cm] uninstalled CTM on {name}");
        }
        Ok(())
    }

    /// Returns the per-frame color-pipeline decision. HW offload is
    /// all-or-nothing across active outputs because the compositor renders a
    /// single global FBO that feeds every output.
    pub(super) fn refresh_color_pipeline_offload(
        &mut self,
        state: &crate::backend::wayland::state::JwmWaylandState,
    ) -> ColorPipelineDecision {
        use crate::backend::wayland_udev::color_pipeline::TransferKind;

        let behavior = crate::config::CONFIG.load();
        let gate_on = behavior.behavior().kms_color_pipeline_offload;
        // CTM (3.3c) transforms linear-RGB values, so it requires the FBO to
        // hold linear data — i.e. scene-linear compositing on. With it off
        // the FBO is OETF-encoded and a per-output gamut matrix would scramble
        // the data. GAMMA_LUT (3.2) is independent: it just applies the
        // output OETF to whatever is in the FBO.
        let ctm_gate_on = gate_on && behavior.behavior().scene_linear_compositing;
        drop(behavior);
        let n = self.outputs.len();

        // Precompute participation once — `participating` is read in many
        // passes below and each pass also takes `&mut self` to call
        // install/uninstall, so a closure borrowing `self.outputs` can't
        // coexist with the mutable calls.
        let participating: Vec<bool> = self
            .outputs
            .iter()
            .map(|o| !o.dpms_off && !state.soft_disabled_outputs.contains(&o.output_name))
            .collect();

        let uniform_tf: Option<TransferKind> = {
            let mut tf: Option<TransferKind> = None;
            for i in 0..n {
                if !participating[i] {
                    continue;
                }
                match tf {
                    None => tf = Some(self.outputs[i].output_tf),
                    Some(t) if t != self.outputs[i].output_tf => {
                        tf = None;
                        break;
                    }
                    _ => {}
                }
            }
            tf
        };

        let shader_fallback = uniform_tf.unwrap_or(TransferKind::Srgb);
        let shader_tf = shader_fallback.shader_id();
        let shader_gamma = shader_fallback.gamma_for_shader();

        let mut decision = ColorPipelineDecision {
            hw_encode_active: false,
            shader_tf,
            shader_gamma,
            hw_ctm_active: false,
        };

        let Some(target) = uniform_tf.filter(|_| gate_on) else {
            for i in 0..n {
                if self.outputs[i].installed_gamma_lut.is_some() {
                    let _ = self.uninstall_gamma_lut(i);
                }
                if self.outputs[i].installed_ctm.is_some() {
                    let _ = self.uninstall_ctm(i);
                }
            }
            return decision;
        };

        // --- GAMMA_LUT activation: drop on non-participating, then cap-check
        // and install all-or-nothing across participating outputs.
        for i in 0..n {
            if !participating[i] && self.outputs[i].installed_gamma_lut.is_some() {
                let _ = self.uninstall_gamma_lut(i);
            }
        }

        let mut any_participating = false;
        let mut lut_capable = true;
        for i in 0..n {
            if !participating[i] {
                continue;
            }
            any_participating = true;
            let cap_ok = self.outputs[i]
                .color_pipeline_caps
                .as_ref()
                .map(|c| c.gamma_lut_supported && c.gamma_lut_size >= 256)
                .unwrap_or(false);
            if !cap_ok {
                lut_capable = false;
                break;
            }
        }
        if !any_participating || !lut_capable {
            for i in 0..n {
                if participating[i] && self.outputs[i].installed_gamma_lut.is_some() {
                    let _ = self.uninstall_gamma_lut(i);
                }
            }
        } else {
            let mut lut_install_failed = false;
            for i in 0..n {
                if !participating[i] {
                    continue;
                }
                if matches!(self.outputs[i].installed_gamma_lut, Some((_, t)) if t == target) {
                    continue;
                }
                if let Err(e) = self.install_gamma_lut(i, target) {
                    log::warn!(
                        "[kms-cm] LUT install on {} failed ({e}); rolling back frame's LUTs",
                        self.outputs[i].output_name,
                    );
                    for j in 0..n {
                        if self.outputs[j].installed_gamma_lut.is_some() {
                            let _ = self.uninstall_gamma_lut(j);
                        }
                    }
                    lut_install_failed = true;
                    break;
                }
            }
            decision.hw_encode_active = !lut_install_failed;
        }

        // --- CTM activation: independent of LUT. Drop on non-participating,
        // verify ctm_supported AND scene-linear gate AND ctm gate across
        // participants, install per-output `output_ctm` (sRGB → output
        // primaries) all-or-nothing. When `hw_ctm_active`, the per-surface
        // ColorTransform pass in backend.rs targets sRGB primaries so the
        // FBO is uniform-sRGB and each CRTC's CTM converts to its native
        // primaries at scanout.
        for i in 0..n {
            if !participating[i] && self.outputs[i].installed_ctm.is_some() {
                let _ = self.uninstall_ctm(i);
            }
        }

        let mut ctm_capable = any_participating && ctm_gate_on;
        for i in 0..n {
            if !participating[i] {
                continue;
            }
            let cap_ok = self.outputs[i]
                .color_pipeline_caps
                .as_ref()
                .map(|c| c.ctm_supported)
                .unwrap_or(false);
            if !cap_ok {
                ctm_capable = false;
                break;
            }
        }
        if !ctm_capable {
            for i in 0..n {
                if participating[i] && self.outputs[i].installed_ctm.is_some() {
                    let _ = self.uninstall_ctm(i);
                }
            }
        } else {
            let mut ctm_install_failed = false;
            for i in 0..n {
                if !participating[i] || self.outputs[i].installed_ctm.is_some() {
                    continue;
                }
                let matrix = self.outputs[i].output_ctm;
                if let Err(e) = self.install_ctm(i, matrix) {
                    log::warn!(
                        "[kms-cm] CTM install on {} failed ({e}); rolling back frame's CTMs",
                        self.outputs[i].output_name,
                    );
                    for j in 0..n {
                        if self.outputs[j].installed_ctm.is_some() {
                            let _ = self.uninstall_ctm(j);
                        }
                    }
                    ctm_install_failed = true;
                    break;
                }
            }
            decision.hw_ctm_active = !ctm_install_failed;
        }

        decision
    }

    pub(super) fn set_gamma_for_output(
        &mut self,
        output_idx: usize,
        gamma_size: u32,
        ramp: &[u16],
    ) -> Result<(), String> {
        let output = self
            .outputs
            .get(output_idx)
            .ok_or("output index out of range")?;
        let crtc = output.crtc;
        let mgr = self.drm_output_manager.lock();
        let dev = mgr.device();

        let sz = gamma_size as usize;
        let expected_len = sz * 3;
        if ramp.len() != expected_len {
            return Err(format!(
                "gamma ramp length mismatch: got {} expected {}",
                ramp.len(),
                expected_len
            ));
        }

        let red = &ramp[..sz];
        let green = &ramp[sz..2 * sz];
        let blue = &ramp[2 * sz..3 * sz];

        dev.set_gamma(crtc, red, green, blue)
            .map_err(|e| format!("DRM set_gamma failed: {e:?}"))
    }

    /// Apply a client-requested output configuration (wlr-output-management).
    ///
    /// `mode` is `(width, height, refresh_mhz)`; a `None` field keeps the current
    /// value. A mode change performs a real DRM modeset via [`DrmOutput::use_mode`]
    /// and is the riskiest step (it can fail or, on broken hardware, blank the
    /// output); position/scale/transform only update the advertised wl_output
    /// state and the compositor-space origin.
    ///
    /// Safety:
    /// - Modeset is gated by `behavior.wlr_output_mgmt_allow_modeset` (default
    ///   false); when disabled, mode changes are dropped silently and the
    ///   non-modeset fields still apply.
    /// - On `DrmOutput::use_mode` failure we attempt a best-effort rollback to
    ///   the previously-active DRM mode so the output is not stranded mid-
    ///   modeset.
    pub(super) fn configure_output(
        &mut self,
        name: &str,
        mode: Option<(i32, i32, i32)>,
        position: Option<(i32, i32)>,
        transform: Option<i32>,
        scale: Option<f64>,
    ) -> Result<(), String> {
        let idx = self
            .output_index_by_name(name)
            .ok_or_else(|| format!("unknown output '{name}'"))?;

        // Resolve a DRM mode if a *different* mode was requested.
        let allow_modeset = crate::config::CONFIG
            .load()
            .behavior()
            .wlr_output_mgmt_allow_modeset;
        let mut prev_drm_mode: Option<smithay::reexports::drm::control::Mode> = None;
        let drm_mode = if let Some((w, h, refresh)) = mode {
            if !allow_modeset {
                // Defense-in-depth: build_changes should have rejected this
                // at validation time. If we reach here the gate was bypassed
                // and we MUST return Err so the client's succeeded() ack is
                // not sent over a silently-dropped mode change.
                return Err(format!(
                    "mode change to {w}x{h}@{refresh} for '{name}' rejected: \
                     behavior.wlr_output_mgmt_allow_modeset = false"
                ));
            } else {
                let conn = self.outputs[idx].connector;
                let mgr = self.drm_output_manager.lock();
                let info = mgr
                    .device()
                    .get_connector(conn, false)
                    .map_err(|e| format!("get_connector failed: {e:?}"))?;
                let found = info.modes().iter().copied().find(|m| {
                    let wl = WlMode::from(*m);
                    wl.size.w == w
                        && wl.size.h == h
                        && (refresh == 0 || (wl.refresh - refresh).abs() <= 200)
                });
                // Capture the currently-active DRM mode (not just the
                // smithay-advertised WlMode) so we can roll back on failure.
                let current_wl = self.outputs[idx].output.current_mode();
                if let Some(cur) = current_wl {
                    prev_drm_mode = info.modes().iter().copied().find(|m| {
                        let wl = WlMode::from(*m);
                        wl.size == cur.size && wl.refresh == cur.refresh
                    });
                }
                drop(mgr);
                match found {
                    Some(m) if self.outputs[idx].output.current_mode() != Some(WlMode::from(m)) => {
                        Some(m)
                    }
                    Some(_) => None, // already the current mode; skip the modeset
                    None => {
                        return Err(format!(
                            "requested mode {w}x{h}@{refresh} not available on '{name}'"
                        ));
                    }
                }
            }
        } else {
            None
        };

        // Riskiest step first: perform the DRM modeset before advertising it.
        if let Some(m) = drm_mode {
            let elements: DrmOutputRenderElements<GlesRenderer, SolidColorRenderElement> =
                DrmOutputRenderElements::default();
            if let Err(e) = self.outputs[idx]
                .drm_output
                .use_mode(m, &mut self.renderer, &elements)
            {
                // Best-effort rollback to the previous mode so the output is not
                // left in an undefined state. If rollback also fails, the output
                // may be black; the user will need to physically replug or
                // re-trigger DPMS via output-power-management.
                let primary_err = format!("DRM use_mode failed: {e:?}");
                if let Some(prev) = prev_drm_mode {
                    let rollback: DrmOutputRenderElements<GlesRenderer, SolidColorRenderElement> =
                        DrmOutputRenderElements::default();
                    match self.outputs[idx]
                        .drm_output
                        .use_mode(prev, &mut self.renderer, &rollback)
                    {
                        Ok(()) => {
                            log::warn!(
                                "[output-mgmt] '{name}': modeset failed, rolled back to previous mode ({primary_err})"
                            );
                        }
                        Err(rollback_err) => {
                            log::error!(
                                "[output-mgmt] '{name}': modeset failed AND rollback failed: \
                                 primary={primary_err}, rollback={rollback_err:?}"
                            );
                        }
                    }
                } else {
                    log::error!(
                        "[output-mgmt] '{name}': modeset failed, no previous mode captured for rollback ({primary_err})"
                    );
                }
                return Err(primary_err);
            }
            self.outputs[idx].mode_size = (m.size().0 as i32, m.size().1 as i32);
        }

        // Advertise updated state to wl_output clients and update layout origin.
        let new_wl_mode = drm_mode.map(WlMode::from);
        let new_transform = transform.map(wl_transform_to_smithay);
        let new_scale = scale.map(smithay::output::Scale::Fractional);
        let new_loc = position.map(Point::from);
        self.outputs[idx].output.change_current_state(
            new_wl_mode,
            new_transform,
            new_scale,
            new_loc,
        );
        if let Some((x, y)) = position {
            self.outputs[idx].origin = (x, y);
        }

        self.needs_render = true;
        Ok(())
    }

    /// Render all elements to an offscreen buffer and save as PNG.
    /// Split out as a free-standing function so it can borrow `self.renderer`
    /// without conflicting with the mutable borrow on `self.outputs`.
    #[allow(dead_code)]
    fn capture_screenshot_offscreen_impl(
        renderer: &mut GlesRenderer,
        width: i32,
        height: i32,
        elements: &[KmsRenderElement],
        path: &std::path::Path,
    ) {
        let size: Size<i32, BufferCoord> = (width, height).into();

        // 1. Create offscreen renderbuffer
        let mut renderbuffer: GlesRenderbuffer =
            match Offscreen::create_buffer(renderer, Fourcc::Abgr8888, size) {
                Ok(rb) => rb,
                Err(e) => {
                    log::error!("[screenshot] create offscreen buffer failed: {e:?}");
                    return;
                }
            };

        // 2. Bind the offscreen renderbuffer
        let mut target = match renderer.bind(&mut renderbuffer) {
            Ok(t) => t,
            Err(e) => {
                log::error!("[screenshot] bind offscreen failed: {e:?}");
                return;
            }
        };

        // 3. Render all elements using OutputDamageTracker
        let phys_size: smithay::utils::Size<i32, Physical> = (width, height).into();
        let mut damage_tracker =
            OutputDamageTracker::new(phys_size, Scale::from(1.0f64), Transform::Normal);
        let clear_color = smithay::backend::renderer::Color32F::new(0.1, 0.15, 0.25, 1.0);
        if let Err(e) = damage_tracker.render_output(
            renderer,
            &mut target,
            0, // age=0 forces full redraw
            elements,
            clear_color,
        ) {
            log::error!("[screenshot] render_output failed: {e:?}");
            return;
        }

        // 4. Read pixels back via ExportMem
        let region = Rectangle::from_size(size);
        let mapping = match renderer.copy_framebuffer(&target, region, Fourcc::Abgr8888) {
            Ok(m) => m,
            Err(e) => {
                log::error!("[screenshot] copy_framebuffer failed: {e:?}");
                return;
            }
        };

        let pixels = match renderer.map_texture(&mapping) {
            Ok(p) => p,
            Err(e) => {
                log::error!("[screenshot] map_texture failed: {e:?}");
                return;
            }
        };

        // 5. Save as PNG (pixels are ABGR8888 / RGBA from GL perspective)
        if let Err(e) = save_rgba_png(path, width as u32, height as u32, pixels) {
            log::error!("[screenshot] save PNG failed: {e}");
        } else {
            log::info!("[screenshot] saved to {}", path.display());
        }
    }

    /// Render to offscreen, then crop a region and save as PNG.
    fn capture_screenshot_region_impl(
        renderer: &mut GlesRenderer,
        width: i32,
        height: i32,
        elements: &[KmsRenderElement],
        path: &std::path::Path,
        rx: i32,
        ry: i32,
        rw: u32,
        rh: u32,
    ) {
        let size: Size<i32, BufferCoord> = (width, height).into();

        let mut renderbuffer: GlesRenderbuffer =
            match Offscreen::create_buffer(renderer, Fourcc::Abgr8888, size) {
                Ok(rb) => rb,
                Err(e) => {
                    log::error!("[screenshot-region] create offscreen buffer failed: {e:?}");
                    return;
                }
            };

        let mut target = match renderer.bind(&mut renderbuffer) {
            Ok(t) => t,
            Err(e) => {
                log::error!("[screenshot-region] bind offscreen failed: {e:?}");
                return;
            }
        };

        let phys_size: smithay::utils::Size<i32, Physical> = (width, height).into();
        let mut damage_tracker =
            OutputDamageTracker::new(phys_size, Scale::from(1.0f64), Transform::Normal);
        let clear_color = smithay::backend::renderer::Color32F::new(0.1, 0.15, 0.25, 1.0);
        if let Err(e) =
            damage_tracker.render_output(renderer, &mut target, 0, elements, clear_color)
        {
            log::error!("[screenshot-region] render_output failed: {e:?}");
            return;
        }

        // Read full framebuffer
        let full_region = Rectangle::from_size(size);
        let mapping = match renderer.copy_framebuffer(&target, full_region, Fourcc::Abgr8888) {
            Ok(m) => m,
            Err(e) => {
                log::error!("[screenshot-region] copy_framebuffer failed: {e:?}");
                return;
            }
        };
        let full_pixels = match renderer.map_texture(&mapping) {
            Ok(p) => p,
            Err(e) => {
                log::error!("[screenshot-region] map_texture failed: {e:?}");
                return;
            }
        };

        // Crop the region from the full pixel buffer.
        // Pixels are in top-to-bottom order (smithay flips Y in projection).
        let x = rx.max(0) as u32;
        let y = ry.max(0) as u32;
        let cw = rw.min((width as u32).saturating_sub(x));
        let ch = rh.min((height as u32).saturating_sub(y));
        if cw == 0 || ch == 0 {
            log::warn!("[screenshot-region] region is empty");
            return;
        }

        let full_row_bytes = (width as u32 * 4) as usize;
        let crop_row_bytes = (cw * 4) as usize;
        let mut cropped = vec![0u8; (cw * ch * 4) as usize];
        for row in 0..ch as usize {
            let src_offset = (y as usize + row) * full_row_bytes + (x as usize * 4);
            let dst_offset = row * crop_row_bytes;
            cropped[dst_offset..dst_offset + crop_row_bytes]
                .copy_from_slice(&full_pixels[src_offset..src_offset + crop_row_bytes]);
        }

        if let Err(e) = save_rgba_png(path, cw, ch, &cropped) {
            log::error!("[screenshot-region] save PNG failed: {e}");
        } else {
            log::info!(
                "[screenshot-region] saved to {} ({}x{} at {},{})",
                path.display(),
                cw,
                ch,
                x,
                y
            );
        }
    }

    /// Fulfill pending wlr-screencopy copy requests for a given output.
    ///
    /// This renders the given elements to an offscreen buffer and copies the
    /// RGBA pixels into each waiting client's wl_shm buffer, then sends the
    /// `flags` + `ready` events on the screencopy frame.
    /// Get a reusable offscreen renderbuffer of the requested size, recreating
    /// the cached one only when the dimensions change. Returns `None` if creation
    /// fails. The buffer lives in `cache` so consecutive frames (continuous
    /// capture) avoid reallocating a full-screen GPU buffer every frame.
    fn screencopy_offscreen_buffer<'a>(
        renderer: &mut GlesRenderer,
        cache: &'a mut Option<(i32, i32, GlesRenderbuffer)>,
        width: i32,
        height: i32,
    ) -> Option<&'a mut GlesRenderbuffer> {
        let needs_new = !matches!(cache, Some((w, h, _)) if *w == width && *h == height);
        if needs_new {
            let size: Size<i32, BufferCoord> = (width, height).into();
            match Offscreen::create_buffer(renderer, Fourcc::Abgr8888, size) {
                Ok(rb) => *cache = Some((width, height, rb)),
                Err(e) => {
                    log::error!("[screencopy] create offscreen buffer failed: {e:?}");
                    *cache = None;
                    return None;
                }
            }
        }
        cache.as_mut().map(|(_, _, rb)| rb)
    }

    /// Render `elements` directly into a client-provided dmabuf buffer, avoiding
    /// the offscreen + GPU readback + R/B-swap CPU copy of the SHM path. The
    /// renderer binds the dmabuf as the render target and the GPU writes the
    /// composited frame straight into the client's buffer. We wait on the
    /// resulting `SyncPoint` so the GPU work is complete before the caller signals
    /// `ready`. Returns `false` (caller fails the frame) if bind/render fails.
    fn render_into_client_dmabuf(
        renderer: &mut GlesRenderer,
        dmabuf: &Dmabuf,
        width: i32,
        height: i32,
        elements: &[KmsRenderElement],
        clear: smithay::backend::renderer::Color32F,
    ) -> bool {
        let mut dmabuf = dmabuf.clone();
        let mut target = match renderer.bind(&mut dmabuf) {
            Ok(t) => t,
            Err(e) => {
                log::error!("[capture/dmabuf] bind client dmabuf failed: {e:?}");
                return false;
            }
        };
        let phys: Size<i32, Physical> = (width, height).into();
        let mut dt = OutputDamageTracker::new(phys, Scale::from(1.0f64), Transform::Normal);
        match dt.render_output(renderer, &mut target, 0, elements, clear) {
            Ok(res) => {
                let _ = res.sync.wait();
                true
            }
            Err(e) => {
                log::error!("[capture/dmabuf] render_output failed: {e:?}");
                false
            }
        }
    }

    fn fulfill_screencopy_frames(
        renderer: &mut GlesRenderer,
        offscreen_cache: &mut Option<(i32, i32, GlesRenderbuffer)>,
        output: &Output,
        width: i32,
        height: i32,
        elements: &[KmsRenderElement],
        pending: &crate::backend::wayland_udev::screencopy::PendingScreencopyQueue,
        counters: Option<
            &std::sync::Arc<std::sync::Mutex<crate::backend::wayland::state::CaptureCounters>>,
        >,
    ) {
        use smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_frame_v1;
        use smithay::wayland::shm::with_buffer_contents_mut;

        let frames: Vec<crate::backend::wayland_udev::screencopy::PendingScreencopyFrame> = {
            let mut queue = pending.lock_safe();
            // Drain frames that match this output.
            let mut matching = Vec::new();
            let mut remaining = Vec::new();
            for f in queue.drain(..) {
                if f.output == *output {
                    matching.push(f);
                } else {
                    remaining.push(f);
                }
            }
            *queue = remaining;
            matching
        };

        if frames.is_empty() {
            return;
        }

        log::debug!(
            "[screencopy] fulfilling {} frames for output {}",
            frames.len(),
            output.name(),
        );

        // Split out dmabuf-backed frames and render directly into them (no GPU
        // readback, no CPU R/B swap). The rest keep the SHM offscreen path below.
        let mut shm_frames = Vec::with_capacity(frames.len());
        for f in frames {
            let dmabuf = get_dmabuf(&f.buffer).ok().cloned();
            match dmabuf {
                Some(dmabuf) => {
                    // We render the full output into the client buffer; sub-region
                    // capture into a dmabuf is unsupported, so fail those (rare) and
                    // let the client fall back to SHM.
                    if f.region.is_some() {
                        log::warn!("[screencopy] region capture into dmabuf unsupported");
                        Self::note_screencopy_render_failed(counters);
                        f.frame.failed();
                        continue;
                    }
                    let clear = smithay::backend::renderer::Color32F::new(0.1, 0.15, 0.25, 1.0);
                    if Self::render_into_client_dmabuf(
                        renderer, &dmabuf, width, height, elements, clear,
                    ) {
                        f.frame.flags(zwlr_screencopy_frame_v1::Flags::empty());
                        if f.with_damage {
                            f.frame.damage(0, 0, width as u32, height as u32);
                        }
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default();
                        let tv_sec = now.as_secs();
                        f.frame.ready(
                            (tv_sec >> 32) as u32,
                            (tv_sec & 0xFFFFFFFF) as u32,
                            now.subsec_nanos(),
                        );
                        Self::note_screencopy_fulfilled(counters);
                    } else {
                        Self::note_screencopy_render_failed(counters);
                        f.frame.failed();
                    }
                }
                None => shm_frames.push(f),
            }
        }
        let mut frames = shm_frames;
        if frames.is_empty() {
            return;
        }

        // Render to a cached offscreen buffer (reused across frames).
        let size: Size<i32, BufferCoord> = (width, height).into();
        let renderbuffer =
            match Self::screencopy_offscreen_buffer(renderer, offscreen_cache, width, height) {
                Some(rb) => rb,
                None => {
                    for f in &frames {
                        Self::note_screencopy_render_failed(counters);
                        f.frame.failed();
                    }
                    return;
                }
            };

        let mut target = match renderer.bind(renderbuffer) {
            Ok(t) => t,
            Err(e) => {
                log::error!("[screencopy] bind offscreen failed: {e:?}");
                for f in &frames {
                    Self::note_screencopy_render_failed(counters);
                    f.frame.failed();
                }
                return;
            }
        };

        let phys_size: smithay::utils::Size<i32, Physical> = (width, height).into();
        let mut damage_tracker =
            OutputDamageTracker::new(phys_size, Scale::from(1.0f64), Transform::Normal);
        let clear_color = smithay::backend::renderer::Color32F::new(0.1, 0.15, 0.25, 1.0);
        if let Err(e) =
            damage_tracker.render_output(renderer, &mut target, 0, elements, clear_color)
        {
            log::error!("[screencopy] render_output failed: {e:?}");
            for f in &frames {
                Self::note_screencopy_render_failed(counters);
                f.frame.failed();
            }
            return;
        }

        // Read back pixels (ABGR8888 from GL).
        let region = Rectangle::from_size(size);
        let mapping = match renderer.copy_framebuffer(&target, region, Fourcc::Abgr8888) {
            Ok(m) => m,
            Err(e) => {
                log::error!("[screencopy] copy_framebuffer failed: {e:?}");
                for f in &frames {
                    Self::note_screencopy_render_failed(counters);
                    f.frame.failed();
                }
                return;
            }
        };

        let pixels = match renderer.map_texture(&mapping) {
            Ok(p) => p,
            Err(e) => {
                log::error!("[screencopy] map_texture failed: {e:?}");
                for f in &frames {
                    Self::note_screencopy_render_failed(counters);
                    f.frame.failed();
                }
                return;
            }
        };

        // GL gives us ABGR (little-endian RGBA bytes).
        // wl_shm ARGB8888 is native-endian: on little-endian it's [B, G, R, A] in memory.
        // GL ABGR8888 is [R, G, B, A] in memory.
        // We need to convert RGBA → BGRA (swap R and B channels).

        for frame_info in frames.drain(..) {
            let copy_result =
                with_buffer_contents_mut(&frame_info.buffer, |ptr, pool_len, buf_data| {
                    let buf_offset = buf_data.offset as usize;
                    let buf_stride = buf_data.stride as usize;
                    let buf_h = buf_data.height as usize;
                    let buf_w = buf_data.width as usize;

                    // Source region
                    let (src_x, src_y, src_w, src_h) =
                        if let Some((rx, ry, rw, rh)) = frame_info.region {
                            (rx as usize, ry as usize, rw as usize, rh as usize)
                        } else {
                            (0usize, 0usize, width as usize, height as usize)
                        };

                    let copy_h = src_h.min(buf_h);
                    let copy_w = src_w.min(buf_w);
                    let src_stride = width as usize * 4;

                    for row in 0..copy_h {
                        let src_row = src_y + row;
                        if src_row >= height as usize {
                            break;
                        }
                        let src_row_start = src_row * src_stride + src_x * 4;
                        let dst_row_start = buf_offset + row * buf_stride;

                        if dst_row_start + copy_w * 4 > pool_len {
                            break;
                        }

                        for col in 0..copy_w {
                            let si = src_row_start + col * 4;
                            let di = dst_row_start + col * 4;
                            if si + 3 >= pixels.len() {
                                break;
                            }
                            // ABGR (GL) = [R, G, B, A] in memory → ARGB8888 (shm) = [B, G, R, A] in memory
                            unsafe {
                                *ptr.add(di) = pixels[si + 2]; // B
                                *ptr.add(di + 1) = pixels[si + 1]; // G
                                *ptr.add(di + 2) = pixels[si]; // R
                                *ptr.add(di + 3) = pixels[si + 3]; // A
                            }
                        }
                    }
                });

            match copy_result {
                Ok(()) => {
                    // Send flags (no y-invert) then ready.
                    frame_info
                        .frame
                        .flags(zwlr_screencopy_frame_v1::Flags::empty());
                    // copy_with_damage requires a damage event before ready. We
                    // don't track per-frame damage for screencopy, so report the
                    // whole captured area as damaged.
                    if frame_info.with_damage {
                        let (dmg_w, dmg_h) = match frame_info.region {
                            Some((_, _, rw, rh)) => (rw as u32, rh as u32),
                            None => (width as u32, height as u32),
                        };
                        frame_info.frame.damage(0, 0, dmg_w, dmg_h);
                    }
                    // Timestamp: use current time.
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default();
                    let tv_sec = now.as_secs();
                    let tv_nsec = now.subsec_nanos();
                    frame_info.frame.ready(
                        (tv_sec >> 32) as u32,
                        (tv_sec & 0xFFFFFFFF) as u32,
                        tv_nsec,
                    );
                    log::debug!("[screencopy] frame ready for output {}", output.name());
                    Self::note_screencopy_fulfilled(counters);
                }
                Err(e) => {
                    log::warn!("[screencopy] buffer access failed: {e:?}");
                    Self::note_screencopy_render_failed(counters);
                    frame_info.frame.failed();
                }
            }
        }
    }

    /// Fulfill pending ext-image-copy-capture-v1 frames for `output`. Mirrors
    /// `fulfill_screencopy_frames` (render to offscreen, read back, copy into the
    /// client SHM buffer) but sends the ext protocol completion events. Output
    /// sources are serviced; toplevel sources are failed (not yet supported) so
    /// clients do not wait forever.
    fn fulfill_image_capture_frames(
        renderer: &mut GlesRenderer,
        offscreen_cache: &mut Option<(i32, i32, GlesRenderbuffer)>,
        output: &Output,
        width: i32,
        height: i32,
        elements: &[KmsRenderElement],
        pending: &crate::backend::wayland_udev::image_copy_capture::PendingImageCaptureQueue,
        counters: Option<
            &std::sync::Arc<std::sync::Mutex<crate::backend::wayland::state::CaptureCounters>>,
        >,
    ) {
        use crate::backend::wayland_udev::image_copy_capture::CaptureSource;
        use smithay::reexports::wayland_protocols::ext::image_copy_capture::v1::server::ext_image_copy_capture_frame_v1::FailureReason;
        use smithay::reexports::wayland_server::protocol::wl_output;
        use smithay::wayland::shm::with_buffer_contents_mut;

        let frames = {
            let mut queue = pending.lock_safe();
            let mut matching = Vec::new();
            let mut remaining = Vec::new();
            for f in queue.drain(..) {
                match &f.source {
                    CaptureSource::Output(o) if o == output => matching.push(f),
                    // Toplevel frames are output-independent; leave them queued for
                    // `fulfill_image_capture_toplevel_frames`, which runs once after
                    // the per-output loop with access to per-window surface state.
                    CaptureSource::Output(_) | CaptureSource::Toplevel(_) => remaining.push(f),
                }
            }
            *queue = remaining;
            matching
        };

        if frames.is_empty() {
            return;
        }

        // Render dmabuf-backed frames directly into the client buffer (no readback).
        let mut shm_frames = Vec::with_capacity(frames.len());
        for f in frames {
            let dmabuf = get_dmabuf(&f.buffer).ok().cloned();
            match dmabuf {
                Some(dmabuf) => {
                    let clear = smithay::backend::renderer::Color32F::new(0.0, 0.0, 0.0, 1.0);
                    if Self::render_into_client_dmabuf(
                        renderer, &dmabuf, width, height, elements, clear,
                    ) {
                        f.frame.transform(wl_output::Transform::Normal);
                        f.frame.damage(0, 0, width, height);
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default();
                        let tv_sec = now.as_secs();
                        f.frame.presentation_time(
                            (tv_sec >> 32) as u32,
                            (tv_sec & 0xFFFFFFFF) as u32,
                            now.subsec_nanos(),
                        );
                        f.frame.ready();
                        Self::note_image_capture_fulfilled(counters);
                    } else {
                        Self::note_image_capture_render_failed(counters);
                        f.frame.failed(FailureReason::Unknown);
                    }
                }
                None => shm_frames.push(f),
            }
        }
        let frames = shm_frames;
        if frames.is_empty() {
            return;
        }

        let fail_all =
            |frames: &[crate::backend::wayland_udev::image_copy_capture::PendingImageCapture]| {
                for f in frames {
                    Self::note_image_capture_render_failed(counters);
                    f.frame.failed(FailureReason::Unknown);
                }
            };

        let size: Size<i32, BufferCoord> = (width, height).into();
        let renderbuffer =
            match Self::screencopy_offscreen_buffer(renderer, offscreen_cache, width, height) {
                Some(rb) => rb,
                None => {
                    fail_all(&frames);
                    return;
                }
            };

        let mut target = match renderer.bind(renderbuffer) {
            Ok(t) => t,
            Err(e) => {
                log::error!("[image-capture] bind offscreen failed: {e:?}");
                fail_all(&frames);
                return;
            }
        };

        let phys_size: smithay::utils::Size<i32, Physical> = (width, height).into();
        let mut damage_tracker =
            OutputDamageTracker::new(phys_size, Scale::from(1.0f64), Transform::Normal);
        let clear_color = smithay::backend::renderer::Color32F::new(0.0, 0.0, 0.0, 1.0);
        if let Err(e) =
            damage_tracker.render_output(renderer, &mut target, 0, elements, clear_color)
        {
            log::error!("[image-capture] render_output failed: {e:?}");
            fail_all(&frames);
            return;
        }

        let region = Rectangle::from_size(size);
        let mapping = match renderer.copy_framebuffer(&target, region, Fourcc::Abgr8888) {
            Ok(m) => m,
            Err(e) => {
                log::error!("[image-capture] copy_framebuffer failed: {e:?}");
                fail_all(&frames);
                return;
            }
        };
        let pixels = match renderer.map_texture(&mapping) {
            Ok(p) => p,
            Err(e) => {
                log::error!("[image-capture] map_texture failed: {e:?}");
                fail_all(&frames);
                return;
            }
        };

        for frame_info in frames {
            let copy_result =
                with_buffer_contents_mut(&frame_info.buffer, |ptr, pool_len, buf_data| {
                    let buf_offset = buf_data.offset as usize;
                    let buf_stride = buf_data.stride as usize;
                    let buf_h = buf_data.height as usize;
                    let buf_w = buf_data.width as usize;
                    let src_stride = width as usize * 4;
                    let copy_h = buf_h.min(height as usize);
                    let copy_w = buf_w.min(width as usize);

                    for row in 0..copy_h {
                        let src_row_start = row * src_stride;
                        let dst_row_start = buf_offset + row * buf_stride;
                        if dst_row_start + copy_w * 4 > pool_len {
                            break;
                        }
                        for col in 0..copy_w {
                            let si = src_row_start + col * 4;
                            let di = dst_row_start + col * 4;
                            if si + 3 >= pixels.len() {
                                break;
                            }
                            // GL ABGR8888 [R,G,B,A] → shm ARGB8888 [B,G,R,A].
                            unsafe {
                                *ptr.add(di) = pixels[si + 2];
                                *ptr.add(di + 1) = pixels[si + 1];
                                *ptr.add(di + 2) = pixels[si];
                                *ptr.add(di + 3) = pixels[si + 3];
                            }
                        }
                    }
                });

            match copy_result {
                Ok(()) => {
                    frame_info.frame.transform(wl_output::Transform::Normal);
                    frame_info.frame.damage(0, 0, width, height);
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default();
                    let tv_sec = now.as_secs();
                    frame_info.frame.presentation_time(
                        (tv_sec >> 32) as u32,
                        (tv_sec & 0xFFFFFFFF) as u32,
                        now.subsec_nanos(),
                    );
                    frame_info.frame.ready();
                    Self::note_image_capture_fulfilled(counters);
                }
                Err(e) => {
                    log::warn!("[image-capture] buffer access failed: {e:?}");
                    Self::note_image_capture_render_failed(counters);
                    frame_info.frame.failed(FailureReason::Unknown);
                }
            }
        }
    }

    /// Fulfill pending ext-image-copy-capture-v1 *toplevel* (single window) frames.
    ///
    /// Unlike output capture, this renders only one window's surface tree into a
    /// window-sized offscreen buffer and reads it back into the client buffer.
    /// Runs once per render cycle since toplevel frames are not tied to an output.
    fn fulfill_image_capture_toplevel_frames(
        renderer: &mut GlesRenderer,
        offscreen_cache: &mut Option<(i32, i32, GlesRenderbuffer)>,
        state: &crate::backend::wayland::state::JwmWaylandState,
        pending: &crate::backend::wayland_udev::image_copy_capture::PendingImageCaptureQueue,
        counters: Option<
            &std::sync::Arc<std::sync::Mutex<crate::backend::wayland::state::CaptureCounters>>,
        >,
    ) {
        use crate::backend::wayland_udev::image_copy_capture::CaptureSource;
        use smithay::reexports::wayland_protocols::ext::image_copy_capture::v1::server::ext_image_copy_capture_frame_v1::FailureReason;
        use smithay::reexports::wayland_server::protocol::wl_output;
        use smithay::wayland::shm::with_buffer_contents_mut;

        let frames = {
            let mut queue = pending.lock_safe();
            let mut matching = Vec::new();
            let mut remaining = Vec::new();
            for f in queue.drain(..) {
                match &f.source {
                    CaptureSource::Toplevel(_) => matching.push(f),
                    CaptureSource::Output(_) => remaining.push(f),
                }
            }
            *queue = remaining;
            matching
        };

        if frames.is_empty() {
            return;
        }

        for frame_info in frames {
            let CaptureSource::Toplevel(win) = frame_info.source else {
                continue;
            };

            let Some(surface) = state.surface_for_window(win) else {
                Self::note_image_capture_render_failed(counters);
                frame_info.frame.failed(FailureReason::Unknown);
                continue;
            };
            let Some(geo) = state.window_geometry.get(&win).copied() else {
                Self::note_image_capture_render_failed(counters);
                frame_info.frame.failed(FailureReason::Unknown);
                continue;
            };
            let (width, height) = (geo.w as i32, geo.h as i32);
            if width <= 0 || height <= 0 {
                Self::note_image_capture_render_failed(counters);
                frame_info.frame.failed(FailureReason::Unknown);
                continue;
            }

            // Shift the surface buffer origin by -window_geometry.loc so client-side
            // shadow/CSD margins don't push the content off the capture buffer.
            let (off_x, off_y) = with_states(&surface, |states| {
                let mut cached = states.cached_state.get::<SurfaceCachedState>();
                cached
                    .current()
                    .geometry
                    .map(|r| (r.loc.x, r.loc.y))
                    .unwrap_or((0, 0))
            });

            let scale = Scale::from(1.0f64);
            let location: Point<i32, Physical> = (-off_x, -off_y).into();
            let tree = SurfaceTree::from_surface(&surface);
            let elements: Vec<KmsRenderElement> = AsRenderElements::<GlesRenderer>::render_elements(
                &tree, renderer, location, scale, 1.0,
            );

            // dmabuf fast path: render the window straight into the client buffer.
            if let Some(dmabuf) = get_dmabuf(&frame_info.buffer).ok().cloned() {
                let clear = smithay::backend::renderer::Color32F::new(0.0, 0.0, 0.0, 0.0);
                if Self::render_into_client_dmabuf(
                    renderer, &dmabuf, width, height, &elements, clear,
                ) {
                    frame_info.frame.transform(wl_output::Transform::Normal);
                    frame_info.frame.damage(0, 0, width, height);
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default();
                    let tv_sec = now.as_secs();
                    frame_info.frame.presentation_time(
                        (tv_sec >> 32) as u32,
                        (tv_sec & 0xFFFFFFFF) as u32,
                        now.subsec_nanos(),
                    );
                    frame_info.frame.ready();
                    Self::note_image_capture_fulfilled(counters);
                } else {
                    Self::note_image_capture_render_failed(counters);
                    frame_info.frame.failed(FailureReason::Unknown);
                }
                continue;
            }

            let size: Size<i32, BufferCoord> = (width, height).into();
            let renderbuffer =
                match Self::screencopy_offscreen_buffer(renderer, offscreen_cache, width, height) {
                    Some(rb) => rb,
                    None => {
                        Self::note_image_capture_render_failed(counters);
                        frame_info.frame.failed(FailureReason::Unknown);
                        continue;
                    }
                };

            let mut target = match renderer.bind(renderbuffer) {
                Ok(t) => t,
                Err(e) => {
                    log::error!("[image-capture/toplevel] bind offscreen failed: {e:?}");
                    Self::note_image_capture_render_failed(counters);
                    frame_info.frame.failed(FailureReason::Unknown);
                    continue;
                }
            };

            let phys_size: smithay::utils::Size<i32, Physical> = (width, height).into();
            let mut damage_tracker =
                OutputDamageTracker::new(phys_size, Scale::from(1.0f64), Transform::Normal);
            // Transparent clear so areas outside the window's content stay clear.
            let clear_color = smithay::backend::renderer::Color32F::new(0.0, 0.0, 0.0, 0.0);
            if let Err(e) =
                damage_tracker.render_output(renderer, &mut target, 0, &elements, clear_color)
            {
                log::error!("[image-capture/toplevel] render_output failed: {e:?}");
                Self::note_image_capture_render_failed(counters);
                frame_info.frame.failed(FailureReason::Unknown);
                continue;
            }

            let region = Rectangle::from_size(size);
            let mapping = match renderer.copy_framebuffer(&target, region, Fourcc::Abgr8888) {
                Ok(m) => m,
                Err(e) => {
                    log::error!("[image-capture/toplevel] copy_framebuffer failed: {e:?}");
                    Self::note_image_capture_render_failed(counters);
                    frame_info.frame.failed(FailureReason::Unknown);
                    continue;
                }
            };
            let pixels = match renderer.map_texture(&mapping) {
                Ok(p) => p,
                Err(e) => {
                    log::error!("[image-capture/toplevel] map_texture failed: {e:?}");
                    Self::note_image_capture_render_failed(counters);
                    frame_info.frame.failed(FailureReason::Unknown);
                    continue;
                }
            };

            let copy_result =
                with_buffer_contents_mut(&frame_info.buffer, |ptr, pool_len, buf_data| {
                    let buf_offset = buf_data.offset as usize;
                    let buf_stride = buf_data.stride as usize;
                    let buf_h = buf_data.height as usize;
                    let buf_w = buf_data.width as usize;
                    let src_stride = width as usize * 4;
                    let copy_h = buf_h.min(height as usize);
                    let copy_w = buf_w.min(width as usize);

                    for row in 0..copy_h {
                        let src_row_start = row * src_stride;
                        let dst_row_start = buf_offset + row * buf_stride;
                        if dst_row_start + copy_w * 4 > pool_len {
                            break;
                        }
                        for col in 0..copy_w {
                            let si = src_row_start + col * 4;
                            let di = dst_row_start + col * 4;
                            if si + 3 >= pixels.len() {
                                break;
                            }
                            // GL ABGR8888 [R,G,B,A] → shm ARGB8888 [B,G,R,A].
                            unsafe {
                                *ptr.add(di) = pixels[si + 2];
                                *ptr.add(di + 1) = pixels[si + 1];
                                *ptr.add(di + 2) = pixels[si];
                                *ptr.add(di + 3) = pixels[si + 3];
                            }
                        }
                    }
                });

            match copy_result {
                Ok(()) => {
                    frame_info.frame.transform(wl_output::Transform::Normal);
                    frame_info.frame.damage(0, 0, width, height);
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default();
                    let tv_sec = now.as_secs();
                    frame_info.frame.presentation_time(
                        (tv_sec >> 32) as u32,
                        (tv_sec & 0xFFFFFFFF) as u32,
                        now.subsec_nanos(),
                    );
                    frame_info.frame.ready();
                    Self::note_image_capture_fulfilled(counters);
                }
                Err(e) => {
                    log::warn!("[image-capture/toplevel] buffer access failed: {e:?}");
                    Self::note_image_capture_render_failed(counters);
                    frame_info.frame.failed(FailureReason::Unknown);
                }
            }
        }
    }

    pub(super) fn outputs(&self) -> Vec<Output> {
        self.outputs.iter().map(|o| o.output.clone()).collect()
    }

    /// Actual hardware gamma LUT size per output (output name -> entries).
    /// Queried from the CRTC; falls back to 256 if the driver doesn't report it.
    pub(super) fn gamma_sizes(&mut self) -> Vec<(String, u32)> {
        let mgr = self.drm_output_manager.lock();
        let dev = mgr.device();
        self.outputs
            .iter()
            .map(|o| {
                let size = dev
                    .get_crtc(o.crtc)
                    .ok()
                    .map(|info| info.gamma_length())
                    .filter(|n| *n > 0)
                    .unwrap_or(256);
                (o.output.name(), size)
            })
            .collect()
    }

    pub(super) fn direct_scanout_output_statuses(
        &self,
    ) -> Vec<crate::backend::api::DirectScanoutOutputStatus> {
        self.last_direct_scanout_outputs.clone()
    }

    pub(super) fn presentation_timing_status(
        &self,
    ) -> crate::backend::api::PresentationTimingStatus {
        let now = std::time::Instant::now();
        crate::backend::api::PresentationTimingStatus {
            any_frame_pending: self.outputs.iter().any(|o| o.frame_pending),
            outputs: self
                .outputs
                .iter()
                .map(|o| {
                    let watchdog =
                        (o.refresh_interval * 5).max(std::time::Duration::from_millis(100));
                    crate::backend::api::PresentationTimingOutputStatus {
                        output_name: o.output_name.clone(),
                        refresh_interval_ms: o.refresh_interval.as_secs_f64() * 1000.0,
                        last_vblank_monotonic_ms: o
                            .last_vblank
                            .map(|t| t.as_millis().min(u128::from(u64::MAX)) as u64),
                        last_vblank_ago_ms: o.last_vblank_received_at.map(|t| {
                            now.duration_since(t).as_millis().min(u128::from(u64::MAX)) as u64
                        }),
                        frame_pending: o.frame_pending,
                        frame_pending_for_ms: o.frame_pending_since.map(|t| {
                            now.duration_since(t).as_millis().min(u128::from(u64::MAX)) as u64
                        }),
                        watchdog_timeout_ms: watchdog.as_millis().min(u128::from(u64::MAX)) as u64,
                        frame_callback_roots: o.frame_callback_roots.len(),
                        visible_surface_count: o.frame_callback_visible.len(),
                        send_frame_callbacks: o.send_frame_callbacks,
                    }
                })
                .collect(),
        }
    }

    pub(super) fn dmabuf_render_formats(&self) -> Vec<DmabufFormat> {
        self.renderer
            .egl_context()
            .dmabuf_render_formats()
            .iter()
            .copied()
            .collect()
    }

    pub(super) fn dev_t(&self) -> libc::dev_t {
        use std::os::unix::io::AsRawFd;
        let raw_fd = self.drm_device_fd.as_raw_fd();
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        unsafe { libc::fstat(raw_fd, &mut stat) };
        stat.st_rdev
    }

    pub(super) fn new(
        session: &mut LibSeatSession,
        dev_path: &Path,
        dev_id: u64,
        output_layout: &std::collections::HashMap<u64, (i32, i32)>,
        display_handle: &smithay::reexports::wayland_server::DisplayHandle,
        flush_tx: Sender<()>,
        flush_pending: Arc<AtomicBool>,
        event_loop_handle: LoopHandle<'static, crate::backend::wayland::state::JwmWaylandState>,
    ) -> Result<KmsHandle, KmsInitError> {
        let fd = session
            .open(
                dev_path,
                OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
            )
            .map_err(KmsInitError::DeviceOpen)?;
        let fd = DrmDeviceFd::new(DeviceFd::from(fd));

        let (drm, notifier) = DrmDevice::new(fd.clone(), true).map_err(KmsInitError::DrmInit)?;
        let gbm = GbmDevice::new(fd.clone()).map_err(KmsInitError::GbmInit)?;

        let display = unsafe { EGLDisplay::new(gbm.clone()).map_err(KmsInitError::EglInit)? };
        let context = EGLContext::new_with_priority(&display, ContextPriority::High)
            .map_err(KmsInitError::EglInit)?;
        let mut renderer = unsafe { GlesRenderer::new(context).map_err(KmsInitError::GlesInit)? };

        let allocator = GbmAllocator::new(
            gbm.clone(),
            GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
        );
        let exporter = GbmFramebufferExporter::new(gbm.clone(), NodeFilter::None);

        let render_formats: FormatSet = renderer
            .egl_context()
            .dmabuf_render_formats()
            .iter()
            .copied()
            .collect();

        // Try 10-bit first (for HDR), then fall back to 8-bit.
        let color_formats = [
            Fourcc::Argb2101010,
            Fourcc::Xrgb2101010,
            Fourcc::Argb8888,
            Fourcc::Xrgb8888,
        ];

        let mut drm_output_manager = DrmOutputManager::new(
            drm,
            allocator,
            exporter,
            Some(gbm.clone()),
            color_formats.into_iter(),
            render_formats,
        );

        #[derive(Clone)]
        struct PendingOutputInit {
            crtc: crtc::Handle,
            mode: smithay::reexports::drm::control::Mode,
            connector: connector::Handle,
            output: Output,
            mode_size: (i32, i32),
            origin: (i32, i32),
            frame_callback_throttle: Option<std::time::Duration>,
        }

        // Create outputs for all connected connectors with a usable (distinct) CRTC.
        let pending: Vec<PendingOutputInit> = {
            let drm_device = drm_output_manager.device();
            let res = drm_device.resource_handles().map_err(|e| {
                KmsInitError::InitializeOutput(format!("resource_handles failed: {e:?}"))
            })?;

            let mut used_crtcs: HashSet<crtc::Handle> = HashSet::new();
            let mut pending = Vec::new();

            for conn_handle in res.connectors() {
                let conn = drm_device.get_connector(*conn_handle, true).map_err(|e| {
                    KmsInitError::InitializeOutput(format!("get_connector failed: {e:?}"))
                })?;

                if conn.state() != connector::State::Connected || conn.modes().is_empty() {
                    continue;
                }

                let Some(crtc) = pick_crtc(drm_device, &res, &conn, &used_crtcs) else {
                    continue;
                };
                used_crtcs.insert(crtc);

                let Some(mode) = conn
                    .modes()
                    .iter()
                    .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
                    .copied()
                    .or_else(|| conn.modes().first().copied())
                else {
                    log::warn!(
                        "[kms] connector {:?}-{} reported no usable mode; skipping",
                        conn.interface(),
                        conn.interface_id()
                    );
                    continue;
                };

                let wl_mode = WlMode::from(mode);
                let frame_callback_throttle = if wl_mode.refresh > 0 {
                    // Smithay's Mode.refresh is in mHz (e.g. 60000 == 60Hz).
                    Some(std::time::Duration::from_nanos(
                        (1_000_000_000u64.saturating_mul(1000)) / (wl_mode.refresh as u64),
                    ))
                } else {
                    None
                };

                let (phys_w, phys_h) = conn.size().unwrap_or((0, 0));
                let output_name = format!("{:?}-{}", conn.interface(), conn.interface_id());
                let output = Output::new(
                    output_name,
                    PhysicalProperties {
                        size: (phys_w as i32, phys_h as i32).into(),
                        subpixel: Subpixel::Unknown,
                        make: "Unknown".into(),
                        model: "Unknown".into(),
                        serial_number: "Unknown".into(),
                    },
                );

                for m in conn.modes() {
                    output.add_mode(WlMode::from(*m));
                }
                output.set_preferred(wl_mode);

                let key = (dev_id << 32) | (u32::from(*conn_handle) as u64);
                let (ox, oy) = output_layout.get(&key).copied().unwrap_or((0, 0));
                output.change_current_state(Some(wl_mode), None, None, Some((ox, oy).into()));

                pending.push(PendingOutputInit {
                    crtc,
                    mode,
                    connector: conn.handle(),
                    output,
                    mode_size: (mode.size().0 as i32, mode.size().1 as i32),
                    origin: (ox, oy),
                    frame_callback_throttle,
                });
            }

            pending
        };

        let render_elements: DrmOutputRenderElements<GlesRenderer, SolidColorRenderElement> =
            DrmOutputRenderElements::default();
        let mut outputs: Vec<KmsOutputState> = Vec::new();

        for p in pending {
            let _wl_output_global = p
                .output
                .create_global::<crate::backend::wayland::state::JwmWaylandState>(display_handle);

            let drm_output = drm_output_manager
                .lock()
                .initialize_output::<_, SolidColorRenderElement>(
                    p.crtc,
                    p.mode,
                    &[p.connector],
                    &p.output,
                    None,
                    &mut renderer,
                    &render_elements,
                )
                .map_err(|e| KmsInitError::InitializeOutput(format!("{e}")))?;

            // Enable VRR (Variable Refresh Rate / FreeSync / Adaptive Sync) on the CRTC if supported.
            {
                let mgr = drm_output_manager.lock();
                let dev = mgr.device();
                if let Ok(props) = dev.get_properties(p.crtc) {
                    let (handles, _values) = props.as_props_and_values();
                    for &prop_handle in handles {
                        if let Ok(info) = dev.get_property(prop_handle) {
                            if info.name().to_str() == Ok("VRR_ENABLED") {
                                match Self::set_drm_property(dev, p.crtc, prop_handle, 1) {
                                    Err(e) => log::debug!(
                                        "[kms] failed to enable VRR on crtc {:?}: {e}",
                                        p.crtc
                                    ),
                                    Ok(()) => log::info!("[kms] VRR enabled on crtc {:?}", p.crtc),
                                }
                                break;
                            }
                        }
                    }
                }
            }

            // Probe color-pipeline caps inline (the standalone helper takes
            // &mut self which isn't available here).
            let color_pipeline_caps = {
                let mgr = drm_output_manager.lock();
                let dev = mgr.device();
                let mut caps = crate::backend::api::KmsColorPipelineCaps::default();
                if let Ok(props) = dev.get_properties(p.crtc) {
                    let (handles, values) = props.as_props_and_values();
                    for (i, &prop_handle) in handles.iter().enumerate() {
                        if let Ok(info) = dev.get_property(prop_handle) {
                            match info.name().to_str().unwrap_or("") {
                                "DEGAMMA_LUT" => caps.degamma_lut_supported = true,
                                "GAMMA_LUT" => caps.gamma_lut_supported = true,
                                "CTM" => caps.ctm_supported = true,
                                "DEGAMMA_LUT_SIZE" => caps.degamma_lut_size = values[i] as u32,
                                "GAMMA_LUT_SIZE" => caps.gamma_lut_size = values[i] as u32,
                                _ => {}
                            }
                        }
                    }
                }
                Some(caps)
            };

            let refresh_interval = p
                .frame_callback_throttle
                .unwrap_or(std::time::Duration::from_millis(16));
            let output_name = p.output.name();
            let (output_tf, output_ctm) = {
                use crate::backend::wayland_udev::color_pipeline::{
                    ColorSpacePrimaries, TransferKind, rgb_to_rgb_matrix,
                };
                let params =
                    crate::backend::wayland_udev::color_management::params_for_output(&p.output);
                let tf = TransferKind::from_params(&params);
                let prim = ColorSpacePrimaries::from_params(&params);
                let ctm = rgb_to_rgb_matrix(&ColorSpacePrimaries::SRGB_D65, &prim);
                (tf, ctm)
            };
            outputs.push(KmsOutputState {
                crtc: p.crtc,
                connector: p.connector,
                mode_size: p.mode_size,
                origin: p.origin,
                output: p.output,
                drm_output,
                frame_pending: false,
                frame_pending_since: None,
                send_frame_callbacks: false,
                frame_callback_roots: Vec::new(),
                frame_callback_throttle: p.frame_callback_throttle,
                frame_callback_visible: HashSet::new(),
                surfaces_on_output: HashSet::new(),
                last_vblank: None,
                last_vblank_received_at: None,
                refresh_interval,
                output_name,
                color_pipeline_caps,
                installed_gamma_lut: None,
                installed_ctm: None,
                output_tf,
                output_ctm,
                dpms_off: false,
            });
        }

        if outputs.is_empty() {
            return Err(KmsInitError::NoConnector);
        }

        let cursor_theme_name = std::env::var("XCURSOR_THEME")
            .ok()
            .unwrap_or_else(|| "default".into());
        let cursor_size = std::env::var("XCURSOR_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(24u32);
        let cursor_theme = CursorTheme::load(&cursor_theme_name);

        let handle: KmsHandle = Rc::new(RefCell::new(KmsState {
            dev_path: dev_path.to_path_buf(),
            drm_device_fd: fd.clone(),
            registration_token: None,
            flush_tx,
            flush_pending,
            drm_output_manager,
            gbm,
            renderer,
            needs_render: true,
            compositor_texture_cache: None,
            compositor_texture_keepalive: Vec::new(),
            background_id: Id::new(),

            cursor_theme,
            cursor_size,
            cursor_images: HashMap::new(),
            cursor_cache: HashMap::new(),

            cursor_fallback_body_ids: (0..CURSOR_RECTS.len()).map(|_| Id::new()).collect(),
            cursor_fallback_shadow_ids: (0..CURSOR_RECTS.len()).map(|_| Id::new()).collect(),

            pending_screenshot: None,
            pending_screenshot_region: None,
            screencopy_pending: None,
            image_capture_pending: None,
            capture_counters: None,

            outputs,
            screencopy_offscreen: None,
            image_capture_toplevel_offscreen: None,
            last_presentation_time: None,
            last_direct_scanout_outputs: Vec::new(),
        }));

        let handle_clone = handle.clone();
        let token = event_loop_handle
            .insert_source(notifier, move |event, metadata, _state| match event {
                DrmEvent::VBlank(crtc) => {
                    handle_clone.borrow_mut().on_vblank(crtc, metadata);
                }
                DrmEvent::Error(err) => {
                    log::warn!("drm event error: {err:?}");
                }
            })
            .expect("failed to register drm notifier");

        handle.borrow_mut().registration_token = Some(token);

        Ok(handle)
    }

    pub(super) fn render_if_needed(
        &mut self,
        state: &crate::backend::wayland::state::JwmWaylandState,
        cursor_kind: StdCursorKind,
        compositor: Option<&super::super::compositor::WaylandCompositor>,
    ) {
        if !self.needs_render {
            return;
        }

        self.last_direct_scanout_outputs.clear();
        let mut any_skipped = false;
        for out_idx in 0..self.outputs.len() {
            // Outputs marked soft-disabled by wlr-output-management
            // `disable_head` Apply stop receiving frames but keep their
            // DrmOutput alive so a later `enable_head` Apply can resume.
            if state
                .soft_disabled_outputs
                .contains(&self.outputs[out_idx].output.name())
            {
                continue;
            }
            let frame_pending = self.outputs[out_idx].frame_pending;
            if frame_pending {
                // Watchdog: a queued page flip should produce a vblank within one
                // refresh interval. If it hasn't after several intervals (clamped
                // to a sane floor), assume the flip was dropped and force-clear so
                // the output doesn't stall permanently.
                let out = &self.outputs[out_idx];
                let timeout = (out.refresh_interval * 5).max(std::time::Duration::from_millis(100));
                let stale = out
                    .frame_pending_since
                    .map(|t| t.elapsed() >= timeout)
                    .unwrap_or(false);
                if stale {
                    log::warn!(
                        "[vblank-watchdog] output {} frame_pending for >{:?} without vblank; \
                         force-clearing to recover",
                        out.output.name(),
                        timeout,
                    );
                    let out = &mut self.outputs[out_idx];
                    out.frame_pending = false;
                    out.frame_pending_since = None;
                    let _ = out.drm_output.frame_submitted();
                } else {
                    any_skipped = true;
                    continue;
                }
            }

            let scale: Scale<f64> = self.outputs[out_idx]
                .output
                .current_scale()
                .fractional_scale()
                .into();
            let (out_w, out_h) = self.outputs[out_idx].mode_size;
            let (ox, oy) = self.outputs[out_idx].origin;
            let output_rect_global = Rectangle::<i32, smithay::utils::Logical>::new(
                (ox, oy).into(),
                (out_w, out_h).into(),
            );

            // DrmOutput::render_frame expects elements in front-to-back order.
            // So: cursor/top-most surfaces first, solid background last.
            let mut elements: Vec<KmsRenderElement> = Vec::new();

            // Cursor will be pushed FIRST (front-most).
            let cursor_x = state.pointer_location.x.round() as i32;
            let cursor_y = state.pointer_location.y.round() as i32;
            if cursor_x >= ox
                && cursor_y >= oy
                && cursor_x < (ox + out_w)
                && cursor_y < (oy + out_h)
            {
                // Approximate a cursor scale factor from the output scale.
                let cursor_scale = scale.x.max(1.0).ceil() as u32;
                let cursor_bitmap = self.cursor_bitmap(cursor_kind, cursor_scale);

                if let Some(bitmap) = cursor_bitmap.as_ref() {
                    let loc: Point<i32, Physical> =
                        ((cursor_x - ox) - bitmap.xhot, (cursor_y - oy) - bitmap.yhot).into();
                    if let Ok(elem) = MemoryRenderBufferRenderElement::from_buffer(
                        &mut self.renderer,
                        loc.to_f64(),
                        &bitmap.buffer,
                        None,
                        None,
                        None,
                        Kind::Cursor,
                    ) {
                        elements.push(KmsRenderElement::Memory(elem));
                    }
                } else {
                    // Fallback: simple software pointer so we still have a visible cursor.
                    let base_x = cursor_x - ox;
                    let base_y = cursor_y - oy;

                    for (idx, (rx, ry, rw, rh)) in CURSOR_RECTS.iter().copied().enumerate() {
                        let geo: Rectangle<i32, Physical> =
                            Rectangle::new((base_x + rx, base_y + ry).into(), (rw, rh).into());
                        let body = SolidColorRenderElement::new(
                            self.cursor_fallback_body_ids[idx].clone(),
                            geo,
                            0usize,
                            smithay::backend::renderer::Color32F::new(0.98, 0.98, 0.98, 1.0),
                            Kind::Cursor,
                        );
                        elements.push(KmsRenderElement::Solid(body));
                    }
                    for (idx, (rx, ry, rw, rh)) in CURSOR_RECTS.iter().copied().enumerate() {
                        let geo: Rectangle<i32, Physical> = Rectangle::new(
                            (base_x + rx + 1, base_y + ry + 1).into(),
                            (rw, rh).into(),
                        );
                        let shadow = SolidColorRenderElement::new(
                            self.cursor_fallback_shadow_ids[idx].clone(),
                            geo,
                            0usize,
                            smithay::backend::renderer::Color32F::new(0.0, 0.0, 0.0, 0.55),
                            Kind::Cursor,
                        );
                        elements.push(KmsRenderElement::Solid(shadow));
                    }
                }
            }

            let out = &mut self.outputs[out_idx];

            let mut visible_surfaces: HashSet<wayland_server::Weak<WlSurface>> = HashSet::new();
            let mut frame_roots: Vec<WlSurface> = Vec::new();

            if state.session_locked {
                if let Some(lock_surface) = state.lock_surfaces.get(&out.output.name()) {
                    let surface = lock_surface.wl_surface().clone();
                    frame_roots.push(surface.clone());

                    with_surface_tree_downward(
                        &surface,
                        (),
                        |_, _, _| TraversalAction::DoChildren(()),
                        |child_surface, child_states, _| {
                            let data = child_states.data_map.get::<RendererSurfaceStateUserData>();
                            let Some(data) = data else {
                                return;
                            };
                            if data.lock_safe().view().is_some() {
                                out.output.enter(child_surface);
                                visible_surfaces.insert(child_surface.downgrade());
                            }
                        },
                        |_, _, _| true,
                    );

                    let tree = SurfaceTree::from_surface(&surface);
                    let lock_elements: Vec<KmsRenderElement> =
                        AsRenderElements::<GlesRenderer>::render_elements(
                            &tree,
                            &mut self.renderer,
                            Point::<i32, Physical>::from((0, 0)),
                            scale,
                            1.0,
                        );
                    elements.extend(lock_elements);
                }

                // Opaque shield behind the lock surface and above regular clients.
                elements.push(KmsRenderElement::Solid(SolidColorRenderElement::new(
                    Id::new(),
                    Rectangle::<i32, Physical>::from_size((out_w, out_h).into()),
                    0usize,
                    smithay::backend::renderer::Color32F::new(0.0, 0.0, 0.0, 1.0),
                    Kind::Unspecified,
                )));
            }

            // DnD drag icon: rendered just below the cursor, in front of all windows.
            // Placed before the compositor/element branch split so it overlays both
            // render paths identically.
            if let Some(icon) = state.dnd_icon.as_ref() {
                let surface = icon.surface.clone();
                frame_roots.push(surface.clone());
                with_surface_tree_downward(
                    &surface,
                    (),
                    |_, _, _| TraversalAction::DoChildren(()),
                    |child_surface, child_states, _| {
                        let data = child_states.data_map.get::<RendererSurfaceStateUserData>();
                        let Some(data) = data else {
                            return;
                        };
                        if data.lock_safe().view().is_some() {
                            out.output.enter(child_surface);
                            visible_surfaces.insert(child_surface.downgrade());
                        }
                    },
                    |_, _, _| true,
                );
                let loc: Point<i32, Physical> = (
                    (cursor_x - ox) + icon.offset.x,
                    (cursor_y - oy) + icon.offset.y,
                )
                    .into();
                let tree = SurfaceTree::from_surface(&surface);
                let icon_elements: Vec<KmsRenderElement> =
                    AsRenderElements::<GlesRenderer>::render_elements(
                        &tree,
                        &mut self.renderer,
                        loc,
                        scale,
                        1.0,
                    );
                elements.extend(icon_elements);
            }

            // Layer surfaces above normal windows.
            {
                let map = layer_map_for_output(&out.output);
                for layer in [WlrLayer::Overlay, WlrLayer::Top] {
                    for ls in map.layers_on(layer) {
                        let Some(geo) = map.layer_geometry(ls) else {
                            continue;
                        };
                        let rect_global = Rectangle::<i32, smithay::utils::Logical>::new(
                            (ox + geo.loc.x, oy + geo.loc.y).into(),
                            geo.size,
                        );
                        if !rect_global.overlaps(output_rect_global) {
                            continue;
                        }

                        let surface = ls.wl_surface().clone();
                        frame_roots.push(surface.clone());

                        with_surface_tree_downward(
                            &surface,
                            (),
                            |_, _, _| TraversalAction::DoChildren(()),
                            |child_surface, child_states, _| {
                                let data =
                                    child_states.data_map.get::<RendererSurfaceStateUserData>();
                                let Some(data) = data else {
                                    return;
                                };
                                if data.lock_safe().view().is_some() {
                                    out.output.enter(child_surface);
                                    visible_surfaces.insert(child_surface.downgrade());
                                }
                            },
                            |_, _, _| true,
                        );

                        let location: Point<i32, Physical> = (geo.loc.x, geo.loc.y).into();
                        let tree = SurfaceTree::from_surface(&surface);
                        let layer_elements: Vec<KmsRenderElement> =
                            AsRenderElements::<GlesRenderer>::render_elements(
                                &tree,
                                &mut self.renderer,
                                location,
                                scale,
                                1.0,
                            );
                        elements.extend(layer_elements);
                    }
                }
            }

            // Direct scanout detection: if there's a single fullscreen window and no
            // top/overlay layer surfaces, bypass the compositor FBO and let DRM attempt
            // direct scanout via the primary plane (zero-copy, no GPU composition).
            // Respects the `fullscreen_unredirect` config flag, mirroring the X11
            // backend's check_fullscreen_unredirect (which gates XComposite unredirect).
            let fullscreen_unredirect = crate::config::CONFIG
                .load()
                .behavior()
                .fullscreen_unredirect;
            let system_ui_active = compositor.as_ref().is_some_and(|c| c.has_system_ui());
            let (direct_scanout_eligible, direct_scanout_reason) = if compositor.is_none() {
                (false, "compositor disabled".to_string())
            } else if system_ui_active {
                (false, "JWM system UI requires composition".to_string())
            } else if !fullscreen_unredirect {
                (false, "fullscreen_unredirect disabled".to_string())
            } else if !elements.is_empty() {
                (
                    false,
                    "cursor or overlay/layer surface requires composition".to_string(),
                )
            } else if state.window_stack.len() != 1 {
                (
                    false,
                    format!(
                        "expected exactly 1 stacked window, got {}",
                        state.window_stack.len()
                    ),
                )
            } else {
                let win = state.window_stack[0];
                let fullscreen = state
                    .window_is_fullscreen
                    .get(&win)
                    .copied()
                    .unwrap_or(false);
                let mapped = state.mapped_windows.contains(&win);
                if fullscreen && mapped {
                    (true, "eligible".to_string())
                } else if !mapped {
                    (false, format!("window {:?} is not mapped", win))
                } else {
                    (false, format!("window {:?} is not fullscreen", win))
                }
            };
            self.last_direct_scanout_outputs
                .push(crate::backend::api::DirectScanoutOutputStatus {
                    output_name: out.output_name.clone(),
                    eligible: direct_scanout_eligible,
                    reason: direct_scanout_reason,
                });

            let use_compositor = compositor.is_some() && !direct_scanout_eligible;

            if use_compositor {
                let comp = compositor.unwrap();
                // Compositor path: surfaces already imported in compositor_render_frame;
                // just collect frame_roots for callback delivery.
                for win in state.window_stack.iter().rev() {
                    if !state.mapped_windows.contains(win) {
                        continue;
                    }
                    let Some(surface) = state.surface_for_window(*win) else {
                        continue;
                    };
                    frame_roots.push(surface.clone());
                    with_surface_tree_downward(
                        &surface,
                        (),
                        |_, _, _| TraversalAction::DoChildren(()),
                        |child_surface, child_states, _| {
                            let data = child_states.data_map.get::<RendererSurfaceStateUserData>();
                            let Some(data) = data else {
                                return;
                            };
                            if data.lock_safe().view().is_some() {
                                out.output.enter(child_surface);
                                visible_surfaces.insert(child_surface.downgrade());
                            }
                        },
                        |_, _, _| true,
                    );
                }
                // Include xdg_popup surfaces for frame callbacks.
                for popup in state.popups.values() {
                    let popup_surface = popup.wl_surface().clone();
                    frame_roots.push(popup_surface.clone());
                    with_surface_tree_downward(
                        &popup_surface,
                        (),
                        |_, _, _| TraversalAction::DoChildren(()),
                        |child_surface, child_states, _| {
                            let data = child_states.data_map.get::<RendererSurfaceStateUserData>();
                            let Some(data) = data else {
                                return;
                            };
                            if data.lock_safe().view().is_some() {
                                out.output.enter(child_surface);
                                visible_surfaces.insert(child_surface.downgrade());
                            }
                        },
                        |_, _, _| true,
                    );
                }
                // Include IME popup surfaces for frame callbacks.
                for popup in &state.im_popups {
                    if !popup.alive() {
                        continue;
                    }
                    let im_surface = popup.wl_surface().clone();
                    frame_roots.push(im_surface.clone());
                    with_surface_tree_downward(
                        &im_surface,
                        (),
                        |_, _, _| TraversalAction::DoChildren(()),
                        |child_surface, child_states, _| {
                            let data = child_states.data_map.get::<RendererSurfaceStateUserData>();
                            let Some(data) = data else {
                                return;
                            };
                            if data.lock_safe().view().is_some() {
                                out.output.enter(child_surface);
                                visible_surfaces.insert(child_surface.downgrade());
                            }
                        },
                        |_, _, _| true,
                    );
                }
                // Wrap the compositor's output FBO texture as a full-screen render element.
                let (sw, sh) = comp.screen_size();
                let tex_id = comp.output_texture_id();
                let output_tex = match &self.compositor_texture_cache {
                    Some((cached_id, cached_tex)) if *cached_id == tex_id => cached_tex.clone(),
                    _ => {
                        let size: Size<i32, BufferCoord> = (sw as i32, sh as i32).into();
                        let tex = unsafe {
                            GlesTexture::from_raw(
                                &self.renderer,
                                Some(gl_ffi::RGBA8),
                                false,
                                tex_id,
                                size,
                            )
                        };
                        // Retain a strong ref so Smithay's Drop never calls
                        // glDeleteTextures on the compositor-owned FBO texture.
                        // Dedupe by id so recycled ids don't accumulate.
                        if !self
                            .compositor_texture_keepalive
                            .iter()
                            .any(|(id, _)| *id == tex_id)
                        {
                            self.compositor_texture_keepalive
                                .push((tex_id, tex.clone()));
                        }
                        self.compositor_texture_cache = Some((tex_id, tex.clone()));
                        tex
                    }
                };
                let context_id = self.renderer.context_id();
                // Position is output-relative: subtract the output's global origin so each
                // output sees the correct slice of the single full-screen FBO.
                let elem = TextureRenderElement::from_static_texture(
                    Id::new(),
                    context_id,
                    ((-ox) as f64, (-oy) as f64),
                    output_tex,
                    1,
                    Transform::Flipped180,
                    None,
                    None,
                    None,
                    None,
                    Kind::Unspecified,
                );
                elements.push(KmsRenderElement::Texture(elem));
            } else {
                // smithay's try_assign_overlay_plane only considers Kind::ScanoutCandidate
                // elements; the kernel atomic test still has final say.
                let overlay_candidate_window = if crate::config::CONFIG
                    .load()
                    .behavior()
                    .fullscreen_unredirect
                {
                    let mut fs = None;
                    for w in &state.window_stack {
                        if state.mapped_windows.contains(w)
                            && state.window_is_fullscreen.get(w).copied().unwrap_or(false)
                        {
                            if fs.is_some() {
                                fs = None;
                                break;
                            }
                            fs = Some(*w);
                        }
                    }
                    fs
                } else {
                    None
                };
                for win in state.window_stack.iter().rev() {
                    if !state.mapped_windows.contains(win) {
                        continue;
                    }
                    let Some(geo) = state.window_geometry.get(win) else {
                        continue;
                    };
                    let Some(surface) = state.surface_for_window(*win) else {
                        continue;
                    };

                    // Many toolkits set an xdg_surface window-geometry with a non-zero loc (e.g. to
                    // exclude client-side shadows). `state.window_geometry` tracks the window-geometry
                    // origin in global coords, but the wl_surface buffer origin must be shifted by
                    // -committed_geometry.loc to visually align.
                    let (toplevel_off_x, toplevel_off_y) = with_states(&surface, |states| {
                        let mut cached = states.cached_state.get::<SurfaceCachedState>();
                        cached
                            .current()
                            .geometry
                            .map(|r| (r.loc.x, r.loc.y))
                            .unwrap_or((0, 0))
                    });

                    // Render any popups belonging to this toplevel above it (but below cursor).
                    // Popups are separate wl_surfaces, not subsurfaces, so they won't appear in the
                    // parent's SurfaceTree.
                    for (popup_surface, popup_rect) in state.popup_rects_for_toplevel(*win) {
                        if !popup_rect.overlaps(output_rect_global) {
                            continue;
                        }

                        frame_roots.push(popup_surface.clone());

                        with_surface_tree_downward(
                            &popup_surface,
                            (),
                            |_, _, _| TraversalAction::DoChildren(()),
                            |child_surface, child_states, _| {
                                let data =
                                    child_states.data_map.get::<RendererSurfaceStateUserData>();
                                let Some(data) = data else {
                                    return;
                                };
                                if data.lock_safe().view().is_some() {
                                    out.output.enter(child_surface);
                                    visible_surfaces.insert(child_surface.downgrade());
                                }
                            },
                            |_, _, _| true,
                        );

                        let (popup_off_x, popup_off_y) = with_states(&popup_surface, |states| {
                            let mut cached = states.cached_state.get::<SurfaceCachedState>();
                            cached
                                .current()
                                .geometry
                                .map(|r| (r.loc.x, r.loc.y))
                                .unwrap_or((0, 0))
                        });

                        let location: Point<i32, Physical> = (
                            popup_rect.loc.x - ox - popup_off_x,
                            popup_rect.loc.y - oy - popup_off_y,
                        )
                            .into();
                        let tree = SurfaceTree::from_surface(&popup_surface);
                        let popup_elements: Vec<KmsRenderElement> =
                            AsRenderElements::<GlesRenderer>::render_elements(
                                &tree,
                                &mut self.renderer,
                                location,
                                scale,
                                1.0,
                            );
                        elements.extend(popup_elements);
                    }

                    let win_rect = Rectangle::<i32, smithay::utils::Logical>::new(
                        (geo.x, geo.y).into(),
                        (geo.w as i32, geo.h as i32).into(),
                    );
                    if !win_rect.overlaps(output_rect_global) {
                        continue;
                    }

                    frame_roots.push(surface.clone());

                    with_surface_tree_downward(
                        &surface,
                        (),
                        |_, _, _| TraversalAction::DoChildren(()),
                        |child_surface, child_states, _| {
                            let data = child_states.data_map.get::<RendererSurfaceStateUserData>();
                            let Some(data) = data else {
                                return;
                            };
                            if data.lock_safe().view().is_some() {
                                out.output.enter(child_surface);
                                visible_surfaces.insert(child_surface.downgrade());
                            }
                        },
                        |_, _, _| true,
                    );

                    let location: Point<i32, Physical> =
                        (geo.x - ox - toplevel_off_x, geo.y - oy - toplevel_off_y).into();
                    let window_kind = if Some(*win) == overlay_candidate_window {
                        Kind::ScanoutCandidate
                    } else {
                        Kind::Unspecified
                    };
                    let window_elements: Vec<KmsRenderElement> = render_elements_from_surface_tree(
                        &mut self.renderer,
                        &surface,
                        location,
                        scale,
                        1.0,
                        window_kind,
                    );
                    elements.extend(window_elements);

                    // Render window borders (server-side decorations for tiling WM).
                    if geo.border > 0 {
                        let bw = geo.border as i32;
                        let [cr, cg, cb, ca] = state
                            .window_border_color
                            .get(win)
                            .copied()
                            .unwrap_or([0.3, 0.3, 0.35, 1.0]);
                        let border_color =
                            smithay::backend::renderer::Color32F::new(cr, cg, cb, ca);
                        let full_geo: Rectangle<i32, Physical> = Rectangle::new(
                            (geo.x - ox - bw, geo.y - oy - bw).into(),
                            (geo.w as i32 + 2 * bw, geo.h as i32 + 2 * bw).into(),
                        );
                        elements.push(KmsRenderElement::Solid(SolidColorRenderElement::new(
                            Id::new(),
                            full_geo,
                            0usize,
                            border_color,
                            Kind::Unspecified,
                        )));
                    }
                }

                // IME popup surfaces (candidate windows) above normal windows.
                for anchor in state.im_popup_positions() {
                    let im_surface = anchor.surface;
                    frame_roots.push(im_surface.clone());
                    with_surface_tree_downward(
                        &im_surface,
                        (),
                        |_, _, _| TraversalAction::DoChildren(()),
                        |child_surface, child_states, _| {
                            let data = child_states.data_map.get::<RendererSurfaceStateUserData>();
                            let Some(data) = data else {
                                return;
                            };
                            if data.lock_safe().view().is_some() {
                                out.output.enter(child_surface);
                                visible_surfaces.insert(child_surface.downgrade());
                            }
                        },
                        |_, _, _| true,
                    );
                    // Place the candidate box below the cursor, flipping above it when
                    // it would overflow the parent monitor's bottom edge, and clamp
                    // horizontally. Mirrors the compositor render path in backend.rs.
                    let bbox = smithay::desktop::utils::bbox_from_surface_tree(
                        &im_surface,
                        Point::<i32, smithay::utils::Logical>::from((0, 0)),
                    );
                    let pw = bbox.size.w.max(1);
                    let ph = bbox.size.h.max(1);
                    let bx = (anchor.x + bbox.loc.x)
                        .min(anchor.area_right - pw)
                        .max(anchor.area_left);
                    let below_top = anchor.cursor_bottom + bbox.loc.y;
                    let by = if below_top + ph <= anchor.area_bottom {
                        below_top
                    } else {
                        (anchor.cursor_top - ph).max(anchor.area_top)
                    };
                    // Convert the clamped bbox top-left back to the root surface origin.
                    let abs_x = bx - bbox.loc.x;
                    let abs_y = by - bbox.loc.y;
                    let location: Point<i32, Physical> = (abs_x - ox, abs_y - oy).into();
                    let tree = SurfaceTree::from_surface(&im_surface);
                    let im_elements: Vec<KmsRenderElement> =
                        AsRenderElements::<GlesRenderer>::render_elements(
                            &tree,
                            &mut self.renderer,
                            location,
                            scale,
                            1.0,
                        );
                    elements.extend(im_elements);
                }

                // Layer surfaces below normal windows.
                {
                    let map = layer_map_for_output(&out.output);
                    for layer in [WlrLayer::Bottom, WlrLayer::Background] {
                        for ls in map.layers_on(layer) {
                            let Some(geo) = map.layer_geometry(ls) else {
                                continue;
                            };
                            let rect_global = Rectangle::<i32, smithay::utils::Logical>::new(
                                (ox + geo.loc.x, oy + geo.loc.y).into(),
                                geo.size,
                            );
                            if !rect_global.overlaps(output_rect_global) {
                                continue;
                            }

                            let surface = ls.wl_surface().clone();
                            frame_roots.push(surface.clone());

                            with_surface_tree_downward(
                                &surface,
                                (),
                                |_, _, _| TraversalAction::DoChildren(()),
                                |child_surface, child_states, _| {
                                    let data =
                                        child_states.data_map.get::<RendererSurfaceStateUserData>();
                                    let Some(data) = data else {
                                        return;
                                    };
                                    if data.lock_safe().view().is_some() {
                                        out.output.enter(child_surface);
                                        visible_surfaces.insert(child_surface.downgrade());
                                    }
                                },
                                |_, _, _| true,
                            );

                            let location: Point<i32, Physical> = (geo.loc.x, geo.loc.y).into();
                            let tree = SurfaceTree::from_surface(&surface);
                            let layer_elements: Vec<KmsRenderElement> =
                                AsRenderElements::<GlesRenderer>::render_elements(
                                    &tree,
                                    &mut self.renderer,
                                    location,
                                    scale,
                                    1.0,
                                );
                            elements.extend(layer_elements);
                        }
                    }
                }

                // Solid background LAST (back-most). Keep it opaque so we don't leak the previous
                // framebuffer contents on tty (which can look like a solid blue screen).
                let bg_geo = Rectangle::<i32, Physical>::from_size((out_w, out_h).into());
                let bg = SolidColorRenderElement::new(
                    self.background_id.clone(),
                    bg_geo,
                    0usize,
                    smithay::backend::renderer::Color32F::new(0.1, 0.15, 0.25, 1.0),
                    Kind::Unspecified,
                );
                elements.push(KmsRenderElement::Solid(bg));
            }

            // Notify the wp-color-management state of surface→output changes
            // before the leave events go out, so a client receiving leave on
            // wl_surface can correlate it with a preferred_changed firing on
            // the corresponding feedback object. Done as a diff against the
            // previous set so we hit each transition exactly once.
            if let Some(cm) = state.color_manager.as_ref() {
                for entering in visible_surfaces.difference(&out.surfaces_on_output) {
                    if let Ok(surf) = entering.upgrade() {
                        cm.on_surface_enters_output(&surf.id(), &out.output);
                    }
                }
                for leaving in out.surfaces_on_output.difference(&visible_surfaces) {
                    if let Ok(surf) = leaving.upgrade() {
                        cm.on_surface_leaves_output(&surf.id(), &out.output);
                    }
                }
            }
            for gone in out.surfaces_on_output.difference(&visible_surfaces) {
                if let Ok(surf) = gone.upgrade() {
                    out.output.leave(&surf);
                }
            }
            out.surfaces_on_output.clone_from(&visible_surfaces);
            // Drop the `out` borrow so we can access other `self` fields below.
            let _ = out;

            // ── Screenshot capture (offscreen render) ───────────────────────
            if out_idx == 0 {
                if let Some(screenshot_path) = self.pending_screenshot.take() {
                    Self::capture_screenshot_offscreen_impl(
                        &mut self.renderer,
                        out_w,
                        out_h,
                        &elements,
                        &screenshot_path,
                    );
                }
                if let Some((region_path, rx, ry, rw, rh)) = self.pending_screenshot_region.take() {
                    Self::capture_screenshot_region_impl(
                        &mut self.renderer,
                        out_w,
                        out_h,
                        &elements,
                        &region_path,
                        rx,
                        ry,
                        rw,
                        rh,
                    );
                }
            }

            // ── wlr-screencopy fulfillment ──────────────────────────────────
            if let Some(ref pending_queue) = self.screencopy_pending {
                let output_ref = &self.outputs[out_idx].output;
                Self::fulfill_screencopy_frames(
                    &mut self.renderer,
                    &mut self.screencopy_offscreen,
                    output_ref,
                    out_w,
                    out_h,
                    &elements,
                    pending_queue,
                    self.capture_counters.as_ref(),
                );
            }

            // ── ext-image-copy-capture fulfillment ──────────────────────────
            if let Some(ref pending_queue) = self.image_capture_pending {
                let output_ref = &self.outputs[out_idx].output;
                Self::fulfill_image_capture_frames(
                    &mut self.renderer,
                    &mut self.screencopy_offscreen,
                    output_ref,
                    out_w,
                    out_h,
                    &elements,
                    pending_queue,
                    self.capture_counters.as_ref(),
                );
            }

            // Re-borrow for render_frame + queue_frame.
            let flush_tx = self.flush_tx.clone();
            let flush_pending = self.flush_pending.clone();
            let out = &mut self.outputs[out_idx];

            match out.drm_output.render_frame(
                &mut self.renderer,
                &elements,
                smithay::backend::renderer::Color32F::new(0.0, 0.0, 0.0, 1.0),
                FrameFlags::DEFAULT,
            ) {
                Ok(res) => {
                    if res.is_empty {
                        out.send_frame_callbacks = true;
                        out.frame_callback_roots = frame_roots;
                        out.frame_callback_visible = visible_surfaces;
                        Self::deliver_frame_callbacks(out, &flush_tx, &flush_pending, None);
                        continue;
                    }

                    if let Err(err) = out.drm_output.queue_frame(()) {
                        log::warn!("drm queue_frame failed: {err:?}");

                        // If we started while not being DRM master (e.g. GNOME was active),
                        // switching VTs later can make us eligible to become master. Try to
                        // (re-)activate the DRM backend so subsequent frames can be queued.
                        match self.drm_output_manager.lock().activate(false) {
                            Ok(_) => {
                                log::info!(
                                    "drm backend activated after queue_frame failure; will retry rendering"
                                );
                                self.needs_render = true;
                            }
                            Err(act_err) => {
                                log::warn!(
                                    "drm backend activate failed after queue_frame failure: {act_err:?}"
                                );
                            }
                        }
                    } else {
                        out.frame_pending = true;
                        out.frame_pending_since = Some(std::time::Instant::now());
                        out.send_frame_callbacks = true;
                        out.frame_callback_roots = frame_roots;
                        out.frame_callback_visible = visible_surfaces;
                    }
                }
                Err(err) => {
                    log::warn!("drm render_frame failed: {err:?}");

                    match self.drm_output_manager.lock().activate(false) {
                        Ok(_) => {
                            log::info!(
                                "drm backend activated after render_frame failure; will retry rendering"
                            );
                            self.needs_render = true;
                        }
                        Err(act_err) => {
                            log::warn!(
                                "drm backend activate failed after render_frame failure: {act_err:?}"
                            );
                        }
                    }
                }
            }
        }

        // ── ext-image-copy-capture toplevel (single-window) fulfillment ──
        // Output-independent: run once after all outputs are rendered.
        if let Some(ref pending_queue) = self.image_capture_pending {
            Self::fulfill_image_capture_toplevel_frames(
                &mut self.renderer,
                &mut self.image_capture_toplevel_offscreen,
                state,
                pending_queue,
                self.capture_counters.as_ref(),
            );
        }

        if !any_skipped {
            self.needs_render = false;
        }

        // Rendering can enqueue Wayland events (enter/leave, etc.).
        if !self.flush_pending.swap(true, Ordering::SeqCst) {
            let _ = self.flush_tx.send(());
        }
    }

    pub(super) fn on_vblank(
        &mut self,
        crtc: crtc::Handle,
        metadata: &mut Option<DrmEventMetadata>,
    ) {
        let flush_tx = self.flush_tx.clone();
        let flush_pending = self.flush_pending.clone();
        let Some(out) = self.outputs.iter_mut().find(|o| o.crtc == crtc) else {
            return;
        };

        if let Err(err) = out.drm_output.frame_submitted() {
            log::debug!("drm frame_submitted error: {err:?}");
        }
        out.frame_pending = false;
        out.frame_pending_since = None;

        // Extract precise flip timestamp from DRM metadata for presentation feedback
        let presentation_time = metadata.as_ref().and_then(|m| match m.time {
            smithay::backend::drm::DrmEventTime::Monotonic(t) => Some(t),
            smithay::backend::drm::DrmEventTime::Realtime(_) => None,
        });

        if let Some(vblank_time) = presentation_time {
            out.last_vblank = Some(vblank_time);
            out.last_vblank_received_at = Some(std::time::Instant::now());
            self.last_presentation_time = Some(std::time::Instant::now());
        }

        Self::deliver_frame_callbacks(out, &flush_tx, &flush_pending, presentation_time);
    }
}

impl Drop for KmsState {
    /// Best-effort cleanup of any GAMMA_LUT blobs still tracked at teardown.
    /// The kernel reclaims blobs on FD close anyway, so this is belt-and-braces
    /// — it eliminates the brief window in which an orderly shutdown leaks a
    /// blob reference and avoids "blob leaked" dmesg warnings under
    /// drm.debug=0x4.
    fn drop(&mut self) {
        let blobs: Vec<u64> = self
            .outputs
            .iter()
            .flat_map(|o| {
                o.installed_gamma_lut
                    .map(|(id, _)| id)
                    .into_iter()
                    .chain(o.installed_ctm.into_iter())
            })
            .collect();
        if blobs.is_empty() {
            return;
        }
        let mgr = self.drm_output_manager.lock();
        let dev = mgr.device();
        for id in blobs {
            let _ = dev.destroy_property_blob(id);
        }
    }
}

/// Convert a wl_output transform numeric value (0..=7) into a smithay `Transform`.
fn wl_transform_to_smithay(value: i32) -> Transform {
    match value {
        1 => Transform::_90,
        2 => Transform::_180,
        3 => Transform::_270,
        4 => Transform::Flipped,
        5 => Transform::Flipped90,
        6 => Transform::Flipped180,
        7 => Transform::Flipped270,
        _ => Transform::Normal,
    }
}

fn pick_crtc(
    drm_device: &DrmDevice,
    res: &smithay::reexports::drm::control::ResourceHandles,
    conn: &connector::Info,
    used_crtcs: &HashSet<crtc::Handle>,
) -> Option<crtc::Handle> {
    // Prefer encoder's current CRTC, otherwise pick the first possible.
    for enc_handle in conn.encoders() {
        let enc = drm_device.get_encoder(*enc_handle).ok()?;
        if let Some(crtc) = enc.crtc() {
            if !used_crtcs.contains(&crtc) {
                return Some(crtc);
            }
        }

        let possible = enc.possible_crtcs();
        for crtc in res.filter_crtcs(possible) {
            if !used_crtcs.contains(&crtc) {
                return Some(crtc);
            }
        }
    }

    None
}

/// Save raw RGBA pixel data as a PNG file.
///
/// Smithay's `OutputDamageTracker::render_output` flips Y in its projection matrix,
/// so `copy_framebuffer` already returns rows in top-to-bottom (scanout) order.
fn save_rgba_png(
    path: &std::path::Path,
    width: u32,
    height: u32,
    pixels: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::BufWriter;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::File::create(path)?;
    let w = BufWriter::new(file);

    let mut encoder = png::Encoder::new(w, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;

    writer.write_image_data(pixels)?;
    Ok(())
}
