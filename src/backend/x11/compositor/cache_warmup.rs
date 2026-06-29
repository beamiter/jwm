/// Smart Cache Warmup (P7C)
///
/// Predictive cache warming based on usage patterns:
/// 1. Pre-load common shader variants (browser, terminal, video player)
/// 2. Pre-render frequent blur sizes (statistical analysis)
/// 3. Startup optimization: warm critical paths
///
/// Performance: Reduces cache miss penalty (2-5ms per miss → instant hit)
use std::collections::HashMap;
use std::time::Instant;

/// Common window classes that should be pre-warmed
const COMMON_WINDOW_CLASSES: &[&str] = &[
    "firefox",
    "chrome",
    "chromium",
    "brave", // Browsers
    "alacritty",
    "kitty",
    "wezterm",
    "gnome-terminal", // Terminals
    "code",
    "vscode",
    "sublime_text",
    "emacs",
    "vim", // Editors
    "mpv",
    "vlc",
    "ffplay", // Video players
    "steam",
    "lutris", // Gaming
];

/// Blur size frequency tracker
#[derive(Clone, Debug)]
pub struct BlurSizeStats {
    /// Blur size (width, height) → hit count
    pub size_frequency: HashMap<(u32, u32), u64>,
    /// Total blur operations
    pub total_blurs: u64,
    /// Last update time
    pub last_update: Instant,
}

impl BlurSizeStats {
    pub fn new() -> Self {
        Self {
            size_frequency: HashMap::new(),
            total_blurs: 0,
            last_update: Instant::now(),
        }
    }

    /// Record blur operation
    pub fn record_blur(&mut self, width: u32, height: u32) {
        let key = (width, height);
        *self.size_frequency.entry(key).or_insert(0) += 1;
        self.total_blurs += 1;
        self.last_update = Instant::now();
    }

    /// Get top N most frequent blur sizes
    pub fn top_sizes(&self, n: usize) -> Vec<((u32, u32), u64)> {
        let mut sizes: Vec<_> = self
            .size_frequency
            .iter()
            .map(|(&size, &count)| (size, count))
            .collect();
        sizes.sort_by_key(|&(_, count)| std::cmp::Reverse(count));
        sizes.into_iter().take(n).collect()
    }

    /// Get cache warmup candidates (sizes with >5% frequency)
    pub fn warmup_candidates(&self) -> Vec<(u32, u32)> {
        let threshold = (self.total_blurs as f32 * 0.05) as u64;
        self.size_frequency
            .iter()
            .filter(|&(_, count)| *count > threshold)
            .map(|(size, _)| *size)
            .collect()
    }
}

impl Default for BlurSizeStats {
    fn default() -> Self {
        Self::new()
    }
}

/// Shader variant for a window class
#[derive(Clone, Debug)]
pub struct ShaderVariant {
    /// Window class (e.g., "firefox")
    pub class_name: String,
    /// Shader features needed (blur, shadow, corner_radius, etc.)
    pub features: Vec<String>,
    /// Hit count (for priority)
    pub hit_count: u64,
    /// Last used time
    pub last_used: Instant,
}

impl ShaderVariant {
    pub fn new(class_name: String, features: Vec<String>) -> Self {
        Self {
            class_name,
            features,
            hit_count: 0,
            last_used: Instant::now(),
        }
    }

    /// Record usage
    pub fn record_hit(&mut self) {
        self.hit_count += 1;
        self.last_used = Instant::now();
    }
}

/// Smart cache warmup manager
pub struct CacheWarmupManager {
    /// Blur size statistics
    blur_stats: BlurSizeStats,
    /// Shader variant tracking
    shader_variants: HashMap<String, ShaderVariant>,
    /// Pre-warmed blur sizes
    prewarmed_blur_sizes: Vec<(u32, u32)>,
    /// Pre-warmed shader classes
    prewarmed_shaders: Vec<String>,
    /// Warmup completed flag
    warmup_completed: bool,
    /// Warmup start time
    warmup_start: Option<Instant>,
    /// Statistics
    total_warmup_time_ms: f32,
    cache_hits_after_warmup: u64,
    cache_misses_after_warmup: u64,
}

