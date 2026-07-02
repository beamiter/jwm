mod annotations;
mod effects;
mod expose;
mod font;
pub mod math;
mod overview;
mod pipeline;
mod postprocess;
pub mod shaders;
mod tfp;
mod transitions;

// Optimization modules
pub mod async_x11;
pub mod blur_optimize;
pub mod cache_warmup;
pub mod direct_scanout;
pub mod dirty_region;
pub mod frame_rate;
pub mod gpu_fence_sync;
pub mod integration_helpers;
pub mod optimization_manager;
pub mod pbo_uploader;
pub mod per_monitor;
pub mod perf_metrics;
pub mod pixel_buffer_pool;
pub mod power_saving;
pub mod predictive_render;
pub mod profiler;
pub mod render_batcher;
pub mod render_stats;
pub mod shader_cache;
pub mod subpixel_integration;
pub mod subpixel_render;
pub mod texture_pool;

// Sync control modules
pub mod audio_sync;
pub mod oml_sync_control;
pub mod present;

// Benchmark
pub mod benchmark;

mod config;
mod features;
mod init;
mod render;
mod rules;
mod wallpaper;

pub use async_x11::{DeferredOpQueue, EventQueue, InputPriority, PriorityEventQueue};
pub use blur_optimize::{AdaptiveBlur, BlurCache, BlurCacheStats, GaussianBlurParams};
pub use cache_warmup::{BlurSizeStats, CacheWarmupManager};
pub use direct_scanout::{DirectScanoutManager, DirectScanoutStats, WindowScanoutInfo};
pub use dirty_region::{DirtyRect, DirtyRegionTracker};
pub use frame_rate::{AdaptiveFrameRate, FrameRateLimiter};
pub use gpu_fence_sync::GPUFenceSyncManager;
pub use oml_sync_control::OmlSyncControl;
pub use optimization_manager::{OptimizationManager, OptimizationStatus};
pub use pbo_uploader::PBOUploader;
pub use per_monitor::{MonitorRenderRegion, PerMonitorRenderer};
pub use perf_metrics::PerfMetrics;
pub use pixel_buffer_pool::PixelBufferPool;
pub use power_saving::{BatteryStatus, PowerProfile, PowerSavingConfig, PowerSavingManager};
pub use predictive_render::{PredictiveRenderManager, SceneActivity};
pub use profiler::{FrameProfiler, ProfileZone, ZoneStats};
pub use render_batcher::{BatchKey, GLStateTracker, QuadInstance, RenderBatcher};
pub use render_stats::{GLCallStats, PassStats, RenderStats};
pub use shader_cache::ShaderCache;
pub use subpixel_integration::{SubpixelCompositorIntegration, SubpixelRenderParams};
pub use subpixel_render::{SubpixelMetrics, SubpixelMode, SubpixelRenderManager, WindowType};
pub use texture_pool::TexturePool;

use glow::HasContext;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc;
use x11rb::connection::Connection;
use x11rb::protocol::composite::ConnectionExt as CompositeExt;
use x11rb::protocol::xproto::ConnectionExt as XProtoExt;
use x11rb::rust_connection::RustConnection;

use math::ortho;

// ---------------------------------------------------------------------------
// TFP function pointers (glXBindTexImageEXT / glXReleaseTexImageEXT)
// ---------------------------------------------------------------------------

type GlXBindTexImageEXT =
    unsafe extern "C" fn(*mut x11::xlib::Display, x11::glx::GLXDrawable, i32, *const i32);
type GlXReleaseTexImageEXT =
    unsafe extern "C" fn(*mut x11::xlib::Display, x11::glx::GLXDrawable, i32);

struct TfpFunctions {
    bind: GlXBindTexImageEXT,
    release: GlXReleaseTexImageEXT,
}

// GLX_BIND_TO_TEXTURE_*_EXT constants
const GLX_BIND_TO_TEXTURE_RGBA_EXT: i32 = 0x20D1;
const GLX_BIND_TO_TEXTURE_RGB_EXT: i32 = 0x20D0;
#[allow(dead_code)]
const GLX_Y_INVERTED_EXT: i32 = 0x20D4;
const GLX_TEXTURE_FORMAT_EXT: i32 = 0x20D5;
const GLX_TEXTURE_TARGET_EXT: i32 = 0x20D6;
const GLX_TEXTURE_2D_EXT: i32 = 0x20DC;
const GLX_TEXTURE_FORMAT_RGBA_EXT: i32 = 0x20DA;
const GLX_TEXTURE_FORMAT_RGB_EXT: i32 = 0x20D9;
const GLX_FRONT_LEFT_EXT: i32 = 0x20DE;

// ---------------------------------------------------------------------------
// VSync method selection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VsyncMethod {
    Global,         // glXSwapInterval=1 (traditional, all windows locked to one vblank)
    OmlSyncControl, // GLX_OML_sync_control (per-window MSC-based timing)
    Present,        // X11 Present extension (per-window independent presentation)
}

impl Default for VsyncMethod {
    fn default() -> Self {
        VsyncMethod::Global
    }
}

// ---------------------------------------------------------------------------
// TFP window texture state machine
// ---------------------------------------------------------------------------

/// Explicit state machine for window texture lifecycle.
/// Replaces scattered bool flags (dirty, needs_pixmap_refresh, fading_out).
#[derive(Debug, Clone)]
#[allow(dead_code)]
enum WindowTextureState {
    /// Window just mapped, pixmap being created
    Initializing,
    /// Normal operation, texture ready for rendering
    Active {
        /// Whether the texture needs TFP refresh from pixmap
        dirty: bool,
    },
    /// Geometry changed, pixmap needs recreation on next render
    PendingRefresh,
    /// Window closing, fading out opacity
    FadingOut {
        /// Current opacity (0.0 = fully transparent)
        opacity: f32,
    },
    /// Special animation (e.g., genie minimize)
    Animating {
        /// Animation type/context
        kind: String,
    },
}

impl WindowTextureState {
    /// Check if texture is ready for rendering
    #[allow(dead_code)]
    fn is_renderable(&self) -> bool {
        matches!(
            self,
            WindowTextureState::Active { .. }
                | WindowTextureState::FadingOut { .. }
                | WindowTextureState::Animating { .. }
        )
    }

