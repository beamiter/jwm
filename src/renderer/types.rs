/// Rendering types and parameters
use std::fmt;

// === Opaque IDs for GPU resources ===

/// Opaque texture identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TextureId(pub u64);

/// Opaque framebuffer identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FboId(pub u32);

/// Opaque shader program identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ShaderId(pub u32);

// === Geometric Types ===

/// Rectangle in pixel or normalized coordinates
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }
}

/// RGBA color (0.0 to 1.0 range)
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub fn new(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    pub fn black() -> Self {
        Self::new(0.0, 0.0, 0.0, 1.0)
    }

    pub fn white() -> Self {
        Self::new(1.0, 1.0, 1.0, 1.0)
    }

    pub fn transparent() -> Self {
        Self::new(0.0, 0.0, 0.0, 0.0)
    }
}

// === Draw Parameters ===

/// Parameters for drawing a textured quad
#[derive(Clone, Debug)]
pub struct DrawParams {
    /// Texture to draw
    pub texture: TextureId,
    /// Rectangle in screen coordinates (pixels)
    pub rect: Rect,
    /// UV rectangle (0.0 to 1.0 normalized texture coordinates)
    pub uv_rect: Rect,
    /// Projection matrix (orthographic, row-major)
    pub projection: [f32; 16],
    /// Opacity (0.0 = transparent, 1.0 = opaque, negative = use texture alpha)
    pub opacity: f32,
    /// Corner radius in pixels (0 = sharp corners)
    pub corner_radius: f32,
    /// Window size in pixels (for corner radius SDF)
    pub size: (f32, f32),
    /// Dim multiplier (1.0 = normal, <1.0 = darken)
    pub dim: f32,
    /// Ripple progress (0.0 to 1.0, <0 = inactive)
    pub ripple_progress: f32,
    /// Ripple amplitude (UV distortion strength)
    pub ripple_amplitude: f32,
}

impl Default for DrawParams {
    fn default() -> Self {
        Self {
            texture: TextureId(0),
            rect: Rect::new(0.0, 0.0, 0.0, 0.0),
            uv_rect: Rect::new(0.0, 0.0, 1.0, 1.0),
            projection: [0.0; 16],
            opacity: 1.0,
            corner_radius: 0.0,
            size: (0.0, 0.0),
            dim: 1.0,
            ripple_progress: -1.0,
            ripple_amplitude: 0.0,
        }
    }
}

/// Parameters for drawing a shadow
#[derive(Clone, Debug)]
pub struct ShadowParams {
    /// Rectangle for the shadow (expanded by spread)
    pub rect: Rect,
    /// Window size (for SDF calculation)
    pub window_size: (f32, f32),
    /// Corner radius (matches window)
    pub corner_radius: f32,
    /// Shadow spread/blur radius
    pub spread: f32,
    /// Shadow color (RGBA)
    pub color: Color,
    /// Projection matrix
    pub projection: [f32; 16],
}

/// Parameters for blur pass
#[derive(Clone, Debug)]
pub struct BlurPassParams {
    /// Source FBO (None = use current framebuffer)
    pub source_fbo: Option<FboId>,
    /// Number of blur levels/passes
    pub levels: usize,
    /// Blur quality setting
    pub quality: BlurQuality,
    /// Screen width
    pub screen_w: u32,
    /// Screen height
    pub screen_h: u32,
}

/// Blur quality levels.
///
/// Variants are ordered Minimal < Reduced < Full so `Ord::min`/`max` and
/// adaptive-downgrade comparisons can pick the lower/higher quality directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BlurQuality {
    Minimal, // Single pass (box blur)
    Reduced, // Half blur levels
    Full,    // All blur levels
}

/// GPU fence synchronization status
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FenceStatus {
    Signaled,       // Fence completed
    TimeoutExpired, // Wait timed out
    Failed,         // Error occurred
}

impl fmt::Display for FenceStatus {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            FenceStatus::Signaled => write!(f, "Signaled"),
            FenceStatus::TimeoutExpired => write!(f, "TimeoutExpired"),
            FenceStatus::Failed => write!(f, "Failed"),
        }
    }
}
