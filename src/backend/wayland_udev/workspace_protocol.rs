/// ext-workspace-v1 protocol implementation for JWM.
///
/// Maps JWM's bitmask tag system to the ext-workspace protocol:
/// - Each monitor = one workspace group, keyed by its output
/// - Each tag bit position = one workspace
/// - Active tags in the bitmask = active workspaces
/// - A workspace's `id` is `<output name>-<tag index>`: the output's
///   connector name, not the policy monitor index, because a group outlives
///   the index shifts of a monitor hotplug and ids must stay unique
///
/// [`WorkspaceState::sync_monitors`] publishes JWM's monitors and their
/// active tags to every bound manager; a manager bound later starts from the
/// last published state. [`WorkspaceState::rebind_outputs`] follows that
/// state to rebuilt, moved or newly created outputs between two publishes. `activate`
/// requests queue on their manager and reach policy on the manager's
/// `commit`, as the protocol requires. Only `activate` is advertised: a
/// JWM view never has no tag, and toggling one off has no policy event.
use crate::sync_ext::MutexExt;
use std::sync::{Arc, Mutex};

use log::{debug, info};

use smithay::output::Output;
use smithay::reexports::wayland_protocols::ext::workspace::v1::server::{
    ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1},
    ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1},
    ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
};
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, Weak,
};

use crate::backend::api::BackendEvent;
use crate::backend::wayland::state::JwmWaylandState;

/// Global data of the ext_workspace_manager_v1 global.
pub struct WorkspaceGlobalData;

/// User data of one bound manager: what it was sent, and what its client
/// asked for since the last commit.
#[derive(Default)]
pub struct WorkspaceManagerData {
    groups: Mutex<Vec<SentGroup>>,
    /// `(group output, tag index)` per `activate` since the last `commit`.
    pending_activations: Mutex<Vec<(Output, usize)>>,
}

pub struct WorkspaceGroupData {
    /// The output this group stands for.
    pub output: Output,
}
unsafe impl Send for WorkspaceGroupData {}

pub struct WorkspaceHandleData {
    /// The manager this workspace was announced on; its activation requests
    /// wait there for the manager's commit.
    pub manager: Weak<ExtWorkspaceManagerV1>,
    /// The output of the workspace's group.
    pub output: Output,
    pub tag_index: usize,
}
unsafe impl Send for WorkspaceHandleData {}

/// One JWM monitor as the protocol last published it.
#[derive(Clone)]
struct PublishedMonitor {
    output: Output,
    /// Policy monitor index, reported back in `WorkspaceActivate`.
    monitor: usize,
    active_tags: u32,
}

/// A group one manager was sent.
struct SentGroup {
    output: Output,
    group: ExtWorkspaceGroupHandleV1,
    /// The client's wl_output objects the group was sent `output_enter` for.
    entered: Vec<WlOutput>,
    /// One workspace per tag, in tag order.
    workspaces: Vec<ExtWorkspaceHandleV1>,
    /// Active-tag mask last sent for this group.
    active_tags: u32,
}

impl SentGroup {
    /// Retire the group: every workspace leaves it and is removed first, as
    /// the protocol requires, then the group itself.
    fn remove(&self) {
        for workspace in &self.workspaces {
            self.group.workspace_leave(workspace);
            workspace.removed();
        }
        self.group.removed();
    }

    /// Send `state` for every workspace whose active bit changed. Returns
    /// whether anything was sent.
    fn set_active_tags(&mut self, active_tags: u32) -> bool {
        let mut sent = false;
        for (tag, workspace) in self.workspaces.iter().enumerate() {
            let Some(bit) = tag_bit(tag) else {
                continue;
            };
            if (self.active_tags ^ active_tags) & bit != 0 {
                workspace.state(workspace_state(active_tags & bit != 0));
                sent = true;
            }
        }
        self.active_tags = active_tags;
        sent
    }

    /// Enter every wl_output the client bound for this group's output since
    /// the group was sent. Returns whether anything was sent.
    fn enter_new_outputs(&mut self, client: &Client) -> bool {
        self.entered.retain(Resource::is_alive);
        let mut sent = false;
        for wl_output in self.output.client_outputs(client) {
            if !self.entered.contains(&wl_output) {
                self.group.output_enter(&wl_output);
                self.entered.push(wl_output);
                sent = true;
            }
        }
        sent
    }
}

fn tag_bit(tag: usize) -> Option<u32> {
    u32::try_from(tag)
        .ok()
        .and_then(|tag| 1u32.checked_shl(tag))
}

fn workspace_state(active: bool) -> ext_workspace_handle_v1::State {
    if active {
        ext_workspace_handle_v1::State::Active
    } else {
        ext_workspace_handle_v1::State::empty()
    }
}

/// Match JWM monitors, as `Backend::compositor_set_monitors` receives them
/// (`(index, x, y, width, height, active_tags)`), to the outputs at their
/// origin. A monitor without an output there is left out.
fn match_monitors_to_outputs(
    outputs: &[Output],
    monitors: &[(u32, i32, i32, u32, u32, u32)],
) -> Vec<PublishedMonitor> {
    let mut published: Vec<PublishedMonitor> = Vec::with_capacity(monitors.len());
    for &(index, x, y, _, _, active_tags) in monitors {
        let output = outputs.iter().find(|output| {
            let location = output.current_location();
            (location.x, location.y) == (x, y)
                && !published.iter().any(|monitor| monitor.output == **output)
        });
        let Some(output) = output else {
            debug!("[workspace] monitor {index} at ({x},{y}) has no output; not published");
            continue;
        };
        published.push(PublishedMonitor {
            output: output.clone(),
            monitor: index as usize,
            active_tags,
        });
    }
    published
}

/// Tracks bound workspace manager clients so we can push state updates.
#[derive(Clone)]
pub struct WorkspaceState {
    inner: Arc<Mutex<WorkspaceStateInner>>,
}

struct WorkspaceStateInner {
    managers: Vec<Weak<ExtWorkspaceManagerV1>>,
    tags_length: usize,
    /// Monitors as last published. `None` until the first sync; a manager
    /// bound before that sees the backend's outputs with no active tags.
    monitors: Option<Vec<PublishedMonitor>>,
    /// Policy's monitor list as the last sync received it, including the
    /// monitors that had no output then. A rebind matches those to the
    /// outputs that appear afterwards.
    policy_monitors: Vec<(u32, i32, i32, u32, u32, u32)>,
}

