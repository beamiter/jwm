//! Four-edge shell policy. Native hosts own windows; this module owns timing
//! and bounded physical geometry so revealing one edge cannot reveal another.
use serde::Deserialize;
use std::time::{Duration, Instant};

use crate::{BarEdge, BarPlacement, MonitorGeometry};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevealMode {
    #[default]
    Hover,
    Always,
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EdgeShellConfig {
    /// The official Tao/Glow frontend hosts these companion panels.
    pub enabled: bool,
    pub left: RevealMode,
    pub right: RevealMode,
    pub bottom: RevealMode,
    pub reveal_delay_ms: u64,
    pub hide_delay_ms: u64,
    pub trigger_pixels: u32,
}

impl Default for EdgeShellConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            left: RevealMode::Hover,
            right: RevealMode::Hover,
            bottom: RevealMode::Hover,
            reveal_delay_ms: 80,
            hide_delay_ms: 400,
            trigger_pixels: 2,
        }
    }
}

impl EdgeShellConfig {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.reveal_delay_ms > 2_000 || self.hide_delay_ms > 5_000 {
            return Err("reveal_delay_ms must be 0..2000 and hide_delay_ms 0..5000");
        }
        if !(1..=8).contains(&self.trigger_pixels) {
            return Err("trigger_pixels must be 1..8 physical pixels");
        }
        Ok(())
    }

    pub const fn mode(&self, edge: BarEdge) -> RevealMode {
        if !self.enabled {
            return RevealMode::Disabled;
        }
        match edge {
            BarEdge::Left => self.left,
            BarEdge::Right => self.right,
            BarEdge::Bottom => self.bottom,
            BarEdge::Top => RevealMode::Disabled,
        }
    }
}

#[derive(Debug)]
pub struct EdgeReveal {
    mode: RevealMode,
    visible: bool,
    pending: Option<(Instant, bool)>,
}

impl EdgeReveal {
    pub fn new(mode: RevealMode) -> Self {
        Self {
            mode,
            visible: mode == RevealMode::Always,
            pending: None,
        }
    }

    pub const fn visible(&self) -> bool {
        self.visible
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.pending.map(|pending| pending.0)
    }

    /// Change a live edge independently, cancelling deadlines from its old mode.
    pub fn set_mode(&mut self, mode: RevealMode, pointer_inside: bool) {
        self.mode = mode;
        self.pending = None;
        self.visible = mode == RevealMode::Always || (mode == RevealMode::Hover && pointer_inside);
    }

    pub fn enter(&mut self, now: Instant, config: &EdgeShellConfig) {
        self.pending = if self.mode == RevealMode::Hover && !self.visible {
            Some((now + Duration::from_millis(config.reveal_delay_ms), true))
        } else {
            None
        };
    }

    pub fn leave(&mut self, now: Instant, config: &EdgeShellConfig) {
        self.pending = if self.mode == RevealMode::Hover && self.visible {
            Some((now + Duration::from_millis(config.hide_delay_ms), false))
        } else {
            None
        };
    }

    pub fn tick(&mut self, now: Instant) -> bool {
        if let Some((deadline, visible)) = self.pending
            && now >= deadline
        {
            self.pending = None;
            let changed = self.visible != visible;
            self.visible = visible;
            return changed;
        }
        false
    }
}

/// A separate input-only trigger covers the physical edge, independently of
/// the centered panel's length. Shared corners remain available to their
/// adjoining edge instead of two windows competing for the same pixels.
pub fn trigger_geometry(output: MonitorGeometry, edge: BarEdge, trigger: u32) -> BarPlacement {
    let width = output.width.clamp(1, 16_384);
    let height = output.height.clamp(1, 16_384);
    let t = trigger.clamp(1, 8).min(width).min(height);
    let corner = t.min((if edge.is_vertical() { height } else { width }).saturating_sub(1) / 2);
    let (x, y, w, h) = match edge {
        BarEdge::Left => (0, corner, t, height - 2 * corner),
        BarEdge::Right => (width - t, corner, t, height - 2 * corner),
        BarEdge::Bottom => (corner, height - t, width - 2 * corner, t),
        BarEdge::Top => (corner, 0, width - 2 * corner, t),
    };
    BarPlacement {
        x: output.x.saturating_add(x as i32),
        y: output.y.saturating_add(y as i32),
        width: w,
        height: h,
    }
}

/// Root-coordinate containment also handles negative monitor origins.
pub fn contains_pointer(rect: BarPlacement, x: i32, y: i32) -> bool {
    x >= rect.x
        && y >= rect.y
        && i64::from(x) < i64::from(rect.x) + i64::from(rect.width)
        && i64::from(y) < i64::from(rect.y) + i64::from(rect.height)
}

