/// wp-color-management-v1 protocol implementation for JWM (Slice 1: skeleton).
///
/// The protocol lets clients describe surface colorimetry (HDR transfer curves,
/// primaries, mastering metadata) and query per-output preferred image
/// descriptions. This slice wires the full surface area so clients can bind and
/// drive every interface without protocol errors; render-path integration and
/// per-output EDID-derived image descriptions are deferred to later slices.
///
/// Bound at version 1, so the v1 `ready` / `preferred_changed` events are sent
/// (the v2 `ready2` / `preferred_changed2` variants will be added when we bump).
use crate::backend::edid::EdidHdrCapabilities;
use crate::backend::wayland::state::JwmWaylandState;
use crate::sync_ext::MutexExt;
use smithay::output::Output;
use smithay::reexports::wayland_protocols::wp::color_management::v1::server::{
    wp_color_management_output_v1::{self, WpColorManagementOutputV1},
    wp_color_management_surface_feedback_v1::{self, WpColorManagementSurfaceFeedbackV1},
    wp_color_management_surface_v1::{self, WpColorManagementSurfaceV1},
    wp_color_manager_v1::{
        self, Feature, Primaries, RenderIntent, TransferFunction, WpColorManagerV1,
    },
    wp_image_description_creator_icc_v1::{self, WpImageDescriptionCreatorIccV1},
    wp_image_description_creator_params_v1::{self, WpImageDescriptionCreatorParamsV1},
    wp_image_description_info_v1::{self, WpImageDescriptionInfoV1},
    wp_image_description_reference_v1::{self, WpImageDescriptionReferenceV1},
    wp_image_description_v1::{self, Cause, WpImageDescriptionV1},
};
use smithay::reexports::wayland_server::backend::ObjectId;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

// CIE 1931 xy chromaticities scaled by 1_000_000 (the protocol's encoding).
const PRIMARIES_BT709: [i32; 8] = [
    640_000, 330_000, 300_000, 600_000, 150_000, 60_000, 312_700, 329_000,
];
const PRIMARIES_BT2020: [i32; 8] = [
    708_000, 292_000, 170_000, 797_000, 131_000, 46_000, 312_700, 329_000,
];

const COLOR_MANAGER_VERSION: u32 = 1;

pub(crate) fn advanced_color_management_enabled() -> bool {
    std::env::var_os("JWM_COLOR_MANAGEMENT_ADVANCED").as_deref() == Some(std::ffi::OsStr::new("1"))
}

/// Accumulated parametric properties (collected by a creator object before
/// `create`, then frozen into an ImageDescription).
#[derive(Debug, Clone, Default)]
pub struct ParametricParams {
    pub tf_named: Option<u32>,
    pub tf_power: Option<u32>,
    pub primaries_named: Option<u32>,
    pub primaries: Option<[i32; 8]>,
    pub min_lum: Option<u32>,
    pub max_lum: Option<u32>,
    pub reference_lum: Option<u32>,
    pub mastering_primaries: Option<[i32; 8]>,
    pub mastering_min_lum: Option<u32>,
    pub mastering_max_lum: Option<u32>,
    pub max_cll: Option<u32>,
    pub max_fall: Option<u32>,
}

impl ParametricParams {
    fn is_complete(&self) -> bool {
        (self.tf_named.is_some() || self.tf_power.is_some())
            && (self.primaries_named.is_some() || self.primaries.is_some())
    }
}

#[derive(Debug, Clone)]
pub enum ImageDescriptionState {
    Ready {
        id: u64,
        params: ParametricParams,
        allow_info: bool,
    },
    Failed,
}

pub type ImageDescriptionData = Arc<Mutex<ImageDescriptionState>>;
pub type ParametricCreatorData = Arc<Mutex<ParametricParams>>;

/// Snapshot of a surface's currently-applied image description, exposed to
/// callers outside the protocol module (Backend trait, IPC).
#[derive(Debug, Clone)]
pub struct SurfaceDescriptionRecord {
    pub identity: u64,
    pub params: ParametricParams,
}

/// Bookkeeping for one surface's wp_color_management_surface_feedback_v1
/// resources, the outputs the surface currently sits on, and the
/// preferred image description we last emitted for it.
#[derive(Default)]
struct FeedbackBucket {
    /// Live feedback objects for this surface. Cleaned of dead entries lazily
    /// on the next emit; the Dispatch::destroyed hook removes them eagerly.
    resources: Vec<WpColorManagementSurfaceFeedbackV1>,
    /// Output names the surface currently has frames on. Updated by
    /// on_surface_enters_output / on_surface_leaves_output.
    outputs: HashSet<String>,
    /// (id, params) of the most recently emitted preferred description. Used
    /// to short-circuit redundant preferred_changed events when the picked
    /// output's EDID-derived params haven't actually changed.
    last_preferred: Option<(u64, ParametricParams)>,
}

