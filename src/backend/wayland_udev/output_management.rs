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

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

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
    /// Config-head objects created via `enable_head`.
    pub enabled_heads: Mutex<Vec<ZwlrOutputConfigurationHeadV1>>,
    /// Output names targeted by `disable_head`.
    pub disabled_heads: Mutex<Vec<String>>,
}
unsafe impl Send for OutputConfigData {}

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
                                    if ok { res.succeeded(); } else { res.failed(); }
                                }),
                            },
                        );
                        state.push_event(
                            crate::backend::api::BackendEvent::OutputConfigure { changes },
                        );
                    }
                    Err(e) => {
                        warn!("[output-mgmt] apply rejected: {e}");
                        resource.failed();
                    }
                }
            }
            zwlr_output_configuration_v1::Request::Test => match build_changes(state, data) {
                Ok(_) => resource.succeeded(),
                Err(e) => {
                    debug!("[output-mgmt] test rejected: {e}");
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
fn mode_is_change(
    current: Option<smithay::output::Mode>,
    requested: (i32, i32, i32),
) -> bool {
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

/// Validate the pending configuration against live outputs and lower it into a
/// list of `OutputConfigChange`. Returns `Err` with a reason if invalid.
fn build_changes(
    state: &JwmWaylandState,
    data: &OutputConfigData,
) -> Result<Vec<OutputConfigChange>, String> {
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
            .ok_or_else(|| format!("unknown output '{name}'"))?;

        let pending = head_data.pending.lock_safe().clone();

        // set_mode takes precedence over set_custom_mode; both express (w, h, refresh).
        let requested_mode = pending.mode.or(pending.custom_mode);
        if let Some((w, h, refresh)) = requested_mode {
            if w <= 0 || h <= 0 {
                return Err(format!("invalid mode {w}x{h} for '{name}'"));
            }
            // For modes selected via set_mode, ensure they belong to the output.
            if pending.mode.is_some() {
                let known = output.modes().iter().any(|m| {
                    m.size.w == w
                        && m.size.h == h
                        && (refresh == 0 || (m.refresh - refresh).abs() <= 200)
                });
                if !known {
                    return Err(format!("mode {w}x{h}@{refresh} not on '{name}'"));
                }
            }
            // Reject up-front when a real modeset is requested but the safety
            // gate is closed. Without this, Apply would silently drop the mode
            // change at the KMS layer and still report succeeded() to the
            // client — lying about which fields were applied.
            if !allow_modeset && mode_is_change(output.current_mode(), (w, h, refresh)) {
                return Err(format!(
                    "mode change to {w}x{h}@{refresh} for '{name}' rejected: \
                     behavior.wlr_output_mgmt_allow_modeset = false"
                ));
            }
        }

        if let Some(t) = pending.transform {
            if !(0..=7).contains(&t) {
                return Err(format!("invalid transform {t} for '{name}'"));
            }
        }

        if let Some(s) = pending.scale {
            if s <= 0.0 {
                return Err(format!("invalid scale {s} for '{name}'"));
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

    Ok(changes)
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
    use super::mode_is_change;
    use smithay::output::Mode as SmithayMode;
    use smithay::utils::Size;

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
}
