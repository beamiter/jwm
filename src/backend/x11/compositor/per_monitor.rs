/// Per-monitor rendering optimization for multi-display setups
use std::collections::HashMap;

/// Monitor geometry and rendering state
#[derive(Clone, Debug)]
pub struct MonitorRenderRegion {
    pub monitor_id: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// Whether this monitor needs a redraw
    pub needs_redraw: bool,
    /// Last frame this monitor was rendered
    pub last_rendered_frame: u64,
}

/// Manages per-monitor rendering to optimize multi-display scenarios
pub struct PerMonitorRenderer {
    monitors: HashMap<u32, MonitorRenderRegion>,
    frame_count: u64,
    /// Scissor rect for GL rendering (x, y, w, h)
    current_scissor: Option<(i32, i32, u32, u32)>,
}

impl PerMonitorRenderer {
    pub fn new() -> Self {
        Self {
            monitors: HashMap::new(),
            frame_count: 0,
            current_scissor: None,
        }
    }

    /// Register a monitor
    pub fn add_monitor(&mut self, id: u32, x: i32, y: i32, width: u32, height: u32) {
        self.monitors.insert(id, MonitorRenderRegion {
            monitor_id: id,
            x,
            y,
            width,
            height,
            needs_redraw: true,
            last_rendered_frame: 0,
        });
        log::info!("per-monitor: added monitor {} at ({}, {}) {}x{}", id, x, y, width, height);
    }

    /// Remove a monitor
    pub fn remove_monitor(&mut self, id: u32) {
        if self.monitors.remove(&id).is_some() {
            log::info!("per-monitor: removed monitor {}", id);
        }
    }

    /// Mark a monitor for redraw
    pub fn mark_monitor_dirty(&mut self, id: u32) {
        if let Some(mon) = self.monitors.get_mut(&id) {
            mon.needs_redraw = true;
        }
    }

    /// Mark all monitors for redraw
    pub fn mark_all_dirty(&mut self) {
        for mon in self.monitors.values_mut() {
            mon.needs_redraw = true;
        }
    }

    /// Get monitors that need rendering this frame
    pub fn monitors_to_render(&self) -> Vec<&MonitorRenderRegion> {
        self.monitors.values()
            .filter(|m| m.needs_redraw)
            .collect()
    }

    /// Get monitor by ID
    pub fn get_monitor(&self, id: u32) -> Option<&MonitorRenderRegion> {
        self.monitors.get(&id)
    }

    /// Get mutable monitor by ID
    pub fn get_monitor_mut(&mut self, id: u32) -> Option<&mut MonitorRenderRegion> {
        self.monitors.get_mut(&id)
    }

    /// Start rendering a specific monitor
    pub fn start_monitor_render(&mut self, id: u32) -> Option<(i32, i32, u32, u32)> {
        if let Some(mon) = self.monitors.get_mut(&id) {
            let scissor = (mon.x, mon.y, mon.width, mon.height);
            self.current_scissor = Some(scissor);
            mon.needs_redraw = false;
            mon.last_rendered_frame = self.frame_count;
            Some(scissor)
        } else {
            None
        }
    }

    /// End monitor rendering
    pub fn end_monitor_render(&mut self) {
        self.current_scissor = None;
    }

    /// Get current scissor rect for GL operations
    pub fn current_scissor(&self) -> Option<(i32, i32, u32, u32)> {
        self.current_scissor
    }

    /// Advance frame counter
    pub fn next_frame(&mut self) {
        self.frame_count += 1;
    }

    /// Get frame count
    pub fn frame_count(&self) -> u64 {
        self.frame_count
    }

    /// Get number of monitors
    pub fn monitor_count(&self) -> usize {
        self.monitors.len()
    }

    /// Get total screen area bounding box
    pub fn total_area(&self) -> Option<(i32, i32, i32, i32)> {
        if self.monitors.is_empty() {
            return None;
        }

        let mut min_x = i32::MAX;
        let mut min_y = i32::MAX;
        let mut max_x = i32::MIN;
        let mut max_y = i32::MIN;

        for mon in self.monitors.values() {
            min_x = min_x.min(mon.x);
            min_y = min_y.min(mon.y);
            max_x = max_x.max(mon.x + mon.width as i32);
            max_y = max_y.max(mon.y + mon.height as i32);
        }

        Some((min_x, min_y, max_x, max_y))
    }

    /// Calculate dirty fraction across all monitors
    pub fn dirty_fraction(&self) -> f32 {
        if self.monitors.is_empty() {
            return 0.0;
        }
        let dirty = self.monitors.values().filter(|m| m.needs_redraw).count();
        dirty as f32 / self.monitors.len() as f32
    }

    /// Reset all monitors (clear dirty flags but keep state)
    pub fn reset_dirty(&mut self) {
        for mon in self.monitors.values_mut() {
            mon.needs_redraw = false;
        }
    }
}

impl Default for PerMonitorRenderer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_per_monitor_renderer() {
        let mut renderer = PerMonitorRenderer::new();

        renderer.add_monitor(0, 0, 0, 1920, 1080);
        renderer.add_monitor(1, 1920, 0, 1920, 1080);

        assert_eq!(renderer.monitor_count(), 2);

        let dirty = renderer.monitors_to_render();
        assert_eq!(dirty.len(), 2);

        renderer.mark_monitor_dirty(0);
        let area = renderer.total_area();
        assert_eq!(area, Some((0, 0, 3840, 1080)));
    }
}
