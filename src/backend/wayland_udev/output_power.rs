//! wlr-output-power-management for the KMS backend.
use std::collections::{HashMap, VecDeque};

use log::info;

use smithay::output::{Output, WeakOutput};
use smithay::reexports::wayland_protocols_wlr::output_power_management::v1::server::{
    zwlr_output_power_manager_v1::{self, ZwlrOutputPowerManagerV1},
    zwlr_output_power_v1::{self, ZwlrOutputPowerV1},
};
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, Weak,
};

use crate::backend::api::BackendEvent;
use crate::backend::wayland::state::JwmWaylandState;

pub struct OutputPowerManagerData;

pub struct OutputPowerData {
    output: Option<WeakOutput>,
}

struct OutputPower {
    on: bool,
    controls: Vec<Weak<ZwlrOutputPowerV1>>,
}

pub(crate) struct OutputPowerRequest {
    pub(crate) output: WeakOutput,
    control: Weak<ZwlrOutputPowerV1>,
}

#[derive(Default)]
pub(crate) struct OutputPowerState {
    // Output equality is object identity, including across a same-name rebuild.
    outputs: HashMap<Output, OutputPower>,
    // Entries are paired in order with BackendEvent::OutputPowerSet. A retired
    // output or failed control invalidates its request without changing order.
    pub(crate) pending: VecDeque<OutputPowerRequest>,
}

impl OutputPowerState {
    pub(crate) fn sync(&mut self, modes: impl IntoIterator<Item = (Output, bool)>) -> bool {
        // A snapshot contains at most the small set of current connectors.
        // Keep output identity without indexing another map by Output.
        let mut modes: Vec<_> = modes.into_iter().collect();
        let mut sent = false;
        self.outputs.retain(|output, power| {
            let mut on = None;
            modes.retain(|(current, mode)| {
                if current != output {
                    return true;
                }
                // Preserve the original map's last-value-wins semantics.
                on = Some(*mode);
                false
            });
            let Some(on) = on else {
                for control in power
                    .controls
                    .iter()
                    .filter_map(|control| control.upgrade().ok())
                {
                    control.failed();
                    sent = true;
                }
                return false;
            };
            sent |= publish_mode(power, on);
            true
        });
        self.outputs.extend(modes.into_iter().map(|(output, on)| {
            (
                output,
                OutputPower {
                    on,
                    controls: Vec::new(),
                },
            )
        }));
        sent
    }

    pub(crate) fn confirm(&mut self, output: &Output, on: bool) -> bool {
        self.outputs
            .get_mut(output)
            .is_some_and(|power| publish_mode(power, on))
    }

    pub(crate) fn fail(&mut self, request: OutputPowerRequest) -> bool {
        for queued in &mut self.pending {
            if queued.control == request.control {
                queued.output = WeakOutput::default();
            }
        }
        let Some(power) = request
            .output
            .upgrade()
            .and_then(|output| self.outputs.get_mut(&output))
        else {
            return false;
        };
        let Some(index) = power
            .controls
            .iter()
            .position(|control| control == &request.control)
        else {
            return false;
        };
        if let Ok(control) = power.controls.swap_remove(index).upgrade() {
            control.failed();
            return true;
        }
        false
    }
}

fn protocol_mode(on: bool) -> zwlr_output_power_v1::Mode {
    if on {
        zwlr_output_power_v1::Mode::On
    } else {
        zwlr_output_power_v1::Mode::Off
    }
}

