use crate::backend::x11::compositor::{WallpaperMode, WobblyState};
pub(super) use crate::backend::x11::compositor_common::effects::{
    Particle, ParticleSystem, RippleState,
};
pub(super) use crate::backend::x11::compositor_common::expose::{
    ExposeEntry, SnapPreview, WindowTab,
};
use std::collections::VecDeque;

pub(super) type GlXBindTexImageEXT =
    unsafe extern "C" fn(*mut x11::xlib::Display, x11::glx::GLXDrawable, i32, *const i32);
pub(super) type GlXReleaseTexImageEXT =
    unsafe extern "C" fn(*mut x11::xlib::Display, x11::glx::GLXDrawable, i32);

pub(super) struct TfpFunctions {
    pub(super) bind: GlXBindTexImageEXT,
    pub(super) release: GlXReleaseTexImageEXT,
}

pub(super) const GLX_BIND_TO_TEXTURE_RGBA_EXT: i32 = 0x20D1;
pub(super) const GLX_BIND_TO_TEXTURE_RGB_EXT: i32 = 0x20D0;
#[allow(dead_code)]
pub(super) const GLX_Y_INVERTED_EXT: i32 = 0x20D4;
pub(super) const GLX_TEXTURE_FORMAT_EXT: i32 = 0x20D5;
pub(super) const GLX_TEXTURE_TARGET_EXT: i32 = 0x20D6;
pub(super) const GLX_TEXTURE_2D_EXT: i32 = 0x20DC;
pub(super) const GLX_TEXTURE_FORMAT_RGBA_EXT: i32 = 0x20DA;
pub(super) const GLX_TEXTURE_FORMAT_RGB_EXT: i32 = 0x20D9;
pub(super) const GLX_FRONT_LEFT_EXT: i32 = 0x20DE;

pub(super) struct WindowTexture {
    pub(super) x: i32,
    pub(super) y: i32,
    pub(super) w: u32,
    pub(super) h: u32,
    pub(super) damage: u32,
    pub(super) pixmap: u32,
    pub(super) glx_pixmap: x11::glx::GLXPixmap,
    pub(super) gl_texture: glow::Texture,
    pub(super) dirty: bool,
    pub(super) has_rgba: bool,
    pub(super) fbconfig: x11::glx::GLXFBConfig,
    pub(super) needs_pixmap_refresh: bool,
    pub(super) x11_win: u32,
    pub(super) fade_opacity: f32,
    pub(super) fading_out: bool,
    pub(super) class_name: String,
    pub(super) opacity_override: Option<f32>,
    pub(super) is_fullscreen: bool,
    pub(super) corner_radius_override: Option<f32>,
    pub(super) scale: f32,
    pub(super) frame_extents: [u32; 4],
    pub(super) is_shaped: bool,
    pub(super) anim_scale: f32,
    pub(super) anim_scale_target: f32,
    pub(super) is_urgent: bool,
    pub(super) is_pip: bool,
    pub(super) is_frosted: bool,
    pub(super) is_override_redirect: bool,
    pub(super) wobbly: Option<WobblyState>,
    pub(super) pending_fence: Option<glow::Fence>,
    pub(super) motion_trail: VecDeque<(i32, i32)>,
    pub(super) audio_sync_target: Option<f32>,
}

pub(super) struct WindowUniforms {
    pub(super) projection: Option<glow::UniformLocation>,
    pub(super) rect: Option<glow::UniformLocation>,
    pub(super) texture: Option<glow::UniformLocation>,
    pub(super) opacity: Option<glow::UniformLocation>,
    pub(super) radius: Option<glow::UniformLocation>,
    pub(super) size: Option<glow::UniformLocation>,
    pub(super) dim: Option<glow::UniformLocation>,
    pub(super) uv_rect: Option<glow::UniformLocation>,
    pub(super) ripple_progress: Option<glow::UniformLocation>,
    pub(super) ripple_amplitude: Option<glow::UniformLocation>,
}

pub(super) struct ShadowUniforms {
    pub(super) projection: Option<glow::UniformLocation>,
    pub(super) rect: Option<glow::UniformLocation>,
    pub(super) shadow_color: Option<glow::UniformLocation>,
    pub(super) size: Option<glow::UniformLocation>,
    pub(super) radius: Option<glow::UniformLocation>,
    pub(super) spread: Option<glow::UniformLocation>,
}

