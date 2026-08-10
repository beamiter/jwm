// ---------------------------------------------------------------------------
// Wayland udev backend compositor - GPU-accelerated composition with effects
// ---------------------------------------------------------------------------

#[allow(dead_code, unreachable_pub)]
mod audio_sync;
mod blur;
#[allow(dead_code, unreachable_pub)]
mod cache_warmup;
mod config;
mod damage;
#[allow(dead_code, unreachable_pub)]
mod direct_scanout;
#[allow(dead_code, unreachable_pub)]
mod dirty_region;
mod effects;
mod expose;
#[allow(dead_code, unreachable_pub)]
mod frame_rate;
#[allow(dead_code, unreachable_pub)]
mod gpu_fence_sync;
#[cfg(test)]
mod headless_render;
mod minimized_thumbnail;
mod overview;
#[allow(dead_code, unreachable_pub)]
mod pbo_uploader;
#[allow(dead_code, unreachable_pub)]
mod per_monitor;
#[allow(dead_code, unreachable_pub)]
mod perf_metrics;
#[allow(dead_code, unreachable_pub)]
mod pixel_buffer_pool;
mod postprocess;
#[allow(dead_code, unreachable_pub)]
mod power_saving;
#[allow(dead_code, unreachable_pub)]
mod predictive_render;
#[allow(dead_code, unreachable_pub)]
mod presentation_timing;
#[allow(dead_code, unreachable_pub)]
mod profiler;
#[allow(dead_code, unreachable_pub)]
mod recording;
mod render;
#[allow(dead_code, unreachable_pub)]
mod render_batcher;
#[allow(dead_code, unreachable_pub)]
mod render_stats;
mod rules;
mod screenshot_readback;
mod screenshot_toolbar;
#[allow(dead_code, unreachable_pub)]
mod shader_cache;
#[allow(dead_code, unreachable_pub)]
mod shader_hot_reload;
pub mod shaders;
#[allow(dead_code, unreachable_pub)]
mod subpixel_render;
#[allow(dead_code, unreachable_pub)]
mod texture_pool;
mod transitions;
mod wallpaper;

use smithay::backend::renderer::gles::{GlesTexture, ffi};
use std::collections::HashMap;
use std::collections::HashSet;
use std::ffi::CString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::backend::api::CompositorRect;
use crate::backend::compositor_common::capture::flip_rgba_vertical;
use crate::backend::compositor_common::effects::MotionTrail;
use crate::backend::compositor_common::genie::{GenieDirection, PreviewDirection};
use crate::backend::compositor_common::math;
use crate::backend::compositor_common::minimized_thumbnail::snapshot_shader_opacity;
use crate::backend::compositor_common::rules::{CornerRadiusRule, OpacityRule, ScaleRule};
use crate::backend::compositor_common::wallpaper::{WallpaperImageData, WallpaperMode};

static NEXT_OUTPUT_TEXTURE_GENERATION: AtomicU64 = AtomicU64::new(1);

fn next_output_texture_generation() -> u64 {
    NEXT_OUTPUT_TEXTURE_GENERATION.fetch_add(1, Ordering::Relaxed)
}

pub(crate) const XDG_POPUP_WINDOW_ID_PREFIX: u64 = 0xFE00_0000_0000_0000;
pub(crate) const IME_POPUP_WINDOW_ID_PREFIX: u64 = 0xFF00_0000_0000_0000;
const AUXILIARY_WINDOW_ID_MASK: u64 = 0xFF00_0000_0000_0000;

pub(crate) fn is_auxiliary_window_id(window_id: u64) -> bool {
    matches!(
        window_id & AUXILIARY_WINDOW_ID_MASK,
        XDG_POPUP_WINDOW_ID_PREFIX | IME_POPUP_WINDOW_ID_PREFIX
    )
}

use crate::backend::compositor_common::transitions::TransitionMode;
use crate::backend::compositor_common::wobbly::WobblyState;

// ---------------------------------------------------------------------------
// Matrix math
// ---------------------------------------------------------------------------

