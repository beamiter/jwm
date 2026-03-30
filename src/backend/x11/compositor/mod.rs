pub mod math;
mod annotations;
mod effects;
mod expose;
mod font;
mod overview;
mod pipeline;
mod postprocess;
pub mod shaders;
mod tfp;
mod transitions;

use glow::HasContext;
use std::collections::HashMap;
use std::ffi::CString;
use std::sync::Arc;
use std::sync::mpsc;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::wrapper::ConnectionExt as WrapperExt;
use x11rb::protocol::composite::ConnectionExt as CompositeExt;
use x11rb::protocol::damage::{self, ConnectionExt as DamageExt};
use x11rb::protocol::xfixes::ConnectionExt as XFixesExt;
use x11rb::protocol::xproto::{self, ConnectionExt as XProtoExt};
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
// Per-window texture state
// ---------------------------------------------------------------------------

struct WindowTexture {
    #[allow(dead_code)]
    x: i32,
    #[allow(dead_code)]
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

#[derive(Clone, Copy, PartialEq)]
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
    texture: Option<glow::UniformLocation>,
    color_temp: Option<glow::UniformLocation>,
    saturation: Option<glow::UniformLocation>,
    brightness: Option<glow::UniformLocation>,
    contrast: Option<glow::UniformLocation>,
    invert: Option<glow::UniformLocation>,
    grayscale: Option<glow::UniformLocation>,
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
    frame_times: Vec<f32>,
    last_frame_time: std::time::Instant,
    // Phase 7.2: Extended debug stats
    draw_calls: u32,
    texture_memory_bytes: u64,
    blur_cache_hits: u64,
    blur_cache_misses: u64,
}

/// Per-window wobbly animation state (grid spring-mass system).
struct WobblyState {
    grid_n: usize,                   // nodes per axis = grid_size + 1
    offsets: Vec<[f32; 2]>,          // grid_n * grid_n node offsets (pixels)
    velocities: Vec<[f32; 2]>,       // grid_n * grid_n node velocities
    dragging: bool,                  // true while interactive move is active
    anchor_row: usize,               // drag anchor node row
    anchor_col: usize,               // drag anchor node column
    last_tick: std::time::Instant,   // for accurate dt calculation
}

/// Entry for Alt-Tab overview mode.
struct OverviewEntry {
    x11_win: u32,
    #[allow(dead_code)]
    target_x: f32,
    #[allow(dead_code)]
    target_y: f32,
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
    orig_x: f32, orig_y: f32, orig_w: f32, orig_h: f32,
    target_x: f32, target_y: f32, target_w: f32, target_h: f32,
    current_x: f32, current_y: f32, current_w: f32, current_h: f32,
    is_hovered: bool,
}

/// Snap preview state.
struct SnapPreview {
    x: f32, y: f32, w: f32, h: f32,
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

const TILE_COLS: u32 = 8;
const TILE_ROWS: u32 = 6;

struct DamageTracker {
    /// Screen divided into TILE_COLS x TILE_ROWS tiles.
    dirty_tiles: Vec<bool>,
    tile_w: u32,
    tile_h: u32,
    screen_w: u32,
    screen_h: u32,
}

impl DamageTracker {
    fn new(screen_w: u32, screen_h: u32) -> Self {
        let tile_w = (screen_w + TILE_COLS - 1) / TILE_COLS;
        let tile_h = (screen_h + TILE_ROWS - 1) / TILE_ROWS;
        Self {
            dirty_tiles: vec![true; (TILE_COLS * TILE_ROWS) as usize],
            tile_w,
            tile_h,
            screen_w,
            screen_h,
        }
    }

    fn mark_all_dirty(&mut self) {
        self.dirty_tiles.fill(true);
    }

    fn clear(&mut self) {
        self.dirty_tiles.fill(false);
    }

    fn dirty_fraction(&self) -> f32 {
        let dirty = self.dirty_tiles.iter().filter(|&&d| d).count();
        dirty as f32 / self.dirty_tiles.len() as f32
    }

