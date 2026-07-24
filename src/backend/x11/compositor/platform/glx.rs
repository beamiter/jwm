//! GLX platform adapter for the shared X11 compositor.
//!
//! Owns the GLX context, the overlay `GLXWindow` drawable, the
//! `GLX_EXT_texture_from_pixmap` function pointers, and every `GLXPixmap`
//! created for window imports. All GLX symbols and resource lifetimes stay in
//! this file; the facade in the parent module dispatches into it.

use super::super::PixmapBinding;
use glow::HasContext;
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_void};
use std::ptr;

// ---------------------------------------------------------------------------
// GLX implementation
// ---------------------------------------------------------------------------

type GlXBindTexImageExt =
    unsafe extern "C" fn(*mut x11::xlib::Display, x11::glx::GLXDrawable, i32, *const i32);
type GlXReleaseTexImageExt =
    unsafe extern "C" fn(*mut x11::xlib::Display, x11::glx::GLXDrawable, i32);

struct TfpFunctions {
    bind: GlXBindTexImageExt,
    release: GlXReleaseTexImageExt,
}

const GLX_BIND_TO_TEXTURE_RGBA_EXT: i32 = 0x20D1;
const GLX_BIND_TO_TEXTURE_RGB_EXT: i32 = 0x20D0;
const GLX_TEXTURE_FORMAT_EXT: i32 = 0x20D5;
const GLX_TEXTURE_TARGET_EXT: i32 = 0x20D6;
const GLX_TEXTURE_2D_EXT: i32 = 0x20DC;
const GLX_TEXTURE_FORMAT_RGBA_EXT: i32 = 0x20DA;
const GLX_TEXTURE_FORMAT_RGB_EXT: i32 = 0x20D9;
const GLX_FRONT_LEFT_EXT: i32 = 0x20DE;
const GLX_BACK_BUFFER_AGE_EXT: i32 = 0x20F4;

fn has_glx_extension(extensions: &str, expected: &str) -> bool {
    extensions
        .split_ascii_whitespace()
        .any(|extension| extension == expected)
}

fn validated_glx_buffer_age(supported: bool, queried_age: u32) -> u32 {
    supported.then_some(queried_age).unwrap_or(0)
}

pub(super) struct GlxPlatform {
    context: x11::glx::GLXContext,
    pub(super) drawable: x11::glx::GLXDrawable,
    buffer_age_supported: bool,
    tfp: TfpFunctions,
    fbconfig_rgba: x11::glx::GLXFBConfig,
    fbconfig_rgb: x11::glx::GLXFBConfig,
    tfp_visual_configs: HashMap<u32, (x11::glx::GLXFBConfig, bool)>,
    tfp_visual_configs_10bit: HashMap<u32, (x11::glx::GLXFBConfig, bool)>,
    pub(super) output_is_10bit: bool,
}

