use std::collections::HashMap;
use std::sync::Mutex;
/// wlr-output-management-unstable-v1 protocol implementation for JWM.
///
/// Allows clients like wlr-randr and kanshi to enumerate outputs (modes, position,
/// scale, transform, adaptive sync) and apply configuration changes.
///
/// Enumeration is sent on manager bind: for each live output we create a head and
/// one mode object per supported mode, then report current mode/position/scale/
/// transform. Apply/Test validate the requested configuration against the live
/// outputs and (for Apply) route an `OutputConfigure` backend event that performs
/// the real DRM modeset / layout change on the compositor thread.
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use log::{debug, info, warn};

use smithay::output::Output;
use smithay::reexports::wayland_protocols_wlr::output_management::v1::server::{
    zwlr_output_configuration_head_v1::{self, ZwlrOutputConfigurationHeadV1},
    zwlr_output_configuration_v1::{self, ZwlrOutputConfigurationV1},
    zwlr_output_head_v1::{self, ZwlrOutputHeadV1},
    zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
    zwlr_output_mode_v1::{self, ZwlrOutputModeV1},
};
use smithay::reexports::wayland_server::protocol::wl_output;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};

use crate::backend::api::OutputConfigChange;
use crate::backend::wayland::state::JwmWaylandState;
use crate::sync_ext::MutexExt;

static SERIAL_COUNTER: AtomicU32 = AtomicU32::new(1);

fn next_serial() -> u32 {
    SERIAL_COUNTER.fetch_add(1, Ordering::Relaxed)
}

// --- Data types ---

pub struct OutputManagerData;
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
    pub serial: u32,
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

/// Initialize the wlr-output-management global.
pub fn init_output_management(dh: &DisplayHandle) {
    dh.create_global::<JwmWaylandState, ZwlrOutputManagerV1, _>(4, OutputManagerData);
    info!("[udev/wayland] zwlr-output-management-unstable-v1 global registered");
}

// --- GlobalDispatch for the manager ---

impl GlobalDispatch<ZwlrOutputManagerV1, OutputManagerData> for JwmWaylandState {
    fn bind(
        state: &mut Self,
        dh: &DisplayHandle,
        client: &Client,
        resource: New<ZwlrOutputManagerV1>,
        _global_data: &OutputManagerData,
        data_init: &mut DataInit<'_, Self>,
    ) {
        state.record_protocol_bind("zwlr_output_manager_v1");
        let manager = data_init.init(resource, OutputManagerData);

        for output in &state.outputs {
            let soft_disabled = state.soft_disabled_outputs.contains(&output.name());
            send_head_for_output(dh, client, &manager, output, soft_disabled);
        }

        manager.done(next_serial());
    }
}

/// Create a head (and its mode objects) for `output` and report its current state.
fn send_head_for_output(
    dh: &DisplayHandle,
    client: &Client,
    manager: &ZwlrOutputManagerV1,
    output: &Output,
    soft_disabled: bool,
) {
    let version = manager.version();
    let name = output.name();

    let Ok(head) = client.create_resource::<ZwlrOutputHeadV1, _, JwmWaylandState>(
        dh,
        version,
        OutputHeadData {
            output_name: name.clone(),
        },
    ) else {
        warn!("[output-mgmt] failed to create head resource for {name}");
        return;
    };

    manager.head(&head);
    head.name(name.clone());
    head.description(output.description());

    let props = output.physical_properties();
    head.physical_size(props.size.w, props.size.h);

    let current_mode = output.current_mode();
    let preferred_mode = output.preferred_mode();
    let mut current_mode_res: Option<ZwlrOutputModeV1> = None;

    for mode in output.modes() {
        let Ok(mode_res) = client.create_resource::<ZwlrOutputModeV1, _, JwmWaylandState>(
            dh,
            version,
            OutputModeData {
                output_name: name.clone(),
                width: mode.size.w,
                height: mode.size.h,
                refresh: mode.refresh,
            },
        ) else {
            continue;
        };

        head.mode(&mode_res);
        mode_res.size(mode.size.w, mode.size.h);
        mode_res.refresh(mode.refresh);
        if Some(mode) == preferred_mode {
            mode_res.preferred();
        }
        if Some(mode) == current_mode {
            current_mode_res = Some(mode_res);
        }
    }

    // A head is enabled when it is actively driving a CRTC. Outputs marked
    // soft-disabled by an earlier `disable_head` Apply are reported as 0.
    head.enabled(if soft_disabled { 0 } else { 1 });
    if let Some(ref mode_res) = current_mode_res {
        head.current_mode(mode_res);
    }

    let loc = output.current_location();
    head.position(loc.x, loc.y);

    let wl_transform: wl_output::Transform = output.current_transform().into();
    head.transform(wl_transform);

    head.scale(output.current_scale().fractional_scale());

    if version >= 2 {
        head.make(props.make.clone());
        head.model(props.model.clone());
        head.serial_number(props.serial_number.clone());
    }

    if version >= 4 {
        // We do not track per-output adaptive sync activation here; report disabled.
        head.adaptive_sync(zwlr_output_head_v1::AdaptiveSyncState::Disabled);
    }
}

// --- Dispatch for the manager ---

impl Dispatch<ZwlrOutputManagerV1, OutputManagerData> for JwmWaylandState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ZwlrOutputManagerV1,
        request: zwlr_output_manager_v1::Request,
        _data: &OutputManagerData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_output_manager_v1::Request::CreateConfiguration { id, serial } => {
                data_init.init(
                    id,
                    OutputConfigData {
                        serial,
                        consumed: AtomicBool::new(false),
                        enabled_heads: Mutex::new(Vec::new()),
                        disabled_heads: Mutex::new(Vec::new()),
                    },
                );
            }
            zwlr_output_manager_v1::Request::Stop => {}
            _ => {}
        }
    }
}

// --- Dispatch for configuration ---

impl Dispatch<ZwlrOutputConfigurationV1, OutputConfigData> for JwmWaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwlrOutputConfigurationV1,
        request: zwlr_output_configuration_v1::Request,
        data: &OutputConfigData,
        _dh: &DisplayHandle,
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
            zwlr_output_configuration_v1::Request::Test => match build_changes(state, data) {
                Ok(_) => resource.succeeded(),
                Err(e) => {
                    debug!("[output-mgmt] test rejected: {e}");
                    state.last_output_management_rejection =
                        Some(e.into_rejection(data.serial, "test"));
                    resource.failed();
                }
            },
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

#[cfg(test)]
mod tests {
    use super::{
        OutputConfigValidationError, adaptive_sync_request_supported, admit_configuration_request,
        mode_is_change, output_config_leaves_enabled_output, output_extent_is_supported,
        proposed_output_framebuffer_size,
    };
    use crate::backend::api::OutputConfigChange;
    use smithay::output::Mode as SmithayMode;
    use smithay::utils::Size;
    use std::collections::HashSet;
    use std::sync::atomic::AtomicBool;

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
