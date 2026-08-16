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
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

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
    backend: PlatformBackend,
    closed: bool,
}

enum PlatformBackend {
    Glx(GlxPlatform),
    Egl(EglPlatform),
}

#[derive(Clone)]
pub(super) struct RecordingCursor {
    /// Shared so a sample that reuses an unchanged cursor shape, and the clone
    /// the capture path takes, both cost a refcount rather than a copy.
    pixels: Arc<Vec<u32>>,
    /// Identifies the cursor *shape*. It changes only when the image does,
    /// which lets the GPU path skip re-uploading the texture on the vast
    /// majority of frames, where only the position moved.
    serial: u64,
    width: u32,
    height: u32,
    hotspot_x: i32,
    hotspot_y: i32,
    xhot: i32,
    yhot: i32,
}

/// Samples the X server cursor on a connection and thread of its own.
///
/// XComposite redirects windows but not the pointer, so the cursor is a
/// server-side sprite that never appears in `glReadPixels` and recording has to
/// draw it in. `XFixesGetCursorImage` is the only source for both its image and
/// its exact root position, and the window manager's own pointer tracking
/// cannot stand in: motion over a client window is delivered to that client and
/// never reaches us, so a locally tracked position goes stale the moment the
/// pointer crosses a window and the recorded cursor lands in the wrong place.
///
/// It is also a request-with-reply — it flushes and blocks in `read()` until
/// the server answers. Called once per captured frame it put a synchronous
/// server round-trip, plus a fresh pixel allocation, inside the capture path
/// between `glReadPixels` and the framebuffer unbinds, on the one thread that
/// also serves input and repaints for every client. The sampler moves both onto
/// a worker with its own display — Xlib allows that as long as no `Display` is
/// shared across threads — and the capture path takes the latest sample without
/// ever waiting for the server.
pub(super) struct RecordingCursorSampler {
    latest: Arc<Mutex<Option<RecordingCursor>>>,
    stop: Arc<(Mutex<bool>, Condvar)>,
    worker: Option<std::thread::JoinHandle<()>>,
    display: Option<SamplerDisplay>,
}

/// A display the sampler worker borrows for its lifetime.
///
/// Xlib is only thread-safe once `XInitThreads` has been called, which jwm does
/// not do, so this deliberately does not open or close the connection on the
/// worker: `XOpenDisplay` and `XCloseDisplay` mutate a process-wide display list
/// that would then be racing the compositor thread's own Xlib use. Both happen
/// on the compositor thread instead — the open before the worker starts, the
/// close after it is joined — leaving the worker to only issue requests. The
/// remaining process-wide state a request touches is the XFixes extension list,
/// and the sampler is the only XFixes user in the process.
struct SamplerDisplay(*mut x11::xlib::Display);

// SAFETY: the pointer is created and destroyed on the compositor thread while
// the worker is not running, and only the worker dereferences it in between, so
// the display is never touched by two threads at once.
unsafe impl Send for SamplerDisplay {}

impl RecordingCursorSampler {
    /// Begin sampling for a recording capturing one frame every `interval`.
    pub(super) fn start(interval: Duration) -> Self {
        let latest = Arc::new(Mutex::new(None));
        let stop = Arc::new((Mutex::new(false), Condvar::new()));

        // A private connection: the compositor's own display belongs to this
        // thread, and sharing it is exactly what the worker exists to avoid.
        let display = unsafe { x11::xlib::XOpenDisplay(ptr::null()) };
        let usable = !display.is_null()
            && unsafe {
                let mut event_base = 0;
                let mut error_base = 0;
                x11::xfixes::XFixesQueryExtension(display, &mut event_base, &mut error_base) != 0
            };
        if !usable {
            if display.is_null() {
                log::warn!("compositor: recording cursor sampler could not open a display");
            } else {
                log::warn!("compositor: XFixes unavailable; recordings omit the cursor");
                unsafe { x11::xlib::XCloseDisplay(display) };
            }
            return Self {
                latest,
                stop,
                worker: None,
                display: None,
            };
        }
        // Sample at twice the capture rate: a sample is then at most half a
        // frame old when a capture picks it up, which keeps a fast-moving
        // pointer aligned with the frame it is drawn onto. The extra round trip
        // costs nothing on a thread nobody waits for. The bounds keep a 240 fps
        // recording from hammering the server and a 1 fps one from letting the
        // cursor lag a whole second behind.
        let period = (interval / 2).clamp(Duration::from_millis(4), Duration::from_millis(50));
        let worker_latest = Arc::clone(&latest);
        let worker_stop = Arc::clone(&stop);
        let borrowed = SamplerDisplay(display);
        let worker = std::thread::Builder::new()
            .name("jwm-cursor-sampler".into())
            .spawn(move || {
                let borrowed = borrowed;
                sample_cursor_until_stopped(borrowed.0, &worker_latest, &worker_stop, period);
            })
            .map_err(|error| {
                log::warn!("compositor: recording cursor sampler unavailable: {error}");
            })
            .ok();
        Self {
            latest,
            stop,
            worker,
            display: Some(SamplerDisplay(display)),
        }
    }

