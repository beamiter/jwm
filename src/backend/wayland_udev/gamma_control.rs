/// wlr-gamma-control-unstable-v1 protocol implementation for JWM.
///
/// Allows color temperature tools like gammastep and wlsunset to adjust
/// display gamma ramps for night light functionality.
use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::sync::{Arc, Mutex};

use log::{debug, info, warn};

use smithay::output::Output;
use smithay::reexports::wayland_protocols_wlr::gamma_control::v1::server::{
    zwlr_gamma_control_manager_v1::{self, ZwlrGammaControlManagerV1},
    zwlr_gamma_control_v1::{self, ZwlrGammaControlV1},
};
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, Weak,
};

use crate::backend::api::BackendEvent;
use crate::backend::wayland::state::{JwmWaylandState, client_is_sandboxed};
use crate::sync_ext::MutexExt;

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

/// The LUT size advertised for `output_name`, or `None` when the reported
/// size is unusable. Outputs without a KMS-reported size (nested backends)
/// advertise the common 256-entry ramp.
fn advertised_gamma_size(state: &JwmWaylandState, output_name: &str) -> Option<u32> {
    let size = state.gamma_sizes.get(output_name).copied().unwrap_or(256);
    (size != 0 && size <= MAX_GAMMA_SIZE).then_some(size)
}

/// A linear ramp for each of the three channels: the DRM default table.
fn identity_ramp(gamma_size: u32) -> Vec<u16> {
    let size = gamma_size as usize;
    let denom = (size.max(2) - 1) as u64;
    let mut ramp = Vec::with_capacity(size * 3);
    for _channel in 0..3 {
        for i in 0..size {
            ramp.push(((i as u64 * 65535) / denom) as u16);
        }
    }
    ramp
}

/// The control holding one output.
struct GammaOwner {
    token: u64,
    /// The ramp length the control was advertised. A LUT of another size
    /// cannot take its ramps any more.
    gamma_size: u32,
    /// The control's resource, attached right after it is created, so a
    /// layout change can tell the client its control went stale.
    control: Option<Weak<ZwlrGammaControlV1>>,
}

#[derive(Default)]
struct GammaOwnerTable {
    next_token: u64,
    /// Output -> the control holding it.
    owners: HashMap<Output, GammaOwner>,
    /// Connector name -> the size of the client ramp handed to the backend
    /// for it, until an identity ramp restores the connector.
    ///
    /// Kept per connector, not per control: a KMS rebuild carries the ramp
    /// onto the connector's new `Output` and fails the control that set it,
    /// so the tint outlives both the claim and the `Output` that control
    /// named. Whichever control is last on the connector owes the restore,
    /// including a re-created one that never sent a ramp of its own.
    tinted: HashMap<String, u32>,
}

/// Which gamma control holds each output.
///
/// wlr-gamma-control gives at most one control per output exclusive access;
/// a request for an output that is already held gets `failed`. Outputs are
/// keyed by identity, not by connector name: a KMS rebuild publishes new
/// `Output` objects, and a client's control for the old one must not lock
/// the connector against the control it creates for the new one.
///
/// The table is shared by the global, its managers and their controls, so
/// it lives exactly as long as the protocol objects that consult it.
#[derive(Clone, Default)]
pub struct GammaOwners(Arc<Mutex<GammaOwnerTable>>);

impl GammaOwners {
    /// Claim `output` for a new control advertised `gamma_size`. Returns the
    /// control's token, or `None` when another live control already holds
    /// the output.
    fn claim(&self, output: &Output, gamma_size: u32) -> Option<u64> {
        let mut table = self.0.lock_safe();
        if table.owners.contains_key(output) {
            return None;
        }
        table.next_token += 1;
        let token = table.next_token;
        table.owners.insert(
            output.clone(),
            GammaOwner {
                token,
                gamma_size,
                control: None,
            },
        );
        Some(token)
    }

    /// Record the resource of the control that claimed `output` as `token`.
    fn attach(&self, output: &Output, token: u64, control: &ZwlrGammaControlV1) {
        if let Some(owner) = self.0.lock_safe().owners.get_mut(output)
            && owner.token == token
        {
            owner.control = Some(control.downgrade());
        }
    }