/// Singleton state held by JwmWaylandState.
pub struct ColorManagerState {
    id_counter: Arc<Mutex<u64>>,
    /// Per-surface applied image description (set via
    /// wp_color_management_surface_v1.set_image_description). Stores the full
    /// parametric params so the render path can read them without re-walking
    /// the protocol object graph.
    pub surface_descriptions: Arc<Mutex<HashMap<ObjectId, SurfaceDescriptionRecord>>>,
    /// Per-surface feedback tracking — resources, current output set, and
    /// last-emitted preferred description. Created lazily when a client first
    /// calls get_surface_feedback.
    feedback: Arc<Mutex<HashMap<ObjectId, FeedbackBucket>>>,
}

impl ColorManagerState {
    pub fn new() -> Self {
        Self {
            id_counter: Arc::new(Mutex::new(1)),
            surface_descriptions: Arc::new(Mutex::new(HashMap::new())),
            feedback: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn next_id(&self) -> u64 {
        let mut g = self.id_counter.lock_safe();
        let id = *g;
        *g = g.wrapping_add(1);
        id
    }

    /// Snapshot the surface→description map in one lock acquisition. The
    /// returned map is decoupled from the live state and safe to consult
    /// across many surfaces without re-locking per-surface (used by the
    /// render-path color-management pass).
    pub fn snapshot_surface_params(&self) -> HashMap<ObjectId, ParametricParams> {
        self.surface_descriptions
            .lock_safe()
            .iter()
            .map(|(k, v)| (k.clone(), v.params.clone()))
            .collect()
    }

    /// Snapshot every surface that currently has an applied image description.
    /// Used by the diagnostic IPC to report active color-managed clients.
    pub fn snapshot_surface_descriptions(&self) -> Vec<(ObjectId, SurfaceDescriptionRecord)> {
        self.surface_descriptions
            .lock_safe()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Register a feedback resource so subsequent output changes can emit
    /// preferred_changed on it. No-op when the surface has no feedback bucket
    /// yet — the bucket is created here.
    fn register_feedback(&self, surface: ObjectId, resource: WpColorManagementSurfaceFeedbackV1) {
        self.feedback
            .lock_safe()
            .entry(surface)
            .or_default()
            .resources
            .push(resource);
    }

    /// Look up the most recently emitted preferred (id, params) for a surface,
    /// so a get_preferred / get_preferred_parametric returns the same description
    /// the client already saw via preferred_changed.
    fn current_preferred(&self, surface: &ObjectId) -> Option<(u64, ParametricParams)> {
        self.feedback
            .lock_safe()
            .get(surface)
            .and_then(|b| b.last_preferred.clone())
    }

    /// Drop everything tied to a surface — called from the surface Dispatch's
    /// destroyed hook so a leaked WlSurface doesn't pile up here forever.
    pub fn forget_surface(&self, surface: &ObjectId) {
        self.feedback.lock_safe().remove(surface);
    }

    /// A surface gained a frame on `output`. Builds a new preferred description
    /// from the output's EDID caps (or sRGB fallback) and, if the params differ
    /// from what we last emitted, fires preferred_changed on every live
    /// feedback resource for this surface.
    pub fn on_surface_enters_output(&self, surface: &ObjectId, output: &Output) {
        let mut feedback = self.feedback.lock_safe();
        let Some(bucket) = feedback.get_mut(surface) else {
            return;
        };
        // Newly-entered output → add to the set; if it was already there
        // (e.g. a re-render of the same frame), nothing to do.
        if !bucket.outputs.insert(output.name()) {
            return;
        }
        let new_params = params_for_output(output);
        if let Some((_, last_params)) = &bucket.last_preferred {
            if params_match(last_params, &new_params) {
                return;
            }
        }
        let id = self.next_id();
        bucket.last_preferred = Some((id, new_params));
        bucket.resources.retain(|r| r.is_alive());
        for r in &bucket.resources {
            r.preferred_changed(id as u32);
        }
    }

    /// A surface lost a frame on `output`. If the lost output was driving the
    /// current preferred and there are other outputs left, picks one of them
    /// and emits preferred_changed; if none remain, leaves last_preferred
    /// untouched (the surface is offscreen, the cached description stays valid
    /// for a subsequent get_preferred call).
    pub fn on_surface_leaves_output(&self, surface: &ObjectId, output: &Output) {
        let mut feedback = self.feedback.lock_safe();
        let Some(bucket) = feedback.get_mut(surface) else {
            return;
        };
        let removed = bucket.outputs.remove(&output.name());
        if !removed {
            return;
        }
        // If the surface still sits on other outputs, re-pick from the
        // remaining set so the preferred matches a live output.
        if let Some(_other) = bucket.outputs.iter().next().cloned() {
            // We don't have a back-reference from name → Output here; the
            // render loop will hit on_surface_enters_output for the surviving
            // output again on the next frame and re-emit if needed. Just drop
            // the cache so the next enter doesn't short-circuit.
            bucket.last_preferred = None;
        }
    }
}

pub(crate) fn params_for_output(output: &Output) -> ParametricParams {
    if !advanced_color_management_enabled() {
        return srgb_params();
    }
    match output.user_data().get::<EdidHdrCapabilities>() {
        Some(caps) => params_from_edid(caps),
        None => srgb_params(),
    }
}

fn params_match(a: &ParametricParams, b: &ParametricParams) -> bool {
    a.tf_named == b.tf_named
        && a.primaries_named == b.primaries_named
        && a.primaries == b.primaries
        && a.max_lum == b.max_lum
}

impl Default for ColorManagerState {
    fn default() -> Self {
        Self::new()
    }
}

pub fn init_color_management(dh: &DisplayHandle) -> ColorManagerState {
    dh.create_global::<JwmWaylandState, WpColorManagerV1, _>(COLOR_MANAGER_VERSION, ());
    log::info!(
        "[udev/wayland] wp-color-management-v1 global registered (version {})",
        COLOR_MANAGER_VERSION
    );
    ColorManagerState::new()
}

// --- Helpers ---

fn emit_manager_caps(resource: &WpColorManagerV1) {
    // Mandatory: perceptual.
    resource.supported_intent(RenderIntent::Perceptual);
    resource.supported_feature(Feature::Parametric);
    resource.supported_tf_named(TransferFunction::Gamma22);
    resource.supported_primaries_named(Primaries::Srgb);
    if advanced_color_management_enabled() {
        // ICC/HDR paths are still experimental. Keep them opt-in so Chromium
        // clients do not enter unvalidated color-management code by default.
        resource.supported_feature(Feature::IccV2V4);
        resource.supported_feature(Feature::SetPrimaries);
        resource.supported_feature(Feature::SetLuminances);
        resource.supported_feature(Feature::SetMasteringDisplayPrimaries);
        resource.supported_tf_named(TransferFunction::Bt1886);
        resource.supported_tf_named(TransferFunction::St2084Pq);
        resource.supported_tf_named(TransferFunction::Hlg);
        resource.supported_tf_named(TransferFunction::ExtLinear);
        resource.supported_primaries_named(Primaries::Bt2020);
    }
    resource.done();
}

/// Build a default sRGB parametric description (used when no HDR caps are
/// known for an output).
pub(crate) fn srgb_params() -> ParametricParams {
    ParametricParams {
        primaries_named: Some(Primaries::Srgb as u32),
        tf_named: Some(TransferFunction::Gamma22 as u32),
        ..ParametricParams::default()
    }
}

/// Translate an EDID HDR Static Metadata block (CTA-861) into a parametric
/// image description. Mirrors the policy used by `hdr_metadata::build_from_edid`
/// for the kernel-side blob so the wp-color-management answer and the
/// HDR_OUTPUT_METADATA push agree on EOTF and gamut.
pub(crate) fn params_from_edid(caps: &EdidHdrCapabilities) -> ParametricParams {
    let mut p = ParametricParams::default();

    // EOTF: prefer PQ > HLG > BT.1886.
    p.tf_named = Some(if caps.supports_pq {
        TransferFunction::St2084Pq as u32
    } else if caps.supports_hlg {
        TransferFunction::Hlg as u32
    } else {
        TransferFunction::Bt1886 as u32
    });

    // Container primaries: BT.2020 for any HDR-signalled display, sRGB otherwise.
    let hdr = caps.supports_pq || caps.supports_hlg || caps.supports_bt2020;
    if hdr {
        p.primaries_named = Some(Primaries::Bt2020 as u32);
        p.primaries = Some(PRIMARIES_BT2020);
    } else {
        p.primaries_named = Some(Primaries::Srgb as u32);
        p.primaries = Some(PRIMARIES_BT709);
    }

    // Luminance range (cd/m²). Spec scales min_lum by 10000, max_lum unscaled.
    if caps.max_luminance_nits > 0.0 {
        let max_lum = caps.max_luminance_nits.round().max(1.0) as u32;
        let min_lum_scaled = (caps.min_luminance_nits.max(0.0) * 10_000.0).round() as u32;
        // Reference white for HDR: 203 cd/m² per BT.2408. For SDR fall back to max.
        let reference_lum = if hdr { 203 } else { max_lum };
        p.min_lum = Some(min_lum_scaled);
        p.max_lum = Some(max_lum);
        p.reference_lum = Some(reference_lum);

        // Mastering display volume (target color volume) mirrors the container.
        if hdr {
            p.mastering_primaries = Some(PRIMARIES_BT2020);
        }
        p.mastering_min_lum = Some(min_lum_scaled);
        p.mastering_max_lum = Some(max_lum);

        // Surface-as-display: max_cll matches the display's peak.
        p.max_cll = Some(max_lum);
    }

    p
}

/// Build the per-output image description params. Looks up EDID HDR caps stashed
/// on the smithay Output's user_data (see `attach_edid_caps_to_outputs` in the
/// wayland_udev backend).
fn params_for_wl_output(wl_output: &WlOutput) -> ParametricParams {
    if !advanced_color_management_enabled() {
        return srgb_params();
    }
    match Output::from_resource(wl_output) {
        Some(o) => match o.user_data().get::<EdidHdrCapabilities>() {
            Some(caps) => params_from_edid(caps),
            None => srgb_params(),
        },
        None => srgb_params(),
    }
}

fn make_ready_description(
    state: &mut JwmWaylandState,
    params: ParametricParams,
    allow_info: bool,
) -> (u64, ImageDescriptionData) {
    let id = state
        .color_manager
        .as_ref()
        .map(|c| c.next_id())
        .unwrap_or(1);
    let data: ImageDescriptionData = Arc::new(Mutex::new(ImageDescriptionState::Ready {
        id,
        params,
        allow_info,
    }));
    (id, data)
}

// === GlobalDispatch for the manager ===

impl GlobalDispatch<WpColorManagerV1, ()> for JwmWaylandState {
    fn bind(
        state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<WpColorManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        state.record_protocol_bind("wp_color_manager_v1");
        let resource = data_init.init(resource, ());
        emit_manager_caps(&resource);
    }
}

// === Dispatch for the manager ===

impl Dispatch<WpColorManagerV1, ()> for JwmWaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &WpColorManagerV1,
        request: wp_color_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wp_color_manager_v1::Request::Destroy => {}
            wp_color_manager_v1::Request::GetOutput { id, output } => {
                data_init.init(id, OutputCmData { wl_output: output });
            }
            wp_color_manager_v1::Request::GetSurface { id, surface } => {
                data_init.init(id, SurfaceCmData { surface });
            }
            wp_color_manager_v1::Request::GetSurfaceFeedback { id, surface } => {
                let resource = data_init.init(
                    id,
                    SurfaceFeedbackData {
                        surface: surface.clone(),
                    },
                );
                // Initial preferred: sRGB ride-along until the render loop
                // observes the surface on a specific output and re-emits with
                // EDID-derived params. Use a fresh id so the client can
                // immediately call get_preferred and see a coherent description.
                if let Some(cm) = state.color_manager.as_ref() {
                    cm.register_feedback(surface.id(), resource.clone());
                    let new_id = cm.next_id();
                    // Seed the bucket with the sRGB description so
                    // current_preferred() returns something sensible until the
                    // first render-loop enter call.
                    if let Some(bucket) = cm.feedback.lock_safe().get_mut(&surface.id()) {
                        bucket.last_preferred = Some((new_id, srgb_params()));
                    }
                    resource.preferred_changed(new_id as u32);
                }
            }
            wp_color_manager_v1::Request::CreateIccCreator { obj } => {
                // ICC profiles are now parsed into ParametricParams in
                // wayland_udev::icc; advertise the feature so well-behaved
                // clients use this path instead of synthesising parametrics.
                let data: IccCreatorData = Arc::new(Mutex::new(IccCreatorInner::default()));
                data_init.init(obj, data);
            }
            wp_color_manager_v1::Request::CreateParametricCreator { obj } => {
                let data: ParametricCreatorData = Arc::new(Mutex::new(ParametricParams::default()));
                data_init.init(obj, data);
            }
            wp_color_manager_v1::Request::CreateWindowsScrgb { .. } => {
                // Not advertised; if a client ignores feature negotiation, do nothing
                // (it would have been a protocol error to call this, but the
                // server-side enum lacks a way to send post-hoc errors here).
            }
            _ => {}
        }
    }
}