/// Orthographic projection matrix (column-major for OpenGL).
pub(crate) fn ortho(l: f32, r: f32, b: f32, t: f32) -> [f32; 16] {
    math::ortho(l, r, b, t, -1.0, 1.0)
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
        // Validate both sources before creating either shader.  Otherwise a
        // CString conversion failure after CreateShader would strand the raw
        // shader name in the still-live KMS context.
        let vs_cstr = CString::new(vs_src).map_err(|e| format!("VS CString: {}", e))?;
        let fs_cstr = CString::new(fs_src).map_err(|e| format!("FS CString: {}", e))?;

        let vs = gl.CreateShader(ffi::VERTEX_SHADER);
        if vs == 0 {
            return Err("glCreateShader returned 0 for vertex shader".into());
        }
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
        if fs == 0 {
            gl.DeleteShader(vs);
            return Err("glCreateShader returned 0 for fragment shader".into());
        }
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
        if program == 0 {
            gl.DeleteShader(vs);
            gl.DeleteShader(fs);
            return Err("glCreateProgram returned 0".into());
        }
        gl.AttachShader(program, vs);
        gl.AttachShader(program, fs);
        gl.LinkProgram(program);

        gl.GetProgramiv(program, ffi::LINK_STATUS, &mut status);
        if status == 0 {
            let mut len = 0i32;
            gl.GetProgramiv(program, ffi::INFO_LOG_LENGTH, &mut len);
            let mut buf = vec![0u8; len as usize];
            gl.GetProgramInfoLog(
                program,
                len,
                std::ptr::null_mut(),
                buf.as_mut_ptr() as *mut _,
            );
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
    pub desat: i32,
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
    // SOTA #2 Phase 2.2 scene-linear output. -1 if not present in the
    // compiled program (e.g. older builds before the uniform was added).
    pub scene_linear: i32,
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
    pub radius_top: i32,
    pub border_width: i32,
    pub scene_linear: i32,
}

pub(crate) struct GradientBorderUniforms {
    pub radius_top: i32,
    pub rect: i32,
    pub projection: i32,
    pub color_a: i32,
    pub color_b: i32,
    pub gradient_angle: i32,
    pub size: i32,
    pub radius: i32,
    pub border_width: i32,
    pub scene_linear: i32,
}

/// Uniforms of the frosted-glass surface program used by every self-drawn
/// panel when `appearance.ui_theme = "glass"`.
pub(crate) struct GlassUniforms {
    pub rect: i32,
    pub projection: i32,
    pub backdrop: i32,
    pub screen_size: i32,
    pub tint: i32,
    pub size: i32,
    pub radius: i32,
    pub radius_top: i32,
    pub corner_exp: i32,
    pub saturation: i32,
    pub luminance: i32,
    pub bevel_width: i32,
    pub refraction: i32,
    pub rim_width: i32,
    pub rim_intensity: i32,
    pub rim_tint: i32,
    pub sheen: i32,
    pub edge_shade: i32,
    pub grain: i32,
    pub alpha: i32,
    pub scene_linear: i32,
}

pub(crate) struct PostprocessUniforms {
    pub rect: i32,
    pub projection: i32,
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
pub(crate) struct SceneLinearEncodeUniforms {
    pub rect: i32,
    pub projection: i32,
    pub texture: i32,
    pub encode_tf: i32,
    pub encode_gamma: i32,
}

#[allow(dead_code)]
pub(crate) struct SceneLinearDecodeUniforms {
    pub rect: i32,
    pub projection: i32,
    pub texture: i32,
}

pub(crate) struct TransitionUniforms {
    pub rect: i32,
    pub projection: i32,
    pub opacity: i32,
    pub uv_rect: i32,
}

#[allow(dead_code)]
pub(crate) struct CubeUniforms {
    pub mvp: i32,
    pub model: i32,
    pub texture: i32,
    pub brightness: i32,
    pub uv_rect: i32,
    pub aspect: i32,
    pub camera: i32,
    pub accent: i32,
    pub alpha: i32,
    pub desat: i32,
    pub edge: i32,
    pub lit: i32,
    pub scene_linear: i32,
    pub has_alpha: i32,
    pub filler: i32,
    pub reflection: i32,
    pub floor_y: i32,
    pub color_managed: i32,
    pub color_matrix: i32,
    pub decode_tf: i32,
    pub decode_gamma: i32,
    pub encode_tf: i32,
    pub encode_gamma: i32,
}

pub(crate) struct OverviewCapUniforms {
    pub mvp: i32,
    pub model: i32,
    pub radius: i32,
    pub y: i32,
    pub sides: i32,
    pub color: i32,
    pub accent: i32,
    pub camera: i32,
    pub scene_linear: i32,
    pub reflection: i32,
    pub floor_y: i32,
}

pub(crate) struct OverviewSkydomeUniforms {
    pub rect: i32,
    pub projection: i32,
    pub opacity: i32,
    pub angle: i32,
    pub ground: i32,
    pub accent: i32,
    pub scene_linear: i32,
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
    pub scene_linear: i32,
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
    pub color_managed: i32,
    pub scene_linear: i32,
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
    pub dock_size: i32,
    pub grid_size: i32,
    pub ripple_progress: i32,
    pub ripple_amplitude: i32,
    pub color_managed: i32,
    pub color_matrix: i32,
    pub decode_tf: i32,
    pub decode_gamma: i32,
    pub encode_tf: i32,
    pub encode_gamma: i32,
    pub scene_linear: i32,
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
    /// Raw GL texture imported from the Wayland surface. Kept alongside
    /// `texture_owner` so the hot render paths do not need to unwrap the
    /// Smithay handle for every draw.
    pub gl_texture: Option<u32>,
    /// Strong Smithay owner for `gl_texture`.
    ///
    /// Surface renderer state and the backend offscreen cache may disappear
    /// before a close/genie animation finishes. Keeping this Arc-backed handle
    /// prevents Smithay from scheduling deletion of the GL texture while the
    /// compositor still samples it.
    pub texture_owner: Option<GlesTexture>,
    pub width: u32,
    pub height: u32,
    pub has_alpha: bool,
    pub y_inverted: bool,
    pub fade_opacity: f32,
    pub fading_out: bool,
    pub anim_scale: f32,
    pub anim_scale_target: f32,
    pub wobbly: Option<WobblyState>,
    pub motion_trail: MotionTrail,
    pub opacity_override: Option<f32>,
    pub corner_radius_override: Option<f32>,
    pub frame_extents: [u32; 4],
    pub is_shaped: bool,
    pub is_fullscreen: bool,
    pub is_urgent: bool,
    pub is_pip: bool,
    pub is_moving: bool,
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
    /// Last on-screen rectangle captured when the window left the live scene.
    /// Close fades render from this geometry because retired windows are no
    /// longer present in the backend-provided `visible_scene`.
    pub closing_rect: Option<(f32, f32, f32, f32)>,
    /// Set when the explicit minimize path starts a genie animation. The
    /// WindowState and GenieAnimation both retain strong texture owners until
    /// `tick_genie` removes them.
    pub is_genie_minimizing: bool,
    /// The live surface exists at its final geometry but is suppressed while
    /// the reverse Genie mesh expands out of its Dock slot.
    pub is_genie_restoring: bool,
    /// wp-color-management transform to apply in the window fragment shader
    /// for this frame. `None` = identity / bypass. Refreshed each frame in
    /// `compositor_render_frame` from `(surface_params, output_params)`; the
    /// live value is read once in the draw loop and then becomes stale. The
    /// minimize path is the sole exception: it snapshots this `Copy` value
    /// together with the retained texture because the unmapped surface can no
    /// longer receive a per-frame refresh.
    pub color_transform: Option<crate::backend::wayland_udev::color_pipeline::ColorTransform>,
}

/// Resource-free compositor metadata retained while a minimized surface has
/// no live or Dock-owned texture. Keeping it separate from `WindowState`
/// prevents hidden clients from participating in render/direct-scanout scans
/// while preserving the semantics needed by a later static import or restore.
#[derive(Clone)]
struct WindowVisualMetadata {
    opacity_override: Option<f32>,
    corner_radius_override: Option<f32>,
    frame_extents: [u32; 4],
    is_shaped: bool,
    is_fullscreen: bool,
    is_urgent: bool,
    is_pip: bool,
    is_frosted: bool,
    frosted_strength: f32,
    class_name: String,
    scale: f32,
    audio_sync_target: Option<f32>,
}

impl From<&WindowState> for WindowVisualMetadata {
    fn from(window: &WindowState) -> Self {
        Self {
            opacity_override: window.opacity_override,
            corner_radius_override: window.corner_radius_override,
            frame_extents: window.frame_extents,
            is_shaped: window.is_shaped,
            is_fullscreen: window.is_fullscreen,
            is_urgent: window.is_urgent,
            is_pip: window.is_pip,
            is_frosted: window.is_frosted,
            frosted_strength: window.frosted_strength,
            class_name: window.class_name.clone(),
            scale: window.scale,
            audio_sync_target: window.audio_sync_target,
        }
    }
}

impl WindowVisualMetadata {
    fn apply_to(&self, window: &mut WindowState) {
        window.opacity_override = self.opacity_override;
        window.corner_radius_override = self.corner_radius_override;
        window.frame_extents = self.frame_extents;
        window.is_shaped = self.is_shaped;
        window.is_fullscreen = self.is_fullscreen;
        window.is_urgent = self.is_urgent;
        window.is_pip = self.is_pip;
        window.is_frosted = self.is_frosted;
        window.frosted_strength = self.frosted_strength;
        window.class_name.clone_from(&self.class_name);
        window.scale = self.scale;
        window.audio_sync_target = self.audio_sync_target;
    }
}

/// Urgency updates may arrive while a surface is being managed but before its
/// compositor state/texture exists. Keep only positive pending updates: a
/// later clear cancels the pending value, and first creation consumes it.
#[derive(Default)]
struct PendingWindowUrgency {
    urgent_windows: HashSet<u64>,
}

impl PendingWindowUrgency {
    fn update(&mut self, window_id: u64, urgent: bool) -> bool {
        if urgent {
            self.urgent_windows.insert(window_id)
        } else {
            self.urgent_windows.remove(&window_id)
        }
    }

    fn take_for_new_window(&mut self, window_id: u64) -> bool {
        self.urgent_windows.remove(&window_id)
    }

    fn discard(&mut self, window_id: u64) -> bool {
        self.urgent_windows.remove(&window_id)
    }
}

/// Active genie minimize animation for one window (Wayland).
///
/// Both this animation and its matching WindowState hold a strong Smithay
/// texture handle. The duplicate handles are cheap Arc clones and make the
/// animation independently safe if the backend surface/offscreen owner is
/// released as soon as the window leaves the live scene.
#[allow(dead_code)]
pub(crate) struct GenieAnimation {
    pub window_id: u64,
    pub start: Instant,
    pub start_progress: f32,
    pub direction: GenieDirection,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub texture_owner: GlesTexture,
    pub has_alpha: bool,
    pub y_inverted: bool,
    pub content_uv: [f32; 4],
    /// Surface-to-output transform captured with the retained texture. The
    /// live WindowState refreshes this every frame, but a minimized surface no
    /// longer participates in that refresh and must carry its last valid
    /// transform alongside its pixels.
    pub color_transform: Option<crate::backend::wayland_udev::color_pipeline::ColorTransform>,
    pub target: CompositorRect,
}

pub(crate) struct MinimizedVisual {
    pub w: f32,
    pub h: f32,
    pub texture_owner: GlesTexture,
    pub has_alpha: bool,
    pub y_inverted: bool,
    pub content_uv: [f32; 4],
    pub color_transform: Option<crate::backend::wayland_udev::color_pipeline::ColorTransform>,
    pub target: Option<CompositorRect>,
    pub cached_at: Instant,
    /// Conservative allocation estimate derived from the retained texture's
    /// physical buffer dimensions, not the animation's logical geometry.
    pub estimated_bytes: u64,
}

#[derive(Clone)]
pub(crate) struct DockPreview {
    pub window_id: u64,
    pub anchor: CompositorRect,
    pub started: Instant,
    pub lease_deadline: Instant,
    pub start_opacity: f32,
    pub start_scale: f32,
    pub direction: PreviewDirection,
    pub opacity: f32,
    pub scale: f32,
    /// The Dock request remains authoritative while a hidden surface is being
    /// imported after cache eviction. Its animation and lease start only once
    /// a real texture is available.
    pub awaiting_source: bool,
}

// ---------------------------------------------------------------------------
// Expose entry
// ---------------------------------------------------------------------------

/// Expose entry keyed by the compositor's u64 window id. Layout and animation
/// come from the shared platform-neutral implementation.
pub(crate) type ExposeEntry = crate::backend::compositor_common::expose::ExposeEntry<u64>;

// ---------------------------------------------------------------------------
// Overview entry
// ---------------------------------------------------------------------------

#[allow(dead_code)]
pub(crate) struct OverviewEntry {
    pub window_id: u64,
    /// Normalized scrolling overview-strip x position when available.
    pub x: f32,
    /// Normalized row position inside the scrolling column when available.
    pub y: f32,
    /// Normalized scrolling overview-strip column width when available.
    pub w: f32,
    /// Normalized row height inside the scrolling column when available.
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
    pub lifetime: f32,
    pub max_lifetime: f32,
}

pub(crate) struct ParticleSystem {
    pub particles: Vec<Particle>,
    pub age: f32,
}

/// The compositor creates the output texture as a raw GLES name, but KMS may
/// subsequently transfer that name into a Smithay `GlesTexture::from_raw`
/// owner. Runtime teardown must honor whichever owner actually exists; a raw
/// `glDeleteTextures` after that transfer would race Smithay's deferred delete.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CompositorOutputTextureOwnership {
    RawCompositor,
    SmithayRenderer,
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
    gradient_border_program: u32,
    postprocess_program: u32,
    // SOTA #2 Phase 2.2: scene-linear encode (linear FBO → encoded
    // output_fbo) and decode (encoded output_fbo → linear FBO) passes.
    // Programs are always built so that an `Effect` toggle could enable
    // the path at runtime; render path checks linear_fbo != 0 to decide
    // whether to dispatch them. Each pass shares BLUR_DOWN_VERTEX with
    // the blur chain (same gl_VertexID-based fullscreen quad).
    #[allow(dead_code)]
    scene_linear_encode_program: u32,
    #[allow(dead_code)]
    scene_linear_decode_program: u32,
    #[allow(dead_code)]
    scene_linear_encode_uniforms: SceneLinearEncodeUniforms,
    #[allow(dead_code)]
    scene_linear_decode_uniforms: SceneLinearDecodeUniforms,
    transition_program: u32,
    cube_program: u32,
    overview_cap_program: u32,
    portal_program: u32,
    edge_glow_program: u32,
    tilt_program: u32,
    wobbly_program: u32,
    genie_program: u32,
    particle_program: u32,
    overview_bg_program: u32,
    overview_skydome_program: u32,
    glass_program: u32,
    hud_program: u32,
    sysui_text_program: u32,
    temporal_blur_mix_program: u32,

    /// Blurred copy of the frame, captured once per frame just before the
    /// self-drawn panels so each of them can sample what it covers. `None`
    /// under the Material theme, or when no blur chain is available.
    glass_backdrop: Option<u32>,

    // Uniform locations
    win_uniforms: WindowUniforms,
    shadow_uniforms: ShadowUniforms,
    blur_uniforms: BlurUniforms,
    border_uniforms: BorderUniforms,
    gradient_border_uniforms: GradientBorderUniforms,
    glass_uniforms: GlassUniforms,
    postprocess_uniforms: PostprocessUniforms,
    transition_uniforms: TransitionUniforms,
    cube_uniforms: CubeUniforms,
    overview_cap_uniforms: OverviewCapUniforms,
    overview_skydome_uniforms: OverviewSkydomeUniforms,
    portal_uniforms: PortalUniforms,
    tilt_uniforms: TiltUniforms,
    wobbly_uniforms: WobblyUniforms,
    #[allow(dead_code)]
    genie_uniforms: GenieUniforms,
    edge_glow_uniforms: EdgeGlowUniforms,

    // GL resources
    quad_vao: u32,
    quad_vbo: u32,
    output_fbo: u32,
    output_texture: u32,
    /// Actual storage format chosen when the output FBO was allocated. This is
    /// a hardware property and must not be inferred from the runtime HDR
    /// post-processing toggle.
    output_internal_format: u32,
    /// Changes whenever the output texture is recreated, even if GL recycles
    /// the numeric texture id.
    output_texture_generation: u64,
    /// FP16 (RGBA16F) intermediate target used when both the color-management
    /// render path and scene-linear compositing are requested. Creation and
    /// hot-enable allocate it at the output dimensions; resize/disable keep it
    /// synchronized with output_fbo. Zero when either gate is off — the render
    /// path checks this sentinel and falls back to the encoded-space pipeline.
    /// Window shaders write linear pixels here; the frame boundary either encodes
    /// them in a shader or blits them for the CRTC OETF.
    scene_linear_requested: bool,
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
    /// Guards the explicit context-current teardown used by runtime toggles.
    /// `Drop` remains context-agnostic for KMS/EGL destruction paths.
    gpu_resources_released: bool,

    // Dimensions
    screen_w: u32,
    screen_h: u32,

    // Per-window state
    windows: HashMap<u64, WindowState>,
    minimized_window_metadata: HashMap<u64, WindowVisualMetadata>,
    pending_window_urgency: PendingWindowUrgency,

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
    shadow_inactive_opacity: f32,
    shadow_spread: f32,
    inactive_opacity: f32,
    active_opacity: f32,
    inactive_dim: f32,
    inactive_desaturate: f32,
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
    /// Capture the last completed output frame before drawing the new
    /// workspace. Set by the workspace notification, consumed at frame start.
    transition_snapshot_pending: bool,
    transition_start: Option<Instant>,
    transition_duration: Duration,
    transition_mode: TransitionMode,
    transition_direction: i32,

    // Overview (3D prism carousel)
    overview_enabled: bool,
    overview_active: bool,
    overview_opacity: f32,
    overview_entries: Vec<OverviewEntry>,
    overview_selection: Option<u64>,
    overview_monitor: (i32, i32, u32, u32),
    overview_rotation: f32,
    overview_target_rotation: f32,
    overview_title_textures: Vec<u32>,
    /// Entry/title changes are recorded without touching GL; the next render
    /// rebuilds the textures while the compositor context is current.
    overview_titles_dirty: bool,

    // Expose
    expose_enabled: bool,
    expose_active: bool,
    expose_opacity: f32,
    expose_entries: Vec<ExposeEntry>,

    // Snap preview
    snap_preview_enabled: bool,
    snap_preview: Option<(f32, f32, f32, f32)>,
    /// Desired visibility.  The rectangle remains retained while fading out.
    snap_preview_target_visible: bool,
    snap_preview_opacity: f32,

    // Peek mode
    peek_enabled: bool,
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
    tilt_amount: f32,
    tilt_perspective: f32,

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
    sys_stats: crate::backend::sys_stats::SysStatsSampler,

    // Optimization
    needs_render: bool,
    last_frame_time: Instant,
    /// Tracks whether compositor-owned effects were already active on the
    /// previous frame. A newly-created effect receives a zero-length first
    /// tick, so an idle gap before creation cannot consume its lifetime.
    effect_clock_active: bool,
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
    scratch_particle_data: Vec<f32>,
    scratch_retired_aux_ids: Vec<u64>,

    // Dock position (for genie)
    dock_x: f32,
    dock_y: f32,

    // Active genie minimize animations
    pub(crate) genie_active: Vec<GenieAnimation>,
    genie_targets: HashMap<u64, CompositorRect>,
    minimized_windows: HashSet<u64>,
    minimized_visuals: HashMap<u64, MinimizedVisual>,
    minimized_thumbnails: minimized_thumbnail::MinimizedThumbnailState,
    // Minimize intent can precede both compositor creation and the client's
    // first imported buffer. These ids need one hidden-surface texture import
    // before they can converge into minimized_visuals.
    pending_minimized_visuals: HashSet<u64>,
    pending_genie_restores: HashSet<u64>,
    dock_preview: Option<DockPreview>,

    // Window groups (tabs): the bars the window manager reserved, strip
    // geometry included, plus one (texture, w, h) per cell rebuilt only when
    // the groups change.
    window_groups: Vec<crate::backend::compositor_common::window_tabs::TabGroup>,
    tab_title_textures: Vec<Vec<Option<(u32, u32, u32)>>>,
    tab_titles_dirty: bool,

    // Monitors info
    monitors: Vec<(u32, i32, i32, u32, u32, u32)>,

    // Zoom to fit
    zoom_to_fit_window: Option<u32>,

    // Annotations
    annotation_active: bool,
    annotation_strokes: Vec<AnnotationStroke>,
    /// Filled shapes the strokes cannot express — redaction bars, counter
    /// bubbles. Cleared with the strokes.
    annotation_quads: Vec<crate::backend::compositor_common::annotation_overlay::AnnotationQuad>,
    /// Text runs, plus their rasterised textures. Rebuilt only when the list
    /// changes, the same bargain `tab_title_textures` strikes.
    annotation_labels: Vec<crate::backend::compositor_common::annotation_overlay::AnnotationLabel>,
    annotation_label_textures: Vec<Option<(u32, u32, u32)>>,
    annotation_labels_dirty: bool,
    annotation_color: [f32; 4],
    annotation_line_width: f32,

    // Screenshot editor toolbar
    /// The strip the window manager published, or `None` when no capture is
    /// being edited; one rasterised glyph per button.
    screenshot_toolbar:
        Option<crate::backend::compositor_common::screenshot_toolbar::ScreenshotToolbar>,
    screenshot_toolbar_icons: Vec<Option<(u32, u32, u32)>>,
    screenshot_toolbar_dirty: bool,
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
    /// Raw wallpaper textures detached while no GL context is available.
    /// They are deleted at the beginning of the next rendered frame.
    retired_wallpaper_textures: Vec<u32>,
    wallpaper_crossfade: bool,
    wallpaper_crossfade_duration_ms: u64,
    old_wallpaper_texture: Option<u32>,
    old_wallpaper_img_w: u32,
    old_wallpaper_img_h: u32,
    old_wallpaper_mode: WallpaperMode,
    wallpaper_transition_start: Option<Instant>,

    // --- Per-window rules ---
    opacity_rules: Vec<OpacityRule>,
    corner_radius_rules: Vec<CornerRadiusRule>,
    scale_rules: Vec<ScaleRule>,
    frosted_glass_rules: Vec<(String, f32)>,
    shadow_exclude: Vec<String>,
    blur_exclude: Vec<String>,
    rounded_corners_exclude: Vec<String>,
    detect_client_opacity: bool,
    blur_use_frame_extents: bool,

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

    // --- Border config ---
    border_enabled: bool,
    border_width: f32,
    border_color_focused: [f32; 4],
    border_color_unfocused: [f32; 4],
    border_gradient_enabled: bool,
    border_gradient_color_a: [f32; 4],
    border_gradient_color_b: [f32; 4],
    border_gradient_angle: f32,
    border_gradient_speed: f32,

    // --- Screenshot ---
    screenshot_requests: crate::backend::compositor_common::screenshot::ScreenshotQueue,
    screenshot_readback: screenshot_readback::ScreenshotReadback,

    // --- Recording control ---
    pending_recording_start: Option<(String, (i32, i32, u32, u32))>,
    pending_recording_stop: bool,
    recording_region_overlay: Option<(i32, i32, u32, u32)>,

    // --- Debug HUD extended ---
    debug_hud_extended: bool,
    /// Material HUD text sections: title, state chip, stat labels, stat values.
    hud_textures: [Option<(u32, u32, u32)>; 4],
    /// Styled system-UI panel text sections: title, query, items, hint.
    sysui_textures: [Option<(u32, u32, u32)>; 4],
    sysui_cache: String,

    // --- Toast notifications (top-right stacked cards) ---
    toast_stack: crate::backend::compositor_common::toast::ToastStack,
    toast_textures: HashMap<u64, [Option<(u32, u32, u32)>; 2]>,
    osd_slot: crate::backend::compositor_common::osd::OsdSlot,
    /// Cached OSD label texture keyed by its text ("icon  label").
    osd_texture: Option<(String, u32, u32, u32)>,
    /// Toast ids evicted outside the render pass; their textures are freed
    /// on the next frame while a GL context is current.
    toast_retired: Vec<u64>,
    hud_text_cache: String,
    system_ui: Option<crate::backend::api::SystemUiOverlay>,
    /// Open/morph spring for the docked system-UI card.
    system_ui_island: crate::backend::compositor_common::dynamic_island::IslandMotion,
    /// Open/morph spring for the docked debug HUD card.
    hud_island: crate::backend::compositor_common::dynamic_island::IslandMotion,
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
    transition_exclude_top: u32,

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
    unsafe {
        create_fbo_texture_fmt(gl, w, h, ffi::RGBA8).unwrap_or_else(|status| {
            panic!("failed to create required RGBA8 framebuffer ({w}x{h}, status=0x{status:x})")
        })
    }
}

unsafe fn create_fbo_texture_10bit(gl: &ffi::Gles2, w: u32, h: u32) -> (u32, u32) {
    unsafe {
        create_fbo_texture_fmt(gl, w, h, GL_RGB10_A2).unwrap_or_else(|status| {
            panic!("failed to create required RGB10_A2 framebuffer ({w}x{h}, status=0x{status:x})")
        })
    }
}

/// Allocate a half-float RGBA FBO for scene-linear compositing. Linear
/// values can exceed [0, 1] (e.g. PQ peak-luminance scaling), so an 8-bit
/// or 10-bit unsigned-normalized format would clamp them. RGBA16F is the
/// GLES 3.0-portable storage with enough range and precision.
unsafe fn create_fbo_texture_fp16(gl: &ffi::Gles2, w: u32, h: u32) -> Result<(u32, u32), u32> {
    unsafe { create_fbo_texture_fmt(gl, w, h, GL_RGBA16F) }
}

unsafe fn create_fbo_texture_fmt(
    gl: &ffi::Gles2,
    w: u32,
    h: u32,
    internal_format: u32,
) -> Result<(u32, u32), u32> {
    unsafe {
        let mut tex = 0u32;
        gl.GenTextures(1, &mut tex);
        if tex == 0 {
            let error = gl.GetError();
            return Err(if error == ffi::NO_ERROR {
                ffi::OUT_OF_MEMORY
            } else {
                error
            });
        }
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
        gl.TexParameteri(
            ffi::TEXTURE_2D,
            ffi::TEXTURE_WRAP_S,
            ffi::CLAMP_TO_EDGE as i32,
        );
        gl.TexParameteri(
            ffi::TEXTURE_2D,
            ffi::TEXTURE_WRAP_T,
            ffi::CLAMP_TO_EDGE as i32,
        );

        let mut fbo = 0u32;
        gl.GenFramebuffers(1, &mut fbo);
        if fbo == 0 {
            let error = gl.GetError();
            gl.BindTexture(ffi::TEXTURE_2D, 0);
            gl.DeleteTextures(1, &tex);
            return Err(if error == ffi::NO_ERROR {
                ffi::OUT_OF_MEMORY
            } else {
                error
            });
        }
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
                "[udev/compositor] rejecting incomplete FBO (status=0x{status:x}) for {w}x{h} internal_format=0x{internal_format:x}"
            );
            gl.BindFramebuffer(ffi::FRAMEBUFFER, 0);
            if fbo != 0 {
                gl.DeleteFramebuffers(1, &fbo);
            }
            if tex != 0 {
                gl.DeleteTextures(1, &tex);
            }
            return Err(status);
        }

        Ok((fbo, tex))
    }
}

