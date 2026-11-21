//! Same-device H.264 VA-API NV12 rendering to a displayable DMA-BUF.

use super::RemoteResult;
use super::gpu_import::{self, EGL_NONE, GR88, Handle, R8, XRGB8888};
use super::telemetry::{NativeStage, NativeStageTimes};
use ffmpeg_next::{chroma, color, ffi, frame::Video};
use gbm::{BufferObjectFlags, Device, Format, Modifier};
use glow::HasContext;
use libloading::Library;
use std::ffi::{CString, c_void};
use std::fs::File;
use std::io;
#[cfg(feature = "remote-wayland")]
use std::os::fd::{AsFd, BorrowedFd};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ImageFormat, ImageOrder, Pixmap, VisualClass, Window};
use x11rb::protocol::{dri3, xproto::ConnectionExt};
use x11rb::rust_connection::RustConnection;

const EGL_OPENGL_ES_API: u32 = 0x30a0;
const EGL_SURFACE_TYPE: i32 = 0x3033;
const EGL_PBUFFER_BIT: i32 = 0x0001;
const EGL_RENDERABLE_TYPE: i32 = 0x3040;
const EGL_OPENGL_ES3_BIT: i32 = 0x0040;
const EGL_CONTEXT_MAJOR_VERSION: i32 = 0x3098;
const GPU_WAIT_NS: i32 = 50_000_000;
const PROBE_SOURCE: &[u8] = include_bytes!("native-video-probe.h264");
const PROBE_SIZE: (u16, u16) = (251, 247);

#[derive(Debug)]
pub(super) struct DecodeFenceFailure(i32);

impl std::fmt::Display for DecodeFenceFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "VA-API surface synchronization failed (0x{:x})", self.0)
    }
}

impl std::error::Error for DecodeFenceFailure {}

type BindApi = unsafe extern "C" fn(u32) -> u32;
type ChooseConfig = unsafe extern "C" fn(Handle, *const i32, *mut Handle, i32, *mut i32) -> u32;
type CreateContext = unsafe extern "C" fn(Handle, Handle, Handle, *const i32) -> Handle;
type MakeCurrent = unsafe extern "C" fn(Handle, Handle, Handle, Handle) -> u32;
type DestroyContext = unsafe extern "C" fn(Handle, Handle) -> u32;
type ImageTargetTexture = unsafe extern "C" fn(u32, Handle);
type SyncSurface = unsafe extern "C" fn(Handle, u32, u64) -> i32;

macro_rules! egl_entry {
    ($display:expr, $name:literal, $signature:ty) => {{
        let address = $display.proc($name);
        if address.is_null() {
            return Err(format!("missing GPU entry point {}", $name.to_string_lossy()).into());
        }
        // The EGL runtime returned this named entry point with the specified C ABI.
        std::mem::transmute::<*const c_void, $signature>(address)
    }};
}

struct EglContext {
    display: gpu_import::Display,
    context: Handle,
    make_current: MakeCurrent,
    destroy: DestroyContext,
}

impl EglContext {
    fn new(node: &Path) -> RemoteResult<Self> {
        let node = CString::new(node.as_os_str().as_bytes())?;
        let display = gpu_import::Display::open(&node)?;
        // This owns a private EGL context on the viewer thread. No native
        // surface is bound because output goes to an imported framebuffer.
        unsafe {
            let bind_api = egl_entry!(display, c"eglBindAPI", BindApi);
            let choose_config = egl_entry!(display, c"eglChooseConfig", ChooseConfig);
            let create = egl_entry!(display, c"eglCreateContext", CreateContext);
            let make_current = egl_entry!(display, c"eglMakeCurrent", MakeCurrent);
            let destroy = egl_entry!(display, c"eglDestroyContext", DestroyContext);
            if bind_api(EGL_OPENGL_ES_API) == 0 {
                return Err("EGL could not bind OpenGL ES".into());
            }
            let attributes = [
                EGL_RENDERABLE_TYPE,
                EGL_OPENGL_ES3_BIT,
                EGL_SURFACE_TYPE,
                EGL_PBUFFER_BIT,
                EGL_NONE,
            ];
            let mut config = null_mut();
            let mut count = 0;
            if choose_config(
                display.handle,
                attributes.as_ptr(),
                &mut config,
                1,
                &mut count,
            ) == 0
                || count != 1
            {
                return Err("EGL device has no OpenGL ES 3 configuration".into());
            }
            let context = create(
                display.handle,
                config,
                null_mut(),
                [EGL_CONTEXT_MAJOR_VERSION, 3, EGL_NONE].as_ptr(),
            );
            if context.is_null() {
                return Err("EGL could not create a GLES3 context".into());
            }
            Ok(Self {
                display,
                context,
                make_current,
                destroy,
            })
        }
    }

    fn activate(&self) -> RemoteResult<CurrentContext<'_>> {
        if unsafe { (self.make_current)(self.display.handle, null_mut(), null_mut(), self.context) }
            == 0
        {
            return Err("EGL could not bind the native renderer".into());
        }
        Ok(CurrentContext(self))
    }
}

struct CurrentContext<'a>(&'a EglContext);