impl WorkspaceState {
    pub fn new(tags_length: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(WorkspaceStateInner {
                managers: Vec::new(),
                tags_length,
                monitors: None,
                policy_monitors: Vec::new(),
            })),
        }
    }

    pub fn tags_length(&self) -> usize {
        self.inner.lock_safe().tags_length
    }

    /// Set the number of workspaces per group: the configured tag count,
    /// which `Config::tags_length` already clamps to 1..=31. Sent groups
    /// keep their workspaces until the next
    /// [`sync_monitors`](Self::sync_monitors) or
    /// [`rebind_outputs`](Self::rebind_outputs), which re-send every group
    /// whose workspace count differs, so a config reload that changes the
    /// count reaches taskbars with the next publish.
    pub fn set_tags_length(&self, tags_length: usize) {
        self.inner.lock_safe().tags_length = tags_length;
    }

    fn add_manager(&self, manager: &ExtWorkspaceManagerV1) {
        let weak = manager.downgrade();
        let mut inner = self.inner.lock_safe();
        // Drop entries for managers whose client disconnected, so repeated
        // binds (every taskbar restart) cannot grow the list without bound.
        inner.managers.retain(Weak::is_alive);
        inner.managers.push(weak);
    }

    /// Forget `manager` once it has been finished, along with any entry
    /// whose client is gone.
    fn remove_manager(&self, manager: &ExtWorkspaceManagerV1) {
        let id = manager.id();
        self.inner
            .lock_safe()
            .managers
            .retain(|weak| weak.is_alive() && weak.id() != id);
    }

    /// Publish JWM's monitors to every bound manager. `monitors` is the list
    /// `Backend::compositor_set_monitors` receives, `(index, x, y, width,
    /// height, active_tags)` per policy monitor; each is matched to the
    /// output at its origin among `outputs`. Groups of outputs that are gone
    /// are removed, new outputs get a group, a group with a workspace count
    /// other than [`tags_length`](Self::tags_length) is sent again, and
    /// workspaces whose active bit changed get a new `state`, followed by
    /// `done` on each manager that was sent anything.
    ///
    /// Call it after every monitor or tag change. Returns whether events
    /// were queued, so the caller can flush clients.
    pub fn sync_monitors(
        &self,
        dh: &DisplayHandle,
        outputs: &[Output],
        monitors: &[(u32, i32, i32, u32, u32, u32)],
    ) -> bool {
        let published = match_monitors_to_outputs(outputs, monitors);
        let mut inner = self.inner.lock_safe();
        inner.policy_monitors = monitors.to_vec();
        publish(dh, &mut inner, published)
    }

    /// Follow the last published monitors to the outputs of the same
    /// connector name among `outputs`, and bring every bound manager in
    /// line: a group whose output was rebuilt (a new `Output` of the same
    /// connector) moves to the new one, a group whose connector is gone is
    /// removed, and a newly bound wl_output of a kept group is entered.
    /// Policy monitor indices and active tags are kept. Nothing happens
    /// before the first [`sync_monitors`](Self::sync_monitors).
    ///
    /// For the backend's output refreshes, which carry no policy geometry:
    /// an output moved by wlr-randr or kanshi is at its new origin before
    /// policy publishes it there, so matching the last published origins
    /// would hand a moved output's group another monitor's tags, or retire
    /// it.
    ///
    /// A policy monitor left without a group (it had no output when policy
    /// last synced, or its output's connector is gone) is matched by origin
    /// to an output no group follows. On a hotplug policy publishes against
    /// the outputs of the old KMS state, before the rebuild creates the new
    /// connector's output, and nothing publishes again afterwards; a
    /// connector swapped in at a removed one's origin, or a dock re-plugged
    /// after every output was gone, is the same. Returns whether events
    /// were queued.
    pub fn rebind_outputs(&self, dh: &DisplayHandle, outputs: &[Output]) -> bool {
        let mut inner = self.inner.lock_safe();
        let Some(monitors) = inner.monitors.as_deref() else {
            return false;
        };
        let mut rebound: Vec<PublishedMonitor> = Vec::with_capacity(monitors.len());
        for monitor in monitors {
            let name = monitor.output.name();
            let output = outputs.iter().find(|output| {
                output.name() == name && !rebound.iter().any(|kept| kept.output == **output)
            });
            let Some(output) = output else {
                debug!("[workspace] output {name} is gone; its group is not published");
                continue;
            };
            rebound.push(PublishedMonitor {
                output: output.clone(),
                monitor: monitor.monitor,
                active_tags: monitor.active_tags,
            });
        }
        let ungrouped: Vec<_> = inner
            .policy_monitors
            .iter()
            .copied()
            .filter(|&(index, ..)| !rebound.iter().any(|kept| kept.monitor == index as usize))
            .collect();
        if !ungrouped.is_empty() {
            let unclaimed: Vec<Output> = outputs
                .iter()
                .filter(|output| !rebound.iter().any(|kept| kept.output == **output))
                .cloned()
                .collect();
            rebound.extend(match_monitors_to_outputs(&unclaimed, &ungrouped));
            // Groups keep policy's monitor order, as a sync publishes them.
            rebound.sort_by_key(|monitor| monitor.monitor);
        }
        publish(dh, &mut inner, rebound)
    }

    /// The monitors a newly bound manager is sent: the last published ones,
    /// or before the first sync every output in order with no active tags.
    fn bind_monitors(&self, outputs: &[Output]) -> Vec<PublishedMonitor> {
        let inner = self.inner.lock_safe();
        match &inner.monitors {
            Some(monitors) => monitors.clone(),
            None => outputs
                .iter()
                .enumerate()
                .map(|(monitor, output)| PublishedMonitor {
                    output: output.clone(),
                    monitor,
                    active_tags: 0,
                })
                .collect(),
        }
    }

    /// The policy monitor index of `output`'s group, for an activation
    /// committed now. Before the first sync, the output's position among
    /// the backend's outputs.
    fn monitor_for_output(&self, output: &Output, outputs: &[Output]) -> Option<usize> {
        let inner = self.inner.lock_safe();
        match &inner.monitors {
            Some(monitors) => monitors
                .iter()
                .find(|monitor| monitor.output == *output)
                .map(|monitor| monitor.monitor),
            None => outputs.iter().position(|candidate| candidate == output),
        }
    }

    #[cfg(test)]
    fn manager_count(&self) -> usize {
        self.inner.lock_safe().managers.len()
    }
}