/// Test-only observations and failure points are carried through this private
/// value rather than global state, so headless constructor tests remain safe
/// when Rust's test harness runs them in parallel.
#[derive(Default)]
struct CompositorConstructionProbe {
    fail_before_program_count: Option<usize>,
    fail_before_framebuffer_count: Option<usize>,
    fail_before_commit: bool,
    programs: Vec<u32>,
    vertex_arrays: Vec<u32>,
    buffers: Vec<u32>,
    framebuffers: Vec<u32>,
    textures: Vec<u32>,
}

/// Owns every raw GLES name created before `WaylandCompositor` itself exists.
///
/// The output texture is still a compositor-owned raw name at this stage;
/// KMS cannot wrap it in a Smithay `GlesTexture` until `new` returns and the
/// compositor is installed.  Consequently every constructor failure can and
/// must delete it directly here.
struct CompositorConstructionGuard<'gl, 'probe> {
    gl: &'gl ffi::Gles2,
    probe: Option<&'probe mut CompositorConstructionProbe>,
    programs: Vec<u32>,
    vertex_arrays: Vec<u32>,
    buffers: Vec<u32>,
    framebuffers: Vec<u32>,
    textures: Vec<u32>,
    committed: bool,
}

impl<'gl, 'probe> CompositorConstructionGuard<'gl, 'probe> {
    fn new(gl: &'gl ffi::Gles2, probe: Option<&'probe mut CompositorConstructionProbe>) -> Self {
        Self {
            gl,
            probe,
            programs: Vec::with_capacity(26),
            vertex_arrays: Vec::with_capacity(2),
            buffers: Vec::with_capacity(2),
            framebuffers: Vec::with_capacity(11),
            textures: Vec::with_capacity(11),
            committed: false,
        }
    }

    unsafe fn compile_program(&mut self, vs_src: &str, fs_src: &str) -> Result<u32, String> {
        if self
            .probe
            .as_ref()
            .and_then(|probe| probe.fail_before_program_count)
            == Some(self.programs.len())
        {
            return Err(format!(
                "injected compositor construction failure before program {}",
                self.programs.len()
            ));
        }
        let program = unsafe { create_program(self.gl, vs_src, fs_src)? };
        self.track_program(program);
        Ok(program)
    }

    fn track_program(&mut self, program: u32) {
        if program == 0 {
            return;
        }
        self.programs.push(program);
        if let Some(probe) = &mut self.probe {
            probe.programs.push(program);
        }
    }

    fn track_vertex_array(&mut self, vertex_array: u32) {
        if vertex_array == 0 {
            return;
        }
        self.vertex_arrays.push(vertex_array);
        if let Some(probe) = &mut self.probe {
            probe.vertex_arrays.push(vertex_array);
        }
    }

    unsafe fn create_vertex_array(&mut self, label: &str) -> Result<u32, String> {
        let mut vertex_array = 0;
        unsafe { self.gl.GenVertexArrays(1, &mut vertex_array) };
        if vertex_array == 0 {
            return Err(format!("glGenVertexArrays returned 0 for {label}"));
        }
        self.track_vertex_array(vertex_array);
        Ok(vertex_array)
    }

    fn track_buffer(&mut self, buffer: u32) {
        if buffer == 0 {
            return;
        }
        self.buffers.push(buffer);
        if let Some(probe) = &mut self.probe {
            probe.buffers.push(buffer);
        }
    }

    unsafe fn create_buffer(&mut self, label: &str) -> Result<u32, String> {
        let mut buffer = 0;
        unsafe { self.gl.GenBuffers(1, &mut buffer) };
        if buffer == 0 {
            return Err(format!("glGenBuffers returned 0 for {label}"));
        }
        self.track_buffer(buffer);
        Ok(buffer)
    }

    unsafe fn create_fbo_texture(
        &mut self,
        w: u32,
        h: u32,
        internal_format: u32,
    ) -> Result<(u32, u32), u32> {
        if self
            .probe
            .as_ref()
            .and_then(|probe| probe.fail_before_framebuffer_count)
            == Some(self.framebuffers.len())
        {
            return Err(ffi::FRAMEBUFFER_UNSUPPORTED);
        }
        let (framebuffer, texture) =
            unsafe { create_fbo_texture_fmt(self.gl, w, h, internal_format)? };
        if framebuffer != 0 {
            self.framebuffers.push(framebuffer);
            if let Some(probe) = &mut self.probe {
                probe.framebuffers.push(framebuffer);
            }
        }
        if texture != 0 {
            self.textures.push(texture);
            if let Some(probe) = &mut self.probe {
                probe.textures.push(texture);
            }
        }
        Ok((framebuffer, texture))
    }

    unsafe fn create_required_fbo_texture(
        &mut self,
        w: u32,
        h: u32,
        internal_format: u32,
        label: &str,
    ) -> Result<(u32, u32), String> {
        unsafe { self.create_fbo_texture(w, h, internal_format) }.map_err(|status| {
            format!("failed to create required {label} framebuffer ({w}x{h}, status=0x{status:x})")
        })
    }

    fn should_fail_before_commit(&self) -> bool {
        self.probe
            .as_ref()
            .is_some_and(|probe| probe.fail_before_commit)
    }

    fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for CompositorConstructionGuard<'_, '_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        unsafe {
            // Clear every binding which could otherwise keep a deleted name
            // live or make the caller inherit constructor-only state.
            self.gl.UseProgram(0);
            self.gl.BindVertexArray(0);
            self.gl.BindBuffer(ffi::ARRAY_BUFFER, 0);
            self.gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, 0);
            self.gl.BindBuffer(ffi::PIXEL_UNPACK_BUFFER, 0);
            self.gl.BindFramebuffer(ffi::FRAMEBUFFER, 0);
            self.gl.ActiveTexture(ffi::TEXTURE0);
            self.gl.BindTexture(ffi::TEXTURE_2D, 0);

