use std::collections::{HashMap, HashSet};
/// wlr-output-management-unstable-v1 protocol implementation for JWM.
///
/// Allows clients like wlr-randr and kanshi to enumerate outputs (modes, position,
/// scale, transform, adaptive sync) and apply configuration changes.
///
/// Enumeration is sent on manager bind: for each live output we create a head and
/// one mode object per supported mode, then report current mode/position/scale/
/// transform. [`OutputManagementState::refresh`] keeps every bound manager in
/// step with later output changes (hotplug, a finished Apply) and bumps the
/// configuration serial. Apply/Test cancel a configuration built against an
/// older serial, otherwise validate it against the live outputs and (for Apply)
/// route an `OutputConfigure` backend event that performs the real DRM
/// modeset / layout change on the compositor thread.
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use log::{debug, info, warn};

use smithay::output::Output;
use smithay::reexports::wayland_protocols_wlr::output_management::v1::server::{
    zwlr_output_configuration_head_v1::{self, ZwlrOutputConfigurationHeadV1},
    zwlr_output_configuration_v1::{self, ZwlrOutputConfigurationV1},
    zwlr_output_head_v1::{self, ZwlrOutputHeadV1},
    zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
    zwlr_output_mode_v1::{self, ZwlrOutputModeV1},
};
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::utils::Transform;

use crate::backend::api::OutputConfigChange;
use crate::backend::wayland::state::JwmWaylandState;
use crate::sync_ext::MutexExt;

static SERIAL_COUNTER: AtomicU32 = AtomicU32::new(1);

fn next_serial() -> u32 {
    SERIAL_COUNTER.fetch_add(1, Ordering::Relaxed)
}

// --- Data types ---

/// User data of one bound `zwlr_output_manager_v1`.
pub struct OutputManagerData {
    /// The registry this manager is tracked in, for its configurations.
    pub management: OutputManagementState,
}
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
}
unsafe impl Send for OutputModeData {}

pub struct OutputConfigData {
    /// The `done` serial the client built this configuration against.
    pub serial: u32,
    /// Registry holding the current serial; a configuration whose `serial`
    /// is older is cancelled instead of applied or tested.
    pub management: OutputManagementState,
    /// `test` and `apply` consume a configuration even when validation fails.
    /// Every later request except `destroy` is a protocol error.
    pub consumed: AtomicBool,
    /// Config-head objects created via `enable_head`.
    pub enabled_heads: Mutex<Vec<ZwlrOutputConfigurationHeadV1>>,
    /// Output names targeted by `disable_head`.
    pub disabled_heads: Mutex<Vec<String>>,
}
unsafe impl Send for OutputConfigData {}

fn admit_configuration_request(consumed: &AtomicBool, consumes: bool) -> bool {
    if consumes {
        consumed
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    } else {
        !consumed.load(Ordering::Relaxed)
    }
}

#[derive(Default, Clone)]
pub struct PendingHeadConfig {
    /// Mode chosen via `set_mode`, resolved to `(w, h, refresh_mhz)`.
    pub mode: Option<(i32, i32, i32)>,
    /// Mode chosen via `set_custom_mode`, as `(w, h, refresh_mhz)`.
    pub custom_mode: Option<(i32, i32, i32)>,
    pub position: Option<(i32, i32)>,
    /// wl_output transform numeric value (0..=7).
    pub transform: Option<i32>,
    pub scale: Option<f64>,
    pub adaptive_sync: Option<bool>,
}

pub struct OutputConfigHeadData {
    pub output_name: String,
    pub pending: Mutex<PendingHeadConfig>,
}
unsafe impl Send for OutputConfigHeadData {}

#[derive(Debug, Clone)]
struct OutputConfigValidationError {
    reason: String,
    output_name: Option<String>,
    field: Option<&'static str>,
    drm_property: Option<&'static str>,
    requested_value: Option<String>,
}

impl OutputConfigValidationError {
    fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            output_name: None,
            field: None,
            drm_property: None,
            requested_value: None,
        }
    }

    fn for_output(output_name: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            output_name: Some(output_name.into()),
            field: None,
            drm_property: None,
            requested_value: None,
        }
    }

    fn field(
        output_name: impl Into<String>,
        field: &'static str,
        drm_property: Option<&'static str>,
        requested_value: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            reason: reason.into(),
            output_name: Some(output_name.into()),
            field: Some(field),
            drm_property,
            requested_value: Some(requested_value.into()),
        }
    }

    fn into_rejection(
        self,
        serial: u32,
        action: &'static str,
    ) -> crate::backend::api::OutputManagementRejectedConfig {
        crate::backend::api::OutputManagementRejectedConfig {
            attempted_at_unix_ms: now_unix_ms(),
            serial,
            action: action.to_string(),
            reason: self.reason,
            output_name: self.output_name,
            field: self.field.map(str::to_string),
            drm_property: self.drm_property.map(str::to_string),
            requested_value: self.requested_value,
        }
    }
}

impl std::fmt::Display for OutputConfigValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn mode_value(w: i32, h: i32, refresh: i32) -> String {
    format!("{w}x{h}@{refresh}")
}

// --- Head registry ---

/// One mode of a head as published: `(width, height, refresh_mhz, preferred)`.
type ModeSnapshot = (i32, i32, i32, bool);

/// Everything a head reports about one output, captured by value so a later
/// refresh can tell what changed. Two outputs with equal snapshots look the
/// same to a client, so a KMS rebuild that recreates identical `Output`s
/// sends nothing.
#[derive(Clone, Debug, PartialEq)]
struct HeadSnapshot {
    name: String,
    description: String,
    physical_size: (i32, i32),
    make: String,
    model: String,
    serial_number: String,
    modes: Vec<ModeSnapshot>,
    /// Index into `modes`.
    current_mode: Option<usize>,
    enabled: bool,
    position: (i32, i32),
    transform: Transform,
    scale: f64,
}

impl HeadSnapshot {
    fn capture(output: &Output, soft_disabled: bool) -> Self {
        let props = output.physical_properties();
        let current = output.current_mode();
        let preferred = output.preferred_mode();
        let all_modes = output.modes();
        let modes = all_modes
            .iter()
            .map(|mode| {
                (
                    mode.size.w,
                    mode.size.h,
                    mode.refresh,
                    Some(*mode) == preferred,
                )
            })
            .collect();
        let location = output.current_location();
        Self {
            name: output.name(),
            description: output.description(),
            physical_size: (props.size.w, props.size.h),
            make: props.make,
            model: props.model,
            serial_number: props.serial_number,
            modes,
            current_mode: current.and_then(|mode| all_modes.iter().position(|m| *m == mode)),
            // A head is enabled when it is actively driving a CRTC. Outputs
            // soft-disabled by an earlier `disable_head` Apply are reported
            // as disabled.
            enabled: !soft_disabled,
            position: (location.x, location.y),
            transform: output.current_transform(),
            scale: output.current_scale().fractional_scale(),
        }
    }

    /// Whether `other` describes the same head with the same mode list, so
    /// the difference can be sent as property events on the existing head.
    /// Heads cannot rename themselves or drop modes in place.
    fn same_identity(&self, other: &Self) -> bool {
        self.name == other.name
            && self.description == other.description
            && self.physical_size == other.physical_size
            && self.make == other.make
            && self.model == other.model
            && self.serial_number == other.serial_number
            && self.modes == other.modes
    }
}

/// How one refresh moves every manager's heads from the published snapshot
/// to the current one.
#[derive(Debug, Default, PartialEq)]
struct HeadRefreshPlan {
    /// Heads to finish, by output name: the output is gone, or changed in a
    /// way a head cannot describe in place.
    remove: Vec<String>,
    /// Indices into the current snapshot of heads to introduce.
    add: Vec<usize>,
    /// `(published, current)` index pairs of heads whose properties changed.
    update: Vec<(usize, usize)>,
}

impl HeadRefreshPlan {
    fn is_empty(&self) -> bool {
        self.remove.is_empty() && self.add.is_empty() && self.update.is_empty()
    }
}

