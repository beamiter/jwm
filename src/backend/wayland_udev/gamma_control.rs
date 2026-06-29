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

/// Upper bound on the LUT size we'll honor. Real hardware reports 256–4096;
/// values above this are a sign of a buggy KMS or a malicious driver and would
/// cause `set_gamma` to allocate gigabytes of host memory per call.
pub(crate) const MAX_GAMMA_SIZE: u32 = 65_536;

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
            zwlr_gamma_control_manager_v1::Request::GetGammaControl {
                id,
                output: wl_output,
            } => {
                let output =
                    Output::from_resource(&wl_output).or_else(|| state.outputs.first().cloned());

                let output = match output {
                    Some(o) => o,
                    None => {
                        warn!("[gamma] no output for gamma control");
                        return;
                    }
                };

                // Advertise the real hardware LUT size; clients upload a ramp of
                // exactly this length, so a wrong value makes set_gamma fail.
                // Clamp against pathological values (a misbehaving KMS could
                // report a giant LUT, which would mean a multi-GB allocation
                // on set_gamma — refuse to advertise the resource in that case).
                let raw = state
                    .gamma_sizes
                    .get(&output.name())
                    .copied()
                    .unwrap_or(256);
                if raw == 0 || raw > MAX_GAMMA_SIZE {
                    warn!(
                        "[gamma] refusing to bind: output {} reports unreasonable gamma_size={raw}",
                        output.name()
                    );
                    return;
                }
                let gamma_size = raw;

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
                        // wlr-gamma-control wire format is little-endian
                        // (matches DRM's `DRM_MODE_LUT_FORMAT_LE`). Using
                        // `from_ne_bytes` here was wrong on big-endian hosts.
                        let ramp: Vec<u16> = buf
                            .chunks_exact(2)
                            .map(|c| u16::from_le_bytes([c[0], c[1]]))
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

    /// Called when the gamma-control object is destroyed — including when the
    /// client (wlsunset/gammastep) crashes or exits without an explicit Destroy
    /// request. Without restoring the ramp here the hardware would stay tinted
    /// indefinitely. Per the wlr-gamma-control spec the original gamma must be
    /// restored; we reset to a linear identity ramp (the DRM default).
    fn destroyed(
        state: &mut Self,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        _resource: &ZwlrGammaControlV1,
        data: &GammaControlData,
    ) {
        let sz = data.gamma_size as usize;
        if sz == 0 {
            return;
        }
        let denom = (sz.max(2) - 1) as u64;
        let mut ramp: Vec<u16> = Vec::with_capacity(sz * 3);
        for _channel in 0..3 {
            for i in 0..sz {
                ramp.push(((i as u64 * 65535) / denom) as u16);
            }
        }
        info!(
            "[gamma] control destroyed, restoring linear ramp for output={}",
            data.output.name()
        );
        state.push_event(BackendEvent::GammaSet {
            output_name: data.output.name(),
            gamma_size: data.gamma_size,
            ramp,
        });
    }
}