impl Drop for CurrentContext<'_> {
    fn drop(&mut self) {
        unsafe { (self.0.make_current)(self.0.display.handle, null_mut(), null_mut(), null_mut()) };
    }
}

impl Drop for EglContext {
    fn drop(&mut self) {
        let _status = unsafe { (self.destroy)(self.display.handle, self.context) };
        #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
        fence_test::context_destroyed(self.context, _status != 0);
    }
}

pub(super) struct VaSync {
    _library: Library,
    sync: SyncSurface,
}

impl VaSync {
    pub(super) fn new() -> RemoteResult<Self> {
        unsafe {
            let library = Library::new("libva.so.2")?;
            let sync = *library.get::<SyncSurface>(b"vaSyncSurface2")?;
            Ok(Self {
                _library: library,
                sync,
            })
        }
    }

    pub(super) fn wait(&self, frame: &Video) -> RemoteResult<()> {
        let (display, surface) = vaapi_handle(frame)?;
        let status = unsafe { (self.sync)(display, surface, GPU_WAIT_NS as u64) };
        if status != 0 {
            return Err(DecodeFenceFailure(status).into());
        }
        Ok(())
    }
}

pub(super) fn vaapi_handle(frame: &Video) -> RemoteResult<(Handle, u32)> {
    // FFmpeg hwcontext_vaapi.h defines AVVAAPIDeviceContext with VADisplay
    // as its first field, and data[3] stores the VASurfaceID. The caller
    // retains the frame while using the returned display and surface.
    unsafe {
        let frame = &*frame.as_ptr();
        let frames = frame
            .hw_frames_ctx
            .as_ref()
            .and_then(|reference| reference.data.cast::<ffi::AVHWFramesContext>().as_ref())
            .ok_or("VA-API frame has no hardware frames context")?;
        let device = frames
            .device_ctx
            .as_ref()
            .ok_or("missing VA-API device context")?;
        if device.type_ != ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI || device.hwctx.is_null() {
            return Err("frame does not belong to a VA-API device".into());
        }
        let display = *device.hwctx.cast::<Handle>();
        if display.is_null() {
            return Err("invalid VA-API display".into());
        }
        Ok((display, u32::try_from(frame.data[3] as usize)?))
    }
}

pub(super) struct Renderer {
    gl: glow::Context,
    egl: EglContext,
    va: VaSync,
    allocator: RgbAllocator,
    target_texture: ImageTargetTexture,
    program: glow::Program,
    vao: glow::VertexArray,
    ambiguous_completion: bool,
    // If GLES cannot certify that it stopped sampling, never return the
    // surface to a live decoder pool. This is dropped after the EGL context.
    retired: Option<Video>,
}

pub(super) struct RgbBuffer {
    fd: OwnedFd,
    stride: u16,
    dimensions: (u16, u16),
}

pub(super) struct RgbAllocator(Device<File>);

impl RgbAllocator {
    pub(super) fn open(node: &Path) -> RemoteResult<Self> {
        Ok(Self(Device::new(
            File::options().read(true).write(true).open(node)?,
        )?))
    }

    pub(super) fn allocate(
        &self,
        size: (u16, u16),
    ) -> RemoteResult<(gbm::BufferObject<()>, RgbBuffer)> {
        let storage = self.0.create_buffer_object::<()>(
            u32::from(size.0),
            u32::from(size.1),
            Format::Xrgb8888,
            BufferObjectFlags::LINEAR | BufferObjectFlags::RENDERING,
        )?;
        if storage.plane_count() != 1
            || storage.modifier() != Modifier::Linear
            || storage.offset(0) != 0
        {
            return Err("native viewer requires a single linear XRGB8888 output plane".into());
        }
        let buffer = RgbBuffer {
            fd: storage.fd()?,
            stride: u16::try_from(storage.stride())?,
            dimensions: size,
        };
        Ok((storage, buffer))
    }
}

#[derive(Clone, Copy)]
struct Yuv {
    coefficients: [f32; 4],
    chroma_offset: [f32; 2],
    limited: bool,
}

impl Yuv {
    fn from_frame(frame: &Video) -> RemoteResult<Self> {
        // The existing H.264 encoder's unspecified matrix uses libswscale's
        // BT.601 default, with MPEG range. Explicit 709 or full range can also
        // be converted without silently interpreting unknown color matrices.
        let coefficients = match frame.color_space() {
            color::Space::Unspecified | color::Space::BT470BG | color::Space::SMPTE170M => {
                [1.402, -0.344_136, -0.714_136, 1.772]
            }
            color::Space::BT709 => [1.5748, -0.187_324, -0.468_124, 1.8556],
            _ => return Err("unsupported VA-API color matrix".into()),
        };
        // Filtering the half-size UV texture at the same normalized coordinate
        // as luma assumes a chroma sample centered between four luma samples.
        // Shift from the declared sample position to that center, in luma
        // pixels. Texture coordinates increase with the imported plane rows.
        let chroma_offset = match frame.chroma_location() {
            chroma::Location::Unspecified | chroma::Location::Left => [0.5, 0.0],
            chroma::Location::Center => [0.0, 0.0],
            chroma::Location::TopLeft => [0.5, 0.5],
            chroma::Location::Top => [0.0, 0.5],
            chroma::Location::BottomLeft => [0.5, -0.5],
            chroma::Location::Bottom => [0.0, -0.5],
        };
        Ok(Self {
            coefficients,
            chroma_offset,
            limited: frame.color_range() != color::Range::JPEG,
        })
    }