impl GlxPlatform {
    pub(super) fn new(
        display: *mut x11::xlib::Display,
        screen_num: i32,
        overlay_window: u32,
        overlay_visual_id: u32,
        hdr_enabled: bool,
    ) -> Result<Self, String> {
        let ext_str = unsafe {
            let raw = x11::glx::glXQueryExtensionsString(display, screen_num);
            if raw.is_null() {
                ""
            } else {
                CStr::from_ptr(raw).to_str().unwrap_or("")
            }
        };
        if !has_glx_extension(ext_str, "GLX_EXT_texture_from_pixmap") {
            return Err("GLX_EXT_texture_from_pixmap not available".into());
        }
        let buffer_age_supported = has_glx_extension(ext_str, "GLX_EXT_buffer_age");
        log::info!("compositor: GLX extensions: {ext_str}");
        log::info!("compositor: GLX partial redraw buffer_age={buffer_age_supported}");

        let (ctx_fbconfig, output_is_10bit) =
            choose_glx_context_config(display, screen_num, overlay_visual_id, hdr_enabled)?;

        let context = unsafe {
            x11::glx::glXCreateNewContext(
                display,
                ctx_fbconfig,
                x11::glx::GLX_RGBA_TYPE,
                ptr::null_mut(),
                1,
            )
        };
        if context.is_null() {
            return Err("glXCreateNewContext failed".into());
        }
        if unsafe { x11::glx::glXIsDirect(display, context) } == 0 {
            unsafe { x11::glx::glXDestroyContext(display, context) };
            return Err("GLX context is indirect; compositor requires direct rendering".into());
        }

        let drawable = unsafe {
            x11::glx::glXCreateWindow(display, ctx_fbconfig, overlay_window as _, ptr::null())
        };
        if drawable == 0 {
            unsafe { x11::glx::glXDestroyContext(display, context) };
            return Err("glXCreateWindow failed".into());
        }
        let current =
            unsafe { x11::glx::glXMakeContextCurrent(display, drawable, drawable, context) };
        if current == 0 {
            unsafe {
                x11::glx::glXDestroyWindow(display, drawable);
                x11::glx::glXDestroyContext(display, context);
            }
            return Err("glXMakeContextCurrent failed".into());
        }

        let bind_ptr = glx_proc("glXBindTexImageEXT");
        let release_ptr = glx_proc("glXReleaseTexImageEXT");
        if bind_ptr.is_null() || release_ptr.is_null() {
            unsafe {
                x11::glx::glXMakeContextCurrent(display, 0, 0, ptr::null_mut());
                x11::glx::glXDestroyWindow(display, drawable);
                x11::glx::glXDestroyContext(display, context);
            }
            return Err("glXBindTexImageEXT / glXReleaseTexImageEXT not available".into());
        }
        let tfp = TfpFunctions {
            bind: unsafe { std::mem::transmute(bind_ptr) },
            release: unsafe { std::mem::transmute(release_ptr) },
        };

        enable_glx_vsync(display, drawable);

        let (fbconfig_rgba, fbconfig_rgb, tfp_visual_configs) =
            enumerate_glx_tfp_configs(display, screen_num, false);
        let (_, _, tfp_visual_configs_10bit) = if hdr_enabled {
            enumerate_glx_tfp_configs(display, screen_num, true)
        } else {
            (ptr::null_mut(), ptr::null_mut(), HashMap::new())
        };
        if fbconfig_rgba.is_null() && fbconfig_rgb.is_null() {
            unsafe {
                x11::glx::glXMakeContextCurrent(display, 0, 0, ptr::null_mut());
                x11::glx::glXDestroyWindow(display, drawable);
                x11::glx::glXDestroyContext(display, context);
            }
            return Err("No GLX FBConfig for texture_from_pixmap".into());
        }

        log::info!(
            "compositor: GLX TFP configs rgba={} rgb={} per_visual={} hdr_visuals={}",
            !fbconfig_rgba.is_null(),
            !fbconfig_rgb.is_null(),
            tfp_visual_configs.len(),
            tfp_visual_configs_10bit.len()
        );

        Ok(Self {
            context,
            drawable,
            buffer_age_supported,
            tfp,
            fbconfig_rgba,
            fbconfig_rgb,
            tfp_visual_configs,
            tfp_visual_configs_10bit,
            output_is_10bit,
        })
    }

    pub(super) fn make_current(&self, display: *mut x11::xlib::Display) -> Result<(), String> {
        let ok = unsafe {
            x11::glx::glXMakeContextCurrent(display, self.drawable, self.drawable, self.context)
        };
        if ok == 0 {
            Err("glXMakeContextCurrent failed".into())
        } else {
            Ok(())
        }
    }

    pub(super) fn buffer_age(&self, display: *mut x11::xlib::Display) -> u32 {
        if !self.buffer_age_supported || display.is_null() {
            return 0;
        }

        let mut age = 0;
        unsafe {
            x11::glx::glXQueryDrawable(display, self.drawable, GLX_BACK_BUFFER_AGE_EXT, &mut age);
        }
        validated_glx_buffer_age(self.buffer_age_supported, age)
    }