            for framebuffer in self.framebuffers.iter_mut().rev() {
                delete_framebuffer_name(self.gl, framebuffer);
            }
            for texture in self.textures.iter_mut().rev() {
                delete_texture_name(self.gl, texture);
            }
            for buffer in self.buffers.iter_mut().rev() {
                delete_buffer_name(self.gl, buffer);
            }
            for vertex_array in self.vertex_arrays.iter_mut().rev() {
                delete_vertex_array_name(self.gl, vertex_array);
            }
            for program in self.programs.iter_mut().rev() {
                delete_program_name(self.gl, program);
            }
        }
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
        unsafe { Self::new_inner(gl, screen_w, screen_h, hdr_10bit, None) }
    }

    unsafe fn new_inner(
        gl: &ffi::Gles2,
        screen_w: u32,
        screen_h: u32,
        hdr_10bit: bool,
        construction_probe: Option<&mut CompositorConstructionProbe>,
    ) -> Result<Self, String> {
        unsafe {
            let mut construction = CompositorConstructionGuard::new(gl, construction_probe);
            let program =
                construction.compile_program(shaders::VERTEX_SHADER, shaders::FRAGMENT_SHADER)?;
            let thumbnail_program = construction.compile_program(
                minimized_thumbnail::THUMBNAIL_DOWNSAMPLE_VERTEX_SHADER,
                crate::backend::compositor_common::minimized_thumbnail::THUMBNAIL_DOWNSAMPLE_FRAGMENT_SHADER,
            )?;
            let minimized_thumbnails =
                minimized_thumbnail::MinimizedThumbnailState::from_program(gl, thumbnail_program);
            let shadow_program = construction
                .compile_program(shaders::VERTEX_SHADER, shaders::SHADOW_FRAGMENT_SHADER)?;
            let blur_down_program = construction
                .compile_program(shaders::BLUR_DOWN_VERTEX, shaders::BLUR_DOWN_FRAGMENT)?;
            let blur_up_program = construction
                .compile_program(shaders::BLUR_DOWN_VERTEX, shaders::BLUR_UP_FRAGMENT)?;
            let border_program = construction
                .compile_program(shaders::VERTEX_SHADER, shaders::BORDER_FRAGMENT_SHADER)?;
            let gradient_border_program = construction.compile_program(
                shaders::VERTEX_SHADER,
                shaders::GRADIENT_BORDER_FRAGMENT_SHADER,
            )?;
            let postprocess_program = construction.compile_program(
                shaders::VERTEX_SHADER,
                shaders::MAGNIFIER_POSTPROCESS_FRAGMENT_SHADER,
            )?;
            let scene_linear_encode_program = construction.compile_program(
                shaders::BLUR_DOWN_VERTEX,
                shaders::SCENE_LINEAR_ENCODE_FRAGMENT,
            )?;
            let scene_linear_decode_program = construction.compile_program(
                shaders::BLUR_DOWN_VERTEX,
                shaders::SCENE_LINEAR_DECODE_FRAGMENT,
            )?;
            let transition_program = construction
                .compile_program(shaders::VERTEX_SHADER, shaders::TRANSITION_FRAGMENT_SHADER)?;
            let cube_program = construction
                .compile_program(shaders::CUBE_VERTEX_SHADER, shaders::CUBE_FRAGMENT_SHADER)?;
            let overview_cap_program = construction.compile_program(
                shaders::OVERVIEW_CAP_VERTEX_SHADER,
                shaders::OVERVIEW_CAP_FRAGMENT_SHADER,
            )?;
            let portal_program = construction
                .compile_program(shaders::VERTEX_SHADER, shaders::PORTAL_FRAGMENT_SHADER)?;
            let edge_glow_program = construction
                .compile_program(shaders::VERTEX_SHADER, shaders::EDGE_GLOW_FRAGMENT_SHADER)?;
            let tilt_program = construction
                .compile_program(shaders::TILT_VERTEX_SHADER, shaders::TILT_FRAGMENT_SHADER)?;
            let wobbly_program = construction
                .compile_program(shaders::WOBBLY_VERTEX_SHADER, shaders::FRAGMENT_SHADER)?;
            let genie_program = construction
                .compile_program(shaders::GENIE_VERTEX_SHADER, shaders::FRAGMENT_SHADER)?;
            let particle_program = construction.compile_program(
                shaders::PARTICLE_VERTEX_SHADER,
                shaders::PARTICLE_FRAGMENT_SHADER,
            )?;
            let overview_bg_program = construction
                .compile_program(shaders::VERTEX_SHADER, shaders::OVERVIEW_BG_FRAGMENT_SHADER)?;
            let overview_skydome_program = construction.compile_program(
                shaders::VERTEX_SHADER,
                shaders::OVERVIEW_SKYDOME_FRAGMENT_SHADER,
            )?;
            // Compiled unconditionally so switching `appearance.ui_theme` at
            // runtime never has to touch the GL context.
            let glass_program = construction
                .compile_program(shaders::VERTEX_SHADER, shaders::GLASS_FRAGMENT_SHADER)?;
            let hud_program = construction
                .compile_program(shaders::VERTEX_SHADER, shaders::HUD_FRAGMENT_SHADER)?;
            let sysui_text_program = construction
                .compile_program(shaders::VERTEX_SHADER, shaders::HUD_TEXT_FRAGMENT_SHADER)?;
            let temporal_blur_mix_program = construction.compile_program(
                shaders::TEMPORAL_BLUR_MIX_VERTEX,
                shaders::TEMPORAL_BLUR_MIX_FRAGMENT,
            )?;
            let temporal_blur_mix_uniforms = TemporalMixUniforms {
                rect: get_uniform_loc(gl, temporal_blur_mix_program, "u_rect"),
                projection: get_uniform_loc(gl, temporal_blur_mix_program, "u_projection"),
                current: get_uniform_loc(gl, temporal_blur_mix_program, "u_current_blur"),
                previous: get_uniform_loc(gl, temporal_blur_mix_program, "u_previous_blur"),
                mix: get_uniform_loc(gl, temporal_blur_mix_program, "u_temporal_mix"),
            };
            let line_program = construction
                .compile_program(shaders::LINE_VERTEX_SHADER, shaders::LINE_FRAGMENT_SHADER)?;
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
                desat: get_uniform_loc(gl, program, "u_desat"),
                uv_rect: get_uniform_loc(gl, program, "u_uv_rect"),
                ripple_progress: get_uniform_loc(gl, program, "u_ripple_progress"),
                ripple_amplitude: get_uniform_loc(gl, program, "u_ripple_amplitude"),
                color_managed: get_uniform_loc(gl, program, "u_color_managed"),
                color_matrix: get_uniform_loc(gl, program, "u_color_matrix"),
                decode_tf: get_uniform_loc(gl, program, "u_decode_tf"),
                decode_gamma: get_uniform_loc(gl, program, "u_decode_gamma"),
                encode_tf: get_uniform_loc(gl, program, "u_encode_tf"),
                encode_gamma: get_uniform_loc(gl, program, "u_encode_gamma"),
                scene_linear: get_uniform_loc(gl, program, "u_scene_linear"),
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
                radius_top: get_uniform_loc(gl, border_program, "u_radius_top"),
                border_width: get_uniform_loc(gl, border_program, "u_border_width"),
                scene_linear: get_uniform_loc(gl, border_program, "u_scene_linear"),
            };

            let gradient_border_uniforms = GradientBorderUniforms {
                rect: get_uniform_loc(gl, gradient_border_program, "u_rect"),
                projection: get_uniform_loc(gl, gradient_border_program, "u_projection"),
                color_a: get_uniform_loc(gl, gradient_border_program, "u_color_a"),
                color_b: get_uniform_loc(gl, gradient_border_program, "u_color_b"),
                gradient_angle: get_uniform_loc(gl, gradient_border_program, "u_gradient_angle"),
                size: get_uniform_loc(gl, gradient_border_program, "u_size"),
                radius: get_uniform_loc(gl, gradient_border_program, "u_radius"),
                radius_top: get_uniform_loc(gl, gradient_border_program, "u_radius_top"),
                border_width: get_uniform_loc(gl, gradient_border_program, "u_border_width"),
                scene_linear: get_uniform_loc(gl, gradient_border_program, "u_scene_linear"),
            };

            let glass_uniforms = GlassUniforms {
                rect: get_uniform_loc(gl, glass_program, "u_rect"),
                projection: get_uniform_loc(gl, glass_program, "u_projection"),
                backdrop: get_uniform_loc(gl, glass_program, "u_backdrop"),
                screen_size: get_uniform_loc(gl, glass_program, "u_screen_size"),
                tint: get_uniform_loc(gl, glass_program, "u_tint"),
                size: get_uniform_loc(gl, glass_program, "u_size"),
                radius: get_uniform_loc(gl, glass_program, "u_radius"),
                radius_top: get_uniform_loc(gl, glass_program, "u_radius_top"),
                corner_exp: get_uniform_loc(gl, glass_program, "u_corner_exp"),
                saturation: get_uniform_loc(gl, glass_program, "u_saturation"),
                luminance: get_uniform_loc(gl, glass_program, "u_luminance"),
                bevel_width: get_uniform_loc(gl, glass_program, "u_bevel_width"),
                refraction: get_uniform_loc(gl, glass_program, "u_refraction"),
                rim_width: get_uniform_loc(gl, glass_program, "u_rim_width"),
                rim_intensity: get_uniform_loc(gl, glass_program, "u_rim_intensity"),
                rim_tint: get_uniform_loc(gl, glass_program, "u_rim_tint"),
                sheen: get_uniform_loc(gl, glass_program, "u_sheen"),
                edge_shade: get_uniform_loc(gl, glass_program, "u_edge_shade"),
                grain: get_uniform_loc(gl, glass_program, "u_grain"),
                alpha: get_uniform_loc(gl, glass_program, "u_alpha"),
                scene_linear: get_uniform_loc(gl, glass_program, "u_scene_linear"),
            };

            let postprocess_uniforms = PostprocessUniforms {
                rect: get_uniform_loc(gl, postprocess_program, "u_rect"),
                projection: get_uniform_loc(gl, postprocess_program, "u_projection"),
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
                tone_mapping_method: get_uniform_loc(
                    gl,
                    postprocess_program,
                    "u_tone_mapping_method",
                ),
            };

            let scene_linear_encode_uniforms = SceneLinearEncodeUniforms {
                rect: get_uniform_loc(gl, scene_linear_encode_program, "u_rect"),
                projection: get_uniform_loc(gl, scene_linear_encode_program, "u_projection"),
                texture: get_uniform_loc(gl, scene_linear_encode_program, "u_texture"),
                encode_tf: get_uniform_loc(gl, scene_linear_encode_program, "u_encode_tf"),
                encode_gamma: get_uniform_loc(gl, scene_linear_encode_program, "u_encode_gamma"),
            };

            let scene_linear_decode_uniforms = SceneLinearDecodeUniforms {
                rect: get_uniform_loc(gl, scene_linear_decode_program, "u_rect"),
                projection: get_uniform_loc(gl, scene_linear_decode_program, "u_projection"),
                texture: get_uniform_loc(gl, scene_linear_decode_program, "u_texture"),
            };

            let transition_uniforms = TransitionUniforms {
                rect: get_uniform_loc(gl, transition_program, "u_rect"),
                projection: get_uniform_loc(gl, transition_program, "u_projection"),
                opacity: get_uniform_loc(gl, transition_program, "u_opacity"),
                uv_rect: get_uniform_loc(gl, transition_program, "u_uv_rect"),
            };

            let cube_uniforms = CubeUniforms {
                mvp: get_uniform_loc(gl, cube_program, "u_mvp"),
                model: get_uniform_loc(gl, cube_program, "u_model"),
                texture: get_uniform_loc(gl, cube_program, "u_texture"),
                brightness: get_uniform_loc(gl, cube_program, "u_brightness"),
                uv_rect: get_uniform_loc(gl, cube_program, "u_uv_rect"),
                aspect: get_uniform_loc(gl, cube_program, "u_aspect"),
                camera: get_uniform_loc(gl, cube_program, "u_camera"),
                accent: get_uniform_loc(gl, cube_program, "u_accent"),
                alpha: get_uniform_loc(gl, cube_program, "u_alpha"),
                desat: get_uniform_loc(gl, cube_program, "u_desat"),
                edge: get_uniform_loc(gl, cube_program, "u_edge"),
                lit: get_uniform_loc(gl, cube_program, "u_lit"),
                scene_linear: get_uniform_loc(gl, cube_program, "u_scene_linear"),
                has_alpha: get_uniform_loc(gl, cube_program, "u_has_alpha"),
                filler: get_uniform_loc(gl, cube_program, "u_filler"),
                reflection: get_uniform_loc(gl, cube_program, "u_reflection"),
                floor_y: get_uniform_loc(gl, cube_program, "u_floor_y"),
                color_managed: get_uniform_loc(gl, cube_program, "u_color_managed"),
                color_matrix: get_uniform_loc(gl, cube_program, "u_color_matrix"),
                decode_tf: get_uniform_loc(gl, cube_program, "u_decode_tf"),
                decode_gamma: get_uniform_loc(gl, cube_program, "u_decode_gamma"),
                encode_tf: get_uniform_loc(gl, cube_program, "u_encode_tf"),
                encode_gamma: get_uniform_loc(gl, cube_program, "u_encode_gamma"),
            };

            let overview_cap_uniforms = OverviewCapUniforms {
                mvp: get_uniform_loc(gl, overview_cap_program, "u_mvp"),
                model: get_uniform_loc(gl, overview_cap_program, "u_model"),
                radius: get_uniform_loc(gl, overview_cap_program, "u_radius"),
                y: get_uniform_loc(gl, overview_cap_program, "u_y"),
                sides: get_uniform_loc(gl, overview_cap_program, "u_sides"),
                color: get_uniform_loc(gl, overview_cap_program, "u_color"),
                accent: get_uniform_loc(gl, overview_cap_program, "u_accent"),
                camera: get_uniform_loc(gl, overview_cap_program, "u_camera"),
                scene_linear: get_uniform_loc(gl, overview_cap_program, "u_scene_linear"),
                reflection: get_uniform_loc(gl, overview_cap_program, "u_reflection"),
                floor_y: get_uniform_loc(gl, overview_cap_program, "u_floor_y"),
            };

            let overview_skydome_uniforms = OverviewSkydomeUniforms {
                rect: get_uniform_loc(gl, overview_skydome_program, "u_rect"),
                projection: get_uniform_loc(gl, overview_skydome_program, "u_projection"),
                opacity: get_uniform_loc(gl, overview_skydome_program, "u_opacity"),
                angle: get_uniform_loc(gl, overview_skydome_program, "u_angle"),
                ground: get_uniform_loc(gl, overview_skydome_program, "u_ground"),
                accent: get_uniform_loc(gl, overview_skydome_program, "u_accent"),
                scene_linear: get_uniform_loc(gl, overview_skydome_program, "u_scene_linear"),
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
                scene_linear: get_uniform_loc(gl, tilt_program, "u_scene_linear"),
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
                color_managed: get_uniform_loc(gl, wobbly_program, "u_color_managed"),
                scene_linear: get_uniform_loc(gl, wobbly_program, "u_scene_linear"),
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
                dock_size: get_uniform_loc(gl, genie_program, "u_dock_size"),
                grid_size: get_uniform_loc(gl, genie_program, "u_grid_size"),
                ripple_progress: get_uniform_loc(gl, genie_program, "u_ripple_progress"),
                ripple_amplitude: get_uniform_loc(gl, genie_program, "u_ripple_amplitude"),
                color_managed: get_uniform_loc(gl, genie_program, "u_color_managed"),
                color_matrix: get_uniform_loc(gl, genie_program, "u_color_matrix"),
                decode_tf: get_uniform_loc(gl, genie_program, "u_decode_tf"),
                decode_gamma: get_uniform_loc(gl, genie_program, "u_decode_gamma"),
                encode_tf: get_uniform_loc(gl, genie_program, "u_encode_tf"),
                encode_gamma: get_uniform_loc(gl, genie_program, "u_encode_gamma"),
                scene_linear: get_uniform_loc(gl, genie_program, "u_scene_linear"),
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

            // ----- Create quad VAO/VBO -----
            //
            // Keep attribute 0 backed by a tiny static buffer. Several quad
            // shaders consume this directly, and some GLES drivers validate
            // VAO/VBO/pointer state aggressively even for shaders that do not.
            let quad_vao = construction.create_vertex_array("quad VAO")?;
            let quad_vbo = construction.create_buffer("quad VBO")?;
            let quad_vertices: [f32; 8] = [
                0.0, 0.0, //
                1.0, 0.0, //
                0.0, 1.0, //
                1.0, 1.0,
            ];
            gl.BindVertexArray(quad_vao);
            gl.BindBuffer(ffi::ARRAY_BUFFER, quad_vbo);
            gl.BufferData(
                ffi::ARRAY_BUFFER,
                (quad_vertices.len() * std::mem::size_of::<f32>()) as isize,
                quad_vertices.as_ptr() as *const _,
                ffi::STATIC_DRAW,
            );
            gl.EnableVertexAttribArray(0);
            gl.VertexAttribPointer(0, 2, ffi::FLOAT, ffi::FALSE as u8, 8, std::ptr::null());
            gl.BindBuffer(ffi::ARRAY_BUFFER, 0);
            gl.BindVertexArray(0);

            // ----- Create output FBO + texture -----
            let output_internal_format = if hdr_10bit { GL_RGB10_A2 } else { ffi::RGBA8 };
            let (output_fbo, output_texture) = construction.create_required_fbo_texture(
                screen_w,
                screen_h,
                output_internal_format,
                "output",
            )?;

            // ----- SOTA #2 Phase 2.1: optional FP16 linear-scene FBO -----
            // Allocated only when both the color-management render path and
            // behavior.scene_linear_compositing are on.
            // Zero/zero sentinel when off; the render path (Phase 2.2) checks
            // for linear_fbo != 0 to decide whether to take the linear path.
            let scene_linear_enabled = {
                let config = crate::config::CONFIG.load();
                let behavior = config.behavior();
                crate::config::scene_linear_render_path_requested(
                    behavior.color_management_render_path,
                    behavior.scene_linear_compositing,
                )
            };
            let (linear_fbo, linear_texture) = if scene_linear_enabled {
                match construction.create_fbo_texture(screen_w, screen_h, GL_RGBA16F) {
                    Ok(resources) => resources,
                    Err(status) => {
                        log::warn!(
                            "[udev/compositor] RGBA16F scene-linear target is unavailable \
                             (status=0x{status:x}); falling back to encoded-space compositing"
                        );
                        (0, 0)
                    }
                }
            } else {
                (0, 0)
            };

            // When the output is 10-bit, keep the whole offscreen chain (scene
            // capture, blur, postprocess, transition) at 10-bit too — an 8-bit
            // intermediate would reintroduce banding before the final 10-bit blit.
            // ----- Create scene FBO + texture -----
            let (scene_fbo, scene_texture) = construction.create_required_fbo_texture(
                screen_w,
                screen_h,
                output_internal_format,
                "scene",
            )?;

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
                let (fbo, texture) = construction.create_required_fbo_texture(
                    bw,
                    bh,
                    output_internal_format,
                    "blur",
                )?;
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
            let (postprocess_fbo, postprocess_texture) = construction.create_required_fbo_texture(
                screen_w,
                screen_h,
                output_internal_format,
                "postprocess",
            )?;

            // ----- Create transition FBO + texture -----
            let (transition_fbo, transition_texture) = construction.create_required_fbo_texture(
                screen_w,
                screen_h,
                output_internal_format,
                "transition",
            )?;

            // ----- Create particle VAO + VBO -----
            let particle_vao = construction.create_vertex_array("particle VAO")?;
            let particle_vbo = construction.create_buffer("particle VBO")?;

            // ----- Unbind -----
            gl.BindFramebuffer(ffi::FRAMEBUFFER, 0);
            gl.BindTexture(ffi::TEXTURE_2D, 0);

            let now = Instant::now();

            let compositor = Self {
                // Shader programs
                program,
                shadow_program,
                blur_down_program,
                blur_up_program,
                border_program,
                gradient_border_program,
                postprocess_program,
                scene_linear_encode_program,
                scene_linear_decode_program,
                scene_linear_encode_uniforms,
                scene_linear_decode_uniforms,
                transition_program,
                cube_program,
                overview_cap_program,
                portal_program,
                edge_glow_program,
                tilt_program,
                wobbly_program,
                genie_program,
                particle_program,
                overview_bg_program,
                overview_skydome_program,
                glass_program,
                hud_program,
                sysui_text_program,
                temporal_blur_mix_program,
                glass_backdrop: None,

                // Uniform locations
                win_uniforms,
                shadow_uniforms,
                blur_uniforms,
                border_uniforms,
                gradient_border_uniforms,
                glass_uniforms,
                postprocess_uniforms,
                transition_uniforms,
                cube_uniforms,
                overview_cap_uniforms,
                overview_skydome_uniforms,
                portal_uniforms,
                tilt_uniforms,
                wobbly_uniforms,
                genie_uniforms,
                edge_glow_uniforms,

                // GL resources
                quad_vao,
                quad_vbo,
                output_fbo,
                output_texture,
                output_internal_format,
                output_texture_generation: next_output_texture_generation(),
                // Treat a failed FP16 allocation as disabled. In particular,
                // never advertise the scene-linear path to damage/KMS-offload
                // code when there is no complete render target behind it.
                // A later config toggle or compositor recreation can retry.
                scene_linear_requested: scene_linear_enabled && linear_fbo != 0,
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
                gpu_resources_released: false,

                // Dimensions
                screen_w,
                screen_h,

                // Per-window state
                windows: HashMap::new(),
                minimized_window_metadata: HashMap::new(),
                pending_window_urgency: PendingWindowUrgency::default(),
                any_color_transform_active: false,

                // Config defaults — intentionally conservative; apply_config() reads config.toml
                corner_radius: 0.0,
                shadow_enabled: false,
                shadow_radius: 24.0,
                shadow_offset: [4.0, 4.0],
                shadow_color: [0.0, 0.0, 0.0, 0.5],
                shadow_inactive_opacity: 0.65,
                shadow_spread: 20.0,
                inactive_opacity: 1.0,
                active_opacity: 1.0,
                inactive_dim: 1.0,
                inactive_desaturate: 0.25,
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
                transition_snapshot_pending: false,
                transition_start: None,
                transition_duration: Duration::from_millis(300),
                transition_mode: TransitionMode::None,
                transition_direction: 0,

                // Overview
                overview_enabled: false,
                overview_active: false,
                overview_opacity: 0.0,
                overview_entries: Vec::new(),
                overview_selection: None,
                overview_monitor: (0, 0, screen_w, screen_h),
                overview_rotation: 0.0,
                overview_target_rotation: 0.0,
                overview_title_textures: Vec::new(),
                overview_titles_dirty: false,

                // Expose
                expose_enabled: false,
                expose_active: false,
                expose_opacity: 0.0,
                expose_entries: Vec::new(),

                // Snap preview
                snap_preview_enabled: false,
                snap_preview: None,
                snap_preview_target_visible: false,
                snap_preview_opacity: 0.0,

                // Peek mode
                peek_enabled: false,
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
                tilt_amount: 0.26,
                tilt_perspective: 800.0,

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
                sys_stats: crate::backend::sys_stats::SysStatsSampler::new(),

                // Optimization
                needs_render: true,
                last_frame_time: now,
                effect_clock_active: false,
                frame_count: 0,
                fps: 0.0,
                prev_scene: Vec::new(),
                scratch_curr_ids: HashSet::new(),
                scratch_prev_geom: HashMap::new(),
                scratch_scanout: Vec::new(),
                scratch_wobbly_flat: Vec::new(),
                scratch_particle_data: Vec::new(),
                scratch_retired_aux_ids: Vec::new(),

                // Dock position
                dock_x: screen_w as f32 * 0.5,
                dock_y: screen_h.saturating_sub(1) as f32,

                // Genie animations
                genie_active: Vec::new(),
                genie_targets: HashMap::new(),
                minimized_windows: HashSet::new(),
                minimized_visuals: HashMap::new(),
                minimized_thumbnails,
                pending_minimized_visuals: HashSet::new(),
                pending_genie_restores: HashSet::new(),
                dock_preview: None,

                // Window groups
                window_groups: Vec::new(),
                tab_title_textures: Vec::new(),
                tab_titles_dirty: false,

                // Monitors
                monitors: Vec::new(),

                // Zoom to fit
                zoom_to_fit_window: None,

                // Annotations
                annotation_active: false,
                annotation_strokes: Vec::new(),
                annotation_quads: Vec::new(),
                annotation_labels: Vec::new(),
                annotation_label_textures: Vec::new(),
                annotation_labels_dirty: false,
                annotation_color: [1.0, 0.0, 0.0, 1.0],
                annotation_line_width: 3.0,
                screenshot_toolbar: None,
                screenshot_toolbar_icons: Vec::new(),
                screenshot_toolbar_dirty: false,
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
                retired_wallpaper_textures: Vec::new(),
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

                // Border config
                border_enabled: true,
                border_width: 2.0,
                border_color_focused: [0.3, 0.6, 1.0, 0.8],
                border_color_unfocused: [0.3, 0.3, 0.3, 0.5],
                border_gradient_enabled: true,
                border_gradient_color_a: [0.24, 0.65, 1.0, 1.0],
                border_gradient_color_b: [0.72, 0.35, 1.0, 1.0],
                border_gradient_angle: 45.0,
                border_gradient_speed: 0.0,

                // Screenshot
                screenshot_requests: Default::default(),
                screenshot_readback: screenshot_readback::ScreenshotReadback::new(),

                // Recording control
                pending_recording_start: None,
                pending_recording_stop: false,
                recording_region_overlay: None,

                // Debug HUD extended
                debug_hud_extended: false,
                hud_textures: [None; 4],
                sysui_textures: [None; 4],
                sysui_cache: String::new(),
                toast_stack: Default::default(),
                toast_textures: HashMap::new(),
                toast_retired: Vec::new(),
                osd_slot: Default::default(),
                osd_texture: None,
                hud_text_cache: String::new(),
                system_ui: None,
                system_ui_island: Default::default(),
                hud_island: Default::default(),
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
                transition_exclude_top: 0,

                // Render stats & texture pool
                render_stats: render_stats::RenderStats::new(),
                texture_pool: texture_pool::TexturePool::new(),
            };

            if construction.should_fail_before_commit() {
                return Err("injected compositor construction failure before commit".into());
            }
            construction.commit();
            Ok(compositor)
        }
    }
}

