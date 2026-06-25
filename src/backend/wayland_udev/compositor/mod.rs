// ---------------------------------------------------------------------------
// Wayland udev backend compositor - GPU-accelerated composition with effects
// ---------------------------------------------------------------------------

pub mod shaders;
#[cfg(test)]
mod headless_render;
mod render;
mod effects;
mod transitions;
mod blur;
mod postprocess;
mod overview;
mod expose;
mod config;
mod damage;
mod wallpaper;
mod rules;
mod font;
#[allow(dead_code, unreachable_pub)]
mod texture_pool;
#[allow(dead_code, unreachable_pub)]
mod render_stats;
#[allow(dead_code, unreachable_pub)]
mod dirty_region;
#[allow(dead_code, unreachable_pub)]
mod per_monitor;
#[allow(dead_code, unreachable_pub)]
mod frame_rate;
#[allow(dead_code, unreachable_pub)]
mod power_saving;
#[allow(dead_code, unreachable_pub)]
mod predictive_render;
#[allow(dead_code, unreachable_pub)]
mod pixel_buffer_pool;
#[allow(dead_code, unreachable_pub)]
mod profiler;
#[allow(dead_code, unreachable_pub)]
mod perf_metrics;
#[allow(dead_code, unreachable_pub)]
mod cache_warmup;
#[allow(dead_code, unreachable_pub)]
mod direct_scanout;
#[allow(dead_code, unreachable_pub)]
mod gpu_fence_sync;
#[allow(dead_code, unreachable_pub)]
mod pbo_uploader;
#[allow(dead_code, unreachable_pub)]
mod render_batcher;
#[allow(dead_code, unreachable_pub)]
mod shader_cache;
#[allow(dead_code, unreachable_pub)]
mod recording;
#[allow(dead_code, unreachable_pub)]
mod shader_hot_reload;
#[allow(dead_code, unreachable_pub)]
mod audio_sync;
#[allow(dead_code, unreachable_pub)]
mod subpixel_render;
#[allow(dead_code, unreachable_pub)]
mod presentation_timing;

use smithay::backend::renderer::gles::ffi;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::ffi::CString;
use std::path::PathBuf;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Matrix math
// ---------------------------------------------------------------------------

/// Orthographic projection matrix (column-major for OpenGL).
pub(crate) fn ortho(l: f32, r: f32, b: f32, t: f32) -> [f32; 16] {
    let near = -1.0f32;
    let far = 1.0f32;
    let tx = -(r + l) / (r - l);
    let ty = -(t + b) / (t - b);
    let tz = -(far + near) / (far - near);
    #[rustfmt::skip]
    let m = [
        2.0 / (r - l), 0.0,            0.0,                 0.0,
        0.0,           2.0 / (t - b),   0.0,                 0.0,
        0.0,           0.0,            -2.0 / (far - near),  0.0,
        tx,            ty,              tz,                  1.0,
    ];
    m
}

// ---------------------------------------------------------------------------
// Shader program helper
// ---------------------------------------------------------------------------

/// Compile a vertex + fragment shader pair and link them into a program.
pub(crate) unsafe fn create_program(
    gl: &ffi::Gles2,
    vs_src: &str,
    fs_src: &str,
) -> Result<u32, String> {
    unsafe {
        let vs = gl.CreateShader(ffi::VERTEX_SHADER);
        let vs_cstr = CString::new(vs_src).map_err(|e| format!("VS CString: {}", e))?;
        let vs_ptr = vs_cstr.as_ptr();
        gl.ShaderSource(vs, 1, &vs_ptr, std::ptr::null());
        gl.CompileShader(vs);

        let mut status = 0i32;
        gl.GetShaderiv(vs, ffi::COMPILE_STATUS, &mut status);
        if status == 0 {
            let mut len = 0i32;
            gl.GetShaderiv(vs, ffi::INFO_LOG_LENGTH, &mut len);
            let mut buf = vec![0u8; len as usize];
            gl.GetShaderInfoLog(vs, len, std::ptr::null_mut(), buf.as_mut_ptr() as *mut _);
            gl.DeleteShader(vs);
            return Err(format!(
                "Vertex shader compile error: {}",
                String::from_utf8_lossy(&buf)
            ));
        }

        let fs = gl.CreateShader(ffi::FRAGMENT_SHADER);
        let fs_cstr = CString::new(fs_src).map_err(|e| format!("FS CString: {}", e))?;
        let fs_ptr = fs_cstr.as_ptr();
        gl.ShaderSource(fs, 1, &fs_ptr, std::ptr::null());
        gl.CompileShader(fs);

        gl.GetShaderiv(fs, ffi::COMPILE_STATUS, &mut status);
        if status == 0 {
            let mut len = 0i32;
            gl.GetShaderiv(fs, ffi::INFO_LOG_LENGTH, &mut len);
            let mut buf = vec![0u8; len as usize];
            gl.GetShaderInfoLog(fs, len, std::ptr::null_mut(), buf.as_mut_ptr() as *mut _);
            gl.DeleteShader(vs);
            gl.DeleteShader(fs);
            return Err(format!(
                "Fragment shader compile error: {}",
                String::from_utf8_lossy(&buf)
            ));
        }

        let program = gl.CreateProgram();
        gl.AttachShader(program, vs);
        gl.AttachShader(program, fs);
        gl.LinkProgram(program);

        gl.GetProgramiv(program, ffi::LINK_STATUS, &mut status);
        if status == 0 {
            let mut len = 0i32;
            gl.GetProgramiv(program, ffi::INFO_LOG_LENGTH, &mut len);
            let mut buf = vec![0u8; len as usize];
            gl.GetProgramInfoLog(program, len, std::ptr::null_mut(), buf.as_mut_ptr() as *mut _);
            gl.DeleteShader(vs);
            gl.DeleteShader(fs);
            gl.DeleteProgram(program);
            return Err(format!(
                "Program link error: {}",
                String::from_utf8_lossy(&buf)
            ));
        }

        gl.DetachShader(program, vs);
        gl.DetachShader(program, fs);
        gl.DeleteShader(vs);
        gl.DeleteShader(fs);

        Ok(program)
    }
}

// ---------------------------------------------------------------------------
// Uniform location structs
// ---------------------------------------------------------------------------

pub(crate) struct WindowUniforms {
    pub rect: i32,
    pub projection: i32,
    pub texture: i32,
    pub opacity: i32,
    pub radius: i32,
    pub size: i32,
    pub dim: i32,
    pub uv_rect: i32,
    pub ripple_progress: i32,
    pub ripple_amplitude: i32,
    // wp-color-management uniforms — locations may be -1 on older shader
    // drivers; the bind helpers no-op on -1 so missing values are safe.
    pub color_managed: i32,
    pub color_matrix: i32,
    pub decode_tf: i32,
    pub decode_gamma: i32,
    pub encode_tf: i32,
    pub encode_gamma: i32,
}

pub(crate) struct ShadowUniforms {
    pub rect: i32,
    pub projection: i32,
    pub shadow_color: i32,
    pub size: i32,
    pub radius: i32,
    pub spread: i32,
}

pub(crate) struct BlurUniforms {
    pub rect: i32,
    pub projection: i32,
    pub texture: i32,
    pub halfpixel: i32,
}

pub(crate) struct TemporalMixUniforms {
    pub rect: i32,
    pub projection: i32,
    pub current: i32,
    pub previous: i32,
    pub mix: i32,
}

pub(crate) struct BorderUniforms {
    pub rect: i32,
    pub projection: i32,
    pub border_color: i32,
    pub size: i32,
    pub radius: i32,
    pub border_width: i32,
}

pub(crate) struct PostprocessUniforms {
    pub texture: i32,
    pub color_temp: i32,
    pub saturation: i32,
    pub brightness: i32,
    pub contrast: i32,
    pub invert: i32,
    pub grayscale: i32,
    pub magnifier_enabled: i32,
    pub magnifier_center: i32,
    pub magnifier_radius: i32,
    pub magnifier_zoom: i32,
    pub colorblind_mode: i32,
    pub hdr_enabled: i32,
    pub hdr_peak_nits: i32,
    pub tone_mapping_method: i32,
}

#[allow(dead_code)]
pub(crate) struct TransitionUniforms {
    pub rect: i32,
    pub projection: i32,
    pub texture: i32,
    pub opacity: i32,
    pub uv_rect: i32,
}

#[allow(dead_code)]
pub(crate) struct CubeUniforms {
    pub mvp: i32,
    pub texture: i32,
    pub brightness: i32,
    pub uv_rect: i32,
    pub aspect: i32,
}

#[allow(dead_code)]
pub(crate) struct PortalUniforms {
    pub rect: i32,
    pub projection: i32,
    pub texture: i32,
    pub progress: i32,
    pub glow: i32,
    pub center: i32,
    pub uv_rect: i32,
}

#[allow(dead_code)]
pub(crate) struct TiltUniforms {
    pub rect: i32,
    pub projection: i32,
    pub texture: i32,
    pub opacity: i32,
    pub radius: i32,
    pub size: i32,
    pub dim: i32,
    pub uv_rect: i32,
    pub tilt: i32,
    pub perspective: i32,
    pub grid_size: i32,
    pub light_dir: i32,
}

pub(crate) struct WobblyUniforms {
    pub rect: i32,
    pub projection: i32,
    pub texture: i32,
    pub opacity: i32,
    pub radius: i32,
    pub size: i32,
    pub dim: i32,
    pub uv_rect: i32,
    pub grid_offsets: i32,
    pub grid_n: i32,
}

