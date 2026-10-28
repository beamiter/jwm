//! Pure monitor-to-bar placement and EWMH strut calculations.
//!
//! Window-system frontends apply the returned values with their own APIs. The
//! calculations stay here so XCB, x11rb, winit, tao, and Tauri agree on scale
//! rounding, minimum dimensions, and reservation on all four output edges.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::MonitorGeometry;

/// Screen edge occupied by a bar. Side bars keep their text upright.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BarEdge {
    #[default]
    Top,
    Bottom,
    Left,
    Right,
}

impl BarEdge {
    #[must_use]
    pub const fn is_vertical(self) -> bool {
        matches!(self, Self::Left | Self::Right)
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Top => "top",
            Self::Bottom => "bottom",
            Self::Left => "left",
            Self::Right => "right",
        }
    }

    /// The window axis that controls cell thickness, never its long axis.
    #[must_use]
    pub const fn physical_thickness(self, width: u32, height: u32) -> u32 {
        if self.is_vertical() { width } else { height }
    }
}

impl fmt::Display for BarEdge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for BarEdge {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "top" => Ok(Self::Top),
            "bottom" => Ok(Self::Bottom),
            "left" => Ok(Self::Left),
            "right" => Ok(Self::Right),
            _ => Err(format!(
                "invalid bar position {value:?}; expected top, bottom, left, or right"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacementError {
    InvalidLogicalHeight,
    InvalidScaleFactor,
}

impl fmt::Display for PlacementError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLogicalHeight => {
                f.write_str("logical bar thickness must be finite and greater than zero")
            }
            Self::InvalidScaleFactor => {
                f.write_str("output scale factor must be finite and greater than zero")
            }
        }
    }
}

impl std::error::Error for PlacementError {}

/// Physical bar rectangle ready for a native window placement API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BarPlacement {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// The four coordinated panels of the desktop shell, in physical output pixels.
/// The frontend and the WM share these rectangles and reservations so a tiled
/// window cannot end up underneath a sidebar or the floating bottom Dock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FourEdgeGeometry {
    pub top: BarPlacement,
    pub left: BarPlacement,
    pub right: BarPlacement,
    pub bottom: BarPlacement,
    /// Workspace depths in left, right, top, bottom order.
    pub reserved: [u32; 4],
}

impl FourEdgeGeometry {
    #[must_use]
    pub fn new(output: MonitorGeometry, top_thickness: u32, padding: u32) -> Self {
        let w = output.width.max(1).min(i32::MAX as u32);
        let h = output.height.max(1).min(i32::MAX as u32);
        let pad = padding.min((w.min(h) - 1) / 2);
        let top_height = top_thickness.max(1).min(h - 2 * pad);
        let unit = f64::from(top_height) / 42.0;
        let scaled = |value: f64| (value * unit).round().clamp(1.0, f64::from(u32::MAX)) as u32;
        let margin = scaled(12.0).min((w.min(h) - 1) / 6);
        let side_width = scaled(60.0).min(w.saturating_sub(2 * margin).max(1));
        let side_inset = scaled(72.0).min((h - 1) / 2);
        let bottom_height = scaled(76.0).min(h.saturating_sub(2 * margin).max(1));
        let bottom_inset = (w / 10).max(margin).min((w - 1) / 2);
        let place = |x, y, width, height| BarPlacement {
            x: bounded_offset(output.x, x),
            y: bounded_offset(output.y, y),
            width,
            height,
        };
        let top = place(pad, pad, w - 2 * pad, top_height);
        let left = place(margin, side_inset, side_width, h - 2 * side_inset);
        let right = place(
            w - margin - side_width,
            side_inset,
            side_width,
            h - 2 * side_inset,
        );
        let bottom = place(
            bottom_inset,
            h - margin - bottom_height,
            w - 2 * bottom_inset,
            bottom_height,
        );
        let horizontal = side_width.saturating_add(2 * margin).min((w - 1) / 2);
        let top_depth = top_height.saturating_add(2 * pad).min(h - 1);
        let bottom_depth = bottom_height
            .saturating_add(2 * margin)
            .min(h - 1 - top_depth);
        Self {
            top,
            left,
            right,
            bottom,
            reserved: [horizontal, horizontal, top_depth, bottom_depth],
        }
    }