pub(super) struct BlurUniforms {
    pub(super) projection: Option<glow::UniformLocation>,
    pub(super) rect: Option<glow::UniformLocation>,
    pub(super) texture: Option<glow::UniformLocation>,
    pub(super) halfpixel: Option<glow::UniformLocation>,
}

pub(super) struct BlurFboLevel {
    pub(super) fbo: glow::Framebuffer,
    pub(super) texture: glow::Texture,
    pub(super) w: u32,
    pub(super) h: u32,
}

pub(super) struct TransitionUniforms {
    pub(super) projection: Option<glow::UniformLocation>,
    pub(super) rect: Option<glow::UniformLocation>,
    pub(super) texture: Option<glow::UniformLocation>,
    pub(super) opacity: Option<glow::UniformLocation>,
    pub(super) uv_rect: Option<glow::UniformLocation>,
}

pub(super) struct CubeUniforms {
    pub(super) mvp: Option<glow::UniformLocation>,
    pub(super) aspect: Option<glow::UniformLocation>,
    pub(super) texture: Option<glow::UniformLocation>,
    pub(super) brightness: Option<glow::UniformLocation>,
    pub(super) uv_rect: Option<glow::UniformLocation>,
}

pub(super) struct PortalUniforms {
    pub(super) projection: Option<glow::UniformLocation>,
    pub(super) rect: Option<glow::UniformLocation>,
    pub(super) texture: Option<glow::UniformLocation>,
    pub(super) progress: Option<glow::UniformLocation>,
    pub(super) glow: Option<glow::UniformLocation>,
    pub(super) center: Option<glow::UniformLocation>,
    pub(super) uv_rect: Option<glow::UniformLocation>,
}

pub(super) struct MonitorWallpaper {
    pub(super) mon_x: i32,
    pub(super) mon_y: i32,
    pub(super) mon_w: u32,
    pub(super) mon_h: u32,
    pub(super) texture: Option<glow::Texture>,
    pub(super) mode: WallpaperMode,
    pub(super) img_w: u32,
    pub(super) img_h: u32,
    pub(super) current_path: String,
}

pub(super) struct BorderUniforms {
    pub(super) projection: Option<glow::UniformLocation>,
    pub(super) rect: Option<glow::UniformLocation>,
    pub(super) border_color: Option<glow::UniformLocation>,
    pub(super) size: Option<glow::UniformLocation>,
    pub(super) radius: Option<glow::UniformLocation>,
    pub(super) border_width: Option<glow::UniformLocation>,
}

pub(super) struct PostprocessUniforms {
    pub(super) projection: Option<glow::UniformLocation>,
    pub(super) rect: Option<glow::UniformLocation>,
    pub(super) texture: Option<glow::UniformLocation>,
    pub(super) color_temp: Option<glow::UniformLocation>,
    pub(super) saturation: Option<glow::UniformLocation>,
    pub(super) brightness: Option<glow::UniformLocation>,
    pub(super) contrast: Option<glow::UniformLocation>,
    pub(super) invert: Option<glow::UniformLocation>,
    pub(super) grayscale: Option<glow::UniformLocation>,
    pub(super) hdr_enabled: Option<glow::UniformLocation>,
    pub(super) hdr_peak_nits: Option<glow::UniformLocation>,
    pub(super) tone_mapping_method: Option<glow::UniformLocation>,
    pub(super) eotf_mode: Option<glow::UniformLocation>,
    pub(super) output_colorspace: Option<glow::UniformLocation>,
}

pub(super) struct HudUniforms {
    pub(super) projection: Option<glow::UniformLocation>,
    pub(super) rect: Option<glow::UniformLocation>,
    pub(super) bg_color: Option<glow::UniformLocation>,
    pub(super) fg_color: Option<glow::UniformLocation>,
    pub(super) size: Option<glow::UniformLocation>,
}

pub(super) struct HudTextUniforms {
    pub(super) projection: Option<glow::UniformLocation>,
    pub(super) rect: Option<glow::UniformLocation>,
    pub(super) texture: Option<glow::UniformLocation>,
}

pub(super) struct LineUniforms {
    pub(super) projection: Option<glow::UniformLocation>,
    pub(super) color: Option<glow::UniformLocation>,
}

pub(super) struct OverviewEntry {
    pub(super) x11_win: u32,
    pub(super) target_w: f32,
    pub(super) target_h: f32,
    pub(super) is_selected: bool,
    pub(super) snapshot_texture: Option<glow::Texture>,
    pub(super) title: String,
    pub(super) title_texture: Option<(glow::Texture, u32, u32)>,
    pub(super) face_index: usize,
}

