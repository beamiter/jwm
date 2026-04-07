/// Unified optimization management for the compositor
/// Integrates all optimization modules for easy access and coordination

use super::perf_metrics::PerfMetrics;
use super::texture_pool::TexturePool;
use super::shader_cache::ShaderCache;
use super::pixel_buffer_pool::PixelBufferPool;
use super::frame_rate::{FrameRateLimiter, AdaptiveFrameRate};
use super::blur_optimize::AdaptiveBlur;
use super::per_monitor::PerMonitorRenderer;
use super::BlurQuality;
use crate::backend::x11::event_coalescer::EventCoalescer;
use crate::backend::x11::batch::X11RequestBatcher;
use std::time::Instant;
use std::path::PathBuf;

/// Central optimization manager coordinating all subsystems
pub struct OptimizationManager {
    pub perf_metrics: PerfMetrics,
    pub texture_pool: TexturePool,
    pub shader_cache: ShaderCache,
    pub pixel_buffer_pool: PixelBufferPool,
    pub frame_rate_limiter: FrameRateLimiter,
    pub adaptive_frame_rate: AdaptiveFrameRate,
    pub adaptive_blur: AdaptiveBlur,
    pub per_monitor_renderer: PerMonitorRenderer,
    pub event_coalescer: EventCoalescer,

    frame_start: Instant,
    last_load_update: Instant,
}

impl OptimizationManager {
    pub fn new(cache_dir: PathBuf, target_fps: u32) -> Self {
        Self {
            perf_metrics: PerfMetrics::new(),
            texture_pool: TexturePool::new(),
            shader_cache: ShaderCache::new(cache_dir),
            pixel_buffer_pool: PixelBufferPool::new(),
            frame_rate_limiter: FrameRateLimiter::new(target_fps),
            adaptive_frame_rate: AdaptiveFrameRate::new(30, target_fps),
            adaptive_blur: AdaptiveBlur::new(),
            per_monitor_renderer: PerMonitorRenderer::new(),
            event_coalescer: EventCoalescer::new(),
            frame_start: Instant::now(),
            last_load_update: Instant::now(),
        }
    }

    /// Start a new frame and return frame duration
    pub fn frame_start(&mut self) -> std::time::Duration {
        let elapsed = self.frame_start.elapsed();
        self.frame_start = Instant::now();
        self.per_monitor_renderer.next_frame();
        elapsed
    }

    /// End a frame and update metrics
    pub fn frame_end(&mut self) {
        let frame_time = self.frame_start.elapsed();
        self.perf_metrics.record_frame(frame_time);

        // Update adaptive systems periodically
        if self.last_load_update.elapsed() > std::time::Duration::from_millis(100) {
            self.update_adaptive_systems();
            self.last_load_update = Instant::now();
        }
    }

    /// Update all adaptive systems based on performance metrics
    fn update_adaptive_systems(&mut self) {
        let gpu_load = self.perf_metrics.estimate_gpu_load(
            self.frame_rate_limiter.target_fps() as f32
        );

        // Update metrics
        self.perf_metrics.set_gpu_load(gpu_load);

        // Update adaptive systems
        self.adaptive_frame_rate.update_load(gpu_load);
        self.adaptive_blur.update_load(gpu_load);

        // Update X11 batch thresholds
        if let Some(batcher) = {
            // This would need access to the X11 batcher, typically stored in backend
            // For now, this is a placeholder
            None as Option<&X11RequestBatcher>
        } {
            batcher.adjust_thresholds(gpu_load);
        }

        let quality_name = match self.adaptive_blur.quality() {
            BlurQuality::Full => "full",
            BlurQuality::Reduced => "reduced",
            BlurQuality::Minimal => "minimal",
        };

        log::debug!("optimization: GPU load={}, FPS={:.1}, quality={}",
            gpu_load,
            self.perf_metrics.recent_fps(),
            quality_name
        );
    }

    /// Get comprehensive optimization status
    pub fn get_status(&self) -> OptimizationStatus {
        OptimizationStatus {
            gpu_load: self.perf_metrics.gpu_load(),
            cpu_load: self.perf_metrics.cpu_load(),
            avg_fps: self.perf_metrics.avg_fps(),
            recent_fps: self.perf_metrics.recent_fps(),
            frame_count: self.perf_metrics.frame_count(),
            blur_quality: self.adaptive_blur.quality(),
            target_fps: self.frame_rate_limiter.target_fps(),
            vsync_enabled: self.frame_rate_limiter.vsync_enabled(),
            texture_pool_available: self.texture_pool.available_count(),
            texture_pool_in_use: self.texture_pool.in_use_count(),
            pixel_buffer_count: self.pixel_buffer_pool.total_buffered(),
            shader_cache_count: self.shader_cache.count(),
            per_monitor_dirty_fraction: self.per_monitor_renderer.dirty_fraction(),
            event_queue_size: self.event_coalescer.queue_size(),
        }
    }