#[allow(dead_code)]
pub(crate) struct GenieUniforms {
    pub rect: i32,
    pub projection: i32,
    pub texture: i32,
    pub opacity: i32,
    pub radius: i32,
    pub size: i32,
    pub dim: i32,
    pub uv_rect: i32,
    pub progress: i32,
    pub dock_pos: i32,
    pub grid_size: i32,
}

pub(crate) struct EdgeGlowUniforms {
    pub rect: i32,
    pub projection: i32,
    pub glow_color: i32,
    pub glow_width: i32,
    pub mouse: i32,
    pub screen_size: i32,
    pub time: i32,
}

// ---------------------------------------------------------------------------
// Blur FBO level
// ---------------------------------------------------------------------------

pub(crate) struct BlurFboLevel {
    pub fbo: u32,
    pub texture: u32,
    pub width: u32,
    pub height: u32,
}

// ---------------------------------------------------------------------------
// Wallpaper types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum WallpaperMode {
    Fill,
    Fit,
    Stretch,
    Center,
}

pub(crate) struct WallpaperImageData {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub mode: WallpaperMode,
}

pub(crate) struct MonitorWallpaper {
    pub mon_x: i32,
    pub mon_y: i32,
    pub mon_w: u32,
    pub mon_h: u32,
    pub texture: Option<u32>,
    pub mode: WallpaperMode,
    pub img_w: u32,
    pub img_h: u32,
    /// Currently-loaded wallpaper path (used to skip reloads when active tags
    /// change but the resolved wallpaper for this monitor stays the same).
    pub current_path: String,
}

// ---------------------------------------------------------------------------
// Blur quality
// ---------------------------------------------------------------------------

pub(crate) use crate::renderer::types::BlurQuality;

// ---------------------------------------------------------------------------
// Annotation types
// ---------------------------------------------------------------------------

pub(crate) struct AnnotationStroke {
    pub points: Vec<(f32, f32)>,
    pub color: [f32; 4],
    pub width: f32,
}

// ---------------------------------------------------------------------------
// Per-window state
// ---------------------------------------------------------------------------

#[allow(dead_code)]
pub(crate) struct WindowState {
    /// Raw GL texture imported from the Wayland surface.
    pub gl_texture: Option<u32>,
    pub width: u32,
    pub height: u32,
    pub has_alpha: bool,
    pub y_inverted: bool,
    pub fade_opacity: f32,
    pub fading_out: bool,
    pub anim_scale: f32,
    pub anim_scale_target: f32,
    pub wobbly: Option<WobblyState>,
    pub motion_trail: VecDeque<(i32, i32)>,
    pub opacity_override: Option<f32>,
    pub corner_radius_override: Option<f32>,
    pub frame_extents: [u32; 4],
    pub is_shaped: bool,
    pub is_fullscreen: bool,
    pub is_urgent: bool,
    pub is_pip: bool,
    pub is_frosted: bool,
    pub frosted_strength: f32,
    pub class_name: String,
    pub scale: f32,
    #[allow(dead_code)]
    pub audio_sync_target: Option<f32>,
    pub ripple_progress: f32,
    pub ripple_active: bool,
    /// UV sub-rect for content within the buffer: [u, v, w, h].
    /// Accounts for CSD geometry offset (shadows/decorations outside window geometry).
    /// Default [0,0,1,1] means full texture = content.
    pub content_uv: [f32; 4],
    /// Set when remove_window started a genie minimize animation; the
    /// WindowState is kept alive so the genie pass can sample its
    /// gl_texture, then removed by tick_genie when the animation completes.
    pub is_genie_minimizing: bool,
    /// wp-color-management transform to apply in the window fragment shader
    /// for this frame. `None` = identity / bypass. Refreshed each frame in
    /// `compositor_render_frame` from `(surface_params, output_params)`; the
    /// stored value is read once in the draw loop and then becomes stale —
    /// do not rely on its lifetime beyond a single frame.
    pub color_transform: Option<crate::backend::wayland_udev::color_pipeline::ColorTransform>,
}

/// Active genie minimize animation for one window (Wayland).
///
/// Unlike X11 we don't transfer ownership of the GL texture — the WindowState
/// stays in `self.windows` (with `is_genie_minimizing=true`) so its
/// EGL-imported `gl_texture` remains valid. `tick_genie` removes both the
/// animation entry and the WindowState when the animation completes.
#[allow(dead_code)]
pub(crate) struct GenieAnimation {
    pub window_id: u64,
    pub start: Instant,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub gl_texture: u32,
    pub has_alpha: bool,
}

// ---------------------------------------------------------------------------
// Wobbly windows state
// ---------------------------------------------------------------------------

pub(crate) struct WobblyState {
    pub grid_n: usize,
    pub offsets: Vec<[f32; 2]>,
    pub velocities: Vec<[f32; 2]>,
    pub dragging: bool,
    pub anchor_row: usize,
    pub anchor_col: usize,
}

// ---------------------------------------------------------------------------
// Transition mode
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum TransitionMode {
    None,
    Slide,
    Cube,
    Flip,
    Fade,
    Zoom,
    Stack,
    Blinds,
    CoverFlow,
    Helix,
    Portal,
}

// ---------------------------------------------------------------------------
// Expose entry
// ---------------------------------------------------------------------------

pub(crate) struct ExposeEntry {
    pub window_id: u64,
    pub orig_x: f32,
    pub orig_y: f32,
    pub orig_w: f32,
    pub orig_h: f32,
    pub target_x: f32,
    pub target_y: f32,
    pub target_w: f32,
    pub target_h: f32,
    pub current_x: f32,
    pub current_y: f32,
    pub current_w: f32,
    pub current_h: f32,
    pub is_hovered: bool,
}

// ---------------------------------------------------------------------------
// Overview entry
// ---------------------------------------------------------------------------

#[allow(dead_code)]
pub(crate) struct OverviewEntry {
    pub window_id: u64,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub focused: bool,
    #[allow(dead_code)]
    pub title: String,
}

// ---------------------------------------------------------------------------
// Particle system
// ---------------------------------------------------------------------------

pub(crate) struct Particle {
    pub x: f32,
    pub y: f32,
    pub vx: f32,
    pub vy: f32,
    pub color: [f32; 4],
    pub life: f32,
}

pub(crate) struct ParticleSystem {
    pub particles: Vec<Particle>,
    pub age: f32,
}

// ---------------------------------------------------------------------------
// Main compositor struct
// ---------------------------------------------------------------------------

#[allow(dead_code)]
pub(crate) struct WaylandCompositor {
    // Shader programs
    program: u32,
    shadow_program: u32,
    blur_down_program: u32,
    blur_up_program: u32,
    border_program: u32,
    postprocess_program: u32,
    transition_program: u32,
    cube_program: u32,
    portal_program: u32,
    edge_glow_program: u32,
    tilt_program: u32,
    wobbly_program: u32,
    genie_program: u32,
    particle_program: u32,
    overview_bg_program: u32,
    hud_program: u32,
    temporal_blur_mix_program: u32,

    // Uniform locations
    win_uniforms: WindowUniforms,
    shadow_uniforms: ShadowUniforms,
    blur_uniforms: BlurUniforms,
    border_uniforms: BorderUniforms,
    postprocess_uniforms: PostprocessUniforms,
    transition_uniforms: TransitionUniforms,
    cube_uniforms: CubeUniforms,
    portal_uniforms: PortalUniforms,
    tilt_uniforms: TiltUniforms,
    wobbly_uniforms: WobblyUniforms,
    #[allow(dead_code)]
    genie_uniforms: GenieUniforms,
    edge_glow_uniforms: EdgeGlowUniforms,

    // GL resources
    quad_vao: u32,
    output_fbo: u32,
    output_texture: u32,
    /// SOTA #2 Phase 2.1: FP16 (RGBA16F) intermediate target used when
    /// `behavior.scene_linear_compositing` is on. Allocated alongside
    /// output_fbo with the same dimensions and torn down/resized in
    /// lockstep. Zero when the gate is off — render path checks for
    /// this sentinel and falls back to the encoded-space pipeline.
    /// Phase 2.2 will wire the window-shader linear-output path and the
    /// final encode pass that reads from this texture.
    #[allow(dead_code)]
    linear_fbo: u32,
    #[allow(dead_code)]
    linear_texture: u32,
    scene_fbo: u32,
    scene_texture: u32,
    blur_fbos: Vec<BlurFboLevel>,
    postprocess_fbo: u32,
    postprocess_texture: u32,
    #[allow(dead_code)]
    transition_fbo: u32,
    transition_texture: u32,
    particle_vao: u32,
    particle_vbo: u32,

    // Dimensions
    screen_w: u32,
    screen_h: u32,

    // Per-window state
    windows: HashMap<u64, WindowState>,

    // Set true while any WindowState carries a non-None color_transform.
    // The gate-off branch of the render path skips its per-window clear loop
    // when this is false, so a session that never enables color management
    // pays no per-frame cost.
    any_color_transform_active: bool,

    // Config
    corner_radius: f32,
    shadow_enabled: bool,
    shadow_radius: f32,
    shadow_offset: [f32; 2],
    shadow_color: [f32; 4],
    shadow_spread: f32,
    inactive_opacity: f32,
    active_opacity: f32,
    inactive_dim: f32,
    blur_enabled: bool,
    blur_strength: u32,
    fade_in_step: f32,
    fade_out_step: f32,

