// ---------------------------------------------------------------------------
// Wayland udev backend compositor - GPU-accelerated composition with effects
// ---------------------------------------------------------------------------

pub mod shaders;
mod render;
mod effects;
mod transitions;
mod blur;
mod postprocess;
mod overview;
mod expose;
mod config;
mod damage;

use smithay::backend::renderer::gles::ffi;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::ffi::CString;
use std::time::{Duration, Instant};

use crate::backend::common_define::WindowId;

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
// Per-window state
// ---------------------------------------------------------------------------

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
    #[allow(dead_code)]
    pub motion_trail: VecDeque<(i32, i32)>,
    pub opacity_override: Option<f32>,
    pub corner_radius_override: Option<f32>,
    pub frame_extents: [u32; 4],
    pub is_shaped: bool,
    pub is_fullscreen: bool,
    pub is_urgent: bool,
    pub is_pip: bool,
    pub is_frosted: bool,
    #[allow(dead_code)]
    pub class_name: String,
    pub ripple_progress: f32,
    pub ripple_active: bool,
    /// UV sub-rect for content within the buffer: [u, v, w, h].
    /// Accounts for CSD geometry offset (shadows/decorations outside window geometry).
    /// Default [0,0,1,1] means full texture = content.
    pub content_uv: [f32; 4],
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
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

// ---------------------------------------------------------------------------
// Overview entry
// ---------------------------------------------------------------------------

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
    #[allow(dead_code)]
    genie_program: u32,
    particle_program: u32,
    overview_bg_program: u32,
    #[allow(dead_code)]
    hud_program: u32,
    #[allow(dead_code)]
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

    // Overview
    overview_active: bool,
    overview_opacity: f32,
    overview_entries: Vec<OverviewEntry>,
    overview_selection: Option<u64>,
    overview_monitor: (i32, i32, u32, u32),

    // Expose
    expose_active: bool,
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

    // Dock position (for genie)
    dock_x: f32,
    dock_y: f32,

    // Window groups (tabs)
    window_groups: Vec<(u32, Vec<(u32, String, bool)>)>,

    // Monitors info
    monitors: Vec<(u32, i32, i32, u32, u32)>,

    // Zoom to fit
    zoom_to_fit_window: Option<u32>,

    // Annotations
    annotation_active: bool,
    annotation_points: Vec<(f32, f32)>,
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

unsafe fn create_fbo_texture(gl: &ffi::Gles2, w: u32, h: u32) -> (u32, u32) {
    unsafe {
        let mut tex = 0u32;
        gl.GenTextures(1, &mut tex);
        gl.BindTexture(ffi::TEXTURE_2D, tex);
        gl.TexImage2D(
            ffi::TEXTURE_2D,
            0,
            ffi::RGBA8 as i32,
            w as i32,
            h as i32,
            0,
            ffi::RGBA,
            ffi::UNSIGNED_BYTE,
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
        let (output_fbo, output_texture) = create_fbo_texture(gl, screen_w, screen_h);

        // ----- Create scene FBO + texture -----
        let (scene_fbo, scene_texture) = create_fbo_texture(gl, screen_w, screen_h);

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
            let (fbo, texture) = create_fbo_texture(gl, bw, bh);
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
        let (postprocess_fbo, postprocess_texture) = create_fbo_texture(gl, screen_w, screen_h);

        // ----- Create transition FBO + texture -----
        let (transition_fbo, transition_texture) = create_fbo_texture(gl, screen_w, screen_h);

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

            // Expose
            expose_active: false,
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
            hdr_enabled: false,
            hdr_peak_nits: 1000.0,
            tone_mapping_method: 0,

            // Debug HUD
            debug_hud_enabled: false,

            // Optimization
            needs_render: true,
            last_frame_time: now,
            frame_count: 0,
            fps: 0.0,

            // Dock position
            dock_x: 0.0,
            dock_y: 0.0,

            // Window groups
            window_groups: Vec::new(),

            // Monitors
            monitors: Vec::new(),

            // Zoom to fit
            zoom_to_fit_window: None,

            // Annotations
            annotation_active: false,
            annotation_points: Vec::new(),
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

            let (output_fbo, output_texture) = create_fbo_texture(gl, w, h);
            self.output_fbo = output_fbo;
            self.output_texture = output_texture;

            let (scene_fbo, scene_texture) = create_fbo_texture(gl, w, h);
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
                let (fbo, texture) = create_fbo_texture(gl, bw, bh);
                self.blur_fbos.push(BlurFboLevel {
                    fbo,
                    texture,
                    width: bw,
                    height: bh,
                });
                bw /= 2;
                bh /= 2;
            }

            let (postprocess_fbo, postprocess_texture) = create_fbo_texture(gl, w, h);
            self.postprocess_fbo = postprocess_fbo;
            self.postprocess_texture = postprocess_texture;

            let (transition_fbo, transition_texture) = create_fbo_texture(gl, w, h);
            self.transition_fbo = transition_fbo;
            self.transition_texture = transition_texture;

            gl.BindFramebuffer(ffi::FRAMEBUFFER, 0);
            gl.BindTexture(ffi::TEXTURE_2D, 0);
        }

        self.needs_render = true;
        self.overview_monitor = (0, 0, w, h);
    }
}