    #[must_use]
    pub const fn panel(self, edge: BarEdge) -> BarPlacement {
        match edge {
            BarEdge::Top => self.top,
            BarEdge::Bottom => self.bottom,
            BarEdge::Left => self.left,
            BarEdge::Right => self.right,
        }
    }
}

#[cfg(test)]
mod four_edge_tests {
    use super::*;

    #[test]
    fn reference_panels_and_work_area_share_one_geometry() {
        let geometry = FourEdgeGeometry::new(
            MonitorGeometry {
                x: 0,
                y: 0,
                width: 1280,
                height: 800,
            },
            42,
            5,
        );
        assert_eq!(
            geometry.top,
            BarPlacement {
                x: 5,
                y: 5,
                width: 1270,
                height: 42
            }
        );
        assert_eq!(
            geometry.left,
            BarPlacement {
                x: 12,
                y: 72,
                width: 60,
                height: 656
            }
        );
        assert_eq!(
            geometry.right,
            BarPlacement {
                x: 1208,
                y: 72,
                width: 60,
                height: 656
            }
        );
        assert_eq!(
            geometry.bottom,
            BarPlacement {
                x: 128,
                y: 712,
                width: 1024,
                height: 76
            }
        );
        assert_eq!(geometry.reserved, [84, 84, 52, 100]);
    }

    #[test]
    fn tiny_and_negative_origin_outputs_keep_panels_inside_and_a_usable_pixel() {
        for (width, height) in [(1, 1), (20, 13), (320, 200), (1280, 800), (3840, 2160)] {
            for thickness in [1, 42, 128, u32::MAX] {
                let output = MonitorGeometry {
                    x: -1920,
                    y: -800,
                    width,
                    height,
                };
                let geometry = FourEdgeGeometry::new(output, thickness, 5);
                for panel in [geometry.top, geometry.left, geometry.right, geometry.bottom] {
                    assert!(panel.width > 0 && panel.height > 0);
                    assert!(panel.x >= output.x && panel.y >= output.y);
                    assert!(
                        i64::from(panel.x) + i64::from(panel.width)
                            <= i64::from(output.x) + i64::from(width)
                    );
                    assert!(
                        i64::from(panel.y) + i64::from(panel.height)
                            <= i64::from(output.y) + i64::from(height)
                    );
                }
                let [left, right, top, bottom] = geometry.reserved;
                assert!(left + right < width && top + bottom < height);
            }
        }
    }
}

impl BarPlacement {
    /// Place a logical-height bar on the top edge of `monitor`.
    pub fn top(
        monitor: MonitorGeometry,
        logical_height: f64,
        scale_factor: f64,
    ) -> Result<Self, PlacementError> {
        Self::at_edge(monitor, logical_height, scale_factor, BarEdge::Top)
    }

    /// Place a bar along a raw physical output rectangle. Thickness is
    /// clamped to that output, so even extreme scale factors cannot make a
    /// side bar consume more than its monitor's width.
    pub fn at_edge(
        monitor: MonitorGeometry,
        logical_thickness: f64,
        scale_factor: f64,
        edge: BarEdge,
    ) -> Result<Self, PlacementError> {
        let thickness = scaled_thickness(logical_thickness, scale_factor)?;
        let width = monitor.width.max(1);
        let height = monitor.height.max(1);
        let (bar_width, bar_height) = if edge.is_vertical() {
            (thickness.min(width), height)
        } else {
            (width, thickness.min(height))
        };
        let offset_x = if edge == BarEdge::Right {
            width - bar_width
        } else {
            0
        };
        let offset_y = if edge == BarEdge::Bottom {
            height - bar_height
        } else {
            0
        };
        Ok(Self {
            x: bounded_offset(monitor.x, offset_x),
            y: bounded_offset(monitor.y, offset_y),
            width: bar_width,
            height: bar_height,
        })
    }

