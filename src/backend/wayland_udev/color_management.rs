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

use crate::backend::wayland::state::JwmWaylandState;
use crate::sync_ext::MutexExt;
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
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const COLOR_MANAGER_VERSION: u32 = 1;

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

/// Singleton state held by JwmWaylandState.
pub struct ColorManagerState {
    id_counter: Arc<Mutex<u64>>,
    /// Per-surface applied image description id (set via
    /// wp_color_management_surface_v1.set_image_description). Slice 1 stores
    /// only the id; the render path is not yet integrated.
    pub surface_descriptions: Arc<Mutex<HashMap<ObjectId, u64>>>,
}

impl ColorManagerState {
    pub fn new() -> Self {
        Self {
            id_counter: Arc::new(Mutex::new(1)),
            surface_descriptions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn next_id(&self) -> u64 {
        let mut g = self.id_counter.lock_safe();
        let id = *g;
        *g = g.wrapping_add(1);
        id
    }
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
    // Parametric path is implemented; ICC is not (create→failed).
    resource.supported_feature(Feature::Parametric);
    resource.supported_feature(Feature::SetPrimaries);
    resource.supported_feature(Feature::SetLuminances);
    resource.supported_feature(Feature::SetMasteringDisplayPrimaries);
    // Common transfer functions a client might pick.
    resource.supported_tf_named(TransferFunction::Bt1886);
    resource.supported_tf_named(TransferFunction::Gamma22);
    resource.supported_tf_named(TransferFunction::St2084Pq);
    resource.supported_tf_named(TransferFunction::Hlg);
    resource.supported_tf_named(TransferFunction::ExtLinear);
    // Common primaries.
    resource.supported_primaries_named(Primaries::Srgb);
    resource.supported_primaries_named(Primaries::Bt2020);
    resource.done();
}

/// Build a default sRGB parametric description (used as the placeholder
/// per-output image description in slice 1).
fn srgb_params() -> ParametricParams {
    ParametricParams {
        primaries_named: Some(Primaries::Srgb as u32),
        tf_named: Some(TransferFunction::Gamma22 as u32),
        ..ParametricParams::default()
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
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<WpColorManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
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
                data_init.init(id, OutputCmData { _output_id: output.id() });
            }
            wp_color_manager_v1::Request::GetSurface { id, surface } => {
                data_init.init(id, SurfaceCmData { surface });
            }
            wp_color_manager_v1::Request::GetSurfaceFeedback { id, surface } => {
                let resource =
                    data_init.init(id, SurfaceFeedbackData { _surface: surface.clone() });
                // Emit initial preferred-changed using a fresh sRGB id so the
                // client knows where to start. Future slices will recompute
                // per output the surface is shown on.
                let id = state
                    .color_manager
                    .as_ref()
                    .map(|c| c.next_id())
                    .unwrap_or(1);
                resource.preferred_changed(id as u32);
            }
            wp_color_manager_v1::Request::CreateIccCreator { obj } => {
                // Advertised feature set does NOT include icc_v2_v4, so well-behaved
                // clients won't call this. If one does anyway, accept the object so
                // the create path can fail gracefully via the failed event.
                data_init.init(obj, IccCreatorData::default());
            }
            wp_color_manager_v1::Request::CreateParametricCreator { obj } => {
                let data: ParametricCreatorData =
                    Arc::new(Mutex::new(ParametricParams::default()));
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
    _output_id: ObjectId,
}
unsafe impl Send for OutputCmData {}
unsafe impl Sync for OutputCmData {}

impl Dispatch<WpColorManagementOutputV1, OutputCmData> for JwmWaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &WpColorManagementOutputV1,
        request: wp_color_management_output_v1::Request,
        _data: &OutputCmData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wp_color_management_output_v1::Request::Destroy => {}
            wp_color_management_output_v1::Request::GetImageDescription { image_description } => {
                // Slice 1: always return sRGB. Slice 2 will derive from EDID HDR caps.
                let (id, data) = make_ready_description(state, srgb_params(), true);
                let desc = data_init.init(image_description, data);
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
                // Look up the description user data to grab its id.
                let user_data = image_description
                    .data::<ImageDescriptionData>()
                    .cloned();
                if let Some(d) = user_data {
                    let id_opt = match &*d.lock_safe() {
                        ImageDescriptionState::Ready { id, .. } => Some(*id),
                        ImageDescriptionState::Failed => None,
                    };
                    if let (Some(id), Some(cm)) = (id_opt, state.color_manager.as_ref()) {
                        cm.surface_descriptions
                            .lock_safe()
                            .insert(data.surface.id(), id);
                    }
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
        }
    }
}

// === wp_color_management_surface_feedback_v1 ===

pub struct SurfaceFeedbackData {
    _surface: WlSurface,
}
unsafe impl Send for SurfaceFeedbackData {}
unsafe impl Sync for SurfaceFeedbackData {}

impl Dispatch<WpColorManagementSurfaceFeedbackV1, SurfaceFeedbackData> for JwmWaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &WpColorManagementSurfaceFeedbackV1,
        request: wp_color_management_surface_feedback_v1::Request,
        _data: &SurfaceFeedbackData,
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
                let (id, data) = make_ready_description(state, srgb_params(), true);
                let desc = data_init.init(image_description, data);
                desc.ready(id as u32);
            }
            _ => {}
        }
    }
}

// === wp_image_description_creator_icc_v1 ===

#[derive(Default)]
pub struct IccCreatorData {
    pub _icc_file_set: bool,
}

impl Dispatch<WpImageDescriptionCreatorIccV1, IccCreatorData> for JwmWaylandState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &WpImageDescriptionCreatorIccV1,
        request: wp_image_description_creator_icc_v1::Request,
        _data: &IccCreatorData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wp_image_description_creator_icc_v1::Request::Create { image_description } => {
                // ICC handling is not implemented in slice 1: fail the description.
                let st: ImageDescriptionData =
                    Arc::new(Mutex::new(ImageDescriptionState::Failed));
                let desc = data_init.init(image_description, st);
                desc.failed(Cause::Unsupported, "ICC profiles not yet supported".into());
            }
            wp_image_description_creator_icc_v1::Request::SetIccFile { .. } => {
                // Discard the fd; we don't parse ICC yet.
            }
            _ => {}
        }
    }
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
                r_x, r_y, g_x, g_y, b_x, b_y, w_x, w_y,
            } => {
                data.lock_safe().primaries = Some([r_x, r_y, g_x, g_y, b_x, b_y, w_x, w_y]);
            }
            wp_image_description_creator_params_v1::Request::SetLuminances {
                min_lum, max_lum, reference_lum,
            } => {
                let mut g = data.lock_safe();
                g.min_lum = Some(min_lum);
                g.max_lum = Some(max_lum);
                g.reference_lum = Some(reference_lum);
            }
            wp_image_description_creator_params_v1::Request::SetMasteringDisplayPrimaries {
                r_x, r_y, g_x, g_y, b_x, b_y, w_x, w_y,
            } => {
                data.lock_safe().mastering_primaries =
                    Some([r_x, r_y, g_x, g_y, b_x, b_y, w_x, w_y]);
            }
            wp_image_description_creator_params_v1::Request::SetMasteringLuminance {
                min_lum, max_lum,
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
                if let ImageDescriptionState::Ready { params, allow_info, .. } = snapshot {
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
    if let (Some(mn), Some(mx), Some(rw)) =
        (params.min_lum, params.max_lum, params.reference_lum)
    {
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
