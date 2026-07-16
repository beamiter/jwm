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

#[cfg(test)]
mod tests {
    use super::*;

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