impl CacheWarmupManager {
    pub fn new() -> Self {
        Self {
            blur_stats: BlurSizeStats::new(),
            shader_variants: HashMap::new(),
            prewarmed_blur_sizes: Vec::new(),
            prewarmed_shaders: Vec::new(),
            warmup_completed: false,
            warmup_start: None,
            total_warmup_time_ms: 0.0,
            cache_hits_after_warmup: 0,
            cache_misses_after_warmup: 0,
        }
    }

    /// Record blur operation for statistics
    pub fn record_blur_operation(&mut self, width: u32, height: u32) {
        self.blur_stats.record_blur(width, height);
    }

    /// Record shader variant usage
    pub fn record_shader_variant(&mut self, class_name: &str, features: Vec<String>) {
        let variant = self
            .shader_variants
            .entry(class_name.to_string())
            .or_insert_with(|| ShaderVariant::new(class_name.to_string(), features.clone()));
        variant.record_hit();
    }

    /// Perform cache warmup at startup
    ///
    /// # Safety
    /// Requires valid GL context
    pub unsafe fn perform_startup_warmup<F>(&mut self, warmup_fn: F) -> f32
    where
        F: Fn(&str, &[(u32, u32)]),
    {
        if self.warmup_completed {
            return 0.0;
        }

        self.warmup_start = Some(Instant::now());
        log::info!("cache_warmup: starting startup warmup");

        // Phase 1: Pre-warm common window classes
        for class in COMMON_WINDOW_CLASSES {
            self.prewarmed_shaders.push(class.to_string());
        }

        // Phase 2: Pre-warm common blur sizes
        // Common resolutions: 1920x1080, 2560x1440, 3840x2160
        let common_blur_sizes = vec![(1920, 1080), (2560, 1440), (1280, 720), (3840, 2160)];
        self.prewarmed_blur_sizes.clone_from(&common_blur_sizes);

        // Call warmup function (provided by compositor)
        for class in &self.prewarmed_shaders {
            warmup_fn(class, &common_blur_sizes);
        }

        let elapsed = self.warmup_start.unwrap().elapsed();
        self.total_warmup_time_ms = elapsed.as_secs_f32() * 1000.0;
        self.warmup_completed = true;

        log::info!(
            "cache_warmup: completed in {:.2}ms ({} shaders, {} blur sizes)",
            self.total_warmup_time_ms,
            self.prewarmed_shaders.len(),
            self.prewarmed_blur_sizes.len()
        );

        self.total_warmup_time_ms
    }

    /// Perform runtime adaptive warmup based on usage patterns
    ///
    /// Called periodically to warm up hot paths discovered at runtime
    ///
    /// # Safety
    /// Requires valid GL context
    pub unsafe fn perform_adaptive_warmup<F>(&mut self, warmup_fn: F)
    where
        F: Fn(&[(u32, u32)]),
    {
        // Get warmup candidates (blur sizes with >5% frequency)
        let candidates = self.blur_stats.warmup_candidates();

        // Only warmup if we have new candidates
        let new_candidates: Vec<_> = candidates
            .iter()
            .filter(|size| !self.prewarmed_blur_sizes.contains(size))
            .copied()
            .collect();

        if !new_candidates.is_empty() {
            log::debug!(
                "cache_warmup: adaptive warmup for {} new blur sizes",
                new_candidates.len()
            );

            warmup_fn(&new_candidates);

            // Add to prewarmed list
            self.prewarmed_blur_sizes.extend(new_candidates);
        }
    }

    /// Record cache hit (after warmup)
    pub fn record_cache_hit(&mut self) {
        if self.warmup_completed {
            self.cache_hits_after_warmup += 1;
        }
    }

    /// Record cache miss (after warmup)
    pub fn record_cache_miss(&mut self) {
        if self.warmup_completed {
            self.cache_misses_after_warmup += 1;
        }
    }

    /// Get cache hit rate after warmup
    pub fn cache_hit_rate(&self) -> f32 {
        let total = self.cache_hits_after_warmup + self.cache_misses_after_warmup;
        if total == 0 {
            return 1.0;
        }
        self.cache_hits_after_warmup as f32 / total as f32
    }

