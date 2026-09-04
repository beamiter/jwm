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
use smithay::backend::renderer::utils::{RendererSurfaceStateUserData, import_surface_tree};
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
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
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
    /// Raw property handles of the scanout color chain (CRTC
    /// DEGAMMA_LUT/CTM/GAMMA_LUT, connector Colorspace incl. its BT2020_RGB
    /// enum value and HDR_OUTPUT_METADATA), probed once at output init. The
    /// controlled atomic commit path programs these handles directly instead
    /// of re-scanning every object's property list per transition.
    color_property_handles: ScanoutColorPropertyHandles,
    /// Format of the DRM swapchain the composited framebuffer is allocated
    /// from. HDR chain validation requires the scanned-out framebuffer to be
    /// 10-bit or deeper.
    swapchain_fourcc: Fourcc,
    /// Formats accepted by the primary plane (raw fourccs), probed once at
    /// output init for the HDR scanout chain validation.
    primary_plane_formats: Vec<u32>,
    /// Per-output target transfer function, refreshed after EDID attachment.
    output_tf: crate::backend::wayland_udev::color_pipeline::TransferKind,
    /// Per-output sRGB→output-primaries 3x3 matrix, cached from the current
    /// output description. Installed by `apply_scanout_color_goals` together
    /// with the output OETF LUT in one atomic request when hardware owns
    /// delivery; otherwise the same matrix is consumed by that output's
    /// software region pass.
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
    /// Aggregated source peak (working-space units) of the surfaces visible
    /// on this output this frame; 1.0 when everything shown is SDR.
    source_peak_working: f32,
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
            tone_map: crate::backend::wayland_udev::color_pipeline::OutputToneMapPlan::for_output(
                candidate.source_peak_working,
                candidate.output_tf,
            ),
        };
        for previous in &regions {
            let same_profile = previous.output_tf == region.output_tf
                && previous.working_to_output_row_major == region.working_to_output_row_major
                && previous.tone_map == region.tone_map;
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

// ============================================================
// Controlled atomic color delivery (HDR P1-6)
// ============================================================
//
// Every CRTC color stage (DEGAMMA/CTM/GAMMA) and both connector signalling
// properties (Colorspace/HDR_OUTPUT_METADATA) are programmed through ONE
// `AtomicModeReq` per delivery-group transition, always probed with TEST_ONLY
// before the real commit. This replaces the earlier per-property commit
// sequence, whose intermediate states (e.g. LUT bound while the paired CTM
// was not yet installed) could reach scanout for frames in between.
//
// The target framebuffer itself is committed by Smithay's DrmCompositor,
// which exposes no hook to merge extra properties into its internal atomic
// commit. The FB pairing guarantee therefore comes from ordering and
// evidence: the color request is committed strictly before the frame's FB is
// queued, and `invalidate_color_delivery_after_hardware_change` refuses to
// report any hardware delivery route until a vblank confirms a frame queued
// after the color change. The swapchain format the FB carries is still part
// of the HDR chain validation below, so the bit depth of the scanned-out
// framebuffer is verified before HDR signalling may be programmed.

/// One raw property assignment inside a controlled atomic color request.
/// `object`/`property` are raw DRM ids so assembly and tests never need a
/// live device; the executor converts them back into typed handles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AtomicColorAssignment {
    object: u32,
    property: u32,
    value: u64,
}

/// Property handles of one output's scanout color chain, probed once at KMS
/// construction. The controlled atomic commit programs these handles directly
/// instead of re-scanning every object's property list per transition.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ScanoutColorPropertyHandles {
    degamma_lut: Option<u32>,
    ctm: Option<u32>,
    gamma_lut: Option<u32>,
    colorspace: Option<u32>,
    /// Raw enum value of the connector Colorspace "BT2020_RGB" entry, required
    /// to signal BT.2020 primaries alongside HDR metadata.
    colorspace_bt2020_rgb: Option<u64>,
    hdr_output_metadata: Option<u32>,
}

/// Complete desired color-chain state for one output in one atomic request.
/// `None` leaves the stage out of the request entirely; `Some(0)` clears it
/// (neutral stage / Default colorspace / SDR signalling); `Some(v)` installs
/// a blob id or enum value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ScanoutColorTarget {
    degamma_lut: Option<u64>,
    ctm: Option<u64>,
    gamma_lut: Option<u64>,
    colorspace: Option<u64>,
    hdr_output_metadata: Option<u64>,
}

/// Per-output input to the atomic request builder.
#[derive(Clone, Copy, Debug)]
struct AtomicColorOutputPlan {
    crtc: u32,
    connector: u32,
    handles: ScanoutColorPropertyHandles,
    target: ScanoutColorTarget,
}

fn push_color_assignment(
    assignments: &mut Vec<AtomicColorAssignment>,
    object: u32,
    handle: Option<u32>,
    value: Option<u64>,
    name: &'static str,
) -> Result<(), String> {
    let Some(value) = value else {
        // The stage is not part of this request at all.
        return Ok(());
    };
    match handle {
        Some(property) => {
            assignments.push(AtomicColorAssignment {
                object,
                property,
                value,
            });
            Ok(())
        }
        // Installing a stage the hardware never advertised is a hard error:
        // the alternative is presenting pixels converted for a stage that
        // does not exist. Clearing an absent property is a no-op because an
        // unexposed stage is already in its neutral state.
        None if value != 0 => Err(format!(
            "cannot install {name}: property not present on the DRM object"
        )),
        None => Ok(()),
    }
}

/// Assemble one controlled atomic request covering every planned output's
/// CRTC color stages and connector signalling. The kernel applies the whole
/// request or none of it; the stable per-output order (CRTC DEGAMMA_LUT → CTM
/// → GAMMA_LUT, then connector Colorspace → HDR_OUTPUT_METADATA) only exists
/// for deterministic logs and tests.
fn build_atomic_color_request(
    plans: &[AtomicColorOutputPlan],
) -> Result<Vec<AtomicColorAssignment>, String> {
    let mut assignments = Vec::new();
    for plan in plans {
        push_color_assignment(
            &mut assignments,
            plan.crtc,
            plan.handles.degamma_lut,
            plan.target.degamma_lut,
            "DEGAMMA_LUT",
        )?;
        push_color_assignment(
            &mut assignments,
            plan.crtc,
            plan.handles.ctm,
            plan.target.ctm,
            "CTM",
        )?;
        push_color_assignment(
            &mut assignments,
            plan.crtc,
            plan.handles.gamma_lut,
            plan.target.gamma_lut,
            "GAMMA_LUT",
        )?;
        push_color_assignment(
            &mut assignments,
            plan.connector,
            plan.handles.colorspace,
            plan.target.colorspace,
            "Colorspace",
        )?;
        push_color_assignment(
            &mut assignments,
            plan.connector,
            plan.handles.hdr_output_metadata,
            plan.target.hdr_output_metadata,
            "HDR_OUTPUT_METADATA",
        )?;
    }
    Ok(assignments)
}

/// Apply one assembled atomic color request: TEST_ONLY first, then the real
/// commit. Legacy-only devices are refused instead of falling back to
/// per-property ioctls — there is no all-or-nothing transaction there, and a
/// partial color transition would leave scanout in a mixed domain (the same
/// rule the init-time neutral reset already enforces).
fn commit_atomic_color_request(
    dev: &DrmDevice,
    assignments: &[AtomicColorAssignment],
) -> Result<(), String> {
    use smithay::reexports::drm::control::atomic::AtomicModeReq;
    use smithay::reexports::drm::control::{AtomicCommitFlags, RawResourceHandle, from_u32};

    if assignments.is_empty() {
        return Ok(());
    }
    if !dev.is_atomic() {
        return Err(format!(
            "refusing {} scanout color assignments without atomic modesetting",
            assignments.len()
        ));
    }
    let mut request = AtomicModeReq::new();
    for assignment in assignments {
        let object = RawResourceHandle::new(assignment.object)
            .ok_or("invalid zero object id in atomic color request")?;
        let property =
            from_u32::<smithay::reexports::drm::control::property::Handle>(assignment.property)
                .ok_or("invalid zero property handle in atomic color request")?;
        request.add_raw_property(object, property, assignment.value);
    }
    dev.atomic_commit(AtomicCommitFlags::TEST_ONLY, request.clone())
        .map_err(|e| format!("test atomic color commit failed: {e:?}"))?;
    dev.atomic_commit(AtomicCommitFlags::empty(), request)
        .map_err(|e| format!("atomic color commit failed: {e:?}"))
}

/// Desired CRTC color-stage contents for one output. `None` clears the stage;
/// `Some` installs a fresh blob with the given payload. Both stages always
/// move together: a CTM is linear-light math and must never scan out without
/// the paired hardware OETF, which the single atomic commit now makes
/// structural instead of sequential.
#[derive(Clone, Copy, Debug, PartialEq)]
struct OutputScanoutColorGoal {
    gamma_lut: Option<crate::backend::wayland_udev::color_pipeline::TransferKind>,
    ctm: Option<[f32; 9]>,
}

impl OutputScanoutColorGoal {
    const CLEAR: Self = Self {
        gamma_lut: None,
        ctm: None,
    };
}

/// True when the tracked installed state already satisfies the goal, so the
/// output can be left out of the atomic request entirely. The CTM compares
/// by presence only: `refresh_output_color_targets` tears the blob down
/// before caching a new matrix, so a live CTM blob always matches the cached
/// `output_ctm`.
fn scanout_color_goal_matches(
    installed_gamma_lut: Option<(
        u64,
        crate::backend::wayland_udev::color_pipeline::TransferKind,
    )>,
    installed_ctm: Option<u64>,
    goal: &OutputScanoutColorGoal,
) -> bool {
    let lut_ok = match (installed_gamma_lut, goal.gamma_lut) {
        (Some((_, installed_tf)), Some(goal_tf)) => installed_tf == goal_tf,
        (None, None) => true,
        _ => false,
    };
    lut_ok && installed_ctm.is_some() == goal.ctm.is_some()
}

/// Per-channel bit depth of a scanout framebuffer format, when known.
/// Unknown formats return `None` and fail HDR validation closed.
fn scanout_format_channel_bits(fourcc: Fourcc) -> Option<u32> {
    match fourcc {
        Fourcc::Argb8888 | Fourcc::Xrgb8888 | Fourcc::Abgr8888 | Fourcc::Xbgr8888 => Some(8),
        Fourcc::Argb2101010 | Fourcc::Xrgb2101010 | Fourcc::Abgr2101010 | Fourcc::Xbgr2101010 => {
            Some(10)
        }
        Fourcc::Argb16161616f
        | Fourcc::Xrgb16161616f
        | Fourcc::Abgr16161616f
        | Fourcc::Xbgr16161616f => Some(16),
        _ => None,
    }
}

/// Why HDR scanout signalling must stay fail-closed. Each gap is a distinct,
/// diagnosable reason; the first gap in a stable precedence order is reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HdrScanoutChainGap {
    /// The color stages live on a different DRM device than the scanout
    /// framebuffer; programming them cannot change the scanned-out domain.
    CrossDevice,
    /// The framebuffer queued for scanout is not a 10-bit (or higher) format.
    FramebufferBitDepth,
    /// The primary plane cannot scan out the framebuffer's format.
    PlaneFormatUnsupported,
    /// The CRTC lacks the GAMMA_LUT/CTM stages the hardware output transform
    /// (gamut conversion + OETF) needs.
    CrtcColorStagesMissing,
    /// The connector cannot signal BT.2020 primaries through Colorspace.
    ConnectorColorspaceMissing,
    /// The connector lacks the HDR_OUTPUT_METADATA property.
    ConnectorHdrMetadataMissing,
}

/// Validate the complete 10-bit format/plane/connector chain for HDR scanout.
/// `same_device` asserts the color stages and the scanout framebuffer live on
/// one DRM device (structurally true for the single-device `KmsState`; the
/// check is explicit so a future multi-GPU path cannot silently skip it).
/// Any gap keeps software SDR delivery and must not claim hardware HDR active.
fn hdr_scanout_chain_gap(
    same_device: bool,
    framebuffer_fourcc: Fourcc,
    primary_plane_formats: &[u32],
    handles: &ScanoutColorPropertyHandles,
) -> Option<HdrScanoutChainGap> {
    if !same_device {
        return Some(HdrScanoutChainGap::CrossDevice);
    }
    let deep_enough =
        scanout_format_channel_bits(framebuffer_fourcc).is_some_and(|bits| bits >= 10);
    if !deep_enough {
        return Some(HdrScanoutChainGap::FramebufferBitDepth);
    }
    if !primary_plane_formats.contains(&(framebuffer_fourcc as u32)) {
        return Some(HdrScanoutChainGap::PlaneFormatUnsupported);
    }
    if handles.gamma_lut.is_none() || handles.ctm.is_none() {
        return Some(HdrScanoutChainGap::CrtcColorStagesMissing);
    }
    if handles.colorspace.is_none() || handles.colorspace_bt2020_rgb.is_none() {
        return Some(HdrScanoutChainGap::ConnectorColorspaceMissing);
    }
    if handles.hdr_output_metadata.is_none() {
        return Some(HdrScanoutChainGap::ConnectorHdrMetadataMissing);
    }
    None
}

/// Create a GAMMA_LUT blob for `tf` with `size` entries. The baked curve is
/// the delivery scanout ramp: the OETF re-anchored so framebuffer-linear 1.0
/// is the 203 cd/m² reference white (`build_gamma_lut_scanout`). drm 0.14's
/// `create_property_blob<T: Sized>` uses `size_of::<T>()` and can't accept a
/// variable-length slice; Smithay solves this in PlaneDamageClips by calling
/// `drm_ffi::mode::create_property_blob` directly on a `&mut [u8]` view of the
/// array, and the same approach is used here.
fn create_gamma_lut_blob(
    dev: &DrmDevice,
    tf: crate::backend::wayland_udev::color_pipeline::TransferKind,
    size: usize,
) -> Result<u64, String> {
    if size < 2 {
        return Err(format!("GAMMA_LUT_SIZE={size} is below the minimum of 2"));
    }
    use std::os::unix::io::AsFd;
    let mut lut = crate::backend::wayland_udev::color_pipeline::build_gamma_lut_scanout(tf, size);
    let bytes = unsafe {
        std::slice::from_raw_parts_mut(
            lut.as_mut_ptr() as *mut u8,
            std::mem::size_of::<crate::backend::wayland_udev::color_pipeline::DrmColorLut>()
                * lut.len(),
        )
    };
    let blob = drm_ffi::mode::create_property_blob(dev.as_fd(), bytes)
        .map_err(|e| format!("create_property_blob(GAMMA_LUT) failed: {e:?}"))?;
    Ok(u64::from(blob.blob_id))
}

