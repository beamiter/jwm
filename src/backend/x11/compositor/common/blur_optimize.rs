/// Blur effect helpers: dual-pass Gaussian parameters and a hit/miss counter.
use std::sync::Arc;

/// Parameters for dual-pass Gaussian blur
#[derive(Clone, Copy, Debug)]
pub struct GaussianBlurParams {
    /// Standard deviation of the Gaussian kernel
    pub sigma: f32,
    /// Number of passes (more = higher quality but slower)
    pub passes: u32,
    /// Whether to use separable (two-pass) filtering
    pub use_separable: bool,
}

impl Default for GaussianBlurParams {
    fn default() -> Self {
        Self {
            sigma: 1.0,
            passes: 2,
            use_separable: true,
        }
    }
}

impl GaussianBlurParams {
    /// Create parameters optimized for performance
    pub fn fast() -> Self {
        Self {
            sigma: 0.8,
            passes: 1,
            use_separable: true,
        }
    }

    /// Create parameters optimized for quality
    pub fn high_quality() -> Self {
        Self {
            sigma: 1.5,
            passes: 4,
            use_separable: true,
        }
    }

    /// Create balanced parameters
    pub fn balanced() -> Self {
        Self {
            sigma: 1.0,
            passes: 2,
            use_separable: true,
        }
    }
}

/// Statistics for blur cache usage
#[derive(Clone, Default, Debug)]
pub struct BlurCacheStats {
    pub hits: usize,
    pub misses: usize,
    pub evictions: usize,
}

impl BlurCacheStats {
    pub fn hit_rate(&self) -> f32 {
        let total = self.hits + self.misses;
        if total == 0 {
            0.0
        } else {
            self.hits as f32 / total as f32
        }
    }

    pub fn reset(&mut self) {
        self.hits = 0;
        self.misses = 0;
        self.evictions = 0;
    }
}

/// Tracks blur-cache hit/miss counts for HUD and metrics.
pub struct BlurCache {
    stats: Arc<std::sync::Mutex<BlurCacheStats>>,
}

impl BlurCache {
    pub fn new() -> Self {
        Self {
            stats: Arc::new(std::sync::Mutex::new(BlurCacheStats::default())),
        }
    }

    pub fn record_hit(&self) {
        if let Ok(mut stats) = self.stats.lock() {
            stats.hits += 1;
        }
    }

    pub fn record_miss(&self) {
        if let Ok(mut stats) = self.stats.lock() {
            stats.misses += 1;
        }
    }

    pub fn record_eviction(&self) {
        if let Ok(mut stats) = self.stats.lock() {
            stats.evictions += 1;
        }
    }

    pub fn stats(&self) -> BlurCacheStats {
        self.stats
            .lock()
            .ok()
            .map(|s| s.clone())
            .unwrap_or_default()
    }

    pub fn reset_stats(&self) {
        if let Ok(mut stats) = self.stats.lock() {
            stats.reset();
        }
    }
}

impl Clone for BlurCache {
    fn clone(&self) -> Self {
        Self {
            stats: self.stats.clone(),
        }
    }
}

impl Default for BlurCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_blur_cache_stats() {
        let cache = BlurCache::new();
        cache.record_hit();
        cache.record_hit();
        cache.record_miss();

        let stats = cache.stats();
        assert_eq!(stats.hits, 2);
        assert_eq!(stats.misses, 1);
        assert!((stats.hit_rate() - 0.666).abs() < 0.01);
    }

    #[test]
    fn gaussian_presets_keep_separable_filtering() {
        assert!(GaussianBlurParams::fast().use_separable);
        assert!(GaussianBlurParams::balanced().use_separable);
        assert!(GaussianBlurParams::high_quality().use_separable);
        assert!(GaussianBlurParams::high_quality().passes > GaussianBlurParams::fast().passes);
    }
}