fn plan_head_refresh(published: &[HeadSnapshot], current: &[HeadSnapshot]) -> HeadRefreshPlan {
    let mut plan = HeadRefreshPlan::default();
    for (old_index, old) in published.iter().enumerate() {
        match current.iter().position(|new| new.name == old.name) {
            Some(new_index) if current[new_index].same_identity(old) => {
                if current[new_index] != *old {
                    plan.update.push((old_index, new_index));
                }
            }
            _ => plan.remove.push(old.name.clone()),
        }
    }
    for (new_index, new) in current.iter().enumerate() {
        let kept = published
            .iter()
            .any(|old| old.name == new.name && old.same_identity(new));
        if !kept {
            plan.add.push(new_index);
        }
    }
    plan
}

/// A head one manager was sent, with its mode objects in snapshot order.
struct SentHead {
    output_name: String,
    head: ZwlrOutputHeadV1,
    modes: Vec<ZwlrOutputModeV1>,
}

impl SentHead {
    /// Tell the client the head and its modes are gone; both become inert.
    fn finish(&self) {
        for mode in &self.modes {
            mode.finished();
        }
        self.head.finished();
    }

    /// Send the properties that differ between `old` and `new`, which share
    /// an identity (see [`HeadSnapshot::same_identity`]).
    fn send_changes(&self, old: &HeadSnapshot, new: &HeadSnapshot) {
        if old.enabled != new.enabled {
            self.head.enabled(i32::from(new.enabled));
        }
        if old.current_mode != new.current_mode
            && let Some(mode) = new.current_mode.and_then(|index| self.modes.get(index))
        {
            self.head.current_mode(mode);
        }
        if old.position != new.position {
            self.head.position(new.position.0, new.position.1);
        }
        if old.transform != new.transform {
            self.head.transform(new.transform.into());
        }
        if old.scale != new.scale {
            self.head.scale(new.scale);
        }
    }
}

struct ManagerHeads {
    manager: ZwlrOutputManagerV1,
    heads: Vec<SentHead>,
}

struct OutputManagementInner {
    /// Serial of the latest `done`. A configuration created with any other
    /// serial was built against outdated heads and is cancelled.
    serial: u32,
    /// The head state `serial` describes, one entry per output.
    published: Vec<HeadSnapshot>,
    managers: Vec<ManagerHeads>,
}

/// Every bound output manager with the heads it was sent, and the serial of
/// the latest `done`. Shared by the global, each manager and each
/// configuration, so an Apply or Test can tell whether the client saw the
/// current output layout.
#[derive(Clone)]
pub struct OutputManagementState {
    inner: Arc<Mutex<OutputManagementInner>>,
}

impl OutputManagementState {
    fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(OutputManagementInner {
                serial: next_serial(),
                published: Vec::new(),
                managers: Vec::new(),
            })),
        }
    }

    /// Serial of the latest `done` sent to the managers.
    pub fn serial(&self) -> u32 {
        self.inner.lock_safe().serial
    }

    /// Bring every bound manager in line with `outputs`: finish the heads of
    /// outputs that are gone, introduce heads for new ones, send changed
    /// properties (mode, position, transform, scale, enabled) on the rest,
    /// then `done` with a new serial. Call it after anything that can change
    /// the outputs: hotplug, a KMS rebuild, the end of an `OutputConfigure`.
    ///
    /// Cheap when nothing changed, which sends nothing and keeps the serial.
    /// Returns whether events were queued, so the caller can flush clients.
    pub fn refresh(
        &self,
        dh: &DisplayHandle,
        outputs: &[Output],
        soft_disabled: &HashSet<String>,
    ) -> bool {
        let current: Vec<HeadSnapshot> = outputs
            .iter()
            .map(|output| HeadSnapshot::capture(output, soft_disabled.contains(&output.name())))
            .collect();
        let mut inner = self.inner.lock_safe();
        inner.managers.retain(|entry| entry.manager.is_alive());
        let plan = plan_head_refresh(&inner.published, &current);
        if plan.is_empty() {
            return false;
        }
        let serial = next_serial();
        let inner = &mut *inner;
        for entry in &mut inner.managers {
            entry.apply(dh, &plan, &inner.published, &current);
            entry.manager.done(serial);
        }
        inner.serial = serial;
        inner.published = current;
        !inner.managers.is_empty()
    }

    /// Start tracking `manager` and send it every published head followed by
    /// `done` with the current serial. The caller refreshes first, so the
    /// published heads are the live outputs.
    fn add_manager(&self, dh: &DisplayHandle, client: &Client, manager: ZwlrOutputManagerV1) {
        let mut inner = self.inner.lock_safe();
        let heads = inner
            .published
            .iter()
            .filter_map(|snapshot| send_head(dh, client, &manager, snapshot))
            .collect();
        manager.done(inner.serial);
        inner.managers.push(ManagerHeads { manager, heads });
    }

    /// Stop tracking `manager` (after `stop`, or once it is destroyed).
    fn remove_manager(&self, manager: &ZwlrOutputManagerV1) {
        self.inner
            .lock_safe()
            .managers
            .retain(|entry| entry.manager != *manager && entry.manager.is_alive());
    }

    #[cfg(test)]
    fn manager_count(&self) -> usize {
        self.inner.lock_safe().managers.len()
    }
}

impl ManagerHeads {
    fn apply(
        &mut self,
        dh: &DisplayHandle,
        plan: &HeadRefreshPlan,
        published: &[HeadSnapshot],
        current: &[HeadSnapshot],
    ) {
        // A head the client already released needs no further events.
        self.heads.retain(|sent| sent.head.is_alive());
        for name in &plan.remove {
            if let Some(index) = self.heads.iter().position(|sent| sent.output_name == *name) {
                self.heads.remove(index).finish();
            }
        }
        for &(old_index, new_index) in &plan.update {
            let (old, new) = (&published[old_index], &current[new_index]);
            if let Some(sent) = self.heads.iter().find(|sent| sent.output_name == new.name) {
                sent.send_changes(old, new);
            }
        }
        let Some(client) = self.manager.client() else {
            return;
        };
        for &new_index in &plan.add {
            if let Some(sent) = send_head(dh, &client, &self.manager, &current[new_index]) {
                self.heads.push(sent);
            }
        }
    }
}

/// Initialize the wlr-output-management global. The returned registry is the
/// one [`OutputManagementState::refresh`] must be called on when outputs
/// change; the global, its managers and their configurations share it.
pub fn init_output_management(dh: &DisplayHandle) -> OutputManagementState {
    let management = OutputManagementState::new();
    dh.create_global::<JwmWaylandState, ZwlrOutputManagerV1, _>(4, management.clone());
    info!("[udev/wayland] zwlr-output-management-unstable-v1 global registered");
    management
}

// --- GlobalDispatch for the manager ---

impl GlobalDispatch<ZwlrOutputManagerV1, OutputManagementState> for JwmWaylandState {
    fn bind(
        state: &mut Self,
        dh: &DisplayHandle,
        client: &Client,
        resource: New<ZwlrOutputManagerV1>,
        global_data: &OutputManagementState,
        data_init: &mut DataInit<'_, Self>,
    ) {
        state.record_protocol_bind("zwlr_output_manager_v1");
        // Announce any output change the managers already bound have not
        // seen yet, so the new manager and the old ones share one serial.
        global_data.refresh(dh, &state.outputs, &state.soft_disabled_outputs);
        let manager = data_init.init(
            resource,
            OutputManagerData {
                management: global_data.clone(),
            },
        );
        global_data.add_manager(dh, client, manager);
    }

    /// Output configuration is privileged: a sandboxed (wp_security_context)
    /// client must not reconfigure the displays.
    fn can_view(client: Client, _global_data: &OutputManagementState) -> bool {
        !crate::backend::wayland::state::client_is_sandboxed(&client)
    }
}