/// Bring every bound manager in line with `published`, follow each manager
/// that was sent anything with `done`, and store `published` as the state
/// later binds and activations use. Returns whether events were queued.
fn publish(
    dh: &DisplayHandle,
    inner: &mut WorkspaceStateInner,
    published: Vec<PublishedMonitor>,
) -> bool {
    inner.managers.retain(Weak::is_alive);
    let tags_length = inner.tags_length;
    let mut sent = false;
    for manager in inner.managers.iter().filter_map(|weak| weak.upgrade().ok()) {
        if sync_manager(dh, &manager, &published, tags_length) {
            manager.done();
            sent = true;
        }
    }
    inner.monitors = Some(published);
    sent
}

/// Bring one manager's groups in line with `monitors`. Returns whether any
/// event was sent; the caller follows with `done`.
fn sync_manager(
    dh: &DisplayHandle,
    manager: &ExtWorkspaceManagerV1,
    monitors: &[PublishedMonitor],
    tags_length: usize,
) -> bool {
    let (Some(client), Some(data)) = (manager.client(), manager.data::<WorkspaceManagerData>())
    else {
        return false;
    };
    let mut groups = data.groups.lock_safe();
    let mut sent = false;
    groups.retain(|group| {
        let published = monitors
            .iter()
            .any(|monitor| monitor.output == group.output);
        if !published {
            group.remove();
            sent = true;
        }
        published
    });
    for monitor in monitors {
        let mut kept = groups
            .iter()
            .position(|group| group.output == monitor.output);
        if let Some(index) = kept
            && groups[index].workspaces.len() != tags_length
        {
            // The tag count changed (a config reload). The group is retired
            // and announced again with the new count, the way a re-plugged
            // output is, so it holds exactly one workspace per live tag: a
            // stale one past the count would activate a tag policy no longer
            // has, and a missing one would leave a tag unreachable.
            groups.remove(index).remove();
            sent = true;
            kept = None;
        }
        match kept.and_then(|index| groups.get_mut(index)) {
            Some(group) => {
                sent |= group.enter_new_outputs(&client);
                sent |= group.set_active_tags(monitor.active_tags);
            }
            None => {
                if let Some(group) = send_group(dh, &client, manager, monitor, tags_length) {
                    groups.push(group);
                    sent = true;
                }
            }
        }
    }
    sent
}

/// Announce a group for `monitor` with one workspace per tag, each with its
/// current active state. `None` when the client is gone.
fn send_group(
    dh: &DisplayHandle,
    client: &Client,
    manager: &ExtWorkspaceManagerV1,
    monitor: &PublishedMonitor,
    tags_length: usize,
) -> Option<SentGroup> {
    let version = manager.version();
    // Child resources are created server-side via Client::create_resource
    // and attached with the manager's workspace_group / workspace events,
    // then linked into the group with workspace_enter.
    let group = client
        .create_resource::<ExtWorkspaceGroupHandleV1, _, JwmWaylandState>(
            dh,
            version,
            WorkspaceGroupData {
                output: monitor.output.clone(),
            },
        )
        .ok()?;
    manager.workspace_group(&group);
    group.capabilities(ext_workspace_group_handle_v1::GroupCapabilities::empty());
    let mut sent = SentGroup {
        output: monitor.output.clone(),
        group,
        entered: Vec::new(),
        workspaces: Vec::with_capacity(tags_length),
        active_tags: monitor.active_tags,
    };
    sent.enter_new_outputs(client);

    for tag_idx in 0..tags_length {
        let ws = client
            .create_resource::<ExtWorkspaceHandleV1, _, JwmWaylandState>(
                dh,
                version,
                WorkspaceHandleData {
                    manager: manager.downgrade(),
                    output: monitor.output.clone(),
                    tag_index: tag_idx,
                },
            )
            .ok()?;
        manager.workspace(&ws);
        // Keyed on the output: a kept group's ids are frozen here, while
        // policy monitor indices shift when a monitor is removed, so an
        // index-based id could be handed to a later output a second time.
        ws.id(format!("{}-{tag_idx}", monitor.output.name()));
        ws.name(format!("{}", tag_idx + 1));
        // Not `deactivate`: the capability tells taskbars the request works,
        // and nothing turns one tag of a JWM view off (see the request).
        ws.capabilities(ext_workspace_handle_v1::WorkspaceCapabilities::Activate);
        let active = tag_bit(tag_idx).is_some_and(|bit| monitor.active_tags & bit != 0);
        ws.state(workspace_state(active));
        sent.group.workspace_enter(&ws);
        sent.workspaces.push(ws);
    }
    Some(sent)
}

/// Initialize the ext-workspace-v1 global.
pub fn init_workspace_protocol(dh: &DisplayHandle, tags_length: usize) -> WorkspaceState {
    let state = WorkspaceState::new(tags_length);
    dh.create_global::<JwmWaylandState, ExtWorkspaceManagerV1, _>(1, WorkspaceGlobalData);
    info!(
        "[udev/wayland] ext-workspace-v1 global registered (tags={})",
        tags_length
    );
    state
}

// --- GlobalDispatch for the manager ---

impl GlobalDispatch<ExtWorkspaceManagerV1, WorkspaceGlobalData> for JwmWaylandState {
    fn bind(
        state: &mut Self,
        handle: &DisplayHandle,
        _client: &Client,
        resource: New<ExtWorkspaceManagerV1>,
        _global_data: &WorkspaceGlobalData,
        data_init: &mut DataInit<'_, Self>,
    ) {
        state.record_protocol_bind("ext_workspace_manager_v1");
        let manager = data_init.init(resource, WorkspaceManagerData::default());

        if let Some(ref ws_state) = state.workspace_state {
            ws_state.add_manager(&manager);
            // One workspace group per monitor, with one workspace per JWM
            // tag, carrying the last published active tags.
            let monitors = ws_state.bind_monitors(&state.outputs);
            sync_manager(handle, &manager, &monitors, ws_state.tags_length());
        }

        manager.done();
    }