    /// Returns the bounding rectangle of all dirty tiles, or None if nothing is dirty.
    /// If >50% dirty, returns full screen (cheaper than many scissor switches).
    fn dirty_bounds(&self) -> Option<(i32, i32, u32, u32)> {
        if self.dirty_fraction() > 0.5 {
            return Some((0, 0, self.screen_w, self.screen_h));
        }
        let mut min_x = self.screen_w as i32;
        let mut min_y = self.screen_h as i32;
        let mut max_x = 0i32;
        let mut max_y = 0i32;
        let mut any_dirty = false;
        for ty in 0..TILE_ROWS {
            for tx in 0..TILE_COLS {
                if self.dirty_tiles[(ty * TILE_COLS + tx) as usize] {
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
        self.tile_w = (screen_w + TILE_COLS - 1) / TILE_COLS;
        self.tile_h = (screen_h + TILE_ROWS - 1) / TILE_ROWS;
        self.dirty_tiles.fill(true);
    }
}

// ---------------------------------------------------------------------------
// Blur quality auto-downgrade (Phase 2.2)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum BlurQuality {
    Full,     // All blur levels
    Reduced,  // Half blur levels
    Minimal,  // 1 blur level
}

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
    #[allow(dead_code)]
    x11_win: u32,
    start: std::time::Instant,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    gl_texture: glow::Texture,
    has_rgba: bool,
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

pub(super) struct Compositor {
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
    overlay_window: u32,
    /// Window that owns the _NET_WM_CM_Sn selection, advertising this
    /// compositor to other clients (screenshot tools, etc.).
    cm_selection_owner: u32,
    glx_drawable: x11::glx::GLXDrawable,
    gl: glow::Context,
    program: glow::Program,
    shadow_program: glow::Program,
    blur_down_program: glow::Program,
    blur_up_program: glow::Program,
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
    // Fullscreen optimisation
    fullscreen_unredirect: bool,
    /// Currently unredirected fullscreen window (if any)
    unredirected_window: Option<u32>,

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
    frame_stats: FrameStats,

    // --- Feature 12: Screenshot ---
    pending_screenshot: Option<std::path::PathBuf>,

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
        let wins: Vec<u32> = self.windows.keys().copied().collect();
        for w in wins {
            self.remove_window(w);
        }
        // Destroy the _NET_WM_CM_Sn selection owner window (releases ownership)
        let _ = self.conn.destroy_window(self.cm_selection_owner);
        // Undo the MANUAL redirect so the X server renders windows normally again
        let _ = self.conn.composite_unredirect_subwindows(
            self.root,
            x11rb::protocol::composite::Redirect::MANUAL,
        );
        let _ = self.conn.composite_release_overlay_window(self.overlay_window);
        let _ = self.conn.flush();
        unsafe {
            x11::glx::glXDestroyContext(self.xlib_display, self.glx_context);
            x11::xlib::XCloseDisplay(self.xlib_display);
        }
    }
}

impl Compositor {
    pub(super) fn new(
        conn: Arc<RustConnection>,
        root: u32,
        screen_w: u32,
        screen_h: u32,
    ) -> Result<Self, String> {
        // 1. Check composite extension
        conn.composite_query_version(0, 4)
            .map_err(|e| format!("composite_query_version: {e}"))?
            .reply()
            .map_err(|e| format!("composite reply: {e}"))?;

        // 2. Redirect subwindows
        conn.composite_redirect_subwindows(root, x11rb::protocol::composite::Redirect::MANUAL)
            .map_err(|e| format!("redirect_subwindows: {e}"))?;

        // RAII guard: if we return Err after the redirect, undo it so the screen
        // doesn't go permanently black.
        struct RedirectGuard {
            conn: Arc<RustConnection>,
            root: u32,
            overlay: Option<u32>,
            active: bool,
        }
        impl Drop for RedirectGuard {
            fn drop(&mut self) {
                if self.active {
                    let _ = self.conn.composite_unredirect_subwindows(
                        self.root,
                        x11rb::protocol::composite::Redirect::MANUAL,
                    );
                    if let Some(ow) = self.overlay {
                        let _ = self.conn.composite_release_overlay_window(ow);
                    }
                    let _ = self.conn.flush();
                }
            }
        }
        let mut guard = RedirectGuard {
            conn: conn.clone(),
            root,
            overlay: None,
            active: true,
        };

        // 3. Damage extension
        conn.damage_query_version(1, 1)
            .map_err(|e| format!("damage_query_version: {e}"))?
            .reply()
            .map_err(|e| format!("damage reply: {e}"))?;

        let damage_ext = conn
            .extension_information(damage::X11_EXTENSION_NAME)
            .map_err(|e| format!("damage ext info: {e}"))?
            .ok_or("damage extension not available")?;
        let damage_event_base = damage_ext.first_event;

        // 4. Get overlay window
        let overlay_reply = conn
            .composite_get_overlay_window(root)
            .map_err(|e| format!("get_overlay_window: {e}"))?
            .reply()
            .map_err(|e| format!("overlay reply: {e}"))?;
        let overlay_window = overlay_reply.overlay_win;
        guard.overlay = Some(overlay_window);

        // 5. Make overlay input-passthrough using XFixes
        {
            // XFixes version negotiation is REQUIRED before using xfixes_set_window_shape_region.
            // Without this, some X servers (e.g. Ubuntu 20's Xorg) silently ignore the request,
            // leaving the overlay opaque to input and blocking all mouse clicks to client windows.
            let xfixes_ver = conn.xfixes_query_version(5, 0)
                .map_err(|e| format!("xfixes_query_version: {e}"))?
                .reply()
                .map_err(|e| format!("xfixes version reply: {e}"))?;
            log::info!(
                "compositor: XFixes version {}.{}",
                xfixes_ver.major_version, xfixes_ver.minor_version
            );

            log::info!(
                "compositor: setting empty INPUT shape on overlay 0x{:x} to pass through input",
                overlay_window
            );
            let region = conn.generate_id().map_err(|e| format!("gen id: {e}"))?;
            conn.xfixes_create_region(region, &[])
                .map_err(|e| format!("create_region: {e}"))?;
            conn.xfixes_set_window_shape_region(
                overlay_window,
                x11rb::protocol::shape::SK::INPUT,
                0,
                0,
                region,
            )
            .map_err(|e| format!("set_window_shape_region: {e}"))?;
            conn.xfixes_destroy_region(region)
                .map_err(|e| format!("destroy_region: {e}"))?;
            // Flush and round-trip to ensure the shape region is applied before proceeding
            conn.flush().map_err(|e| format!("flush after shape: {e}"))?;
            // Round-trip: get_input_focus forces the X server to process all prior requests
            conn.get_input_focus()
                .map_err(|e| format!("sync after shape: {e}"))?
                .reply()
                .map_err(|e| format!("sync reply after shape: {e}"))?;
            log::info!("compositor: overlay input shape set successfully (verified via sync)");
        }

        // 6. Open Xlib display for GLX
        let xlib_display = unsafe { x11::xlib::XOpenDisplay(std::ptr::null()) };
        if xlib_display.is_null() {
            return Err("XOpenDisplay failed".into());
        }
        // Install a no-op error handler for this Xlib display permanently.
        // The default Xlib handler calls exit() on ANY X error, which would
        // kill the entire WM for benign issues like stale pixmaps.
        unsafe {
            x11::xlib::XSetErrorHandler(Some(ignore_x_error));
        }

        let screen_num = unsafe { x11::xlib::XDefaultScreen(xlib_display) };

        // 6b. Verify GLX_EXT_texture_from_pixmap is advertised in the extension string.
        // glXGetProcAddress can return non-null pointers even when the extension
        // is not actually supported (e.g. indirect GLX in nested X servers).
        {
            let ext_str = unsafe {
                let raw = x11::glx::glXQueryExtensionsString(xlib_display, screen_num);
                if raw.is_null() {
                    ""
                } else {
                    std::ffi::CStr::from_ptr(raw).to_str().unwrap_or("")
                }
            };
            if !ext_str.contains("GLX_EXT_texture_from_pixmap") {
                unsafe { x11::xlib::XCloseDisplay(xlib_display) };
                // Guard will undo redirect + release overlay
                return Err(
                    "GLX_EXT_texture_from_pixmap not available (nested X server?)".into(),
                );
            }
            log::info!("GLX extensions: {ext_str}");
        }

        // 7. Choose FBConfig for GLX context.
        // We must pick an FBConfig whose visual matches the overlay window's
        // visual — otherwise glXCreateWindow / glXMakeContextCurrent will fail
        // (or even segfault) due to the visual mismatch.
        let overlay_visual_id = {
            let attrs = conn
                .get_window_attributes(overlay_window)
                .map_err(|e| format!("get_window_attributes(overlay): {e}"))?
                .reply()
                .map_err(|e| format!("overlay attrs reply: {e}"))?;
            attrs.visual
        };
        log::info!(
            "compositor: overlay visual=0x{:x}, choosing matching FBConfig...",
            overlay_visual_id
        );

        // Request a double-buffered FBConfig matching the overlay's exact visual.
        // We use glXSwapBuffers with swap interval=1 for vsync, which eliminates
        // tearing during window movement.
        let ctx_attrs_visual: Vec<i32> = vec![
            x11::glx::GLX_RENDER_TYPE,
            x11::glx::GLX_RGBA_BIT,
            x11::glx::GLX_DRAWABLE_TYPE,
            x11::glx::GLX_WINDOW_BIT,
            x11::glx::GLX_DOUBLEBUFFER,
            1, // double-buffered for tear-free rendering
            x11::glx::GLX_RED_SIZE,
            8,
            x11::glx::GLX_GREEN_SIZE,
            8,
            x11::glx::GLX_BLUE_SIZE,
            8,
            0,
        ];

        let mut n_configs: i32 = 0;
        let configs = unsafe {
            x11::glx::glXChooseFBConfig(
                xlib_display,
                screen_num,
                ctx_attrs_visual.as_ptr(),
                &mut n_configs,
            )
        };
        if configs.is_null() || n_configs == 0 {
            return Err("No suitable GLX FBConfig found".into());
        }

        // Pick the first FBConfig whose visual matches the overlay window.
        let mut ctx_fbconfig: x11::glx::GLXFBConfig = std::ptr::null_mut();
        unsafe {
            for i in 0..n_configs {
                let cfg = *configs.offset(i as isize);
                let vi = x11::glx::glXGetVisualFromFBConfig(xlib_display, cfg);
                if !vi.is_null() {
                    let vid = (*vi).visualid;
                    x11::xlib::XFree(vi as *mut _);
                    if vid == overlay_visual_id as u64 {
                        ctx_fbconfig = cfg;
                        break;
                    }
                }
            }
            // Fallback: if no exact match, just use the first config
            if ctx_fbconfig.is_null() {
                log::warn!(
                    "compositor: no FBConfig matching overlay visual 0x{:x}, using first available",
                    overlay_visual_id
                );
                ctx_fbconfig = *configs;
            }
            x11::xlib::XFree(configs as *mut _);
        }
        log::info!("compositor: found matching FBConfig for context (from {} candidates)", n_configs);

        // 8. Create GLX context
        log::info!("compositor: creating GLX context...");
        let glx_context = unsafe {
            x11::glx::glXCreateNewContext(
                xlib_display,
                ctx_fbconfig,
                x11::glx::GLX_RGBA_TYPE,
                std::ptr::null_mut(),
                1,
            )
        };
        if glx_context.is_null() {
            return Err("glXCreateNewContext failed".into());
        }

        log::info!("compositor: GLX context created, checking direct rendering...");
        // 8b. Require direct rendering — indirect GLX (e.g. in Xephyr) cannot
        //     do texture-from-pixmap because the pixmaps live in the nested
        //     server's address space, not the host GPU's.
        let is_direct = unsafe { x11::glx::glXIsDirect(xlib_display, glx_context) };
        if is_direct == 0 {
            log::warn!("GLX context is indirect — compositor cannot work (nested X server?)");
            unsafe {
                x11::glx::glXDestroyContext(xlib_display, glx_context);
                x11::xlib::XCloseDisplay(xlib_display);
            }
            return Err("GLX context is indirect; compositor requires direct rendering".into());
        }

        log::info!("compositor: direct rendering OK, creating GLX window on overlay 0x{:x}...", overlay_window);
        // 9. Create GLX window on the overlay
        let glx_drawable = unsafe {
            x11::glx::glXCreateWindow(
                xlib_display,
                ctx_fbconfig,
                overlay_window as _,
                std::ptr::null(),
            )
        };
        if glx_drawable == 0 {
            return Err("glXCreateWindow failed".into());
        }

        log::info!("compositor: GLX window created, making context current...");
        // Make context current
        let ok = unsafe {
            x11::glx::glXMakeContextCurrent(
                xlib_display,
                glx_drawable,
                glx_drawable,
                glx_context,
            )
        };
        if ok == 0 {
            return Err("glXMakeContextCurrent failed".into());
        }

        log::info!("compositor: context current OK, loading TFP extension functions...");
        // 10. Load TFP extension functions
        let bind_name = CString::new("glXBindTexImageEXT").unwrap();
        let release_name = CString::new("glXReleaseTexImageEXT").unwrap();
        let bind_ptr =
            unsafe { x11::glx::glXGetProcAddress(bind_name.as_ptr() as *const u8) };
        let release_ptr =
            unsafe { x11::glx::glXGetProcAddress(release_name.as_ptr() as *const u8) };
        if bind_ptr.is_none() || release_ptr.is_none() {
            return Err("glXBindTexImageEXT / glXReleaseTexImageEXT not available".into());
        }
        let tfp = TfpFunctions {
            bind: unsafe { std::mem::transmute(bind_ptr.unwrap()) },
            release: unsafe { std::mem::transmute(release_ptr.unwrap()) },
        };

        // VSync: set swap interval = 1 to synchronize buffer swaps with vblank,
        // preventing tearing during window movement.
        {
            let swap_ext_name = CString::new("glXSwapIntervalEXT").unwrap();
            let swap_mesa_name = CString::new("glXSwapIntervalMESA").unwrap();
            let swap_ext_ptr = unsafe {
                x11::glx::glXGetProcAddress(swap_ext_name.as_ptr() as *const u8)
            };
            let swap_mesa_ptr = unsafe {
                x11::glx::glXGetProcAddress(swap_mesa_name.as_ptr() as *const u8)
            };

            if let Some(ptr) = swap_ext_ptr {
                // glXSwapIntervalEXT(Display*, GLXDrawable, int interval)
                type SwapIntervalEXT = unsafe extern "C" fn(*mut x11::xlib::Display, x11::glx::GLXDrawable, i32);
                let swap_fn: SwapIntervalEXT = unsafe { std::mem::transmute(ptr) };
                unsafe { swap_fn(xlib_display, glx_drawable, 1) };
                log::info!("compositor: vsync enabled via glXSwapIntervalEXT(1)");
            } else if let Some(ptr) = swap_mesa_ptr {
                // glXSwapIntervalMESA(unsigned int interval)
                type SwapIntervalMESA = unsafe extern "C" fn(u32) -> i32;
                let swap_fn: SwapIntervalMESA = unsafe { std::mem::transmute(ptr) };
                unsafe { swap_fn(1) };
                log::info!("compositor: vsync enabled via glXSwapIntervalMESA(1)");
            } else {
                log::warn!("compositor: no swap interval extension available, tearing may occur");
            }
        }

        log::info!("compositor: finding TFP FBConfigs...");
        // 12. Find FBConfigs for TFP (RGBA and RGB)
        let tfp_rgba_attrs: Vec<i32> = vec![
            x11::glx::GLX_DRAWABLE_TYPE,
            x11::glx::GLX_PIXMAP_BIT,
            x11::glx::GLX_RENDER_TYPE,
            x11::glx::GLX_RGBA_BIT,
            GLX_BIND_TO_TEXTURE_RGBA_EXT,
            1,
            x11::glx::GLX_RED_SIZE,
            8,
            x11::glx::GLX_GREEN_SIZE,
            8,
            x11::glx::GLX_BLUE_SIZE,
            8,
            x11::glx::GLX_ALPHA_SIZE,
            8,
            0,
        ];
        let tfp_rgb_attrs: Vec<i32> = vec![
            x11::glx::GLX_DRAWABLE_TYPE,
            x11::glx::GLX_PIXMAP_BIT,
            x11::glx::GLX_RENDER_TYPE,
            x11::glx::GLX_RGBA_BIT,
            GLX_BIND_TO_TEXTURE_RGB_EXT,
            1,
            x11::glx::GLX_RED_SIZE,
            8,
            x11::glx::GLX_GREEN_SIZE,
            8,
            x11::glx::GLX_BLUE_SIZE,
            8,
            0,
        ];

        // Enumerate ALL TFP-compatible FBConfigs and build a per-visual map.
        // On older drivers (e.g. Ubuntu 20's Mesa), using a FBConfig whose
        // visual doesn't match the source pixmap's visual produces garbled
        // textures (e.g. solid orange).  Per-visual matching fixes this.
        let mut tfp_visual_configs: HashMap<u32, (x11::glx::GLXFBConfig, bool)> = HashMap::new();
        let mut fbconfig_rgba: x11::glx::GLXFBConfig = std::ptr::null_mut();
        let mut fbconfig_rgb: x11::glx::GLXFBConfig = std::ptr::null_mut();

        let mut n = 0i32;

        // --- RGBA TFP configs ---
        let cfgs_rgba = unsafe {
            x11::glx::glXChooseFBConfig(
                xlib_display,
                screen_num,
                tfp_rgba_attrs.as_ptr(),
                &mut n,
            )
        };
        if !cfgs_rgba.is_null() && n > 0 {
            fbconfig_rgba = unsafe { *cfgs_rgba };
            for i in 0..n {
                let cfg = unsafe { *cfgs_rgba.offset(i as isize) };
                let mut vid: i32 = 0;
                unsafe {
                    x11::glx::glXGetFBConfigAttrib(
                        xlib_display,
                        cfg,
                        x11::glx::GLX_VISUAL_ID,
                        &mut vid,
                    );
                }
                if vid != 0 {
                    tfp_visual_configs.entry(vid as u32).or_insert((cfg, true));
                }
            }
            unsafe { x11::xlib::XFree(cfgs_rgba as *mut _) };
        }

        // --- RGB TFP configs ---
        let cfgs_rgb = unsafe {
            x11::glx::glXChooseFBConfig(
                xlib_display,
                screen_num,
                tfp_rgb_attrs.as_ptr(),
                &mut n,
            )
        };
        if !cfgs_rgb.is_null() && n > 0 {
            fbconfig_rgb = unsafe { *cfgs_rgb };
            for i in 0..n {
                let cfg = unsafe { *cfgs_rgb.offset(i as isize) };
                let mut vid: i32 = 0;
                unsafe {
                    x11::glx::glXGetFBConfigAttrib(
                        xlib_display,
                        cfg,
                        x11::glx::GLX_VISUAL_ID,
                        &mut vid,
                    );
                }
                if vid != 0 {
                    // Don't overwrite an RGBA entry — prefer RGBA for 32-bit visuals.
                    tfp_visual_configs.entry(vid as u32).or_insert((cfg, false));
                }
            }
            unsafe { x11::xlib::XFree(cfgs_rgb as *mut _) };
        }

        if fbconfig_rgba.is_null() && fbconfig_rgb.is_null() {
            return Err("No FBConfig for texture_from_pixmap".into());
        }
        log::info!(
            "compositor: TFP FBConfigs: rgba={} rgb={} per_visual={}",
            !fbconfig_rgba.is_null(),
            !fbconfig_rgb.is_null(),
            tfp_visual_configs.len(),
        );

        // 13. Create glow GL context
        log::info!("compositor: creating glow GL context...");
        let gl = unsafe {
            glow::Context::from_loader_function(|name| {
                let cname = CString::new(name).unwrap();
                match x11::glx::glXGetProcAddress(cname.as_ptr() as *const u8) {
                    Some(f) => f as *const _,
                    None => std::ptr::null(),
                }
            })
        };

        log::info!("compositor: glow GL context created, compiling shaders...");
        // 14. Compile shaders and create program
        let program = unsafe { Self::create_program(&gl, shaders::VERTEX_SHADER, shaders::FRAGMENT_SHADER)? };
        let shadow_program = unsafe { Self::create_program(&gl, shaders::VERTEX_SHADER, shaders::SHADOW_FRAGMENT_SHADER)? };

        // Cache uniform locations (avoids per-frame string lookups)
        let win_uniforms = unsafe {
            WindowUniforms {
                projection: gl.get_uniform_location(program, "u_projection"),
                rect: gl.get_uniform_location(program, "u_rect"),
                texture: gl.get_uniform_location(program, "u_texture"),
                opacity: gl.get_uniform_location(program, "u_opacity"),
                radius: gl.get_uniform_location(program, "u_radius"),
                size: gl.get_uniform_location(program, "u_size"),
                dim: gl.get_uniform_location(program, "u_dim"),
                uv_rect: gl.get_uniform_location(program, "u_uv_rect"),
                ripple_progress: gl.get_uniform_location(program, "u_ripple_progress"),
                ripple_amplitude: gl.get_uniform_location(program, "u_ripple_amplitude"),
            }
        };
        let shadow_uniforms = unsafe {
            ShadowUniforms {
                projection: gl.get_uniform_location(shadow_program, "u_projection"),
                rect: gl.get_uniform_location(shadow_program, "u_rect"),
                shadow_color: gl.get_uniform_location(shadow_program, "u_shadow_color"),
                size: gl.get_uniform_location(shadow_program, "u_size"),
                radius: gl.get_uniform_location(shadow_program, "u_radius"),
                spread: gl.get_uniform_location(shadow_program, "u_spread"),
            }
        };

        // Compile blur shaders
        let blur_down_program = unsafe { Self::create_program(&gl, shaders::BLUR_DOWN_VERTEX, shaders::BLUR_DOWN_FRAGMENT)? };
        let blur_up_program = unsafe { Self::create_program(&gl, shaders::BLUR_DOWN_VERTEX, shaders::BLUR_UP_FRAGMENT)? };
        let blur_down_uniforms = unsafe {
            BlurUniforms {
                projection: gl.get_uniform_location(blur_down_program, "u_projection"),
                rect: gl.get_uniform_location(blur_down_program, "u_rect"),
                texture: gl.get_uniform_location(blur_down_program, "u_texture"),
                halfpixel: gl.get_uniform_location(blur_down_program, "u_halfpixel"),
            }
        };
        let blur_up_uniforms = unsafe {
            BlurUniforms {
                projection: gl.get_uniform_location(blur_up_program, "u_projection"),
                rect: gl.get_uniform_location(blur_up_program, "u_rect"),
                texture: gl.get_uniform_location(blur_up_program, "u_texture"),
                halfpixel: gl.get_uniform_location(blur_up_program, "u_halfpixel"),
            }
        };

        // Compile border shader (feature 1)
        let border_program = unsafe { Self::create_program(&gl, shaders::VERTEX_SHADER, shaders::BORDER_FRAGMENT_SHADER)? };
        let border_uniforms = unsafe {
            BorderUniforms {
                projection: gl.get_uniform_location(border_program, "u_projection"),
                rect: gl.get_uniform_location(border_program, "u_rect"),
                border_color: gl.get_uniform_location(border_program, "u_border_color"),
                size: gl.get_uniform_location(border_program, "u_size"),
                radius: gl.get_uniform_location(border_program, "u_radius"),
                border_width: gl.get_uniform_location(border_program, "u_border_width"),
            }
        };

        // Compile post-process shader (features 8/9/10 + magnifier)
        let postprocess_program = unsafe { Self::create_program(&gl, shaders::BLUR_DOWN_VERTEX, shaders::MAGNIFIER_POSTPROCESS_FRAGMENT_SHADER)? };
        let postprocess_uniforms = unsafe {
            PostprocessUniforms {
                texture: gl.get_uniform_location(postprocess_program, "u_texture"),
                color_temp: gl.get_uniform_location(postprocess_program, "u_color_temp"),
                saturation: gl.get_uniform_location(postprocess_program, "u_saturation"),
                brightness: gl.get_uniform_location(postprocess_program, "u_brightness"),
                contrast: gl.get_uniform_location(postprocess_program, "u_contrast"),
                invert: gl.get_uniform_location(postprocess_program, "u_invert"),
                grayscale: gl.get_uniform_location(postprocess_program, "u_grayscale"),
            }
        };

        let magnifier_uniforms = unsafe {
            MagnifierUniforms {
                magnifier_enabled: gl.get_uniform_location(postprocess_program, "u_magnifier_enabled"),
                magnifier_center: gl.get_uniform_location(postprocess_program, "u_magnifier_center"),
                magnifier_radius: gl.get_uniform_location(postprocess_program, "u_magnifier_radius"),
                magnifier_zoom: gl.get_uniform_location(postprocess_program, "u_magnifier_zoom"),
                colorblind_mode: gl.get_uniform_location(postprocess_program, "u_colorblind_mode"),
            }
        };

        // Compile HUD shader (feature 11)
        let hud_program = unsafe { Self::create_program(&gl, shaders::VERTEX_SHADER, shaders::HUD_FRAGMENT_SHADER)? };
        let hud_uniforms = unsafe {
            HudUniforms {
                projection: gl.get_uniform_location(hud_program, "u_projection"),
                rect: gl.get_uniform_location(hud_program, "u_rect"),
                bg_color: gl.get_uniform_location(hud_program, "u_bg_color"),
                fg_color: gl.get_uniform_location(hud_program, "u_fg_color"),
                size: gl.get_uniform_location(hud_program, "u_size"),
            }
        };

        // Compile HUD text shader (feature 11b)
        let hud_text_program = unsafe { Self::create_program(&gl, shaders::VERTEX_SHADER, shaders::HUD_TEXT_FRAGMENT_SHADER)? };
        let hud_text_uniforms = unsafe {
            HudTextUniforms {
                projection: gl.get_uniform_location(hud_text_program, "u_projection"),
                rect: gl.get_uniform_location(hud_text_program, "u_rect"),
                texture: gl.get_uniform_location(hud_text_program, "u_texture"),
            }
        };

        // Compile tag-switch transition shader
        let transition_program = unsafe {
            Self::create_program(&gl, shaders::BLUR_DOWN_VERTEX, shaders::TRANSITION_FRAGMENT_SHADER)?
        };
        let transition_uniforms = unsafe {
            TransitionUniforms {
                projection: gl.get_uniform_location(transition_program, "u_projection"),
                rect: gl.get_uniform_location(transition_program, "u_rect"),
                texture: gl.get_uniform_location(transition_program, "u_texture"),
                opacity: gl.get_uniform_location(transition_program, "u_opacity"),
                uv_rect: gl.get_uniform_location(transition_program, "u_uv_rect"),
            }
        };

        // Compile cube transition shader
        let cube_program = unsafe {
            Self::create_program(&gl, shaders::CUBE_VERTEX_SHADER, shaders::CUBE_FRAGMENT_SHADER)?
        };
        let cube_uniforms = unsafe {
            CubeUniforms {
                mvp: gl.get_uniform_location(cube_program, "u_mvp"),
                aspect: gl.get_uniform_location(cube_program, "u_aspect"),
                texture: gl.get_uniform_location(cube_program, "u_texture"),
                brightness: gl.get_uniform_location(cube_program, "u_brightness"),
                uv_rect: gl.get_uniform_location(cube_program, "u_uv_rect"),
            }
        };

        // Compile portal transition shader
        let portal_program = unsafe {
            Self::create_program(&gl, shaders::BLUR_DOWN_VERTEX, shaders::PORTAL_FRAGMENT_SHADER)?
        };
        let portal_uniforms = unsafe {
            PortalUniforms {
                projection: gl.get_uniform_location(portal_program, "u_projection"),
                rect: gl.get_uniform_location(portal_program, "u_rect"),
                texture: gl.get_uniform_location(portal_program, "u_texture"),
                progress: gl.get_uniform_location(portal_program, "u_progress"),
                glow: gl.get_uniform_location(portal_program, "u_glow"),
                center: gl.get_uniform_location(portal_program, "u_center"),
                uv_rect: gl.get_uniform_location(portal_program, "u_uv_rect"),
            }
        };

        // Compile edge glow shader
        let edge_glow_program = unsafe { Self::create_program(&gl, shaders::VERTEX_SHADER, shaders::EDGE_GLOW_FRAGMENT_SHADER)? };
        let edge_glow_uniforms = unsafe {
            EdgeGlowUniforms {
                projection: gl.get_uniform_location(edge_glow_program, "u_projection"),
                rect: gl.get_uniform_location(edge_glow_program, "u_rect"),
                glow_color: gl.get_uniform_location(edge_glow_program, "u_glow_color"),
                glow_width: gl.get_uniform_location(edge_glow_program, "u_glow_width"),
                mouse: gl.get_uniform_location(edge_glow_program, "u_mouse"),
                screen_size: gl.get_uniform_location(edge_glow_program, "u_screen_size"),
                time: gl.get_uniform_location(edge_glow_program, "u_time"),
            }
        };

        // Compile tilt shader (uses tilt vertex + tilt fragment)
        let tilt_program = unsafe { Self::create_program(&gl, shaders::TILT_VERTEX_SHADER, shaders::TILT_FRAGMENT_SHADER)? };
        let tilt_uniforms = unsafe {
            TiltUniforms {
                projection: gl.get_uniform_location(tilt_program, "u_projection"),
                rect: gl.get_uniform_location(tilt_program, "u_rect"),
                texture: gl.get_uniform_location(tilt_program, "u_texture"),
                opacity: gl.get_uniform_location(tilt_program, "u_opacity"),
                radius: gl.get_uniform_location(tilt_program, "u_radius"),
                size: gl.get_uniform_location(tilt_program, "u_size"),
                dim: gl.get_uniform_location(tilt_program, "u_dim"),
                uv_rect: gl.get_uniform_location(tilt_program, "u_uv_rect"),
                tilt: gl.get_uniform_location(tilt_program, "u_tilt"),
                perspective: gl.get_uniform_location(tilt_program, "u_perspective"),
                grid_size: gl.get_uniform_location(tilt_program, "u_grid_size"),
                light_dir: gl.get_uniform_location(tilt_program, "u_light_dir"),
            }
        };

        // Compile wobbly shader (uses wobbly vertex + standard fragment)
        let wobbly_program = unsafe { Self::create_program(&gl, shaders::WOBBLY_VERTEX_SHADER, shaders::FRAGMENT_SHADER)? };
        let wobbly_uniforms = unsafe {
            WobblyUniforms {
                projection: gl.get_uniform_location(wobbly_program, "u_projection"),
                rect: gl.get_uniform_location(wobbly_program, "u_rect"),
                texture: gl.get_uniform_location(wobbly_program, "u_texture"),
                opacity: gl.get_uniform_location(wobbly_program, "u_opacity"),
                radius: gl.get_uniform_location(wobbly_program, "u_radius"),
                size: gl.get_uniform_location(wobbly_program, "u_size"),
                dim: gl.get_uniform_location(wobbly_program, "u_dim"),
                uv_rect: gl.get_uniform_location(wobbly_program, "u_uv_rect"),
                grid_offsets: gl.get_uniform_location(wobbly_program, "u_grid_offsets"),
                grid_n: gl.get_uniform_location(wobbly_program, "u_grid_n"),
            }
        };

        // Compile overview background shader
        let overview_bg_program = unsafe { Self::create_program(&gl, shaders::VERTEX_SHADER, shaders::OVERVIEW_BG_FRAGMENT_SHADER)? };
        let overview_bg_uniforms = unsafe {
            OverviewBgUniforms {
                projection: gl.get_uniform_location(overview_bg_program, "u_projection"),
                rect: gl.get_uniform_location(overview_bg_program, "u_rect"),
                opacity: gl.get_uniform_location(overview_bg_program, "u_opacity"),
            }
        };

        // Compile particle shader
        let particle_program = unsafe { Self::create_program(&gl, shaders::PARTICLE_VERTEX_SHADER, shaders::PARTICLE_FRAGMENT_SHADER)? };
        let particle_uniforms = unsafe {
            ParticleUniforms {
                projection: gl.get_uniform_location(particle_program, "u_projection"),
                point_size: gl.get_uniform_location(particle_program, "u_point_size"),
            }
        };

        // Create particle VAO/VBO
        let (particle_vao, particle_vbo) = unsafe {
            let vao = gl.create_vertex_array().map_err(|e| format!("particle vao: {e}"))?;
            let vbo = gl.create_buffer().map_err(|e| format!("particle vbo: {e}"))?;
            gl.bind_vertex_array(Some(vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            // Layout: vec2 position, vec4 color, float life = 7 floats per vertex
            let stride = 7 * 4; // 7 floats * 4 bytes
            gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, stride, 0);
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_f32(1, 4, glow::FLOAT, false, stride, 2 * 4);
            gl.enable_vertex_attrib_array(1);
            gl.vertex_attrib_pointer_f32(2, 1, glow::FLOAT, false, stride, 6 * 4);
            gl.enable_vertex_attrib_array(2);
            gl.bind_vertex_array(None);
            gl.bind_buffer(glow::ARRAY_BUFFER, None);
            (vao, vbo)
        };

        // 15. Create VAO (empty — vertex shader generates quad from gl_VertexID)
        let quad_vao = unsafe {
            let vao = gl
                .create_vertex_array()
                .map_err(|e| format!("create vao: {e}"))?;
            gl.bind_vertex_array(Some(vao));
            gl.bind_vertex_array(None);
            vao
        };

        // Phase 3.2: Compile genie minimize shader
        let genie_program = unsafe { Self::create_program(&gl, shaders::GENIE_VERTEX_SHADER, shaders::FRAGMENT_SHADER)? };
        let genie_uniforms = unsafe {
            GenieUniforms {
                projection: gl.get_uniform_location(genie_program, "u_projection"),
                rect: gl.get_uniform_location(genie_program, "u_rect"),
                texture: gl.get_uniform_location(genie_program, "u_texture"),
                opacity: gl.get_uniform_location(genie_program, "u_opacity"),
                radius: gl.get_uniform_location(genie_program, "u_radius"),
                size: gl.get_uniform_location(genie_program, "u_size"),
                dim: gl.get_uniform_location(genie_program, "u_dim"),
                uv_rect: gl.get_uniform_location(genie_program, "u_uv_rect"),
                progress: gl.get_uniform_location(genie_program, "u_progress"),
                dock_pos: gl.get_uniform_location(genie_program, "u_dock_pos"),
                grid_size: gl.get_uniform_location(genie_program, "u_grid_size"),
            }
        };

        // 16. Setup GL state
        unsafe {
            gl.viewport(0, 0, screen_w as i32, screen_h as i32);
            gl.enable(glow::BLEND);
            gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
            gl.clear_color(0.0, 0.0, 0.0, 1.0);
        }

        log::info!(
            "Compositor initialized: {}x{}, overlay=0x{:x}, damage_event_base={}",
            screen_w,
            screen_h,
            overlay_window,
            damage_event_base
        );

        // Success — defuse the guard so it doesn't undo our redirect
        guard.active = false;

        // Set _NET_WM_WINDOW_TYPE on the overlay window so that screenshot tools
        // (e.g. Electron-based apps like Feishu/Lark) that enumerate windows via
        // XComposite will skip the overlay and not double-composite its contents
        // alongside the individual redirected window pixmaps.
        {
            let wm_type_atom = conn
                .intern_atom(false, b"_NET_WM_WINDOW_TYPE")
                .map_err(|e| format!("intern _NET_WM_WINDOW_TYPE: {e}"))?
                .reply()
                .map_err(|e| format!("intern reply: {e}"))?
                .atom;
            let notification_atom = conn
                .intern_atom(false, b"_NET_WM_WINDOW_TYPE_NOTIFICATION")
                .map_err(|e| format!("intern _NET_WM_WINDOW_TYPE_NOTIFICATION: {e}"))?
                .reply()
                .map_err(|e| format!("intern reply: {e}"))?
                .atom;
            conn.change_property32(
                xproto::PropMode::REPLACE,
                overlay_window,
                wm_type_atom,
                xproto::AtomEnum::ATOM,
                &[notification_atom],
            )
            .map_err(|e| format!("set overlay _NET_WM_WINDOW_TYPE: {e}"))?;
            let _ = conn.flush();
            log::info!(
                "compositor: set overlay 0x{:x} _NET_WM_WINDOW_TYPE = NOTIFICATION",
                overlay_window
            );
        }

        // Claim the _NET_WM_CM_Sn selection so that other clients (screenshot
        // tools, Electron apps like Feishu/Lark, etc.) know a compositing
        // manager is active and don't try to composite the screen themselves.
        let cm_selection_owner = {
            let sel_name = format!("_NET_WM_CM_S{}", screen_num);
            let cm_atom = conn
                .intern_atom(false, sel_name.as_bytes())
                .map_err(|e| format!("intern {sel_name}: {e}"))?
                .reply()
                .map_err(|e| format!("intern reply {sel_name}: {e}"))?
                .atom;
            let sel_win = conn
                .generate_id()
                .map_err(|e| format!("generate_id for CM selection owner: {e}"))?;
            conn.create_window(
                0, // copy_from_parent depth
                sel_win,
                root,
                0, 0, 1, 1, // off-screen 1x1
                0,
                xproto::WindowClass::INPUT_ONLY,
                0, // copy_from_parent visual
                &xproto::CreateWindowAux::default(),
            )
            .map_err(|e| format!("create CM selection owner window: {e}"))?;
            conn.set_selection_owner(sel_win, cm_atom, x11rb::CURRENT_TIME)
                .map_err(|e| format!("set_selection_owner {sel_name}: {e}"))?;
            let _ = conn.flush();
            log::info!(
                "compositor: claimed {} selection (owner=0x{:x})",
                sel_name, sel_win
            );
            sel_win
        };

        // Read compositor visual settings from config
        let cfg = crate::config::CONFIG.load();
        let behavior = cfg.behavior();
        let anim_speed = cfg.animation_speed();

        // Parse opacity rules ("opacity_percent:class_name")
        let opacity_rules: Vec<OpacityRule> = behavior.opacity_rules.iter().filter_map(|rule| {
            let parts: Vec<&str> = rule.splitn(2, ':').collect();
            if parts.len() == 2 {
                if let Ok(pct) = parts[0].trim().parse::<f32>() {
                    return Some(OpacityRule {
                        opacity: (pct / 100.0).clamp(0.0, 1.0),
                        class_name: parts[1].trim().to_string(),
                    });
                }
            }
            log::warn!("compositor: invalid opacity rule: {rule}");
            None
        }).collect();

        // Parse corner radius rules ("radius:class_name") — feature 3
        let corner_radius_rules: Vec<CornerRadiusRule> = behavior.corner_radius_rules.iter().filter_map(|rule| {
            let parts: Vec<&str> = rule.splitn(2, ':').collect();
            if parts.len() == 2 {
                if let Ok(r) = parts[0].trim().parse::<f32>() {
                    return Some(CornerRadiusRule {
                        radius: r.max(0.0),
                        class_name: parts[1].trim().to_string(),
                    });
                }
            }
            log::warn!("compositor: invalid corner radius rule: {rule}");
            None
        }).collect();

        // Parse scale rules ("scale_percent:class_name") — feature 4
        let scale_rules: Vec<ScaleRule> = behavior.scale_rules.iter().filter_map(|rule| {
            let parts: Vec<&str> = rule.splitn(2, ':').collect();
            if parts.len() == 2 {
                if let Ok(pct) = parts[0].trim().parse::<f32>() {
                    return Some(ScaleRule {
                        scale: (pct / 100.0).clamp(0.1, 2.0),
                        class_name: parts[1].trim().to_string(),
                    });
                }
            }
            log::warn!("compositor: invalid scale rule: {rule}");
            None
        }).collect();

        // Create blur FBOs if blur is enabled
        let blur_fbos = if behavior.blur_enabled {
            unsafe { Self::create_blur_fbos(&gl, screen_w, screen_h, behavior.blur_strength) }
        } else {
            Vec::new()
        };

        // Create scene capture FBO for blur source
        let scene_fbo = if behavior.blur_enabled {
            unsafe { Self::create_scene_fbo(&gl, screen_w, screen_h).ok() }
        } else {
            None
        };

        // Load wallpaper asynchronously — decode on background thread so the
        // desktop appears immediately and the wallpaper fades in once ready.
        let wallpaper_mode = Self::parse_wallpaper_mode(&behavior.wallpaper_mode);
        let pending_wallpaper = if !behavior.wallpaper.is_empty() {
            Some(Self::load_wallpaper_async(&behavior.wallpaper, screen_w, screen_h, wallpaper_mode))
        } else {
            None
        };

        // Create post-process FBO (features 8/9/10) — needed if any post-processing is active
        let needs_postprocess = behavior.color_temperature != 0.0
            || behavior.saturation != 1.0
            || behavior.brightness != 1.0
            || behavior.contrast != 1.0
            || behavior.invert_colors
            || behavior.grayscale;
        let postprocess_fbo = if needs_postprocess {
            unsafe { Self::create_scene_fbo(&gl, screen_w, screen_h).ok() }
        } else {
            None
        };

        Ok(Self {
            conn,
            xlib_display,
            tfp,
            glx_context,
            fbconfig_rgba,
            fbconfig_rgb,
            tfp_visual_configs,
            overlay_window,
            cm_selection_owner,
            glx_drawable,
            gl,
            program,
            shadow_program,
            blur_down_program,
            blur_up_program,
            win_uniforms,
            shadow_uniforms,
            blur_down_uniforms,
            blur_up_uniforms,
            quad_vao,
            windows: HashMap::new(),
            screen_w,
            screen_h,
            root,
            needs_render: true,
            context_current: true,
            last_scene_hash: 0,
            corner_radius: behavior.corner_radius,
            shadow_enabled: behavior.shadow_enabled,
            shadow_radius: behavior.shadow_radius,
            shadow_offset: behavior.shadow_offset,
            shadow_color: behavior.shadow_color,
            inactive_opacity: behavior.inactive_opacity,
            active_opacity: behavior.active_opacity,
            blur_enabled: behavior.blur_enabled,
            blur_strength: behavior.blur_strength,
            blur_fbos,
            scene_fbo,
            fading: behavior.fading,
            fade_in_step: anim_speed.apply_fade_step(behavior.fade_in_step),
            fade_out_step: anim_speed.apply_fade_step(behavior.fade_out_step),
            shadow_exclude: behavior.shadow_exclude.clone(),
            opacity_rules,
            blur_exclude: behavior.blur_exclude.clone(),
            rounded_corners_exclude: behavior.rounded_corners_exclude.clone(),
            detect_client_opacity: behavior.detect_client_opacity,
            fullscreen_unredirect: behavior.fullscreen_unredirect,
            unredirected_window: None,
            // Feature 1: borders
            border_program,
            border_uniforms,
            border_enabled: behavior.border_enabled,
            border_width: behavior.border_width,
            border_color_focused: behavior.border_color_focused,
            border_color_unfocused: behavior.border_color_unfocused,
            // Feature 3: per-window corner radius
            corner_radius_rules,
            // Feature 4: scale
            scale_rules,
            // Feature 6: damage tracking (tile-based, Phase 2.1)
            damage_tracker: DamageTracker::new(screen_w, screen_h),
            // Phase 2.2: Blur quality auto-downgrade
            blur_quality: BlurQuality::Full,
            blur_quality_auto: behavior.blur_quality_auto,
            // Feature 8: color management
            postprocess_program,
            postprocess_uniforms,
            postprocess_fbo,
            color_temperature: behavior.color_temperature,
            saturation: behavior.saturation,
            brightness: behavior.brightness,
            contrast: behavior.contrast,
            // Feature 10: invert / accessibility
            invert_colors: behavior.invert_colors,
            grayscale: behavior.grayscale,
            // Feature 11: debug HUD
            hud_program,
            hud_uniforms,
            hud_text_program,
            hud_text_uniforms,
            hud_text_texture: None,
            hud_text_width: 0,
            hud_text_height: 0,
            hud_text_cache: String::new(),
            debug_hud: behavior.debug_hud,
            frame_stats: FrameStats {
                frame_count: 0,
                last_fps_update: std::time::Instant::now(),
                fps: 0.0,
                frame_times: Vec::with_capacity(120),
                last_frame_time: std::time::Instant::now(),
                draw_calls: 0,
                texture_memory_bytes: 0,
                blur_cache_hits: 0,
                blur_cache_misses: 0,
            },
            // Feature 12: screenshot
            pending_screenshot: None,
            // Feature 13: blur mask
            blur_use_frame_extents: behavior.blur_use_frame_extents,
            // Feature 14: shadow shape
            shadow_bottom_extra: behavior.shadow_bottom_extra,
            // Tag-switch crossfade transition
            transition_program,
            transition_uniforms,
            transition_fbo: None,
            transition_start: None,
            transition_duration: std::time::Duration::from_millis(anim_speed.apply_duration(150)),
            transition_direction: 1.0,
            transition_exclude_top: 0,
            transition_mon_x: 0,
            transition_mon_y: 0,
            transition_mon_w: screen_w,
            transition_mon_h: screen_h,
            transition_mode: match behavior.transition_mode.as_str() {
                "cube" => TransitionMode::Cube,
                "fade" => TransitionMode::Fade,
                "flip" => TransitionMode::Flip,
                "zoom" => TransitionMode::Zoom,
                "stack" => TransitionMode::Stack,
                "blinds" => TransitionMode::Blinds,
                "coverflow" => TransitionMode::CoverFlow,
                "helix" => TransitionMode::Helix,
                "portal" => TransitionMode::Portal,
                _ => TransitionMode::Slide,
            },
            // Cube transition
            cube_program,
            cube_uniforms,
            transition_new_fbo: None,
            // Portal transition
            portal_program,
            portal_uniforms,
            // Window scale animation
            window_animation: behavior.window_animation,
            window_animation_scale: behavior.window_animation_scale,
            // Dim inactive
            inactive_dim: behavior.inactive_dim,
            // Mouse position
            mouse_x: 0.0,
            mouse_y: 0.0,
            // Edge glow
            edge_glow_program,
            edge_glow_uniforms,
            edge_glow: behavior.edge_glow,
            edge_glow_active: false,
            edge_glow_suppressed: false,
            edge_glow_color: behavior.edge_glow_color,
            edge_glow_width: behavior.edge_glow_width,
            // Attention animation
            attention_animation: behavior.attention_animation,
            attention_color: behavior.attention_color,
            compositor_start_time: std::time::Instant::now(),
            // PiP visual
            pip_border_color: behavior.pip_border_color,
            pip_border_width: behavior.pip_border_width,
            // Magnifier
            magnifier_enabled: behavior.magnifier_enabled,
            magnifier_radius: behavior.magnifier_radius,
            magnifier_zoom: behavior.magnifier_zoom,
            magnifier_uniforms,
            // Window tilt
            tilt_program,
            tilt_uniforms,
            window_tilt: behavior.window_tilt,
            tilt_amount: behavior.tilt_amount,
            tilt_perspective: behavior.tilt_perspective,
            tilt_speed: behavior.tilt_speed,
            tilt_grid: behavior.tilt_grid.max(1),
            tilt_current_x: 0.0,
            tilt_current_y: 0.0,
            tilt_target_x: 0.0,
            tilt_target_y: 0.0,
            // Frosted glass
            frosted_glass_rules: behavior.frosted_glass_rules.clone(),
            frosted_glass_strength: behavior.frosted_glass_strength,
            blur_cache_hash: 0,
            // Overview
            overview_active: false,
            overview_windows: Vec::new(),
            overview_opacity: 0.0,
            overview_bg_program,
            overview_bg_uniforms,
            // Overview prism state
            overview_prism_target_angle: 0.0,
            overview_prism_current_angle: 0.0,
            overview_prism_last_tick: None,
            overview_slide_offset: 0,
            overview_total_clients: 0,
            overview_mon_x: 0,
            overview_mon_y: 0,
            overview_mon_w: screen_w,
            overview_mon_h: screen_h,
            overview_entry_progress: 1.0,
            overview_closing: false,
            overview_exit_progress: 1.0,
            // Wobbly windows
            wobbly_program,
            wobbly_uniforms,
            wobbly_windows: behavior.wobbly_windows,
            wobbly_stiffness: behavior.wobbly_stiffness,
            wobbly_damping: behavior.wobbly_damping,
            wobbly_restore_stiffness: behavior.wobbly_restore_stiffness,
            wobbly_grid_size: behavior.wobbly_grid_size,
            // Phase 5: Expose/Mission Control
            expose_active: false,
            expose_enabled: behavior.expose_enabled,
            expose_gap: behavior.expose_gap,
            expose_entries: Vec::new(),
            expose_opacity: 0.0,
            expose_start: None,
            // Phase 5: Smart Snap Preview
            snap_preview_enabled: behavior.snap_preview,
            snap_preview_color: behavior.snap_preview_color,
            snap_animation_duration_ms: behavior.snap_animation_duration_ms,
            snap_target: None,
            // Phase 5: Window Peek
            peek_active: false,
            peek_enabled: behavior.peek_enabled,
            peek_exclude: behavior.peek_exclude.clone(),
            peek_opacity: 1.0,
            peek_start: None,
            // Phase 5: Window Tabs
            window_tabs_enabled: behavior.window_tabs,
            tab_bar_height: behavior.tab_bar_height,
            tab_bar_color: behavior.tab_bar_color,
            tab_active_color: behavior.tab_active_color,
            window_groups: HashMap::new(),
            // Particle effects
            particle_program,
            particle_uniforms,
            particle_effects: behavior.particle_effects,
            particle_count: behavior.particle_count,
            particle_lifetime: behavior.particle_lifetime,
            particle_gravity: behavior.particle_gravity,
            particle_systems: Vec::new(),
            particle_vao,
            particle_vbo,
            // Wallpaper (texture loaded asynchronously)
            wallpaper_texture: None,
            wallpaper_mode,
            wallpaper_path: behavior.wallpaper.clone(),
            wallpaper_img_w: 0,
            wallpaper_img_h: 0,
            monitor_wallpapers: Vec::new(),
            // Phase 6.1: Colorblind correction
            colorblind_mode: match behavior.colorblind_mode.as_str() {
                "deuteranopia" => 1,
                "protanopia" => 2,
                "tritanopia" => 3,
                _ => 0,
            },
            // Phase 6.2: Annotations
            annotation_active: false,
            annotation_strokes: Vec::new(),
            annotation_color: behavior.annotation_color,
            annotation_line_width: behavior.annotation_line_width,
            // Phase 6.3: Zoom to fit
            zoom_to_fit_window: None,
            zoom_to_fit_scale: 1.0,
            zoom_to_fit_target: 1.0,
            // Phase 7.2: Extended debug HUD
            debug_hud_extended: behavior.debug_hud_extended,
            // Phase 7.3: Screen recording
            recording_active: false,
            recording_fps: behavior.recording_fps,
            recording_bitrate: behavior.recording_bitrate.clone(),
            recording_quality: behavior.recording_quality,
            recording_encoder: behavior.recording_encoder.clone(),
            recording_output_dir: behavior.recording_output_dir.clone(),
            recording_process: None,
            recording_last_frame: None,
            recording_pbo: [None, None],
            // Phase 3.1: Motion trail
            motion_trail_enabled: behavior.motion_trail,
            motion_trail_frames: behavior.motion_trail_frames,
            motion_trail_opacity: behavior.motion_trail_opacity,
            // Phase 3.2: Genie minimize
            genie_program,
            genie_uniforms,
            genie_minimize: behavior.genie_minimize,
            genie_duration_ms: behavior.genie_duration_ms,
            genie_active: Vec::new(),
            dock_position: (0.5 * screen_w as f32, screen_h as f32),
            // Phase 3.3: Ripple on open
            ripple_on_open: behavior.ripple_on_open,
            ripple_duration: behavior.ripple_duration,
            ripple_amplitude: behavior.ripple_amplitude,
            ripple_active: Vec::new(),
            // Phase 3.4: Focus highlight
            focus_highlight: behavior.focus_highlight,
            focus_highlight_color: behavior.focus_highlight_color,
            focus_highlight_duration_ms: behavior.focus_highlight_duration_ms,
            focus_highlight_start: None,
            last_focused_window: None,
            // Phase 3.5: Wallpaper crossfade
            wallpaper_crossfade: behavior.wallpaper_crossfade,
            wallpaper_crossfade_duration_ms: behavior.wallpaper_crossfade_duration_ms,
            old_wallpaper_texture: None,
            wallpaper_transition_start: None,
            // Async wallpaper loading
            pending_wallpaper,
            pending_monitor_wallpapers: Vec::new(),
        })
    }

    unsafe fn create_program(gl: &glow::Context, vs_src: &str, fs_src: &str) -> Result<glow::Program, String> {
        unsafe {
            let vs = gl
                .create_shader(glow::VERTEX_SHADER)
                .map_err(|e| format!("create vs: {e}"))?;
            gl.shader_source(vs, vs_src);
            gl.compile_shader(vs);
            if !gl.get_shader_compile_status(vs) {
                let info = gl.get_shader_info_log(vs);
                gl.delete_shader(vs);
                return Err(format!("vertex shader: {info}"));
            }

            let fs = gl
                .create_shader(glow::FRAGMENT_SHADER)
                .map_err(|e| format!("create fs: {e}"))?;
            gl.shader_source(fs, fs_src);
            gl.compile_shader(fs);
            if !gl.get_shader_compile_status(fs) {
                let info = gl.get_shader_info_log(fs);
                gl.delete_shader(vs);
                gl.delete_shader(fs);
                return Err(format!("fragment shader: {info}"));
            }

            let program = gl
                .create_program()
                .map_err(|e| format!("create program: {e}"))?;
            gl.attach_shader(program, vs);
            gl.attach_shader(program, fs);
            gl.link_program(program);
            if !gl.get_program_link_status(program) {
                let info = gl.get_program_info_log(program);
                gl.delete_program(program);
                gl.delete_shader(vs);
                gl.delete_shader(fs);
                return Err(format!("link program: {info}"));
            }
            gl.delete_shader(vs);
            gl.delete_shader(fs);
            Ok(program)
        }
    }

    /// Decode a wallpaper image on a background thread.
    /// Returns a receiver that will deliver the decoded RGBA data.
    fn load_wallpaper_async(
        path: &str,
        max_w: u32,
        max_h: u32,
        mode: WallpaperMode,
    ) -> mpsc::Receiver<WallpaperImageData> {
        let (tx, rx) = mpsc::channel();
        let path = path.to_string();
        std::thread::spawn(move || {
            let img = match image::open(&path) {
                Ok(img) => img,
                Err(e) => {
                    log::warn!("compositor: failed to load wallpaper '{}': {}", path, e);
                    return;
                }
            };

            let img = if max_w > 0 && max_h > 0 && (img.width() > max_w || img.height() > max_h) {
                log::info!(
                    "compositor: downscaling wallpaper '{}' from {}x{} to fit {}x{}",
                    path, img.width(), img.height(), max_w, max_h,
                );
                img.resize(max_w, max_h, image::imageops::FilterType::Lanczos3)
            } else {
                img
            };

            let rgba = img.to_rgba8();
            let (w, h) = (rgba.width(), rgba.height());
            log::info!("compositor: decoded wallpaper '{}' ({}x{})", path, w, h);

            let _ = tx.send(WallpaperImageData {
                rgba: rgba.into_raw(),
                width: w,
                height: h,
                mode,
            });
        });
        rx
    }

    /// Upload decoded wallpaper RGBA data to a GL texture.
    fn upload_wallpaper_texture(
        gl: &glow::Context,
        data: &WallpaperImageData,
    ) -> Option<(glow::Texture, u32, u32)> {
        unsafe {
            let tex = match gl.create_texture() {
                Ok(t) => t,
                Err(e) => {
                    log::warn!("compositor: failed to create wallpaper texture: {}", e);
                    return None;
                }
            };
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                data.width as i32,
                data.height as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(&data.rgba)),
            );
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
            gl.bind_texture(glow::TEXTURE_2D, None);
            log::info!("compositor: uploaded wallpaper texture ({}x{})", data.width, data.height);
            Some((tex, data.width, data.height))
        }
    }


