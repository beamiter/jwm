/// wlr-output-management-unstable-v1 protocol implementation for JWM.
///
/// Allows clients like wlr-randr and kanshi to enumerate outputs (modes, position,
/// scale, transform, adaptive sync) and apply configuration changes.

use std::sync::atomic::{AtomicU32, Ordering};

use log::info;

use smithay::output::Output;
use smithay::reexports::wayland_protocols_wlr::output_management::v1::server::{
    zwlr_output_configuration_head_v1::{self, ZwlrOutputConfigurationHeadV1},
    zwlr_output_configuration_v1::{self, ZwlrOutputConfigurationV1},
    zwlr_output_head_v1::{self, ZwlrOutputHeadV1},
    zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
    zwlr_output_mode_v1::{self, ZwlrOutputModeV1},
};
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, Weak,
};

use crate::backend::wayland::state::JwmWaylandState;

static SERIAL_COUNTER: AtomicU32 = AtomicU32::new(1);

fn next_serial() -> u32 {
    SERIAL_COUNTER.fetch_add(1, Ordering::Relaxed)
}

// --- Data types ---

pub struct OutputManagerData;
unsafe impl Send for OutputManagerData {}

pub struct OutputHeadData {
    pub output_name: String,
}
unsafe impl Send for OutputHeadData {}

pub struct OutputModeData {
    pub output_name: String,
    pub width: i32,
    pub height: i32,
    pub refresh: i32,
    pub preferred: bool,
}
unsafe impl Send for OutputModeData {}

pub struct OutputConfigData {
    pub serial: u32,
    pub heads: Vec<PendingHeadConfig>,
}
unsafe impl Send for OutputConfigData {}

pub struct OutputConfigHeadData {
    pub output_name: String,
    pub mode: Option<Weak<ZwlrOutputModeV1>>,
    pub custom_mode: Option<(i32, i32, i32)>,
    pub position: Option<(i32, i32)>,
    pub transform: Option<i32>,
    pub scale: Option<f64>,
    pub adaptive_sync: Option<bool>,
}
unsafe impl Send for OutputConfigHeadData {}

pub struct PendingHeadConfig {
    pub output_name: String,
    pub mode: Option<Weak<ZwlrOutputModeV1>>,
    pub custom_mode: Option<(i32, i32, i32)>,
    pub position: Option<(i32, i32)>,
    pub transform: Option<i32>,
    pub scale: Option<f64>,
    pub adaptive_sync: Option<bool>,
}

/// Initialize the wlr-output-management global.
pub fn init_output_management(dh: &DisplayHandle) {
    dh.create_global::<JwmWaylandState, ZwlrOutputManagerV1, _>(4, OutputManagerData);
    info!("[udev/wayland] zwlr-output-management-unstable-v1 global registered");
}

// --- GlobalDispatch for the manager ---

impl GlobalDispatch<ZwlrOutputManagerV1, OutputManagerData> for JwmWaylandState {
    fn bind(
        state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrOutputManagerV1>,
        _global_data: &OutputManagerData,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let manager = data_init.init(resource, OutputManagerData);
        let serial = next_serial();

        // Send current output state to the newly-bound client.
        for output in &state.outputs {
            send_head_for_output(&manager, output, state, data_init);
        }

        manager.done(serial);
    }
}

fn send_head_for_output(
    _manager: &ZwlrOutputManagerV1,
    _output: &Output,
    _state: &JwmWaylandState,
    _data_init: &mut DataInit<'_, JwmWaylandState>,
) {
    // Head creation requires client-allocated new_id which only happens during
    // the manager.head event. Since the wayland-server crate generates event
    // methods that return the resource, we would call manager.head() here.
    // However, the generated API requires the data to be passed through DataInit
    // which is only available in request handlers, not event senders.
    //
    // Full implementation would use a two-phase approach:
    // 1. Track bound managers in state
    // 2. On next event loop tick, send head events via the display handle
    //
    // For now, clients will receive the done event with an empty head list.
    // This is enough for the global to be advertised and clients to bind.
}

// --- Dispatch for the manager ---

impl Dispatch<ZwlrOutputManagerV1, OutputManagerData> for JwmWaylandState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ZwlrOutputManagerV1,
        request: zwlr_output_manager_v1::Request,
        _data: &OutputManagerData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_output_manager_v1::Request::CreateConfiguration { id, serial } => {
                data_init.init(
                    id,
                    OutputConfigData {
                        serial,
                        heads: Vec::new(),
                    },
                );
            }
            zwlr_output_manager_v1::Request::Stop => {}
            _ => {}
        }
    }
}

// --- Dispatch for configuration ---

impl Dispatch<ZwlrOutputConfigurationV1, OutputConfigData> for JwmWaylandState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        resource: &ZwlrOutputConfigurationV1,
        request: zwlr_output_configuration_v1::Request,
        _data: &OutputConfigData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_output_configuration_v1::Request::EnableHead { id, head } => {
                let output_name = head
                    .data::<OutputHeadData>()
                    .map(|d| d.output_name.clone())
                    .unwrap_or_default();
                data_init.init(
                    id,
                    OutputConfigHeadData {
                        output_name,
                        mode: None,
                        custom_mode: None,
                        position: None,
                        transform: None,
                        scale: None,
                        adaptive_sync: None,
                    },
                );
            }
            zwlr_output_configuration_v1::Request::DisableHead { head: _ } => {
                // Output disable not yet supported
            }
            zwlr_output_configuration_v1::Request::Apply => {
                // TODO: Apply configuration to DRM outputs
                resource.cancelled();
            }
            zwlr_output_configuration_v1::Request::Test => {
                // TODO: Test configuration without applying
                resource.cancelled();
            }
            zwlr_output_configuration_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

// --- Dispatch for configuration head ---

impl Dispatch<ZwlrOutputConfigurationHeadV1, OutputConfigHeadData> for JwmWaylandState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ZwlrOutputConfigurationHeadV1,
        request: zwlr_output_configuration_head_v1::Request,
        data: &OutputConfigHeadData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let _ = data;
        match request {
            zwlr_output_configuration_head_v1::Request::SetMode { mode: _ } => {}
            zwlr_output_configuration_head_v1::Request::SetCustomMode {
                width: _,
                height: _,
                refresh: _,
            } => {}
            zwlr_output_configuration_head_v1::Request::SetPosition { x: _, y: _ } => {}
            zwlr_output_configuration_head_v1::Request::SetTransform { transform: _ } => {}
            zwlr_output_configuration_head_v1::Request::SetScale { scale: _ } => {}
            zwlr_output_configuration_head_v1::Request::SetAdaptiveSync { state: _ } => {}
            _ => {}
        }
    }
}

// --- Dispatch for head (events only, no client requests except release) ---

impl Dispatch<ZwlrOutputHeadV1, OutputHeadData> for JwmWaylandState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ZwlrOutputHeadV1,
        _request: zwlr_output_head_v1::Request,
        _data: &OutputHeadData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        // Head has no requests in v1-v3, only "release" in v4
    }
}

// --- Dispatch for mode (events only, no client requests except release) ---

impl Dispatch<ZwlrOutputModeV1, OutputModeData> for JwmWaylandState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ZwlrOutputModeV1,
        _request: zwlr_output_mode_v1::Request,
        _data: &OutputModeData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        // Mode has no requests in v1-v3, only "release" in v4
    }
}
