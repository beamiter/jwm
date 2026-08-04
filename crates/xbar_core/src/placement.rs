//! Pure monitor-to-bar placement and EWMH strut calculations.
//!
//! Window-system frontends apply the returned values with their own APIs. The
//! calculations stay here so XCB, x11rb, winit, tao, and Tauri agree on scale
//! rounding, minimum dimensions, and top-edge reservation.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::MonitorGeometry;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacementError {
    InvalidLogicalHeight,
    InvalidScaleFactor,
}

impl fmt::Display for PlacementError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLogicalHeight => {
                f.write_str("logical bar height must be finite and greater than zero")
            }
            Self::InvalidScaleFactor => {
                f.write_str("output scale factor must be finite and greater than zero")
            }
        }
    }
}

impl std::error::Error for PlacementError {}

/// Physical top-bar rectangle ready for a native window placement API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BarPlacement {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl BarPlacement {
    /// Place a logical-height bar on the top edge of `monitor`.
    pub fn top(
        monitor: MonitorGeometry,
        logical_height: f64,
        scale_factor: f64,
    ) -> Result<Self, PlacementError> {
        if !logical_height.is_finite() || logical_height <= 0.0 {
            return Err(PlacementError::InvalidLogicalHeight);
        }
        if !scale_factor.is_finite() || scale_factor <= 0.0 {
            return Err(PlacementError::InvalidScaleFactor);
        }

        let physical_height = (logical_height * scale_factor)
            .round()
            .clamp(1.0, f64::from(u32::MAX)) as u32;
        Ok(Self {
            x: monitor.x,
            y: monitor.y,
            width: monitor.width.max(1),
            height: physical_height,
        })
    }

    /// EWMH top struts matching this physical rectangle.
    #[must_use]
    pub fn ewmh_strut(self) -> EwmhStrut {
        EwmhStrut::top(self)
    }
}

/// Values for `_NET_WM_STRUT` and `_NET_WM_STRUT_PARTIAL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EwmhStrut {
    pub basic: [u32; 4],
    pub partial: [u32; 12],
}

impl EwmhStrut {
    #[must_use]
    pub fn top(placement: BarPlacement) -> Self {
        let top = u32::try_from(placement.y)
            .unwrap_or(0)
            .saturating_add(placement.height);
        let top_start_x = u32::try_from(placement.x).unwrap_or(0);
        let top_end_x = top_start_x.saturating_add(placement.width.saturating_sub(1));
        Self {
            basic: [0, 0, top, 0],
            partial: [0, 0, top, 0, 0, 0, 0, 0, top_start_x, top_end_x, 0, 0],
        }
    }
}

/// wlr-layer-shell layer selection for a Wayland bar surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LayerShellLayer {
    Background,
    Bottom,
    Top,
    Overlay,
}

/// Edge anchors for a layer surface.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerShellAnchors {
    pub top: bool,
    pub bottom: bool,
    pub left: bool,
    pub right: bool,
}

/// Complete wlr-layer-shell configuration for a bar, expressed as data.
///
/// Mirrors [`DockWindowSpec`] for Wayland: the values a frontend passes to
/// `zwlr_layer_surface_v1` (via smithay-client-toolkit or a raw binding) live
/// here so X11 and Wayland bars reserve identical logical space. Sizes are in
/// logical (surface-local) coordinates, matching the layer-shell protocol.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayerShellPlacement {
    /// `namespace` for `get_layer_surface`, conventionally the bar name.
    pub namespace: String,
    pub layer: LayerShellLayer,
    pub anchors: LayerShellAnchors,
    /// `set_exclusive_zone` value: logical pixels reserved for the bar.
    pub exclusive_zone: i32,
    /// Margins outside the anchored edges: top, right, bottom, left.
    pub margins: [i32; 4],
    /// `set_size` height; width 0 lets the compositor stretch between the
    /// left/right anchors.
    pub logical_height: u32,
}

impl LayerShellPlacement {
    /// A top bar spanning the full output width on the `Top` layer, with an
    /// exclusive zone equal to its height.
    pub fn top(namespace: impl Into<String>, logical_height: f64) -> Result<Self, PlacementError> {
        if !logical_height.is_finite() || logical_height <= 0.0 {
            return Err(PlacementError::InvalidLogicalHeight);
        }
        let height = logical_height.ceil().clamp(1.0, f64::from(u32::MAX)) as u32;
        Ok(Self {
            namespace: namespace.into(),
            layer: LayerShellLayer::Top,
            anchors: LayerShellAnchors {
                top: true,
                bottom: false,
                left: true,
                right: true,
            },
            exclusive_zone: i32::try_from(height).unwrap_or(i32::MAX),
            margins: [0; 4],
            logical_height: height,
        })
    }
}

/// One EWMH property write expressed as data.
///
/// `name` and any [`DockPropertyValue::Atoms`] entries are atom *names*; the
/// frontend interns them with its own connection and writes the value with its
/// native `change_property` call. This keeps the dock/strut protocol identical
/// across XCB and x11rb without a connection dependency here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockProperty {
    pub name: &'static str,
    pub value: DockPropertyValue,
}

/// Typed value for a [`DockProperty`] write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DockPropertyValue {
    /// `ATOM[]` property; entries are atom names to intern.
    Atoms(Vec<&'static str>),
    /// `CARDINAL[]` property.
    Cardinals(Vec<u32>),
    /// `UTF8_STRING` text property.
    Utf8Text(String),
}

/// Complete EWMH property set for a top dock bar window.
///
/// [`DockWindowSpec::properties`] describes the initial window setup;
/// [`DockWindowSpec::strut_properties`] describes the two writes that must be
/// repeated whenever monitor geometry moves the bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockWindowSpec {
    pub title: String,
    pub strut: EwmhStrut,
}

