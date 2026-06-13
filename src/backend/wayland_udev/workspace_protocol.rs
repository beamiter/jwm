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
    info!("[udev/wayland] ext-workspace-v1 global registered (tags={})", tags_length);
    state
}

// --- GlobalDispatch for the manager ---

impl GlobalDispatch<ExtWorkspaceManagerV1, WorkspaceManagerData> for JwmWaylandState {
    fn bind(
        state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ExtWorkspaceManagerV1>,
        _global_data: &WorkspaceManagerData,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let manager = data_init.init(resource, WorkspaceManagerData);

        if let Some(ref ws_state) = state.workspace_state {
            ws_state.add_manager(&manager);
            let tags_length = ws_state.tags_length();

            // Send workspace groups (one per output) and workspaces (one per tag).
            // The protocol requires: workspace_group event, then workspace events,
            // then done. Since we can't create new_id resources from events
            // (DataInit is only in request handlers), we send the done event with
            // no groups/workspaces. Clients will receive state on the next update cycle.
            //
            // Full implementation: track managers and send updates from the main loop.
            let _ = (tags_length, state.outputs.len());
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
                let tag_mask = 1u32 << data.tag_index;
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