    // Animation feature flags (all default false; read from config.toml)
    fading_enabled: bool,
    window_animation_enabled: bool,
    edge_glow_enabled: bool,
    attention_animation_enabled: bool,
    wobbly_enabled: bool,
    motion_trail_enabled: bool,
    genie_minimize_enabled: bool,
    ripple_on_open_enabled: bool,
    focus_highlight_enabled: bool,
    particle_effects_enabled: bool,
    window_tilt_enabled: bool,

    // Animation state
    transition_active: bool,
    transition_start: Option<Instant>,
    transition_duration: Duration,
    transition_mode: TransitionMode,
    transition_direction: i32,

    // Overview (3D prism carousel)
    overview_active: bool,
    overview_opacity: f32,
    overview_entries: Vec<OverviewEntry>,
    overview_selection: Option<u64>,
    overview_monitor: (i32, i32, u32, u32),
    overview_rotation: f32,
    overview_target_rotation: f32,
    overview_title_textures: Vec<u32>,

    // Expose
    expose_active: bool,
    expose_opacity: f32,
    expose_entries: Vec<ExposeEntry>,

    // Snap preview
    snap_preview: Option<(f32, f32, f32, f32)>,
    snap_preview_opacity: f32,

    // Peek mode
    peek_active: bool,

    // Particles
    particle_systems: Vec<ParticleSystem>,

    // Edge glow
    edge_glow_active: bool,
    edge_glow_suppressed: bool,

    // Mouse position
    mouse_x: f32,
    mouse_y: f32,

    // Tilt
    tilt_x: f32,
    tilt_y: f32,
    tilt_target_x: f32,
    tilt_target_y: f32,

    // Post-processing state
    postprocess_active: bool,
    color_temperature: f32,
    saturation: f32,
    brightness: f32,
    contrast: f32,
    invert_colors: bool,
    grayscale: bool,
    magnifier_enabled: bool,
    magnifier_zoom: f32,
    magnifier_radius: f32,
    colorblind_mode: i32,
    hdr_enabled: bool,
    hdr_peak_nits: f32,
    tone_mapping_method: i32,

    // Debug HUD
    debug_hud_enabled: bool,

    // Optimization
    needs_render: bool,
    last_frame_time: Instant,
    frame_count: u64,
    fps: f32,

    // Previous frame scene for dirty tracking
    prev_scene: Vec<(u64, i32, i32, u32, u32)>,

    // Reusable per-frame scratch buffers (cleared+refilled each frame to avoid
    // per-frame heap allocation in the render hot path).
    scratch_curr_ids: HashSet<u64>,
    scratch_prev_geom: HashMap<u64, (i32, i32, u32, u32)>,
    scratch_scanout: Vec<(u64, direct_scanout::WindowScanoutInfo)>,
    scratch_wobbly_flat: Vec<f32>,

    // Dock position (for genie)
    dock_x: f32,
    dock_y: f32,

    // Active genie minimize animations
    pub(crate) genie_active: Vec<GenieAnimation>,

    // Window groups (tabs)
    window_groups: Vec<(u32, Vec<(u32, String, bool)>)>,

    // Monitors info
    monitors: Vec<(u32, i32, i32, u32, u32, u32)>,

    // Zoom to fit
    zoom_to_fit_window: Option<u32>,

    // Annotations
    annotation_active: bool,
    annotation_strokes: Vec<AnnotationStroke>,
    annotation_color: [f32; 4],
    annotation_line_width: f32,
    line_program: u32,
    line_uniform_projection: i32,
    line_uniform_color: i32,

    // Performance infrastructure
    dirty_region_tracker: dirty_region::DirtyRegionTracker,
    per_monitor_renderer: per_monitor::PerMonitorRenderer,
    frame_rate_limiter: frame_rate::FrameRateLimiter,
    adaptive_frame_rate: frame_rate::AdaptiveFrameRate,
    power_saving_mgr: power_saving::PowerSavingManager,
    predictive_render_mgr: predictive_render::PredictiveRenderManager,
    pixel_buffer_pool: pixel_buffer_pool::PixelBufferPool,
    frame_profiler: profiler::FrameProfiler,
    perf_metrics: perf_metrics::PerfMetrics,
    cache_warmup_mgr: cache_warmup::CacheWarmupManager,
    direct_scanout_mgr: direct_scanout::DirectScanoutManager,
    gpu_fence_sync_mgr: gpu_fence_sync::GpuFenceSyncManager,
    pbo_uploader: pbo_uploader::PBOUploader,
    gl_state_tracker: render_batcher::GLStateTracker,
    render_batcher: render_batcher::RenderBatcher,
    presentation_timing_mgr: presentation_timing::PresentationTimingManager,
    adaptive_scheduler: presentation_timing::AdaptiveFrameScheduler,

    // Feature modules
    recording: recording::RecordingState,
    shader_hot_reload: shader_hot_reload::ShaderHotReload,
    audio_sync_mgr: audio_sync::AudioSyncManager,
    subpixel_mgr: subpixel_render::SubpixelRenderManager,

    // --- Wallpaper ---
    wallpaper_texture: Option<u32>,
    wallpaper_mode: WallpaperMode,
    wallpaper_path: String,
    wallpaper_img_w: u32,
    wallpaper_img_h: u32,
    monitor_wallpapers: Vec<MonitorWallpaper>,
    pending_wallpaper: Option<std::sync::mpsc::Receiver<WallpaperImageData>>,
    pending_monitor_wallpapers: Vec<(usize, std::sync::mpsc::Receiver<WallpaperImageData>)>,
    wallpaper_crossfade: bool,
    wallpaper_crossfade_duration_ms: u64,
    old_wallpaper_texture: Option<u32>,
    old_wallpaper_img_w: u32,
    old_wallpaper_img_h: u32,
    old_wallpaper_mode: WallpaperMode,
    wallpaper_transition_start: Option<Instant>,

    // --- Per-window rules ---
    opacity_rules: Vec<(f32, String)>,
    corner_radius_rules: Vec<(f32, String)>,
    scale_rules: Vec<(f32, String)>,
    frosted_glass_rules: Vec<(String, f32)>,
    shadow_exclude: Vec<String>,
    blur_exclude: Vec<String>,
    rounded_corners_exclude: Vec<String>,
    detect_client_opacity: bool,
    blur_use_frame_extents: bool,

    // --- Fullscreen unredirect ---
    fullscreen_unredirect: bool,
    unredirected_window: Option<u64>,

    // --- Partial-damage (scissored) redraw ---
    // Experimental: when on, calm frames (no blur/animation/effects) only
    // re-shade the changed bounding box instead of the whole screen. Default
    // off; needs hardware verification before trusting (no display in CI).
    partial_damage_enabled: bool,
    // Force a full redraw on the next frame (set when the toggle flips or the
    // output is resized, so output_fbo is globally valid before partial frames).
    force_full_damage_next: bool,
    // Window ids whose texture content was updated since the last render_frame.
    content_dirty_ids: HashSet<u64>,
    // Previous frame's focused window, to damage focus-driven border/opacity changes.
    prev_focused: Option<u64>,

    // --- VRR ---
    is_game_window: HashMap<u64, bool>,
    vrr_active: bool,
    vrr_last_check: Instant,

    // --- Temporal blur ---
    temporal_blur_enabled: bool,
    temporal_blur_mix_ratio: f32,
    temporal_blur_mix_uniforms: TemporalMixUniforms,
    prev_blur_fbo: Option<(u32, u32)>,
    // Half-res scratch target for the temporal mix pass (mix output != either input).
    temporal_mix_fbo: Option<(u32, u32)>,
    // Reusable read-framebuffer for the temporal-blur history blit. Created once
    // (0 = not yet) and re-attached each frame instead of gen/deleting per frame.
    blur_blit_src_fbo: u32,
    // Last frame's window positions (id, x, y) for motion-aware mix attenuation.
    prev_motion_positions: Vec<(u64, i32, i32)>,
    prev_window_positions_hash: u64,
    temporal_blur_reuse_count: u64,
    temporal_blur_total_count: u64,

    // --- Blur quality ---
    blur_quality: BlurQuality,
    blur_quality_auto: bool,
    blur_quality_by_monitor: HashMap<u32, BlurQuality>,
    blur_strength_by_hz: Vec<(u32, u32)>,
    monitor_refresh_rates: HashMap<u32, u32>,
    last_gpu_load: u32,
    last_gpu_load_update: Instant,

    // --- Window tabs config ---
    window_tabs_enabled: bool,
    tab_bar_height: f32,
    tab_bar_color: [f32; 4],
    tab_active_color: [f32; 4],

    // --- Border config ---
    border_enabled: bool,
    border_width: f32,
    border_color_focused: [f32; 4],
    border_color_unfocused: [f32; 4],

    // --- Screenshot ---
    pending_screenshot: Option<std::path::PathBuf>,
    pending_screenshot_region: Option<(std::path::PathBuf, i32, i32, u32, u32)>,

    // --- Recording control ---
    pending_recording_start: Option<String>,
    pending_recording_stop: bool,

    // --- Debug HUD extended ---
    debug_hud_extended: bool,
    hud_text_texture: Option<u32>,
    hud_text_width: u32,
    hud_text_height: u32,
    hud_text_cache: String,
    compositor_start_time: Instant,

