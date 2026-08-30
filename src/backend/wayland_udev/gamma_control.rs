/// wlr-gamma-control-unstable-v1 protocol implementation for JWM.
///
/// Allows color temperature tools like gammastep and wlsunset to adjust
/// display gamma ramps for night light functionality.
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;

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

/// Read one protocol gamma-table payload without trusting the client fd to be
/// a blocking-safe stream.
///
/// The protocol describes this fd as a memory-mappable, exact-size file. A
/// pipe or socket is not a valid table and, more importantly, a synchronous
/// `read_exact` from one would let a client freeze the compositor indefinitely
/// by retaining its write end. Validate the descriptor before doing any I/O,
/// then use a positional read so the client's current file offset is ignored.
fn read_gamma_table(file: &File, expected_bytes: usize) -> io::Result<Vec<u8>> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "gamma table fd is not a regular file",
        ));
    }

    let expected_len = u64::try_from(expected_bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "gamma table length does not fit in u64",
        )
    })?;
    if metadata.len() != expected_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "gamma table has {} bytes, expected {expected_len}",
                metadata.len()
            ),
        ));
    }

    let mut table = vec![0u8; expected_bytes];
    file.read_exact_at(&mut table, 0)?;
    Ok(table)
}

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
        state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrGammaControlManagerV1>,
        _global_data: &GammaControlManagerData,
        data_init: &mut DataInit<'_, Self>,
    ) {
        state.record_protocol_bind("zwlr_gamma_control_manager_v1");
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
                let file = File::from(fd);
                match read_gamma_table(&file, expected_bytes) {
                    Ok(buf) => {
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

#[cfg(test)]
mod tests {
    use super::read_gamma_table;
    use nix::sys::memfd::{MFdFlags, memfd_create};
    use nix::unistd::pipe;
    use std::fs::File;
    use std::io::{Seek, SeekFrom, Write};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn gamma_table_reads_exact_memfd_from_zero_offset() {
        let fd = memfd_create("jwm-gamma-table-test", MFdFlags::MFD_CLOEXEC).unwrap();
        let mut file = File::from(fd);
        let payload = [1, 0, 2, 0, 3, 0];
        file.write_all(&payload).unwrap();
        file.seek(SeekFrom::End(0)).unwrap();

        assert_eq!(read_gamma_table(&file, payload.len()).unwrap(), payload);
        assert_eq!(
            read_gamma_table(&file, payload.len() - 1)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn gamma_table_rejects_open_pipe_without_blocking() {
        let (read_end, write_end) = pipe().unwrap();
        let file = File::from(read_end);
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            let _ = sender.send(read_gamma_table(&file, 6));
        });

        let result = match receiver.recv_timeout(Duration::from_secs(2)) {
            Ok(result) => result,
            Err(error) => {
                // Unblock a regressed read_exact before failing the test so it
                // cannot leave a stuck test worker behind.
                drop(write_end);
                worker.join().unwrap();
                panic!("gamma-table pipe read blocked the compositor path: {error}");
            }
        };
        drop(write_end);
        worker.join().unwrap();

        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    }
}
