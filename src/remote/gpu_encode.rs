//! Import a test DMA-BUF into a separate VA-API encoder frames context.

use super::{
    RemoteResult,
    gpu_import::{self, GR88, GpuFrameLease, Handle, R8},
    gpu_present::{VaSync, vaapi_handle},
    h264, h264_wire,
};
use ffmpeg_next::{self as ffmpeg, codec, ffi, frame::Video};
use libloading::Library;
use std::ffi::{CString, c_void};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::ptr::null_mut;

#[path = "gpu_host_video.rs"]
pub(super) mod gpu_host_video;

const WIDTH: u32 = 256;
const HEIGHT: u32 = 256;
const PICTURE: &[u8] = include_bytes!("native-video-probe.h264");
const VAAPI: ffi::AVPixelFormat = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
const NV12: ffi::AVPixelFormat = ffi::AVPixelFormat::AV_PIX_FMT_NV12;

const FOURCC_NV12: u32 = u32::from_le_bytes(*b"NV12");
const SURFACE_SETTABLE: u32 = 2;
const MEMORY_DRM_PRIME2: i32 = 0x40000000;
const VA_RT_FORMAT_YUV420: u32 = 1;
const SURFACE_MEMORY_TYPE: i32 = 6;
const EXTERNAL_BUFFER_DESCRIPTOR: i32 = 7;

// Public libva ABI from va.h and va_drmcommon.h. The legacy FFmpeg 4.4
// DRM importer expects different chroma-layer tags and ignores modifiers.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PrimeObject {
    fd: i32,
    size: u32,
    modifier: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PrimeLayer {
    format: u32,
    planes: u32,
    objects: [u32; 4],
    offsets: [u32; 4],
    pitches: [u32; 4],
}

#[repr(C)]
struct PrimeDescriptor {
    format: u32,
    width: u32,
    height: u32,
    object_count: u32,
    objects: [PrimeObject; 4],
    layer_count: u32,
    layers: [PrimeLayer; 4],
}

impl PrimeDescriptor {
    fn nv12(lease: &GpuFrameLease) -> RemoteResult<Self> {
        let descriptor = lease.descriptor()?;
        let count = usize::try_from(descriptor.nb_objects)?;
        let objects = descriptor
            .objects
            .get(..count)
            .ok_or("invalid VA-API object count")?;
        let [luma, chroma, ..] = &descriptor.layers;
        if descriptor.nb_layers != 2 || luma.format != R8 || chroma.format != GR88 {
            return Err("encoder probe requires separate NV12 R8 and GR88 layers".into());
        }
        let mut result = Self {
            format: FOURCC_NV12,
            width: WIDTH,
            height: HEIGHT,
            object_count: count as u32,
            objects: [PrimeObject::default(); 4],
            layer_count: 1,
            layers: [PrimeLayer::default(); 4],
        };
        let layer = &mut result.layers[0];
        layer.format = FOURCC_NV12;
        layer.planes = 2;
        for (index, (plane, extent)) in [(luma, (256, 256)), (chroma, (128, 128))]
            .into_iter()
            .enumerate()
        {
            // Apply the same fd, span, pitch and object index checks as EGL
            // import before handing the layout to the driver.
            gpu_import::attributes(plane, objects, extent, true)?;
            let first = plane.planes[0];
            layer.objects[index] = u32::try_from(first.object_index)?;
            layer.offsets[index] = u32::try_from(first.offset)?;
            layer.pitches[index] = u32::try_from(first.pitch)?;
        }
        for (destination, object) in result.objects.iter_mut().zip(objects) {
            *destination = PrimeObject {
                fd: object.fd,
                size: u32::try_from(object.size)?,
                modifier: object.format_modifier,
            };
        }
        Ok(result)
    }
}

#[repr(C)]
union GenericValue {
    integer: i32,
    pointer: *mut c_void,
}

#[repr(C)]
struct SurfaceAttribute {
    kind: i32,
    flags: u32,
    value_type: i32,
    value: GenericValue,
}

type CreateSurfaces =
    unsafe extern "C" fn(Handle, u32, u32, u32, *mut u32, u32, *mut SurfaceAttribute, u32) -> i32;
type DestroySurfaces = unsafe extern "C" fn(Handle, *mut u32, i32) -> i32;

