/// Subpixel Rendering Integration Example
///
/// This module shows how to integrate SubpixelRenderManager into the rendering pipeline.
/// This is an example implementation reference.
use super::subpixel_render::SubpixelRenderManager;

/// Integration example: Window property tracking for subpixel rendering
pub struct SubpixelWindowState {
    /// Window ID
    pub window_id: u32,
    /// Window class name for type detection
    pub class_name: String,
    /// Current blur strength
    pub blur_strength: f32,
    /// Last update timestamp
    pub last_update: std::time::Instant,
}

impl SubpixelWindowState {
    pub fn new(window_id: u32, class_name: String) -> Self {
        Self {
            window_id,
            class_name,
            blur_strength: 0.0,
            last_update: std::time::Instant::now(),
        }
    }
}

/// Example integration functions
pub trait SubpixelRenderIntegration {
    /// Called when a window is created
    fn on_window_created(&mut self, window_id: u32, class_name: &str);

    /// Called when a window is destroyed
    fn on_window_destroyed(&mut self, window_id: u32);

    /// Called during window blur effect
    fn update_window_blur_strength(&mut self, window_id: u32, strength: f32);

    /// Get recommended rendering parameters for a window
    fn get_render_params(
        &self,
        window_id: u32,
        cpu_load: f32,
        gpu_load: f32,
    ) -> SubpixelRenderParams;
}

/// Parameters for subpixel rendering a window
#[derive(Clone, Debug)]
pub struct SubpixelRenderParams {
    /// Whether to apply subpixel rendering
    pub enabled: bool,
    /// Kernel to use for rendering
    pub kernel_name: String,
    /// Blur strength multiplier
    pub strength_multiplier: f32,
    /// Whether to apply color fringing correction
    pub apply_color_correction: bool,
}

impl Default for SubpixelRenderParams {
    fn default() -> Self {
        Self {
            enabled: false,
            kernel_name: "standard".to_string(),
            strength_multiplier: 1.0,
            apply_color_correction: false,
        }
    }
}

/// Example implementation for Compositor
pub struct SubpixelCompositorIntegration {
    manager: SubpixelRenderManager,
    window_states: std::collections::HashMap<u32, SubpixelWindowState>,
}

impl SubpixelCompositorIntegration {
    pub fn new() -> Self {
        Self {
            manager: SubpixelRenderManager::new(),
            window_states: std::collections::HashMap::new(),
        }
    }

    /// Get metrics about subpixel rendering usage
    pub fn get_metrics(&self) -> SubpixelRenderingMetrics {
        let (total, enabled, ratio) = self.manager.stats();
        let perf_metrics = self.manager.metrics();

        SubpixelRenderingMetrics {
            total_windows: total,
            subpixel_enabled_windows: enabled,
            coverage_percent: ratio,
            avg_blur_strength: perf_metrics.avg_blur_strength,
        }
    }
}

impl Default for SubpixelCompositorIntegration {
    fn default() -> Self {
        Self::new()
    }
}

impl SubpixelRenderIntegration for SubpixelCompositorIntegration {
    fn on_window_created(&mut self, window_id: u32, class_name: &str) {
        let mgr = &mut self.manager;
        mgr.register_window(window_id, class_name);

        let state = SubpixelWindowState::new(window_id, class_name.to_string());
        self.window_states.insert(window_id, state);
    }

    fn on_window_destroyed(&mut self, window_id: u32) {
        let mgr = &mut self.manager;
        mgr.remove_window(window_id);

        self.window_states.remove(&window_id);
    }

    fn update_window_blur_strength(&mut self, window_id: u32, strength: f32) {
        let mgr = &mut self.manager;
        mgr.set_window_blur_strength(window_id, strength);

        // Update tracked state
        if let Some(state) = self.window_states.get_mut(&window_id) {
            state.blur_strength = strength;
            state.last_update = std::time::Instant::now();
        }
    }

    fn get_render_params(
        &self,
        window_id: u32,
        cpu_load: f32,
        gpu_load: f32,
    ) -> SubpixelRenderParams {
        if let Some(kernel) = self
            .manager
            .get_adaptive_kernel(window_id, cpu_load, gpu_load)
        {
            let wt = self.manager.get_subpixel_mode(window_id);
            SubpixelRenderParams {
                enabled: wt != super::subpixel_render::SubpixelMode::None,
                kernel_name: kernel.name,
                strength_multiplier: kernel.blur_strength,
                apply_color_correction: true,
            }
        } else {
            SubpixelRenderParams::default()
        }
    }
}

/// Metrics about subpixel rendering
#[derive(Clone, Debug)]
pub struct SubpixelRenderingMetrics {
    pub total_windows: u64,
    pub subpixel_enabled_windows: u64,
    pub coverage_percent: f32,
    pub avg_blur_strength: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_integration_creation() {
        let integration = SubpixelCompositorIntegration::new();
        let metrics = integration.get_metrics();
        assert_eq!(metrics.total_windows, 0);
    }

    #[test]
    fn test_window_lifecycle() {
        let mut integration = SubpixelCompositorIntegration::new();

        // Create a text editor window
        integration.on_window_created(1, "code");
        let metrics = integration.get_metrics();
        assert_eq!(metrics.total_windows, 1);
        assert_eq!(metrics.subpixel_enabled_windows, 1);

        // Create a video player window
        integration.on_window_created(2, "mpv");
        let metrics = integration.get_metrics();
        assert_eq!(metrics.total_windows, 2);
        assert_eq!(metrics.subpixel_enabled_windows, 1);

        // Destroy text editor
        integration.on_window_destroyed(1);
        let metrics = integration.get_metrics();
        assert_eq!(metrics.total_windows, 1);
        assert_eq!(metrics.subpixel_enabled_windows, 0);

        // Destroy video player
        integration.on_window_destroyed(2);
        let metrics = integration.get_metrics();
        assert_eq!(metrics.total_windows, 0);
    }

    #[test]
    fn test_blur_strength_tracking() {
        let mut integration = SubpixelCompositorIntegration::new();
        integration.on_window_created(1, "code");

        // Update blur strength
        integration.update_window_blur_strength(1, 0.5);
        let metrics = integration.get_metrics();
        assert_eq!(metrics.avg_blur_strength, 0.5);

        // Update to higher strength
        integration.update_window_blur_strength(1, 0.8);
        let metrics = integration.get_metrics();
        assert_eq!(metrics.avg_blur_strength, 0.8);
    }

    #[test]
    fn test_render_params_generation() {
        let mut integration = SubpixelCompositorIntegration::new();
        integration.on_window_created(1, "code");

        // Get params under normal load
        let params = integration.get_render_params(1, 0.3, 0.4);
        assert!(params.enabled);
        assert_eq!(params.kernel_name, "Text Optimized");

        // Get params for unknown window
        let params = integration.get_render_params(999, 0.3, 0.4);
        assert!(!params.enabled);
    }
}