    /// Apply JWM's monitor-info contract. Its x/y are already the padded bar
    /// origin, not the physical output origin. A vertical snapshot carries
    /// the actual bar width and available height; a horizontal snapshot
    /// carries the available width and output height. Reapplying `at_edge`
    /// here would move bottom and right bars twice.
    pub fn from_bar_geometry(
        geometry: MonitorGeometry,
        logical_thickness: f64,
        scale_factor: f64,
        edge: BarEdge,
    ) -> Result<Self, PlacementError> {
        let thickness = scaled_thickness(logical_thickness, scale_factor)?;
        Ok(Self {
            x: geometry.x,
            y: geometry.y,
            width: geometry.width.max(1),
            height: if edge.is_vertical() {
                geometry.height.max(1)
            } else {
                thickness.min(geometry.height.max(1))
            },
        })
    }

    /// EWMH top struts matching this physical rectangle. New callers should
    /// use `EwmhStrut::for_edge` with the actual root rectangle.
    #[must_use]
    pub fn ewmh_strut(self) -> EwmhStrut {
        EwmhStrut::top(self)
    }
}

fn bounded_offset(origin: i32, offset: u32) -> i32 {
    (i64::from(origin) + i64::from(offset)).clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

fn scaled_thickness(logical_thickness: f64, scale_factor: f64) -> Result<u32, PlacementError> {
    if !logical_thickness.is_finite() || logical_thickness <= 0.0 {
        return Err(PlacementError::InvalidLogicalHeight);
    }
    if !scale_factor.is_finite() || scale_factor <= 0.0 {
        return Err(PlacementError::InvalidScaleFactor);
    }

    Ok((logical_thickness * scale_factor)
        .round()
        .clamp(1.0, f64::from(u32::MAX)) as u32)
}

/// Values for `_NET_WM_STRUT` and `_NET_WM_STRUT_PARTIAL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EwmhStrut {
    pub basic: [u32; 4],
    pub partial: [u32; 12],
}

impl EwmhStrut {
    /// Calculate root-relative EWMH distances and inclusive partial ranges.
    /// Bottom/right distances must use the complete X root, not just the
    /// selected monitor. Coordinates outside the root are clipped. A bar
    /// outside the root reserves no space.
    ///
    /// EWMH cannot express a reservation on an interior monitor edge without
    /// spanning the intervening root area. A window manager with per-output
    /// work areas should clear that reservation itself; this routine merely
    /// expresses the requested root-edge protocol accurately.
    #[must_use]
    pub fn for_edge(placement: BarPlacement, root: MonitorGeometry, edge: BarEdge) -> Self {
        let mut strut = Self {
            basic: [0; 4],
            partial: [0; 12],
        };
        let root_width = i64::from(root.width);
        let root_height = i64::from(root.height);
        let x = i64::from(placement.x) - i64::from(root.x);
        let y = i64::from(placement.y) - i64::from(root.y);
        let left = x.clamp(0, root_width);
        let top = y.clamp(0, root_height);
        let right = (x + i64::from(placement.width)).clamp(0, root_width);
        let bottom = (y + i64::from(placement.height)).clamp(0, root_height);
        if left >= right || top >= bottom {
            return strut;
        }
        let (index, range, depth, start, end) = match edge {
            BarEdge::Left => (0, 4, right, top, bottom - 1),
            BarEdge::Right => (1, 6, root_width - left, top, bottom - 1),
            BarEdge::Top => (2, 8, bottom, left, right - 1),
            BarEdge::Bottom => (3, 10, root_height - top, left, right - 1),
        };
        strut.basic[index] = depth as u32;
        strut.partial[index] = depth as u32;
        strut.partial[range] = start as u32;
        strut.partial[range + 1] = end as u32;
        strut
    }

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
    #[serde(default)]
    pub logical_width: u32,
    pub logical_height: u32,
}