    pub(super) fn swap_buffers(&self, display: *mut x11::xlib::Display) -> Result<(), String> {
        unsafe { x11::glx::glXSwapBuffers(display, self.drawable) };
        Ok(())
    }

    pub(super) fn get_proc_address(&self, name: &str) -> *const c_void {
        glx_proc(name)
    }

    fn select_tfp_config(
        &self,
        visual: u32,
        depth: u8,
        hdr_enabled: bool,
    ) -> Result<(x11::glx::GLXFBConfig, bool), String> {
        if hdr_enabled {
            if let Some(&(config, rgba)) = self.tfp_visual_configs_10bit.get(&visual) {
                return Ok((config, rgba));
            }
        }
        if let Some(&(config, rgba)) = self.tfp_visual_configs.get(&visual) {
            return Ok((config, rgba));
        }
        let rgba = depth == 32 && !self.fbconfig_rgba.is_null();
        let config = if rgba {
            self.fbconfig_rgba
        } else {
            self.fbconfig_rgb
        };
        if config.is_null() {
            Err(format!(
                "no GLX TFP config for visual=0x{visual:x} depth={depth}"
            ))
        } else {
            Ok((config, rgba))
        }
    }

    pub(super) fn import_pixmap(
        &self,
        display: *mut x11::xlib::Display,
        gl: &glow::Context,
        texture: glow::Texture,
        pixmap: u32,
        visual: u32,
        depth: u8,
        hdr_enabled: bool,
    ) -> Result<(PixmapBinding, bool), String> {
        let (fbconfig, rgba) = self.select_tfp_config(visual, depth, hdr_enabled)?;
        let tex_fmt = if rgba {
            GLX_TEXTURE_FORMAT_RGBA_EXT
        } else {
            GLX_TEXTURE_FORMAT_RGB_EXT
        };
        let attrs = [
            GLX_TEXTURE_TARGET_EXT,
            GLX_TEXTURE_2D_EXT,
            GLX_TEXTURE_FORMAT_EXT,
            tex_fmt,
            0,
        ];
        let drawable =
            unsafe { x11::glx::glXCreatePixmap(display, fbconfig, pixmap as _, attrs.as_ptr()) };
        if drawable == 0 {
            return Err(format!("glXCreatePixmap failed for pixmap 0x{pixmap:x}"));
        }
        unsafe {
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            (self.tfp.bind)(display, drawable, GLX_FRONT_LEFT_EXT, ptr::null());
            gl.bind_texture(glow::TEXTURE_2D, None);
        }
        Ok((PixmapBinding::Glx { drawable }, rgba))
    }

    pub(super) fn refresh_pixmap(
        &self,
        display: *mut x11::xlib::Display,
        gl: &glow::Context,
        texture: glow::Texture,
        drawable: x11::glx::GLXPixmap,
    ) -> Result<(), String> {
        unsafe {
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            (self.tfp.release)(display, drawable, GLX_FRONT_LEFT_EXT);
            (self.tfp.bind)(display, drawable, GLX_FRONT_LEFT_EXT, ptr::null());
            gl.bind_texture(glow::TEXTURE_2D, None);
        }
        Ok(())
    }

    pub(super) fn release_pixmap(
        &self,
        display: *mut x11::xlib::Display,
        gl: &glow::Context,
        texture: glow::Texture,
        drawable: x11::glx::GLXPixmap,
    ) {
        unsafe {
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            (self.tfp.release)(display, drawable, GLX_FRONT_LEFT_EXT);
            gl.bind_texture(glow::TEXTURE_2D, None);
            x11::glx::glXDestroyPixmap(display, drawable);
        }
    }

