//! Same-device VA-API to EGL DMA-BUF capability probe.
//! The probe creates and destroys EGL images within the mapped frame's lifetime.

use super::RemoteResult;
use ffmpeg_next::{ffi, format::Pixel, frame::Video};
use libloading::Library;
use std::collections::BTreeMap;
use std::ffi::{CStr, CString, c_char, c_void};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::ptr::null_mut;
use std::sync::Mutex;

const EGL_EXTENSIONS: i32 = 0x3055;
pub(super) const EGL_NONE: i32 = 0x3038;
const EGL_WIDTH: i32 = 0x3057;
const EGL_HEIGHT: i32 = 0x3056;
const EGL_DRM_RENDER_NODE_FILE_EXT: i32 = 0x3377;
const EGL_PLATFORM_DEVICE_EXT: u32 = 0x313f;
const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: i32 = 0x3271;
const DRM_FORMAT_MOD_INVALID: u64 = (1 << 56) - 1;
const NV12: u32 = u32::from_le_bytes(*b"NV12");
pub(super) const R8: u32 = u32::from_le_bytes(*b"R8  ");
pub(super) const GR88: u32 = u32::from_le_bytes(*b"GR88");
pub(super) const XRGB8888: u32 = u32::from_le_bytes(*b"XR24");

pub(super) type Handle = *mut c_void;
type GetProc = unsafe extern "C" fn(*const c_char) -> *const c_void;
type QueryDevices = unsafe extern "C" fn(i32, *mut Handle, *mut i32) -> u32;
type QueryDeviceString = unsafe extern "C" fn(Handle, i32) -> *const c_char;
type GetPlatformDisplay = unsafe extern "C" fn(u32, Handle, *const i32) -> Handle;
type Initialize = unsafe extern "C" fn(Handle, *mut i32, *mut i32) -> u32;
type Terminate = unsafe extern "C" fn(Handle) -> u32;
type GetCurrentContext = unsafe extern "C" fn() -> Handle;
type ReleaseThread = unsafe extern "C" fn() -> u32;
type QueryString = unsafe extern "C" fn(Handle, i32) -> *const c_char;
type CreateImage = unsafe extern "C" fn(Handle, Handle, u32, Handle, *const i32) -> Handle;
type DestroyImage = unsafe extern "C" fn(Handle, Handle) -> u32;
type GetError = unsafe extern "C" fn() -> i32;

// EGL initialization is per display, not per caller. Keep concurrent
// in-process viewers and probes from terminating each other's displays.
static EGL_OWNERS: Mutex<BTreeMap<usize, usize>> = Mutex::new(BTreeMap::new());

struct EglLibrary {
    library: Library,
    current_context: GetCurrentContext,
    release_thread: ReleaseThread,
}

impl Drop for EglLibrary {
    fn drop(&mut self) {
        // Device discovery and eglTerminate can use thread state. Retire it
        // while the runtime is still loaded, including on probe failure.
        unsafe {
            if (self.current_context)().is_null() {
                (self.release_thread)();
            }
        }
    }
}

pub(super) struct Display {
    _library: EglLibrary,
    pub(super) handle: Handle,
    get_proc: GetProc,
    terminate: Terminate,
    create_image: CreateImage,
    destroy_image: DestroyImage,
    get_error: GetError,
    pub(super) modifiers: bool,
}

impl Drop for Display {
    fn drop(&mut self) {
        let mut owners = EGL_OWNERS.lock().unwrap_or_else(|error| error.into_inner());
        let key = self.handle as usize;
        if let Some(count) = owners.get_mut(&key) {
            *count -= 1;
            if *count == 0 {
                owners.remove(&key);
                unsafe { (self.terminate)(self.handle) };
            }
        }
    }
}