    /// The most recent sample, or `None` before the first one lands or when the
    /// server has no XFixes. Never blocks on the server.
    pub(super) fn latest(&self) -> Option<RecordingCursor> {
        self.latest.lock().ok().and_then(|latest| latest.clone())
    }
}

impl Drop for RecordingCursorSampler {
    fn drop(&mut self) {
        let (stopped, wakeup) = &*self.stop;
        if let Ok(mut stopped) = stopped.lock() {
            *stopped = true;
        }
        // The worker waits on the condvar rather than sleeping, so the join is
        // bounded by one in-flight round trip instead of a sampling period.
        wakeup.notify_all();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        // Only now that the worker is gone: closing the display is the other
        // half of the process-wide state the worker must not touch.
        if let Some(display) = self.display.take() {
            unsafe { x11::xlib::XCloseDisplay(display.0) };
        }
    }
}

/// # Safety
/// `display` must be a live Xlib connection used by no other thread for as long
/// as this runs.
fn sample_cursor_until_stopped(
    display: *mut x11::xlib::Display,
    latest: &Mutex<Option<RecordingCursor>>,
    stop: &(Mutex<bool>, Condvar),
    period: Duration,
) {
    // The serial changes only when the cursor image does, which is orders of
    // magnitude rarer than the position, so an unchanged shape reuses its pixel
    // buffer instead of converting every sample afresh.
    let mut cached_shape: Option<(u64, Arc<Vec<u32>>)> = None;
    let (stopped, wakeup) = stop;
    loop {
        if let Some(cursor) = unsafe { sample_cursor(display, &mut cached_shape) }
            && let Ok(mut latest) = latest.lock()
        {
            *latest = Some(cursor);
        }
        let Ok(guard) = stopped.lock() else {
            break;
        };
        if *guard {
            break;
        }
        let Ok((guard, _)) = wakeup.wait_timeout(guard, period) else {
            break;
        };
        if *guard {
            break;
        }
    }
}

/// # Safety
/// `display` must be a live Xlib connection owned by the calling thread.
unsafe fn sample_cursor(
    display: *mut x11::xlib::Display,
    cached_shape: &mut Option<(u64, Arc<Vec<u32>>)>,
) -> Option<RecordingCursor> {
    unsafe {
        let image = x11::xfixes::XFixesGetCursorImage(display);
        if image.is_null() {
            return None;
        }
        let image_ref = &*image;
        let serial = image_ref.cursor_serial as u64;
        let pixels = match cached_shape {
            Some((cached_serial, pixels)) if *cached_serial == serial => Some(Arc::clone(pixels)),
            _ => usize::from(image_ref.width)
                .checked_mul(usize::from(image_ref.height))
                .filter(|_| !image_ref.pixels.is_null())
                .map(|pixel_count| {
                    let pixels: Arc<Vec<u32>> = Arc::new(
                        std::slice::from_raw_parts(image_ref.pixels, pixel_count)
                            .iter()
                            .map(|&pixel| pixel as u32)
                            .collect(),
                    );
                    *cached_shape = Some((serial, Arc::clone(&pixels)));
                    pixels
                }),
        };
        let sample = pixels.map(|pixels| RecordingCursor {
            pixels,
            serial,
            width: u32::from(image_ref.width),
            height: u32::from(image_ref.height),
            hotspot_x: i32::from(image_ref.x),
            hotspot_y: i32::from(image_ref.y),
            xhot: i32::from(image_ref.xhot),
            yhot: i32::from(image_ref.yhot),
        });
        x11::xlib::XFree(image.cast());
        sample
    }
}

impl RecordingCursor {
    /// Root position of the pointer in this sample. The cursor is drawn in
    /// after the scene is composited, so it moving is a change the compositor's
    /// own damage tracking never sees.
    pub(super) fn position(&self) -> (i32, i32) {
        (self.hotspot_x, self.hotspot_y)
    }

    /// Identity of the cursor shape, for skipping redundant texture uploads.
    pub(super) fn serial(&self) -> u64 {
        self.serial
    }

    pub(super) fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Where the cursor image's top-left corner sits in root coordinates.
    pub(super) fn top_left(&self) -> (i32, i32) {
        (self.hotspot_x - self.xhot, self.hotspot_y - self.yhot)
    }

    /// The image as premultiplied RGBA bytes, ready for `glTexImage2D`.
    ///
    /// XFixes hands back premultiplied ARGB packed one pixel per `unsigned
    /// long`; GL wants tightly packed byte quads in RGBA order.
    pub(super) fn to_rgba8(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.pixels.len() * 4);
        for &argb in self.pixels.iter() {
            bytes.extend_from_slice(&[
                ((argb >> 16) & 0xff) as u8,
                ((argb >> 8) & 0xff) as u8,
                (argb & 0xff) as u8,
                ((argb >> 24) & 0xff) as u8,
            ]);
        }
        bytes
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
            PlatformBackend::Glx(glx) => glx.load_oml(self.xlib_display),
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
            (PlatformBackend::Egl(egl), PixmapBinding::Egl { image }) => {
                egl.refresh_pixmap(gl, texture, *image);
                Ok(())
            }
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

#[cfg(test)]
mod tests {
    use super::GraphicsApiPreference;

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
}