    // --- Animation parameters ---
    shadow_bottom_extra: f32,
    edge_glow_color: [f32; 4],
    edge_glow_width: f32,
    attention_color: [f32; 4],
    snap_preview_color: [f32; 4],
    snap_animation_duration_ms: u64,
    peek_exclude: Vec<String>,
    peek_opacity: f32,
    peek_start: Option<Instant>,
    expose_gap: f32,
    expose_start: Option<Instant>,
    particle_count: u32,
    particle_lifetime: f32,
    particle_gravity: f32,
    motion_trail_frames: u32,
    motion_trail_opacity: f32,
    tilt_speed: f32,
    tilt_grid: u32,
    wobbly_stiffness: f32,
    wobbly_damping: f32,
    wobbly_restore_stiffness: f32,
    wobbly_grid_size: u32,
    genie_duration_ms: u64,
    ripple_duration: f32,
    ripple_amplitude: f32,
    focus_highlight_color: [f32; 4],
    focus_highlight_duration_ms: u64,
    focus_highlight_start: Option<(u64, Instant)>,
    last_focused_window: Option<u64>,
    pip_border_color: [f32; 4],
    pip_border_width: f32,
    window_animation_scale: f32,

    // --- Transition per-monitor ---
    transition_mon: Option<(i32, i32, u32, u32)>,

    // --- Render stats ---
    render_stats: render_stats::RenderStats,
    texture_pool: texture_pool::TexturePool,
}

// ---------------------------------------------------------------------------
// Helper: get uniform location by name
// ---------------------------------------------------------------------------

unsafe fn get_uniform_loc(gl: &ffi::Gles2, program: u32, name: &str) -> i32 {
    unsafe {
        let cname = CString::new(name).unwrap();
        gl.GetUniformLocation(program, cname.as_ptr())
    }
}

// ---------------------------------------------------------------------------
// Helper: create a texture + FBO pair at given dimensions
// ---------------------------------------------------------------------------

const GL_RGB10_A2: u32 = 0x8059;
const GL_UNSIGNED_INT_2_10_10_10_REV: u32 = 0x8368;
const GL_RGBA16F: u32 = 0x881A;
const GL_HALF_FLOAT: u32 = 0x140B;

unsafe fn create_fbo_texture(gl: &ffi::Gles2, w: u32, h: u32) -> (u32, u32) {
    unsafe { create_fbo_texture_fmt(gl, w, h, ffi::RGBA8) }
}

unsafe fn create_fbo_texture_10bit(gl: &ffi::Gles2, w: u32, h: u32) -> (u32, u32) {
    unsafe { create_fbo_texture_fmt(gl, w, h, GL_RGB10_A2) }
}

/// Allocate a half-float RGBA FBO for scene-linear compositing. Linear
/// values can exceed [0, 1] (e.g. PQ peak-luminance scaling), so an 8-bit
/// or 10-bit unsigned-normalized format would clamp them. RGBA16F is the
/// GLES 3.0-portable storage with enough range and precision.
unsafe fn create_fbo_texture_fp16(gl: &ffi::Gles2, w: u32, h: u32) -> (u32, u32) {
    unsafe { create_fbo_texture_fmt(gl, w, h, GL_RGBA16F) }
}

unsafe fn create_fbo_texture_fmt(gl: &ffi::Gles2, w: u32, h: u32, internal_format: u32) -> (u32, u32) {
    unsafe {
        let mut tex = 0u32;
        gl.GenTextures(1, &mut tex);
        gl.BindTexture(ffi::TEXTURE_2D, tex);
        let pixel_type = if internal_format == GL_RGB10_A2 {
            GL_UNSIGNED_INT_2_10_10_10_REV
        } else if internal_format == GL_RGBA16F {
            GL_HALF_FLOAT
        } else {
            ffi::UNSIGNED_BYTE
        };
        gl.TexImage2D(
            ffi::TEXTURE_2D,
            0,
            internal_format as i32,
            w as i32,
            h as i32,
            0,
            ffi::RGBA,
            pixel_type,
            std::ptr::null(),
        );
        gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MIN_FILTER, ffi::LINEAR as i32);
        gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MAG_FILTER, ffi::LINEAR as i32);
        gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_WRAP_S, ffi::CLAMP_TO_EDGE as i32);
        gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_WRAP_T, ffi::CLAMP_TO_EDGE as i32);

        let mut fbo = 0u32;
        gl.GenFramebuffers(1, &mut fbo);
        gl.BindFramebuffer(ffi::FRAMEBUFFER, fbo);
        gl.FramebufferTexture2D(
            ffi::FRAMEBUFFER,
            ffi::COLOR_ATTACHMENT0,
            ffi::TEXTURE_2D,
            tex,
            0,
        );

        let status = gl.CheckFramebufferStatus(ffi::FRAMEBUFFER);
        if status != ffi::FRAMEBUFFER_COMPLETE {
            log::warn!(
                "[udev/compositor] incomplete FBO (status=0x{status:x}) for {w}x{h} internal_format=0x{internal_format:x}; rendering to it will be blank"
            );
        }

        (fbo, tex)
    }
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

