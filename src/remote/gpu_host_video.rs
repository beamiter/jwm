//! Native XRGB capture buffers and same-device VA-API H.264 encoding.
use super::{
    CreateSurfaces, DestroySurfaces, EXTERNAL_BUFFER_DESCRIPTOR, FramesContext, GenericValue,
    Handle, MEMORY_DRM_PRIME2, NV12, PrimeDescriptor, PrimeLayer, PrimeObject, RemoteResult,
    SURFACE_MEMORY_TYPE, SURFACE_SETTABLE, SurfaceAttribute, VAAPI, Video, check, codec, ffi,
};
use crate::remote::{gpu_import::XRGB8888, gpu_present::VaSync};
use ffmpeg_next::{self as ffmpeg, color};
use libloading::Library;
#[cfg(feature = "remote-wayland")]
use std::os::fd::{AsFd, BorrowedFd};
use std::{
    ffi::{CString, c_void},
    fs::File,
    io::{self, Seek, SeekFrom},
    os::{fd::AsRawFd, unix::ffi::OsStrExt},
    path::Path,
    ptr::null_mut,
    time::{Duration, Instant},
};
use x11rb::{
    connection::Connection,
    protocol::{
        dri3, sync,
        xproto::{
            ConnectionExt as _, CreateGCAux, Drawable, Gcontext, Pixmap, Rectangle, SubwindowMode,
        },
    },
    rust_connection::RustConnection,
};

const BGRX: u32 = u32::from_le_bytes(*b"BGRX");
const VA_RGB32: u32 = 0x0002_0000;
const VA_INVALID_ID: u32 = u32::MAX;
const PIXEL_FORMAT: i32 = 1;
const VA_VIDEO_PROC: i32 = 10;
const VA_PROGRESSIVE: i32 = 1;
const VA_PIPELINE_PARAMETER: i32 = 41;
const BT709: i32 = 2;
const FENCE_LIMIT: Duration = Duration::from_millis(100);

pub(in crate::remote) struct Frame {
    pub(in crate::remote) pixels: Pixels,
    pub(in crate::remote) geometry_epoch: u64,
    pub(in crate::remote) source: (u16, u16),
    pub(in crate::remote) encoded: (u16, u16),
}

/// An immutable capture snapshot. The exported fd owns the storage.
/// Mailbox replacement drops the fd immediately.
pub(in crate::remote) struct Pixels {
    memory: File,
    visible: (u16, u16),
    storage: (u16, u16),
    size: u32,
    pitch: u32,
    offset: u32,
    modifier: u64,
}

impl Pixels {
    pub(in crate::remote) fn from_xrgb(
        mut memory: File,
        visible: (u16, u16),
        storage: (u16, u16),
        pitch: u32,
        offset: u32,
        modifier: u64,
    ) -> RemoteResult<Self> {
        if visible.0 == 0
            || visible.1 == 0
            || visible.0 > storage.0
            || visible.1 > storage.1
            || modifier == (1 << 56) - 1
        {
            return Err("invalid capture DMA-BUF dimensions or modifier".into());
        }
        let size = u32::try_from(memory.seek(SeekFrom::End(0))?)?;
        let row_bytes = u64::from(storage.0) * 4;
        let end = u64::from(offset) + u64::from(storage.1 - 1) * u64::from(pitch) + row_bytes;
        if u64::from(pitch) < row_bytes || end > u64::from(size) {
            return Err("invalid capture DMA-BUF span".into());
        }
        Ok(Self {
            memory,
            visible,
            storage,
            size,
            pitch,
            offset,
            modifier,
        })
    }

    #[cfg(feature = "remote-wayland")]
    pub(in crate::remote) fn wayland_plane(&self) -> (BorrowedFd<'_>, u32, u32) {
        (self.memory.as_fd(), self.offset, self.pitch)
    }
}

struct Staging<'a> {
    conn: &'a RustConnection,
    pixmap: Pixmap,
    gc: Option<Gcontext>,
    fence: Option<sync::Fence>,
}