/// A panel includes the transparent passage from the physical edge to its
/// floating capsule. The sensor spans the same long axis, keeping corners free.
pub fn panel_geometry(
    output: MonitorGeometry,
    edge: BarEdge,
    visible: bool,
    trigger: u32,
) -> BarPlacement {
    let width = output.width.clamp(1, 16_384);
    let height = output.height.clamp(1, 16_384);
    let vertical = edge.is_vertical();
    let long = if vertical {
        height
            .saturating_sub(144)
            .clamp(1, if edge == BarEdge::Right { 720 } else { 680 })
    } else {
        (width * 4 / 5).clamp(1, 1_344)
    };
    let cross = if visible {
        if vertical { 90 } else { 76 }
    } else {
        trigger.clamp(1, 8)
    };
    let (w, h) = if vertical {
        (cross.min(width), long.min(height))
    } else {
        (long.min(width), cross.min(height))
    };
    let (x, y) = match edge {
        BarEdge::Left => (
            0,
            ((height - height.saturating_sub(144).clamp(1, 680)) / 2).saturating_sub(10),
        ),
        BarEdge::Right => (
            width - w,
            ((height - height.saturating_sub(144).clamp(1, 680)) / 2).saturating_sub(10),
        ),
        BarEdge::Bottom => ((width - w) / 2, height - h),
        BarEdge::Top => ((width - w) / 2, 0),
    };
    BarPlacement {
        x: output.x.saturating_add(x as i32),
        y: output.y.saturating_add(y as i32),
        width: w,
        height: h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sensors_reach_beyond_the_painted_capsule_on_large_outputs() {
        let output = MonitorGeometry {
            x: -3840,
            y: 100,
            width: 3840,
            height: 2160,
        };
        for (edge, x, y) in [
            (BarEdge::Left, -3840, 110),
            (BarEdge::Right, -1, 2240),
            (BarEdge::Bottom, -3830, 2259),
        ] {
            let sensor = trigger_geometry(output, edge, 2);
            assert!(contains_pointer(sensor, x, y));
            assert!(!contains_pointer(
                panel_geometry(output, edge, true, 2),
                x,
                y
            ));
        }
    }

    #[test]
    fn live_pin_changes_cancel_old_hover_deadlines() {
        let config = EdgeShellConfig::default();
        let now = Instant::now();
        let mut edge = EdgeReveal::new(RevealMode::Hover);
        edge.enter(now, &config);
        edge.set_mode(RevealMode::Always, false);
        edge.leave(now, &config);
        assert!(edge.visible());
        assert!(edge.deadline().is_none());
        edge.set_mode(RevealMode::Hover, true);
        assert!(edge.visible());
        edge.set_mode(RevealMode::Disabled, true);
        assert!(!edge.visible());
        assert!(!edge.tick(now + Duration::from_secs(10)));
    }
    #[test]
    fn independent_edges_cancel_accidental_entries_and_delay_hiding() {
        let cfg = EdgeShellConfig::default();
        let now = Instant::now();
        let mut left = EdgeReveal::new(cfg.left);
        let right = EdgeReveal::new(cfg.right);
        left.enter(now, &cfg);
        left.leave(now + Duration::from_millis(20), &cfg);
        assert!(!left.tick(now + Duration::from_secs(1)));
        left.enter(now, &cfg);
        assert!(left.tick(now + Duration::from_millis(80)));
        assert!(left.visible());
        assert!(!right.visible());
        left.leave(now, &cfg);
        left.enter(now + Duration::from_millis(300), &cfg);
        assert!(!left.tick(now + Duration::from_secs(1)));
        assert!(left.visible());
        left.leave(now, &cfg);
        assert!(left.tick(now + Duration::from_millis(400)));
        assert!(!left.visible());
    }
    #[test]
    fn pinned_and_disabled_edges_ignore_pointer_timing() {
        for mode in [RevealMode::Always, RevealMode::Disabled] {
            let mut edge = EdgeReveal::new(mode);
            let cfg = EdgeShellConfig::default();
            edge.enter(Instant::now(), &cfg);
            edge.leave(Instant::now(), &cfg);
            assert!(edge.deadline().is_none());
            assert_eq!(edge.visible(), mode == RevealMode::Always);
        }
    }
    #[test]
    fn rectangles_stay_on_tiny_negative_and_scaled_outputs() {
        for (width, height) in [(1, 1), (32, 20), (1920, 1080), (3840, 2160)] {
            let output = MonitorGeometry {
                x: -1920,
                y: -1080,
                width,
                height,
            };
            for edge in [BarEdge::Left, BarEdge::Right, BarEdge::Bottom] {
                for visible in [false, true] {
                    let rect = panel_geometry(output, edge, visible, 2);
                    assert!(rect.width > 0 && rect.height > 0);
                    assert!(rect.x >= output.x && rect.y >= output.y);
                    assert!(
                        i64::from(rect.x) + i64::from(rect.width)
                            <= i64::from(output.x) + i64::from(width)
                    );
                    assert!(
                        i64::from(rect.y) + i64::from(rect.height)
                            <= i64::from(output.y) + i64::from(height)
                    );
                }
            }
        }
    }
}