unsafe fn delete_program_name(gl: &ffi::Gles2, name: &mut u32) {
    let name = std::mem::take(name);
    if name != 0 {
        unsafe { gl.DeleteProgram(name) };
    }
}

unsafe fn delete_texture_name(gl: &ffi::Gles2, name: &mut u32) {
    let name = std::mem::take(name);
    if name != 0 {
        unsafe { gl.DeleteTextures(1, &name) };
    }
}

unsafe fn delete_framebuffer_name(gl: &ffi::Gles2, name: &mut u32) {
    let name = std::mem::take(name);
    if name != 0 {
        unsafe { gl.DeleteFramebuffers(1, &name) };
    }
}

unsafe fn delete_buffer_name(gl: &ffi::Gles2, name: &mut u32) {
    let name = std::mem::take(name);
    if name != 0 {
        unsafe { gl.DeleteBuffers(1, &name) };
    }
}

unsafe fn delete_vertex_array_name(gl: &ffi::Gles2, name: &mut u32) {
    let name = std::mem::take(name);
    if name != 0 {
        unsafe { gl.DeleteVertexArrays(1, &name) };
    }
}

impl WaylandCompositor {
    /// Release every raw GLES object owned by this compositor while the KMS
    /// renderer's EGL context is current.
    ///
    /// Smithay `GlesTexture` values in WindowState/Genie/retained visuals are
    /// deliberately excluded: their strong owners are dropped normally and
    /// enqueue renderer-managed cleanup. The output texture is the one mixed
    /// ownership exception and follows the explicit ownership argument.
    ///
    /// Returns `true` only for the first release. Every scalar handle is set to
    /// zero and every container is drained, while the guard makes a repeated
    /// call a strict no-op.
    pub(crate) unsafe fn release_gpu_resources(
        &mut self,
        gl: &ffi::Gles2,
        output_texture_ownership: CompositorOutputTextureOwnership,
    ) -> bool {
        if std::mem::replace(&mut self.gpu_resources_released, true) {
            return false;
        }

        unsafe {
            // Jobs and synchronization objects may still reference the main
            // render targets. Retire them before deleting any target storage.
            self.recording.stop(gl);
            self.screenshot_readback.clear(gl);
            self.gpu_fence_sync_mgr.clear(gl);
            self.pbo_uploader.clear(gl);

            // Do not leave Smithay's renderer with bindings to names which are
            // about to be deleted from under it.
            gl.UseProgram(0);
            gl.BindVertexArray(0);
            gl.BindBuffer(ffi::ARRAY_BUFFER, 0);
            gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, 0);
            gl.BindBuffer(ffi::PIXEL_UNPACK_BUFFER, 0);
            gl.BindFramebuffer(ffi::FRAMEBUFFER, 0);
            gl.ActiveTexture(ffi::TEXTURE0);
            gl.BindTexture(ffi::TEXTURE_2D, 0);

            // `glass_backdrop` aliases blur_fbos[0].texture; it is never an
            // independent owner.
            self.glass_backdrop = None;

            self.clear_overview_textures(gl);
            for row in self.tab_title_textures.drain(..) {
                for (texture, _, _) in row.into_iter().flatten() {
                    if texture != 0 {
                        gl.DeleteTextures(1, &texture);
                    }
                }
            }
            for (texture, _, _) in self.annotation_label_textures.drain(..).flatten() {
                if texture != 0 {
                    gl.DeleteTextures(1, &texture);
                }
            }
            for (texture, _, _) in self.screenshot_toolbar_icons.drain(..).flatten() {
                if texture != 0 {
                    gl.DeleteTextures(1, &texture);
                }
            }
            for slot in self
                .hud_textures
                .iter_mut()
                .chain(self.sysui_textures.iter_mut())
            {
                if let Some((texture, _, _)) = slot.take()
                    && texture != 0
                {
                    gl.DeleteTextures(1, &texture);
                }
            }
            for (_, slots) in self.toast_textures.drain() {
                for (texture, _, _) in slots.into_iter().flatten() {
                    if texture != 0 {
                        gl.DeleteTextures(1, &texture);
                    }
                }
            }
            if let Some((_, texture, _, _)) = self.osd_texture.take()
                && texture != 0
            {
                gl.DeleteTextures(1, &texture);
            }

            if let Some(texture) = self.wallpaper_texture.take()
                && texture != 0
            {
                gl.DeleteTextures(1, &texture);
            }
            if let Some(texture) = self.old_wallpaper_texture.take()
                && texture != 0
            {
                gl.DeleteTextures(1, &texture);
            }
            for wallpaper in &mut self.monitor_wallpapers {
                if let Some(texture) = wallpaper.texture.take()
                    && texture != 0
                {
                    gl.DeleteTextures(1, &texture);
                }
            }
            for texture in self.retired_wallpaper_textures.drain(..) {
                if texture != 0 {
                    gl.DeleteTextures(1, &texture);
                }
            }
            self.texture_pool.clear(gl);

            if let Some((mut framebuffer, mut texture)) = self.prev_blur_fbo.take() {
                delete_framebuffer_name(gl, &mut framebuffer);
                delete_texture_name(gl, &mut texture);
            }
            if let Some((mut framebuffer, mut texture)) = self.temporal_mix_fbo.take() {
                delete_framebuffer_name(gl, &mut framebuffer);
                delete_texture_name(gl, &mut texture);
            }
            delete_framebuffer_name(gl, &mut self.blur_blit_src_fbo);

            for mut level in self.blur_fbos.drain(..) {
                delete_framebuffer_name(gl, &mut level.fbo);
                delete_texture_name(gl, &mut level.texture);
            }
            delete_framebuffer_name(gl, &mut self.output_fbo);
            delete_framebuffer_name(gl, &mut self.linear_fbo);
            delete_framebuffer_name(gl, &mut self.scene_fbo);
            delete_framebuffer_name(gl, &mut self.postprocess_fbo);
            delete_framebuffer_name(gl, &mut self.transition_fbo);

            match output_texture_ownership {
                CompositorOutputTextureOwnership::RawCompositor => {
                    delete_texture_name(gl, &mut self.output_texture);
                }
                CompositorOutputTextureOwnership::SmithayRenderer => {
                    // KMS drops the exact generation's GlesTexture wrapper
                    // after this method returns. Clearing the raw alias here
                    // prevents any accidental second delete.
                    self.output_texture = 0;
                }
            }
            delete_texture_name(gl, &mut self.linear_texture);
            delete_texture_name(gl, &mut self.scene_texture);
            delete_texture_name(gl, &mut self.postprocess_texture);
            delete_texture_name(gl, &mut self.transition_texture);

            delete_buffer_name(gl, &mut self.particle_vbo);
            delete_buffer_name(gl, &mut self.quad_vbo);
            delete_vertex_array_name(gl, &mut self.particle_vao);
            delete_vertex_array_name(gl, &mut self.quad_vao);

            // Thumbnail owns an independent raw texture tier and one program.
            self.minimized_thumbnails.release_gpu_resources(gl);

            for program in [
                &mut self.program,
                &mut self.shadow_program,
                &mut self.blur_down_program,
                &mut self.blur_up_program,
                &mut self.border_program,
                &mut self.gradient_border_program,
                &mut self.postprocess_program,
                &mut self.scene_linear_encode_program,
                &mut self.scene_linear_decode_program,
                &mut self.transition_program,
                &mut self.cube_program,
                &mut self.overview_cap_program,
                &mut self.portal_program,
                &mut self.edge_glow_program,
                &mut self.tilt_program,
                &mut self.wobbly_program,
                &mut self.genie_program,
                &mut self.particle_program,
                &mut self.overview_bg_program,
                &mut self.overview_skydome_program,
                &mut self.glass_program,
                &mut self.hud_program,
                &mut self.sysui_text_program,
                &mut self.temporal_blur_mix_program,
                &mut self.line_program,
            ] {
                delete_program_name(gl, program);
            }
        }
        true
    }
}