impl<'a> Staging<'a> {
    fn new(
        conn: &'a RustConnection,
        root: Drawable,
        size: (u16, u16),
        color: u32,
    ) -> RemoteResult<Self> {
        let pixmap = conn.generate_id()?;
        conn.create_pixmap(24, pixmap, root, size.0, size.1)?
            .check()?;
        let mut target = Self {
            conn,
            pixmap,
            gc: None,
            fence: None,
        };
        let gc = conn.generate_id()?;
        conn.create_gc(
            gc,
            pixmap,
            &CreateGCAux::new()
                .foreground(color)
                .subwindow_mode(SubwindowMode::INCLUDE_INFERIORS)
                .graphics_exposures(0),
        )?
        .check()?;
        target.gc = Some(gc);
        // Cleared before the copy so a concurrent server clip cannot expose
        // stale bytes in uncovered parts of a newly allocated pixmap.
        conn.poly_fill_rectangle(
            pixmap,
            gc,
            &[Rectangle {
                x: 0,
                y: 0,
                width: size.0,
                height: size.1,
            }],
        )?
        .check()?;
        Ok(target)
    }

    fn finish_copy(&mut self) -> RemoteResult<()> {
        let fence = self.conn.generate_id()?;
        sync::create_fence(self.conn, self.pixmap, fence, false)?.check()?;
        self.fence = Some(fence);
        sync::trigger_fence(self.conn, fence)?.check()?;
        let deadline = Instant::now() + FENCE_LIMIT;
        loop {
            if sync::query_fence(self.conn, fence)?.reply()?.triggered {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(
                    io::Error::new(io::ErrorKind::TimedOut, "X11 capture fence timed out").into(),
                );
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

impl Drop for Staging<'_> {
    fn drop(&mut self) {
        if let Some(fence) = self.fence {
            let _ = sync::destroy_fence(self.conn, fence);
        }
        if let Some(gc) = self.gc {
            let _ = self.conn.free_gc(gc);
        }
        let _ = self.conn.free_pixmap(self.pixmap);
    }
}

pub(in crate::remote) fn copy_x11(
    conn: &RustConnection,
    root: Drawable,
    source: Drawable,
    origin: (i16, i16),
    dimensions: (u16, u16),
) -> RemoteResult<Pixels> {
    crate::remote::h264_wire::h264_padded_dimensions(dimensions.0, dimensions.1)?;
    let mut target = Staging::new(conn, root, dimensions, 0)?;
    conn.copy_area(
        source,
        target.pixmap,
        target.gc.unwrap(),
        origin.0,
        origin.1,
        0,
        0,
        dimensions.0,
        dimensions.1,
    )?
    .check()?;
    // Export may migrate or flush the pixmap's GPU storage. Fence after
    // export and resolve the server's rendering before giving it to VA-API.
    let reply = dri3::buffers_from_pixmap(conn, target.pixmap)?.reply()?;
    target.finish_copy()?;
    if (reply.width, reply.height, reply.depth, reply.bpp) != (dimensions.0, dimensions.1, 24, 32)
        || reply.buffers.len() != 1
        || reply.strides.len() != 1
        || reply.offsets.len() != 1
        || reply.modifier == (1 << 56) - 1
    {
        return Err("unsupported X11 capture DMA-BUF layout".into());
    }
    Pixels::from_xrgb(
        File::from(reply.buffers.into_iter().next().unwrap()),
        dimensions,
        dimensions,
        reply.strides[0],
        reply.offsets[0],
        reply.modifier,
    )
}

pub(in crate::remote) fn probe(
    conn: &RustConnection,
    root: Drawable,
    node: &Path,
    fps: u16,
) -> RemoteResult<()> {
    let version = sync::initialize(conn, 3, 1)?.reply()?;
    if (version.major_version, version.minor_version) < (3, 1) {
        return Err("GPU capture requires X11 SYNC fences".into());
    }
    let size = (256, 256);
    let source = Staging::new(conn, root, size, 0xda6c31)?;
    let pixels = copy_x11(conn, root, source.pixmap, (0, 0), size)?;
    let mut encoder = Encoder::new(node, size, fps)?;
    let mut encoded = Vec::new();
    encoder.encode(&pixels, 0, true, &mut encoded)?;
    if !crate::remote::h264_wire::hardware_has_idr(
        crate::remote::messages::FrameCodec::H264,
        &encoded,
    ) {
        return Err("VA-API capture probe did not produce an IDR".into());
    }
    Ok(())
}

#[repr(C)]
struct VaRectangle {
    x: i16,
    y: i16,
    width: u16,
    height: u16,
}

// VAProcPipelineParameterBuffer, libva 2.14 and later. Pointer-width-specific
// tail sizes preserve its C ABI. We use only the two rectangles and color.
#[repr(C)]
struct PipelineParameters {
    surface: u32,
    source: *const VaRectangle,
    source_color: i32,
    output: *const VaRectangle,
    background: u32,
    output_color: i32,
    pipeline_flags: u32,
    filter_flags: u32,
    filters: *const u32,
    filter_count: u32,
    forward: *const u32,
    forward_count: u32,
    backward: *const u32,
    backward_count: u32,
    rotation: u32,
    blend: *const c_void,
    mirror: u32,
    additional: *const u32,
    additional_count: u32,
    input_flags: u32,
    output_flags: u32,
    input_color: [u8; 8],
    output_color_properties: [u8; 8],
    processing: i32,
    hdr: *const c_void,
    #[cfg(target_pointer_width = "64")]
    reserved: [u32; 16],
    #[cfg(target_pointer_width = "32")]
    reserved: [u32; 19],
}

#[cfg(target_arch = "x86_64")]
const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<PipelineParameters>() == 224);
    assert!(offset_of!(PipelineParameters, source) == 8);
    assert!(offset_of!(PipelineParameters, output) == 24);
    assert!(offset_of!(PipelineParameters, input_color) == 132);
    assert!(offset_of!(PipelineParameters, hdr) == 152);
};

type DestroyId = unsafe extern "C" fn(Handle, u32) -> i32;

struct VaApi {
    create_surfaces: CreateSurfaces,
    destroy_surfaces: DestroySurfaces,
    create_config: unsafe extern "C" fn(Handle, i32, i32, *const c_void, i32, *mut u32) -> i32,
    destroy_config: DestroyId,
    create_context:
        unsafe extern "C" fn(Handle, u32, i32, i32, i32, *mut u32, i32, *mut u32) -> i32,
    destroy_context: DestroyId,
    create_buffer: unsafe extern "C" fn(Handle, u32, i32, u32, u32, *mut c_void, *mut u32) -> i32,
    destroy_buffer: DestroyId,
    begin: unsafe extern "C" fn(Handle, u32, u32) -> i32,
    render: unsafe extern "C" fn(Handle, u32, *mut u32, i32) -> i32,
    end: unsafe extern "C" fn(Handle, u32) -> i32,
    _library: Library,
}

impl VaApi {
    fn open() -> RemoteResult<Self> {
        // The loaded library outlives all calls and resource guards.
        unsafe {
            let library = Library::new("libva.so.2")?;
            Ok(Self {
                create_surfaces: *library.get(b"vaCreateSurfaces")?,
                destroy_surfaces: *library.get(b"vaDestroySurfaces")?,
                create_config: *library.get(b"vaCreateConfig")?,
                destroy_config: *library.get(b"vaDestroyConfig")?,
                create_context: *library.get(b"vaCreateContext")?,
                destroy_context: *library.get(b"vaDestroyContext")?,
                create_buffer: *library.get(b"vaCreateBuffer")?,
                destroy_buffer: *library.get(b"vaDestroyBuffer")?,
                begin: *library.get(b"vaBeginPicture")?,
                render: *library.get(b"vaRenderPicture")?,
                end: *library.get(b"vaEndPicture")?,
                _library: library,
            })
        }
    }
}

fn va_check(operation: &str, status: i32) -> RemoteResult<()> {
    if status != 0 {
        return Err(io::Error::other(format!("{operation}: VA-API status 0x{status:x}")).into());
    }
    Ok(())
}

struct VaId {
    id: u32,
    display: Handle,
    destroy: DestroyId,
}

impl Drop for VaId {
    fn drop(&mut self) {
        if self.id != VA_INVALID_ID {
            unsafe { (self.destroy)(self.display, self.id) };
        }
    }
}

struct ImportedRgb<'a> {
    display: Handle,
    id: u32,
    api: &'a VaApi,
    _pixels: &'a Pixels,
}