impl WaylandCompositor {
    pub(crate) unsafe fn new(
        gl: &ffi::Gles2,
        screen_w: u32,
        screen_h: u32,
        hdr_10bit: bool,
    ) -> Result<Self, String> {
        unsafe {
        let program = create_program(gl, shaders::VERTEX_SHADER, shaders::FRAGMENT_SHADER)?;
        let shadow_program =
            create_program(gl, shaders::VERTEX_SHADER, shaders::SHADOW_FRAGMENT_SHADER)?;
        let blur_down_program =
            create_program(gl, shaders::BLUR_DOWN_VERTEX, shaders::BLUR_DOWN_FRAGMENT)?;
        let blur_up_program =
            create_program(gl, shaders::BLUR_DOWN_VERTEX, shaders::BLUR_UP_FRAGMENT)?;
        let border_program =
            create_program(gl, shaders::VERTEX_SHADER, shaders::BORDER_FRAGMENT_SHADER)?;
        let postprocess_program =
            create_program(gl, shaders::VERTEX_SHADER, shaders::POSTPROCESS_FRAGMENT_SHADER)?;
        let transition_program =
            create_program(gl, shaders::VERTEX_SHADER, shaders::TRANSITION_FRAGMENT_SHADER)?;
        let cube_program =
            create_program(gl, shaders::CUBE_VERTEX_SHADER, shaders::CUBE_FRAGMENT_SHADER)?;
        let portal_program =
            create_program(gl, shaders::VERTEX_SHADER, shaders::PORTAL_FRAGMENT_SHADER)?;
        let edge_glow_program =
            create_program(gl, shaders::VERTEX_SHADER, shaders::EDGE_GLOW_FRAGMENT_SHADER)?;
        let tilt_program =
            create_program(gl, shaders::TILT_VERTEX_SHADER, shaders::TILT_FRAGMENT_SHADER)?;
        let wobbly_program =
            create_program(gl, shaders::WOBBLY_VERTEX_SHADER, shaders::FRAGMENT_SHADER)?;
        let genie_program =
            create_program(gl, shaders::GENIE_VERTEX_SHADER, shaders::FRAGMENT_SHADER)?;
        let particle_program =
            create_program(gl, shaders::PARTICLE_VERTEX_SHADER, shaders::PARTICLE_FRAGMENT_SHADER)?;
        let overview_bg_program =
            create_program(gl, shaders::VERTEX_SHADER, shaders::OVERVIEW_BG_FRAGMENT_SHADER)?;
        let hud_program =
            create_program(gl, shaders::VERTEX_SHADER, shaders::HUD_FRAGMENT_SHADER)?;
        let temporal_blur_mix_program =
            create_program(gl, shaders::TEMPORAL_BLUR_MIX_VERTEX, shaders::TEMPORAL_BLUR_MIX_FRAGMENT)?;
        let temporal_blur_mix_uniforms = TemporalMixUniforms {
            rect: get_uniform_loc(gl, temporal_blur_mix_program, "u_rect"),
            projection: get_uniform_loc(gl, temporal_blur_mix_program, "u_projection"),
            current: get_uniform_loc(gl, temporal_blur_mix_program, "u_current_blur"),
            previous: get_uniform_loc(gl, temporal_blur_mix_program, "u_previous_blur"),
            mix: get_uniform_loc(gl, temporal_blur_mix_program, "u_temporal_mix"),
        };
        let line_program =
            create_program(gl, shaders::LINE_VERTEX_SHADER, shaders::LINE_FRAGMENT_SHADER)?;
        let line_uniform_projection = get_uniform_loc(gl, line_program, "u_projection");
        let line_uniform_color = get_uniform_loc(gl, line_program, "u_color");

        // ----- Get uniform locations -----
        let win_uniforms = WindowUniforms {
            rect: get_uniform_loc(gl, program, "u_rect"),
            projection: get_uniform_loc(gl, program, "u_projection"),
            texture: get_uniform_loc(gl, program, "u_texture"),
            opacity: get_uniform_loc(gl, program, "u_opacity"),
            radius: get_uniform_loc(gl, program, "u_radius"),
            size: get_uniform_loc(gl, program, "u_size"),
            dim: get_uniform_loc(gl, program, "u_dim"),
            uv_rect: get_uniform_loc(gl, program, "u_uv_rect"),
            ripple_progress: get_uniform_loc(gl, program, "u_ripple_progress"),
            ripple_amplitude: get_uniform_loc(gl, program, "u_ripple_amplitude"),
            color_managed: get_uniform_loc(gl, program, "u_color_managed"),
            color_matrix: get_uniform_loc(gl, program, "u_color_matrix"),
            decode_tf: get_uniform_loc(gl, program, "u_decode_tf"),
            decode_gamma: get_uniform_loc(gl, program, "u_decode_gamma"),
            encode_tf: get_uniform_loc(gl, program, "u_encode_tf"),
            encode_gamma: get_uniform_loc(gl, program, "u_encode_gamma"),
        };

        let shadow_uniforms = ShadowUniforms {
            rect: get_uniform_loc(gl, shadow_program, "u_rect"),
            projection: get_uniform_loc(gl, shadow_program, "u_projection"),
            shadow_color: get_uniform_loc(gl, shadow_program, "u_shadow_color"),
            size: get_uniform_loc(gl, shadow_program, "u_size"),
            radius: get_uniform_loc(gl, shadow_program, "u_radius"),
            spread: get_uniform_loc(gl, shadow_program, "u_spread"),
        };

        let blur_uniforms = BlurUniforms {
            rect: get_uniform_loc(gl, blur_down_program, "u_rect"),
            projection: get_uniform_loc(gl, blur_down_program, "u_projection"),
            texture: get_uniform_loc(gl, blur_down_program, "u_texture"),
            halfpixel: get_uniform_loc(gl, blur_down_program, "u_halfpixel"),
        };

        let border_uniforms = BorderUniforms {
            rect: get_uniform_loc(gl, border_program, "u_rect"),
            projection: get_uniform_loc(gl, border_program, "u_projection"),
            border_color: get_uniform_loc(gl, border_program, "u_border_color"),
            size: get_uniform_loc(gl, border_program, "u_size"),
            radius: get_uniform_loc(gl, border_program, "u_radius"),
            border_width: get_uniform_loc(gl, border_program, "u_border_width"),
        };

        let postprocess_uniforms = PostprocessUniforms {
            texture: get_uniform_loc(gl, postprocess_program, "u_texture"),
            color_temp: get_uniform_loc(gl, postprocess_program, "u_color_temp"),
            saturation: get_uniform_loc(gl, postprocess_program, "u_saturation"),
            brightness: get_uniform_loc(gl, postprocess_program, "u_brightness"),
            contrast: get_uniform_loc(gl, postprocess_program, "u_contrast"),
            invert: get_uniform_loc(gl, postprocess_program, "u_invert"),
            grayscale: get_uniform_loc(gl, postprocess_program, "u_grayscale"),
            magnifier_enabled: get_uniform_loc(gl, postprocess_program, "u_magnifier_enabled"),
            magnifier_center: get_uniform_loc(gl, postprocess_program, "u_magnifier_center"),
            magnifier_radius: get_uniform_loc(gl, postprocess_program, "u_magnifier_radius"),
            magnifier_zoom: get_uniform_loc(gl, postprocess_program, "u_magnifier_zoom"),
            colorblind_mode: get_uniform_loc(gl, postprocess_program, "u_colorblind_mode"),
            hdr_enabled: get_uniform_loc(gl, postprocess_program, "u_hdr_enabled"),
            hdr_peak_nits: get_uniform_loc(gl, postprocess_program, "u_hdr_peak_nits"),
            tone_mapping_method: get_uniform_loc(gl, postprocess_program, "u_tone_mapping_method"),
        };

        let transition_uniforms = TransitionUniforms {
            rect: get_uniform_loc(gl, transition_program, "u_rect"),
            projection: get_uniform_loc(gl, transition_program, "u_projection"),
            texture: get_uniform_loc(gl, transition_program, "u_texture"),
            opacity: get_uniform_loc(gl, transition_program, "u_opacity"),
            uv_rect: get_uniform_loc(gl, transition_program, "u_uv_rect"),
        };

        let cube_uniforms = CubeUniforms {
            mvp: get_uniform_loc(gl, cube_program, "u_mvp"),
            texture: get_uniform_loc(gl, cube_program, "u_texture"),
            brightness: get_uniform_loc(gl, cube_program, "u_brightness"),
            uv_rect: get_uniform_loc(gl, cube_program, "u_uv_rect"),
            aspect: get_uniform_loc(gl, cube_program, "u_aspect"),
        };

        let portal_uniforms = PortalUniforms {
            rect: get_uniform_loc(gl, portal_program, "u_rect"),
            projection: get_uniform_loc(gl, portal_program, "u_projection"),
            texture: get_uniform_loc(gl, portal_program, "u_texture"),
            progress: get_uniform_loc(gl, portal_program, "u_progress"),
            glow: get_uniform_loc(gl, portal_program, "u_glow"),
            center: get_uniform_loc(gl, portal_program, "u_center"),
            uv_rect: get_uniform_loc(gl, portal_program, "u_uv_rect"),
        };

        let tilt_uniforms = TiltUniforms {
            rect: get_uniform_loc(gl, tilt_program, "u_rect"),
            projection: get_uniform_loc(gl, tilt_program, "u_projection"),
            texture: get_uniform_loc(gl, tilt_program, "u_texture"),
            opacity: get_uniform_loc(gl, tilt_program, "u_opacity"),
            radius: get_uniform_loc(gl, tilt_program, "u_radius"),
            size: get_uniform_loc(gl, tilt_program, "u_size"),
            dim: get_uniform_loc(gl, tilt_program, "u_dim"),
            uv_rect: get_uniform_loc(gl, tilt_program, "u_uv_rect"),
            tilt: get_uniform_loc(gl, tilt_program, "u_tilt"),
            perspective: get_uniform_loc(gl, tilt_program, "u_perspective"),
            grid_size: get_uniform_loc(gl, tilt_program, "u_grid_size"),
            light_dir: get_uniform_loc(gl, tilt_program, "u_light_dir"),
        };

        let wobbly_uniforms = WobblyUniforms {
            rect: get_uniform_loc(gl, wobbly_program, "u_rect"),
            projection: get_uniform_loc(gl, wobbly_program, "u_projection"),
            texture: get_uniform_loc(gl, wobbly_program, "u_texture"),
            opacity: get_uniform_loc(gl, wobbly_program, "u_opacity"),
            radius: get_uniform_loc(gl, wobbly_program, "u_radius"),
            size: get_uniform_loc(gl, wobbly_program, "u_size"),
            dim: get_uniform_loc(gl, wobbly_program, "u_dim"),
            uv_rect: get_uniform_loc(gl, wobbly_program, "u_uv_rect"),
            grid_offsets: get_uniform_loc(gl, wobbly_program, "u_grid_offsets"),
            grid_n: get_uniform_loc(gl, wobbly_program, "u_grid_n"),
        };

        let genie_uniforms = GenieUniforms {
            rect: get_uniform_loc(gl, genie_program, "u_rect"),
            projection: get_uniform_loc(gl, genie_program, "u_projection"),
            texture: get_uniform_loc(gl, genie_program, "u_texture"),
            opacity: get_uniform_loc(gl, genie_program, "u_opacity"),
            radius: get_uniform_loc(gl, genie_program, "u_radius"),
            size: get_uniform_loc(gl, genie_program, "u_size"),
            dim: get_uniform_loc(gl, genie_program, "u_dim"),
            uv_rect: get_uniform_loc(gl, genie_program, "u_uv_rect"),
            progress: get_uniform_loc(gl, genie_program, "u_progress"),
            dock_pos: get_uniform_loc(gl, genie_program, "u_dock_pos"),
            grid_size: get_uniform_loc(gl, genie_program, "u_grid_size"),
        };

        let edge_glow_uniforms = EdgeGlowUniforms {
            rect: get_uniform_loc(gl, edge_glow_program, "u_rect"),
            projection: get_uniform_loc(gl, edge_glow_program, "u_projection"),
            glow_color: get_uniform_loc(gl, edge_glow_program, "u_glow_color"),
            glow_width: get_uniform_loc(gl, edge_glow_program, "u_glow_width"),
            mouse: get_uniform_loc(gl, edge_glow_program, "u_mouse"),
            screen_size: get_uniform_loc(gl, edge_glow_program, "u_screen_size"),
            time: get_uniform_loc(gl, edge_glow_program, "u_time"),
        };

        // ----- Create quad VAO (empty, using gl_VertexID) -----
        let mut quad_vao = 0u32;
        gl.GenVertexArrays(1, &mut quad_vao);

        // ----- Create output FBO + texture -----
        let (output_fbo, output_texture) = if hdr_10bit {
            create_fbo_texture_10bit(gl, screen_w, screen_h)
        } else {
            create_fbo_texture(gl, screen_w, screen_h)
        };

        // ----- SOTA #2 Phase 2.1: optional FP16 linear-scene FBO -----
        // Allocated only when behavior.scene_linear_compositing is on.
        // Zero/zero sentinel when off; the render path (Phase 2.2) checks
        // for linear_fbo != 0 to decide whether to take the linear path.
        let scene_linear_enabled = crate::config::CONFIG
            .load()
            .behavior()
            .scene_linear_compositing;
        let (linear_fbo, linear_texture) = if scene_linear_enabled {
            create_fbo_texture_fp16(gl, screen_w, screen_h)
        } else {
            (0, 0)
        };

        // When the output is 10-bit, keep the whole offscreen chain (scene
        // capture, blur, postprocess, transition) at 10-bit too — an 8-bit
        // intermediate would reintroduce banding before the final 10-bit blit.
        let mk_fbo = |w: u32, h: u32| {
            if hdr_10bit {
                create_fbo_texture_10bit(gl, w, h)
            } else {
                create_fbo_texture(gl, w, h)
            }
        };

        // ----- Create scene FBO + texture -----
        let (scene_fbo, scene_texture) = mk_fbo(screen_w, screen_h);

        // ----- Create blur FBO chain (6 levels, each half the previous) -----
        let mut blur_fbos = Vec::with_capacity(6);
        let mut bw = screen_w / 2;
        let mut bh = screen_h / 2;
        for _ in 0..6 {
            if bw < 1 {
                bw = 1;
            }
            if bh < 1 {
                bh = 1;
            }
            let (fbo, texture) = mk_fbo(bw, bh);
            blur_fbos.push(BlurFboLevel {
                fbo,
                texture,
                width: bw,
                height: bh,
            });
            bw /= 2;
            bh /= 2;
        }

        // ----- Create postprocess FBO + texture -----
        let (postprocess_fbo, postprocess_texture) = mk_fbo(screen_w, screen_h);

        // ----- Create transition FBO + texture -----
        let (transition_fbo, transition_texture) = mk_fbo(screen_w, screen_h);

        // ----- Create particle VAO + VBO -----
        let mut particle_vao = 0u32;
        gl.GenVertexArrays(1, &mut particle_vao);
        let mut particle_vbo = 0u32;
        gl.GenBuffers(1, &mut particle_vbo);

        // ----- Unbind -----
        gl.BindFramebuffer(ffi::FRAMEBUFFER, 0);
        gl.BindTexture(ffi::TEXTURE_2D, 0);

        let now = Instant::now();

        Ok(Self {
            // Shader programs
            program,
            shadow_program,
            blur_down_program,
            blur_up_program,
            border_program,
            postprocess_program,
            transition_program,
            cube_program,
            portal_program,
            edge_glow_program,
            tilt_program,
            wobbly_program,
            genie_program,
            particle_program,
            overview_bg_program,
            hud_program,
            temporal_blur_mix_program,

            // Uniform locations
            win_uniforms,
            shadow_uniforms,
            blur_uniforms,
            border_uniforms,
            postprocess_uniforms,
            transition_uniforms,
            cube_uniforms,
            portal_uniforms,
            tilt_uniforms,
            wobbly_uniforms,
            genie_uniforms,
            edge_glow_uniforms,

            // GL resources
            quad_vao,
            output_fbo,
            output_texture,
            linear_fbo,
            linear_texture,
            scene_fbo,
            scene_texture,
            blur_fbos,
            postprocess_fbo,
            postprocess_texture,
            transition_fbo,
            transition_texture,
            particle_vao,
            particle_vbo,

            // Dimensions
            screen_w,
            screen_h,

            // Per-window state
            windows: HashMap::new(),
            any_color_transform_active: false,

            // Config defaults — intentionally conservative; apply_config() reads config.toml
            corner_radius: 0.0,
            shadow_enabled: false,
            shadow_radius: 24.0,
            shadow_offset: [4.0, 4.0],
            shadow_color: [0.0, 0.0, 0.0, 0.5],
            shadow_spread: 20.0,
            inactive_opacity: 1.0,
            active_opacity: 1.0,
            inactive_dim: 1.0,
            blur_enabled: false,
            blur_strength: 3,
            fade_in_step: 0.03,
            fade_out_step: 0.03,

            // Animation feature flags — all off until config.toml enables them
            fading_enabled: false,
            window_animation_enabled: false,
            edge_glow_enabled: false,
            attention_animation_enabled: false,
            wobbly_enabled: false,
            motion_trail_enabled: false,
            genie_minimize_enabled: false,
            ripple_on_open_enabled: false,
            focus_highlight_enabled: false,
            particle_effects_enabled: false,
            window_tilt_enabled: false,

            // Animation state
            transition_active: false,
            transition_start: None,
            transition_duration: Duration::from_millis(300),
            transition_mode: TransitionMode::None,
            transition_direction: 0,

            // Overview
            overview_active: false,
            overview_opacity: 0.0,
            overview_entries: Vec::new(),
            overview_selection: None,
            overview_monitor: (0, 0, screen_w, screen_h),
            overview_rotation: 0.0,
            overview_target_rotation: 0.0,
            overview_title_textures: Vec::new(),

            // Expose
            expose_active: false,
            expose_opacity: 0.0,
            expose_entries: Vec::new(),

            // Snap preview
            snap_preview: None,
            snap_preview_opacity: 0.0,

            // Peek mode
            peek_active: false,

            // Particles
            particle_systems: Vec::new(),

            // Edge glow
            edge_glow_active: false,
            edge_glow_suppressed: false,

            // Mouse position
            mouse_x: 0.0,
            mouse_y: 0.0,

            // Tilt
            tilt_x: 0.0,
            tilt_y: 0.0,
            tilt_target_x: 0.0,
            tilt_target_y: 0.0,

            // Post-processing
            postprocess_active: false,
            color_temperature: 0.0,
            saturation: 1.0,
            brightness: 1.0,
            contrast: 1.0,
            invert_colors: false,
            grayscale: false,
            magnifier_enabled: false,
            magnifier_zoom: 2.0,
            magnifier_radius: 100.0,
            colorblind_mode: 0,
            hdr_enabled: hdr_10bit,
            hdr_peak_nits: 1000.0,
            tone_mapping_method: 0,

            // Debug HUD
            debug_hud_enabled: false,

            // Optimization
            needs_render: true,
            last_frame_time: now,
            frame_count: 0,
            fps: 0.0,
            prev_scene: Vec::new(),
            scratch_curr_ids: HashSet::new(),
            scratch_prev_geom: HashMap::new(),
            scratch_scanout: Vec::new(),
            scratch_wobbly_flat: Vec::new(),

            // Dock position
            dock_x: 0.0,
            dock_y: 0.0,

            // Genie animations
            genie_active: Vec::new(),

            // Window groups
            window_groups: Vec::new(),

            // Monitors
            monitors: Vec::new(),

            // Zoom to fit
            zoom_to_fit_window: None,

            // Annotations
            annotation_active: false,
            annotation_strokes: Vec::new(),
            annotation_color: [1.0, 0.0, 0.0, 1.0],
            annotation_line_width: 3.0,
            line_program,
            line_uniform_projection,
            line_uniform_color,

            // Performance infrastructure
            dirty_region_tracker: dirty_region::DirtyRegionTracker::new(screen_w, screen_h),
            per_monitor_renderer: per_monitor::PerMonitorRenderer::new(),
            frame_rate_limiter: frame_rate::FrameRateLimiter::new(60),
            adaptive_frame_rate: frame_rate::AdaptiveFrameRate::new(15, 60),
            power_saving_mgr: power_saving::PowerSavingManager::new(
                power_saving::PowerSavingConfig::default(),
            ),
            predictive_render_mgr: predictive_render::PredictiveRenderManager::new(),
            pixel_buffer_pool: pixel_buffer_pool::PixelBufferPool::new(),
            frame_profiler: profiler::FrameProfiler::new(),
            perf_metrics: perf_metrics::PerfMetrics::new(),
            cache_warmup_mgr: cache_warmup::CacheWarmupManager::new(),
            direct_scanout_mgr: direct_scanout::DirectScanoutManager::new(screen_w, screen_h),
            gpu_fence_sync_mgr: gpu_fence_sync::GpuFenceSyncManager::new(),
            pbo_uploader: pbo_uploader::PBOUploader::new(4 * 1024 * 1024, 4),
            gl_state_tracker: render_batcher::GLStateTracker::new(),
            render_batcher: render_batcher::RenderBatcher::new(),
            presentation_timing_mgr: presentation_timing::PresentationTimingManager::new(),
            adaptive_scheduler: presentation_timing::AdaptiveFrameScheduler::new(60),

            // Feature modules
            recording: recording::RecordingState::new(),
            shader_hot_reload: shader_hot_reload::ShaderHotReload::new(),
            audio_sync_mgr: audio_sync::AudioSyncManager::new(),
            subpixel_mgr: subpixel_render::SubpixelRenderManager::new(),

            // Wallpaper
            wallpaper_texture: None,
            wallpaper_mode: WallpaperMode::Fill,
            wallpaper_path: String::new(),
            wallpaper_img_w: 0,
            wallpaper_img_h: 0,
            monitor_wallpapers: Vec::new(),
            pending_wallpaper: None,
            pending_monitor_wallpapers: Vec::new(),
            wallpaper_crossfade: true,
            wallpaper_crossfade_duration_ms: 500,
            old_wallpaper_texture: None,
            old_wallpaper_img_w: 0,
            old_wallpaper_img_h: 0,
            old_wallpaper_mode: WallpaperMode::Fill,
            wallpaper_transition_start: None,

            // Per-window rules
            opacity_rules: Vec::new(),
            corner_radius_rules: Vec::new(),
            scale_rules: Vec::new(),
            frosted_glass_rules: Vec::new(),
            shadow_exclude: Vec::new(),
            blur_exclude: Vec::new(),
            rounded_corners_exclude: Vec::new(),
            detect_client_opacity: true,
            blur_use_frame_extents: false,

            // Fullscreen unredirect
            fullscreen_unredirect: false,
            unredirected_window: None,

            // Partial-damage redraw: on by default. The allow_partial gate
            // (render.rs) only engages it on calm frames (no animation, no blur,
            // no overview/peek/annotation) and the damage box is always a
            // superset of changed pixels, so a fully-correct output_fbo is
            // presented in full — stale pixels can't appear. Toggle off at
            // runtime with Mod1+Shift+d if a regression is seen on hardware.
            partial_damage_enabled: true,
            force_full_damage_next: true,
            content_dirty_ids: HashSet::new(),
            prev_focused: None,

            // VRR
            is_game_window: HashMap::new(),
            vrr_active: false,
            vrr_last_check: now,

            // Temporal blur (default-on; config may override via apply_config)
            temporal_blur_enabled: true,
            temporal_blur_mix_ratio: 0.8,
            temporal_blur_mix_uniforms,
            prev_blur_fbo: None,
            temporal_mix_fbo: None,
            blur_blit_src_fbo: 0,
            prev_motion_positions: Vec::new(),
            prev_window_positions_hash: 0,
            temporal_blur_reuse_count: 0,
            temporal_blur_total_count: 0,

            // Blur quality
            blur_quality: BlurQuality::Full,
            blur_quality_auto: false,
            blur_quality_by_monitor: HashMap::new(),
            blur_strength_by_hz: Vec::new(),
            monitor_refresh_rates: HashMap::new(),
            last_gpu_load: 0,
            last_gpu_load_update: now,

            // Window tabs
            window_tabs_enabled: false,
            tab_bar_height: 24.0,
            tab_bar_color: [0.2, 0.2, 0.2, 0.9],
            tab_active_color: [0.3, 0.5, 0.8, 1.0],

            // Border config
            border_enabled: true,
            border_width: 2.0,
            border_color_focused: [0.3, 0.6, 1.0, 0.8],
            border_color_unfocused: [0.3, 0.3, 0.3, 0.5],

            // Screenshot
            pending_screenshot: None,
            pending_screenshot_region: None,

            // Recording control
            pending_recording_start: None,
            pending_recording_stop: false,

            // Debug HUD extended
            debug_hud_extended: false,
            hud_text_texture: None,
            hud_text_width: 0,
            hud_text_height: 0,
            hud_text_cache: String::new(),
            compositor_start_time: now,

            // Animation parameters
            shadow_bottom_extra: 0.0,
            edge_glow_color: [0.3, 0.6, 1.0, 0.6],
            edge_glow_width: 20.0,
            attention_color: [1.0, 0.5, 0.0, 0.8],
            snap_preview_color: [0.3, 0.6, 1.0, 0.3],
            snap_animation_duration_ms: 200,
            peek_exclude: Vec::new(),
            peek_opacity: 0.0,
            peek_start: None,
            expose_gap: 20.0,
            expose_start: None,
            particle_count: 30,
            particle_lifetime: 1.0,
            particle_gravity: 400.0,
            motion_trail_frames: 5,
            motion_trail_opacity: 0.3,
            tilt_speed: 8.0,
            tilt_grid: 12,
            wobbly_stiffness: 600.0,
            wobbly_damping: 30.0,
            wobbly_restore_stiffness: 200.0,
            wobbly_grid_size: 6,
            genie_duration_ms: 300,
            ripple_duration: 0.4,
            ripple_amplitude: 0.03,
            focus_highlight_color: [0.3, 0.6, 1.0, 0.8],
            focus_highlight_duration_ms: 300,
            focus_highlight_start: None,
            last_focused_window: None,
            pip_border_color: [1.0, 0.8, 0.0, 0.9],
            pip_border_width: 3.0,
            window_animation_scale: 0.92,

            // Transition per-monitor
            transition_mon: None,

            // Render stats & texture pool
            render_stats: render_stats::RenderStats::new(),
            texture_pool: texture_pool::TexturePool::new(),
        })
        }
    }
}

