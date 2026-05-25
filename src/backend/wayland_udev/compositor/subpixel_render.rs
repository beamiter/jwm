use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum SubpixelMode {
    None,
    RGB,
    BGR,
    VRGB,
}

#[derive(Clone, Debug)]
pub(crate) struct MonitorDPI {
    pub dpi_x: f32,
    pub dpi_y: f32,
    pub geometry: SubpixelMode,
}

#[derive(Clone, Debug)]
pub(crate) struct SubpixelBlurKernel {
    pub name: String,
    pub weights_r: Vec<f32>,
    pub weights_g: Vec<f32>,
    pub weights_b: Vec<f32>,
    pub radius: u32,
    pub blur_strength: f32,
}

pub(crate) struct SubpixelRenderManager {
    windows: HashMap<u64, SubpixelWindowState>,
    monitor_dpi: MonitorDPI,
    enabled: bool,
}

struct SubpixelWindowState {
    window_id: u64,
    class_name: String,
    blur_strength: f32,
    subpixel_mode: SubpixelMode,
}

impl MonitorDPI {
    /// Standard 96x96 DPI with RGB subpixel layout.
    pub(crate) fn standard() -> Self {
        Self {
            dpi_x: 96.0,
            dpi_y: 96.0,
            geometry: SubpixelMode::RGB,
        }
    }

    /// High-DPI scaled monitor (96 * scale) with RGB subpixel layout.
    pub(crate) fn high_dpi(scale: f32) -> Self {
        Self {
            dpi_x: 96.0 * scale,
            dpi_y: 96.0 * scale,
            geometry: SubpixelMode::RGB,
        }
    }
}

impl SubpixelBlurKernel {
    /// Standard symmetric gaussian-like blur kernel applied uniformly to all channels.
    pub(crate) fn standard_kernel(strength: f32) -> Self {
        let radius = (strength * 3.0).ceil() as u32;
        let size = (radius * 2 + 1) as usize;
        let sigma = strength.max(0.5);

        let mut weights = Vec::with_capacity(size);
        let mut sum = 0.0_f32;

        for i in 0..size {
            let x = i as f32 - radius as f32;
            let w = (-x * x / (2.0 * sigma * sigma)).exp();
            weights.push(w);
            sum += w;
        }

        // Normalize
        for w in &mut weights {
            *w /= sum;
        }

        Self {
            name: "standard".to_string(),
            weights_r: weights.clone(),
            weights_g: weights.clone(),
            weights_b: weights,
            radius,
            blur_strength: strength,
        }
    }

    /// RGB subpixel-aware kernel with per-channel offset weights.
    /// R channel is shifted left, G is centered, B is shifted right.
    pub(crate) fn rgb_kernel(strength: f32) -> Self {
        let radius = (strength * 3.0).ceil() as u32;
        let size = (radius * 2 + 1) as usize;
        let sigma = strength.max(0.5);

        // Subpixel offset: 1/3 pixel for RGB layout
        let offset = 1.0 / 3.0_f32;

        let mut weights_r = Vec::with_capacity(size);
        let mut weights_g = Vec::with_capacity(size);
        let mut weights_b = Vec::with_capacity(size);

        for i in 0..size {
            let x = i as f32 - radius as f32;

            // R shifted left
            let xr = x + offset;
            weights_r.push((-xr * xr / (2.0 * sigma * sigma)).exp());

            // G centered
            weights_g.push((-x * x / (2.0 * sigma * sigma)).exp());

            // B shifted right
            let xb = x - offset;
            weights_b.push((-xb * xb / (2.0 * sigma * sigma)).exp());
        }

        let mut kernel = Self {
            name: "rgb_subpixel".to_string(),
            weights_r,
            weights_g,
            weights_b,
            radius,
            blur_strength: strength,
        };
        kernel.normalize();
        kernel
    }

    /// Normalize each channel's weights to sum to 1.0.
    pub(crate) fn normalize(&mut self) {
        fn normalize_channel(weights: &mut [f32]) {
            let sum: f32 = weights.iter().sum();
            if sum > 0.0 {
                for w in weights.iter_mut() {
                    *w /= sum;
                }
            }
        }

        normalize_channel(&mut self.weights_r);
        normalize_channel(&mut self.weights_g);
        normalize_channel(&mut self.weights_b);
    }
}