impl<'a> ImportedRgb<'a> {
    fn open(api: &'a VaApi, display: Handle, pixels: &'a Pixels) -> RemoteResult<Self> {
        let (width, height) = (u32::from(pixels.storage.0), u32::from(pixels.storage.1));
        let mut descriptor = PrimeDescriptor {
            format: BGRX,
            width,
            height,
            object_count: 1,
            objects: [PrimeObject::default(); 4],
            layer_count: 1,
            layers: [PrimeLayer::default(); 4],
        };
        descriptor.objects[0] = PrimeObject {
            fd: pixels.memory.as_raw_fd(),
            size: pixels.size,
            modifier: pixels.modifier,
        };
        descriptor.layers[0] = PrimeLayer {
            format: XRGB8888,
            planes: 1,
            objects: [0; 4],
            offsets: [pixels.offset, 0, 0, 0],
            pitches: [pixels.pitch, 0, 0, 0],
        };
        let integer = |kind, integer| SurfaceAttribute {
            kind,
            flags: SURFACE_SETTABLE,
            value_type: 1,
            value: GenericValue { integer },
        };
        let mut attributes = [
            integer(SURFACE_MEMORY_TYPE, MEMORY_DRM_PRIME2),
            SurfaceAttribute {
                kind: EXTERNAL_BUFFER_DESCRIPTOR,
                flags: SURFACE_SETTABLE,
                value_type: 3,
                value: GenericValue {
                    pointer: (&mut descriptor as *mut PrimeDescriptor).cast(),
                },
            },
            integer(PIXEL_FORMAT, BGRX as i32),
        ];
        let mut result = Self {
            display,
            id: VA_INVALID_ID,
            api,
            _pixels: pixels,
        };
        va_check("importing the capture DMA-BUF", unsafe {
            (api.create_surfaces)(
                display,
                VA_RGB32,
                width,
                height,
                &mut result.id,
                1,
                attributes.as_mut_ptr(),
                attributes.len() as u32,
            )
        })?;
        Ok(result)
    }
}