    /// `program` must be the bound NV12 shader in the current context.
    unsafe fn set_uniforms(&self, gl: &glow::Context, program: glow::Program, visible: (u16, u16)) {
        let location = |name| unsafe { gl.get_uniform_location(program, name) };
        unsafe {
            gl.uniform_1_i32(location("luma").as_ref(), 0);
            gl.uniform_1_i32(location("chroma").as_ref(), 1);
            gl.uniform_2_f32(
                location("visible_size").as_ref(),
                f32::from(visible.0),
                f32::from(visible.1),
            );
            let [x, y] = self.chroma_offset;
            gl.uniform_2_f32(location("chroma_offset").as_ref(), x, y);
            gl.uniform_4_f32_slice(location("coefficients").as_ref(), &self.coefficients);
            gl.uniform_1_i32(location("limited").as_ref(), i32::from(self.limited));
        }
    }
}

impl Renderer {
    pub(super) fn new(node: &Path) -> RemoteResult<Self> {
        let va = VaSync::new()?;
        let allocator = RgbAllocator::open(node)?;
        let egl = EglContext::new(node)?;
        // Every function comes from the library that owns the live display.
        let (gl, target_texture, program, vao) = unsafe {
            let _current = egl.activate()?;
            let gl = glow::Context::from_loader_function_cstr(|name| egl.display.proc(name));
            if !gl.supported_extensions().contains("GL_OES_EGL_image") {
                return Err("GLES cannot bind imported EGL images".into());
            }
            let target = egl_entry!(
                egl.display,
                c"glEGLImageTargetTexture2DOES",
                ImageTargetTexture
            );
            let program = create_program(&gl)?;
            let vao = gl.create_vertex_array()?;
            (gl, target, program, vao)
        };
        Ok(Self {
            gl,
            egl,
            va,
            allocator,
            target_texture,
            program,
            vao,
            ambiguous_completion: false,
            retired: None,
        })
    }

    pub(super) fn render(&mut self, frame: &Video, visible: (u16, u16)) -> RemoteResult<RgbBuffer> {
        self.render_to(
            frame,
            visible,
            visible,
            (0, 0, visible.0, visible.1),
            None,
            None,
        )
    }