/// Create a head (and its mode objects) for `snapshot` on `manager` and
/// report its current state. `None` when the client is gone.
fn send_head(
    dh: &DisplayHandle,
    client: &Client,
    manager: &ZwlrOutputManagerV1,
    snapshot: &HeadSnapshot,
) -> Option<SentHead> {
    let version = manager.version();
    let name = snapshot.name.clone();

    let Ok(head) = client.create_resource::<ZwlrOutputHeadV1, _, JwmWaylandState>(
        dh,
        version,
        OutputHeadData {
            output_name: name.clone(),
        },
    ) else {
        warn!("[output-mgmt] failed to create head resource for {name}");
        return None;
    };

    manager.head(&head);
    head.name(name.clone());
    head.description(snapshot.description.clone());
    head.physical_size(snapshot.physical_size.0, snapshot.physical_size.1);

    let mut modes = Vec::with_capacity(snapshot.modes.len());
    for &(width, height, refresh, preferred) in &snapshot.modes {
        let Ok(mode) = client.create_resource::<ZwlrOutputModeV1, _, JwmWaylandState>(
            dh,
            version,
            OutputModeData {
                output_name: name.clone(),
                width,
                height,
                refresh,
            },
        ) else {
            // Only a dead client refuses a resource; it needs no more events.
            return None;
        };
        head.mode(&mode);
        mode.size(width, height);
        mode.refresh(refresh);
        if preferred {
            mode.preferred();
        }
        modes.push(mode);
    }

    head.enabled(i32::from(snapshot.enabled));
    if let Some(mode) = snapshot.current_mode.and_then(|index| modes.get(index)) {
        head.current_mode(mode);
    }
    head.position(snapshot.position.0, snapshot.position.1);
    head.transform(snapshot.transform.into());
    head.scale(snapshot.scale);

    if version >= 2 {
        head.make(snapshot.make.clone());
        head.model(snapshot.model.clone());
        head.serial_number(snapshot.serial_number.clone());
    }

    if version >= 4 {
        // We do not track per-output adaptive sync activation here; report disabled.
        head.adaptive_sync(zwlr_output_head_v1::AdaptiveSyncState::Disabled);
    }

    Some(SentHead {
        output_name: name,
        head,
        modes,
    })
}

// --- Dispatch for the manager ---

impl Dispatch<ZwlrOutputManagerV1, OutputManagerData> for JwmWaylandState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        resource: &ZwlrOutputManagerV1,
        request: zwlr_output_manager_v1::Request,
        data: &OutputManagerData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_output_manager_v1::Request::CreateConfiguration { id, serial } => {
                data_init.init(
                    id,
                    OutputConfigData {
                        serial,
                        management: data.management.clone(),
                        consumed: AtomicBool::new(false),
                        enabled_heads: Mutex::new(Vec::new()),
                        disabled_heads: Mutex::new(Vec::new()),
                    },
                );
            }
            zwlr_output_manager_v1::Request::Stop => {
                // The protocol answers `stop` with the `finished` destructor
                // event, and clients such as nwg-displays or way-displays wait
                // for it before tearing down. Nothing else is queued for this
                // manager once it leaves the registry, so it can finish at once.
                data.management.remove_manager(resource);
                resource.finished();
            }
            _ => {}
        }
    }

    fn destroyed(
        _state: &mut Self,
        _client: ClientId,
        resource: &ZwlrOutputManagerV1,
        data: &OutputManagerData,
    ) {
        data.management.remove_manager(resource);
    }
}

/// Answer `cancelled` when `data` was built against heads that have changed
/// since, as the protocol requires for an outdated serial. Any change not
/// announced yet is announced first, so the client learns the serial to
/// retry with. Returns whether the configuration was cancelled.
fn cancel_outdated_configuration(
    state: &mut JwmWaylandState,
    dh: &DisplayHandle,
    resource: &ZwlrOutputConfigurationV1,
    data: &OutputConfigData,
    action: &'static str,
) -> bool {
    data.management
        .refresh(dh, &state.outputs, &state.soft_disabled_outputs);
    let current = data.management.serial();
    if data.serial == current {
        return false;
    }
    let error = OutputConfigValidationError::new(format!(
        "configuration serial {} is outdated (current serial {current}); the outputs changed",
        data.serial
    ));
    debug!("[output-mgmt] {action} cancelled: {error}");
    state.last_output_management_rejection = Some(error.into_rejection(data.serial, action));
    resource.cancelled();
    true
}

// --- Dispatch for configuration ---