impl Display {
    pub(super) fn open(node: &CStr) -> RemoteResult<Self> {
        // Load entry points from the runtime EGL implementation. The Display
        // keeps the library alive for every call and its final termination.
        unsafe {
            let library = Library::new("libEGL.so.1")?;
            let runtime = EglLibrary {
                current_context: *library.get::<GetCurrentContext>(b"eglGetCurrentContext")?,
                release_thread: *library.get::<ReleaseThread>(b"eglReleaseThread")?,
                library,
            };
            let get_proc = *runtime.library.get::<GetProc>(b"eglGetProcAddress")?;
            macro_rules! function {
                ($name:literal, $signature:ty) => {{
                    let entry = get_proc($name.as_ptr());
                    if entry.is_null() {
                        return Err(format!(
                            "EGL entry point {} is unavailable",
                            $name.to_string_lossy()
                        )
                        .into());
                    }
                    std::mem::transmute::<*const c_void, $signature>(entry)
                }};
            }
            let query_devices = function!(c"eglQueryDevicesEXT", QueryDevices);
            let query_device = function!(c"eglQueryDeviceStringEXT", QueryDeviceString);
            let get_display = function!(c"eglGetPlatformDisplayEXT", GetPlatformDisplay);
            let create_image = function!(c"eglCreateImageKHR", CreateImage);
            let destroy_image = function!(c"eglDestroyImageKHR", DestroyImage);
            let initialize = *runtime.library.get::<Initialize>(b"eglInitialize")?;
            let terminate = *runtime.library.get::<Terminate>(b"eglTerminate")?;
            let query_string = *runtime.library.get::<QueryString>(b"eglQueryString")?;
            let get_error = *runtime.library.get::<GetError>(b"eglGetError")?;
            let mut devices = [null_mut(); 16];
            let mut count = 0;
            if query_devices(devices.len() as i32, devices.as_mut_ptr(), &mut count) == 0 {
                return Err("EGL could not enumerate GPU devices".into());
            }
            let candidates = devices
                .get(..usize::try_from(count)?)
                .ok_or("invalid EGL device count")?;
            let device = candidates
                .iter()
                .copied()
                .find(|&device| {
                    let path = query_device(device, EGL_DRM_RENDER_NODE_FILE_EXT);
                    !path.is_null() && CStr::from_ptr(path) == node
                })
                .ok_or("EGL has no device for the selected DRM render node")?;
            let handle = get_display(EGL_PLATFORM_DEVICE_EXT, device, [EGL_NONE].as_ptr());
            if handle.is_null() {
                return Err(format!("EGL device selection failed (0x{:04x})", get_error()).into());
            }
            {
                let mut owners = EGL_OWNERS.lock().unwrap_or_else(|error| error.into_inner());
                let count = owners.entry(handle as usize).or_default();
                if *count == 0 && initialize(handle, null_mut(), null_mut()) == 0 {
                    owners.remove(&(handle as usize));
                    return Err(format!(
                        "EGL device initialization failed (0x{:04x})",
                        get_error()
                    )
                    .into());
                }
                *count += 1;
            }
            let mut display = Self {
                _library: runtime,
                handle,
                get_proc,
                terminate,
                create_image,
                destroy_image,
                get_error,
                modifiers: false,
            };
            let extensions = query_string(handle, EGL_EXTENSIONS);
            if extensions.is_null() {
                return Err("EGL device extensions are unavailable".into());
            }
            let extensions = CStr::from_ptr(extensions).to_str()?;
            let supports = |extension| {
                extensions
                    .split_ascii_whitespace()
                    .any(|item| item == extension)
            };
            if !supports("EGL_EXT_image_dma_buf_import") {
                return Err("EGL device cannot import DMA-BUF images".into());
            }
            display.modifiers = supports("EGL_EXT_image_dma_buf_import_modifiers");
            Ok(display)
        }
    }

    pub(super) fn proc(&self, name: &CStr) -> *const c_void {
        unsafe { (self.get_proc)(name.as_ptr()) }
    }

    pub(super) fn import(&self, attributes: &[i32]) -> RemoteResult<Image<'_>> {
        let handle = unsafe {
            (self.create_image)(
                self.handle,
                null_mut(),
                EGL_LINUX_DMA_BUF_EXT,
                null_mut(),
                attributes.as_ptr(),
            )
        };
        if handle.is_null() {
            let error = unsafe { (self.get_error)() };
            return Err(format!("EGL DMA-BUF import failed (0x{error:04x})").into());
        }
        Ok(Image {
            display: self,
            handle,
        })
    }
}

pub(super) struct Image<'a> {
    display: &'a Display,
    pub(super) handle: Handle,
}

impl Drop for Image<'_> {
    fn drop(&mut self) {
        unsafe { (self.display.destroy_image)(self.display.handle, self.handle) };
    }
}

pub(super) struct GpuFrameLease {
    mapped: Video,
}

impl GpuFrameLease {
    pub(super) fn map(decoded: &Video) -> RemoteResult<Self> {
        let mut mapped = Video::empty();
        mapped.set_format(Pixel::DRM_PRIME);
        let result = unsafe {
            ffi::av_hwframe_map(
                mapped.as_mut_ptr(),
                decoded.as_ptr(),
                ffi::AV_HWFRAME_MAP_READ as i32 | ffi::AV_HWFRAME_MAP_DIRECT as i32,
            )
        };
        if result < 0 {
            return Err(io::Error::other(format!(
                "VA-API direct export: {}",
                ffmpeg_next::Error::from(result)
            ))
            .into());
        }
        Ok(Self { mapped })
    }

