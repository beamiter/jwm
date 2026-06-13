/// wlr-gamma-control-unstable-v1 protocol implementation for JWM.
///
/// Allows color temperature tools like gammastep and wlsunset to adjust
/// display gamma ramps for night light functionality.

use std::io::Read;
use std::os::unix::io::{AsRawFd, FromRawFd};

use log::{info, warn};

use smithay::output::Output;
use smithay::reexports::wayland_protocols_wlr::gamma_control::v1::server::{
    zwlr_gamma_control_manager_v1::{self, ZwlrGammaControlManagerV1},
    zwlr_gamma_control_v1::{self, ZwlrGammaControlV1},
};
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New,
};

use crate::backend::api::BackendEvent;
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

                // Advertise the real hardware LUT size; clients upload a ramp of
                // exactly this length, so a wrong value makes set_gamma fail.
                let gamma_size = state
                    .gamma_sizes
                    .get(&output.name())
                    .copied()
                    .unwrap_or(256);

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
        state: &mut Self,
        _client: &Client,
        _resource: &ZwlrGammaControlV1,
        request: zwlr_gamma_control_v1::Request,
        data: &GammaControlData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_gamma_control_v1::Request::SetGamma { fd } => {
                let expected_bytes = (data.gamma_size as usize) * 3 * std::mem::size_of::<u16>();
                let mut buf = vec![0u8; expected_bytes];

                let raw_fd = fd.as_raw_fd();
                let mut file = unsafe { std::fs::File::from_raw_fd(raw_fd) };
                match file.read_exact(&mut buf) {
                    Ok(()) => {
                        // Convert bytes to u16 values (little-endian)
                        let ramp: Vec<u16> = buf
                            .chunks_exact(2)
                            .map(|c| u16::from_ne_bytes([c[0], c[1]]))
                            .collect();

                        info!(
                            "[gamma] set_gamma for output={} (size={})",
                            data.output.name(),
                            data.gamma_size
                        );

                        state.push_event(BackendEvent::GammaSet {
                            output_name: data.output.name(),
                            gamma_size: data.gamma_size,
                            ramp,
                        });
                    }
                    Err(e) => {
                        warn!("[gamma] failed to read gamma table from fd: {e}");
                    }
                }
                // Intentionally leak the File to avoid closing the OwnedFd
                std::mem::forget(file);
            }
            zwlr_gamma_control_v1::Request::Destroy => {}
            _ => {}
        }
    }
}