// ---------------------------------------------------------------------------
// Drop - runtime toggles call release_gpu_resources from KMS::with_renderer.
// KMS/context destruction may still drop this value with no current context,
// in which case the context itself reclaims any remaining raw objects.
// ---------------------------------------------------------------------------

impl Drop for WaylandCompositor {
    fn drop(&mut self) {
        // Intentionally empty: calling GL from Drop would be invalid on the KMS
        // teardown path. Runtime disable performs the explicit current-context
        // release before dropping this value.
    }
}

#[cfg(test)]
mod gpu_release_contract_tests {
    use std::collections::BTreeSet;

    // Persistent raw-GLES ownership manifest. Smithay GlesTexture fields are
    // intentionally absent; `glass_backdrop` is present only so the teardown
    // contract locks in its alias-only treatment.
    const RAW_GPU_OWNER_FIELDS: &[&str] = &[
        "program",
        "shadow_program",
        "blur_down_program",
        "blur_up_program",
        "border_program",
        "gradient_border_program",
        "postprocess_program",
        "scene_linear_encode_program",
        "scene_linear_decode_program",
        "transition_program",
        "cube_program",
        "overview_cap_program",
        "portal_program",
        "edge_glow_program",
        "tilt_program",
        "wobbly_program",
        "genie_program",
        "particle_program",
        "overview_bg_program",
        "overview_skydome_program",
        "glass_program",
        "hud_program",
        "sysui_text_program",
        "temporal_blur_mix_program",
        "line_program",
        "quad_vao",
        "quad_vbo",
        "particle_vao",
        "particle_vbo",
        "output_fbo",
        "output_texture",
        "linear_fbo",
        "linear_texture",
        "scene_fbo",
        "scene_texture",
        "blur_fbos",
        "postprocess_fbo",
        "postprocess_texture",
        "transition_fbo",
        "transition_texture",
        "prev_blur_fbo",
        "temporal_mix_fbo",
        "blur_blit_src_fbo",
        "glass_backdrop",
        "overview_title_textures",
        "tab_title_textures",
        "annotation_label_textures",
        "screenshot_toolbar_icons",
        "hud_textures",
        "sysui_textures",
        "toast_textures",
        "osd_texture",
        "wallpaper_texture",
        "old_wallpaper_texture",
        "monitor_wallpapers",
        "retired_wallpaper_textures",
        "recording",
        "screenshot_readback",
        "gpu_fence_sync_mgr",
        "pbo_uploader",
        "texture_pool",
        "minimized_thumbnails",
    ];

    fn braced_item_after<'a>(source: &'a str, needle: &str) -> &'a str {
        let start = source.find(needle).expect("source item missing");
        let open = start
            + source[start..]
                .find('{')
                .expect("source item has no opening brace");
        let mut depth = 0usize;
        for (offset, byte) in source[open..].bytes().enumerate() {
            match byte {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return &source[start..open + offset + 1];
                    }
                }
                _ => {}
            }
        }
        panic!("source item has no closing brace");
    }

    #[test]
    fn raw_gpu_owner_manifest_is_unique_and_fully_referenced_by_teardown() {
        let unique = RAW_GPU_OWNER_FIELDS
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        assert_eq!(unique.len(), RAW_GPU_OWNER_FIELDS.len());

        let source = include_str!("mod.rs");
        let release = braced_item_after(source, "pub(crate) unsafe fn release_gpu_resources(");
        let compact_release = release
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>();
        for field in RAW_GPU_OWNER_FIELDS {
            let release_reference = match *field {
                "overview_title_textures" => "self.clear_overview_textures(gl)".to_string(),
                _ => format!("self.{field}"),
            };
            assert!(
                compact_release.contains(&release_reference),
                "raw GPU owner `{field}` is absent from release_gpu_resources"
            );
        }
        assert!(compact_release.contains("self.glass_backdrop=None"));
        assert!(!compact_release.contains("delete_texture_name(gl,&mutself.glass_backdrop"));
    }

    #[test]
    fn every_scalar_gl_name_in_the_compositor_struct_is_manifested() {
        let source = include_str!("mod.rs");
        let compositor = braced_item_after(source, "pub(crate) struct WaylandCompositor");
        let manifested = RAW_GPU_OWNER_FIELDS
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        for line in compositor.lines() {
            let line = line.trim();
            let Some((field, ty)) = line.split_once(':') else {
                continue;
            };
            let field = field.trim();
            let ty = ty.trim().trim_end_matches(',');
            let looks_like_owned_gl_name = ty == "u32"
                && (field.ends_with("_program")
                    || field.ends_with("_vao")
                    || field.ends_with("_vbo")
                    || field.ends_with("_fbo")
                    || field.ends_with("_texture")
                    || field == "program");
            if looks_like_owned_gl_name {
                assert!(
                    manifested.contains(field),
                    "new scalar raw GL field `{field}` needs an explicit teardown decision"
                );
            }
        }
    }

    #[test]
    fn constructor_routes_every_raw_allocation_through_the_rollback_guard() {
        let source = include_str!("mod.rs");
        let constructor = braced_item_after(source, "unsafe fn new_inner(");
        let compact = constructor
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>();

        assert_eq!(
            compact.matches("construction.compile_program(").count(),
            26,
            "all 25 compositor programs plus thumbnail must be guard-owned"
        );
        assert!(compact.contains("MinimizedThumbnailState::from_program(gl,thumbnail_program)"));
        assert_eq!(
            compact.matches("construction.create_vertex_array(").count(),
            2
        );
        assert_eq!(compact.matches("construction.create_buffer(").count(), 2);
        assert_eq!(
            compact
                .matches("construction.create_required_fbo_texture(")
                .count(),
            5,
            "output, scene, blur-loop, postprocess and transition allocations must be guarded"
        );
        assert!(compact.contains("construction.create_fbo_texture(screen_w,screen_h,GL_RGBA16F)"));

        for bypass in [
            "create_program(gl,",
            "create_fbo_texture(gl,",
            "create_fbo_texture_10bit(gl,",
            "gl.GenVertexArrays(",
            "gl.GenBuffers(",
        ] {
            assert!(
                !compact.contains(bypass),
                "constructor bypasses rollback guard through `{bypass}`"
            );
        }

        let aggregate = compact
            .find("letcompositor=Self{")
            .expect("constructor aggregate missing");
        let injected_failure = compact
            .find("construction.should_fail_before_commit()")
            .expect("pre-commit failure hook missing");
        let commit = compact
            .find("construction.commit()")
            .expect("construction ownership commit missing");
        let success = compact
            .rfind("Ok(compositor)")
            .expect("success return missing");
        assert!(aggregate < injected_failure && injected_failure < commit && commit < success);
    }
}

