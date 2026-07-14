//! GLX and EGL/OpenGL ES platform integration for the shared X11 compositor.
//!
//! The x11rb and xcb backends both feed the same compositor implementation, so
//! the graphics API selection belongs here rather than in either protocol
//! backend.  Window contents continue to come from XComposite named pixmaps:
//!
//! * GLX imports them through `GLX_EXT_texture_from_pixmap`.
//! * EGL/GLES imports them through `EGL_KHR_image_pixmap` + `GL_OES_EGL_image`.

use super::{DirtyRect, OmlSyncControl, PixmapBinding};
use glow::HasContext;
use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_void};
use std::ptr;

// ---------------------------------------------------------------------------
// Public selection surface
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GraphicsApiPreference {
    Auto,
    EglGles,
    Glx,
}

impl GraphicsApiPreference {
    pub(super) fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "egl" | "gles" | "egl-gles" | "egl_gles" => Ok(Self::EglGles),
            "glx" | "opengl" => Ok(Self::Glx),
            other => Err(format!(
                "unsupported compositor_api '{other}' (expected auto, egl, or glx)"
            )),
        }
    }
}

pub(super) struct GraphicsPlatform {
    xlib_display: *mut x11::xlib::Display,
    screen_num: i32,
    backend: PlatformBackend,
    closed: bool,
}

enum PlatformBackend {
    Glx(GlxPlatform),
    Egl(EglPlatform),
}

impl GraphicsPlatform {
    pub(super) fn new(
        overlay_window: u32,
        overlay_visual_id: u32,
        hdr_enabled: bool,
        preference: GraphicsApiPreference,
    ) -> Result<Self, String> {
        let xlib_display = unsafe { x11::xlib::XOpenDisplay(ptr::null()) };
        if xlib_display.is_null() {
            return Err("XOpenDisplay failed".into());
        }

        unsafe {
            // Xlib's default handler exits the whole process for otherwise
            // recoverable errors (for example, a stale pixmap during teardown).
            x11::xlib::XSetErrorHandler(Some(super::ignore_x_error));
        }
        let screen_num = unsafe { x11::xlib::XDefaultScreen(xlib_display) };

        let backend_result = match preference {
            GraphicsApiPreference::Glx => GlxPlatform::new(
                xlib_display,
                screen_num,
                overlay_window,
                overlay_visual_id,
                hdr_enabled,
            )
            .map(PlatformBackend::Glx),
            GraphicsApiPreference::EglGles => {
                EglPlatform::new(xlib_display, overlay_window, overlay_visual_id, hdr_enabled)
                    .map(PlatformBackend::Egl)
            }
            GraphicsApiPreference::Auto => {
                match EglPlatform::new(xlib_display, overlay_window, overlay_visual_id, hdr_enabled)
                {
                    Ok(egl) => Ok(PlatformBackend::Egl(egl)),
                    Err(egl_error) => {
                        log::warn!(
                            "compositor: EGL/GLES initialization failed ({egl_error}); falling back to GLX"
                        );
                        GlxPlatform::new(
                            xlib_display,
                            screen_num,
                            overlay_window,
                            overlay_visual_id,
                            hdr_enabled,
                        )
                        .map(PlatformBackend::Glx)
                    }
                }
            }
        };
        let backend = match backend_result {
            Ok(backend) => backend,
            Err(error) => {
                unsafe { x11::xlib::XCloseDisplay(xlib_display) };
                return Err(error);
            }
        };

        let platform = Self {
            xlib_display,
            screen_num,
            backend,
            closed: false,
        };
        log::info!(
            "compositor: graphics API={} visual=0x{:x} hdr10={}",
            platform.api_name(),
            overlay_visual_id,
            platform.output_is_10bit()
        );
        Ok(platform)
    }

    pub(super) fn screen_num(&self) -> i32 {
        self.screen_num
    }

