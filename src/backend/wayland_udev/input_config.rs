//! Apply optional touchpad policy without changing other input devices.
//!
//! Device handles stay on the libinput event-loop thread. IPC reads an owned
//! snapshot only; it never calls libinput from another thread.
use std::collections::BTreeMap;
use std::sync::{LazyLock, Mutex};

use serde_json::{Value, json};
use smithay::reexports::input::{
    AccelProfile, ClickMethod, Device, DeviceCapability, DeviceConfigError, DragLockState,
    ScrollMethod, SendEventsMode,
};

use crate::config::InputConfig;
use crate::config::input::{InputDeviceIdentity, TouchpadOptions};

type Settings = BTreeMap<String, Value>;

static STATUS: LazyLock<Mutex<Value>> = LazyLock::new(|| {
    Mutex::new(json!({
        "available": false, "active": false, "state": "not_initialized",
        "gesture_devices": 0, "touchpad_count": 0, "devices": [],
    }))
});

pub(crate) fn status() -> Value {
    STATUS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
}

pub(crate) fn gesture_status() -> Value {
    let snapshot = status();
    let count = snapshot["gesture_devices"].as_u64().unwrap_or(0);
    let available = snapshot["available"].as_bool().unwrap_or(false);
    let active = snapshot["active"].as_bool().unwrap_or(false);
    json!({
        "available": available,
        "active": active,
        "operational": available && active && count > 0,
        "state": snapshot["state"],
        "gesture_devices": count,
        "device_supported": count > 0,
        "reason": if !active { Some("Input session is inactive") }
            else if count == 0 { Some("No gesture-capable touchpad is currently present") }
            else { None },
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApplyError {
    Unsupported,
    #[cfg(not(feature = "libinput-1-21"))]
    UnsupportedByBindings,
    Invalid,
}

impl From<DeviceConfigError> for ApplyError {
    fn from(error: DeviceConfigError) -> Self {
        match error {
            DeviceConfigError::Unsupported => Self::Unsupported,
            DeviceConfigError::Invalid => Self::Invalid,
        }
    }
}

impl ApplyError {
    fn name(self) -> &'static str {
        match self {
            Self::Unsupported => "unsupported",
            #[cfg(not(feature = "libinput-1-21"))]
            Self::UnsupportedByBindings => "unsupported_by_bindings",
            Self::Invalid => "invalid",
        }
    }
}

fn settings(options: &TouchpadOptions) -> Settings {
    serde_json::from_value(serde_json::to_value(options).expect("finite validated input config"))
        .expect("touchpad options serialize as a map")
}

/// Removing a previously specified option restores its value from before JWM
/// applied any policy. Never touch an option that JWM has not managed.
fn apply_policy(
    requested: &Settings,
    previous: &Settings,
    initial: &Settings,
    mut apply: impl FnMut(&str, &Value) -> Result<(), ApplyError>,
) -> Value {
    let mut changes = requested.clone();
    for field in previous.keys() {
        if !requested.contains_key(field)
            && let Some(value) = initial.get(field)
        {
            changes.insert(field.clone(), value.clone());
        }
    }
    let results: BTreeMap<_, _> = changes
        .into_iter()
        .map(|(field, value)| {
            let result = apply(&field, &value);
            let action = if requested.contains_key(&field) {
                "set"
            } else {
                "restore"
            };
            (
                field,
                json!({
                    "action": action,
                    "requested": value,
                    "status": result.err().map_or("applied", ApplyError::name),
                }),
            )
        })
        .collect();
    json!(results)
}

fn click_name(value: ClickMethod) -> &'static str {
    match value {
        ClickMethod::ButtonAreas => "button_areas",
        ClickMethod::Clickfinger => "clickfinger",
        _ => "unknown",
    }
}

fn scroll_name(value: ScrollMethod) -> &'static str {
    match value {
        ScrollMethod::NoScroll => "none",
        ScrollMethod::TwoFinger => "two_finger",
        ScrollMethod::Edge => "edge",
        ScrollMethod::OnButtonDown => "on_button_down",
        _ => "unknown",
    }
}