impl Drop for ImportedRgb<'_> {
    fn drop(&mut self) {
        if self.id != VA_INVALID_ID {
            unsafe { (self.api.destroy_surfaces)(self.display, &mut self.id, 1) };
        }
    }
}

pub(in crate::remote) struct Encoder {
    encoder: codec::encoder::video::Encoder,
    config: VaId,
    sync: VaSync,
    api: VaApi,
    frames: FramesContext,
    size: (u16, u16),
}

pub(in crate::remote) struct Timings {
    pub(in crate::remote) import: Duration,
    pub(in crate::remote) conversion: Duration,
    pub(in crate::remote) submit: Duration,
    pub(in crate::remote) output: Duration,
}

impl Encoder {
    pub(in crate::remote) fn new(node: &Path, size: (u16, u16), fps: u16) -> RemoteResult<Self> {
        ffmpeg::init()?;
        let node = CString::new(node.as_os_str().as_bytes())?;
        let api = VaApi::open()?;
        let sync = VaSync::new()?;
        let mut device = FramesContext(null_mut());
        let (frames, display) = unsafe {
            check(
                "opening the VA-API encode device",
                ffi::av_hwdevice_ctx_create(
                    &mut device.0,
                    ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                    node.as_ptr(),
                    null_mut(),
                    0,
                ),
            )?;
            let device_context = &*(*device.0).data.cast::<ffi::AVHWDeviceContext>();
            let display = *device_context.hwctx.cast::<Handle>();
            if display.is_null() {
                return Err("VA-API encoder has no display".into());
            }
            let frames = FramesContext(ffi::av_hwframe_ctx_alloc(device.0));
            if frames.0.is_null() {
                return Err("unable to allocate VA-API encoder frames context".into());
            }
            let context = &mut *(*frames.0).data.cast::<ffi::AVHWFramesContext>();
            context.format = VAAPI;
            context.sw_format = NV12;
            context.width = i32::from(size.0);
            context.height = i32::from(size.1);
            context.initial_pool_size = 4;
            check(
                "allocating the NV12 encode pool",
                ffi::av_hwframe_ctx_init(frames.0),
            )?;
            (frames, display)
        };
        let mut config = VaId {
            id: VA_INVALID_ID,
            display,
            destroy: api.destroy_config,
        };
        va_check("creating the VA-API video processing config", unsafe {
            (api.create_config)(
                display,
                -1,
                VA_VIDEO_PROC,
                std::ptr::null(),
                0,
                &mut config.id,
            )
        })?;
        let codec =
            ffmpeg::encoder::find_by_name("h264_vaapi").ok_or("FFmpeg h264_vaapi unavailable")?;
        let mut encoder = codec::Context::new_with_codec(codec).encoder().video()?;
        encoder.set_width(u32::from(size.0));
        encoder.set_height(u32::from(size.1));
        encoder.set_format(VAAPI.into());
        encoder.set_time_base((1, i32::from(fps)));
        encoder.set_frame_rate(Some((i32::from(fps), 1)));
        encoder.set_gop(u32::from(fps));
        encoder.set_max_b_frames(0);
        encoder.set_bit_rate(4_000_000);
        encoder.set_colorspace(color::Space::BT709);
        encoder.set_color_range(color::Range::MPEG);
        unsafe { (*encoder.as_mut_ptr()).hw_frames_ctx = frames.reference()? };
        let mut options = ffmpeg::Dictionary::new();
        options.set("async_depth", "1");
        let encoder = encoder.open_as_with(codec, options)?;
        Ok(Self {
            encoder,
            config,
            sync,
            api,
            frames,
            size,
        })
    }