pub(super) struct EdgeGlowUniforms {
    pub(super) projection: Option<glow::UniformLocation>,
    pub(super) rect: Option<glow::UniformLocation>,
    pub(super) glow_color: Option<glow::UniformLocation>,
    pub(super) glow_width: Option<glow::UniformLocation>,
    pub(super) mouse: Option<glow::UniformLocation>,
    pub(super) screen_size: Option<glow::UniformLocation>,
    pub(super) time: Option<glow::UniformLocation>,
}

pub(super) struct TiltUniforms {
    pub(super) projection: Option<glow::UniformLocation>,
    pub(super) rect: Option<glow::UniformLocation>,
    pub(super) texture: Option<glow::UniformLocation>,
    pub(super) opacity: Option<glow::UniformLocation>,
    pub(super) radius: Option<glow::UniformLocation>,
    pub(super) size: Option<glow::UniformLocation>,
    pub(super) dim: Option<glow::UniformLocation>,
    pub(super) uv_rect: Option<glow::UniformLocation>,
    pub(super) tilt: Option<glow::UniformLocation>,
    pub(super) perspective: Option<glow::UniformLocation>,
    pub(super) grid_size: Option<glow::UniformLocation>,
    pub(super) light_dir: Option<glow::UniformLocation>,
}

pub(super) struct WobblyUniforms {
    pub(super) projection: Option<glow::UniformLocation>,
    pub(super) rect: Option<glow::UniformLocation>,
    pub(super) texture: Option<glow::UniformLocation>,
    pub(super) opacity: Option<glow::UniformLocation>,
    pub(super) radius: Option<glow::UniformLocation>,
    pub(super) size: Option<glow::UniformLocation>,
    pub(super) dim: Option<glow::UniformLocation>,
    pub(super) uv_rect: Option<glow::UniformLocation>,
    pub(super) grid_offsets: Option<glow::UniformLocation>,
    pub(super) grid_n: Option<glow::UniformLocation>,
}

pub(super) struct OverviewBgUniforms {
    pub(super) projection: Option<glow::UniformLocation>,
    pub(super) rect: Option<glow::UniformLocation>,
    pub(super) opacity: Option<glow::UniformLocation>,
}

pub(super) struct MagnifierUniforms {
    pub(super) magnifier_enabled: Option<glow::UniformLocation>,
    pub(super) magnifier_center: Option<glow::UniformLocation>,
    pub(super) magnifier_radius: Option<glow::UniformLocation>,
    pub(super) magnifier_zoom: Option<glow::UniformLocation>,
    pub(super) slime_enabled: Option<glow::UniformLocation>,
    pub(super) slime_points: Option<glow::UniformLocation>,
    pub(super) slime_bbox: Option<glow::UniformLocation>,
    pub(super) slime_screen_size: Option<glow::UniformLocation>,
    pub(super) slime_scale: Option<glow::UniformLocation>,
    pub(super) slime_strength: Option<glow::UniformLocation>,
    pub(super) slime_opacity: Option<glow::UniformLocation>,
    pub(super) colorblind_mode: Option<glow::UniformLocation>,
}

pub(super) struct ParticleUniforms {
    pub(super) projection: Option<glow::UniformLocation>,
    pub(super) point_size: Option<glow::UniformLocation>,
}

pub(super) struct GenieUniforms {
    pub(super) projection: Option<glow::UniformLocation>,
    pub(super) rect: Option<glow::UniformLocation>,
    pub(super) texture: Option<glow::UniformLocation>,
    pub(super) opacity: Option<glow::UniformLocation>,
    pub(super) radius: Option<glow::UniformLocation>,
    pub(super) size: Option<glow::UniformLocation>,
    pub(super) dim: Option<glow::UniformLocation>,
    pub(super) uv_rect: Option<glow::UniformLocation>,
    pub(super) progress: Option<glow::UniformLocation>,
    pub(super) dock_pos: Option<glow::UniformLocation>,
    pub(super) grid_size: Option<glow::UniformLocation>,
}

pub(super) struct GenieAnimation {
    pub(super) start: std::time::Instant,
    pub(super) x: f32,
    pub(super) y: f32,
    pub(super) w: f32,
    pub(super) h: f32,
    pub(super) gl_texture: glow::Texture,
    pub(super) has_rgba: bool,
    pub(super) glx_pixmap: x11::glx::GLXPixmap,
    pub(super) pixmap: u32,
    pub(super) damage: u32,
}