    pub(super) fn api_name(&self) -> &'static str {
        match self.backend {
            PlatformBackend::Glx(_) => "glx/opengl",
            PlatformBackend::Egl(_) => "egl/gles3",
        }
    }

    pub(super) fn output_is_10bit(&self) -> bool {
        match &self.backend {
            PlatformBackend::Glx(glx) => glx.output_is_10bit,
            PlatformBackend::Egl(egl) => egl.output_is_10bit,
        }
    }

    pub(super) fn is_gles(&self) -> bool {
        matches!(self.backend, PlatformBackend::Egl(_))
    }

    /// Return the number of frames since the current back buffer was defined.
    /// Zero means its contents cannot be reused and requires a full redraw.
    pub(super) fn partial_redraw_buffer_age(&self) -> u32 {
        match &self.backend {
            // Preserve the established GLX behavior. EGL explicitly verifies it.
            PlatformBackend::Glx(_) => 1,
            PlatformBackend::Egl(egl) => egl.buffer_age(),
        }
    }

    pub(super) fn supports_swap_with_damage(&self) -> bool {
        match &self.backend {
            PlatformBackend::Glx(_) => false,
            PlatformBackend::Egl(egl) => egl.swap_buffers_with_damage.get().is_some(),
        }
    }

    pub(super) fn make_current(&self) -> Result<(), String> {
        match &self.backend {
            PlatformBackend::Glx(glx) => glx.make_current(self.xlib_display),
            PlatformBackend::Egl(egl) => egl.make_current(),
        }
    }

    pub(super) fn swap_buffers(&self, damage: Option<&[i32]>) -> Result<(), String> {
        match &self.backend {
            PlatformBackend::Glx(glx) => glx.swap_buffers(self.xlib_display),
            PlatformBackend::Egl(egl) => egl.swap_buffers(damage),
        }
    }

    pub(super) fn get_proc_address(&self, name: &str) -> *const c_void {
        match &self.backend {
            PlatformBackend::Glx(glx) => glx.get_proc_address(name),
            PlatformBackend::Egl(egl) => egl.get_proc_address(name),
        }
    }

    pub(super) fn load_oml(&self) -> Option<OmlSyncControl> {
        match &self.backend {
            PlatformBackend::Glx(glx) => OmlSyncControl::load(self.xlib_display, glx.drawable),
            PlatformBackend::Egl(_) => None,
        }
    }

    /// Synchronize the secondary Xlib connection with the protocol backend and,
    /// on EGL, wait for native X rendering before GLES samples imported pixmaps.
    pub(super) fn sync_x11(&self) -> Result<(), String> {
        unsafe {
            x11::xlib::XSync(self.xlib_display, 0);
        }
        if let PlatformBackend::Egl(egl) = &self.backend {
            egl.wait_native()?;
        }
        Ok(())
    }

    pub(super) fn import_pixmap(
        &self,
        gl: &glow::Context,
        texture: glow::Texture,
        pixmap: u32,
        visual: u32,
        depth: u8,
        hdr_enabled: bool,
    ) -> Result<(PixmapBinding, bool), String> {
        match &self.backend {
            PlatformBackend::Glx(glx) => glx.import_pixmap(
                self.xlib_display,
                gl,
                texture,
                pixmap,
                visual,
                depth,
                hdr_enabled,
            ),
            PlatformBackend::Egl(egl) => egl.import_pixmap(gl, texture, pixmap, depth),
        }
    }

    pub(super) fn refresh_pixmap_binding(
        &self,
        gl: &glow::Context,
        texture: glow::Texture,
        binding: &PixmapBinding,
    ) -> Result<(), String> {
        match (&self.backend, binding) {
            (PlatformBackend::Glx(glx), PixmapBinding::Glx { drawable }) => {
                glx.refresh_pixmap(self.xlib_display, gl, texture, *drawable)
            }
            // EGLImages remain live siblings of the named X pixmap.  XSync +
            // eglWaitNative in sync_x11() makes newly rendered pixels visible;
            // no re-import is required for ordinary Damage events.
            (PlatformBackend::Egl(_), PixmapBinding::Egl { .. }) => Ok(()),
            _ => Err("window pixmap binding belongs to a different graphics API".into()),
        }
    }

    pub(super) fn release_pixmap_binding(
        &self,
        gl: &glow::Context,
        texture: glow::Texture,
        binding: PixmapBinding,
    ) {
        match (&self.backend, binding) {
            (PlatformBackend::Glx(glx), PixmapBinding::Glx { drawable }) => {
                glx.release_pixmap(self.xlib_display, gl, texture, drawable);
            }
            (PlatformBackend::Egl(egl), PixmapBinding::Egl { image }) => {
                egl.release_pixmap(image);
            }
            (PlatformBackend::Glx(_), PixmapBinding::Egl { .. })
            | (PlatformBackend::Egl(_), PixmapBinding::Glx { .. }) => {
                log::warn!("compositor: mismatched pixmap binding during cleanup");
            }
        }
    }

    pub(super) fn shutdown(&mut self) {
        if self.closed {
            return;
        }
        unsafe {
            match &mut self.backend {
                PlatformBackend::Glx(glx) => glx.shutdown(self.xlib_display),
                PlatformBackend::Egl(egl) => egl.shutdown(),
            }
            x11::xlib::XCloseDisplay(self.xlib_display);
        }
        self.xlib_display = ptr::null_mut();
        self.closed = true;
    }
}