    /// Compute the draw rect (x, y, w, h) for a wallpaper within a target area.
    /// `area`: (x, y, w, h) of the target area in screen coords.
    /// `img_w`, `img_h`: source image dimensions.
    fn compute_wallpaper_rect(
        mode: WallpaperMode,
        area: (f32, f32, f32, f32),
        img_w: u32,
        img_h: u32,
    ) -> (f32, f32, f32, f32) {
        let (ax, ay, aw, ah) = area;
        let iw = img_w as f32;
        let ih = img_h as f32;
        if iw <= 0.0 || ih <= 0.0 {
            return (ax, ay, aw, ah);
        }
        match mode {
            WallpaperMode::Stretch => (ax, ay, aw, ah),
            WallpaperMode::Fill => {
                let scale = (aw / iw).max(ah / ih);
                let dw = iw * scale;
                let dh = ih * scale;
                let dx = ax + (aw - dw) * 0.5;
                let dy = ay + (ah - dh) * 0.5;
                (dx, dy, dw, dh)
            }
            WallpaperMode::Fit => {
                let scale = (aw / iw).min(ah / ih);
                let dw = iw * scale;
                let dh = ih * scale;
                let dx = ax + (aw - dw) * 0.5;
                let dy = ay + (ah - dh) * 0.5;
                (dx, dy, dw, dh)
            }
            WallpaperMode::Center => {
                let dx = ax + (aw - iw) * 0.5;
                let dy = ay + (ah - ih) * 0.5;
                (dx, dy, iw, ih)
            }
        }
    }

    fn parse_wallpaper_mode(s: &str) -> WallpaperMode {
        match s {
            "fit" => WallpaperMode::Fit,
            "stretch" => WallpaperMode::Stretch,
            "center" => WallpaperMode::Center,
            _ => WallpaperMode::Fill,
        }
    }

    /// Update monitor geometries and per-monitor wallpaper textures.
    /// Called when monitors are added/removed/changed.
    /// `monitors`: list of (index, x, y, w, h) for each monitor.
    pub(super) fn set_monitors(&mut self, monitors: &[(u32, i32, i32, u32, u32)]) {
        // Phase 3.5: Save old wallpaper texture for crossfade
        if self.wallpaper_crossfade && self.wallpaper_texture.is_some() {
            if let Some(old) = self.old_wallpaper_texture.take() {
                unsafe { self.gl.delete_texture(old); }
            }
            self.old_wallpaper_texture = self.wallpaper_texture;
            self.wallpaper_transition_start = Some(std::time::Instant::now());
        }

        // Clean up old per-monitor textures
        unsafe {
            for mw in self.monitor_wallpapers.drain(..) {
                if let Some(tex) = mw.texture {
                    self.gl.delete_texture(tex);
                }
            }
        }

        let cfg = crate::config::CONFIG.load();
        let behavior = cfg.behavior();

        // Clear any previous pending monitor wallpaper loads
        self.pending_monitor_wallpapers.clear();

        for &(idx, x, y, w, h) in monitors {
            // Check if there's a per-monitor config for this index
            let per_mon = behavior.wallpaper_monitors.iter().find(|wm| wm.monitor == idx);

            let (path, mode_str) = if let Some(pm) = per_mon {
                (
                    if pm.path.is_empty() { &behavior.wallpaper } else { &pm.path },
                    if pm.mode.is_empty() { &behavior.wallpaper_mode } else { &pm.mode },
                )
            } else {
                (&behavior.wallpaper, &behavior.wallpaper_mode)
            };

            let mode = Self::parse_wallpaper_mode(mode_str);
            let mon_idx = self.monitor_wallpapers.len();

            // Spawn async decode for per-monitor wallpaper
            if !path.is_empty() {
                let rx = Self::load_wallpaper_async(path, self.screen_w, self.screen_h, mode);
                self.pending_monitor_wallpapers.push((mon_idx, rx));
            }

            self.monitor_wallpapers.push(MonitorWallpaper {
                mon_x: x,
                mon_y: y,
                mon_w: w,
                mon_h: h,
                texture: None, // will be filled when async load completes
                mode,
                img_w: 0,
                img_h: 0,
            });
        }

        self.needs_render = true;
        log::info!(
            "compositor: set_monitors: {} monitors, {} with wallpaper overrides",
            monitors.len(),
            behavior.wallpaper_monitors.len(),
        );
    }

    /// Check if a window class matches any entry in an exclude list.
    fn class_matches_exclude(class_name: &str, exclude_list: &[String]) -> bool {
        if class_name.is_empty() {
            return false;
        }
        // Screenshot overlays like Flameshot are full-screen translucent windows
        // that update every pointer move. Running blur/shadow/rounding on them is
        // very expensive and causes visible stutter during region selection.
        if class_name.eq_ignore_ascii_case("flameshot") {
            return true;
        }
        exclude_list.iter().any(|ex| ex.eq_ignore_ascii_case(class_name))
    }

    /// Look up per-window opacity from opacity_rules.
    fn lookup_opacity_rule(&self, class_name: &str) -> Option<f32> {
        if class_name.is_empty() {
            return None;
        }
        for rule in &self.opacity_rules {
            if rule.class_name.eq_ignore_ascii_case(class_name) {
                return Some(rule.opacity);
            }
        }
        None
    }

    /// Look up per-window corner radius (feature 3).
    fn lookup_corner_radius_rule(&self, class_name: &str) -> Option<f32> {
        if class_name.is_empty() {
            return None;
        }
        for rule in &self.corner_radius_rules {
            if rule.class_name.eq_ignore_ascii_case(class_name) {
                return Some(rule.radius);
            }
        }
        None
    }

    /// Look up whether a window should have frosted glass effect.
    fn lookup_frosted_glass_rule(&self, class_name: &str) -> bool {
        if class_name.is_empty() {
            return false;
        }
        self.frosted_glass_rules.iter().any(|r| r.eq_ignore_ascii_case(class_name))
    }

    /// Whether a window should receive per-frame backdrop blur compositing.
    fn needs_backdrop_blur(&self, wt: &WindowTexture) -> bool {
        if Self::class_matches_exclude(&wt.class_name, &self.blur_exclude) {
            return false;
        }
        // Skip backdrop blur for large override-redirect RGBA windows.  These
        // are typically screen-sharing overlays (e.g. Feishu/Lark) or screenshot
        // selection tools that are intentionally transparent.  Applying blur
        // behind them produces an unwanted frosted-glass effect that covers the
        // actual screen content.
        //
        // "Large" = covers at least 80 % of any single monitor in both dimensions.
        if wt.is_override_redirect && wt.has_rgba {
            let dominated = self.monitor_wallpapers.iter().any(|mw| {
                wt.w >= mw.mon_w * 4 / 5 && wt.h >= mw.mon_h * 4 / 5
            });
            if dominated {
                return false;
            }
        }
        wt.is_frosted
            || wt.has_rgba
            || wt.fade_opacity < 1.0
            || wt.opacity_override.map_or(false, |o| o < 1.0)
    }

    /// Look up per-window scale (feature 4).
    fn lookup_scale_rule(&self, class_name: &str) -> Option<f32> {
        if class_name.is_empty() {
            return None;
        }
        for rule in &self.scale_rules {
            if rule.class_name.eq_ignore_ascii_case(class_name) {
                return Some(rule.scale);
            }
        }
        None
    }