    pub(super) unsafe fn shutdown(&mut self, display: *mut x11::xlib::Display) {
        if self.context.is_null() {
            return;
        }
        unsafe {
            x11::glx::glXMakeContextCurrent(display, 0, 0, ptr::null_mut());
            if self.drawable != 0 {
                x11::glx::glXDestroyWindow(display, self.drawable);
            }
            x11::glx::glXDestroyContext(display, self.context);
        }
        self.drawable = 0;
        self.context = ptr::null_mut();
    }
}

fn glx_proc(name: &str) -> *const c_void {
    let Ok(name) = CString::new(name) else {
        return ptr::null();
    };
    unsafe {
        x11::glx::glXGetProcAddress(name.as_ptr() as *const u8)
            .map_or(ptr::null(), |proc| proc as *const c_void)
    }
}

fn enable_glx_vsync(display: *mut x11::xlib::Display, drawable: x11::glx::GLXDrawable) {
    let ext = glx_proc("glXSwapIntervalEXT");
    if !ext.is_null() {
        type SwapIntervalExt =
            unsafe extern "C" fn(*mut x11::xlib::Display, x11::glx::GLXDrawable, i32);
        let swap: SwapIntervalExt = unsafe { std::mem::transmute(ext) };
        unsafe { swap(display, drawable, 1) };
        log::info!("compositor: GLX vsync enabled through glXSwapIntervalEXT");
        return;
    }
    let mesa = glx_proc("glXSwapIntervalMESA");
    if !mesa.is_null() {
        type SwapIntervalMesa = unsafe extern "C" fn(u32) -> i32;
        let swap: SwapIntervalMesa = unsafe { std::mem::transmute(mesa) };
        unsafe {
            swap(1);
        }
        log::info!("compositor: GLX vsync enabled through glXSwapIntervalMESA");
    } else {
        log::warn!("compositor: no GLX swap interval extension; tearing may occur");
    }
}

fn choose_glx_context_config(
    display: *mut x11::xlib::Display,
    screen_num: i32,
    overlay_visual_id: u32,
    hdr_enabled: bool,
) -> Result<(x11::glx::GLXFBConfig, bool), String> {
    let try_choose = |bits: i32| -> Option<x11::glx::GLXFBConfig> {
        let alpha = if bits == 10 { 2 } else { 0 };
        let attrs = [
            x11::glx::GLX_RENDER_TYPE,
            x11::glx::GLX_RGBA_BIT,
            x11::glx::GLX_DRAWABLE_TYPE,
            x11::glx::GLX_WINDOW_BIT,
            x11::glx::GLX_DOUBLEBUFFER,
            1,
            x11::glx::GLX_RED_SIZE,
            bits,
            x11::glx::GLX_GREEN_SIZE,
            bits,
            x11::glx::GLX_BLUE_SIZE,
            bits,
            x11::glx::GLX_ALPHA_SIZE,
            alpha,
            0,
        ];
        let mut count = 0;
        let configs =
            unsafe { x11::glx::glXChooseFBConfig(display, screen_num, attrs.as_ptr(), &mut count) };
        if configs.is_null() || count == 0 {
            return None;
        }
        let mut selected = ptr::null_mut();
        unsafe {
            for index in 0..count {
                let config = *configs.offset(index as isize);
                let visual = x11::glx::glXGetVisualFromFBConfig(display, config);
                if !visual.is_null() {
                    let visual_id = (*visual).visualid;
                    x11::xlib::XFree(visual as *mut _);
                    if visual_id == overlay_visual_id as u64 {
                        selected = config;
                        break;
                    }
                }
            }
            // Preserve the legacy fallback for drivers whose overlay visual is
            // not exposed verbatim by glXChooseFBConfig.
            if selected.is_null() {
                selected = *configs;
                log::warn!(
                    "compositor: no GLX FBConfig matching visual 0x{:x}; using first candidate",
                    overlay_visual_id
                );
            }
            x11::xlib::XFree(configs as *mut _);
        }
        Some(selected)
    };

    if hdr_enabled {
        if let Some(config) = try_choose(10) {
            return Ok((config, true));
        }
        log::warn!("compositor: no 10-bit GLX config; falling back to 8-bit output");
    }
    try_choose(8)
        .map(|config| (config, false))
        .ok_or_else(|| "No suitable GLX FBConfig found".to_string())
}