    /// Check if TFP refresh is needed
    #[allow(dead_code)]
    fn needs_tfp_refresh(&self) -> bool {
        matches!(self, WindowTextureState::Active { dirty: true })
    }

    /// Mark texture as dirty (needs TFP refresh)
    #[allow(dead_code)]
    fn mark_dirty(&mut self) {
        if let WindowTextureState::Active { dirty } = self {
            *dirty = true;
        }
    }

    /// Mark texture clean (TFP refresh complete)
    #[allow(dead_code)]
    fn mark_clean(&mut self) {
        if let WindowTextureState::Active { dirty } = self {
            *dirty = false;
        }
    }
}

// ---------------------------------------------------------------------------

struct WindowTexture {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    damage: u32,
    pixmap: u32,
    glx_pixmap: x11::glx::GLXPixmap,
    gl_texture: glow::Texture,
    dirty: bool,
    has_rgba: bool,
    /// The TFP FBConfig used for this window's GLX pixmap.
    fbconfig: x11::glx::GLXFBConfig,
    /// When true, the pixmap needs to be recreated (deferred from update_geometry).
    needs_pixmap_refresh: bool,
    /// The X11 window ID, needed for deferred pixmap recreation.
    x11_win: u32,
    /// Current fade opacity (0.0 = fully transparent, 1.0 = fully visible).
    /// Used for fade-in/fade-out animations.
    fade_opacity: f32,
    /// Whether this window is fading out (will be removed when opacity reaches 0).
    fading_out: bool,
    /// Window class name (for per-window rules).
    class_name: String,
    /// Per-window opacity override from opacity_rules (0.0..1.0), or None for default.
    opacity_override: Option<f32>,
    /// Whether this window is fullscreen.
    is_fullscreen: bool,
    // --- Feature 3: Per-window corner radius ---
    corner_radius_override: Option<f32>,
    // --- Feature 4: Window scale ---
    scale: f32,
    // --- Feature 13: Frame extents for blur mask ---
    frame_extents: [u32; 4], // left, right, top, bottom
    // --- Feature 14: Window has X Shape (non-rectangular) ---
    is_shaped: bool,
    // --- Scale animation ---
    anim_scale: f32,
    anim_scale_target: f32,
    // --- Urgent state ---
    is_urgent: bool,
    // --- PiP state ---
    is_pip: bool,
    // --- Frosted glass ---
    is_frosted: bool,
    // --- Override-redirect (unmanaged overlay) ---
    is_override_redirect: bool,
    // --- Wobbly state ---
    wobbly: Option<WobblyState>,
    // --- Phase 2.3: Fence sync for async pixmap refresh ---
    pending_fence: Option<glow::Fence>,
    // --- Phase 3.1: Motion trail ---
    motion_trail: std::collections::VecDeque<(i32, i32)>,
    // --- Audio sync: target presentation FPS to match audio stream ---
    audio_sync_target: Option<f32>,
}

// ---------------------------------------------------------------------------
// Cached uniform locations
// ---------------------------------------------------------------------------

struct WindowUniforms {
    projection: Option<glow::UniformLocation>,
    rect: Option<glow::UniformLocation>,
    texture: Option<glow::UniformLocation>,
    opacity: Option<glow::UniformLocation>,
    radius: Option<glow::UniformLocation>,
    size: Option<glow::UniformLocation>,
    dim: Option<glow::UniformLocation>,
    uv_rect: Option<glow::UniformLocation>,
    ripple_progress: Option<glow::UniformLocation>,
    ripple_amplitude: Option<glow::UniformLocation>,
}

struct ShadowUniforms {
    projection: Option<glow::UniformLocation>,
    rect: Option<glow::UniformLocation>,
    shadow_color: Option<glow::UniformLocation>,
    size: Option<glow::UniformLocation>,
    radius: Option<glow::UniformLocation>,
    spread: Option<glow::UniformLocation>,
}

struct BlurUniforms {
    projection: Option<glow::UniformLocation>,
    rect: Option<glow::UniformLocation>,
    texture: Option<glow::UniformLocation>,
    halfpixel: Option<glow::UniformLocation>,
}

/// A single level in the blur mipmap chain.
struct BlurFboLevel {
    fbo: glow::Framebuffer,
    texture: glow::Texture,
    w: u32,
    h: u32,
}

// ---------------------------------------------------------------------------
// Tag-switch transition uniforms
// ---------------------------------------------------------------------------

struct TransitionUniforms {
    projection: Option<glow::UniformLocation>,
    rect: Option<glow::UniformLocation>,
    texture: Option<glow::UniformLocation>,
    opacity: Option<glow::UniformLocation>,
    uv_rect: Option<glow::UniformLocation>,
}

// ---------------------------------------------------------------------------
// Cube transition uniforms
// ---------------------------------------------------------------------------

struct CubeUniforms {
    mvp: Option<glow::UniformLocation>,
    aspect: Option<glow::UniformLocation>,
    texture: Option<glow::UniformLocation>,
    brightness: Option<glow::UniformLocation>,
    uv_rect: Option<glow::UniformLocation>,
}

// ---------------------------------------------------------------------------
// Portal transition uniforms
// ---------------------------------------------------------------------------

struct PortalUniforms {
    projection: Option<glow::UniformLocation>,
    rect: Option<glow::UniformLocation>,
    texture: Option<glow::UniformLocation>,
    progress: Option<glow::UniformLocation>,
    glow: Option<glow::UniformLocation>,
    center: Option<glow::UniformLocation>,
    uv_rect: Option<glow::UniformLocation>,
}

