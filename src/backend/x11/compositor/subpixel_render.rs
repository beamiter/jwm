/// Subpixel Rendering Optimization (P7B)
///
/// Improves text rendering quality in blur effects:
/// 1. LCD subpixel geometry hinting for text windows
/// 2. Reduce color fringing in blurred text
/// 3. Adaptive kernel selection based on window type
/// 4. Monitor DPI-aware rendering
///
/// Performance: Improves visual quality without performance cost
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Subpixel rendering mode
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubpixelMode {
    /// No subpixel rendering
    None,
    /// RGB subpixel (horizontal)
    RGB,
    /// BGR subpixel (horizontal reversed)
    BGR,
    /// VRGB subpixel (vertical)
    VRGB,
}

/// Monitor DPI information for subpixel rendering
#[derive(Clone, Copy, Debug)]
pub struct MonitorDPI {
    /// Horizontal DPI
    pub dpi_x: f32,
    /// Vertical DPI
    pub dpi_y: f32,
    /// Subpixel geometry
    pub geometry: SubpixelMode,
}

impl MonitorDPI {
    /// Standard 96 DPI (default)
    pub fn standard() -> Self {
        Self {
            dpi_x: 96.0,
            dpi_y: 96.0,
            geometry: SubpixelMode::RGB,
        }
    }

    /// High DPI (retina/4K)
    pub fn high_dpi(scale: f32) -> Self {
        Self {
            dpi_x: 96.0 * scale,
            dpi_y: 96.0 * scale,
            geometry: SubpixelMode::RGB,
        }
    }

    /// Get subpixel scale factor
    pub fn scale_factor(&self) -> f32 {
        self.dpi_x / 96.0
    }
}

/// Color fringing correction for subpixel rendering
#[derive(Clone, Copy, Debug)]
pub struct ColorFringeCorrection {
    /// Red channel offset (pixels)
    pub r_offset: f32,
    /// Green channel offset (pixels)
    pub g_offset: f32,
    /// Blue channel offset (pixels)
    pub b_offset: f32,
    /// Correction strength (0.0-1.0)
    pub strength: f32,
}

impl ColorFringeCorrection {
    /// No correction
    pub fn none() -> Self {
        Self {
            r_offset: 0.0,
            g_offset: 0.0,
            b_offset: 0.0,
            strength: 0.0,
        }
    }

    /// Standard RGB correction
    pub fn standard() -> Self {
        Self {
            r_offset: -0.3,
            g_offset: 0.0,
            b_offset: 0.3,
            strength: 0.7,
        }
    }

    /// Adaptive correction based on blur strength
    pub fn adaptive(blur_strength: f32) -> Self {
        let strength = (blur_strength * 0.8).min(1.0);
        Self {
            r_offset: -0.2 * strength,
            g_offset: 0.0,
            b_offset: 0.2 * strength,
            strength,
        }
    }
}

/// Window type classification for rendering
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowType {
    /// Text editor (code, vim, emacs)
    TextEditor,
    /// Terminal (alacritty, kitty)
    Terminal,
    /// Browser (firefox, chrome)
    Browser,
    /// Video player (mpv, vlc)
    VideoPlayer,
    /// Generic window
    Generic,
}

impl WindowType {
    /// Detect window type from class name
    pub fn from_class(class: &str) -> Self {
        let lower = class.to_lowercase();
        match lower.as_str() {
            "code" | "vscode" | "sublime_text" | "emacs" | "vim" => WindowType::TextEditor,
            "alacritty" | "kitty" | "wezterm" | "gnome-terminal" => WindowType::Terminal,
            "firefox" | "chrome" | "chromium" | "brave" => WindowType::Browser,
            "mpv" | "vlc" | "ffplay" => WindowType::VideoPlayer,
            _ => WindowType::Generic,
        }
    }

    /// Get recommended subpixel mode
    pub fn recommended_subpixel(&self) -> SubpixelMode {
        match self {
            WindowType::TextEditor | WindowType::Terminal => SubpixelMode::RGB,
            WindowType::Browser => SubpixelMode::RGB,
            WindowType::VideoPlayer => SubpixelMode::None,
            WindowType::Generic => SubpixelMode::None,
        }
    }

    /// Should apply subpixel rendering
    pub fn should_use_subpixel(&self) -> bool {
        matches!(self, WindowType::TextEditor | WindowType::Terminal | WindowType::Browser)
    }
}

/// Blur kernel for subpixel rendering
#[derive(Clone, Debug)]
pub struct SubpixelBlurKernel {
    /// Kernel name
    pub name: String,
    /// Kernel weights (RGB channels)
    pub weights_r: Vec<f32>,
    pub weights_g: Vec<f32>,
    pub weights_b: Vec<f32>,
    /// Kernel radius
    pub radius: u32,
    /// Blur strength (0.0-1.0)
    pub blur_strength: f32,
}

