use super::dirty_region::DirtyRect;

#[derive(Debug, Clone)]
struct MonitorRegion {
    id: u32,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    needs_redraw: bool,
    last_rendered_frame: u64,
}

pub(crate) struct PerMonitorRenderer {
    monitors: Vec<MonitorRegion>,
    current_frame: u64,
}

impl PerMonitorRenderer {
    pub(crate) fn new() -> Self {
        Self {
            monitors: Vec::new(),
            current_frame: 0,
        }
    }

    pub(crate) fn set_monitors(&mut self, monitors: &[(u32, i32, i32, u32, u32, u32)]) {
        self.monitors.clear();
        for &(id, x, y, w, h, _active_tags) in monitors {
            self.monitors.push(MonitorRegion {
                id,
                x,
                y,
                width: w,
                height: h,
                needs_redraw: true,
                last_rendered_frame: 0,
            });
        }
    }

    pub(crate) fn mark_all_dirty(&mut self) {
        for m in &mut self.monitors {
            m.needs_redraw = true;
        }
    }

    pub(crate) fn mark_dirty_from_regions(&mut self, regions: &[DirtyRect]) {
        for m in &mut self.monitors {
            if m.needs_redraw {
                continue;
            }
            let mx = m.x as f32;
            let my = m.y as f32;
            let mw = m.width as f32;
            let mh = m.height as f32;
            for r in regions {
                if r.x < mx + mw && r.x + r.width > mx && r.y < my + mh && r.y + r.height > my {
                    m.needs_redraw = true;
                    break;
                }
            }
        }
    }

    pub(crate) fn next_frame(&mut self) {
        self.current_frame += 1;
    }

    pub(crate) fn monitors_needing_render(&self) -> Vec<(u32, i32, i32, u32, u32)> {
        self.monitors
            .iter()
            .filter(|m| m.needs_redraw)
            .map(|m| (m.id, m.x, m.y, m.width, m.height))
            .collect()
    }

    pub(crate) fn mark_rendered(&mut self, monitor_id: u32) {
        if let Some(m) = self.monitors.iter_mut().find(|m| m.id == monitor_id) {
            m.needs_redraw = false;
            m.last_rendered_frame = self.current_frame;
        }
    }

    pub(crate) fn dirty_fraction(&self) -> f32 {
        if self.monitors.is_empty() {
            return 1.0;
        }
        let dirty = self.monitors.iter().filter(|m| m.needs_redraw).count();
        dirty as f32 / self.monitors.len() as f32
    }

    pub(crate) fn monitor_count(&self) -> usize {
        self.monitors.len()
    }
}
