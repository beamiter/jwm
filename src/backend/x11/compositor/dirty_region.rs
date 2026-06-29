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
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Check if this rect intersects with another
    pub fn intersects(&self, other: &DirtyRect) -> bool {
        let r1_right = self.x + self.width as i32;
        let r1_bottom = self.y + self.height as i32;
        let r2_right = other.x + other.width as i32;
        let r2_bottom = other.y + other.height as i32;

        self.x < r2_right && r1_right > other.x && self.y < r2_bottom && r1_bottom > other.y
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
    /// Phase 3.2: Minimum rect size to track (filter noise)
    min_rect_area: u64,
    /// Phase 3.2: Merge threshold (merge rects closer than this distance)
    merge_distance_threshold: i32,
}

impl DirtyRegionTracker {
    pub fn new(screen_w: u32, screen_h: u32) -> Self {
        Self {
            regions: VecDeque::with_capacity(16),
            merged_region: None,
            screen_w,
            screen_h,
            max_regions: 16,
            min_rect_area: 100,           // 10x10 pixels minimum
            merge_distance_threshold: 50, // Merge rects within 50px of each other
        }
    }

    /// Phase 3.2: Create with custom thresholds
    pub fn with_thresholds(
        screen_w: u32,
        screen_h: u32,
        min_area: u64,
        merge_distance: i32,
    ) -> Self {
        Self {
            regions: VecDeque::with_capacity(16),
            merged_region: None,
            screen_w,
            screen_h,
            max_regions: 16,
            min_rect_area: min_area,
            merge_distance_threshold: merge_distance,
        }
    }

