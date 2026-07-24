//! GLX and EGL/OpenGL ES platform integration for the shared X11 compositor.
//!
//! The x11rb and xcb backends both feed the same compositor implementation, so
//! the graphics API selection belongs here rather than in either protocol
//! backend.  Window contents continue to come from XComposite named pixmaps:
//!
//! * GLX imports them through `GLX_EXT_texture_from_pixmap`.
//! * EGL/GLES imports them through `EGL_KHR_image_pixmap` + `GL_OES_EGL_image`.

use self::egl::EglPlatform;
use self::glx::GlxPlatform;
use super::{OmlSyncControl, PixmapBinding};
use std::ffi::c_void;
use std::ptr;

/// EGL/GLES 3 platform adapter: context, surface, and EGLImage ownership.
mod egl;
/// GLX platform adapter: context, drawable, and TFP pixmap ownership.
mod glx;

pub(super) use self::egl::append_egl_damage_rect;

// ---------------------------------------------------------------------------
// Public selection surface
// ---------------------------------------------------------------------------

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
    cursor_capture_supported: bool,
    backend: PlatformBackend,
    closed: bool,
}

enum PlatformBackend {
    Glx(GlxPlatform),
    Egl(EglPlatform),
}

pub(super) struct RecordingCursor {
    pixels: Vec<u32>,
    width: u32,
    height: u32,
    hotspot_x: i32,
    hotspot_y: i32,
    xhot: i32,
    yhot: i32,
}