fn accel_name(value: AccelProfile) -> &'static str {
    match value {
        AccelProfile::Adaptive => "adaptive",
        AccelProfile::Flat => "flat",
        _ => "unknown",
    }
}

fn send_events_name(mode: SendEventsMode) -> &'static str {
    if mode.contains(SendEventsMode::DISABLED) {
        "disabled"
    } else if mode.contains(SendEventsMode::DISABLED_ON_EXTERNAL_MOUSE) {
        "disabled_on_external_mouse"
    } else {
        "enabled"
    }
}

fn effective_settings(device: &Device) -> Settings {
    let mut result = Settings::new();
    if device.config_tap_finger_count() > 0 {
        result.insert("tap".into(), json!(device.config_tap_enabled()));
        result.insert("tap_drag".into(), json!(device.config_tap_drag_enabled()));
        result.insert(
            "drag_lock".into(),
            json!(device.config_tap_drag_lock_enabled()),
        );
    }
    if device.config_scroll_has_natural_scroll() {
        result.insert(
            "natural_scroll".into(),
            json!(device.config_scroll_natural_scroll_enabled()),
        );
    }
    if device.config_dwt_is_available() {
        result.insert(
            "disable_while_typing".into(),
            json!(device.config_dwt_enabled()),
        );
    }
    #[cfg(feature = "libinput-1-21")]
    if device.config_dwtp_is_available() {
        result.insert(
            "disable_while_trackpointing".into(),
            json!(device.config_dwtp_enabled()),
        );
    }
    if device.config_left_handed_is_available() {
        result.insert("left_handed".into(), json!(device.config_left_handed()));
    }
    if device.config_accel_is_available() {
        result.insert("accel_speed".into(), json!(device.config_accel_speed()));
    }
    if let Some(value) = device.config_accel_profile() {
        result.insert("accel_profile".into(), json!(accel_name(value)));
    }
    result.insert(
        "click_method".into(),
        json!(device.config_click_method().map_or("none", click_name)),
    );
    if let Some(value) = device.config_scroll_method() {
        result.insert("scroll_method".into(), json!(scroll_name(value)));
    }
    result.insert(
        "send_events".into(),
        json!(send_events_name(device.config_send_events_mode())),
    );
    result
}