// ---------------------------------------------------------------------------
// Drop - GL resources are released when the EGL context is destroyed.
// We cannot call GL functions in Drop because we don't have access to the
// current EGL/GL context at destruction time. The GPU driver reclaims all
// resources when the EGL context is destroyed, so this is safe.
// ---------------------------------------------------------------------------

impl Drop for WaylandCompositor {
    fn drop(&mut self) {
        // Intentionally empty: GL resources (programs, textures, FBOs, VAOs, VBOs)
        // are owned by the EGL context and will be cleaned up when that context
        // is destroyed. Calling GL functions here would require a current context
        // which we cannot guarantee.
    }
}

// ---------------------------------------------------------------------------
// Render state queries
// ---------------------------------------------------------------------------

impl WaylandCompositor {
    /// Returns true if the compositor has pending work that requires a new frame.
    pub(crate) fn needs_render(&self) -> bool {
        self.needs_render
    }

    /// Clear the needs_render flag after a frame has been rendered.
    #[allow(dead_code)]
    pub(crate) fn clear_needs_render(&mut self) {
        self.needs_render = false;
    }

    /// Raw GL texture ID of the composited output (color attachment of output_fbo).
    pub(crate) fn output_texture_id(&self) -> u32 {
        self.output_texture
    }

    /// Current screen dimensions.
    pub(crate) fn screen_size(&self) -> (u32, u32) {
        (self.screen_w, self.screen_h)
    }