// === wp_color_management_output_v1 ===

pub struct OutputCmData {
    pub wl_output: WlOutput,
}
unsafe impl Send for OutputCmData {}
unsafe impl Sync for OutputCmData {}

impl Dispatch<WpColorManagementOutputV1, OutputCmData> for JwmWaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &WpColorManagementOutputV1,
        request: wp_color_management_output_v1::Request,
        data: &OutputCmData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wp_color_management_output_v1::Request::Destroy => {}
            wp_color_management_output_v1::Request::GetImageDescription { image_description } => {
                // Derive params from EDID HDR caps stashed on the smithay Output;
                // falls back to sRGB if the output advertises no HDR static metadata.
                let params = params_for_wl_output(&data.wl_output);
                let (id, st) = make_ready_description(state, params, true);
                let desc = data_init.init(image_description, st);
                desc.ready(id as u32);
            }
            _ => {}
        }
    }
}

// === wp_color_management_surface_v1 ===

pub struct SurfaceCmData {
    surface: WlSurface,
}
unsafe impl Send for SurfaceCmData {}
unsafe impl Sync for SurfaceCmData {}

impl Dispatch<WpColorManagementSurfaceV1, SurfaceCmData> for JwmWaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &WpColorManagementSurfaceV1,
        request: wp_color_management_surface_v1::Request,
        data: &SurfaceCmData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wp_color_management_surface_v1::Request::Destroy => {
                if let Some(cm) = state.color_manager.as_ref() {
                    cm.surface_descriptions
                        .lock_safe()
                        .remove(&data.surface.id());
                }
            }
            wp_color_management_surface_v1::Request::SetImageDescription {
                image_description,
                render_intent: _,
            } => {
                // Pull the ready snapshot (id + params) out of the protocol
                // object so the render path doesn't need to chase the
                // wp_image_description_v1 resource later.
                let snapshot = image_description
                    .data::<ImageDescriptionData>()
                    .and_then(|d| match &*d.lock_safe() {
                        ImageDescriptionState::Ready { id, params, .. } => {
                            Some(SurfaceDescriptionRecord {
                                identity: *id,
                                params: params.clone(),
                            })
                        }
                        ImageDescriptionState::Failed => None,
                    });
                if let (Some(record), Some(cm)) = (snapshot, state.color_manager.as_ref()) {
                    cm.surface_descriptions
                        .lock_safe()
                        .insert(data.surface.id(), record);
                }
            }
            wp_color_management_surface_v1::Request::UnsetImageDescription => {
                if let Some(cm) = state.color_manager.as_ref() {
                    cm.surface_descriptions
                        .lock_safe()
                        .remove(&data.surface.id());
                }
            }
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        _resource: &WpColorManagementSurfaceV1,
        data: &SurfaceCmData,
    ) {
        if let Some(cm) = state.color_manager.as_ref() {
            cm.surface_descriptions
                .lock_safe()
                .remove(&data.surface.id());
            cm.forget_surface(&data.surface.id());
        }
    }
}