struct ImportedSurface {
    _library: Library,
    destroy: DestroySurfaces,
    display: Handle,
    id: u32,
    _source: GpuFrameLease,
}

impl ImportedSurface {
    fn open(display: Handle, source: GpuFrameLease) -> RemoteResult<Self> {
        let mut descriptor = PrimeDescriptor::nv12(&source)?;
        let mut attributes = [
            SurfaceAttribute {
                kind: SURFACE_MEMORY_TYPE,
                flags: SURFACE_SETTABLE,
                value_type: 1,
                value: GenericValue {
                    integer: MEMORY_DRM_PRIME2,
                },
            },
            SurfaceAttribute {
                kind: EXTERNAL_BUFFER_DESCRIPTOR,
                flags: SURFACE_SETTABLE,
                value_type: 3,
                value: GenericValue {
                    pointer: (&mut descriptor as *mut PrimeDescriptor).cast(),
                },
            },
        ];
        let mut id = u32::MAX;
        // The PRIME_2 import consumes the descriptors during the call. The
        // source continues to own the fd and VADisplay for the whole lease.
        unsafe {
            let library = Library::new("libva.so.2")?;
            let create = *library.get::<CreateSurfaces>(b"vaCreateSurfaces")?;
            let destroy = *library.get::<DestroySurfaces>(b"vaDestroySurfaces")?;
            let status = create(
                display,
                VA_RT_FORMAT_YUV420,
                WIDTH,
                HEIGHT,
                &mut id,
                1,
                attributes.as_mut_ptr(),
                attributes.len() as u32,
            );
            if status != 0 || id == u32::MAX {
                return Err(format!("VA-API PRIME_2 import failed (0x{status:x})").into());
            }
            Ok(Self {
                _library: library,
                destroy,
                display,
                id,
                _source: source,
            })
        }
    }
}

impl Drop for ImportedSurface {
    fn drop(&mut self) {
        unsafe { (self.destroy)(self.display, &mut self.id, 1) };
    }
}

unsafe extern "C" fn free_surface(opaque: *mut c_void, _data: *mut u8) {
    // The buffer is the unique owner of the boxed surface record.
    drop(unsafe { Box::from_raw(opaque.cast::<ImportedSurface>()) });
}

struct FramesContext(*mut ffi::AVBufferRef);

impl FramesContext {
    fn for_surface(source: &Video) -> RemoteResult<Self> {
        // The decoded surface owns its hardware/device context. The newly
        // allocated encoder context takes its own reference to that device.
        unsafe {
            let input = (*source.as_ptr())
                .hw_frames_ctx
                .as_ref()
                .ok_or("surface has no frames context")?;
            let decoder = input
                .data
                .cast::<ffi::AVHWFramesContext>()
                .as_ref()
                .ok_or("missing decoder frames context")?;
            if decoder.format != VAAPI || decoder.sw_format != NV12 || decoder.device_ref.is_null()
            {
                return Err(
                    "VA-API encode probe requires an NV12 surface on the selected device".into(),
                );
            }
            let buf = ffi::av_hwframe_ctx_alloc(decoder.device_ref);
            if buf.is_null() {
                return Err(io::Error::other("allocating VA-API encoder frames context").into());
            }
            let owned = Self(buf);
            let frames = &mut *(*buf).data.cast::<ffi::AVHWFramesContext>();
            frames.format = VAAPI;
            frames.sw_format = NV12;
            frames.width = WIDTH as i32;
            frames.height = HEIGHT as i32;
            check(
                "initializing VA-API encoder frames context",
                ffi::av_hwframe_ctx_init(owned.0),
            )?;
            Ok(owned)
        }
    }

    fn reference(&self) -> RemoteResult<*mut ffi::AVBufferRef> {
        let reference = unsafe { ffi::av_buffer_ref(self.0) };
        if reference.is_null() {
            return Err(io::Error::other("retaining VA-API encoder frames context").into());
        }
        Ok(reference)
    }