impl Dispatch<ZwlrOutputConfigurationV1, OutputConfigData> for JwmWaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwlrOutputConfigurationV1,
        request: zwlr_output_configuration_v1::Request,
        data: &OutputConfigData,
        dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let consumes = matches!(
            &request,
            zwlr_output_configuration_v1::Request::Apply
                | zwlr_output_configuration_v1::Request::Test
        );
        let destroys = matches!(&request, zwlr_output_configuration_v1::Request::Destroy);
        if !destroys && !admit_configuration_request(&data.consumed, consumes) {
            resource.post_error(
                zwlr_output_configuration_v1::Error::AlreadyUsed,
                "output configuration was already applied or tested",
            );
            return;
        }

        match request {
            zwlr_output_configuration_v1::Request::EnableHead { id, head } => {
                let output_name = head
                    .data::<OutputHeadData>()
                    .map(|d| d.output_name.clone())
                    .unwrap_or_default();
                let config_head = data_init.init(
                    id,
                    OutputConfigHeadData {
                        output_name,
                        pending: Mutex::new(PendingHeadConfig::default()),
                    },
                );
                data.enabled_heads.lock_safe().push(config_head);
            }
            zwlr_output_configuration_v1::Request::DisableHead { head } => {
                if let Some(d) = head.data::<OutputHeadData>() {
                    data.disabled_heads.lock_safe().push(d.output_name.clone());
                }
            }
            zwlr_output_configuration_v1::Request::Apply => {
                if cancel_outdated_configuration(state, dh, resource, data, "apply") {
                    return;
                }
                match build_changes(state, data) {
                    Ok(changes) => {
                        debug!("[output-mgmt] apply: {} change(s)", changes.len());
                        // Queue an ack callback that fires after the udev backend
                        // finishes (or fails) the modeset. The wlr-output-management
                        // spec defines `succeeded` as "the configuration was applied",
                        // so reporting it before the modeset returns can lie to clients
                        // (kanshi, wlr-randr) about success of e.g. a rejected mode.
                        let res = resource.clone();
                        state.pending_output_acks.push_back(
                            crate::backend::wayland::state::PendingOutputAck {
                                on_complete: Box::new(move |ok| {
                                    if ok {
                                        res.succeeded();
                                    } else {
                                        res.failed();
                                    }
                                }),
                            },
                        );
                        state.push_event(crate::backend::api::BackendEvent::OutputConfigure {
                            changes,
                        });
                    }
                    Err(e) => {
                        warn!("[output-mgmt] apply rejected: {e}");
                        state.last_output_management_rejection =
                            Some(e.into_rejection(data.serial, "apply"));
                        resource.failed();
                    }
                }
            }
            zwlr_output_configuration_v1::Request::Test => {
                if cancel_outdated_configuration(state, dh, resource, data, "test") {
                    return;
                }
                match build_changes(state, data) {
                    Ok(_) => resource.succeeded(),
                    Err(e) => {
                        debug!("[output-mgmt] test rejected: {e}");
                        state.last_output_management_rejection =
                            Some(e.into_rejection(data.serial, "test"));
                        resource.failed();
                    }
                }
            }
            zwlr_output_configuration_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

/// Compare a `(width, height, refresh_mhz)` request to a smithay current mode.
/// Returns true when the request would actually change the mode (so a real
/// DRM modeset would be needed). A `refresh` of 0 means "any refresh" — i.e.
/// only width/height must match.
fn mode_is_change(current: Option<smithay::output::Mode>, requested: (i32, i32, i32)) -> bool {
    let (w, h, refresh) = requested;
    match current {
        None => true,
        Some(cur) => {
            !(cur.size.w == w
                && cur.size.h == h
                && (refresh == 0 || (cur.refresh - refresh).abs() <= 200))
        }
    }
}

fn output_extent_is_supported(position: (i32, i32), mode_size: (i32, i32)) -> bool {
    const MAX_FRAMEBUFFER_EXTENT: i32 = 32_768;
    let (x, y) = position;
    let (width, height) = mode_size;
    x >= 0
        && y >= 0
        && width > 0
        && height > 0
        && x.checked_add(width)
            .is_some_and(|right| right <= MAX_FRAMEBUFFER_EXTENT)
        && y.checked_add(height)
            .is_some_and(|bottom| bottom <= MAX_FRAMEBUFFER_EXTENT)
}

/// Project the physical framebuffer envelope after a configuration without
/// mutating KMS. Soft-disable retains geometry, matching `KmsState`.
pub(crate) fn proposed_output_framebuffer_size(
    current_outputs: &[(String, (i32, i32), (i32, i32))],
    changes: &[OutputConfigChange],
) -> Result<(u32, u32), String> {
    let mut layout = current_outputs
        .iter()
        .cloned()
        .map(|(name, origin, mode)| (name, (origin, mode)))
        .collect::<HashMap<_, _>>();

    for change in changes {
        let Some((origin, mode)) = layout.get_mut(&change.name) else {
            return Err(format!("unknown output '{}'", change.name));
        };
        if !change.enabled {
            continue;
        }
        if let Some(position) = change.position {
            *origin = position;
        }
        if let Some((width, height, _)) = change.mode {
            if width <= 0 || height <= 0 {
                return Err(format!(
                    "invalid mode {width}x{height} for output '{}'",
                    change.name
                ));
            }
            *mode = (width, height);
        }
    }

    let bounded_extent = |origin: i32, size: i32| {
        (i64::from(origin) + i64::from(size)).clamp(0, i64::from(i32::MAX)) as u32
    };
    let width = layout
        .values()
        .map(|(origin, mode)| bounded_extent(origin.0, mode.0))
        .max()
        .unwrap_or(1920)
        .max(1);
    let height = layout
        .values()
        .map(|(origin, mode)| bounded_extent(origin.1, mode.1))
        .max()
        .unwrap_or(1080)
        .max(1);
    Ok((width, height))
}

/// The advertised protocol head currently reports adaptive sync disabled and
/// the output transaction snapshot does not own VRR state. Reject either
/// explicit value instead of acknowledging a request the backend would
/// silently ignore.
fn adaptive_sync_request_supported(request: Option<bool>) -> bool {
    request.is_none()
}

/// Validate the pending configuration against live outputs and lower it into a
/// list of `OutputConfigChange`. Returns `Err` with a reason if invalid.
fn build_changes(
    state: &JwmWaylandState,
    data: &OutputConfigData,
) -> Result<Vec<OutputConfigChange>, OutputConfigValidationError> {
    let mut changes = Vec::new();
    let allow_modeset = crate::config::CONFIG
        .load()
        .behavior()
        .wlr_output_mgmt_allow_modeset;

    for config_head in data.enabled_heads.lock_safe().iter() {
        let Some(head_data) = config_head.data::<OutputConfigHeadData>() else {
            continue;
        };
        let name = head_data.output_name.clone();

        let output = state
            .outputs
            .iter()
            .find(|o| o.name() == name)
            .ok_or_else(|| {
                OutputConfigValidationError::for_output(&name, format!("unknown output '{name}'"))
            })?;

        let pending = head_data.pending.lock_safe().clone();

        // set_mode takes precedence over set_custom_mode; both express (w, h, refresh).
        let requested_mode = pending.mode.or(pending.custom_mode);
        if let Some((w, h, refresh)) = requested_mode {
            if w <= 0 || h <= 0 {
                return Err(OutputConfigValidationError::field(
                    &name,
                    "mode",
                    Some("MODE_ID"),
                    mode_value(w, h, refresh),
                    format!("invalid mode {w}x{h} for '{name}'"),
                ));
            }
            // For modes selected via set_mode, ensure they belong to the output.
            if pending.mode.is_some() {
                let known = output.modes().iter().any(|m| {
                    m.size.w == w
                        && m.size.h == h
                        && (refresh == 0 || (m.refresh - refresh).abs() <= 200)
                });
                if !known {
                    return Err(OutputConfigValidationError::field(
                        &name,
                        "mode",
                        Some("MODE_ID"),
                        mode_value(w, h, refresh),
                        format!("mode {w}x{h}@{refresh} not on '{name}'"),
                    ));
                }
            }
            // Reject up-front when a real modeset is requested but the safety
            // gate is closed. Without this, Apply would silently drop the mode
            // change at the KMS layer and still report succeeded() to the
            // client — lying about which fields were applied.
            if !allow_modeset && mode_is_change(output.current_mode(), (w, h, refresh)) {
                return Err(OutputConfigValidationError::field(
                    &name,
                    "mode",
                    Some("MODE_ID"),
                    mode_value(w, h, refresh),
                    format!(
                        "mode change to {w}x{h}@{refresh} for '{name}' rejected: \
                         behavior.wlr_output_mgmt_allow_modeset = false"
                    ),
                ));
            }
        }

        if let Some(t) = pending.transform {
            if !(0..=7).contains(&t) {
                return Err(OutputConfigValidationError::field(
                    &name,
                    "transform",
                    Some("rotation/reflection"),
                    t.to_string(),
                    format!("invalid transform {t} for '{name}'"),
                ));
            }
        }

        if let Some(s) = pending.scale {
            if s <= 0.0 {
                return Err(OutputConfigValidationError::field(
                    &name,
                    "scale",
                    None,
                    s.to_string(),
                    format!("invalid scale {s} for '{name}'"),
                ));
            }
        }

        if !adaptive_sync_request_supported(pending.adaptive_sync) {
            let requested = if pending.adaptive_sync == Some(true) {
                "enabled"
            } else {
                "disabled"
            };
            return Err(OutputConfigValidationError::field(
                &name,
                "adaptive_sync",
                Some("VRR_ENABLED"),
                requested,
                format!(
                    "adaptive sync request '{requested}' for '{name}' is not transactionally supported"
                ),
            ));
        }

        if pending.position.is_some() || requested_mode.is_some() {
            let position = pending.position.unwrap_or_else(|| {
                let current = output.current_location();
                (current.x, current.y)
            });
            let mode_size = requested_mode
                .map(|(width, height, _)| (width, height))
                .or_else(|| output.current_mode().map(|mode| (mode.size.w, mode.size.h)))
                .ok_or_else(|| {
                    OutputConfigValidationError::field(
                        &name,
                        "layout_extent",
                        None,
                        format!("{},{}", position.0, position.1),
                        format!("cannot validate position for '{name}' without an active mode"),
                    )
                })?;
            if !output_extent_is_supported(position, mode_size) {
                return Err(OutputConfigValidationError::field(
                    &name,
                    "layout_extent",
                    None,
                    format!("{},{}", position.0, position.1),
                    format!(
                        "position ({},{}) with mode {}x{} for '{name}' is outside the compositor framebuffer domain",
                        position.0, position.1, mode_size.0, mode_size.1
                    ),
                ));
            }
        }

        changes.push(OutputConfigChange {
            name,
            enabled: true,
            mode: requested_mode,
            position: pending.position,
            transform: pending.transform,
            scale: pending.scale,
            adaptive_sync: pending.adaptive_sync,
        });
    }

    for name in data.disabled_heads.lock_safe().iter() {
        if !state.outputs.iter().any(|output| output.name() == *name) {
            return Err(OutputConfigValidationError::for_output(
                name,
                format!("unknown output '{name}'"),
            ));
        }
        changes.push(OutputConfigChange {
            name: name.clone(),
            enabled: false,
            mode: None,
            position: None,
            transform: None,
            scale: None,
            adaptive_sync: None,
        });
    }

    if !output_config_leaves_enabled_output(
        state.outputs.iter().map(|output| output.name()),
        &state.soft_disabled_outputs,
        &changes,
    ) {
        return Err(OutputConfigValidationError::new(
            "configuration would leave no enabled outputs",
        ));
    }

    // Apply cannot transactionally grow/shrink the compositor's complete FBO
    // chain yet. Keep Test honest by enforcing the same deterministic envelope
    // constraint here, before either request can queue a backend mutation.
    let current_layout = state
        .outputs
        .iter()
        .map(|output| {
            let mode = output.current_mode().ok_or_else(|| {
                OutputConfigValidationError::field(
                    output.name(),
                    "layout_extent",
                    None,
                    "missing-current-mode",
                    format!(
                        "cannot validate framebuffer envelope: output '{}' has no current mode",
                        output.name()
                    ),
                )
            })?;
            let location = output.current_location();
            Ok((
                output.name(),
                (location.x, location.y),
                (mode.size.w, mode.size.h),
            ))
        })
        .collect::<Result<Vec<_>, OutputConfigValidationError>>()?;
    let current_size = proposed_output_framebuffer_size(&current_layout, &[])
        .map_err(OutputConfigValidationError::new)?;
    let proposed_size = proposed_output_framebuffer_size(&current_layout, &changes)
        .map_err(OutputConfigValidationError::new)?;
    if proposed_size != current_size {
        return Err(OutputConfigValidationError::field(
            "*",
            "layout_extent",
            None,
            format!("{}x{}", proposed_size.0, proposed_size.1),
            format!(
                "runtime framebuffer envelope change from {}x{} to {}x{} is not yet supported; reinitialize KMS to apply this layout",
                current_size.0, current_size.1, proposed_size.0, proposed_size.1
            ),
        ));
    }

    Ok(changes)
}

fn output_config_leaves_enabled_output(
    outputs: impl IntoIterator<Item = String>,
    soft_disabled_outputs: &std::collections::HashSet<String>,
    changes: &[OutputConfigChange],
) -> bool {
    let mut enabled_outputs: std::collections::HashSet<String> = outputs
        .into_iter()
        .filter(|name| !soft_disabled_outputs.contains(name))
        .collect();

    for change in changes {
        if change.enabled {
            enabled_outputs.insert(change.name.clone());
        } else {
            enabled_outputs.remove(&change.name);
        }
    }

    !enabled_outputs.is_empty()
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
        let mut pending = data.pending.lock_safe();
        match request {
            zwlr_output_configuration_head_v1::Request::SetMode { mode } => {
                if let Some(m) = mode.data::<OutputModeData>() {
                    pending.mode = Some((m.width, m.height, m.refresh));
                }
            }
            zwlr_output_configuration_head_v1::Request::SetCustomMode {
                width,
                height,
                refresh,
            } => {
                pending.custom_mode = Some((width, height, refresh));
            }
            zwlr_output_configuration_head_v1::Request::SetPosition { x, y } => {
                pending.position = Some((x, y));
            }
            zwlr_output_configuration_head_v1::Request::SetTransform { transform } => {
                if let Ok(t) = transform.into_result() {
                    pending.transform = Some(t as i32);
                }
            }
            zwlr_output_configuration_head_v1::Request::SetScale { scale } => {
                pending.scale = Some(scale);
            }
            zwlr_output_configuration_head_v1::Request::SetAdaptiveSync { state } => {
                if let Ok(s) = state.into_result() {
                    pending.adaptive_sync =
                        Some(s == zwlr_output_head_v1::AdaptiveSyncState::Enabled);
                }
            }
            _ => {}
        }
    }
}