    /// Log detailed optimization statistics
    pub fn log_stats(&self) {
        let status = self.get_status();
        let quality_name = match status.blur_quality {
            BlurQuality::Full => "full",
            BlurQuality::Reduced => "reduced",
            BlurQuality::Minimal => "minimal",
        };

        log::info!("=== Optimization Statistics ===");
        log::info!("  GPU Load: {}%", status.gpu_load);
        log::info!("  CPU Load: {}%", status.cpu_load);
        log::info!("  FPS: {:.1} (target: {})", status.recent_fps, status.target_fps);
        log::info!("  Blur Quality: {}", quality_name);
        log::info!("  VSync: {}", if status.vsync_enabled { "enabled" } else { "disabled" });
        log::info!("  Texture Pool: {} available, {} in use",
            status.texture_pool_available, status.texture_pool_in_use);
        log::info!("  Pixel Buffers: {} buffered", status.pixel_buffer_count);
        log::info!("  Shader Cache: {} programs", status.shader_cache_count);
        log::info!("  Per-Monitor: {:.0}% dirty", status.per_monitor_dirty_fraction * 100.0);
        log::info!("  Event Queue: {} events", status.event_queue_size);
    }

    /// Clear all caches and pools
    pub fn clear_resources(&mut self, gl: &glow::Context) {
        self.texture_pool.clear(gl);
        self.shader_cache.clear(gl);
        self.pixel_buffer_pool.clear();
        self.event_coalescer.clear();
        log::info!("optimization: cleared all resource pools");
    }

    /// Enable/disable specific optimizations
    pub fn set_blur_enabled(&self, enabled: bool) {
        if enabled {
            self.adaptive_blur.set_quality(BlurQuality::Full);
        } else {
            self.adaptive_blur.set_quality(BlurQuality::Minimal);
        }
    }

    /// Set target frame rate with adaptive adjustment
    pub fn set_target_fps(&self, fps: u32) {
        self.frame_rate_limiter.set_target_fps(fps);
        self.adaptive_frame_rate.limiter().set_target_fps(fps);
    }

    /// Get frame time budget in milliseconds
    pub fn frame_budget_ms(&self) -> f32 {
        self.frame_rate_limiter.frame_budget().as_secs_f32() * 1000.0
    }
}

/// Snapshot of optimization system status
#[derive(Clone)]
pub struct OptimizationStatus {
    pub gpu_load: u32,
    pub cpu_load: u32,
    pub avg_fps: f32,
    pub recent_fps: f32,
    pub frame_count: u64,
    pub blur_quality: BlurQuality,
    pub target_fps: u32,
    pub vsync_enabled: bool,
    pub texture_pool_available: usize,
    pub texture_pool_in_use: usize,
    pub pixel_buffer_count: usize,
    pub shader_cache_count: usize,
    pub per_monitor_dirty_fraction: f32,
    pub event_queue_size: usize,
}

impl OptimizationStatus {
    /// Check if system is overloaded
    pub fn is_overloaded(&self) -> bool {
        self.gpu_load > 90 || self.cpu_load > 90
    }

    /// Check if system is idle
    pub fn is_idle(&self) -> bool {
        self.gpu_load < 20 && self.cpu_load < 20
    }

    /// Get a summary string for logging
    pub fn summary(&self) -> String {
        let quality_name = match self.blur_quality {
            BlurQuality::Full => "full",
            BlurQuality::Reduced => "reduced",
            BlurQuality::Minimal => "minimal",
        };

        format!(
            "GPU:{}% CPU:{}% FPS:{:.1} Blur:{} Load:{}/{}",
            self.gpu_load,
            self.cpu_load,
            self.recent_fps,
            quality_name,
            if self.is_overloaded() { "HIGH" } else if self.is_idle() { "IDLE" } else { "OK" },
            self.target_fps
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_optimization_status() {
        let status = OptimizationStatus {
            gpu_load: 95,
            cpu_load: 50,
            avg_fps: 60.0,
            recent_fps: 55.0,
            frame_count: 1000,
            blur_quality: BlurQuality::Minimal,
            target_fps: 60,
            vsync_enabled: true,
            texture_pool_available: 10,
            texture_pool_in_use: 5,
            pixel_buffer_count: 3,
            shader_cache_count: 8,
            per_monitor_dirty_fraction: 0.5,
            event_queue_size: 2,
        };

        assert!(status.is_overloaded());
        assert!(!status.is_idle());
        assert!(status.summary().contains("GPU:95%"));
    }
}