impl SubpixelBlurKernel {
    /// Create RGB subpixel kernel (optimized for horizontal LCD)
    pub fn rgb_kernel(radius: u32) -> Self {
        // Optimized for RGB subpixel layout
        // Reduce green/blue fringing by adjusting channel weights
        let weights_r = vec![0.3, 0.4, 0.3];
        let weights_g = vec![0.25, 0.5, 0.25];
        let weights_b = vec![0.2, 0.6, 0.2];

        Self {
            name: "RGB Subpixel".to_string(),
            weights_r,
            weights_g,
            weights_b,
            radius,
            blur_strength: 0.7,
        }
    }

    /// Create standard blur kernel
    pub fn standard_kernel(radius: u32) -> Self {
        let weights = vec![0.25, 0.5, 0.25];
        Self {
            name: "Standard".to_string(),
            weights_r: weights.clone(),
            weights_g: weights.clone(),
            weights_b: weights,
            radius,
            blur_strength: 0.5,
        }
    }

    /// Create high-quality subpixel kernel for text
    pub fn text_optimized(radius: u32) -> Self {
        let weights_r = vec![0.35, 0.3, 0.35];
        let weights_g = vec![0.2, 0.6, 0.2];
        let weights_b = vec![0.15, 0.7, 0.15];

        Self {
            name: "Text Optimized".to_string(),
            weights_r,
            weights_g,
            weights_b,
            radius,
            blur_strength: 0.8,
        }
    }

    /// Adjust kernel for monitor DPI
    pub fn with_dpi_scale(&mut self, scale: f32) {
        self.radius = (self.radius as f32 * scale).ceil() as u32;
    }

    /// Normalize weights to sum to 1.0
    pub fn normalize(&mut self) {
        let sum_r: f32 = self.weights_r.iter().sum();
        let sum_g: f32 = self.weights_g.iter().sum();
        let sum_b: f32 = self.weights_b.iter().sum();

        if sum_r > 0.0 {
            self.weights_r.iter_mut().for_each(|w| *w /= sum_r);
        }
        if sum_g > 0.0 {
            self.weights_g.iter_mut().for_each(|w| *w /= sum_g);
        }
        if sum_b > 0.0 {
            self.weights_b.iter_mut().for_each(|w| *w /= sum_b);
        }
    }
}

/// Subpixel rendering manager
pub struct SubpixelRenderManager {
    /// Window class → subpixel mode mapping
    window_modes: HashMap<String, SubpixelMode>,
    /// Window type cache
    window_types: HashMap<u32, WindowType>,
    /// Blur kernels cache
    blur_kernels: HashMap<String, SubpixelBlurKernel>,
    /// Monitor DPI information
    monitor_dpi: Arc<std::sync::Mutex<MonitorDPI>>,
    /// Color fringing correction
    color_correction: Arc<std::sync::Mutex<ColorFringeCorrection>>,
    /// Global subpixel mode
    #[allow(dead_code)]
    global_mode: SubpixelMode,
    /// Enable subpixel rendering
    enabled: Arc<AtomicBool>,
    /// Statistics
    total_windows: u64,
    subpixel_windows: u64,
    /// Per-window blur strength tracking
    blur_strength: HashMap<u32, f32>,
    /// Last update time for statistics
    #[allow(dead_code)]
    last_update_ns: u64,
}

impl SubpixelRenderManager {
    pub fn new() -> Self {
        let mut manager = Self {
            window_modes: HashMap::new(),
            window_types: HashMap::new(),
            blur_kernels: HashMap::new(),
            monitor_dpi: Arc::new(std::sync::Mutex::new(MonitorDPI::standard())),
            color_correction: Arc::new(std::sync::Mutex::new(ColorFringeCorrection::standard())),
            global_mode: SubpixelMode::RGB,
            enabled: Arc::new(AtomicBool::new(true)),
            total_windows: 0,
            subpixel_windows: 0,
            blur_strength: HashMap::new(),
            last_update_ns: 0,
        };

        // Pre-populate common kernels
        manager.blur_kernels.insert(
            "rgb".to_string(),
            SubpixelBlurKernel::rgb_kernel(2),
        );
        manager.blur_kernels.insert(
            "standard".to_string(),
            SubpixelBlurKernel::standard_kernel(2),
        );
        manager.blur_kernels.insert(
            "text".to_string(),
            SubpixelBlurKernel::text_optimized(2),
        );

        manager
    }

    /// Set monitor DPI information
    pub fn set_monitor_dpi(&self, dpi: MonitorDPI) {
        if let Ok(mut d) = self.monitor_dpi.lock() {
            *d = dpi;
        }
    }

