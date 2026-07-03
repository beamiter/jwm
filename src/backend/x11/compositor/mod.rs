mod annotations;
mod effects;
mod expose;
mod font;
mod overview;
mod pipeline;
mod postprocess;
mod tfp;
mod transitions;
mod types;

// Optimization modules

// Sync control modules
pub mod oml_sync_control;
pub mod present;

// Backend-independent modules shared by X11 compositors.
pub mod benchmark {
    pub use crate::backend::x11::compositor_common::benchmark::*;
}
pub mod blur_optimize {
    pub use crate::backend::x11::compositor_common::blur_optimize::*;
}
pub mod cache_warmup {
    pub use crate::backend::x11::compositor_common::cache_warmup::*;
}
pub mod direct_scanout {
    pub use crate::backend::x11::compositor_common::direct_scanout::*;
}
pub mod dirty_region {
    pub use crate::backend::x11::compositor_common::dirty_region::*;
}
pub(crate) mod damage_tracker {
    pub(crate) use crate::backend::x11::compositor_common::damage_tracker::*;
}
pub mod frame_rate {
    pub use crate::backend::x11::compositor_common::frame_rate::*;
}
pub(crate) mod frame_stats {
    pub(crate) use crate::backend::x11::compositor_common::frame_stats::*;
}
pub mod gpu_fence_sync {
    pub use crate::backend::x11::compositor_common::gpu_fence_sync::*;
}
pub mod math {
    pub use crate::backend::x11::compositor_common::math::*;
}
pub mod pbo_uploader {
    pub use crate::backend::x11::compositor_common::pbo_uploader::*;
}
pub mod per_monitor {
    pub use crate::backend::x11::compositor_common::per_monitor::*;
}
pub mod perf_metrics {
    pub use crate::backend::x11::compositor_common::perf_metrics::*;
}
pub mod pixel_buffer_pool {
    pub use crate::backend::x11::compositor_common::pixel_buffer_pool::*;
}
pub mod power_saving {
    pub use crate::backend::x11::compositor_common::power_saving::*;
}
pub mod predictive_render {
    pub use crate::backend::x11::compositor_common::predictive_render::*;
}
pub mod profiler {
    pub use crate::backend::x11::compositor_common::profiler::*;
}
pub mod render_batcher {
    pub use crate::backend::x11::compositor_common::render_batcher::*;
}
pub mod render_stats {
    pub use crate::backend::x11::compositor_common::render_stats::*;
}
pub(crate) mod rules_common {
    pub(crate) use crate::backend::x11::compositor_common::rules::*;
}
pub mod shader_cache {
    pub use crate::backend::x11::compositor_common::shader_cache::*;
}
pub mod subpixel_integration {
    pub use crate::backend::x11::compositor_common::subpixel_integration::*;
}
pub mod subpixel_render {
    pub use crate::backend::x11::compositor_common::subpixel_render::*;
}
pub mod texture_pool {
    pub use crate::backend::x11::compositor_common::texture_pool::*;
}
pub mod async_x11 {
    pub use crate::backend::x11::compositor_common::async_x11::*;
}
pub mod annotations_common {
    pub use crate::backend::x11::compositor_common::annotations::*;
}
pub mod audio_sync {
    pub use crate::backend::x11::compositor_common::audio_sync::*;
}
pub mod effects_common {
    pub use crate::backend::x11::compositor_common::effects::*;
}
pub mod expose_common {
    pub use crate::backend::x11::compositor_common::expose::*;
}
pub mod integration_helpers {
    pub use crate::backend::x11::compositor_common::integration_helpers::*;
}
pub(crate) mod latency {
    pub(crate) use crate::backend::x11::compositor_common::latency::*;
}
pub mod optimization_manager {
    pub use crate::backend::x11::compositor_common::optimization_manager::*;
}
pub mod oml_sync_common {
    pub use crate::backend::x11::compositor_common::oml_sync::*;
}
pub mod shaders {
    pub use crate::backend::x11::compositor_common::shaders::*;
}
pub mod transitions_common {
    pub use crate::backend::x11::compositor_common::transitions::*;
}
pub(crate) mod wallpaper_common {
    pub(crate) use crate::backend::x11::compositor_common::wallpaper::*;
}
pub(crate) mod vsync {
    pub(crate) use crate::backend::x11::compositor_common::vsync::*;
}
pub(crate) mod wobbly {
    pub(crate) use crate::backend::x11::compositor_common::wobbly::*;
}

mod config;
mod features;
mod init;
mod render;
mod rules;
mod wallpaper;

