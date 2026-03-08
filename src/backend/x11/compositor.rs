use glow::HasContext;
use std::collections::HashMap;
use std::ffi::CString;
use std::sync::Arc;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::composite::ConnectionExt as CompositeExt;
use x11rb::protocol::damage::{self, ConnectionExt as DamageExt};
use x11rb::protocol::xfixes::ConnectionExt as XFixesExt;
use x11rb::protocol::xproto::ConnectionExt as XProtoExt;
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

/// Parsed opacity rule: "opacity_percent:class_name"
#[derive(Clone)]
struct OpacityRule {
    opacity: f32, // 0.0..1.0
    class_name: String,
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
}

// Safety: The compositor is only accessed from the single-threaded X11 event loop.
// All raw pointers (Display*, GLXContext, etc.) are only used from that thread.
unsafe impl Send for Compositor {}

impl Drop for Compositor {
    fn drop(&mut self) {
        unsafe {
            self.gl.delete_program(self.program);
            self.gl.delete_program(self.shadow_program);
            self.gl.delete_program(self.blur_down_program);
            self.gl.delete_program(self.blur_up_program);
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
        }
        let wins: Vec<u32> = self.windows.keys().copied().collect();
        for w in wins {
            self.remove_window(w);
        }
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

        Ok(Self {
            conn,
            xlib_display,
            tfp,
            glx_context,
            fbconfig_rgba,
            fbconfig_rgb,
            tfp_visual_configs,
            overlay_window,
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
        false
    }

    pub(super) fn overlay_window(&self) -> u32 {
        self.overlay_window
    }

    pub(super) fn clear_needs_render(&mut self) {
        self.needs_render = false;
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
    }

    pub(super) fn remove_window(&mut self, x11_win: u32) {
        // If fading is enabled and the window exists, start fade-out instead of immediate remove
        if self.fading {
            if let Some(wt) = self.windows.get_mut(&x11_win) {
                if !wt.fading_out && wt.fade_opacity > 0.0 {
                    wt.fading_out = true;
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
            wt.x = x;
            wt.y = y;
            self.needs_render = true;

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
        // Look up opacity rule before borrowing windows mutably
        let opacity_override = self.lookup_opacity_rule(class_name);
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            if wt.class_name != class_name {
                wt.class_name = class_name.to_string();
                wt.opacity_override = opacity_override;
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
        if !self.fading {
            return false;
        }
        let mut any_active = false;
        let mut to_remove = Vec::new();

        for (&win, wt) in self.windows.iter_mut() {
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

        // Fullscreen unredirect check
        if self.check_fullscreen_unredirect(scene, focused) {
            return false;
        }

        // Tick fade animations
        let fades_active = self.tick_fades();

        // Skip-unchanged-frame: if scene hasn't changed and no textures are
        // dirty, we can skip the entire GL render.
        let has_dirty = scene.iter().any(|&(win, _, _, _, _)| {
            self.windows.get(&win).map_or(false, |wt| wt.dirty || wt.needs_pixmap_refresh)
        });
        let hash = Self::scene_hash(scene, focused);
        if !has_dirty && !fades_active && hash == self.last_scene_hash {
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

        // Refresh dirty textures
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

        let visible_scene = &scene[first_visible..];

        // === Pass 1: Draw shadows ===
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

                self.gl.uniform_4_f32(
                    self.shadow_uniforms.shadow_color.as_ref(), sr, sg, sb, sa,
                );
                self.gl.uniform_1_f32(
                    self.shadow_uniforms.spread.as_ref(), spread,
                );
                self.gl.uniform_1_f32(
                    self.shadow_uniforms.radius.as_ref(), self.corner_radius,
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
                    // Fade: modulate shadow alpha
                    let fade = wt.fade_opacity;
                    let sa_faded = sa * fade;
                    if sa_faded <= 0.0 { continue; }

                    self.gl.uniform_4_f32(
                        self.shadow_uniforms.shadow_color.as_ref(), sr, sg, sb, sa_faded,
                    );

                    let sx = x as f32 + ox - spread;
                    let sy = y as f32 + oy - spread;
                    let sw = w as f32 + 2.0 * spread;
                    let sh = h as f32 + 2.0 * spread;
                    self.gl.uniform_4_f32(
                        self.shadow_uniforms.rect.as_ref(), sx, sy, sw, sh,
                    );
                    self.gl.uniform_2_f32(
                        self.shadow_uniforms.size.as_ref(), w as f32, h as f32,
                    );
                    self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                }

                self.gl.bind_vertex_array(None);
                self.gl.use_program(None);
            }
        }

        // === Pass 1.5: Background blur ===
        // We need to capture the current scene (shadows + background) into an FBO,
        // then run the Kawase blur passes on it, and draw the blurred result
        // behind translucent windows.
        let has_blur_windows = self.blur_enabled
            && !self.blur_fbos.is_empty()
            && self.scene_fbo.is_some()
            && visible_scene.iter().any(|&(win, _, _, _, _)| {
                self.windows.get(&win).map_or(false, |wt| {
                    (wt.has_rgba || wt.fade_opacity < 1.0 || wt.opacity_override.is_some())
                        && !Self::class_matches_exclude(&wt.class_name, &self.blur_exclude)
                })
            });

        let blur_texture = if has_blur_windows {
            self.run_blur_passes(&proj)
        } else {
            None
        };

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

                    // Per-window corner radius (exclude check)
                    let radius = if Self::class_matches_exclude(&wt.class_name, &self.rounded_corners_exclude) {
                        0.0
                    } else {
                        self.corner_radius
                    };
                    self.gl.uniform_1_f32(self.win_uniforms.radius.as_ref(), radius);

                    // Compute effective opacity
                    let base_opacity = if is_focused { self.active_opacity } else { self.inactive_opacity };
                    let rule_opacity = wt.opacity_override.unwrap_or(base_opacity);
                    let dim = rule_opacity * fade;

                    // detect_client_opacity: if window manages its own alpha, don't force opacity
                    let opacity = if wt.has_rgba {
                        if self.detect_client_opacity {
                            // Let the window's own alpha through, just multiply by dim
                            -dim
                        } else {
                            -1.0f32 * fade
                        }
                    } else {
                        dim
                    };

                    // Draw blurred background behind translucent windows
                    if let Some(blur_tex) = blur_texture {
                        let needs_blur = (wt.has_rgba || fade < 1.0 || wt.opacity_override.map_or(false, |o| o < 1.0))
                            && !Self::class_matches_exclude(&wt.class_name, &self.blur_exclude);
                        if needs_blur {
                            // Draw the blurred scene texture in the window area
                            self.gl.active_texture(glow::TEXTURE0);
                            self.gl.bind_texture(glow::TEXTURE_2D, Some(blur_tex));
                            self.gl.uniform_1_f32(self.win_uniforms.opacity.as_ref(), fade);
                            self.gl.uniform_1_f32(self.win_uniforms.dim.as_ref(), 1.0);
                            self.gl.uniform_1_f32(self.win_uniforms.radius.as_ref(), radius);
                            self.gl.uniform_2_f32(self.win_uniforms.size.as_ref(), w as f32, h as f32);
                            self.gl.uniform_4_f32(
                                self.win_uniforms.rect.as_ref(),
                                x as f32, y as f32, w as f32, h as f32,
                            );
                            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                        }
                    }

                    self.gl.uniform_1_f32(self.win_uniforms.opacity.as_ref(), opacity);
                    self.gl.uniform_1_f32(self.win_uniforms.dim.as_ref(), dim);
                    self.gl.uniform_2_f32(
                        self.win_uniforms.size.as_ref(), w as f32, h as f32,
                    );
                    self.gl.uniform_4_f32(
                        self.win_uniforms.rect.as_ref(),
                        x as f32, y as f32, w as f32, h as f32,
                    );
                    self.gl.active_texture(glow::TEXTURE0);
                    self.gl.bind_texture(glow::TEXTURE_2D, Some(wt.gl_texture));
                    self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                }
            }

            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
        }

        // Swap buffers (double-buffered with vsync for tear-free output).
        unsafe {
            x11::glx::glXSwapBuffers(self.xlib_display, self.glx_drawable);
        }

        // Schedule re-render if fades are still in progress
        if fades_active {
            self.needs_render = true;
        }

        true
    }

    /// Capture the current framebuffer into scene_fbo, then run dual Kawase blur passes.
    /// Returns the texture containing the final blurred result.
    fn run_blur_passes(&self, _proj: &[f32; 16]) -> Option<glow::Texture> {
        let (scene_fbo, scene_tex) = self.scene_fbo.as_ref()?;
        if self.blur_fbos.is_empty() {
            return None;
        }

        unsafe {
            // Copy current framebuffer to scene FBO
            self.gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None); // read from default (back buffer)
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