fn apply_input_setting(device: &mut Device, field: &str, value: &Value) -> Result<(), ApplyError> {
    let boolean = || value.as_bool().ok_or(ApplyError::Invalid);
    let text = || value.as_str().ok_or(ApplyError::Invalid);
    let available = |supported| {
        if supported {
            Ok(())
        } else {
            Err(ApplyError::Unsupported)
        }
    };
    let result = match field {
        "tap" => {
            available(device.config_tap_finger_count() > 0)?;
            device.config_tap_set_enabled(boolean()?)
        }
        "tap_drag" => {
            available(device.config_tap_finger_count() > 0)?;
            device.config_tap_set_drag_enabled(boolean()?)
        }
        "drag_lock" => {
            available(device.config_tap_finger_count() > 0)?;
            device.config_tap_set_drag_lock_enabled(if boolean()? {
                DragLockState::EnabledTimeout
            } else {
                DragLockState::Disabled
            })
        }
        "natural_scroll" => {
            available(device.config_scroll_has_natural_scroll())?;
            device.config_scroll_set_natural_scroll_enabled(boolean()?)
        }
        "disable_while_typing" => {
            available(device.config_dwt_is_available())?;
            device.config_dwt_set_enabled(boolean()?)
        }
        #[cfg(feature = "libinput-1-21")]
        "disable_while_trackpointing" => {
            available(device.config_dwtp_is_available())?;
            device.config_dwtp_set_enabled(boolean()?)
        }
        // The optional feature unifies input's libinput_1_21 API with Smithay.
        // Baseline builds retain 1.19 and report the unavailable API honestly.
        #[cfg(not(feature = "libinput-1-21"))]
        "disable_while_trackpointing" => return Err(ApplyError::UnsupportedByBindings),
        "left_handed" => {
            available(device.config_left_handed_is_available())?;
            device.config_left_handed_set(boolean()?)
        }
        "click_method" => {
            if text()? == "none" {
                // The high-level binding cannot express CLICK_METHOD_NONE.
                // This is only used to restore a captured initial value;
                // configuration validation does not accept "none" here.
                use smithay::reexports::input::{AsRaw, ffi};
                // The device is a live handle owned by this event loop.
                let result = unsafe {
                    ffi::libinput_device_config_click_set_method(
                        device.as_raw_mut(),
                        ffi::libinput_config_click_method_LIBINPUT_CONFIG_CLICK_METHOD_NONE,
                    )
                };
                return match result {
                    ffi::libinput_config_status_LIBINPUT_CONFIG_STATUS_SUCCESS => Ok(()),
                    ffi::libinput_config_status_LIBINPUT_CONFIG_STATUS_UNSUPPORTED => {
                        Err(ApplyError::Unsupported)
                    }
                    _ => Err(ApplyError::Invalid),
                };
            }
            let method = match text()? {
                "button_areas" => ClickMethod::ButtonAreas,
                "clickfinger" => ClickMethod::Clickfinger,
                _ => return Err(ApplyError::Invalid),
            };
            available(device.config_click_methods().contains(&method))?;
            device.config_click_set_method(method)
        }
        "scroll_method" => {
            let method = match text()? {
                "none" => ScrollMethod::NoScroll,
                "two_finger" => ScrollMethod::TwoFinger,
                "edge" => ScrollMethod::Edge,
                // A previous device default may use this method even though
                // touchpad configuration does not offer it as a new setting.
                "on_button_down" => ScrollMethod::OnButtonDown,
                _ => return Err(ApplyError::Invalid),
            };
            available(
                method == ScrollMethod::NoScroll
                    || device.config_scroll_methods().contains(&method),
            )?;
            device.config_scroll_set_method(method)
        }
        "accel_profile" => {
            let profile = match text()? {
                "adaptive" => AccelProfile::Adaptive,
                "flat" => AccelProfile::Flat,
                _ => return Err(ApplyError::Invalid),
            };
            available(device.config_accel_profiles().contains(&profile))?;
            device.config_accel_set_profile(profile)
        }
        "accel_speed" => {
            available(device.config_accel_is_available())?;
            let speed = value
                .as_f64()
                .filter(|speed| speed.is_finite() && (-1.0..=1.0).contains(speed))
                .ok_or(ApplyError::Invalid)?;
            device.config_accel_set_speed(speed)
        }
        "send_events" => {
            let mode = match text()? {
                "enabled" => SendEventsMode::ENABLED,
                "disabled" => SendEventsMode::DISABLED,
                "disabled_on_external_mouse" => SendEventsMode::DISABLED_ON_EXTERNAL_MOUSE,
                _ => return Err(ApplyError::Invalid),
            };
            available(
                mode == SendEventsMode::ENABLED || device.config_send_events_modes().contains(mode),
            )?;
            device.config_send_events_set_mode(mode)
        }
        _ => return Err(ApplyError::Invalid),
    };
    result.map_err(Into::into)
}

fn identity(device: &Device) -> InputDeviceIdentity {
    // The udev device is obtained from this libinput device's own context,
    // satisfying input::Device::udev_device's context ownership requirement.
    let touchpad_tag = unsafe { device.udev_device() }.is_some_and(|udev| {
        udev.property_value("ID_INPUT_TOUCHPAD")
            .is_some_and(|value| value == "1")
    });
    let name = device.name().into_owned();
    InputDeviceIdentity {
        identifier: format!(
            "{:04x}:{:04x}:{}",
            device.id_vendor(),
            device.id_product(),
            name
        ),
        name,
        vendor: device.id_vendor(),
        product: device.id_product(),
        touchpad: device.has_capability(DeviceCapability::Pointer)
            && (touchpad_tag
                || device.config_tap_finger_count() > 0
                || device
                    .config_scroll_methods()
                    .contains(&ScrollMethod::TwoFinger)),
        gestures: device.has_capability(DeviceCapability::Gesture),
    }
}

