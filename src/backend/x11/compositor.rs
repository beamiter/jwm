use glow::HasContext;
use std::collections::HashMap;
use std::ffi::CString;
use std::sync::Arc;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::wrapper::ConnectionExt as WrapperExt;
use x11rb::protocol::composite::ConnectionExt as CompositeExt;
use x11rb::protocol::damage::{self, ConnectionExt as DamageExt};
use x11rb::protocol::xfixes::ConnectionExt as XFixesExt;
use x11rb::protocol::xproto::{self, ConnectionExt as XProtoExt};
use x11rb::rust_connection::RustConnection;

use super::shaders;

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
    // --- Wobbly state ---
    wobbly: Option<WobblyState>,
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

#[derive(Clone, Copy, PartialEq)]
enum TransitionMode {
    Slide,
    Cube,
}

#[derive(Clone, Copy, PartialEq)]
enum WallpaperMode {
    Fill,
    Fit,
    Stretch,
    Center,
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

/// Frame timing statistics for the debug HUD (feature 11).
struct FrameStats {
    frame_count: u64,
    last_fps_update: std::time::Instant,
    fps: f32,
    frame_times: Vec<f32>,
    last_frame_time: std::time::Instant,
}

/// Per-window wobbly animation state.
struct WobblyState {
    corner_offsets: [[f32; 2]; 4],    // TL, TR, BL, BR displacement
    corner_velocities: [[f32; 2]; 4], // velocity per corner
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
    tilt: Option<glow::UniformLocation>,
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
    corner_offsets: [Option<glow::UniformLocation>; 4],
    grid_size: Option<glow::UniformLocation>,
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
}

/// Particle shader uniform locations.
struct ParticleUniforms {
    projection: Option<glow::UniformLocation>,
    point_size: Option<glow::UniformLocation>,
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
    damage_event_base: u8,
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
    /// Windows pending removal after fade-out completes
    fade_out_pending: Vec<u32>,
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
    damage_regions: Vec<(i32, i32, u32, u32)>,

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

    // --- Frosted glass ---
    frosted_glass_rules: Vec<String>,
    frosted_glass_strength: u32,

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