    pub(in crate::remote) fn encode(
        &mut self,
        pixels: &Pixels,
        sequence: u64,
        fresh: bool,
        output: &mut Vec<u8>,
    ) -> RemoteResult<Timings> {
        let started = Instant::now();
        let display = self.config.display;
        let source = ImportedRgb::open(&self.api, display, pixels)?;
        let imported = Instant::now();
        let mut frame = Video::empty();
        check("acquiring an NV12 encode surface", unsafe {
            ffi::av_hwframe_get_buffer(self.frames.0, frame.as_mut_ptr(), 0)
        })?;
        let target = super::vaapi_handle(&frame)?.1;
        self.convert(source.id, target, pixels.visible, &frame)?;
        let converted = Instant::now();
        let pts = i64::try_from(sequence)?;
        frame.set_pts(Some(pts));
        if fresh {
            frame.set_kind(ffmpeg::picture::Type::I);
        }
        self.encoder.send_frame(&frame)?;
        let submitted = Instant::now();
        let mut packet = ffmpeg::Packet::empty();
        self.encoder.receive_packet(&mut packet)?;
        if packet.pts() != Some(pts) || (fresh && !packet.is_key()) {
            return Err("VA-API encode returned an unexpected packet".into());
        }
        output.clear();
        output.extend_from_slice(packet.data().ok_or("VA-API returned an empty packet")?);
        Ok(Timings {
            import: imported - started,
            conversion: converted - imported,
            submit: submitted - converted,
            output: submitted.elapsed(),
        })
    }

    fn convert(
        &self,
        source: u32,
        mut target: u32,
        size: (u16, u16),
        frame: &Video,
    ) -> RemoteResult<()> {
        let api = &self.api;
        let display = self.config.display;
        let mut context = VaId {
            id: VA_INVALID_ID,
            display,
            destroy: api.destroy_context,
        };
        let rect = |(width, height)| VaRectangle {
            x: 0,
            y: 0,
            width,
            height,
        };
        let input = rect(size);
        let output = rect(self.size);
        // Unused pointers, enums and reserved fields are zero per the libva
        // ABI. The VA buffer copies this struct, borrowing the two rectangles
        // until rendering has been submitted.
        let mut parameters: PipelineParameters = unsafe { std::mem::zeroed() };
        parameters.surface = source;
        parameters.source = &input;
        parameters.output = &output;
        parameters.background = 0xff000000;
        parameters.output_color = BT709;
        let mut buffer = VaId {
            id: VA_INVALID_ID,
            display,
            destroy: api.destroy_buffer,
        };
        unsafe {
            va_check(
                "creating the VA-API conversion context",
                (api.create_context)(
                    display,
                    self.config.id,
                    i32::from(self.size.0),
                    i32::from(self.size.1),
                    VA_PROGRESSIVE,
                    &mut target,
                    1,
                    &mut context.id,
                ),
            )?;
            va_check(
                "creating the VA-API conversion parameters",
                (api.create_buffer)(
                    display,
                    context.id,
                    VA_PIPELINE_PARAMETER,
                    std::mem::size_of::<PipelineParameters>() as u32,
                    1,
                    (&mut parameters as *mut PipelineParameters).cast(),
                    &mut buffer.id,
                ),
            )?;
            va_check(
                "beginning RGB conversion",
                (api.begin)(display, context.id, target),
            )?;
            let rendered = (api.render)(display, context.id, &mut buffer.id, 1);
            let ended = (api.end)(display, context.id);
            va_check("submitting RGB conversion", rendered)?;
            va_check("ending RGB conversion", ended)?;
        }
        self.sync.wait(frame)
    }
}