impl DockWindowSpec {
    /// Dock spec for `title` covering the physical `placement` rectangle.
    #[must_use]
    pub fn top(title: impl Into<String>, placement: BarPlacement) -> Self {
        Self {
            title: title.into(),
            strut: placement.ewmh_strut(),
        }
    }

    /// Every property to write when the dock window is created.
    #[must_use]
    pub fn properties(&self) -> Vec<DockProperty> {
        let mut properties = vec![
            DockProperty {
                name: "_NET_WM_WINDOW_TYPE",
                value: DockPropertyValue::Atoms(vec!["_NET_WM_WINDOW_TYPE_DOCK"]),
            },
            DockProperty {
                name: "_NET_WM_STATE",
                value: DockPropertyValue::Atoms(vec!["_NET_WM_STATE_ABOVE"]),
            },
            DockProperty {
                name: "_NET_WM_DESKTOP",
                value: DockPropertyValue::Cardinals(vec![u32::MAX]),
            },
        ];
        properties.extend(self.strut_properties());
        properties.push(DockProperty {
            name: "_NET_WM_NAME",
            value: DockPropertyValue::Utf8Text(self.title.clone()),
        });
        properties
    }

    /// The strut writes to repeat after each geometry change.
    #[must_use]
    pub fn strut_properties(&self) -> [DockProperty; 2] {
        [
            DockProperty {
                name: "_NET_WM_STRUT_PARTIAL",
                value: DockPropertyValue::Cardinals(self.strut.partial.to_vec()),
            },
            DockProperty {
                name: "_NET_WM_STRUT",
                value: DockPropertyValue::Cardinals(self.strut.basic.to_vec()),
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layer_shell_top_bar_reserves_its_height_and_rejects_invalid_input() {
        let placement = LayerShellPlacement::top("xbar", 38.4).unwrap();
        assert_eq!(placement.logical_height, 39);
        assert_eq!(placement.exclusive_zone, 39);
        assert_eq!(placement.layer, LayerShellLayer::Top);
        assert!(placement.anchors.top && placement.anchors.left && placement.anchors.right);
        assert!(!placement.anchors.bottom);
        assert_eq!(placement.margins, [0; 4]);
        assert_eq!(placement.namespace, "xbar");

        assert_eq!(
            LayerShellPlacement::top("xbar", 0.0),
            Err(PlacementError::InvalidLogicalHeight)
        );
        assert_eq!(
            LayerShellPlacement::top("xbar", f64::NAN),
            Err(PlacementError::InvalidLogicalHeight)
        );
    }

    #[test]
    fn dock_spec_lists_complete_property_protocol_in_write_order() {
        let placement = BarPlacement {
            x: 10,
            y: 0,
            width: 1920,
            height: 40,
        };
        let spec = DockWindowSpec::top("xcb_bar", placement);
        let properties = spec.properties();

        assert_eq!(properties.len(), 6);
        assert_eq!(properties[0].name, "_NET_WM_WINDOW_TYPE");
        assert_eq!(
            properties[0].value,
            DockPropertyValue::Atoms(vec!["_NET_WM_WINDOW_TYPE_DOCK"])
        );
        assert_eq!(
            properties[2].value,
            DockPropertyValue::Cardinals(vec![u32::MAX])
        );
        assert_eq!(properties[3].name, "_NET_WM_STRUT_PARTIAL");
        assert_eq!(
            properties[4].value,
            DockPropertyValue::Cardinals(vec![0, 0, 40, 0])
        );
        assert_eq!(
            properties[5].value,
            DockPropertyValue::Utf8Text("xcb_bar".to_owned())
        );

        let strut_only = spec.strut_properties();
        assert_eq!(strut_only[0], properties[3]);
        assert_eq!(strut_only[1], properties[4]);
    }

    #[test]
    fn top_placement_rounds_scale_and_preserves_monitor_origin() {
        let placement = BarPlacement::top(
            MonitorGeometry {
                x: 1920,
                y: 12,
                width: 2560,
                height: 1440,
            },
            38.0,
            1.25,
        )
        .unwrap();

        assert_eq!(
            placement,
            BarPlacement {
                x: 1920,
                y: 12,
                width: 2560,
                height: 48,
            }
        );
        assert_eq!(placement.ewmh_strut().basic, [0, 0, 60, 0]);
        assert_eq!(placement.ewmh_strut().partial[8], 1920);
        assert_eq!(placement.ewmh_strut().partial[9], 4479);
    }

    #[test]
    fn placement_rejects_invalid_scale_inputs_and_normalizes_width() {
        let monitor = MonitorGeometry {
            width: 0,
            ..MonitorGeometry::default()
        };
        assert_eq!(
            BarPlacement::top(monitor, 38.0, 0.0),
            Err(PlacementError::InvalidScaleFactor)
        );
        assert_eq!(
            BarPlacement::top(monitor, f64::NAN, 1.0),
            Err(PlacementError::InvalidLogicalHeight)
        );
        assert_eq!(BarPlacement::top(monitor, 0.1, 0.1).unwrap().width, 1);
        assert_eq!(BarPlacement::top(monitor, 0.1, 0.1).unwrap().height, 1);
    }

    #[test]
    fn ewmh_strut_saturates_negative_origins_like_existing_native_bars() {
        let placement = BarPlacement {
            x: -1920,
            y: -20,
            width: 1920,
            height: 40,
        };
        let strut = placement.ewmh_strut();
        assert_eq!(strut.basic, [0, 0, 40, 0]);
        assert_eq!(strut.partial[8], 0);
        assert_eq!(strut.partial[9], 1919);
    }
}