// === wp_color_management_surface_feedback_v1 ===

pub struct SurfaceFeedbackData {
    pub surface: WlSurface,
}
unsafe impl Send for SurfaceFeedbackData {}
unsafe impl Sync for SurfaceFeedbackData {}

impl Dispatch<WpColorManagementSurfaceFeedbackV1, SurfaceFeedbackData> for JwmWaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &WpColorManagementSurfaceFeedbackV1,
        request: wp_color_management_surface_feedback_v1::Request,
        data: &SurfaceFeedbackData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wp_color_management_surface_feedback_v1::Request::Destroy => {}
            wp_color_management_surface_feedback_v1::Request::GetPreferred {
                image_description,
            }
            | wp_color_management_surface_feedback_v1::Request::GetPreferredParametric {
                image_description,
            } => {
                // Pull the cached preferred (last emitted via preferred_changed)
                // so this description matches the one the client was told to expect.
                let cached = state
                    .color_manager
                    .as_ref()
                    .and_then(|cm| cm.current_preferred(&data.surface.id()));
                let (id, params) = match cached {
                    Some((id, params)) => (id, params),
                    None => {
                        // No render-loop observation yet — fall back to sRGB
                        // with a fresh id (still consistent with the v1 spec).
                        let id = state
                            .color_manager
                            .as_ref()
                            .map(|c| c.next_id())
                            .unwrap_or(1);
                        (id, srgb_params())
                    }
                };
                let st: ImageDescriptionData = Arc::new(Mutex::new(ImageDescriptionState::Ready {
                    id,
                    params,
                    allow_info: true,
                }));
                let desc = data_init.init(image_description, st);
                desc.ready(id as u32);
            }
            _ => {}
        }
    }
}