// --- Dispatch for head (events only; release in v4) ---

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
    }
}

// --- Dispatch for mode (events only; release in v4) ---

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
    }
}

/// Raw Wayland wire client for the protocol tests in this directory. It
/// drives a real `Display<JwmWaylandState>` over a socket pair without a
/// client library, the way `lifecycle_tests` does, so a test can bind a
/// global, send requests and assert on the exact events the server wrote.
#[cfg(test)]
pub(crate) mod wire_test_support {
    use crate::backend::wayland::state::{JwmClientState, JwmWaylandState};
    use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
    use smithay::reexports::calloop::{EventLoop, channel};
    use smithay::reexports::wayland_server::{Display, DisplayHandle};
    use smithay::wayland::security_context::SecurityContext;
    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    const DISPLAY_ID: u32 = 1;
    const REGISTRY_ID: u32 = 2;

    /// One server-to-client message.
    #[derive(Debug)]
    pub(crate) struct WireEvent {
        pub(crate) sender: u32,
        pub(crate) opcode: u16,
        pub(crate) payload: Vec<u8>,
    }

    impl WireEvent {
        /// The `index`th 32-bit argument word (an object id, uint or int).
        pub(crate) fn word(&self, index: usize) -> u32 {
            read_u32(&self.payload, index * 4)
        }

        /// Every argument as 32-bit words.
        pub(crate) fn words(&self) -> Vec<u32> {
            (0..self.payload.len() / 4)
                .map(|index| self.word(index))
                .collect()
        }
    }

    pub(crate) struct WireClient {
        pub(crate) state: JwmWaylandState,
        display: Display<JwmWaylandState>,
        peer: UnixStream,
        inbox: Vec<u8>,
        next_id: u32,
        globals: Vec<(u32, String, u32)>,
        _event_loop: EventLoop<'static, JwmWaylandState>,
    }

    impl WireClient {
        /// Build a headless server, let `setup` register globals or seed
        /// state, then connect a client and read the registry.
        pub(crate) fn new(setup: impl FnOnce(&DisplayHandle, &mut JwmWaylandState)) -> Self {
            Self::connect(setup, false)
        }

        /// Like [`WireClient::new`], but the client is sandboxed: it connected
        /// through a wp_security_context listener, the way a Flatpak app does.
        pub(crate) fn new_sandboxed(
            setup: impl FnOnce(&DisplayHandle, &mut JwmWaylandState),
        ) -> Self {
            Self::connect(setup, true)
        }

        fn connect(
            setup: impl FnOnce(&DisplayHandle, &mut JwmWaylandState),
            sandboxed: bool,
        ) -> Self {
            let event_loop: EventLoop<'static, JwmWaylandState> =
                EventLoop::try_new().expect("create test event loop");
            let display = Display::<JwmWaylandState>::new().expect("create test display");
            let mut display_handle = display.handle();
            let (flush_tx, _flush_rx) = channel::channel();
            let (mut state, socket_name) = JwmWaylandState::init(
                &display_handle,
                event_loop.handle(),
                Arc::new(Mutex::new(VecDeque::new())),
                flush_tx,
                Arc::new(AtomicBool::new(false)),
                "wire-test-seat".to_owned(),
                false,
                false,
            )
            .expect("initialize headless Wayland state");
            assert!(socket_name.is_none());
            setup(&display_handle, &mut state);

            let (server, peer) = UnixStream::pair().expect("create Wayland socket pair");
            peer.set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .expect("set wire read timeout");
            let client_state = if sandboxed {
                // The context names the client that created it; any live
                // client id will do.
                let (creator, _creator_peer) =
                    UnixStream::pair().expect("create creator socket pair");
                let creator = display_handle
                    .insert_client(creator, Arc::new(JwmClientState::default()))
                    .expect("insert security-context creator");
                JwmClientState {
                    security_context: Some(SecurityContext {
                        sandbox_engine: Some("org.flatpak".to_owned()),
                        app_id: Some("org.example.Sandboxed".to_owned()),
                        instance_id: None,
                        creator_client_id: creator.id(),
                    }),
                    ..JwmClientState::default()
                }
            } else {
                JwmClientState::default()
            };
            display_handle
                .insert_client(server, Arc::new(client_state))
                .expect("insert raw Wayland client");

            let mut client = Self {
                state,
                display,
                peer,
                inbox: Vec::new(),
                next_id: REGISTRY_ID + 1,
                globals: Vec::new(),
                _event_loop: event_loop,
            };
            // wl_display.get_registry(new_id)
            client.request(DISPLAY_ID, 1, &REGISTRY_ID.to_ne_bytes());
            let mut globals = Vec::new();
            client.roundtrip_with(|event| {
                if event.sender == REGISTRY_ID && event.opcode == 0 {
                    globals.push(parse_global(&event.payload));
                }
            });
            client.globals = globals;
            client
        }

        /// Handle of the server's display, for driving server-side updates.
        pub(crate) fn display_handle(&self) -> DisplayHandle {
            self.display.handle()
        }

        /// Whether the registry advertised a global named `interface`.
        pub(crate) fn advertises(&self, interface: &str) -> bool {
            self.globals
                .iter()
                .any(|(_, advertised, _)| advertised == interface)
        }