    fn import(&self, source: GpuFrameLease, display: Handle) -> RemoteResult<Video> {
        let mut input = Video::empty();
        let surface = Box::new(ImportedSurface::open(display, source)?);
        // avcodec may retain or clone this frame. The AVBufferRef keeps the
        // imported surface and the underlying DMA-BUF leased in both cases.
        unsafe {
            let frame = &mut *input.as_mut_ptr();
            frame.format = VAAPI as i32;
            frame.width = WIDTH as i32;
            frame.height = HEIGHT as i32;
            frame.hw_frames_ctx = self.reference()?;
            frame.data[3] = surface.id as usize as *mut u8;
            let opaque = Box::into_raw(surface);
            let reference =
                ffi::av_buffer_create(null_mut(), 0, Some(free_surface), opaque.cast(), 0);
            if reference.is_null() {
                drop(Box::from_raw(opaque));
                return Err(io::Error::other("retaining imported VA-API surface").into());
            }
            frame.buf[0] = reference;
        }
        Ok(input)
    }
}

impl Drop for FramesContext {
    fn drop(&mut self) {
        unsafe { ffi::av_buffer_unref(&mut self.0) };
    }
}

fn check(operation: &str, result: i32) -> RemoteResult<()> {
    if result < 0 {
        return Err(
            io::Error::other(format!("{operation}: {}", ffmpeg::Error::from(result))).into(),
        );
    }
    Ok(())
}

fn encode(frames: &FramesContext, input: &mut Video) -> RemoteResult<Vec<u8>> {
    let codec =
        ffmpeg::encoder::find_by_name("h264_vaapi").ok_or("FFmpeg h264_vaapi unavailable")?;
    let mut encoder = codec::Context::new_with_codec(codec).encoder().video()?;
    encoder.set_width(WIDTH);
    encoder.set_height(HEIGHT);
    encoder.set_format(VAAPI.into());
    encoder.set_time_base((1, 30));
    encoder.set_frame_rate(Some((30, 1)));
    encoder.set_gop(30);
    encoder.set_max_b_frames(0);
    encoder.set_bit_rate(2_000_000);
    let reference = frames.reference()?;
    unsafe { (*encoder.as_mut_ptr()).hw_frames_ctx = reference };
    let mut options = ffmpeg::Dictionary::new();
    options.set("async_depth", "1");
    let mut encoder = encoder.open_as_with(codec, options)?;
    input.set_pts(Some(0));
    input.set_kind(ffmpeg::picture::Type::I);
    encoder.send_frame(input)?;
    let mut packet = ffmpeg::Packet::empty();
    encoder.receive_packet(&mut packet)?;
    let bytes = packet
        .data()
        .ok_or("VA-API encoder produced an empty packet")?;
    if bytes.len() > 1024 * 1024
        || packet.pts() != Some(0)
        || !packet.is_key()
        || !h264_wire::hardware_has_idr(super::messages::FrameCodec::H264, bytes)
    {
        return Err("VA-API encoder did not return the bounded test IDR".into());
    }
    Ok(bytes.to_vec())
}

pub(super) fn probe(render_node: &Path) -> RemoteResult<String> {
    let path = render_node.canonicalize()?;
    let node = CString::new(path.as_os_str().as_bytes())?;
    let sync = VaSync::new()?;
    let encoded = h264::with_vaapi_probe_surface(&node, PICTURE, |source| {
        sync.wait(source)?;
        let exported = GpuFrameLease::map(source)?;
        let frames = FramesContext::for_surface(source)?;
        let (display, _) = vaapi_handle(source)?;
        let mut input = frames.import(exported, display)?;
        encode(&frames, &mut input)
    })?;
    // A decoded CPU reference is used only to validate the test packet.
    let expected = h264::vaapi_probe_rgb(&node, PICTURE)?;
    let decoded = h264::vaapi_probe_rgb(&node, &encoded)?;
    for (x, y) in [(16, 16), (240, 16), (16, 240), (240, 240)] {
        let index = (y * WIDTH as usize + x) * 3;
        if decoded[index..index + 3]
            .iter()
            .zip(&expected[index..index + 3])
            .any(|(actual, expected)| actual.abs_diff(*expected) > 10)
        {
            return Err(
                format!("GPU-encoded picture differs from the test color at ({x},{y})").into(),
            );
        }
    }
    Ok(format!(
        "NV12 DMA-BUF imported into h264_vaapi on {} and produced a {}-byte IDR matching four test colors",
        path.display(),
        encoded.len(),
    ))
}