struct ManagedDevice {
    device: Device,
    identity: InputDeviceIdentity,
    initial: Settings,
    requested: Settings,
    results: Value,
}

impl ManagedDevice {
    fn apply(&mut self, config: &InputConfig) {
        let requested = settings(&config.for_device(&self.identity));
        self.results = apply_policy(
            &requested,
            &self.requested,
            &self.initial,
            |field, value| apply_input_setting(&mut self.device, field, value),
        );
        for (field, result) in self.results.as_object().into_iter().flatten() {
            if result["status"] != "applied" {
                log::warn!(
                    "[input] {}: {} is {}",
                    self.device.sysname(),
                    field,
                    result["status"]
                );
            }
        }
        self.requested = requested;
    }

    fn snapshot(&self) -> Value {
        let device = &self.device;
        #[cfg(feature = "libinput-1-21")]
        let dwtp_supported = Some(device.config_dwtp_is_available());
        #[cfg(not(feature = "libinput-1-21"))]
        let dwtp_supported: Option<bool> = None;
        let capabilities: Vec<_> = [
            (DeviceCapability::Pointer, "pointer"),
            (DeviceCapability::Keyboard, "keyboard"),
            (DeviceCapability::Touch, "touch"),
            (DeviceCapability::Gesture, "gesture"),
            (DeviceCapability::TabletTool, "tablet_tool"),
            (DeviceCapability::TabletPad, "tablet_pad"),
            (DeviceCapability::Switch, "switch"),
        ]
        .into_iter()
        .filter_map(|(capability, name)| device.has_capability(capability).then_some(name))
        .collect();
        json!({
            "id": device.sysname(),
            "identifier": self.identity.identifier,
            "name": self.identity.name,
            "vendor": self.identity.vendor,
            "product": self.identity.product,
            "touchpad": self.identity.touchpad,
            "capabilities": capabilities,
            "supported": {
                "tap_fingers": device.config_tap_finger_count(),
                "natural_scroll": device.config_scroll_has_natural_scroll(),
                "disable_while_typing": device.config_dwt_is_available(),
                "disable_while_trackpointing": dwtp_supported,
                "disable_while_trackpointing_reason": dwtp_supported.is_none().then_some("Build with --features libinput-1-21 to enable this API"),
                "left_handed": device.config_left_handed_is_available(),
                "accel": device.config_accel_is_available(),
                "accel_profiles": device.config_accel_profiles().into_iter().map(accel_name).collect::<Vec<_>>(),
                "click_methods": device.config_click_methods().into_iter().map(click_name).collect::<Vec<_>>(),
                "scroll_methods": device.config_scroll_methods().into_iter().map(scroll_name).collect::<Vec<_>>(),
                "send_events": ([Some("enabled"),
                    device.config_send_events_modes().contains(SendEventsMode::DISABLED).then_some("disabled"),
                    device.config_send_events_modes().contains(SendEventsMode::DISABLED_ON_EXTERNAL_MOUSE)
                        .then_some("disabled_on_external_mouse")].into_iter().flatten().collect::<Vec<_>>()),
            },
            "requested": self.requested,
            "effective": effective_settings(device),
            "initial": self.initial,
            "settings_results": self.results,
        })
    }
}

pub(crate) struct InputDevices {
    devices: BTreeMap<String, ManagedDevice>,
    active: bool,
}

impl InputDevices {
    pub(crate) fn new(active: bool) -> Self {
        let devices = Self {
            devices: BTreeMap::new(),
            active,
        };
        devices.publish();
        devices
    }

    pub(crate) fn added(&mut self, device: Device, config: &InputConfig) {
        let id = device.sysname().to_owned();
        let mut managed = ManagedDevice {
            identity: identity(&device),
            initial: effective_settings(&device),
            device,
            requested: Settings::new(),
            results: json!({}),
        };
        managed.apply(config);
        self.devices.insert(id, managed);
        self.publish();
    }

    pub(crate) fn removed(&mut self, device: &Device) {
        self.devices.remove(device.sysname());
        self.publish();
    }

