//! EGL/GLES 3 platform adapter for the shared X11 compositor.
//!
//! Owns the EGL display, context, window surface, the loaded GLES library
//! handle, and every `EGLImage` created for pixmap imports, plus the
//! damage/buffer-age extension entry points. All EGL symbols and resource
//! lifetimes stay in this file; the facade in the parent module dispatches
//! into it.

use super::super::{DirtyRect, PixmapBinding};
use glow::HasContext;
use std::cell::Cell;
use std::ffi::{CStr, CString, c_void};
use std::ptr;

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
type EglSetDamageRegion =
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

pub(super) struct EglPlatform {
    display: EglDisplay,
    context: EglContext,
    surface: EglSurface,
    create_image: EglCreateImage,
    destroy_image: EglDestroyImage,
    image_target_texture: GlEglImageTargetTexture2dOes,
    pub(super) swap_buffers_with_damage: Cell<Option<EglSwapBuffersWithDamage>>,
    set_damage_region: Cell<Option<EglSetDamageRegion>>,
    buffer_age_supported: Cell<bool>,
    ext_buffer_age_supported: bool,
    buffer_preserved: bool,
    gles_library: *mut c_void,
    pub(super) output_is_10bit: bool,
}

impl EglPlatform {
    pub(super) fn new(
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
            let partial_update_advertised = has_egl_extension(extensions, "EGL_KHR_partial_update");
            // KHR_partial_update exposes the same buffer-age query token and
            // semantics even when EXT_buffer_age is not separately advertised.
            let ext_buffer_age_supported = has_egl_extension(extensions, "EGL_EXT_buffer_age");
            let set_damage_region: Option<EglSetDamageRegion> = if partial_update_advertised {
                let proc = egl_proc_any(&["eglSetDamageRegionKHR"]);
                (!proc.is_null()).then(|| unsafe {
                    std::mem::transmute::<*const c_void, EglSetDamageRegion>(proc)
                })
            } else {
                None
            };
            let buffer_age_supported = ext_buffer_age_supported || set_damage_region.is_some();
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
                "compositor: EGL partial redraw buffer_age={} partial_update={} preserved_back_buffer={} swap_with_damage={}",
                buffer_age_supported,
                set_damage_region.is_some(),
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
                set_damage_region: Cell::new(set_damage_region),
                buffer_age_supported: Cell::new(buffer_age_supported),
                ext_buffer_age_supported,
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

    pub(super) fn make_current(&self) -> Result<(), String> {
        if unsafe { eglMakeCurrent(self.display, self.surface, self.surface, self.context) }
            == EGL_FALSE
        {
            Err(egl_error("eglMakeCurrent"))
        } else {
            Ok(())
        }
    }

    pub(super) fn buffer_age(&self) -> u32 {
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
            self.set_damage_region.set(None);
            log::warn!(
                "compositor: EGL buffer-age query failed; disabling partial redraw for this surface: {}",
                egl_error("eglQuerySurface(EGL_BUFFER_AGE_EXT)")
            );
            return 0;
        }
        u32::try_from(age).unwrap_or(0)
    }

    pub(super) fn set_damage_region(&self, damage: &[EglInt]) -> bool {
        let Some(set_damage_region) = self.set_damage_region.get() else {
            return self.ext_buffer_age_supported;
        };
        let Some(rect_count) = egl_damage_rect_count(damage) else {
            return self.ext_buffer_age_supported;
        };
        if unsafe { set_damage_region(self.display, self.surface, damage.as_ptr(), rect_count) }
            != EGL_FALSE
        {
            return true;
        }

        // EXT_buffer_age remains sufficient on its own. A KHR-only path must
        // stop reusing buffers because content inside the default full damage
        // region would otherwise be undefined.
        self.set_damage_region.set(None);
        if !self.ext_buffer_age_supported {
            self.buffer_age_supported.set(false);
        }
        log::warn!(
            "compositor: EGL partial-update failed; disabling it for this surface: {}",
            egl_error("eglSetDamageRegionKHR")
        );
        self.ext_buffer_age_supported
    }

    pub(super) fn swap_buffers(&self, damage: Option<&[EglInt]>) -> Result<(), String> {
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

    pub(super) fn wait_native(&self) -> Result<(), String> {
        if unsafe { eglWaitNative(EGL_CORE_NATIVE_ENGINE) } == EGL_FALSE {
            Err(egl_error("eglWaitNative"))
        } else {
            Ok(())
        }
    }

    pub(super) fn get_proc_address(&self, name: &str) -> *const c_void {
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

    pub(super) fn import_pixmap(
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

    pub(super) fn release_pixmap(&self, image: EglImage) {
        if !image.is_null() && unsafe { (self.destroy_image)(self.display, image) } == EGL_FALSE {
            log::warn!(
                "compositor: eglDestroyImageKHR failed: {}",
                egl_error("EGL")
            );
        }
    }

    pub(super) unsafe fn shutdown(&mut self) {
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

pub(in crate::backend::x11::compositor) fn append_egl_damage_rect(
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
    use super::{append_egl_damage_rect, egl_damage_rect_count, has_egl_extension};
    use crate::backend::x11::compositor::DirtyRect;

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
        let extensions = "EGL_EXT_buffer_age EGL_KHR_image_base EGL_KHR_partial_update EGL_KHR_swap_buffers_with_damage";

        assert!(has_egl_extension(extensions, "EGL_EXT_buffer_age"));
        assert!(has_egl_extension(extensions, "EGL_KHR_image_base"));
        assert!(has_egl_extension(extensions, "EGL_KHR_partial_update"));
        assert!(!has_egl_extension(extensions, "EGL_KHR_image"));
        assert!(!has_egl_extension(extensions, "EGL_EXT_buffer"));
    }
}