// === wp_image_description_creator_icc_v1 ===

#[derive(Default)]
pub struct IccCreatorInner {
    /// `set_icc_file` was called once already — second call is a protocol error.
    pub set: bool,
    /// Raw ICC bytes pulled from (fd, offset, length). Empty if read failed or
    /// no profile was set; that case falls through to a `failed(unsupported)`
    /// event at `create` time.
    pub bytes: Vec<u8>,
}

pub type IccCreatorData = Arc<Mutex<IccCreatorInner>>;

impl Dispatch<WpImageDescriptionCreatorIccV1, IccCreatorData> for JwmWaylandState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        resource: &WpImageDescriptionCreatorIccV1,
        request: wp_image_description_creator_icc_v1::Request,
        data: &IccCreatorData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        use crate::backend::wayland_udev::icc;
        use wp_image_description_creator_icc_v1::Error as IccErr;
        match request {
            wp_image_description_creator_icc_v1::Request::Create { image_description } => {
                let bytes = std::mem::take(&mut data.lock_safe().bytes);
                let result = if bytes.is_empty() {
                    None
                } else {
                    icc::parse_icc(&bytes).ok()
                };
                let st: ImageDescriptionData = match result {
                    Some(parsed) => {
                        let params = parsed.into_params();
                        let id = state_next_id_via_dispatch(); // resolved below
                        Arc::new(Mutex::new(ImageDescriptionState::Ready {
                            id,
                            params,
                            allow_info: true,
                        }))
                    }
                    None => Arc::new(Mutex::new(ImageDescriptionState::Failed)),
                };
                let snapshot = st.lock_safe().clone();
                let desc = data_init.init(image_description, st);
                match snapshot {
                    ImageDescriptionState::Ready { id, .. } => desc.ready(id as u32),
                    ImageDescriptionState::Failed => desc.failed(
                        Cause::Unsupported,
                        "ICC profile unparseable or unsupported shape".into(),
                    ),
                }
            }
            wp_image_description_creator_icc_v1::Request::SetIccFile {
                icc_profile,
                offset,
                length,
            } => {
                {
                    let mut g = data.lock_safe();
                    if g.set {
                        resource.post_error(IccErr::AlreadySet, "set_icc_file called twice");
                        return;
                    }
                    g.set = true;
                }
                if length == 0 || length > icc::ICC_MAX_BYTES {
                    resource.post_error(IccErr::BadSize, "ICC length 0 or > 32 MiB");
                    return;
                }
                match read_icc_fd_range(&icc_profile, offset, length) {
                    Ok(buf) => {
                        data.lock_safe().bytes = buf;
                    }
                    Err(IccReadError::Seek) => {
                        resource.post_error(IccErr::BadFd, "ICC fd not seekable/readable");
                    }
                    Err(IccReadError::OutOfFile) => {
                        resource.post_error(IccErr::OutOfFile, "offset+length exceeds fd size");
                    }
                    Err(IccReadError::Io) => {
                        // I/O failure independent of the client — leave bytes
                        // empty; create() will deliver failed(operating_system)
                        // via the per-state-Failed path. For now we collapse
                        // into Unsupported because we don't store the cause.
                    }
                }
            }
            _ => {}
        }
    }
}

