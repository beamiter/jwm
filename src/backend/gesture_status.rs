//! Backend-neutral native-gesture capability snapshots.
//!
//! Input providers publish lifecycle and device capability changes here. Policy
//! reads only these data, without depending on a concrete input implementation.

use std::sync::{LazyLock, Mutex};

#[derive(Default)]
pub(crate) struct RuntimeStatus {
    pub(crate) available: bool,
    pub(crate) server_version: Option<(u16, u16)>,
    pub(crate) gesture_devices: usize,
    pub(crate) active: bool,
    pub(crate) state: String,
    pub(crate) reason: Option<String>,
}

static RUNTIME_STATUS: LazyLock<Mutex<RuntimeStatus>> = LazyLock::new(|| {
    Mutex::new(RuntimeStatus {
        state: "not_initialized".into(),
        ..RuntimeStatus::default()
    })
});

/// Runtime capability, distinct from configuration intent. Even operational
/// gestures can be suppressed by another client's active pointer grab.
pub(crate) fn runtime_status() -> serde_json::Value {
    let status = RUNTIME_STATUS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    serde_json::json!({
        "available": status.available,
        "server_version": status.server_version.map(|(major, minor)| format!("{major}.{minor}")),
        "gesture_devices": status.gesture_devices,
        "device_supported": status.gesture_devices > 0,
        "active": status.active,
        "operational": status.available && status.active && status.gesture_devices > 0,
        "state": status.state,
        "reason": status.reason,
    })
}

pub(crate) fn change_status(update: impl FnOnce(&mut RuntimeStatus)) {
    let mut status = RUNTIME_STATUS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    update(&mut status);
}