/// Raw-GLES equivalent of the X11 thumbnail state guard. The udev compositor
/// can call thumbnail capture between render passes, so resetting to a guessed
/// output FBO/program is not sufficient: callers may be in any nested pass.
struct ThumbnailGlesState<'a> {
    gl: &'a ffi::Gles2,
    draw_framebuffer: i32,
    read_framebuffer: i32,
    viewport: [i32; 4],
    program: i32,
    vertex_array: i32,
    active_texture: i32,
    active_texture_binding: i32,
    texture0_binding: i32,
    pixel_pack_buffer: i32,
    pixel_unpack_buffer: i32,
    pack_alignment: i32,
    unpack_alignment: i32,
    blend_enabled: bool,
    scissor_enabled: bool,
    depth_test_enabled: bool,
    stencil_test_enabled: bool,
    cull_face_enabled: bool,
    color_mask: [u8; 4],
    clear_color: [f32; 4],
}

impl<'a> ThumbnailGlesState<'a> {
    unsafe fn begin(gl: &'a ffi::Gles2) -> Self {
        unsafe {
            let mut draw_framebuffer = 0;
            let mut read_framebuffer = 0;
            let mut viewport = [0; 4];
            let mut program = 0;
            let mut vertex_array = 0;
            let mut active_texture = 0;
            let mut active_texture_binding = 0;
            let mut texture0_binding = 0;
            let mut pixel_pack_buffer = 0;
            let mut pixel_unpack_buffer = 0;
            let mut pack_alignment = 0;
            let mut unpack_alignment = 0;
            let mut color_mask = [ffi::FALSE; 4];
            let mut clear_color = [0.0; 4];

            gl.GetIntegerv(ffi::DRAW_FRAMEBUFFER_BINDING, &mut draw_framebuffer);
            gl.GetIntegerv(ffi::READ_FRAMEBUFFER_BINDING, &mut read_framebuffer);
            gl.GetIntegerv(ffi::VIEWPORT, viewport.as_mut_ptr());
            gl.GetIntegerv(ffi::CURRENT_PROGRAM, &mut program);
            gl.GetIntegerv(ffi::VERTEX_ARRAY_BINDING, &mut vertex_array);
            gl.GetIntegerv(ffi::ACTIVE_TEXTURE, &mut active_texture);
            gl.GetIntegerv(ffi::TEXTURE_BINDING_2D, &mut active_texture_binding);
            gl.ActiveTexture(ffi::TEXTURE0);
            gl.GetIntegerv(ffi::TEXTURE_BINDING_2D, &mut texture0_binding);
            gl.GetIntegerv(ffi::PIXEL_PACK_BUFFER_BINDING, &mut pixel_pack_buffer);
            gl.GetIntegerv(ffi::PIXEL_UNPACK_BUFFER_BINDING, &mut pixel_unpack_buffer);
            gl.GetIntegerv(ffi::PACK_ALIGNMENT, &mut pack_alignment);
            gl.GetIntegerv(ffi::UNPACK_ALIGNMENT, &mut unpack_alignment);
            gl.GetBooleanv(ffi::COLOR_WRITEMASK, color_mask.as_mut_ptr());
            gl.GetFloatv(ffi::COLOR_CLEAR_VALUE, clear_color.as_mut_ptr());
            let blend_enabled = gl.IsEnabled(ffi::BLEND) != ffi::FALSE;
            let scissor_enabled = gl.IsEnabled(ffi::SCISSOR_TEST) != ffi::FALSE;
            let depth_test_enabled = gl.IsEnabled(ffi::DEPTH_TEST) != ffi::FALSE;
            let stencil_test_enabled = gl.IsEnabled(ffi::STENCIL_TEST) != ffi::FALSE;
            let cull_face_enabled = gl.IsEnabled(ffi::CULL_FACE) != ffi::FALSE;

            gl.Disable(ffi::BLEND);
            gl.Disable(ffi::SCISSOR_TEST);
            gl.Disable(ffi::DEPTH_TEST);
            gl.Disable(ffi::STENCIL_TEST);
            gl.Disable(ffi::CULL_FACE);
            gl.ColorMask(ffi::TRUE, ffi::TRUE, ffi::TRUE, ffi::TRUE);
            gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, 0);
            gl.BindBuffer(ffi::PIXEL_UNPACK_BUFFER, 0);
            gl.PixelStorei(ffi::PACK_ALIGNMENT, 1);
            gl.PixelStorei(ffi::UNPACK_ALIGNMENT, 1);

            Self {
                gl,
                draw_framebuffer,
                read_framebuffer,
                viewport,
                program,
                vertex_array,
                active_texture,
                active_texture_binding,
                texture0_binding,
                pixel_pack_buffer,
                pixel_unpack_buffer,
                pack_alignment,
                unpack_alignment,
                blend_enabled,
                scissor_enabled,
                depth_test_enabled,
                stencil_test_enabled,
                cull_face_enabled,
                color_mask,
                clear_color,
            }
        }
    }
}

impl Drop for ThumbnailGlesState<'_> {
    fn drop(&mut self) {
        unsafe {
            self.gl
                .BindFramebuffer(ffi::DRAW_FRAMEBUFFER, self.draw_framebuffer.max(0) as u32);
            self.gl
                .BindFramebuffer(ffi::READ_FRAMEBUFFER, self.read_framebuffer.max(0) as u32);
            self.gl.Viewport(
                self.viewport[0],
                self.viewport[1],
                self.viewport[2],
                self.viewport[3],
            );
            self.gl.UseProgram(self.program.max(0) as u32);
            self.gl.BindVertexArray(self.vertex_array.max(0) as u32);

            self.gl.ActiveTexture(ffi::TEXTURE0);
            self.gl
                .BindTexture(ffi::TEXTURE_2D, self.texture0_binding.max(0) as u32);
            if self.active_texture != ffi::TEXTURE0 as i32 {
                self.gl.ActiveTexture(self.active_texture.max(0) as u32);
                self.gl
                    .BindTexture(ffi::TEXTURE_2D, self.active_texture_binding.max(0) as u32);
            }
            self.gl.ActiveTexture(self.active_texture.max(0) as u32);

            self.gl
                .BindBuffer(ffi::PIXEL_PACK_BUFFER, self.pixel_pack_buffer.max(0) as u32);
            self.gl.BindBuffer(
                ffi::PIXEL_UNPACK_BUFFER,
                self.pixel_unpack_buffer.max(0) as u32,
            );
            self.gl
                .PixelStorei(ffi::PACK_ALIGNMENT, self.pack_alignment);
            self.gl
                .PixelStorei(ffi::UNPACK_ALIGNMENT, self.unpack_alignment);
            self.gl.ColorMask(
                self.color_mask[0],
                self.color_mask[1],
                self.color_mask[2],
                self.color_mask[3],
            );
            self.gl.ClearColor(
                self.clear_color[0],
                self.clear_color[1],
                self.clear_color[2],
                self.clear_color[3],
            );
            if self.blend_enabled {
                self.gl.Enable(ffi::BLEND);
            } else {
                self.gl.Disable(ffi::BLEND);
            }
            if self.scissor_enabled {
                self.gl.Enable(ffi::SCISSOR_TEST);
            } else {
                self.gl.Disable(ffi::SCISSOR_TEST);
            }
            if self.depth_test_enabled {
                self.gl.Enable(ffi::DEPTH_TEST);
            } else {
                self.gl.Disable(ffi::DEPTH_TEST);
            }
            if self.stencil_test_enabled {
                self.gl.Enable(ffi::STENCIL_TEST);
            } else {
                self.gl.Disable(ffi::STENCIL_TEST);
            }
            if self.cull_face_enabled {
                self.gl.Enable(ffi::CULL_FACE);
            } else {
                self.gl.Disable(ffi::CULL_FACE);
            }
        }
    }
}

unsafe fn read_gles_uniform_f32<const N: usize>(
    gl: &ffi::Gles2,
    program: u32,
    location: i32,
) -> Option<[f32; N]> {
    if location < 0 {
        return None;
    }
    let mut value = [0.0; N];
    unsafe { gl.GetUniformfv(program, location, value.as_mut_ptr()) };
    Some(value)
}

unsafe fn read_gles_uniform_i32(gl: &ffi::Gles2, program: u32, location: i32) -> Option<i32> {
    if location < 0 {
        return None;
    }
    let mut value = 0;
    unsafe { gl.GetUniformiv(program, location, &mut value) };
    Some(value)
}

struct ThumbnailGlesUniformState<'a> {
    gl: &'a ffi::Gles2,
    program: u32,
    uniforms: &'a WindowUniforms,
    projection: Option<[f32; 16]>,
    rect: Option<[f32; 4]>,
    texture: Option<i32>,
    opacity: Option<f32>,
    radius: Option<f32>,
    size: Option<[f32; 2]>,
    dim: Option<f32>,
    desat: Option<f32>,
    uv_rect: Option<[f32; 4]>,
    ripple_progress: Option<f32>,
    ripple_amplitude: Option<f32>,
    color_managed: Option<i32>,
    color_matrix: Option<[f32; 9]>,
    decode_tf: Option<i32>,
    decode_gamma: Option<f32>,
    encode_tf: Option<i32>,
    encode_gamma: Option<f32>,
    scene_linear: Option<i32>,
}

impl<'a> ThumbnailGlesUniformState<'a> {
    unsafe fn capture(gl: &'a ffi::Gles2, program: u32, uniforms: &'a WindowUniforms) -> Self {
        unsafe {
            Self {
                gl,
                program,
                uniforms,
                projection: read_gles_uniform_f32(gl, program, uniforms.projection),
                rect: read_gles_uniform_f32(gl, program, uniforms.rect),
                texture: read_gles_uniform_i32(gl, program, uniforms.texture),
                opacity: read_gles_uniform_f32::<1>(gl, program, uniforms.opacity)
                    .map(|value| value[0]),
                radius: read_gles_uniform_f32::<1>(gl, program, uniforms.radius)
                    .map(|value| value[0]),
                size: read_gles_uniform_f32(gl, program, uniforms.size),
                dim: read_gles_uniform_f32::<1>(gl, program, uniforms.dim).map(|value| value[0]),
                desat: read_gles_uniform_f32::<1>(gl, program, uniforms.desat)
                    .map(|value| value[0]),
                uv_rect: read_gles_uniform_f32(gl, program, uniforms.uv_rect),
                ripple_progress: read_gles_uniform_f32::<1>(gl, program, uniforms.ripple_progress)
                    .map(|value| value[0]),
                ripple_amplitude: read_gles_uniform_f32::<1>(
                    gl,
                    program,
                    uniforms.ripple_amplitude,
                )
                .map(|value| value[0]),
                color_managed: read_gles_uniform_i32(gl, program, uniforms.color_managed),
                color_matrix: read_gles_uniform_f32(gl, program, uniforms.color_matrix),
                decode_tf: read_gles_uniform_i32(gl, program, uniforms.decode_tf),
                decode_gamma: read_gles_uniform_f32::<1>(gl, program, uniforms.decode_gamma)
                    .map(|value| value[0]),
                encode_tf: read_gles_uniform_i32(gl, program, uniforms.encode_tf),
                encode_gamma: read_gles_uniform_f32::<1>(gl, program, uniforms.encode_gamma)
                    .map(|value| value[0]),
                scene_linear: read_gles_uniform_i32(gl, program, uniforms.scene_linear),
            }
        }
    }
}