    // --- Wobbly windows ---
    wobbly_program: glow::Program,
    wobbly_uniforms: WobblyUniforms,
    wobbly_windows: bool,
    wobbly_stiffness: f32,
    wobbly_damping: f32,
    wobbly_grid_size: u32,

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
    wallpaper_img_w: u32,
    wallpaper_img_h: u32,
    /// Per-monitor wallpaper overrides. Populated by set_monitors().
    monitor_wallpapers: Vec<MonitorWallpaper>,
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
            self.gl.delete_program(self.transition_program);
            self.gl.delete_program(self.cube_program);
            self.gl.delete_program(self.edge_glow_program);
            self.gl.delete_program(self.tilt_program);
            self.gl.delete_program(self.wobbly_program);
            self.gl.delete_program(self.overview_bg_program);
            self.gl.delete_program(self.particle_program);
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
            }
        };

        // Compile tilt shader (uses tilt vertex + standard fragment)
        let tilt_program = unsafe { Self::create_program(&gl, shaders::TILT_VERTEX_SHADER, shaders::FRAGMENT_SHADER)? };
        let tilt_uniforms = unsafe {
            TiltUniforms {
                projection: gl.get_uniform_location(tilt_program, "u_projection"),
                rect: gl.get_uniform_location(tilt_program, "u_rect"),
                texture: gl.get_uniform_location(tilt_program, "u_texture"),
                opacity: gl.get_uniform_location(tilt_program, "u_opacity"),
                radius: gl.get_uniform_location(tilt_program, "u_radius"),
                size: gl.get_uniform_location(tilt_program, "u_size"),
                dim: gl.get_uniform_location(tilt_program, "u_dim"),
                tilt: gl.get_uniform_location(tilt_program, "u_tilt"),
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
                corner_offsets: [
                    gl.get_uniform_location(wobbly_program, "u_corner_offsets[0]"),
                    gl.get_uniform_location(wobbly_program, "u_corner_offsets[1]"),
                    gl.get_uniform_location(wobbly_program, "u_corner_offsets[2]"),
                    gl.get_uniform_location(wobbly_program, "u_corner_offsets[3]"),
                ],
                grid_size: gl.get_uniform_location(wobbly_program, "u_grid_size"),
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

        // Load wallpaper texture if configured
        let wallpaper_mode = Self::parse_wallpaper_mode(&behavior.wallpaper_mode);
        let (wallpaper_texture, wallpaper_img_w, wallpaper_img_h) = if !behavior.wallpaper.is_empty() {
            match Self::load_wallpaper_texture(&gl, &behavior.wallpaper, screen_w, screen_h) {
                Some((tex, w, h)) => (Some(tex), w, h),
                None => (None, 0, 0),
            }
        } else {
            (None, 0, 0)
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
            damage_event_base,
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
            fade_in_step: behavior.fade_in_step,
            fade_out_step: behavior.fade_out_step,
            fade_out_pending: Vec::new(),
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
            // Feature 6: damage regions
            damage_regions: Vec::new(),
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
            debug_hud: behavior.debug_hud,
            frame_stats: FrameStats {
                frame_count: 0,
                last_fps_update: std::time::Instant::now(),
                fps: 0.0,
                frame_times: Vec::with_capacity(120),
                last_frame_time: std::time::Instant::now(),
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
            transition_duration: std::time::Duration::from_millis(150),
            transition_direction: 1.0,
            transition_exclude_top: 0,
            transition_mon_x: 0,
            transition_mon_y: 0,
            transition_mon_w: screen_w,
            transition_mon_h: screen_h,
            transition_mode: match behavior.transition_mode.as_str() {
                "cube" => TransitionMode::Cube,
                _ => TransitionMode::Slide,
            },
            // Cube transition
            cube_program,
            cube_uniforms,
            transition_new_fbo: None,
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
            // Frosted glass
            frosted_glass_rules: behavior.frosted_glass_rules.clone(),
            frosted_glass_strength: behavior.frosted_glass_strength,
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
            // Wobbly windows
            wobbly_program,
            wobbly_uniforms,
            wobbly_windows: behavior.wobbly_windows,
            wobbly_stiffness: behavior.wobbly_stiffness,
            wobbly_damping: behavior.wobbly_damping,
            wobbly_grid_size: behavior.wobbly_grid_size,
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
            // Wallpaper
            wallpaper_texture,
            wallpaper_mode,
            wallpaper_img_w,
            wallpaper_img_h,
            monitor_wallpapers: Vec::new(),
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

    /// Load a wallpaper image from disk and upload it as a GL texture.
    /// Returns (texture, image_width, image_height) or None on failure.
    /// If the image is larger than `max_w`x`max_h`, it is downscaled to fit
    /// (preserving aspect ratio) to reduce GPU memory and sampling cost.
    fn load_wallpaper_texture(
        gl: &glow::Context,
        path: &str,
        max_w: u32,
        max_h: u32,
    ) -> Option<(glow::Texture, u32, u32)> {
        let img = match image::open(path) {
            Ok(img) => img,
            Err(e) => {
                log::warn!("compositor: failed to load wallpaper '{}': {}", path, e);
                return None;
            }
        };

        // Downscale to screen dimensions if the image is larger — there is no
        // benefit in keeping pixels beyond the display resolution, and huge
        // textures cause noticeable per-frame slowdowns during sampling.
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
        log::info!("compositor: loaded wallpaper '{}' ({}x{})", path, w, h);

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
                w as i32,
                h as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(rgba.as_raw())),
            );
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
            gl.bind_texture(glow::TEXTURE_2D, None);
            Some((tex, w, h))
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
            let (texture, img_w, img_h) = if !path.is_empty() {
                match Self::load_wallpaper_texture(&self.gl, path, self.screen_w, self.screen_h) {
                    Some((tex, iw, ih)) => (Some(tex), iw, ih),
                    None => (None, 0, 0),
                }
            } else {
                (None, 0, 0)
            };

            self.monitor_wallpapers.push(MonitorWallpaper {
                mon_x: x,
                mon_y: y,
                mon_w: w,
                mon_h: h,
                texture,
                mode,
                img_w,
                img_h,
            });
        }

        self.needs_render = true;
        log::info!(
            "compositor: set_monitors: {} monitors, {} with wallpaper overrides",
            monitors.len(),
            behavior.wallpaper_monitors.len(),
        );
    }

    /// Create the dual Kawase blur FBO mipmap chain.
    /// Each level is half the size of the previous.
    unsafe fn create_blur_fbos(gl: &glow::Context, w: u32, h: u32, levels: u32) -> Vec<BlurFboLevel> {
        let levels = levels.clamp(1, 6);
        let mut fbos = Vec::new();
        let mut cur_w = w / 2;
        let mut cur_h = h / 2;
        unsafe {
            for _ in 0..levels {
                if cur_w == 0 { cur_w = 1; }
                if cur_h == 0 { cur_h = 1; }
                let tex = match gl.create_texture() {
                    Ok(t) => t,
                    Err(_) => break,
                };
                gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                gl.tex_image_2d(
                    glow::TEXTURE_2D, 0, glow::RGBA8 as i32,
                    cur_w as i32, cur_h as i32, 0,
                    glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(None),
                );
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);

                let fbo = match gl.create_framebuffer() {
                    Ok(f) => f,
                    Err(_) => {
                        gl.delete_texture(tex);
                        break;
                    }
                };
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
                gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(tex), 0);

                fbos.push(BlurFboLevel { fbo, texture: tex, w: cur_w, h: cur_h });
                cur_w /= 2;
                cur_h /= 2;
            }
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.bind_texture(glow::TEXTURE_2D, None);
        }
        log::info!("compositor: created {} blur FBO levels", fbos.len());
        fbos
    }

    /// Create the scene capture FBO used as blur source.
    unsafe fn create_scene_fbo(gl: &glow::Context, w: u32, h: u32) -> Result<(glow::Framebuffer, glow::Texture), String> {
        unsafe {
            let tex = gl.create_texture().map_err(|e| format!("scene_fbo tex: {e}"))?;
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            gl.tex_image_2d(
                glow::TEXTURE_2D, 0, glow::RGBA8 as i32,
                w as i32, h as i32, 0,
                glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(None),
            );
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);

            let fbo = gl.create_framebuffer().map_err(|e| format!("scene_fbo: {e}"))?;
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(tex), 0);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.bind_texture(glow::TEXTURE_2D, None);
            Ok((fbo, tex))
        }
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

    pub(super) fn damage_event_base(&self) -> u8 {
        self.damage_event_base
    }

    pub(super) fn needs_render(&self) -> bool {
        if self.needs_render {
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
        // Need render if overview is active
        if self.overview_active { return true; }
        // Need render if particles are active
        if !self.particle_systems.is_empty() { return true; }
        // Need render if any window has active wobbly
        if self.wobbly_windows {
            for wt in self.windows.values() {
                if let Some(ref w) = wt.wobbly {
                    if w.corner_offsets.iter().any(|o| o[0].abs() > 0.1 || o[1].abs() > 0.1) {
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
        // Need render if edge glow is active
        if self.edge_glow { return true; }
        // Need render if window tilt is active
        if self.window_tilt { return true; }
        // Need render if scale animation active
        if self.window_animation {
            for wt in self.windows.values() {
                if (wt.anim_scale - wt.anim_scale_target).abs() > 0.001 { return true; }
            }
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
    // Feature 6: Mark damage region for partial redraw
    // =====================================================================
    pub(super) fn mark_damage_region(&mut self, x: i32, y: i32, w: u32, h: u32) {
        self.damage_regions.push((x, y, w, h));
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

    /// Lazily create postprocess FBO if it doesn't exist yet.
    fn ensure_postprocess_fbo(&mut self) {
        if self.postprocess_fbo.is_none() {
            self.postprocess_fbo = unsafe {
                Self::create_scene_fbo(&self.gl, self.screen_w, self.screen_h).ok()
            };
        }
    }

    /// Whether post-processing is active.
    fn needs_postprocess(&self) -> bool {
        self.color_temperature != 0.0
            || self.saturation != 1.0
            || self.brightness != 1.0
            || self.contrast != 1.0
            || self.invert_colors
            || self.grayscale
            || self.magnifier_enabled
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

        // Create new-scene FBO for cube mode if needed
        if self.transition_mode == TransitionMode::Cube && self.transition_new_fbo.is_none() {
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
            self.damage_regions.clear();
            self.damage_regions
                .push((0, 0, self.screen_w, self.screen_h));
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
        self.damage_regions.clear();
        self.damage_regions.push((0, 0, self.screen_w, self.screen_h));
        self.needs_render = true;
    }

    /// Returns true if a tag-switch transition is in progress.
    fn transition_active(&self) -> bool {
        self.transition_start.is_some()
    }

    /// Compute transition progress (0.0 → 1.0). Returns None if no transition.
    fn transition_progress(&self, now: std::time::Instant) -> Option<f32> {
        let start = self.transition_start?;
        let elapsed = now.duration_since(start);
        if elapsed >= self.transition_duration {
            None // transition complete
        } else {
            let t = elapsed.as_secs_f32() / self.transition_duration.as_secs_f32();
            // EaseOut cubic for smooth slide deceleration.
            let inv = 1.0 - t;
            Some(1.0 - inv * inv * inv)
        }
    }

    /// Render the 3D cube transition overlay.
    /// `progress` goes from 0.0 (old scene fully visible) to 1.0 (new scene fully visible).
    ///
    /// The two tags are adjacent faces of a cube. The cube rotates 90° around
    /// its vertical (Y) axis so the old front face turns away and the new side
    /// face turns in.  During the rotation both faces share an edge that is
    /// visible as a vertical line where the two tag contents meet.
    fn render_cube_transition(&mut self, progress: f32, _ortho_proj: &[f32; 16]) {
        let old_tex = match &self.transition_fbo {
            Some((_, tex)) => *tex,
            None => return,
        };

        let mon_x = self.transition_mon_x;
        let mon_y = self.transition_mon_y;
        let mon_w = self.transition_mon_w;
        let mon_h = self.transition_mon_h;

        // Capture the current back-buffer (new scene) into transition_new_fbo
        if self.transition_new_fbo.is_none() {
            self.transition_new_fbo = unsafe {
                Self::create_scene_fbo(&self.gl, mon_w, mon_h).ok()
            };
        }
        let new_tex = match &self.transition_new_fbo {
            Some((fbo, tex)) => {
                let fbo = *fbo;
                let tex = *tex;
                let blit_gl_y = self.screen_h as i32 - (mon_y + mon_h as i32);
                unsafe {
                    self.gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
                    self.gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(fbo));
                    self.gl.blit_framebuffer(
                        mon_x, blit_gl_y,
                        mon_x + mon_w as i32, blit_gl_y + mon_h as i32,
                        0, 0, mon_w as i32, mon_h as i32,
                        glow::COLOR_BUFFER_BIT,
                        glow::NEAREST,
                    );
                    self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                }
                tex
            }
            None => return,
        };

        let exclude_top = self.transition_exclude_top.min(mon_h);
        let workspace_h = mon_h.saturating_sub(exclude_top);
        if workspace_h == 0 {
            return;
        }

        let aspect = mon_w as f32 / workspace_h as f32;
        let top_frac = if mon_h == 0 {
            0.0
        } else {
            exclude_top as f32 / mon_h as f32
        };
        // UV rect: workspace portion of the FBO texture (below status bar)
        let uv_rect = [0.0f32, 0.0, 1.0, 1.0 - top_frac];

        // --- Cube geometry ---
        // The face quad spans [-aspect, -1] to [+aspect, +1] in model space
        // (vertex shader: (pos * 2 - 1) * aspect for X, (pos * 2 - 1) for Y).
        // For a square cross-section (cube viewed from above), the half-depth
        // from center to each face equals the face half-width = aspect.
        let d = aspect;

        // Camera distance: face exactly fills screen when face-on at z=d,
        // fov_y=90° ⟹ camera_z = 1 + d.  Zoom out slightly at the midpoint
        // to keep the rotating cube corners within the viewport.
        let half_pi = std::f32::consts::FRAC_PI_2;
        let zoom = 1.0 + 0.25 * (progress * std::f32::consts::PI).sin();
        let camera_z = (1.0 + d) * zoom;

        let persp = perspective_matrix(half_pi, aspect, 0.1, camera_z * 3.0);
        let view = translate_matrix(0.0, 0.0, -camera_z);

        // Global rotation applied to the whole cube as a rigid body.
        // direction=+1 (forward): positive Y-rotation moves the front face
        //   left and brings the right face to the front.
        // direction=-1 (backward): vice-versa.
        let angle = self.transition_direction * progress * half_pi;
        let cube_rot = rotate_y_matrix(angle);

        // Old face: front face of the cube, at z = +d
        let old_model = mat4_mul(&cube_rot, &translate_matrix(0.0, 0.0, d));
        let old_mvp = mat4_mul(&persp, &mat4_mul(&view, &old_model));

        // New face: adjacent side face.  Start from the face template at z=+d,
        // then rotate it ∓90° so it sits on the appropriate side of the cube.
        // For direction=+1 the new face sits at x=+d (right side);
        // for direction=-1 it sits at x=-d (left side).
        let new_base = mat4_mul(
            &rotate_y_matrix(-self.transition_direction * half_pi),
            &translate_matrix(0.0, 0.0, d),
        );
        let new_model = mat4_mul(&cube_rot, &new_base);
        let new_mvp = mat4_mul(&persp, &mat4_mul(&view, &new_model));

        // Simulate directional lighting: the face that points more towards the
        // camera is brighter, the one turning away is dimmer.
        let old_brightness = (0.35 + 0.65 * (progress * half_pi).cos()).max(0.0);
        let new_brightness = (0.35 + 0.65 * (progress * half_pi).sin()).max(0.0);

        // OpenGL Y for the monitor's workspace region
        let scissor_gl_y = self.screen_h as i32 - (mon_y + mon_h as i32);
        unsafe {
            // Restrict rendering to the monitor's workspace area (below status bar)
            self.gl.enable(glow::SCISSOR_TEST);
            self.gl.scissor(mon_x, scissor_gl_y, mon_w as i32, workspace_h as i32);
            self.gl.viewport(mon_x, scissor_gl_y, mon_w as i32, workspace_h as i32);

            // Clear workspace area for cube rendering
            self.gl.clear(glow::COLOR_BUFFER_BIT);

            self.gl.use_program(Some(self.cube_program));
            self.gl.uniform_1_f32(self.cube_uniforms.aspect.as_ref(), aspect);
            self.gl.uniform_1_i32(self.cube_uniforms.texture.as_ref(), 0);
            self.gl.uniform_4_f32(
                self.cube_uniforms.uv_rect.as_ref(),
                uv_rect[0], uv_rect[1], uv_rect[2], uv_rect[3],
            );
            self.gl.bind_vertex_array(Some(self.quad_vao));
            self.gl.active_texture(glow::TEXTURE0);

            // Painter's algorithm: draw the farther face first so the closer
            // face correctly occludes it.  At progress < 0.5 the old face is
            // closer; at progress > 0.5 the new face is closer.
            if progress < 0.5 {
                // New face farther → draw first
                self.gl.uniform_matrix_4_f32_slice(self.cube_uniforms.mvp.as_ref(), false, &new_mvp);
                self.gl.uniform_1_f32(self.cube_uniforms.brightness.as_ref(), new_brightness);
                self.gl.bind_texture(glow::TEXTURE_2D, Some(new_tex));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

                // Old face closer → draw second
                self.gl.uniform_matrix_4_f32_slice(self.cube_uniforms.mvp.as_ref(), false, &old_mvp);
                self.gl.uniform_1_f32(self.cube_uniforms.brightness.as_ref(), old_brightness);
                self.gl.bind_texture(glow::TEXTURE_2D, Some(old_tex));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            } else {
                // Old face farther → draw first
                self.gl.uniform_matrix_4_f32_slice(self.cube_uniforms.mvp.as_ref(), false, &old_mvp);
                self.gl.uniform_1_f32(self.cube_uniforms.brightness.as_ref(), old_brightness);
                self.gl.bind_texture(glow::TEXTURE_2D, Some(old_tex));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

                // New face closer → draw second
                self.gl.uniform_matrix_4_f32_slice(self.cube_uniforms.mvp.as_ref(), false, &new_mvp);
                self.gl.uniform_1_f32(self.cube_uniforms.brightness.as_ref(), new_brightness);
                self.gl.bind_texture(glow::TEXTURE_2D, Some(new_tex));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            }

            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);

            // Restore viewport and disable scissor
            self.gl.disable(glow::SCISSOR_TEST);
            self.gl.viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
        }
    }

    // =====================================================================
    // Feature 11: Debug HUD toggle
    // =====================================================================
    pub(super) fn set_transition_mode(&mut self, mode: &str) {
        let new_mode = match mode {
            "cube" => TransitionMode::Cube,
            _ => TransitionMode::Slide,
        };
        self.transition_mode = new_mode;
    }

    pub(super) fn set_debug_hud(&mut self, enabled: bool) {
        self.debug_hud = enabled;
        self.needs_render = true;
    }

    pub(super) fn debug_hud_enabled(&self) -> bool {
        self.debug_hud
    }

    pub(super) fn frame_stats_fps(&self) -> f32 {
        self.frame_stats.fps
    }

    // =====================================================================
    // Feature 12: Screenshot
    // =====================================================================
    pub(super) fn request_screenshot(&mut self, path: std::path::PathBuf) {
        self.pending_screenshot = Some(path);
        self.needs_render = true;
    }

    /// Capture the current framebuffer to a PNG file.
    fn capture_screenshot(&mut self, path: &std::path::Path) -> bool {
        let w = self.screen_w;
        let h = self.screen_h;
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        unsafe {
            self.gl.read_pixels(
                0, 0, w as i32, h as i32,
                glow::RGBA, glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut pixels)),
            );
        }
        // OpenGL reads bottom-to-top, flip vertically
        let row_bytes = (w * 4) as usize;
        let mut flipped = vec![0u8; pixels.len()];
        for y in 0..h as usize {
            let src_row = (h as usize - 1 - y) * row_bytes;
            let dst_row = y * row_bytes;
            flipped[dst_row..dst_row + row_bytes].copy_from_slice(&pixels[src_row..src_row + row_bytes]);
        }
        // Write PNG
        let file = match std::fs::File::create(path) {
            Ok(f) => f,
            Err(e) => {
                log::warn!("compositor: screenshot create failed: {e}");
                return false;
            }
        };
        let writer = std::io::BufWriter::new(file);
        let mut encoder = png::Encoder::new(writer, w, h);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        match encoder.write_header().and_then(|mut w| w.write_image_data(&flipped)) {
            Ok(_) => {
                log::info!("compositor: screenshot saved to {}", path.display());
                true
            }
            Err(e) => {
                log::warn!("compositor: screenshot encode failed: {e}");
                false
            }
        }
    }

    // =====================================================================
    // Feature 7: Window thumbnail rendering
    // =====================================================================
    /// Render a specific window to an off-screen FBO and return RGBA pixel data.
    /// Returns None if the window isn't tracked. Dimensions are (width, height).
    pub(super) fn capture_window_thumbnail(&self, x11_win: u32, max_size: u32) -> Option<(Vec<u8>, u32, u32)> {
        let wt = self.windows.get(&x11_win)?;
        if wt.w == 0 || wt.h == 0 {
            return None;
        }

        // Calculate thumbnail size preserving aspect ratio
        let aspect = wt.w as f32 / wt.h as f32;
        let (tw, th) = if wt.w >= wt.h {
            let tw = max_size.min(wt.w);
            (tw, (tw as f32 / aspect) as u32)
        } else {
            let th = max_size.min(wt.h);
            ((th as f32 * aspect) as u32, th)
        };
        let tw = tw.max(1);
        let th = th.max(1);

        unsafe {
            // Create temp FBO
            let tex = self.gl.create_texture().ok()?;
            self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            self.gl.tex_image_2d(
                glow::TEXTURE_2D, 0, glow::RGBA8 as i32,
                tw as i32, th as i32, 0,
                glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(None),
            );
            self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
            self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
            let fbo = self.gl.create_framebuffer().ok()?;
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            self.gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(tex), 0);

            self.gl.viewport(0, 0, tw as i32, th as i32);
            self.gl.clear_color(0.0, 0.0, 0.0, 0.0);
            self.gl.clear(glow::COLOR_BUFFER_BIT);

            let proj = ortho(0.0, tw as f32, th as f32, 0.0, -1.0, 1.0);
            self.gl.use_program(Some(self.program));
            self.gl.uniform_matrix_4_f32_slice(self.win_uniforms.projection.as_ref(), false, &proj);
            self.gl.uniform_1_i32(self.win_uniforms.texture.as_ref(), 0);
            self.gl.uniform_1_f32(self.win_uniforms.opacity.as_ref(), 1.0);
            self.gl.uniform_1_f32(self.win_uniforms.radius.as_ref(), 0.0);
            self.gl.uniform_1_f32(self.win_uniforms.dim.as_ref(), 1.0);
            self.gl.uniform_2_f32(self.win_uniforms.size.as_ref(), tw as f32, th as f32);
            self.gl.uniform_4_f32(self.win_uniforms.rect.as_ref(), 0.0, 0.0, tw as f32, th as f32);
            self.gl.bind_vertex_array(Some(self.quad_vao));
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(wt.gl_texture));
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

            // Read pixels
            let mut pixels = vec![0u8; (tw * th * 4) as usize];
            self.gl.read_pixels(
                0, 0, tw as i32, th as i32,
                glow::RGBA, glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut pixels)),
            );

            // Cleanup temp FBO
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            self.gl.delete_framebuffer(fbo);
            self.gl.delete_texture(tex);
            self.gl.viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);

            Some((pixels, tw, th))
        }
    }

    fn clear_overview_snapshots(&mut self) {
        unsafe {
            for entry in &mut self.overview_windows {
                if let Some(texture) = entry.snapshot_texture.take() {
                    self.gl.delete_texture(texture);
                }
            }
        }
    }

    fn clear_overview_title_textures(&mut self) {
        unsafe {
            for entry in &mut self.overview_windows {
                if let Some((texture, _, _)) = entry.title_texture.take() {
                    self.gl.delete_texture(texture);
                }
            }
        }
    }

    /// Render a title string into an RGBA pixel buffer using a simple embedded bitmap font.
    /// Returns (pixels, width, height) or None if the title is empty.
    fn render_title_to_pixels(title: &str, max_width: u32) -> Option<(Vec<u8>, u32, u32)> {
        if title.is_empty() {
            return None;
        }

        // Simple 6x10 bitmap font for printable ASCII (32..=126).
        // Each character is 6 columns x 10 rows, stored as 10 bytes (1 byte per row,
        // top 6 bits used). Non-ASCII characters are rendered as '?'.
        #[rustfmt::skip]
        const FONT_6X10: [u8; 95 * 10] = [
            // Space (32)
            0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,
            // ! (33)
            0x20,0x20,0x20,0x20,0x20,0x20,0x00,0x20,0x00,0x00,
            // " (34)
            0x50,0x50,0x50,0x00,0x00,0x00,0x00,0x00,0x00,0x00,
            // # (35)
            0x50,0x50,0xF8,0x50,0xF8,0x50,0x50,0x00,0x00,0x00,
            // $ (36)
            0x20,0x78,0xA0,0x70,0x28,0xF0,0x20,0x00,0x00,0x00,
            // % (37)
            0xC0,0xC8,0x10,0x20,0x40,0x98,0x18,0x00,0x00,0x00,
            // & (38)
            0x60,0x90,0xA0,0x40,0xA8,0x90,0x68,0x00,0x00,0x00,
            // ' (39)
            0x60,0x20,0x40,0x00,0x00,0x00,0x00,0x00,0x00,0x00,
            // ( (40)
            0x10,0x20,0x40,0x40,0x40,0x20,0x10,0x00,0x00,0x00,
            // ) (41)
            0x40,0x20,0x10,0x10,0x10,0x20,0x40,0x00,0x00,0x00,
            // * (42)
            0x00,0x20,0xA8,0x70,0xA8,0x20,0x00,0x00,0x00,0x00,
            // + (43)
            0x00,0x20,0x20,0xF8,0x20,0x20,0x00,0x00,0x00,0x00,
            // , (44)
            0x00,0x00,0x00,0x00,0x00,0x60,0x20,0x40,0x00,0x00,
            // - (45)
            0x00,0x00,0x00,0xF8,0x00,0x00,0x00,0x00,0x00,0x00,
            // . (46)
            0x00,0x00,0x00,0x00,0x00,0x60,0x60,0x00,0x00,0x00,
            // / (47)
            0x00,0x08,0x10,0x20,0x40,0x80,0x00,0x00,0x00,0x00,
            // 0 (48)
            0x70,0x88,0x98,0xA8,0xC8,0x88,0x70,0x00,0x00,0x00,
            // 1 (49)
            0x20,0x60,0x20,0x20,0x20,0x20,0x70,0x00,0x00,0x00,
            // 2 (50)
            0x70,0x88,0x08,0x10,0x20,0x40,0xF8,0x00,0x00,0x00,
            // 3 (51)
            0xF8,0x10,0x20,0x10,0x08,0x88,0x70,0x00,0x00,0x00,
            // 4 (52)
            0x10,0x30,0x50,0x90,0xF8,0x10,0x10,0x00,0x00,0x00,
            // 5 (53)
            0xF8,0x80,0xF0,0x08,0x08,0x88,0x70,0x00,0x00,0x00,
            // 6 (54)
            0x30,0x40,0x80,0xF0,0x88,0x88,0x70,0x00,0x00,0x00,
            // 7 (55)
            0xF8,0x08,0x10,0x20,0x40,0x40,0x40,0x00,0x00,0x00,
            // 8 (56)
            0x70,0x88,0x88,0x70,0x88,0x88,0x70,0x00,0x00,0x00,
            // 9 (57)
            0x70,0x88,0x88,0x78,0x08,0x10,0x60,0x00,0x00,0x00,
            // : (58)
            0x00,0x60,0x60,0x00,0x60,0x60,0x00,0x00,0x00,0x00,
            // ; (59)
            0x00,0x60,0x60,0x00,0x60,0x20,0x40,0x00,0x00,0x00,
            // < (60)
            0x10,0x20,0x40,0x80,0x40,0x20,0x10,0x00,0x00,0x00,
            // = (61)
            0x00,0x00,0xF8,0x00,0xF8,0x00,0x00,0x00,0x00,0x00,
            // > (62)
            0x40,0x20,0x10,0x08,0x10,0x20,0x40,0x00,0x00,0x00,
            // ? (63)
            0x70,0x88,0x08,0x10,0x20,0x00,0x20,0x00,0x00,0x00,
            // @ (64)
            0x70,0x88,0xB8,0xA8,0xB8,0x80,0x70,0x00,0x00,0x00,
            // A (65)
            0x70,0x88,0x88,0xF8,0x88,0x88,0x88,0x00,0x00,0x00,
            // B (66)
            0xF0,0x88,0x88,0xF0,0x88,0x88,0xF0,0x00,0x00,0x00,
            // C (67)
            0x70,0x88,0x80,0x80,0x80,0x88,0x70,0x00,0x00,0x00,
            // D (68)
            0xE0,0x90,0x88,0x88,0x88,0x90,0xE0,0x00,0x00,0x00,
            // E (69)
            0xF8,0x80,0x80,0xF0,0x80,0x80,0xF8,0x00,0x00,0x00,
            // F (70)
            0xF8,0x80,0x80,0xF0,0x80,0x80,0x80,0x00,0x00,0x00,
            // G (71)
            0x70,0x88,0x80,0xB8,0x88,0x88,0x78,0x00,0x00,0x00,
            // H (72)
            0x88,0x88,0x88,0xF8,0x88,0x88,0x88,0x00,0x00,0x00,
            // I (73)
            0x70,0x20,0x20,0x20,0x20,0x20,0x70,0x00,0x00,0x00,
            // J (74)
            0x38,0x10,0x10,0x10,0x10,0x90,0x60,0x00,0x00,0x00,
            // K (75)
            0x88,0x90,0xA0,0xC0,0xA0,0x90,0x88,0x00,0x00,0x00,
            // L (76)
            0x80,0x80,0x80,0x80,0x80,0x80,0xF8,0x00,0x00,0x00,
            // M (77)
            0x88,0xD8,0xA8,0xA8,0x88,0x88,0x88,0x00,0x00,0x00,
            // N (78)
            0x88,0xC8,0xA8,0x98,0x88,0x88,0x88,0x00,0x00,0x00,
            // O (79)
            0x70,0x88,0x88,0x88,0x88,0x88,0x70,0x00,0x00,0x00,
            // P (80)
            0xF0,0x88,0x88,0xF0,0x80,0x80,0x80,0x00,0x00,0x00,
            // Q (81)
            0x70,0x88,0x88,0x88,0xA8,0x90,0x68,0x00,0x00,0x00,
            // R (82)
            0xF0,0x88,0x88,0xF0,0xA0,0x90,0x88,0x00,0x00,0x00,
            // S (83)
            0x78,0x80,0x80,0x70,0x08,0x08,0xF0,0x00,0x00,0x00,
            // T (84)
            0xF8,0x20,0x20,0x20,0x20,0x20,0x20,0x00,0x00,0x00,
            // U (85)
            0x88,0x88,0x88,0x88,0x88,0x88,0x70,0x00,0x00,0x00,
            // V (86)
            0x88,0x88,0x88,0x88,0x88,0x50,0x20,0x00,0x00,0x00,
            // W (87)
            0x88,0x88,0x88,0xA8,0xA8,0xD8,0x88,0x00,0x00,0x00,
            // X (88)
            0x88,0x88,0x50,0x20,0x50,0x88,0x88,0x00,0x00,0x00,
            // Y (89)
            0x88,0x88,0x50,0x20,0x20,0x20,0x20,0x00,0x00,0x00,
            // Z (90)
            0xF8,0x08,0x10,0x20,0x40,0x80,0xF8,0x00,0x00,0x00,
            // [ (91)
            0x70,0x40,0x40,0x40,0x40,0x40,0x70,0x00,0x00,0x00,
            // \ (92)
            0x00,0x80,0x40,0x20,0x10,0x08,0x00,0x00,0x00,0x00,
            // ] (93)
            0x70,0x10,0x10,0x10,0x10,0x10,0x70,0x00,0x00,0x00,
            // ^ (94)
            0x20,0x50,0x88,0x00,0x00,0x00,0x00,0x00,0x00,0x00,
            // _ (95)
            0x00,0x00,0x00,0x00,0x00,0x00,0xF8,0x00,0x00,0x00,
            // ` (96)
            0x40,0x20,0x10,0x00,0x00,0x00,0x00,0x00,0x00,0x00,
            // a (97)
            0x00,0x00,0x70,0x08,0x78,0x88,0x78,0x00,0x00,0x00,
            // b (98)
            0x80,0x80,0xB0,0xC8,0x88,0x88,0xF0,0x00,0x00,0x00,
            // c (99)
            0x00,0x00,0x70,0x80,0x80,0x88,0x70,0x00,0x00,0x00,
            // d (100)
            0x08,0x08,0x68,0x98,0x88,0x88,0x78,0x00,0x00,0x00,
            // e (101)
            0x00,0x00,0x70,0x88,0xF8,0x80,0x70,0x00,0x00,0x00,
            // f (102)
            0x30,0x48,0x40,0xE0,0x40,0x40,0x40,0x00,0x00,0x00,
            // g (103)
            0x00,0x00,0x78,0x88,0x78,0x08,0x70,0x00,0x00,0x00,
            // h (104)
            0x80,0x80,0xB0,0xC8,0x88,0x88,0x88,0x00,0x00,0x00,
            // i (105)
            0x20,0x00,0x60,0x20,0x20,0x20,0x70,0x00,0x00,0x00,
            // j (106)
            0x10,0x00,0x30,0x10,0x10,0x90,0x60,0x00,0x00,0x00,
            // k (107)
            0x80,0x80,0x90,0xA0,0xC0,0xA0,0x90,0x00,0x00,0x00,
            // l (108)
            0x60,0x20,0x20,0x20,0x20,0x20,0x70,0x00,0x00,0x00,
            // m (109)
            0x00,0x00,0xD0,0xA8,0xA8,0x88,0x88,0x00,0x00,0x00,
            // n (110)
            0x00,0x00,0xB0,0xC8,0x88,0x88,0x88,0x00,0x00,0x00,
            // o (111)
            0x00,0x00,0x70,0x88,0x88,0x88,0x70,0x00,0x00,0x00,
            // p (112)
            0x00,0x00,0xF0,0x88,0xF0,0x80,0x80,0x00,0x00,0x00,
            // q (113)
            0x00,0x00,0x68,0x98,0x78,0x08,0x08,0x00,0x00,0x00,
            // r (114)
            0x00,0x00,0xB0,0xC8,0x80,0x80,0x80,0x00,0x00,0x00,
            // s (115)
            0x00,0x00,0x78,0x80,0x70,0x08,0xF0,0x00,0x00,0x00,
            // t (116)
            0x40,0x40,0xE0,0x40,0x40,0x48,0x30,0x00,0x00,0x00,
            // u (117)
            0x00,0x00,0x88,0x88,0x88,0x98,0x68,0x00,0x00,0x00,
            // v (118)
            0x00,0x00,0x88,0x88,0x88,0x50,0x20,0x00,0x00,0x00,
            // w (119)
            0x00,0x00,0x88,0x88,0xA8,0xA8,0x50,0x00,0x00,0x00,
            // x (120)
            0x00,0x00,0x88,0x50,0x20,0x50,0x88,0x00,0x00,0x00,
            // y (121)
            0x00,0x00,0x88,0x88,0x78,0x08,0x70,0x00,0x00,0x00,
            // z (122)
            0x00,0x00,0xF8,0x10,0x20,0x40,0xF8,0x00,0x00,0x00,
            // { (123)
            0x10,0x20,0x20,0x40,0x20,0x20,0x10,0x00,0x00,0x00,
            // | (124)
            0x20,0x20,0x20,0x20,0x20,0x20,0x20,0x00,0x00,0x00,
            // } (125)
            0x40,0x20,0x20,0x10,0x20,0x20,0x40,0x00,0x00,0x00,
            // ~ (126)
            0x00,0x40,0xA8,0x10,0x00,0x00,0x00,0x00,0x00,0x00,
        ];

        const GLYPH_W: u32 = 6;
        const GLYPH_H: u32 = 10;
        const SCALE: u32 = 2; // render at 2x for readability
        const CHAR_W: u32 = GLYPH_W * SCALE;
        const CHAR_H: u32 = GLYPH_H * SCALE;
        const PAD_X: u32 = 8;  // horizontal padding
        const PAD_Y: u32 = 4;  // vertical padding

        // Truncate to fit max_width
        let max_chars = ((max_width.saturating_sub(PAD_X * 2)) / CHAR_W) as usize;
        if max_chars == 0 {
            return None;
        }

        let display_title: String = title
            .chars()
            .take(max_chars)
            .map(|c| if c.is_ascii_graphic() || c == ' ' { c } else { '?' })
            .collect();

        let text_w = display_title.len() as u32 * CHAR_W;
        let img_w = text_w + PAD_X * 2;
        let img_h = CHAR_H + PAD_Y * 2;
        let mut pixels = vec![0u8; (img_w * img_h * 4) as usize];

        // Draw semi-transparent dark background (rounded pill shape)
        for py in 0..img_h {
            for px in 0..img_w {
                let idx = ((py * img_w + px) * 4) as usize;
                // Simple rounded rect: check if inside pill shape
                let radius = (img_h / 2) as f32;
                let cx = px as f32;
                let cy = py as f32;
                let inside = if cx < radius {
                    let dx = radius - cx;
                    let dy = cy - radius;
                    dx * dx + dy * dy <= radius * radius
                } else if cx > (img_w as f32 - radius) {
                    let dx = cx - (img_w as f32 - radius);
                    let dy = cy - radius;
                    dx * dx + dy * dy <= radius * radius
                } else {
                    true
                };
                if inside {
                    pixels[idx] = 15;     // R
                    pixels[idx + 1] = 15; // G
                    pixels[idx + 2] = 20; // B
                    pixels[idx + 3] = 200; // A (semi-transparent dark)
                }
            }
        }

        // Draw text glyphs
        for (ci, ch) in display_title.chars().enumerate() {
            let glyph_idx = if (32..=126).contains(&(ch as u32)) {
                (ch as u32 - 32) as usize
            } else {
                ('?' as u32 - 32) as usize
            };
            let glyph = &FONT_6X10[glyph_idx * 10..(glyph_idx + 1) * 10];

            let base_x = PAD_X + ci as u32 * CHAR_W;
            let base_y = PAD_Y;

            for row in 0..GLYPH_H {
                let bits = glyph[row as usize];
                for col in 0..GLYPH_W {
                    if bits & (0x80 >> col) != 0 {
                        // Draw scaled pixel
                        for sy in 0..SCALE {
                            for sx in 0..SCALE {
                                let px = base_x + col * SCALE + sx;
                                let py = base_y + row * SCALE + sy;
                                if px < img_w && py < img_h {
                                    let idx = ((py * img_w + px) * 4) as usize;
                                    pixels[idx] = 240;     // R
                                    pixels[idx + 1] = 240; // G
                                    pixels[idx + 2] = 245; // B
                                    pixels[idx + 3] = 255; // A
                                }
                            }
                        }
                    }
                }
            }
        }

        Some((pixels, img_w, img_h))
    }

    fn create_overview_title_textures(&mut self) {
        let entries: Vec<(String, f32)> = self
            .overview_windows
            .iter()
            .map(|e| (e.title.clone(), e.target_w))
            .collect();

        let textures: Vec<Option<(glow::Texture, u32, u32)>> = entries
            .iter()
            .map(|(title, target_w)| {
                let max_w = (*target_w as u32).max(120);
                let (pixels, w, h) = Self::render_title_to_pixels(title, max_w)?;
                unsafe {
                    let tex = self.gl.create_texture().ok()?;
                    self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                    self.gl.tex_image_2d(
                        glow::TEXTURE_2D, 0, glow::RGBA8 as i32,
                        w as i32, h as i32, 0,
                        glow::RGBA, glow::UNSIGNED_BYTE,
                        glow::PixelUnpackData::Slice(Some(&pixels)),
                    );
                    self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
                    self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
                    self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
                    self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
                    self.gl.bind_texture(glow::TEXTURE_2D, None);
                    Some((tex, w, h))
                }
            })
            .collect();

        for (entry, title_tex) in self.overview_windows.iter_mut().zip(textures.into_iter()) {
            entry.title_texture = title_tex;
        }
    }

    fn upload_overview_snapshot_texture(
        &self,
        pixels: &[u8],
        width: u32,
        height: u32,
    ) -> Option<glow::Texture> {
        unsafe {
            let texture = self.gl.create_texture().ok()?;
            self.gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            self.gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                width as i32,
                height as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(pixels)),
            );
            self.gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                glow::LINEAR as i32,
            );
            self.gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MAG_FILTER,
                glow::LINEAR as i32,
            );
            self.gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_S,
                glow::CLAMP_TO_EDGE as i32,
            );
            self.gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_T,
                glow::CLAMP_TO_EDGE as i32,
            );
            self.gl.bind_texture(glow::TEXTURE_2D, None);
            Some(texture)
        }
    }

    fn create_overview_snapshot_texture(
        &self,
        x11_win: u32,
        max_size: u32,
    ) -> Option<glow::Texture> {
        let (pixels, width, height) = self.capture_window_thumbnail(x11_win, max_size)?;
        let row_bytes = (width * 4) as usize;
        let mut flipped = vec![0u8; pixels.len()];
        for y in 0..height as usize {
            let src_row = (height as usize - 1 - y) * row_bytes;
            let dst_row = y * row_bytes;
            flipped[dst_row..dst_row + row_bytes]
                .copy_from_slice(&pixels[src_row..src_row + row_bytes]);
        }
        self.upload_overview_snapshot_texture(&flipped, width, height)
    }

    fn refresh_overview_snapshots(&mut self) {
        self.clear_overview_snapshots();

        let requests: Vec<(u32, u32)> = self
            .overview_windows
            .iter()
            .map(|entry| {
                let desired = (entry.target_w.max(entry.target_h) * 2.0).ceil() as u32;
                let max_size = desired.clamp(256, 1024);
                (entry.x11_win, max_size)
            })
            .collect();

        let snapshots: Vec<Option<glow::Texture>> = requests
            .into_iter()
            .map(|(x11_win, max_size)| self.create_overview_snapshot_texture(x11_win, max_size))
            .collect();

        for (entry, snapshot_texture) in self.overview_windows.iter_mut().zip(snapshots.into_iter()) {
            entry.snapshot_texture = snapshot_texture;
        }
    }

    // =====================================================================
    // Feature 13: Set frame extents for blur mask
    // =====================================================================
    pub(super) fn set_frame_extents(&mut self, x11_win: u32, left: u32, right: u32, top: u32, bottom: u32) {
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            wt.frame_extents = [left, right, top, bottom];
        }
    }

    // =====================================================================
    // Feature 14: Set shaped window
    // =====================================================================
    pub(super) fn set_window_shaped(&mut self, x11_win: u32, shaped: bool) {
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            wt.is_shaped = shaped;
        }
    }

    // ----- Window management -----

    pub(super) fn add_window(&mut self, x11_win: u32, x: i32, y: i32, w: u32, h: u32) {
        if self.windows.contains_key(&x11_win) {
            return;
        }
        if w == 0 || h == 0 {
            return;
        }
        log::info!(
            "compositor: add_window START 0x{:x} {}x{} at ({},{})",
            x11_win, w, h, x, y
        );

        // Create damage
        let damage_id = match self.conn.generate_id() {
            Ok(id) => id,
            Err(e) => {
                log::warn!("compositor: generate_id for damage failed: {e}");
                return;
            }
        };
        if let Err(e) = self
            .conn
            .damage_create(damage_id, x11_win, damage::ReportLevel::NON_EMPTY)
        {
            log::warn!("compositor: damage_create failed for 0x{x11_win:x}: {e}");
            return;
        }

        // NameWindowPixmap
        let pixmap = match self.conn.generate_id() {
            Ok(id) => id,
            Err(e) => {
                log::warn!("compositor: generate_id for pixmap failed: {e}");
                let _ = self.conn.damage_destroy(damage_id);
                return;
            }
        };
        if let Err(e) = self.conn.composite_name_window_pixmap(x11_win, pixmap) {
            log::warn!("compositor: name_window_pixmap failed for 0x{x11_win:x}: {e}");
            let _ = self.conn.damage_destroy(damage_id);
            return;
        }
        // Flush x11rb AND sync Xlib so the pixmap XID is visible to GLX.
        let _ = self.conn.flush();

        // Select the TFP FBConfig for this window.  First try an exact match
        // by visual ID (required on older Mesa, e.g. Ubuntu 20); fall back to
        // the generic depth-based selection.
        let win_visual = self
            .conn
            .get_window_attributes(x11_win)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|a| a.visual)
            .unwrap_or(0);
        let win_depth = self
            .conn
            .get_geometry(x11_win)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|g| g.depth)
            .unwrap_or(24);

        let (fbconfig, use_rgba) = if let Some(&(cfg, is_rgba)) =
            self.tfp_visual_configs.get(&win_visual)
        {
            log::debug!(
                "compositor: win 0x{:x} visual 0x{:x} -> per-visual FBConfig (rgba={})",
                x11_win, win_visual, is_rgba
            );
            (cfg, is_rgba)
        } else {
            // Fallback: depth-based selection
            let rgba = win_depth == 32 && !self.fbconfig_rgba.is_null();
            let cfg = if rgba {
                self.fbconfig_rgba
            } else {
                self.fbconfig_rgb
            };
            log::debug!(
                "compositor: win 0x{:x} visual 0x{:x} depth={} -> depth-based FBConfig (rgba={})",
                x11_win, win_visual, win_depth, rgba
            );
            (cfg, rgba)
        };
        if fbconfig.is_null() {
            log::warn!(
                "compositor: no fbconfig for visual=0x{:x} depth={} win=0x{:x}",
                win_visual, win_depth, x11_win
            );
            let _ = self.conn.free_pixmap(pixmap);
            let _ = self.conn.damage_destroy(damage_id);
            return;
        }
        let tex_fmt = if use_rgba {
            GLX_TEXTURE_FORMAT_RGBA_EXT
        } else {
            GLX_TEXTURE_FORMAT_RGB_EXT
        };

        // Create GLX pixmap for TFP
        let pixmap_attrs: Vec<i32> = vec![
            GLX_TEXTURE_TARGET_EXT,
            GLX_TEXTURE_2D_EXT,
            GLX_TEXTURE_FORMAT_EXT,
            tex_fmt,
            0,
        ];

        log::info!(
            "compositor: add_window 0x{:x} depth={} rgba={} pixmap=0x{:x}, calling glXCreatePixmap...",
            x11_win, win_depth, use_rgba, pixmap
        );
        let glx_pixmap = unsafe {
            // Sync both connections so the Xlib display can see the pixmap
            // created by x11rb.
            x11::xlib::XSync(self.xlib_display, 0);

            x11::glx::glXCreatePixmap(
                self.xlib_display,
                fbconfig,
                pixmap as _,
                pixmap_attrs.as_ptr(),
            )
        };
        log::info!("compositor: glXCreatePixmap returned 0x{:x}", glx_pixmap);
        if glx_pixmap == 0 {
            log::warn!("compositor: glXCreatePixmap failed for 0x{x11_win:x}");
            let _ = self.conn.free_pixmap(pixmap);
            let _ = self.conn.damage_destroy(damage_id);
            return;
        }

        // Create GL texture
        let gl_texture = unsafe {
            match self.gl.create_texture() {
                Ok(t) => t,
                Err(e) => {
                    log::warn!("compositor: create_texture failed: {e}");
                    x11::glx::glXDestroyPixmap(self.xlib_display, glx_pixmap);
                    let _ = self.conn.free_pixmap(pixmap);
                    let _ = self.conn.damage_destroy(damage_id);
                    return;
                }
            }
        };

        // Bind texture
        log::info!("compositor: add_window 0x{:x} binding TFP texture...", x11_win);
        unsafe {
            self.gl.bind_texture(glow::TEXTURE_2D, Some(gl_texture));
            self.gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                glow::NEAREST as i32,
            );
            self.gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MAG_FILTER,
                glow::NEAREST as i32,
            );
            self.gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_S,
                glow::CLAMP_TO_EDGE as i32,
            );
            self.gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_T,
                glow::CLAMP_TO_EDGE as i32,
            );
            (self.tfp.bind)(
                self.xlib_display,
                glx_pixmap,
                GLX_FRONT_LEFT_EXT,
                std::ptr::null(),
            );
            self.gl.bind_texture(glow::TEXTURE_2D, None);
        }
        log::info!("compositor: add_window 0x{:x} COMPLETE", x11_win);

        // Start with fade opacity = 0 if fading is enabled (will fade in)
        let initial_fade = if self.fading { 0.0 } else { 1.0 };

        self.windows.insert(
            x11_win,
            WindowTexture {
                x,
                y,
                w,
                h,
                damage: damage_id,
                pixmap,
                glx_pixmap,
                gl_texture,
                dirty: true,
                has_rgba: use_rgba,
                fbconfig,
                needs_pixmap_refresh: false,
                x11_win,
                fade_opacity: initial_fade,
                fading_out: false,
                class_name: String::new(),
                opacity_override: None,
                is_fullscreen: false,
                corner_radius_override: None,
                scale: 1.0,
                frame_extents: [0; 4],
                is_shaped: false,
                anim_scale: if self.window_animation { self.window_animation_scale } else { 1.0 },
                anim_scale_target: 1.0,
                is_urgent: false,
                is_pip: false,
                is_frosted: false,
                wobbly: None,
            },
        );
        self.needs_render = true;

        log::debug!(
            "compositor: add_window 0x{:x} {}x{} at ({},{})",
            x11_win,
            w,
            h,
            x,
            y
        );
    }

    /// Update the compositor's screen dimensions (e.g. after a RandR hotplug).
    /// The overlay window is resized automatically by the X server, but we need
    /// to update our GL viewport and projection matrix dimensions.
    pub(super) fn resize(&mut self, new_w: u32, new_h: u32) {
        if new_w == self.screen_w && new_h == self.screen_h {
            return;
        }
        log::info!(
            "compositor: resize {}x{} -> {}x{}",
            self.screen_w, self.screen_h, new_w, new_h
        );
        self.screen_w = new_w;
        self.screen_h = new_h;
        self.needs_render = true;

        // Recreate blur FBOs for new screen size
        if self.blur_enabled {
            unsafe {
                for level in self.blur_fbos.drain(..) {
                    self.gl.delete_framebuffer(level.fbo);
                    self.gl.delete_texture(level.texture);
                }
                self.blur_fbos = Self::create_blur_fbos(&self.gl, new_w, new_h, self.blur_strength);
                if let Some((fbo, tex)) = self.scene_fbo.take() {
                    self.gl.delete_framebuffer(fbo);
                    self.gl.delete_texture(tex);
                }
                self.scene_fbo = Self::create_scene_fbo(&self.gl, new_w, new_h).ok();
            }
        }
        // Recreate postprocess FBO
        if self.postprocess_fbo.is_some() {
            unsafe {
                if let Some((fbo, tex)) = self.postprocess_fbo.take() {
                    self.gl.delete_framebuffer(fbo);
                    self.gl.delete_texture(tex);
                }
                self.postprocess_fbo = Self::create_scene_fbo(&self.gl, new_w, new_h).ok();
            }
        }
        // Cancel in-progress transition on resize (screen geometry changed)
        if let Some((fbo, tex)) = self.transition_fbo.take() {
            unsafe {
                self.gl.delete_framebuffer(fbo);
                self.gl.delete_texture(tex);
            }
            self.transition_start = None;
        }
    }

    pub(super) fn remove_window(&mut self, x11_win: u32) {
        // Spawn particles for closing window
        if self.particle_effects {
            if let Some(wt) = self.windows.get(&x11_win) {
                self.spawn_particles_for_window(wt.x, wt.y, wt.w, wt.h);
            }
        }

        // If fading is enabled and the window exists, start fade-out instead of immediate remove
        if self.fading {
            if let Some(wt) = self.windows.get_mut(&x11_win) {
                if !wt.fading_out && wt.fade_opacity > 0.0 {
                    wt.fading_out = true;
                    wt.anim_scale_target = self.window_animation_scale;
                    self.needs_render = true;
                    return;
                }
            }
        }

        self.remove_window_immediate(x11_win);
    }

    /// Actually remove a window (no fade). Used internally.
    fn remove_window_immediate(&mut self, x11_win: u32) {
        let Some(wt) = self.windows.remove(&x11_win) else {
            return;
        };
        self.needs_render = true;
        // Undo fullscreen unredirect if this was the unredirected window
        if self.unredirected_window == Some(x11_win) {
            self.unredirected_window = None;
        }

        unsafe {
            self.gl.bind_texture(glow::TEXTURE_2D, Some(wt.gl_texture));
            (self.tfp.release)(self.xlib_display, wt.glx_pixmap, GLX_FRONT_LEFT_EXT);
            self.gl.bind_texture(glow::TEXTURE_2D, None);
            self.gl.delete_texture(wt.gl_texture);
            x11::glx::glXDestroyPixmap(self.xlib_display, wt.glx_pixmap);
        }
        let _ = self.conn.free_pixmap(wt.pixmap);
        let _ = self.conn.damage_destroy(wt.damage);

        log::debug!("compositor: remove_window 0x{:x}", x11_win);
    }

    pub(super) fn update_geometry(&mut self, x11_win: u32, x: i32, y: i32, w: u32, h: u32) {
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            let size_changed = wt.w != w || wt.h != h;
            let moved = wt.x != x || wt.y != y;
            wt.x = x;
            wt.y = y;
            self.needs_render = true;

            if moved {
                // Window move exposes old screen area and occupies new area.
                // Damage events are not always sufficient for both regions,
                // so request a full-frame redraw to prevent trails/ghosting.
                self.damage_regions.clear();
                self.damage_regions
                    .push((0, 0, self.screen_w, self.screen_h));
            }

            if size_changed && w > 0 && h > 0 {
                wt.w = w;
                wt.h = h;
                // Defer the heavy pixmap recreation to the next render_frame()
                // call, so multiple resize events within a single frame are batched.
                wt.needs_pixmap_refresh = true;
            }
        }
    }

    /// Recreate GLX pixmaps for windows that had their size changed.
    /// Called once per frame in render_frame() to batch all pending recreations.
    fn refresh_pixmaps(&mut self) {
        // Collect window IDs that need refresh to avoid borrowing issues
        let refresh_wins: Vec<u32> = self
            .windows
            .iter()
            .filter(|(_, wt)| wt.needs_pixmap_refresh)
            .map(|(&id, _)| id)
            .collect();

        if refresh_wins.is_empty() {
            return;
        }

        // Release old pixmaps for all windows that need refresh
        for &win in &refresh_wins {
            let wt = self.windows.get(&win).unwrap();
            unsafe {
                self.gl.bind_texture(glow::TEXTURE_2D, Some(wt.gl_texture));
                (self.tfp.release)(self.xlib_display, wt.glx_pixmap, GLX_FRONT_LEFT_EXT);
                self.gl.bind_texture(glow::TEXTURE_2D, None);
                x11::glx::glXDestroyPixmap(self.xlib_display, wt.glx_pixmap);
            }
            let _ = self.conn.free_pixmap(wt.pixmap);
        }

        // Create new named pixmaps for all windows via x11rb
        let mut new_pixmaps: Vec<(u32, u32)> = Vec::new(); // (win, pixmap)
        for &win in &refresh_wins {
            let wt = self.windows.get_mut(&win).unwrap();
            let pixmap = match self.conn.generate_id() {
                Ok(id) => id,
                Err(_) => {
                    wt.glx_pixmap = 0;
                    wt.pixmap = 0;
                    wt.needs_pixmap_refresh = false;
                    continue;
                }
            };
            if self
                .conn
                .composite_name_window_pixmap(wt.x11_win, pixmap)
                .is_err()
            {
                wt.glx_pixmap = 0;
                wt.pixmap = 0;
                wt.needs_pixmap_refresh = false;
                continue;
            }
            wt.pixmap = pixmap;
            new_pixmaps.push((win, pixmap));
        }

        // Single flush + sync for the entire batch
        let _ = self.conn.flush();
        unsafe {
            x11::xlib::XSync(self.xlib_display, 0);
        }

        // Create GLX pixmaps and rebind textures
        for (win, pixmap) in new_pixmaps {
            let wt = self.windows.get_mut(&win).unwrap();
            let fbconfig = wt.fbconfig;
            let tex_fmt = if wt.has_rgba {
                GLX_TEXTURE_FORMAT_RGBA_EXT
            } else {
                GLX_TEXTURE_FORMAT_RGB_EXT
            };
            let pixmap_attrs: Vec<i32> = vec![
                GLX_TEXTURE_TARGET_EXT,
                GLX_TEXTURE_2D_EXT,
                GLX_TEXTURE_FORMAT_EXT,
                tex_fmt,
                0,
            ];
            let glx_pixmap = unsafe {
                x11::glx::glXCreatePixmap(
                    self.xlib_display,
                    fbconfig,
                    pixmap as _,
                    pixmap_attrs.as_ptr(),
                )
            };
            if glx_pixmap == 0 {
                let _ = self.conn.free_pixmap(pixmap);
                wt.pixmap = 0;
                wt.glx_pixmap = 0;
                wt.needs_pixmap_refresh = false;
                continue;
            }

            unsafe {
                self.gl.bind_texture(glow::TEXTURE_2D, Some(wt.gl_texture));
                (self.tfp.bind)(
                    self.xlib_display,
                    glx_pixmap,
                    GLX_FRONT_LEFT_EXT,
                    std::ptr::null(),
                );
                self.gl.bind_texture(glow::TEXTURE_2D, None);
            }

            wt.glx_pixmap = glx_pixmap;
            wt.dirty = true;
            wt.needs_pixmap_refresh = false;
        }

        // Clear flag for any remaining windows (error paths above)
        for &win in &refresh_wins {
            if let Some(wt) = self.windows.get_mut(&win) {
                wt.needs_pixmap_refresh = false;
            }
        }
    }

    pub(super) fn mark_damaged(&mut self, x11_win: u32) {
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            wt.dirty = true;
            self.needs_render = true;
            // Subtract damage so we get future notifications
            let _ = self.conn.damage_subtract(wt.damage, 0u32, 0u32);
        }
    }

    /// Set the window class name (for per-window rules).
    pub(super) fn set_window_class(&mut self, x11_win: u32, class_name: &str) {
        // Look up per-window rules before borrowing windows mutably
        let opacity_override = self.lookup_opacity_rule(class_name);
        let corner_radius_override = self.lookup_corner_radius_rule(class_name);
        let scale = self.lookup_scale_rule(class_name);
        let is_frosted = self.lookup_frosted_glass_rule(class_name);
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            if wt.class_name != class_name {
                wt.class_name = class_name.to_string();
                wt.opacity_override = opacity_override;
                wt.corner_radius_override = corner_radius_override;
                wt.is_frosted = is_frosted;
                if let Some(s) = scale {
                    wt.scale = s;
                }
                self.needs_render = true;
            }
        }
    }

    /// Set/unset fullscreen state for a window (for fullscreen unredirect).
    pub(super) fn set_window_fullscreen(&mut self, x11_win: u32, fullscreen: bool) {
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            if wt.is_fullscreen != fullscreen {
                wt.is_fullscreen = fullscreen;
                self.needs_render = true;
            }
        }
    }

    /// Advance fade animations. Returns true if any fades are still in progress.
    fn tick_fades(&mut self) -> bool {
        let mut any_active = false;
        let mut to_remove = Vec::new();

        for (&win, wt) in self.windows.iter_mut() {
            // Fade animation
            if self.fading {
                if wt.fading_out {
                    wt.fade_opacity -= self.fade_out_step;
                    if wt.fade_opacity <= 0.0 {
                        wt.fade_opacity = 0.0;
                        to_remove.push(win);
                    } else {
                        any_active = true;
                    }
                } else if wt.fade_opacity < 1.0 {
                    wt.fade_opacity += self.fade_in_step;
                    if wt.fade_opacity >= 1.0 {
                        wt.fade_opacity = 1.0;
                    } else {
                        any_active = true;
                    }
                }
            }

            // Scale animation (window open/close zoom)
            if self.window_animation {
                let diff = wt.anim_scale_target - wt.anim_scale;
                if diff.abs() > 0.001 {
                    let step = if diff > 0.0 { self.fade_in_step } else { -self.fade_out_step };
                    wt.anim_scale += step;
                    if (wt.anim_scale_target - wt.anim_scale).abs() < 0.001
                        || (step > 0.0 && wt.anim_scale >= wt.anim_scale_target)
                        || (step < 0.0 && wt.anim_scale <= wt.anim_scale_target)
                    {
                        wt.anim_scale = wt.anim_scale_target;
                    } else {
                        any_active = true;
                    }
                }
            }
        }

        for win in to_remove {
            self.remove_window_immediate(win);
        }

        any_active
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

        // Skip-unchanged-frame: if scene hasn't changed and no textures are
        // dirty, we can skip the entire GL render (unless screenshot pending or HUD active).
        let has_dirty = scene.iter().any(|&(win, _, _, _, _)| {
            self.windows.get(&win).map_or(false, |wt| wt.dirty || wt.needs_pixmap_refresh)
        });
        let force_render = self.pending_screenshot.is_some() || self.debug_hud || self.transition_active() || self.overview_active;
        let hash = Self::scene_hash(scene, focused);
        if !has_dirty && !fades_active && !force_render && hash == self.last_scene_hash {
            return false;
        }
        self.last_scene_hash = hash;

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

        // Feature 6: Apply scissor test for partial redraw if damage regions available
        let use_scissor = !self.damage_regions.is_empty() && !force_render;
        let mut damage_scissor = (0i32, 0i32, self.screen_w as i32, self.screen_h as i32);
        if use_scissor {
            unsafe {
                self.gl.enable(glow::SCISSOR_TEST);
                // Compute bounding box of all damage regions
                let mut min_x = self.screen_w as i32;
                let mut min_y = self.screen_h as i32;
                let mut max_x = 0i32;
                let mut max_y = 0i32;
                for &(x, y, w, h) in &self.damage_regions {
                    min_x = min_x.min(x);
                    min_y = min_y.min(y);
                    max_x = max_x.max(x + w as i32);
                    max_y = max_y.max(y + h as i32);
                }
                // GL scissor uses bottom-left origin
                let gl_y = self.screen_h as i32 - max_y;
                damage_scissor = (min_x, gl_y, max_x - min_x, max_y - min_y);
                self.gl.scissor(damage_scissor.0, damage_scissor.1, damage_scissor.2, damage_scissor.3);
            }
        }
        self.damage_regions.clear();

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
                    let sx = cx + ox - spread;
                    let sy = cy + sy_offset - spread;
                    let sw = win_w + 2.0 * spread;
                    let sh = win_h + 2.0 * spread + bottom_extra;
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

        // === Pass 1.5: Background blur (now computed per-window in Pass 2) ===
        let blur_available = self.blur_enabled
            && !self.blur_fbos.is_empty()
            && self.scene_fbo.is_some();

        // === Pass 2: Draw window textures ===
        unsafe {
            self.gl.use_program(Some(self.program));
            self.gl.uniform_matrix_4_f32_slice(
                self.win_uniforms.projection.as_ref(), false, &proj,
            );
            self.gl.uniform_1_i32(self.win_uniforms.texture.as_ref(), 0);
            self.gl.bind_vertex_array(Some(self.quad_vao));

            for &(win, x, y, w, h) in visible_scene {
                if let Some(wt) = self.windows.get(&win) {
                    let is_focused = focused == Some(win);
                    let fade = wt.fade_opacity;
                    if fade <= 0.0 { continue; }

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

                    // Feature 4: Apply per-window scale
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

                    // Feature 13: Draw blurred background behind translucent windows (with frame extents mask)
                    // Blur is captured per-window so it includes all windows drawn below.
                    if blur_available {
                        if self.needs_backdrop_blur(wt) {
                            // Temporarily break out of the window shader to run blur passes.
                            // Capture the current framebuffer (which includes all windows
                            // drawn so far) and produce a blurred texture from it.
                            self.gl.bind_vertex_array(None);
                            self.gl.use_program(None);
                            if use_scissor {
                                self.gl.disable(glow::SCISSOR_TEST);
                            }

                            let blur_tex = self.run_blur_passes_from_fbo(
                                if postprocess_active {
                                    self.postprocess_fbo.as_ref().map(|(fbo, _)| *fbo)
                                } else {
                                    None
                                },
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
                            self.gl.bind_vertex_array(Some(self.quad_vao));
                            self.gl.uniform_1_f32(self.win_uniforms.radius.as_ref(), radius);

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
                                self.gl.uniform_1_f32(self.win_uniforms.opacity.as_ref(), fade);
                                self.gl.uniform_1_f32(self.win_uniforms.dim.as_ref(), 1.0);
                                self.gl.uniform_2_f32(self.win_uniforms.size.as_ref(), bw, bh);
                                self.gl.uniform_4_f32(
                                    self.win_uniforms.rect.as_ref(), bx, by, bw, bh,
                                );
                                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                            }
                        }
                    }

                    self.gl.uniform_1_f32(self.win_uniforms.opacity.as_ref(), opacity);
                    self.gl.uniform_1_f32(self.win_uniforms.dim.as_ref(), dim);
                    self.gl.uniform_2_f32(
                        self.win_uniforms.size.as_ref(), draw_w, draw_h,
                    );
                    self.gl.uniform_4_f32(
                        self.win_uniforms.rect.as_ref(), draw_x, draw_y, draw_w, draw_h,
                    );
                    self.gl.active_texture(glow::TEXTURE0);
                    self.gl.bind_texture(glow::TEXTURE_2D, Some(wt.gl_texture));
                    self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                }
            }

            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
        }

        // === Pass 3: Window borders (feature 1) ===
        if self.border_enabled && self.border_width > 0.0 {
            unsafe {
                self.gl.use_program(Some(self.border_program));
                self.gl.uniform_matrix_4_f32_slice(
                    self.border_uniforms.projection.as_ref(), false, &proj,
                );
                self.gl.bind_vertex_array(Some(self.quad_vao));

                for &(win, x, y, w, h) in visible_scene {
                    let wt = match self.windows.get(&win) {
                        Some(wt) => wt,
                        None => continue,
                    };
                    let fade = wt.fade_opacity;
                    if fade <= 0.0 { continue; }

                    let is_focused = focused == Some(win);
                    let color = if wt.is_urgent && self.attention_animation {
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

                    let bw = if wt.is_urgent && self.attention_animation {
                        self.border_width.max(2.0)
                    } else if wt.is_pip {
                        self.pip_border_width
                    } else {
                        self.border_width
                    };

                    // Per-window corner radius (feature 3)
                    let radius = wt.corner_radius_override.unwrap_or(
                        if Self::class_matches_exclude(&wt.class_name, &self.rounded_corners_exclude) {
                            0.0
                        } else {
                            self.corner_radius
                        }
                    );

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

                    self.gl.uniform_1_f32(
                        self.border_uniforms.border_width.as_ref(), bw,
                    );
                    self.gl.uniform_4_f32(
                        self.border_uniforms.border_color.as_ref(),
                        color[0], color[1], color[2], color[3] * fade,
                    );
                    self.gl.uniform_1_f32(
                        self.border_uniforms.radius.as_ref(), radius,
                    );
                    self.gl.uniform_2_f32(
                        self.border_uniforms.size.as_ref(), draw_w, draw_h,
                    );
                    self.gl.uniform_4_f32(
                        self.border_uniforms.rect.as_ref(), draw_x, draw_y, draw_w, draw_h,
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

                        self.gl.uniform_1_f32(self.border_uniforms.border_width.as_ref(), bw);
                        self.gl.uniform_4_f32(
                            self.border_uniforms.border_color.as_ref(),
                            color[0], color[1], color[2], color[3] * fade,
                        );
                        self.gl.uniform_1_f32(self.border_uniforms.radius.as_ref(), radius);
                        self.gl.uniform_2_f32(self.border_uniforms.size.as_ref(), draw_w, draw_h);
                        self.gl.uniform_4_f32(
                            self.border_uniforms.rect.as_ref(), draw_x, draw_y, draw_w, draw_h,
                        );
                        self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                    }

                    self.gl.bind_vertex_array(None);
                    self.gl.use_program(None);
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
                    self.gl.uniform_2_f32(self.magnifier_uniforms.magnifier_center.as_ref(), cx, cy);
                    self.gl.uniform_1_f32(self.magnifier_uniforms.magnifier_radius.as_ref(), self.magnifier_radius / self.screen_w as f32);
                    self.gl.uniform_1_f32(self.magnifier_uniforms.magnifier_zoom.as_ref(), self.magnifier_zoom);
                }

                self.gl.active_texture(glow::TEXTURE0);
                self.gl.bind_texture(glow::TEXTURE_2D, Some(pp_tex));
                self.gl.bind_vertex_array(Some(self.quad_vao));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

                self.gl.bind_vertex_array(None);
                self.gl.use_program(None);
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

            // Draw HUD panel
            let hud_w = 160.0f32;
            let hud_h = 40.0f32;
            let hud_x = self.screen_w as f32 - hud_w - 10.0;
            let hud_y = 10.0f32;

            unsafe {
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
                self.gl.bind_vertex_array(None);
                self.gl.use_program(None);
            }
            // FPS logged instead of rendered as text (GL text rendering is complex)
            if self.frame_stats.frame_count % 60 == 0 {
                let avg_dt = if self.frame_stats.frame_times.is_empty() { 0.0 }
                    else { self.frame_stats.frame_times.iter().sum::<f32>() / self.frame_stats.frame_times.len() as f32 };
                log::info!(
                    "[HUD] FPS: {:.1}, frame_time: {:.2}ms, windows: {}",
                    self.frame_stats.fps, avg_dt * 1000.0, self.windows.len()
                );
            }
        }

        // === Pass 5b: Screen edge glow ===
        if self.edge_glow && self.edge_glow_width > 0.0 {
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
                    // --- Slide mode: old fades out in place, new slides in ---
                    if let Some((_, snap_tex)) = &self.transition_fbo {
                        let snap_tex = *snap_tex;

                        // Capture the current back-buffer (new scene) into transition_new_fbo
                        if self.transition_new_fbo.is_none() {
                            self.transition_new_fbo = unsafe {
                                Self::create_scene_fbo(&self.gl, mon_w, mon_h).ok()
                            };
                        }

                        if let Some((new_fbo, new_tex)) = &self.transition_new_fbo {
                            let new_fbo = *new_fbo;
                            let new_tex = *new_tex;

                            // OpenGL Y for monitor region
                            let blit_gl_y = self.screen_h as i32 - (mon_y + mon_h as i32);
                            unsafe {
                                // Blit only the monitor region from back-buffer into new FBO
                                self.gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
                                self.gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(new_fbo));
                                self.gl.blit_framebuffer(
                                    mon_x, blit_gl_y,
                                    mon_x + mon_w as i32, blit_gl_y + mon_h as i32,
                                    0, 0, mon_w as i32, mon_h as i32,
                                    glow::COLOR_BUFFER_BIT,
                                    glow::NEAREST,
                                );
                                self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);

                                if draw_h > 0.0 {
                                    // Scissor to the monitor's workspace area (below status bar)
                                    self.gl.enable(glow::SCISSOR_TEST);
                                    self.gl.scissor(
                                        mon_x,
                                        scissor_gl_y,
                                        mon_w as i32,
                                        (mon_h - exclude_top) as i32,
                                    );
                                    self.gl.clear_color(0.0, 0.0, 0.0, 1.0);
                                    self.gl.clear(glow::COLOR_BUFFER_BIT);

                                    // Use SRC_ALPHA blending for correct fade
                                    self.gl.blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);

                                    self.gl.use_program(Some(self.transition_program));
                                    self.gl.uniform_matrix_4_f32_slice(
                                        self.transition_uniforms.projection.as_ref(), false, &proj,
                                    );
                                    self.gl.uniform_1_i32(self.transition_uniforms.texture.as_ref(), 0);
                                    self.gl.active_texture(glow::TEXTURE0);

                                    let uv = [0.0f32, 0.0, 1.0, 1.0 - top_frac];

                                    // 1) Draw new scene sliding in (within monitor bounds)
                                    let slide_x_new = self.transition_direction
                                        * (mon_w as f32)
                                        * (1.0 - progress);
                                    self.gl.uniform_4_f32(
                                        self.transition_uniforms.rect.as_ref(),
                                        draw_x + slide_x_new, draw_y, mon_w as f32, draw_h,
                                    );
                                    self.gl.uniform_1_f32(
                                        self.transition_uniforms.opacity.as_ref(), 1.0,
                                    );
                                    self.gl.uniform_4_f32(
                                        self.transition_uniforms.uv_rect.as_ref(),
                                        uv[0], uv[1], uv[2], uv[3],
                                    );
                                    self.gl.bind_texture(glow::TEXTURE_2D, Some(new_tex));
                                    self.gl.bind_vertex_array(Some(self.quad_vao));
                                    self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

                                    // 2) Draw old scene fading out in place
                                    let fade_opacity = 1.0 - progress;
                                    self.gl.uniform_4_f32(
                                        self.transition_uniforms.rect.as_ref(),
                                        draw_x, draw_y, mon_w as f32, draw_h,
                                    );
                                    self.gl.uniform_1_f32(
                                        self.transition_uniforms.opacity.as_ref(), fade_opacity,
                                    );
                                    self.gl.uniform_4_f32(
                                        self.transition_uniforms.uv_rect.as_ref(),
                                        uv[0], uv[1], uv[2], uv[3],
                                    );
                                    self.gl.bind_texture(glow::TEXTURE_2D, Some(snap_tex));
                                    self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

                                    self.gl.bind_vertex_array(None);
                                    self.gl.use_program(None);

                                    // Restore premultiplied alpha blending and disable scissor
                                    self.gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
                                    self.gl.disable(glow::SCISSOR_TEST);
                                }
                            }
                        }
                    }
                }
                TransitionMode::Cube => {
                    // --- Cube mode: 3D rotating cube transition ---
                    self.render_cube_transition(progress, &proj);
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

        // Schedule re-render if fades or transition are still in progress
        if fades_active || transition_still_active || wobbly_active || !self.particle_systems.is_empty() || self.overview_active {
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
        if self.edge_glow || self.magnifier_enabled || self.window_tilt {
            self.needs_render = true;
        }
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
        self.clear_overview_snapshots();
        self.clear_overview_title_textures();
        self.overview_active = active;
        let n = windows.len();
        let face_w = self.screen_w as f32 * 0.667;
        let face_h = self.screen_h as f32 * 0.667;
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
        }
        self.overview_opacity = if active { 1.0 } else { 0.0 };
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
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            wt.wobbly = Some(WobblyState {
                corner_offsets: [[0.0; 2]; 4],
                corner_velocities: [[0.0; 2]; 4],
            });
        }
    }

    pub(super) fn notify_window_move_delta(&mut self, x11_win: u32, dx: f32, dy: f32) {
        if self.wobbly_windows {
            if let Some(wt) = self.windows.get_mut(&x11_win) {
                if let Some(ref mut w) = wt.wobbly {
                    // Displace non-pinned corners (pin top-left, displace others)
                    // All corners get displaced, but top-left (corner 0) returns faster
                    for i in 1..4 {
                        w.corner_offsets[i][0] -= dx * 0.5;
                        w.corner_offsets[i][1] -= dy * 0.5;
                    }
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
            self.damage_regions.clear();
            self.damage_regions.push((0, 0, self.screen_w, self.screen_h));
        }
        self.needs_render = true;
    }

    pub(super) fn notify_window_move_end(&mut self, x11_win: u32) {
        // Don't clear wobbly state - let it settle naturally
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            if let Some(ref mut w) = wt.wobbly {
                // All corners spring back to zero
                for i in 0..4 {
                    // Keep the offsets, they'll spring back in tick_wobbly
                    let _ = w.corner_velocities[i];
                }
            }
        }
    }

    /// Tick wobbly spring physics. Returns true if any wobbly is active.
    fn tick_wobbly(&mut self) -> bool {
        if !self.wobbly_windows { return false; }
        let dt = 1.0 / 60.0; // approximate frame time
        let stiffness = self.wobbly_stiffness;
        let damping = self.wobbly_damping;
        let mut any_active = false;
        let mut to_clear = Vec::new();

        for (&win, wt) in self.windows.iter_mut() {
            if let Some(ref mut w) = wt.wobbly {
                let mut all_settled = true;
                for i in 0..4 {
                    for axis in 0..2 {
                        let offset = w.corner_offsets[i][axis];
                        let vel = w.corner_velocities[i][axis];
                        let accel = -stiffness * offset - damping * vel;
                        let new_vel = vel + accel * dt;
                        let new_offset = offset + new_vel * dt;
                        w.corner_offsets[i][axis] = new_offset;
                        w.corner_velocities[i][axis] = new_vel;
                        if new_offset.abs() > 0.1 || new_vel.abs() > 0.1 {
                            all_settled = false;
                        }
                    }
                }
                if all_settled {
                    to_clear.push(win);
                } else {
                    any_active = true;
                }
            }
        }

        for win in to_clear {
            if let Some(wt) = self.windows.get_mut(&win) {
                wt.wobbly = None;
            }
        }

        any_active
    }

    /// Tick particle systems. Removes dead particles and empty systems.
    fn tick_particles(&mut self) {
        let dt = 1.0 / 60.0;
        let gravity = self.particle_gravity;

        self.particle_systems.retain_mut(|sys| {
            sys.particles.retain_mut(|p| {
                p.vy += gravity * dt;
                p.x += p.vx * dt;
                p.y += p.vy * dt;
                p.lifetime -= dt;
                p.lifetime > 0.0
            });
            !sys.particles.is_empty()
        });
    }

    /// Render active particle systems.
    fn render_particles(&self, proj: &[f32; 16]) {
        if self.particle_systems.is_empty() { return; }

        // Collect all particles into a flat buffer
        let mut data: Vec<f32> = Vec::new();
        let mut count = 0u32;
        for sys in &self.particle_systems {
            for p in &sys.particles {
                let life_frac = (p.lifetime / p.max_lifetime).clamp(0.0, 1.0);
                data.extend_from_slice(&[p.x, p.y, p.color[0], p.color[1], p.color[2], p.color[3], life_frac]);
                count += 1;
            }
        }

        if count == 0 { return; }

        unsafe {
            self.gl.use_program(Some(self.particle_program));
            self.gl.uniform_matrix_4_f32_slice(
                self.particle_uniforms.projection.as_ref(), false, proj,
            );
            self.gl.uniform_1_f32(self.particle_uniforms.point_size.as_ref(), 4.0);

            self.gl.enable(glow::PROGRAM_POINT_SIZE);
            self.gl.bind_vertex_array(Some(self.particle_vao));
            self.gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.particle_vbo));

            let byte_data: &[u8] = std::slice::from_raw_parts(
                data.as_ptr() as *const u8,
                data.len() * 4,
            );
            self.gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, byte_data, glow::DYNAMIC_DRAW);
            self.gl.draw_arrays(glow::POINTS, 0, count as i32);

            self.gl.disable(glow::PROGRAM_POINT_SIZE);
            self.gl.bind_buffer(glow::ARRAY_BUFFER, None);
            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
        }
    }

    /// Tick the overview prism rotation animation (exponential ease-out).
    fn tick_overview_prism(&mut self) {
        let now = std::time::Instant::now();
        let dt = if let Some(last) = self.overview_prism_last_tick {
            now.duration_since(last).as_secs_f32().min(0.1)
        } else {
            1.0 / 60.0
        };
        self.overview_prism_last_tick = Some(now);

        let diff = self.overview_prism_target_angle - self.overview_prism_current_angle;
        if diff.abs() < 0.001 {
            self.overview_prism_current_angle = self.overview_prism_target_angle;
        } else {
            // Exponential ease-out: close 88% of the remaining gap per 0.1s.
            // t = 1 - e^(-speed * dt), speed=20 gives ~86% per 0.1s frame,
            // feels snappy at any frame rate.
            let t = 1.0 - (-20.0_f32 * dt).exp();
            self.overview_prism_current_angle += diff * t;
            self.needs_render = true;
        }
    }

    /// Render overview overlay (Alt-Tab preview) as a 3D hexagonal prism carousel.
    /// Rendering is confined to the monitor that owns the overview.
    fn render_overview(&self, proj: &[f32; 16], _focused: Option<u32>) {
        if self.overview_windows.is_empty() { return; }

        // Monitor-local dimensions
        let mon_x = self.overview_mon_x;
        let mon_y = self.overview_mon_y;
        let mon_w = self.overview_mon_w;
        let mon_h = self.overview_mon_h;
        let mw = mon_w as f32;
        let mh = mon_h as f32;

        unsafe {
            // === 1. Dark background overlay (only on this monitor) ===
            self.gl.use_program(Some(self.overview_bg_program));
            self.gl.uniform_matrix_4_f32_slice(
                self.overview_bg_uniforms.projection.as_ref(), false, proj,
            );
            self.gl.uniform_4_f32(
                self.overview_bg_uniforms.rect.as_ref(),
                mon_x as f32, mon_y as f32, mw, mh,
            );
            self.gl.uniform_1_f32(self.overview_bg_uniforms.opacity.as_ref(), self.overview_opacity);
            self.gl.bind_vertex_array(Some(self.quad_vao));
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

            // === 2. Scissor + viewport to this monitor for the 3D prism ===
            let scissor_gl_y = self.screen_h as i32 - (mon_y + mon_h as i32);
            self.gl.enable(glow::SCISSOR_TEST);
            self.gl.scissor(mon_x, scissor_gl_y, mon_w as i32, mon_h as i32);
            self.gl.viewport(mon_x, scissor_gl_y, mon_w as i32, mon_h as i32);

            // === 3. Compute hexagonal prism geometry ===
            let face_w = mw * 0.667;
            let face_h = mh * 0.667;
            let face_aspect = face_w / face_h;
            let apothem = face_aspect * 3.0_f32.sqrt();

            let fov_y = std::f32::consts::FRAC_PI_4; // 45 degrees
            let camera_z = (apothem + 1.0 / (fov_y * 0.5).tan()) * 1.6;
            let mon_aspect = mw / mh;

            let persp = perspective_matrix(fov_y, mon_aspect, 0.1, camera_z * 4.0);
            // Tilt the prism ~12° so top edges lean away from the viewer,
            // giving a stronger 3D depth cue.
            let tilt_angle = -0.21;
            let view = mat4_mul(
                &translate_matrix(0.0, 0.0, -camera_z),
                &rotate_x_matrix(tilt_angle),
            );

            let global_rot = rotate_y_matrix(self.overview_prism_current_angle);

            // === 4. Build per-face draw info ===
            struct FaceDrawInfo {
                mvp: [f32; 16],
                z_depth: f32,
                brightness: f32,
                entry_idx: usize,
            }

            let pi_over_3 = std::f32::consts::FRAC_PI_3;
            let mut faces: Vec<FaceDrawInfo> = Vec::new();

            for (idx, entry) in self.overview_windows.iter().enumerate() {
                let face_i = entry.face_index;
                let face_angle = face_i as f32 * pi_over_3;

                let face_rot = rotate_y_matrix(face_angle);
                let face_translate = translate_matrix(0.0, 0.0, apothem);
                let face_model = mat4_mul(&face_rot, &face_translate);

                let model = mat4_mul(&global_rot, &face_model);
                let mv = mat4_mul(&view, &model);
                let mvp = mat4_mul(&persp, &mv);

                let z_depth = mv[14];

                let total_angle = face_angle + self.overview_prism_current_angle;
                let cos_facing = total_angle.cos();
                let brightness = if entry.is_selected {
                    0.45 + 0.55 * cos_facing.max(0.0)
                } else {
                    0.30 + 0.50 * cos_facing.max(0.0)
                };

                faces.push(FaceDrawInfo {
                    mvp,
                    z_depth,
                    brightness,
                    entry_idx: idx,
                });
            }

            // === 5. Painter's algorithm: sort by Z-depth ascending (farthest first) ===
            faces.sort_by(|a, b| a.z_depth.partial_cmp(&b.z_depth).unwrap_or(std::cmp::Ordering::Equal));

            // === 6. Draw faces using cube_program ===
            self.gl.use_program(Some(self.cube_program));
            self.gl.uniform_1_f32(self.cube_uniforms.aspect.as_ref(), face_aspect);
            self.gl.uniform_1_i32(self.cube_uniforms.texture.as_ref(), 0);
            // Snapshot textures have top-left origin (X11/image convention)
            // but GL texture coords have bottom-left origin.  Flip Y by
            // setting uv_rect to (0, 1, 1, -1): start at v=1, height=-1.
            self.gl.uniform_4_f32(
                self.cube_uniforms.uv_rect.as_ref(),
                0.0, 1.0, 1.0, -1.0,
            );
            self.gl.active_texture(glow::TEXTURE0);

            for face in &faces {
                let entry = &self.overview_windows[face.entry_idx];

                if face.brightness < 0.05 { continue; }

                let texture = if let Some(tex) = entry.snapshot_texture {
                    tex
                } else {
                    match self.windows.get(&entry.x11_win) {
                        Some(wt) => wt.gl_texture,
                        None => continue,
                    }
                };

                self.gl.uniform_matrix_4_f32_slice(
                    self.cube_uniforms.mvp.as_ref(), false, &face.mvp,
                );
                self.gl.uniform_1_f32(
                    self.cube_uniforms.brightness.as_ref(), face.brightness,
                );
                self.gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                self.gl.tex_parameter_i32(
                    glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32,
                );
                self.gl.tex_parameter_i32(
                    glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32,
                );
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                self.gl.tex_parameter_i32(
                    glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::NEAREST as i32,
                );
                self.gl.tex_parameter_i32(
                    glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::NEAREST as i32,
                );
            }

            // Restore full-screen viewport and disable scissor
            self.gl.disable(glow::SCISSOR_TEST);
            self.gl.viewport(0, 0, self.screen_w as i32, self.screen_h as i32);

            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
        }
    }

    /// Spawn particles when a window is removed (particle effect).
    fn spawn_particles_for_window(&mut self, x: i32, y: i32, w: u32, h: u32) {
        if !self.particle_effects { return; }

        let count = self.particle_count as usize;
        let lifetime = self.particle_lifetime;
        let mut particles = Vec::with_capacity(count);

        let cols = (count as f32).sqrt().ceil() as u32;
        let rows = (count as u32 + cols - 1) / cols;

        for i in 0..count {
            let col = i as u32 % cols;
            let row = i as u32 / cols;

            let px = x as f32 + (col as f32 + 0.5) / cols as f32 * w as f32;
            let py = y as f32 + (row as f32 + 0.5) / rows as f32 * h as f32;

            // Random velocity (using simple deterministic hash)
            let hash = ((i * 2654435761) ^ (col as usize * 1597334677)) as f32;
            let vx = (hash % 200.0) - 100.0;
            let vy = -((hash / 200.0) % 300.0) - 50.0; // upward bias

            // Color from window position (gradient)
            let r = (col as f32 / cols as f32 * 0.5 + 0.5).clamp(0.3, 1.0);
            let g = (row as f32 / rows as f32 * 0.5 + 0.5).clamp(0.3, 1.0);
            let b = 0.8;

            particles.push(Particle {
                x: px, y: py,
                vx, vy,
                color: [r, g, b, 1.0],
                lifetime,
                max_lifetime: lifetime,
            });
        }

        self.particle_systems.push(ParticleSystem { particles });
        self.needs_render = true;
    }

    /// Capture the current framebuffer into scene_fbo, then run dual Kawase blur passes.
    /// `source_fbo` specifies which FBO to read from (None = default back buffer).
    fn run_blur_passes_from_fbo(&self, source_fbo: Option<glow::Framebuffer>) -> Option<glow::Texture> {
        let (scene_fbo, scene_tex) = self.scene_fbo.as_ref()?;
        if self.blur_fbos.is_empty() {
            return None;
        }

        unsafe {
            // Copy current framebuffer to scene FBO
            self.gl.bind_framebuffer(glow::READ_FRAMEBUFFER, source_fbo);
            self.gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(*scene_fbo));
            self.gl.blit_framebuffer(
                0, 0, self.screen_w as i32, self.screen_h as i32,
                0, 0, self.screen_w as i32, self.screen_h as i32,
                glow::COLOR_BUFFER_BIT,
                glow::LINEAR,
            );

            // === Downsample passes ===
            self.gl.use_program(Some(self.blur_down_program));
            self.gl.uniform_1_i32(self.blur_down_uniforms.texture.as_ref(), 0);
            self.gl.bind_vertex_array(Some(self.quad_vao));

            let mut src_tex = *scene_tex;
            let mut src_w = self.screen_w;
            let mut src_h = self.screen_h;

            for level in &self.blur_fbos {
                self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(level.fbo));
                self.gl.viewport(0, 0, level.w as i32, level.h as i32);

                let hp_proj = ortho(0.0, level.w as f32, level.h as f32, 0.0, -1.0, 1.0);
                self.gl.uniform_matrix_4_f32_slice(
                    self.blur_down_uniforms.projection.as_ref(), false, &hp_proj,
                );
                self.gl.uniform_4_f32(
                    self.blur_down_uniforms.rect.as_ref(),
                    0.0, 0.0, level.w as f32, level.h as f32,
                );
                self.gl.uniform_2_f32(
                    self.blur_down_uniforms.halfpixel.as_ref(),
                    0.5 / src_w as f32, 0.5 / src_h as f32,
                );

                self.gl.active_texture(glow::TEXTURE0);
                self.gl.bind_texture(glow::TEXTURE_2D, Some(src_tex));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

                src_tex = level.texture;
                src_w = level.w;
                src_h = level.h;
            }

            // === Upsample passes ===
            self.gl.use_program(Some(self.blur_up_program));
            self.gl.uniform_1_i32(self.blur_up_uniforms.texture.as_ref(), 0);

            // Upsample from smallest to largest (reverse order, stopping before the last)
            for i in (0..self.blur_fbos.len() - 1).rev() {
                let target = &self.blur_fbos[i];
                let source_tex = if i + 1 < self.blur_fbos.len() {
                    self.blur_fbos[i + 1].texture
                } else {
                    src_tex
                };

                self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(target.fbo));
                self.gl.viewport(0, 0, target.w as i32, target.h as i32);

                let hp_proj = ortho(0.0, target.w as f32, target.h as f32, 0.0, -1.0, 1.0);
                self.gl.uniform_matrix_4_f32_slice(
                    self.blur_up_uniforms.projection.as_ref(), false, &hp_proj,
                );
                self.gl.uniform_4_f32(
                    self.blur_up_uniforms.rect.as_ref(),
                    0.0, 0.0, target.w as f32, target.h as f32,
                );

                let src_level = &self.blur_fbos[i + 1];
                self.gl.uniform_2_f32(
                    self.blur_up_uniforms.halfpixel.as_ref(),
                    0.5 / src_level.w as f32, 0.5 / src_level.h as f32,
                );

                self.gl.active_texture(glow::TEXTURE0);
                self.gl.bind_texture(glow::TEXTURE_2D, Some(source_tex));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            }

            // Bind back to default framebuffer
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            self.gl.viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
        }

        // Return the first (largest) blur level texture as the blurred result
        Some(self.blur_fbos[0].texture)
    }

    pub(super) fn tracked_window_count(&self) -> usize {
        self.windows.len()
    }

    #[allow(dead_code)]
    pub(super) fn has_window(&self, x11_win: u32) -> bool {
        self.windows.contains_key(&x11_win)
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

// Orthographic projection matrix (column-major for OpenGL)
fn ortho(left: f32, right: f32, bottom: f32, top: f32, near: f32, far: f32) -> [f32; 16] {
    let tx = -(right + left) / (right - left);
    let ty = -(top + bottom) / (top - bottom);
    let tz = -(far + near) / (far - near);
    #[rustfmt::skip]
    let m = [
        2.0 / (right - left), 0.0,                  0.0,                 0.0,
        0.0,                  2.0 / (top - bottom),  0.0,                 0.0,
        0.0,                  0.0,                  -2.0 / (far - near),  0.0,
        tx,                   ty,                    tz,                  1.0,
    ];
    m
}

// ---------------------------------------------------------------------------
// 3D matrix helpers for cube transition (column-major for OpenGL)
// ---------------------------------------------------------------------------

/// Perspective projection matrix.
fn perspective_matrix(fov_y: f32, aspect: f32, near: f32, far: f32) -> [f32; 16] {
    let f = 1.0 / (fov_y * 0.5).tan();
    #[rustfmt::skip]
    let m = [
        f / aspect, 0.0, 0.0,                              0.0,
        0.0,        f,   0.0,                              0.0,
        0.0,        0.0, (far + near) / (near - far),     -1.0,
        0.0,        0.0, (2.0 * far * near) / (near - far), 0.0,
    ];
    m
}

/// Translation matrix.
fn translate_matrix(x: f32, y: f32, z: f32) -> [f32; 16] {
    #[rustfmt::skip]
    let m = [
        1.0, 0.0, 0.0, 0.0,
        0.0, 1.0, 0.0, 0.0,
        0.0, 0.0, 1.0, 0.0,
        x,   y,   z,   1.0,
    ];
    m
}

/// Rotation around the Y axis.
fn rotate_y_matrix(angle: f32) -> [f32; 16] {
    let c = angle.cos();
    let s = angle.sin();
    #[rustfmt::skip]
    let m = [
         c,  0.0, -s,  0.0,
        0.0, 1.0, 0.0, 0.0,
         s,  0.0,  c,  0.0,
        0.0, 0.0, 0.0, 1.0,
    ];
    m
}

/// Rotation around the X axis.
fn rotate_x_matrix(angle: f32) -> [f32; 16] {
    let c = angle.cos();
    let s = angle.sin();
    #[rustfmt::skip]
    let m = [
        1.0, 0.0, 0.0, 0.0,
        0.0,  c,   s,  0.0,
        0.0, -s,   c,  0.0,
        0.0, 0.0, 0.0, 1.0,
    ];
    m
}

/// 4×4 matrix multiply (column-major).
fn mat4_mul(a: &[f32; 16], b: &[f32; 16]) -> [f32; 16] {
    let mut m = [0.0f32; 16];
    for col in 0..4 {
        for row in 0..4 {
            m[col * 4 + row] = (0..4)
                .map(|k| a[k * 4 + row] * b[col * 4 + k])
                .sum();
        }
    }
    m
}
