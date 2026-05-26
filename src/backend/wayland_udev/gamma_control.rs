/// wlr-gamma-control-unstable-v1 protocol implementation for JWM.
///
/// Allows color temperature tools like gammastep and wlsunset to adjust
/// display gamma ramps for night light functionality.

use log::{info, warn};

use smithay::output::Output;
use smithay::reexports::wayland_protocols_wlr::gamma_control::v1::server::{
    zwlr_gamma_control_manager_v1::{self, ZwlrGammaControlManagerV1},
    zwlr_gamma_control_v1::{self, ZwlrGammaControlV1},
};
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New,
};

use crate::backend::wayland::state::JwmWaylandState;

pub struct GammaControlManagerData;
unsafe impl Send for GammaControlManagerData {}

pub struct GammaControlData {
    pub output: Output,
    pub gamma_size: u32,
}
unsafe impl Send for GammaControlData {}

/// Initialize the wlr-gamma-control-manager global.
pub fn init_gamma_control(dh: &DisplayHandle) {
    dh.create_global::<JwmWaylandState, ZwlrGammaControlManagerV1, _>(1, GammaControlManagerData);
    info!("[udev/wayland] zwlr-gamma-control-unstable-v1 global registered");
}

// --- GlobalDispatch for the manager ---

impl GlobalDispatch<ZwlrGammaControlManagerV1, GammaControlManagerData> for JwmWaylandState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrGammaControlManagerV1>,
        _global_data: &GammaControlManagerData,
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, GammaControlManagerData);
    }
}

// --- Dispatch for the manager ---

impl Dispatch<ZwlrGammaControlManagerV1, GammaControlManagerData> for JwmWaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ZwlrGammaControlManagerV1,
        request: zwlr_gamma_control_manager_v1::Request,
        _data: &GammaControlManagerData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_gamma_control_manager_v1::Request::GetGammaControl { id, output: wl_output } => {
                let output = Output::from_resource(&wl_output)
                    .or_else(|| state.outputs.first().cloned());

                let output = match output {
                    Some(o) => o,
                    None => {
                        warn!("[gamma] no output for gamma control");
                        return;
                    }
                };

                // Default gamma table size: 256 entries per channel (R, G, B).
                let gamma_size = 256u32;

                let ctrl = data_init.init(
                    id,
                    GammaControlData {
                        output: output.clone(),
                        gamma_size,
                    },
                );

                ctrl.gamma_size(gamma_size);
            }
            zwlr_gamma_control_manager_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

// --- Dispatch for per-output gamma control ---

impl Dispatch<ZwlrGammaControlV1, GammaControlData> for JwmWaylandState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ZwlrGammaControlV1,
        request: zwlr_gamma_control_v1::Request,
        data: &GammaControlData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_gamma_control_v1::Request::SetGamma { fd } => {
                info!(
                    "[gamma] set_gamma for output={} (size={})",
                    data.output.name(),
                    data.gamma_size
                );
                // TODO: Read gamma table from fd and apply via DRM GAMMA_LUT property.
                // The fd contains gamma_size * 3 * sizeof(u16) bytes (R, G, B ramps).
                let _ = fd;
            }
            zwlr_gamma_control_v1::Request::Destroy => {}
            _ => {}
        }
    }
}
