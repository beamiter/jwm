use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DirtyRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl DirtyRect {
    pub fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn intersects(&self, other: &DirtyRect) -> bool {
        self.x < other.x + other.width
            && self.x + self.width > other.x
            && self.y < other.y + other.height
            && self.y + self.height > other.y
    }

    pub fn union(&self, other: &DirtyRect) -> DirtyRect {
        let min_x = self.x.min(other.x);
        let min_y = self.y.min(other.y);
        let max_x = (self.x + self.width).max(other.x + other.width);
        let max_y = (self.y + self.height).max(other.y + other.height);
        DirtyRect {
            x: min_x,
            y: min_y,
            width: max_x - min_x,
            height: max_y - min_y,
        }
    }

    pub fn area(&self) -> f32 {
        self.width * self.height
    }

    pub fn expand(&self, margin: f32) -> DirtyRect {
        DirtyRect {
            x: self.x - margin,
            y: self.y - margin,
            width: self.width + margin * 2.0,
            height: self.height + margin * 2.0,
        }
    }

    pub fn contains_point(&self, px: f32, py: f32) -> bool {
        px >= self.x && px <= self.x + self.width && py >= self.y && py <= self.y + self.height
    }
}

pub struct DirtyRegionTracker {
    regions: VecDeque<DirtyRect>,
    screen_w: u32,
    screen_h: u32,
    merge_distance: f32,
    cached_merged: Option<DirtyRect>,
}

impl DirtyRegionTracker {
    pub fn new(w: u32, h: u32) -> Self {
        Self {
            regions: VecDeque::new(),
            screen_w: w,
            screen_h: h,
            merge_distance: 50.0,
            cached_merged: None,
        }
    }

    pub fn mark_dirty(&mut self, rect: DirtyRect) {
        self.cached_merged = None;

        // Try to merge with an existing region if within merge_distance.
        // Expanding the candidate by merge_distance and checking intersection is equivalent
        // to testing whether the gap between the two rects is less than merge_distance.
        let expanded = rect.expand(self.merge_distance);
        for existing in self.regions.iter_mut() {
            if expanded.intersects(existing) {
                *existing = existing.union(&rect);
                return;
            }
        }

        self.regions.push_back(rect);
    }

    pub fn mark_all_dirty(&mut self) {
        self.regions.clear();
        self.cached_merged = None;
        self.regions.push_back(DirtyRect::new(
            0.0,
            0.0,
            self.screen_w as f32,
            self.screen_h as f32,
        ));
    }

    pub fn clear(&mut self) {
        self.regions.clear();
        self.cached_merged = None;
    }

    pub fn merged(&mut self) -> Option<DirtyRect> {
        if let Some(cached) = self.cached_merged {
            return Some(cached);
        }

        if self.regions.is_empty() {
            return None;
        }

        let mut result = self.regions[0];
        for rect in self.regions.iter().skip(1) {
            result = result.union(rect);
        }

        self.cached_merged = Some(result);
        Some(result)
    }

    pub fn regions(&self) -> &VecDeque<DirtyRect> {
        &self.regions
    }

    pub fn dirty_fraction(&mut self) -> f32 {
        let screen_area = self.screen_w as f32 * self.screen_h as f32;
        if screen_area == 0.0 {
            return 0.0;
        }

        match self.merged() {
            Some(m) => (m.area() / screen_area).min(1.0),
            None => 0.0,
        }
    }

    /// Current covered fraction without updating the cached merged rectangle.
    /// This is used by read-only metrics collection so IPC cannot perturb the
    /// render-path cache.
    pub fn current_dirty_fraction(&self) -> f32 {
        let screen_area = self.screen_w as f32 * self.screen_h as f32;
        if screen_area == 0.0 || self.regions.is_empty() {
            return 0.0;
        }
        let mut merged = self.regions[0];
        for rect in self.regions.iter().skip(1) {
            merged = merged.union(rect);
        }
        (merged.area() / screen_area).min(1.0)
    }

    pub fn is_region_dirty(&self, rect: &DirtyRect) -> bool {
        for existing in &self.regions {
            if existing.intersects(rect) {
                return true;
            }
        }
        false
    }

    pub fn should_redraw_full_screen(&mut self, threshold: f32) -> bool {
        self.dirty_fraction() >= threshold
    }

    pub fn resize(&mut self, w: u32, h: u32) {
        self.screen_w = w;
        self.screen_h = h;
        self.mark_all_dirty();
    }

    pub fn region_count(&self) -> usize {
        self.regions.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_dirty_fraction_is_read_only() {
        let mut tracker = DirtyRegionTracker::new(100, 100);
        tracker.mark_dirty(DirtyRect::new(0.0, 0.0, 20.0, 20.0));

        assert_eq!(tracker.current_dirty_fraction(), 0.04);
        assert_eq!(tracker.region_count(), 1);
    }
}