        /// Bind the most recently advertised global named `interface`, so a
        /// test's own registration wins over one `JwmWaylandState::init`
        /// may already have made from the default config.
        pub(crate) fn bind(&mut self, interface: &str, version: u32) -> u32 {
            let (name, advertised) = self
                .globals
                .iter()
                .rev()
                .find_map(|(name, advertised_interface, advertised)| {
                    (advertised_interface == interface).then_some((*name, *advertised))
                })
                .unwrap_or_else(|| panic!("{interface} global is advertised"));
            let id = self.new_id();
            let mut payload = name.to_ne_bytes().to_vec();
            payload.extend_from_slice(&wire_string(interface));
            payload.extend_from_slice(&version.min(advertised).to_ne_bytes());
            payload.extend_from_slice(&id.to_ne_bytes());
            self.request(REGISTRY_ID, 0, &payload);
            id
        }

        pub(crate) fn request(&mut self, object: u32, opcode: u16, payload: &[u8]) {
            let size = 8 + payload.len();
            assert_eq!(size % 4, 0, "wire payloads are 32-bit aligned");
            let mut message = Vec::with_capacity(size);
            message.extend_from_slice(&object.to_ne_bytes());
            message.extend_from_slice(&(((size as u32) << 16) | u32::from(opcode)).to_ne_bytes());
            message.extend_from_slice(payload);
            self.peer
                .write_all(&message)
                .expect("write Wayland request");
        }

        /// Dispatch everything sent so far and return the events the server
        /// wrote in reply, up to the `wl_display.sync` callback marking the
        /// end. Panics on a `wl_display.error`.
        pub(crate) fn roundtrip(&mut self) -> Vec<WireEvent> {
            let mut events = Vec::new();
            self.roundtrip_with(|event| events.push(event));
            events
        }

        fn roundtrip_with(&mut self, mut on_event: impl FnMut(WireEvent)) {
            let callback = self.new_id();
            // wl_display.sync(new_id)
            self.request(DISPLAY_ID, 0, &callback.to_ne_bytes());
            self.display
                .dispatch_clients(&mut self.state)
                .expect("dispatch Wayland requests");
            self.display.flush_clients().expect("flush Wayland events");
            loop {
                let event = self.next_event();
                assert!(
                    !(event.sender == DISPLAY_ID && event.opcode == 0),
                    "server raised a protocol error: {event:?}"
                );
                if event.sender == callback && event.opcode == 0 {
                    return;
                }
                on_event(event);
            }
        }

        fn next_event(&mut self) -> WireEvent {
            loop {
                if self.inbox.len() >= 8 {
                    let size = (read_u32(&self.inbox, 4) >> 16) as usize;
                    assert!(size >= 8, "malformed Wayland event header");
                    if self.inbox.len() >= size {
                        let event = WireEvent {
                            sender: read_u32(&self.inbox, 0),
                            opcode: read_u32(&self.inbox, 4) as u16,
                            payload: self.inbox[8..size].to_vec(),
                        };
                        self.inbox.drain(..size);
                        return event;
                    }
                }
                let mut chunk = [0u8; 8192];
                let read = self.peer.read(&mut chunk).expect("read Wayland events");
                assert!(read > 0, "Wayland server closed the connection");
                self.inbox.extend_from_slice(&chunk[..read]);
            }
        }

        /// Allocate the next client-side object id.
        pub(crate) fn new_id(&mut self) -> u32 {
            let id = self.next_id;
            self.next_id += 1;
            id
        }
    }

    /// A 1920x1080@60 output with no `wl_output` global, enough for
    /// protocols that describe outputs by name and mode.
    pub(crate) fn test_output(name: &str) -> Output {
        let output = Output::new(
            name.to_owned(),
            PhysicalProperties {
                size: (300, 200).into(),
                subpixel: Subpixel::Unknown,
                make: "JWM".into(),
                model: "wire-test".into(),
                serial_number: "0".into(),
            },
        );
        let mode = Mode {
            size: (1920, 1080).into(),
            refresh: 60_000,
        };
        output.change_current_state(Some(mode), None, None, Some((0, 0).into()));
        output.set_preferred(mode);
        output
    }

    fn read_u32(bytes: &[u8], offset: usize) -> u32 {
        let mut word = [0u8; 4];
        word.copy_from_slice(&bytes[offset..offset + 4]);
        u32::from_ne_bytes(word)
    }

    fn wire_string(value: &str) -> Vec<u8> {
        let len = value.len() + 1;
        let mut encoded = (len as u32).to_ne_bytes().to_vec();
        encoded.extend_from_slice(value.as_bytes());
        encoded.push(0);
        encoded.resize((encoded.len() + 3) & !3, 0);
        encoded
    }

    /// Decode a `wl_registry.global(name, interface, version)` payload.
    fn parse_global(payload: &[u8]) -> (u32, String, u32) {
        let name = read_u32(payload, 0);
        let len = read_u32(payload, 4) as usize;
        let interface = std::str::from_utf8(&payload[8..8 + len.saturating_sub(1)])
            .expect("registry interface is UTF-8")
            .to_owned();
        let version = read_u32(payload, 8 + ((len + 3) & !3));
        (name, interface, version)
    }
}

#[cfg(test)]
mod tests {
    use super::wire_test_support::{WireClient, WireEvent, test_output};
    use super::{
        HeadRefreshPlan, HeadSnapshot, OutputConfigValidationError, OutputManagementState,
        adaptive_sync_request_supported, admit_configuration_request, init_output_management,
        mode_is_change, output_config_leaves_enabled_output, output_extent_is_supported,
        plan_head_refresh, proposed_output_framebuffer_size,
    };
    use crate::backend::api::{BackendEvent, OutputConfigChange};
    use crate::backend::wayland::state::JwmWaylandState;
    use crate::backend::wayland_udev::{
        foreign_toplevel_management, image_copy_capture, output_power, screencopy, virtual_pointer,
    };
    use smithay::output::Mode as SmithayMode;
    use smithay::reexports::wayland_server::DisplayHandle;
    use smithay::utils::{Size, Transform};
    use std::collections::HashSet;
    use std::sync::atomic::AtomicBool;

    // zwlr_output_manager_v1 requests and events.
    const MANAGER_CREATE_CONFIGURATION: u16 = 0;
    const MANAGER_STOP: u16 = 1;
    const MANAGER_HEAD: u16 = 0;
    const MANAGER_DONE: u16 = 1;
    const MANAGER_FINISHED: u16 = 2;
    // zwlr_output_head_v1 events.
    const HEAD_MODE: u16 = 3;
    const HEAD_ENABLED: u16 = 4;
    const HEAD_POSITION: u16 = 6;
    const HEAD_FINISHED: u16 = 9;
    // zwlr_output_mode_v1.finished
    const MODE_FINISHED: u16 = 3;
    // zwlr_output_configuration_v1 requests and events.
    const CONFIGURATION_APPLY: u16 = 2;
    const CONFIGURATION_TEST: u16 = 3;
    const CONFIGURATION_SUCCEEDED: u16 = 0;
    const CONFIGURATION_CANCELLED: u16 = 2;