fn enumerate_glx_tfp_configs(
    display: *mut x11::xlib::Display,
    screen_num: i32,
    ten_bit: bool,
) -> (
    x11::glx::GLXFBConfig,
    x11::glx::GLXFBConfig,
    HashMap<u32, (x11::glx::GLXFBConfig, bool)>,
) {
    let bits = if ten_bit { 10 } else { 8 };
    let alpha = if ten_bit { 2 } else { 8 };
    let rgba_attrs = [
        x11::glx::GLX_DRAWABLE_TYPE,
        x11::glx::GLX_PIXMAP_BIT,
        x11::glx::GLX_RENDER_TYPE,
        x11::glx::GLX_RGBA_BIT,
        GLX_BIND_TO_TEXTURE_RGBA_EXT,
        1,
        x11::glx::GLX_RED_SIZE,
        bits,
        x11::glx::GLX_GREEN_SIZE,
        bits,
        x11::glx::GLX_BLUE_SIZE,
        bits,
        x11::glx::GLX_ALPHA_SIZE,
        alpha,
        0,
    ];
    let rgb_attrs = [
        x11::glx::GLX_DRAWABLE_TYPE,
        x11::glx::GLX_PIXMAP_BIT,
        x11::glx::GLX_RENDER_TYPE,
        x11::glx::GLX_RGBA_BIT,
        GLX_BIND_TO_TEXTURE_RGB_EXT,
        1,
        x11::glx::GLX_RED_SIZE,
        bits,
        x11::glx::GLX_GREEN_SIZE,
        bits,
        x11::glx::GLX_BLUE_SIZE,
        bits,
        0,
    ];

    let mut map = HashMap::new();
    let mut rgba = ptr::null_mut();
    let mut rgb = ptr::null_mut();
    collect_glx_configs(display, screen_num, &rgba_attrs, true, &mut rgba, &mut map);
    collect_glx_configs(display, screen_num, &rgb_attrs, false, &mut rgb, &mut map);
    (rgba, rgb, map)
}

fn collect_glx_configs(
    display: *mut x11::xlib::Display,
    screen_num: i32,
    attrs: &[i32],
    rgba: bool,
    first: &mut x11::glx::GLXFBConfig,
    map: &mut HashMap<u32, (x11::glx::GLXFBConfig, bool)>,
) {
    let mut count = 0;
    let configs =
        unsafe { x11::glx::glXChooseFBConfig(display, screen_num, attrs.as_ptr(), &mut count) };
    if configs.is_null() || count == 0 {
        return;
    }
    unsafe {
        *first = *configs;
        for index in 0..count {
            let config = *configs.offset(index as isize);
            let mut visual = 0;
            x11::glx::glXGetFBConfigAttrib(display, config, x11::glx::GLX_VISUAL_ID, &mut visual);
            if visual != 0 {
                map.entry(visual as u32).or_insert((config, rgba));
            }
        }
        x11::xlib::XFree(configs as *mut _);
    }
}

#[cfg(test)]
mod tests {
    use super::{has_glx_extension, validated_glx_buffer_age};

    #[test]
    fn glx_buffer_age_requires_the_extension_and_preserves_real_age() {
        let extensions = "GLX_ARB_create_context GLX_EXT_buffer_age GLX_EXT_texture_from_pixmap";

        assert!(has_glx_extension(extensions, "GLX_EXT_buffer_age"));
        assert!(!has_glx_extension(extensions, "GLX_EXT_buffer"));
        assert_eq!(validated_glx_buffer_age(false, 1), 0);
        assert_eq!(validated_glx_buffer_age(true, 0), 0);
        assert_eq!(validated_glx_buffer_age(true, 2), 2);
        assert_eq!(validated_glx_buffer_age(true, 3), 3);
    }
}