    pub(super) fn descriptor(&self) -> RemoteResult<&ffi::AVDRMFrameDescriptor> {
        // av_hwframe_map places the DRM descriptor in data[0] for the life of
        // the mapped AVFrame. The export keeps the source and its fd owners alive.
        if self.mapped.format() != Pixel::DRM_PRIME {
            return Err("unexpected mapped GPU frame format".into());
        }
        let ptr = unsafe { (*self.mapped.as_ptr()).data[0].cast::<ffi::AVDRMFrameDescriptor>() };
        if ptr.is_null() || !ptr.is_aligned() {
            return Err("invalid DRM frame descriptor address".into());
        }
        Ok(unsafe { &*ptr })
    }
}

pub(super) fn attributes(
    layer: &ffi::AVDRMLayerDescriptor,
    objects: &[ffi::AVDRMObjectDescriptor],
    size: (i32, i32),
    modifiers: bool,
) -> RemoteResult<Vec<i32>> {
    let [width, height] = [size.0, size.1];
    let expected_planes = match layer.format {
        NV12 => 2,
        R8 | GR88 | XRGB8888 => 1,
        _ => return Err("unsupported DRM layer format".into()),
    };
    if usize::try_from(layer.nb_planes)? != expected_planes {
        return Err("unexpected DRM plane count".into());
    }
    let mut values = vec![
        EGL_WIDTH,
        width,
        EGL_HEIGHT,
        height,
        EGL_LINUX_DRM_FOURCC_EXT,
        layer.format as i32,
    ];
    for (index, plane) in layer.planes[..expected_planes].iter().enumerate() {
        let object = objects
            .get(usize::try_from(plane.object_index)?)
            .ok_or("unknown DRM plane object")?;
        let offset = i32::try_from(plane.offset)?;
        let pitch = i32::try_from(plane.pitch)?;
        let rows = if layer.format == NV12 && index == 1 {
            height / 2
        } else {
            height
        };
        let row_bytes = match layer.format {
            GR88 => width * 2,
            XRGB8888 => width * 4,
            _ => width,
        };
        let end = i64::from(offset) + i64::from(rows - 1) * i64::from(pitch) + i64::from(row_bytes);
        if object.fd < 0 || offset < 0 || pitch < row_bytes || usize::try_from(end)? > object.size {
            return Err("invalid DRM plane bounds".into());
        }
        let key = 0x3272 + index as i32 * 3;
        values.extend([key, object.fd, key + 1, offset, key + 2, pitch]);
        let modifier = object.format_modifier;
        if modifier != DRM_FORMAT_MOD_INVALID {
            if modifiers {
                let key = 0x3443 + index as i32 * 2;
                values.extend([key, modifier as i32, key + 1, (modifier >> 32) as i32]);
            } else if modifier != 0 {
                return Err("nonlinear DRM allocation requires EGL modifier support".into());
            }
        }
    }
    values.push(EGL_NONE);
    Ok(values)
}

pub(super) fn probe(render_node: &Path) -> RemoteResult<String> {
    let node = render_node.canonicalize()?;
    let node_name = CString::new(node.as_os_str().as_bytes())?;
    let display = Display::open(&node_name)?;
    super::h264::with_vaapi_probe_surface(&node_name, super::h264::H264_PROBE, |decoded| {
        let lease = GpuFrameLease::map(decoded)?;
        let descriptor = lease.descriptor()?;
        let objects = descriptor
            .objects
            .get(..usize::try_from(descriptor.nb_objects)?)
            .ok_or("invalid DRM object count")?;
        let layers = descriptor
            .layers
            .get(..usize::try_from(descriptor.nb_layers)?)
            .ok_or("invalid DRM layer count")?;
        let sizes = match layers {
            [single] if single.format == NV12 => &[(256, 256)][..],
            [luma, chroma] if luma.format == R8 && chroma.format == GR88 => {
                &[(256, 256), (128, 128)][..]
            }
            _ => return Err("VA-API probe did not export supported NV12 layers".into()),
        };
        let mut images = Vec::new();
        for (layer, &size) in layers.iter().zip(sizes) {
            images.push(display.import(&attributes(layer, objects, size, display.modifiers)?)?);
        }
        Ok(format!(
            "H.264 VA-API NV12 on {} imported into EGL ({} layer(s))",
            node.display(),
            images.len()
        ))
    })
}
