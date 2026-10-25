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
    /// `None` for an inert control: its output was gone when it was created.
    pub output_name: Option<String>,
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
        state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrOutputPowerManagerV1>,
        _global_data: &OutputPowerManagerData,
        data_init: &mut DataInit<'_, Self>,
    ) {
        state.record_protocol_bind("zwlr_output_power_manager_v1");
        data_init.init(resource, OutputPowerManagerData);
    }

    /// Output power is privileged: a sandboxed (wp_security_context) client
    /// must not blank the displays.
    fn can_view(client: Client, _global_data: &OutputPowerManagerData) -> bool {
        !crate::backend::wayland::state::client_is_sandboxed(&client)
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
                // Only the requested, still-connected output. A stale wl_output
                // (its monitor was unplugged) used to fall back to the first
                // output, so a client blanking the gone display turned off a
                // different one; the protocol answers `failed` instead.
                let output_name = smithay::output::Output::from_resource(&output)
                    .filter(|output| state.outputs.contains(output))
                    .map(|output| output.name());
                let power = data_init.init(
                    id,
                    OutputPowerData {
                        output_name: output_name.clone(),
                    },
                );
                match output_name {
                    Some(_) => power.mode(zwlr_output_power_v1::Mode::On),
                    None => power.failed(),
                }
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
        // An inert control was already told `failed`; it never acts.
        let Some(output_name) = data.output_name.as_ref() else {
            return;
        };
        match request {
            zwlr_output_power_v1::Request::SetMode { mode } => match mode.into_result() {
                Ok(zwlr_output_power_v1::Mode::Off) => {
                    info!("[udev/wayland] DPMS off requested for {output_name}");
                    state.pending_events.lock_safe().push_back(
                        crate::backend::api::BackendEvent::OutputPowerSet {
                            output_name: output_name.clone(),
                            on: false,
                        },
                    );
                    resource.mode(zwlr_output_power_v1::Mode::Off);
                }
                Ok(zwlr_output_power_v1::Mode::On) => {
                    info!("[udev/wayland] DPMS on requested for {output_name}");
                    state.pending_events.lock_safe().push_back(
                        crate::backend::api::BackendEvent::OutputPowerSet {
                            output_name: output_name.clone(),
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

#[cfg(test)]
mod tests {
    use super::init_output_power_management;
    use crate::backend::api::BackendEvent;
    use crate::backend::wayland::state::JwmWaylandState;
    use crate::backend::wayland_udev::image_copy_capture::wire_test_client::{Server, test_output};

    /// zwlr_output_power_manager_v1.get_output_power
    const GET_OUTPUT_POWER: u16 = 0;
    /// zwlr_output_power_v1.set_mode
    const SET_MODE: u16 = 0;
    /// zwlr_output_power_v1 events
    const MODE_EVENT: u16 = 0;
    const FAILED_EVENT: u16 = 1;

    fn power_sets(server: &Server) -> Vec<(String, bool)> {
        server
            .backend_events
            .lock()
            .unwrap()
            .drain(..)
            .filter_map(|event| match event {
                BackendEvent::OutputPowerSet { output_name, on } => Some((output_name, on)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn power_control_for_a_gone_output_fails_without_touching_another() {
        let mut server = Server::new();
        let display = server.display.handle();
        init_output_power_management(&display);
        let unplugged = test_output("UNPLUGGED-1");
        let global = unplugged.create_global::<JwmWaylandState>(&display);
        let mut client = server.connect();
        let manager = client.bind("zwlr_output_power_manager_v1", 1);
        let output = client.bind("wl_output", 4);
        server.roundtrip();
        client.events();
        display.remove_global::<JwmWaylandState>(global);
        drop(unplugged);

        // Another monitor is still connected: it must not be substituted.
        server.state.outputs.push(test_output("OTHER-1"));
        let control = client.new_id();
        client.request(manager, GET_OUTPUT_POWER, &[control, output]);
        client.request(control, SET_MODE, &[0]);
        server.roundtrip();
        let events: Vec<u16> = client
            .events()
            .into_iter()
            .filter(|event| event.sender == control)
            .map(|event| event.opcode)
            .collect();
        assert_eq!(events, [FAILED_EVENT]);
        assert_eq!(power_sets(&server), []);
    }

    #[test]
    fn power_control_for_a_live_output_reaches_that_output() {
        let mut server = Server::new();
        let display = server.display.handle();
        init_output_power_management(&display);
        let live = test_output("LIVE-1");
        let _global = live.create_global::<JwmWaylandState>(&display);
        server.state.outputs.push(test_output("OTHER-1"));
        server.state.outputs.push(live);
        let mut client = server.connect();
        let manager = client.bind("zwlr_output_power_manager_v1", 1);
        let output = client.bind("wl_output", 4);
        server.roundtrip();
        client.events();
        let control = client.new_id();
        client.request(manager, GET_OUTPUT_POWER, &[control, output]);
        client.request(control, SET_MODE, &[0]);
        server.roundtrip();
        let events: Vec<u16> = client
            .events()
            .into_iter()
            .filter(|event| event.sender == control)
            .map(|event| event.opcode)
            .collect();
        assert_eq!(events, [MODE_EVENT, MODE_EVENT]);
        assert_eq!(power_sets(&server), [("LIVE-1".to_string(), false)]);
    }
}
