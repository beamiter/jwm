/// wlr-output-power-management-unstable-v1 protocol implementation for JWM.
///
/// Allows idle daemons like swayidle to blank/unblank displays (DPMS).
use crate::sync_ext::MutexExt;
use log::info;

use smithay::reexports::wayland_protocols_wlr::output_power_management::v1::server::{
    zwlr_output_power_manager_v1::{self, ZwlrOutputPowerManagerV1},
    zwlr_output_power_v1::{self, ZwlrOutputPowerV1},
};
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New,
};

use crate::backend::wayland::state::JwmWaylandState;

pub struct OutputPowerManagerData;
unsafe impl Send for OutputPowerManagerData {}

pub struct OutputPowerData {
    pub output_name: String,
}
unsafe impl Send for OutputPowerData {}

/// Initialize the output power management global.
pub fn init_output_power_management(dh: &DisplayHandle) {
    dh.create_global::<JwmWaylandState, ZwlrOutputPowerManagerV1, _>(1, OutputPowerManagerData);
    info!("[udev/wayland] zwlr-output-power-management-unstable-v1 global registered");
}

// --- GlobalDispatch for the manager ---

impl GlobalDispatch<ZwlrOutputPowerManagerV1, OutputPowerManagerData> for JwmWaylandState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrOutputPowerManagerV1>,
        _global_data: &OutputPowerManagerData,
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, OutputPowerManagerData);
    }
}

// --- Dispatch for the manager ---

impl Dispatch<ZwlrOutputPowerManagerV1, OutputPowerManagerData> for JwmWaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ZwlrOutputPowerManagerV1,
        request: zwlr_output_power_manager_v1::Request,
        _data: &OutputPowerManagerData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_output_power_manager_v1::Request::GetOutputPower { id, output } => {
                // Honor the requested wl_output; only fall back to the primary
                // output when the resource can't be resolved (e.g. stale object).
                let output_name = smithay::output::Output::from_resource(&output)
                    .or_else(|| state.outputs.first().cloned())
                    .map(|o| o.name())
                    .unwrap_or_else(|| "unknown".to_string());

                let power = data_init.init(
                    id,
                    OutputPowerData {
                        output_name: output_name.clone(),
                    },
                );
                // Send current mode (On)
                power.mode(zwlr_output_power_v1::Mode::On);
            }
            zwlr_output_power_manager_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

// --- Dispatch for per-output power control ---

impl Dispatch<ZwlrOutputPowerV1, OutputPowerData> for JwmWaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwlrOutputPowerV1,
        request: zwlr_output_power_v1::Request,
        data: &OutputPowerData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_output_power_v1::Request::SetMode { mode } => match mode.into_result() {
                Ok(zwlr_output_power_v1::Mode::Off) => {
                    info!("[udev/wayland] DPMS off requested for {}", data.output_name);
                    state.pending_events.lock_safe().push_back(
                        crate::backend::api::BackendEvent::OutputPowerSet {
                            output_name: data.output_name.clone(),
                            on: false,
                        },
                    );
                    resource.mode(zwlr_output_power_v1::Mode::Off);
                }
                Ok(zwlr_output_power_v1::Mode::On) => {
                    info!("[udev/wayland] DPMS on requested for {}", data.output_name);
                    state.pending_events.lock_safe().push_back(
                        crate::backend::api::BackendEvent::OutputPowerSet {
                            output_name: data.output_name.clone(),
                            on: true,
                        },
                    );
                    state.needs_redraw = true;
                    resource.mode(zwlr_output_power_v1::Mode::On);
                }
                _ => {}
            },
            zwlr_output_power_v1::Request::Destroy => {}
            _ => {}
        }
    }
}