    /// Whether experimental partial-damage (scissored) redraw is enabled.
    pub(crate) fn partial_damage_enabled(&self) -> bool {
        self.partial_damage_enabled
    }

    /// Toggle experimental partial-damage redraw. Forces one full redraw on the
    /// next frame so output_fbo is globally valid before partial frames resume.
    pub(crate) fn set_partial_damage(&mut self, on: bool) {
        if self.partial_damage_enabled != on {
            self.partial_damage_enabled = on;
            self.force_full_damage_next = true;
            self.needs_render = true;
        }
    }

    /// Feed a vblank presentation timestamp for frame pacing.
    pub(crate) fn on_vblank_presented(&mut self, presented_at: std::time::Instant) {
        let was_late = presented_at.elapsed() > std::time::Duration::from_millis(2);
        self.adaptive_scheduler.on_frame_presented(was_late);
    }

    /// Request recording start — deferred until next render_frame when GL is active.
    pub(crate) fn start_recording(&mut self, path: &str) {
        self.pending_recording_start = Some(path.to_string());
    }

    /// Request recording stop — deferred until next render_frame when GL is active.
    pub(crate) fn stop_recording(&mut self) {
        self.pending_recording_stop = true;
    }

    /// Notify audio timing for a window (feeds AudioSyncManager).
    pub(crate) fn notify_audio_timing(&mut self, window_id: u64, fps: f32, buffer_latency_ms: u32) {
        self.audio_sync_mgr.register_stream(window_id, fps, buffer_latency_ms);
    }

    /// Capture a scaled-down thumbnail of a window's texture.
    /// Returns (RGBA pixels, width, height) or None if the window has no texture.
    pub(crate) unsafe fn capture_thumbnail(
        &self,
        gl: &ffi::Gles2,
        window_id: u64,
        max_size: u32,
    ) -> Option<(Vec<u8>, u32, u32)> {
        let ws = self.windows.get(&window_id)?;
        let tex = ws.gl_texture?;
        if ws.width == 0 || ws.height == 0 {
            return None;
        }

        let (tw, th) = if ws.width > ws.height {
            let tw = max_size.min(ws.width);
            let th = (ws.height as f32 * tw as f32 / ws.width as f32) as u32;
            (tw.max(1), th.max(1))
        } else {
            let th = max_size.min(ws.height);
            let tw = (ws.width as f32 * th as f32 / ws.height as f32) as u32;
            (tw.max(1), th.max(1))
        };

        unsafe {
            let mut tmp_tex = 0u32;
            gl.GenTextures(1, &mut tmp_tex);
            gl.BindTexture(ffi::TEXTURE_2D, tmp_tex);
            gl.TexImage2D(
                ffi::TEXTURE_2D, 0, ffi::RGBA8 as i32,
                tw as i32, th as i32, 0,
                ffi::RGBA, ffi::UNSIGNED_BYTE, std::ptr::null(),
            );
            gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MIN_FILTER, ffi::LINEAR as i32);
            gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MAG_FILTER, ffi::LINEAR as i32);

            let mut tmp_fbo = 0u32;
            gl.GenFramebuffers(1, &mut tmp_fbo);
            gl.BindFramebuffer(ffi::FRAMEBUFFER, tmp_fbo);
            gl.FramebufferTexture2D(
                ffi::FRAMEBUFFER, ffi::COLOR_ATTACHMENT0, ffi::TEXTURE_2D, tmp_tex, 0,
            );

            gl.Viewport(0, 0, tw as i32, th as i32);
            gl.ClearColor(0.0, 0.0, 0.0, 0.0);
            gl.Clear(ffi::COLOR_BUFFER_BIT);

            gl.UseProgram(self.program);
            let projection = ortho(0.0, tw as f32, th as f32, 0.0);
            gl.UniformMatrix4fv(self.win_uniforms.projection, 1, ffi::FALSE as u8, projection.as_ptr());
            gl.Uniform1i(self.win_uniforms.texture, 0);
            gl.Uniform1f(self.win_uniforms.opacity, 1.0);
            gl.Uniform1f(self.win_uniforms.dim, 1.0);
            gl.Uniform1f(self.win_uniforms.radius, 0.0);
            gl.Uniform2f(self.win_uniforms.size, tw as f32, th as f32);
            gl.Uniform1f(self.win_uniforms.ripple_amplitude, 0.0);

            let [cu, cv, cuw, cuh] = ws.content_uv;
            if ws.y_inverted {
                gl.Uniform4f(self.win_uniforms.uv_rect, cu, cv + cuh, cuw, -cuh);
            } else {
                gl.Uniform4f(self.win_uniforms.uv_rect, cu, cv, cuw, cuh);
            }

