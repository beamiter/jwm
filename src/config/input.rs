//! Device policy shared by configuration validation and the libinput backend.
#![cfg_attr(not(feature = "backend-wayland-udev"), allow(dead_code))]
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct InputConfig {
    pub touchpad: TouchpadOptions,
    /// Matching rules are applied in file order; later explicit values win.
    pub touchpad_rules: Vec<TouchpadRule>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TouchpadOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tap: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tap_drag: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drag_lock: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub natural_scroll: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disable_while_typing: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disable_while_trackpointing: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub left_handed: Option<bool>,
    /// "button_areas" or "clickfinger"; unsupported requests are diagnosed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub click_method: Option<String>,
    /// "two_finger", "edge" or "none".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scroll_method: Option<String>,
    /// "adaptive" or "flat".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accel_profile: Option<String>,
    /// libinput speed in the inclusive range [-1, 1].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accel_speed: Option<f64>,
    /// "enabled", "disabled" or "disabled_on_external_mouse".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub send_events: Option<String>,
}

impl TouchpadOptions {
    pub(crate) fn overlay(&mut self, other: &Self) {
        macro_rules! merge {
            ($($field:ident),+ $(,)?) => { $(
                if other.$field.is_some() {
                    self.$field = other.$field.clone();
                }
            )+ };
        }
        merge!(
            tap,
            tap_drag,
            drag_lock,
            natural_scroll,
            disable_while_typing,
            disable_while_trackpointing,
            left_handed,
            click_method,
            scroll_method,
            accel_profile,
            accel_speed,
            send_events
        );
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TouchpadRule {
    /// Case-insensitive substring of the descriptive device name.
    pub name: Option<String>,
    /// Stable identifier reported by get_inputs, unlike eventN hotplug names.
    pub identifier: Option<String>,
    pub vendor: Option<u32>,
    pub product: Option<u32>,
    /// Restrict a rule to devices with (or without) native gesture support.
    pub gestures: Option<bool>,
    #[serde(flatten)]
    pub settings: TouchpadOptions,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InputDeviceIdentity {
    pub name: String,
    pub identifier: String,
    pub vendor: u32,
    pub product: u32,
    pub touchpad: bool,
    pub gestures: bool,
}

impl TouchpadRule {
    pub(crate) fn matches(&self, device: &InputDeviceIdentity) -> bool {
        device.touchpad
            && self.name.as_ref().is_none_or(|name| {
                !name.trim().is_empty() && device.name.to_lowercase().contains(&name.to_lowercase())
            })
            && self
                .identifier
                .as_ref()
                .is_none_or(|id| id == &device.identifier)
            && self.vendor.is_none_or(|vendor| vendor == device.vendor)
            && self.product.is_none_or(|product| product == device.product)
            && self
                .gestures
                .is_none_or(|gestures| gestures == device.gestures)
    }
}

impl InputConfig {
    pub(crate) fn for_device(&self, device: &InputDeviceIdentity) -> TouchpadOptions {
        if !device.touchpad {
            return TouchpadOptions::default();
        }
        let mut options = self.touchpad.clone();
        for rule in &self.touchpad_rules {
            if rule.matches(device) {
                options.overlay(&rule.settings);
            }
        }
        options
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device() -> InputDeviceIdentity {
        InputDeviceIdentity {
            name: "ELAN Internal Touchpad".into(),
            identifier: "04f3:1234:ELAN Internal Touchpad".into(),
            vendor: 0x04f3,
            product: 0x1234,
            touchpad: true,
            gestures: true,
        }
    }

    #[test]
    fn omitted_settings_preserve_defaults_and_false_is_an_override() {
        let config: InputConfig = toml::from_str(
            "[touchpad]\ntap = true\nnatural_scroll = true\n\
             [[touchpad_rules]]\nname = 'elan'\ntap = false\n",
        )
        .unwrap();
        let options = config.for_device(&device());
        assert_eq!(options.tap, Some(false));
        assert_eq!(options.natural_scroll, Some(true));
        assert_eq!(options.accel_speed, None);
        assert_eq!(options.disable_while_typing, None);
    }

    #[test]
    fn rules_match_all_selectors_and_later_values_win() {
        let config: InputConfig = toml::from_str(
            "[[touchpad_rules]]\nvendor = 0x04f3\nproduct = 0x1234\n\
             gestures = true\naccel_speed = 0.2\n\
             [[touchpad_rules]]\nidentifier = '04f3:1234:ELAN Internal Touchpad'\n\
             accel_speed = -0.2\n[[touchpad_rules]]\nproduct = 9\naccel_speed = 0.9\n",
        )
        .unwrap();
        assert_eq!(config.for_device(&device()).accel_speed, Some(-0.2));
        let mut mouse = device();
        mouse.touchpad = false;
        assert_eq!(config.for_device(&mouse), TouchpadOptions::default());
    }

    #[test]
    fn configuration_roundtrips_without_inventing_defaults() {
        let config = InputConfig::default();
        let text = toml::to_string(&config).unwrap();
        assert_eq!(toml::from_str::<InputConfig>(&text).unwrap(), config);
        assert!(!text.contains("tap ="));
    }
}