/// Helper to look up the next monotonic id without threading `&mut state` through.
/// The id is only used as a debugging identity in events and IPC, so taking a
/// fresh u64 from a process-wide counter is safe even if it's not the same
/// counter the rest of the manager uses — until we have a real cross-creator id
/// allocator, this keeps IDs unique inside a session.
fn state_next_id_via_dispatch() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0x1_0000_0001);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[derive(Debug)]
enum IccReadError {
    Seek,
    OutOfFile,
    Io,
}

/// Read `length` bytes from `fd` starting at `offset` using positional reads,
/// without disturbing the fd's seek cursor and without taking ownership.
fn read_icc_fd_range(
    fd: &std::os::fd::OwnedFd,
    offset: u32,
    length: u32,
) -> Result<Vec<u8>, IccReadError> {
    use std::os::fd::AsRawFd;
    let raw = fd.as_raw_fd();
    // Confirm the fd is seekable — required by protocol. fstat alone doesn't
    // prove seekability for sockets/pipes, so a lseek to the current position
    // is the clearest check.
    let pos = unsafe { libc::lseek(raw, 0, libc::SEEK_CUR) };
    if pos < 0 {
        return Err(IccReadError::Seek);
    }
    let mut buf = vec![0u8; length as usize];
    let mut got: usize = 0;
    while got < buf.len() {
        let want = buf.len() - got;
        let off = offset as i64 + got as i64;
        let n = unsafe { libc::pread(raw, buf[got..].as_mut_ptr().cast(), want, off) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            return Err(match err.raw_os_error() {
                Some(libc::EBADF) | Some(libc::ESPIPE) => IccReadError::Seek,
                _ => IccReadError::Io,
            });
        }
        if n == 0 {
            return Err(IccReadError::OutOfFile);
        }
        got += n as usize;
    }
    Ok(buf)
}