/// Create a CTM blob from a row-major 3×3 sRGB→output-primaries matrix.
fn create_ctm_blob(dev: &DrmDevice, matrix: [f32; 9]) -> Result<u64, String> {
    use std::os::unix::io::AsFd;
    let mut ctm = crate::backend::wayland_udev::color_pipeline::build_ctm(matrix);
    let bytes = unsafe {
        std::slice::from_raw_parts_mut(
            &mut ctm as *mut _ as *mut u8,
            std::mem::size_of::<crate::backend::wayland_udev::color_pipeline::DrmColorCtm>(),
        )
    };
    let blob = drm_ffi::mode::create_property_blob(dev.as_fd(), bytes)
        .map_err(|e| format!("create_property_blob(CTM) failed: {e:?}"))?;
    Ok(u64::from(blob.blob_id))
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
///
/// The recognized wire names live in `api::LINEAR_TAIL_BLOCKER_NAMES`; every
/// variant's `wire_name` must appear in that table (a unit test enforces it).
/// The compositor-owned frame-tail overlays are not listed here: they are
/// typed by `TailOverlayClass` (compositor `tail_domain` module) and merged
/// into the reported inventory by `record_color_delivery_attempt`.
///
/// `CaptureReadback` is retained for name-table compatibility with recorded
/// payloads but is no longer emitted: capture derives from the compositor's
/// explicitly encoded capture view and never constrains the route.
/// `CompositorEncodedTail` is now emitted only when the common-linear target
/// itself is unavailable; visible encoded-only overlays are reported under
/// their own per-class names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LinearTailBlocker {
    CompositorEncodedTail,
    CaptureReadback,
    SessionLockSurface,
    DragIcon,
    Cursor,
    TopLayerSurface,
    OverlayLayerSurface,
}

impl LinearTailBlocker {
    /// Stable report order. `CompositorEncodedTail` is owned by the
    /// compositor and prepended by the caller; the plan never sets it.
    const ALL: [Self; 7] = [
        Self::CompositorEncodedTail,
        Self::CaptureReadback,
        Self::SessionLockSurface,
        Self::DragIcon,
        Self::Cursor,
        Self::TopLayerSurface,
        Self::OverlayLayerSurface,
    ];

    const fn wire_name(self) -> &'static str {
        match self {
            Self::CompositorEncodedTail => "compositor_encoded_tail",
            Self::CaptureReadback => "capture_readback",
            Self::SessionLockSurface => "session_lock_surface",
            Self::DragIcon => "drag_icon",
            Self::Cursor => "cursor",
            Self::TopLayerSurface => "top_layer_surface",
            Self::OverlayLayerSurface => "overlay_layer_surface",
        }
    }
}

/// The element classes KMS/Smithay assemble outside the compositor's common
/// linear-sRGB texture. Each class owns exactly one `LinearTailBlocker`, so
/// the assembly gate, the route blocker list, and the IPC diagnostics all
/// read a single enumeration instead of maintaining parallel vocabularies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ExternalElementClass {
    /// Pointer cursor, whether drawn from the loaded theme bitmap or from the
    /// procedural software fallback; both sources are equally external.
    Cursor,
    /// Drag-and-drop icon surface tree following the pointer.
    DragIcon,
    /// Session-lock surface tree bound to one output.
    SessionLock,
    /// wlr-layer-shell Top layer surface trees overlapping an output.
    LayerTop,
    /// wlr-layer-shell Overlay layer trees overlapping an output.
    LayerOverlay,
}

impl ExternalElementClass {
    const ALL: [Self; 5] = [
        Self::Cursor,
        Self::DragIcon,
        Self::SessionLock,
        Self::LayerTop,
        Self::LayerOverlay,
    ];

    const fn blocker(self) -> LinearTailBlocker {
        match self {
            Self::Cursor => LinearTailBlocker::Cursor,
            Self::DragIcon => LinearTailBlocker::DragIcon,
            Self::SessionLock => LinearTailBlocker::SessionLockSurface,
            Self::LayerTop => LinearTailBlocker::TopLayerSurface,
            Self::LayerOverlay => LinearTailBlocker::OverlayLayerSurface,
        }
    }

    const fn wire_name(self) -> &'static str {
        self.blocker().wire_name()
    }

    pub(super) const fn index(self) -> usize {
        self as usize
    }

    /// Classes the common-linear adapter can stage into the compositor target.
    /// SessionLock stays external on purpose: a locked session is rare, its
    /// exact-sRGB fallback is visually inconsequential, and the KMS-side
    /// shield + lock-surface assembly remains the single audited occlusion
    /// boundary (lock pixels must provably cover the client scene).
    const fn supports_internalization(self) -> bool {
        !matches!(self, Self::SessionLock)
    }
}

/// What one external element class does on one output for one frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExternalElementDisposition {
    /// No pixel of this class reaches the output: nothing is assembled and no
    /// blocker is contributed. This extends the DPMS-off/soft-disabled
    /// precedent to absent sources, uncommitted trees, and off-output
    /// geometry.
    Hidden,
    /// Visible and importable, but KMS assembles it outside the common
    /// linear-sRGB texture because the class was not staged into the
    /// compositor this frame (or cannot be staged at all). It stays a
    /// fail-closed blocker while externally assembled.
    ExternalAssembly,
    /// Visible and staged into the compositor's common linear-sRGB target this
    /// frame: the compositor draws the class through the shared sRGB ingress
    /// before the per-output matrix + OETF, so KMS must not assemble it again
    /// and the class no longer forces the exact-sRGB fallback. The verdict is
    /// only ever applied from a successful staging pass
    /// (`apply_internalized`); it is never a classification outcome.
    Internalized,
    /// Visible, but some content failed the import precheck, so the
    /// common-linear adapter must never internalize this class. It assembles
    /// externally and blocks exactly like `ExternalAssembly`; the split exists
    /// so diagnostics and the adapter precheck share one vocabulary.
    ImportBlocked,
}

impl ExternalElementDisposition {
    /// Whether the KMS element assembly draws the class this frame.
    const fn assembles_externally(self) -> bool {
        matches!(self, Self::ExternalAssembly | Self::ImportBlocked)
    }

    /// Whether the class reaches scanout on this output through either
    /// assembly path — used for surface enter/frame-callback bookkeeping,
    /// which internalized trees still need.
    const fn produces_pixels(self) -> bool {
        !matches!(self, Self::Hidden)
    }

    /// Whether the class forces the exact-sRGB fallback. Internalized classes
    /// go through the common-linear pipeline and therefore do not block it.
    const fn contributes_blocker(self) -> bool {
        matches!(self, Self::ExternalAssembly | Self::ImportBlocked)
    }
}

/// Pure per-class, per-output decision. Invisible elements (absent source or
/// uncommitted tree) never block; every visible element blocks while assembly
/// is external; an import-precheck failure is recorded as `ImportBlocked` so
/// the adapter can never internalize the class silently. `Internalized` is
/// assigned only by `ExternalElementColorPlan::apply_internalized` after a
/// successful staging pass, never here.
fn classify_external_element(
    present: bool,
    drawable: bool,
    importable: bool,
) -> ExternalElementDisposition {
    if !present || !drawable {
        ExternalElementDisposition::Hidden
    } else if importable {
        ExternalElementDisposition::ExternalAssembly
    } else {
        ExternalElementDisposition::ImportBlocked
    }
}

/// Half-open rectangle/output intersection in the global layout space,
/// shared by the color plan and the KMS element assembly so both make the
/// same visibility call. The strict comparisons mirror smithay's
/// `Rectangle::overlaps`; i64 math keeps extreme coordinates from
/// overflowing or saturating differently.
fn rect_overlaps_output(
    rect_loc: (i32, i32),
    rect_size: (i32, i32),
    origin: (i32, i32),
    mode_size: (i32, i32),
) -> bool {
    let (rx, ry) = (i64::from(rect_loc.0), i64::from(rect_loc.1));
    let (rw, rh) = (i64::from(rect_size.0), i64::from(rect_size.1));
    let (ox, oy) = (i64::from(origin.0), i64::from(origin.1));
    let (ow, oh) = (i64::from(mode_size.0), i64::from(mode_size.1));
    rx < ox + ow && ox < rx + rw && ry < oy + oh && oy < ry + rh
}

/// Per-class observation on one output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ExternalElementClassPlan {
    disposition: ExternalElementDisposition,
    /// Stable snake_case token stating the visibility/importability basis.
    basis: &'static str,
}

impl ExternalElementClassPlan {
    const HIDDEN: Self = Self {
        disposition: ExternalElementDisposition::Hidden,
        basis: "not_observed",
    };
}

/// One output's slice of the frame's external element plan.
#[derive(Clone, Debug, PartialEq, Eq)]
struct OutputExternalElementPlan {
    output_name: String,
    /// Mirrors the frame loop's participation rule: not DPMS-off and not
    /// soft-disabled. Non-participating outputs assemble nothing and
    /// contribute no blocker.
    participating: bool,
    /// Resolved pointer position when the cursor class assembles on this
    /// output; `None` when the pointer is elsewhere or not representable.
    cursor_position: Option<(i32, i32)>,
    classes: [ExternalElementClassPlan; ExternalElementClass::ALL.len()],
}

impl OutputExternalElementPlan {
    fn new(output_name: String, participating: bool) -> Self {
        let classes = if participating {
            [ExternalElementClassPlan::HIDDEN; ExternalElementClass::ALL.len()]
        } else {
            [ExternalElementClassPlan {
                disposition: ExternalElementDisposition::Hidden,
                basis: "output_not_participating",
            }; ExternalElementClass::ALL.len()]
        };
        Self {
            output_name,
            participating,
            cursor_position: None,
            classes,
        }
    }

    fn observe(
        &mut self,
        class: ExternalElementClass,
        disposition: ExternalElementDisposition,
        basis: &'static str,
    ) {
        self.classes[class.index()] = ExternalElementClassPlan { disposition, basis };
    }

    fn class(&self, class: ExternalElementClass) -> ExternalElementClassPlan {
        self.classes[class.index()]
    }

    /// Whether the KMS element assembly draws this class on this output.
    fn assembles(&self, class: ExternalElementClass) -> bool {
        self.participating && self.class(class).disposition.assembles_externally()
    }

    /// Whether this class produces pixels on this output through either
    /// assembly path. Internalized trees still need output enter/leave and
    /// frame-callback bookkeeping even though KMS no longer draws them.
    fn shows(&self, class: ExternalElementClass) -> bool {
        self.participating && self.class(class).disposition.produces_pixels()
    }

    /// Whether this output contributes the class blocker to the frame.
    fn contributes(&self, class: ExternalElementClass) -> bool {
        self.participating && self.class(class).disposition.contributes_blocker()
    }
}

/// Per-frame plan for the elements KMS/Smithay assembles outside the
/// compositor's common linear-sRGB texture. One derivation feeds three
/// consumers: the KMS element assembly (`render_if_needed`), the color
/// delivery route (`blockers`), and the IPC diagnostics (`class_statuses`).
///
/// Capture/readback is intentionally not part of this plan: screenshots,
/// screencopy and recording read the compositor's explicitly encoded capture
/// view (render.rs section 18c), so their presence never constrains the
/// physical route. `KmsState::capture_readback_pending` still reports pending
/// capture work so the compositor knows when to derive that view.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct ExternalElementColorPlan {
    /// The frame's resolved pointer position, shared by cursor placement and
    /// drag-icon placement. `None` fails closed: affected classes report a
    /// blocker but cannot be placed, so nothing is drawn for them.
    cursor_position: Option<(i32, i32)>,
    /// Aligned with `KmsState.outputs` by index.
    outputs: Vec<OutputExternalElementPlan>,
}

impl ExternalElementColorPlan {
    fn output(&self, output_idx: usize) -> Option<&OutputExternalElementPlan> {
        self.outputs.get(output_idx)
    }

    /// Whether the class forces the exact-sRGB fallback on any output. A
    /// visible class no longer blocks once every visible slice of it has been
    /// internalized into the common-linear target.
    fn class_contributes_blocker(&self, class: ExternalElementClass) -> bool {
        self.outputs.iter().any(|output| output.contributes(class))
    }

    /// Whether any output shows the class through either assembly path.
    fn class_produces_pixels(&self, class: ExternalElementClass) -> bool {
        self.outputs.iter().any(|output| output.shows(class))
    }

    pub(super) fn is_safe(&self) -> bool {
        !ExternalElementClass::ALL
            .into_iter()
            .any(|class| self.class_contributes_blocker(class))
    }

    /// Whether internalizing every migratable visible class would leave the
    /// frame linear-tail safe: no visible non-migratable class (session lock)
    /// and no import-blocked tree. Staging costs real GL work, so the frame
    /// loop asks this pure question before paying it.
    pub(super) fn internalization_could_make_safe(&self) -> bool {
        ExternalElementClass::ALL.into_iter().all(|class| {
            if !self.class_produces_pixels(class) {
                return true;
            }
            if !class.supports_internalization() {
                return false;
            }
            self.outputs.iter().all(|output| {
                !output.participating
                    || output.class(class).disposition != ExternalElementDisposition::ImportBlocked
            })
        })
    }

    /// Flip every `ExternalAssembly` output of the flagged classes to
    /// `Internalized`. The verdict comes from the staging pass that actually
    /// imported the class content and handed it to the compositor; `Hidden`
    /// and `ImportBlocked` entries are never rewritten, so a class whose
    /// staging failed (bit left clear) keeps its blocker and pulls the frame
    /// back to the exact-sRGB path.
    fn apply_internalized(&mut self, classes: &[bool; ExternalElementClass::ALL.len()]) {
        for class in ExternalElementClass::ALL {
            if !classes[class.index()] {
                continue;
            }
            for output in &mut self.outputs {
                if output.class(class).disposition == ExternalElementDisposition::ExternalAssembly {
                    let basis = output.class(class).basis;
                    output.observe(class, ExternalElementDisposition::Internalized, basis);
                }
            }
        }
    }

    /// Frame-level blocker list in stable `LinearTailBlocker::ALL` order. Only
    /// the KMS-assembled element classes appear here; the compositor-owned
    /// frame tail is merged in by `record_color_delivery_attempt`, and capture
    /// no longer blocks (it reads the independent encoded capture view).
    pub(super) fn blockers(&self) -> Vec<LinearTailBlocker> {
        LinearTailBlocker::ALL
            .into_iter()
            .filter(|blocker| match blocker {
                // The compositor reports its own encoded tail separately.
                LinearTailBlocker::CompositorEncodedTail => false,
                // Legacy name, retained for recorded payloads: capture derives
                // from the independent encoded view and never blocks.
                LinearTailBlocker::CaptureReadback => false,
                class_blocker => ExternalElementClass::ALL.into_iter().any(|class| {
                    class.blocker() == *class_blocker && self.class_contributes_blocker(class)
                }),
            })
            .collect()
    }

    /// Per-class diagnostic view for the render-decision IPC.
    pub(super) fn class_statuses(&self) -> Vec<crate::backend::api::ExternalElementClassStatus> {
        ExternalElementClass::ALL
            .into_iter()
            .map(|class| {
                let shown_outputs: Vec<&OutputExternalElementPlan> = self
                    .outputs
                    .iter()
                    .filter(|output| output.shows(class))
                    .collect();
                let visible = !shown_outputs.is_empty();
                let importable = visible
                    && shown_outputs.iter().all(|output| {
                        output.class(class).disposition != ExternalElementDisposition::ImportBlocked
                    });
                // Staging is all-or-nothing per class per frame, so a visible
                // class is either fully internalized or fully external.
                let internalized = visible
                    && shown_outputs.iter().all(|output| {
                        output.class(class).disposition == ExternalElementDisposition::Internalized
                    });
                let blocked = self.class_contributes_blocker(class);
                let basis = shown_outputs
                    .first()
                    .map(|output| output.class(class).basis)
                    .or_else(|| {
                        self.outputs
                            .iter()
                            .find(|output| output.participating)
                            .map(|output| output.class(class).basis)
                    })
                    .unwrap_or("no_participating_output");
                crate::backend::api::ExternalElementClassStatus {
                    class: class.wire_name().to_owned(),
                    visible,
                    importable,
                    assembly: if !visible {
                        "none"
                    } else if internalized {
                        "common_linear"
                    } else {
                        "kms_external"
                    }
                    .to_owned(),
                    blocker: blocked.then(|| class.wire_name().to_owned()),
                    outputs: shown_outputs
                        .iter()
                        .map(|output| output.output_name.clone())
                        .collect(),
                    basis: basis.to_owned(),
                }
            })
            .collect()
    }
}

