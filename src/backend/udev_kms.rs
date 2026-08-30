use crate::sync_ext::MutexExt;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use smithay::backend::allocator::Format as DmabufFormat;
use smithay::backend::allocator::Fourcc;
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::format::FormatSet;
use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::drm::compositor::{FrameFlags, PrimaryPlaneElement};
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
use smithay::utils::{Buffer as BufferCoord, Clock, Monotonic, Size};
use smithay::utils::{DeviceFd, Physical, Point, Rectangle, Scale, Transform};
use smithay::wayland::compositor::{TraversalAction, with_states, with_surface_tree_downward};
use smithay::wayland::dmabuf::get_dmabuf;
use smithay::wayland::presentation::{PresentationFeedbackCachedState, Refresh};
use smithay::wayland::shell::wlr_layer::Layer as WlrLayer;
use smithay::wayland::shell::xdg::SurfaceCachedState;

use crate::backend::api::CompositorRect;
use crate::backend::common_define::StdCursorKind;
use crate::backend::error::{BackendErrorContext, ErrorBoundary};

use xcursor::{CursorTheme, parser::Image};

smithay::backend::renderer::element::render_elements! {
    pub KmsRenderElement<=GlesRenderer>;
    Surface=WaylandSurfaceRenderElement<GlesRenderer>,
    Solid=SolidColorRenderElement,
    Memory=MemoryRenderBufferRenderElement<GlesRenderer>,
    Texture=TextureRenderElement<GlesTexture>,
}

pub(super) type KmsHandle = Rc<RefCell<KmsState>>;

/// `[wayland-udev/renderer] operation` context for frame-production and
/// capture log lines. These errors never cross an API boundary (the render
/// loop degrades and retries instead of propagating), so the roadmap's
/// backend-tagged context appears in the log record itself.
fn renderer_ctx(operation: &'static str) -> BackendErrorContext {
    BackendErrorContext::new("wayland-udev", ErrorBoundary::Renderer, operation)
}

/// `[wayland-udev/device] operation` context for DRM/KMS device operations.
fn device_ctx(operation: &'static str) -> BackendErrorContext {
    BackendErrorContext::new("wayland-udev", ErrorBoundary::Device, operation)
}

const fn compositor_output_texture_identity_matches(
    texture: u32,
    generation: u64,
    candidate_texture: u32,
    candidate_generation: u64,
) -> bool {
    texture == candidate_texture && generation == candidate_generation
}

struct KmsOutputState {
    crtc: crtc::Handle,
    connector: connector::Handle,
    mode_size: (i32, i32),
    origin: (i32, i32),
    /// Set when a failed `use_mode` could not restore the previously active
    /// DRM mode. wl_output still advertises the old mode in that case, so the
    /// transaction rollback must not mistake the userspace cache for proof
    /// that hardware was restored.
    drm_mode_uncertain: bool,

    output: Output,
    drm_output: DrmOutput<
        GbmAllocator<DrmDeviceFd>,
        GbmFramebufferExporter<DrmDeviceFd>,
        QueuedFrameData,
        DrmDeviceFd,
    >,

    frame_pending: bool,
    frame_pending_boundary: Option<FrameQueueBoundary>,
    /// A watchdog/DPMS cancellation can race a late page-flip event. The first
    /// subsequent event re-establishes ordering but is not trusted as a color
    /// delivery observation.
    color_delivery_observation_uncertain: bool,
    /// Keep forcing a damaged replacement buffer until a post-cancellation or
    /// post-reactivation frame reaches a trustworthy vblank. This prevents a
    /// static, no-damage desktop from leaving diagnostics unknown forever.
    color_delivery_retry_required: bool,
    /// Color-domain plan paired with the queued framebuffer plus the most
    /// recent plan confirmed by a page-flip/vblank.
    color_delivery: OutputColorDeliveryTracker,
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
    /// monitor is sRGB-primaries). EDID is attached after KMS construction, so
    /// `refresh_output_color_targets` replaces this target and drops any stale
    /// blob whenever the advertised output description changes.
    installed_ctm: Option<u64>,
    /// Per-output target transfer function, refreshed after EDID attachment.
    output_tf: crate::backend::wayland_udev::color_pipeline::TransferKind,
    /// Per-output sRGB→output-primaries 3x3 matrix, cached from the current
    /// output description. Pushed via `install_ctm` together with the output
    /// OETF LUT when hardware owns delivery; otherwise the same matrix is
    /// consumed by that output's software region pass.
    output_ctm: [f32; 9],
    /// A live zwlr-gamma-control client owns the legacy CRTC ramp. While set,
    /// compositor OETF offload stays disabled and software output delivery
    /// feeds encoded pixels through the user ramp instead of competing for the
    /// same GAMMA_LUT state.
    legacy_gamma_override: bool,
    /// `true` while DPMS is off; the LUT install path skips this output.
    dpms_off: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct OutputConfigurationState {
    mode: (i32, i32, i32),
    position: (i32, i32),
    scale: f64,
    wl_transform: i32,
    dpms_on: bool,
}

#[derive(Clone, Debug)]
struct OutputConfigurationSnapshotEntry {
    name: String,
    state: OutputConfigurationState,
}

/// Pre-mutation state for one wlr-output-management transaction. Keeping the
/// snapshot inside KMS ensures the DRM mode/refresh and the internal DPMS
/// tracker are captured from the same owner which will perform rollback.
#[derive(Clone, Debug)]
pub(super) struct OutputConfigurationSnapshot {
    entries: Vec<OutputConfigurationSnapshotEntry>,
}

/// Produce a reverse, de-duplicated rollback plan. A repeated output is
/// restored once at its last mutation point; every touched output must have
/// been captured before the transaction started.
fn plan_output_configuration_rollback(
    snapshot_names: &[String],
    touched_outputs: &[String],
) -> Result<Vec<usize>, String> {
    let mut seen = HashSet::new();
    let mut plan = Vec::new();
    for name in touched_outputs.iter().rev() {
        if !seen.insert(name.as_str()) {
            continue;
        }
        let index = snapshot_names
            .iter()
            .position(|snapshot_name| snapshot_name == name)
            .ok_or_else(|| format!("touched output '{name}' is missing from the snapshot"))?;
        plan.push(index);
    }
    Ok(plan)
}

fn rollback_mode_requires_restore(
    current: Option<(i32, i32, i32)>,
    expected: (i32, i32, i32),
    drm_mode_uncertain: bool,
) -> bool {
    drm_mode_uncertain || current != Some(expected)
}

#[derive(Clone, Copy, Debug)]
struct OutputColorRegionCandidate {
    participating: bool,
    origin: (i32, i32),
    mode_size: (i32, i32),
    scale: f64,
    transform: Transform,
    output_tf: crate::backend::wayland_udev::color_pipeline::TransferKind,
    working_to_output_row_major: [f32; 9],
}

fn physical_rects_overlap(a: [i32; 4], b: [i32; 4]) -> bool {
    let ax1 = i64::from(a[0]) + i64::from(a[2]);
    let ay1 = i64::from(a[1]) + i64::from(a[3]);
    let bx1 = i64::from(b[0]) + i64::from(b[2]);
    let by1 = i64::from(b[1]) + i64::from(b[3]);
    i64::from(a[0]) < bx1 && i64::from(b[0]) < ax1 && i64::from(a[1]) < by1 && i64::from(b[1]) < ay1
}

/// Build the software delivery partitions supported by the current single
/// global framebuffer. Any unsupported topology rejects the entire plan:
/// partially applying output transforms would leave the texture in mixed or
/// ambiguous color domains.
fn plan_software_color_regions(
    candidates: &[OutputColorRegionCandidate],
) -> Option<Vec<crate::backend::wayland_udev::color_pipeline::OutputColorRegion>> {
    use crate::backend::wayland_udev::color_pipeline::OutputColorRegion;

    let mut regions: Vec<OutputColorRegion> = Vec::new();
    for candidate in candidates
        .iter()
        .filter(|candidate| candidate.participating)
    {
        let (x, y) = candidate.origin;
        let (width, height) = candidate.mode_size;
        if x < 0
            || y < 0
            || width <= 0
            || height <= 0
            || candidate.scale != 1.0
            || candidate.transform != Transform::Normal
            || x.checked_add(width).is_none()
            || y.checked_add(height).is_none()
        {
            return None;
        }

        let region = OutputColorRegion {
            rect: [x, y, width, height],
            output_tf: candidate.output_tf,
            working_to_output_row_major: candidate.working_to_output_row_major,
        };
        for previous in &regions {
            let same_profile = previous.output_tf == region.output_tf
                && previous.working_to_output_row_major == region.working_to_output_row_major;
            if !same_profile && physical_rects_overlap(previous.rect, region.rect) {
                return None;
            }
        }
        regions.push(region);
    }
    Some(regions)
}

fn output_color_target(
    params: &crate::backend::wayland_udev::color_management::ParametricParams,
) -> (
    crate::backend::wayland_udev::color_pipeline::TransferKind,
    [f32; 9],
) {
    use crate::backend::wayland_udev::color_pipeline::{
        ColorSpacePrimaries, TransferKind, rgb_to_rgb_matrix,
    };

    let output_tf = TransferKind::from_params(params);
    let output_primaries = ColorSpacePrimaries::from_params(params);
    let working_to_output = rgb_to_rgb_matrix(&ColorSpacePrimaries::SRGB_D65, &output_primaries);
    (output_tf, working_to_output)
}

fn gamma_ramp_is_identity(gamma_size: u32, ramp: &[u16]) -> bool {
    let size = gamma_size as usize;
    if size == 0 || ramp.len() != size.saturating_mul(3) {
        return false;
    }
    let denominator = (size.max(2) - 1) as u64;
    ramp.chunks_exact(size).all(|channel| {
        channel.iter().enumerate().all(|(index, &value)| {
            value == ((index as u64 * u64::from(u16::MAX)) / denominator) as u16
        })
    })
}

/// Blob-valued CRTC properties whose contents change the color domain of the
/// framebuffer reaching scanout. The tracker must never assume these are in
/// their neutral state merely because a new `KmsState` has no blob handles of
/// its own: DRM state can outlive the userspace FD which originally installed
/// it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CrtcColorProperty {
    DegammaLut,
    Ctm,
    GammaLut,
}

fn crtc_color_property(name: &str) -> Option<CrtcColorProperty> {
    match name {
        "DEGAMMA_LUT" => Some(CrtcColorProperty::DegammaLut),
        "CTM" => Some(CrtcColorProperty::Ctm),
        "GAMMA_LUT" => Some(CrtcColorProperty::GammaLut),
        _ => None,
    }
}

fn connector_color_property_neutral_value(name: &str) -> Option<u64> {
    match name {
        "HDR_OUTPUT_METADATA" | "Colorspace" => Some(0),
        _ => None,
    }
}

/// Delivery-stage ownership chosen by `refresh_color_pipeline_offload`.
///
/// Surface transforms do not depend on these flags: a scene-linear frame is
/// always composed in common linear sRGB. The hardware flags only report that
/// every participating CRTC owns the final gamut conversion and OETF;
/// otherwise `software_regions` describes that remaining output work.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct ColorPipelineDecision {
    pub hw_encode_active: bool,
    pub hw_ctm_active: bool,
    /// A tracked CRTC property could not be cleared or replaced. Presenting a
    /// newly encoded frame while the hardware domain is uncertain would be
    /// worse than retaining the last known-good scanout, so the backend keeps
    /// retrying and suppresses KMS submission until ownership is coherent.
    pub delivery_blocked: bool,
    /// `Some` contains a complete, non-conflicting software delivery plan.
    /// `None` requests the renderer's conservative global-sRGB fallback.
    /// Successful all-CRTC hardware delivery also sets this to `None` because
    /// no shader-side output conversion remains.
    pub software_regions:
        Option<Vec<crate::backend::wayland_udev::color_pipeline::OutputColorRegion>>,
}

/// Per-frame inventory of elements Smithay assembles outside the compositor's
/// common linear-sRGB texture. Keeping the classes explicit makes fallback
/// diagnostics actionable and gives later internalization work one checklist
/// to update instead of another aggregate boolean.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LinearTailBlocker {
    CompositorEncodedTail,
    CaptureReadback,
    SessionLockSurface,
    DragIcon,
    Cursor,
    TopOrOverlayLayerSurface,
}

impl LinearTailBlocker {
    const ALL: [Self; 6] = [
        Self::CompositorEncodedTail,
        Self::CaptureReadback,
        Self::SessionLockSurface,
        Self::DragIcon,
        Self::Cursor,
        Self::TopOrOverlayLayerSurface,
    ];