fn publish_mode(power: &mut OutputPower, on: bool) -> bool {
    if power.on == on {
        return false;
    }
    power.on = on;
    let mut sent = false;
    power.controls.retain(|control| {
        let Ok(control) = control.upgrade() else {
            return false;
        };
        control.mode(protocol_mode(on));
        sent = true;
        true
    });
    sent
}

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
                let output = Output::from_resource(&output)
                    .filter(|output| state.outputs.contains(output))
                    .filter(|output| state.output_power.outputs.contains_key(output));
                let power = data_init.init(
                    id,
                    OutputPowerData {
                        output: output.as_ref().map(Output::downgrade),
                    },
                );
                if let Some(entry) =
                    output.and_then(|output| state.output_power.outputs.get_mut(&output))
                {
                    // Queries commonly connect, read the initial mode, and
                    // exit. Drop expired watchers even if power never changes.
                    entry.controls.retain(Weak::is_alive);
                    entry.controls.push(power.downgrade());
                    power.mode(protocol_mode(entry.on));
                } else {
                    power.failed();
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
        let zwlr_output_power_v1::Request::SetMode { mode } = request else {
            return;
        };
        let Some(output) = data.output.as_ref().and_then(WeakOutput::upgrade) else {
            return;
        };
        let control = resource.downgrade();
        if !state
            .output_power
            .outputs
            .get(&output)
            .is_some_and(|power| power.controls.contains(&control))
        {
            return;
        }
        let on = match mode.into_result() {
            Ok(zwlr_output_power_v1::Mode::Off) => false,
            Ok(zwlr_output_power_v1::Mode::On) => true,
            _ => {
                resource.post_error(
                    zwlr_output_power_v1::Error::InvalidMode,
                    "invalid output power mode",
                );
                return;
            }
        };
        let output_name = output.name();
        info!("[udev/wayland] DPMS on={on} requested for {output_name}");
        state.output_power.pending.push_back(OutputPowerRequest {
            output: output.downgrade(),
            control,
        });
        state.push_event(BackendEvent::OutputPowerSet { output_name, on });
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        resource: &ZwlrOutputPowerV1,
        data: &OutputPowerData,
    ) {
        if let Some(power) = data
            .output
            .as_ref()
            .and_then(WeakOutput::upgrade)
            .and_then(|output| state.output_power.outputs.get_mut(&output))
        {
            let control = resource.downgrade();
            power.controls.retain(|candidate| candidate != &control);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{OutputPowerState, init_output_power_management};
    use crate::backend::api::BackendEvent;
    use crate::backend::wayland::state::JwmWaylandState;
    use crate::backend::wayland_udev::image_copy_capture::wire_test_client::{
        Client as RawClient, Server, test_output,
    };

    /// zwlr_output_power_manager_v1.get_output_power
    const GET_OUTPUT_POWER: u16 = 0;
    /// zwlr_output_power_v1.set_mode
    const SET_MODE: u16 = 0;
    /// zwlr_output_power_v1.destroy
    const DESTROY: u16 = 1;
    /// zwlr_output_power_v1 events
    const MODE_EVENT: u16 = 0;
    const FAILED_EVENT: u16 = 1;

    fn control_events(client: &mut RawClient, control: u32) -> Vec<(u16, Vec<u32>)> {
        client
            .events()
            .into_iter()
            .filter(|event| event.sender == control)
            .map(|event| (event.opcode, event.args))
            .collect()
    }

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
    fn power_snapshot_preserves_output_identity_and_last_mode() {
        let a = test_output("DP-1");
        let b = test_output("DP-1");
        let mut power = OutputPowerState::default();
        assert!(!power.sync([(a.clone(), false), (a.clone(), true), (b.clone(), false)]));
        assert!(power.outputs[&a].on);
        assert!(!power.outputs[&b].on);
        assert!(!power.sync([(a.clone(), true), (a.clone(), false)]));
        assert!(!power.outputs[&a].on);
        assert!(!power.outputs.contains_key(&b));
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
    fn power_mode_is_published_after_confirmation_to_existing_and_new_controls() {
        let mut server = Server::new();
        let display = server.display.handle();
        init_output_power_management(&display);
        let live = test_output("LIVE-1");
        let _global = live.create_global::<JwmWaylandState>(&display);
        server.state.outputs.push(test_output("OTHER-1"));
        server.state.outputs.push(live.clone());
        server.state.output_power.sync([(live.clone(), true)]);
        let mut client = server.connect();
        let manager = client.bind("zwlr_output_power_manager_v1", 1);
        let output = client.bind("wl_output", 4);
        server.roundtrip();
        client.events();
        let control = client.new_id();
        client.request(manager, GET_OUTPUT_POWER, &[control, output]);
        client.request(control, SET_MODE, &[0]);
        server.roundtrip();
        assert_eq!(
            control_events(&mut client, control),
            [(MODE_EVENT, vec![1])]
        );
        assert_eq!(power_sets(&server), [("LIVE-1".to_string(), false)]);
        let request = server.state.output_power.pending.pop_front().unwrap();
        assert_eq!(&request.output.upgrade().unwrap(), &live);
        assert!(server.state.output_power.confirm(&live, false));
        server.roundtrip();
        assert_eq!(
            control_events(&mut client, control),
            [(MODE_EVENT, vec![0])]
        );

        let watcher = client.new_id();
        client.request(manager, GET_OUTPUT_POWER, &[watcher, output]);
        server.roundtrip();
        assert_eq!(
            control_events(&mut client, watcher),
            [(MODE_EVENT, vec![0])]
        );
        assert!(server.state.output_power.confirm(&live, true));
        server.roundtrip();
        assert_eq!(
            client
                .events()
                .into_iter()
                .filter(|event| event.opcode == MODE_EVENT
                    && [control, watcher].contains(&event.sender))
                .count(),
            2
        );
    }

    #[test]
    fn power_control_cleanup_preserves_an_accepted_write() {
        let mut server = Server::new();
        let display = server.display.handle();
        init_output_power_management(&display);
        let live = test_output("LIVE-1");
        let _global = live.create_global::<JwmWaylandState>(&display);
        server.state.outputs.push(live.clone());
        server.state.output_power.sync([(live.clone(), true)]);

        let mut watcher = server.connect();
        let manager = watcher.bind("zwlr_output_power_manager_v1", 1);
        let output = watcher.bind("wl_output", 4);
        let watcher_control = watcher.new_id();
        watcher.request(manager, GET_OUTPUT_POWER, &[watcher_control, output]);
        server.roundtrip();
        assert_eq!(
            control_events(&mut watcher, watcher_control),
            [(MODE_EVENT, vec![1])]
        );

        for destroy in [true, false] {
            let mut client = server.connect();
            let manager = client.bind("zwlr_output_power_manager_v1", 1);
            let output = client.bind("wl_output", 4);
            let control = client.new_id();
            client.request(manager, GET_OUTPUT_POWER, &[control, output]);
            server.roundtrip();
            assert_eq!(server.state.output_power.outputs[&live].controls.len(), 2);
            client.request(control, SET_MODE, &[0]);
            if destroy {
                client.request(control, DESTROY, &[]);
                server.roundtrip();
            } else {
                server.roundtrip();
                drop(client);
                server.roundtrip();
            }

            assert_eq!(server.state.output_power.outputs[&live].controls.len(), 1);
            assert_eq!(power_sets(&server), [("LIVE-1".to_string(), false)]);
            let request = server.state.output_power.pending.pop_front().unwrap();
            assert_eq!(request.output.upgrade().as_ref(), Some(&live));
        }

        assert!(server.state.output_power.confirm(&live, false));
        server.roundtrip();
        assert_eq!(
            control_events(&mut watcher, watcher_control),
            [(MODE_EVENT, vec![0])]
        );
    }

    #[test]
    fn failed_power_write_retires_requester_and_cancels_its_queued_writes() {
        let mut server = Server::new();
        let display = server.display.handle();
        init_output_power_management(&display);
        let live = test_output("LIVE-1");
        let _global = live.create_global::<JwmWaylandState>(&display);
        server.state.outputs.push(live.clone());
        server.state.output_power.sync([(live.clone(), true)]);
        let mut client = server.connect();
        let manager = client.bind("zwlr_output_power_manager_v1", 1);
        let output = client.bind("wl_output", 4);
        let control = client.new_id();
        client.request(manager, GET_OUTPUT_POWER, &[control, output]);
        client.request(control, SET_MODE, &[0]);
        client.request(control, SET_MODE, &[1]);
        server.roundtrip();
        assert_eq!(
            control_events(&mut client, control),
            [(MODE_EVENT, vec![1])]
        );
        assert_eq!(power_sets(&server).len(), 2);
        let request = server.state.output_power.pending.pop_front().unwrap();
        assert!(server.state.output_power.fail(request));
        assert!(
            server
                .state
                .output_power
                .pending
                .pop_front()
                .unwrap()
                .output
                .upgrade()
                .is_none()
        );
        client.request(control, SET_MODE, &[0]);
        let watcher = client.new_id();
        client.request(manager, GET_OUTPUT_POWER, &[watcher, output]);
        server.roundtrip();
        let events = client.events();
        assert!(events.iter().any(|event| event.sender == watcher
            && event.opcode == MODE_EVENT
            && event.args == [1]));
        assert_eq!(
            events
                .iter()
                .filter(|event| event.sender == control)
                .map(|event| event.opcode)
                .collect::<Vec<_>>(),
            [FAILED_EVENT]
        );
        assert!(power_sets(&server).is_empty());
    }

    #[test]
    fn power_control_never_targets_a_same_name_replacement() {
        let mut server = Server::new();
        let display = server.display.handle();
        init_output_power_management(&display);
        let old = test_output("REPLACED-1");
        let _global = old.create_global::<JwmWaylandState>(&display);
        server.state.outputs.push(old.clone());
        server.state.output_power.sync([(old.clone(), true)]);
        let mut client = server.connect();
        let manager = client.bind("zwlr_output_power_manager_v1", 1);
        let output = client.bind("wl_output", 4);
        let control = client.new_id();
        client.request(manager, GET_OUTPUT_POWER, &[control, output]);
        client.request(control, SET_MODE, &[0]);
        server.roundtrip();
        assert_eq!(
            control_events(&mut client, control),
            [(MODE_EVENT, vec![1])]
        );
        assert_eq!(power_sets(&server).len(), 1);

        let replacement = test_output("REPLACED-1");
        server.state.outputs = vec![replacement.clone()];
        assert!(
            server
                .state
                .output_power
                .sync([(replacement.clone(), true)])
        );
        let request = server.state.output_power.pending.pop_front().unwrap();
        assert_eq!(request.output.upgrade().as_ref(), Some(&old));
        assert_ne!(request.output.upgrade().as_ref(), Some(&replacement));
        assert!(!server.state.output_power.fail(request));
        client.request(control, SET_MODE, &[0]);
        server.roundtrip();
        assert_eq!(
            control_events(&mut client, control),
            [(FAILED_EVENT, vec![])]
        );
        assert!(power_sets(&server).is_empty());
    }
}
