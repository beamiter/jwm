//! The compositor owns the physical rectangle of each managed bar.
//!
//! Its output edge, reserved workspace strip and shared-message origin are
//! derived together. Frontends receive that origin verbatim, including on
//! bottom/right bars and outputs with negative coordinates.

use crate::config::Config;
use crate::core::models::WMMonitor;
use crate::core::types::Rect;
use xbar_core::placement::BarEdge;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BarGeometry {
    pub rect: Rect,
    pub edge: BarEdge,
    pub reservation: i32,
}

impl BarGeometry {
    pub(crate) fn for_monitor(monitor: &WMMonitor, config: &Config) -> Self {
        let geometry = &monitor.geometry;
        Self::new(
            Rect::new(geometry.m_x, geometry.m_y, geometry.m_w, geometry.m_h),
            config.status_bar_position_for_monitor(monitor.num),
            config.status_bar_height(),
            config.status_bar_padding(),
        )
    }

    pub(crate) fn new(output: Rect, edge: BarEdge, thickness: i32, padding: i32) -> Self {
        let width = output.w.max(1);
        let height = output.h.max(1);
        // Extremely small outputs and stale hand-edited values still produce
        // a representable positive surface without integer wraparound.
        let pad = padding.max(0).min((width.min(height) - 1) / 2);
        let cross_extent = if edge.is_vertical() { width } else { height };
        let thickness = thickness.max(1).min(cross_extent - 2 * pad);
        let (x, y, w, h) = match edge {
            BarEdge::Top => (pad, pad, width - 2 * pad, thickness),
            BarEdge::Bottom => (pad, height - pad - thickness, width - 2 * pad, thickness),
            BarEdge::Left => (pad, pad, thickness, height - 2 * pad),
            BarEdge::Right => (width - pad - thickness, pad, thickness, height - 2 * pad),
        };
        Self {
            rect: Rect::new(output.x.saturating_add(x), output.y.saturating_add(y), w, h),
            edge,
            // Layout engines always receive at least one physical pixel.
            reservation: thickness.saturating_add(2 * pad).min(cross_extent - 1),
        }
    }

    /// Only subtract the portion not already taken by external struts.
    pub(crate) fn unreserved_depth(self, physical: Rect, work: Rect) -> i32 {
        let covered = match self.edge {
            BarEdge::Top => i64::from(work.y) - i64::from(physical.y),
            BarEdge::Bottom => {
                i64::from(physical.y) + i64::from(physical.h)
                    - i64::from(work.y)
                    - i64::from(work.h)
            }
            BarEdge::Left => i64::from(work.x) - i64::from(physical.x),
            BarEdge::Right => {
                i64::from(physical.x) + i64::from(physical.w)
                    - i64::from(work.x)
                    - i64::from(work.w)
            }
        };
        (i64::from(self.reservation) - covered.max(0)).max(0) as i32
    }
}

