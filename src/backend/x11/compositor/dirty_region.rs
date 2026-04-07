/// Enhanced dirty region tracking for optimized partial redraws
use std::collections::VecDeque;

/// A rectangular dirty region
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirtyRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl DirtyRect {
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self { x, y, width, height }
    }

    /// Check if this rect intersects with another
    pub fn intersects(&self, other: &DirtyRect) -> bool {
        let r1_right = self.x + self.width as i32;
        let r1_bottom = self.y + self.height as i32;
        let r2_right = other.x + other.width as i32;
        let r2_bottom = other.y + other.height as i32;

        self.x < r2_right && r1_right > other.x
            && self.y < r2_bottom && r1_bottom > other.y
    }

    /// Union two rects
    pub fn union(&self, other: &DirtyRect) -> DirtyRect {
        let min_x = self.x.min(other.x);
        let min_y = self.y.min(other.y);
        let max_x = (self.x + self.width as i32).max(other.x + other.width as i32);
        let max_y = (self.y + self.height as i32).max(other.y + other.height as i32);

        DirtyRect {
            x: min_x,
            y: min_y,
            width: (max_x - min_x) as u32,
            height: (max_y - min_y) as u32,
        }
    }

    /// Get area in pixels
    pub fn area(&self) -> u64 {
        self.width as u64 * self.height as u64
    }

    /// Expand this rect by a margin
    pub fn expand(&self, margin: i32) -> DirtyRect {
        DirtyRect {
            x: (self.x - margin).max(0),
            y: (self.y - margin).max(0),
            width: (self.width as i32 + margin * 2).max(0) as u32,
            height: (self.height as i32 + margin * 2).max(0) as u32,
        }
    }
}

/// Tracks dirty regions for partial screen redraws
pub struct DirtyRegionTracker {
    regions: VecDeque<DirtyRect>,
    merged_region: Option<DirtyRect>,
    screen_w: u32,
    screen_h: u32,
    max_regions: usize,
}

impl DirtyRegionTracker {
    pub fn new(screen_w: u32, screen_h: u32) -> Self {
        Self {
            regions: VecDeque::with_capacity(16),
            merged_region: None,
            screen_w,
            screen_h,
            max_regions: 16,
        }
    }

    /// Mark a region as dirty
    pub fn mark_dirty(&mut self, rect: DirtyRect) {
        // Clamp to screen bounds
        let clamped = DirtyRect {
            x: rect.x.max(0) as i32,
            y: rect.y.max(0) as i32,
            width: rect.width.min(self.screen_w),
            height: rect.height.min(self.screen_h),
        };

        self.regions.push_back(clamped);

        // Merge regions if we have too many
        if self.regions.len() > self.max_regions {
            self.merge_regions();
        }

        // Invalidate cached merged region
        self.merged_region = None;
    }

    /// Mark the entire screen as dirty
    pub fn mark_all_dirty(&mut self) {
        self.regions.clear();
        self.merged_region = Some(DirtyRect {
            x: 0,
            y: 0,
            width: self.screen_w,
            height: self.screen_h,
        });
    }

    /// Get the merged bounding rect of all dirty regions
    pub fn merged(&mut self) -> Option<DirtyRect> {
        if let Some(cached) = self.merged_region {
            return Some(cached);
        }

        if self.regions.is_empty() {
            return None;
        }

        let mut result = self.regions[0];
        for region in self.regions.iter().skip(1) {
            result = result.union(region);
        }

        self.merged_region = Some(result);
        Some(result)
    }

    /// Get all dirty regions
    pub fn regions(&self) -> Vec<DirtyRect> {
        self.regions.iter().copied().collect()
    }

    /// Clear all dirty regions
    pub fn clear(&mut self) {
        self.regions.clear();
        self.merged_region = None;
    }

    /// Calculate the fraction of screen that is dirty
    pub fn dirty_fraction(&mut self) -> f32 {
        if let Some(merged) = self.merged() {
            merged.area() as f32 / (self.screen_w as u64 * self.screen_h as u64) as f32
        } else {
            0.0
        }
    }

    /// Check if a specific region is affected by any dirty rect
    pub fn is_region_dirty(&self, rect: &DirtyRect) -> bool {
        self.regions.iter().any(|r| r.intersects(rect))
    }

    /// Merge overlapping regions to reduce complexity
    fn merge_regions(&mut self) {
        if self.regions.len() < 2 {
            return;
        }

        let mut merged = vec![];
        let mut current = self.regions.pop_front().unwrap();

        while let Some(next) = self.regions.pop_front() {
            if current.intersects(&next) {
                current = current.union(&next);
            } else {
                merged.push(current);
                current = next;
            }
        }
        merged.push(current);

        self.regions = merged.into();
    }

    /// Resize the screen and mark all as dirty
    pub fn resize(&mut self, new_w: u32, new_h: u32) {
        self.screen_w = new_w;
        self.screen_h = new_h;
        self.mark_all_dirty();
    }

    /// Get number of tracked regions
    pub fn region_count(&self) -> usize {
        self.regions.len()
    }

    /// Check if we should just redraw the entire screen
    /// Returns true if dirty area exceeds threshold
    pub fn should_redraw_full_screen(&mut self, threshold: f32) -> bool {
        self.dirty_fraction() > threshold
    }
}

impl Default for DirtyRegionTracker {
    fn default() -> Self {
        Self::new(1920, 1080)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dirty_rect_intersection() {
        let r1 = DirtyRect::new(0, 0, 100, 100);
        let r2 = DirtyRect::new(50, 50, 100, 100);
        assert!(r1.intersects(&r2));

        let r3 = DirtyRect::new(200, 200, 100, 100);
        assert!(!r1.intersects(&r3));
    }

    #[test]
    fn test_dirty_rect_union() {
        let r1 = DirtyRect::new(0, 0, 100, 100);
        let r2 = DirtyRect::new(50, 50, 100, 100);
        let union = r1.union(&r2);

        assert_eq!(union.x, 0);
        assert_eq!(union.y, 0);
        assert_eq!(union.width, 150);
        assert_eq!(union.height, 150);
    }

    #[test]
    fn test_dirty_region_tracking() {
        let mut tracker = DirtyRegionTracker::new(1920, 1080);

        tracker.mark_dirty(DirtyRect::new(0, 0, 100, 100));
        tracker.mark_dirty(DirtyRect::new(1900, 1000, 100, 100));

        assert_eq!(tracker.region_count(), 2);

        let merged = tracker.merged();
        assert!(merged.is_some());
    }

    #[test]
    fn test_dirty_fraction() {
        let mut tracker = DirtyRegionTracker::new(1920, 1080);

        // Mark a small region
        tracker.mark_dirty(DirtyRect::new(0, 0, 100, 100));
        let fraction = tracker.dirty_fraction();
        assert!(fraction < 0.01); // Less than 1%

        // Mark the whole screen
        tracker.mark_all_dirty();
        let fraction = tracker.dirty_fraction();
        assert!((fraction - 1.0).abs() < 0.01); // Nearly 100%
    }
}