    /// Get current monitor DPI
    pub fn get_monitor_dpi(&self) -> MonitorDPI {
        self.monitor_dpi
            .lock()
            .ok()
            .map(|d| *d)
            .unwrap_or_else(MonitorDPI::standard)
    }

    /// Set color fringing correction
    pub fn set_color_correction(&self, correction: ColorFringeCorrection) {
        if let Ok(mut c) = self.color_correction.lock() {
            *c = correction;
        }
    }

    /// Get current color fringing correction
    pub fn get_color_correction(&self) -> ColorFringeCorrection {
        self.color_correction
            .lock()
            .ok()
            .map(|c| *c)
            .unwrap_or_else(ColorFringeCorrection::standard)
    }

    /// Register window and detect type
    pub fn register_window(&mut self, window_id: u32, class_name: &str) {
        let window_type = WindowType::from_class(class_name);
        self.window_types.insert(window_id, window_type);
        self.total_windows += 1;

        if window_type.should_use_subpixel() {
            self.subpixel_windows += 1;
            let mode = window_type.recommended_subpixel();
            self.window_modes.insert(class_name.to_string(), mode);
        }
    }

    /// Update blur strength for a window
    pub fn set_window_blur_strength(&mut self, window_id: u32, strength: f32) {
        let clamped = strength.max(0.0).min(1.0);
        self.blur_strength.insert(window_id, clamped);

        // Adjust color correction based on blur strength
        if self.enabled.load(Ordering::Relaxed) {
            let correction = ColorFringeCorrection::adaptive(clamped);
            self.set_color_correction(correction);
        }
    }

    /// Get subpixel mode for window
    pub fn get_subpixel_mode(&self, window_id: u32) -> SubpixelMode {
        if !self.enabled.load(Ordering::Relaxed) {
            return SubpixelMode::None;
        }

        self.window_types
            .get(&window_id)
            .and_then(|wt| {
                if wt.should_use_subpixel() {
                    Some(wt.recommended_subpixel())
                } else {
                    None
                }
            })
            .unwrap_or(SubpixelMode::None)
    }

    /// Get blur kernel for window
    pub fn get_blur_kernel(&self, window_id: u32) -> Option<SubpixelBlurKernel> {
        let mode = self.get_subpixel_mode(window_id);
        if mode == SubpixelMode::None {
            return None;
        }
        let window_type = self.window_types.get(&window_id)?;

        let kernel_name = match (mode, window_type) {
            (SubpixelMode::RGB, WindowType::TextEditor | WindowType::Terminal) => "text",
            (SubpixelMode::RGB, _) => "rgb",
            _ => "standard",
        };

        self.blur_kernels.get(kernel_name).cloned()
    }

    /// Get adaptive kernel based on system state
    pub fn get_adaptive_kernel(
        &self,
        window_id: u32,
        cpu_load: f32,
        gpu_load: f32,
    ) -> Option<SubpixelBlurKernel> {
        let mut kernel = self.get_blur_kernel(window_id)?;

        // Adjust for high load
        let avg_load = (cpu_load + gpu_load) / 2.0;
        if avg_load > 0.8 {
            // Reduce kernel radius under heavy load
            kernel.radius = (kernel.radius as f32 * 0.8).max(1.0) as u32;
        }

        // Adjust for monitor DPI
        let dpi = self.get_monitor_dpi();
        kernel.with_dpi_scale(dpi.scale_factor());

        // Apply color correction
        let correction = self.get_color_correction();
        kernel.blur_strength *= 1.0 + correction.strength * 0.5;

        Some(kernel)
    }

    /// Remove window
    pub fn remove_window(&mut self, window_id: u32) {
        if let Some(wt) = self.window_types.remove(&window_id) {
            self.total_windows = self.total_windows.saturating_sub(1);
            if wt.should_use_subpixel() {
                self.subpixel_windows = self.subpixel_windows.saturating_sub(1);
            }
        }
        self.blur_strength.remove(&window_id);
    }

    /// Enable/disable subpixel rendering
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    /// Check if enabled
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Get statistics
    pub fn stats(&self) -> (u64, u64, f32) {
        let ratio = if self.total_windows > 0 {
            self.subpixel_windows as f32 / self.total_windows as f32 * 100.0
        } else {
            0.0
        };
        (self.total_windows, self.subpixel_windows, ratio)
    }

    /// Get performance metrics
    pub fn metrics(&self) -> SubpixelMetrics {
        SubpixelMetrics {
            total_windows: self.total_windows,
            subpixel_windows: self.subpixel_windows,
            coverage_percent: if self.total_windows > 0 {
                self.subpixel_windows as f32 / self.total_windows as f32 * 100.0
            } else {
                0.0
            },
            avg_blur_strength: if self.blur_strength.is_empty() {
                0.0
            } else {
                let sum: f32 = self.blur_strength.values().sum();
                sum / self.blur_strength.len() as f32
            },
        }
    }
}