    /// Whether the control identified by `token` still holds `output`.
    fn holds(&self, output: &Output, token: u64) -> bool {
        self.0
            .lock_safe()
            .owners
            .get(output)
            .is_some_and(|owner| owner.token == token)
    }

    /// Whether any control holds `output`.
    fn is_held(&self, output: &Output) -> bool {
        self.0.lock_safe().owners.contains_key(output)
    }

    /// Release `output` if `token` still holds it. Returns whether it did.
    fn release(&self, output: &Output, token: u64) -> bool {
        let mut table = self.0.lock_safe();
        if !table
            .owners
            .get(output)
            .is_some_and(|owner| owner.token == token)
        {
            return false;
        }
        table.owners.remove(output);
        true
    }

    /// Release every output whose owner `current` rejects (given the output
    /// and the size its control was advertised) and return those controls.
    /// Tints of connectors `published` no longer names are forgotten: the
    /// rebuild that removed the connector dropped its ramp with it.
    fn release_stale(
        &self,
        mut current: impl FnMut(&Output, u32) -> bool,
        published: impl Fn(&str) -> bool,
    ) -> Vec<Weak<ZwlrGammaControlV1>> {
        let mut stale = Vec::new();
        let mut table = self.0.lock_safe();
        table.owners.retain(|output, owner| {
            if current(output, owner.gamma_size) {
                return true;
            }
            stale.extend(owner.control.take());
            false
        });
        table.tinted.retain(|name, _| published(name));
        stale
    }

    /// Record that a ramp of `gamma_size` entries was handed to the backend
    /// for connector `output_name`.
    fn mark_tinted(&self, output_name: &str, gamma_size: u32) {
        self.0
            .lock_safe()
            .tinted
            .insert(output_name.to_owned(), gamma_size);
    }

    /// Take the tint record of `output_name`: the size of the ramp it
    /// carries, or `None` when no client ramp is on it.
    fn take_tint(&self, output_name: &str) -> Option<u32> {
        self.0.lock_safe().tinted.remove(output_name)
    }
}

/// Send `failed` to every control whose output a layout change removed or
/// whose LUT changed size, and free those outputs for new controls.
///
/// Called by the backend after it republished `state.outputs` and
/// `state.gamma_sizes` (hotplug, KMS rebuild, output configuration). A
/// rebuild publishes new `Output` objects, so every control of the old
/// layout is failed. Without this the client learned it only on its next
/// `set_gamma`, and a night-light tool at its target temperature sends none,
/// so the rebuilt output stayed untinted until the next transition.
/// Returns how many controls were sent `failed`; the caller owes a flush.
pub(crate) fn fail_stale_controls(state: &JwmWaylandState) -> usize {
    let Some(owners) = state.gamma_owners.as_ref() else {
        return 0;
    };
    let stale = owners.release_stale(
        |output, gamma_size| {
            state.outputs.contains(output)
                && advertised_gamma_size(state, &output.name()) == Some(gamma_size)
        },
        |name| state.outputs.iter().any(|output| output.name() == name),
    );
    let mut failed = 0;
    for control in stale {
        if let Ok(control) = control.upgrade() {
            control.failed();
            failed += 1;
        }
    }
    if failed > 0 {
        info!("[gamma] the output layout changed under {failed} gamma control(s); sent failed");
    }
    failed
}

pub struct GammaControlManagerData {
    owners: GammaOwners,
}
unsafe impl Send for GammaControlManagerData {}

pub struct GammaControlData {
    /// The output this control drives. `None` for an inert control that was
    /// sent `failed` at creation.
    pub output: Option<Output>,
    pub gamma_size: u32,
    /// Ownership token from [`GammaOwners::claim`]; `None` for an inert
    /// control, which never held its output.
    token: Option<u64>,
    owners: GammaOwners,
}
unsafe impl Send for GammaControlData {}

impl GammaControlData {
    fn inert(owners: &GammaOwners) -> Self {
        Self {
            output: None,
            gamma_size: 0,
            token: None,
            owners: owners.clone(),
        }
    }

    /// The output this control still holds, with its ownership token. `None`
    /// once the control has been sent `failed`.
    fn held_output(&self) -> Option<(&Output, u64)> {
        let output = self.output.as_ref()?;
        let token = self.token?;
        self.owners.holds(output, token).then_some((output, token))
    }
}