    /// Get statistics
    pub fn stats(&self) -> String {
        let hit_rate = self.cache_hit_rate() * 100.0;
        let top_sizes = self.blur_stats.top_sizes(5);

        let mut stats = format!(
            "CacheWarmup: warmup={:.2}ms, shaders={}, blur_sizes={}, hit_rate={:.1}%\n",
            self.total_warmup_time_ms,
            self.prewarmed_shaders.len(),
            self.prewarmed_blur_sizes.len(),
            hit_rate
        );

        stats.push_str("  Top blur sizes: ");
        for ((w, h), count) in top_sizes.iter().take(3) {
            stats.push_str(&format!("{}x{}({}), ", w, h, count));
        }

        stats
    }

    /// Get top blur sizes for analysis
    pub fn top_blur_sizes(&self, n: usize) -> Vec<((u32, u32), u64)> {
        self.blur_stats.top_sizes(n)
    }

    /// Check if warmup completed
    pub fn is_warmup_completed(&self) -> bool {
        self.warmup_completed
    }
}

impl Default for CacheWarmupManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Shader feature detector
pub struct ShaderFeatureDetector {
    /// Feature usage tracking: feature_name → count
    feature_counts: HashMap<String, u64>,
    /// Feature combinations: (features_vec) → count
    feature_combos: HashMap<Vec<String>, u64>,
}

impl ShaderFeatureDetector {
    pub fn new() -> Self {
        Self {
            feature_counts: HashMap::new(),
            feature_combos: HashMap::new(),
        }
    }

    /// Record feature usage
    pub fn record_features(&mut self, features: &[String]) {
        // Track individual features
        for feature in features {
            *self.feature_counts.entry(feature.clone()).or_insert(0) += 1;
        }

        // Track feature combinations
        let mut combo = features.to_vec();
        combo.sort();
        *self.feature_combos.entry(combo).or_insert(0) += 1;
    }

    /// Get most common features
    pub fn common_features(&self, n: usize) -> Vec<(String, u64)> {
        let mut features: Vec<_> = self
            .feature_counts
            .iter()
            .map(|(feat, &count)| (feat.clone(), count))
            .collect();
        features.sort_by_key(|&(_, count)| std::cmp::Reverse(count));
        features.into_iter().take(n).collect()
    }

    /// Get most common feature combinations
    pub fn common_combos(&self, n: usize) -> Vec<(Vec<String>, u64)> {
        let mut combos: Vec<_> = self
            .feature_combos
            .iter()
            .map(|(combo, &count)| (combo.clone(), count))
            .collect();
        combos.sort_by_key(|&(_, count)| std::cmp::Reverse(count));
        combos.into_iter().take(n).collect()
    }
}

impl Default for ShaderFeatureDetector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_blur_stats_creation() {
        let stats = BlurSizeStats::new();
        assert_eq!(stats.total_blurs, 0);
    }

    #[test]
    fn test_blur_stats_recording() {
        let mut stats = BlurSizeStats::new();
        stats.record_blur(1920, 1080);
        stats.record_blur(1920, 1080);
        stats.record_blur(2560, 1440);

        assert_eq!(stats.total_blurs, 3);
        assert_eq!(stats.size_frequency.get(&(1920, 1080)), Some(&2));
    }

    #[test]
    fn test_blur_stats_top_sizes() {
        let mut stats = BlurSizeStats::new();
        stats.record_blur(1920, 1080);
        stats.record_blur(1920, 1080);
        stats.record_blur(1920, 1080);
        stats.record_blur(2560, 1440);

        let top = stats.top_sizes(2);
        assert_eq!(top.len(), 2);
        assert_eq!(top[0], ((1920, 1080), 3));
        assert_eq!(top[1], ((2560, 1440), 1));
    }

    #[test]
    fn test_warmup_manager_creation() {
        let mgr = CacheWarmupManager::new();
        assert!(!mgr.is_warmup_completed());
    }

    #[test]
    fn test_shader_feature_detector() {
        let mut detector = ShaderFeatureDetector::new();

        detector.record_features(&["blur".to_string(), "shadow".to_string()]);
        detector.record_features(&["blur".to_string()]);

        let common = detector.common_features(3);
        assert_eq!(common[0].0, "blur");
        assert_eq!(common[0].1, 2);
    }
}