impl SubpixelRenderManager {
    pub(crate) fn new() -> Self {
        Self {
            windows: HashMap::new(),
            monitor_dpi: MonitorDPI::standard(),
            enabled: false,
        }
    }

    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Set the monitor DPI information used for kernel generation.
    pub(crate) fn set_monitor_dpi(&mut self, dpi: MonitorDPI) {
        self.monitor_dpi = dpi;
    }

    /// Register a window and auto-detect its subpixel mode based on class name.
    /// Terminals and text editors get RGB mode for sharper text rendering.
    /// Video players get None mode since subpixel rendering is not beneficial for video.
    pub(crate) fn register_window(&mut self, window_id: u64, class_name: &str) {
        let lower = class_name.to_lowercase();
        let subpixel_mode = if lower.contains("term")
            || lower.contains("alacritty")
            || lower.contains("kitty")
            || lower.contains("wezterm")
            || lower.contains("editor")
            || lower.contains("code")
            || lower.contains("vim")
            || lower.contains("emacs")
            || lower.contains("gedit")
            || lower.contains("kate")
        {
            SubpixelMode::RGB
        } else if lower.contains("mpv")
            || lower.contains("vlc")
            || lower.contains("video")
            || lower.contains("totem")
        {
            SubpixelMode::None
        } else {
            self.monitor_dpi.geometry
        };

        self.windows.insert(
            window_id,
            SubpixelWindowState {
                window_id,
                class_name: class_name.to_string(),
                blur_strength: 1.0,
                subpixel_mode,
            },
        );
    }

    /// Remove a window from the subpixel render manager.
    pub(crate) fn remove_window(&mut self, window_id: u64) {
        self.windows.remove(&window_id);
    }

    /// Set the blur strength for a specific window.
    pub(crate) fn set_window_blur_strength(&mut self, window_id: u64, strength: f32) {
        if let Some(state) = self.windows.get_mut(&window_id) {
            state.blur_strength = strength.clamp(0.0, 10.0);
        }
    }

    /// Get the subpixel rendering mode for a window.
    pub(crate) fn get_subpixel_mode(&self, window_id: u64) -> SubpixelMode {
        self.windows
            .get(&window_id)
            .map(|s| s.subpixel_mode)
            .unwrap_or(SubpixelMode::None)
    }

    /// Get the blur kernel for a window based on its subpixel mode and the monitor DPI.
    /// Returns an RGB-aware kernel for RGB/BGR modes, standard kernel otherwise.
    pub(crate) fn get_blur_kernel(&self, window_id: u64) -> Option<SubpixelBlurKernel> {
        let state = self.windows.get(&window_id)?;

        if !self.enabled {
            return None;
        }

        let kernel = match state.subpixel_mode {
            SubpixelMode::RGB | SubpixelMode::BGR => {
                SubpixelBlurKernel::rgb_kernel(state.blur_strength)
            }
            SubpixelMode::VRGB | SubpixelMode::None => {
                SubpixelBlurKernel::standard_kernel(state.blur_strength)
            }
        };

        Some(kernel)
    }

    /// Get an adaptive blur kernel that reduces radius under high system load.
    /// When CPU or GPU load exceeds 70%, the blur radius is reduced proportionally
    /// to maintain frame budget.
    pub(crate) fn get_adaptive_kernel(
        &self,
        window_id: u64,
        cpu_load: u32,
        gpu_load: u32,
    ) -> Option<SubpixelBlurKernel> {
        let state = self.windows.get(&window_id)?;

        if !self.enabled {
            return None;
        }

        let max_load = cpu_load.max(gpu_load);
        let strength = if max_load > 70 {
            // Scale down blur strength linearly from 100% at 70% load to 30% at 100% load
            let reduction = (max_load - 70) as f32 / 30.0; // 0.0 at 70%, 1.0 at 100%
            state.blur_strength * (1.0 - reduction * 0.7)
        } else {
            state.blur_strength
        };

        let kernel = match state.subpixel_mode {
            SubpixelMode::RGB | SubpixelMode::BGR => {
                SubpixelBlurKernel::rgb_kernel(strength)
            }
            SubpixelMode::VRGB | SubpixelMode::None => {
                SubpixelBlurKernel::standard_kernel(strength)
            }
        };

        Some(kernel)
    }
}