impl Drop for GraphicsPlatform {
    fn drop(&mut self) {
        self.shutdown();
    }
}

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

struct GlxPlatform {
    context: x11::glx::GLXContext,
    drawable: x11::glx::GLXDrawable,
    tfp: TfpFunctions,
    fbconfig_rgba: x11::glx::GLXFBConfig,
    fbconfig_rgb: x11::glx::GLXFBConfig,
    tfp_visual_configs: HashMap<u32, (x11::glx::GLXFBConfig, bool)>,
    tfp_visual_configs_10bit: HashMap<u32, (x11::glx::GLXFBConfig, bool)>,
    output_is_10bit: bool,
}

impl GlxPlatform {
    fn new(
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
        if !ext_str.contains("GLX_EXT_texture_from_pixmap") {
            return Err("GLX_EXT_texture_from_pixmap not available".into());
        }
        log::info!("compositor: GLX extensions: {ext_str}");

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
            tfp,
            fbconfig_rgba,
            fbconfig_rgb,
            tfp_visual_configs,
            tfp_visual_configs_10bit,
            output_is_10bit,
        })
    }

    fn make_current(&self, display: *mut x11::xlib::Display) -> Result<(), String> {
        let ok = unsafe {
            x11::glx::glXMakeContextCurrent(display, self.drawable, self.drawable, self.context)
        };
        if ok == 0 {
            Err("glXMakeContextCurrent failed".into())
        } else {
            Ok(())
        }
    }

    fn swap_buffers(&self, display: *mut x11::xlib::Display) -> Result<(), String> {
        unsafe { x11::glx::glXSwapBuffers(display, self.drawable) };
        Ok(())
    }

    fn get_proc_address(&self, name: &str) -> *const c_void {
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

    fn import_pixmap(
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

    fn refresh_pixmap(
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

    fn release_pixmap(
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

    unsafe fn shutdown(&mut self, display: *mut x11::xlib::Display) {
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

// ---------------------------------------------------------------------------
// EGL / GLES 3 implementation
// ---------------------------------------------------------------------------

type EglDisplay = *mut c_void;
type EglConfig = *mut c_void;
type EglContext = *mut c_void;
type EglSurface = *mut c_void;
type EglImage = *mut c_void;
type EglBoolean = u32;
type EglEnum = u32;
type EglInt = i32;
type EglClientBuffer = *mut c_void;

type EglCreateImage = unsafe extern "C" fn(
    EglDisplay,
    EglContext,
    EglEnum,
    EglClientBuffer,
    *const EglInt,
) -> EglImage;
type EglDestroyImage = unsafe extern "C" fn(EglDisplay, EglImage) -> EglBoolean;
type EglSwapBuffersWithDamage =
    unsafe extern "C" fn(EglDisplay, EglSurface, *const EglInt, EglInt) -> EglBoolean;
type GlEglImageTargetTexture2dOes = unsafe extern "system" fn(u32, *const c_void);

const EGL_FALSE: EglBoolean = 0;
const EGL_TRUE: EglBoolean = 1;
const EGL_NONE: EglInt = 0x3038;
const EGL_RED_SIZE: EglInt = 0x3024;
const EGL_GREEN_SIZE: EglInt = 0x3023;
const EGL_BLUE_SIZE: EglInt = 0x3022;
const EGL_ALPHA_SIZE: EglInt = 0x3021;
const EGL_SURFACE_TYPE: EglInt = 0x3033;
const EGL_WINDOW_BIT: EglInt = 0x0004;
const EGL_RENDERABLE_TYPE: EglInt = 0x3040;
const EGL_OPENGL_ES3_BIT: EglInt = 0x0040;
const EGL_NATIVE_VISUAL_ID: EglInt = 0x302E;
const EGL_CONTEXT_CLIENT_VERSION: EglInt = 0x3098;
const EGL_EXTENSIONS: EglInt = 0x3055;
const EGL_OPENGL_ES_API: EglEnum = 0x30A0;
const EGL_NATIVE_PIXMAP_KHR: EglEnum = 0x30B0;
const EGL_IMAGE_PRESERVED_KHR: EglInt = 0x30D2;
const EGL_CORE_NATIVE_ENGINE: EglInt = 0x305B;
const EGL_SWAP_BEHAVIOR: EglInt = 0x3093;
const EGL_BUFFER_PRESERVED: EglInt = 0x3094;
const EGL_SWAP_BEHAVIOR_PRESERVED_BIT: EglInt = 0x0400;
const EGL_BUFFER_AGE_EXT: EglInt = 0x313D;

#[link(name = "EGL")]
unsafe extern "C" {
    fn eglGetDisplay(native_display: *mut c_void) -> EglDisplay;
    fn eglInitialize(display: EglDisplay, major: *mut EglInt, minor: *mut EglInt) -> EglBoolean;
    fn eglTerminate(display: EglDisplay) -> EglBoolean;
    fn eglBindAPI(api: EglEnum) -> EglBoolean;
    fn eglChooseConfig(
        display: EglDisplay,
        attribs: *const EglInt,
        configs: *mut EglConfig,
        config_size: EglInt,
        num_configs: *mut EglInt,
    ) -> EglBoolean;
    fn eglGetConfigAttrib(
        display: EglDisplay,
        config: EglConfig,
        attribute: EglInt,
        value: *mut EglInt,
    ) -> EglBoolean;
    fn eglCreateContext(
        display: EglDisplay,
        config: EglConfig,
        share_context: EglContext,
        attribs: *const EglInt,
    ) -> EglContext;
    fn eglDestroyContext(display: EglDisplay, context: EglContext) -> EglBoolean;
    fn eglCreateWindowSurface(
        display: EglDisplay,
        config: EglConfig,
        native_window: libc::c_ulong,
        attribs: *const EglInt,
    ) -> EglSurface;
    fn eglDestroySurface(display: EglDisplay, surface: EglSurface) -> EglBoolean;
    fn eglMakeCurrent(
        display: EglDisplay,
        draw: EglSurface,
        read: EglSurface,
        context: EglContext,
    ) -> EglBoolean;
    fn eglSwapBuffers(display: EglDisplay, surface: EglSurface) -> EglBoolean;
    fn eglSwapInterval(display: EglDisplay, interval: EglInt) -> EglBoolean;
    fn eglSurfaceAttrib(
        display: EglDisplay,
        surface: EglSurface,
        attribute: EglInt,
        value: EglInt,
    ) -> EglBoolean;
    fn eglQuerySurface(
        display: EglDisplay,
        surface: EglSurface,
        attribute: EglInt,
        value: *mut EglInt,
    ) -> EglBoolean;
    fn eglWaitNative(engine: EglInt) -> EglBoolean;
    fn eglQueryString(display: EglDisplay, name: EglInt) -> *const libc::c_char;
    fn eglGetError() -> EglInt;
    fn eglGetProcAddress(name: *const libc::c_char) -> *const c_void;
}

struct EglPlatform {
    display: EglDisplay,
    context: EglContext,
    surface: EglSurface,
    create_image: EglCreateImage,
    destroy_image: EglDestroyImage,
    image_target_texture: GlEglImageTargetTexture2dOes,
    swap_buffers_with_damage: Cell<Option<EglSwapBuffersWithDamage>>,
    buffer_age_supported: Cell<bool>,
    buffer_preserved: bool,
    gles_library: *mut c_void,
    output_is_10bit: bool,
}

impl EglPlatform {
    fn new(
        xlib_display: *mut x11::xlib::Display,
        overlay_window: u32,
        overlay_visual_id: u32,
        hdr_enabled: bool,
    ) -> Result<Self, String> {
        let display = unsafe { eglGetDisplay(xlib_display as *mut c_void) };
        if display.is_null() {
            return Err(egl_error("eglGetDisplay"));
        }
        let mut major = 0;
        let mut minor = 0;
        if unsafe { eglInitialize(display, &mut major, &mut minor) } == EGL_FALSE {
            return Err(egl_error("eglInitialize"));
        }
        log::info!("compositor: initialized EGL {major}.{minor}");

        let result = (|| {
            if unsafe { eglBindAPI(EGL_OPENGL_ES_API) } == EGL_FALSE {
                return Err(egl_error("eglBindAPI(EGL_OPENGL_ES_API)"));
            }

            let extensions = unsafe {
                let value = eglQueryString(display, EGL_EXTENSIONS);
                if value.is_null() {
                    ""
                } else {
                    CStr::from_ptr(value).to_str().unwrap_or("")
                }
            };
            let has_image_base = has_egl_extension(extensions, "EGL_KHR_image_base")
                || has_egl_extension(extensions, "EGL_KHR_image");
            if !has_image_base || !has_egl_extension(extensions, "EGL_KHR_image_pixmap") {
                return Err(format!(
                    "EGL image-pixmap import unavailable (extensions: {extensions})"
                ));
            }
            let buffer_age_supported = has_egl_extension(extensions, "EGL_EXT_buffer_age");
            let swap_buffers_with_damage: Option<EglSwapBuffersWithDamage> =
                if has_egl_extension(extensions, "EGL_KHR_swap_buffers_with_damage") {
                    let proc = egl_proc_any(&["eglSwapBuffersWithDamageKHR"]);
                    (!proc.is_null()).then(|| unsafe {
                        std::mem::transmute::<*const c_void, EglSwapBuffersWithDamage>(proc)
                    })
                } else if has_egl_extension(extensions, "EGL_EXT_swap_buffers_with_damage") {
                    let proc = egl_proc_any(&["eglSwapBuffersWithDamageEXT"]);
                    (!proc.is_null()).then(|| unsafe {
                        std::mem::transmute::<*const c_void, EglSwapBuffersWithDamage>(proc)
                    })
                } else {
                    None
                };

            let (config, output_is_10bit) =
                choose_egl_config(display, overlay_visual_id, hdr_enabled)?;
            let context_attrs = [EGL_CONTEXT_CLIENT_VERSION, 3, EGL_NONE];
            let context = unsafe {
                eglCreateContext(display, config, ptr::null_mut(), context_attrs.as_ptr())
            };
            if context.is_null() {
                return Err(egl_error("eglCreateContext(GLES 3)"));
            }

            let surface = unsafe {
                eglCreateWindowSurface(
                    display,
                    config,
                    overlay_window as libc::c_ulong,
                    [EGL_NONE].as_ptr(),
                )
            };
            if surface.is_null() {
                unsafe {
                    eglDestroyContext(display, context);
                }
                return Err(egl_error("eglCreateWindowSurface"));
            }
            if unsafe { eglMakeCurrent(display, surface, surface, context) } == EGL_FALSE {
                unsafe {
                    eglDestroySurface(display, surface);
                    eglDestroyContext(display, context);
                }
                return Err(egl_error("eglMakeCurrent"));
            }

            let mut surface_type = 0;
            let supports_preserved_swap =
                unsafe { eglGetConfigAttrib(display, config, EGL_SURFACE_TYPE, &mut surface_type) }
                    != EGL_FALSE
                    && surface_type & EGL_SWAP_BEHAVIOR_PRESERVED_BIT != 0;
            // Buffer age avoids the copy-back dependency introduced by preserved
            // swap. Keep preserved swap only as the compatibility fallback when
            // EGL_EXT_buffer_age is unavailable.
            let buffer_preserved = if !buffer_age_supported && supports_preserved_swap {
                let preserved = unsafe {
                    eglSurfaceAttrib(display, surface, EGL_SWAP_BEHAVIOR, EGL_BUFFER_PRESERVED)
                } != EGL_FALSE;
                if !preserved {
                    log::debug!(
                        "compositor: EGL preserved swap request failed: {}",
                        egl_error("eglSurfaceAttrib(EGL_SWAP_BEHAVIOR)")
                    );
                }
                preserved
            } else {
                false
            };
            log::info!(
                "compositor: EGL partial redraw buffer_age={} preserved_back_buffer={} swap_with_damage={}",
                buffer_age_supported,
                buffer_preserved,
                swap_buffers_with_damage.is_some()
            );

            if unsafe { eglSwapInterval(display, 1) } == EGL_FALSE {
                log::warn!(
                    "compositor: eglSwapInterval(1) failed: {}",
                    egl_error("EGL")
                );
            }

            let create_image_ptr = egl_proc_any(&["eglCreateImageKHR"]);
            let destroy_image_ptr = egl_proc_any(&["eglDestroyImageKHR"]);
            let image_target_ptr = egl_proc_any(&["glEGLImageTargetTexture2DOES"]);
            if create_image_ptr.is_null()
                || destroy_image_ptr.is_null()
                || image_target_ptr.is_null()
            {
                unsafe {
                    eglMakeCurrent(display, ptr::null_mut(), ptr::null_mut(), ptr::null_mut());
                    eglDestroySurface(display, surface);
                    eglDestroyContext(display, context);
                }
                return Err(
                    "eglCreateImageKHR/eglDestroyImageKHR/glEGLImageTargetTexture2DOES unavailable"
                        .into(),
                );
            }

            let gles_library = open_gles_library();
            let probe_gl = unsafe {
                glow::Context::from_loader_function(|name| {
                    let Ok(name) = CString::new(name) else {
                        return ptr::null();
                    };
                    let proc = eglGetProcAddress(name.as_ptr());
                    if !proc.is_null() {
                        proc
                    } else if gles_library.is_null() {
                        ptr::null()
                    } else {
                        libc::dlsym(gles_library, name.as_ptr()) as *const c_void
                    }
                })
            };
            if !probe_gl.supported_extensions().contains("GL_OES_EGL_image") {
                drop(probe_gl);
                unsafe {
                    eglMakeCurrent(display, ptr::null_mut(), ptr::null_mut(), ptr::null_mut());
                    eglDestroySurface(display, surface);
                    eglDestroyContext(display, context);
                    if !gles_library.is_null() {
                        libc::dlclose(gles_library);
                    }
                }
                return Err("OpenGL ES context does not advertise GL_OES_EGL_image".into());
            }
            drop(probe_gl);
            Ok(Self {
                display,
                context,
                surface,
                create_image: unsafe { std::mem::transmute(create_image_ptr) },
                destroy_image: unsafe { std::mem::transmute(destroy_image_ptr) },
                image_target_texture: unsafe { std::mem::transmute(image_target_ptr) },
                swap_buffers_with_damage: Cell::new(swap_buffers_with_damage),
                buffer_age_supported: Cell::new(buffer_age_supported),
                buffer_preserved,
                gles_library,
                output_is_10bit,
            })
        })();

        if result.is_err() {
            unsafe {
                eglTerminate(display);
            }
        }
        result
    }

    fn make_current(&self) -> Result<(), String> {
        if unsafe { eglMakeCurrent(self.display, self.surface, self.surface, self.context) }
            == EGL_FALSE
        {
            Err(egl_error("eglMakeCurrent"))
        } else {
            Ok(())
        }
    }

    fn buffer_age(&self) -> u32 {
        if self.buffer_preserved {
            return 1;
        }
        if !self.buffer_age_supported.get() {
            return 0;
        }

        let mut age = 0;
        if unsafe { eglQuerySurface(self.display, self.surface, EGL_BUFFER_AGE_EXT, &mut age) }
            == EGL_FALSE
        {
            // A few drivers expose the extension globally but reject it for a
            // particular surface. Downgrade once and use full redraws instead of
            // repeating a failing query every frame.
            self.buffer_age_supported.set(false);
            log::warn!(
                "compositor: EGL buffer-age query failed; disabling partial redraw for this surface: {}",
                egl_error("eglQuerySurface(EGL_BUFFER_AGE_EXT)")
            );
            return 0;
        }
        u32::try_from(age).unwrap_or(0)
    }

    fn swap_buffers(&self, damage: Option<&[EglInt]>) -> Result<(), String> {
        if let (Some(swap_with_damage), Some(rects)) = (self.swap_buffers_with_damage.get(), damage)
            && let Some(rect_count) = egl_damage_rect_count(rects)
        {
            if unsafe { swap_with_damage(self.display, self.surface, rects.as_ptr(), rect_count) }
                != EGL_FALSE
            {
                return Ok(());
            }
            // Some drivers advertise the extension but reject it for a specific
            // surface. Disable the path after its first failure so every future
            // frame does not repeat a failed entry point and eglGetError call.
            self.swap_buffers_with_damage.set(None);
            log::warn!(
                "compositor: EGL swap-with-damage failed; disabling it for this surface: {}",
                egl_error("eglSwapBuffersWithDamage")
            );
        }
        if unsafe { eglSwapBuffers(self.display, self.surface) } == EGL_FALSE {
            Err(egl_error("eglSwapBuffers"))
        } else {
            Ok(())
        }
    }

    fn wait_native(&self) -> Result<(), String> {
        if unsafe { eglWaitNative(EGL_CORE_NATIVE_ENGINE) } == EGL_FALSE {
            Err(egl_error("eglWaitNative"))
        } else {
            Ok(())
        }
    }

    fn get_proc_address(&self, name: &str) -> *const c_void {
        let Ok(name) = CString::new(name) else {
            return ptr::null();
        };
        let proc = unsafe { eglGetProcAddress(name.as_ptr()) };
        if !proc.is_null() {
            return proc;
        }
        if self.gles_library.is_null() {
            ptr::null()
        } else {
            unsafe { libc::dlsym(self.gles_library, name.as_ptr()) as *const c_void }
        }
    }

    fn import_pixmap(
        &self,
        gl: &glow::Context,
        texture: glow::Texture,
        pixmap: u32,
        depth: u8,
    ) -> Result<(PixmapBinding, bool), String> {
        let preserved = [EGL_IMAGE_PRESERVED_KHR, EGL_TRUE as EglInt, EGL_NONE];
        let client_buffer = pixmap as usize as EglClientBuffer;
        let mut image = unsafe {
            (self.create_image)(
                self.display,
                ptr::null_mut(),
                EGL_NATIVE_PIXMAP_KHR,
                client_buffer,
                preserved.as_ptr(),
            )
        };
        if image.is_null() {
            image = unsafe {
                (self.create_image)(
                    self.display,
                    ptr::null_mut(),
                    EGL_NATIVE_PIXMAP_KHR,
                    client_buffer,
                    [EGL_NONE].as_ptr(),
                )
            };
        }
        if image.is_null() {
            return Err(egl_error(&format!(
                "eglCreateImageKHR(native pixmap 0x{pixmap:x})"
            )));
        }

        unsafe {
            // Clear a stale error so the check below reports this import only.
            while gl.get_error() != glow::NO_ERROR {}
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            (self.image_target_texture)(glow::TEXTURE_2D, image as *const c_void);
            gl.bind_texture(glow::TEXTURE_2D, None);
            let error = gl.get_error();
            if error != glow::NO_ERROR {
                (self.destroy_image)(self.display, image);
                return Err(format!(
                    "glEGLImageTargetTexture2DOES failed with GL error 0x{error:x}"
                ));
            }
        }

        Ok((PixmapBinding::Egl { image }, depth == 32))
    }

    fn release_pixmap(&self, image: EglImage) {
        if !image.is_null() && unsafe { (self.destroy_image)(self.display, image) } == EGL_FALSE {
            log::warn!(
                "compositor: eglDestroyImageKHR failed: {}",
                egl_error("EGL")
            );
        }
    }

    unsafe fn shutdown(&mut self) {
        if self.display.is_null() {
            return;
        }
        unsafe {
            eglMakeCurrent(
                self.display,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            );
            if !self.surface.is_null() {
                eglDestroySurface(self.display, self.surface);
            }
            if !self.context.is_null() {
                eglDestroyContext(self.display, self.context);
            }
            eglTerminate(self.display);
            if !self.gles_library.is_null() {
                libc::dlclose(self.gles_library);
            }
        }
        self.surface = ptr::null_mut();
        self.context = ptr::null_mut();
        self.display = ptr::null_mut();
        self.gles_library = ptr::null_mut();
    }
}

fn egl_damage_rect_count(rects: &[EglInt]) -> Option<EglInt> {
    if rects.is_empty() || !rects.len().is_multiple_of(4) {
        return None;
    }
    let count = rects.len() / 4;
    EglInt::try_from(count).ok()
}

fn has_egl_extension(extensions: &str, expected: &str) -> bool {
    extensions
        .split_ascii_whitespace()
        .any(|extension| extension == expected)
}

pub(super) fn append_egl_damage_rect(
    output: &mut Vec<EglInt>,
    surface_height: u32,
    dirty: DirtyRect,
) {
    let (Ok(surface_height), Ok(width), Ok(height)) = (
        EglInt::try_from(surface_height),
        EglInt::try_from(dirty.width),
        EglInt::try_from(dirty.height),
    ) else {
        return;
    };
    output.extend_from_slice(&[dirty.x, surface_height - dirty.y - height, width, height]);
}

fn choose_egl_config(
    display: EglDisplay,
    overlay_visual_id: u32,
    hdr_enabled: bool,
) -> Result<(EglConfig, bool), String> {
    if hdr_enabled {
        if let Some(config) = find_egl_config(display, overlay_visual_id, 10) {
            return Ok((config, true));
        }
        log::warn!("compositor: no matching 10-bit EGL config; falling back to 8-bit");
    }
    find_egl_config(display, overlay_visual_id, 8)
        .map(|config| (config, false))
        .ok_or_else(|| {
            format!("no EGL/GLES window config matching overlay visual 0x{overlay_visual_id:x}")
        })
}

fn find_egl_config(display: EglDisplay, visual_id: u32, bits: EglInt) -> Option<EglConfig> {
    let alpha = if bits == 10 { 2 } else { 0 };
    let attrs = [
        EGL_SURFACE_TYPE,
        EGL_WINDOW_BIT,
        EGL_RENDERABLE_TYPE,
        EGL_OPENGL_ES3_BIT,
        EGL_RED_SIZE,
        bits,
        EGL_GREEN_SIZE,
        bits,
        EGL_BLUE_SIZE,
        bits,
        EGL_ALPHA_SIZE,
        alpha,
        EGL_NONE,
    ];
    let mut count = 0;
    if unsafe { eglChooseConfig(display, attrs.as_ptr(), ptr::null_mut(), 0, &mut count) }
        == EGL_FALSE
        || count == 0
    {
        return None;
    }
    let mut configs = vec![ptr::null_mut(); count as usize];
    if unsafe {
        eglChooseConfig(
            display,
            attrs.as_ptr(),
            configs.as_mut_ptr(),
            count,
            &mut count,
        )
    } == EGL_FALSE
    {
        return None;
    }
    let mut fallback = None;
    for config in configs.into_iter().take(count as usize) {
        let mut native_visual = 0;
        let visual_matches = unsafe {
            eglGetConfigAttrib(display, config, EGL_NATIVE_VISUAL_ID, &mut native_visual)
        } != EGL_FALSE
            && native_visual as u32 == visual_id;
        if !visual_matches {
            continue;
        }
        if fallback.is_none() {
            fallback = Some(config);
        }
        let mut surface_type = 0;
        let preserves_back_buffer =
            unsafe { eglGetConfigAttrib(display, config, EGL_SURFACE_TYPE, &mut surface_type) }
                != EGL_FALSE
                && surface_type & EGL_SWAP_BEHAVIOR_PRESERVED_BIT != 0;
        if preserves_back_buffer {
            return Some(config);
        }
    }
    fallback
}

fn egl_proc_any(names: &[&str]) -> *const c_void {
    for name in names {
        let Ok(name) = CString::new(*name) else {
            continue;
        };
        let proc = unsafe { eglGetProcAddress(name.as_ptr()) };
        if !proc.is_null() {
            return proc;
        }
    }
    ptr::null()
}

fn open_gles_library() -> *mut c_void {
    for library in ["libGLESv2.so.2", "libGLESv2.so"] {
        let Ok(library) = CString::new(library) else {
            continue;
        };
        let handle = unsafe { libc::dlopen(library.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if !handle.is_null() {
            return handle;
        }
    }
    ptr::null_mut()
}

fn egl_error(operation: &str) -> String {
    let error = unsafe { eglGetError() };
    format!("{operation} failed (EGL error 0x{error:04x})")
}

#[cfg(test)]
mod tests {
    use super::{
        GraphicsApiPreference, append_egl_damage_rect, egl_damage_rect_count, has_egl_extension,
    };
    use crate::backend::x11::compositor::DirtyRect;

    #[test]
    fn parses_graphics_api_aliases() {
        assert_eq!(
            GraphicsApiPreference::parse("auto").unwrap(),
            GraphicsApiPreference::Auto
        );
        assert_eq!(
            GraphicsApiPreference::parse("egl").unwrap(),
            GraphicsApiPreference::EglGles
        );
        assert_eq!(
            GraphicsApiPreference::parse("gles").unwrap(),
            GraphicsApiPreference::EglGles
        );
        assert_eq!(
            GraphicsApiPreference::parse("glx").unwrap(),
            GraphicsApiPreference::Glx
        );
        assert!(GraphicsApiPreference::parse("vulkan").is_err());
    }

    #[test]
    fn counts_flattened_egl_damage_rectangles() {
        assert_eq!(egl_damage_rect_count(&[1, 2, 30, 40]), Some(1));
        assert_eq!(
            egl_damage_rect_count(&[1, 2, 30, 40, 100, 200, 5, 6]),
            Some(2)
        );
        assert_eq!(egl_damage_rect_count(&[]), None);
        assert_eq!(egl_damage_rect_count(&[1, 2, 3]), None);
    }

    #[test]
    fn converts_disjoint_damage_to_egl_bottom_left_coordinates() {
        let mut output = Vec::new();
        append_egl_damage_rect(&mut output, 600, DirtyRect::new(10, 20, 30, 40));
        append_egl_damage_rect(&mut output, 600, DirtyRect::new(500, 400, 20, 50));

        assert_eq!(output, [10, 540, 30, 40, 500, 150, 20, 50]);
        assert_eq!(egl_damage_rect_count(&output), Some(2));
    }

    #[test]
    fn matches_complete_egl_extension_names() {
        let extensions = "EGL_EXT_buffer_age EGL_KHR_image_base EGL_KHR_swap_buffers_with_damage";

        assert!(has_egl_extension(extensions, "EGL_EXT_buffer_age"));
        assert!(has_egl_extension(extensions, "EGL_KHR_image_base"));
        assert!(!has_egl_extension(extensions, "EGL_KHR_image"));
        assert!(!has_egl_extension(extensions, "EGL_EXT_buffer"));
    }
}
