//! Tile-based damage tracking for partial redraw optimization.

#[derive(Debug, Clone)]
pub(crate) struct DamageTracker {
    dirty_tiles: Vec<bool>,
    tile_w: u32,
    tile_h: u32,
    tile_cols: u32,
    tile_rows: u32,
    screen_w: u32,
    screen_h: u32,
    window_count: usize,
    animating: bool,
}

impl DamageTracker {
    pub(crate) fn new(screen_w: u32, screen_h: u32) -> Self {
        let (tile_cols, tile_rows) = Self::compute_grid_size(screen_w, screen_h);
        let tile_w = (screen_w + tile_cols - 1) / tile_cols;
        let tile_h = (screen_h + tile_rows - 1) / tile_rows;
        Self {
            dirty_tiles: vec![true; (tile_cols * tile_rows) as usize],
            tile_w,
            tile_h,
            tile_cols,
            tile_rows,
            screen_w,
            screen_h,
            window_count: 0,
            animating: false,
        }
    }

    /// Compute dynamic grid size based on resolution.
    /// 4K: 16x12, 1080p: 8x6, 720p: 5x4.
    pub(crate) fn compute_grid_size(screen_w: u32, screen_h: u32) -> (u32, u32) {
        let cols = (screen_w / 240).clamp(4, 16);
        let rows = (screen_h / 180).clamp(3, 12);
        (cols, rows)
    }

    pub(crate) fn update_state(&mut self, window_count: usize, animating: bool) {
        self.window_count = window_count;
        self.animating = animating;
    }

    pub(crate) fn mark_all_dirty(&mut self) {
        self.dirty_tiles.fill(true);
    }

    pub(crate) fn clear(&mut self) {
        self.dirty_tiles.fill(false);
    }

    pub(crate) fn mark_region_dirty(&mut self, x: i32, y: i32, w: u32, h: u32) {
        let x1 = x.max(0) as u32;
        let y1 = y.max(0) as u32;
        let x2 = (x + w as i32).min(self.screen_w as i32) as u32;
        let y2 = (y + h as i32).min(self.screen_h as i32) as u32;

        let tile_x1 = x1 / self.tile_w;
        let tile_y1 = y1 / self.tile_h;
        let tile_x2 = (x2 + self.tile_w - 1) / self.tile_w;
        let tile_y2 = (y2 + self.tile_h - 1) / self.tile_h;

        for ty in tile_y1..tile_y2.min(self.tile_rows) {
            for tx in tile_x1..tile_x2.min(self.tile_cols) {
                self.dirty_tiles[(ty * self.tile_cols + tx) as usize] = true;
            }
        }
    }

    pub(crate) fn dirty_tile_count(&self) -> usize {
        self.dirty_tiles.iter().filter(|&&d| d).count()
    }

    pub(crate) fn tile_count(&self) -> usize {
        self.dirty_tiles.len()
    }

    pub(crate) fn dirty_fraction(&self) -> f32 {
        let total = self.tile_count();
        if total == 0 {
            return 0.0;
        }
        self.dirty_tile_count() as f32 / total as f32
    }

    #[allow(dead_code)]
    pub(crate) fn dirty_bounds(&self) -> Option<(i32, i32, u32, u32)> {
        let threshold = self.dynamic_threshold();
        if self.dirty_fraction() > threshold {
            return Some((0, 0, self.screen_w, self.screen_h));
        }
        let mut min_x = self.screen_w as i32;
        let mut min_y = self.screen_h as i32;
        let mut max_x = 0i32;
        let mut max_y = 0i32;
        let mut any_dirty = false;
        for ty in 0..self.tile_rows {
            for tx in 0..self.tile_cols {
                if self.dirty_tiles[(ty * self.tile_cols + tx) as usize] {
                    any_dirty = true;
                    let px = (tx * self.tile_w) as i32;
                    let py = (ty * self.tile_h) as i32;
                    min_x = min_x.min(px);
                    min_y = min_y.min(py);
                    max_x = max_x.max(px + self.tile_w as i32);
                    max_y = max_y.max(py + self.tile_h as i32);
                }
            }
        }
        if any_dirty {
            Some((min_x, min_y, (max_x - min_x) as u32, (max_y - min_y) as u32))
        } else {
            None
        }
    }

    pub(crate) fn resize(&mut self, screen_w: u32, screen_h: u32) {
        self.screen_w = screen_w;
        self.screen_h = screen_h;
        let (tile_cols, tile_rows) = Self::compute_grid_size(screen_w, screen_h);
        self.tile_cols = tile_cols;
        self.tile_rows = tile_rows;
        self.tile_w = (screen_w + tile_cols - 1) / tile_cols;
        self.tile_h = (screen_h + tile_rows - 1) / tile_rows;
        self.dirty_tiles = vec![true; (tile_cols * tile_rows) as usize];
    }

    fn dynamic_threshold(&self) -> f32 {
        if self.animating {
            return 0.3;
        }
        match self.window_count {
            0..=3 => 0.7,
            4..=8 => 0.5,
            _ => 0.35,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::DamageTracker;

    #[test]
    fn starts_fully_dirty() {
        let tracker = DamageTracker::new(1920, 1080);
        assert_eq!(tracker.dirty_fraction(), 1.0);
        assert_eq!(tracker.dirty_tile_count(), tracker.tile_count());
    }

    #[test]
    fn clear_resets_dirty_tiles() {
        let mut tracker = DamageTracker::new(1920, 1080);
        tracker.clear();
        assert_eq!(tracker.dirty_tile_count(), 0);
        assert_eq!(tracker.dirty_fraction(), 0.0);
    }

    #[test]
    fn region_marks_some_tiles() {
        let mut tracker = DamageTracker::new(1920, 1080);
        tracker.clear();
        tracker.mark_region_dirty(10, 10, 50, 50);
        assert!(tracker.dirty_tile_count() > 0);
        assert!(tracker.dirty_fraction() < 1.0);
    }
}