impl LayerShellPlacement {
    /// A top bar spanning the full output width on the `Top` layer, with an
    /// exclusive zone equal to its height.
    pub fn top(namespace: impl Into<String>, logical_height: f64) -> Result<Self, PlacementError> {
        Self::at_edge(namespace, logical_height, BarEdge::Top)
    }

    /// Anchor to one edge and stretch along its two perpendicular edges.
    /// A zero long-axis size asks the compositor to fill the output.
    pub fn at_edge(
        namespace: impl Into<String>,
        logical_thickness: f64,
        edge: BarEdge,
    ) -> Result<Self, PlacementError> {
        if !logical_thickness.is_finite() || logical_thickness <= 0.0 {
            return Err(PlacementError::InvalidLogicalHeight);
        }
        let thickness = logical_thickness.ceil().clamp(1.0, f64::from(i32::MAX)) as u32;
        Ok(Self {
            namespace: namespace.into(),
            layer: LayerShellLayer::Top,
            anchors: LayerShellAnchors {
                top: edge != BarEdge::Bottom,
                bottom: edge != BarEdge::Top,
                left: edge != BarEdge::Right,
                right: edge != BarEdge::Left,
            },
            exclusive_zone: thickness as i32,
            margins: [0; 4],
            logical_width: if edge.is_vertical() { thickness } else { 0 },
            logical_height: if edge.is_vertical() { 0 } else { thickness },
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

/// Complete EWMH property set for a dock bar window on any edge.
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
    #[must_use]
    pub fn for_edge(
        title: impl Into<String>,
        placement: BarPlacement,
        root: MonitorGeometry,
        edge: BarEdge,
    ) -> Self {
        Self {
            title: title.into(),
            strut: EwmhStrut::for_edge(placement, root, edge),
        }
    }

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
    fn all_four_edges_use_the_short_axis_and_preserve_output_origin() {
        let monitor = MonitorGeometry {
            x: -1440,
            y: 240,
            width: 1440,
            height: 900,
        };
        let expected = [
            (
                BarEdge::Top,
                BarPlacement {
                    x: -1440,
                    y: 240,
                    width: 1440,
                    height: 48,
                },
            ),
            (
                BarEdge::Bottom,
                BarPlacement {
                    x: -1440,
                    y: 1092,
                    width: 1440,
                    height: 48,
                },
            ),
            (
                BarEdge::Left,
                BarPlacement {
                    x: -1440,
                    y: 240,
                    width: 48,
                    height: 900,
                },
            ),
            (
                BarEdge::Right,
                BarPlacement {
                    x: -48,
                    y: 240,
                    width: 48,
                    height: 900,
                },
            ),
        ];
        for (edge, expected) in expected {
            let placement = BarPlacement::at_edge(monitor, 38.0, 1.25, edge).unwrap();
            assert_eq!(placement, expected, "{edge}");
            assert_eq!(
                edge.physical_thickness(placement.width, placement.height),
                48
            );
            assert_eq!(edge.as_str().parse::<BarEdge>().unwrap(), edge);
        }
        assert_eq!(" RIGHT ".parse::<BarEdge>().unwrap(), BarEdge::Right);
        assert!("diagonal".parse::<BarEdge>().is_err());
        assert_eq!(BarEdge::default(), BarEdge::Top);
    }

    #[test]
    fn thickness_cannot_exceed_an_output_or_overflow_coordinates() {
        let monitor = MonitorGeometry {
            x: 100,
            y: 200,
            width: 20,
            height: 10,
        };
        for edge in [BarEdge::Top, BarEdge::Bottom, BarEdge::Left, BarEdge::Right] {
            assert_eq!(
                BarPlacement::at_edge(monitor, f64::MAX, f64::MAX, edge).unwrap(),
                BarPlacement {
                    x: 100,
                    y: 200,
                    width: 20,
                    height: 10
                }
            );
            let extreme = MonitorGeometry {
                x: i32::MAX,
                y: i32::MAX,
                width: u32::MAX,
                height: u32::MAX,
            };
            let bounded = BarPlacement::at_edge(extreme, 1.0, 1.0, edge).unwrap();
            assert!(bounded.x >= 0 && bounded.y >= 0);
        }
    }

    #[test]
    fn runtime_snapshots_do_not_offset_bottom_or_right_twice() {
        let bottom = MonitorGeometry {
            x: 16,
            y: 1032,
            width: 1888,
            height: 1048,
        };
        assert_eq!(
            BarPlacement::from_bar_geometry(bottom, 32.0, 1.5, BarEdge::Bottom).unwrap(),
            BarPlacement {
                x: 16,
                y: 1032,
                width: 1888,
                height: 48
            }
        );
        let right = MonitorGeometry {
            x: 1872,
            y: 16,
            width: 48,
            height: 1048,
        };
        assert_eq!(
            BarPlacement::from_bar_geometry(right, 32.0, 1.5, BarEdge::Right).unwrap(),
            BarPlacement {
                x: 1872,
                y: 16,
                width: 48,
                height: 1048
            }
        );
    }

    #[test]
    fn ewmh_all_edges_use_root_extents_and_inclusive_partial_ranges() {
        let root = MonitorGeometry {
            x: 0,
            y: 0,
            width: 3840,
            height: 2160,
        };
        let monitor = MonitorGeometry {
            x: 1920,
            y: 1080,
            width: 1920,
            height: 1080,
        };
        for (edge, basic, range, endpoints) in [
            (BarEdge::Top, [0, 0, 1120, 0], 8, [1920, 3839]),
            (BarEdge::Bottom, [0, 0, 0, 40], 10, [1920, 3839]),
            (BarEdge::Left, [1960, 0, 0, 0], 4, [1080, 2159]),
            (BarEdge::Right, [0, 40, 0, 0], 6, [1080, 2159]),
        ] {
            let placement = BarPlacement::at_edge(monitor, 40.0, 1.0, edge).unwrap();
            let strut = EwmhStrut::for_edge(placement, root, edge);
            assert_eq!(strut.basic, basic, "{edge}");
            assert_eq!(&strut.partial[..4], &basic);
            assert_eq!(&strut.partial[range..range + 2], &endpoints);
            for index in 4..12 {
                if !(range..range + 2).contains(&index) {
                    assert_eq!(strut.partial[index], 0, "unused range on {edge}");
                }
            }
        }
    }

    #[test]
    fn struts_clip_partial_visibility_and_handle_negative_root_origins() {
        let root = MonitorGeometry {
            x: -1920,
            y: -1080,
            width: 3840,
            height: 2160,
        };
        let placement = BarPlacement {
            x: -2020,
            y: -1090,
            width: 1100,
            height: 40,
        };
        let strut = EwmhStrut::for_edge(placement, root, BarEdge::Top);
        assert_eq!(strut.basic, [0, 0, 30, 0]);
        assert_eq!(&strut.partial[8..10], &[0, 999]);
        let outside = BarPlacement {
            x: i32::MAX,
            y: i32::MAX,
            width: u32::MAX,
            height: u32::MAX,
        };
        assert_eq!(
            EwmhStrut::for_edge(outside, root, BarEdge::Right).partial,
            [0; 12]
        );
        assert_eq!(
            EwmhStrut::for_edge(placement, MonitorGeometry::default(), BarEdge::Top).basic,
            [0; 4]
        );
    }

    #[test]
    fn layer_shell_stretches_the_correct_axis_for_every_edge() {
        for edge in [BarEdge::Top, BarEdge::Bottom, BarEdge::Left, BarEdge::Right] {
            let layer = LayerShellPlacement::at_edge("xbar", 38.4, edge).unwrap();
            assert_eq!(layer.exclusive_zone, 39);
            assert_eq!(
                (layer.logical_width, layer.logical_height),
                if edge.is_vertical() { (39, 0) } else { (0, 39) }
            );
            assert_eq!(layer.anchors.top, edge != BarEdge::Bottom);
            assert_eq!(layer.anchors.bottom, edge != BarEdge::Top);
            assert_eq!(layer.anchors.left, edge != BarEdge::Right);
            assert_eq!(layer.anchors.right, edge != BarEdge::Left);
        }
    }

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