/// Whether a class is worth staging into the common-linear target this frame:
/// at least one participating output assembles it externally, and no output
/// reports it import-blocked (an import-blocked tree anywhere keeps the whole
/// class external, matching the precheck's hard-blocker contract).
fn class_is_staging_candidate(
    plan: &ExternalElementColorPlan,
    class: ExternalElementClass,
) -> bool {
    let mut any_assembly = false;
    for output in &plan.outputs {
        if !output.participating {
            continue;
        }
        match output.class(class).disposition {
            ExternalElementDisposition::ExternalAssembly => any_assembly = true,
            ExternalElementDisposition::ImportBlocked => return false,
            _ => {}
        }
    }
    any_assembly
}

/// Commit or discard a staging outcome. The internalized dispositions are
/// applied only when the resulting plan is linear-tail safe; otherwise the
/// untouched plan keeps every blocker, the frame takes the exact-sRGB
/// fallback, and KMS assembles every class as before (fail-closed: no element
/// is dropped and no frame mixes color domains).
pub(super) fn commit_staged_internalization(
    plan: &ExternalElementColorPlan,
    staged_classes: &[bool; ExternalElementClass::ALL.len()],
) -> (ExternalElementColorPlan, bool) {
    let mut trial = plan.clone();
    trial.apply_internalized(staged_classes);
    if trial.is_safe() {
        (trial, true)
    } else {
        (plan.clone(), false)
    }
}

/// Which external element classes the compositor's current output texture
/// already carries, written after every successful composited frame. The KMS
/// assembly consults it so an internalized class is never drawn twice; the
/// texture identity check pins the verdict to exactly the framebuffer it
/// describes, so a recreated/resized compositor silently reverts to external
/// assembly until its first staged frame lands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct InternalizedExternalFrame {
    texture_id: u32,
    generation: u64,
    classes: [bool; ExternalElementClass::ALL.len()],
}

impl InternalizedExternalFrame {
    pub(super) fn new(
        texture_id: u32,
        generation: u64,
        classes: [bool; ExternalElementClass::ALL.len()],
    ) -> Self {
        Self {
            texture_id,
            generation,
            classes,
        }
    }

    /// Apply this verdict to a freshly derived plan when — and only when —
    /// the compositor texture about to be wrapped is the one the verdict
    /// describes.
    fn apply_to_plan(&self, plan: &mut ExternalElementColorPlan, texture_id: u32, generation: u64) {
        if compositor_output_texture_identity_matches(
            texture_id,
            generation,
            self.texture_id,
            self.generation,
        ) {
            plan.apply_internalized(&self.classes);
        }
    }
}

/// Per-surface wp-color-management descriptions, snapshotted once per staging
/// pass so a mid-frame commit cannot make the frame disagree with itself.
type SurfaceColorDescriptions = std::collections::HashMap<
    wayland_server::backend::ObjectId,
    crate::backend::color_policy::ParametricParams,
>;

/// Result of `KmsState::stage_external_elements_for_linear`: the compositor
/// draw list (back-to-front) plus the per-class verdict of which classes were
/// fully staged. A class bit is set only when every visible instance of the
/// class was imported and composited successfully.
pub(super) struct StagedExternalElements {
    pub draws: Vec<super::super::compositor::ExternalElementVisual>,
    pub classes: [bool; ExternalElementClass::ALL.len()],
}

/// Committed-content summary of one external surface tree. `drawable` counts
/// surfaces whose committed buffer produces render content; `unimportable`
/// counts drawable surfaces whose buffer fails the import precheck.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SurfaceTreeContent {
    drawable: u32,
    unimportable: u32,
}

impl SurfaceTreeContent {
    fn has_drawable(&self) -> bool {
        self.drawable > 0
    }

    fn all_importable(&self) -> bool {
        self.unimportable == 0
    }
}

/// Whether a committed buffer can become a GL texture in this renderer. Shm,
/// single-pixel, and EGL-reader buffers import through the renderer's
/// standard paths; dmabuf buffers import when their format is in the EGL
/// texture-format set; unmanaged buffers fail closed. This is a plan-time
/// precheck — the assembly's actual GL import remains the final word.
fn buffer_import_supported(buffer: &WlBuffer, dmabuf_texture_formats: &FormatSet) -> bool {
    use smithay::backend::allocator::Buffer as _;
    use smithay::backend::renderer::{BufferType, buffer_type};
    match buffer_type(buffer) {
        // `buffer_type` only reports managed buffers, so any other variant is
        // backed by a smithay import path.
        Some(BufferType::Shm) | Some(BufferType::SinglePixel) => true,
        Some(BufferType::Dma) => {
            get_dmabuf(buffer).is_ok_and(|dmabuf| dmabuf_texture_formats.contains(&dmabuf.format()))
        }
        Some(_) => true,
        None => false,
    }
}

/// Walk one external surface tree the same way the element assembly does,
/// counting drawable and import-failing surfaces for the color plan.
fn summarize_external_surface_tree(
    surface: &WlSurface,
    dmabuf_texture_formats: &FormatSet,
) -> SurfaceTreeContent {
    let mut content = SurfaceTreeContent::default();
    with_surface_tree_downward(
        surface,
        (),
        |_, _, _| TraversalAction::DoChildren(()),
        |_child_surface, child_states, _| {
            let data = child_states.data_map.get::<RendererSurfaceStateUserData>();
            let Some(data) = data else {
                return;
            };
            let data = data.lock_safe();
            if data.view().is_none() {
                return;
            }
            content.drawable += 1;
            // A view without a buffer should be impossible; fail closed.
            let importable = data
                .buffer()
                .is_some_and(|buffer| buffer_import_supported(buffer, dmabuf_texture_formats));
            if !importable {
                content.unimportable += 1;
            }
        },
        |_, _, _| true,
    );
    content
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
    /// Same wrapper cache for the compositor's dedicated capture-view texture,
    /// used only while an offscreen capture render runs.
    capture_texture_cache: Option<(u32, u32, u32, u32, u64, GlesTexture)>,
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
    /// External element classes baked into the compositor texture the KMS
    /// assembly is about to wrap; written after every successful composited
    /// frame, `None` when that frame internalized nothing.
    internalized_external_frame: Option<InternalizedExternalFrame>,
    /// Set only after the constructor's final all-CRTC neutral-color commit.
    /// A failed/incomplete reinit must not run the Drop reset: the previous
    /// `KmsState` still owns and tracks those live properties.
    owns_scanout_color_state: bool,
}

#[derive(Clone)]
struct CursorBitmap {
    buffer: MemoryRenderBuffer,
    /// Buffer dimensions in physical pixels (the KMS element derives them
    /// from the buffer; the staging path needs them for the offscreen size).
    width: i32,
    height: i32,
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

/// Merge the compositor's typed frame-tail status with the KMS
/// external-element plan into the reported blocker inventory. When the
/// common-linear target is unavailable the aggregate `compositor_encoded_tail`
/// name stands in for the whole tail; otherwise every visible encoded-only
/// overlay class reports under its own name in `TailOverlayClass::ALL` draw
/// order. The KMS-assembled classes follow in `LinearTailBlocker::ALL` order.
fn linear_tail_blocker_names(
    compositor_tail: &super::super::compositor::tail_domain::LinearTailStatus,
    external_element_plan: &ExternalElementColorPlan,
) -> Vec<String> {
    let mut names: Vec<String> = if compositor_tail.linear_target_ready {
        compositor_tail
            .overlay_blockers
            .iter()
            .filter_map(|class| class.blocker_wire_name())
            .map(str::to_owned)
            .collect()
    } else {
        vec![
            LinearTailBlocker::CompositorEncodedTail
                .wire_name()
                .to_owned(),
        ]
    };
    names.extend(
        external_element_plan
            .blockers()
            .into_iter()
            .map(|blocker| blocker.wire_name().to_owned()),
    );
    names
}

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
                width: img.width as i32,
                height: img.height as i32,
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

    /// Record which external element classes the frame just rendered into the
    /// compositor texture. Called after every successful composited frame;
    /// `None` (or an all-clear set) when nothing was internalized, so the KMS
    /// assembly returns to drawing every visible class on the next frame.
    pub(super) fn set_internalized_external_frame(
        &mut self,
        frame: Option<InternalizedExternalFrame>,
    ) {
        self.internalized_external_frame = frame;
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

    /// Whether any capture consumer (IPC screenshot, wlr-screencopy,
    /// ext-image-copy-capture) waits on this frame. The frame loop passes this
    /// to the compositor so it derives the explicitly encoded capture view;
    /// per the frame-tail domain table, pending capture never enters the
    /// color-delivery route decision.
    pub(super) fn capture_readback_pending(&self) -> bool {
        self.screenshot_requests.has_pending()
            || self
                .screencopy_pending
                .as_ref()
                .is_some_and(|queue| !queue.lock_safe().is_empty())
            || self
                .image_capture_pending
                .as_ref()
                .is_some_and(|queue| !queue.lock_safe().is_empty())
    }

    /// Build the per-frame plan for the elements Smithay assembles outside
    /// the compositor's common linear-sRGB texture: cursor (theme bitmap or
    /// software fallback), drag icon, session-lock surface, and top/overlay
    /// layer surface trees, each with their subsurface trees. The same plan
    /// gates the KMS element assembly in `render_if_needed`, feeds the
    /// delivery-route blocker list, and fills the IPC diagnostics, so check
    /// and draw cannot drift apart. Classes the staging pass internalized
    /// carry the `Internalized` disposition (applied afterwards via
    /// `apply_internalized`): they leave the KMS assembly and stop blocking
    /// the linear route; every other visible class still requires the global
    /// encoded-sRGB fallback.
    pub(super) fn external_element_color_plan(
        &self,
        state: &crate::backend::wayland::state::JwmWaylandState,
    ) -> ExternalElementColorPlan {
        let dmabuf_texture_formats = self.renderer.egl_context().dmabuf_texture_formats();
        let cursor_position =
            rounded_pointer_location(state.pointer_location.x, state.pointer_location.y);
        let mut plan = ExternalElementColorPlan {
            cursor_position,
            outputs: Vec::with_capacity(self.outputs.len()),
        };
        for output in &self.outputs {
            let participating =
                !output.dpms_off && !state.soft_disabled_outputs.contains(&output.output_name);
            let mut output_plan =
                OutputExternalElementPlan::new(output.output_name.clone(), participating);
            if participating {
                self.observe_output_external_elements(
                    state,
                    output,
                    cursor_position,
                    dmabuf_texture_formats,
                    &mut output_plan,
                );
            }
            plan.outputs.push(output_plan);
        }
        plan
    }

    /// Observe every external element class on one participating output. The
    /// judgment is per class per output: absent sources, uncommitted trees,
    /// and off-output geometry all stay `Hidden` and never force the
    /// exact-sRGB fallback.
    fn observe_output_external_elements(
        &self,
        state: &crate::backend::wayland::state::JwmWaylandState,
        output: &KmsOutputState,
        cursor_position: Option<(i32, i32)>,
        dmabuf_texture_formats: &FormatSet,
        output_plan: &mut OutputExternalElementPlan,
    ) {
        // Cursor: the theme bitmap and the procedural fallback are both plain
        // CPU content, so a resolved position is always importable. An
        // invalid coordinate cannot prove the externally drawn cursor
        // disappeared, so it fails closed with a blocker but no placement.
        match cursor_position {
            Some(point) if point_in_output(point, output.origin, output.mode_size) => {
                output_plan.cursor_position = Some(point);
                output_plan.observe(
                    ExternalElementClass::Cursor,
                    classify_external_element(true, true, true),
                    "pointer_on_output",
                );
            }
            Some(_) => output_plan.observe(
                ExternalElementClass::Cursor,
                ExternalElementDisposition::Hidden,
                "pointer_off_output",
            ),
            None => output_plan.observe(
                ExternalElementClass::Cursor,
                ExternalElementDisposition::ExternalAssembly,
                "pointer_position_unknown",
            ),
        }

        match state.dnd_icon.as_ref() {
            None => output_plan.observe(
                ExternalElementClass::DragIcon,
                ExternalElementDisposition::Hidden,
                "no_drag_icon",
            ),
            Some(icon) => {
                let content =
                    summarize_external_surface_tree(&icon.surface, dmabuf_texture_formats);
                if !content.has_drawable() {
                    // An icon without a committed buffer produces no render
                    // elements anywhere, so it must not force the fallback.
                    output_plan.observe(
                        ExternalElementClass::DragIcon,
                        ExternalElementDisposition::Hidden,
                        "drag_icon_tree_uncommitted",
                    );
                } else if cursor_position.is_none() {
                    output_plan.observe(
                        ExternalElementClass::DragIcon,
                        ExternalElementDisposition::ExternalAssembly,
                        "pointer_position_unknown",
                    );
                } else {
                    // The icon follows the pointer and can straddle an output
                    // boundary, and the assembly places it on every
                    // participating output (clipped at scanout). Conservatively
                    // mark every participating output as hosting it.
                    output_plan.observe(
                        ExternalElementClass::DragIcon,
                        classify_external_element(true, true, content.all_importable()),
                        "drag_icon_follows_pointer",
                    );
                }
            }
        }

        if !state.session_locked {
            output_plan.observe(
                ExternalElementClass::SessionLock,
                ExternalElementDisposition::Hidden,
                "session_unlocked",
            );
        } else {
            match state.lock_surfaces.get(&output.output_name) {
                // Without a lock surface only the domain-invariant black
                // shield reaches scanout on this output.
                None => output_plan.observe(
                    ExternalElementClass::SessionLock,
                    ExternalElementDisposition::Hidden,
                    "no_lock_surface_on_output",
                ),
                Some(lock_surface) => {
                    let content = summarize_external_surface_tree(
                        lock_surface.wl_surface(),
                        dmabuf_texture_formats,
                    );
                    if content.has_drawable() {
                        output_plan.observe(
                            ExternalElementClass::SessionLock,
                            classify_external_element(true, true, content.all_importable()),
                            "lock_surface_committed",
                        );
                    } else {
                        output_plan.observe(
                            ExternalElementClass::SessionLock,
                            ExternalElementDisposition::Hidden,
                            "lock_surface_uncommitted",
                        );
                    }
                }
            }
        }

        let map = layer_map_for_output(&output.output);
        for (layer, class) in [
            (WlrLayer::Top, ExternalElementClass::LayerTop),
            (WlrLayer::Overlay, ExternalElementClass::LayerOverlay),
        ] {
            let mut any_drawable = false;
            let mut any_unimportable = false;
            let mut basis = "no_layer_mapped";
            for layer_surface in map.layers_on(layer) {
                let Some(geometry) = map.layer_geometry(layer_surface) else {
                    continue;
                };
                let overlaps = rect_overlaps_output(
                    (
                        output.origin.0 + geometry.loc.x,
                        output.origin.1 + geometry.loc.y,
                    ),
                    (geometry.size.w, geometry.size.h),
                    output.origin,
                    output.mode_size,
                );
                if !overlaps {
                    basis = "layer_outside_output";
                    continue;
                }
                let content = summarize_external_surface_tree(
                    layer_surface.wl_surface(),
                    dmabuf_texture_formats,
                );
                if !content.has_drawable() {
                    basis = "layer_tree_uncommitted";
                    continue;
                }
                any_drawable = true;
                if !content.all_importable() {
                    any_unimportable = true;
                }
            }
            if any_drawable {
                basis = "layer_overlaps_output";
            }
            output_plan.observe(
                class,
                classify_external_element(any_drawable, any_drawable, !any_unimportable),
                basis,
            );
        }
    }

    /// Composite one set of KMS render elements into a single offscreen
    /// texture through the same Smithay GL paths the scanout assembly uses,
    /// preserving premultiplied-alpha, encoded-sRGB content 1:1. `None` means
    /// the import/draw failed and the owning class must stay external this
    /// frame (fail-closed into the exact-sRGB fallback).
    fn composite_elements_to_texture(
        renderer: &mut GlesRenderer,
        elements: &[KmsRenderElement],
        size: (i32, i32),
    ) -> Option<GlesTexture> {
        if size.0 <= 0 || size.1 <= 0 {
            return None;
        }
        let mut texture =
            Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, size.into())
                .map_err(|error| {
                    log::debug!("[kms-cm] external element offscreen alloc failed: {error:?}");
                    error
                })
                .ok()?;
        let mut target = renderer
            .bind(&mut texture)
            .map_err(|error| {
                log::debug!("[kms-cm] external element offscreen bind failed: {error:?}");
                error
            })
            .ok()?;
        let phys: Size<i32, Physical> = size.into();
        let mut tracker = OutputDamageTracker::new(phys, Scale::from(1.0f64), Transform::Normal);
        // age=0 forces a full redraw; the transparent clear keeps uncovered
        // texels see-through so the compositor blends exactly the element.
        tracker
            .render_output(
                renderer,
                &mut target,
                0,
                elements,
                smithay::backend::renderer::Color32F::new(0.0, 0.0, 0.0, 0.0),
            )
            .map_err(|error| {
                log::debug!("[kms-cm] external element offscreen render failed: {error:?}");
                error
            })
            .ok()?;
        drop(target);
        // `render_output` flips Y in its projection, so the texture is stored
        // top-to-bottom and the compositor samples it with a plain 0..1 UV.
        Some(texture)
    }

