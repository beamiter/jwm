/// wp-tearing-control-v1 protocol implementation for JWM.
///
/// Allows game clients to opt into asynchronous page flips (tearing) for reduced
/// input latency. The compositor stores the per-surface presentation hint and
/// checks it during the DRM page flip path.

use smithay::reexports::wayland_protocols::wp::tearing_control::v1::server::{
    wp_tearing_control_manager_v1::{self, WpTearingControlManagerV1},
    wp_tearing_control_v1::{self, WpTearingControlV1},
};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};

use crate::backend::wayland::state::JwmWaylandState;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Per-surface tearing preference, stored in JwmWaylandState.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TearingHint {
    Vsync,
    Async,
}

impl Default for TearingHint {
    fn default() -> Self {
        Self::Vsync
    }
}

/// Shared map of surface ObjectId -> TearingHint, queried during page flip.
pub type TearingHintMap = Arc<Mutex<HashMap<smithay::reexports::wayland_server::backend::ObjectId, TearingHint>>>;

pub fn new_tearing_hint_map() -> TearingHintMap {
    Arc::new(Mutex::new(HashMap::new()))
}

/// User data stored per wp_tearing_control_v1 object.
pub struct TearingControlData {
    surface: WlSurface,
}

// TearingControlData contains Wayland protocol objects which are !Send.
// JWM runs everything on the main thread so this is fine.
unsafe impl Send for TearingControlData {}

/// Initialize the wp_tearing_control_manager_v1 global.
pub fn init_tearing_control_manager(dh: &DisplayHandle) -> TearingHintMap {
    dh.create_global::<JwmWaylandState, WpTearingControlManagerV1, _>(1, ());
    log::info!("[udev/wayland] wp-tearing-control-v1 global registered");
    new_tearing_hint_map()
}

// --- GlobalDispatch for the manager ---

impl GlobalDispatch<WpTearingControlManagerV1, ()> for JwmWaylandState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<WpTearingControlManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

// --- Dispatch for the manager ---

impl Dispatch<WpTearingControlManagerV1, ()> for JwmWaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &WpTearingControlManagerV1,
        request: wp_tearing_control_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wp_tearing_control_manager_v1::Request::GetTearingControl { id, surface } => {
                let data = TearingControlData {
                    surface: surface.clone(),
                };
                data_init.init(id, data);
                // Initialize with vsync
                if let Some(ref hints) = state.tearing_hints {
                    hints.lock().unwrap().insert(surface.id(), TearingHint::Vsync);
                }
            }
            wp_tearing_control_manager_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

// --- Dispatch for per-surface tearing control ---

impl Dispatch<WpTearingControlV1, TearingControlData> for JwmWaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &WpTearingControlV1,
        request: wp_tearing_control_v1::Request,
        data: &TearingControlData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wp_tearing_control_v1::Request::SetPresentationHint { hint } => {
                let hint = match hint.into_result() {
                    Ok(wp_tearing_control_v1::PresentationHint::Async) => TearingHint::Async,
                    _ => TearingHint::Vsync,
                };
                if let Some(ref hints) = state.tearing_hints {
                    hints.lock().unwrap().insert(data.surface.id(), hint);
                }
            }
            wp_tearing_control_v1::Request::Destroy => {
                if let Some(ref hints) = state.tearing_hints {
                    hints.lock().unwrap().remove(&data.surface.id());
                }
            }
            _ => {}
        }
    }
}