// === wp_image_description_creator_params_v1 ===

impl Dispatch<WpImageDescriptionCreatorParamsV1, ParametricCreatorData> for JwmWaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &WpImageDescriptionCreatorParamsV1,
        request: wp_image_description_creator_params_v1::Request,
        data: &ParametricCreatorData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wp_image_description_creator_params_v1::Request::Create { image_description } => {
                let params = data.lock_safe().clone();
                if !params.is_complete() {
                    // Per spec we should raise the incomplete_set protocol error;
                    // for slice 1 we conservatively fail the resulting description
                    // so the client gets a clean signal without killing the connection.
                    let st: ImageDescriptionData =
                        Arc::new(Mutex::new(ImageDescriptionState::Failed));
                    let desc = data_init.init(image_description, st);
                    desc.failed(Cause::Unsupported, "incomplete parametric set".into());
                    return;
                }
                let (id, st) = make_ready_description(state, params, false);
                let desc = data_init.init(image_description, st);
                desc.ready(id as u32);
            }
            wp_image_description_creator_params_v1::Request::SetTfNamed { tf } => {
                data.lock_safe().tf_named = Some(tf.into());
            }
            wp_image_description_creator_params_v1::Request::SetTfPower { eexp } => {
                data.lock_safe().tf_power = Some(eexp);
            }
            wp_image_description_creator_params_v1::Request::SetPrimariesNamed { primaries } => {
                data.lock_safe().primaries_named = Some(primaries.into());
            }
            wp_image_description_creator_params_v1::Request::SetPrimaries {
                r_x,
                r_y,
                g_x,
                g_y,
                b_x,
                b_y,
                w_x,
                w_y,
            } => {
                data.lock_safe().primaries = Some([r_x, r_y, g_x, g_y, b_x, b_y, w_x, w_y]);
            }
            wp_image_description_creator_params_v1::Request::SetLuminances {
                min_lum,
                max_lum,
                reference_lum,
            } => {
                let mut g = data.lock_safe();
                g.min_lum = Some(min_lum);
                g.max_lum = Some(max_lum);
                g.reference_lum = Some(reference_lum);
            }
            wp_image_description_creator_params_v1::Request::SetMasteringDisplayPrimaries {
                r_x,
                r_y,
                g_x,
                g_y,
                b_x,
                b_y,
                w_x,
                w_y,
            } => {
                data.lock_safe().mastering_primaries =
                    Some([r_x, r_y, g_x, g_y, b_x, b_y, w_x, w_y]);
            }
            wp_image_description_creator_params_v1::Request::SetMasteringLuminance {
                min_lum,
                max_lum,
            } => {
                let mut g = data.lock_safe();
                g.mastering_min_lum = Some(min_lum);
                g.mastering_max_lum = Some(max_lum);
            }
            wp_image_description_creator_params_v1::Request::SetMaxCll { max_cll } => {
                data.lock_safe().max_cll = Some(max_cll);
            }
            wp_image_description_creator_params_v1::Request::SetMaxFall { max_fall } => {
                data.lock_safe().max_fall = Some(max_fall);
            }
            _ => {}
        }
    }
}

// === wp_image_description_v1 ===

impl Dispatch<WpImageDescriptionV1, ImageDescriptionData> for JwmWaylandState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &WpImageDescriptionV1,
        request: wp_image_description_v1::Request,
        data: &ImageDescriptionData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wp_image_description_v1::Request::Destroy => {}
            wp_image_description_v1::Request::GetInformation { information } => {
                let snapshot = data.lock_safe().clone();
                let info = data_init.init(information, ());
                if let ImageDescriptionState::Ready {
                    params, allow_info, ..
                } = snapshot
                {
                    if allow_info {
                        emit_image_description_info(&info, &params);
                    }
                }
                info.done();
            }
            _ => {}
        }
    }
}