    pub(crate) fn apply_config(&mut self, config: &InputConfig) {
        for device in self.devices.values_mut() {
            device.apply(config);
        }
        self.publish();
    }

    pub(crate) fn set_active(&mut self, active: bool) {
        self.active = active;
        self.publish();
    }

    fn publish(&self) {
        let gesture_devices = self
            .devices
            .values()
            .filter(|device| {
                device.identity.touchpad
                    && device.identity.gestures
                    && !device
                        .device
                        .config_send_events_mode()
                        .contains(SendEventsMode::DISABLED)
            })
            .count();
        let snapshot = json!({
            "available": true,
            "backend": "wayland-udev",
            "active": self.active,
            "state": if self.active { "native_libinput" } else { "suspended" },
            "touchpad_count": self.devices.values().filter(|device| device.identity.touchpad).count(),
            "gesture_devices": gesture_devices,
            "devices": self.devices.values().map(ManagedDevice::snapshot).collect::<Vec<_>>(),
        });
        *STATUS.lock().unwrap_or_else(|error| error.into_inner()) = snapshot;
    }
}

impl Drop for InputDevices {
    fn drop(&mut self) {
        *STATUS.lock().unwrap_or_else(|error| error.into_inner()) = json!({
            "available": false, "active": false, "state": "stopped",
            "touchpad_count": 0, "gesture_devices": 0, "devices": [],
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(value: Value) -> Settings {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn an_empty_policy_does_not_change_device_defaults() {
        let initial = values(json!({"tap": false, "natural_scroll": false}));
        let report = apply_policy(&Settings::new(), &Settings::new(), &initial, |_, _| {
            panic!("unrequested settings must not be applied")
        });
        assert_eq!(report, json!({}));
    }

    #[test]
    fn removing_one_setting_restores_only_its_initial_value() {
        let initial = values(json!({"tap": false, "accel_speed": 0.0, "natural_scroll": true}));
        let previous = values(json!({"tap": true, "accel_speed": 0.4}));
        let requested = values(json!({"accel_speed": -0.2}));
        let mut applied = Settings::new();
        let report = apply_policy(&requested, &previous, &initial, |field, value| {
            applied.insert(field.to_owned(), value.clone());
            Ok(())
        });
        assert_eq!(applied, values(json!({"tap": false, "accel_speed": -0.2})));
        assert_eq!(report["tap"]["action"], "restore");
        assert_eq!(report["accel_speed"]["action"], "set");
    }

    #[test]
    fn unsupported_options_are_reported_without_blocking_supported_changes() {
        let requested = values(json!({"tap": true, "click_method": "clickfinger",
            "disable_while_trackpointing": true}));
        let mut tapping = false;
        #[cfg(feature = "libinput-1-21")]
        let unavailable_dwtp = ApplyError::Unsupported;
        #[cfg(not(feature = "libinput-1-21"))]
        let unavailable_dwtp = ApplyError::UnsupportedByBindings;
        let report = apply_policy(
            &requested,
            &Settings::new(),
            &Settings::new(),
            |field, value| match field {
                "tap" => {
                    tapping = value.as_bool().unwrap();
                    Ok(())
                }
                "click_method" => Err(ApplyError::Unsupported),
                _ => Err(unavailable_dwtp),
            },
        );
        assert!(tapping);
        assert_eq!(report["tap"]["status"], "applied");
        assert_eq!(report["click_method"]["status"], "unsupported");
        assert_eq!(
            report["disable_while_trackpointing"]["status"],
            unavailable_dwtp.name()
        );
    }

    #[test]
    fn removing_dwtp_restores_the_captured_getter_value() {
        let initial = values(json!({"disable_while_trackpointing": true}));
        let previous = values(json!({"disable_while_trackpointing": false}));
        let mut restored = Settings::new();
        let report = apply_policy(&Settings::new(), &previous, &initial, |field, value| {
            restored.insert(field.to_owned(), value.clone());
            Ok(())
        });
        assert_eq!(restored, initial);
        assert_eq!(report["disable_while_trackpointing"]["action"], "restore");
        assert_eq!(report["disable_while_trackpointing"]["status"], "applied");
    }
}