pub use async_x11::{DeferredOpQueue, EventQueue, InputPriority, PriorityEventQueue};
pub use blur_optimize::{AdaptiveBlur, BlurCache, BlurCacheStats, GaussianBlurParams};
pub use cache_warmup::{BlurSizeStats, CacheWarmupManager};
pub(crate) use damage_tracker::DamageTracker;
pub use direct_scanout::{DirectScanoutManager, DirectScanoutStats, WindowScanoutInfo};
pub use dirty_region::{DirtyRect, DirtyRegionTracker};
pub use frame_rate::{AdaptiveFrameRate, FrameRateLimiter};
pub(crate) use frame_stats::FrameStats;
pub use gpu_fence_sync::GPUFenceSyncManager;
pub(crate) use latency::latency_stats;
pub use oml_sync_control::OmlSyncControl;
pub use optimization_manager::{OptimizationManager, OptimizationStatus};
pub use pbo_uploader::PBOUploader;
pub use per_monitor::{MonitorRenderRegion, PerMonitorRenderer};
pub use perf_metrics::PerfMetrics;
pub use pixel_buffer_pool::PixelBufferPool;
pub use crate::backend::x11::compositor_common::present::PresentController;
pub use power_saving::{BatteryStatus, PowerProfile, PowerSavingConfig, PowerSavingManager};
pub use predictive_render::{PredictiveRenderManager, SceneActivity};
pub use profiler::{FrameProfiler, ProfileZone, ZoneStats};
pub use render_batcher::{BatchKey, GLStateTracker, QuadInstance, RenderBatcher};
pub use render_stats::{GLCallStats, PassStats, RenderStats};
pub(crate) use rules_common::{
    CornerRadiusRule, OpacityRule, ScaleRule, corner_radius_rule_for_class, opacity_rule_for_class,
    parse_corner_radius_rules, parse_opacity_rules, parse_scale_rules, scale_rule_for_class,
};
pub(crate) use rules_common::{
    blur_strength_for_hz, class_matches_exclude, contains_ignore_case, monitor_id_by_overlap,
    parse_blur_quality_by_monitor, parse_blur_strength_by_hz,
};
pub use shader_cache::ShaderCache;
pub use subpixel_integration::{SubpixelCompositorIntegration, SubpixelRenderParams};
pub use subpixel_render::{SubpixelMetrics, SubpixelMode, SubpixelRenderManager, WindowType};
pub use texture_pool::TexturePool;
pub(crate) use vsync::VsyncMethod;
pub(crate) use wallpaper_common::{
    WallpaperImageData, WallpaperMode, compute_wallpaper_rect, parse_wallpaper_mode,
    resolve_wallpaper_for_tag,
};
pub(crate) use wobbly::WobblyState;
use types::*;

use glow::HasContext;
use crate::backend::x11::compositor_common::{
    X11BootstrapOps, X11CompositeRedirectOps, X11ConnectionOps, X11PresentOps, X11RandrOps,
    X11TextureSourceOps, X11WindowResourceOps,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc;

use math::ortho;
use transitions_common::TransitionMode;

// ---------------------------------------------------------------------------
// Blur quality auto-downgrade (Phase 2.2)
// ---------------------------------------------------------------------------

pub(crate) use crate::renderer::types::BlurQuality;

// ---------------------------------------------------------------------------
// Phase 3.3: Window open ripple
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Compositor
// ---------------------------------------------------------------------------

pub(crate) trait CompositorConnection:
    X11BootstrapOps
    + X11ConnectionOps
    + X11CompositeRedirectOps
    + X11PresentOps
    + X11RandrOps
    + X11TextureSourceOps
    + X11WindowResourceOps
    + Send
    + Sync
    + 'static
{
}

impl<T> CompositorConnection for T
where
    T: X11BootstrapOps
        + X11ConnectionOps
        + X11CompositeRedirectOps
        + X11PresentOps
        + X11RandrOps
        + X11TextureSourceOps
        + X11WindowResourceOps
        + Send
        + Sync
        + 'static,
{
}

pub(crate) struct Compositor<C>
where
    C: CompositorConnection,
{
    conn: Arc<C>,
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
    present_mgr: Option<Box<dyn PresentController>>,

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
    partial_damage_enabled: bool,
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
    annotation_strokes: Vec<annotations_common::AnnotationStroke>,
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
unsafe impl<C: CompositorConnection> Send for Compositor<C> {}

impl<C: CompositorConnection> Drop for Compositor<C> {
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
        let _ = self.conn.destroy_window_resource(self.cm_selection_owner);
        // Undo the MANUAL redirect so the X server renders windows normally again
        let _ = self.conn.unredirect_subwindows_manual(self.root);
        let _ = self.conn.release_overlay_window(self.overlay_window);
        let _ = self.conn.flush_x11();
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