    const fn wire_name(self) -> &'static str {
        crate::backend::api::LINEAR_TAIL_BLOCKER_NAMES[self as usize]
    }

    const fn bit(self) -> u8 {
        1 << self as u8
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct ExternalElementColorPlan {
    blocker_bits: u8,
}

impl ExternalElementColorPlan {
    fn from_frame_flags(capture_readback: bool, session_lock: bool, drag_icon: bool) -> Self {
        let mut plan = Self::default();
        plan.set(LinearTailBlocker::CaptureReadback, capture_readback);
        plan.set(LinearTailBlocker::SessionLockSurface, session_lock);
        plan.set(LinearTailBlocker::DragIcon, drag_icon);
        plan
    }

    fn set(&mut self, blocker: LinearTailBlocker, present: bool) {
        if present {
            self.blocker_bits |= blocker.bit();
        } else {
            self.blocker_bits &= !blocker.bit();
        }
    }

    fn observe_output(
        &mut self,
        cursor: Option<(i32, i32)>,
        origin: (i32, i32),
        mode_size: (i32, i32),
        participating: bool,
        has_top_or_overlay_layer: bool,
    ) {
        if !participating {
            return;
        }
        // An invalid pointer coordinate is not proof that the externally
        // rendered cursor disappeared, so inventory it conservatively.
        if cursor.is_none_or(|point| point_in_output(point, origin, mode_size)) {
            self.set(LinearTailBlocker::Cursor, true);
        }
        if has_top_or_overlay_layer {
            self.set(LinearTailBlocker::TopOrOverlayLayerSurface, true);
        }
    }

    pub(super) fn is_safe(&self) -> bool {
        self.blocker_bits == 0
    }

    pub(super) fn blockers(&self) -> Vec<LinearTailBlocker> {
        LinearTailBlocker::ALL
            .into_iter()
            .filter(|blocker| self.blocker_bits & blocker.bit() != 0)
            .collect()
    }
}

fn rounded_pointer_location(x: f64, y: f64) -> Option<(i32, i32)> {
    if !x.is_finite()
        || !y.is_finite()
        || x.round() < f64::from(i32::MIN)
        || x.round() > f64::from(i32::MAX)
        || y.round() < f64::from(i32::MIN)
        || y.round() > f64::from(i32::MAX)
    {
        return None;
    }
    Some((x.round() as i32, y.round() as i32))
}

fn point_in_output(point: (i32, i32), origin: (i32, i32), mode_size: (i32, i32)) -> bool {
    let (width, height) = mode_size;
    if width <= 0 || height <= 0 {
        return false;
    }
    let (x, y) = (i64::from(point.0), i64::from(point.1));
    let (ox, oy) = (i64::from(origin.0), i64::from(origin.1));
    x >= ox && y >= oy && x < ox + i64::from(width) && y < oy + i64::from(height)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PreparedColorDelivery {
    route: &'static str,
    working_space: &'static str,
    targets_output_profile: bool,
    fallback_reason: Option<&'static str>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ColorDeliveryPlan {
    policy_sequence: u64,
    route: String,
    working_space: String,
    target_transfer_function: String,
    target_primaries: String,
    hdr_metadata_active: bool,
    colorspace_signal: String,
    fallback_reason: Option<String>,
}

#[derive(Clone, Debug)]
struct QueuedFrameData {
    color_delivery: Option<ColorDeliveryPlan>,
}

#[derive(Clone, Copy, Debug)]
struct FrameQueueBoundary {
    monotonic: std::time::Duration,
    realtime: std::time::SystemTime,
}

impl FrameQueueBoundary {
    fn now() -> Self {
        Self {
            monotonic: Clock::<Monotonic>::new().now().into(),
            realtime: std::time::SystemTime::now(),
        }
    }
}

fn vblank_is_not_older_than_queue(
    metadata: Option<&DrmEventMetadata>,
    boundary: FrameQueueBoundary,
) -> bool {
    metadata.is_some_and(|metadata| match metadata.time {
        smithay::backend::drm::DrmEventTime::Monotonic(time) => time >= boundary.monotonic,
        smithay::backend::drm::DrmEventTime::Realtime(time) => time >= boundary.realtime,
    })
}

fn frame_watchdog_timeout(refresh_interval: std::time::Duration) -> std::time::Duration {
    (refresh_interval * 5).max(std::time::Duration::from_millis(100))
}

fn frame_watchdog_remaining(
    refresh_interval: std::time::Duration,
    pending_for: std::time::Duration,
) -> std::time::Duration {
    frame_watchdog_timeout(refresh_interval).saturating_sub(pending_for)
}

fn submitted_color_delivery_observation(
    submitted: Option<QueuedFrameData>,
    observation_uncertain: bool,
) -> (Option<ColorDeliveryPlan>, bool) {
    match submitted {
        Some(frame) if !observation_uncertain => (frame.color_delivery, false),
        // A cancellation may consume Smithay's queued data before a late DRM
        // event arrives. If no such event arrives, the first real new vblank is
        // deliberately fail-closed; request one more frame so a static desktop
        // still converges on a trustworthy observation.
        Some(_) | None => (None, true),
    }
}

#[derive(Clone, Debug)]
struct LastColorDelivery {
    presentation: crate::backend::api::ColorDeliveryPresentationStatus,
    received_at: std::time::Instant,
}

#[derive(Clone, Debug, Default)]
struct OutputColorDeliveryTracker {
    last_success: Option<LastColorDelivery>,
}

impl OutputColorDeliveryTracker {
    fn invalidate(&mut self) {
        self.last_success = None;
    }

    /// Promote user data returned by Smithay for the frame acknowledged at the
    /// backend's presentation-completion boundary.
    fn present(
        &mut self,
        plan: Option<ColorDeliveryPlan>,
        generation: &mut u64,
        presented_at: Option<std::time::Duration>,
        received_at: std::time::Instant,
    ) -> bool {
        let Some(plan) = plan else {
            return false;
        };
        *generation = generation.saturating_add(1);
        self.last_success = Some(LastColorDelivery {
            presentation: crate::backend::api::ColorDeliveryPresentationStatus {
                generation: *generation,
                policy_sequence: plan.policy_sequence,
                route: plan.route,
                working_space: plan.working_space,
                target_transfer_function: plan.target_transfer_function,
                target_primaries: plan.target_primaries,
                hdr_metadata_active: plan.hdr_metadata_active,
                colorspace_signal: plan.colorspace_signal,
                fallback_reason: plan.fallback_reason,
                presented_at_monotonic_ms: presented_at
                    .map(|time| time.as_millis().min(u128::from(u64::MAX)) as u64),
                presented_ago_ms: Some(0),
            },
            received_at,
        });
        true
    }

    fn last_success_status(
        &self,
        now: std::time::Instant,
    ) -> Option<crate::backend::api::ColorDeliveryPresentationStatus> {
        self.last_success.as_ref().map(|last| {
            let mut presentation = last.presentation.clone();
            presentation.presented_ago_ms = Some(
                now.duration_since(last.received_at)
                    .as_millis()
                    .min(u128::from(u64::MAX)) as u64,
            );
            presentation
        })
    }
}

fn prepared_color_delivery(
    decision: &ColorPipelineDecision,
    linear_tail_safe: bool,
    scene_linear_active: bool,
) -> PreparedColorDelivery {
    if decision.delivery_blocked {
        return PreparedColorDelivery {
            route: "hold_last_success",
            working_space: "unknown",
            targets_output_profile: false,
            fallback_reason: Some("kms_color_state_unresolved"),
        };
    }
    if !scene_linear_active {
        return PreparedColorDelivery {
            route: "legacy_encoded_srgb",
            working_space: "legacy_encoded_srgb",
            targets_output_profile: false,
            fallback_reason: Some("scene_linear_target_inactive"),
        };
    }
    if !linear_tail_safe {
        return PreparedColorDelivery {
            route: "global_srgb_fallback",
            working_space: "encoded_srgb",
            targets_output_profile: false,
            fallback_reason: Some("linear_tail_unsafe"),
        };
    }
    if decision.hw_encode_active && decision.hw_ctm_active {
        return PreparedColorDelivery {
            route: "kms_ctm_gamma_lut",
            working_space: "normalized_linear_srgb",
            targets_output_profile: true,
            fallback_reason: None,
        };
    }
    if decision.software_regions.is_some() {
        return PreparedColorDelivery {
            route: "software_per_output_regions",
            working_space: "normalized_linear_srgb",
            targets_output_profile: true,
            fallback_reason: None,
        };
    }
    PreparedColorDelivery {
        route: "global_srgb_fallback",
        working_space: "encoded_srgb",
        targets_output_profile: false,
        fallback_reason: Some("unsupported_output_topology"),
    }
}

fn legacy_color_delivery_attempt_needed(current: Option<&PreparedColorDelivery>) -> bool {
    !current.is_some_and(|prepared| prepared.route == "legacy_encoded_srgb")
}

fn transfer_kind_name(
    transfer: crate::backend::wayland_udev::color_pipeline::TransferKind,
) -> String {
    use crate::backend::wayland_udev::color_pipeline::TransferKind;
    match transfer {
        TransferKind::Linear => "linear".into(),
        TransferKind::Power { gamma_x10000 } => format!("power_{gamma_x10000}"),
        TransferKind::Bt1886 => "bt1886".into(),
        TransferKind::Gamma22 => "gamma22".into(),
        TransferKind::St2084Pq => "st2084_pq".into(),
        TransferKind::Hlg => "hlg".into(),
        TransferKind::Srgb => "srgb".into(),
    }
}

fn output_primaries_name(output: &Output) -> String {
    let params = crate::backend::wayland_udev::color_management::params_for_output(output);
    match params.primaries_named {
        Some(1) => "srgb".into(),
        Some(6) => "bt2020".into(),
        Some(value) => format!("named_{value}"),
        None if params.primaries.is_some() => "custom".into(),
        None => "srgb".into(),
    }
}

/// A CRTC CTM operates on linear light. It is therefore only valid when the
/// output OETF has also moved into the CRTC GAMMA_LUT and the compositor is
/// leaving scene-linear pixels for scanout.
const fn ctm_offload_allowed(
    gate_on: bool,
    hw_encode_active: bool,
    any_participating: bool,
) -> bool {
    gate_on && hw_encode_active && any_participating
}

const fn client_direct_scanout_presented(
    direct_scanout_eligible: bool,
    primary_plane_is_element: bool,
) -> bool {
    // A policy candidate is not an observation. Smithay may still fall back to
    // a swapchain composition when KMS rejects the client buffer.
    direct_scanout_eligible && primary_plane_is_element
}

const fn direct_scanout_allowed_for_color_retry(
    policy_eligible: bool,
    color_delivery_retry_required: bool,
) -> bool {
    // Force one swapchain-backed frame after an uncertain observation. A
    // static direct-scanout element may otherwise produce no new commit even
    // when the swapchain damage history is reset.
    policy_eligible && !color_delivery_retry_required
}

fn frame_flags_for_color_delivery(
    color_delivery_retry_required: bool,
    manual_surface_path: bool,
    direct_scanout_eligible: bool,
) -> FrameFlags {
    if color_delivery_retry_required || (manual_surface_path && !direct_scanout_eligible) {
        // With no effects compositor, the render list contains raw client
        // surfaces. Disallow Smithay plane assignment unless the same policy
        // that labels the route approved it; retries also need a guaranteed
        // swapchain commit even for an unchanged fullscreen client buffer.
        FrameFlags::empty()
    } else {
        FrameFlags::DEFAULT
    }
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
        QueuedFrameData,
        DrmDeviceFd,
    >,
    #[allow(dead_code)]
    gbm: GbmDevice<DrmDeviceFd>,
    renderer: GlesRenderer,

    pub(super) needs_render: bool,
    compositor_texture_cache: Option<(u32, u32, u32, u32, u64, GlesTexture)>,
    // Strong refs to every compositor output-FBO texture generation we've wrapped.
    // Older generations were explicitly deleted by the compositor's resize path,
    // so their wrappers must remain alive until context teardown to avoid a delayed
    // delete of a recycled GL name. Runtime compositor disable retires only the
    // precisely matching current generation, whose texture ownership is then
    // released by Smithay exactly once.
    compositor_texture_keepalive: Vec<(u64, GlesTexture)>,
    background_id: Id,

    cursor_theme: CursorTheme,
    /// Name the current `cursor_theme` was loaded from, so a config hot-reload
    /// can skip re-loading the theme (and clearing caches) when it is unchanged.
    cursor_theme_name: String,
    cursor_size: u32,
    cursor_images: HashMap<String, Vec<Image>>,
    cursor_cache: HashMap<(StdCursorKind, u32), CursorBitmap>,

    cursor_fallback_body_ids: Vec<Id>,
    cursor_fallback_shadow_ids: Vec<Id>,

    screenshot_requests: crate::backend::compositor_common::screenshot::ScreenshotQueue,

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

    /// Mirrors the most recent `ColorPipelineDecision::delivery_blocked` so
    /// `render_if_needed` cannot submit an incompatible framebuffer after a
    /// failed LUT/CTM teardown.
    color_pipeline_delivery_blocked: bool,
    prepared_color_delivery: Option<PreparedColorDelivery>,
    color_delivery_policy_sequence: u64,
    color_delivery_generation: u64,
    last_color_delivery_policy: Option<crate::backend::api::ColorDeliveryPolicyDecisionStatus>,
    /// Set only after the constructor's final all-CRTC neutral-color commit.
    /// A failed/incomplete reinit must not run the Drop reset: the previous
    /// `KmsState` still owns and tracks those live properties.
    owns_scanout_color_state: bool,
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

use crate::backend::xcursor_theme::{cursor_candidates, load_cursor_images, pick_nearest_image};

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
        #[allow(
            clippy::mutable_key_type,
            reason = "Wayland Weak hashes by stable protocol-object identity; its internal liveness flag is not part of Hash or Eq"
        )]
        let visible = out.frame_callback_visible.clone();
        let refresh = out.refresh_interval;
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

            // wp-fifo/wp-commit-timing are intentionally advertised in
            // unmanaged mode; that mode installs no Smithay barriers to
            // signal here. Frame callbacks remain tied to the real vblank.
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
            .and_then(|path| load_cursor_images(&path))
            .unwrap_or_default();

        self.cursor_images.insert(icon.to_string(), images);
        self.cursor_images.get(icon)
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
            let img = pick_nearest_image(images, target_size)?;
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

    /// Re-read the cursor theme/size from the live config (called on hot
    /// reload). Reloads the Xcursor theme only when the name changed, and drops
    /// the rasterized-bitmap cache whenever either the theme or size changed so
    /// the next frame re-rasterizes at the new shape/size.
    pub(super) fn reload_cursor_config(&mut self) {
        let (theme_name, size) = crate::config::CONFIG.load().resolved_cursor();
        let mut changed = false;

        if theme_name != self.cursor_theme_name {
            self.cursor_theme = CursorTheme::load(&theme_name);
            self.cursor_theme_name = theme_name;
            self.cursor_images.clear();
            changed = true;
        }
        if size != self.cursor_size {
            self.cursor_size = size;
            changed = true;
        }

        if changed {
            self.cursor_cache.clear();
            log::info!(
                "[cursor] reloaded theme={:?} size={}px",
                self.cursor_theme_name,
                self.cursor_size
            );
            self.request_render();
        }
    }

    pub(super) fn request_render(&mut self) {
        self.needs_render = true;
    }

    fn invalidate_color_delivery_after_hardware_change(&mut self, output_idx: usize) {
        let Some(out) = self.outputs.get_mut(output_idx) else {
            return;
        };
        out.color_delivery.invalidate();
        if !out.dpms_off {
            if out.frame_pending {
                out.color_delivery_observation_uncertain = true;
            }
            out.color_delivery_retry_required = true;
            self.needs_render = true;
        }
    }

    pub(super) fn any_frame_pending(&self) -> bool {
        self.outputs.iter().any(|o| !o.dpms_off && o.frame_pending)
    }

    /// Return the nearest deadline at which a queued frame must be retired if
    /// its page-flip event never arrives. Keeping this deadline in the outer
    /// event-loop timeout is essential: after a successful queue there may be
    /// no animation, client, or handler work left to wake the loop again.
    pub(super) fn next_frame_watchdog_wakeup(&self) -> Option<std::time::Duration> {
        let now = std::time::Instant::now();
        self.outputs
            .iter()
            .filter(|out| !out.dpms_off && out.frame_pending)
            .map(|out| {
                out.frame_pending_since
                    .map_or(std::time::Duration::ZERO, |since| {
                        let pending_for = now.checked_duration_since(since).unwrap_or_default();
                        frame_watchdog_remaining(out.refresh_interval, pending_for)
                    })
            })
            .min()
    }

    /// Retire page flips that exceeded their refresh-derived deadline before
    /// the scheduler decides whether KMS can accept another frame. This cannot
    /// live in `render_if_needed`: the outer loop deliberately suppresses that
    /// call while any frame is pending.
    pub(super) fn recover_stale_frames(&mut self) -> usize {
        let now = std::time::Instant::now();
        let mut recovered = 0;

        for out in &mut self.outputs {
            if out.dpms_off || !out.frame_pending {
                continue;
            }
            let timeout = frame_watchdog_timeout(out.refresh_interval);
            let stale = out.frame_pending_since.is_none_or(|since| {
                now.checked_duration_since(since).unwrap_or_default() >= timeout
            });
            if !stale {
                continue;
            }

            log::warn!(
                "{}: output {} missed its {:?} page-flip deadline; force-clearing to recover",
                renderer_ctx("await page-flip vblank"),
                out.output.name(),
                timeout,
            );
            // Retire Smithay's queued user data together with the local flag.
            // A late kernel event is rejected by the queue boundary in
            // `on_vblank`, and the next accepted observation is fail-closed.
            let _ = out.drm_output.frame_submitted();
            out.frame_pending = false;
            out.frame_pending_since = None;
            out.frame_pending_boundary = None;
            out.color_delivery_observation_uncertain = true;
            out.color_delivery_retry_required = true;
            out.color_delivery.invalidate();
            recovered += 1;
        }

        if recovered > 0 {
            self.needs_render = true;
        }
        recovered
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
        let bounded_extent = |origin: i32, size: i32| {
            (i64::from(origin) + i64::from(size)).clamp(0, i64::from(i32::MAX)) as u32
        };
        let w = self
            .outputs
            .iter()
            .map(|o| bounded_extent(o.origin.0, o.mode_size.0))
            .max()
            .unwrap_or(1920)
            .max(1);
        let h = self
            .outputs
            .iter()
            .map(|o| bounded_extent(o.origin.1, o.mode_size.1))
            .max()
            .unwrap_or(1080)
            .max(1);
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

    /// Whether Smithay currently owns the compositor output texture through a
    /// `GlesTexture::from_raw` wrapper. The generation is part of the identity:
    /// drivers may recycle the numeric texture name after a compositor resize.
    pub(super) fn compositor_output_texture_is_renderer_owned(
        &self,
        texture: u32,
        generation: u64,
    ) -> bool {
        self.compositor_texture_cache.as_ref().is_some_and(
            |(cached_texture, _, _, _, cached_generation, _)| {
                compositor_output_texture_identity_matches(
                    texture,
                    generation,
                    *cached_texture,
                    *cached_generation,
                )
            },
        ) && self
            .compositor_texture_keepalive
            .iter()
            .any(|(kept_generation, kept_texture)| {
                compositor_output_texture_identity_matches(
                    texture,
                    generation,
                    kept_texture.tex_id(),
                    *kept_generation,
                )
            })
    }

    /// Drop only the current compositor-output wrapper after the compositor
    /// has released every raw object which references it. Older resize-era
    /// wrappers deliberately stay pinned: their raw names were already deleted
    /// and may since have been recycled for unrelated textures.
    pub(super) fn retire_compositor_output_texture(
        &mut self,
        texture: u32,
        generation: u64,
    ) -> bool {
        let cache_matches = self.compositor_texture_cache.as_ref().is_some_and(
            |(cached_texture, _, _, _, cached_generation, _)| {
                compositor_output_texture_identity_matches(
                    texture,
                    generation,
                    *cached_texture,
                    *cached_generation,
                )
            },
        );
        if cache_matches {
            self.compositor_texture_cache.take();
        }

        let before = self.compositor_texture_keepalive.len();
        self.compositor_texture_keepalive
            .retain(|(kept_generation, kept_texture)| {
                !compositor_output_texture_identity_matches(
                    texture,
                    generation,
                    kept_texture.tex_id(),
                    *kept_generation,
                )
            });
        let retired = cache_matches || self.compositor_texture_keepalive.len() != before;
        if retired && let Err(error) = self.renderer.cleanup_texture_cache() {
            // Dropping the wrapper already queued the exact texture name. A
            // later renderer operation or EGL-context teardown will finish it;
            // importantly, no stale raw alias remains in the compositor.
            log::warn!(
                "{}: deferred compositor output texture cleanup after wrapper retirement: {error}",
                renderer_ctx("runtime compositor disable")
            );
        }
        retired
    }

    pub(super) fn request_screenshot(&mut self, path: std::path::PathBuf) {
        self.screenshot_requests.request_full(path);
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
        self.screenshot_requests.request_region(path, x, y, w, h);
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

    /// Whether every element that will reach scanout is already inside the
    /// compositor's common linear-sRGB texture. Smithay currently adds the
    /// cursor, drag icon, lock surface, and top/overlay layer surfaces after
    /// the compositor has finalized that texture. Until those elements gain
    /// an explicit color-domain adapter, their presence requires the global
    /// encoded-sRGB fallback.
    pub(super) fn external_element_color_plan(
        &self,
        state: &crate::backend::wayland::state::JwmWaylandState,
    ) -> ExternalElementColorPlan {
        let capture_pending = self.screenshot_requests.has_pending()
            || self
                .screencopy_pending
                .as_ref()
                .is_some_and(|queue| !queue.lock_safe().is_empty())
            || self
                .image_capture_pending
                .as_ref()
                .is_some_and(|queue| !queue.lock_safe().is_empty());
        let mut plan = ExternalElementColorPlan::from_frame_flags(
            capture_pending,
            state.session_locked,
            state.dnd_icon.is_some(),
        );

        let cursor = rounded_pointer_location(state.pointer_location.x, state.pointer_location.y);
        for output in &self.outputs {
            let participating =
                !output.dpms_off && !state.soft_disabled_outputs.contains(&output.output_name);
            if !participating {
                continue;
            }
            let map = layer_map_for_output(&output.output);
            let has_top_or_overlay_layer = [WlrLayer::Overlay, WlrLayer::Top]
                .into_iter()
                .any(|layer| map.layers_on(layer).next().is_some());
            plan.observe_output(
                cursor,
                output.origin,
                output.mode_size,
                participating,
                has_top_or_overlay_layer,
            );
        }
        plan
    }

    /// Record the current frame's color-delivery decision without claiming it
    /// reached the display. `render_if_needed` attaches the prepared plan to a
    /// successfully queued framebuffer; `on_vblank` is the only promotion
    /// point into the last-success snapshot.
    pub(super) fn record_color_delivery_attempt(
        &mut self,
        decision: &ColorPipelineDecision,
        linear_tail_blockers: &[LinearTailBlocker],
        scene_linear_active: bool,
    ) {
        let linear_tail_safe = linear_tail_blockers.is_empty();
        let prepared = prepared_color_delivery(decision, linear_tail_safe, scene_linear_active);
        self.color_delivery_policy_sequence = self.color_delivery_policy_sequence.saturating_add(1);
        self.last_color_delivery_policy =
            Some(crate::backend::api::ColorDeliveryPolicyDecisionStatus {
                sequence: self.color_delivery_policy_sequence,
                composited_route: prepared.route.into(),
                blocked: decision.delivery_blocked,
                reason: prepared.fallback_reason.map(str::to_owned),
                scene_linear_active,
                linear_tail_safe,
                linear_tail_blockers: Some(
                    linear_tail_blockers
                        .iter()
                        .map(|blocker| blocker.wire_name().to_owned())
                        .collect(),
                ),
            });
        self.prepared_color_delivery = (!decision.delivery_blocked).then_some(prepared);
    }

    fn ensure_legacy_color_delivery_attempt(&mut self) {
        if !legacy_color_delivery_attempt_needed(self.prepared_color_delivery.as_ref()) {
            return;
        }
        let prepared = PreparedColorDelivery {
            route: "legacy_encoded_srgb",
            working_space: "legacy_encoded_srgb",
            targets_output_profile: false,
            fallback_reason: Some("effects_compositor_inactive"),
        };
        self.color_delivery_policy_sequence = self.color_delivery_policy_sequence.saturating_add(1);
        self.last_color_delivery_policy =
            Some(crate::backend::api::ColorDeliveryPolicyDecisionStatus {
                sequence: self.color_delivery_policy_sequence,
                composited_route: prepared.route.into(),
                blocked: false,
                reason: prepared.fallback_reason.map(str::to_owned),
                scene_linear_active: false,
                linear_tail_safe: false,
                linear_tail_blockers: None,
            });
        self.prepared_color_delivery = Some(prepared);
    }

    fn color_delivery_plan_for_output(
        &self,
        output_idx: usize,
        direct_scanout: bool,
    ) -> Option<ColorDeliveryPlan> {
        let output = self.outputs.get(output_idx)?;
        let hdr_metadata_active =
            crate::backend::wayland_udev::color_management::output_hdr_metadata_active(
                &output.output,
            );
        if direct_scanout {
            return Some(ColorDeliveryPlan {
                policy_sequence: self.color_delivery_policy_sequence,
                route: "direct_scanout".into(),
                working_space: "client_buffer".into(),
                target_transfer_function: "source_buffer_unknown".into(),
                target_primaries: "source_buffer_unknown".into(),
                hdr_metadata_active,
                colorspace_signal: if hdr_metadata_active {
                    "hdr_metadata_unspecified_colorspace".into()
                } else {
                    "default_sdr".into()
                },
                fallback_reason: None,
            });
        }

        let prepared = self.prepared_color_delivery.as_ref()?;
        let (target_transfer_function, target_primaries) = if prepared.targets_output_profile {
            (
                transfer_kind_name(output.output_tf),
                output_primaries_name(&output.output),
            )
        } else {
            ("srgb".into(), "srgb".into())
        };
        Some(ColorDeliveryPlan {
            policy_sequence: self.color_delivery_policy_sequence,
            route: prepared.route.into(),
            working_space: prepared.working_space.into(),
            target_transfer_function,
            target_primaries,
            hdr_metadata_active,
            colorspace_signal: if hdr_metadata_active {
                "hdr_metadata_unspecified_colorspace".into()
            } else {
                "default_sdr".into()
            },
            fallback_reason: prepared.fallback_reason.map(str::to_owned),
        })
    }

    /// Rebuild output transfer/gamut targets after the backend has attached
    /// EDID capabilities to each Smithay `Output`.
    ///
    /// KMS construction necessarily precedes that attachment, so the initial
    /// cache is the conservative sRGB default. A changed target invalidates
    /// both pieces of installed hardware state: retaining either the previous
    /// OETF LUT or CTM for even one frame would scan out pixels in the wrong
    /// color domain.
    pub(super) fn refresh_output_color_targets(&mut self) -> bool {
        let targets: Vec<_> = self
            .outputs
            .iter()
            .map(|output| {
                let params = crate::backend::wayland_udev::color_management::params_for_output(
                    &output.output,
                );
                output_color_target(&params)
            })
            .collect();

        let mut changed = false;
        let mut ready = true;
        for (index, (output_tf, output_ctm)) in targets.into_iter().enumerate() {
            if self.outputs[index].output_tf == output_tf
                && self.outputs[index].output_ctm == output_ctm
            {
                continue;
            }

            let mut teardown_ok = true;
            if self.outputs[index].installed_gamma_lut.is_some() {
                if let Err(error) = self.uninstall_gamma_lut(index) {
                    log::warn!(
                        "[kms-cm] stale LUT teardown on {} failed: {error}",
                        self.outputs[index].output_name,
                    );
                    teardown_ok = false;
                }
            }
            if teardown_ok && self.outputs[index].installed_ctm.is_some() {
                if let Err(error) = self.uninstall_ctm(index) {
                    log::warn!(
                        "[kms-cm] stale CTM teardown on {} failed: {error}",
                        self.outputs[index].output_name,
                    );
                    teardown_ok = false;
                }
            }
            if !teardown_ok {
                // Keep the cached target paired with the tracked hardware
                // state. The per-frame refresh retries this transition and
                // suppresses presentation in the meantime.
                ready = false;
                continue;
            }

            self.outputs[index].output_tf = output_tf;
            self.outputs[index].output_ctm = output_ctm;
            changed = true;
            log::info!(
                "[kms-cm] refreshed output target for {} (tf={output_tf:?})",
                self.outputs[index].output_name,
            );
        }

        if changed {
            self.needs_render = true;
        }
        ready
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

    /// Establish the neutral color-domain baseline for every CRTC and
    /// connector claimed by a newly-created KMS state.
    ///
    /// A previous compositor/master may have left blob ids bound even though
    /// this instance starts with `installed_gamma_lut = None` and
    /// `installed_ctm = None`. Clear every blob-valued color stage in one atomic
    /// request so initialization either owns a known encoded-input baseline or
    /// fails without partially invalidating the still-live old KMS state during
    /// a reinit.
    fn reset_scanout_color_properties(
        dev: &DrmDevice,
        crtcs: &[crtc::Handle],
        connectors: &[connector::Handle],
    ) -> Result<usize, String> {
        use smithay::reexports::drm::control::AtomicCommitFlags;
        use smithay::reexports::drm::control::atomic::AtomicModeReq;

        let mut crtc_properties = Vec::new();
        for &crtc in crtcs {
            let props = dev
                .get_properties(crtc)
                .map_err(|e| format!("get CRTC {crtc:?} properties failed: {e:?}"))?;
            let (handles, _values) = props.as_props_and_values();
            for &property in handles {
                let info = dev.get_property(property).map_err(|e| {
                    format!("get CRTC {crtc:?} property {property:?} failed: {e:?}")
                })?;
                let name = info.name().to_str().map_err(|_| {
                    format!("CRTC {crtc:?} property {property:?} has a non-UTF-8 name")
                })?;
                if let Some(kind) = crtc_color_property(name) {
                    crtc_properties.push((crtc, property, kind));
                }
            }
        }

        let mut connector_properties = Vec::new();
        for &connector in connectors {
            let props = dev
                .get_properties(connector)
                .map_err(|e| format!("get connector {connector:?} properties failed: {e:?}"))?;
            let (handles, _values) = props.as_props_and_values();
            for &property in handles {
                let info = dev.get_property(property).map_err(|e| {
                    format!("get connector {connector:?} property {property:?} failed: {e:?}")
                })?;
                let name = info.name().to_str().map_err(|_| {
                    format!("connector {connector:?} property {property:?} has a non-UTF-8 name")
                })?;
                if let Some(neutral_value) = connector_color_property_neutral_value(name) {
                    connector_properties.push((connector, property, neutral_value));
                }
            }
        }

        let property_count = crtc_properties.len() + connector_properties.len();
        if property_count == 0 {
            return Ok(0);
        }
        if !dev.is_atomic() {
            // There is no all-or-nothing transaction with which to protect a
            // still-active old KMS state from a partial reinit reset. Refuse to
            // claim the CRTCs instead of clearing a prefix through legacy
            // SETPROPERTY and then discovering that a later property failed.
            return Err(format!(
                "found {property_count} scanout color properties but atomic modesetting is unavailable"
            ));
        }

        let mut request = AtomicModeReq::new();
        for &(crtc, property, _kind) in &crtc_properties {
            request.add_raw_property(crtc.into(), property, 0);
        }
        for &(connector, property, neutral_value) in &connector_properties {
            request.add_raw_property(connector.into(), property, neutral_value);
        }
        dev.atomic_commit(AtomicCommitFlags::TEST_ONLY, request.clone())
            .map_err(|e| format!("test neutral scanout color reset failed: {e:?}"))?;
        dev.atomic_commit(AtomicCommitFlags::empty(), request)
            .map_err(|e| format!("commit neutral scanout color reset failed: {e:?}"))?;

        Ok(property_count)
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
        if output.drm_mode_uncertain {
            return Err(format!(
                "output '{}' hardware mode is uncertain after a failed modeset rollback",
                output.output_name
            ));
        }
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
        let (conn_handle, smithay_output) = self
            .outputs
            .get(output_idx)
            .map(|output| (output.connector, output.output.clone()))
            .ok_or("output index out of range")?;
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

        let mut property_result = Err("HDR_OUTPUT_METADATA property not found on connector".into());
        if let Ok(props) = dev.get_properties(conn_handle) {
            let (handles, _values) = props.as_props_and_values();
            for &prop_handle in handles {
                if let Ok(info) = dev.get_property(prop_handle) {
                    if info.name().to_str() == Ok("HDR_OUTPUT_METADATA") {
                        property_result =
                            Self::set_drm_property(dev, conn_handle, prop_handle, blob_id);
                        break;
                    }
                }
            }
        }
        drop(mgr);
        property_result?;

        crate::backend::wayland_udev::color_management::set_output_hdr_metadata_active(
            &smithay_output,
            blob.is_some(),
        );
        self.invalidate_color_delivery_after_hardware_change(output_idx);
        if !self.refresh_output_color_targets() {
            self.color_pipeline_delivery_blocked = true;
        }
        self.needs_render = true;
        Ok(())
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
            let participation_changed = self.outputs[output_idx].dpms_off == on;
            self.outputs[output_idx].dpms_off = !on;
            if participation_changed {
                // A presentation observed before a power/participation epoch
                // cannot describe the first frame after re-enable. Keep the
                // aggregate unknown until that frame reaches vblank.
                self.invalidate_color_delivery_after_hardware_change(output_idx);
            }
            if !on && self.outputs[output_idx].frame_pending {
                // A powered-down connector may never emit the vblank for an
                // already queued flip. Retire the DrmOutput bookkeeping now so
                // it cannot keep every other CRTC behind `can_present=false`,
                // and so power-on starts from a clean full-redraw request.
                let _ = self.outputs[output_idx].drm_output.frame_submitted();
                self.outputs[output_idx].frame_pending = false;
                self.outputs[output_idx].frame_pending_since = None;
                self.outputs[output_idx].frame_pending_boundary = None;
                self.outputs[output_idx].color_delivery_observation_uncertain = true;
            }
            // Change the connector power state first. If that write fails the
            // still-visible scanout must retain its matching LUT/CTM instead
            // of being reinterpreted mid-frame. Once blanked, drop both blobs;
            // power-on schedules a fresh frame which reinstalls or replaces
            // the complete delivery plan.
            if !on && self.outputs[output_idx].installed_gamma_lut.is_some() {
                if let Err(e) = self.uninstall_gamma_lut(output_idx) {
                    log::warn!(
                        "[kms-cm] DPMS-off LUT teardown failed on {}: {e}",
                        self.outputs[output_idx].output_name
                    );
                }
            }
            if !on && self.outputs[output_idx].installed_ctm.is_some() {
                if let Err(e) = self.uninstall_ctm(output_idx) {
                    log::warn!(
                        "[kms-cm] DPMS-off CTM teardown failed on {}: {e}",
                        self.outputs[output_idx].output_name
                    );
                }
            }
        }
        result
    }

    /// Return every CRTC to its default encoded-input state before the
    /// compositor (and therefore the common-linear producer) is removed.
    /// Failure keeps the tracked blob alive and blocks scanout so a later
    /// runtime-toggle retry cannot silently feed encoded client buffers
    /// through a stale output transform.
    pub(super) fn disable_color_pipeline(&mut self) -> Result<(), String> {
        for index in 0..self.outputs.len() {
            // Clear the linear-light matrix first. If that fails, leave the
            // paired OETF and all tracked handles untouched; presentation is
            // blocked and the caller retains the compositor for a retry.
            if self.outputs[index].installed_ctm.is_some() {
                if let Err(error) = self.uninstall_ctm(index) {
                    self.color_pipeline_delivery_blocked = true;
                    return Err(error);
                }
            }
            if self.outputs[index].installed_gamma_lut.is_some() {
                if let Err(error) = self.uninstall_gamma_lut(index) {
                    self.color_pipeline_delivery_blocked = true;
                    return Err(error);
                }
            }
        }
        for index in 0..self.outputs.len() {
            if crate::backend::wayland_udev::color_management::output_hdr_metadata_active(
                &self.outputs[index].output,
            ) {
                if let Err(error) = self.set_hdr_metadata_for_output(index, None) {
                    self.color_pipeline_delivery_blocked = true;
                    return Err(error);
                }
            }
        }
        self.color_pipeline_delivery_blocked = false;
        self.prepared_color_delivery = None;
        self.needs_render = true;
        Ok(())
    }

    // ============================================================
    // KMS color pipeline activation (GAMMA_LUT + CTM)
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
        self.invalidate_color_delivery_after_hardware_change(output_idx);
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
        // destroyed. If clearing fails, retain both the blob handle and the
        // tracked state: the CRTC may still reference this transform and the
        // next refresh must be able to retry instead of falsely selecting a
        // software-encoded route underneath it.
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
        prop_result?;
        let _ = dev.destroy_property_blob(blob);
        drop(mgr);

        self.outputs[output_idx].installed_gamma_lut = None;
        self.invalidate_color_delivery_after_hardware_change(output_idx);
        log::info!(
            "[kms-cm] uninstalled GAMMA_LUT on {}",
            self.outputs[output_idx].output_name
        );
        Ok(())
    }

    /// Install a 3×3 CTM (color transform matrix) on the CRTC. Mirrors
    /// `install_gamma_lut`: variable-length blob via `drm_ffi::mode::
    /// create_property_blob`, atomic prop bind, free-on-failure, replace-old-
    /// after-success. The caller supplies the cached sRGB-to-output-primary
    /// matrix for this CRTC (identity on an sRGB-primary output).
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
        self.invalidate_color_delivery_after_hardware_change(output_idx);
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
        prop_result?;
        let _ = dev.destroy_property_blob(blob);
        drop(mgr);

        self.outputs[output_idx].installed_ctm = None;
        self.invalidate_color_delivery_after_hardware_change(output_idx);
        log::info!(
            "[kms-cm] uninstalled CTM on {}",
            self.outputs[output_idx].output_name
        );
        Ok(())
    }

    /// Returns the per-frame color-pipeline decision. Hardware ownership is a
    /// paired CTM+LUT transaction across every participating output; otherwise
    /// independently described software regions consume the common scene.
    pub(super) fn refresh_color_pipeline_offload(
        &mut self,
        state: &crate::backend::wayland::state::JwmWaylandState,
        linear_tail_safe: bool,
        scene_linear_active: bool,
    ) -> ColorPipelineDecision {
        use crate::backend::wayland_udev::color_pipeline::TransferKind;

        let mut hdr_metadata_ready = true;
        if !scene_linear_active {
            for index in 0..self.outputs.len() {
                if crate::backend::wayland_udev::color_management::output_hdr_metadata_active(
                    &self.outputs[index].output,
                ) && let Err(error) = self.set_hdr_metadata_for_output(index, None)
                {
                    log::warn!(
                        "[kms-cm] failed to return {} to SDR signalling: {error}",
                        self.outputs[index].output_name
                    );
                    hdr_metadata_ready = false;
                }
            }
        }

        // EDID/user-data can change independently of a modeset, and a failed
        // property teardown must be retried before any differently encoded
        // framebuffer reaches scanout.
        let output_targets_ready = hdr_metadata_ready && self.refresh_output_color_targets();

        let behavior = crate::config::CONFIG.load();
        let mut gate_on = behavior.behavior().kms_color_pipeline_offload
            && crate::config::scene_linear_render_path_requested(
                behavior.behavior().color_management_render_path,
                behavior.behavior().scene_linear_compositing,
            )
            && linear_tail_safe;
        // CTM transforms linear RGB, and the installed GAMMA_LUT contains the
        // output OETF. Both therefore require an actually allocated linear
        // scene target. `linear_tail_safe` also rejects compositor and
        // KMS-side elements that would otherwise enter after output delivery
        // in the wrong domain.
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
        if self
            .outputs
            .iter()
            .enumerate()
            .any(|(index, output)| participating[index] && output.legacy_gamma_override)
        {
            gate_on = false;
        }

        let region_candidates: Vec<OutputColorRegionCandidate> = self
            .outputs
            .iter()
            .enumerate()
            .map(|(index, output)| OutputColorRegionCandidate {
                participating: participating[index],
                origin: output.origin,
                mode_size: output.mode_size,
                scale: output.output.current_scale().fractional_scale(),
                transform: output.output.current_transform(),
                output_tf: output.output_tf,
                working_to_output_row_major: output.output_ctm,
            })
            .collect();
        let software_regions = plan_software_color_regions(&region_candidates);

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

        let mut decision = ColorPipelineDecision {
            hw_encode_active: false,
            hw_ctm_active: false,
            delivery_blocked: !output_targets_ready,
            software_regions,
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
            return self.finish_color_pipeline_decision(decision, &participating);
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

        // --- CTM activation: only after GAMMA_LUT succeeded for every
        // participant. A CTM is linear-light math; applying it without the
        // hardware OETF would transform shader-encoded pixels in the wrong
        // domain. Install per-output `output_ctm` (sRGB → output primaries)
        // all-or-nothing. When `hw_ctm_active`, the per-surface ColorTransform
        // pass in backend.rs targets sRGB primaries so the FBO is uniform-sRGB
        // and each CRTC's CTM converts to native primaries at scanout.
        for i in 0..n {
            if !participating[i] && self.outputs[i].installed_ctm.is_some() {
                let _ = self.uninstall_ctm(i);
            }
        }

        let mut ctm_capable =
            ctm_offload_allowed(gate_on, decision.hw_encode_active, any_participating);
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

        if decision.hw_encode_active && decision.hw_ctm_active {
            // The CRTC pair consumes the common linear-sRGB texture directly;
            // no software output conversion remains.
            decision.software_regions = None;
        } else {
            // A hardware OETF without the matching linear-light gamut stage
            // leaves no unambiguous domain for the shared framebuffer. Roll
            // back every LUT and use the complete software plan (or the
            // renderer's explicit global-sRGB fallback when it is unavailable).
            if decision.hw_encode_active {
                for i in 0..n {
                    if self.outputs[i].installed_gamma_lut.is_some() {
                        let _ = self.uninstall_gamma_lut(i);
                    }
                }
            }
            if decision.hw_ctm_active {
                for i in 0..n {
                    if self.outputs[i].installed_ctm.is_some() {
                        let _ = self.uninstall_ctm(i);
                    }
                }
            }
            decision.hw_encode_active = false;
            decision.hw_ctm_active = false;
        }

        self.finish_color_pipeline_decision(decision, &participating)
    }

    fn finish_color_pipeline_decision(
        &mut self,
        mut decision: ColorPipelineDecision,
        participating: &[bool],
    ) -> ColorPipelineDecision {
        let hardware_pair_active = decision.hw_encode_active && decision.hw_ctm_active;
        if decision.hw_encode_active != decision.hw_ctm_active {
            decision.delivery_blocked = true;
        }

        for (index, output) in self.outputs.iter().enumerate() {
            if !participating.get(index).copied().unwrap_or(false) {
                continue;
            }
            let has_lut = output.installed_gamma_lut.is_some();
            let has_ctm = output.installed_ctm.is_some();
            let coherent = if hardware_pair_active {
                has_lut && has_ctm
            } else {
                !has_lut && !has_ctm
            };
            if !coherent {
                decision.delivery_blocked = true;
                break;
            }
        }

        self.color_pipeline_delivery_blocked = decision.delivery_blocked;
        if decision.delivery_blocked {
            log::warn!("[kms-cm] holding the last scanout while LUT/CTM ownership is unresolved");
        }
        decision
    }

    pub(super) fn set_gamma_for_output(
        &mut self,
        output_idx: usize,
        gamma_size: u32,
        ramp: &[u16],
    ) -> Result<(), String> {
        let sz = gamma_size as usize;
        let expected_len = sz.checked_mul(3).ok_or("gamma ramp length overflow")?;
        if ramp.len() != expected_len {
            return Err(format!(
                "gamma ramp length mismatch: got {} expected {}",
                ramp.len(),
                expected_len
            ));
        }
        let crtc = self
            .outputs
            .get(output_idx)
            .ok_or("output index out of range")?
            .crtc;
        let identity = gamma_ramp_is_identity(gamma_size, ramp);

        // zwlr-gamma-control and the compositor output OETF both own the CRTC
        // GAMMA_LUT. Never let them coexist: move gamut/OETF delivery back to
        // shaders before installing the client ramp. Identity restoration on
        // resource destruction releases the override for the next frame.
        if self.outputs[output_idx].installed_ctm.is_some() {
            self.uninstall_ctm(output_idx)?;
        }
        if self.outputs[output_idx].installed_gamma_lut.is_some() {
            self.uninstall_gamma_lut(output_idx)?;
        }

        let red = &ramp[..sz];
        let green = &ramp[sz..2 * sz];
        let blue = &ramp[2 * sz..3 * sz];
        let result = {
            let mgr = self.drm_output_manager.lock();
            mgr.device()
                .set_gamma(crtc, red, green, blue)
                .map_err(|e| format!("DRM set_gamma failed: {e:?}"))
        };
        self.outputs[output_idx].legacy_gamma_override = result.is_err() || !identity;
        self.invalidate_color_delivery_after_hardware_change(output_idx);
        result
    }

    fn output_configuration_state(
        &self,
        output_idx: usize,
    ) -> Result<OutputConfigurationState, String> {
        let output = self
            .outputs
            .get(output_idx)
            .ok_or("output index out of range")?;
        let mode = output
            .output
            .current_mode()
            .ok_or_else(|| format!("output '{}' has no current mode", output.output_name))?;
        let location = output.output.current_location();
        let position = (location.x, location.y);
        if output.origin != position {
            return Err(format!(
                "output '{}' has divergent KMS {:?} and wl_output {:?} positions",
                output.output_name, output.origin, position
            ));
        }
        if output.mode_size != (mode.size.w, mode.size.h) {
            return Err(format!(
                "output '{}' has divergent KMS {:?} and wl_output {:?} modes",
                output.output_name,
                output.mode_size,
                (mode.size.w, mode.size.h)
            ));
        }
        Ok(OutputConfigurationState {
            mode: (mode.size.w, mode.size.h, mode.refresh),
            position,
            scale: output.output.current_scale().fractional_scale(),
            wl_transform: smithay_transform_to_wl(output.output.current_transform()),
            dpms_on: !output.dpms_off,
        })
    }

    /// Capture every requested output before the first transaction mutation.
    /// Unknown outputs and internally divergent advertised/KMS state reject the
    /// transaction while it is still side-effect free.
    pub(super) fn snapshot_output_configuration(
        &self,
        output_names: &[String],
    ) -> Result<OutputConfigurationSnapshot, String> {
        let mut entries = Vec::new();
        let mut seen = HashSet::new();
        for name in output_names {
            if !seen.insert(name.as_str()) {
                continue;
            }
            let output_idx = self
                .output_index_by_name(name)
                .ok_or_else(|| format!("unknown output '{name}'"))?;
            entries.push(OutputConfigurationSnapshotEntry {
                name: name.clone(),
                state: self.output_configuration_state(output_idx)?,
            });
        }
        Ok(OutputConfigurationSnapshot { entries })
    }

    /// Restore every output whose mutation was attempted, in reverse order.
    /// The existing single-output modeset rollback remains the inner safety
    /// net; this outer transaction rollback repairs outputs which succeeded
    /// before a later output failed.
    pub(super) fn rollback_output_configuration(
        &mut self,
        snapshot: &OutputConfigurationSnapshot,
        touched_outputs: &[String],
    ) -> Result<usize, String> {
        let snapshot_names: Vec<_> = snapshot
            .entries
            .iter()
            .map(|entry| entry.name.clone())
            .collect();
        let plan = plan_output_configuration_rollback(&snapshot_names, touched_outputs)?;
        let planned = plan.len();
        let mut restored = 0;
        let mut failures = Vec::new();

        for snapshot_index in plan {
            let entry = snapshot.entries[snapshot_index].clone();
            let Some(output_idx) = self.output_index_by_name(&entry.name) else {
                failures.push(format!(
                    "'{}': output disappeared during rollback",
                    entry.name
                ));
                continue;
            };

            let before = self.output_configuration_state(output_idx);
            let mode_changed = rollback_mode_requires_restore(
                before.as_ref().ok().map(|current| current.mode),
                entry.state.mode,
                self.outputs[output_idx].drm_mode_uncertain,
            );
            let configuration_changed = before.as_ref().map_or(true, |current| {
                mode_changed
                    || current.position != entry.state.position
                    || current.scale != entry.state.scale
                    || current.wl_transform != entry.state.wl_transform
            });
            let mut operation_errors = Vec::new();

            // A real modeset is most reliable on a powered connector. An
            // originally-off output is blanked again after its mode and
            // advertised state have been restored.
            if mode_changed && self.outputs[output_idx].dpms_off {
                if let Err(error) = self.set_dpms_for_output(output_idx, true) {
                    operation_errors.push(format!("temporary DPMS-on failed: {error}"));
                }
            }

            if configuration_changed {
                let restore_mode = mode_changed.then_some(entry.state.mode);
                if let Err(error) = self.configure_output_with_modeset_policy(
                    &entry.name,
                    restore_mode,
                    Some(entry.state.position),
                    Some(entry.state.wl_transform),
                    Some(entry.state.scale),
                    true,
                ) {
                    operation_errors.push(format!("configuration restore failed: {error}"));
                }
            }

            let dpms_on = !self.outputs[output_idx].dpms_off;
            if dpms_on != entry.state.dpms_on
                && let Err(error) = self.set_dpms_for_output(output_idx, entry.state.dpms_on)
            {
                operation_errors.push(format!("DPMS restore failed: {error}"));
            }

            let hardware_mode_certain = !self.outputs[output_idx].drm_mode_uncertain;
            match self.output_configuration_state(output_idx) {
                Ok(current) if current == entry.state && hardware_mode_certain => {
                    restored += 1;
                    if !operation_errors.is_empty() {
                        log::debug!(
                            "[output-mgmt] '{}' rollback reached the snapshot after transient errors: {}",
                            entry.name,
                            operation_errors.join(", ")
                        );
                    }
                }
                Ok(current) => {
                    let mut mismatches = Vec::new();
                    if current.mode != entry.state.mode || !hardware_mode_certain {
                        mismatches.push("mode/refresh");
                    }
                    if current.position != entry.state.position {
                        mismatches.push("position");
                    }
                    if current.scale != entry.state.scale {
                        mismatches.push("scale");
                    }
                    if current.wl_transform != entry.state.wl_transform {
                        mismatches.push("transform");
                    }
                    if current.dpms_on != entry.state.dpms_on {
                        mismatches.push("DPMS");
                    }
                    let operations = if operation_errors.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", operation_errors.join(", "))
                    };
                    failures.push(format!(
                        "'{}': final state differs in {}{}",
                        entry.name,
                        mismatches.join("/"),
                        operations
                    ));
                }
                Err(error) => {
                    let operations = if operation_errors.is_empty() {
                        String::new()
                    } else {
                        format!("; {}", operation_errors.join(", "))
                    };
                    failures.push(format!("'{}': {error}{operations}", entry.name));
                }
            }
        }

        self.needs_render = true;
        if failures.is_empty() {
            Ok(restored)
        } else {
            Err(format!(
                "restored {restored}/{planned} touched output(s): {}",
                failures.join("; ")
            ))
        }
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
    ///   false); when disabled, the transaction is rejected before mutation
    ///   rather than acknowledging a silently ignored mode request.
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
        let allow_modeset = crate::config::CONFIG
            .load()
            .behavior()
            .wlr_output_mgmt_allow_modeset;
        self.configure_output_with_modeset_policy(
            name,
            mode,
            position,
            transform,
            scale,
            allow_modeset,
        )
    }

    fn configure_output_with_modeset_policy(
        &mut self,
        name: &str,
        mode: Option<(i32, i32, i32)>,
        position: Option<(i32, i32)>,
        transform: Option<i32>,
        scale: Option<f64>,
        allow_modeset: bool,
    ) -> Result<(), String> {
        let idx = self
            .output_index_by_name(name)
            .ok_or_else(|| format!("unknown output '{name}'"))?;

        // Resolve a DRM mode if a *different* mode was requested.
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
                let force_drm_mode = self.outputs[idx].drm_mode_uncertain;
                let mgr = self.drm_output_manager.lock();
                let info = mgr
                    .device()
                    .get_connector(conn, false)
                    .map_err(|e| format!("get_connector failed: {e:?}"))?;
                // Prefer the exact refresh captured by a transaction snapshot;
                // retain the protocol's ±200 mHz tolerance as a fallback for
                // ordinary client requests.
                let found = info
                    .modes()
                    .iter()
                    .copied()
                    .find(|m| {
                        let wl = WlMode::from(*m);
                        wl.size.w == w && wl.size.h == h && wl.refresh == refresh
                    })
                    .or_else(|| {
                        info.modes().iter().copied().find(|m| {
                            let wl = WlMode::from(*m);
                            wl.size.w == w
                                && wl.size.h == h
                                && (refresh == 0 || (wl.refresh - refresh).abs() <= 200)
                        })
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
                    Some(m)
                        if force_drm_mode
                            || self.outputs[idx].output.current_mode() != Some(WlMode::from(m)) =>
                    {
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
                let mut primary_err = format!("DRM use_mode failed: {e:?}");
                if let Some(prev) = prev_drm_mode {
                    let rollback: DrmOutputRenderElements<GlesRenderer, SolidColorRenderElement> =
                        DrmOutputRenderElements::default();
                    match self.outputs[idx]
                        .drm_output
                        .use_mode(prev, &mut self.renderer, &rollback)
                    {
                        Ok(()) => {
                            self.outputs[idx].drm_mode_uncertain = false;
                            log::warn!(
                                "{}: '{name}': modeset failed, rolled back to previous mode ({primary_err})",
                                device_ctx("apply output mode")
                            );
                        }
                        Err(rollback_err) => {
                            self.outputs[idx].drm_mode_uncertain = true;
                            primary_err.push_str(&format!(
                                "; previous-mode rollback failed: {rollback_err:?}"
                            ));
                            log::error!(
                                "{}: '{name}': modeset failed AND rollback failed: \
                                 primary={primary_err}, rollback={rollback_err:?}",
                                device_ctx("apply output mode")
                            );
                        }
                    }
                } else {
                    self.outputs[idx].drm_mode_uncertain = true;
                    primary_err.push_str("; no previous mode available for rollback");
                    log::error!(
                        "{}: '{name}': modeset failed, no previous mode captured for rollback ({primary_err})",
                        device_ctx("apply output mode")
                    );
                }
                return Err(primary_err);
            }
            self.outputs[idx].drm_mode_uncertain = false;
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
                    log::error!(
                        "{}: {e:?}",
                        renderer_ctx("screenshot: create offscreen buffer")
                    );
                    return;
                }
            };

        // 2. Bind the offscreen renderbuffer
        let mut target = match renderer.bind(&mut renderbuffer) {
            Ok(t) => t,
            Err(e) => {
                log::error!("{}: {e:?}", renderer_ctx("screenshot: bind offscreen"));
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
            log::error!("{}: {e:?}", renderer_ctx("screenshot: render_output"));
            return;
        }

        // 4. Read pixels back via ExportMem
        let region = Rectangle::from_size(size);
        let mapping = match renderer.copy_framebuffer(&target, region, Fourcc::Abgr8888) {
            Ok(m) => m,
            Err(e) => {
                log::error!("{}: {e:?}", renderer_ctx("screenshot: copy_framebuffer"));
                return;
            }
        };

        let pixels = match renderer.map_texture(&mapping) {
            Ok(p) => p,
            Err(e) => {
                log::error!("{}: {e:?}", renderer_ctx("screenshot: map_texture"));
                return;
            }
        };

        // PNG compression and filesystem I/O are deliberately outside the
        // compositor frame. Readback still has to happen on the GL thread,
        // but encoding a 4K frame must not block input and presentation.
        let pixels = pixels.to_vec();
        spawn_screenshot_png_write(
            path.to_owned(),
            width as u32,
            height as u32,
            pixels,
            "screenshot",
        );
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
                    log::error!(
                        "{}: {e:?}",
                        renderer_ctx("screenshot-region: create offscreen buffer")
                    );
                    return;
                }
            };

        let mut target = match renderer.bind(&mut renderbuffer) {
            Ok(t) => t,
            Err(e) => {
                log::error!(
                    "{}: {e:?}",
                    renderer_ctx("screenshot-region: bind offscreen")
                );
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
            log::error!(
                "{}: {e:?}",
                renderer_ctx("screenshot-region: render_output")
            );
            return;
        }

        // Read full framebuffer
        let full_region = Rectangle::from_size(size);
        let mapping = match renderer.copy_framebuffer(&target, full_region, Fourcc::Abgr8888) {
            Ok(m) => m,
            Err(e) => {
                log::error!(
                    "{}: {e:?}",
                    renderer_ctx("screenshot-region: copy_framebuffer")
                );
                return;
            }
        };
        let full_pixels = match renderer.map_texture(&mapping) {
            Ok(p) => p,
            Err(e) => {
                log::error!("{}: {e:?}", renderer_ctx("screenshot-region: map_texture"));
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
            log::warn!(
                "{}: region is empty",
                renderer_ctx("screenshot-region: crop")
            );
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

        spawn_screenshot_png_write(path.to_owned(), cw, ch, cropped, "screenshot-region");
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
                    log::error!(
                        "{}: {e:?}",
                        renderer_ctx("screencopy: create offscreen buffer")
                    );
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
                log::error!(
                    "{}: {e:?}",
                    renderer_ctx("capture/dmabuf: bind client dmabuf")
                );
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
                log::error!("{}: {e:?}", renderer_ctx("capture/dmabuf: render_output"));
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
                        log::warn!(
                            "{}: region capture into dmabuf unsupported",
                            renderer_ctx("screencopy: dmabuf capture")
                        );
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
                log::error!("{}: {e:?}", renderer_ctx("screencopy: bind offscreen"));
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
            log::error!("{}: {e:?}", renderer_ctx("screencopy: render_output"));
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
                log::error!("{}: {e:?}", renderer_ctx("screencopy: copy_framebuffer"));
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
                log::error!("{}: {e:?}", renderer_ctx("screencopy: map_texture"));
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
                    log::warn!("{}: {e:?}", renderer_ctx("screencopy: buffer access"));
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
                log::error!("{}: {e:?}", renderer_ctx("image-capture: bind offscreen"));
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
            log::error!("{}: {e:?}", renderer_ctx("image-capture: render_output"));
            fail_all(&frames);
            return;
        }

        let region = Rectangle::from_size(size);
        let mapping = match renderer.copy_framebuffer(&target, region, Fourcc::Abgr8888) {
            Ok(m) => m,
            Err(e) => {
                log::error!("{}: {e:?}", renderer_ctx("image-capture: copy_framebuffer"));
                fail_all(&frames);
                return;
            }
        };
        let pixels = match renderer.map_texture(&mapping) {
            Ok(p) => p,
            Err(e) => {
                log::error!("{}: {e:?}", renderer_ctx("image-capture: map_texture"));
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
                    log::warn!("{}: {e:?}", renderer_ctx("image-capture: buffer access"));
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
                    log::error!(
                        "{}: {e:?}",
                        renderer_ctx("image-capture/toplevel: bind offscreen")
                    );
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
                log::error!(
                    "{}: {e:?}",
                    renderer_ctx("image-capture/toplevel: render_output")
                );
                Self::note_image_capture_render_failed(counters);
                frame_info.frame.failed(FailureReason::Unknown);
                continue;
            }

            let region = Rectangle::from_size(size);
            let mapping = match renderer.copy_framebuffer(&target, region, Fourcc::Abgr8888) {
                Ok(m) => m,
                Err(e) => {
                    log::error!(
                        "{}: {e:?}",
                        renderer_ctx("image-capture/toplevel: copy_framebuffer")
                    );
                    Self::note_image_capture_render_failed(counters);
                    frame_info.frame.failed(FailureReason::Unknown);
                    continue;
                }
            };
            let pixels = match renderer.map_texture(&mapping) {
                Ok(p) => p,
                Err(e) => {
                    log::error!(
                        "{}: {e:?}",
                        renderer_ctx("image-capture/toplevel: map_texture")
                    );
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
                    log::warn!(
                        "{}: {e:?}",
                        renderer_ctx("image-capture/toplevel: buffer access")
                    );
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
                    let watchdog = frame_watchdog_timeout(o.refresh_interval);
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

    pub(super) fn color_delivery_status(
        &self,
        soft_disabled_outputs: &HashSet<String>,
    ) -> crate::backend::api::ColorDeliveryStatus {
        let now = std::time::Instant::now();
        crate::backend::api::ColorDeliveryStatus {
            schema_version: 1,
            observation: "last_successful_presentation".into(),
            generation: self.color_delivery_generation,
            last_policy_decision: self.last_color_delivery_policy.clone(),
            outputs: self
                .outputs
                .iter()
                .map(|output| {
                    let last_success = output.color_delivery.last_success_status(now);
                    crate::backend::api::ColorDeliveryOutputStatus {
                        output_name: output.output_name.clone(),
                        participating: !output.dpms_off
                            && !soft_disabled_outputs.contains(&output.output_name),
                        last_success,
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
            let output_params =
                crate::backend::wayland_udev::color_management::params_for_output(&p.output);
            let (output_tf, output_ctm) = output_color_target(&output_params);
            outputs.push(KmsOutputState {
                crtc: p.crtc,
                connector: p.connector,
                mode_size: p.mode_size,
                origin: p.origin,
                drm_mode_uncertain: false,
                output: p.output,
                drm_output,
                frame_pending: false,
                frame_pending_boundary: None,
                color_delivery_observation_uncertain: false,
                color_delivery_retry_required: false,
                color_delivery: OutputColorDeliveryTracker::default(),
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
                legacy_gamma_override: false,
                dpms_off: false,
            });
        }

        if outputs.is_empty() {
            return Err(KmsInitError::NoConnector);
        }

        // Cursor shape (theme) and size follow the `[appearance]` config, with
        // the XCURSOR_THEME/XCURSOR_SIZE environment as a compatibility fallback.
        let (cursor_theme_name, cursor_size) = crate::config::CONFIG.load().resolved_cursor();
        let cursor_theme = CursorTheme::load(&cursor_theme_name);
        log::info!("[cursor] theme={cursor_theme_name:?} size={cursor_size}px");

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
            cursor_theme_name,
            cursor_size,
            cursor_images: HashMap::new(),
            cursor_cache: HashMap::new(),

            cursor_fallback_body_ids: (0..CURSOR_RECTS.len()).map(|_| Id::new()).collect(),
            cursor_fallback_shadow_ids: (0..CURSOR_RECTS.len()).map(|_| Id::new()).collect(),

            screenshot_requests: Default::default(),
            screencopy_pending: None,
            image_capture_pending: None,
            capture_counters: None,

            outputs,
            screencopy_offscreen: None,
            image_capture_toplevel_offscreen: None,
            last_presentation_time: None,
            last_direct_scanout_outputs: Vec::new(),
            // Construction has not established the neutral hardware baseline
            // yet. Even though the event loop cannot dispatch this handle until
            // `new` returns, keep the invariant explicit in the state itself.
            color_pipeline_delivery_blocked: true,
            prepared_color_delivery: None,
            color_delivery_policy_sequence: 0,
            color_delivery_generation: 0,
            last_color_delivery_policy: None,
            owns_scanout_color_state: false,
        }));

        let handle_clone = handle.clone();
        let token = event_loop_handle
            .insert_source(notifier, move |event, metadata, _state| match event {
                DrmEvent::VBlank(crtc) => {
                    handle_clone.borrow_mut().on_vblank(crtc, metadata);
                }
                DrmEvent::Error(err) => {
                    log::warn!("{}: {err:?}", renderer_ctx("process DRM event"));
                }
            })
            .map_err(|error| {
                KmsInitError::InitializeOutput(format!("failed to register DRM notifier: {error}"))
            })?;

        handle.borrow_mut().registration_token = Some(token);

        // Do not inherit a previous DRM master's color blobs under an empty
        // userspace tracker. This is deliberately the constructor's final
        // fallible hardware step and one all-output transaction:
        // `maybe_reinit_kms` keeps the old KMS state alive until construction
        // succeeds, so a partial reset would make its next framebuffer use the
        // wrong domain. Only the CRTCs/connectors selected and initialized
        // above are touched. DEGAMMA is reset because an inherited decoder
        // changes the input domain; HDR_OUTPUT_METADATA is reset because an
        // inherited output signal would make the sink reinterpret safe sRGB.
        let reset_result = {
            let mut state = handle.borrow_mut();
            let crtcs: Vec<_> = state.outputs.iter().map(|output| output.crtc).collect();
            let connectors: Vec<_> = state
                .outputs
                .iter()
                .map(|output| output.connector)
                .collect();
            let mgr = state.drm_output_manager.lock();
            Self::reset_scanout_color_properties(mgr.device(), &crtcs, &connectors)
        };
        let cleared = match reset_result {
            Ok(cleared) => cleared,
            Err(error) => {
                // Remove the callback's strong Rc before returning. Because
                // owns_scanout_color_state is still false, dropping this failed
                // construction cannot retry the reset and mutate color state
                // still tracked by the old live KMS instance.
                if let Some(token) = handle.borrow_mut().registration_token.take() {
                    let _ = event_loop_handle.remove(token);
                }
                return Err(KmsInitError::InitializeOutput(format!(
                    "failed to establish neutral scanout color state: {error}"
                )));
            }
        };
        {
            let mut state = handle.borrow_mut();
            state.owns_scanout_color_state = true;
            state.color_pipeline_delivery_blocked = false;
        }
        if cleared > 0 {
            log::info!(
                "[kms-cm] reset {cleared} inherited DEGAMMA_LUT/CTM/GAMMA_LUT/HDR_OUTPUT_METADATA/Colorspace properties"
            );
        }

        Ok(handle)
    }

    pub(super) fn render_if_needed(
        &mut self,
        state: &crate::backend::wayland::state::JwmWaylandState,
        cursor_kind: StdCursorKind,
        compositor: Option<&super::super::compositor::WaylandCompositor>,
    ) {
        if !self.needs_render || self.color_pipeline_delivery_blocked {
            return;
        }

        // A disabled effects compositor still has a concrete, diagnosable
        // encoded-sRGB delivery path even though no color-policy refresh ran.
        // Force replacement of a plan retained from the last composited frame.
        if compositor.is_none() {
            self.ensure_legacy_color_delivery_attempt();
        }

        self.last_direct_scanout_outputs.clear();
        let mut any_skipped = false;
        let mut any_failed = false;
        for out_idx in 0..self.outputs.len() {
            // Outputs marked soft-disabled by wlr-output-management
            // `disable_head` Apply stop receiving frames but keep their
            // DrmOutput alive so a later `enable_head` Apply can resume.
            // Use the cached name throughout this per-output/per-frame path:
            // Smithay's `Output::name()` returns a newly allocated `String`.
            if state
                .soft_disabled_outputs
                .contains(&self.outputs[out_idx].output_name)
                || self.outputs[out_idx].dpms_off
            {
                continue;
            }
            let frame_pending = self.outputs[out_idx].frame_pending;
            if frame_pending {
                any_skipped = true;
                continue;
            }
            let color_delivery_retry_required = self.outputs[out_idx].color_delivery_retry_required;
            let manual_surface_path = compositor.is_none();

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
            // `origin` comes from the physical-pixel OutputInfo layout and
            // `mode_size` is the DRM mode's physical size. These are also the
            // coordinates used to slice the compositor's global output FBO,
            // matching the Dock-facing CompositorRect contract. Do not apply
            // wl_output scale here: that scale belongs to client logical space.
            let output_rect_global_physical =
                CompositorRect::new(ox as f32, oy as f32, out_w as f32, out_h as f32);

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

            #[allow(
                clippy::mutable_key_type,
                reason = "Wayland Weak hashes by stable protocol-object identity; a set is required for frame visibility and output enter/leave differences"
            )]
            let mut visible_surfaces: HashSet<wayland_server::Weak<WlSurface>> = HashSet::new();
            let mut frame_roots: Vec<WlSurface> = Vec::new();

            if state.session_locked {
                if let Some(lock_surface) = state.lock_surfaces.get(&out.output_name) {
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
            // Respect both direct-scanout controls. `fullscreen_unredirect`
            // keeps parity with the X11 fullscreen bypass, while
            // `direct_scanout_enabled` is the explicit KMS fast-path gate.
            let (fullscreen_unredirect, direct_scanout_enabled) = {
                let cfg = crate::config::CONFIG.load();
                let behavior = cfg.behavior();
                (
                    behavior.fullscreen_unredirect,
                    behavior.direct_scanout_enabled,
                )
            };
            let system_ui_active = compositor.as_ref().is_some_and(|c| c.has_system_ui());
            let recording_requires_composition = compositor
                .as_ref()
                .is_some_and(|c| c.recording_requires_composition());
            let compositor_effect_reason = compositor
                .as_ref()
                .and_then(|c| c.direct_scanout_block_reason(output_rect_global_physical));
            let (direct_scanout_policy_eligible, direct_scanout_policy_reason) =
                if recording_requires_composition {
                    (
                        false,
                        "recording or recording-region overlay requires composition".to_string(),
                    )
                } else if system_ui_active {
                    (false, "JWM system UI requires composition".to_string())
                } else if !direct_scanout_enabled {
                    (false, "direct_scanout_enabled disabled".to_string())
                } else if !fullscreen_unredirect {
                    (false, "fullscreen_unredirect disabled".to_string())
                } else if let Some(reason) = compositor_effect_reason {
                    (false, reason.to_string())
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
            let direct_scanout_eligible = direct_scanout_allowed_for_color_retry(
                direct_scanout_policy_eligible,
                color_delivery_retry_required,
            );
            let direct_scanout_reason =
                if direct_scanout_policy_eligible && !direct_scanout_eligible {
                    "color-delivery observation retry requires one composited frame".to_string()
                } else {
                    direct_scanout_policy_reason
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
                let tex_format = comp.output_texture_internal_format();
                let tex_generation = comp.output_texture_generation();
                let output_tex = match &self.compositor_texture_cache {
                    Some((
                        cached_id,
                        cached_w,
                        cached_h,
                        cached_format,
                        cached_generation,
                        cached_tex,
                    )) if *cached_id == tex_id
                        && *cached_w == sw
                        && *cached_h == sh
                        && *cached_format == tex_format
                        && *cached_generation == tex_generation =>
                    {
                        cached_tex.clone()
                    }
                    _ => {
                        let size: Size<i32, BufferCoord> = (sw as i32, sh as i32).into();
                        let tex = unsafe {
                            GlesTexture::from_raw(
                                &self.renderer,
                                Some(tex_format),
                                false,
                                tex_id,
                                size,
                            )
                        };
                        // Retain every wrapper generation. The compositor owns
                        // and may explicitly recreate/delete the raw GL name;
                        // letting Smithay later drop an older wrapper could
                        // delete a recycled id belonging to a newer texture.
                        self.compositor_texture_keepalive
                            .push((tex_generation, tex.clone()));
                        self.compositor_texture_cache =
                            Some((tex_id, sw, sh, tex_format, tex_generation, tex.clone()));
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
                let overlay_candidate_window = if fullscreen_unredirect && direct_scanout_enabled {
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

                // Solid background LAST (back-most). When the effects
                // compositor is disabled there is no compositor-owned
                // wallpaper, so keep the desktop predictably pure black.
                let bg_geo = Rectangle::<i32, Physical>::from_size((out_w, out_h).into());
                let bg = SolidColorRenderElement::new(
                    self.background_id.clone(),
                    bg_geo,
                    0usize,
                    smithay::backend::renderer::Color32F::new(0.0, 0.0, 0.0, 1.0),
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
                for request in self.screenshot_requests.take_all() {
                    match request {
                        crate::backend::compositor_common::screenshot::ScreenshotRequest::Full(
                            path,
                        ) => Self::capture_screenshot_offscreen_impl(
                            &mut self.renderer,
                            out_w,
                            out_h,
                            &elements,
                            &path,
                        ),
                        crate::backend::compositor_common::screenshot::ScreenshotRequest::Region {
                            path,
                            x,
                            y,
                            width,
                            height,
                        } => Self::capture_screenshot_region_impl(
                            &mut self.renderer,
                            out_w,
                            out_h,
                            &elements,
                            &path,
                            x,
                            y,
                            width,
                            height,
                        ),
                    }
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
            let composited_color_delivery = self.color_delivery_plan_for_output(out_idx, false);
            let direct_color_delivery = self.color_delivery_plan_for_output(out_idx, true);
            let out = &mut self.outputs[out_idx];

            if out.color_delivery_retry_required {
                // `needs_render` alone is insufficient on a static desktop:
                // Smithay will return an empty frame when its damage history
                // still matches the current buffer. Resetting the swapchain
                // guarantees a real replacement queue/vblank observation.
                out.drm_output.reset_buffers();
            }
            let frame_flags = frame_flags_for_color_delivery(
                color_delivery_retry_required,
                manual_surface_path,
                direct_scanout_eligible,
            );

            match out.drm_output.render_frame(
                &mut self.renderer,
                &elements,
                smithay::backend::renderer::Color32F::new(0.0, 0.0, 0.0, 1.0),
                frame_flags,
            ) {
                Ok(res) => {
                    let client_direct_scanout = client_direct_scanout_presented(
                        direct_scanout_eligible,
                        matches!(
                            &res.primary_element,
                            PrimaryPlaneElement::Element(KmsRenderElement::Surface(_))
                        ),
                    );
                    if res.is_empty {
                        if out.color_delivery_retry_required {
                            // Keep the outer render wakeup armed and try again
                            // after resetting damage/buffer history.
                            out.drm_output.reset_buffers();
                            any_failed = true;
                        }
                        out.send_frame_callbacks = true;
                        out.frame_callback_roots = frame_roots;
                        out.frame_callback_visible = visible_surfaces;
                        Self::deliver_frame_callbacks(out, &flush_tx, &flush_pending, None);
                        continue;
                    }

                    let frame_data = QueuedFrameData {
                        color_delivery: if client_direct_scanout {
                            direct_color_delivery
                        } else {
                            composited_color_delivery
                        },
                    };
                    // Sample both DRM timestamp domains before submitting: a
                    // flip can happen immediately after the ioctl returns.
                    let queue_boundary = FrameQueueBoundary::now();
                    if let Err(err) = out.drm_output.queue_frame(frame_data) {
                        any_failed = true;
                        log::warn!("{}: {err:?}", renderer_ctx("queue DRM frame"));

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
                                    "{}: {act_err:?}",
                                    renderer_ctx(
                                        "reactivate DRM backend after queue_frame failure"
                                    )
                                );
                            }
                        }
                    } else {
                        out.frame_pending = true;
                        out.frame_pending_since = Some(std::time::Instant::now());
                        out.frame_pending_boundary = Some(queue_boundary);
                        out.send_frame_callbacks = true;
                        out.frame_callback_roots = frame_roots;
                        out.frame_callback_visible = visible_surfaces;
                    }
                }
                Err(err) => {
                    any_failed = true;
                    log::warn!("{}: {err:?}", renderer_ctx("render DRM frame"));

                    match self.drm_output_manager.lock().activate(false) {
                        Ok(_) => {
                            log::info!(
                                "drm backend activated after render_frame failure; will retry rendering"
                            );
                            self.needs_render = true;
                        }
                        Err(act_err) => {
                            log::warn!(
                                "{}: {act_err:?}",
                                renderer_ctx("reactivate DRM backend after render_frame failure")
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

        if !any_skipped && !any_failed {
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
        let Some(output_idx) = self.outputs.iter().position(|output| output.crtc == crtc) else {
            return;
        };

        // A watchdog or DPMS cancellation may leave its page-flip event in the
        // DRM fd. Never let that late event acknowledge user data belonging to
        // a newer queue. Timestamp comparison handles the common case; the
        // uncertainty flag makes the first post-cancellation observation
        // fail-closed when event timing cannot disambiguate it.
        {
            let out = &mut self.outputs[output_idx];
            if !out.frame_pending {
                out.color_delivery_observation_uncertain = false;
                return;
            }
            let matches_queue = out.frame_pending_boundary.is_some_and(|boundary| {
                vblank_is_not_older_than_queue(metadata.as_ref(), boundary)
            });
            if !matches_queue {
                out.color_delivery_observation_uncertain = false;
                log::debug!(
                    "[kms-cm] ignored a page-flip event older than the queued frame on {}",
                    out.output_name
                );
                return;
            }
        }

        // Extract precise flip timestamp from DRM metadata for presentation feedback
        let presentation_time = metadata.as_ref().and_then(|m| match m.time {
            smithay::backend::drm::DrmEventTime::Monotonic(t) => Some(t),
            smithay::backend::drm::DrmEventTime::Realtime(_) => None,
        });

        let received_at = std::time::Instant::now();
        let (submitted_color_delivery, retry_color_delivery_observation) = {
            let out = &mut self.outputs[output_idx];
            let uncertain = out.color_delivery_observation_uncertain;
            let submitted_frame = match out.drm_output.frame_submitted() {
                Ok(frame) => frame,
                Err(err) => {
                    log::debug!("drm frame_submitted error: {err:?}");
                    None
                }
            };
            let observation = submitted_color_delivery_observation(submitted_frame, uncertain);
            out.frame_pending = false;
            out.frame_pending_since = None;
            out.frame_pending_boundary = None;
            out.color_delivery_observation_uncertain = false;
            if let Some(vblank_time) = presentation_time {
                out.last_vblank = Some(vblank_time);
                out.last_vblank_received_at = Some(received_at);
            }
            observation
        };

        if presentation_time.is_some() {
            self.last_presentation_time = Some(received_at);
        }
        let promoted = self.outputs[output_idx].color_delivery.present(
            submitted_color_delivery,
            &mut self.color_delivery_generation,
            presentation_time,
            received_at,
        );
        if promoted {
            self.outputs[output_idx].color_delivery_retry_required = false;
        }
        if retry_color_delivery_observation
            || self.outputs[output_idx].color_delivery_retry_required
        {
            self.outputs[output_idx].drm_output.reset_buffers();
            self.needs_render = true;
        }

        let out = &mut self.outputs[output_idx];
        Self::deliver_frame_callbacks(out, &flush_tx, &flush_pending, presentation_time);
    }
}

impl Drop for KmsState {
    /// Best-effort cleanup of CRTC/connector color state and tracked blobs at teardown.
    /// Color properties can survive the userspace FD/master which installed
    /// them, so clear the hardware references before releasing our blob ids.
    /// The next KMS owner also performs a mandatory reset during initialization;
    /// Drop remains best-effort because orderly teardown may run after DRM
    /// master/session ownership has already been revoked.
    fn drop(&mut self) {
        let crtcs: Vec<_> = self.outputs.iter().map(|output| output.crtc).collect();
        let connectors: Vec<_> = self.outputs.iter().map(|output| output.connector).collect();
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
        let mgr = self.drm_output_manager.lock();
        let dev = mgr.device();
        if self.owns_scanout_color_state {
            match Self::reset_scanout_color_properties(dev, &crtcs, &connectors) {
                Ok(cleared) if cleared > 0 => {
                    log::debug!(
                        "[kms-cm] cleared {cleared} scanout color properties during teardown"
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    log::warn!(
                        "[kms-cm] failed to clear scanout color properties during teardown: {error}"
                    );
                }
            }
        }
        for id in blobs {
            if let Err(error) = dev.destroy_property_blob(id) {
                log::debug!("[kms-cm] destroy tracked color blob {id} failed: {error:?}");
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

fn smithay_transform_to_wl(transform: Transform) -> i32 {
    match transform {
        Transform::Normal => 0,
        Transform::_90 => 1,
        Transform::_180 => 2,
        Transform::_270 => 3,
        Transform::Flipped => 4,
        Transform::Flipped90 => 5,
        Transform::Flipped180 => 6,
        Transform::Flipped270 => 7,
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
    crate::backend::compositor_common::screenshot::save_png_atomically(
        path, pixels, width, height,
    )?;
    Ok(())
}

/// Persist a completed readback without holding up the compositor's frame
/// loop. The pixel buffer is owned before this is called, so the renderer and
/// its GL mapping can be released immediately.
fn spawn_screenshot_png_write(
    path: std::path::PathBuf,
    width: u32,
    height: u32,
    pixels: Vec<u8>,
    label: &'static str,
) {
    match std::thread::Builder::new()
        .name("jwm-screenshot-png".to_owned())
        .spawn(move || match save_rgba_png(&path, width, height, &pixels) {
            Ok(()) => log::info!("[{label}] saved to {}", path.display()),
            Err(error) => log::error!("[{label}: save PNG] {error}"),
        }) {
        Ok(_) => {}
        Err(error) => log::error!("[screenshot] could not start PNG writer: {error}"),
    }
}

#[cfg(test)]
mod compositor_texture_ownership_tests {
    use super::{
        ColorDeliveryPlan, ColorPipelineDecision, CrtcColorProperty, ExternalElementColorPlan,
        FrameQueueBoundary, LinearTailBlocker, OutputColorDeliveryTracker,
        OutputColorRegionCandidate, QueuedFrameData, client_direct_scanout_presented,
        compositor_output_texture_identity_matches, connector_color_property_neutral_value,
        crtc_color_property, ctm_offload_allowed, direct_scanout_allowed_for_color_retry,
        frame_flags_for_color_delivery, frame_watchdog_remaining, frame_watchdog_timeout,
        gamma_ramp_is_identity, legacy_color_delivery_attempt_needed, output_color_target,
        plan_output_configuration_rollback, plan_software_color_regions, point_in_output,
        prepared_color_delivery, rollback_mode_requires_restore, rounded_pointer_location,
        smithay_transform_to_wl, submitted_color_delivery_observation,
        vblank_is_not_older_than_queue, wl_transform_to_smithay,
    };
    use crate::backend::wayland_udev::color_management::ParametricParams;
    use crate::backend::wayland_udev::color_pipeline::{IDENTITY_CTM, TransferKind};
    use smithay::backend::drm::compositor::FrameFlags;
    use smithay::utils::Transform;

    fn color_region_candidate(
        origin: (i32, i32),
        output_tf: TransferKind,
    ) -> OutputColorRegionCandidate {
        OutputColorRegionCandidate {
            participating: true,
            origin,
            mode_size: (1920, 1080),
            scale: 1.0,
            transform: Transform::Normal,
            output_tf,
            working_to_output_row_major: IDENTITY_CTM,
        }
    }

    #[test]
    fn renderer_wrapper_identity_includes_generation_not_only_recycled_gl_name() {
        assert!(compositor_output_texture_identity_matches(17, 4, 17, 4));
        assert!(!compositor_output_texture_identity_matches(17, 4, 17, 3));
        assert!(!compositor_output_texture_identity_matches(17, 4, 18, 4));
    }

    #[test]
    fn render_hot_path_reuses_cached_output_names() {
        const SOURCE: &str = include_str!("udev_kms.rs");
        let render = SOURCE
            .split_once("pub(super) fn render_if_needed(")
            .expect("render_if_needed exists")
            .1
            .split_once("pub(super) fn on_vblank(")
            .expect("on_vblank follows render_if_needed")
            .0;

        assert!(
            !render.contains(".output.name()"),
            "the per-frame render path must not allocate output names"
        );
        assert!(render.contains("contains(&self.outputs[out_idx].output_name)"));
        assert!(render.contains("state.lock_surfaces.get(&out.output_name)"));
    }

    #[test]
    fn ctm_offload_requires_successful_hardware_oetf() {
        assert!(ctm_offload_allowed(true, true, true));
        assert!(!ctm_offload_allowed(false, true, true));
        assert!(!ctm_offload_allowed(true, false, true));
        assert!(!ctm_offload_allowed(true, true, false));
    }

    #[test]
    fn direct_scanout_requires_an_actual_primary_plane_assignment() {
        assert!(client_direct_scanout_presented(true, true));
        assert!(!client_direct_scanout_presented(true, false));
        assert!(!client_direct_scanout_presented(false, true));
        assert!(!client_direct_scanout_presented(false, false));
    }

    #[test]
    fn trustworthy_retry_temporarily_forces_a_composited_frame() {
        assert!(direct_scanout_allowed_for_color_retry(true, false));
        assert!(!direct_scanout_allowed_for_color_retry(true, true));
        assert!(!direct_scanout_allowed_for_color_retry(false, false));

        assert_eq!(
            frame_flags_for_color_delivery(true, false, true),
            FrameFlags::empty(),
            "retry must disable every KMS plane fast path"
        );
        assert_eq!(
            frame_flags_for_color_delivery(false, true, false),
            FrameFlags::empty(),
            "manual surfaces must not bypass a rejected direct-scanout policy"
        );
        assert_eq!(
            frame_flags_for_color_delivery(false, true, true),
            FrameFlags::DEFAULT
        );
    }

    #[test]
    fn late_vblank_timestamp_cannot_acknowledge_a_newer_queue() {
        let boundary = FrameQueueBoundary {
            monotonic: std::time::Duration::from_secs(20),
            realtime: std::time::UNIX_EPOCH + std::time::Duration::from_secs(40),
        };
        let monotonic = |seconds| smithay::backend::drm::DrmEventMetadata {
            time: smithay::backend::drm::DrmEventTime::Monotonic(std::time::Duration::from_secs(
                seconds,
            )),
            sequence: seconds as u32,
        };
        assert!(!vblank_is_not_older_than_queue(
            Some(&monotonic(19)),
            boundary
        ));
        assert!(vblank_is_not_older_than_queue(
            Some(&monotonic(20)),
            boundary
        ));
        assert!(!vblank_is_not_older_than_queue(None, boundary));

        let old_realtime = smithay::backend::drm::DrmEventMetadata {
            time: smithay::backend::drm::DrmEventTime::Realtime(
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(39),
            ),
            sequence: 1,
        };
        assert!(!vblank_is_not_older_than_queue(
            Some(&old_realtime),
            boundary
        ));
    }

    #[test]
    fn frame_watchdog_has_a_floor_and_becomes_immediately_due() {
        let fast_refresh = std::time::Duration::from_millis(8);
        let slow_refresh = std::time::Duration::from_millis(25);
        assert_eq!(
            frame_watchdog_timeout(fast_refresh),
            std::time::Duration::from_millis(100)
        );
        assert_eq!(
            frame_watchdog_timeout(slow_refresh),
            std::time::Duration::from_millis(125)
        );
        assert_eq!(
            frame_watchdog_remaining(slow_refresh, std::time::Duration::from_millis(80)),
            std::time::Duration::from_millis(45)
        );
        assert_eq!(
            frame_watchdog_remaining(slow_refresh, std::time::Duration::from_millis(130)),
            std::time::Duration::ZERO
        );
    }

    #[test]
    fn color_delivery_attempt_route_matches_the_frame_domain() {
        let mut decision = ColorPipelineDecision {
            hw_encode_active: false,
            hw_ctm_active: false,
            delivery_blocked: false,
            software_regions: Some(Vec::new()),
        };

        assert_eq!(
            prepared_color_delivery(&decision, true, false).route,
            "legacy_encoded_srgb"
        );
        assert_eq!(
            prepared_color_delivery(&decision, false, true).route,
            "global_srgb_fallback"
        );
        assert_eq!(
            prepared_color_delivery(&decision, true, true).route,
            "software_per_output_regions"
        );

        decision.software_regions = None;
        assert_eq!(
            prepared_color_delivery(&decision, true, true).fallback_reason,
            Some("unsupported_output_topology")
        );

        decision.hw_encode_active = true;
        decision.hw_ctm_active = true;
        assert_eq!(
            prepared_color_delivery(&decision, true, true).route,
            "kms_ctm_gamma_lut"
        );

        decision.delivery_blocked = true;
        let blocked = prepared_color_delivery(&decision, true, true);
        assert_eq!(blocked.route, "hold_last_success");
        assert_eq!(blocked.fallback_reason, Some("kms_color_state_unresolved"));
    }

    #[test]
    fn external_element_plan_reports_every_visible_frame_tail_class() {
        let safe = ExternalElementColorPlan::default();
        assert!(safe.is_safe());
        assert!(safe.blockers().is_empty());

        let mut blocked = ExternalElementColorPlan::from_frame_flags(true, true, true);
        blocked.observe_output(Some((5, 5)), (0, 0), (10, 10), true, true);
        assert!(!blocked.is_safe());
        assert_eq!(
            blocked
                .blockers()
                .into_iter()
                .map(LinearTailBlocker::wire_name)
                .collect::<Vec<_>>(),
            [
                "capture_readback",
                "session_lock_surface",
                "drag_icon",
                "cursor",
                "top_or_overlay_layer_surface",
            ]
        );
    }

    #[test]
    fn cursor_output_hit_testing_uses_half_open_signed_geometry() {
        let origin = (-1920, -1080);
        let size = (1920, 1080);
        assert!(point_in_output((-1920, -1080), origin, size));
        assert!(point_in_output((-1, -1), origin, size));
        assert!(!point_in_output((0, -1), origin, size));
        assert!(!point_in_output((-1, 0), origin, size));
        assert!(!point_in_output((-1921, -1080), origin, size));
        assert!(!point_in_output((0, 0), origin, (0, 1080)));
    }

    #[test]
    fn cursor_output_hit_testing_cannot_overflow_i32_edges() {
        assert!(point_in_output(
            (i32::MAX, i32::MAX),
            (i32::MAX, i32::MAX),
            (1, 1),
        ));
        assert!(point_in_output(
            (i32::MIN, i32::MIN),
            (i32::MIN, i32::MIN),
            (i32::MAX, i32::MAX),
        ));
        assert!(!point_in_output(
            (i32::MAX, i32::MAX),
            (i32::MIN, i32::MIN),
            (i32::MAX, i32::MAX),
        ));
    }

    #[test]
    fn pointer_rounding_rejects_nonfinite_or_unrepresentable_coordinates() {
        assert_eq!(rounded_pointer_location(4.49, -2.5), Some((4, -3)));
        assert_eq!(rounded_pointer_location(f64::NAN, 0.0), None);
        assert_eq!(rounded_pointer_location(0.0, f64::INFINITY), None);
        assert_eq!(
            rounded_pointer_location(f64::from(i32::MAX) + 1.0, 0.0),
            None
        );
    }

    #[test]
    fn inactive_outputs_contribute_no_cursor_or_layer_blocker() {
        let mut plan = ExternalElementColorPlan::default();
        plan.observe_output(Some((5, 5)), (0, 0), (10, 10), false, true);
        assert!(plan.is_safe());
    }

    #[test]
    fn output_inventory_accumulates_instead_of_last_output_winning() {
        let mut plan = ExternalElementColorPlan::default();
        plan.observe_output(Some((5, 5)), (0, 0), (10, 10), true, true);
        plan.observe_output(Some((5, 5)), (100, 100), (10, 10), true, false);
        assert_eq!(
            plan.blockers(),
            [
                LinearTailBlocker::Cursor,
                LinearTailBlocker::TopOrOverlayLayerSurface,
            ]
        );
    }

    #[test]
    fn invalid_pointer_state_fails_closed_on_a_participating_output() {
        let mut plan = ExternalElementColorPlan::default();
        plan.observe_output(None, (0, 0), (1920, 1080), true, false);
        assert_eq!(plan.blockers(), [LinearTailBlocker::Cursor]);
    }

    #[test]
    fn color_delivery_policy_serializes_observed_tail_inventory() {
        let status = crate::backend::api::ColorDeliveryPolicyDecisionStatus {
            sequence: 3,
            composited_route: "global_srgb_fallback".into(),
            blocked: false,
            reason: Some("linear_tail_unsafe".into()),
            scene_linear_active: true,
            linear_tail_safe: false,
            linear_tail_blockers: Some(vec!["cursor".into(), "drag_icon".into()]),
        };
        let encoded = serde_json::to_value(&status).unwrap();
        assert!(status.linear_tail_inventory_consistent());
        assert_eq!(
            status.observed_linear_tail_blockers().unwrap(),
            ["cursor", "drag_icon"]
        );
        assert_eq!(encoded["linear_tail_blockers"][0], "cursor");
        assert_eq!(encoded["linear_tail_blockers"][1], "drag_icon");

        let mut observed_clear = status.clone();
        observed_clear.linear_tail_safe = true;
        observed_clear.linear_tail_blockers = Some(Vec::new());
        let clear = serde_json::to_value(observed_clear).unwrap();
        assert_eq!(clear["linear_tail_blockers"], serde_json::json!([]));

        let mut legacy = encoded;
        legacy
            .as_object_mut()
            .unwrap()
            .remove("linear_tail_blockers");
        let decoded: crate::backend::api::ColorDeliveryPolicyDecisionStatus =
            serde_json::from_value(legacy).unwrap();
        assert_eq!(decoded.linear_tail_blockers, None);
        assert!(decoded.linear_tail_inventory_consistent());

        let mut inconsistent = status;
        inconsistent.linear_tail_safe = true;
        assert!(!inconsistent.linear_tail_inventory_consistent());
        inconsistent.linear_tail_safe = false;
        inconsistent.linear_tail_blockers = Some(vec!["cursor".into(), "cursor".into()]);
        assert!(!inconsistent.linear_tail_inventory_consistent());
        inconsistent.linear_tail_blockers = Some(vec!["future_blocker_2".into()]);
        assert!(inconsistent.linear_tail_inventory_consistent());
    }

    #[test]
    fn compositor_disable_replaces_a_stale_deferred_delivery_plan() {
        let deferred = super::PreparedColorDelivery {
            route: "kms_ctm_gamma_lut",
            working_space: "normalized_linear_srgb",
            targets_output_profile: true,
            fallback_reason: None,
        };
        let legacy = super::PreparedColorDelivery {
            route: "legacy_encoded_srgb",
            working_space: "legacy_encoded_srgb",
            targets_output_profile: false,
            fallback_reason: Some("effects_compositor_inactive"),
        };

        assert!(legacy_color_delivery_attempt_needed(None));
        assert!(legacy_color_delivery_attempt_needed(Some(&deferred)));
        assert!(!legacy_color_delivery_attempt_needed(Some(&legacy)));
    }

    #[test]
    fn color_delivery_tracker_promotes_only_queued_vblank_plans() {
        let plan = |route: &str| ColorDeliveryPlan {
            policy_sequence: 7,
            route: route.into(),
            working_space: "normalized_linear_srgb".into(),
            target_transfer_function: "srgb".into(),
            target_primaries: "srgb".into(),
            hdr_metadata_active: false,
            colorspace_signal: "default_sdr".into(),
            fallback_reason: None,
        };
        let mut tracker = OutputColorDeliveryTracker::default();
        let mut generation = 0;
        let now = std::time::Instant::now();

        assert!(tracker.present(
            Some(plan("software_per_output_regions")),
            &mut generation,
            Some(std::time::Duration::from_millis(100)),
            now,
        ));
        assert_eq!(generation, 1);
        assert_eq!(
            tracker.last_success.as_ref().unwrap().presentation.route,
            "software_per_output_regions"
        );
        assert_eq!(
            tracker
                .last_success
                .as_ref()
                .unwrap()
                .presentation
                .policy_sequence,
            7
        );

        assert!(!tracker.present(
            None,
            &mut generation,
            Some(std::time::Duration::from_millis(116)),
            now,
        ));
        assert_eq!(generation, 1);
        assert_eq!(
            tracker.last_success.as_ref().unwrap().presentation.route,
            "software_per_output_regions",
            "a cancelled/failed attempt must not overwrite last success"
        );

        assert!(tracker.present(
            Some(plan("kms_ctm_gamma_lut")),
            &mut generation,
            Some(std::time::Duration::from_millis(132)),
            now,
        ));
        assert_eq!(generation, 2);
        assert_eq!(
            tracker.last_success.as_ref().unwrap().presentation.route,
            "kms_ctm_gamma_lut"
        );
    }

    #[test]
    fn participation_epoch_invalidates_pre_disable_delivery() {
        let plan = ColorDeliveryPlan {
            policy_sequence: 11,
            route: "kms_ctm_gamma_lut".into(),
            working_space: "normalized_linear_srgb".into(),
            target_transfer_function: "st2084_pq".into(),
            target_primaries: "bt2020".into(),
            hdr_metadata_active: true,
            colorspace_signal: "hdr_metadata_unspecified_colorspace".into(),
            fallback_reason: None,
        };
        let mut tracker = OutputColorDeliveryTracker::default();
        let mut generation = 0;
        assert!(tracker.present(
            Some(plan),
            &mut generation,
            Some(std::time::Duration::from_secs(1)),
            std::time::Instant::now(),
        ));

        // Both a direct DPMS off→on cycle and output-management's
        // disable_head→enable_head cycle pass through the same successful DPMS
        // transition and therefore this invalidation boundary.
        tracker.invalidate();
        assert!(tracker.last_success.is_none());
        assert_eq!(generation, 1, "the global generation stays monotonic");
    }

    #[test]
    fn uncertain_first_vblank_requests_retry_then_second_vblank_promotes() {
        let plan = ColorDeliveryPlan {
            policy_sequence: 14,
            route: "software_per_output_regions".into(),
            working_space: "normalized_linear_srgb".into(),
            target_transfer_function: "srgb".into(),
            target_primaries: "srgb".into(),
            hdr_metadata_active: false,
            colorspace_signal: "default_sdr".into(),
            fallback_reason: None,
        };
        let mut tracker = OutputColorDeliveryTracker::default();
        let mut generation = 0;
        let now = std::time::Instant::now();

        // Model watchdog cancellation followed by no late old event: the
        // first event belongs to the new frame, but uncertainty must still be
        // fail-closed and arm a replacement frame.
        let (first, retry) = submitted_color_delivery_observation(
            Some(QueuedFrameData {
                color_delivery: Some(plan.clone()),
            }),
            true,
        );
        assert!(retry);
        assert!(!tracker.present(first, &mut generation, None, now));

        let (second, retry) = submitted_color_delivery_observation(
            Some(QueuedFrameData {
                color_delivery: Some(plan),
            }),
            false,
        );
        assert!(!retry);
        assert!(tracker.present(second, &mut generation, None, now));
        assert_eq!(generation, 1);
        assert_eq!(
            tracker.last_success.unwrap().presentation.policy_sequence,
            14
        );
    }

    #[test]
    fn gamma_override_identity_detection_matches_protocol_reset_ramp() {
        let size = 4_u32;
        let channel = [0_u16, 21_845, 43_690, 65_535];
        let ramp = channel.repeat(3);
        assert!(gamma_ramp_is_identity(size, &ramp));

        let mut tinted = ramp;
        tinted[5] = tinted[5].saturating_sub(1);
        assert!(!gamma_ramp_is_identity(size, &tinted));
        assert!(!gamma_ramp_is_identity(size, &tinted[..8]));
    }

    #[test]
    fn crtc_color_reset_contract_covers_only_blob_valued_color_stages() {
        assert_eq!(
            crtc_color_property("DEGAMMA_LUT"),
            Some(CrtcColorProperty::DegammaLut)
        );
        assert_eq!(crtc_color_property("CTM"), Some(CrtcColorProperty::Ctm));
        assert_eq!(
            crtc_color_property("GAMMA_LUT"),
            Some(CrtcColorProperty::GammaLut)
        );

        // Size/capability metadata and unrelated CRTC properties must never be
        // written to zero by the reset transaction.
        for name in [
            "DEGAMMA_LUT_SIZE",
            "GAMMA_LUT_SIZE",
            "ACTIVE",
            "MODE_ID",
            "VRR_ENABLED",
        ] {
            assert_eq!(
                crtc_color_property(name),
                None,
                "unexpected reset for {name}"
            );
        }
        assert_eq!(
            connector_color_property_neutral_value("HDR_OUTPUT_METADATA"),
            Some(0)
        );
        assert_eq!(
            connector_color_property_neutral_value("Colorspace"),
            Some(0)
        );
        assert_eq!(connector_color_property_neutral_value("EDID"), None);
    }

    #[test]
    fn wl_transform_conversion_round_trips_every_protocol_value() {
        for value in 0..=7 {
            assert_eq!(
                smithay_transform_to_wl(wl_transform_to_smithay(value)),
                value
            );
        }
        assert_eq!(wl_transform_to_smithay(-1), Transform::Normal);
        assert_eq!(wl_transform_to_smithay(8), Transform::Normal);
    }

    #[test]
    fn output_configuration_rollback_plan_is_reverse_and_unique() {
        let snapshot_names = vec!["DP-1".into(), "HDMI-A-1".into(), "eDP-1".into()];
        let touched = vec![
            "DP-1".into(),
            "HDMI-A-1".into(),
            "DP-1".into(),
            "eDP-1".into(),
        ];
        assert_eq!(
            plan_output_configuration_rollback(&snapshot_names, &touched).unwrap(),
            vec![2, 0, 1]
        );

        let missing = vec!["DP-1".into(), "UNKNOWN".into()];
        assert!(plan_output_configuration_rollback(&snapshot_names, &missing).is_err());

        let expected_mode = (1920, 1080, 60_000);
        assert!(!rollback_mode_requires_restore(
            Some(expected_mode),
            expected_mode,
            false
        ));
        assert!(rollback_mode_requires_restore(
            Some(expected_mode),
            expected_mode,
            true
        ));
    }

    #[test]
    fn software_color_regions_accept_supported_mixed_output_layout() {
        let candidates = [
            color_region_candidate((0, 0), TransferKind::Gamma22),
            color_region_candidate((1920, 0), TransferKind::St2084Pq),
        ];
        let regions = plan_software_color_regions(&candidates).expect("supported layout");
        assert_eq!(regions.len(), 2);
        assert_eq!(regions[0].rect, [0, 0, 1920, 1080]);
        assert_eq!(regions[1].rect, [1920, 0, 1920, 1080]);
        assert_eq!(regions[1].output_tf, TransferKind::St2084Pq);
    }

    #[test]
    fn software_color_regions_reject_ambiguous_or_unmappable_geometry() {
        let base = color_region_candidate((0, 0), TransferKind::Gamma22);

        let mut negative_origin = base;
        negative_origin.origin = (-1, 0);
        let mut empty_mode = base;
        empty_mode.mode_size = (0, 1080);
        let mut scaled = base;
        scaled.scale = 2.0;
        let mut rotated = base;
        rotated.transform = Transform::_90;
        for invalid in [negative_origin, empty_mode, scaled, rotated] {
            assert!(plan_software_color_regions(&[invalid]).is_none());
        }

        let same_profile_mirror = [base, base];
        assert!(plan_software_color_regions(&same_profile_mirror).is_some());

        let different_tf_mirror = [base, color_region_candidate((0, 0), TransferKind::Hlg)];
        assert!(plan_software_color_regions(&different_tf_mirror).is_none());

        let mut different_gamut = base;
        different_gamut.working_to_output_row_major[0] = 0.9;
        assert!(plan_software_color_regions(&[base, different_gamut]).is_none());

        let mut inactive_invalid = negative_origin;
        inactive_invalid.participating = false;
        assert_eq!(
            plan_software_color_regions(&[base, inactive_invalid])
                .expect("inactive output does not constrain the plan")
                .len(),
            1
        );
    }

    #[test]
    fn output_color_target_maps_linear_srgb_to_advertised_profile() {
        let hdr = ParametricParams {
            primaries_named: Some(6),
            tf_named: Some(11),
            ..Default::default()
        };
        let (hdr_tf, hdr_matrix) = output_color_target(&hdr);
        assert_eq!(hdr_tf, TransferKind::St2084Pq);
        assert_ne!(hdr_matrix, IDENTITY_CTM);

        let srgb = crate::backend::wayland_udev::color_management::srgb_params();
        let (srgb_tf, srgb_matrix) = output_color_target(&srgb);
        assert_eq!(srgb_tf, TransferKind::Srgb);
        assert!(
            srgb_matrix
                .iter()
                .zip(IDENTITY_CTM)
                .all(|(actual, expected)| (*actual - expected).abs() < 1e-5)
        );
    }
}