/// EWMH describes root edges, while JWM reserves bars separately per output.
/// An interior output edge therefore advertises no root strut: encoding it
/// as a deep root strip would also eat a neighbouring monitor's workspace.
/// The complete padded depth belongs to the reservation, not just the pixels
/// inside the native window. Ranges are inclusive and root-origin-relative.
pub(crate) fn bar_strut(bar: BarGeometry, output: Rect, root: Rect) -> [u32; 12] {
    let mut partial = [0; 12];
    let x = i64::from(output.x) - i64::from(root.x);
    let y = i64::from(output.y) - i64::from(root.y);
    let right = x + i64::from(output.w.max(1));
    let bottom = y + i64::from(output.h.max(1));
    let root_width = i64::from(root.w.max(1));
    let root_height = i64::from(root.h.max(1));
    let depth = bar.reservation.max(0) as u32;
    let horizontal = (x.max(0) as u32, (right - 1).clamp(0, root_width - 1) as u32);
    let vertical = (
        y.max(0) as u32,
        (bottom - 1).clamp(0, root_height - 1) as u32,
    );
    match bar.edge {
        BarEdge::Top if y == 0 => {
            partial[2] = depth;
            partial[8] = horizontal.0;
            partial[9] = horizontal.1;
        }
        BarEdge::Bottom if bottom == root_height => {
            partial[3] = depth;
            partial[10] = horizontal.0;
            partial[11] = horizontal.1;
        }
        BarEdge::Left if x == 0 => {
            partial[0] = depth;
            partial[4] = vertical.0;
            partial[5] = vertical.1;
        }
        BarEdge::Right if right == root_width => {
            partial[1] = depth;
            partial[6] = vertical.0;
            partial[7] = vertical.1;
        }
        _ => {}
    }
    partial
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn four_edges_share_one_padding_and_physical_thickness_policy() {
        let output = Rect::new(-1920, -1080, 1920, 1080);
        for (edge, expected) in [
            (BarEdge::Top, Rect::new(-1915, -1075, 1910, 42)),
            (BarEdge::Bottom, Rect::new(-1915, -47, 1910, 42)),
            (BarEdge::Left, Rect::new(-1915, -1075, 42, 1070)),
            (BarEdge::Right, Rect::new(-47, -1075, 42, 1070)),
        ] {
            let bar = BarGeometry::new(output, edge, 42, 5);
            assert_eq!(bar.rect, expected);
            assert_eq!(bar.reservation, 52);
            assert_eq!(bar.unreserved_depth(output, output), 52);
        }
    }

    #[test]
    fn existing_external_struts_are_discounted_on_the_correct_edge() {
        let output = Rect::new(-1200, 200, 1200, 900);
        for (edge, work) in [
            (BarEdge::Top, Rect::new(-1200, 230, 1200, 870)),
            (BarEdge::Bottom, Rect::new(-1200, 200, 1200, 870)),
            (BarEdge::Left, Rect::new(-1170, 200, 1170, 900)),
            (BarEdge::Right, Rect::new(-1200, 200, 1170, 900)),
        ] {
            assert_eq!(
                BarGeometry::new(output, edge, 42, 5).unreserved_depth(output, work),
                22
            );
        }
    }

    #[test]
    fn tiny_outputs_and_extreme_config_remain_bounded() {
        for edge in [BarEdge::Top, BarEdge::Bottom, BarEdge::Left, BarEdge::Right] {
            for (width, height) in [(0, 0), (1, 1), (12, 3), (50, 100)] {
                let bar =
                    BarGeometry::new(Rect::new(0, 0, width, height), edge, i32::MAX, i32::MAX);
                assert!(bar.rect.w > 0 && bar.rect.h > 0);
                assert!(bar.rect.x >= 0 && bar.rect.y >= 0);
                assert!(bar.rect.x + bar.rect.w <= width.max(1));
                assert!(bar.rect.y + bar.rect.h <= height.max(1));
                let extent = if edge.is_vertical() { width } else { height };
                assert!(bar.reservation < extent.max(1));
            }
        }
    }

    #[test]
    fn spawned_thickness_recreates_the_wm_rect_on_small_scaled_outputs() {
        let output = Rect::new(-90, 10, 90, 70);
        for edge in [BarEdge::Top, BarEdge::Bottom, BarEdge::Left, BarEdge::Right] {
            let config = Config::default()
                .with_validated_value("status_bar.position", &serde_json::json!(edge.as_str()))
                .unwrap()
                .with_validated_value("appearance.status_bar_height", &serde_json::json!(128))
                .unwrap();
            let launch = crate::jwm::types::BarLaunchConfig::for_output(&config, 0, output);
            let geometry = BarGeometry::new(output, edge, 128, config.status_bar_padding());
            let snapshot = xbar_core::MonitorGeometry {
                x: geometry.rect.x,
                y: geometry.rect.y,
                width: geometry.rect.w as u32,
                height: if edge.is_vertical() {
                    geometry.rect.h as u32
                } else {
                    output.h as u32
                },
            };
            for scale in [1.0, 1.5, 2.0] {
                let frontend = xbar_core::BarPlacement::from_bar_geometry(
                    snapshot,
                    f64::from(launch.thickness) / scale,
                    scale,
                    edge,
                )
                .unwrap();
                assert_eq!((frontend.x, frontend.y), (geometry.rect.x, geometry.rect.y));
                assert_eq!(
                    (frontend.width, frontend.height),
                    (geometry.rect.w as u32, geometry.rect.h as u32)
                );
            }
        }
    }

    #[test]
    fn root_struts_use_all_four_sides_and_never_reserve_another_output() {
        let root = Rect::new(-1920, -1080, 3840, 2160);
        let upper_left = Rect::new(-1920, -1080, 1920, 1080);
        let lower_right = Rect::new(0, 0, 1920, 1080);
        let top = bar_strut(
            BarGeometry::new(upper_left, BarEdge::Top, 42, 5),
            upper_left,
            root,
        );
        assert_eq!(top, [0, 0, 52, 0, 0, 0, 0, 0, 0, 1919, 0, 0]);
        let bottom = bar_strut(
            BarGeometry::new(lower_right, BarEdge::Bottom, 42, 5),
            lower_right,
            root,
        );
        assert_eq!(bottom, [0, 0, 0, 52, 0, 0, 0, 0, 0, 0, 1920, 3839]);
        let left = bar_strut(
            BarGeometry::new(upper_left, BarEdge::Left, 42, 5),
            upper_left,
            root,
        );
        assert_eq!(left, [52, 0, 0, 0, 0, 1079, 0, 0, 0, 0, 0, 0]);
        let right = bar_strut(
            BarGeometry::new(lower_right, BarEdge::Right, 42, 5),
            lower_right,
            root,
        );
        assert_eq!(right, [0, 52, 0, 0, 0, 0, 1080, 2159, 0, 0, 0, 0]);
        for (output, edge) in [
            (upper_left, BarEdge::Right),
            (upper_left, BarEdge::Bottom),
            (lower_right, BarEdge::Top),
            (lower_right, BarEdge::Left),
        ] {
            assert_eq!(
                bar_strut(BarGeometry::new(output, edge, 42, 5), output, root),
                [0; 12]
            );
        }
    }
}
