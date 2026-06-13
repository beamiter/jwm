use crate::sync_ext::MutexExt;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use smithay::backend::allocator::format::FormatSet;
use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::allocator::Format as DmabufFormat;
use smithay::backend::allocator::Fourcc;
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
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::texture::TextureRenderElement;
use smithay::backend::renderer::element::{AsRenderElements, Id, Kind};
use smithay::backend::renderer::gles::ffi as gl_ffi;
use smithay::backend::renderer::gles::GlesRenderbuffer;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::utils::RendererSurfaceStateUserData;
use smithay::backend::renderer::{Bind, ExportMem, Offscreen, Renderer};
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::Session;
use smithay::desktop::layer_map_for_output;
use smithay::desktop::space::SurfaceTree;
use smithay::desktop::utils::send_frames_surface_tree;
use smithay::output::{Mode as WlMode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::channel::Sender;
use smithay::reexports::calloop::{LoopHandle, RegistrationToken};
use smithay::reexports::drm::control::{connector, crtc, Device as ControlDevice, ModeTypeFlags};
use smithay::reexports::rustix::fs::OFlags;
use smithay::reexports::wayland_server;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::{Buffer as BufferCoord, Size};
use smithay::utils::{DeviceFd, Physical, Point, Rectangle, Scale, Transform};
use smithay::wayland::compositor::{with_states, with_surface_tree_downward, TraversalAction};
use smithay::wayland::shell::wlr_layer::Layer as WlrLayer;
use smithay::wayland::shell::xdg::SurfaceCachedState;
use smithay::wayland::presentation::{PresentationFeedbackCachedState, Refresh};

use crate::backend::common_define::StdCursorKind;

use xcursor::{
    parser::{parse_xcursor, Image},
    CursorTheme,
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

    send_frame_callbacks: bool,
    frame_callback_roots: Vec<WlSurface>,
    frame_callback_throttle: Option<std::time::Duration>,
    frame_callback_visible: HashSet<wayland_server::Weak<WlSurface>>,

    surfaces_on_output: HashSet<wayland_server::Weak<WlSurface>>,

    last_vblank: Option<std::time::Duration>,
    refresh_interval: std::time::Duration,
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

    outputs: Vec<KmsOutputState>,

    /// Latest vblank presentation timestamp (monotonic) for frame pacing feedback.
    last_presentation_time: Option<std::time::Instant>,
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
        self.dmabuf_render_formats().iter().any(|f| {
            f.code == Fourcc::Argb2101010 || f.code == Fourcc::Xrgb2101010
        })
    }

    /// Query VRR capabilities for a given output (by index into self.outputs).
    pub(super) fn query_vrr_for_output(&mut self, output_idx: usize) -> Option<crate::backend::api::VrrCapabilities> {
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

    /// Set VRR enabled/disabled for a given output (by index into self.outputs).
    pub(super) fn set_vrr_for_output(&mut self, output_idx: usize, enabled: bool) -> Result<(), String> {
        let output = self.outputs.get(output_idx).ok_or("output index out of range")?;
        let crtc = output.crtc;
        let mgr = self.drm_output_manager.lock();
        let dev = mgr.device();
        if let Ok(props) = dev.get_properties(crtc) {
            let (handles, _values) = props.as_props_and_values();
            for &prop_handle in handles {
                if let Ok(info) = dev.get_property(prop_handle) {
                    if info.name().to_str() == Ok("VRR_ENABLED") {
                        dev.set_property(crtc, prop_handle, if enabled { 1 } else { 0 })
                            .map_err(|e| format!("DRM set_property failed: {e:?}"))?;
                        return Ok(());
                    }
                }
            }
        }
        Err("VRR_ENABLED property not found on CRTC".to_string())
    }

    pub(super) fn output_index_by_name(&self, name: &str) -> Option<usize> {
        self.outputs.iter().position(|o| o.output.name() == name)
    }

    pub(super) fn set_dpms_for_output(&mut self, output_idx: usize, on: bool) -> Result<(), String> {
        let output = self.outputs.get(output_idx).ok_or("output index out of range")?;
        let conn_handle = output.connector;
        let mgr = self.drm_output_manager.lock();
        let dev = mgr.device();
        if let Ok(props) = dev.get_properties(conn_handle) {
            let (handles, _values) = props.as_props_and_values();
            for &prop_handle in handles {
                if let Ok(info) = dev.get_property(prop_handle) {
                    if info.name().to_str() == Ok("DPMS") {
                        let val = if on { 0 } else { 3 }; // 0=On, 3=Off
                        dev.set_property(conn_handle, prop_handle, val)
                            .map_err(|e| format!("DRM set_property DPMS failed: {e:?}"))?;
                        return Ok(());
                    }
                }
            }
        }
        Err("DPMS property not found on connector".to_string())
    }

    pub(super) fn set_gamma_for_output(&mut self, output_idx: usize, gamma_size: u32, ramp: &[u16]) -> Result<(), String> {
        let output = self.outputs.get(output_idx).ok_or("output index out of range")?;
        let crtc = output.crtc;
        let mgr = self.drm_output_manager.lock();
        let dev = mgr.device();

        let sz = gamma_size as usize;
        let expected_len = sz * 3;
        if ramp.len() != expected_len {
            return Err(format!("gamma ramp length mismatch: got {} expected {}", ramp.len(), expected_len));
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
        let drm_mode = if let Some((w, h, refresh)) = mode {
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
            drop(mgr);
            match found {
                Some(m) if self.outputs[idx].output.current_mode() != Some(WlMode::from(m)) => {
                    Some(m)
                }
                Some(_) => None, // already the current mode; skip the modeset
                None => {
                    return Err(format!(
                        "requested mode {w}x{h}@{refresh} not available on '{name}'"
                    ))
                }
            }
        } else {
            None
        };

        // Riskiest step first: perform the DRM modeset before advertising it.
        if let Some(m) = drm_mode {
            let elements: DrmOutputRenderElements<GlesRenderer, SolidColorRenderElement> =
                DrmOutputRenderElements::default();
            self.outputs[idx]
                .drm_output
                .use_mode(m, &mut self.renderer, &elements)
                .map_err(|e| format!("DRM use_mode failed: {e:?}"))?;
            self.outputs[idx].mode_size = (m.size().0 as i32, m.size().1 as i32);
        }

        // Advertise updated state to wl_output clients and update layout origin.
        let new_wl_mode = drm_mode.map(WlMode::from);
        let new_transform = transform.map(wl_transform_to_smithay);
        let new_scale = scale.map(smithay::output::Scale::Fractional);
        let new_loc = position.map(Point::from);
        self.outputs[idx]
            .output
            .change_current_state(new_wl_mode, new_transform, new_scale, new_loc);
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
    fn fulfill_screencopy_frames(
        renderer: &mut GlesRenderer,
        output: &Output,
        width: i32,
        height: i32,
        elements: &[KmsRenderElement],
        pending: &crate::backend::wayland_udev::screencopy::PendingScreencopyQueue,
    ) {
        use smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_frame_v1;
        use smithay::wayland::shm::with_buffer_contents_mut;

        let mut frames: Vec<crate::backend::wayland_udev::screencopy::PendingScreencopyFrame> = {
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

        // Render to offscreen buffer (same approach as screenshot capture).
        let size: Size<i32, BufferCoord> = (width, height).into();
        let mut renderbuffer: GlesRenderbuffer =
            match Offscreen::create_buffer(renderer, Fourcc::Abgr8888, size) {
                Ok(rb) => rb,
                Err(e) => {
                    log::error!("[screencopy] create offscreen buffer failed: {e:?}");
                    for f in &frames {
                        f.frame.failed();
                    }
                    return;
                }
            };

        let mut target = match renderer.bind(&mut renderbuffer) {
            Ok(t) => t,
            Err(e) => {
                log::error!("[screencopy] bind offscreen failed: {e:?}");
                for f in &frames {
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
                }
                Err(e) => {
                    log::warn!("[screencopy] buffer access failed: {e:?}");
                    frame_info.frame.failed();
                }
            }
        }
    }

    pub(super) fn outputs(&self) -> Vec<Output> {
        self.outputs.iter().map(|o| o.output.clone()).collect()
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

                let mode = conn
                    .modes()
                    .iter()
                    .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
                    .copied()
                    .or_else(|| conn.modes().first().copied())
                    .unwrap();

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
                                if let Err(e) = dev.set_property(p.crtc, prop_handle, 1) {
                                    log::debug!("[kms] failed to enable VRR on crtc {:?}: {e:?}", p.crtc);
                                } else {
                                    log::info!("[kms] VRR enabled on crtc {:?}", p.crtc);
                                }
                                break;
                            }
                        }
                    }
                }
            }

            let refresh_interval = p.frame_callback_throttle.unwrap_or(std::time::Duration::from_millis(16));
            outputs.push(KmsOutputState {
                crtc: p.crtc,
                connector: p.connector,
                mode_size: p.mode_size,
                origin: p.origin,
                output: p.output,
                drm_output,
                frame_pending: false,
                send_frame_callbacks: false,
                frame_callback_roots: Vec::new(),
                frame_callback_throttle: p.frame_callback_throttle,
                frame_callback_visible: HashSet::new(),
                surfaces_on_output: HashSet::new(),
                last_vblank: None,
                refresh_interval,
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

            outputs,
            last_presentation_time: None,
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

        let mut any_skipped = false;
        for out_idx in 0..self.outputs.len() {
            let frame_pending = self.outputs[out_idx].frame_pending;
            if frame_pending {
                any_skipped = true;
                continue;
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
            let direct_scanout_eligible = compositor.is_some()
                && elements.is_empty()  // no cursor on this output, no overlay layers
                && state.window_stack.len() == 1
                && state.window_stack.first().map_or(false, |win| {
                    state.window_is_fullscreen.get(win).copied().unwrap_or(false)
                        && state.mapped_windows.contains(win)
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
                        // Leak once to prevent Smithay's Drop from calling glDeleteTextures
                        // on the compositor's owned FBO texture.
                        std::mem::forget(tex.clone());
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
                    let tree = SurfaceTree::from_surface(&surface);
                    let window_elements: Vec<KmsRenderElement> =
                        AsRenderElements::<GlesRenderer>::render_elements(
                            &tree,
                            &mut self.renderer,
                            location,
                            scale,
                            1.0,
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
                for (im_surface, abs_x, abs_y) in state.im_popup_positions() {
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
                    output_ref,
                    out_w,
                    out_h,
                    &elements,
                    pending_queue,
                );
            }

            // Re-borrow for render_frame + queue_frame.
            let out = &mut self.outputs[out_idx];

            match out.drm_output.render_frame(
                &mut self.renderer,
                &elements,
                smithay::backend::renderer::Color32F::new(0.0, 0.0, 0.0, 1.0),
                FrameFlags::DEFAULT,
            ) {
                Ok(res) => {
                    if res.is_empty {
                        out.send_frame_callbacks = false;
                        out.frame_callback_roots.clear();
                        out.frame_callback_visible.clear();
                        continue;
                    }

                    if let Err(err) = out.drm_output.queue_frame(()) {
                        log::warn!("drm queue_frame failed: {err:?}");

                        // If we started while not being DRM master (e.g. GNOME was active),
                        // switching VTs later can make us eligible to become master. Try to
                        // (re-)activate the DRM backend so subsequent frames can be queued.
                        match self.drm_output_manager.lock().activate(false) {
                            Ok(_) => {
                                log::info!("drm backend activated after queue_frame failure; will retry rendering");
                                self.needs_render = true;
                            }
                            Err(act_err) => {
                                log::warn!("drm backend activate failed after queue_frame failure: {act_err:?}");
                            }
                        }
                    } else {
                        out.frame_pending = true;
                        out.send_frame_callbacks = true;
                        out.frame_callback_roots = frame_roots;
                        out.frame_callback_visible = visible_surfaces;
                    }
                }
                Err(err) => {
                    log::warn!("drm render_frame failed: {err:?}");

                    match self.drm_output_manager.lock().activate(false) {
                        Ok(_) => {
                            log::info!("drm backend activated after render_frame failure; will retry rendering");
                            self.needs_render = true;
                        }
                        Err(act_err) => {
                            log::warn!("drm backend activate failed after render_frame failure: {act_err:?}");
                        }
                    }
                }
            }
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
        let Some(out) = self.outputs.iter_mut().find(|o| o.crtc == crtc) else {
            return;
        };

        if let Err(err) = out.drm_output.frame_submitted() {
            log::debug!("drm frame_submitted error: {err:?}");
        }
        out.frame_pending = false;

        // Extract precise flip timestamp from DRM metadata for presentation feedback
        let presentation_time = metadata
            .as_ref()
            .and_then(|m| match m.time {
                smithay::backend::drm::DrmEventTime::Monotonic(t) => Some(t),
                smithay::backend::drm::DrmEventTime::Realtime(_) => None,
            });

        if let Some(vblank_time) = presentation_time {
            out.last_vblank = Some(vblank_time);
            self.last_presentation_time = Some(std::time::Instant::now());
        }

        if out.send_frame_callbacks {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or(std::time::Duration::ZERO);

            let throttle = out.frame_callback_throttle;
            let output = out.output.clone();
            let visible = out.frame_callback_visible.clone();
            let refresh = out.refresh_interval;

            for root in &out.frame_callback_roots {
                // Send presentation feedback for wp_presentation protocol
                if let Some(vblank_time) = presentation_time {
                    with_surface_tree_downward(
                        root,
                        (),
                        |_, _, _| TraversalAction::DoChildren(()),
                        |_surface, states, _| {
                            let mut cached = states.cached_state.get::<PresentationFeedbackCachedState>();
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

                send_frames_surface_tree(root, &output, now, throttle, |surface, states| {
                    let data = states.data_map.get::<RendererSurfaceStateUserData>();
                    let Some(data) = data else {
                        return None;
                    };
                    if data.lock_safe().view().is_none() {
                        return None;
                    }
                    if visible.contains(&surface.downgrade()) {
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
            if !self.flush_pending.swap(true, Ordering::SeqCst) {
                let _ = self.flush_tx.send(());
            }
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