    /// The common-linear adapter carries no per-element color transform, so
    /// every drawable surface in an external tree must be undescribed or
    /// exactly sRGB-default. A described PQ/HLG/wide-gamut tree fails this
    /// check and its class stays on the external assembly path — fail-closed
    /// rather than guessing at the source domain.
    fn tree_descriptions_stay_srgb_default(
        surface: &WlSurface,
        surface_params: Option<&SurfaceColorDescriptions>,
    ) -> bool {
        let Some(params) = surface_params else {
            return true;
        };
        let mut supported = true;
        with_surface_tree_downward(
            surface,
            (),
            |_, _, _| TraversalAction::DoChildren(()),
            |child_surface, _, _| {
                if let Some(params) = params.get(&child_surface.id()) {
                    supported &=
                        crate::backend::wayland_udev::color_pipeline::description_is_srgb_default(
                            params,
                        );
                }
            },
            |_, _, _| true,
        );
        supported
    }

    /// Stage one external surface tree: import through the renderer and
    /// composite it into an offscreen texture at `scale`, mirroring the KMS
    /// assembly's `render_elements` placement (root at `root_global` in global
    /// physical pixels, subsurface offsets scaled). The returned rectangle is
    /// the tree's visible bounding box in global physical pixels.
    fn stage_surface_tree(
        renderer: &mut GlesRenderer,
        surface: &WlSurface,
        root_global: (i32, i32),
        scale: f64,
        surface_params: Option<&SurfaceColorDescriptions>,
        draws: &mut Vec<super::super::compositor::ExternalElementVisual>,
    ) -> bool {
        if !Self::tree_descriptions_stay_srgb_default(surface, surface_params) {
            return false;
        }
        let bbox = smithay::desktop::utils::bbox_from_surface_tree(
            surface,
            Point::<i32, smithay::utils::Logical>::from((0, 0)),
        );
        if bbox.size.w <= 0 || bbox.size.h <= 0 {
            return false;
        }
        if import_surface_tree(renderer, surface).is_err() {
            return false;
        }
        let offset: Point<i32, Physical> = (
            (-f64::from(bbox.loc.x) * scale).round() as i32,
            (-f64::from(bbox.loc.y) * scale).round() as i32,
        )
            .into();
        let elements: Vec<KmsRenderElement> = render_elements_from_surface_tree(
            renderer,
            surface,
            offset,
            Scale::from(scale),
            1.0,
            Kind::Unspecified,
        );
        if elements.is_empty() {
            return false;
        }
        let size = (
            (f64::from(bbox.size.w) * scale).ceil() as i32,
            (f64::from(bbox.size.h) * scale).ceil() as i32,
        );
        let Some(texture) = Self::composite_elements_to_texture(renderer, &elements, size) else {
            return false;
        };
        let rect = [
            root_global.0 + (f64::from(bbox.loc.x) * scale).round() as i32,
            root_global.1 + (f64::from(bbox.loc.y) * scale).round() as i32,
            size.0,
            size.1,
        ];
        let tex_id = texture.tex_id();
        draws.push(super::super::compositor::ExternalElementVisual {
            texture: tex_id,
            owner: Some(texture),
            rect,
        });
        true
    }

    /// Stage the pointer cursor (theme bitmap or procedural fallback) at its
    /// hotspot-adjusted global physical position, mirroring the KMS assembly's
    /// per-output scale selection through the pointer's host output.
    fn stage_cursor(
        &mut self,
        plan: &ExternalElementColorPlan,
        cursor_kind: StdCursorKind,
        draws: &mut Vec<super::super::compositor::ExternalElementVisual>,
    ) -> bool {
        if !class_is_staging_candidate(plan, ExternalElementClass::Cursor) {
            return false;
        }
        // The plan resolves the pointer's host output once for check and draw
        // alike; an unplaceable pointer cannot be staged and keeps its blocker.
        let Some(host_idx) = plan
            .outputs
            .iter()
            .position(|output| output.participating && output.cursor_position.is_some())
        else {
            return false;
        };
        let Some((cursor_x, cursor_y)) = plan.outputs[host_idx].cursor_position else {
            return false;
        };
        let scale: Scale<f64> = self.outputs[host_idx]
            .output
            .current_scale()
            .fractional_scale()
            .into();
        let cursor_scale = scale.x.max(1.0).ceil() as u32;

        match self.cursor_bitmap(cursor_kind, cursor_scale) {
            Some(bitmap) => {
                let size = (bitmap.width, bitmap.height);
                if size.0 <= 0 || size.1 <= 0 {
                    return false;
                }
                let element = match MemoryRenderBufferRenderElement::from_buffer(
                    &mut self.renderer,
                    Point::<f64, Physical>::from((0.0, 0.0)),
                    &bitmap.buffer,
                    None,
                    None,
                    None,
                    Kind::Cursor,
                ) {
                    Ok(element) => element,
                    Err(_) => return false,
                };
                let Some(texture) = Self::composite_elements_to_texture(
                    &mut self.renderer,
                    &[KmsRenderElement::Memory(element)],
                    size,
                ) else {
                    return false;
                };
                let tex_id = texture.tex_id();
                draws.push(super::super::compositor::ExternalElementVisual {
                    texture: tex_id,
                    owner: Some(texture),
                    rect: [
                        cursor_x - bitmap.xhot,
                        cursor_y - bitmap.yhot,
                        size.0,
                        size.1,
                    ],
                });
                true
            }
            None => {
                // The procedural software fallback: body rects first, then the
                // +1px shadow rects, exactly the KMS front-to-back order.
                let mut min_x = i32::MAX;
                let mut min_y = i32::MAX;
                let mut max_x = i32::MIN;
                let mut max_y = i32::MIN;
                for &(rx, ry, rw, rh) in CURSOR_RECTS {
                    for (x, y) in [(rx, ry), (rx + 1, ry + 1)] {
                        min_x = min_x.min(x);
                        min_y = min_y.min(y);
                        max_x = max_x.max(x + rw);
                        max_y = max_y.max(y + rh);
                    }
                }
                let mut elements: Vec<KmsRenderElement> = Vec::new();
                for &(rx, ry, rw, rh) in CURSOR_RECTS {
                    let geo: Rectangle<i32, Physical> =
                        Rectangle::new((rx - min_x, ry - min_y).into(), (rw, rh).into());
                    elements.push(KmsRenderElement::Solid(SolidColorRenderElement::new(
                        Id::new(),
                        geo,
                        0usize,
                        smithay::backend::renderer::Color32F::new(0.98, 0.98, 0.98, 1.0),
                        Kind::Cursor,
                    )));
                }
                for &(rx, ry, rw, rh) in CURSOR_RECTS {
                    let geo: Rectangle<i32, Physical> =
                        Rectangle::new((rx + 1 - min_x, ry + 1 - min_y).into(), (rw, rh).into());
                    elements.push(KmsRenderElement::Solid(SolidColorRenderElement::new(
                        Id::new(),
                        geo,
                        0usize,
                        smithay::backend::renderer::Color32F::new(0.0, 0.0, 0.0, 0.55),
                        Kind::Cursor,
                    )));
                }
                let Some(texture) = Self::composite_elements_to_texture(
                    &mut self.renderer,
                    &elements,
                    (max_x - min_x, max_y - min_y),
                ) else {
                    return false;
                };
                let tex_id = texture.tex_id();
                draws.push(super::super::compositor::ExternalElementVisual {
                    texture: tex_id,
                    owner: Some(texture),
                    rect: [
                        cursor_x + min_x,
                        cursor_y + min_y,
                        max_x - min_x,
                        max_y - min_y,
                    ],
                });
                true
            }
        }
    }

    /// Stage the drag-and-drop icon following the pointer, mirroring the KMS
    /// placement (`cursor + icon.offset` in global physical pixels) and the
    /// pointer host output's scale.
    fn stage_drag_icon(
        &mut self,
        state: &crate::backend::wayland::state::JwmWaylandState,
        plan: &ExternalElementColorPlan,
        surface_params: Option<&SurfaceColorDescriptions>,
        draws: &mut Vec<super::super::compositor::ExternalElementVisual>,
    ) -> bool {
        if !class_is_staging_candidate(plan, ExternalElementClass::DragIcon) {
            return false;
        }
        let Some(icon) = state.dnd_icon.as_ref() else {
            return false;
        };
        // Without a resolved pointer the assembly draws nothing but blocks;
        // keep that fail-closed shape here.
        let Some((cursor_x, cursor_y)) = plan.cursor_position else {
            return false;
        };
        let host_scale = plan
            .outputs
            .iter()
            .position(|output| output.participating && output.cursor_position.is_some())
            .map(|idx| self.outputs[idx].output.current_scale().fractional_scale())
            .unwrap_or(1.0);
        Self::stage_surface_tree(
            &mut self.renderer,
            &icon.surface,
            (cursor_x + icon.offset.x, cursor_y + icon.offset.y),
            host_scale,
            surface_params,
            draws,
        )
    }

    /// Stage every overlapping surface of one layer-shell class across the
    /// participating outputs. All-or-nothing per class: any instance that
    /// fails to import or composite rolls back the class's staged draws so
    /// the frame can fall back as a whole.
    fn stage_layer_class(
        &mut self,
        plan: &ExternalElementColorPlan,
        layer: WlrLayer,
        class: ExternalElementClass,
        surface_params: Option<&SurfaceColorDescriptions>,
        draws: &mut Vec<super::super::compositor::ExternalElementVisual>,
    ) -> bool {
        if !class_is_staging_candidate(plan, class) {
            return false;
        }
        let staged_before = draws.len();
        let mut staged_any = false;
        for idx in 0..self.outputs.len() {
            let Some(output_plan) = plan.output(idx) else {
                continue;
            };
            if output_plan.class(class).disposition != ExternalElementDisposition::ExternalAssembly
            {
                continue;
            }
            let output = &self.outputs[idx];
            let (ox, oy) = output.origin;
            let (out_w, out_h) = output.mode_size;
            let scale = output.output.current_scale().fractional_scale();
            // Collect first: the layer map borrows the output while the
            // composite below needs the renderer.
            let map = layer_map_for_output(&output.output);
            let mut instances: Vec<(WlSurface, Point<i32, smithay::utils::Logical>)> = Vec::new();
            for layer_surface in map.layers_on(layer) {
                let Some(geometry) = map.layer_geometry(layer_surface) else {
                    continue;
                };
                if !rect_overlaps_output(
                    (ox + geometry.loc.x, oy + geometry.loc.y),
                    (geometry.size.w, geometry.size.h),
                    (ox, oy),
                    (out_w, out_h),
                ) {
                    continue;
                }
                instances.push((layer_surface.wl_surface().clone(), geometry.loc));
            }
            drop(map);
            for (surface, loc) in instances {
                if !Self::stage_surface_tree(
                    &mut self.renderer,
                    &surface,
                    (ox + loc.x, oy + loc.y),
                    scale,
                    surface_params,
                    draws,
                ) {
                    draws.truncate(staged_before);
                    return false;
                }
                staged_any = true;
            }
        }
        staged_any
    }

    /// Stage every internalizable external element class visible this frame
    /// into textures the compositor can draw into its common linear-sRGB
    /// target. The draw list is accumulated front-to-back in exactly the KMS
    /// element order (cursor, drag icon, overlay, top) and reversed at the end
    /// for the compositor's back-to-front painter order. Classes that fail
    /// staging keep their `ExternalAssembly`/`ImportBlocked` disposition, so
    /// the untouched plan still forces the exact-sRGB fallback and the KMS
    /// assembly keeps drawing them — never dropped, never mixed-domain.
    pub(super) fn stage_external_elements_for_linear(
        &mut self,
        state: &crate::backend::wayland::state::JwmWaylandState,
        plan: &ExternalElementColorPlan,
        cursor_kind: StdCursorKind,
    ) -> StagedExternalElements {
        let surface_params = state
            .color_manager
            .as_ref()
            .map(|manager| manager.snapshot_surface_params());
        let mut draws = Vec::new();
        let mut classes = [false; ExternalElementClass::ALL.len()];
        if self.stage_cursor(plan, cursor_kind, &mut draws) {
            classes[ExternalElementClass::Cursor.index()] = true;
        }
        if self.stage_drag_icon(state, plan, surface_params.as_ref(), &mut draws) {
            classes[ExternalElementClass::DragIcon.index()] = true;
        }
        if self.stage_layer_class(
            plan,
            WlrLayer::Overlay,
            ExternalElementClass::LayerOverlay,
            surface_params.as_ref(),
            &mut draws,
        ) {
            classes[ExternalElementClass::LayerOverlay.index()] = true;
        }
        if self.stage_layer_class(
            plan,
            WlrLayer::Top,
            ExternalElementClass::LayerTop,
            surface_params.as_ref(),
            &mut draws,
        ) {
            classes[ExternalElementClass::LayerTop.index()] = true;
        }
        draws.reverse();
        StagedExternalElements { draws, classes }
    }