/// Initialize the wlr-gamma-control-manager global. Returns its ownership
/// table, which [`fail_stale_controls`] consults after a layout change.
pub fn init_gamma_control(dh: &DisplayHandle) -> GammaOwners {
    let owners = GammaOwners::default();
    dh.create_global::<JwmWaylandState, ZwlrGammaControlManagerV1, _>(
        1,
        GammaControlManagerData {
            owners: owners.clone(),
        },
    );
    info!("[udev/wayland] zwlr-gamma-control-unstable-v1 global registered");
    owners
}

// --- GlobalDispatch for the manager ---

impl GlobalDispatch<ZwlrGammaControlManagerV1, GammaControlManagerData> for JwmWaylandState {
    fn bind(
        state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrGammaControlManagerV1>,
        global_data: &GammaControlManagerData,
        data_init: &mut DataInit<'_, Self>,
    ) {
        state.record_protocol_bind("zwlr_gamma_control_manager_v1");
        data_init.init(
            resource,
            GammaControlManagerData {
                owners: global_data.owners.clone(),
            },
        );
    }

    /// Output gamma is a privileged, screen-wide effect: a sandboxed client
    /// must not tint or blank every monitor.
    fn can_view(client: Client, _global_data: &GammaControlManagerData) -> bool {
        !client_is_sandboxed(&client)
    }
}

// --- Dispatch for the manager ---

