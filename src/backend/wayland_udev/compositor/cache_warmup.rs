use std::collections::HashMap;
use std::time::Instant;

/// Tracks blur operation sizes by frequency.
#[derive(Debug, Clone)]
pub struct BlurSizeStats {
    hits: HashMap<(u32, u32), u64>,
}

impl BlurSizeStats {
    pub fn new() -> Self {
        Self {
            hits: HashMap::new(),
        }
    }

    /// Record a blur operation at the given dimensions.
    pub fn record(&mut self, w: u32, h: u32) {
        *self.hits.entry((w, h)).or_insert(0) += 1;
    }

    /// Return the top `n` sizes sorted by frequency (descending).
    pub fn top_sizes(&self, n: usize) -> Vec<((u32, u32), u64)> {
        let mut entries: Vec<((u32, u32), u64)> = self.hits.iter().map(|(&k, &v)| (k, v)).collect();
        entries.sort_by(|a, b| b.1.cmp(&a.1));
        entries.truncate(n);
        entries
    }

    /// Return sizes that account for more than 5% of total hits.
    pub fn warmup_candidates(&self) -> Vec<(u32, u32)> {
        let total: u64 = self.hits.values().sum();
        if total == 0 {
            return Vec::new();
        }
        let threshold = (total as f64 * 0.05) as u64;
        let mut candidates: Vec<((u32, u32), u64)> = self
            .hits
            .iter()
            .filter(|&(_, &count)| count > threshold)
            .map(|(&size, &count)| (size, count))
            .collect();
        candidates.sort_by(|a, b| b.1.cmp(&a.1));
        candidates.into_iter().map(|(size, _)| size).collect()
    }
}

impl Default for BlurSizeStats {
    fn default() -> Self {
        Self::new()
    }
}

/// Manages cache warmup tracking for blur shaders and related operations.
pub struct CacheWarmupManager {
    blur_stats: BlurSizeStats,
    cache_hits: u64,
    cache_misses: u64,
    warmup_completed: bool,
    startup_time: Instant,
}

impl CacheWarmupManager {
    pub fn new() -> Self {
        Self {
            blur_stats: BlurSizeStats::new(),
            cache_hits: 0,
            cache_misses: 0,
            warmup_completed: false,
            startup_time: Instant::now(),
        }
    }

    /// Record a blur operation at the given dimensions.
    pub fn record_blur_operation(&mut self, w: u32, h: u32) {
        self.blur_stats.record(w, h);
    }

    /// Record a cache hit.
    pub fn record_cache_hit(&mut self) {
        self.cache_hits += 1;
    }

    /// Record a cache miss.
    pub fn record_cache_miss(&mut self) {
        self.cache_misses += 1;
    }

    /// Return the cache hit rate as a value in [0.0, 1.0].
    /// Returns 0.0 if no cache operations have been recorded.
    pub fn cache_hit_rate(&self) -> f32 {
        let total = self.cache_hits + self.cache_misses;
        if total == 0 {
            0.0
        } else {
            self.cache_hits as f32 / total as f32
        }
    }

    /// Return the top `n` blur sizes by frequency.
    pub fn top_blur_sizes(&self, n: usize) -> Vec<((u32, u32), u64)> {
        self.blur_stats.top_sizes(n)
    }

    /// Return blur sizes that are good warmup candidates (>5% of total).
    pub fn warmup_candidates(&self) -> Vec<(u32, u32)> {
        self.blur_stats.warmup_candidates()
    }

    /// Mark warmup as completed.
    pub fn mark_warmup_completed(&mut self) {
        self.warmup_completed = true;
    }

    /// Check whether warmup has been completed.
    pub fn is_warmup_completed(&self) -> bool {
        self.warmup_completed
    }

    /// Return a formatted summary string of the current statistics.
    pub fn stats_string(&self) -> String {
        let elapsed = self.startup_time.elapsed();
        let hit_rate = self.cache_hit_rate() * 100.0;
        let candidates = self.warmup_candidates();
        format!(
            "CacheWarmup: uptime={:.1}s, hits={}, misses={}, hit_rate={:.1}%, \
             warmup_completed={}, candidates={}",
            elapsed.as_secs_f64(),
            self.cache_hits,
            self.cache_misses,
            hit_rate,
            self.warmup_completed,
            candidates.len(),
        )
    }
}

impl Default for CacheWarmupManager {
    fn default() -> Self {
        Self::new()
    }
}