    pub(super) fn render_to(
        &mut self,
        frame: &Video,
        visible: (u16, u16),
        size: (u16, u16),
        content: (u16, u16, u16, u16),
        reusable: Option<RgbBuffer>,
        mut timings: Option<&mut NativeStageTimes>,
    ) -> RemoteResult<RgbBuffer> {
        if self.ambiguous_completion {
            return Err("GPU renderer has an unresolved draw".into());
        }
        let _current = self.egl.activate()?;
        let width = i32::try_from(frame.width())?;
        let height = i32::try_from(frame.height())?;
        if unsafe { (*frame.as_ptr()).format } != ffi::AVPixelFormat::AV_PIX_FMT_VAAPI as i32
            || visible.0 == 0
            || visible.1 == 0
            || width > 4096
            || height > 4096
            || i64::from(width) * i64::from(height) > 4096 * 2160
            || width % 2 != 0
            || height % 2 != 0
            || i32::from(visible.0) > width
            || i32::from(visible.1) > height
        {
            return Err("unsupported VA-API surface dimensions or format".into());
        }
        let colors = Yuv::from_frame(frame)?;
        {
            let _timer = timings
                .as_deref_mut()
                .map(|timings| timings.start(NativeStage::VaSurfaceWait));
            self.va.wait(frame)?;
        }
        let lease = {
            let _timer = timings
                .as_deref_mut()
                .map(|timings| timings.start(NativeStage::VaFrameExport));
            gpu_import::GpuFrameLease::map(frame)?
        };
        let descriptor = lease.descriptor()?;
        let objects = descriptor
            .objects
            .get(..usize::try_from(descriptor.nb_objects)?)
            .ok_or("invalid DRM object count")?;
        if descriptor.nb_layers != 2
            || descriptor.layers[0].format != R8
            || descriptor.layers[1].format != GR88
        {
            return Err("native viewer requires separate VA-API NV12 R8 and GR88 layers".into());
        }
        let y_attributes = gpu_import::attributes(
            &descriptor.layers[0],
            objects,
            (width, height),
            self.egl.display.modifiers,
        )?;
        let uv_attributes = gpu_import::attributes(
            &descriptor.layers[1],
            objects,
            (width / 2, height / 2),
            self.egl.display.modifiers,
        )?;
        // A released Wayland target owns its exported DMA-BUF FD. Keep a fresh
        // target's GBM object alive through submission as on the unpooled path.
        #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
        let reused = reusable.is_some();
        let _allocation;
        let target = if let Some(target) = reusable {
            if target.dimensions != size {
                return Err("incompatible native video render target".into());
            }
            target
        } else {
            let _timer = timings
                .as_deref_mut()
                .map(|timings| timings.start(NativeStage::RgbTargetAllocation));
            let (storage, target) = self.allocator.allocate(size)?;
            _allocation = storage;
            target
        };

        let submit_timer = timings
            .as_deref_mut()
            .map(|timings| timings.start(NativeStage::EglPrepareAndSubmit));
        let destination = rgb_attributes(
            target.fd.as_raw_fd(),
            target.stride,
            size,
            self.egl.display.modifiers,
        )?;
        let y = self.egl.display.import(&y_attributes)?;
        let uv = self.egl.display.import(&uv_attributes)?;
        let rgb = self.egl.display.import(&destination)?;

        let gl = &self.gl;
        // Allocate and bind the objects only while this thread owns the
        // context. Driver objects retain their EGL images through submission.
        unsafe {
            let mut drawing = DrawObjects::new(gl);
            for (index, image) in [y.handle, uv.handle, rgb.handle].into_iter().enumerate() {
                drawing.attach(index, image, self.target_texture)?;
            }
            let framebuffer = gl.create_framebuffer()?;
            drawing.framebuffer = Some(framebuffer);
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                drawing.textures[2],
                0,
            );
            if gl.check_framebuffer_status(glow::FRAMEBUFFER) != glow::FRAMEBUFFER_COMPLETE {
                return Err("imported XRGB8888 storage is not renderable".into());
            }
            if content != (0, 0, size.0, size.1) {
                gl.clear_color(0.0, 0.0, 0.0, 1.0);
                gl.clear(glow::COLOR_BUFFER_BIT);
            }
            gl.viewport(
                i32::from(content.0),
                i32::from(content.1),
                i32::from(content.2),
                i32::from(content.3),
            );
            gl.use_program(Some(self.program));
            gl.bind_vertex_array(Some(self.vao));
            colors.set_uniforms(gl, self.program, visible);
            if gl.get_error() != glow::NO_ERROR {
                return Err("GLES could not prepare the imported RGB draw".into());
            }
            self.ambiguous_completion = true;
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            let fence = gl.fence_sync(glow::SYNC_GPU_COMMANDS_COMPLETE, 0)?;
            drop(submit_timer);
            let status = {
                let _timer = timings
                    .as_deref_mut()
                    .map(|timings| timings.start(NativeStage::EglFenceWait));
                gl.client_wait_sync(fence, glow::SYNC_FLUSH_COMMANDS_BIT, GPU_WAIT_NS)
            };
            gl.delete_sync(fence);
            #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
            let status =
                fence_test::observe_status(status, frame, self.egl.context, &target, reused);
            match status {
                glow::ALREADY_SIGNALED | glow::CONDITION_SATISFIED => {}
                glow::TIMEOUT_EXPIRED => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "GLES frame fence did not complete",
                    )
                    .into());
                }
                glow::WAIT_FAILED => {
                    return Err(io::Error::other("GLES frame fence wait failed").into());
                }
                _ => {
                    return Err(io::Error::other(format!(
                        "unexpected GLES frame fence status 0x{status:04x}"
                    ))
                    .into());
                }
            }
            self.ambiguous_completion = false;
            if gl.get_error() != glow::NO_ERROR {
                return Err("GLES rejected native video rendering".into());
            }
        }
        Ok(target)
    }

    pub(super) fn retire_if_pending(&mut self, surface: Video) {
        if self.ambiguous_completion && self.retired.is_none() {
            self.retired = Some(surface);
        }
    }

    pub(super) fn has_unresolved_draw(&self) -> bool {
        self.ambiguous_completion
    }
}

#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
impl Drop for Renderer {
    fn drop(&mut self) {
        fence_test::renderer_teardown(
            self.egl.context,
            self.retired.as_ref(),
            self.ambiguous_completion,
        );
    }
}

#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
pub(super) mod fence_test {
    use super::*;
    use nix::sys::stat::fstat;
    use std::cell::RefCell;

    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub(in crate::remote) enum Fault {
        FirstDrawTimeout {
            cleanup_fails: bool,
        },
        #[default]
        SecondDrawTimeout,
        SecondReusedDrawWaitFailed,
    }

    type BackingIdentity = (nix::libc::dev_t, nix::libc::ino_t);

    fn backing_identity(target: &RgbBuffer) -> BackingIdentity {
        let stat = fstat(&target.fd).expect("stat the live test DMA-BUF");
        (stat.st_dev, stat.st_ino)
    }

    #[derive(Default)]
    pub(in crate::remote) struct Proof {
        pub(in crate::remote) real_signaled_draws: usize,
        pub(in crate::remote) real_signaled_reused_draws: usize,
        pub(in crate::remote) injected_status: Option<u32>,
        pub(in crate::remote) faulted_reused_target_not_pooled: bool,
        pub(in crate::remote) exact_frame_held_at_teardown: bool,
        pub(in crate::remote) same_egl_context_destroyed: bool,
        pub(in crate::remote) x11_prefetch_joined_after_request: bool,
        fault: Fault,
        faulted_backing: Option<BackingIdentity>,
        context: usize,
        frame: usize,
    }