#[derive(Clone, Copy, PartialEq)]
enum TransitionMode {
    Slide,
    Cube,
    Fade,
    Flip,
    Zoom,
    Stack,
    Blinds,
    CoverFlow,
    Helix,
    Portal,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum WallpaperMode {
    Fill,
    Fit,
    Stretch,
    Center,
}

/// Decoded wallpaper image data ready for GPU upload (produced by background thread).
struct WallpaperImageData {
    rgba: Vec<u8>,
    width: u32,
    height: u32,
    mode: WallpaperMode,
}

/// Per-monitor wallpaper state.
struct MonitorWallpaper {
    /// Monitor geometry in screen coordinates.
    mon_x: i32,
    mon_y: i32,
    mon_w: u32,
    mon_h: u32,
    /// GL texture for this monitor's wallpaper (None = use default).
    texture: Option<glow::Texture>,
    mode: WallpaperMode,
    img_w: u32,
    img_h: u32,
    /// Currently-loaded wallpaper path (used to skip reloads when active tags
    /// change but the resolved wallpaper for this monitor stays the same).
    current_path: String,
}

/// Parsed opacity rule: "opacity_percent:class_name"
#[derive(Clone)]
struct OpacityRule {
    opacity: f32, // 0.0..1.0
    class_name: String,
}

/// Parsed corner radius rule: "radius:class_name"
#[derive(Clone)]
struct CornerRadiusRule {
    radius: f32,
    class_name: String,
}

/// Parsed scale rule: "scale_percent:class_name"
#[derive(Clone)]
struct ScaleRule {
    scale: f32, // 0.0..1.0
    class_name: String,
}

// --- Feature 1: Border uniforms ---
struct BorderUniforms {
    projection: Option<glow::UniformLocation>,
    rect: Option<glow::UniformLocation>,
    border_color: Option<glow::UniformLocation>,
    size: Option<glow::UniformLocation>,
    radius: Option<glow::UniformLocation>,
    border_width: Option<glow::UniformLocation>,
}

// --- Feature 9/10: Post-process uniforms ---
struct PostprocessUniforms {
    projection: Option<glow::UniformLocation>, // P5F.1: Cache to avoid per-frame lookup
    rect: Option<glow::UniformLocation>,       // P5F.1: Cache to avoid per-frame lookup
    texture: Option<glow::UniformLocation>,
    color_temp: Option<glow::UniformLocation>,
    saturation: Option<glow::UniformLocation>,
    brightness: Option<glow::UniformLocation>,
    contrast: Option<glow::UniformLocation>,
    invert: Option<glow::UniformLocation>,
    grayscale: Option<glow::UniformLocation>,
    hdr_enabled: Option<glow::UniformLocation>,
    hdr_peak_nits: Option<glow::UniformLocation>,
    tone_mapping_method: Option<glow::UniformLocation>,
    eotf_mode: Option<glow::UniformLocation>,
    output_colorspace: Option<glow::UniformLocation>,
}

// --- Feature 11: HUD uniforms ---
struct HudUniforms {
    projection: Option<glow::UniformLocation>,
    rect: Option<glow::UniformLocation>,
    bg_color: Option<glow::UniformLocation>,
    fg_color: Option<glow::UniformLocation>,
    size: Option<glow::UniformLocation>,
}

struct HudTextUniforms {
    projection: Option<glow::UniformLocation>,
    rect: Option<glow::UniformLocation>,
    texture: Option<glow::UniformLocation>,
}

/// Frame timing statistics for the debug HUD (feature 11).
struct FrameStats {
    frame_count: u64,
    last_fps_update: std::time::Instant,
    fps: f32,
    frame_times: std::collections::VecDeque<f32>, // P5F.3: VecDeque for O(1) operations
    last_frame_time: std::time::Instant,
    // Phase 7.2: Extended debug stats
    draw_calls: u32,
    texture_memory_bytes: u64,
    blur_cache_hits: u64,
    blur_cache_misses: u64,
    // Task 8: Input latency tracking
    last_input_time: Option<std::time::Instant>,
    latency_samples: std::collections::VecDeque<f32>, // in ms, ring buffer up to 300 samples
}

/// Per-window wobbly animation state (grid spring-mass system).
struct WobblyState {
    grid_n: usize,                 // nodes per axis = grid_size + 1
    offsets: Vec<[f32; 2]>,        // grid_n * grid_n node offsets (pixels)
    velocities: Vec<[f32; 2]>,     // grid_n * grid_n node velocities
    dragging: bool,                // true while interactive move is active
    anchor_row: usize,             // drag anchor node row
    anchor_col: usize,             // drag anchor node column
    last_tick: std::time::Instant, // for accurate dt calculation
}

/// Entry for Alt-Tab overview mode.
struct OverviewEntry {
    x11_win: u32,
    target_w: f32,
    target_h: f32,
    is_selected: bool,
    snapshot_texture: Option<glow::Texture>,
    title: String,
    /// (texture, width, height) for the rendered title label.
    title_texture: Option<(glow::Texture, u32, u32)>,
    /// Which face of the hexagonal prism this entry occupies (0..5).
    face_index: usize,
}

/// Single particle for close animation.
struct Particle {
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    color: [f32; 4],
    lifetime: f32,
    max_lifetime: f32,
}

/// Entry for Expose/Mission Control mode.
struct ExposeEntry {
    x11_win: u32,
    orig_x: f32,
    orig_y: f32,
    orig_w: f32,
    orig_h: f32,
    target_x: f32,
    target_y: f32,
    target_w: f32,
    target_h: f32,
    current_x: f32,
    current_y: f32,
    current_w: f32,
    current_h: f32,
    is_hovered: bool,
}

/// Snap preview state.
struct SnapPreview {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    opacity: f32,
    start: std::time::Instant,
    fading_out: bool,
}

/// Single tab in a window group.
struct WindowTab {
    x11_win: u32,
    title: String,
    is_active: bool,
}

/// Active particle system (one per closing window).
struct ParticleSystem {
    particles: Vec<Particle>,
}

/// Cached uniform locations for edge glow shader.
struct EdgeGlowUniforms {
    projection: Option<glow::UniformLocation>,
    rect: Option<glow::UniformLocation>,
    glow_color: Option<glow::UniformLocation>,
    glow_width: Option<glow::UniformLocation>,
    mouse: Option<glow::UniformLocation>,
    screen_size: Option<glow::UniformLocation>,
    time: Option<glow::UniformLocation>,
}

/// Cached uniform locations for tilt shader.
struct TiltUniforms {
    projection: Option<glow::UniformLocation>,
    rect: Option<glow::UniformLocation>,
    texture: Option<glow::UniformLocation>,
    opacity: Option<glow::UniformLocation>,
    radius: Option<glow::UniformLocation>,
    size: Option<glow::UniformLocation>,
    dim: Option<glow::UniformLocation>,
    uv_rect: Option<glow::UniformLocation>,
    tilt: Option<glow::UniformLocation>,
    perspective: Option<glow::UniformLocation>,
    grid_size: Option<glow::UniformLocation>,
    light_dir: Option<glow::UniformLocation>,
}

/// Cached uniform locations for wobbly shader.
struct WobblyUniforms {
    projection: Option<glow::UniformLocation>,
    rect: Option<glow::UniformLocation>,
    texture: Option<glow::UniformLocation>,
    opacity: Option<glow::UniformLocation>,
    radius: Option<glow::UniformLocation>,
    size: Option<glow::UniformLocation>,
    dim: Option<glow::UniformLocation>,
    uv_rect: Option<glow::UniformLocation>,
    grid_offsets: Option<glow::UniformLocation>, // vec2 u_grid_offsets[289]
    grid_n: Option<glow::UniformLocation>,       // int u_grid_n
}

/// Cached uniform locations for overview background shader.
struct OverviewBgUniforms {
    projection: Option<glow::UniformLocation>,
    rect: Option<glow::UniformLocation>,
    opacity: Option<glow::UniformLocation>,
}

/// Magnifier uniform locations (added to PostprocessUniforms).
struct MagnifierUniforms {
    magnifier_enabled: Option<glow::UniformLocation>,
    magnifier_center: Option<glow::UniformLocation>,
    magnifier_radius: Option<glow::UniformLocation>,
    magnifier_zoom: Option<glow::UniformLocation>,
    colorblind_mode: Option<glow::UniformLocation>,
}

/// Particle shader uniform locations.
struct ParticleUniforms {
    projection: Option<glow::UniformLocation>,
    point_size: Option<glow::UniformLocation>,
}

// ---------------------------------------------------------------------------
// Tile-based damage tracker for partial redraw optimization (Phase 2.1)
// ---------------------------------------------------------------------------

struct DamageTracker {
    /// Screen divided into dynamically-sized tiles.
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
    fn new(screen_w: u32, screen_h: u32) -> Self {
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

    /// Compute dynamic grid size based on resolution
    /// 4K: 16x12, 1080p: 8x6, 720p: 5x4
    fn compute_grid_size(screen_w: u32, screen_h: u32) -> (u32, u32) {
        let cols = (screen_w / 240).clamp(4, 16);
        let rows = (screen_h / 180).clamp(3, 12);
        (cols, rows)
    }

    /// Update tracking state for dynamic threshold calculation
    fn update_state(&mut self, window_count: usize, animating: bool) {
        self.window_count = window_count;
        self.animating = animating;
    }

    /// Calculate dynamic threshold based on scene complexity
    #[allow(dead_code)]
    fn dynamic_threshold(&self) -> f32 {
        // Animations need more precision (smaller threshold = more likely to use scissor)
        if self.animating {
            return 0.3;
        }
        // More windows = more likely to have localized damage
        match self.window_count {
            0..=3 => 0.7, // Few windows = large tiles OK, higher threshold
            4..=8 => 0.5, // Moderate
            _ => 0.35,    // Many windows = keep scissor precision
        }
    }

    fn mark_all_dirty(&mut self) {
        self.dirty_tiles.fill(true);
    }

    fn clear(&mut self) {
        self.dirty_tiles.fill(false);
    }

    /// Mark a specific region as dirty (tiles overlapping the rect)
    fn mark_region_dirty(&mut self, x: i32, y: i32, w: u32, h: u32) {
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

    fn dirty_fraction(&self) -> f32 {
        let dirty = self.dirty_tiles.iter().filter(|&&d| d).count();
        dirty as f32 / self.dirty_tiles.len() as f32
    }

    /// Returns the bounding rectangle of all dirty tiles, or None if nothing is dirty.
    /// Uses dynamic threshold based on scene complexity.
    #[allow(dead_code)]
    fn dirty_bounds(&self) -> Option<(i32, i32, u32, u32)> {
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

    fn resize(&mut self, screen_w: u32, screen_h: u32) {
        self.screen_w = screen_w;
        self.screen_h = screen_h;
        // Recompute grid size based on new resolution
        let (tile_cols, tile_rows) = Self::compute_grid_size(screen_w, screen_h);
        self.tile_cols = tile_cols;
        self.tile_rows = tile_rows;
        self.tile_w = (screen_w + tile_cols - 1) / tile_cols;
        self.tile_h = (screen_h + tile_rows - 1) / tile_rows;
        self.dirty_tiles = vec![true; (tile_cols * tile_rows) as usize];
    }
}

// ---------------------------------------------------------------------------
// Blur quality auto-downgrade (Phase 2.2)
// ---------------------------------------------------------------------------

pub(crate) use crate::renderer::types::BlurQuality;

// ---------------------------------------------------------------------------
// Phase 3.2: Genie minimize animation
// ---------------------------------------------------------------------------

struct GenieUniforms {
    projection: Option<glow::UniformLocation>,
    rect: Option<glow::UniformLocation>,
    texture: Option<glow::UniformLocation>,
    opacity: Option<glow::UniformLocation>,
    radius: Option<glow::UniformLocation>,
    size: Option<glow::UniformLocation>,
    dim: Option<glow::UniformLocation>,
    uv_rect: Option<glow::UniformLocation>,
    progress: Option<glow::UniformLocation>,
    dock_pos: Option<glow::UniformLocation>,
    grid_size: Option<glow::UniformLocation>,
}

/// Active genie minimize animation for one window.
struct GenieAnimation {
    start: std::time::Instant,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    gl_texture: glow::Texture,
    has_rgba: bool,
    // The animation owns these resources (transferred from the WindowTexture)
    // and frees them when it completes; the texture is sampled for the whole
    // animation, so it must not be freed earlier.
    glx_pixmap: x11::glx::GLXPixmap,
    pixmap: u32,
    damage: u32,
}

// ---------------------------------------------------------------------------
// Phase 3.3: Window open ripple
// ---------------------------------------------------------------------------

struct RippleState {
    x11_win: u32,
    start: std::time::Instant,
}

// ---------------------------------------------------------------------------
// Compositor
// ---------------------------------------------------------------------------

pub(crate) struct Compositor {
    conn: Arc<RustConnection>,
    xlib_display: *mut x11::xlib::Display,
    tfp: TfpFunctions,
    glx_context: x11::glx::GLXContext,
    fbconfig_rgba: x11::glx::GLXFBConfig,
    fbconfig_rgb: x11::glx::GLXFBConfig,
    /// Per-visual TFP FBConfig map: visual_id -> (FBConfig, is_rgba).
    /// On some drivers (e.g. Ubuntu 20's Mesa), TFP requires the FBConfig to
    /// match the source window's visual exactly — a generic depth-based
    /// fallback produces garbled textures for mismatched visuals.
    tfp_visual_configs: HashMap<u32, (x11::glx::GLXFBConfig, bool)>,
    /// 10-bit TFP FBConfig map (for HDR source windows): visual_id -> (FBConfig, is_rgba)
    #[allow(dead_code)]
    tfp_visual_configs_10bit: HashMap<u32, (x11::glx::GLXFBConfig, bool)>,
    overlay_window: u32,
    /// Window that owns the _NET_WM_CM_Sn selection, advertising this
    /// compositor to other clients (screenshot tools, etc.).
    cm_selection_owner: u32,
    glx_drawable: x11::glx::GLXDrawable,
    gl: glow::Context,
    #[allow(dead_code)]
    shader_cache: ShaderCache,
    program: glow::Program,
    shadow_program: glow::Program,
    blur_down_program: glow::Program,
    blur_up_program: glow::Program,
    // P4: Temporal blur mix program
    #[allow(dead_code)]
    temporal_blur_mix_program: glow::Program,
    #[allow(dead_code)]
    temporal_blur_mix_uniforms: BlurUniforms,
    win_uniforms: WindowUniforms,
    shadow_uniforms: ShadowUniforms,
    blur_down_uniforms: BlurUniforms,
    blur_up_uniforms: BlurUniforms,
    quad_vao: glow::VertexArray,
    windows: HashMap<u32, WindowTexture>,
    screen_w: u32,
    screen_h: u32,
    #[allow(dead_code)]
    root: u32,
    needs_render: bool,
    context_current: bool,
    /// Hash of the last rendered scene for skip-unchanged-frame optimization.
    last_scene_hash: u64,
    // Compositor visual settings (read from config once at init)
    corner_radius: f32,
    shadow_enabled: bool,
    shadow_radius: f32,
    shadow_offset: [f32; 2],
    shadow_color: [f32; 4],
    inactive_opacity: f32,
    active_opacity: f32,
    // Blur settings
    blur_enabled: bool,
    blur_strength: u32,
    blur_fbos: Vec<BlurFboLevel>,
    /// FBO to capture the scene (for blur source)
    scene_fbo: Option<(glow::Framebuffer, glow::Texture)>,
    // Fade settings
    fading: bool,
    fade_in_step: f32,
    fade_out_step: f32,
    // Per-window rule settings
    shadow_exclude: Vec<String>,
    opacity_rules: Vec<OpacityRule>,
    blur_exclude: Vec<String>,
    rounded_corners_exclude: Vec<String>,
    detect_client_opacity: bool,
    // Fullscreen optimization
    fullscreen_unredirect: bool,
    /// Currently unredirected fullscreen window (if any)
    unredirected_window: Option<u32>,

    // --- VSync method ---
    vsync_method: VsyncMethod,
    /// GLX_OML_sync_control for per-window MSC-based vblank timing
    oml: Option<oml_sync_control::OmlSyncControl>,
    /// Audio sync manager for per-window audio-video synchronization
    audio_sync: audio_sync::AudioSyncManager,
    /// Present extension for per-window independent presentation
    present_mgr: Option<present::PresentManager>,

    // --- Feature 1: Window borders ---
    border_program: glow::Program,
    border_uniforms: BorderUniforms,
    border_enabled: bool,
    border_width: f32,
    border_color_focused: [f32; 4],
    border_color_unfocused: [f32; 4],

    // --- Feature 3: Per-window corner radius rules ---
    corner_radius_rules: Vec<CornerRadiusRule>,

    // --- Feature 4: Window scale ---
    scale_rules: Vec<ScaleRule>,

    // --- Feature 6: Damage region tracking for partial redraw ---
    damage_tracker: DamageTracker,
    // P5C: Rectangle-level precise dirty tracking
    dirty_region_tracker: DirtyRegionTracker,

    // --- Phase 2.2: Blur quality auto-downgrade ---
    blur_quality: BlurQuality,
    blur_quality_auto: bool,

    // --- Feature 8: Color temperature / color management ---
    postprocess_program: glow::Program,
    postprocess_uniforms: PostprocessUniforms,
    /// FBO for post-process pass (captures the composited scene)
    postprocess_fbo: Option<(glow::Framebuffer, glow::Texture)>,
    color_temperature: f32,
    saturation: f32,
    brightness: f32,
    contrast: f32,

    // --- Feature 10: Invert / accessibility ---
    invert_colors: bool,
    grayscale: bool,

    // --- P3: HDR / 10-bit output ---
    hdr_enabled: bool,
    hdr_peak_nits: f32,
    tone_mapping_method: i32, // 0=none, 1=Reinhard, 2=ACES

    // --- Feature 11: Debug HUD ---
    hud_program: glow::Program,
    hud_uniforms: HudUniforms,
    hud_text_program: glow::Program,
    hud_text_uniforms: HudTextUniforms,
    hud_text_texture: Option<glow::Texture>,
    hud_text_width: u32,
    hud_text_height: u32,
    hud_text_cache: String,
    debug_hud: bool,
    sys_stats: crate::backend::sys_stats::SysStatsSampler,
    frame_stats: FrameStats,

    // --- Feature 12: Screenshot ---
    pending_screenshot: Option<std::path::PathBuf>,
    pending_screenshot_region: Option<(std::path::PathBuf, i32, i32, u32, u32)>,

    // --- Feature 13: Blur mask / frame extents ---
    blur_use_frame_extents: bool,

    // --- Feature 14: Shadow shape ---
    shadow_bottom_extra: f32,

    // --- Tag-switch slide transition ---
    transition_program: glow::Program,
    transition_uniforms: TransitionUniforms,
    /// FBO + texture holding a snapshot of the old scene before tag switch.
    transition_fbo: Option<(glow::Framebuffer, glow::Texture)>,
    /// When Some, a slide transition is in progress.
    transition_start: Option<std::time::Instant>,
    /// Duration of the slide transition.
    transition_duration: std::time::Duration,
    /// +1 = forward (old scene slides left), -1 = backward (old scene slides right).
    transition_direction: f32,
    /// Pixels at the top of the screen to exclude from the transition overlay.
    transition_exclude_top: u32,
    /// Monitor rect (x, y, w, h) for the transition — clips animation to one monitor.
    transition_mon_x: i32,
    transition_mon_y: i32,
    transition_mon_w: u32,
    transition_mon_h: u32,
    /// Transition animation mode (slide or cube).
    transition_mode: TransitionMode,

    // --- Cube transition ---
    cube_program: glow::Program,
    cube_uniforms: CubeUniforms,
    /// FBO + texture holding a snapshot of the new scene (for cube mode).
    transition_new_fbo: Option<(glow::Framebuffer, glow::Texture)>,

    // --- Portal transition ---
    portal_program: glow::Program,
    portal_uniforms: PortalUniforms,

    // --- Window scale animation ---
    window_animation: bool,
    window_animation_scale: f32,

    // --- Dim inactive ---
    inactive_dim: f32,

    // --- Mouse position (shared by magnifier, tilt, edge glow) ---
    mouse_x: f32,
    mouse_y: f32,

    // --- Screen edge glow ---
    edge_glow_program: glow::Program,
    edge_glow_uniforms: EdgeGlowUniforms,
    edge_glow: bool,
    edge_glow_active: bool,
    /// Suppressed while pointer is over a client window (prevents re-activation).
    edge_glow_suppressed: bool,
    edge_glow_color: [f32; 4],
    edge_glow_width: f32,

    // --- Attention animation ---
    attention_animation: bool,
    attention_color: [f32; 4],
    compositor_start_time: std::time::Instant,

    // --- PiP visual treatment ---
    pip_border_color: [f32; 4],
    pip_border_width: f32,

    // --- Magnifier ---
    magnifier_enabled: bool,
    magnifier_radius: f32,
    magnifier_zoom: f32,
    magnifier_uniforms: MagnifierUniforms,

    // --- Window 3D tilt ---
    tilt_program: glow::Program,
    tilt_uniforms: TiltUniforms,
    window_tilt: bool,
    tilt_amount: f32,
    tilt_perspective: f32,
    tilt_speed: f32,
    tilt_grid: u32,
    tilt_current_x: f32,
    tilt_current_y: f32,
    tilt_target_x: f32,
    tilt_target_y: f32,

    // --- Frosted glass ---
    frosted_glass_rules: Vec<String>,
    frosted_glass_strength: u32,
    /// Hash of the below-scene at the time of the last blur computation.
    /// Used to skip expensive blur passes when only the frosted window itself
    /// changed (e.g. fcitx popup updating while typing).
    blur_cache_hash: u64,

    // --- Alt-Tab overview ---
    overview_active: bool,
    overview_windows: Vec<OverviewEntry>,
    overview_opacity: f32,
    overview_bg_program: glow::Program,
    overview_bg_uniforms: OverviewBgUniforms,
    // --- Overview prism state ---
    overview_prism_target_angle: f32,
    overview_prism_current_angle: f32,
    overview_prism_last_tick: Option<std::time::Instant>,
    overview_slide_offset: usize,
    overview_total_clients: usize,
    // Monitor bounds for overview (multi-monitor)
    overview_mon_x: i32,
    overview_mon_y: i32,
    overview_mon_w: u32,
    overview_mon_h: u32,
    overview_entry_progress: f32,
    overview_closing: bool,
    overview_exit_progress: f32,

    // --- Wobbly windows ---
    wobbly_program: glow::Program,
    wobbly_uniforms: WobblyUniforms,
    wobbly_windows: bool,
    wobbly_stiffness: f32,
    wobbly_damping: f32,
    wobbly_restore_stiffness: f32,
    wobbly_grid_size: u32,

    // --- Phase 5: Expose/Mission Control ---
    expose_active: bool,
    expose_enabled: bool,
    expose_gap: f32,
    expose_entries: Vec<ExposeEntry>,
    expose_opacity: f32,
    expose_start: Option<std::time::Instant>,

    // --- Phase 5: Smart Snap Preview ---
    snap_preview_enabled: bool,
    snap_preview_color: [f32; 4],
    snap_animation_duration_ms: u64,
    snap_target: Option<SnapPreview>,

    // --- Phase 5: Window Peek (Boss Key) ---
    peek_active: bool,
    peek_enabled: bool,
    peek_exclude: Vec<String>,
    peek_opacity: f32,
    peek_start: Option<std::time::Instant>,

    // --- Phase 5: Window Tabs ---
    window_tabs_enabled: bool,
    tab_bar_height: f32,
    tab_bar_color: [f32; 4],
    tab_active_color: [f32; 4],
    window_groups: HashMap<u32, Vec<WindowTab>>,

    // --- Particle effects ---
    particle_program: glow::Program,
    particle_uniforms: ParticleUniforms,
    particle_effects: bool,
    particle_count: u32,
    particle_lifetime: f32,
    particle_gravity: f32,
    particle_systems: Vec<ParticleSystem>,
    particle_vao: glow::VertexArray,
    particle_vbo: glow::Buffer,

    // --- Wallpaper ---
    /// Default wallpaper texture (used for monitors without a per-monitor override).
    wallpaper_texture: Option<glow::Texture>,
    wallpaper_mode: WallpaperMode,
    /// Stored wallpaper path for change detection during hot-reload.
    wallpaper_path: String,
    wallpaper_img_w: u32,
    wallpaper_img_h: u32,
    /// Per-monitor wallpaper overrides. Populated by set_monitors().
    monitor_wallpapers: Vec<MonitorWallpaper>,

    // --- Phase 6.1: Colorblind correction ---
    colorblind_mode: i32, // 0=none, 1=deuteranopia, 2=protanopia, 3=tritanopia

    // --- Phase 6.2: Screen annotations ---
    annotation_active: bool,
    annotation_strokes: Vec<annotations::AnnotationStroke>,
    annotation_color: [f32; 4],
    annotation_line_width: f32,

    // --- Phase 6.3: Zoom to fit ---
    zoom_to_fit_window: Option<u32>,
    zoom_to_fit_scale: f32,
    zoom_to_fit_target: f32,

    // --- Phase 7.1: Shader hot reload ---
    // --- Phase 7.2: Extended debug HUD ---
    debug_hud_extended: bool,

    // --- Phase 7.3: Screen recording ---
    recording_active: bool,
    recording_fps: u32,
    recording_bitrate: String,
    recording_quality: u32,
    recording_encoder: String,
    recording_output_dir: String,
    recording_process: Option<std::process::Child>,
    recording_last_frame: Option<std::time::Instant>,
    recording_pbo: [Option<glow::Buffer>; 2],

    // --- Phase 3.1: Motion trail (drag ghosting) ---
    motion_trail_enabled: bool,
    motion_trail_frames: u32,
    motion_trail_opacity: f32,

    // --- Phase 3.2: Genie minimize animation ---
    genie_program: glow::Program,
    genie_uniforms: GenieUniforms,
    genie_minimize: bool,
    genie_duration_ms: u64,
    genie_active: Vec<GenieAnimation>,
    dock_position: (f32, f32),

    // --- Phase 3.3: Window open ripple ---
    ripple_on_open: bool,
    ripple_duration: f32,
    ripple_amplitude: f32,
    ripple_active: Vec<RippleState>,

    // --- Phase 3.4: Focus switch highlight ---
    focus_highlight: bool,
    focus_highlight_color: [f32; 4],
    focus_highlight_duration_ms: u64,
    focus_highlight_start: Option<(u32, std::time::Instant)>,
    last_focused_window: Option<u32>,

    // --- Phase 3.5: Wallpaper crossfade ---
    wallpaper_crossfade: bool,
    wallpaper_crossfade_duration_ms: u64,
    old_wallpaper_texture: Option<glow::Texture>,
    wallpaper_transition_start: Option<std::time::Instant>,

    // --- Async wallpaper loading ---
    /// Receiver for the default wallpaper decoded on a background thread.
    pending_wallpaper: Option<mpsc::Receiver<WallpaperImageData>>,
    /// Receivers for per-monitor wallpapers decoded on background threads.
    /// Each entry: (mon_index_in_vec, receiver).
    pending_monitor_wallpapers: Vec<(usize, mpsc::Receiver<WallpaperImageData>)>,

    // --- Shader hot-reload ---
    shader_hot_reload_enabled: bool,
    shader_dir: String,
    shader_file_mtimes: std::collections::HashMap<String, std::time::SystemTime>,

    // --- VRR (Variable Refresh Rate) ---
    is_game_window: HashMap<u32, bool>,
    vrr_active: bool,
    vrr_last_check: std::time::Instant,

    // --- Adaptive Blur: Hysteresis to prevent flicker ---
    last_gpu_load: u32,
    last_gpu_load_update: std::time::Instant,

    // --- P4: Per-monitor and temporal blur optimization ---
    /// Parsed blur strength mapping: Hz -> strength (e.g., 60->2, 144->4)
    blur_strength_by_hz: Vec<(u32, u32)>, // [(hz, strength), ...]
    /// Per-monitor blur quality: monitor_index -> BlurQuality
    blur_quality_by_monitor: HashMap<u32, BlurQuality>,
    /// Monitor rectangles: monitor_index -> (x, y, width, height) from RandR
    monitor_rects: Vec<(u32, i32, i32, u32, u32)>, // P5B: Real geometry for window->monitor mapping
    /// Monitor refresh rates: monitor_index -> Hz
    monitor_refresh_rates: HashMap<u32, u32>,
    /// Temporal blur: previous frame blur FBO
    prev_blur_fbo: Option<(glow::Framebuffer, glow::Texture)>,
    /// Temporal blur: previous frame window positions hash (to detect movement)
    prev_window_positions_hash: u64,
    /// Temporal blur: mix ratio (0.0 = all new, 1.0 = all previous)
    #[allow(dead_code)]
    temporal_blur_mix_ratio: f32,
    /// Temporal blur: is enabled
    temporal_blur_enabled: bool,
    /// Temporal blur: count of reuse frames
    temporal_blur_reuse_count: u64,
    /// Temporal blur: total blur frames (for hit rate calculation)
    temporal_blur_total_count: u64,

    // --- P6C: Zero-copy texture upload optimization ---
    /// PBO uploader for async texture uploads (overview/font rendering)
    pbo_uploader: PBOUploader,

    // --- P6B: GPU Fence Sync optimization ---
    /// GPU fence manager for non-blocking TFP sync
    gpu_fence_sync_mgr: GPUFenceSyncManager,

    // --- P6A: Async X11 communication ---
    /// Priority-aware event queue (separates event processing from rendering)
    #[allow(dead_code)]
    priority_event_queue: PriorityEventQueue,
    /// Deferred X11 operations (NameWindowPixmap, etc.)
    deferred_ops_queue: DeferredOpQueue,

    // --- P7A: Predictive rendering ---
    /// Predictive render manager for adaptive FPS and power saving
    #[allow(dead_code)]
    predictive_render_mgr: PredictiveRenderManager,

    // --- P7C: Smart cache warmup ---
    /// Cache warmup manager for predictive pre-loading
    #[allow(dead_code)]
    cache_warmup_mgr: CacheWarmupManager,

    // --- P7D: Power saving mode ---
    /// Power saving manager for battery-aware optimization
    #[allow(dead_code)]
    power_saving_mgr: PowerSavingManager,

    // --- P7B: Subpixel rendering optimization ---
    /// Subpixel rendering manager for improved text quality
    #[allow(dead_code)]
    subpixel_render_mgr: SubpixelRenderManager,

    // --- Phase 2 Optimizations ---
    /// Direct scanout manager for fullscreen bypass
    direct_scanout_mgr: DirectScanoutManager,
    /// Frame profiler for render pipeline timing
    frame_profiler: FrameProfiler,
    /// GL state tracker to avoid redundant state changes
    gl_state_tracker:
        GLStateTracker<glow::Program, glow::Texture, glow::VertexArray, glow::Framebuffer>,

    // --- Benchmark harness ---
    benchmark: benchmark::BenchmarkHarness,

    // --- HDR output control ---
    eotf_mode: i32,         // 0=sRGB gamma, 1=PQ (ST2084), 2=HLG
    output_colorspace: i32, // 0=BT.709, 1=BT.2020
    hdr_output_10bit: bool, // true if GLX context is actually 10-bit

    // --- Reusable per-frame scratch buffers (render_frame) ---
    // Detached via mem::take during the frame, refilled, then restored, so the
    // hot render path runs without per-frame heap allocation.
    scratch_scene_info: Vec<(u32, WindowScanoutInfo)>,
    scratch_blur_dirty: Vec<u32>,
    scratch_tfp_order: Vec<u32>,
}

// Safety: The compositor is only accessed from the single-threaded X11 event loop.
// All raw pointers (Display*, GLXContext, etc.) are only used from that thread.
unsafe impl Send for Compositor {}

impl Drop for Compositor {
    fn drop(&mut self) {
        self.clear_overview_snapshots();
        unsafe {
            self.gl.delete_program(self.program);
            self.gl.delete_program(self.shadow_program);
            self.gl.delete_program(self.blur_down_program);
            self.gl.delete_program(self.blur_up_program);
            self.gl.delete_program(self.temporal_blur_mix_program);
            self.gl.delete_program(self.border_program);
            self.gl.delete_program(self.postprocess_program);
            self.gl.delete_program(self.hud_program);
            self.gl.delete_program(self.hud_text_program);
            if let Some(tex) = self.hud_text_texture.take() {
                self.gl.delete_texture(tex);
            }
            self.gl.delete_program(self.transition_program);
            self.gl.delete_program(self.cube_program);
            self.gl.delete_program(self.portal_program);
            self.gl.delete_program(self.edge_glow_program);
            self.gl.delete_program(self.tilt_program);
            self.gl.delete_program(self.wobbly_program);
            self.gl.delete_program(self.overview_bg_program);
            self.gl.delete_program(self.particle_program);
            self.gl.delete_program(self.genie_program);
            self.gl.delete_buffer(self.particle_vbo);
            self.gl.delete_vertex_array(self.particle_vao);
            self.gl.delete_vertex_array(self.quad_vao);
            // Clean up blur FBOs
            for level in self.blur_fbos.drain(..) {
                self.gl.delete_framebuffer(level.fbo);
                self.gl.delete_texture(level.texture);
            }
            if let Some((fbo, tex)) = self.scene_fbo.take() {
                self.gl.delete_framebuffer(fbo);
                self.gl.delete_texture(tex);
            }
            if let Some((fbo, tex)) = self.postprocess_fbo.take() {
                self.gl.delete_framebuffer(fbo);
                self.gl.delete_texture(tex);
            }
            if let Some((fbo, tex)) = self.transition_fbo.take() {
                self.gl.delete_framebuffer(fbo);
                self.gl.delete_texture(tex);
            }
            if let Some((fbo, tex)) = self.transition_new_fbo.take() {
                self.gl.delete_framebuffer(fbo);
                self.gl.delete_texture(tex);
            }
            if let Some(tex) = self.wallpaper_texture.take() {
                self.gl.delete_texture(tex);
            }
            for mw in self.monitor_wallpapers.drain(..) {
                if let Some(tex) = mw.texture {
                    self.gl.delete_texture(tex);
                }
            }
            // Phase 3.2: Clean up genie animation textures
            for ga in self.genie_active.drain(..) {
                self.gl.delete_texture(ga.gl_texture);
            }
            // Phase 3.5: Clean up old wallpaper crossfade texture
            if let Some(tex) = self.old_wallpaper_texture.take() {
                self.gl.delete_texture(tex);
            }
            // Clean up recording PBOs
            for pbo in &mut self.recording_pbo {
                if let Some(buf) = pbo.take() {
                    self.gl.delete_buffer(buf);
                }
            }
        }
        // Stop recording if active
        if self.recording_active {
            self.recording_active = false;
            if let Some(mut child) = self.recording_process.take() {
                drop(child.stdin.take());
                let _ = child.wait();
            }
        }
        // Tear down synchronously: remove_window() would start a fade-out / genie
        // animation that never ticks again during Drop, leaking the GL texture,
        // GLX pixmap, X pixmap and Damage. Free everything immediately instead.
        let wins: Vec<u32> = self.windows.keys().copied().collect();
        for w in wins {
            self.remove_window_immediate(w);
        }
        // Destroy the _NET_WM_CM_Sn selection owner window (releases ownership)
        let _ = self.conn.destroy_window(self.cm_selection_owner);
        // Undo the MANUAL redirect so the X server renders windows normally again
        let _ = self.conn.composite_unredirect_subwindows(
            self.root,
            x11rb::protocol::composite::Redirect::MANUAL,
        );
        let _ = self
            .conn
            .composite_release_overlay_window(self.overlay_window);
        let _ = self.conn.flush();
        unsafe {
            x11::glx::glXDestroyContext(self.xlib_display, self.glx_context);
            x11::xlib::XCloseDisplay(self.xlib_display);
        }
    }
}

/// X error handler that logs errors instead of calling exit().
unsafe extern "C" fn ignore_x_error(
    _display: *mut x11::xlib::Display,
    event: *mut x11::xlib::XErrorEvent,
) -> i32 {
    let e = unsafe { &*event };
    log::debug!(
        "compositor: X error: type={}, error_code={}, request_code={}, minor_code={}, resourceid=0x{:x}",
        e.type_,
        e.error_code,
        e.request_code,
        e.minor_code,
        e.resourceid
    );
    0
}
