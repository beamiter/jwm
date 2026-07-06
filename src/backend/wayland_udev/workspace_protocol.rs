/// ext-workspace-v1 protocol implementation for JWM.
///
/// Maps JWM's bitmask tag system to the ext-workspace protocol:
/// - Each monitor = one workspace group
/// - Each tag bit position = one workspace
/// - Active tags in the bitmask = active workspaces
use crate::sync_ext::MutexExt;
use std::sync::{Arc, Mutex};

use log::info;

use smithay::reexports::wayland_protocols::ext::workspace::v1::server::{
    ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1},
    ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1},
    ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
};
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, Weak,
};

use crate::backend::wayland::state::JwmWaylandState;

pub struct WorkspaceManagerData;
unsafe impl Send for WorkspaceManagerData {}

pub struct WorkspaceGroupData {
    pub monitor_index: usize,
}
unsafe impl Send for WorkspaceGroupData {}

pub struct WorkspaceHandleData {
    pub monitor_index: usize,
    pub tag_index: usize,
}
unsafe impl Send for WorkspaceHandleData {}

/// Tracks bound workspace manager clients so we can push state updates.
#[derive(Clone)]
pub struct WorkspaceState {
    inner: Arc<Mutex<WorkspaceStateInner>>,
}

struct WorkspaceStateInner {
    managers: Vec<Weak<ExtWorkspaceManagerV1>>,
    tags_length: usize,
}

impl WorkspaceState {
    pub fn new(tags_length: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(WorkspaceStateInner {
                managers: Vec::new(),
                tags_length,
            })),
        }
    }

    pub fn tags_length(&self) -> usize {
        self.inner.lock_safe().tags_length
    }

    fn add_manager(&self, manager: &ExtWorkspaceManagerV1) {
        let weak = manager.downgrade();
        self.inner.lock_safe().managers.push(weak);
    }
}

/// Initialize the ext-workspace-v1 global.
pub fn init_workspace_protocol(dh: &DisplayHandle, tags_length: usize) -> WorkspaceState {
    let state = WorkspaceState::new(tags_length);
    dh.create_global::<JwmWaylandState, ExtWorkspaceManagerV1, _>(1, WorkspaceManagerData);
    info!(
        "[udev/wayland] ext-workspace-v1 global registered (tags={})",
        tags_length
    );
    state
}

// --- GlobalDispatch for the manager ---

impl GlobalDispatch<ExtWorkspaceManagerV1, WorkspaceManagerData> for JwmWaylandState {
    fn bind(
        state: &mut Self,
        handle: &DisplayHandle,
        client: &Client,
        resource: New<ExtWorkspaceManagerV1>,
        _global_data: &WorkspaceManagerData,
        data_init: &mut DataInit<'_, Self>,
    ) {
        state.record_protocol_bind("ext_workspace_manager_v1");
        let manager = data_init.init(resource, WorkspaceManagerData);

        if let Some(ref ws_state) = state.workspace_state {
            ws_state.add_manager(&manager);
            let tags_length = ws_state.tags_length();
            let version = manager.version();

            // One workspace group per output, with one workspace per JWM tag.
            // Child resources are created server-side via Client::create_resource
            // and attached with the manager's workspace_group / workspace events,
            // then linked into the group with workspace_enter.
            for (mon_idx, output) in state.outputs.iter().enumerate() {
                let Ok(group) = client
                    .create_resource::<ExtWorkspaceGroupHandleV1, _, JwmWaylandState>(
                        handle,
                        version,
                        WorkspaceGroupData {
                            monitor_index: mon_idx,
                        },
                    )
                else {
                    continue;
                };
                manager.workspace_group(&group);
                group.capabilities(ext_workspace_group_handle_v1::GroupCapabilities::empty());
                for wl_output in output.client_outputs(client) {
                    group.output_enter(&wl_output);
                }

                for tag_idx in 0..tags_length {
                    let Ok(ws) = client
                        .create_resource::<ExtWorkspaceHandleV1, _, JwmWaylandState>(
                            handle,
                            version,
                            WorkspaceHandleData {
                                monitor_index: mon_idx,
                                tag_index: tag_idx,
                            },
                        )
                    else {
                        continue;
                    };
                    manager.workspace(&ws);
                    ws.id(format!("{mon_idx}-{tag_idx}"));
                    ws.name(format!("{}", tag_idx + 1));
                    ws.capabilities(
                        ext_workspace_handle_v1::WorkspaceCapabilities::Activate
                            | ext_workspace_handle_v1::WorkspaceCapabilities::Deactivate,
                    );
                    ws.state(ext_workspace_handle_v1::State::empty());
                    group.workspace_enter(&ws);
                }
            }
        }

        manager.done();
    }
}

// --- Dispatch for the manager ---

impl Dispatch<ExtWorkspaceManagerV1, WorkspaceManagerData> for JwmWaylandState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ExtWorkspaceManagerV1,
        request: ext_workspace_manager_v1::Request,
        _data: &WorkspaceManagerData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            ext_workspace_manager_v1::Request::Commit => {
                // Client finished sending requests; process atomically.
                // Pending activate/deactivate requests are applied here.
            }
            ext_workspace_manager_v1::Request::Stop => {
                // Client no longer wants events. We'll send finished eventually.
            }
            _ => {}
        }
    }
}

// --- Dispatch for workspace group handle ---

impl Dispatch<ExtWorkspaceGroupHandleV1, WorkspaceGroupData> for JwmWaylandState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ExtWorkspaceGroupHandleV1,
        request: ext_workspace_group_handle_v1::Request,
        _data: &WorkspaceGroupData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            ext_workspace_group_handle_v1::Request::CreateWorkspace { workspace: _ } => {
                // JWM has fixed tag count, ignore dynamic creation.
            }
            ext_workspace_group_handle_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

// --- Dispatch for workspace handle ---

impl Dispatch<ExtWorkspaceHandleV1, WorkspaceHandleData> for JwmWaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ExtWorkspaceHandleV1,
        request: ext_workspace_handle_v1::Request,
        data: &WorkspaceHandleData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            ext_workspace_handle_v1::Request::Activate => {
                info!(
                    "[udev/wayland] workspace activate: monitor={} tag={}",
                    data.monitor_index, data.tag_index
                );
                // tag_index is supplied by us at bind time, but defending the
                // shift is cheap and prevents UB if the workspace count ever
                // grows past 32 (left-shift by ≥ width is undefined behavior
                // in debug, wraps to 0 in release — both wrong).
                let Some(tag_mask) = 1u32.checked_shl(data.tag_index as u32) else {
                    log::warn!(
                        "[udev/wayland] workspace tag_index={} out of range for u32 mask; dropping",
                        data.tag_index
                    );
                    return;
                };
                state.pending_events.lock_safe().push_back(
                    crate::backend::api::BackendEvent::WorkspaceActivate {
                        monitor: data.monitor_index,
                        tag_mask,
                    },
                );
            }
            ext_workspace_handle_v1::Request::Deactivate => {
                info!(
                    "[udev/wayland] workspace deactivate: monitor={} tag={}",
                    data.monitor_index, data.tag_index
                );
            }
            ext_workspace_handle_v1::Request::Assign { workspace_group: _ } => {
                // JWM doesn't support moving workspaces between groups.
            }
            ext_workspace_handle_v1::Request::Remove => {
                // JWM has fixed tags, ignore removal.
            }
            ext_workspace_handle_v1::Request::Destroy => {}
            _ => {}
        }
    }
}