    thread_local! { static ACTIVE: RefCell<Option<Proof>> = const { RefCell::new(None) }; }

    pub(in crate::remote) fn with_fence_fault<T>(
        fault: Fault,
        run: impl FnOnce() -> T,
    ) -> (T, Proof) {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                ACTIVE.with(|slot| slot.borrow_mut().take());
            }
        }
        ACTIVE.with(|slot| {
            assert!(
                slot.borrow_mut()
                    .replace(Proof {
                        fault,
                        ..Proof::default()
                    })
                    .is_none()
            );
        });
        let _reset = Reset;
        let result = run();
        let proof = ACTIVE.with(|slot| slot.borrow_mut().take().unwrap());
        (result, proof)
    }

    pub(super) fn observe_status(
        status: u32,
        frame: &Video,
        context: Handle,
        target: &RgbBuffer,
        reused: bool,
    ) -> u32 {
        ACTIVE.with(|slot| {
            let mut slot = slot.borrow_mut();
            let Some(proof) = slot.as_mut() else {
                return status;
            };
            if matches!(status, glow::ALREADY_SIGNALED | glow::CONDITION_SATISFIED) {
                proof.real_signaled_draws += 1;
                proof.real_signaled_reused_draws += usize::from(reused);
                let injection = match proof.fault {
                    Fault::FirstDrawTimeout { .. } if proof.real_signaled_draws == 1 => {
                        Some(glow::TIMEOUT_EXPIRED)
                    }
                    Fault::SecondDrawTimeout if proof.real_signaled_draws == 2 => {
                        Some(glow::TIMEOUT_EXPIRED)
                    }
                    Fault::SecondReusedDrawWaitFailed
                        if reused && proof.real_signaled_reused_draws == 2 =>
                    {
                        proof.faulted_backing = Some(backing_identity(target));
                        Some(glow::WAIT_FAILED)
                    }
                    _ => None,
                };
                if let Some(injected) = injection {
                    proof.injected_status = Some(injected);
                    proof.context = context as usize;
                    // Compare identity only. No extra reference masks early release.
                    proof.frame = unsafe { frame.as_ptr() } as usize;
                    return injected;
                }
            }
            status
        })
    }

    pub(in crate::remote) fn x11_prefetch_joined(pending: bool) -> RemoteResult<()> {
        ACTIVE.with(|slot| {
            if let Some(proof) = slot.borrow_mut().as_mut()
                && let Fault::FirstDrawTimeout { cleanup_fails } = proof.fault
                && proof.injected_status.is_some()
            {
                proof.x11_prefetch_joined_after_request = pending;
                if cleanup_fails {
                    return Err(
                        io::Error::other("injected X11 RGB preallocation cleanup failure").into(),
                    );
                }
            }
            Ok(())
        })
    }

    pub(in crate::remote) fn observe_wayland_pool_after_render_error<'a>(
        mut backings: impl Iterator<Item = &'a RgbBuffer>,
    ) {
        ACTIVE.with(|slot| {
            if let Some(proof) = slot.borrow_mut().as_mut()
                && let Some(faulted) = proof.faulted_backing
            {
                proof.faulted_reused_target_not_pooled =
                    backings.all(|backing| backing_identity(backing) != faulted);
            }
        });
    }

    pub(super) fn renderer_teardown(context: Handle, frame: Option<&Video>, unresolved: bool) {
        ACTIVE.with(|slot| {
            if let Some(proof) = slot.borrow_mut().as_mut()
                && proof.injected_status.is_some()
                && proof.context == context as usize
            {
                proof.exact_frame_held_at_teardown = unresolved
                    && frame.is_some_and(|held| unsafe { held.as_ptr() } as usize == proof.frame);
            }
        });
    }

    pub(super) fn context_destroyed(context: Handle, destroyed: bool) {
        ACTIVE.with(|slot| {
            if let Some(proof) = slot.borrow_mut().as_mut()
                && proof.injected_status.is_some()
                && proof.context == context as usize
            {
                proof.same_egl_context_destroyed = destroyed && proof.exact_frame_held_at_teardown;
            }
        });
    }
}

fn rgb_attributes(
    fd: i32,
    stride: u16,
    size: (u16, u16),
    modifiers: bool,
) -> RemoteResult<Vec<i32>> {
    let layer = ffi::AVDRMLayerDescriptor {
        format: XRGB8888,
        nb_planes: 1,
        planes: [ffi::AVDRMPlaneDescriptor {
            object_index: 0,
            offset: 0,
            pitch: stride as isize,
        }; 4],
    };
    let object = ffi::AVDRMObjectDescriptor {
        fd,
        size: usize::from(stride) * usize::from(size.1),
        format_modifier: 0,
    };
    gpu_import::attributes(
        &layer,
        &[object],
        (i32::from(size.0), i32::from(size.1)),
        modifiers,
    )
}

struct DrawObjects<'a> {
    gl: &'a glow::Context,
    textures: [Option<glow::Texture>; 3],
    framebuffer: Option<glow::Framebuffer>,
}