impl Dispatch<ZwlrGammaControlManagerV1, GammaControlManagerData> for JwmWaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ZwlrGammaControlManagerV1,
        request: zwlr_gamma_control_manager_v1::Request,
        data: &GammaControlManagerData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_gamma_control_manager_v1::Request::GetGammaControl {
                id,
                output: wl_output,
            } => {
                // Every path initializes the new_id (returning without it
                // aborts the compositor inside wayland-backend) and answers
                // with either gamma_size or failed. A stale wl_output never
                // falls back to another output: that would tint a monitor
                // the client did not ask for.
                let Some(output) = Output::from_resource(&wl_output)
                    .filter(|output| state.outputs.contains(output))
                else {
                    warn!("[gamma] gamma control requested for an output that is gone");
                    let ctrl = data_init.init(id, GammaControlData::inert(&data.owners));
                    ctrl.failed();
                    return;
                };
                let name = output.name();

                // Advertise the real hardware LUT size; clients upload a ramp of
                // exactly this length, so a wrong value makes set_gamma fail.
                // Clamp against pathological values (a misbehaving KMS could
                // report a giant LUT, which would mean a multi-GB allocation
                // on set_gamma — refuse the control in that case).
                let Some(gamma_size) = advertised_gamma_size(state, &name) else {
                    warn!("[gamma] refusing control: output {name} reports an unusable gamma size");
                    let ctrl = data_init.init(id, GammaControlData::inert(&data.owners));
                    ctrl.failed();
                    return;
                };
                let Some(token) = data.owners.claim(&output, gamma_size) else {
                    info!("[gamma] output {name} already has a gamma control; sending failed");
                    let ctrl = data_init.init(id, GammaControlData::inert(&data.owners));
                    ctrl.failed();
                    return;
                };

                let ctrl = data_init.init(
                    id,
                    GammaControlData {
                        output: Some(output.clone()),
                        gamma_size,
                        token: Some(token),
                        owners: data.owners.clone(),
                    },
                );
                data.owners.attach(&output, token, &ctrl);

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
        resource: &ZwlrGammaControlV1,
        request: zwlr_gamma_control_v1::Request,
        data: &GammaControlData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_gamma_control_v1::Request::SetGamma { fd } => {
                // A control that was sent `failed` no longer owns a ramp.
                let Some((output, token)) = data.held_output() else {
                    debug!("[gamma] ignoring set_gamma on a failed control");
                    return;
                };
                let output_name = output.name();
                if !state.outputs.contains(output)
                    || advertised_gamma_size(state, &output_name) != Some(data.gamma_size)
                {
                    // The output was unplugged or rebuilt, or its LUT changed
                    // size (a hotplug moved it to another CRTC), so a ramp of
                    // the advertised length can no longer be applied to it.
                    // Tell the client, which re-creates the control for the
                    // current output, and free the output for that control.
                    warn!(
                        "[gamma] control for output {output_name} (size {}) is stale; \
                         sending failed",
                        data.gamma_size
                    );
                    data.owners.release(output, token);
                    resource.failed();
                    return;
                }
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
                            "[gamma] set_gamma for output={output_name} (size={})",
                            data.gamma_size
                        );

                        data.owners.mark_tinted(&output_name, data.gamma_size);
                        state.push_event(BackendEvent::GammaSet {
                            output_name,
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
    ///
    /// The restore is owed per connector, not per control. A connector with
    /// no client ramp on it has nothing to restore, and one another control
    /// holds is that control's to restore: a probing client, or one that was
    /// refused with `failed`, must not wipe the owner's tint. A KMS rebuild
    /// carries the ramp onto the connector's new `Output` and fails the
    /// control that set it, so a failed control, or the re-created one that
    /// never sent a ramp, still restores the connector when it is the last
    /// one on it. A connector that is no longer published lost its ramp with
    /// the rebuild that removed it.
    fn destroyed(
        state: &mut Self,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        _resource: &ZwlrGammaControlV1,
        data: &GammaControlData,
    ) {
        let (Some(output), Some(token)) = (data.output.as_ref(), data.token) else {
            return;
        };
        data.owners.release(output, token);
        let output_name = output.name();
        // The connector's current output: this control's own, or the one a
        // rebuild published under the same name.
        let Some(current) = state
            .outputs
            .iter()
            .find(|candidate| candidate.name() == output_name)
        else {
            return;
        };
        if data.owners.is_held(current) {
            return;
        }
        let Some(tinted_size) = data.owners.take_tint(&output_name) else {
            return;
        };
        // Restore at the LUT's current size: after a `failed` for a changed
        // size the advertised one no longer matches the CRTC.
        let gamma_size = advertised_gamma_size(state, &output_name).unwrap_or(tinted_size);
        info!("[gamma] control destroyed, restoring linear ramp for output={output_name}");
        state.push_event(BackendEvent::GammaSet {
            output_name,
            gamma_size,
            ramp: identity_ramp(gamma_size),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{
        GammaOwners, fail_stale_controls, identity_ramp, init_gamma_control, read_gamma_table,
    };
    use crate::backend::api::BackendEvent;
    use crate::backend::wayland::state::JwmWaylandState;
    use crate::backend::wayland_udev::image_copy_capture::wire_test_client::{
        Client, Server, test_output,
    };
    use nix::sys::memfd::{MFdFlags, memfd_create};
    use nix::unistd::pipe;
    use std::fs::File;
    use std::io::{Seek, SeekFrom, Write};
    use std::os::fd::AsRawFd;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    // zwlr_gamma_control_manager_v1 / zwlr_gamma_control_v1 wire opcodes.
    const GET_GAMMA_CONTROL: u16 = 0;
    const SET_GAMMA: u16 = 0;
    const DESTROY: u16 = 1;
    const GAMMA_SIZE_EVENT: u16 = 0;
    const FAILED_EVENT: u16 = 1;

    const OUTPUT: &str = "GAMMA-1";
    const LUT_SIZE: u32 = 4;

    /// A headless server publishing one output with a 4-entry LUT.
    fn gamma_server() -> Server {
        let mut server = Server::new();
        let display = server.display.handle();
        // The test's global replaces the one the default config made, and
        // its table is the one layout changes are checked against.
        server.state.gamma_owners = Some(init_gamma_control(&display));
        let output = test_output(OUTPUT);
        output.create_global::<JwmWaylandState>(&display);
        server.state.outputs.push(output);
        server.state.gamma_sizes.insert(OUTPUT.to_owned(), LUT_SIZE);
        server
    }

    /// A client bound to the gamma manager and to the most recent wl_output.
    struct GammaClient {
        client: Client,
        manager: u32,
        output: u32,
    }

    impl GammaClient {
        fn connect(server: &mut Server) -> Self {
            let mut client = server.connect();
            let manager = client.bind("zwlr_gamma_control_manager_v1", 1);
            let output = client.bind("wl_output", 4);
            server.roundtrip();
            client.events();
            Self {
                client,
                manager,
                output,
            }
        }

        /// Request a control; returns it with the events it was sent.
        fn get_control(&mut self, server: &mut Server) -> (u32, Vec<(u16, Vec<u32>)>) {
            let control = self.client.new_id();
            self.client
                .request(self.manager, GET_GAMMA_CONTROL, &[control, self.output]);
            server.roundtrip();
            let events = self
                .client
                .events()
                .into_iter()
                .filter(|event| event.sender == control)
                .map(|event| (event.opcode, event.args))
                .collect();
            (control, events)
        }

        fn set_gamma(&mut self, control: u32, ramp: &[u16]) {
            let fd = memfd_create("jwm-gamma-wire-test", MFdFlags::MFD_CLOEXEC).unwrap();
            let mut table = File::from(fd);
            let bytes: Vec<u8> = ramp.iter().flat_map(|value| value.to_le_bytes()).collect();
            table.write_all(&bytes).unwrap();
            self.client
                .request_with_fd(control, SET_GAMMA, table.as_raw_fd());
        }

        fn destroy(&mut self, control: u32) {
            self.client.request(control, DESTROY, &[]);
        }
    }

    /// Drain the GammaSet requests the protocol handed to the backend.
    fn gamma_sets(server: &Server) -> Vec<(String, u32, Vec<u16>)> {
        server
            .backend_events
            .lock()
            .unwrap()
            .drain(..)
            .filter_map(|event| match event {
                BackendEvent::GammaSet {
                    output_name,
                    gamma_size,
                    ramp,
                } => Some((output_name, gamma_size, ramp)),
                _ => None,
            })
            .collect()
    }

    fn warm_ramp() -> Vec<u16> {
        let mut ramp = identity_ramp(LUT_SIZE);
        // Pull blue down, as a night-light tool does.
        for value in &mut ramp[2 * LUT_SIZE as usize..] {
            *value /= 2;
        }
        ramp
    }

    #[test]
    fn gamma_ownership_is_exclusive_per_output_object() {
        let owners = GammaOwners::default();
        let output = test_output("HDMI-A-1");
        // A KMS rebuild publishes a new Output for the same connector.
        let rebuilt = test_output("HDMI-A-1");

        let token = owners
            .claim(&output, LUT_SIZE)
            .expect("a free output can be claimed");
        assert_eq!(
            owners.claim(&output, LUT_SIZE),
            None,
            "one control per output"
        );
        assert!(owners.holds(&output, token));
        let rebuilt_token = owners
            .claim(&rebuilt, LUT_SIZE)
            .expect("a stale control must not lock the rebuilt output");
        assert!(!owners.release(&output, rebuilt_token));
        assert!(owners.is_held(&output));
        assert!(owners.release(&output, token));
        assert!(!owners.is_held(&output));
        assert!(!owners.release(&output, token), "release is idempotent");
        assert!(owners.holds(&rebuilt, rebuilt_token));
        assert!(owners.claim(&output, LUT_SIZE).is_some());
    }

    /// Opcodes of the events `control` was sent since the last read.
    fn control_events(client: &mut GammaClient, control: u32) -> Vec<u16> {
        client
            .client
            .events()
            .into_iter()
            .filter(|event| event.sender == control)
            .map(|event| event.opcode)
            .collect()
    }

    #[test]
    fn a_rebuilt_output_fails_its_old_control_without_waiting_for_set_gamma() {
        let mut server = gamma_server();
        let mut client = GammaClient::connect(&mut server);
        let (stale, events) = client.get_control(&mut server);
        assert_eq!(events, [(GAMMA_SIZE_EVENT, vec![LUT_SIZE])]);
        let old_output = server.state.outputs[0].clone();
        client.set_gamma(stale, &warm_ramp());
        server.roundtrip();
        assert_eq!(gamma_sets(&server).len(), 1);

        // Layout unchanged: the live control is left alone.
        assert_eq!(fail_stale_controls(&server.state), 0);
        server.roundtrip();
        assert!(control_events(&mut client, stale).is_empty());

        // A KMS rebuild publishes a new Output for the same connector. A
        // night-light at its target temperature never sends another ramp,
        // so the client must hear about it now to re-create its control.
        server.state.outputs = vec![test_output(OUTPUT)];
        assert_eq!(fail_stale_controls(&server.state), 1);
        server.roundtrip();
        assert_eq!(control_events(&mut client, stale), [FAILED_EVENT]);
        let owners = server.state.gamma_owners.as_ref().expect("gamma table");
        assert!(!owners.is_held(&old_output), "the stale claim is released");

        // Failed once: the next layout change does not repeat it.
        assert_eq!(fail_stale_controls(&server.state), 0);
        server.roundtrip();
        assert!(control_events(&mut client, stale).is_empty());

        // The rebuild carried the ramp onto the connector's new output, and
        // no other control took it over, so this control still owes the
        // restore: without it the tint outlived every control.
        client.destroy(stale);
        server.roundtrip();
        assert_eq!(
            gamma_sets(&server),
            [(OUTPUT.to_owned(), LUT_SIZE, identity_ramp(LUT_SIZE))],
            "the carried ramp is restored when its failed control goes"
        );
    }

    #[test]
    fn a_carried_ramp_is_restored_by_the_last_control_on_the_connector() {
        let mut server = gamma_server();
        let mut night_light = GammaClient::connect(&mut server);
        let (stale, _) = night_light.get_control(&mut server);
        night_light.set_gamma(stale, &warm_ramp());
        server.roundtrip();
        assert_eq!(gamma_sets(&server).len(), 1);

        // A KMS rebuild carries the ramp onto a new Output for the connector
        // and fails the control that set it.
        let rebuilt = test_output(OUTPUT);
        rebuilt.create_global::<JwmWaylandState>(&server.display.handle());
        server.state.outputs = vec![rebuilt];
        assert_eq!(fail_stale_controls(&server.state), 1);
        server.roundtrip();
        assert_eq!(control_events(&mut night_light, stale), [FAILED_EVENT]);

        // The tool re-creates its control on the rebuilt output (bound here
        // through a second connection, the most recent wl_output global).
        let mut recreated = GammaClient::connect(&mut server);
        let (control, events) = recreated.get_control(&mut server);
        assert_eq!(events, [(GAMMA_SIZE_EVENT, vec![LUT_SIZE])]);

        // The failed control goes first: the connector now has a control of
        // its own, so its tint is left in that control's charge.
        night_light.destroy(stale);
        server.roundtrip();
        assert_eq!(
            gamma_sets(&server),
            [],
            "a held connector is its current control's to restore"
        );

        // The tool exits before it sent its next ramp: the carried one is
        // still on the connector, so the last control restores it.
        recreated.destroy(control);
        server.roundtrip();
        assert_eq!(
            gamma_sets(&server),
            [(OUTPUT.to_owned(), LUT_SIZE, identity_ramp(LUT_SIZE))],
            "the last control on a tinted connector restores it"
        );

        // Restored once: another control's destroy has nothing left to undo.
        let (probe, _) = recreated.get_control(&mut server);
        recreated.destroy(probe);
        server.roundtrip();
        assert_eq!(gamma_sets(&server), []);
    }

    #[test]
    fn an_unplugged_connector_forgets_its_ramp() {
        let mut server = gamma_server();
        let mut client = GammaClient::connect(&mut server);
        let (stale, _) = client.get_control(&mut server);
        client.set_gamma(stale, &warm_ramp());
        server.roundtrip();
        assert_eq!(gamma_sets(&server).len(), 1);

        // The connector is unplugged: the rebuild drops its ramp.
        server.state.outputs.clear();
        assert_eq!(fail_stale_controls(&server.state), 1);
        // Plugged back in, it comes up with the default ramp, so the failed
        // control has nothing to restore on it.
        server.state.outputs.push(test_output(OUTPUT));
        assert_eq!(fail_stale_controls(&server.state), 0);
        client.destroy(stale);
        server.roundtrip();
        assert_eq!(
            gamma_sets(&server),
            [],
            "a replugged connector never carried the ramp"
        );
    }

    #[test]
    fn a_resized_lut_fails_the_control_on_the_layout_change() {
        let mut server = gamma_server();
        let mut client = GammaClient::connect(&mut server);
        let (stale, _) = client.get_control(&mut server);

        // A hotplug moved the output onto a CRTC with a larger LUT.
        server.state.gamma_sizes.insert(OUTPUT.to_owned(), 8);
        fail_stale_controls(&server.state);
        server.roundtrip();
        assert_eq!(control_events(&mut client, stale), [FAILED_EVENT]);

        // The output is free for a control of the new size.
        let (_, events) = client.get_control(&mut server);
        assert_eq!(events, [(GAMMA_SIZE_EVENT, vec![8])]);
    }

    #[test]
    fn a_second_gamma_control_fails_and_leaves_the_owner_in_charge() {
        let mut server = gamma_server();
        let mut owner = GammaClient::connect(&mut server);
        let mut probe = GammaClient::connect(&mut server);

        let (owner_control, events) = owner.get_control(&mut server);
        assert_eq!(events, [(GAMMA_SIZE_EVENT, vec![LUT_SIZE])]);
        let (probe_control, events) = probe.get_control(&mut server);
        assert_eq!(
            events,
            [(FAILED_EVENT, vec![])],
            "the output already has an exclusive gamma control"
        );

        owner.set_gamma(owner_control, &warm_ramp());
        probe.set_gamma(probe_control, &identity_ramp(LUT_SIZE));
        server.roundtrip();
        assert_eq!(
            gamma_sets(&server),
            [(OUTPUT.to_owned(), LUT_SIZE, warm_ramp())],
            "only the owner's ramp reaches the backend"
        );

        probe.destroy(probe_control);
        server.roundtrip();
        assert_eq!(
            gamma_sets(&server),
            [],
            "destroying a refused control must not reset the owner's tint"
        );

        owner.destroy(owner_control);
        server.roundtrip();
        assert_eq!(
            gamma_sets(&server),
            [(OUTPUT.to_owned(), LUT_SIZE, identity_ramp(LUT_SIZE))],
            "the owner restores the ramp it installed"
        );

        // The output is free again.
        let (_, events) = probe.get_control(&mut server);
        assert_eq!(events, [(GAMMA_SIZE_EVENT, vec![LUT_SIZE])]);
    }

    #[test]
    fn destroying_an_owner_that_never_set_a_ramp_restores_nothing() {
        let mut server = gamma_server();
        let mut probe = GammaClient::connect(&mut server);
        let (control, events) = probe.get_control(&mut server);
        assert_eq!(events, [(GAMMA_SIZE_EVENT, vec![LUT_SIZE])]);
        probe.destroy(control);
        server.roundtrip();
        assert_eq!(gamma_sets(&server), []);
    }

    #[test]
    fn a_resized_lut_fails_the_stale_control_and_frees_the_output() {
        let mut server = gamma_server();
        let mut client = GammaClient::connect(&mut server);
        let (stale, _) = client.get_control(&mut server);

        // A hotplug moved the output onto a CRTC with a larger LUT.
        server.state.gamma_sizes.insert(OUTPUT.to_owned(), 8);
        client.set_gamma(stale, &warm_ramp());
        server.roundtrip();
        let events: Vec<u16> = client
            .client
            .events()
            .into_iter()
            .filter(|event| event.sender == stale)
            .map(|event| event.opcode)
            .collect();
        assert_eq!(events, [FAILED_EVENT]);
        assert_eq!(gamma_sets(&server), []);

        let (_, events) = client.get_control(&mut server);
        assert_eq!(events, [(GAMMA_SIZE_EVENT, vec![8])]);
    }

    #[test]
    fn a_gamma_control_for_a_gone_output_fails_without_touching_another() {
        let mut server = Server::new();
        let display = server.display.handle();
        server.state.gamma_owners = Some(init_gamma_control(&display));
        let unplugged = test_output("UNPLUGGED-1");
        let global = unplugged.create_global::<JwmWaylandState>(&display);
        let mut client = GammaClient::connect(&mut server);
        display.remove_global::<JwmWaylandState>(global);
        drop(unplugged);

        // Another monitor is still connected: it must not be substituted.
        server.state.outputs.push(test_output("OTHER-1"));
        let (_, events) = client.get_control(&mut server);
        assert_eq!(events, [(FAILED_EVENT, vec![])]);

        // With no output at all the request used to return without
        // initializing the new_id, which aborts dispatch in wayland-backend.
        server.state.outputs.clear();
        let (control, events) = client.get_control(&mut server);
        assert_eq!(events, [(FAILED_EVENT, vec![])]);
        client.destroy(control);
        server.roundtrip();
        assert_eq!(gamma_sets(&server), []);
    }

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
