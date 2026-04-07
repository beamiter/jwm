/// Blur effect optimizations including adaptive quality and dual-pass Gaussian
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use super::BlurQuality;

/// Adaptive blur quality based on system load
pub struct AdaptiveBlur {
    quality: Arc<std::sync::Mutex<BlurQuality>>,
    current_load: Arc<AtomicU32>,
}

impl AdaptiveBlur {
    pub fn new() -> Self {
        Self {
            quality: Arc::new(std::sync::Mutex::new(BlurQuality::Full)),
            current_load: Arc::new(AtomicU32::new(50)),
        }
    }

    /// Update blur quality based on GPU/CPU load
    /// load: 0-100, higher means busier system
    pub fn update_load(&self, load: u32) {
        self.current_load.store(load.min(100), Ordering::Relaxed);

        let quality = if load > 90 {
            BlurQuality::Minimal
        } else if load > 80 {
            BlurQuality::Minimal
        } else if load > 70 {
            BlurQuality::Reduced
        } else {
            BlurQuality::Full
        };

        if let Ok(mut q) = self.quality.lock() {
            if *q != quality {
                log::info!("blur: adaptive quality changed to {}",
                    match quality {
                        BlurQuality::Full => "full",
                        BlurQuality::Reduced => "reduced",
                        BlurQuality::Minimal => "minimal",
                    }
                );
                *q = quality;
            }
        }
    }

    /// Get current blur quality
    pub fn quality(&self) -> BlurQuality {
        self.quality.lock()
            .ok()
            .map(|q| *q)
            .unwrap_or(BlurQuality::Full)
    }

    /// Get current system load
    pub fn current_load(&self) -> u32 {
        self.current_load.load(Ordering::Relaxed)
    }

    /// Force a specific quality level
    pub fn set_quality(&self, quality: BlurQuality) {
        if let Ok(mut q) = self.quality.lock() {
            if *q != quality {
                log::info!("blur: quality manually set to {}",
                    match quality {
                        BlurQuality::Full => "full",
                        BlurQuality::Reduced => "reduced",
                        BlurQuality::Minimal => "minimal",
                    }
                );
                *q = quality;
            }
        }
    }
}

impl Clone for AdaptiveBlur {
    fn clone(&self) -> Self {
        Self {
            quality: self.quality.clone(),
            current_load: self.current_load.clone(),
        }
    }
}

impl Default for AdaptiveBlur {
    fn default() -> Self {
        Self::new()
    }
}

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

/// Tracks blur cache performance
#[derive(Clone, Default, Debug)]
pub struct BlurCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub total_blur_time_us: u64,
    pub cache_memory_bytes: u64,
}

impl BlurCacheStats {
    pub fn hit_rate(&self) -> f32 {
        let total = (self.hits + self.misses) as f32;
        if total == 0.0 {
            0.0
        } else {
            self.hits as f32 / total
        }
    }

    pub fn reset(&mut self) {
        self.hits = 0;
        self.misses = 0;
        self.total_blur_time_us = 0;
    }
}

/// Caches blurred regions to avoid re-blurring identical areas
pub struct BlurCache {
    stats: Arc<std::sync::Mutex<BlurCacheStats>>,
}

impl BlurCache {
    pub fn new() -> Self {
        Self {
            stats: Arc::new(std::sync::Mutex::new(BlurCacheStats::default())),
        }
    }

    /// Record a cache hit
    pub fn record_hit(&self) {
        if let Ok(mut stats) = self.stats.lock() {
            stats.hits += 1;
        }
    }

    /// Record a cache miss
    pub fn record_miss(&self) {
        if let Ok(mut stats) = self.stats.lock() {
            stats.misses += 1;
        }
    }

    /// Record blur processing time
    pub fn record_blur_time(&self, microseconds: u64) {
        if let Ok(mut stats) = self.stats.lock() {
            stats.total_blur_time_us += microseconds;
        }
    }

    /// Get cache statistics
    pub fn stats(&self) -> BlurCacheStats {
        self.stats.lock()
            .ok()
            .map(|s| s.clone())
            .unwrap_or_default()
    }

    /// Reset statistics
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
    fn test_adaptive_blur() {
        let blur = AdaptiveBlur::new();

        blur.update_load(95);
        assert_eq!(blur.quality(), BlurQuality::Minimal);

        blur.update_load(85);
        assert_eq!(blur.quality(), BlurQuality::Minimal);

        blur.update_load(25);
        assert_eq!(blur.quality(), BlurQuality::Full);
    }

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
}