impl<'a> DrawObjects<'a> {
    fn new(gl: &'a glow::Context) -> Self {
        Self {
            gl,
            textures: [None; 3],
            framebuffer: None,
        }
    }

    unsafe fn attach(
        &mut self,
        index: usize,
        handle: Handle,
        import: ImageTargetTexture,
    ) -> RemoteResult<()> {
        unsafe {
            let gl = self.gl;
            let texture = gl.create_texture()?;
            self.textures[index] = Some(texture);
            gl.active_texture(glow::TEXTURE0 + index as u32);
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            import(glow::TEXTURE_2D, handle);
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                glow::LINEAR as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MAG_FILTER,
                glow::LINEAR as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_S,
                glow::CLAMP_TO_EDGE as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_T,
                glow::CLAMP_TO_EDGE as i32,
            );
            if gl.get_error() != glow::NO_ERROR {
                return Err("GLES rejected the imported DMA-BUF texture".into());
            }
        }
        Ok(())
    }
}

impl Drop for DrawObjects<'_> {
    fn drop(&mut self) {
        unsafe {
            let gl = self.gl;
            if let Some(framebuffer) = self.framebuffer {
                gl.delete_framebuffer(framebuffer);
            }
            for (index, texture) in self.textures.into_iter().enumerate() {
                if let Some(texture) = texture {
                    gl.active_texture(glow::TEXTURE0 + index as u32);
                    gl.bind_texture(glow::TEXTURE_2D, None);
                    gl.delete_texture(texture);
                }
            }
        }
    }
}

const VERTEX: &str = r#"#version 300 es
out vec2 uv;
void main() {
    vec2 point = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
    uv = point;
    gl_Position = vec4(2.0 * point - 1.0, 0.0, 1.0);
}
"#;

const FRAGMENT: &str = r#"#version 300 es
precision highp float;
uniform sampler2D luma;
uniform sampler2D chroma;
uniform vec2 visible_size;
uniform vec2 chroma_offset;
uniform vec4 coefficients;
uniform bool limited;
in vec2 uv;
out vec4 rgb;
void main() {
    vec2 storage_size = vec2(textureSize(luma, 0));
    vec2 source = visible_size * uv;
    float y = texture(luma, source / storage_size).r;
    // Clamp to the last visible UV texel center instead of coded padding.
    vec2 chroma_edge = 2.0 * ceil(visible_size * 0.5) - 1.0;
    vec2 chroma_uv = min(source + chroma_offset, chroma_edge) / storage_size;
    vec2 c = texture(chroma, chroma_uv).rg - vec2(128.0 / 255.0);
    if (limited) {
        y = (255.0 * y - 16.0) / 219.0;
        c *= 255.0 / 224.0;
    }
    rgb = vec4(
        y + coefficients.x * c.y,
        y + dot(coefficients.yz, c),
        y + coefficients.w * c.x,
        1.0);
}
"#;

unsafe fn create_program(gl: &glow::Context) -> RemoteResult<glow::Program> {
    // On initialization failure the caller drops the entire private context.
    unsafe {
        let program = gl.create_program()?;
        for (kind, source) in [
            (glow::VERTEX_SHADER, VERTEX),
            (glow::FRAGMENT_SHADER, FRAGMENT),
        ] {
            let shader = gl.create_shader(kind)?;
            gl.shader_source(shader, source);
            gl.compile_shader(shader);
            if !gl.get_shader_compile_status(shader) {
                return Err(format!(
                    "GLES shader compilation failed: {}",
                    gl.get_shader_info_log(shader)
                )
                .into());
            }
            gl.attach_shader(program, shader);
            gl.delete_shader(shader);
        }
        gl.link_program(program);
        if !gl.get_program_link_status(program) {
            return Err(format!(
                "GLES program link failed: {}",
                gl.get_program_info_log(program)
            )
            .into());
        }
        Ok(program)
    }
}

pub(super) fn select_device(
    conn: &RustConnection,
    root: Window,
    requested: &Path,
) -> RemoteResult<PathBuf> {
    let path = requested.canonicalize()?;
    let info = path.metadata()?;
    if !info.file_type().is_char_device() {
        return Err("GPU selection requires a DRM render node".into());
    }
    let config = conn.setup();
    let screen = config
        .roots
        .iter()
        .find(|candidate| candidate.root == root)
        .ok_or("unknown DRI3 screen")?;
    if !cfg!(target_endian = "little")
        || config.image_byte_order != ImageOrder::LSB_FIRST
        || screen.root_depth != 24
        || !screen
            .allowed_depths
            .iter()
            .flat_map(|depth| &depth.visuals)
            .any(|visual| {
                visual.visual_id == screen.root_visual
                    && visual.class == VisualClass::TRUE_COLOR
                    && (visual.red_mask, visual.green_mask, visual.blue_mask)
                        == (0xff0000, 0x00ff00, 0x0000ff)
            })
    {
        return Err("DRI3 presentation requires a depth-24 XRGB8888 root visual".into());
    }
    dri3::query_version(conn, 1, 0)?.reply()?;
    let reply = dri3::open(conn, root, 0)?.reply()?;
    let server = File::from(reply.device_fd);
    // Drivers may return the primary node rather than the render node for
    // the same card. Compare the kernel's parent device in that case.
    let server_device = server.metadata()?.rdev();
    if !same_drm_device(server_device, info.rdev()) {
        return Err("the X11 DRI3 screen uses a different DRM device".into());
    }
    Ok(path)
}