impl RecordingCursor {
    pub(super) fn composite_into(
        &self,
        rgba: &mut [u8],
        width: u32,
        height: u32,
        source_region: (i32, i32, u32, u32),
    ) {
        composite_premultiplied_argb_cursor(
            rgba,
            width,
            height,
            &self.pixels,
            self.width,
            self.height,
            self.hotspot_x,
            self.hotspot_y,
            self.xhot,
            self.yhot,
            source_region,
        );
    }
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
        let cursor_capture_supported = unsafe {
            let mut event_base = 0;
            let mut error_base = 0;
            x11::xfixes::XFixesQueryExtension(xlib_display, &mut event_base, &mut error_base) != 0
        };

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
            cursor_capture_supported,
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
            PlatformBackend::Glx(glx) => glx.buffer_age(self.xlib_display),
            PlatformBackend::Egl(egl) => egl.buffer_age(),
        }
    }

    pub(super) fn supports_swap_with_damage(&self) -> bool {
        match &self.backend {
            PlatformBackend::Glx(_) => false,
            PlatformBackend::Egl(egl) => egl.swap_buffers_with_damage.get().is_some(),
        }
    }

    /// Tell EGL which pixels of the current back buffer will be repaired. The
    /// return value says whether partial rendering remains safe for this frame.
    pub(super) fn set_damage_region(&self, damage: &[i32]) -> bool {
        match &self.backend {
            PlatformBackend::Glx(_) => true,
            PlatformBackend::Egl(egl) => egl.set_damage_region(damage),
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

    /// Synchronize native X rendering before GLX/EGL samples imported pixmaps.
    ///
    /// GLX_EXT_texture_from_pixmap does not provide implicit synchronization
    /// with X rendering. EGL additionally requires eglWaitNative after XSync.
    pub(super) fn sync_x11(&self) -> Result<(), String> {
        unsafe {
            x11::xlib::XSync(self.xlib_display, 0);
        }
        match &self.backend {
            PlatformBackend::Glx(_) => unsafe {
                // Order completed X rendering before subsequent GL texture
                // sampling. GLX_EXT_texture_from_pixmap deliberately leaves
                // this producer/consumer synchronization to the application.
                x11::glx::glXWaitX();
            },
            PlatformBackend::Egl(egl) => egl.wait_native()?,
        }
        Ok(())
    }

    /// Composite the X server cursor into a bottom-up RGBA readback buffer.
    ///
    /// XComposite redirects windows, but the X server cursor is a separate
    /// sprite and is consequently absent from `glReadPixels`.  XFixes exposes
    /// the current premultiplied ARGB cursor image and its exact root position,
    /// allowing recording to add it without affecting the on-screen back
    /// buffer (where the server still draws the real cursor).
    pub(super) fn capture_recording_cursor(&self) -> Option<RecordingCursor> {
        if !self.cursor_capture_supported || self.xlib_display.is_null() {
            return None;
        }

        unsafe {
            let image = x11::xfixes::XFixesGetCursorImage(self.xlib_display);
            if image.is_null() {
                return None;
            }

            let image_ref = &*image;
            let pixel_count =
                usize::from(image_ref.width).checked_mul(usize::from(image_ref.height));
            let snapshot = pixel_count
                .filter(|_| !image_ref.pixels.is_null())
                .map(|pixel_count| RecordingCursor {
                    pixels: std::slice::from_raw_parts(image_ref.pixels, pixel_count)
                        .iter()
                        .map(|&pixel| pixel as u32)
                        .collect(),
                    width: u32::from(image_ref.width),
                    height: u32::from(image_ref.height),
                    hotspot_x: i32::from(image_ref.x),
                    hotspot_y: i32::from(image_ref.y),
                    xhot: i32::from(image_ref.xhot),
                    yhot: i32::from(image_ref.yhot),
                });

            x11::xlib::XFree(image.cast());
            snapshot
        }
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

#[allow(clippy::too_many_arguments)]
fn composite_premultiplied_argb_cursor(
    frame: &mut [u8],
    frame_width: u32,
    frame_height: u32,
    cursor: &[u32],
    cursor_width: u32,
    cursor_height: u32,
    hotspot_x: i32,
    hotspot_y: i32,
    xhot: i32,
    yhot: i32,
    source_region: (i32, i32, u32, u32),
) {
    let Some(frame_len) = usize::try_from(frame_width)
        .ok()
        .and_then(|w| {
            usize::try_from(frame_height)
                .ok()
                .and_then(|h| w.checked_mul(h))
        })
        .and_then(|pixels| pixels.checked_mul(4))
    else {
        return;
    };
    if frame.len() < frame_len {
        return;
    }

    let (source_region_x, source_region_y, source_region_width, source_region_height) =
        source_region;
    if source_region_width == 0 || source_region_height == 0 {
        return;
    }
    let scale_x = f64::from(frame_width) / f64::from(source_region_width);
    let scale_y = f64::from(frame_height) / f64::from(source_region_height);
    let cursor_left = hotspot_x - xhot;
    let cursor_top = hotspot_y - yhot;
    for source_y in 0..cursor_height {
        let screen_y = cursor_top + source_y as i32;
        let destination_top = ((screen_y - source_region_y) as f64 * scale_y).floor() as i32;
        let destination_bottom = ((screen_y + 1 - source_region_y) as f64 * scale_y).ceil() as i32;
        if destination_bottom <= 0 || destination_top >= frame_height as i32 {
            continue;
        }

        for source_x in 0..cursor_width {
            let screen_x = cursor_left + source_x as i32;
            let destination_left = ((screen_x - source_region_x) as f64 * scale_x).floor() as i32;
            let destination_right =
                ((screen_x + 1 - source_region_x) as f64 * scale_x).ceil() as i32;
            if destination_right <= 0 || destination_left >= frame_width as i32 {
                continue;
            }

            let source_index = source_y as usize * cursor_width as usize + source_x as usize;
            let Some(&argb) = cursor.get(source_index) else {
                continue;
            };
            let alpha = (argb >> 24) as u8;
            if alpha == 0 {
                continue;
            }

            let inverse_alpha = 255 - u32::from(alpha);
            let source_channels = [
                ((argb >> 16) & 0xff) as u8,
                ((argb >> 8) & 0xff) as u8,
                (argb & 0xff) as u8,
            ];
            for destination_y in destination_top.max(0)..destination_bottom.min(frame_height as i32)
            {
                // glReadPixels stores row zero at the bottom while root and
                // region coordinates use a top-left origin.
                let frame_y = frame_height - 1 - destination_y as u32;
                for destination_x in
                    destination_left.max(0)..destination_right.min(frame_width as i32)
                {
                    let destination_index =
                        ((frame_y as usize * frame_width as usize) + destination_x as usize) * 4;
                    for (offset, source) in source_channels.into_iter().enumerate() {
                        let destination = u32::from(frame[destination_index + offset]);
                        frame[destination_index + offset] = u8::try_from(
                            (u32::from(source) + (destination * inverse_alpha + 127) / 255)
                                .min(255),
                        )
                        .unwrap_or(255);
                    }
                    let destination_alpha = u32::from(frame[destination_index + 3]);
                    frame[destination_index + 3] = u8::try_from(
                        (u32::from(alpha) + (destination_alpha * inverse_alpha + 127) / 255)
                            .min(255),
                    )
                    .unwrap_or(255);
                }
            }
        }
    }
}

impl Drop for GraphicsPlatform {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::{GraphicsApiPreference, composite_premultiplied_argb_cursor};

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
    fn composites_xfixes_cursor_with_bottom_up_frame_coordinates() {
        let mut frame = [10_u8, 20, 30, 255].repeat(3 * 3);
        let cursor = [0xffff0000, 0x80008000];

        composite_premultiplied_argb_cursor(
            &mut frame,
            3,
            3,
            &cursor,
            2,
            1,
            1,
            1,
            0,
            0,
            (0, 0, 3, 3),
        );

        // Screen y=1 maps to the middle row in this symmetric 3-row frame.
        let opaque_red = (1 * 3 + 1) * 4;
        assert_eq!(&frame[opaque_red..opaque_red + 4], &[255, 0, 0, 255]);
        let half_green = (1 * 3 + 2) * 4;
        assert_eq!(&frame[half_green..half_green + 4], &[5, 138, 15, 255]);
    }

    #[test]
    fn clips_cursor_at_recording_edges() {
        let mut frame = vec![0_u8; 2 * 2 * 4];
        let cursor = [0xffffffff; 4];

        composite_premultiplied_argb_cursor(
            &mut frame,
            2,
            2,
            &cursor,
            2,
            2,
            0,
            0,
            1,
            1,
            (0, 0, 2, 2),
        );

        // Only cursor source (1,1) lands on screen pixel (0,0), which is the
        // second row in the bottom-up readback.
        assert_eq!(&frame[8..12], &[255, 255, 255, 255]);
        assert_eq!(frame.iter().filter(|&&channel| channel == 255).count(), 4);
    }
}