impl Drop for ThumbnailGlesUniformState<'_> {
    fn drop(&mut self) {
        unsafe {
            self.gl.UseProgram(self.program);
            if let Some(value) = self.projection {
                self.gl
                    .UniformMatrix4fv(self.uniforms.projection, 1, ffi::FALSE, value.as_ptr());
            }
            if let Some(value) = self.rect {
                self.gl
                    .Uniform4f(self.uniforms.rect, value[0], value[1], value[2], value[3]);
            }
            if let Some(value) = self.texture {
                self.gl.Uniform1i(self.uniforms.texture, value);
            }
            if let Some(value) = self.opacity {
                self.gl.Uniform1f(self.uniforms.opacity, value);
            }
            if let Some(value) = self.radius {
                self.gl.Uniform1f(self.uniforms.radius, value);
            }
            if let Some(value) = self.size {
                self.gl.Uniform2f(self.uniforms.size, value[0], value[1]);
            }
            if let Some(value) = self.dim {
                self.gl.Uniform1f(self.uniforms.dim, value);
            }
            if let Some(value) = self.desat {
                self.gl.Uniform1f(self.uniforms.desat, value);
            }
            if let Some(value) = self.uv_rect {
                self.gl.Uniform4f(
                    self.uniforms.uv_rect,
                    value[0],
                    value[1],
                    value[2],
                    value[3],
                );
            }
            if let Some(value) = self.ripple_progress {
                self.gl.Uniform1f(self.uniforms.ripple_progress, value);
            }
            if let Some(value) = self.ripple_amplitude {
                self.gl.Uniform1f(self.uniforms.ripple_amplitude, value);
            }
            if let Some(value) = self.color_managed {
                self.gl.Uniform1i(self.uniforms.color_managed, value);
            }
            if let Some(value) = self.color_matrix {
                self.gl
                    .UniformMatrix3fv(self.uniforms.color_matrix, 1, ffi::FALSE, value.as_ptr());
            }
            if let Some(value) = self.decode_tf {
                self.gl.Uniform1i(self.uniforms.decode_tf, value);
            }
            if let Some(value) = self.decode_gamma {
                self.gl.Uniform1f(self.uniforms.decode_gamma, value);
            }
            if let Some(value) = self.encode_tf {
                self.gl.Uniform1i(self.uniforms.encode_tf, value);
            }
            if let Some(value) = self.encode_gamma {
                self.gl.Uniform1f(self.uniforms.encode_gamma, value);
            }
            if let Some(value) = self.scene_linear {
                self.gl.Uniform1i(self.uniforms.scene_linear, value);
            }
        }
    }
}

struct ThumbnailGlesResources<'a> {
    gl: &'a ffi::Gles2,
    texture: u32,
    framebuffer: u32,
}

impl Drop for ThumbnailGlesResources<'_> {
    fn drop(&mut self) {
        unsafe {
            if self.framebuffer != 0 {
                self.gl.DeleteFramebuffers(1, &self.framebuffer);
            }
            if self.texture != 0 {
                self.gl.DeleteTextures(1, &self.texture);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Render state queries
// ---------------------------------------------------------------------------

impl WaylandCompositor {
    /// Returns true if the compositor has pending work that requires a new frame.
    pub(crate) fn needs_render(&self) -> bool {
        self.needs_render
            || self.dock_preview.as_ref().is_some_and(|preview| {
                !preview.awaiting_source
                    && crate::backend::compositor_common::genie::preview_lease_timeout(
                        preview.direction,
                        Instant::now(),
                        preview.lease_deadline,
                    ) == Some(std::time::Duration::ZERO)
            })
    }

    /// Nearest wall-clock wakeup owned by a settled compositor overlay. Active
    /// transitions already use the regular 16 ms animation cadence; this is
    /// only needed so a crashed bar or lost LEAVE cannot strand a preview.
    pub(crate) fn next_wakeup(&self) -> Option<std::time::Duration> {
        let preview = self.dock_preview.as_ref()?;
        if preview.awaiting_source {
            return None;
        }
        crate::backend::compositor_common::genie::preview_lease_timeout(
            preview.direction,
            Instant::now(),
            preview.lease_deadline,
        )
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

    /// Internal GL format of the compositor-owned output texture.
    pub(crate) fn output_texture_internal_format(&self) -> u32 {
        self.output_internal_format
    }

    pub(crate) fn output_texture_generation(&self) -> u64 {
        self.output_texture_generation
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
        self.start_recording_region(path, (0, 0, self.screen_w, self.screen_h));
    }

    pub(crate) fn start_recording_region(&mut self, path: &str, region: (i32, i32, u32, u32)) {
        let region = self.clamp_recording_region(region);
        self.pending_recording_start = Some((path.to_string(), region));
    }

    pub(crate) fn set_recording_region(&mut self, region: (i32, i32, u32, u32)) {
        let region = self.clamp_recording_region(region);
        if let Some((_, pending_region)) = self.pending_recording_start.as_mut() {
            *pending_region = region;
        }
        self.recording.set_region(region);
        self.needs_render = true;
    }

    pub(crate) fn set_recording_region_overlay(&mut self, region: Option<(i32, i32, u32, u32)>) {
        self.recording_region_overlay = region;
        self.needs_render = true;
        self.force_full_damage_next = true;
    }

    pub(crate) fn recording_requires_composition(&self) -> bool {
        self.recording.is_active()
            || self.pending_recording_start.is_some()
            || self.pending_recording_stop
            || self.recording_region_overlay.is_some()
    }

    fn clamp_recording_region(&self, region: (i32, i32, u32, u32)) -> (i32, i32, u32, u32) {
        let (x, y, width, height) = region;
        let x = x.clamp(0, self.screen_w.saturating_sub(1) as i32);
        let y = y.clamp(0, self.screen_h.saturating_sub(1) as i32);
        let width = width.max(1).min(self.screen_w.saturating_sub(x as u32));
        let height = height.max(1).min(self.screen_h.saturating_sub(y as u32));
        (x, y, width, height)
    }

    /// Request recording stop — deferred until next render_frame when GL is active.
    pub(crate) fn stop_recording(&mut self) {
        self.pending_recording_stop = true;
    }

    /// Notify audio timing for a window (feeds AudioSyncManager).
    pub(crate) fn notify_audio_timing(&mut self, window_id: u64, fps: f32, buffer_latency_ms: u32) {
        self.audio_sync_mgr
            .register_stream(window_id, fps, buffer_latency_ms);
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
            let _state = ThumbnailGlesState::begin(gl);
            let mut resources = ThumbnailGlesResources {
                gl,
                texture: 0,
                framebuffer: 0,
            };
            gl.GenTextures(1, &mut resources.texture);
            if resources.texture == 0 {
                log::warn!("wayland compositor: thumbnail texture allocation failed");
                return None;
            }
            gl.BindTexture(ffi::TEXTURE_2D, resources.texture);
            gl.TexImage2D(
                ffi::TEXTURE_2D,
                0,
                ffi::RGBA8 as i32,
                tw as i32,
                th as i32,
                0,
                ffi::RGBA,
                ffi::UNSIGNED_BYTE,
                std::ptr::null(),
            );
            gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MIN_FILTER, ffi::LINEAR as i32);
            gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MAG_FILTER, ffi::LINEAR as i32);

            gl.GenFramebuffers(1, &mut resources.framebuffer);
            if resources.framebuffer == 0 {
                log::warn!("wayland compositor: thumbnail framebuffer allocation failed");
                return None;
            }
            gl.BindFramebuffer(ffi::FRAMEBUFFER, resources.framebuffer);
            gl.FramebufferTexture2D(
                ffi::FRAMEBUFFER,
                ffi::COLOR_ATTACHMENT0,
                ffi::TEXTURE_2D,
                resources.texture,
                0,
            );
            if gl.CheckFramebufferStatus(ffi::FRAMEBUFFER) != ffi::FRAMEBUFFER_COMPLETE {
                log::warn!("wayland compositor: thumbnail framebuffer is incomplete");
                return None;
            }

            gl.Viewport(0, 0, tw as i32, th as i32);
            gl.ClearColor(0.0, 0.0, 0.0, 0.0);
            gl.Clear(ffi::COLOR_BUFFER_BIT);

            let _uniform_state =
                ThumbnailGlesUniformState::capture(gl, self.program, &self.win_uniforms);
            gl.UseProgram(self.program);
            let projection = ortho(0.0, tw as f32, th as f32, 0.0);
            gl.UniformMatrix4fv(
                self.win_uniforms.projection,
                1,
                ffi::FALSE as u8,
                projection.as_ptr(),
            );
            gl.Uniform1i(self.win_uniforms.texture, 0);
            gl.Uniform1f(
                self.win_uniforms.opacity,
                snapshot_shader_opacity(ws.has_alpha),
            );
            gl.Uniform1f(self.win_uniforms.dim, 1.0);
            gl.Uniform1f(self.win_uniforms.radius, 0.0);
            gl.Uniform2f(self.win_uniforms.size, tw as f32, th as f32);
            gl.Uniform1f(self.win_uniforms.desat, 0.0);
            gl.Uniform1f(self.win_uniforms.ripple_progress, -1.0);
            gl.Uniform1f(self.win_uniforms.ripple_amplitude, 0.0);
            gl.Uniform1i(self.win_uniforms.color_managed, 0);
            let identity = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
            gl.UniformMatrix3fv(
                self.win_uniforms.color_matrix,
                1,
                ffi::FALSE,
                identity.as_ptr(),
            );
            gl.Uniform1i(self.win_uniforms.decode_tf, 0);
            gl.Uniform1f(self.win_uniforms.decode_gamma, 1.0);
            gl.Uniform1i(self.win_uniforms.encode_tf, 0);
            gl.Uniform1f(self.win_uniforms.encode_gamma, 1.0);
            gl.Uniform1i(self.win_uniforms.scene_linear, 0);

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
                0,
                0,
                tw as i32,
                th as i32,
                ffi::RGBA,
                ffi::UNSIGNED_BYTE,
                pixels.as_mut_ptr() as *mut _,
            );
            flip_rgba_vertical(&mut pixels, tw, th);

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

    pub(crate) fn get_direct_scanout_status(
        &self,
        kms_outputs: Vec<crate::backend::api::DirectScanoutOutputStatus>,
    ) -> crate::backend::api::DirectScanoutStatus {
        let ds_stats = self.direct_scanout_mgr.stats();
        crate::backend::api::DirectScanoutStatus {
            enabled: self.direct_scanout_mgr.is_enabled(),
            active: self.direct_scanout_mgr.is_active(),
            current_window: self.direct_scanout_mgr.current_scanout(),
            scanout_count: ds_stats.scanout_count,
            bypass_time_ms: ds_stats.bypass_time_ms,
            candidate_count: self.direct_scanout_mgr.candidate_count(),
            compositor_reason: self.direct_scanout_mgr.last_reason().to_string(),
            kms_outputs,
        }
    }

    /// Collect compositor metrics from all subsystems.
    pub(crate) fn get_metrics(&self) -> crate::backend::api::CompositorMetrics {
        let avg = self.perf_metrics.avg_frame_time().as_secs_f32() * 1000.0;
        let max = self.perf_metrics.max_frame_time().as_secs_f32() * 1000.0;
        let min = self.perf_metrics.min_frame_time().as_secs_f32() * 1000.0;
        let p95 = self.perf_metrics.frame_time_percentile(0.95).as_secs_f32() * 1000.0;
        let p99 = self.perf_metrics.frame_time_percentile(0.99).as_secs_f32() * 1000.0;
        let temporal_rate = if self.temporal_blur_total_count > 0 {
            100.0 * self.temporal_blur_reuse_count as f32 / self.temporal_blur_total_count as f32
        } else {
            0.0
        };
        let ds_stats = self.direct_scanout_mgr.stats();
        crate::backend::api::CompositorMetrics {
            renderer_api: "egl/gles".to_string(),
            fps: self.fps,
            frame_count: self.frame_count,
            avg_frame_time_ms: avg,
            max_frame_time_ms: max,
            min_frame_time_ms: min,
            frame_time_p95_ms: p95,
            frame_time_p99_ms: p99,
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
            dirty_fraction_percent: self.dirty_region_tracker.current_dirty_fraction() * 100.0,
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
            gl_state_changes_avoided: self
                .gl_state_tracker
                .redundant_changes_avoided()
                .min(u32::MAX as u64) as u32,
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

            let (output_fbo, output_texture) = if self.output_internal_format == GL_RGB10_A2 {
                create_fbo_texture_10bit(gl, w, h)
            } else {
                create_fbo_texture(gl, w, h)
            };
            self.output_fbo = output_fbo;
            self.output_texture = output_texture;
            self.output_texture_generation = next_output_texture_generation();

            // Mirror the requested runtime state. Programs are always built,
            // so resize can also complete a hot enable.
            if self.linear_fbo != 0 {
                gl.DeleteFramebuffers(1, &self.linear_fbo);
                gl.DeleteTextures(1, &self.linear_texture);
                self.linear_fbo = 0;
                self.linear_texture = 0;
            }
            if self.scene_linear_requested {
                match create_fbo_texture_fp16(gl, w, h) {
                    Ok((lf, lt)) => {
                        self.linear_fbo = lf;
                        self.linear_texture = lt;
                    }
                    Err(status) => {
                        log::warn!(
                            "[udev/compositor] disabling scene-linear compositing after \
                             RGBA16F resize allocation failed (status=0x{status:x})"
                        );
                        self.scene_linear_requested = false;
                    }
                }
            }

            // Keep the offscreen chain at the same bit depth as on construction
            // (see new()): 10-bit when the output is 10-bit, else 8-bit. Without
            // this the chain silently reverts to 8-bit after any resize.
            let hdr_10bit = self.output_internal_format == GL_RGB10_A2;
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
            self.transition_active = false;
            self.transition_snapshot_pending = false;
            self.transition_start = None;
            self.transition_mon = None;

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
        self.overview_titles_dirty = true;
        self.overview_monitor = (0, 0, w, h);
    }
}