            gl.Uniform4f(self.win_uniforms.rect, 0.0, 0.0, tw as f32, th as f32);

            gl.ActiveTexture(ffi::TEXTURE0);
            gl.BindTexture(ffi::TEXTURE_2D, tex);
            gl.BindVertexArray(self.quad_vao);
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);

            let buffer_size = (tw * th * 4) as usize;
            let mut pixels = vec![0u8; buffer_size];
            gl.ReadPixels(
                0, 0, tw as i32, th as i32,
                ffi::RGBA, ffi::UNSIGNED_BYTE,
                pixels.as_mut_ptr() as *mut _,
            );

            // Flip vertically (OpenGL reads bottom-up)
            let row_bytes = (tw * 4) as usize;
            for row in 0..(th as usize / 2) {
                let top = row * row_bytes;
                let bot = ((th as usize) - 1 - row) * row_bytes;
                for i in 0..row_bytes {
                    pixels.swap(top + i, bot + i);
                }
            }

            // Restore state
            gl.BindFramebuffer(ffi::FRAMEBUFFER, 0);
            gl.DeleteFramebuffers(1, &tmp_fbo);
            gl.DeleteTextures(1, &tmp_tex);
            gl.Viewport(0, 0, self.screen_w as i32, self.screen_h as i32);

            Some((pixels, tw, th))
        }
    }

    /// Diagnostic snapshot of the blur pipeline, surfaced via the
    /// `get_blur_status` IPC. Lets dual-monitor Hz selection + reuse rate be
    /// verified without HW.
    pub(crate) fn get_blur_status(&self) -> crate::backend::api::BlurStatus {
        let temporal_rate = if self.temporal_blur_total_count > 0 {
            100.0 * self.temporal_blur_reuse_count as f32 / self.temporal_blur_total_count as f32
        } else {
            0.0
        };
        let mut per_monitor_hz: Vec<(u32, u32)> = self
            .monitor_refresh_rates
            .iter()
            .map(|(&id, &hz)| (id, hz))
            .collect();
        per_monitor_hz.sort_by_key(|&(id, _)| id);
        let mut quality_by_monitor: Vec<(u32, String)> = self
            .blur_quality_by_monitor
            .iter()
            .map(|(&id, q)| (id, format!("{:?}", q)))
            .collect();
        quality_by_monitor.sort_by_key(|&(id, _)| id);
        crate::backend::api::BlurStatus {
            current_strength: self.blur_strength,
            temporal_enabled: self.temporal_blur_enabled,
            temporal_reuse_rate_pct: temporal_rate,
            hz_table: self.blur_strength_by_hz.clone(),
            per_monitor_hz,
            blur_quality_by_monitor: quality_by_monitor,
        }
    }

    /// Collect compositor metrics from all subsystems.
    pub(crate) fn get_metrics(&self) -> crate::backend::api::CompositorMetrics {
        let avg = self.perf_metrics.avg_frame_time().as_secs_f32() * 1000.0;
        let max = self.perf_metrics.max_frame_time().as_secs_f32() * 1000.0;
        let min = self.perf_metrics.min_frame_time().as_secs_f32() * 1000.0;
        let temporal_rate = if self.temporal_blur_total_count > 0 {
            100.0 * self.temporal_blur_reuse_count as f32 / self.temporal_blur_total_count as f32
        } else {
            0.0
        };
        let ds_stats = self.direct_scanout_mgr.stats();
        crate::backend::api::CompositorMetrics {
            fps: self.fps,
            frame_count: self.frame_count,
            avg_frame_time_ms: avg,
            max_frame_time_ms: max,
            min_frame_time_ms: min,
            gpu_load_percent: self.perf_metrics.gpu_load(),
            cpu_load_percent: self.perf_metrics.cpu_load(),
            draw_calls: 0,
            texture_memory_bytes: 0,
            blur_cache_hits: 0,
            blur_cache_misses: 0,
            blur_cache_hit_rate: 0.0,
            temporal_blur_reuse_count: self.temporal_blur_reuse_count,
            temporal_blur_total_count: self.temporal_blur_total_count,
            temporal_blur_reuse_rate: temporal_rate,
            dirty_regions_count: self.dirty_region_tracker.region_count(),
            dirty_fraction_percent: 0.0,
            window_count: self.windows.len(),
            blur_quality: format!("{:?}", self.blur_quality),
            vrr_enabled: self.vrr_active,
            vrr_active: self.vrr_active,
            current_refresh_rate: 0,
            input_latency_avg_ms: 0.0,
            input_latency_p50_ms: 0.0,
            input_latency_p95_ms: 0.0,
            input_latency_p99_ms: 0.0,
            direct_scanout_active: self.direct_scanout_mgr.is_active(),
            direct_scanout_count: ds_stats.scanout_count,
            direct_scanout_bypass_time_ms: ds_stats.bypass_time_ms,
            gl_state_changes_avoided: 0,
            profiling_enabled: self.frame_profiler.is_enabled(),
            dirty_region_merge_count: 0,
        }
    }

}

// ---------------------------------------------------------------------------
// Resize
// ---------------------------------------------------------------------------

impl WaylandCompositor {
    /// Recreate FBOs at the new screen dimensions.
    #[allow(dead_code)]
    pub(crate) unsafe fn resize(&mut self, gl: &ffi::Gles2, w: u32, h: u32) {
        if w == self.screen_w && h == self.screen_h {
            return;
        }

        self.screen_w = w;
        self.screen_h = h;
        // output_fbo is recreated below; its contents are undefined until a full
        // redraw, so partial-damage frames must not persist stale regions.
        self.force_full_damage_next = true;

        unsafe {
            gl.DeleteFramebuffers(1, &self.output_fbo);
            gl.DeleteTextures(1, &self.output_texture);
            gl.DeleteFramebuffers(1, &self.scene_fbo);
            gl.DeleteTextures(1, &self.scene_texture);
            gl.DeleteFramebuffers(1, &self.postprocess_fbo);
            gl.DeleteTextures(1, &self.postprocess_texture);
            gl.DeleteFramebuffers(1, &self.transition_fbo);
            gl.DeleteTextures(1, &self.transition_texture);

            for level in &self.blur_fbos {
                gl.DeleteFramebuffers(1, &level.fbo);
                gl.DeleteTextures(1, &level.texture);
            }

            let (output_fbo, output_texture) = if self.hdr_enabled {
                create_fbo_texture_10bit(gl, w, h)
            } else {
                create_fbo_texture(gl, w, h)
            };
            self.output_fbo = output_fbo;
            self.output_texture = output_texture;

            // Mirror the linear-scene FBO if it was active. Zero means the
            // gate was off at construction; we don't dynamically enable it
            // mid-session (that needs a backend re-init for shader-program
            // recompilation in Phase 2.2 anyway).
            if self.linear_fbo != 0 {
                gl.DeleteFramebuffers(1, &self.linear_fbo);
                gl.DeleteTextures(1, &self.linear_texture);
                let (lf, lt) = create_fbo_texture_fp16(gl, w, h);
                self.linear_fbo = lf;
                self.linear_texture = lt;
            }

            // Keep the offscreen chain at the same bit depth as on construction
            // (see new()): 10-bit when the output is 10-bit, else 8-bit. Without
            // this the chain silently reverts to 8-bit after any resize.
            let hdr_10bit = self.hdr_enabled;
            let mk_fbo = |w: u32, h: u32| {
                if hdr_10bit {
                    create_fbo_texture_10bit(gl, w, h)
                } else {
                    create_fbo_texture(gl, w, h)
                }
            };

            let (scene_fbo, scene_texture) = mk_fbo(w, h);
            self.scene_fbo = scene_fbo;
            self.scene_texture = scene_texture;

            self.blur_fbos.clear();
            let mut bw = w / 2;
            let mut bh = h / 2;
            for _ in 0..6 {
                if bw < 1 {
                    bw = 1;
                }
                if bh < 1 {
                    bh = 1;
                }
                let (fbo, texture) = mk_fbo(bw, bh);
                self.blur_fbos.push(BlurFboLevel {
                    fbo,
                    texture,
                    width: bw,
                    height: bh,
                });
                bw /= 2;
                bh /= 2;
            }

            let (postprocess_fbo, postprocess_texture) = mk_fbo(w, h);
            self.postprocess_fbo = postprocess_fbo;
            self.postprocess_texture = postprocess_texture;

            let (transition_fbo, transition_texture) = mk_fbo(w, h);
            self.transition_fbo = transition_fbo;
            self.transition_texture = transition_texture;

            // Temporal-blur scratch buffers are half-res and lazily allocated;
            // drop them so they are recreated at the new size on next use.
            // (Leaving them stale would mismatch blur_fbos[0] and leak GL memory.)
            if let Some((fbo, tex)) = self.prev_blur_fbo.take() {
                gl.DeleteFramebuffers(1, &fbo);
                gl.DeleteTextures(1, &tex);
            }
            if let Some((fbo, tex)) = self.temporal_mix_fbo.take() {
                gl.DeleteFramebuffers(1, &fbo);
                gl.DeleteTextures(1, &tex);
            }
            self.prev_motion_positions.clear();
            self.prev_window_positions_hash = 0;

            gl.BindFramebuffer(ffi::FRAMEBUFFER, 0);
            gl.BindTexture(ffi::TEXTURE_2D, 0);
        }

        self.needs_render = true;
        self.overview_monitor = (0, 0, w, h);
    }
}