pub(super) fn same_drm_device(first: u64, second: u64) -> bool {
    first == second
        || sysfs_device(first)
            .zip(sysfs_device(second))
            .is_some_and(|(first, second)| first == second)
}

fn sysfs_device(device: u64) -> Option<PathBuf> {
    let major = libc::major(device);
    let minor = libc::minor(device);
    PathBuf::from(format!("/sys/dev/char/{major}:{minor}/device"))
        .canonicalize()
        .ok()
}

impl RgbBuffer {
    #[cfg(test)]
    pub(super) fn test_backing(fd: OwnedFd, dimensions: (u16, u16)) -> Self {
        Self {
            fd,
            stride: dimensions.0 * 4,
            dimensions,
        }
    }

    pub(super) fn dimensions(&self) -> (u16, u16) {
        self.dimensions
    }

    #[cfg(feature = "remote-wayland")]
    pub(super) fn wayland_plane(&self) -> (BorrowedFd<'_>, u32) {
        (self.fd.as_fd(), u32::from(self.stride))
    }

    pub(super) fn import(self, conn: &RustConnection, root: Window) -> RemoteResult<Pixmap> {
        let pixmap = conn.generate_id()?;
        let size = u32::from(self.stride) * u32::from(self.dimensions.1);
        dri3::pixmap_from_buffer(
            conn,
            pixmap,
            root,
            size,
            self.dimensions.0,
            self.dimensions.1,
            self.stride,
            24,
            32,
            self.fd,
        )?
        .check()?;
        Ok(pixmap)
    }
}

pub(super) fn probe(render_node: &Path, display: Option<&str>) -> RemoteResult<String> {
    let (conn, root) = super::x11_viewer::open_root(display)?;
    let node = select_device(&conn, root, render_node)?;
    let node_name = CString::new(node.as_os_str().as_bytes())?;
    let mut renderer = Renderer::new(&node)?;
    let cpu = super::h264::vaapi_probe_rgb(&node_name, PROBE_SOURCE)?;
    super::h264::with_vaapi_probe_surface(&node_name, PROBE_SOURCE, |frame| {
        let pixmap = renderer.render(frame, PROBE_SIZE)?.import(&conn, root)?;
        let result = (|| -> RemoteResult<()> {
            let mut verified = Vec::new();
            for (x, y) in [(16, 16), (233, 16), (16, 231), (233, 231)] {
                let image = conn
                    .get_image(ImageFormat::Z_PIXMAP, pixmap, x, y, 1, 1, u32::MAX)?
                    .reply()?;
                let [blue, green, red, _] = image.data.as_slice() else {
                    return Err("unexpected X11 pixel layout in native rendering probe".into());
                };
                let actual = [*red, *green, *blue];
                let offset = (y as usize * 256 + x as usize) * 3;
                let expected = &cpu[offset..offset + 3];
                if actual
                    .iter()
                    .zip(expected)
                    .any(|(gpu, cpu)| gpu.abs_diff(*cpu) > 4)
                {
                    return Err(format!(
                        "X11 GPU probe at ({x},{y}): RGB {actual:?} differs from CPU reference {expected:?}",
                    ).into());
                }
                if verified.contains(&actual) {
                    return Err(
                        "the native rendering probe did not distinguish the four test colors"
                            .into(),
                    );
                }
                verified.push(actual);
            }
            Ok(())
        })();
        conn.free_pixmap(pixmap)?.check()?;
        result
    })?;
    Ok(format!(
        "VA-API NV12 rendered with GLES to an X11 DRI3 pixmap on {} (251x247 crop, four CPU reference colors matched)",
        node.display(),
    ))
}

#[cfg(all(test, feature = "backend-wayland-udev"))]
mod tests {
    use super::*;
    use crate::backend::wayland_udev::compositor::headless_render::HeadlessSmithayGles;