    /// A sandboxed (wp_security_context) client must not see which tags the
    /// user views on each named output, nor switch them: the same desktop
    /// control the foreign-toplevel globals are withheld for.
    fn can_view(client: Client, _global_data: &WorkspaceGlobalData) -> bool {
        !crate::backend::wayland::state::client_is_sandboxed(&client)
    }
}

// --- Dispatch for the manager ---

impl Dispatch<ExtWorkspaceManagerV1, WorkspaceManagerData> for JwmWaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ExtWorkspaceManagerV1,
        request: ext_workspace_manager_v1::Request,
        data: &WorkspaceManagerData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            ext_workspace_manager_v1::Request::Commit => {
                // The requests before a commit apply atomically, so queued
                // activations reach policy only now, in request order. Each
                // names its group's monitor as of this commit.
                let activations = std::mem::take(&mut *data.pending_activations.lock_safe());
                for (output, tag_index) in activations {
                    // tag_index is supplied by us when the workspace is
                    // created, but defending the shift is cheap: a shift by
                    // 32 or more would otherwise wrap onto a real tag.
                    let Some(tag_mask) = tag_bit(tag_index) else {
                        log::warn!(
                            "[udev/wayland] workspace tag_index={tag_index} out of range for u32 \
                             mask; dropping"
                        );
                        continue;
                    };
                    let monitor = state
                        .workspace_state
                        .as_ref()
                        .and_then(|ws_state| ws_state.monitor_for_output(&output, &state.outputs));
                    let Some(monitor) = monitor else {
                        debug!(
                            "[udev/wayland] workspace activate dropped: output {} is no longer \
                             published",
                            output.name()
                        );
                        continue;
                    };
                    info!("[udev/wayland] workspace activate: monitor={monitor} tag={tag_index}");
                    state.push_event(BackendEvent::WorkspaceActivate {
                        monitor: Some(monitor),
                        tag_mask,
                    });
                }
            }
            ext_workspace_manager_v1::Request::Stop => {
                // The protocol expects `finished` (a destructor event) once
                // `stop` is processed. No later updates are queued for this
                // manager, so finish it now and stop tracking it.
                resource.finished();
                if let Some(ref ws_state) = state.workspace_state {
                    ws_state.remove_manager(resource);
                }
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
        _state: &mut Self,
        _client: &Client,
        _resource: &ExtWorkspaceHandleV1,
        request: ext_workspace_handle_v1::Request,
        data: &WorkspaceHandleData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            ext_workspace_handle_v1::Request::Activate => {
                // Applied on the manager's next commit, never before it.
                let Ok(manager) = data.manager.upgrade() else {
                    return;
                };
                if let Some(manager_data) = manager.data::<WorkspaceManagerData>() {
                    manager_data
                        .pending_activations
                        .lock_safe()
                        .push((data.output.clone(), data.tag_index));
                }
            }
            ext_workspace_handle_v1::Request::Deactivate => {
                // The capability is not advertised, so the protocol lets the
                // request be ignored.
                debug!(
                    "[udev/wayland] workspace deactivate ignored: output={} tag={}",
                    data.output.name(),
                    data.tag_index
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

#[cfg(test)]
mod tests {
    use super::{WorkspaceState, init_workspace_protocol, match_monitors_to_outputs};
    use crate::backend::api::BackendEvent;
    use crate::backend::wayland::state::JwmWaylandState;
    use crate::backend::wayland_udev::output_management::wire_test_support::{
        WireClient, WireEvent, test_output,
    };
    use smithay::output::Output;
    use smithay::reexports::wayland_server::DisplayHandle;

    // ext_workspace_manager_v1 requests and events.
    const MANAGER_COMMIT: u16 = 0;
    const MANAGER_STOP: u16 = 1;
    const MANAGER_WORKSPACE_GROUP: u16 = 0;
    const MANAGER_WORKSPACE: u16 = 1;
    const MANAGER_DONE: u16 = 2;
    const MANAGER_FINISHED: u16 = 3;
    // ext_workspace_group_handle_v1 events.
    const GROUP_WORKSPACE_LEAVE: u16 = 4;
    const GROUP_REMOVED: u16 = 5;
    // ext_workspace_handle_v1 requests and events.
    const WORKSPACE_ACTIVATE: u16 = 1;
    const WORKSPACE_DEACTIVATE: u16 = 2;
    const WORKSPACE_ID: u16 = 0;
    const WORKSPACE_STATE: u16 = 3;
    const WORKSPACE_CAPABILITIES: u16 = 4;
    const WORKSPACE_REMOVED: u16 = 5;
    /// ext_workspace_handle_v1.state bit `active`.
    const ACTIVE: u32 = 1;
    /// ext_workspace_handle_v1.workspace_capabilities bit `activate`.
    const CAN_ACTIVATE: u32 = 1;

    fn output_at(name: &str, origin: (i32, i32)) -> Output {
        let output = test_output(name);
        output.change_current_state(None, None, None, Some(origin.into()));
        output
    }

    /// A client of a test-registered workspace global with three tags per
    /// monitor, over the given outputs.
    fn workspace_client(outputs: &[(&str, (i32, i32))]) -> (WireClient, WorkspaceState) {
        let mut registered = None;
        let client = WireClient::new(|display, state| {
            let workspaces = init_workspace_protocol(display, 3);
            state.workspace_state = Some(workspaces.clone());
            state.outputs = outputs
                .iter()
                .map(|(name, origin)| output_at(name, *origin))
                .collect();
            registered = Some(workspaces);
        });
        (
            client,
            registered.expect("workspace protocol is registered"),
        )
    }

    fn sync(
        client: &WireClient,
        workspaces: &WorkspaceState,
        monitors: &[(u32, i32, i32, u32, u32, u32)],
    ) -> bool {
        workspaces.sync_monitors(&client.display_handle(), &client.state.outputs, monitors)
    }

    /// Objects `manager` announced with `opcode`, in order.
    fn announced(events: &[WireEvent], manager: u32, opcode: u16) -> Vec<u32> {
        events
            .iter()
            .filter(|event| event.sender == manager && event.opcode == opcode)
            .map(|event| event.word(0))
            .collect()
    }

    fn opcodes(events: &[WireEvent], object: u32) -> Vec<u16> {
        events
            .iter()
            .filter(|event| event.sender == object)
            .map(|event| event.opcode)
            .collect()
    }

    /// `(workspace, state bits)` for every state event of `workspaces`.
    fn states(events: &[WireEvent], workspaces: &[u32]) -> Vec<(u32, u32)> {
        events
            .iter()
            .filter(|event| event.opcode == WORKSPACE_STATE && workspaces.contains(&event.sender))
            .map(|event| (event.sender, event.word(0)))
            .collect()
    }

    /// The `id` string sent for each of `workspaces`, in event order.
    fn ids(events: &[WireEvent], workspaces: &[u32]) -> Vec<String> {
        events
            .iter()
            .filter(|event| event.opcode == WORKSPACE_ID && workspaces.contains(&event.sender))
            .map(|event| {
                // A wire string is its length (with the NUL), then its bytes.
                let len = usize::try_from(event.word(0)).expect("string length fits usize");
                let bytes = &event.payload[4..4 + len.saturating_sub(1)];
                String::from_utf8(bytes.to_vec()).expect("workspace id is UTF-8")
            })
            .collect()
    }

    fn position(events: &[WireEvent], sender: u32, opcode: u16, word: Option<u32>) -> usize {
        events
            .iter()
            .position(|event| {
                event.sender == sender
                    && event.opcode == opcode
                    && word.is_none_or(|word| event.word(0) == word)
            })
            .unwrap_or_else(|| panic!("event {sender}/{opcode} was sent: {events:?}"))
    }

    /// Drain the `WorkspaceActivate` events policy would receive.
    fn activations(client: &WireClient) -> Vec<(usize, u32)> {
        let mut queue = client
            .state
            .pending_events
            .lock()
            .expect("backend event queue");
        let mut activations = Vec::new();
        queue.retain(|event| match event {
            BackendEvent::WorkspaceActivate { monitor, tag_mask } => {
                activations.push((
                    monitor.expect("ext-workspace names its group's monitor"),
                    *tag_mask,
                ));
                false
            }
            _ => true,
        });
        activations
    }

    #[test]
    fn sync_publishes_active_tags_and_follows_hotplug() {
        let (mut client, workspaces) = workspace_client(&[("WIRE-1", (0, 0))]);
        let manager = client.bind("ext_workspace_manager_v1", 1);
        let initial = client.roundtrip();
        let [first_group] = announced(&initial, manager, MANAGER_WORKSPACE_GROUP)[..] else {
            panic!("one group per output: {initial:?}");
        };
        let first = announced(&initial, manager, MANAGER_WORKSPACE);
        assert_eq!(first.len(), 3);
        assert_eq!(ids(&initial, &first), ["WIRE-1-0", "WIRE-1-1", "WIRE-1-2"]);
        // Before the first sync no tag is known to be active.
        assert_eq!(
            states(&initial, &first),
            [(first[0], 0), (first[1], 0), (first[2], 0)]
        );

        // Regression: tag changes never reached the protocol, so waybar's
        // ext/workspaces never showed an active workspace.
        assert!(sync(&client, &workspaces, &[(0, 0, 0, 1920, 1080, 0b010)]));
        let events = client.roundtrip();
        assert_eq!(states(&events, &first), [(first[1], ACTIVE)]);
        assert_eq!(opcodes(&events, manager), [MANAGER_DONE]);

        assert!(sync(&client, &workspaces, &[(0, 0, 0, 1920, 1080, 0b001)]));
        let events = client.roundtrip();
        assert_eq!(states(&events, &first), [(first[0], ACTIVE), (first[1], 0)]);

        // Unchanged: nothing is sent.
        assert!(!sync(&client, &workspaces, &[(0, 0, 0, 1920, 1080, 0b001)]));
        assert!(opcodes(&client.roundtrip(), manager).is_empty());

        // Hotplug: the new output gets a group carrying its active tag.
        client.state.outputs.push(output_at("WIRE-2", (1920, 0)));
        let both = [
            (0, 0, 0, 1920, 1080, 0b001),
            (1, 1920, 0, 1920, 1080, 0b100),
        ];
        assert!(sync(&client, &workspaces, &both));
        let events = client.roundtrip();
        let [second_group] = announced(&events, manager, MANAGER_WORKSPACE_GROUP)[..] else {
            panic!("the hotplugged output gets one group: {events:?}");
        };
        let second = announced(&events, manager, MANAGER_WORKSPACE);
        assert_eq!(
            states(&events, &second),
            [(second[0], 0), (second[1], 0), (second[2], ACTIVE)]
        );
        let second_ids = ids(&events, &second);
        assert_eq!(second_ids, ["WIRE-2-0", "WIRE-2-1", "WIRE-2-2"]);
        assert!(opcodes(&events, first_group).is_empty());

        // Unplug: each workspace leaves its group and is removed, then the
        // group is removed.
        client.state.outputs.remove(0);
        assert!(sync(
            &client,
            &workspaces,
            &[(0, 1920, 0, 1920, 1080, 0b100)]
        ));
        let events = client.roundtrip();
        let group_removed = position(&events, first_group, GROUP_REMOVED, None);
        for workspace in &first {
            let left = position(
                &events,
                first_group,
                GROUP_WORKSPACE_LEAVE,
                Some(*workspace),
            );
            let removed = position(&events, *workspace, WORKSPACE_REMOVED, None);
            assert!(left < removed && removed < group_removed, "{events:?}");
        }
        assert!(opcodes(&events, second_group).is_empty());

        // A manager bound now starts from the published state.
        let late = client.bind("ext_workspace_manager_v1", 1);
        let events = client.roundtrip();
        assert_eq!(
            announced(&events, late, MANAGER_WORKSPACE_GROUP).len(),
            1,
            "only the remaining output has a group"
        );
        let late_workspaces = announced(&events, late, MANAGER_WORKSPACE);
        assert_eq!(
            states(&events, &late_workspaces),
            [
                (late_workspaces[0], 0),
                (late_workspaces[1], 0),
                (late_workspaces[2], ACTIVE)
            ]
        );

        // Regression: ids came from the policy monitor index. WIRE-2 kept its
        // group (ids from index 1) while it moved to index 0, so WIRE-1,
        // re-plugged and appended as monitor 1, was sent the same ids as
        // WIRE-2's live workspaces.
        client.state.outputs.push(output_at("WIRE-1", (0, 0)));
        assert!(sync(
            &client,
            &workspaces,
            &[
                (0, 1920, 0, 1920, 1080, 0b100),
                (1, 0, 0, 1920, 1080, 0b001)
            ]
        ));
        let events = client.roundtrip();
        let replugged = announced(&events, manager, MANAGER_WORKSPACE);
        let replugged_ids = ids(&events, &replugged);
        assert_eq!(replugged_ids, ["WIRE-1-0", "WIRE-1-1", "WIRE-1-2"]);
        assert!(opcodes(&events, second_group).is_empty(), "WIRE-2 is kept");
        assert!(
            replugged_ids.iter().all(|id| !second_ids.contains(id)),
            "live workspace ids must stay unique: {replugged_ids:?} vs {second_ids:?}"
        );
    }

    #[test]
    fn activation_waits_for_commit_and_names_the_group_monitor() {
        let (mut client, workspaces) =
            workspace_client(&[("WIRE-1", (0, 0)), ("WIRE-2", (1920, 0))]);
        // Policy lists WIRE-2 first; groups follow policy's monitor order.
        assert!(!sync(
            &client,
            &workspaces,
            &[(0, 1920, 0, 1920, 1080, 1), (1, 0, 0, 1920, 1080, 1)],
        ));
        let manager = client.bind("ext_workspace_manager_v1", 1);
        let initial = client.roundtrip();
        let announced_workspaces = announced(&initial, manager, MANAGER_WORKSPACE);
        assert_eq!(announced_workspaces.len(), 6);
        let wire_1_third_tag = announced_workspaces[5];

        // Policy reorders its monitors: WIRE-1 is monitor 0 now.
        sync(
            &client,
            &workspaces,
            &[(0, 0, 0, 1920, 1080, 1), (1, 1920, 0, 1920, 1080, 1)],
        );
        client.roundtrip();

        client.request(wire_1_third_tag, WORKSPACE_ACTIVATE, &[]);
        client.roundtrip();
        assert!(
            activations(&client).is_empty(),
            "an activation must wait for the manager's commit"
        );

        client.request(manager, MANAGER_COMMIT, &[]);
        client.roundtrip();
        assert_eq!(
            activations(&client),
            [(0, 0b100)],
            "the activation names the group's monitor as of the commit"
        );

        // A commit with nothing queued changes nothing.
        client.request(manager, MANAGER_COMMIT, &[]);
        client.roundtrip();
        assert!(activations(&client).is_empty());
    }

    #[test]
    fn a_changed_tag_count_resends_every_group_with_the_new_count() {
        let (mut client, workspaces) =
            workspace_client(&[("WIRE-1", (0, 0)), ("WIRE-2", (1920, 0))]);
        let monitors = [
            (0, 0, 0, 1920, 1080, 0b010),
            (1, 1920, 0, 1920, 1080, 0b001),
        ];
        sync(&client, &workspaces, &monitors);
        let manager = client.bind("ext_workspace_manager_v1", 1);
        let initial = client.roundtrip();
        let old_groups = announced(&initial, manager, MANAGER_WORKSPACE_GROUP);
        assert_eq!(old_groups.len(), 2);
        let old_workspaces = announced(&initial, manager, MANAGER_WORKSPACE);
        assert_eq!(old_workspaces.len(), 6);

        // A config reload grows the count from three to five. Setting it
        // alone sends nothing; the next publish re-sends the groups.
        workspaces.set_tags_length(5);
        assert_eq!(workspaces.tags_length(), 5);
        assert!(opcodes(&client.roundtrip(), manager).is_empty());
        assert!(sync(&client, &workspaces, &monitors));
        let events = client.roundtrip();
        for (group, workspaces) in old_groups.iter().zip(old_workspaces.chunks(3)) {
            let group_removed = position(&events, *group, GROUP_REMOVED, None);
            for workspace in workspaces {
                let left = position(&events, *group, GROUP_WORKSPACE_LEAVE, Some(*workspace));
                let removed = position(&events, *workspace, WORKSPACE_REMOVED, None);
                assert!(left < removed && removed < group_removed, "{events:?}");
            }
        }
        let new_groups = announced(&events, manager, MANAGER_WORKSPACE_GROUP);
        assert_eq!(new_groups.len(), 2, "{events:?}");
        assert!(new_groups.iter().all(|group| !old_groups.contains(group)));
        let grown = announced(&events, manager, MANAGER_WORKSPACE);
        assert_eq!(
            ids(&events, &grown),
            [
                "WIRE-1-0", "WIRE-1-1", "WIRE-1-2", "WIRE-1-3", "WIRE-1-4", "WIRE-2-0", "WIRE-2-1",
                "WIRE-2-2", "WIRE-2-3", "WIRE-2-4",
            ]
        );
        // Each re-sent group carries its monitor's active tag.
        let active: Vec<u32> = states(&events, &grown)
            .into_iter()
            .filter(|(_, state)| *state == ACTIVE)
            .map(|(workspace, _)| workspace)
            .collect();
        assert_eq!(active, [grown[1], grown[5]]);
        assert_eq!(opcodes(&events, manager).last(), Some(&MANAGER_DONE));

        // The count matches now: an unchanged publish sends nothing.
        assert!(!sync(&client, &workspaces, &monitors));
        assert!(opcodes(&client.roundtrip(), manager).is_empty());

        // Shrinking works the same way, and a manager bound afterwards
        // starts with the new count.
        workspaces.set_tags_length(2);
        assert!(sync(&client, &workspaces, &monitors));
        let events = client.roundtrip();
        assert_eq!(announced(&events, manager, MANAGER_WORKSPACE).len(), 4);
        for group in &new_groups {
            position(&events, *group, GROUP_REMOVED, None);
        }
        let late = client.bind("ext_workspace_manager_v1", 1);
        let events = client.roundtrip();
        assert_eq!(announced(&events, late, MANAGER_WORKSPACE).len(), 4);
    }

    #[test]
    fn monitors_are_matched_to_the_output_at_their_origin() {
        let left = output_at("LEFT", (0, 0));
        let right = output_at("RIGHT", (1920, 0));
        let outputs = [left.clone(), right.clone()];
        let published = match_monitors_to_outputs(
            &outputs,
            &[
                (0, 1920, 0, 1920, 1080, 0b1),
                (1, 5000, 0, 1920, 1080, 0b1),
                (2, 0, 0, 1920, 1080, 0b10),
            ],
        );
        let matched: Vec<(String, usize, u32)> = published
            .iter()
            .map(|monitor| (monitor.output.name(), monitor.monitor, monitor.active_tags))
            .collect();
        assert_eq!(
            matched,
            [("RIGHT".to_owned(), 0, 0b1), ("LEFT".to_owned(), 2, 0b10)]
        );

        // Two monitors on one origin do not share an output.
        let mirrored = match_monitors_to_outputs(
            &[left],
            &[(0, 0, 0, 1920, 1080, 1), (1, 0, 0, 1920, 1080, 1)],
        );
        assert_eq!(mirrored.len(), 1);
    }

    /// Regression: an output refresh re-matched policy's last published
    /// monitors to the outputs by origin, but the backend moves an output
    /// (a wlr-randr or kanshi Apply) before policy publishes the new
    /// layout. Swapped outputs traded their groups' tags and monitor
    /// indices, and a moved output's group was removed and announced again.
    #[test]
    fn a_refresh_follows_outputs_by_name_not_by_their_stale_origin() {
        let (mut client, workspaces) = workspace_client(&[("DP-1", (0, 0)), ("DP-2", (1920, 0))]);
        let dh = client.display_handle();
        assert!(
            !workspaces.rebind_outputs(&dh, &client.state.outputs),
            "nothing is published before policy's first sync"
        );
        let monitors = [
            (0, 0, 0, 1920, 1080, 0b001),
            (1, 1920, 0, 1920, 1080, 0b010),
        ];
        sync(&client, &workspaces, &monitors);
        let manager = client.bind("ext_workspace_manager_v1", 1);
        let initial = client.roundtrip();
        let groups = announced(&initial, manager, MANAGER_WORKSPACE_GROUP);
        let all_workspaces = announced(&initial, manager, MANAGER_WORKSPACE);
        let [dp1_group, dp2_group] = groups[..] else {
            panic!("one group per output: {initial:?}");
        };
        assert_eq!(all_workspaces.len(), 6);

        // `wlr-randr --output DP-1 --pos 1920,0 --output DP-2 --pos 0,0`:
        // the outputs have moved, policy has not published yet.
        client.state.outputs[0].change_current_state(None, None, None, Some((1920, 0).into()));
        client.state.outputs[1].change_current_state(None, None, None, Some((0, 0).into()));
        assert!(!workspaces.rebind_outputs(&dh, &client.state.outputs));
        let events = client.roundtrip();
        assert!(opcodes(&events, manager).is_empty(), "{events:?}");
        assert!(states(&events, &all_workspaces).is_empty(), "{events:?}");
        // DP-1 is still policy's monitor 0 until policy says otherwise.
        client.request(all_workspaces[1], WORKSPACE_ACTIVATE, &[]);
        client.request(manager, MANAGER_COMMIT, &[]);
        client.roundtrip();
        assert_eq!(activations(&client), [(0, 0b010)]);

        // Policy publishes the swapped layout: the same groups match.
        assert!(!sync(
            &client,
            &workspaces,
            &[
                (0, 1920, 0, 1920, 1080, 0b001),
                (1, 0, 0, 1920, 1080, 0b010)
            ]
        ));

        // A plain move keeps the moved output's group as well.
        client.state.outputs[0].change_current_state(None, None, None, Some((3840, 0).into()));
        assert!(!workspaces.rebind_outputs(&dh, &client.state.outputs));
        let events = client.roundtrip();
        assert!(opcodes(&events, dp1_group).is_empty(), "{events:?}");
        assert!(opcodes(&events, manager).is_empty(), "{events:?}");

        // A connector that is gone loses its group; the other one stays.
        client.state.outputs.remove(1);
        assert!(workspaces.rebind_outputs(&dh, &client.state.outputs));
        let events = client.roundtrip();
        position(&events, dp2_group, GROUP_REMOVED, None);
        assert!(opcodes(&events, dp1_group).is_empty(), "{events:?}");
        assert_eq!(opcodes(&events, manager), [MANAGER_DONE]);
    }

    /// Regression: a rebind followed only the monitors that had a group. On
    /// a hotplug policy publishes against the old KMS state's outputs, before
    /// the rebuild creates the new connector's output, and nothing publishes
    /// again afterwards, so the new monitor never got a group until the next
    /// tag switch.
    #[test]
    fn a_refresh_gives_a_hotplugged_output_the_group_policy_published_for_it() {
        let (mut client, workspaces) = workspace_client(&[("DP-1", (0, 0))]);
        let dh = client.display_handle();
        let manager = client.bind("ext_workspace_manager_v1", 1);
        let initial = client.roundtrip();
        let [dp1_group] = announced(&initial, manager, MANAGER_WORKSPACE_GROUP)[..] else {
            panic!("one group per output: {initial:?}");
        };

        // DP-2 is plugged in: policy already sees it, the KMS outputs do not.
        let both = [
            (0, 0, 0, 1920, 1080, 0b001),
            (1, 1920, 0, 1920, 1080, 0b100),
        ];
        sync(&client, &workspaces, &both);
        let events = client.roundtrip();
        assert!(announced(&events, manager, MANAGER_WORKSPACE_GROUP).is_empty());

        // The rebuild creates DP-2's output, and one at an origin policy has
        // no monitor for, which stays without a group.
        client.state.outputs.push(output_at("DP-2", (1920, 0)));
        client.state.outputs.push(output_at("DP-3", (5000, 0)));
        assert!(workspaces.rebind_outputs(&dh, &client.state.outputs));
        let events = client.roundtrip();
        let [dp2_group] = announced(&events, manager, MANAGER_WORKSPACE_GROUP)[..] else {
            panic!("the hotplugged output gets one group: {events:?}");
        };
        let dp2 = announced(&events, manager, MANAGER_WORKSPACE);
        assert_eq!(ids(&events, &dp2), ["DP-2-0", "DP-2-1", "DP-2-2"]);
        assert_eq!(
            states(&events, &dp2),
            [(dp2[0], 0), (dp2[1], 0), (dp2[2], ACTIVE)]
        );
        assert!(opcodes(&events, dp1_group).is_empty(), "{events:?}");
        assert_eq!(opcodes(&events, manager).last(), Some(&MANAGER_DONE));
        // It is policy's monitor 1.
        client.request(dp2[0], WORKSPACE_ACTIVATE, &[]);
        client.request(manager, MANAGER_COMMIT, &[]);
        client.roundtrip();
        assert_eq!(activations(&client), [(1, 0b001)]);
        // Settled: a repeated refresh sends nothing.
        assert!(!workspaces.rebind_outputs(&dh, &client.state.outputs));

        // A connector swapped in at a removed one's origin: policy's publish
        // still matched the dying DP-2, the rebuild replaces it by HDMI-A-1.
        sync(&client, &workspaces, &both);
        client.state.outputs.truncate(1);
        client.state.outputs.push(output_at("HDMI-A-1", (1920, 0)));
        assert!(workspaces.rebind_outputs(&dh, &client.state.outputs));
        let events = client.roundtrip();
        position(&events, dp2_group, GROUP_REMOVED, None);
        let hdmi = announced(&events, manager, MANAGER_WORKSPACE);
        assert_eq!(
            ids(&events, &hdmi),
            ["HDMI-A-1-0", "HDMI-A-1-1", "HDMI-A-1-2"]
        );
        assert_eq!(states(&events, &hdmi)[2], (hdmi[2], ACTIVE));
        assert!(opcodes(&events, dp1_group).is_empty(), "{events:?}");

        // A dock carrying every output is unplugged, then plugged back in.
        // Policy publishes the returning layout while no output exists yet.
        client.state.outputs.clear();
        assert!(workspaces.rebind_outputs(&dh, &client.state.outputs));
        client.roundtrip();
        assert!(!sync(&client, &workspaces, &both));
        client.state.outputs = vec![output_at("DP-1", (0, 0)), output_at("DP-2", (1920, 0))];
        assert!(workspaces.rebind_outputs(&dh, &client.state.outputs));
        let events = client.roundtrip();
        assert_eq!(
            announced(&events, manager, MANAGER_WORKSPACE_GROUP).len(),
            2,
            "{events:?}"
        );
        let returned = announced(&events, manager, MANAGER_WORKSPACE);
        assert_eq!(
            ids(&events, &returned),
            ["DP-1-0", "DP-1-1", "DP-1-2", "DP-2-0", "DP-2-1", "DP-2-2"]
        );
        let active: Vec<u32> = states(&events, &returned)
            .into_iter()
            .filter(|(_, state)| *state == ACTIVE)
            .map(|(workspace, _)| workspace)
            .collect();
        assert_eq!(active, [returned[0], returned[5]]);
    }

    /// Regression: workspaces advertised `deactivate`, whose request only
    /// logged, so a taskbar that deactivates on click did nothing.
    #[test]
    fn workspaces_advertise_only_activate_and_ignore_deactivate() {
        let (mut client, workspaces) = workspace_client(&[("WIRE-1", (0, 0))]);
        sync(&client, &workspaces, &[(0, 0, 0, 1920, 1080, 0b101)]);
        let manager = client.bind("ext_workspace_manager_v1", 1);
        let initial = client.roundtrip();
        let announced_workspaces = announced(&initial, manager, MANAGER_WORKSPACE);
        let capabilities: Vec<u32> = initial
            .iter()
            .filter(|event| {
                event.opcode == WORKSPACE_CAPABILITIES
                    && announced_workspaces.contains(&event.sender)
            })
            .map(|event| event.word(0))
            .collect();
        assert_eq!(capabilities, [CAN_ACTIVATE; 3]);

        // A client that deactivates anyway changes nothing.
        client.request(announced_workspaces[2], WORKSPACE_DEACTIVATE, &[]);
        client.request(manager, MANAGER_COMMIT, &[]);
        let events = client.roundtrip();
        assert!(activations(&client).is_empty());
        assert!(states(&events, &announced_workspaces).is_empty());
    }

    #[test]
    fn sandboxed_clients_are_not_offered_the_workspace_manager() {
        // Tag activity per named output, and `activate` on any monitor.
        let register = |display: &DisplayHandle, _: &mut JwmWaylandState| {
            init_workspace_protocol(display, 3);
        };
        assert!(WireClient::new(register).advertises("ext_workspace_manager_v1"));
        assert!(!WireClient::new_sandboxed(register).advertises("ext_workspace_manager_v1"));
    }

    #[test]
    fn stop_is_answered_with_finished_and_forgets_the_manager() {
        let (mut client, _) = workspace_client(&[("WIRE-1", (0, 0))]);
        let tracked = |client: &WireClient| {
            client
                .state
                .workspace_state
                .as_ref()
                .map(|workspaces| workspaces.manager_count())
        };

        let manager = client.bind("ext_workspace_manager_v1", 1);
        let initial = client.roundtrip();
        assert!(
            initial
                .iter()
                .any(|event| event.sender == manager && event.opcode == MANAGER_DONE),
            "bind ends with ext_workspace_manager_v1.done: {initial:?}"
        );
        assert_eq!(tracked(&client), Some(1));

        // Regression: `stop` used to be ignored, so the client never got the
        // `finished` handshake and the manager stayed in the list forever.
        client.request(manager, MANAGER_STOP, &[]);
        let events = client.roundtrip();
        assert!(
            events
                .iter()
                .any(|event| event.sender == manager && event.opcode == MANAGER_FINISHED),
            "stop must be answered with ext_workspace_manager_v1.finished: {events:?}"
        );
        assert_eq!(tracked(&client), Some(0));

        // A later bind starts from a clean list instead of accumulating.
        client.bind("ext_workspace_manager_v1", 1);
        client.roundtrip();
        assert_eq!(tracked(&client), Some(1));
    }
}