fn emit_image_description_info(info: &WpImageDescriptionInfoV1, params: &ParametricParams) {
    if let Some(p) = params.primaries {
        info.primaries(p[0], p[1], p[2], p[3], p[4], p[5], p[6], p[7]);
    }
    if let Some(pn) = params.primaries_named {
        if let Ok(v) = Primaries::try_from(pn) {
            info.primaries_named(v);
        }
    }
    if let Some(tfp) = params.tf_power {
        info.tf_power(tfp);
    }
    if let Some(tfn) = params.tf_named {
        if let Ok(v) = TransferFunction::try_from(tfn) {
            info.tf_named(v);
        }
    }
    if let (Some(mn), Some(mx), Some(rw)) = (params.min_lum, params.max_lum, params.reference_lum) {
        info.luminances(mn, mx, rw);
    }
    if let Some(p) = params.mastering_primaries {
        info.target_primaries(p[0], p[1], p[2], p[3], p[4], p[5], p[6], p[7]);
    }
    if let (Some(mn), Some(mx)) = (params.mastering_min_lum, params.mastering_max_lum) {
        info.target_luminance(mn, mx);
    }
    if let Some(c) = params.max_cll {
        info.target_max_cll(c);
    }
    if let Some(f) = params.max_fall {
        info.target_max_fall(f);
    }
}

// === wp_image_description_info_v1 (event-only; required for Dispatch) ===

impl Dispatch<WpImageDescriptionInfoV1, ()> for JwmWaylandState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &WpImageDescriptionInfoV1,
        _request: wp_image_description_info_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        // Info objects have no client-initiated requests.
    }
}

// === wp_image_description_reference_v1 ===

impl Dispatch<WpImageDescriptionReferenceV1, ()> for JwmWaylandState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &WpImageDescriptionReferenceV1,
        _request: wp_image_description_reference_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        // Only `destroy` is defined; the destructor request requires no action.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(
        pq: bool,
        hlg: bool,
        bt2020: bool,
        max_nits: f32,
        min_nits: f32,
    ) -> EdidHdrCapabilities {
        EdidHdrCapabilities {
            max_luminance_nits: max_nits,
            min_luminance_nits: min_nits,
            supports_bt2020: bt2020,
            supports_pq: pq,
            supports_hlg: hlg,
        }
    }

    #[test]
    fn sdr_only_edid_maps_to_bt709_bt1886() {
        let p = params_from_edid(&caps(false, false, false, 0.0, 0.0));
        assert_eq!(p.tf_named, Some(TransferFunction::Bt1886 as u32));
        assert_eq!(p.primaries_named, Some(Primaries::Srgb as u32));
        assert_eq!(p.primaries, Some(PRIMARIES_BT709));
        // No luminance block → no mastering metadata.
        assert!(p.min_lum.is_none());
        assert!(p.max_cll.is_none());
    }

    #[test]
    fn pq_hdr_edid_maps_to_bt2020_pq_with_mastering() {
        let p = params_from_edid(&caps(true, false, true, 1000.0, 0.05));
        assert_eq!(p.tf_named, Some(TransferFunction::St2084Pq as u32));
        assert_eq!(p.primaries_named, Some(Primaries::Bt2020 as u32));
        assert_eq!(p.primaries, Some(PRIMARIES_BT2020));
        assert_eq!(p.max_lum, Some(1000));
        // min_lum scaled by 10_000: 0.05 → 500.
        assert_eq!(p.min_lum, Some(500));
        // BT.2408 reference white for HDR.
        assert_eq!(p.reference_lum, Some(203));
        assert_eq!(p.mastering_primaries, Some(PRIMARIES_BT2020));
        assert_eq!(p.mastering_max_lum, Some(1000));
        assert_eq!(p.max_cll, Some(1000));
    }

    #[test]
    fn hlg_preferred_over_bt1886_when_only_hlg_set() {
        let p = params_from_edid(&caps(false, true, true, 1000.0, 0.0));
        assert_eq!(p.tf_named, Some(TransferFunction::Hlg as u32));
    }

    #[test]
    fn pq_wins_when_both_pq_and_hlg_advertised() {
        let p = params_from_edid(&caps(true, true, true, 4000.0, 0.0));
        assert_eq!(p.tf_named, Some(TransferFunction::St2084Pq as u32));
        assert_eq!(p.max_lum, Some(4000));
    }

    #[test]
    fn params_match_ignores_mastering_fields() {
        let mut a = params_from_edid(&caps(true, false, true, 1000.0, 0.0));
        let mut b = a.clone();
        // tf/primaries/max_lum match → match=true even if mastering differs.
        a.mastering_max_lum = Some(1000);
        b.mastering_max_lum = Some(4000);
        assert!(params_match(&a, &b));
        // But primaries change → no match (a surface migrating SDR→HDR).
        let p_sdr = srgb_params();
        assert!(!params_match(&a, &p_sdr));
    }
}