    pub(super) fn needs_render(&self) -> bool {
        if self.needs_render || self.recording_active {
            return true;
        }
        // Also need render if any fade animations are in progress
        if self.fading {
            for wt in self.windows.values() {
                if wt.fading_out || wt.fade_opacity < 1.0 {
                    return true;
                }
            }
        }
        // Need render if overview or expose is active (or expose exit animation in progress)
        if self.overview_active || self.expose_active || !self.expose_entries.is_empty() { return true; }
        // Need render if particles are active
        if !self.particle_systems.is_empty() { return true; }
        // Need render if any window has active wobbly
        if self.wobbly_windows {
            for wt in self.windows.values() {
                if let Some(ref w) = wt.wobbly {
                    if w.dragging || w.offsets.iter().any(|o| o[0].abs() > 0.1 || o[1].abs() > 0.1) {
                        return true;
                    }
                }
            }
        }
        // Need render if attention animation is active for any window
        if self.attention_animation {
            for wt in self.windows.values() {
                if wt.is_urgent { return true; }
            }
        }
        // Need render if magnifier is active (tracking mouse)
        if self.magnifier_enabled { return true; }
        // Need render if edge glow is active (mouse near screen edge)
        if self.edge_glow && self.edge_glow_active { return true; }
        // Need render if window tilt is animating
        if self.window_tilt {
            let epsilon = 0.0001;
            if (self.tilt_current_x - self.tilt_target_x).abs() > epsilon
                || (self.tilt_current_y - self.tilt_target_y).abs() > epsilon
                || self.tilt_current_x.abs() > epsilon
                || self.tilt_current_y.abs() > epsilon
            {
                return true;
            }
        }
        // Need render if scale animation active
        if self.window_animation {
            for wt in self.windows.values() {
                if (wt.anim_scale - wt.anim_scale_target).abs() > 0.001 { return true; }
            }
        }
        // Need render to poll async wallpaper loading
        if self.pending_wallpaper.is_some() || !self.pending_monitor_wallpapers.is_empty() {
            return true;
        }
        false
    }

    pub(super) fn overlay_window(&self) -> u32 {
        self.overlay_window
    }

    pub(super) fn clear_needs_render(&mut self) {
        self.needs_render = false;
    }

    // =====================================================================
    // Feature 8/9/10: Runtime post-processing toggles
    // =====================================================================
    pub(super) fn set_color_temperature(&mut self, temp: f32) {
        if (self.color_temperature - temp).abs() > f32::EPSILON {
            self.color_temperature = temp;
            self.ensure_postprocess_fbo();
            self.needs_render = true;
        }
    }

    pub(super) fn set_saturation(&mut self, sat: f32) {
        if (self.saturation - sat).abs() > f32::EPSILON {
            self.saturation = sat;
            self.ensure_postprocess_fbo();
            self.needs_render = true;
        }
    }

    pub(super) fn set_brightness(&mut self, val: f32) {
        if (self.brightness - val).abs() > f32::EPSILON {
            self.brightness = val;
            self.ensure_postprocess_fbo();
            self.needs_render = true;
        }
    }

    pub(super) fn set_contrast(&mut self, val: f32) {
        if (self.contrast - val).abs() > f32::EPSILON {
            self.contrast = val;
            self.ensure_postprocess_fbo();
            self.needs_render = true;
        }
    }

    pub(super) fn set_invert_colors(&mut self, invert: bool) {
        if self.invert_colors != invert {
            self.invert_colors = invert;
            self.ensure_postprocess_fbo();
            self.needs_render = true;
        }
    }

    pub(super) fn set_grayscale(&mut self, gs: bool) {
        if self.grayscale != gs {
            self.grayscale = gs;
            self.ensure_postprocess_fbo();
            self.needs_render = true;
        }
    }

    // =====================================================================
    // Hot-reload: apply all config changes at once
    // =====================================================================

    /// Re-sync all cached compositor fields from the current config.
    /// Called on config file hot-reload so users don't need to restart.
    pub(super) fn apply_config(&mut self) {
        let cfg = crate::config::CONFIG.load();
        let behavior = cfg.behavior();
        let anim_speed = cfg.animation_speed();

        // --- Core visual settings ---
        self.corner_radius = behavior.corner_radius;
        self.shadow_enabled = behavior.shadow_enabled;
        self.shadow_radius = behavior.shadow_radius;
        self.shadow_offset = behavior.shadow_offset;
        self.shadow_color = behavior.shadow_color;
        self.shadow_bottom_extra = behavior.shadow_bottom_extra;
        self.inactive_opacity = behavior.inactive_opacity;
        self.active_opacity = behavior.active_opacity;
        self.fading = behavior.fading;
        self.fade_in_step = anim_speed.apply_fade_step(behavior.fade_in_step);
        self.fade_out_step = anim_speed.apply_fade_step(behavior.fade_out_step);
        self.detect_client_opacity = behavior.detect_client_opacity;
        self.fullscreen_unredirect = behavior.fullscreen_unredirect;
        self.blur_use_frame_extents = behavior.blur_use_frame_extents;
        self.blur_quality_auto = behavior.blur_quality_auto;

        // --- Blur (may need FBO rebuild) ---
        if self.blur_enabled != behavior.blur_enabled || self.blur_strength != behavior.blur_strength {
            // Tear down old blur FBOs
            unsafe {
                for level in self.blur_fbos.drain(..) {
                    self.gl.delete_framebuffer(level.fbo);
                    self.gl.delete_texture(level.texture);
                }
            }
            self.blur_enabled = behavior.blur_enabled;
            self.blur_strength = behavior.blur_strength;
            // Recreate if enabled
            if self.blur_enabled {
                self.blur_fbos = unsafe {
                    Self::create_blur_fbos(&self.gl, self.screen_w, self.screen_h, self.blur_strength)
                };
            }
        }

        // --- Per-window rules (re-parse from strings) ---
        self.shadow_exclude = behavior.shadow_exclude.clone();
        self.blur_exclude = behavior.blur_exclude.clone();
        self.rounded_corners_exclude = behavior.rounded_corners_exclude.clone();

        self.opacity_rules = behavior.opacity_rules.iter().filter_map(|rule| {
            let parts: Vec<&str> = rule.splitn(2, ':').collect();
            if parts.len() == 2 {
                if let Ok(pct) = parts[0].trim().parse::<f32>() {
                    return Some(OpacityRule {
                        opacity: (pct / 100.0).clamp(0.0, 1.0),
                        class_name: parts[1].trim().to_string(),
                    });
                }
            }
            None
        }).collect();

        self.corner_radius_rules = behavior.corner_radius_rules.iter().filter_map(|rule| {
            let parts: Vec<&str> = rule.splitn(2, ':').collect();
            if parts.len() == 2 {
                if let Ok(r) = parts[0].trim().parse::<f32>() {
                    return Some(CornerRadiusRule {
                        radius: r.max(0.0),
                        class_name: parts[1].trim().to_string(),
                    });
                }
            }
            None
        }).collect();

        self.scale_rules = behavior.scale_rules.iter().filter_map(|rule| {
            let parts: Vec<&str> = rule.splitn(2, ':').collect();
            if parts.len() == 2 {
                if let Ok(pct) = parts[0].trim().parse::<f32>() {
                    return Some(ScaleRule {
                        scale: (pct / 100.0).clamp(0.1, 2.0),
                        class_name: parts[1].trim().to_string(),
                    });
                }
            }
            None
        }).collect();

        // --- Borders ---
        self.border_enabled = behavior.border_enabled;
        self.border_width = behavior.border_width;
        self.border_color_focused = behavior.border_color_focused;
        self.border_color_unfocused = behavior.border_color_unfocused;

        // --- Color post-processing (use existing setters for postprocess FBO management) ---
        self.set_color_temperature(behavior.color_temperature);
        self.set_saturation(behavior.saturation);
        self.set_brightness(behavior.brightness);
        self.set_contrast(behavior.contrast);
        self.set_invert_colors(behavior.invert_colors);
        self.set_grayscale(behavior.grayscale);
        self.set_colorblind_mode(&behavior.colorblind_mode);

        // --- Debug HUD ---
        self.debug_hud = behavior.debug_hud;
        self.debug_hud_extended = behavior.debug_hud_extended;

        // --- Transition mode ---
        self.set_transition_mode(&behavior.transition_mode);
        self.transition_duration = std::time::Duration::from_millis(anim_speed.apply_duration(150));

        // --- Window animation ---
        self.window_animation = behavior.window_animation;
        self.window_animation_scale = behavior.window_animation_scale;

        // --- Dim inactive ---
        self.inactive_dim = behavior.inactive_dim;

        // --- Edge glow ---
        self.edge_glow = behavior.edge_glow;
        self.edge_glow_color = behavior.edge_glow_color;
        self.edge_glow_width = behavior.edge_glow_width;

        // --- Attention animation ---
        self.attention_animation = behavior.attention_animation;
        self.attention_color = behavior.attention_color;

        // --- PiP ---
        self.pip_border_color = behavior.pip_border_color;
        self.pip_border_width = behavior.pip_border_width;

        // --- Magnifier ---
        self.magnifier_enabled = behavior.magnifier_enabled;
        self.magnifier_radius = behavior.magnifier_radius;
        self.magnifier_zoom = behavior.magnifier_zoom;

        // --- Window tilt ---
        self.window_tilt = behavior.window_tilt;
        self.tilt_amount = behavior.tilt_amount;
        self.tilt_perspective = behavior.tilt_perspective;
        self.tilt_speed = behavior.tilt_speed;
        self.tilt_grid = behavior.tilt_grid.max(1);

        // --- Frosted glass ---
        self.frosted_glass_rules = behavior.frosted_glass_rules.clone();
        self.frosted_glass_strength = behavior.frosted_glass_strength;

        // --- Wobbly windows ---
        self.wobbly_windows = behavior.wobbly_windows;
        self.wobbly_stiffness = behavior.wobbly_stiffness;
        self.wobbly_damping = behavior.wobbly_damping;
        self.wobbly_restore_stiffness = behavior.wobbly_restore_stiffness;
        self.wobbly_grid_size = behavior.wobbly_grid_size;

        // --- Expose ---
        self.expose_enabled = behavior.expose_enabled;
        self.expose_gap = behavior.expose_gap;

        // --- Snap preview ---
        self.snap_preview_enabled = behavior.snap_preview;
        self.snap_preview_color = behavior.snap_preview_color;
        self.snap_animation_duration_ms = behavior.snap_animation_duration_ms;

        // --- Peek ---
        self.peek_enabled = behavior.peek_enabled;
        self.peek_exclude = behavior.peek_exclude.clone();

        // --- Window tabs ---
        self.window_tabs_enabled = behavior.window_tabs;
        self.tab_bar_height = behavior.tab_bar_height;
        self.tab_bar_color = behavior.tab_bar_color;
        self.tab_active_color = behavior.tab_active_color;

        // --- Particle effects ---
        self.particle_effects = behavior.particle_effects;
        self.particle_count = behavior.particle_count;
        self.particle_lifetime = behavior.particle_lifetime;
        self.particle_gravity = behavior.particle_gravity;

        // --- Motion trail ---
        self.motion_trail_enabled = behavior.motion_trail;
        self.motion_trail_frames = behavior.motion_trail_frames;
        self.motion_trail_opacity = behavior.motion_trail_opacity;

        // --- Genie minimize ---
        self.genie_minimize = behavior.genie_minimize;
        self.genie_duration_ms = behavior.genie_duration_ms;

        // --- Ripple on open ---
        self.ripple_on_open = behavior.ripple_on_open;
        self.ripple_duration = behavior.ripple_duration;
        self.ripple_amplitude = behavior.ripple_amplitude;

        // --- Focus highlight ---
        self.focus_highlight = behavior.focus_highlight;
        self.focus_highlight_color = behavior.focus_highlight_color;
        self.focus_highlight_duration_ms = behavior.focus_highlight_duration_ms;

        // --- Wallpaper crossfade ---
        self.wallpaper_crossfade = behavior.wallpaper_crossfade;
        self.wallpaper_crossfade_duration_ms = behavior.wallpaper_crossfade_duration_ms;

        // --- Annotations ---
        self.annotation_color = behavior.annotation_color;
        self.annotation_line_width = behavior.annotation_line_width;

        // --- Recording ---
        self.recording_fps = behavior.recording_fps;
        self.recording_bitrate = behavior.recording_bitrate.clone();
        self.recording_quality = behavior.recording_quality;
        self.recording_encoder = behavior.recording_encoder.clone();
        self.recording_output_dir = behavior.recording_output_dir.clone();

        // --- Wallpaper (trigger async reload if path or mode changed) ---
        let new_mode = Self::parse_wallpaper_mode(&behavior.wallpaper_mode);
        if behavior.wallpaper != self.wallpaper_path || new_mode != self.wallpaper_mode {
            self.wallpaper_mode = new_mode;
            self.wallpaper_path = behavior.wallpaper.clone();
            if !self.wallpaper_path.is_empty() {
                self.pending_wallpaper = Some(Self::load_wallpaper_async(
                    &self.wallpaper_path,
                    self.screen_w,
                    self.screen_h,
                    self.wallpaper_mode,
                ));
            }
        }

        self.needs_render = true;
    }

    // =====================================================================
    // Tag-switch slide transition
    // =====================================================================

    /// Called just before a tag switch. Captures the current back-buffer into
    /// a snapshot texture so `render_frame` can slide the old scene out.
    /// `mon_rect` is (x, y, w, h) of the monitor where the switch happens.
    pub(super) fn notify_tag_switch(
        &mut self,
        duration: std::time::Duration,
        direction: i32,
        exclude_top: u32,
        mon_rect: (i32, i32, u32, u32),
    ) {
        // Ensure GL context is current
        if !self.context_current {
            unsafe {
                x11::glx::glXMakeContextCurrent(
                    self.xlib_display,
                    self.glx_drawable,
                    self.glx_drawable,
                    self.glx_context,
                );
            }
            self.context_current = true;
        }

        let (mon_x, mon_y, mon_w, mon_h) = mon_rect;
        let mon_w = mon_w.max(1);
        let mon_h = mon_h.max(1);

        // Recreate FBOs if monitor size changed
        let size_changed = self.transition_fbo.as_ref().map_or(true, |_| {
            self.transition_mon_w != mon_w || self.transition_mon_h != mon_h
        });
        if size_changed {
            if let Some((fbo, tex)) = self.transition_fbo.take() {
                unsafe {
                    self.gl.delete_framebuffer(fbo);
                    self.gl.delete_texture(tex);
                }
            }
            if let Some((fbo, tex)) = self.transition_new_fbo.take() {
                unsafe {
                    self.gl.delete_framebuffer(fbo);
                    self.gl.delete_texture(tex);
                }
            }
        }

        // Create snapshot FBO at monitor size
        if self.transition_fbo.is_none() {
            self.transition_fbo = unsafe {
                Self::create_scene_fbo(&self.gl, mon_w, mon_h).ok()
            };
        }

        // Create new-scene FBO for modes that need both old and new textures
        let needs_new_fbo = matches!(self.transition_mode, TransitionMode::Cube | TransitionMode::Flip | TransitionMode::Blinds | TransitionMode::CoverFlow | TransitionMode::Helix | TransitionMode::Portal);
        if needs_new_fbo && self.transition_new_fbo.is_none() {
            self.transition_new_fbo = unsafe {
                Self::create_scene_fbo(&self.gl, mon_w, mon_h).ok()
            };
        }

        // Store monitor rect for rendering
        self.transition_mon_x = mon_x;
        self.transition_mon_y = mon_y;
        self.transition_mon_w = mon_w;
        self.transition_mon_h = mon_h;

        if let Some((snap_fbo, _)) = &self.transition_fbo {
            // OpenGL Y is flipped: glY = screen_h - (mon_y + mon_h)
            let gl_y = self.screen_h as i32 - (mon_y + mon_h as i32);
            unsafe {
                // Blit only the monitor region from back-buffer into snapshot FBO
                self.gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
                self.gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(*snap_fbo));
                self.gl.blit_framebuffer(
                    mon_x, gl_y,
                    mon_x + mon_w as i32, gl_y + mon_h as i32,
                    0, 0, mon_w as i32, mon_h as i32,
                    glow::COLOR_BUFFER_BIT,
                    glow::NEAREST,
                );
                self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            }
            self.transition_start = Some(std::time::Instant::now());
            self.transition_duration = duration;
            self.transition_direction = if direction >= 0 { 1.0 } else { -1.0 };
            self.transition_exclude_top = exclude_top.min(mon_h.saturating_sub(1));
            // Tag switch can radically change visible scene; force a full redraw
            // to avoid stale pixels from partial-damage scissor regions.
            self.damage_tracker.mark_all_dirty();
            self.needs_render = true;
            log::debug!(
                "compositor: tag-switch slide transition started ({:?}, dir={}, mon={}x{}+{}+{})",
                duration,
                direction,
                mon_w, mon_h, mon_x, mon_y,
            );
        }
    }

    pub(super) fn force_full_redraw(&mut self) {
        self.damage_tracker.mark_all_dirty();
        self.needs_render = true;
    }

    // =====================================================================
    // Feature 11: Debug HUD toggle
    // =====================================================================
    pub(super) fn set_transition_mode(&mut self, mode: &str) {
        let new_mode = match mode {
            "cube" => TransitionMode::Cube,
            "fade" => TransitionMode::Fade,
            "flip" => TransitionMode::Flip,
            "zoom" => TransitionMode::Zoom,
            "stack" => TransitionMode::Stack,
            "blinds" => TransitionMode::Blinds,
            "coverflow" => TransitionMode::CoverFlow,
            "helix" => TransitionMode::Helix,
            "portal" => TransitionMode::Portal,
            _ => TransitionMode::Slide,
        };
        self.transition_mode = new_mode;
    }

    pub(super) fn set_debug_hud(&mut self, enabled: bool) {
        self.debug_hud = enabled;
        self.needs_render = true;
    }

    #[allow(dead_code)]
    pub(super) fn debug_hud_enabled(&self) -> bool {
        self.debug_hud
    }

    pub(super) fn frame_stats_fps(&self) -> f32 {
        self.frame_stats.fps
    }