    fn read_nv12(
        gl: &glow::Context,
        color: &Video,
        visible: (u16, u16),
        y: &[u8; 16],
        uv: &[u8; 8],
    ) -> Vec<u8> {
        let w = i32::from(visible.0);
        let h = i32::from(visible.1);
        let mut rgba = vec![0; usize::from(visible.0) * usize::from(visible.1) * 4];
        // Use the production shader and uniforms with ordinary GL textures in
        // place of imported DMA-BUFs. Row zero occupies the first texture row.
        unsafe {
            let program = create_program(gl).unwrap();
            let vao = gl.create_vertex_array().unwrap();
            let mut drawing = DrawObjects::new(gl);
            for (index, (width, height, internal, format, pixels)) in [
                (4, 4, glow::R8, glow::RED, Some(y.as_slice())),
                (2, 2, glow::RG8, glow::RG, Some(uv.as_slice())),
                (w, h, glow::RGBA8, glow::RGBA, None),
            ]
            .into_iter()
            .enumerate()
            {
                let texture = gl.create_texture().unwrap();
                drawing.textures[index] = Some(texture);
                gl.active_texture(glow::TEXTURE0 + index as u32);
                gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    internal as i32,
                    width,
                    height,
                    0,
                    format,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(pixels),
                );
                for filter in [glow::TEXTURE_MIN_FILTER, glow::TEXTURE_MAG_FILTER] {
                    gl.tex_parameter_i32(glow::TEXTURE_2D, filter, glow::LINEAR as i32);
                }
                for axis in [glow::TEXTURE_WRAP_S, glow::TEXTURE_WRAP_T] {
                    gl.tex_parameter_i32(glow::TEXTURE_2D, axis, glow::CLAMP_TO_EDGE as i32);
                }
            }
            let framebuffer = gl.create_framebuffer().unwrap();
            drawing.framebuffer = Some(framebuffer);
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                drawing.textures[2],
                0,
            );
            assert_eq!(
                gl.check_framebuffer_status(glow::FRAMEBUFFER),
                glow::FRAMEBUFFER_COMPLETE
            );
            gl.viewport(0, 0, w, h);
            gl.use_program(Some(program));
            gl.bind_vertex_array(Some(vao));
            Yuv::from_frame(color)
                .unwrap()
                .set_uniforms(gl, program, visible);
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            gl.read_pixels(
                0,
                0,
                w,
                h,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut rgba)),
            );
            assert_eq!(gl.get_error(), glow::NO_ERROR);
            gl.delete_vertex_array(vao);
            gl.use_program(None);
            gl.delete_program(program);
        }
        rgba
    }

    fn assert_rgb(actual: &[u8], rgb: [u8; 3]) {
        assert!(
            actual[..3].iter().zip(rgb).all(|(a, b)| a.abs_diff(b) <= 1),
            "GLES pixel {actual:?}, expected {rgb:?}"
        );
        assert_eq!(actual[3], 255);
    }

    #[test]
    fn nv12_shader_matrix_range_and_chroma_sample_location() {
        let Some(mut context) = HeadlessSmithayGles::new() else {
            return;
        };
        context
            .renderer
            .with_context(|_| {
                let gl = unsafe {
                    glow::Context::from_loader_function(|name| {
                        egl::get_proc_address(name) as *const c_void
                    })
                };
                let mut frame = Video::empty();
                let y = [
                    16, 16, 16, 16, 32, 32, 32, 32, 126, 126, 126, 126, 235, 235, 235, 235,
                ];
                for (matrix, range, rgb, gray) in [
                    (
                        color::Space::SMPTE170M,
                        color::Range::MPEG,
                        [226, 102, 26],
                        [0, 19, 128, 255],
                    ),
                    (
                        color::Space::SMPTE170M,
                        color::Range::JPEG,
                        [212, 103, 36],
                        [16, 32, 126, 235],
                    ),
                    (
                        color::Space::BT709,
                        color::Range::MPEG,
                        [238, 110, 21],
                        [0, 19, 128, 255],
                    ),
                    (
                        color::Space::BT709,
                        color::Range::JPEG,
                        [222, 110, 32],
                        [16, 32, 126, 235],
                    ),
                ] {
                    frame.set_color_space(matrix);
                    frame.set_color_range(range);
                    let colors = read_nv12(
                        &gl,
                        &frame,
                        (4, 4),
                        &[128; 16],
                        &[76, 188, 76, 188, 76, 188, 76, 188],
                    );
                    assert_rgb(&colors[20..24], rgb);
                    let ramp = read_nv12(&gl, &frame, (4, 4), &y, &[128; 8]);
                    for (row, expected) in gray.into_iter().enumerate() {
                        assert_rgb(&ramp[row * 16..][..4], [expected; 3]);
                    }
                }

                // Both axes vary independently. At luma (1,1) the top-left
                // baseline, horizontal phase and vertical phase are observable.
                let uv = [80, 128, 112, 128, 144, 128, 176, 128];
                frame.set_color_space(color::Space::BT709);
                frame.set_color_range(color::Range::JPEG);
                for (location, rgb) in [
                    (chroma::Location::Unspecified, [128, 131, 98]),
                    (chroma::Location::Left, [128, 131, 98]),
                    (chroma::Location::Center, [128, 132, 83]),
                    (chroma::Location::TopLeft, [128, 128, 128]),
                    (chroma::Location::Top, [128, 129, 113]),
                    (chroma::Location::BottomLeft, [128, 134, 69]),
                    (chroma::Location::Bottom, [128, 135, 54]),
                ] {
                    // The frame owns the metadata, and ffmpeg-next exposes
                    // only a getter for AVFrame::chroma_location.
                    unsafe { (*frame.as_mut_ptr()).chroma_location = location.into() };
                    let result = read_nv12(&gl, &frame, (4, 4), &[128; 16], &uv);
                    assert_rgb(&result[20..24], rgb);
                }
                unsafe { (*frame.as_mut_ptr()).chroma_location = chroma::Location::TopLeft.into() };
                let cropped = read_nv12(&gl, &frame, (2, 2), &[128; 16], &uv);
                assert_rgb(&cropped[12..16], [128, 137, 39]);
            })
            .unwrap();
    }
}