    fn words(values: &[u32]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|value| value.to_ne_bytes())
            .collect()
    }

    /// A client bound to a test-registered output manager, with the
    /// registry the test drives refreshes through.
    fn bound_manager(outputs: &[&str]) -> (WireClient, OutputManagementState, u32) {
        let mut management = None;
        let mut client = WireClient::new(|display, state| {
            management = Some(init_output_management(display));
            state.outputs = outputs.iter().map(|name| test_output(name)).collect();
        });
        let management = management.expect("output management is registered");
        let manager = client.bind("zwlr_output_manager_v1", 4);
        (client, management, manager)
    }

    fn refresh(client: &WireClient, management: &OutputManagementState) -> bool {
        management.refresh(
            &client.display_handle(),
            &client.state.outputs,
            &client.state.soft_disabled_outputs,
        )
    }

    /// Heads `manager` introduced, in order.
    fn announced_heads(events: &[WireEvent], manager: u32) -> Vec<u32> {
        events
            .iter()
            .filter(|event| event.sender == manager && event.opcode == MANAGER_HEAD)
            .map(|event| event.word(0))
            .collect()
    }

    fn done_serial(events: &[WireEvent], manager: u32) -> Option<u32> {
        events
            .iter()
            .rev()
            .find(|event| event.sender == manager && event.opcode == MANAGER_DONE)
            .map(|event| event.word(0))
    }

    fn opcodes(events: &[WireEvent], object: u32) -> Vec<u16> {
        events
            .iter()
            .filter(|event| event.sender == object)
            .map(|event| event.opcode)
            .collect()
    }

    /// Create a configuration against `serial`, send `request` (test or
    /// apply) on it with no heads changed, and return its events.
    fn submit_configuration(
        client: &mut WireClient,
        manager: u32,
        serial: u32,
        request: u16,
    ) -> (u32, Vec<WireEvent>) {
        let configuration = client.new_id();
        client.request(
            manager,
            MANAGER_CREATE_CONFIGURATION,
            &words(&[configuration, serial]),
        );
        client.request(configuration, request, &[]);
        (configuration, client.roundtrip())
    }

    #[test]
    fn hotplug_and_unplug_reach_bound_managers_and_stale_configurations_are_cancelled() {
        let (mut client, management, manager) = bound_manager(&["WIRE-1"]);
        let initial = client.roundtrip();
        let [first_head] = announced_heads(&initial, manager)[..] else {
            panic!("one head per output: {initial:?}");
        };
        let first_mode = initial
            .iter()
            .find(|event| event.sender == first_head && event.opcode == HEAD_MODE)
            .map(|event| event.word(0))
            .expect("the head announces its mode");
        let first_serial = done_serial(&initial, manager).expect("bind ends with done");
        assert_eq!(management.manager_count(), 1);

        // Regression: a hotplugged output never reached a bound manager, so
        // kanshi never saw the new head.
        let second = test_output("WIRE-2");
        second.change_current_state(None, None, None, Some((1920, 0).into()));
        client.state.outputs.push(second);
        assert!(refresh(&client, &management));
        let events = client.roundtrip();
        assert_eq!(announced_heads(&events, manager).len(), 1);
        assert!(
            opcodes(&events, first_head).is_empty(),
            "an unchanged head is not re-sent"
        );
        let second_serial = done_serial(&events, manager).expect("the change ends with done");
        assert_ne!(second_serial, first_serial);
        assert_eq!(management.serial(), second_serial);

        // A configuration built before the hotplug is cancelled, not tested
        // or applied against heads the client has not seen.
        let (stale, events) =
            submit_configuration(&mut client, manager, first_serial, CONFIGURATION_TEST);
        assert_eq!(opcodes(&events, stale), [CONFIGURATION_CANCELLED]);
        let (current, events) =
            submit_configuration(&mut client, manager, second_serial, CONFIGURATION_TEST);
        assert_eq!(opcodes(&events, current), [CONFIGURATION_SUCCEEDED]);

        // Unplug: the head and its mode are finished, then done.
        client.state.outputs.remove(0);
        assert!(refresh(&client, &management));
        let events = client.roundtrip();
        assert_eq!(opcodes(&events, first_mode), [MODE_FINISHED]);
        assert_eq!(opcodes(&events, first_head), [HEAD_FINISHED]);
        let third_serial = done_serial(&events, manager).expect("the unplug ends with done");
        assert_ne!(third_serial, second_serial);

        // Nothing changed since: nothing is sent and the serial holds.
        assert!(!refresh(&client, &management));
        let events = client.roundtrip();
        assert!(opcodes(&events, manager).is_empty());
        assert_eq!(management.serial(), third_serial);
    }

    #[test]
    fn changed_properties_are_sent_on_the_existing_head() {
        let (mut client, management, manager) = bound_manager(&["WIRE-1", "WIRE-2"]);
        let initial = client.roundtrip();
        let heads = announced_heads(&initial, manager);
        assert_eq!(heads.len(), 2);

        client.state.outputs[1].change_current_state(None, None, None, Some((1920, 0).into()));
        client
            .state
            .soft_disabled_outputs
            .insert("WIRE-2".to_owned());
        assert!(refresh(&client, &management));
        let events = client.roundtrip();
        assert!(announced_heads(&events, manager).is_empty());
        assert!(opcodes(&events, heads[0]).is_empty());
        let changed: Vec<(u16, Vec<u32>)> = events
            .iter()
            .filter(|event| event.sender == heads[1])
            .map(|event| (event.opcode, event.words()))
            .collect();
        assert_eq!(
            changed,
            [(HEAD_ENABLED, vec![0]), (HEAD_POSITION, vec![1920, 0])]
        );
        assert!(done_serial(&events, manager).is_some());
    }

    #[test]
    fn apply_after_an_unannounced_change_announces_it_and_is_cancelled() {
        let (mut client, _management, manager) = bound_manager(&["WIRE-1"]);
        let initial = client.roundtrip();
        let [head] = announced_heads(&initial, manager)[..] else {
            panic!("one head per output: {initial:?}");
        };
        let serial = done_serial(&initial, manager).expect("bind ends with done");

        // The output moved and nothing refreshed the managers yet.
        client.state.outputs[0].change_current_state(None, None, None, Some((0, 1080).into()));
        let (configuration, events) =
            submit_configuration(&mut client, manager, serial, CONFIGURATION_APPLY);
        assert_eq!(opcodes(&events, configuration), [CONFIGURATION_CANCELLED]);
        assert_eq!(opcodes(&events, head), [HEAD_POSITION]);
        let fresh = done_serial(&events, manager).expect("the change is announced");
        assert_ne!(fresh, serial);
        assert!(client.state.pending_output_acks.is_empty());
        assert!(
            client
                .state
                .last_output_management_rejection
                .as_ref()
                .is_some_and(|rejection| rejection.action == "apply" && rejection.serial == serial)
        );
        let queued_configures = |client: &WireClient| {
            client
                .state
                .pending_events
                .lock()
                .expect("backend event queue")
                .iter()
                .filter(|event| matches!(event, BackendEvent::OutputConfigure { .. }))
                .count()
        };
        assert_eq!(queued_configures(&client), 0);

        // Retrying with the announced serial reaches the backend.
        submit_configuration(&mut client, manager, fresh, CONFIGURATION_APPLY);
        assert_eq!(client.state.pending_output_acks.len(), 1);
        assert_eq!(queued_configures(&client), 1);
    }

    #[test]
    fn sandboxed_clients_are_not_offered_capture_or_output_control_globals() {
        const PRIVILEGED: [&str; 8] = [
            "zwlr_output_manager_v1",
            "zwlr_output_power_manager_v1",
            "zwlr_screencopy_manager_v1",
            "ext_output_image_capture_source_manager_v1",
            "ext_foreign_toplevel_image_capture_source_manager_v1",
            "ext_image_copy_capture_manager_v1",
            "zwlr_virtual_pointer_manager_v1",
            "zwlr_foreign_toplevel_manager_v1",
        ];
        let register = |display: &DisplayHandle, _: &mut JwmWaylandState| {
            init_output_management(display);
            output_power::init_output_power_management(display);
            screencopy::init_screencopy_manager(display);
            image_copy_capture::init_image_copy_capture(display);
            virtual_pointer::init_virtual_pointer_manager(display);
            foreign_toplevel_management::init_foreign_toplevel_management(display);
        };
        let trusted = WireClient::new(register);
        let sandboxed = WireClient::new_sandboxed(register);
        for interface in PRIVILEGED {
            assert!(
                trusted.advertises(interface),
                "{interface} must stay available to ordinary clients"
            );
            assert!(
                !sandboxed.advertises(interface),
                "{interface} was offered to a sandboxed client"
            );
        }
        assert!(sandboxed.advertises("wl_compositor"));
    }

    fn snapshot(name: &str, position: (i32, i32)) -> HeadSnapshot {
        HeadSnapshot {
            name: name.to_owned(),
            description: format!("JWM {name}"),
            physical_size: (300, 200),
            make: "JWM".to_owned(),
            model: "plan".to_owned(),
            serial_number: "0".to_owned(),
            modes: vec![(1920, 1080, 60_000, true), (1280, 720, 60_000, false)],
            current_mode: Some(0),
            enabled: true,
            position,
            transform: Transform::Normal,
            scale: 1.0,
        }
    }

    #[test]
    fn head_refresh_plan_updates_in_place_only_when_the_identity_holds() {
        let published = vec![snapshot("A", (0, 0)), snapshot("B", (1920, 0))];
        assert!(plan_head_refresh(&published, &published).is_empty());

        // Properties change in place; a new output is added; a gone one removed.
        let mut moved = snapshot("B", (0, 1080));
        moved.current_mode = Some(1);
        moved.scale = 1.5;
        let current = vec![moved.clone(), snapshot("C", (1920, 0))];
        assert_eq!(
            plan_head_refresh(&published, &current),
            HeadRefreshPlan {
                remove: vec!["A".to_owned()],
                add: vec![1],
                update: vec![(1, 0)],
            }
        );

        // A new mode list cannot be described on the old head: recreate it.
        let mut remoded = snapshot("A", (0, 0));
        remoded.modes.push((3840, 2160, 30_000, false));
        let current = vec![remoded, snapshot("B", (1920, 0))];
        assert_eq!(
            plan_head_refresh(&published, &current),
            HeadRefreshPlan {
                remove: vec!["A".to_owned()],
                add: vec![0],
                update: vec![],
            }
        );
    }

    #[test]
    fn stop_is_answered_with_finished() {
        let (mut client, management, manager) = bound_manager(&["WIRE-1"]);
        let initial = client.roundtrip();
        let manager_events: Vec<u16> = initial
            .iter()
            .filter(|event| event.sender == manager)
            .map(|event| event.opcode)
            .collect();
        assert_eq!(manager_events, [MANAGER_HEAD, MANAGER_DONE]);

        // Regression: `stop` used to be ignored, so a client waiting for the
        // `finished` handshake before tearing down blocked forever.
        client.request(manager, MANAGER_STOP, &[]);
        let events = client.roundtrip();
        assert!(
            events
                .iter()
                .any(|event| event.sender == manager && event.opcode == MANAGER_FINISHED),
            "stop must be answered with zwlr_output_manager_v1.finished: {events:?}"
        );
        // A finished manager gets no later head updates.
        assert_eq!(management.manager_count(), 0);
        client.state.outputs.push(test_output("WIRE-2"));
        assert!(!refresh(&client, &management));
    }

    #[test]
    fn test_or_apply_consumes_the_configuration_exactly_once() {
        let consumed = AtomicBool::new(false);

        // Mutating requests remain valid until test/apply claims the object.
        assert!(admit_configuration_request(&consumed, false));
        assert!(admit_configuration_request(&consumed, true));

        for _ in 0..10_000 {
            assert!(!admit_configuration_request(&consumed, false));
            assert!(!admit_configuration_request(&consumed, true));
        }
    }

    fn mode(w: i32, h: i32, refresh: i32) -> SmithayMode {
        SmithayMode {
            size: Size::from((w, h)),
            refresh,
        }
    }

    #[test]
    fn no_current_mode_is_always_a_change() {
        assert!(mode_is_change(None, (1920, 1080, 60_000)));
    }

    #[test]
    fn exact_match_is_not_a_change() {
        let cur = mode(1920, 1080, 60_000);
        assert!(!mode_is_change(Some(cur), (1920, 1080, 60_000)));
    }

    #[test]
    fn refresh_zero_matches_any_refresh_at_same_size() {
        let cur = mode(2560, 1440, 144_000);
        assert!(!mode_is_change(Some(cur), (2560, 1440, 0)));
    }

    #[test]
    fn refresh_within_0_2hz_tolerance_is_not_a_change() {
        let cur = mode(1920, 1080, 60_000);
        // wlr-randr often quantizes to mHz; tolerate ±200 mHz.
        assert!(!mode_is_change(Some(cur), (1920, 1080, 59_950)));
        assert!(!mode_is_change(Some(cur), (1920, 1080, 60_200)));
    }

    #[test]
    fn refresh_outside_tolerance_is_a_change() {
        let cur = mode(1920, 1080, 60_000);
        assert!(mode_is_change(Some(cur), (1920, 1080, 59_000)));
    }

    #[test]
    fn different_size_is_a_change_regardless_of_refresh() {
        let cur = mode(1920, 1080, 60_000);
        assert!(mode_is_change(Some(cur), (2560, 1440, 60_000)));
        assert!(mode_is_change(Some(cur), (2560, 1440, 0)));
    }

    #[test]
    fn output_extent_rejects_negative_or_overflowing_framebuffer_coordinates() {
        assert!(output_extent_is_supported((0, 0), (1920, 1080)));
        assert!(output_extent_is_supported((1920, 0), (2560, 1440)));
        assert!(!output_extent_is_supported((-1, 0), (1920, 1080)));
        assert!(!output_extent_is_supported((0, -1), (1920, 1080)));
        assert!(!output_extent_is_supported(
            (i32::MAX - 10, 0),
            (1920, 1080)
        ));
        assert!(!output_extent_is_supported((0, 0), (0, 1080)));
    }

    #[test]
    fn adaptive_sync_requests_are_rejected_instead_of_silently_ignored() {
        assert!(adaptive_sync_request_supported(None));
        assert!(!adaptive_sync_request_supported(Some(false)));
        assert!(!adaptive_sync_request_supported(Some(true)));
    }

    #[test]
    fn framebuffer_envelope_projection_distinguishes_testable_layouts() {
        let current = vec![
            ("eDP-1".to_string(), (0, 0), (1920, 1080)),
            ("DP-1".to_string(), (1920, 0), (1920, 1080)),
        ];
        assert_eq!(
            proposed_output_framebuffer_size(&current, &[]).unwrap(),
            (3840, 1080)
        );

        let mut shrunk = change("DP-1", true);
        shrunk.position = Some((0, 0));
        assert_eq!(
            proposed_output_framebuffer_size(&current, &[shrunk]).unwrap(),
            (1920, 1080)
        );

        let mut expanded = change("DP-1", true);
        expanded.position = Some((2000, 0));
        assert_eq!(
            proposed_output_framebuffer_size(&current, &[expanded]).unwrap(),
            (3920, 1080)
        );
    }

    fn change(name: &str, enabled: bool) -> OutputConfigChange {
        OutputConfigChange {
            name: name.to_string(),
            enabled,
            mode: None,
            position: None,
            transform: None,
            scale: None,
            adaptive_sync: None,
        }
    }

    #[test]
    fn output_config_allows_disabling_one_of_two_outputs() {
        assert!(output_config_leaves_enabled_output(
            ["HDMI-A-1".to_string(), "DP-1".to_string()],
            &HashSet::new(),
            &[change("DP-1", false)],
        ));
    }

    #[test]
    fn output_config_rejects_disabling_last_enabled_output() {
        assert!(!output_config_leaves_enabled_output(
            ["HDMI-A-1".to_string()],
            &HashSet::new(),
            &[change("HDMI-A-1", false)],
        ));
    }

    #[test]
    fn output_config_allows_reenabling_soft_disabled_output() {
        let soft_disabled = HashSet::from(["HDMI-A-1".to_string()]);
        assert!(output_config_leaves_enabled_output(
            ["HDMI-A-1".to_string()],
            &soft_disabled,
            &[change("HDMI-A-1", true)],
        ));
    }

    #[test]
    fn validation_error_preserves_structured_rejection_context() {
        let rejection = OutputConfigValidationError::field(
            "DP-1",
            "mode",
            Some("MODE_ID"),
            "3840x2160@144000",
            "mode change rejected",
        )
        .into_rejection(42, "apply");

        assert_eq!(rejection.serial, 42);
        assert_eq!(rejection.action, "apply");
        assert_eq!(rejection.output_name.as_deref(), Some("DP-1"));
        assert_eq!(rejection.field.as_deref(), Some("mode"));
        assert_eq!(rejection.drm_property.as_deref(), Some("MODE_ID"));
        assert_eq!(
            rejection.requested_value.as_deref(),
            Some("3840x2160@144000")
        );
        assert_eq!(rejection.reason, "mode change rejected");
        assert!(rejection.attempted_at_unix_ms > 0);
    }
}