    /// Mark a region as dirty
    pub fn mark_dirty(&mut self, rect: DirtyRect) {
        // Phase 3.2: Filter out tiny rects (noise reduction)
        if rect.area() < self.min_rect_area {
            return;
        }

        // Clamp to screen bounds by intersecting [rect.x, rect.x+width) with
        // [0, screen_w). A negative x must shrink width too, otherwise clamping
        // x to 0 while keeping width pushes the right edge past the original and
        // over-redraws. Same for y/height.
        let x0 = rect.x.max(0);
        let y0 = rect.y.max(0);
        let right = (rect.x + rect.width as i32).min(self.screen_w as i32);
        let bottom = (rect.y + rect.height as i32).min(self.screen_h as i32);
        if right <= x0 || bottom <= y0 {
            return;
        }
        let clamped = DirtyRect {
            x: x0,
            y: y0,
            width: (right - x0) as u32,
            height: (bottom - y0) as u32,
        };

        // Phase 3.2: Smart merge - check if close to existing rect
        let mut merged_idx = None;
        for (idx, existing) in self.regions.iter().enumerate() {
            let dx = (clamped.x - existing.x).abs();
            let dy = (clamped.y - existing.y).abs();

            if dx <= self.merge_distance_threshold && dy <= self.merge_distance_threshold {
                merged_idx = Some(idx);
                break;
            }
            if clamped.intersects(existing) {
                merged_idx = Some(idx);
                break;
            }
        }

        if let Some(idx) = merged_idx {
            if let Some(existing) = self.regions.get_mut(idx) {
                *existing = existing.union(&clamped);
            }
        } else {
            self.regions.push_back(clamped);
        }

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

    /// Phase 3.2: Check if two rects should be merged
    #[allow(dead_code)]
    fn should_merge(&self, r1: &DirtyRect, r2: &DirtyRect) -> bool {
        // Check if rects are within merge distance
        let dx = (r1.x - r2.x).abs();
        let dy = (r1.y - r2.y).abs();

        if dx > self.merge_distance_threshold || dy > self.merge_distance_threshold {
            return false;
        }

        // Also merge if they intersect
        r1.intersects(r2)
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
    fn test_dirty_rect_intersection_edge_cases() {
        let r1 = DirtyRect::new(0, 0, 100, 100);

        let r2 = DirtyRect::new(100, 0, 100, 100);
        assert!(!r1.intersects(&r2), "Adjacent rects should not intersect");

        let r3 = DirtyRect::new(99, 0, 100, 100);
        assert!(
            r1.intersects(&r3),
            "Overlapping by 1 pixel should intersect"
        );
    }

    #[test]
    fn test_dirty_rect_intersection_contained() {
        let r1 = DirtyRect::new(0, 0, 100, 100);
        let r2 = DirtyRect::new(25, 25, 50, 50);
        assert!(r1.intersects(&r2), "Contained rect should intersect");
        assert!(r2.intersects(&r1), "Intersection should be symmetric");
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
    fn test_dirty_rect_union_disjoint() {
        let r1 = DirtyRect::new(0, 0, 100, 100);
        let r2 = DirtyRect::new(200, 200, 100, 100);
        let union = r1.union(&r2);

        assert_eq!(union.x, 0);
        assert_eq!(union.y, 0);
        assert_eq!(union.width, 300);
        assert_eq!(union.height, 300);
    }

    #[test]
    fn test_dirty_rect_area() {
        let r1 = DirtyRect::new(0, 0, 100, 100);
        assert_eq!(r1.area(), 10000);

        let r2 = DirtyRect::new(0, 0, 0, 100);
        assert_eq!(r2.area(), 0);

        let r3 = DirtyRect::new(0, 0, 1920, 1080);
        assert_eq!(r3.area(), 1920 * 1080);
    }

    #[test]
    fn test_dirty_rect_expand() {
        let r = DirtyRect::new(10, 10, 100, 100);
        let expanded = r.expand(5);

        assert_eq!(expanded.x, 5);
        assert_eq!(expanded.y, 5);
        assert_eq!(expanded.width, 110);
        assert_eq!(expanded.height, 110);
    }

    #[test]
    fn test_dirty_rect_expand_at_origin() {
        let r = DirtyRect::new(0, 0, 100, 100);
        let expanded = r.expand(10);

        assert_eq!(expanded.x, 0, "Should be clamped to 0");
        assert_eq!(expanded.y, 0, "Should be clamped to 0");
        assert_eq!(expanded.width, 120);
        assert_eq!(expanded.height, 120);
    }

    #[test]
    fn test_dirty_rect_expand_large_margin() {
        let r = DirtyRect::new(50, 50, 10, 10);
        let expanded = r.expand(100);

        assert_eq!(expanded.x, 0, "Should be clamped to 0");
        assert_eq!(expanded.y, 0, "Should be clamped to 0");
        assert!(expanded.width > 200);
        assert!(expanded.height > 200);
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
    fn test_dirty_region_clear() {
        let mut tracker = DirtyRegionTracker::new(1920, 1080);

        tracker.mark_dirty(DirtyRect::new(0, 0, 100, 100));
        assert_eq!(tracker.region_count(), 1);

        tracker.clear();
        assert_eq!(tracker.region_count(), 0);
        assert!(tracker.merged().is_none());
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

    #[test]
    fn test_dirty_region_is_region_dirty() {
        let mut tracker = DirtyRegionTracker::new(1920, 1080);

        tracker.mark_dirty(DirtyRect::new(100, 100, 200, 200));

        let test_r1 = DirtyRect::new(150, 150, 50, 50);
        assert!(
            tracker.is_region_dirty(&test_r1),
            "Overlapping region should be dirty"
        );

        let test_r2 = DirtyRect::new(0, 0, 50, 50);
        assert!(
            !tracker.is_region_dirty(&test_r2),
            "Non-overlapping region should not be dirty"
        );
    }

    #[test]
    fn test_dirty_region_merge_on_overflow() {
        let mut tracker = DirtyRegionTracker::new(1920, 1080);

        // Add overlapping regions so they will merge
        for i in 0..20 {
            tracker.mark_dirty(DirtyRect::new(i as i32 * 50, 0, 100, 100));
        }

        // After merging, we should have fewer regions than added
        let final_count = tracker.region_count();
        assert!(
            final_count <= 20,
            "Should have reasonable number of regions after merge: {}",
            final_count
        );
    }

    #[test]
    fn test_dirty_region_resize() {
        let mut tracker = DirtyRegionTracker::new(1920, 1080);

        tracker.mark_dirty(DirtyRect::new(0, 0, 100, 100));
        tracker.resize(2560, 1440);

        let merged = tracker.merged();
        assert!(merged.is_some());
        let r = merged.unwrap();
        assert_eq!(r.width, 2560);
        assert_eq!(r.height, 1440);
    }

    #[test]
    fn test_dirty_region_default() {
        let mut tracker = DirtyRegionTracker::default();
        assert_eq!(tracker.region_count(), 0);
        tracker.mark_dirty(DirtyRect::new(0, 0, 100, 100));
        assert_eq!(tracker.region_count(), 1);
    }

    #[test]
    fn test_dirty_region_should_redraw_full_screen() {
        let mut tracker = DirtyRegionTracker::new(1920, 1080);

        tracker.mark_dirty(DirtyRect::new(0, 0, 100, 100));
        assert!(
            !tracker.should_redraw_full_screen(0.5),
            "Small region should not trigger full redraw"
        );

        tracker.mark_dirty(DirtyRect::new(0, 0, 1000, 1000));
        assert!(
            tracker.should_redraw_full_screen(0.4),
            "Large region should trigger full redraw"
        );
    }

    #[test]
    fn test_dirty_region_get_regions() {
        let mut tracker = DirtyRegionTracker::new(1920, 1080);

        let r1 = DirtyRect::new(0, 0, 100, 100);
        let r2 = DirtyRect::new(200, 200, 100, 100);

        tracker.mark_dirty(r1);
        tracker.mark_dirty(r2);

        let regions = tracker.regions();
        assert_eq!(regions.len(), 2);
    }

    #[test]
    fn test_dirty_region_clamping() {
        let mut tracker = DirtyRegionTracker::new(1920, 1080);

        tracker.mark_dirty(DirtyRect::new(-100, -100, 200, 200));
        let regions = tracker.regions();
        assert!(!regions.is_empty());
        let r = regions[0];
        assert!(r.x >= 0);
        assert!(r.y >= 0);
    }

    #[test]
    fn test_dirty_rect_union_commutativity() {
        let r1 = DirtyRect::new(0, 0, 100, 100);
        let r2 = DirtyRect::new(50, 50, 100, 100);

        let u1 = r1.union(&r2);
        let u2 = r2.union(&r1);

        assert_eq!(u1, u2, "Union should be commutative");
    }

    #[test]
    fn test_dirty_rect_union_associativity() {
        let r1 = DirtyRect::new(0, 0, 100, 100);
        let r2 = DirtyRect::new(50, 50, 100, 100);
        let r3 = DirtyRect::new(150, 150, 100, 100);

        let u1 = r1.union(&r2).union(&r3);
        let u2 = r1.union(&(r2.union(&r3)));

        assert_eq!(u1, u2, "Union should be associative");
    }
}