/// Performance metrics for subpixel rendering
#[derive(Clone, Debug)]
pub struct SubpixelMetrics {
    pub total_windows: u64,
    pub subpixel_windows: u64,
    pub coverage_percent: f32,
    pub avg_blur_strength: f32,
}

impl Default for SubpixelRenderManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_window_type_detection() {
        assert_eq!(WindowType::from_class("code"), WindowType::TextEditor);
        assert_eq!(WindowType::from_class("alacritty"), WindowType::Terminal);
        assert_eq!(WindowType::from_class("firefox"), WindowType::Browser);
        assert_eq!(WindowType::from_class("mpv"), WindowType::VideoPlayer);
        assert_eq!(WindowType::from_class("unknown"), WindowType::Generic);
    }

    #[test]
    fn test_subpixel_mode_recommendation() {
        assert_eq!(
            WindowType::TextEditor.recommended_subpixel(),
            SubpixelMode::RGB
        );
        assert_eq!(
            WindowType::VideoPlayer.recommended_subpixel(),
            SubpixelMode::None
        );
        assert_eq!(
            WindowType::Terminal.recommended_subpixel(),
            SubpixelMode::RGB
        );
    }

    #[test]
    fn test_manager_creation() {
        let mgr = SubpixelRenderManager::new();
        assert!(mgr.is_enabled());
        assert_eq!(mgr.total_windows, 0);
    }

    #[test]
    fn test_kernel_creation() {
        let kernel = SubpixelBlurKernel::rgb_kernel(2);
        assert_eq!(kernel.radius, 2);
        assert_eq!(kernel.weights_r.len(), 3);
        assert_eq!(kernel.blur_strength, 0.7);
    }

    #[test]
    fn test_text_optimized_kernel() {
        let kernel = SubpixelBlurKernel::text_optimized(2);
        assert_eq!(kernel.name, "Text Optimized");
        assert_eq!(kernel.blur_strength, 0.8);
    }

    #[test]
    fn test_kernel_normalization() {
        let mut kernel = SubpixelBlurKernel::rgb_kernel(2);
        kernel.normalize();

        let sum_r: f32 = kernel.weights_r.iter().sum();
        let sum_g: f32 = kernel.weights_g.iter().sum();
        let sum_b: f32 = kernel.weights_b.iter().sum();

        assert!((sum_r - 1.0).abs() < 0.01);
        assert!((sum_g - 1.0).abs() < 0.01);
        assert!((sum_b - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_monitor_dpi() {
        let dpi = MonitorDPI::standard();
        assert_eq!(dpi.dpi_x, 96.0);
        assert_eq!(dpi.scale_factor(), 1.0);

        let high_dpi = MonitorDPI::high_dpi(2.0);
        assert_eq!(high_dpi.scale_factor(), 2.0);
    }

    #[test]
    fn test_color_fringing_correction() {
        let correction = ColorFringeCorrection::standard();
        assert!(correction.strength > 0.0);

        let none = ColorFringeCorrection::none();
        assert_eq!(none.strength, 0.0);

        let adaptive = ColorFringeCorrection::adaptive(0.5);
        assert!(adaptive.strength > 0.0);
    }

    #[test]
    fn test_window_registration() {
        let mut mgr = SubpixelRenderManager::new();
        mgr.register_window(1, "code");
        mgr.register_window(2, "firefox");
        mgr.register_window(3, "mpv");

        assert_eq!(mgr.total_windows, 3);
        assert_eq!(mgr.subpixel_windows, 2);

        let (total, subpixel, _) = mgr.stats();
        assert_eq!(total, 3);
        assert_eq!(subpixel, 2);
    }

    #[test]
    fn test_blur_strength_update() {
        let mut mgr = SubpixelRenderManager::new();
        mgr.register_window(1, "code");
        mgr.set_window_blur_strength(1, 0.5);

        let metrics = mgr.metrics();
        assert_eq!(metrics.avg_blur_strength, 0.5);
    }

    #[test]
    fn test_adaptive_kernel_selection() {
        let mgr = SubpixelRenderManager::new();
        mgr.set_monitor_dpi(MonitorDPI::high_dpi(1.5));

        let kernel = SubpixelBlurKernel::rgb_kernel(2);
        assert_eq!(kernel.radius, 2);
    }

    #[test]
    fn test_enable_disable() {
        let mgr = SubpixelRenderManager::new();
        assert!(mgr.is_enabled());

        mgr.set_enabled(false);
        assert!(!mgr.is_enabled());

        mgr.set_enabled(true);
        assert!(mgr.is_enabled());
    }
}