    /// Rasterize HUD text and upload as a GL texture. Skips upload when the
    /// formatted string is identical to the previous frame.
    fn update_hud_text_texture(&mut self, text: &str) {
        if text == self.hud_text_cache && self.hud_text_texture.is_some() {
            return;
        }

        let scale = 2u32;
        let fg = [0, 230, 64, 255]; // green
        let (pixels, w, h) = font::render_text_to_rgba(text, scale, fg);
        if w == 0 || h == 0 {
            return;
        }

        unsafe {
            if let Some(old) = self.hud_text_texture.take() {
                self.gl.delete_texture(old);
            }
            if let Ok(tex) = self.gl.create_texture() {
                self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                self.gl.tex_image_2d(
                    glow::TEXTURE_2D, 0, glow::RGBA8 as i32,
                    w as i32, h as i32, 0,
                    glow::RGBA, glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(Some(&pixels)),
                );
                self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::NEAREST as i32);
                self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::NEAREST as i32);
                self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
                self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
                self.gl.bind_texture(glow::TEXTURE_2D, None);
                self.hud_text_texture = Some(tex);
                self.hud_text_width = w;
                self.hud_text_height = h;
            }
        }

        self.hud_text_cache = text.to_string();
    }

    // =====================================================================
    // Feature 12: Screenshot
    // =====================================================================
    pub(super) fn request_screenshot(&mut self, path: std::path::PathBuf) {
        self.pending_screenshot = Some(path);
        self.needs_render = true;
    }

    /// Check if there's a single fullscreen opaque window covering the screen.
    /// If so, and fullscreen_unredirect is enabled, we can skip compositing.
    fn check_fullscreen_unredirect(&mut self, scene: &[(u32, i32, i32, u32, u32)], focused: Option<u32>) -> bool {
        if !self.fullscreen_unredirect {
            return false;
        }
        // Only unredirect if the top (focused) window is fullscreen and opaque
        if let Some(focused_win) = focused {
            if let Some(wt) = self.windows.get(&focused_win) {
                if wt.is_fullscreen && !wt.has_rgba {
                    // Check if it covers the full screen
                    if let Some(&(_, x, y, w, h)) = scene.iter().rfind(|&&(win, _, _, _, _)| win == focused_win) {
                        if x <= 0 && y <= 0
                            && (x + w as i32) >= self.screen_w as i32
                            && (y + h as i32) >= self.screen_h as i32
                        {
                            // Unredirect: the X server draws directly
                            if self.unredirected_window != Some(focused_win) {
                                let _ = self.conn.composite_unredirect_window(
                                    focused_win,
                                    x11rb::protocol::composite::Redirect::MANUAL,
                                );
                                let _ = self.conn.flush();
                                self.unredirected_window = Some(focused_win);
                                log::info!("compositor: unredirected fullscreen window 0x{:x}", focused_win);
                            }
                            return true;
                        }
                    }
                }
            }
        }
        // Re-redirect if we had an unredirected window that's no longer fullscreen
        if let Some(prev) = self.unredirected_window.take() {
            let _ = self.conn.composite_redirect_window(
                prev,
                x11rb::protocol::composite::Redirect::MANUAL,
            );
            let _ = self.conn.flush();
            log::info!("compositor: re-redirected window 0x{:x}", prev);
            self.needs_render = true;
        }
        false
    }

    // ----- Rendering -----

    /// Compute a simple hash of the scene + focused window for skip-unchanged detection.
    fn scene_hash(scene: &[(u32, i32, i32, u32, u32)], focused: Option<u32>) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        scene.hash(&mut hasher);
        focused.hash(&mut hasher);
        hasher.finish()
    }

    /// Render a composited frame.
    ///
    /// `scene` is an ordered list of (x11_win, x, y, w, h) from bottom to top.
    /// `focused` is the X11 window ID of the focused window (if any).
    /// Returns true if a frame was rendered.
    pub(super) fn render_frame(
        &mut self,
        scene: &[(u32, i32, i32, u32, u32)],
        focused: Option<u32>,
    ) -> bool {
        // Feature 11: Frame timing start
        let _frame_start = std::time::Instant::now();

        // Periodic diagnostic logging
        static RENDER_LOG_COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let count = RENDER_LOG_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if count < 5 || count % 500 == 0 {
            log::info!(
                "[compositor::render_frame] frame={} scene={} tracked={}",
                count,
                scene.len(),
                self.windows.len()
            );
        }

        // Track render frequency for flicker diagnosis
        static RENDER_FREQ_COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        static RENDER_FREQ_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            let epoch = RENDER_FREQ_EPOCH.load(std::sync::atomic::Ordering::Relaxed);
            if epoch == 0 {
                RENDER_FREQ_EPOCH.store(now_ms, std::sync::atomic::Ordering::Relaxed);
            }
            let fc = RENDER_FREQ_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if now_ms - epoch >= 2000 {
                let elapsed = (now_ms - epoch) as f64 / 1000.0;
                log::info!(
                    "[compositor::render_freq] {:.1} renders/sec (needs_render={}, focused={:?})",
                    fc as f64 / elapsed, self.needs_render, focused,
                );
                RENDER_FREQ_COUNT.store(0, std::sync::atomic::Ordering::Relaxed);
                RENDER_FREQ_EPOCH.store(now_ms, std::sync::atomic::Ordering::Relaxed);
            }
        }

        // Fullscreen unredirect check
        if self.check_fullscreen_unredirect(scene, focused) {
            return false;
        }

        // Tick fade animations
        let fades_active = self.tick_fades();

        // Tick wobbly spring physics
        let wobbly_active = self.tick_wobbly();

        // Tick Phase 5 animations
        let expose_animating = self.tick_expose();
        let snap_animating = self.tick_snap_preview();
        let peek_animating = self.tick_peek();

        // Tick Phase 3 animations
        let genie_active = self.tick_genie();
        let ripples_active = self.tick_ripples();
        let focus_highlight_active = self.tick_focus_highlight();
        let wallpaper_crossfade_active = self.tick_wallpaper_crossfade();

        // Phase 3.4: Detect focus change
        if self.focus_highlight {
            if let Some(fw) = focused {
                if self.last_focused_window != Some(fw) {
                    self.focus_highlight_start = Some((fw, std::time::Instant::now()));
                }
            }
            self.last_focused_window = focused;
        }

        // Poll for async wallpaper decode results and upload to GPU if ready.
        let mut wallpaper_just_loaded = false;
        if let Some(rx) = &self.pending_wallpaper {
            if let Ok(data) = rx.try_recv() {
                if let Some((tex, w, h)) = Self::upload_wallpaper_texture(&self.gl, &data) {
                    self.wallpaper_texture = Some(tex);
                    self.wallpaper_img_w = w;
                    self.wallpaper_img_h = h;
                    self.wallpaper_mode = data.mode;
                    wallpaper_just_loaded = true;
                    log::info!("compositor: async wallpaper ready ({}x{})", w, h);
                }
                self.pending_wallpaper = None;
            }
        }
        // Poll per-monitor wallpaper results
        self.pending_monitor_wallpapers.retain_mut(|(idx, rx)| {
            if let Ok(data) = rx.try_recv() {
                if let Some(mw) = self.monitor_wallpapers.get_mut(*idx) {
                    if let Some((tex, w, h)) = Self::upload_wallpaper_texture(&self.gl, &data) {
                        mw.texture = Some(tex);
                        mw.img_w = w;
                        mw.img_h = h;
                        mw.mode = data.mode;
                        wallpaper_just_loaded = true;
                        log::info!("compositor: async monitor wallpaper [{}] ready ({}x{})", idx, w, h);
                    }
                }
                false // remove from pending list
            } else {
                true // keep waiting
            }
        });
        if wallpaper_just_loaded {
            self.needs_render = true;
        }

        // Skip-unchanged-frame: if scene hasn't changed and no textures are
        // dirty, we can skip the entire GL render (unless screenshot pending or HUD active).
        let has_dirty = scene.iter().any(|&(win, _, _, _, _)| {
            self.windows.get(&win).map_or(false, |wt| wt.dirty || wt.needs_pixmap_refresh)
        });
        let explicit_render = std::mem::replace(&mut self.needs_render, false);
        let force_render = self.pending_screenshot.is_some() || self.debug_hud || self.transition_active() || self.overview_active
            || self.expose_active || expose_animating || snap_animating || peek_animating
            || genie_active || ripples_active || focus_highlight_active || wallpaper_crossfade_active
            || self.recording_active || self.annotation_active || wallpaper_just_loaded
            || wobbly_active || explicit_render;
        let hash = Self::scene_hash(scene, focused);
        let scene_changed = hash != self.last_scene_hash;
        if !has_dirty && !fades_active && !force_render && !scene_changed {
            return false;
        }
        self.last_scene_hash = hash;

        // Reset tilt targets — the render loop will set them if a focused window
        // uses tilt; otherwise they stay at 0 so the tilt smoothly returns to rest.
        if self.window_tilt {
            self.tilt_target_x = 0.0;
            self.tilt_target_y = 0.0;
        }

        // Invalidate blur cache when scene structure/focus changes or animations
        // are active — these affect the rendered output of windows below the
        // frosted window even though no individual texture is "dirty".
        if scene_changed || fades_active || force_render {
            self.blur_cache_hash = 0;
        }

        // Ensure context is current
        if !self.context_current {
            unsafe {
                x11::glx::glXMakeContextCurrent(
                    self.xlib_display,
                    self.glx_drawable,
                    self.glx_drawable,
                    self.glx_context,
                );
            }
            self.context_current = true;
        }

        // Recreate pixmaps for windows that were resized (batched, single XSync)
        self.refresh_pixmaps();

        // Collect which windows are dirty this frame (before TFP refresh clears
        // the flags).  Used by the blur cache to skip expensive blur passes when
        // only the frosted window itself updated (e.g. fcitx candidate list).
        let blur_dirty_wins: Vec<u32> = scene.iter()
            .filter_map(|&(win, _, _, _, _)| {
                self.windows.get(&win).and_then(|wt| if wt.dirty { Some(win) } else { None })
            })
            .collect();

        // Refresh TFP textures for dirty windows.
        // NOTE: We intentionally do NOT call glGetError() here.  The old code
        // checked for GL errors after every TFP rebind and, on error, set
        // needs_pixmap_refresh which triggers a costly pixmap recreation +
        // XSync on the *next* frame.  For rapidly-updating windows (e.g.
        // flameshot selection overlay) a transient TFP race could cause this
        // error every frame, creating a cascade of XSync stalls that made
        // the compositor lag seconds behind the actual window content.
        // Removing the per-frame glGetError avoids the GPU pipeline sync and
        // the refresh cascade.  Genuine pixmap invalidation (window resize)
        // is handled by update_geometry → needs_pixmap_refresh instead.
        for &(win, _, _, _, _) in scene {
            if let Some(wt) = self.windows.get_mut(&win) {
                if wt.dirty && wt.glx_pixmap != 0 {
                    // Phase 2.3: Check fence before rebind — skip if GPU not done yet
                    if let Some(fence) = wt.pending_fence.take() {
                        let status = unsafe {
                            self.gl.client_wait_sync(fence, 0, 0)
                        };
                        if status == glow::TIMEOUT_EXPIRED {
                            // GPU not done yet, skip this window's update; use old texture
                            wt.pending_fence = Some(fence);
                            continue;
                        }
                        unsafe { self.gl.delete_sync(fence); }
                    }

                    unsafe {
                        self.gl.bind_texture(glow::TEXTURE_2D, Some(wt.gl_texture));
                        (self.tfp.release)(
                            self.xlib_display,
                            wt.glx_pixmap,
                            GLX_FRONT_LEFT_EXT,
                        );
                        (self.tfp.bind)(
                            self.xlib_display,
                            wt.glx_pixmap,
                            GLX_FRONT_LEFT_EXT,
                            std::ptr::null(),
                        );
                        self.gl.bind_texture(glow::TEXTURE_2D, None);

                        // Insert fence after rebind
                        wt.pending_fence = self.gl.fence_sync(glow::SYNC_GPU_COMMANDS_COMPLETE, 0).ok();
                    }
                    wt.dirty = false;
                }
            }
        }

        // --- Occlusion culling ---
        let mut first_visible = 0usize;
        {
            let sw = self.screen_w as i32;
            let sh = self.screen_h as i32;
            for i in (0..scene.len()).rev() {
                let (win, x, y, w, h) = scene[i];
                let is_rgba = self.windows.get(&win).map_or(false, |wt| wt.has_rgba);
                let has_fade = self.windows.get(&win).map_or(false, |wt| wt.fade_opacity < 1.0);
                if !is_rgba && !has_fade && x <= 0 && y <= 0
                    && (x + w as i32) >= sw && (y + h as i32) >= sh
                {
                    first_visible = i;
                    break;
                }
            }
        }

        // Feature 8/9/10: If postprocessing is active, render into postprocess FBO
        let postprocess_active = self.needs_postprocess() && self.postprocess_fbo.is_some();
        if postprocess_active {
            let (pp_fbo, _) = self.postprocess_fbo.as_ref().unwrap();
            unsafe {
                self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(*pp_fbo));
            }
        }

        // Feature 6 / Phase 2.1: Apply scissor test using tile-based damage tracker
        let damage_bounds = self.damage_tracker.dirty_bounds();
        let use_scissor = damage_bounds.is_some() && !force_render;
        let mut damage_scissor = (0i32, 0i32, self.screen_w as i32, self.screen_h as i32);
        if let (true, Some((dx, dy, dw, dh))) = (use_scissor, damage_bounds) {
            unsafe {
                self.gl.enable(glow::SCISSOR_TEST);
                // GL scissor uses bottom-left origin
                let gl_y = self.screen_h as i32 - dy - dh as i32;
                damage_scissor = (dx, gl_y, dw as i32, dh as i32);
                self.gl.scissor(damage_scissor.0, damage_scissor.1, damage_scissor.2, damage_scissor.3);
            }
        }
        self.damage_tracker.clear();

        // Clear
        unsafe {
            self.gl
                .viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
            self.gl.clear(glow::COLOR_BUFFER_BIT);
        }

        // Build orthographic projection matrix (column-major)
        let proj = ortho(
            0.0,
            self.screen_w as f32,
            self.screen_h as f32,
            0.0,
            -1.0,
            1.0,
        );

        // Draw wallpaper background (per-monitor or global fallback)
        // Skip if a fully-opaque window already covers the entire screen (occluded).
        {
            let wallpaper_occluded = first_visible > 0;
            let has_wallpaper = !wallpaper_occluded
                && (!self.monitor_wallpapers.is_empty()
                    || self.wallpaper_texture.is_some());
            if has_wallpaper {
                unsafe {
                    self.gl.use_program(Some(self.program));
                    self.gl.uniform_matrix_4_f32_slice(
                        self.win_uniforms.projection.as_ref(), false, &proj,
                    );
                    self.gl.uniform_1_i32(self.win_uniforms.texture.as_ref(), 0);
                    self.gl.bind_vertex_array(Some(self.quad_vao));
                    self.gl.uniform_1_f32(self.win_uniforms.opacity.as_ref(), 1.0);
                    self.gl.uniform_1_f32(self.win_uniforms.radius.as_ref(), 0.0);
                    self.gl.uniform_1_f32(self.win_uniforms.dim.as_ref(), 1.0);
                    self.gl.uniform_4_f32(self.win_uniforms.uv_rect.as_ref(), 0.0, 0.0, 1.0, 1.0);
                    self.gl.active_texture(glow::TEXTURE0);

                    if !self.monitor_wallpapers.is_empty() {
                        // Temporarily disable damage-region scissor for wallpaper
                        if use_scissor {
                            self.gl.disable(glow::SCISSOR_TEST);
                        }

                        // Per-monitor wallpaper rendering with per-monitor scissor
                        for mw in &self.monitor_wallpapers {
                            // Resolve texture: per-monitor override or global default
                            let (tex, mode, iw, ih) = if let Some(t) = mw.texture {
                                (t, mw.mode, mw.img_w, mw.img_h)
                            } else if let Some(t) = self.wallpaper_texture {
                                (t, self.wallpaper_mode, self.wallpaper_img_w, self.wallpaper_img_h)
                            } else {
                                continue;
                            };

                            // Scissor to this monitor's area
                            let gl_y = self.screen_h as i32 - (mw.mon_y + mw.mon_h as i32);
                            self.gl.enable(glow::SCISSOR_TEST);
                            self.gl.scissor(
                                mw.mon_x,
                                gl_y,
                                mw.mon_w as i32,
                                mw.mon_h as i32,
                            );

                            let area = (
                                mw.mon_x as f32,
                                mw.mon_y as f32,
                                mw.mon_w as f32,
                                mw.mon_h as f32,
                            );
                            let (rx, ry, rw, rh) =
                                Self::compute_wallpaper_rect(mode, area, iw, ih);
                            self.gl.uniform_4_f32(
                                self.win_uniforms.rect.as_ref(), rx, ry, rw, rh,
                            );
                            self.gl.uniform_2_f32(
                                self.win_uniforms.size.as_ref(), rw, rh,
                            );
                            self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                        }
                        self.gl.disable(glow::SCISSOR_TEST);
                    } else if let Some(wp_tex) = self.wallpaper_texture {
                        // Single global wallpaper (no monitors set yet)
                        let area = (0.0, 0.0, self.screen_w as f32, self.screen_h as f32);
                        let (rx, ry, rw, rh) = Self::compute_wallpaper_rect(
                            self.wallpaper_mode,
                            area,
                            self.wallpaper_img_w,
                            self.wallpaper_img_h,
                        );
                        self.gl.uniform_4_f32(
                            self.win_uniforms.rect.as_ref(), rx, ry, rw, rh,
                        );
                        self.gl.uniform_2_f32(
                            self.win_uniforms.size.as_ref(), rw, rh,
                        );
                        self.gl.bind_texture(glow::TEXTURE_2D, Some(wp_tex));
                        self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                    }

                    // Phase 3.5: Draw old wallpaper for crossfade
                    if let (Some(old_tex), Some(start)) = (self.old_wallpaper_texture, self.wallpaper_transition_start) {
                        let elapsed = start.elapsed().as_millis() as f32;
                        let duration = self.wallpaper_crossfade_duration_ms as f32;
                        let old_opacity = (1.0 - elapsed / duration).max(0.0);
                        if old_opacity > 0.0 {
                            let area = (0.0, 0.0, self.screen_w as f32, self.screen_h as f32);
                            let (rx, ry, rw, rh) = Self::compute_wallpaper_rect(
                                self.wallpaper_mode, area,
                                self.wallpaper_img_w, self.wallpaper_img_h,
                            );
                            self.gl.uniform_1_f32(self.win_uniforms.opacity.as_ref(), old_opacity);
                            self.gl.uniform_4_f32(self.win_uniforms.rect.as_ref(), rx, ry, rw, rh);
                            self.gl.uniform_2_f32(self.win_uniforms.size.as_ref(), rw, rh);
                            self.gl.bind_texture(glow::TEXTURE_2D, Some(old_tex));
                            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                            // Restore opacity for subsequent draws
                            self.gl.uniform_1_f32(self.win_uniforms.opacity.as_ref(), 1.0);
                        }
                    }

                    self.gl.bind_texture(glow::TEXTURE_2D, None);
                    self.gl.bind_vertex_array(None);
                    self.gl.use_program(None);

                    // Restore damage-region scissor if it was active
                    if use_scissor {
                        self.gl.scissor(damage_scissor.0, damage_scissor.1, damage_scissor.2, damage_scissor.3);
                        self.gl.enable(glow::SCISSOR_TEST);
                    }
                }
            }
        }

        let visible_scene = &scene[first_visible..];

        // When overview is active, skip rendering windows that belong to the
        // overview monitor — they would be hidden behind the opaque overview
        // background anyway and their presence can visually compete with the
        // 3D prism thumbnails.
        let overview_skip = |x: i32, y: i32, w: u32, h: u32| -> bool {
            if !self.overview_active { return false; }
            let cx = x + w as i32 / 2;
            let cy = y + h as i32 / 2;
            let mx = self.overview_mon_x;
            let my = self.overview_mon_y;
            cx >= mx && cx < mx + self.overview_mon_w as i32
                && cy >= my && cy < my + self.overview_mon_h as i32
        };

        // === Pass 1: Draw shadows (feature 14: improved shape) ===
        if self.shadow_enabled && self.shadow_radius > 0.0 {
            unsafe {
                self.gl.use_program(Some(self.shadow_program));
                self.gl.uniform_matrix_4_f32_slice(
                    self.shadow_uniforms.projection.as_ref(), false, &proj,
                );
                self.gl.bind_vertex_array(Some(self.quad_vao));

                let spread = self.shadow_radius;
                let [ox, oy] = self.shadow_offset;
                let [sr, sg, sb, sa] = self.shadow_color;
                let bottom_extra = self.shadow_bottom_extra;

                self.gl.uniform_1_f32(
                    self.shadow_uniforms.spread.as_ref(), spread,
                );

                for &(win, x, y, w, h) in visible_scene {
                    if overview_skip(x, y, w, h) { continue; }
                    let wt = match self.windows.get(&win) {
                        Some(wt) => wt,
                        None => continue,
                    };
                    // Per-window shadow exclude
                    if Self::class_matches_exclude(&wt.class_name, &self.shadow_exclude) {
                        continue;
                    }
                    // Feature 14: Skip shadow for shaped windows (non-rectangular)
                    if wt.is_shaped {
                        continue;
                    }
                    // Fade: modulate shadow alpha
                    let fade = wt.fade_opacity;
                    let sa_faded = sa * fade;
                    if sa_faded <= 0.0 { continue; }

                    self.gl.uniform_4_f32(
                        self.shadow_uniforms.shadow_color.as_ref(), sr, sg, sb, sa_faded,
                    );

                    // Feature 3: Per-window corner radius for shadow
                    let win_radius = wt.corner_radius_override.unwrap_or(
                        if Self::class_matches_exclude(&wt.class_name, &self.rounded_corners_exclude) {
                            0.0
                        } else {
                            self.corner_radius
                        }
                    );
                    self.gl.uniform_1_f32(
                        self.shadow_uniforms.radius.as_ref(), win_radius,
                    );

                    // Feature 14: Non-uniform shadow offset (heavier bottom)
                    let sy_offset = oy + bottom_extra;
                    let anim_s = wt.anim_scale;
                    let win_w = w as f32 * anim_s;
                    let win_h = h as f32 * anim_s;
                    let cx = x as f32 + (w as f32 - win_w) * 0.5;
                    let cy = y as f32 + (h as f32 - win_h) * 0.5;
                    let mut sx = cx + ox - spread;
                    let mut sy = cy + sy_offset - spread;
                    let mut sw = win_w + 2.0 * spread;
                    let mut sh = win_h + 2.0 * spread + bottom_extra;

                    // Dynamic shadow offset for tilted focused window
                    if self.window_tilt && focused == Some(win) {
                        let tilt_mag = (self.tilt_current_x.powi(2) + self.tilt_current_y.powi(2)).sqrt();
                        let extra = tilt_mag * 15.0;
                        sx += self.tilt_current_y * 30.0 - extra;
                        sy += self.tilt_current_x * 30.0 - extra;
                        sw += extra * 2.0;
                        sh += extra * 2.0;
                    }
                    self.gl.uniform_4_f32(
                        self.shadow_uniforms.rect.as_ref(), sx, sy, sw, sh,
                    );
                    self.gl.uniform_2_f32(
                        self.shadow_uniforms.size.as_ref(), win_w, win_h,
                    );
                    self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                }

                self.gl.bind_vertex_array(None);
                self.gl.use_program(None);
            }
        }

        // Phase 2.2: Auto blur quality downgrade during animations/transitions
        if self.blur_quality_auto {
            self.blur_quality = if self.transition_active() || self.overview_active {
                BlurQuality::Minimal
            } else if fades_active || wobbly_active {
                BlurQuality::Reduced
            } else {
                BlurQuality::Full
            };
        }

        // === Pass 1.5: Background blur (now computed per-window in Pass 2) ===
        let blur_available = self.blur_enabled
            && !self.blur_fbos.is_empty()
            && self.scene_fbo.is_some();

        // === Pass 2: Draw window textures ===
        // Track the below-scene for blur caching: a running hash of (win, x, y, w, h)
        // for all windows drawn so far, plus whether any was dirty this frame.
        let mut blur_below_hash: u64 = 0u64;
        let mut blur_below_dirty = false;

        unsafe {
            self.gl.use_program(Some(self.program));
            self.gl.uniform_matrix_4_f32_slice(
                self.win_uniforms.projection.as_ref(), false, &proj,
            );
            self.gl.uniform_1_i32(self.win_uniforms.texture.as_ref(), 0);
            self.gl.uniform_4_f32(self.win_uniforms.uv_rect.as_ref(), 0.0, 0.0, 1.0, 1.0);
            self.gl.uniform_1_f32(self.win_uniforms.ripple_amplitude.as_ref(), 0.0);
            self.gl.bind_vertex_array(Some(self.quad_vao));

            for &(win, x, y, w, h) in visible_scene {
                if overview_skip(x, y, w, h) { continue; }
                if let Some(wt) = self.windows.get(&win) {
                    let is_focused = focused == Some(win);
                    let fade = wt.fade_opacity;
                    if fade <= 0.0 { continue; }

                    // Phase 5.3: Peek opacity multiplier
                    let peek_mul = self.peek_opacity_for(&wt.class_name);

                    // Feature 3: Per-window corner radius
                    let radius = wt.corner_radius_override.unwrap_or(
                        if Self::class_matches_exclude(&wt.class_name, &self.rounded_corners_exclude) {
                            0.0
                        } else {
                            self.corner_radius
                        }
                    );
                    self.gl.uniform_1_f32(self.win_uniforms.radius.as_ref(), radius);

                    // Compute effective opacity
                    let base_opacity = if is_focused { self.active_opacity } else { self.inactive_opacity };
                    let rule_opacity = wt.opacity_override.unwrap_or(base_opacity);
                    let has_explicit_transparency = wt.opacity_override.map_or(false, |o| o < 1.0);
                    let inactive_dim_factor = if is_focused { 1.0 } else { self.inactive_dim };
                    let dim = if wt.has_rgba {
                        rule_opacity * fade * inactive_dim_factor
                    } else {
                        inactive_dim_factor
                    };

                    // detect_client_opacity: if window manages its own alpha, don't force opacity.
                    // For RGB windows, keep fully opaque by default, but allow explicit
                    // per-window opacity overrides (and fade animations) to output real
                    // alpha so translucent windows can reveal realtime blurred backdrop.
                    let opacity = if wt.has_rgba {
                        if self.detect_client_opacity {
                            -dim
                        } else {
                            -1.0f32 * fade
                        }
                    } else {
                        if has_explicit_transparency || fade < 1.0 {
                            (rule_opacity * fade).clamp(0.0, 1.0)
                        } else {
                            1.0f32
                        }
                    };

                    // Phase 5.3: Apply peek opacity
                    let opacity = if peek_mul < 1.0 {
                        if opacity < 0.0 { opacity * peek_mul } else { (opacity * peek_mul).clamp(0.0, 1.0) }
                    } else {
                        opacity
                    };
                    // Feature 4: Apply per-window scale + Phase 3.4 focus bounce
                    let focus_bounce = if self.focus_highlight && focused == Some(win) {
                        if let Some((hw, start)) = self.focus_highlight_start {
                            if hw == win && start.elapsed().as_millis() < self.focus_highlight_duration_ms as u128 {
                                let t = start.elapsed().as_millis() as f32 / self.focus_highlight_duration_ms as f32;
                                1.0 + 0.02 * (1.0 - t) * ((t * std::f32::consts::PI).sin())
                            } else { 1.0 }
                        } else { 1.0 }
                    } else { 1.0 };
                    let scale = wt.scale * wt.anim_scale * focus_bounce;
                    let (draw_x, draw_y, draw_w, draw_h) = if (scale - 1.0).abs() > f32::EPSILON {
                        let cw = w as f32 * scale;
                        let ch = h as f32 * scale;
                        let cx = x as f32 + (w as f32 - cw) * 0.5;
                        let cy = y as f32 + (h as f32 - ch) * 0.5;
                        (cx, cy, cw, ch)
                    } else {
                        (x as f32, y as f32, w as f32, h as f32)
                    };

                    // Feature 13: Draw blurred background behind translucent windows (with frame extents mask)
                    // Blur is captured per-window so it includes all windows drawn below.
                    if blur_available {
                        if self.needs_backdrop_blur(wt) {
                            // Blur cache: if no window below this one was dirty and
                            // the below-scene structure hasn't changed, the previous
                            // blur result stored in blur_fbos[0] is still valid.
                            let cache_hit = !blur_below_dirty
                                && blur_below_hash != 0
                                && blur_below_hash == self.blur_cache_hash;

                            let blur_tex = if cache_hit {
                                Some(self.blur_fbos[0].texture)
                            } else {
                                // Temporarily break out of the window shader to run blur passes.
                                // Capture the current framebuffer (which includes all windows
                                // drawn so far) and produce a blurred texture from it.
                                self.gl.bind_vertex_array(None);
                                self.gl.use_program(None);
                                if use_scissor {
                                    self.gl.disable(glow::SCISSOR_TEST);
                                }

                                let base_levels = if wt.is_frosted {
                                    self.frosted_glass_strength as usize
                                } else {
                                    self.blur_fbos.len()
                                };
                                // Phase 2.2: Apply blur quality cap
                                let blur_levels = match self.blur_quality {
                                    BlurQuality::Full => base_levels,
                                    BlurQuality::Reduced => (base_levels / 2).max(1),
                                    BlurQuality::Minimal => 1,
                                };
                                let tex = self.run_blur_passes_from_fbo(
                                    if postprocess_active {
                                        self.postprocess_fbo.as_ref().map(|(fbo, _)| *fbo)
                                    } else {
                                        None
                                    },
                                    blur_levels,
                                );

                                // Restore state for window drawing
                                if use_scissor {
                                    self.gl.enable(glow::SCISSOR_TEST);
                                }
                                if postprocess_active {
                                    let (pp_fbo, _) = self.postprocess_fbo.as_ref().unwrap();
                                    self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(*pp_fbo));
                                } else {
                                    self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                                }
                                self.gl.viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
                                self.gl.use_program(Some(self.program));
                                self.gl.uniform_matrix_4_f32_slice(
                                    self.win_uniforms.projection.as_ref(), false, &proj,
                                );
                                self.gl.uniform_1_i32(self.win_uniforms.texture.as_ref(), 0);
                                self.gl.uniform_4_f32(self.win_uniforms.uv_rect.as_ref(), 0.0, 0.0, 1.0, 1.0);
                                self.gl.bind_vertex_array(Some(self.quad_vao));
                                self.gl.uniform_1_f32(self.win_uniforms.radius.as_ref(), radius);

                                self.blur_cache_hash = blur_below_hash;
                                tex
                            };

                            if let Some(blur_tex) = blur_tex {
                                // Feature 13: If blur_use_frame_extents, crop blur to client area
                                let (bx, by, bw, bh) = if self.blur_use_frame_extents {
                                    let [fl, fr, ft, fb] = wt.frame_extents;
                                    let bx = draw_x + fl as f32;
                                    let by = draw_y + ft as f32;
                                    let bw = (draw_w - fl as f32 - fr as f32).max(1.0);
                                    let bh = (draw_h - ft as f32 - fb as f32).max(1.0);
                                    (bx, by, bw, bh)
                                } else {
                                    (draw_x, draw_y, draw_w, draw_h)
                                };
                                self.gl.active_texture(glow::TEXTURE0);
                                self.gl.bind_texture(glow::TEXTURE_2D, Some(blur_tex));
                                let uv_x = (bx / self.screen_w as f32).clamp(0.0, 1.0);
                                let uv_w = (bw / self.screen_w as f32).clamp(0.0, 1.0);
                                let uv_y_top = (by / self.screen_h as f32).clamp(0.0, 1.0);
                                let uv_h = (bh / self.screen_h as f32).clamp(0.0, 1.0);
                                self.gl.uniform_4_f32(
                                    self.win_uniforms.uv_rect.as_ref(),
                                    uv_x,
                                    uv_y_top,
                                    uv_w,
                                    uv_h,
                                );
                                self.gl.uniform_1_f32(self.win_uniforms.opacity.as_ref(), fade);
                                self.gl.uniform_1_f32(self.win_uniforms.dim.as_ref(), 1.0);
                                self.gl.uniform_2_f32(self.win_uniforms.size.as_ref(), bw, bh);
                                self.gl.uniform_4_f32(
                                    self.win_uniforms.rect.as_ref(), bx, by, bw, bh,
                                );
                                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                                // Restore default UV for regular window textures.
                                self.gl.uniform_4_f32(
                                    self.win_uniforms.uv_rect.as_ref(),
                                    0.0, 0.0, 1.0, 1.0,
                                );
                            }
                        }
                    }

                    // Phase 3.1: Motion trail ghost copies at historical positions
                    if self.motion_trail_enabled && !wt.motion_trail.is_empty() {
                        let trail_len = wt.motion_trail.len();
                        for (i, &(tx, ty)) in wt.motion_trail.iter().enumerate() {
                            let trail_opacity = self.motion_trail_opacity * (i as f32 + 1.0) / trail_len as f32;
                            self.gl.uniform_1_f32(self.win_uniforms.opacity.as_ref(), trail_opacity);
                            self.gl.uniform_1_f32(self.win_uniforms.dim.as_ref(), 0.7);
                            self.gl.uniform_4_f32(self.win_uniforms.rect.as_ref(), tx as f32, ty as f32, draw_w, draw_h);
                            self.gl.uniform_2_f32(self.win_uniforms.size.as_ref(), draw_w, draw_h);
                            self.gl.active_texture(glow::TEXTURE0);
                            self.gl.bind_texture(glow::TEXTURE_2D, Some(wt.gl_texture));
                            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                        }
                    }

                    self.gl.active_texture(glow::TEXTURE0);
                    self.gl.bind_texture(glow::TEXTURE_2D, Some(wt.gl_texture));

                    // Wobbly windows: use grid spring-mass deformation shader
                    if self.wobbly_windows && wt.wobbly.is_some() {
                        let wobbly = wt.wobbly.as_ref().unwrap();
                        self.gl.use_program(Some(self.wobbly_program));
                        self.gl.uniform_matrix_4_f32_slice(
                            self.wobbly_uniforms.projection.as_ref(), false, &proj,
                        );
                        self.gl.uniform_4_f32(
                            self.wobbly_uniforms.rect.as_ref(), draw_x, draw_y, draw_w, draw_h,
                        );
                        self.gl.uniform_1_i32(self.wobbly_uniforms.texture.as_ref(), 0);
                        self.gl.uniform_1_f32(self.wobbly_uniforms.opacity.as_ref(), opacity);
                        self.gl.uniform_1_f32(self.wobbly_uniforms.radius.as_ref(), radius);
                        self.gl.uniform_2_f32(self.wobbly_uniforms.size.as_ref(), draw_w, draw_h);
                        self.gl.uniform_1_f32(self.wobbly_uniforms.dim.as_ref(), dim);
                        self.gl.uniform_4_f32(
                            self.wobbly_uniforms.uv_rect.as_ref(), 0.0, 0.0, 1.0, 1.0,
                        );
                        // Upload grid offsets as flat vec2 array
                        let flat: Vec<f32> = wobbly.offsets.iter()
                            .flat_map(|o| [o[0], o[1]])
                            .collect();
                        self.gl.uniform_2_f32_slice(
                            self.wobbly_uniforms.grid_offsets.as_ref(), &flat,
                        );
                        let grid_n = wobbly.grid_n as i32;
                        self.gl.uniform_1_i32(self.wobbly_uniforms.grid_n.as_ref(), grid_n);
                        // Grid: (grid_n-1)^2 quads, 6 verts each
                        let quads = grid_n - 1;
                        self.gl.draw_arrays(glow::TRIANGLES, 0, quads * quads * 6);

                        // Restore standard window program
                        self.gl.use_program(Some(self.program));
                        self.gl.uniform_matrix_4_f32_slice(
                            self.win_uniforms.projection.as_ref(), false, &proj,
                        );
                        self.gl.uniform_1_i32(self.win_uniforms.texture.as_ref(), 0);
                        self.gl.uniform_4_f32(
                            self.win_uniforms.uv_rect.as_ref(), 0.0, 0.0, 1.0, 1.0,
                        );
                        self.gl.uniform_1_f32(self.win_uniforms.radius.as_ref(), radius);
                    } else if self.window_tilt && is_focused {
                        // Update tilt target from mouse position (clamped)
                        let cx = draw_x + draw_w * 0.5;
                        let cy = draw_y + draw_h * 0.5;
                        let rel_x = ((self.mouse_x - cx) / (draw_w * 0.5)).clamp(-1.0, 1.0);
                        let rel_y = ((self.mouse_y - cy) / (draw_h * 0.5)).clamp(-1.0, 1.0);
                        self.tilt_target_x = (-rel_y * self.tilt_amount).clamp(-0.35, 0.35);
                        self.tilt_target_y = (rel_x * self.tilt_amount).clamp(-0.35, 0.35);

                        self.gl.use_program(Some(self.tilt_program));
                        self.gl.uniform_matrix_4_f32_slice(
                            self.tilt_uniforms.projection.as_ref(), false, &proj,
                        );
                        self.gl.uniform_4_f32(
                            self.tilt_uniforms.rect.as_ref(), draw_x, draw_y, draw_w, draw_h,
                        );
                        self.gl.uniform_1_i32(self.tilt_uniforms.texture.as_ref(), 0);
                        self.gl.uniform_1_f32(self.tilt_uniforms.opacity.as_ref(), opacity);
                        self.gl.uniform_1_f32(self.tilt_uniforms.radius.as_ref(), radius);
                        self.gl.uniform_2_f32(self.tilt_uniforms.size.as_ref(), draw_w, draw_h);
                        self.gl.uniform_1_f32(self.tilt_uniforms.dim.as_ref(), dim);
                        self.gl.uniform_4_f32(
                            self.tilt_uniforms.uv_rect.as_ref(), 0.0, 0.0, 1.0, 1.0,
                        );
                        self.gl.uniform_2_f32(self.tilt_uniforms.tilt.as_ref(), self.tilt_current_x, self.tilt_current_y);
                        self.gl.uniform_1_f32(self.tilt_uniforms.perspective.as_ref(), self.tilt_perspective);
                        let grid = self.tilt_grid as i32;
                        self.gl.uniform_1_i32(self.tilt_uniforms.grid_size.as_ref(), grid);
                        self.gl.uniform_2_f32(self.tilt_uniforms.light_dir.as_ref(), 0.0, -1.0);
                        // Grid: grid^2 quads, 6 verts each
                        self.gl.draw_arrays(glow::TRIANGLES, 0, grid * grid * 6);

                        // Restore standard window program
                        self.gl.use_program(Some(self.program));
                        self.gl.uniform_matrix_4_f32_slice(
                            self.win_uniforms.projection.as_ref(), false, &proj,
                        );
                        self.gl.uniform_1_i32(self.win_uniforms.texture.as_ref(), 0);
                        self.gl.uniform_4_f32(
                            self.win_uniforms.uv_rect.as_ref(), 0.0, 0.0, 1.0, 1.0,
                        );
                        self.gl.uniform_1_f32(self.win_uniforms.radius.as_ref(), radius);
                    } else {
                        self.gl.uniform_1_f32(self.win_uniforms.opacity.as_ref(), opacity);
                        self.gl.uniform_1_f32(self.win_uniforms.dim.as_ref(), dim);
                        self.gl.uniform_2_f32(
                            self.win_uniforms.size.as_ref(), draw_w, draw_h,
                        );
                        self.gl.uniform_4_f32(
                            self.win_uniforms.rect.as_ref(), draw_x, draw_y, draw_w, draw_h,
                        );

                        // Window-open ripple: set per-window distortion uniforms
                        let ripple_prog = self.ripple_active.iter()
                            .find(|r| r.x11_win == win)
                            .map(|r| {
                                let elapsed = r.start.elapsed().as_secs_f32();
                                (elapsed / self.ripple_duration).min(1.0)
                            });
                        if let Some(progress) = ripple_prog {
                            self.gl.uniform_1_f32(self.win_uniforms.ripple_progress.as_ref(), progress);
                            self.gl.uniform_1_f32(self.win_uniforms.ripple_amplitude.as_ref(), self.ripple_amplitude);
                        } else {
                            self.gl.uniform_1_f32(self.win_uniforms.ripple_amplitude.as_ref(), 0.0);
                        }

                        self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

                        // Reset ripple for next window
                        if ripple_prog.is_some() {
                            self.gl.uniform_1_f32(self.win_uniforms.ripple_amplitude.as_ref(), 0.0);
                        }
                    }

                    // Update blur below-scene tracking after drawing this window.
                    // The hash encodes (win, x, y, w, h) so structural changes
                    // (reorder, move, resize, add/remove) cause a cache miss.
                    blur_below_hash = blur_below_hash
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(win as u64)
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(((x as u64) << 32) | (y as u32 as u64))
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(((w as u64) << 32) | (h as u64));
                    if blur_dirty_wins.contains(&win) {
                        blur_below_dirty = true;
                    }
                }
            }

            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
        }

        // === Pass 2b: Genie minimize animations ===
        if !self.genie_active.is_empty() {
            let genie_duration_ms = self.genie_duration_ms;
            unsafe {
                self.gl.use_program(Some(self.genie_program));
                self.gl.uniform_matrix_4_f32_slice(
                    self.genie_uniforms.projection.as_ref(), false, &proj,
                );
                self.gl.uniform_1_i32(self.genie_uniforms.texture.as_ref(), 0);
                self.gl.uniform_4_f32(self.genie_uniforms.uv_rect.as_ref(), 0.0, 0.0, 1.0, 1.0);
                self.gl.uniform_1_f32(self.genie_uniforms.radius.as_ref(), 0.0);
                let grid = 12i32;
                self.gl.uniform_1_i32(self.genie_uniforms.grid_size.as_ref(), grid);
                self.gl.bind_vertex_array(Some(self.quad_vao));

                let dock = self.dock_position;
                for ga in &self.genie_active {
                    let elapsed = ga.start.elapsed().as_millis() as f32;
                    let progress = (elapsed / genie_duration_ms as f32).min(1.0);
                    let opacity = 1.0 - progress;
                    self.gl.uniform_4_f32(
                        self.genie_uniforms.rect.as_ref(), ga.x, ga.y, ga.w, ga.h,
                    );
                    self.gl.uniform_2_f32(
                        self.genie_uniforms.size.as_ref(), ga.w, ga.h,
                    );
                    self.gl.uniform_1_f32(self.genie_uniforms.progress.as_ref(), progress);
                    self.gl.uniform_2_f32(self.genie_uniforms.dock_pos.as_ref(), dock.0, dock.1);
                    self.gl.uniform_1_f32(self.genie_uniforms.opacity.as_ref(), if ga.has_rgba { -opacity } else { opacity });
                    self.gl.uniform_1_f32(self.genie_uniforms.dim.as_ref(), 1.0);
                    self.gl.active_texture(glow::TEXTURE0);
                    self.gl.bind_texture(glow::TEXTURE_2D, Some(ga.gl_texture));
                    self.gl.draw_arrays(glow::TRIANGLES, 0, grid * grid * 6);
                }

                self.gl.bind_vertex_array(None);
                self.gl.use_program(None);
            }
        }

        // === Pass 3: Window borders (feature 1) ===
        // When compositor is active, it owns border rendering.  Read the WM's
        // border_px so the visual border always matches the layout gap.
        let wm_border_px = crate::config::CONFIG.load().border_px() as f32;
        let effective_border_enabled = self.border_enabled || wm_border_px > 0.0;
        let base_border_width = if self.border_enabled { self.border_width } else { wm_border_px };
        if effective_border_enabled && base_border_width > 0.0 {
            unsafe {
                self.gl.use_program(Some(self.border_program));
                self.gl.uniform_matrix_4_f32_slice(
                    self.border_uniforms.projection.as_ref(), false, &proj,
                );
                self.gl.bind_vertex_array(Some(self.quad_vao));

                for &(win, x, y, w, h) in visible_scene {
                    if overview_skip(x, y, w, h) { continue; }
                    let wt = match self.windows.get(&win) {
                        Some(wt) => wt,
                        None => continue,
                    };
                    let fade = wt.fade_opacity;
                    if fade <= 0.0 { continue; }

                    let is_focused = focused == Some(win);

                    // Phase 3.4: Focus highlight animation
                    let focus_highlight_active_for_win = if let Some((hw, start)) = self.focus_highlight_start {
                        hw == win && start.elapsed().as_millis() < self.focus_highlight_duration_ms as u128
                    } else { false };

                    let color = if focus_highlight_active_for_win {
                        let elapsed_ms = self.focus_highlight_start.unwrap().1.elapsed().as_millis() as f32;
                        let dur = self.focus_highlight_duration_ms as f32;
                        let pulse = ((elapsed_ms / dur * std::f32::consts::PI).sin()).abs();
                        let [r, g, b, a] = self.focus_highlight_color;
                        [r, g, b, a * pulse]
                    } else if wt.is_urgent && self.attention_animation {
                        let elapsed = self.compositor_start_time.elapsed().as_secs_f32();
                        let pulse = (elapsed * 4.0).sin() * 0.5 + 0.5;
                        let [r, g, b, a] = self.attention_color;
                        [r, g, b, a * pulse]
                    } else if wt.is_pip {
                        self.pip_border_color
                    } else if is_focused {
                        self.border_color_focused
                    } else {
                        self.border_color_unfocused
                    };

                    let bw = if focus_highlight_active_for_win {
                        (base_border_width + 2.0).max(3.0)
                    } else if wt.is_urgent && self.attention_animation {
                        base_border_width.max(2.0)
                    } else if wt.is_pip {
                        self.pip_border_width
                    } else {
                        base_border_width
                    };

                    // Per-window corner radius (feature 3)
                    let radius = wt.corner_radius_override.unwrap_or(
                        if Self::class_matches_exclude(&wt.class_name, &self.rounded_corners_exclude) {
                            0.0
                        } else {
                            self.corner_radius
                        }
                    );

                    // Phase 3.4: Focus highlight scale bounce (1.02x)
                    let highlight_scale = if focus_highlight_active_for_win {
                        let elapsed_ms = self.focus_highlight_start.unwrap().1.elapsed().as_millis() as f32;
                        let dur = self.focus_highlight_duration_ms as f32;
                        let t = (elapsed_ms / dur).min(1.0);
                        1.0 + 0.02 * (1.0 - t) * ((t * std::f32::consts::PI).sin())
                    } else { 1.0 };
                    let _ = highlight_scale; // used below in scale computation

                    // Feature 4: Apply scale
                    let scale = wt.scale * wt.anim_scale;
                    let (draw_x, draw_y, draw_w, draw_h) = if (scale - 1.0).abs() > f32::EPSILON {
                        let cw = w as f32 * scale;
                        let ch = h as f32 * scale;
                        let cx = x as f32 + (w as f32 - cw) * 0.5;
                        let cy = y as f32 + (h as f32 - ch) * 0.5;
                        (cx, cy, cw, ch)
                    } else {
                        (x as f32, y as f32, w as f32, h as f32)
                    };

                    // Expand the draw rect outward by border_width so the
                    // border is rendered *outside* the window content area.
                    // The SDF in the shader treats dist=0 at the expanded rect
                    // edge and dist=-bw at the original window edge, so the
                    // border naturally fills the gap between the two.
                    let bdr_x = draw_x - bw;
                    let bdr_y = draw_y - bw;
                    let bdr_w = draw_w + 2.0 * bw;
                    let bdr_h = draw_h + 2.0 * bw;

                    self.gl.uniform_1_f32(
                        self.border_uniforms.border_width.as_ref(), bw,
                    );
                    self.gl.uniform_4_f32(
                        self.border_uniforms.border_color.as_ref(),
                        color[0], color[1], color[2], color[3] * fade,
                    );
                    self.gl.uniform_1_f32(
                        self.border_uniforms.radius.as_ref(), radius + bw,
                    );
                    self.gl.uniform_2_f32(
                        self.border_uniforms.size.as_ref(), bdr_w, bdr_h,
                    );
                    self.gl.uniform_4_f32(
                        self.border_uniforms.rect.as_ref(), bdr_x, bdr_y, bdr_w, bdr_h,
                    );
                    self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                }

                self.gl.bind_vertex_array(None);
                self.gl.use_program(None);
            }
        }

        // === Pass 3b: Urgent/PiP borders (even if borders disabled) ===
        if !self.border_enabled {
            let has_special = visible_scene.iter().any(|&(win, _, _, _, _)| {
                self.windows.get(&win).map_or(false, |wt| {
                    (wt.is_urgent && self.attention_animation) || wt.is_pip
                })
            });
            if has_special {
                // Draw borders only for urgent/pip windows
                unsafe {
                    self.gl.use_program(Some(self.border_program));
                    self.gl.uniform_matrix_4_f32_slice(
                        self.border_uniforms.projection.as_ref(), false, &proj,
                    );
                    self.gl.bind_vertex_array(Some(self.quad_vao));

                    for &(win, x, y, w, h) in visible_scene {
                        if overview_skip(x, y, w, h) { continue; }
                        let wt = match self.windows.get(&win) {
                            Some(wt) => wt,
                            None => continue,
                        };
                        if !((wt.is_urgent && self.attention_animation) || wt.is_pip) {
                            continue;
                        }
                        let fade = wt.fade_opacity;
                        if fade <= 0.0 { continue; }

                        let color = if wt.is_urgent && self.attention_animation {
                            let elapsed = self.compositor_start_time.elapsed().as_secs_f32();
                            let pulse = (elapsed * 4.0).sin() * 0.5 + 0.5;
                            let [r, g, b, a] = self.attention_color;
                            [r, g, b, a * pulse]
                        } else {
                            self.pip_border_color
                        };

                        let bw = if wt.is_pip { self.pip_border_width } else { 2.0 };

                        let radius = wt.corner_radius_override.unwrap_or(
                            if Self::class_matches_exclude(&wt.class_name, &self.rounded_corners_exclude) {
                                0.0
                            } else {
                                self.corner_radius
                            }
                        );

                        let scale = wt.scale * wt.anim_scale;
                        let (draw_x, draw_y, draw_w, draw_h) = if (scale - 1.0).abs() > f32::EPSILON {
                            let cw = w as f32 * scale;
                            let ch = h as f32 * scale;
                            let cx = x as f32 + (w as f32 - cw) * 0.5;
                            let cy = y as f32 + (h as f32 - ch) * 0.5;
                            (cx, cy, cw, ch)
                        } else {
                            (x as f32, y as f32, w as f32, h as f32)
                        };

                        // Expand rect outward for outside-window border rendering.
                        let bdr_x = draw_x - bw;
                        let bdr_y = draw_y - bw;
                        let bdr_w = draw_w + 2.0 * bw;
                        let bdr_h = draw_h + 2.0 * bw;

                        self.gl.uniform_1_f32(self.border_uniforms.border_width.as_ref(), bw);
                        self.gl.uniform_4_f32(
                            self.border_uniforms.border_color.as_ref(),
                            color[0], color[1], color[2], color[3] * fade,
                        );
                        self.gl.uniform_1_f32(self.border_uniforms.radius.as_ref(), radius + bw);
                        self.gl.uniform_2_f32(self.border_uniforms.size.as_ref(), bdr_w, bdr_h);
                        self.gl.uniform_4_f32(
                            self.border_uniforms.rect.as_ref(), bdr_x, bdr_y, bdr_w, bdr_h,
                        );
                        self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                    }

                    self.gl.bind_vertex_array(None);
                    self.gl.use_program(None);
                }
            }
        }

        // === Pass 3c: Window tab bars ===
        if self.window_tabs_enabled && !self.window_groups.is_empty() {
            for &(win, x, y, w, _h) in visible_scene {
                if let Some((_gid, tabs)) = self.find_window_group(win) {
                    let tabs_owned: Vec<WindowTab> = tabs.iter().map(|t| WindowTab {
                        x11_win: t.x11_win,
                        title: t.title.clone(),
                        is_active: t.is_active,
                    }).collect();
                    self.render_tab_bar(&proj, x as f32, y as f32, w as f32, &tabs_owned);
                }
            }
        }

        // Disable scissor (feature 6)
        if use_scissor {
            unsafe { self.gl.disable(glow::SCISSOR_TEST); }
        }

        // === Pass 4: Post-processing (features 8/9/10) ===
        if postprocess_active {
            let (_, pp_tex) = self.postprocess_fbo.as_ref().unwrap();
            let pp_tex = *pp_tex;
            unsafe {
                // Switch back to default framebuffer
                self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                self.gl.viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
                self.gl.clear(glow::COLOR_BUFFER_BIT);

                self.gl.use_program(Some(self.postprocess_program));
                // Set up fullscreen quad
                let pp_proj = ortho(0.0, self.screen_w as f32, self.screen_h as f32, 0.0, -1.0, 1.0);
                // The postprocess program uses blur vertex shader which has u_rect and u_projection
                // We need to get those uniform locations
                let pp_proj_loc = self.gl.get_uniform_location(self.postprocess_program, "u_projection");
                let pp_rect_loc = self.gl.get_uniform_location(self.postprocess_program, "u_rect");
                self.gl.uniform_matrix_4_f32_slice(pp_proj_loc.as_ref(), false, &pp_proj);
                self.gl.uniform_4_f32(pp_rect_loc.as_ref(), 0.0, 0.0, self.screen_w as f32, self.screen_h as f32);

                self.gl.uniform_1_i32(self.postprocess_uniforms.texture.as_ref(), 0);
                self.gl.uniform_1_f32(self.postprocess_uniforms.color_temp.as_ref(), self.color_temperature);
                self.gl.uniform_1_f32(self.postprocess_uniforms.saturation.as_ref(), self.saturation);
                self.gl.uniform_1_f32(self.postprocess_uniforms.brightness.as_ref(), self.brightness);
                self.gl.uniform_1_f32(self.postprocess_uniforms.contrast.as_ref(), self.contrast);
                self.gl.uniform_1_i32(self.postprocess_uniforms.invert.as_ref(), if self.invert_colors { 1 } else { 0 });
                self.gl.uniform_1_i32(self.postprocess_uniforms.grayscale.as_ref(), if self.grayscale { 1 } else { 0 });

                // Magnifier uniforms
                self.gl.uniform_1_i32(self.magnifier_uniforms.magnifier_enabled.as_ref(), if self.magnifier_enabled { 1 } else { 0 });
                if self.magnifier_enabled {
                    let cx = self.mouse_x / self.screen_w as f32;
                    let cy = self.mouse_y / self.screen_h as f32;
                    // The fragment shader flips Y (uv.y = 1.0 - v_uv.y) so that
                    // uv.y=1 corresponds to the top of the screen.  Flip cy to match.
                    self.gl.uniform_2_f32(self.magnifier_uniforms.magnifier_center.as_ref(), cx, 1.0 - cy);
                    self.gl.uniform_1_f32(self.magnifier_uniforms.magnifier_radius.as_ref(), self.magnifier_radius / self.screen_w as f32);
                    self.gl.uniform_1_f32(self.magnifier_uniforms.magnifier_zoom.as_ref(), self.magnifier_zoom);
                }

                // Colorblind correction uniform
                self.gl.uniform_1_i32(self.magnifier_uniforms.colorblind_mode.as_ref(), self.colorblind_mode);

                self.gl.active_texture(glow::TEXTURE0);
                self.gl.bind_texture(glow::TEXTURE_2D, Some(pp_tex));
                self.gl.bind_vertex_array(Some(self.quad_vao));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

                self.gl.bind_vertex_array(None);
                self.gl.use_program(None);
            }
        }

        // Tick tilt after the render loop has set tilt_target from the focused window.
        // If no focused window set tilt_target this frame, it keeps 0 from the reset
        // at the start of the loop (see the tilt branch which sets tilt_target_x/y).
        {
            let dt = self.frame_stats.last_frame_time.elapsed().as_secs_f32();
            let tilt_animating = self.tick_tilt(dt);
            if tilt_animating {
                self.needs_render = true;
            }
        }

        // === Pass 5: Debug HUD (feature 11) ===
        if self.debug_hud {
            // Update frame stats
            let now = std::time::Instant::now();
            let dt = now.duration_since(self.frame_stats.last_frame_time).as_secs_f32();
            self.frame_stats.last_frame_time = now;
            self.frame_stats.frame_count += 1;
            self.frame_stats.frame_times.push(dt);
            if self.frame_stats.frame_times.len() > 120 {
                self.frame_stats.frame_times.remove(0);
            }
            let elapsed = now.duration_since(self.frame_stats.last_fps_update).as_secs_f32();
            if elapsed >= 1.0 {
                self.frame_stats.fps = self.frame_stats.frame_times.len() as f32 / elapsed;
                self.frame_stats.frame_times.clear();
                self.frame_stats.last_fps_update = now;
            }

            // Format HUD text
            let avg_dt = if self.frame_stats.frame_times.is_empty() { 0.0 }
                else { self.frame_stats.frame_times.iter().sum::<f32>() / self.frame_stats.frame_times.len() as f32 };
            let mut hud_text = format!(
                "FPS: {:.1}  {:.1}ms\nWindows: {}",
                self.frame_stats.fps, avg_dt * 1000.0, self.windows.len(),
            );
            if self.debug_hud_extended {
                let tex_mem_kb = self.frame_stats.texture_memory_bytes / 1024;
                use std::fmt::Write;
                let _ = write!(
                    hud_text,
                    "\nDraw calls: {}\nTex mem: {}KB\nBlur: {}h/{}m",
                    self.frame_stats.draw_calls, tex_mem_kb,
                    self.frame_stats.blur_cache_hits, self.frame_stats.blur_cache_misses,
                );
            }

            // Update text texture (skips upload if content unchanged)
            self.update_hud_text_texture(&hud_text);

            // Compute panel dimensions from text texture
            let pad = 8.0f32;
            let text_w = self.hud_text_width as f32;
            let text_h = self.hud_text_height as f32;
            let hud_w = text_w + pad * 2.0;
            let hud_h = text_h + pad * 2.0;
            let hud_x = self.screen_w as f32 - hud_w - 10.0;
            let hud_y = 10.0f32;

            unsafe {
                // Draw background panel
                self.gl.use_program(Some(self.hud_program));
                self.gl.uniform_matrix_4_f32_slice(
                    self.hud_uniforms.projection.as_ref(), false, &proj,
                );
                self.gl.uniform_4_f32(
                    self.hud_uniforms.bg_color.as_ref(), 0.0, 0.0, 0.0, 0.7,
                );
                self.gl.uniform_4_f32(
                    self.hud_uniforms.fg_color.as_ref(), 0.0, 1.0, 0.0, 1.0,
                );
                self.gl.uniform_2_f32(
                    self.hud_uniforms.size.as_ref(), hud_w, hud_h,
                );
                self.gl.uniform_4_f32(
                    self.hud_uniforms.rect.as_ref(), hud_x, hud_y, hud_w, hud_h,
                );
                self.gl.bind_vertex_array(Some(self.quad_vao));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

                // Draw text overlay
                if let Some(tex) = self.hud_text_texture {
                    self.gl.use_program(Some(self.hud_text_program));
                    self.gl.uniform_matrix_4_f32_slice(
                        self.hud_text_uniforms.projection.as_ref(), false, &proj,
                    );
                    self.gl.uniform_4_f32(
                        self.hud_text_uniforms.rect.as_ref(),
                        hud_x + pad, hud_y + pad, text_w, text_h,
                    );
                    self.gl.uniform_1_i32(
                        self.hud_text_uniforms.texture.as_ref(), 0,
                    );
                    self.gl.active_texture(glow::TEXTURE0);
                    self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                    self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                }

                self.gl.bind_vertex_array(None);
                self.gl.use_program(None);
            }

            // Log stats periodically
            if self.frame_stats.frame_count % 60 == 0 {
                if self.debug_hud_extended {
                    let tex_mem_kb = self.frame_stats.texture_memory_bytes / 1024;
                    log::info!(
                        "[HUD] FPS: {:.1}, frame_time: {:.2}ms, windows: {}, draw_calls: {}, tex_mem: {}KB, blur_hits: {}, blur_misses: {}",
                        self.frame_stats.fps, avg_dt * 1000.0, self.windows.len(),
                        self.frame_stats.draw_calls, tex_mem_kb,
                        self.frame_stats.blur_cache_hits, self.frame_stats.blur_cache_misses,
                    );
                    self.frame_stats.draw_calls = 0;
                } else {
                    log::info!(
                        "[HUD] FPS: {:.1}, frame_time: {:.2}ms, windows: {}",
                        self.frame_stats.fps, avg_dt * 1000.0, self.windows.len()
                    );
                }
            }
        }

        // === Pass 5b: Screen edge glow ===
        // Tick the countdown so the glow expires even without new mouse events.
        if self.edge_glow {
            self.edge_glow_tick(self.mouse_x, self.mouse_y);
        }
        if self.edge_glow_active && self.edge_glow_width > 0.0 {
            unsafe {
                self.gl.use_program(Some(self.edge_glow_program));
                self.gl.uniform_matrix_4_f32_slice(
                    self.edge_glow_uniforms.projection.as_ref(), false, &proj,
                );
                self.gl.uniform_4_f32(
                    self.edge_glow_uniforms.rect.as_ref(),
                    0.0, 0.0, self.screen_w as f32, self.screen_h as f32,
                );
                self.gl.uniform_4_f32(
                    self.edge_glow_uniforms.glow_color.as_ref(),
                    self.edge_glow_color[0], self.edge_glow_color[1],
                    self.edge_glow_color[2], self.edge_glow_color[3],
                );
                self.gl.uniform_1_f32(self.edge_glow_uniforms.glow_width.as_ref(), self.edge_glow_width);
                self.gl.uniform_2_f32(self.edge_glow_uniforms.mouse.as_ref(), self.mouse_x, self.mouse_y);
                self.gl.uniform_2_f32(
                    self.edge_glow_uniforms.screen_size.as_ref(),
                    self.screen_w as f32, self.screen_h as f32,
                );
                self.gl.uniform_1_f32(
                    self.edge_glow_uniforms.time.as_ref(),
                    self.compositor_start_time.elapsed().as_secs_f32(),
                );
                self.gl.bind_vertex_array(Some(self.quad_vao));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                self.gl.bind_vertex_array(None);
                self.gl.use_program(None);
            }
        }

        // === Pass 5c: Particle effects ===
        if !self.particle_systems.is_empty() {
            self.tick_particles();
            self.render_particles(&proj);
        }

        // === Pass 5d: Overview overlay ===
        if self.overview_active {
            self.tick_overview_prism();
            self.render_overview(&proj, focused);
        }

        // === Pass 5f: Expose/Mission Control overlay ===
        if !self.expose_entries.is_empty() {
            self.render_expose(&proj);
        }

        // === Pass 5g: Snap preview ===
        self.render_snap_preview(&proj);

        // === Pass 5e: Annotations overlay ===
        if self.annotation_active && !self.annotation_strokes.is_empty() {
            self.render_annotations(&proj);
        }

        // === Feature 12: Screenshot capture (after all rendering, before swap) ===
        if let Some(path) = self.pending_screenshot.take() {
            self.capture_screenshot(&path);
        }

        // === Tag-switch transition overlay ===
        let transition_still_active = if let Some(progress) = self.transition_progress(std::time::Instant::now()) {
            // Monitor-local geometry for the transition
            let mon_x = self.transition_mon_x;
            let mon_y = self.transition_mon_y;
            let mon_w = self.transition_mon_w;
            let mon_h = self.transition_mon_h;
            let exclude_top = self.transition_exclude_top.min(mon_h);
            let draw_y = (mon_y as u32 + exclude_top) as f32; // Y in screen coords
            let draw_h = (mon_h - exclude_top) as f32;
            let draw_x = mon_x as f32;
            let top_frac = if mon_h == 0 { 0.0 } else { exclude_top as f32 / mon_h as f32 };
            // OpenGL scissor Y is flipped
            let scissor_gl_y = self.screen_h as i32 - (mon_y + mon_h as i32);

            match self.transition_mode {
                TransitionMode::Slide => {
                    // --- Slide mode: old scene slides out + fades ---
                    // New scene is already in the back-buffer at final position.
                    // Old snapshot slides in transition_direction while fading out,
                    // giving the effect of current windows sliding away to reveal
                    // the target windows underneath.
                    if let Some((_, snap_tex)) = &self.transition_fbo {
                        let snap_tex = *snap_tex;

                        // Slide offset: old scene moves in the transition direction
                        let slide_offset = progress * self.transition_direction * mon_w as f32;

                        // Fade out smoothly over the full duration
                        let fade_opacity = (1.0 - progress).max(0.0);

                        unsafe {
                            if draw_h > 0.0 && fade_opacity > 0.0 {
                                self.gl.enable(glow::SCISSOR_TEST);
                                self.gl.scissor(
                                    mon_x,
                                    scissor_gl_y,
                                    mon_w as i32,
                                    (mon_h - exclude_top) as i32,
                                );

                                self.gl.blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);

                                self.gl.use_program(Some(self.transition_program));
                                self.gl.uniform_matrix_4_f32_slice(
                                    self.transition_uniforms.projection.as_ref(), false, &proj,
                                );
                                self.gl.uniform_1_i32(self.transition_uniforms.texture.as_ref(), 0);
                                self.gl.active_texture(glow::TEXTURE0);

                                let uv = [0.0f32, 0.0, 1.0, 1.0 - top_frac];

                                self.gl.uniform_4_f32(
                                    self.transition_uniforms.rect.as_ref(),
                                    draw_x + slide_offset, draw_y, mon_w as f32, draw_h,
                                );
                                self.gl.uniform_1_f32(
                                    self.transition_uniforms.opacity.as_ref(), fade_opacity,
                                );
                                self.gl.uniform_4_f32(
                                    self.transition_uniforms.uv_rect.as_ref(),
                                    uv[0], uv[1], uv[2], uv[3],
                                );
                                self.gl.bind_texture(glow::TEXTURE_2D, Some(snap_tex));
                                self.gl.bind_vertex_array(Some(self.quad_vao));
                                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

                                self.gl.bind_vertex_array(None);
                                self.gl.use_program(None);

                                self.gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
                                self.gl.disable(glow::SCISSOR_TEST);
                            }
                        }
                    }
                }
                TransitionMode::Cube => {
                    // --- Cube mode: 3D rotating cube transition ---
                    self.render_cube_transition(progress, &proj);
                }
                TransitionMode::Fade => {
                    // --- Fade mode: old scene fades out, new scene fades in ---
                    if let Some((_, snap_tex)) = &self.transition_fbo {
                        let snap_tex = *snap_tex;
                        let fade_opacity = (1.0 - progress).max(0.0);
                        unsafe {
                            if draw_h > 0.0 && fade_opacity > 0.0 {
                                self.gl.enable(glow::SCISSOR_TEST);
                                self.gl.scissor(mon_x, scissor_gl_y, mon_w as i32, (mon_h - exclude_top) as i32);
                                self.gl.blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);
                                self.gl.use_program(Some(self.transition_program));
                                self.gl.uniform_matrix_4_f32_slice(self.transition_uniforms.projection.as_ref(), false, &proj);
                                self.gl.uniform_1_i32(self.transition_uniforms.texture.as_ref(), 0);
                                self.gl.active_texture(glow::TEXTURE0);
                                let uv = [0.0f32, 0.0, 1.0, 1.0 - top_frac];
                                self.gl.uniform_4_f32(self.transition_uniforms.rect.as_ref(), draw_x, draw_y, mon_w as f32, draw_h);
                                self.gl.uniform_1_f32(self.transition_uniforms.opacity.as_ref(), fade_opacity);
                                self.gl.uniform_4_f32(self.transition_uniforms.uv_rect.as_ref(), uv[0], uv[1], uv[2], uv[3]);
                                self.gl.bind_texture(glow::TEXTURE_2D, Some(snap_tex));
                                self.gl.bind_vertex_array(Some(self.quad_vao));
                                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                                self.gl.bind_vertex_array(None);
                                self.gl.use_program(None);
                                self.gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
                                self.gl.disable(glow::SCISSOR_TEST);
                            }
                        }
                    }
                }
                TransitionMode::Flip => {
                    // --- Flip mode: card-flip around Y axis ---
                    self.render_flip_transition(progress, &proj);
                }
                TransitionMode::Zoom => {
                    // --- Zoom mode: old scene shrinks + fades, new scene grows in ---
                    if let Some((_, snap_tex)) = &self.transition_fbo {
                        let snap_tex = *snap_tex;
                        let fade_opacity = (1.0 - progress).max(0.0);
                        // Old scene shrinks toward center
                        let scale = 1.0 - progress * 0.5; // 1.0 → 0.5
                        let scaled_w = mon_w as f32 * scale;
                        let scaled_h = draw_h * scale;
                        let offset_x = draw_x + (mon_w as f32 - scaled_w) * 0.5;
                        let offset_y = draw_y + (draw_h - scaled_h) * 0.5;
                        unsafe {
                            if draw_h > 0.0 && fade_opacity > 0.0 {
                                self.gl.enable(glow::SCISSOR_TEST);
                                self.gl.scissor(mon_x, scissor_gl_y, mon_w as i32, (mon_h - exclude_top) as i32);
                                self.gl.blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);
                                self.gl.use_program(Some(self.transition_program));
                                self.gl.uniform_matrix_4_f32_slice(self.transition_uniforms.projection.as_ref(), false, &proj);
                                self.gl.uniform_1_i32(self.transition_uniforms.texture.as_ref(), 0);
                                self.gl.active_texture(glow::TEXTURE0);
                                let uv = [0.0f32, 0.0, 1.0, 1.0 - top_frac];
                                self.gl.uniform_4_f32(self.transition_uniforms.rect.as_ref(), offset_x, offset_y, scaled_w, scaled_h);
                                self.gl.uniform_1_f32(self.transition_uniforms.opacity.as_ref(), fade_opacity);
                                self.gl.uniform_4_f32(self.transition_uniforms.uv_rect.as_ref(), uv[0], uv[1], uv[2], uv[3]);
                                self.gl.bind_texture(glow::TEXTURE_2D, Some(snap_tex));
                                self.gl.bind_vertex_array(Some(self.quad_vao));
                                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                                self.gl.bind_vertex_array(None);
                                self.gl.use_program(None);
                                self.gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
                                self.gl.disable(glow::SCISSOR_TEST);
                            }
                        }
                    }
                }
                TransitionMode::Stack => {
                    // --- Stack mode: new scene slides over old with depth effect ---
                    if let Some((_, snap_tex)) = &self.transition_fbo {
                        let snap_tex = *snap_tex;
                        // Old scene stays in place but darkens and scales down slightly
                        let dim = 1.0 - progress * 0.3; // 1.0 → 0.7
                        let old_scale = 1.0 - progress * 0.05; // 1.0 → 0.95
                        let old_w = mon_w as f32 * old_scale;
                        let old_h = draw_h * old_scale;
                        let old_x = draw_x + (mon_w as f32 - old_w) * 0.5;
                        let old_y = draw_y + (draw_h - old_h) * 0.5;
                        unsafe {
                            if draw_h > 0.0 {
                                self.gl.enable(glow::SCISSOR_TEST);
                                self.gl.scissor(mon_x, scissor_gl_y, mon_w as i32, (mon_h - exclude_top) as i32);

                                // First: clear workspace area and redraw wallpaper behind
                                self.gl.clear_color(0.0, 0.0, 0.0, 1.0);
                                self.gl.clear(glow::COLOR_BUFFER_BIT);
                                self.gl.viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
                                self.draw_wallpaper_in_region(&proj, mon_x, mon_y, mon_w, mon_h);

                                // Draw dimmed/scaled old scene
                                self.gl.blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);
                                self.gl.use_program(Some(self.transition_program));
                                self.gl.uniform_matrix_4_f32_slice(self.transition_uniforms.projection.as_ref(), false, &proj);
                                self.gl.uniform_1_i32(self.transition_uniforms.texture.as_ref(), 0);
                                self.gl.active_texture(glow::TEXTURE0);
                                let uv = [0.0f32, 0.0, 1.0, 1.0 - top_frac];
                                self.gl.uniform_4_f32(self.transition_uniforms.rect.as_ref(), old_x, old_y, old_w, old_h);
                                self.gl.uniform_1_f32(self.transition_uniforms.opacity.as_ref(), dim);
                                self.gl.uniform_4_f32(self.transition_uniforms.uv_rect.as_ref(), uv[0], uv[1], uv[2], uv[3]);
                                self.gl.bind_texture(glow::TEXTURE_2D, Some(snap_tex));
                                self.gl.bind_vertex_array(Some(self.quad_vao));
                                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

                                // Draw new scene sliding in from the transition direction
                                // New scene is already rendered in the back-buffer; we blit
                                // from transition_new_fbo if available, otherwise approximate
                                // by drawing the back-buffer content as a sliding overlay.
                                // For Stack, capture new scene like cube does.
                                if self.transition_new_fbo.is_none() {
                                    self.transition_new_fbo =
                                        Self::create_scene_fbo(&self.gl, mon_w, mon_h).ok();
                                }
                                if let Some((new_fbo, new_tex)) = &self.transition_new_fbo {
                                    let new_fbo = *new_fbo;
                                    let new_tex = *new_tex;
                                    // Blit current back-buffer into new_fbo
                                    let blit_gl_y = self.screen_h as i32 - (mon_y + mon_h as i32);
                                    self.gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
                                    self.gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(new_fbo));
                                    self.gl.blit_framebuffer(
                                        mon_x, blit_gl_y,
                                        mon_x + mon_w as i32, blit_gl_y + mon_h as i32,
                                        0, 0, mon_w as i32, mon_h as i32,
                                        glow::COLOR_BUFFER_BIT, glow::NEAREST,
                                    );
                                    self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);

                                    // New scene slides in from the side
                                    let new_slide = (1.0 - progress) * self.transition_direction * mon_w as f32;
                                    self.gl.uniform_4_f32(self.transition_uniforms.rect.as_ref(), draw_x + new_slide, draw_y, mon_w as f32, draw_h);
                                    self.gl.uniform_1_f32(self.transition_uniforms.opacity.as_ref(), 1.0);
                                    self.gl.uniform_4_f32(self.transition_uniforms.uv_rect.as_ref(), uv[0], uv[1], uv[2], uv[3]);
                                    self.gl.bind_texture(glow::TEXTURE_2D, Some(new_tex));
                                    self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                                }

                                self.gl.bind_vertex_array(None);
                                self.gl.use_program(None);
                                self.gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
                                self.gl.disable(glow::SCISSOR_TEST);
                            }
                        }
                    }
                }
                TransitionMode::Blinds => {
                    // --- Blinds mode: vertical strips flip to reveal new scene ---
                    self.render_blinds_transition(progress, &proj);
                }
                TransitionMode::CoverFlow => {
                    self.render_coverflow_transition(progress, &proj);
                }
                TransitionMode::Helix => {
                    self.render_helix_transition(progress, &proj);
                }
                TransitionMode::Portal => {
                    self.render_portal_transition(progress, &proj);
                }
            }
            true
        } else {
            // Transition finished — clean up
            if self.transition_start.is_some() {
                self.transition_start = None;
                log::debug!("compositor: tag-switch transition completed");
            }
            false
        };

        // Swap buffers (double-buffered with vsync for tear-free output).
        unsafe {
            x11::glx::glXSwapBuffers(self.xlib_display, self.glx_drawable);
        }

        // === Phase 7.3: Recording frame capture ===
        if self.recording_active {
            self.capture_recording_frame();
        }

        // Schedule re-render if fades or transition are still in progress
        if fades_active || transition_still_active || wobbly_active || !self.particle_systems.is_empty() || self.overview_active
            || genie_active || ripples_active || focus_highlight_active || wallpaper_crossfade_active
            || expose_animating || snap_animating || peek_animating || self.expose_active
        {
            self.needs_render = true;
        }

        // Schedule re-render if recording is active (need continuous frames)
        if self.recording_active {
            self.needs_render = true;
        }

        // Animate zoom-to-fit scale
        if (self.zoom_to_fit_scale - self.zoom_to_fit_target).abs() > 0.001 {
            self.zoom_to_fit_scale += (self.zoom_to_fit_target - self.zoom_to_fit_scale) * 0.15;
            if (self.zoom_to_fit_scale - self.zoom_to_fit_target).abs() < 0.001 {
                self.zoom_to_fit_scale = self.zoom_to_fit_target;
            }
            self.needs_render = true;
        }

        true
    }

    // =====================================================================
    // New feature methods
    // =====================================================================

    pub(super) fn set_mouse_position(&mut self, x: f32, y: f32) {
        self.mouse_x = x;
        self.mouse_y = y;
        if self.edge_glow {
            self.edge_glow_tick(x, y);
        }
        if self.magnifier_enabled || self.window_tilt {
            self.needs_render = true;
        }
        if self.expose_active {
            self.expose_set_hover(x, y);
        }
    }

    /// Core edge-glow state machine (called from mouse events and render tick).
    ///
    /// - Mouse at edge (unsuppressed) → activate.
    /// - Mouse away or suppressed     → deactivate immediately.
    fn edge_glow_tick(&mut self, mx: f32, my: f32) {
        let sw = self.screen_w as f32;
        let sh = self.screen_h as f32;
        let min_dist = mx.min(sw - mx).min(my).min(sh - my);
        let at_edge = min_dist < self.edge_glow_width;

        if at_edge && !self.edge_glow_suppressed {
            if !self.edge_glow_active {
                self.edge_glow_active = true;
                self.needs_render = true;
            }
        } else if self.edge_glow_active {
            self.edge_glow_active = false;
            self.needs_render = true;
        }
    }

    /// Immediately deactivate the edge glow and suppress re-activation
    /// until the pointer leaves the window (returns to root/desktop).
    pub(super) fn deactivate_edge_glow(&mut self) {
        if self.edge_glow {
            self.edge_glow_suppressed = true;
            if self.edge_glow_active {
                self.edge_glow_active = false;
                self.needs_render = true;
            }
        }
    }

    /// Clear the edge-glow suppression (pointer returned to desktop).
    pub(super) fn unsuppress_edge_glow(&mut self) {
        self.edge_glow_suppressed = false;
    }

    pub(super) fn set_window_urgent(&mut self, x11_win: u32, urgent: bool) {
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            wt.is_urgent = urgent;
            self.needs_render = true;
        }
    }

    pub(super) fn set_window_pip(&mut self, x11_win: u32, pip: bool) {
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            wt.is_pip = pip;
            self.needs_render = true;
        }
    }

    pub(super) fn set_magnifier(&mut self, enabled: bool) {
        self.magnifier_enabled = enabled;
        self.ensure_postprocess_fbo();
        self.needs_render = true;
    }

    pub(super) fn set_overview_monitor(&mut self, x: i32, y: i32, w: u32, h: u32) {
        self.overview_mon_x = x;
        self.overview_mon_y = y;
        self.overview_mon_w = w;
        self.overview_mon_h = h;
    }

    pub(super) fn set_overview_mode(&mut self, active: bool, windows: Vec<(u32, f32, f32, f32, f32, bool, String)>) {
        if !active && self.overview_active && !self.overview_closing {
            // Begin exit animation — don't clear state yet
            self.overview_closing = true;
            self.overview_exit_progress = 1.0;
            self.needs_render = true;
            return;
        }
        self.clear_overview_snapshots();
        self.clear_overview_title_textures();
        self.overview_active = active;
        self.overview_closing = false;
        let n = windows.len();
        let face_w = self.screen_w as f32 * 0.8;
        let face_h = self.screen_h as f32 * 0.8;
        self.overview_windows = windows.into_iter().enumerate().map(|(i, (win, _x, _y, _w, _h, sel, title))| {
            OverviewEntry {
                x11_win: win,
                target_x: 0.0,
                target_y: 0.0,
                target_w: face_w,
                target_h: face_h,
                is_selected: sel,
                snapshot_texture: None,
                title,
                title_texture: None,
                face_index: i.min(5),
            }
        }).collect();
        self.overview_total_clients = n;
        self.overview_slide_offset = 0;
        self.overview_prism_target_angle = 0.0;
        self.overview_prism_current_angle = 0.0;
        self.overview_prism_last_tick = None;
        if active {
            self.refresh_overview_snapshots();
            self.create_overview_title_textures();
            self.overview_entry_progress = 0.0;
            self.overview_exit_progress = 1.0;
            self.overview_opacity = 0.0;
        } else {
            self.overview_entry_progress = 1.0;
            self.overview_exit_progress = 1.0;
            self.overview_opacity = 0.0;
        }
        self.needs_render = true;
    }

    pub(super) fn set_overview_selection(&mut self, x11_win: u32) {
        let mut selected_face = 0usize;
        for entry in &mut self.overview_windows {
            let sel = entry.x11_win == x11_win;
            entry.is_selected = sel;
            if sel {
                selected_face = entry.face_index;
            }
        }
        // Rotate prism so selected face faces the camera.
        let new_target = -(selected_face as f32) * std::f32::consts::FRAC_PI_3;
        // Normalize angular difference to shortest path (within -PI..PI).
        let mut diff = new_target - self.overview_prism_target_angle;
        while diff > std::f32::consts::PI { diff -= 2.0 * std::f32::consts::PI; }
        while diff < -std::f32::consts::PI { diff += 2.0 * std::f32::consts::PI; }
        self.overview_prism_target_angle += diff;
        self.needs_render = true;
    }

    pub(super) fn notify_window_move_start(&mut self, x11_win: u32) {
        if !self.wobbly_windows { return; }
        let grid_n = (self.wobbly_grid_size as usize + 1).min(17);
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            // Determine anchor node: closest grid node to mouse position
            let rel_x = ((self.mouse_x - wt.x as f32).max(0.0)).min(wt.w as f32);
            let rel_y = ((self.mouse_y - wt.y as f32).max(0.0)).min(wt.h as f32);
            let anchor_col = ((rel_x / wt.w as f32) * (grid_n - 1) as f32).round() as usize;
            let anchor_row = ((rel_y / wt.h as f32) * (grid_n - 1) as f32).round() as usize;

            let count = grid_n * grid_n;
            wt.wobbly = Some(WobblyState {
                grid_n,
                offsets: vec![[0.0; 2]; count],
                velocities: vec![[0.0; 2]; count],
                dragging: true,
                anchor_row: anchor_row.min(grid_n - 1),
                anchor_col: anchor_col.min(grid_n - 1),
                last_tick: std::time::Instant::now(),
            });
        } else {
            log::warn!("[wobbly] move_start: window 0x{:x} not tracked by compositor", x11_win);
        }
    }

    pub(super) fn notify_window_move_delta(&mut self, x11_win: u32, dx: f32, dy: f32) {
        // Phase 3.1: Record position for motion trail
        if self.motion_trail_enabled {
            if let Some(wt) = self.windows.get(&x11_win) {
                let cur_x = wt.x;
                let cur_y = wt.y;
                self.update_motion_trail(x11_win, cur_x, cur_y);
            }
        }

        if self.wobbly_windows {
            if let Some(wt) = self.windows.get_mut(&x11_win) {
                if let Some(ref mut w) = wt.wobbly {
                    // The window has already moved to the new position.
                    // Anchor node stays at [0,0] (moves with the window).
                    // All OTHER nodes get a reverse impulse to simulate inertia.
                    let n = w.grid_n;
                    let ar = w.anchor_row;
                    let ac = w.anchor_col;
                    for row in 0..n {
                        for col in 0..n {
                            if row == ar && col == ac { continue; }
                            let idx = row * n + col;
                            w.offsets[idx][0] -= dx;
                            w.offsets[idx][1] -= dy;
                        }
                    }
                    // Ensure anchor stays pinned at zero
                    let ai = ar * n + ac;
                    w.offsets[ai] = [0.0, 0.0];
                    w.velocities[ai] = [0.0, 0.0];
                }
            }
        }

        // During interactive move/resize, request full-frame redraw when backdrop
        // blur is active so translucent windows see real-time updated background.
        let blur_active = self.blur_enabled
            && self.scene_fbo.is_some()
            && !self.blur_fbos.is_empty()
            && self.windows.values().any(|wt| self.needs_backdrop_blur(wt));
        if blur_active {
            self.damage_tracker.mark_all_dirty();
        }
        self.needs_render = true;
    }

    pub(super) fn notify_window_move_end(&mut self, x11_win: u32) {
        // Phase 3.1: Clear motion trail
        self.clear_motion_trail(x11_win);

        // Release anchor — let all nodes spring back via tick_wobbly
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            if let Some(ref mut w) = wt.wobbly {
                w.dragging = false;
            }
        }
    }

    #[allow(dead_code)]
    pub(super) fn tracked_window_count(&self) -> usize {
        self.windows.len()
    }

    /// Set dock/taskbar position for genie minimize target.
    pub(super) fn set_dock_position(&mut self, x: f32, y: f32) {
        self.dock_position = (x, y);
    }

    #[allow(dead_code)]
    pub(super) fn has_window(&self, x11_win: u32) -> bool {
        self.windows.contains_key(&x11_win)
    }

    // =====================================================================
    // Phase 6: Accessibility & Utility
    // =====================================================================

    pub(super) fn set_colorblind_mode(&mut self, mode: &str) {
        let m = match mode {
            "deuteranopia" => 1,
            "protanopia" => 2,
            "tritanopia" => 3,
            _ => 0,
        };
        if self.colorblind_mode != m {
            self.colorblind_mode = m;
            self.ensure_postprocess_fbo();
            self.needs_render = true;
        }
    }

    pub(super) fn zoom_to_fit(&mut self, window: Option<u32>) {
        if let Some(win) = window {
            if self.zoom_to_fit_window == Some(win) {
                self.zoom_to_fit_window = None;
                self.zoom_to_fit_target = 1.0;
            } else {
                self.zoom_to_fit_window = Some(win);
                if let Some(wt) = self.windows.get(&win) {
                    if wt.w > 0 && wt.h > 0 {
                        let sx = self.screen_w as f32 / wt.w as f32;
                        let sy = self.screen_h as f32 / wt.h as f32;
                        self.zoom_to_fit_target = sx.min(sy);
                    }
                }
            }
            self.needs_render = true;
        } else {
            self.zoom_to_fit_window = None;
            self.zoom_to_fit_target = 1.0;
            self.needs_render = true;
        }
    }

    // =====================================================================
    // Phase 7: Diagnostics
    // =====================================================================

    #[allow(dead_code)]
    pub(super) fn reload_shader_from_file(&mut self, name: &str, path: &std::path::Path) -> Result<(), String> {
        let fs_src = std::fs::read_to_string(path)
            .map_err(|e| format!("read shader file: {e}"))?;
        let vs_src = shaders::VERTEX_SHADER;
        match unsafe { Self::create_program(&self.gl, vs_src, &fs_src) } {
            Ok(new_program) => {
                match name {
                    "window" => { unsafe { self.gl.delete_program(self.program); } self.program = new_program; }
                    "shadow" => { unsafe { self.gl.delete_program(self.shadow_program); } self.shadow_program = new_program; }
                    _ => { unsafe { self.gl.delete_program(new_program); } return Err(format!("unknown shader: {name}")); }
                }
                self.needs_render = true;
                Ok(())
            }
            Err(e) => {
                log::warn!("compositor: shader reload failed for {name}: {e}");
                Err(e)
            }
        }
    }

    pub(super) fn start_recording(&mut self, output_path: &str) {
        if self.recording_active { return; }
        let w = self.screen_w;
        let h = self.screen_h;
        let fps = self.recording_fps;

        let stderr_file = std::fs::File::create("/tmp/jwm-ffmpeg.log")
            .unwrap_or_else(|_| std::fs::File::create("/dev/null").unwrap());

        // Select encoder: respect config or auto-probe (NVENC > VAAPI > SW).
        let probe = |args: &[&str]| -> bool {
            std::process::Command::new("ffmpeg")
                .args(args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };

        enum Encoder { Nvenc, Vaapi, Sw }
        let encoder = match self.recording_encoder.as_str() {
            "nvenc" => Encoder::Nvenc,
            "vaapi" => Encoder::Vaapi,
            "software" => Encoder::Sw,
            _ => {
                // auto: probe NVENC > VAAPI > SW
                if probe(&["-f", "lavfi", "-i", "nullsrc=s=64x64", "-frames:v", "1", "-c:v", "h264_nvenc", "-f", "null", "-"]) {
                    Encoder::Nvenc
                } else if std::path::Path::new("/dev/dri/renderD128").exists()
                    && probe(&["-vaapi_device", "/dev/dri/renderD128", "-f", "lavfi", "-i", "nullsrc=s=64x64", "-frames:v", "1", "-f", "null", "-"])
                {
                    Encoder::Vaapi
                } else {
                    Encoder::Sw
                }
            }
        };

        let codec_name = match encoder { Encoder::Nvenc => "h264_nvenc", Encoder::Vaapi => "h264_vaapi", Encoder::Sw => "libopenh264" };
        let bitrate = &self.recording_bitrate;
        let quality_str = self.recording_quality.to_string();
        log::info!("compositor: recording encoder={codec_name}, size={w}x{h}, fps={fps}, bitrate={bitrate}, qp={quality_str}, output={output_path}");

        let size_str = format!("{w}x{h}");
        let fps_str = fps.to_string();
        let mut args: Vec<&str> = Vec::new();

        if matches!(encoder, Encoder::Vaapi) {
            args.extend_from_slice(&["-vaapi_device", "/dev/dri/renderD128"]);
        }
        // Input: use wall clock timestamps so video duration matches real time.
        // The nominal `-r` is moved to the output side; ffmpeg duplicates/drops
        // frames automatically to produce a constant-frame-rate file.
        args.extend_from_slice(&[
            "-use_wallclock_as_timestamps", "1",
            "-f", "rawvideo",
            "-pix_fmt", "rgba",
            "-s", &size_str,
            "-i", "pipe:0",
        ]);
        match encoder {
            Encoder::Nvenc => args.extend_from_slice(&["-vf", "vflip"]),
            Encoder::Vaapi => args.extend_from_slice(&["-vf", "vflip,format=nv12,hwupload"]),
            Encoder::Sw => args.extend_from_slice(&["-vf", "vflip"]),
        }
        args.push("-c:v"); args.push(codec_name);
        match encoder {
            Encoder::Vaapi => args.extend_from_slice(&["-rc_mode", "CQP", "-qp", &quality_str]),
            _ => args.extend_from_slice(&["-b:v", bitrate]),
        }
        args.extend_from_slice(&["-r", &fps_str, "-y", output_path]);

        let child = match std::process::Command::new("ffmpeg")
            .args(&args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(stderr_file)
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                log::warn!("compositor: failed to start ffmpeg: {e}");
                return;
            }
        };

        unsafe {
            for pbo in &mut self.recording_pbo {
                if let Ok(buf) = self.gl.create_buffer() {
                    self.gl.bind_buffer(glow::PIXEL_PACK_BUFFER, Some(buf));
                    self.gl.buffer_data_size(
                        glow::PIXEL_PACK_BUFFER,
                        (w * h * 4) as i32,
                        glow::STREAM_READ,
                    );
                    self.gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
                    *pbo = Some(buf);
                }
            }
        }

        self.recording_process = Some(child);
        self.recording_active = true;
        self.recording_last_frame = None;
        log::info!("compositor: recording started to {output_path}");
    }

    pub(super) fn stop_recording(&mut self) {
        if !self.recording_active { return; }
        self.recording_active = false;

        unsafe {
            for pbo in &mut self.recording_pbo {
                if let Some(buf) = pbo.take() {
                    self.gl.delete_buffer(buf);
                }
            }
        }

        if let Some(mut child) = self.recording_process.take() {
            drop(child.stdin.take());
            let _ = child.wait();
        }
        log::info!("compositor: recording stopped");
    }

    fn capture_recording_frame(&mut self) {
        if !self.recording_active { return; }

        let now = std::time::Instant::now();
        let min_interval = std::time::Duration::from_secs_f32(1.0 / self.recording_fps as f32);
        if let Some(last) = self.recording_last_frame {
            if now.duration_since(last) < min_interval {
                return;
            }
        }
        self.recording_last_frame = Some(now);

        let w = self.screen_w;
        let h = self.screen_h;
        let buf_size = (w * h * 4) as usize;

        // Simple single-buffer approach: read_pixels into PBO, map, write to ffmpeg.
        if let Some(pbo) = self.recording_pbo[0] {
            unsafe {
                self.gl.bind_buffer(glow::PIXEL_PACK_BUFFER, Some(pbo));
                self.gl.read_pixels(
                    0, 0, w as i32, h as i32,
                    glow::RGBA, glow::UNSIGNED_BYTE,
                    glow::PixelPackData::BufferOffset(0),
                );

                let ptr = self.gl.map_buffer_range(
                    glow::PIXEL_PACK_BUFFER,
                    0,
                    buf_size as i32,
                    glow::MAP_READ_BIT,
                );
                if !ptr.is_null() {
                    let pixels = std::slice::from_raw_parts(ptr as *const u8, buf_size);
                    if let Some(ref mut child) = self.recording_process {
                        if let Some(ref mut stdin) = child.stdin {
                            use std::io::Write;
                            if let Err(e) = stdin.write_all(pixels) {
                                log::warn!("compositor: recording write failed: {e}, stopping");
                                self.gl.unmap_buffer(glow::PIXEL_PACK_BUFFER);
                                self.gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
                                self.recording_active = false;
                                return;
                            }
                        }
                    }
                    self.gl.unmap_buffer(glow::PIXEL_PACK_BUFFER);
                } else {
                    log::warn!("compositor: recording PBO map returned null");
                }
                self.gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
            }
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
        e.type_, e.error_code, e.request_code, e.minor_code, e.resourceid
    );
    0
}