    /// Record the current frame's color-delivery decision without claiming it
    /// reached the display. `render_if_needed` attaches the prepared plan to a
    /// successfully queued framebuffer; `on_vblank` is the only promotion
    /// point into the last-success snapshot. The external-element plan that
    /// gated the assembly is recorded alongside, so the IPC diagnosis shows
    /// the same classes the route decision saw.
    ///
    /// The compositor's frame tail arrives as the typed `LinearTailStatus`
    /// from the domain table: when the common-linear target is unavailable the
    /// aggregate `compositor_encoded_tail` name is emitted; otherwise every
    /// visible encoded-only overlay class is reported under its own name, in
    /// `TailOverlayClass::ALL` draw order, ahead of the KMS-side classes.
    pub(super) fn record_color_delivery_attempt(
        &mut self,
        decision: &ColorPipelineDecision,
        external_element_plan: &ExternalElementColorPlan,
        compositor_tail: &super::super::compositor::tail_domain::LinearTailStatus,
        scene_linear_active: bool,
    ) {
        let compositor_tail_safe = compositor_tail.linear_tail_safe();
        let linear_tail_blockers =
            linear_tail_blocker_names(compositor_tail, external_element_plan);
        let linear_tail_safe = compositor_tail_safe && external_element_plan.is_safe();
        debug_assert_eq!(linear_tail_safe, linear_tail_blockers.is_empty());
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
                linear_tail_blockers: Some(linear_tail_blockers),
                external_elements: Some(external_element_plan.class_statuses()),
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
                // The legacy path has no external-element plan; the field
                // stays absent rather than claiming an empty inventory.
                external_elements: None,
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

            if self.outputs[index].installed_gamma_lut.is_some()
                || self.outputs[index].installed_ctm.is_some()
            {
                // Clear the stale CTM+LUT pair in one atomic request: the two
                // stages must leave scanout together, never one at a time.
                if let Err(error) =
                    self.apply_scanout_color_goals(&[(index, OutputScanoutColorGoal::CLEAR)])
                {
                    log::warn!(
                        "[kms-cm] stale color stage teardown on {} failed: {error}",
                        self.outputs[index].output_name,
                    );
                    // Keep the cached target paired with the tracked hardware
                    // state. The per-frame refresh retries this transition and
                    // suppresses presentation in the meantime.
                    ready = false;
                    continue;
                }
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

    /// Push (or clear) the connector HDR signalling — HDR_OUTPUT_METADATA and
    /// Colorspace together in ONE controlled atomic request (TEST_ONLY +
    /// commit), so the sink never sees HDR metadata under a Default colorspace
    /// or vice versa.
    ///
    /// Pass `Some(&blob)` (32-byte CTA-861.3 HDR Static Metadata) to put the
    /// display into HDR mode, or `None` to revert to SDR (blob_id = 0,
    /// Colorspace = Default). Enabling is fail-closed on the complete scanout
    /// chain: same DRM device, a 10-bit-or-deeper swapchain framebuffer the
    /// primary plane can scan out, the CRTC GAMMA_LUT/CTM stages, and both
    /// connector signalling properties. Any gap keeps software SDR delivery
    /// and never claims hardware HDR active. (The compositor-level gate in
    /// `set_hdr_metadata` rejects enables earlier; this layer stays correct
    /// even if that gate is lifted.)
    pub(super) fn set_hdr_metadata_for_output(
        &mut self,
        output_idx: usize,
        blob: Option<&[u8; 32]>,
    ) -> Result<(), String> {
        let (conn_handle, smithay_output, handles, swapchain_fourcc, plane_formats) = self
            .outputs
            .get(output_idx)
            .map(|output| {
                (
                    output.connector,
                    output.output.clone(),
                    output.color_property_handles,
                    output.swapchain_fourcc,
                    output.primary_plane_formats.clone(),
                )
            })
            .ok_or("output index out of range")?;

        if blob.is_some()
            && let Some(gap) = hdr_scanout_chain_gap(
                // The scanout swapchain and the color stages both live on this
                // KmsState's single DRM device (allocator and KMS fd derive
                // from the same device fd).
                true,
                swapchain_fourcc,
                &plane_formats,
                &handles,
            )
        {
            return Err(format!(
                "HDR scanout chain incomplete ({gap:?}); keeping software SDR delivery"
            ));
        }

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

        // Chain validation guarantees the BT2020_RGB enum value exists when
        // enabling; clearing always targets the neutral Default (0).
        let colorspace = if blob.is_some() {
            handles.colorspace_bt2020_rgb.unwrap_or(0)
        } else {
            0
        };
        let plan = [AtomicColorOutputPlan {
            crtc: u32::from(self.outputs[output_idx].crtc),
            connector: u32::from(conn_handle),
            handles,
            target: ScanoutColorTarget {
                // CRTC stages are owned by `apply_scanout_color_goals` and
                // stay out of the signalling request.
                degamma_lut: None,
                ctm: None,
                gamma_lut: None,
                colorspace: Some(colorspace),
                hdr_output_metadata: Some(blob_id),
            },
        }];
        let commit_result = build_atomic_color_request(&plan)
            .and_then(|assignments| commit_atomic_color_request(dev, &assignments));
        if let Err(error) = commit_result {
            if blob_id != 0 {
                let _ = dev.destroy_property_blob(blob_id);
            }
            return Err(error);
        }
        drop(mgr);

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

    /// Program every changed output's CRTC color stages in ONE controlled
    /// atomic request (TEST_ONLY + commit), replacing the previous
    /// per-property commit sequence whose intermediate states (e.g. LUT bound
    /// while the paired CTM was not yet installed) could reach scanout. The
    /// kernel's atomicity provides the all-or-nothing semantics the old
    /// rollback loops emulated: on any failure no tracked state changes and
    /// every newly created blob is destroyed. DEGAMMA is pinned to neutral in
    /// the same request whenever the stage exists, so the complete
    /// input→output color chain is always defined by one commit.
    fn apply_scanout_color_goals(
        &mut self,
        goals: &[(usize, OutputScanoutColorGoal)],
    ) -> Result<(), String> {
        // Only outputs whose tracked state differs from the goal participate
        // in the request; an unchanged output keeps its live blobs.
        let changed: Vec<(usize, OutputScanoutColorGoal)> = goals
            .iter()
            .filter_map(|&(index, goal)| {
                let output = self.outputs.get(index)?;
                (!scanout_color_goal_matches(
                    output.installed_gamma_lut,
                    output.installed_ctm,
                    &goal,
                ))
                .then_some((index, goal))
            })
            .collect();
        if changed.is_empty() {
            return Ok(());
        }

        struct StagedOutputBlobs {
            index: usize,
            gamma_lut: Option<(
                u64,
                crate::backend::wayland_udev::color_pipeline::TransferKind,
            )>,
            ctm: Option<u64>,
        }

        // Create every new blob before touching hardware. A blob is inert
        // until an atomic commit references it, so unwinding after a failure
        // only needs to destroy the freshly created ids.
        let mut created: Vec<u64> = Vec::new();
        let mut staged: Vec<StagedOutputBlobs> = Vec::new();
        let result = (|state: &mut Self| {
            let mut plans: Vec<AtomicColorOutputPlan> = Vec::new();
            let commit_result = {
                let mgr = state.drm_output_manager.lock();
                let dev = mgr.device();
                for &(index, goal) in &changed {
                    let output = &state.outputs[index];
                    let caps = output
                        .color_pipeline_caps
                        .as_ref()
                        .ok_or("no color pipeline caps cached for output")?;
                    let gamma_lut = match goal.gamma_lut {
                        Some(tf) => {
                            if !caps.gamma_lut_supported {
                                return Err("CRTC does not advertise GAMMA_LUT".to_string());
                            }
                            let id = create_gamma_lut_blob(dev, tf, caps.gamma_lut_size as usize)?;
                            created.push(id);
                            Some((id, tf))
                        }
                        None => None,
                    };
                    let ctm = match goal.ctm {
                        Some(matrix) => {
                            if !caps.ctm_supported {
                                return Err("CRTC does not advertise CTM".to_string());
                            }
                            let id = create_ctm_blob(dev, matrix)?;
                            created.push(id);
                            Some(id)
                        }
                        None => None,
                    };
                    plans.push(AtomicColorOutputPlan {
                        crtc: u32::from(output.crtc),
                        connector: u32::from(output.connector),
                        handles: output.color_property_handles,
                        target: ScanoutColorTarget {
                            degamma_lut: Some(0),
                            ctm: Some(ctm.unwrap_or(0)),
                            gamma_lut: Some(gamma_lut.map(|(id, _)| id).unwrap_or(0)),
                            // Connector signalling transitions are owned by
                            // `set_hdr_metadata_for_output` and stay out of
                            // the CRTC stage request.
                            colorspace: None,
                            hdr_output_metadata: None,
                        },
                    });
                    staged.push(StagedOutputBlobs {
                        index,
                        gamma_lut,
                        ctm,
                    });
                }
                let assignments = build_atomic_color_request(&plans)?;
                commit_atomic_color_request(dev, &assignments)
            };
            commit_result.map(|_| plans.len())
        })(self);

        let committed = match result {
            Ok(committed) => committed,
            Err(error) => {
                let mgr = self.drm_output_manager.lock();
                for id in created {
                    let _ = mgr.device().destroy_property_blob(id);
                }
                return Err(error);
            }
        };

        // The request is live: swap tracked state and release the replaced
        // blobs (the commit atomically dropped their hardware references),
        // then invalidate delivery evidence for every changed output so no
        // pre-transition frame can be reported under the new color state.
        let mut replaced: Vec<u64> = Vec::new();
        for entry in &staged {
            let output = &self.outputs[entry.index];
            if let Some((id, _)) = output.installed_gamma_lut {
                replaced.push(id);
            }
            replaced.extend(output.installed_ctm);
        }
        for entry in staged {
            self.outputs[entry.index].installed_gamma_lut = entry.gamma_lut;
            self.outputs[entry.index].installed_ctm = entry.ctm;
            self.invalidate_color_delivery_after_hardware_change(entry.index);
        }
        {
            let mgr = self.drm_output_manager.lock();
            for id in replaced {
                let _ = mgr.device().destroy_property_blob(id);
            }
        }
        log::info!("[kms-cm] atomic color stage commit covered {committed} output(s)");
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
    ///
    /// `source_peaks` carries the frame's per-output aggregation of visible
    /// surface source peaks (working-space units, keyed by output name); it
    /// feeds each software region's delivery tone-map plan. Outputs missing
    /// from the map default to the SDR reference (1.0), which selects the
    /// pass-through policy — the exact pre-tone-map behavior.
    pub(super) fn refresh_color_pipeline_offload(
        &mut self,
        state: &crate::backend::wayland::state::JwmWaylandState,
        linear_tail_safe: bool,
        scene_linear_active: bool,
        source_peaks: &std::collections::HashMap<String, f32>,
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
                source_peak_working: source_peaks
                    .get(&output.output_name)
                    .copied()
                    .unwrap_or(1.0),
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

        // --- CRTC color stage activation, one controlled atomic request.
        //
        // A CTM is linear-light math and only valid paired with the hardware
        // OETF in GAMMA_LUT; both stages therefore move in the same commit
        // instead of sequentially. `apply_scanout_color_goals` covers every
        // output (participating or not) whose tracked state differs from the
        // goal, so gate-off, DPMS-off participation drops, target changes,
        // and capability shortfalls all converge on the same neutral-state
        // request.
        let target = uniform_tf.filter(|_| gate_on);

        let mut any_participating = false;
        let mut lut_capable = true;
        let mut ctm_capable = true;
        for i in 0..n {
            if !participating[i] {
                continue;
            }
            any_participating = true;
            let caps = self.outputs[i].color_pipeline_caps.as_ref();
            if !caps
                .map(|c| c.gamma_lut_supported && c.gamma_lut_size >= 256)
                .unwrap_or(false)
            {
                lut_capable = false;
            }
            if !caps.map(|c| c.ctm_supported).unwrap_or(false) {
                ctm_capable = false;
            }
        }
        let hw_pair_target = target.filter(|_| any_participating && lut_capable && ctm_capable);

        // When `hw_pair_target` is set, the per-surface ColorTransform pass in
        // backend.rs targets sRGB primaries so the FBO is uniform-sRGB and
        // each CRTC's CTM converts to native primaries at scanout.
        let goals: Vec<(usize, OutputScanoutColorGoal)> = (0..n)
            .map(|i| {
                let goal = match hw_pair_target.filter(|_| participating[i]) {
                    Some(tf) => OutputScanoutColorGoal {
                        gamma_lut: Some(tf),
                        ctm: Some(self.outputs[i].output_ctm),
                    },
                    None => OutputScanoutColorGoal::CLEAR,
                };
                (i, goal)
            })
            .collect();
        if let Err(error) = self.apply_scanout_color_goals(&goals) {
            // Mirror the old rollback: return every output to neutral in a
            // second all-or-nothing request so software delivery can proceed
            // under a known domain. If even that fails, the tracked state no
            // longer describes the hardware and the coherence check in
            // `finish_color_pipeline_decision` blocks presentation until a
            // later refresh resolves ownership.
            log::warn!(
                "[kms-cm] atomic color stage commit failed ({error}); clearing all stages to neutral"
            );
            let clear_goals: Vec<(usize, OutputScanoutColorGoal)> =
                (0..n).map(|i| (i, OutputScanoutColorGoal::CLEAR)).collect();
            if let Err(clear_error) = self.apply_scanout_color_goals(&clear_goals) {
                log::warn!("[kms-cm] neutral clear commit also failed: {clear_error}");
            }
        }

        // Report what the hardware actually owns now, not what was requested:
        // a failed commit leaves the previous state installed, and the
        // coherence check below blocks presentation until a retry resolves
        // ownership.
        decision.hw_encode_active = hw_pair_target.is_some_and(|tf| {
            (0..n).all(|i| {
                !participating[i]
                    || matches!(self.outputs[i].installed_gamma_lut, Some((_, t)) if t == tf)
            })
        });
        decision.hw_ctm_active = hw_pair_target.is_some()
            && (0..n).all(|i| !participating[i] || self.outputs[i].installed_ctm.is_some());

        if decision.hw_encode_active && decision.hw_ctm_active {
            // The CRTC pair consumes the common linear-sRGB texture directly;
            // no software output conversion remains.
            decision.software_regions = None;
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

    /// Wrap one compositor-owned raw texture in a Smithay `GlesTexture`,
    /// caching the wrapper per texture identity. Every wrapper generation is
    /// retained in `keepalive`: the compositor owns and may explicitly
    /// recreate/delete the raw GL name, so a stale wrapper must never delete
    /// a recycled id belonging to a newer texture.
    fn wrap_compositor_texture(
        renderer: &GlesRenderer,
        cache: &mut Option<(u32, u32, u32, u32, u64, GlesTexture)>,
        keepalive: &mut Vec<(u64, GlesTexture)>,
        tex_id: u32,
        width: u32,
        height: u32,
        internal_format: u32,
        generation: u64,
    ) -> GlesTexture {
        if let Some((cached_id, cached_w, cached_h, cached_format, cached_generation, cached_tex)) =
            cache.as_ref()
            && *cached_id == tex_id
            && *cached_w == width
            && *cached_h == height
            && *cached_format == internal_format
            && *cached_generation == generation
        {
            return cached_tex.clone();
        }
        let size: Size<i32, BufferCoord> = (width as i32, height as i32).into();
        let tex =
            unsafe { GlesTexture::from_raw(renderer, Some(internal_format), false, tex_id, size) };
        keepalive.push((generation, tex.clone()));
        *cache = Some((
            tex_id,
            width,
            height,
            internal_format,
            generation,
            tex.clone(),
        ));
        tex
    }

    /// The full-screen compositor-frame element for one output. `origin` is
    /// the output's global layout origin: subtracting it slices the output's
    /// rectangle out of the single global framebuffer. Used for both the
    /// scanout texture and, during offscreen capture renders, the explicitly
    /// encoded capture view.
    fn compositor_frame_element(
        renderer: &GlesRenderer,
        texture: GlesTexture,
        origin: (i32, i32),
    ) -> KmsRenderElement {
        let context_id = renderer.context_id();
        KmsRenderElement::Texture(TextureRenderElement::from_static_texture(
            Id::new(),
            context_id,
            ((-origin.0) as f64, (-origin.1) as f64),
            texture,
            1,
            Transform::Flipped180,
            None,
            None,
            None,
            None,
            Kind::Unspecified,
        ))
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
            // &mut self which isn't available here). The same scan also
            // captures the raw property handles the controlled atomic commit
            // programs later, plus the connector signalling handles.
            let (color_pipeline_caps, color_property_handles) = {
                let mgr = drm_output_manager.lock();
                let dev = mgr.device();
                let mut caps = crate::backend::api::KmsColorPipelineCaps::default();
                let mut color_handles = ScanoutColorPropertyHandles::default();
                if let Ok(props) = dev.get_properties(p.crtc) {
                    let (handles, values) = props.as_props_and_values();
                    for (i, &prop_handle) in handles.iter().enumerate() {
                        if let Ok(info) = dev.get_property(prop_handle) {
                            match info.name().to_str().unwrap_or("") {
                                "DEGAMMA_LUT" => {
                                    caps.degamma_lut_supported = true;
                                    color_handles.degamma_lut = Some(u32::from(prop_handle));
                                }
                                "GAMMA_LUT" => {
                                    caps.gamma_lut_supported = true;
                                    color_handles.gamma_lut = Some(u32::from(prop_handle));
                                }
                                "CTM" => {
                                    caps.ctm_supported = true;
                                    color_handles.ctm = Some(u32::from(prop_handle));
                                }
                                "DEGAMMA_LUT_SIZE" => caps.degamma_lut_size = values[i] as u32,
                                "GAMMA_LUT_SIZE" => caps.gamma_lut_size = values[i] as u32,
                                _ => {}
                            }
                        }
                    }
                }
                if let Ok(props) = dev.get_properties(p.connector) {
                    let (handles, _values) = props.as_props_and_values();
                    for &prop_handle in handles {
                        if let Ok(info) = dev.get_property(prop_handle) {
                            match info.name().to_str().unwrap_or("") {
                                "Colorspace" => {
                                    color_handles.colorspace = Some(u32::from(prop_handle));
                                    // The BT2020_RGB enum value is required to
                                    // signal BT.2020 primaries for HDR; capture
                                    // it from the property's enum table.
                                    if let smithay::reexports::drm::control::property::ValueType::Enum(enums) = info.value_type() {
                                        let (_raw, enum_values) = enums.values();
                                        color_handles.colorspace_bt2020_rgb =
                                            enum_values.iter().find_map(|entry| {
                                                (entry.name().to_str() == Ok("BT2020_RGB"))
                                                    .then_some(entry.value())
                                            });
                                    }
                                }
                                "HDR_OUTPUT_METADATA" => {
                                    color_handles.hdr_output_metadata =
                                        Some(u32::from(prop_handle));
                                }
                                _ => {}
                            }
                        }
                    }
                }
                (Some(caps), color_handles)
            };

            // Snapshot the framebuffer side of the scanout chain: the format
            // the swapchain allocates for this output and the formats its
            // primary plane can scan out. HDR signalling stays fail-closed
            // unless this whole chain is 10-bit or deeper.
            let swapchain_fourcc = drm_output.with_compositor(|compositor| compositor.format());
            let primary_plane_formats: Vec<u32> = drm_output.with_compositor(|compositor| {
                // Smithay's PlaneInfo carries a FormatSet (fourcc + modifiers);
                // the chain validation only needs the fourccs.
                compositor
                    .surface()
                    .plane_info()
                    .formats
                    .iter()
                    .map(|format| format.code as u32)
                    .collect()
            });

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
                color_property_handles,
                swapchain_fourcc,
                primary_plane_formats,
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
            capture_texture_cache: None,
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
            internalized_external_frame: None,
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

        // The same per-frame plan that fed the color-delivery route also
        // gates the element assembly below: visibility is decided in exactly
        // one place (`external_element_color_plan`), never re-derived here.
        // The plan is then overlaid with the verdict of what the compositor
        // actually internalized into its output texture (pinned to that exact
        // texture by id+generation), so a staged class is never drawn twice
        // and a texture without the staged content is never skipped.
        let mut external_plan = self.external_element_color_plan(state);
        if let Some(compositor) = compositor
            && let Some(frame) = self.internalized_external_frame
        {
            frame.apply_to_plan(
                &mut external_plan,
                compositor.output_texture_id(),
                compositor.output_texture_generation(),
            );
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
            // The plan is aligned with `self.outputs` by index; the early
            // continue above already established this output participates.
            let Some(external_output) = external_plan.output(out_idx) else {
                continue;
            };

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
            // Position of the full-screen compositor-texture element, so the
            // capture path below can temporarily point it at the compositor's
            // explicitly encoded capture view.
            let mut compositor_element_index: Option<usize> = None;

            // Cursor will be pushed FIRST (front-most), when the frame's
            // external element plan places it on this output. The plan
            // resolves the pointer coordinate once for check and draw alike;
            // an unrepresentable coordinate contributes a blocker but no
            // placement, so nothing is drawn for it.
            if external_output.assembles(ExternalElementClass::Cursor)
                && let Some((cursor_x, cursor_y)) = external_output.cursor_position
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
                if external_output.assembles(ExternalElementClass::SessionLock)
                    && let Some(lock_surface) = state.lock_surfaces.get(&out.output_name)
                {
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
            // render paths identically. The plan's resolved pointer position places
            // it; without one the class reports a blocker but draws nothing.
            // An internalized icon is drawn by the compositor instead, but its
            // tree still needs the output-enter and frame-callback bookkeeping.
            if external_output.shows(ExternalElementClass::DragIcon)
                && let Some(icon) = state.dnd_icon.as_ref()
            {
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
                if external_output.assembles(ExternalElementClass::DragIcon)
                    && let Some((cursor_x, cursor_y)) = external_plan.cursor_position
                {
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
            }

            // Layer surfaces above normal windows, gated per class by the same
            // plan that reported them to the color-delivery route.
            {
                let map = layer_map_for_output(&out.output);
                for (layer, class) in [
                    (WlrLayer::Overlay, ExternalElementClass::LayerOverlay),
                    (WlrLayer::Top, ExternalElementClass::LayerTop),
                ] {
                    if !external_output.shows(class) {
                        continue;
                    }
                    for ls in map.layers_on(layer) {
                        let Some(geo) = map.layer_geometry(ls) else {
                            continue;
                        };
                        if !rect_overlaps_output(
                            (ox + geo.loc.x, oy + geo.loc.y),
                            (geo.size.w, geo.size.h),
                            (ox, oy),
                            (out_w, out_h),
                        ) {
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

                        // Internalized layers are inside the compositor
                        // texture already; only the external assembly pushes
                        // elements.
                        if external_output.assembles(class) {
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
                let output_tex = Self::wrap_compositor_texture(
                    &self.renderer,
                    &mut self.compositor_texture_cache,
                    &mut self.compositor_texture_keepalive,
                    comp.output_texture_id(),
                    sw,
                    sh,
                    comp.output_texture_internal_format(),
                    comp.output_texture_generation(),
                );
                // Position is output-relative: subtract the output's global origin so each
                // output sees the correct slice of the single full-screen FBO.
                compositor_element_index = Some(elements.len());
                elements.push(Self::compositor_frame_element(
                    &self.renderer,
                    output_tex,
                    (ox, oy),
                ));
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

            // ── Capture view selection ─────────────────────────────────────
            // Capture consumers re-render this element list offscreen. On
            // deferred color-delivery routes the scanout compositor texture is
            // output-referred (linear for CRTC delivery, per-output encoded
            // for region delivery), which is not capture semantics, so the
            // compositor element temporarily points at the explicitly encoded
            // capture view (compositor render.rs section 18c). The scanout
            // list is restored before render_frame below: capture never
            // changes the physical route.
            // The capture view only matters when this output's element list
            // actually carries the compositor texture; a direct-scanout list
            // of client surface elements is route-independent already.
            let capture_view =
                if self.capture_readback_pending() && compositor_element_index.is_some() {
                    compositor.map(|comp| comp.capture_view())
                } else {
                    None
                };
            let capture_unavailable = matches!(
                capture_view,
                Some(super::super::compositor::CompositorCaptureView::Unavailable)
            );
            let mut swapped_compositor_element = None;
            if let (
                Some(comp),
                Some(element_index),
                Some(super::super::compositor::CompositorCaptureView::Dedicated {
                    texture,
                    internal_format,
                    generation,
                }),
            ) = (compositor, compositor_element_index, capture_view)
            {
                // The capture view spans the same global framebuffer as the
                // scanout texture, so the wrapper size is the compositor's
                // screen size, not this output's mode size.
                let (sw, sh) = comp.screen_size();
                let capture_tex = Self::wrap_compositor_texture(
                    &self.renderer,
                    &mut self.capture_texture_cache,
                    &mut self.compositor_texture_keepalive,
                    texture,
                    sw,
                    sh,
                    internal_format,
                    generation,
                );
                let capture_element =
                    Self::compositor_frame_element(&self.renderer, capture_tex, (ox, oy));
                swapped_compositor_element = Some(std::mem::replace(
                    &mut elements[element_index],
                    capture_element,
                ));
            }
            if capture_unavailable {
                // A deferred route without a fresh capture view (its
                // allocation failed): skip this frame's captures instead of
                // reading pixels in the wrong color domain. The pending
                // queues stay armed, so the next frame retries.
                any_failed = true;
                log::warn!(
                    "[kms] capture skipped: compositor has no encoded capture view this frame"
                );
            }

            // ── Screenshot capture (offscreen render) ───────────────────────
            if out_idx == 0 && !capture_unavailable {
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
            if !capture_unavailable && let Some(ref pending_queue) = self.screencopy_pending {
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
            if !capture_unavailable && let Some(ref pending_queue) = self.image_capture_pending {
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

            // Restore the scanout compositor texture before the DRM render:
            // the physical route must be exactly what it would have been
            // without any capture consumer.
            if let (Some(element_index), Some(original)) =
                (compositor_element_index, swapped_compositor_element)
            {
                elements[element_index] = original;
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
        AtomicColorAssignment, AtomicColorOutputPlan, ColorDeliveryPlan, ColorPipelineDecision,
        CrtcColorProperty, ExternalElementClass, ExternalElementColorPlan,
        ExternalElementDisposition, FrameQueueBoundary, HdrScanoutChainGap,
        InternalizedExternalFrame, LinearTailBlocker, OutputColorDeliveryTracker,
        OutputColorRegionCandidate, OutputExternalElementPlan, OutputScanoutColorGoal,
        QueuedFrameData, ScanoutColorPropertyHandles, ScanoutColorTarget,
        build_atomic_color_request, class_is_staging_candidate, classify_external_element,
        client_direct_scanout_presented, commit_staged_internalization,
        compositor_output_texture_identity_matches, connector_color_property_neutral_value,
        crtc_color_property, direct_scanout_allowed_for_color_retry,
        frame_flags_for_color_delivery, frame_watchdog_remaining, frame_watchdog_timeout,
        gamma_ramp_is_identity, hdr_scanout_chain_gap, legacy_color_delivery_attempt_needed,
        linear_tail_blocker_names, output_color_target, plan_output_configuration_rollback,
        plan_software_color_regions, point_in_output, prepared_color_delivery,
        rect_overlaps_output, rollback_mode_requires_restore, rounded_pointer_location,
        scanout_color_goal_matches, scanout_format_channel_bits, smithay_transform_to_wl,
        submitted_color_delivery_observation, vblank_is_not_older_than_queue,
        wl_transform_to_smithay,
    };
    use crate::backend::wayland_udev::color_management::ParametricParams;
    use crate::backend::wayland_udev::color_pipeline::{IDENTITY_CTM, TransferKind};
    use crate::backend::wayland_udev::compositor::tail_domain::{
        LinearTailStatus, TailOverlayVisibility, tail_overlay_blockers,
    };
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
            source_peak_working: 1.0,
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

    /// Build a plan with one output carrying the given per-class dispositions,
    /// exercising the same aggregation the frame loop relies on.
    fn plan_with_output(
        participating: bool,
        cursor_position: Option<(i32, i32)>,
        classes: &[(ExternalElementClass, ExternalElementDisposition)],
    ) -> ExternalElementColorPlan {
        let mut output = OutputExternalElementPlan::new("HDMI-A-1".to_owned(), participating);
        output.cursor_position = cursor_position;
        for &(class, disposition) in classes {
            output.observe(class, disposition, "test");
        }
        ExternalElementColorPlan {
            cursor_position,
            outputs: vec![output],
        }
    }

    #[test]
    fn external_element_plan_reports_every_visible_frame_tail_class() {
        let safe = ExternalElementColorPlan::default();
        assert!(safe.is_safe());
        assert!(safe.blockers().is_empty());

        let blocked = plan_with_output(
            true,
            Some((5, 5)),
            &[
                (
                    ExternalElementClass::Cursor,
                    ExternalElementDisposition::ExternalAssembly,
                ),
                (
                    ExternalElementClass::DragIcon,
                    ExternalElementDisposition::ExternalAssembly,
                ),
                (
                    ExternalElementClass::SessionLock,
                    ExternalElementDisposition::ExternalAssembly,
                ),
                (
                    ExternalElementClass::LayerTop,
                    ExternalElementDisposition::ExternalAssembly,
                ),
                (
                    ExternalElementClass::LayerOverlay,
                    ExternalElementDisposition::ExternalAssembly,
                ),
            ],
        );
        assert!(!blocked.is_safe());
        // Capture/readback deliberately contributes nothing: it reads the
        // compositor's independent encoded capture view, so the KMS class
        // list is exactly the five external element classes.
        assert_eq!(
            blocked
                .blockers()
                .into_iter()
                .map(LinearTailBlocker::wire_name)
                .collect::<Vec<_>>(),
            [
                "session_lock_surface",
                "drag_icon",
                "cursor",
                "top_layer_surface",
                "overlay_layer_surface",
            ]
        );
    }

    #[test]
    fn external_element_classification_tracks_visibility_and_importability() {
        use ExternalElementDisposition::*;
        // Invisible classes never assemble and never block.
        assert_eq!(classify_external_element(false, false, false), Hidden);
        assert_eq!(classify_external_element(false, true, true), Hidden);
        // An active source without drawable content (e.g. an uncommitted
        // surface tree) produces no pixels and no blocker.
        assert_eq!(classify_external_element(true, false, true), Hidden);
        // Visible and importable: externally assembled today, still blocking
        // until a common-linear adapter owns the class.
        assert_eq!(
            classify_external_element(true, true, true),
            ExternalAssembly
        );
        // Visible but unimportable — including any unimportable subsurface in
        // the tree — stays a hard, named blocker even for a future adapter.
        assert_eq!(classify_external_element(true, true, false), ImportBlocked);

        assert!(!Hidden.assembles_externally());
        assert!(!Hidden.contributes_blocker());
        assert!(ExternalAssembly.assembles_externally());
        assert!(ExternalAssembly.contributes_blocker());
        assert!(ImportBlocked.assembles_externally());
        assert!(ImportBlocked.contributes_blocker());
    }

    #[test]
    fn rect_overlap_matches_smithay_half_open_semantics_without_overflow() {
        let origin = (-1920, 0);
        let size = (1920, 1080);
        assert!(rect_overlaps_output((-1920, 0), (100, 100), origin, size));
        assert!(rect_overlaps_output((-10, 0), (20, 20), origin, size));
        // Edge-touching is not overlapping (strict half-open comparison).
        assert!(!rect_overlaps_output((0, 0), (100, 100), origin, size));
        assert!(!rect_overlaps_output(
            (-1920, 1080),
            (100, 100),
            origin,
            size
        ));
        // Degenerate rectangles follow smithay's `Rectangle::overlaps`.
        assert!(rect_overlaps_output((5, 5), (0, 0), (0, 0), (10, 10)));
        assert!(!rect_overlaps_output((10, 5), (0, 0), (0, 0), (10, 10)));
        // i64 math keeps extreme layout coordinates exact.
        assert!(rect_overlaps_output(
            (i32::MAX - 10, i32::MAX - 10),
            (20, 20),
            (i32::MAX - 20, i32::MAX - 20),
            (30, 30),
        ));
        assert!(!rect_overlaps_output(
            (i32::MIN, i32::MIN),
            (10, 10),
            (i32::MAX - 100, i32::MAX - 100),
            (50, 50),
        ));
    }

    #[test]
    fn every_blocker_wire_name_is_in_the_recognized_name_table() {
        for blocker in LinearTailBlocker::ALL {
            assert!(
                crate::backend::api::is_known_linear_tail_blocker_name(blocker.wire_name()),
                "{}",
                blocker.wire_name()
            );
        }
        // Each external element class owns exactly one blocker, and the class
        // name and blocker name are one vocabulary.
        for class in ExternalElementClass::ALL {
            assert_eq!(class.wire_name(), class.blocker().wire_name());
        }
        // The compositor-owned frame tail is one vocabulary too: every
        // encoded-only overlay class's blocker name is recognized. (The
        // per-class domain matrix itself is tested in the compositor's
        // tail_domain module.)
        for class in crate::backend::wayland_udev::compositor::tail_domain::TailOverlayClass::ALL {
            if let Some(name) = class.blocker_wire_name() {
                assert!(
                    crate::backend::api::is_known_linear_tail_blocker_name(name),
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn tail_blocker_inventory_merges_typed_compositor_status_and_plan() {
        let tail_status = |ready: bool, visibility: &TailOverlayVisibility| LinearTailStatus {
            linear_target_ready: ready,
            overlay_blockers: tail_overlay_blockers(visibility),
        };
        let clear = TailOverlayVisibility::default();

        // Clear tail, clear plan: an empty inventory, which is also the safe
        // state (the record path debug-asserts the two agree).
        let safe_plan = ExternalElementColorPlan::default();
        assert!(linear_tail_blocker_names(&tail_status(true, &clear), &safe_plan).is_empty());

        // Common-linear-aware overlays (expose/peek/snap preview/overview)
        // are visible but contribute no blocker after their migration.
        let mut migrated = TailOverlayVisibility::default();
        migrated.expose = true;
        migrated.peek = true;
        migrated.snap_preview = true;
        migrated.overview = true;
        assert!(
            linear_tail_blocker_names(&tail_status(true, &migrated), &safe_plan).is_empty(),
            "migrated common-linear-aware overlays must not appear in the inventory"
        );

        // Encoded-only overlays report their own names in draw order.
        let mut encoded = TailOverlayVisibility::default();
        encoded.recording_region_overlay = true;
        encoded.workspace_transition = true;
        encoded.toast = true;
        assert_eq!(
            linear_tail_blocker_names(&tail_status(true, &encoded), &safe_plan),
            [
                "workspace_transition_overlay",
                "toast_overlay",
                "recording_region_overlay"
            ]
        );

        // Without a live linear target the tail reports the aggregate name,
        // not the per-class list.
        assert_eq!(
            linear_tail_blocker_names(&tail_status(false, &encoded), &safe_plan),
            ["compositor_encoded_tail"]
        );

        // The KMS-assembled classes follow the compositor-owned tail.
        let blocked_plan = plan_with_output(
            true,
            Some((5, 5)),
            &[(
                ExternalElementClass::Cursor,
                ExternalElementDisposition::ExternalAssembly,
            )],
        );
        assert_eq!(
            linear_tail_blocker_names(&tail_status(true, &encoded), &blocked_plan),
            [
                "workspace_transition_overlay",
                "toast_overlay",
                "recording_region_overlay",
                "cursor"
            ]
        );
        // Capture/readback appears nowhere: it reads the independent encoded
        // capture view and never constrains the route (P0-4 decoupling).
        for visibility in [&clear, &migrated, &encoded] {
            let names = linear_tail_blocker_names(&tail_status(true, visibility), &blocked_plan);
            assert!(!names.iter().any(|name| name == "capture_readback"));
        }
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
    fn inactive_outputs_contribute_no_blockers_or_assembly() {
        // Defense in depth: even a populated entry on a non-participating
        // output neither assembles nor blocks (the DPMS-off/soft-disabled
        // precedent).
        let plan = plan_with_output(
            false,
            Some((5, 5)),
            &[(
                ExternalElementClass::Cursor,
                ExternalElementDisposition::ExternalAssembly,
            )],
        );
        assert!(plan.is_safe());
        assert!(plan.blockers().is_empty());
        assert!(!plan.outputs[0].assembles(ExternalElementClass::Cursor));
    }

    #[test]
    fn blockers_accumulate_across_outputs_instead_of_last_output_winning() {
        let mut plan = plan_with_output(
            true,
            Some((5, 5)),
            &[
                (
                    ExternalElementClass::Cursor,
                    ExternalElementDisposition::ExternalAssembly,
                ),
                (
                    ExternalElementClass::LayerTop,
                    ExternalElementDisposition::ExternalAssembly,
                ),
            ],
        );
        let mut second = OutputExternalElementPlan::new("DP-1".to_owned(), true);
        second.observe(
            ExternalElementClass::LayerOverlay,
            ExternalElementDisposition::ExternalAssembly,
            "test",
        );
        plan.outputs.push(second);
        assert_eq!(
            plan.blockers(),
            [
                LinearTailBlocker::Cursor,
                LinearTailBlocker::TopLayerSurface,
                LinearTailBlocker::OverlayLayerSurface,
            ]
        );
    }

    #[test]
    fn invalid_pointer_state_fails_closed_on_a_participating_output() {
        // `observe_output_external_elements` maps an unresolvable pointer to
        // a blocking cursor class without a placement; assembly then draws
        // nothing while the blocker keeps the frame on exact-sRGB.
        let plan = plan_with_output(
            true,
            None,
            &[(
                ExternalElementClass::Cursor,
                ExternalElementDisposition::ExternalAssembly,
            )],
        );
        assert_eq!(plan.blockers(), [LinearTailBlocker::Cursor]);
        assert!(plan.outputs[0].assembles(ExternalElementClass::Cursor));
        assert_eq!(plan.outputs[0].cursor_position, None);
    }

    #[test]
    fn class_statuses_report_per_class_visibility_importability_and_outputs() {
        let mut plan = plan_with_output(
            true,
            Some((5, 5)),
            &[
                (
                    ExternalElementClass::Cursor,
                    ExternalElementDisposition::ExternalAssembly,
                ),
                (
                    ExternalElementClass::LayerTop,
                    ExternalElementDisposition::ImportBlocked,
                ),
            ],
        );
        let mut second = OutputExternalElementPlan::new("DP-1".to_owned(), true);
        second.observe(
            ExternalElementClass::LayerTop,
            ExternalElementDisposition::ExternalAssembly,
            "test",
        );
        plan.outputs.push(second);

        let statuses = plan.class_statuses();
        assert_eq!(statuses.len(), ExternalElementClass::ALL.len());

        let cursor = &statuses[0];
        assert_eq!(cursor.class, "cursor");
        assert!(cursor.visible && cursor.importable);
        assert_eq!(cursor.assembly, "kms_external");
        assert_eq!(cursor.blocker.as_deref(), Some("cursor"));
        assert_eq!(cursor.outputs, ["HDMI-A-1".to_owned()]);

        // One import-blocked output makes the whole class unimportable.
        let layer_top = statuses
            .iter()
            .find(|status| status.class == "top_layer_surface")
            .unwrap();
        assert!(layer_top.visible && !layer_top.importable);
        assert_eq!(layer_top.outputs.len(), 2);
        assert_eq!(layer_top.blocker.as_deref(), Some("top_layer_surface"));

        let lock = statuses
            .iter()
            .find(|status| status.class == "session_lock_surface")
            .unwrap();
        assert!(!lock.visible && !lock.importable);
        assert_eq!(lock.assembly, "none");
        assert_eq!(lock.blocker, None);
        assert!(lock.outputs.is_empty());
    }

    #[test]
    fn internalized_disposition_leaves_kms_assembly_and_stops_blocking() {
        use ExternalElementDisposition::*;
        // The internalized class is drawn by the compositor into the common
        // linear target: KMS must not assemble it again, it still produces
        // pixels (enter/leave and frame callbacks continue), and it no longer
        // forces the exact-sRGB fallback.
        assert!(!Internalized.assembles_externally());
        assert!(Internalized.produces_pixels());
        assert!(!Internalized.contributes_blocker());
        assert!(!Hidden.produces_pixels());
        assert!(ExternalAssembly.produces_pixels());
        assert!(ImportBlocked.produces_pixels());
    }

    #[test]
    fn apply_internalized_flips_only_external_assembly_entries() {
        let mut plan = plan_with_output(
            true,
            Some((5, 5)),
            &[
                (
                    ExternalElementClass::Cursor,
                    ExternalElementDisposition::ExternalAssembly,
                ),
                (
                    ExternalElementClass::DragIcon,
                    ExternalElementDisposition::ImportBlocked,
                ),
            ],
        );
        let mut second = OutputExternalElementPlan::new("DP-1".to_owned(), false);
        second.observe(
            ExternalElementClass::Cursor,
            ExternalElementDisposition::Hidden,
            "output_not_participating",
        );
        plan.outputs.push(second);

        let mut classes = [false; ExternalElementClass::ALL.len()];
        classes[ExternalElementClass::Cursor.index()] = true;
        classes[ExternalElementClass::DragIcon.index()] = true;
        plan.apply_internalized(&classes);

        assert_eq!(
            plan.outputs[0]
                .class(ExternalElementClass::Cursor)
                .disposition,
            ExternalElementDisposition::Internalized
        );
        // ImportBlocked is a hard blocker the adapter must never dissolve.
        assert_eq!(
            plan.outputs[0]
                .class(ExternalElementClass::DragIcon)
                .disposition,
            ExternalElementDisposition::ImportBlocked
        );
        // Hidden entries (here: non-participating output) never flip.
        assert_eq!(
            plan.outputs[1]
                .class(ExternalElementClass::Cursor)
                .disposition,
            ExternalElementDisposition::Hidden
        );
        assert!(!plan.outputs[1].shows(ExternalElementClass::Cursor));
        assert!(plan.outputs[0].shows(ExternalElementClass::Cursor));
        assert!(!plan.outputs[0].assembles(ExternalElementClass::Cursor));
    }

    #[test]
    fn internalized_class_unblocks_plan_and_reports_common_linear() {
        let mut plan = plan_with_output(
            true,
            Some((5, 5)),
            &[
                (
                    ExternalElementClass::Cursor,
                    ExternalElementDisposition::ExternalAssembly,
                ),
                (
                    ExternalElementClass::LayerTop,
                    ExternalElementDisposition::ExternalAssembly,
                ),
            ],
        );
        assert!(!plan.is_safe());
        assert_eq!(
            plan.blockers(),
            [
                LinearTailBlocker::Cursor,
                LinearTailBlocker::TopLayerSurface
            ]
        );

        let mut classes = [false; ExternalElementClass::ALL.len()];
        classes[ExternalElementClass::Cursor.index()] = true;
        classes[ExternalElementClass::LayerTop.index()] = true;
        plan.apply_internalized(&classes);

        assert!(plan.is_safe());
        assert!(plan.blockers().is_empty());
        let statuses = plan.class_statuses();
        let cursor = &statuses[0];
        assert!(cursor.visible && cursor.importable);
        assert_eq!(cursor.assembly, "common_linear");
        assert_eq!(cursor.blocker, None);
        assert_eq!(cursor.outputs, ["HDMI-A-1".to_owned()]);
        let layer_top = statuses
            .iter()
            .find(|status| status.class == "top_layer_surface")
            .unwrap();
        assert_eq!(layer_top.assembly, "common_linear");
        assert_eq!(layer_top.blocker, None);
    }

    #[test]
    fn staging_candidate_matrix_follows_dispositions() {
        let plan = plan_with_output(
            true,
            Some((5, 5)),
            &[
                (
                    ExternalElementClass::Cursor,
                    ExternalElementDisposition::ExternalAssembly,
                ),
                (
                    ExternalElementClass::DragIcon,
                    ExternalElementDisposition::Hidden,
                ),
            ],
        );
        assert!(class_is_staging_candidate(
            &plan,
            ExternalElementClass::Cursor
        ));
        assert!(!class_is_staging_candidate(
            &plan,
            ExternalElementClass::DragIcon
        ));
        assert!(!class_is_staging_candidate(
            &plan,
            ExternalElementClass::SessionLock
        ));

        // An import-blocked output anywhere disqualifies the whole class.
        let mut blocked = plan_with_output(
            true,
            Some((5, 5)),
            &[(
                ExternalElementClass::LayerTop,
                ExternalElementDisposition::ExternalAssembly,
            )],
        );
        let mut second = OutputExternalElementPlan::new("DP-1".to_owned(), true);
        second.observe(
            ExternalElementClass::LayerTop,
            ExternalElementDisposition::ImportBlocked,
            "test",
        );
        blocked.outputs.push(second);
        assert!(!class_is_staging_candidate(
            &blocked,
            ExternalElementClass::LayerTop
        ));

        // A class already internalized is not a staging candidate anymore.
        let mut internalized = plan_with_output(
            true,
            Some((5, 5)),
            &[(
                ExternalElementClass::Cursor,
                ExternalElementDisposition::Internalized,
            )],
        );
        assert!(!class_is_staging_candidate(
            &internalized,
            ExternalElementClass::Cursor
        ));
        internalized.outputs[0].observe(
            ExternalElementClass::Cursor,
            ExternalElementDisposition::ExternalAssembly,
            "test",
        );
        assert!(class_is_staging_candidate(
            &internalized,
            ExternalElementClass::Cursor
        ));
    }

    #[test]
    fn internalization_gate_rejects_lock_and_import_blocked() {
        use ExternalElementDisposition::*;
        // Only a visible cursor: internalizing it would make the frame safe.
        let cursor_only = plan_with_output(
            true,
            Some((5, 5)),
            &[(ExternalElementClass::Cursor, ExternalAssembly)],
        );
        assert!(cursor_only.internalization_could_make_safe());

        // Session lock is deliberately not migratable: the KMS shield path
        // stays the audited occlusion boundary.
        let locked = plan_with_output(
            true,
            Some((5, 5)),
            &[
                (ExternalElementClass::Cursor, ExternalAssembly),
                (ExternalElementClass::SessionLock, ExternalAssembly),
            ],
        );
        assert!(!locked.internalization_could_make_safe());
        assert!(!ExternalElementClass::SessionLock.supports_internalization());
        for class in [
            ExternalElementClass::Cursor,
            ExternalElementClass::DragIcon,
            ExternalElementClass::LayerTop,
            ExternalElementClass::LayerOverlay,
        ] {
            assert!(class.supports_internalization());
        }

        // Capture/readback requests are no longer frame blockers at all: they
        // read the compositor's independent encoded capture view, so the plan
        // has no capture input to gate (see the domain table in
        // compositor/tail_domain.rs).

        // An import-blocked tree must stay external even though it is
        // otherwise migratable.
        let import_blocked = plan_with_output(
            true,
            Some((5, 5)),
            &[(ExternalElementClass::DragIcon, ImportBlocked)],
        );
        assert!(!import_blocked.internalization_could_make_safe());
    }

    #[test]
    fn commit_staged_internalization_is_all_or_nothing_per_frame() {
        use ExternalElementDisposition::*;
        let plan = plan_with_output(
            true,
            Some((5, 5)),
            &[
                (ExternalElementClass::Cursor, ExternalAssembly),
                (ExternalElementClass::LayerOverlay, ExternalAssembly),
            ],
        );

        // Every visible class staged: the committed plan internalizes both
        // and the frame leaves the exact-sRGB fallback.
        let mut all = [false; ExternalElementClass::ALL.len()];
        all[ExternalElementClass::Cursor.index()] = true;
        all[ExternalElementClass::LayerOverlay.index()] = true;
        let (committed, ok) = commit_staged_internalization(&plan, &all);
        assert!(ok);
        assert!(committed.is_safe());
        assert!(committed.blockers().is_empty());

        // One class failed staging: its blocker survives, so the frame
        // reverts wholesale — the compositor must not draw the other class
        // either, and KMS keeps assembling both.
        let mut partial = [false; ExternalElementClass::ALL.len()];
        partial[ExternalElementClass::Cursor.index()] = true;
        let (committed, ok) = commit_staged_internalization(&plan, &partial);
        assert!(!ok);
        assert_eq!(committed, plan);
        assert!(!committed.is_safe());

        // Nothing to migrate on an already-safe plan: commit is a no-op.
        let safe = ExternalElementColorPlan::default();
        let (committed, ok) =
            commit_staged_internalization(&safe, &[false; ExternalElementClass::ALL.len()]);
        assert!(ok);
        assert_eq!(committed, safe);
    }

    #[test]
    fn internalized_frame_verdict_is_pinned_to_texture_identity() {
        let plan = plan_with_output(
            true,
            Some((5, 5)),
            &[(
                ExternalElementClass::Cursor,
                ExternalElementDisposition::ExternalAssembly,
            )],
        );
        let mut classes = [false; ExternalElementClass::ALL.len()];
        classes[ExternalElementClass::Cursor.index()] = true;
        let verdict = InternalizedExternalFrame::new(42, 7, classes);

        // Matching texture identity: the class flips to internalized.
        let mut matching = plan.clone();
        verdict.apply_to_plan(&mut matching, 42, 7);
        assert_eq!(
            matching.outputs[0]
                .class(ExternalElementClass::Cursor)
                .disposition,
            ExternalElementDisposition::Internalized
        );

        // A recreated or resized compositor produces a new texture identity;
        // the stale verdict must not dissolve the external assembly then.
        for (tex, generation) in [(43, 7), (42, 8)] {
            let mut stale = plan.clone();
            verdict.apply_to_plan(&mut stale, tex, generation);
            assert_eq!(stale, plan);
            assert!(stale.outputs[0].assembles(ExternalElementClass::Cursor));
        }
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
            external_elements: None,
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
    fn software_color_regions_carry_the_delivery_tone_map_plan() {
        use crate::backend::wayland_udev::color_pipeline::{
            OutputToneMapPlan, ToneMapPolicy, working_space_scale,
        };

        // Default (all-SDR) candidates select the pass-through policy on both
        // SDR and HDR output transfers — the SDR pixel-identity baseline.
        let regions = plan_software_color_regions(&[
            color_region_candidate((0, 0), TransferKind::Srgb),
            color_region_candidate((1920, 0), TransferKind::St2084Pq),
        ])
        .expect("supported layout");
        assert_eq!(regions[0].tone_map, OutputToneMapPlan::IDENTITY);
        assert_eq!(
            regions[1].tone_map,
            OutputToneMapPlan {
                policy: ToneMapPolicy::ReferenceWhite,
                source_peak_working: 1.0,
                target_peak_working: working_space_scale(TransferKind::St2084Pq),
            }
        );

        // HDR content visible on an SDR output selects Clip at reference white.
        let mut hdr_on_sdr = color_region_candidate((0, 0), TransferKind::Srgb);
        hdr_on_sdr.source_peak_working = working_space_scale(TransferKind::St2084Pq);
        let regions = plan_software_color_regions(&[hdr_on_sdr]).expect("single supported output");
        assert_eq!(
            regions[0].tone_map,
            OutputToneMapPlan {
                policy: ToneMapPolicy::Clip,
                source_peak_working: working_space_scale(TransferKind::St2084Pq),
                target_peak_working: 1.0,
            }
        );
    }

    #[test]
    fn software_color_regions_reject_overlapping_regions_with_different_peaks() {
        // Mirrored outputs share one framebuffer area: one scissor rectangle
        // can carry only one tone-map plan, so conflicting plans must reject
        // the whole plan rather than pick one silently.
        let base = color_region_candidate((0, 0), TransferKind::Srgb);
        let mut hdr_content = base;
        hdr_content.source_peak_working = 4.0;
        assert!(plan_software_color_regions(&[base, hdr_content]).is_none());

        // Identical peaks (true mirror of the same content) still pass.
        assert!(plan_software_color_regions(&[base, base]).is_some());
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

    // ---- HDR P1-6: controlled atomic color delivery + 10-bit chain ----

    use smithay::backend::allocator::Fourcc;

    fn full_chain_handles() -> ScanoutColorPropertyHandles {
        ScanoutColorPropertyHandles {
            degamma_lut: Some(10),
            ctm: Some(11),
            gamma_lut: Some(12),
            colorspace: Some(20),
            colorspace_bt2020_rgb: Some(1),
            hdr_output_metadata: Some(21),
        }
    }

    #[test]
    fn atomic_color_request_covers_the_whole_chain_in_stable_order() {
        let plan = AtomicColorOutputPlan {
            crtc: 100,
            connector: 200,
            handles: full_chain_handles(),
            target: ScanoutColorTarget {
                degamma_lut: Some(0),
                ctm: Some(501),
                gamma_lut: Some(502),
                colorspace: Some(1),
                hdr_output_metadata: Some(503),
            },
        };
        let assignments = build_atomic_color_request(&[plan]).expect("complete chain builds");
        assert_eq!(
            assignments,
            vec![
                // CRTC stages first: DEGAMMA stays pinned neutral, then the
                // paired CTM + OETF LUT, then the connector signalling.
                AtomicColorAssignment {
                    object: 100,
                    property: 10,
                    value: 0
                },
                AtomicColorAssignment {
                    object: 100,
                    property: 11,
                    value: 501
                },
                AtomicColorAssignment {
                    object: 100,
                    property: 12,
                    value: 502
                },
                AtomicColorAssignment {
                    object: 200,
                    property: 20,
                    value: 1
                },
                AtomicColorAssignment {
                    object: 200,
                    property: 21,
                    value: 503
                },
            ]
        );
    }

    #[test]
    fn atomic_color_request_spans_every_output_in_one_request() {
        let handles = full_chain_handles();
        let target = ScanoutColorTarget {
            degamma_lut: Some(0),
            ctm: Some(0),
            gamma_lut: Some(601),
            colorspace: None,
            hdr_output_metadata: None,
        };
        let plans = [
            AtomicColorOutputPlan {
                crtc: 100,
                connector: 200,
                handles,
                target,
            },
            AtomicColorOutputPlan {
                crtc: 101,
                connector: 201,
                handles,
                target: ScanoutColorTarget {
                    gamma_lut: Some(602),
                    ..target
                },
            },
        ];
        let assignments = build_atomic_color_request(&plans).expect("multi-output request builds");
        // One request object carries both CRTCs' stage transitions; the kernel
        // applies all of them or none.
        let objects: Vec<u32> = assignments.iter().map(|a| a.object).collect();
        assert_eq!(objects, vec![100, 100, 100, 101, 101, 101]);
        assert_eq!(assignments[2].value, 601);
        assert_eq!(assignments[5].value, 602);
    }

    #[test]
    fn atomic_color_request_refuses_to_install_a_stage_the_hardware_lacks() {
        let plan = AtomicColorOutputPlan {
            crtc: 100,
            connector: 200,
            handles: ScanoutColorPropertyHandles {
                gamma_lut: Some(12),
                ..ScanoutColorPropertyHandles::default()
            },
            target: ScanoutColorTarget {
                ctm: Some(42),
                ..ScanoutColorTarget::default()
            },
        };
        let error = build_atomic_color_request(&[plan]).expect_err("install without property");
        assert!(error.contains("CTM"), "unexpected error: {error}");
    }

    #[test]
    fn atomic_color_request_treats_clearing_an_absent_stage_as_neutral_noop() {
        let plan = AtomicColorOutputPlan {
            crtc: 100,
            connector: 200,
            handles: ScanoutColorPropertyHandles::default(),
            target: ScanoutColorTarget {
                degamma_lut: Some(0),
                ctm: Some(0),
                gamma_lut: Some(0),
                colorspace: Some(0),
                hdr_output_metadata: Some(0),
            },
        };
        assert!(
            build_atomic_color_request(&[plan])
                .expect("clears never fail")
                .is_empty(),
            "clearing an unexposed stage must not emit an assignment"
        );
    }

    #[test]
    fn atomic_color_request_leaves_untouched_stages_out() {
        let plan = AtomicColorOutputPlan {
            crtc: 100,
            connector: 200,
            handles: full_chain_handles(),
            target: ScanoutColorTarget {
                hdr_output_metadata: Some(77),
                ..ScanoutColorTarget::default()
            },
        };
        let assignments = build_atomic_color_request(&[plan]).expect("signalling-only request");
        assert_eq!(
            assignments,
            vec![AtomicColorAssignment {
                object: 200,
                property: 21,
                value: 77
            }]
        );
    }

    #[test]
    fn scanout_format_bit_depth_classifies_known_layouts() {
        assert_eq!(scanout_format_channel_bits(Fourcc::Argb8888), Some(8));
        assert_eq!(scanout_format_channel_bits(Fourcc::Xbgr8888), Some(8));
        assert_eq!(scanout_format_channel_bits(Fourcc::Argb2101010), Some(10));
        assert_eq!(scanout_format_channel_bits(Fourcc::Xrgb2101010), Some(10));
        assert_eq!(scanout_format_channel_bits(Fourcc::Xbgr16161616f), Some(16));
        // YUV and unknown layouts carry no usable per-channel RGB depth and
        // fail HDR validation closed.
        assert_eq!(scanout_format_channel_bits(Fourcc::Nv12), None);
    }

    #[test]
    fn hdr_scanout_chain_accepts_only_a_complete_10bit_path() {
        let formats = [Fourcc::Xrgb2101010 as u32, Fourcc::Xrgb8888 as u32];
        assert_eq!(
            hdr_scanout_chain_gap(true, Fourcc::Xrgb2101010, &formats, &full_chain_handles()),
            None,
            "10-bit framebuffer + plane + CRTC stages + connector signalling"
        );
        assert_eq!(
            hdr_scanout_chain_gap(
                true,
                Fourcc::Xrgb16161616f,
                &[Fourcc::Xrgb16161616f as u32],
                &full_chain_handles()
            ),
            None,
            "16-bit float scanout also satisfies the bit-depth gate"
        );
    }

    #[test]
    fn hdr_scanout_chain_reports_each_gap_fail_closed() {
        let formats = [Fourcc::Xrgb2101010 as u32];
        let handles = full_chain_handles();
        let gap = |same_device,
                   fourcc: Fourcc,
                   formats: &[u32],
                   handles: &ScanoutColorPropertyHandles| {
            hdr_scanout_chain_gap(same_device, fourcc, formats, handles)
        };

        assert_eq!(
            gap(false, Fourcc::Xrgb2101010, &formats, &handles),
            Some(HdrScanoutChainGap::CrossDevice)
        );
        assert_eq!(
            gap(true, Fourcc::Argb8888, &formats, &handles),
            Some(HdrScanoutChainGap::FramebufferBitDepth)
        );
        assert_eq!(
            gap(true, Fourcc::Nv12, &formats, &handles),
            Some(HdrScanoutChainGap::FramebufferBitDepth),
            "unknown depth must not pass the 10-bit gate"
        );
        assert_eq!(
            gap(true, Fourcc::Abgr2101010, &formats, &handles),
            Some(HdrScanoutChainGap::PlaneFormatUnsupported),
            "the exact swapchain format must be scanout-capable"
        );
        assert_eq!(
            gap(
                true,
                Fourcc::Xrgb2101010,
                &formats,
                &ScanoutColorPropertyHandles {
                    gamma_lut: None,
                    ..handles
                }
            ),
            Some(HdrScanoutChainGap::CrtcColorStagesMissing)
        );
        assert_eq!(
            gap(
                true,
                Fourcc::Xrgb2101010,
                &formats,
                &ScanoutColorPropertyHandles {
                    ctm: None,
                    ..handles
                }
            ),
            Some(HdrScanoutChainGap::CrtcColorStagesMissing)
        );
        assert_eq!(
            gap(
                true,
                Fourcc::Xrgb2101010,
                &formats,
                &ScanoutColorPropertyHandles {
                    colorspace_bt2020_rgb: None,
                    ..handles
                }
            ),
            Some(HdrScanoutChainGap::ConnectorColorspaceMissing),
            "a Colorspace property without BT2020_RGB cannot signal HDR primaries"
        );
        assert_eq!(
            gap(
                true,
                Fourcc::Xrgb2101010,
                &formats,
                &ScanoutColorPropertyHandles {
                    hdr_output_metadata: None,
                    ..handles
                }
            ),
            Some(HdrScanoutChainGap::ConnectorHdrMetadataMissing)
        );
    }

    #[test]
    fn hdr_scanout_chain_gap_precedence_is_stable() {
        let handles = ScanoutColorPropertyHandles::default();
        // Everything broken: the cross-device gap dominates.
        assert_eq!(
            hdr_scanout_chain_gap(false, Fourcc::Argb8888, &[], &handles),
            Some(HdrScanoutChainGap::CrossDevice)
        );
        // Only the connector side broken: Colorspace before HDR metadata.
        assert_eq!(
            hdr_scanout_chain_gap(
                true,
                Fourcc::Xrgb2101010,
                &[Fourcc::Xrgb2101010 as u32],
                &handles
            ),
            Some(HdrScanoutChainGap::CrtcColorStagesMissing)
        );
    }

    #[test]
    fn scanout_color_goal_matches_only_identical_tracked_state() {
        let goal = OutputScanoutColorGoal {
            gamma_lut: Some(TransferKind::St2084Pq),
            ctm: Some(IDENTITY_CTM),
        };
        assert!(scanout_color_goal_matches(
            Some((9, TransferKind::St2084Pq)),
            Some(10),
            &goal
        ));
        assert!(
            !scanout_color_goal_matches(Some((9, TransferKind::Hlg)), Some(10), &goal),
            "a different installed TF must re-enter the atomic request"
        );
        assert!(!scanout_color_goal_matches(None, Some(10), &goal));
        assert!(!scanout_color_goal_matches(
            Some((9, TransferKind::St2084Pq)),
            None,
            &goal
        ));

        let clear = OutputScanoutColorGoal::CLEAR;
        assert!(scanout_color_goal_matches(None, None, &clear));
        assert!(!scanout_color_goal_matches(
            Some((9, TransferKind::Srgb)),
            None,
            &clear
        ));
        assert!(!scanout_color_goal_matches(None, Some(10), &clear));
    }
}
