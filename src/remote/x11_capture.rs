//! Read the final X11 compositor surface through the Composite overlay.
//!
//! This is intentionally kept in the out-of-process LAN MVP.  A slow encoder
//! or peer can therefore never stall JWM's display event loop.  Both the
//! x11rb and xcb JWM backends render into the same X Composite overlay, so one
//! small X11 client covers both transports.

use super::RemoteResult;
use image::{RgbImage, RgbaImage, imageops::FilterType};
use std::borrow::Cow;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::ptr::{self, NonNull};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::image::{BitsPerPixel, Image as XImage, PixelLayout, ScanlinePad};
use x11rb::protocol::render::{CreatePictureAux, PictOp, Pictformat, Picture, Repeat, Transform};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, Format, ImageFormat, Pixmap, PropMode,
    Screen, VisualClass, Visualid, Visualtype, Window, WindowClass,
};
use x11rb::protocol::{composite, render, shm, xfixes};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::x11_utils::TryParseFd;

const COMPOSITE_CLIENT_VERSION: (u32, u32) = (0, 4);
const COMPOSITE_OVERLAY_VERSION: (u32, u32) = (0, 3);
const XFIXES_CLIENT_VERSION: (u32, u32) = (5, 0);
const RENDER_CLIENT_VERSION: (u32, u32) = (0, 11);
const RENDER_TRANSFORM_VERSION: (u32, u32) = (0, 10);
const SHM_FD_VERSION: (u16, u16) = (1, 2);
const REMOTE_CAPTURE_OWNER: &[u8] = b"_JWM_REMOTE_CAPTURE_OWNER";

/// Which X drawable supplies the remote desktop image.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CaptureSource {
    /// Prefer the compositor overlay and fall back to the root drawable when
    /// Composite is unavailable.
    #[default]
    Auto,
    /// Capture the Composite overlay.  This includes JWM's effects and system
    /// UI, but requires the Composite extension.
    Overlay,
    /// Capture the root drawable.  This is a compatibility fallback for X
    /// servers whose overlay cannot be read back.
    Root,
}

impl std::str::FromStr for CaptureSource {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "overlay" | "compositor" => Ok(Self::Overlay),
            "root" => Ok(Self::Root),
            _ => Err(format!(
                "unknown capture source {value:?}; expected auto, overlay, or root"
            )),
        }
    }
}

/// One top-to-bottom RGB frame plus the unscaled root coordinate space.
#[derive(Debug)]
pub struct CapturedFrame {
    pub image: RgbImage,
    pub source_width: u16,
    pub source_height: u16,
}

pub struct X11Capture {
    conn: RustConnection,
    screen_num: usize,
    root: Window,
    drawable: Window,
    overlay_acquired: bool,
    compositor_selection: Option<Atom>,
    compositor_owner: Window,
    composite_ready: bool,
    cursor_available: bool,
    inhibitor_atom: Atom,
    inhibitor_window: Window,
    requested_source: CaptureSource,
    max_width: u16,
    render_scaler: Option<RenderScaler>,
    shm_readback: ShmReadback,
}

impl X11Capture {
    pub fn connect(
        display: Option<&str>,
        requested_source: CaptureSource,
        max_width: u16,
    ) -> RemoteResult<Self> {
        let (conn, screen_num) = x11rb::connect(display)?;
        let screen = conn
            .setup()
            .roots
            .get(screen_num)
            .ok_or_else(|| invalid_data("X11 selected an unavailable screen"))?;
        let root = screen.root;
        // MIT-SHM requires QueryVersion to complete before any other request
        // from the extension. Segment allocation remains lazy until the first
        // frame establishes the exact native image size.
        let shm_readback = ShmReadback::connect(&conn, screen.root_depth);

        let compositor_selection = compositor_selection(&conn, screen_num)?;
        let compositor_owner = conn
            .get_selection_owner(compositor_selection)?
            .reply()?
            .owner;
        let composite_ready = if requested_source == CaptureSource::Root {
            false
        } else {
            match query_composite_overlay(&conn) {
                Ok(()) => true,
                Err(error) if requested_source == CaptureSource::Auto => {
                    eprintln!(
                        "remote: X Composite overlay unavailable ({error}); using root capture"
                    );
                    false
                }
                Err(error) => return Err(error),
            }
        };
        let (drawable, overlay_acquired) = match requested_source {
            CaptureSource::Root => (root, false),
            CaptureSource::Auto if !composite_ready || compositor_owner == x11rb::NONE => {
                if compositor_owner == x11rb::NONE {
                    eprintln!("remote: no X11 compositor owner found; using root capture");
                }
                (root, false)
            }
            CaptureSource::Overlay if compositor_owner == x11rb::NONE => {
                return Err(invalid_data("no X11 compositor owns this screen").into());
            }
            CaptureSource::Auto | CaptureSource::Overlay => (acquire_overlay(&conn, root)?, true),
        };
        let cursor_available = match query_xfixes(&conn) {
            Ok(()) => true,
            Err(error) => {
                eprintln!("remote: XFixes cursor capture unavailable: {error}");
                false
            }
        };
        let (inhibitor_atom, inhibitor_window) = match install_capture_inhibitor(&conn, root) {
            Ok(inhibitor) => inhibitor,
            Err(error) => {
                if overlay_acquired {
                    let _ = composite::release_overlay_window(&conn, root);
                    let _ = conn.flush();
                }
                return Err(error);
            }
        };
        let render_scaler = if max_width == 0 || requested_source == CaptureSource::Root {
            None
        } else {
            match RenderScaler::connect(&conn, screen_num, screen) {
                Ok(scaler) => Some(scaler),
                Err(error) => {
                    eprintln!(
                        "jwm-remote: XRender downscaling unavailable ({error}); using CPU fallback"
                    );
                    None
                }
            }
        };

        Ok(Self {
            conn,
            screen_num,
            root,
            drawable,
            overlay_acquired,
            compositor_selection: Some(compositor_selection),
            compositor_owner,
            composite_ready,
            cursor_available,
            inhibitor_atom,
            inhibitor_window,
            requested_source,
            max_width,
            render_scaler,
            shm_readback,
        })
    }

    /// Capture and optionally downscale a frame.
    ///
    /// `GetImage` is synchronous, but this process is deliberately separate
    /// from JWM.  The compositor event loop is never made to wait for JPEG or
    /// network I/O. The synchronous server readback can still contend with
    /// the compositor on very large roots, which is why the MVP exposes a
    /// conservative frame-rate default.
    pub fn frame(&mut self) -> RemoteResult<CapturedFrame> {
        self.sync_overlay_source()?;
        match self.capture_drawable(self.drawable) {
            Ok(frame) => Ok(frame),
            Err(error) if self.overlay_acquired && self.requested_source == CaptureSource::Auto => {
                eprintln!(
                    "remote: compositor overlay readback failed ({error}); switching to root capture"
                );
                self.release_overlay();
                self.capture_drawable(self.root)
            }
            Err(error) => Err(error),
        }
    }

    fn capture_drawable(&mut self, drawable: Window) -> RemoteResult<CapturedFrame> {
        let geometry = self.conn.get_geometry(self.root)?.reply()?;
        let source_width = geometry.width;
        let source_height = geometry.height;
        if source_width == 0 || source_height == 0 {
            return Err(invalid_data("X11 root has an empty geometry").into());
        }
        super::frame::validate_dimensions(source_width, source_height)?;

        let (output_width, output_height) =
            scaled_dimensions(source_width, source_height, self.max_width);
        // A Window used directly as an XRender source does not reliably
        // include its child windows. The Composite overlay has the final
        // composited pixels, while root fallback deliberately keeps the
        // proven GetImage + CPU resize path.
        if drawable != self.root && (output_width != source_width || output_height != source_height)
        {
            let render_result = self.render_scaler.as_mut().map(|scaler| {
                scaler.capture(
                    &self.conn,
                    &mut self.shm_readback,
                    drawable,
                    source_width,
                    source_height,
                    output_width,
                    output_height,
                )
            });
            if let Some(result) = render_result {
                match result {
                    Ok(mut image) => {
                        self.composite_cursor(&mut image, source_width, source_height)?;
                        return Ok(CapturedFrame {
                            image,
                            source_width,
                            source_height,
                        });
                    }
                    Err(error) => {
                        eprintln!(
                            "jwm-remote: XRender downscaling stopped ({error}); using CPU fallback"
                        );
                        self.release_render_scaler();
                    }
                }
            }
        }

        let root_depth = self.screen()?.root_depth;
        let mut image = self.shm_readback.capture_rgb(
            &self.conn,
            drawable,
            source_width,
            source_height,
            ReadbackLayout::ReplyVisual {
                screen_num: self.screen_num,
                expected_depth: root_depth,
            },
        )?;
        self.composite_cursor(&mut image, source_width, source_height)?;
        let image = if output_width == source_width && output_height == source_height {
            image
        } else {
            image::imageops::resize(
                &image,
                u32::from(output_width),
                u32::from(output_height),
                FilterType::Triangle,
            )
        };

        Ok(CapturedFrame {
            image,
            source_width,
            source_height,
        })
    }

    fn composite_cursor(
        &mut self,
        image: &mut RgbImage,
        source_width: u16,
        source_height: u16,
    ) -> RemoteResult<()> {
        if !self.cursor_available {
            return Ok(());
        }
        match xfixes::get_cursor_image(&self.conn)?.reply() {
            Ok(cursor) => {
                composite_scaled_cursor(image, &cursor, source_width, source_height);
                Ok(())
            }
            Err(error) => {
                eprintln!("remote: cursor capture stopped: {error}");
                self.cursor_available = false;
                Ok(())
            }
        }
    }

    fn screen(&self) -> RemoteResult<&Screen> {
        self.conn
            .setup()
            .roots
            .get(self.screen_num)
            .ok_or_else(|| invalid_data("X11 screen disappeared").into())
    }

    fn sync_overlay_source(&mut self) -> RemoteResult<()> {
        let Some(selection) = self.compositor_selection else {
            return Ok(());
        };
        let owner = self.conn.get_selection_owner(selection)?.reply()?.owner;
        if owner == self.compositor_owner {
            return Ok(());
        }

        self.release_overlay();
        self.compositor_owner = owner;
        self.publish_capture_inhibitor()?;
        if self.requested_source == CaptureSource::Root {
            return Ok(());
        }
        if owner == x11rb::NONE {
            if self.requested_source == CaptureSource::Overlay {
                return Err(invalid_data("X11 compositor stopped during remote capture").into());
            }
            self.drawable = self.root;
            return Ok(());
        }
        if self.composite_ready {
            match acquire_overlay(&self.conn, self.root) {
                Ok(overlay) => {
                    self.drawable = overlay;
                    self.overlay_acquired = true;
                }
                Err(error) if self.requested_source == CaptureSource::Auto => {
                    eprintln!(
                        "remote: new compositor overlay unavailable ({error}); using root capture"
                    );
                    self.drawable = self.root;
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn release_overlay(&mut self) {
        if let Some(scaler) = self.render_scaler.as_mut() {
            scaler.release_source(&self.conn);
        }
        if self.overlay_acquired {
            let _ = composite::release_overlay_window(&self.conn, self.root);
            let _ = self.conn.flush();
            self.overlay_acquired = false;
        }
        self.drawable = self.root;
    }

    fn release_render_scaler(&mut self) {
        if let Some(mut scaler) = self.render_scaler.take() {
            scaler.release(&self.conn);
            let _ = self.conn.flush();
        }
    }

    fn publish_capture_inhibitor(&self) -> RemoteResult<()> {
        self.conn
            .change_property32(
                PropMode::REPLACE,
                self.root,
                self.inhibitor_atom,
                AtomEnum::WINDOW,
                &[self.inhibitor_window],
            )?
            .check()?;
        self.conn.flush()?;
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum ReadbackLayout {
    /// Windows report their visual in GetImage/ShmGetImage. Keep validating
    /// it instead of assuming that every capture drawable uses the root masks.
    ReplyVisual {
        screen_num: usize,
        expected_depth: u8,
    },
    /// Pixmap GetImage replies have visual NONE. XRender's target pixmap was
    /// explicitly created with the root format, so its layout is already known.
    Known {
        expected_depth: u8,
        layout: PixelLayout,
    },
}

impl ReadbackLayout {
    fn expected_depth(self) -> u8 {
        match self {
            Self::ReplyVisual { expected_depth, .. } | Self::Known { expected_depth, .. } => {
                expected_depth
            }
        }
    }
}

enum ShmReadback {
    Disabled,
    Enabled(EnabledShmReadback),
}

impl ShmReadback {
    fn connect(conn: &RustConnection, depth: u8) -> Self {
        match Self::try_connect(conn, depth) {
            Ok(Some(readback)) => Self::Enabled(readback),
            Ok(None) => Self::Disabled,
            Err(error) => {
                eprintln!(
                    "jwm-remote: MIT-SHM FD readback unavailable ({error}); using core GetImage"
                );
                Self::Disabled
            }
        }
    }

    fn try_connect(conn: &RustConnection, depth: u8) -> RemoteResult<Option<EnabledShmReadback>> {
        if conn
            .extension_information(shm::X11_EXTENSION_NAME)?
            .is_none()
        {
            eprintln!("jwm-remote: MIT-SHM unavailable; using core GetImage");
            return Ok(None);
        }

        // The protocol requires this reply before any other MIT-SHM request.
        let version = shm::query_version(conn)?.reply()?;
        let negotiated = (version.major_version, version.minor_version);
        if !supports_shm_fd_version(version.major_version, version.minor_version) {
            eprintln!(
                "jwm-remote: MIT-SHM {}.{} lacks 1.2 FD segments; using core GetImage",
                version.major_version, version.minor_version
            );
            return Ok(None);
        }
        let format = conn
            .setup()
            .pixmap_formats
            .iter()
            .find(|format| format.depth == depth)
            .copied()
            .ok_or_else(|| invalid_data(format!("X11 has no pixmap format for depth {depth}")))?;
        // Validate the native format before announcing SHM as available.
        shm_buffer_size(1, 1, format)?;
        Ok(Some(EnabledShmReadback {
            format,
            version: negotiated,
            segment: None,
            reported_active: false,
        }))
    }

    fn capture_rgb(
        &mut self,
        conn: &RustConnection,
        drawable: u32,
        width: u16,
        height: u16,
        layout: ReadbackLayout,
    ) -> RemoteResult<RgbImage> {
        let shm_result = match self {
            Self::Disabled => None,
            Self::Enabled(readback) => {
                Some(readback.capture_rgb(conn, drawable, width, height, layout))
            }
        };
        match resolve_readback(shm_result, || {
            core_capture_rgb(conn, drawable, width, height, layout)
        }) {
            ReadbackOutcome::Image(image) => Ok(image),
            ReadbackOutcome::CoreFallback { image, shm_error } => {
                self.disable(conn);
                eprintln!(
                    "jwm-remote: MIT-SHM FD readback stopped ({shm_error}); using core GetImage"
                );
                Ok(image)
            }
            ReadbackOutcome::Error(error) => Err(error),
        }
    }

    fn disable(&mut self, conn: &RustConnection) {
        if let Self::Enabled(mut readback) = std::mem::replace(self, Self::Disabled) {
            readback.release(conn);
        }
    }

    fn release(&mut self, conn: &RustConnection) {
        self.disable(conn);
    }
}

struct EnabledShmReadback {
    format: Format,
    version: (u16, u16),
    segment: Option<ShmSegment>,
    reported_active: bool,
}

impl EnabledShmReadback {
    fn capture_rgb(
        &mut self,
        conn: &RustConnection,
        drawable: u32,
        width: u16,
        height: u16,
        layout: ReadbackLayout,
    ) -> RemoteResult<RgbImage> {
        let expected_size = shm_buffer_size(width, height, self.format)?;
        self.ensure_capacity(conn, expected_size)?;
        let (image, capacity) = {
            let segment = self
                .segment
                .as_ref()
                .ok_or_else(|| invalid_data("MIT-SHM segment was not created"))?;
            let reply = shm::get_image(
                conn,
                drawable,
                0,
                0,
                width,
                height,
                u32::MAX,
                u8::from(ImageFormat::Z_PIXMAP),
                segment.id,
                0,
            )?
            .reply()?;
            let image_size = validate_shm_reply(
                self.format.depth,
                expected_size,
                segment.mapping.len(),
                reply.depth,
                reply.size,
            )?;
            let bytes = segment.mapping.bytes(image_size)?;
            let ximage = XImage::new(
                width,
                height,
                self.format.scanline_pad.try_into()?,
                reply.depth,
                self.format.bits_per_pixel.try_into()?,
                conn.setup().image_byte_order.try_into()?,
                Cow::Borrowed(bytes),
            )?;
            (
                decode_readback_image(conn, &ximage, width, height, reply.visual, layout)?,
                segment.mapping.len(),
            )
        };
        if !self.reported_active {
            eprintln!(
                "jwm-remote: MIT-SHM {}.{} FD readback active ({capacity} bytes)",
                self.version.0, self.version.1
            );
            self.reported_active = true;
        }
        Ok(image)
    }

    fn ensure_capacity(&mut self, conn: &RustConnection, required: usize) -> RemoteResult<()> {
        let current_capacity = self.segment.as_ref().map(|segment| segment.mapping.len());
        if !shm_needs_growth(current_capacity, required) {
            return Ok(());
        }

        // Build the replacement completely before releasing the old segment.
        // A failed RandR growth therefore leaves the previous resource valid.
        let replacement = ShmSegment::create(conn, required)?;
        let old = self.segment.replace(replacement);
        if let Some(old) = old {
            old.release(conn);
        }
        Ok(())
    }

    fn release(&mut self, conn: &RustConnection) {
        if let Some(segment) = self.segment.take() {
            segment.release(conn);
        }
    }
}

struct ShmSegment {
    id: shm::Seg,
    mapping: MappedRegion,
    // Keeping the owned descriptor makes close ordering explicit. It is
    // closed after the mapping is unmapped when this struct is dropped.
    _file: File,
}

impl ShmSegment {
    fn create(conn: &RustConnection, capacity: usize) -> RemoteResult<Self> {
        let size = u32::try_from(capacity)
            .map_err(|_| invalid_data("MIT-SHM image buffer exceeds the protocol size limit"))?;
        if size == 0 {
            return Err(invalid_data("MIT-SHM image buffer is empty").into());
        }
        let id = conn.generate_id()?;
        // `false` is required: the server writes captured pixels into the segment.
        let cookie = shm::create_segment(conn, id, size, false)?;
        // Split waiting from parsing. An X11/raw transport error does not prove
        // that the server created the resource, so do not detach an uncreated
        // XID. Once a success reply exists, any malformed/missing FD path must
        // detach because the server-side segment is already live.
        let (buffer, mut fds) = cookie.raw_reply()?;
        let reply = match shm::CreateSegmentReply::try_parse_fd(buffer.as_ref(), &mut fds) {
            Ok((reply, _)) => reply,
            Err(error) => {
                detach_shm_segment(conn, id);
                return Err(error.into());
            }
        };
        let result = (|| -> RemoteResult<Self> {
            if reply.nfd != 1 {
                return Err(invalid_data(format!(
                    "MIT-SHM CreateSegment returned {} file descriptors",
                    reply.nfd
                ))
                .into());
            }
            let file = File::from(reply.shm_fd);
            let file_size = file.metadata()?.len();
            if file_size < u64::from(size) {
                return Err(invalid_data(format!(
                    "MIT-SHM segment is shorter than requested: {file_size} < {size}"
                ))
                .into());
            }
            let mapping = MappedRegion::new(&file, capacity)?;
            Ok(Self {
                id,
                mapping,
                _file: file,
            })
        })();
        match result {
            Ok(segment) => Ok(segment),
            Err(error) => {
                // A parsed success reply makes this XID a live server resource.
                detach_shm_segment(conn, id);
                Err(error)
            }
        }
    }

    fn release(self, conn: &RustConnection) {
        // ShmGetImage is synchronous, so no server write is outstanding here.
        // Check Detach before dropping the local mapping and descriptor.
        detach_shm_segment(conn, self.id);
        drop(self);
    }
}

fn detach_shm_segment(conn: &RustConnection, segment: shm::Seg) {
    if let Ok(cookie) = shm::detach(conn, segment) {
        let _ = cookie.check();
    }
}

struct MappedRegion {
    address: NonNull<u8>,
    length: usize,
}

impl MappedRegion {
    fn new(file: &File, length: usize) -> io::Result<Self> {
        if length == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot mmap an empty MIT-SHM segment",
            ));
        }
        // SAFETY: `file` is a live CreateSegment descriptor, `length` was
        // checked against fstat, and the mapping is owned until munmap in Drop.
        // The client only reads; CreateSegment(false) gives the server write access.
        let mapped = unsafe {
            libc::mmap(
                ptr::null_mut(),
                length,
                libc::PROT_READ,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if mapped == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let Some(address) = NonNull::new(mapped.cast::<u8>()) else {
            // SAFETY: mmap succeeded and returned this exact address/length.
            unsafe {
                libc::munmap(mapped, length);
            }
            return Err(io::Error::other("MIT-SHM mmap returned a null address"));
        };
        Ok(Self { address, length })
    }

    fn len(&self) -> usize {
        self.length
    }

    fn bytes(&self, length: usize) -> io::Result<&[u8]> {
        if length > self.length {
            return Err(invalid_data(format!(
                "MIT-SHM reply exceeds its mapping: {length} > {}",
                self.length
            )));
        }
        // SAFETY: the mapping is valid for `self.length`, the preceding
        // ShmGetImage reply guarantees the server finished writing, and the
        // returned borrow cannot outlive `self` or overlap the next capture.
        Ok(unsafe { std::slice::from_raw_parts(self.address.as_ptr(), length) })
    }
}

impl Drop for MappedRegion {
    fn drop(&mut self) {
        // SAFETY: this exact address/length pair came from the successful mmap
        // in `new` and is unmapped exactly once here.
        unsafe {
            libc::munmap(self.address.as_ptr().cast(), self.length);
        }
    }
}

enum ReadbackOutcome<T, E> {
    Image(T),
    CoreFallback { image: T, shm_error: E },
    Error(E),
}

fn resolve_readback<T, E>(
    shm_result: Option<Result<T, E>>,
    core: impl FnOnce() -> Result<T, E>,
) -> ReadbackOutcome<T, E> {
    match shm_result {
        None => match core() {
            Ok(image) => ReadbackOutcome::Image(image),
            Err(error) => ReadbackOutcome::Error(error),
        },
        Some(Ok(image)) => ReadbackOutcome::Image(image),
        Some(Err(shm_error)) => match core() {
            Ok(image) => ReadbackOutcome::CoreFallback { image, shm_error },
            // When both paths reject the same drawable, preserve the core
            // error and let the existing overlay/Render lifecycle handle it.
            // Do not globally disable SHM for a transient BadDrawable.
            Err(error) => ReadbackOutcome::Error(error),
        },
    }
}

fn supports_shm_fd_version(major: u16, minor: u16) -> bool {
    (major, minor) >= SHM_FD_VERSION
}

fn shm_needs_growth(current_capacity: Option<usize>, required: usize) -> bool {
    current_capacity.is_none_or(|capacity| capacity < required)
}

fn core_capture_rgb(
    conn: &RustConnection,
    drawable: u32,
    width: u16,
    height: u16,
    layout: ReadbackLayout,
) -> RemoteResult<RgbImage> {
    let reply = conn
        .get_image(
            ImageFormat::Z_PIXMAP,
            drawable,
            0,
            0,
            width,
            height,
            u32::MAX,
        )?
        .reply()?;
    let visual = reply.visual;
    let image = XImage::get_from_reply(conn.setup(), width, height, reply)?;
    decode_readback_image(conn, &image, width, height, visual, layout)
}

fn decode_readback_image(
    conn: &RustConnection,
    image: &XImage<'_>,
    width: u16,
    height: u16,
    visual_id: Visualid,
    layout: ReadbackLayout,
) -> RemoteResult<RgbImage> {
    if image.depth() != layout.expected_depth() {
        return Err(invalid_data(format!(
            "X11 capture depth changed from {} to {}",
            layout.expected_depth(),
            image.depth()
        ))
        .into());
    }
    let pixel_layout = match layout {
        ReadbackLayout::ReplyVisual { screen_num, .. } => {
            let screen = conn
                .setup()
                .roots
                .get(screen_num)
                .ok_or_else(|| invalid_data("X11 selected an unavailable screen"))?;
            let visual = find_visual(screen, visual_id)
                .ok_or_else(|| invalid_data("X11 capture visual is not described by the screen"))?;
            if visual.class != VisualClass::TRUE_COLOR {
                return Err(invalid_data("remote capture requires an X11 TrueColor visual").into());
            }
            PixelLayout::from_visual_type(visual)?
        }
        ReadbackLayout::Known { layout, .. } => layout,
    };
    decode_ximage(image, width, height, pixel_layout)
}

fn shm_buffer_size(width: u16, height: u16, format: Format) -> RemoteResult<usize> {
    if width == 0 || height == 0 {
        return Err(invalid_data("MIT-SHM image dimensions must be nonzero").into());
    }
    let bits_per_pixel = usize::from(
        BitsPerPixel::try_from(format.bits_per_pixel)
            .map_err(|_| invalid_data("X11 reported invalid native bits-per-pixel"))?,
    );
    let scanline_pad = usize::from(
        ScanlinePad::try_from(format.scanline_pad)
            .map_err(|_| invalid_data("X11 reported invalid native scanline padding"))?,
    );
    let row_bits = usize::from(width)
        .checked_mul(bits_per_pixel)
        .ok_or_else(|| invalid_data("MIT-SHM scanline size overflow"))?;
    let padded_units = row_bits
        .checked_add(scanline_pad - 1)
        .ok_or_else(|| invalid_data("MIT-SHM scanline padding overflow"))?
        / scanline_pad;
    let padded_bits = padded_units
        .checked_mul(scanline_pad)
        .ok_or_else(|| invalid_data("MIT-SHM padded scanline size overflow"))?;
    let stride = padded_bits / 8;
    let size = stride
        .checked_mul(usize::from(height))
        .ok_or_else(|| invalid_data("MIT-SHM image size overflow"))?;
    u32::try_from(size)
        .map_err(|_| invalid_data("MIT-SHM image exceeds the protocol size limit"))?;
    Ok(size)
}

fn validate_shm_reply(
    expected_depth: u8,
    expected_size: usize,
    mapped_size: usize,
    reply_depth: u8,
    reply_size: u32,
) -> RemoteResult<usize> {
    if reply_depth != expected_depth {
        return Err(invalid_data(format!(
            "MIT-SHM capture depth changed from {expected_depth} to {reply_depth}"
        ))
        .into());
    }
    let reply_size = usize::try_from(reply_size)
        .map_err(|_| invalid_data("MIT-SHM reply size exceeds this platform"))?;
    if reply_size != expected_size {
        return Err(invalid_data(format!(
            "MIT-SHM returned {reply_size} bytes; expected {expected_size}"
        ))
        .into());
    }
    if reply_size > mapped_size {
        return Err(invalid_data(format!(
            "MIT-SHM reply exceeds its mapping: {reply_size} > {mapped_size}"
        ))
        .into());
    }
    Ok(reply_size)
}

struct RenderSource {
    drawable: Window,
    picture: Picture,
    transform: Option<(u16, u16, u16, u16)>,
}

struct RenderTarget {
    pixmap: Pixmap,
    picture: Picture,
    width: u16,
    height: u16,
}

struct RenderScaler {
    visual_formats: Vec<(Visualid, Pictformat)>,
    root: Window,
    root_depth: u8,
    root_format: Pictformat,
    root_layout: PixelLayout,
    source: Option<RenderSource>,
    target: Option<RenderTarget>,
    reported_dimensions: Option<(u16, u16, u16, u16)>,
}

impl RenderScaler {
    fn connect(conn: &RustConnection, screen_num: usize, screen: &Screen) -> RemoteResult<Self> {
        let version =
            render::query_version(conn, RENDER_CLIENT_VERSION.0, RENDER_CLIENT_VERSION.1)?
                .reply()?;
        if (version.major_version, version.minor_version) < RENDER_TRANSFORM_VERSION {
            return Err(invalid_data(format!(
                "XRender {}.{} is too old; server-side scaling requires {}.{}",
                version.major_version,
                version.minor_version,
                RENDER_TRANSFORM_VERSION.0,
                RENDER_TRANSFORM_VERSION.1
            ))
            .into());
        }

        let formats = render::query_pict_formats(conn)?.reply()?;
        let render_screen = formats
            .screens
            .get(screen_num)
            .ok_or_else(|| invalid_data("XRender did not describe the selected X11 screen"))?;
        let visual_formats: Vec<_> = render_screen
            .depths
            .iter()
            .flat_map(|depth| depth.visuals.iter())
            .map(|visual| (visual.visual, visual.format))
            .collect();
        let root_format = visual_formats
            .iter()
            .find_map(|(visual, format)| (*visual == screen.root_visual).then_some(*format))
            .ok_or_else(|| invalid_data("XRender has no format for the root visual"))?;
        let root_visual = find_visual(screen, screen.root_visual)
            .ok_or_else(|| invalid_data("X11 root visual is not described by the screen"))?;
        if root_visual.class != VisualClass::TRUE_COLOR {
            return Err(
                invalid_data("remote capture requires an X11 TrueColor root visual").into(),
            );
        }

        Ok(Self {
            visual_formats,
            root: screen.root,
            root_depth: screen.root_depth,
            root_format,
            root_layout: PixelLayout::from_visual_type(root_visual)?,
            source: None,
            target: None,
            reported_dimensions: None,
        })
    }

    fn capture(
        &mut self,
        conn: &RustConnection,
        readback: &mut ShmReadback,
        drawable: Window,
        source_width: u16,
        source_height: u16,
        output_width: u16,
        output_height: u16,
    ) -> RemoteResult<RgbImage> {
        self.ensure_target(conn, output_width, output_height)?;
        self.ensure_source(conn, drawable)?;
        let dimensions = (source_width, source_height, output_width, output_height);
        let source = self
            .source
            .as_mut()
            .ok_or_else(|| invalid_data("XRender source picture was not created"))?;
        if source.transform != Some(dimensions) {
            render::set_picture_transform(
                conn,
                source.picture,
                scale_transform(source_width, source_height, output_width, output_height)?,
            )?
            .check()?;
            source.transform = Some(dimensions);
        }
        let target = self
            .target
            .as_ref()
            .ok_or_else(|| invalid_data("XRender target picture was not created"))?;
        let composite = render::composite(
            conn,
            PictOp::SRC,
            source.picture,
            x11rb::NONE,
            target.picture,
            0,
            0,
            0,
            0,
            0,
            0,
            output_width,
            output_height,
        )?;
        let image = readback.capture_rgb(
            conn,
            target.pixmap,
            output_width,
            output_height,
            ReadbackLayout::Known {
                expected_depth: self.root_depth,
                layout: self.root_layout,
            },
        )?;
        // Waiting for ShmGetImage or core GetImage also advances the connection beyond the
        // preceding Composite request. Checking its cookie now reports a
        // precise Render error without adding another round trip normally.
        composite.check()?;
        if self.reported_dimensions != Some(dimensions) {
            eprintln!(
                "jwm-remote: XRender downscale {}x{} -> {}x{}",
                source_width, source_height, output_width, output_height
            );
            self.reported_dimensions = Some(dimensions);
        }
        Ok(image)
    }

    fn ensure_source(&mut self, conn: &RustConnection, drawable: Window) -> RemoteResult<()> {
        if self
            .source
            .as_ref()
            .is_some_and(|source| source.drawable == drawable)
        {
            return Ok(());
        }
        let visual = conn.get_window_attributes(drawable)?.reply()?.visual;
        let format = self
            .visual_formats
            .iter()
            .find_map(|(candidate, format)| (*candidate == visual).then_some(*format))
            .ok_or_else(|| invalid_data("XRender has no format for the capture visual"))?;
        let picture = conn.generate_id()?;
        let create = render::create_picture(
            conn,
            picture,
            drawable,
            format,
            &CreatePictureAux::new().repeat(Repeat::PAD),
        )?
        .check();
        if let Err(error) = create {
            return Err(error.into());
        }
        if let Err(error) = render::set_picture_filter(conn, picture, b"bilinear", &[])?.check() {
            let _ = render::free_picture(conn, picture);
            return Err(error.into());
        }

        let old = self.source.replace(RenderSource {
            drawable,
            picture,
            transform: None,
        });
        if let Some(old) = old {
            let _ = render::free_picture(conn, old.picture);
        }
        Ok(())
    }

    fn ensure_target(
        &mut self,
        conn: &RustConnection,
        width: u16,
        height: u16,
    ) -> RemoteResult<()> {
        if self
            .target
            .as_ref()
            .is_some_and(|target| target.width == width && target.height == height)
        {
            return Ok(());
        }

        let pixmap = conn.generate_id()?;
        conn.create_pixmap(self.root_depth, pixmap, self.root, width, height)?
            .check()?;
        let picture = conn.generate_id()?;
        let create = render::create_picture(
            conn,
            picture,
            pixmap,
            self.root_format,
            &CreatePictureAux::new(),
        )?
        .check();
        if let Err(error) = create {
            let _ = conn.free_pixmap(pixmap);
            return Err(error.into());
        }

        let old = self.target.replace(RenderTarget {
            pixmap,
            picture,
            width,
            height,
        });
        if let Some(old) = old {
            let _ = render::free_picture(conn, old.picture);
            let _ = conn.free_pixmap(old.pixmap);
        }
        Ok(())
    }

    fn release_source(&mut self, conn: &RustConnection) {
        if let Some(source) = self.source.take() {
            let _ = render::free_picture(conn, source.picture);
        }
    }

    fn release(&mut self, conn: &RustConnection) {
        self.release_source(conn);
        if let Some(target) = self.target.take() {
            let _ = render::free_picture(conn, target.picture);
            let _ = conn.free_pixmap(target.pixmap);
        }
    }
}

fn scale_transform(
    source_width: u16,
    source_height: u16,
    output_width: u16,
    output_height: u16,
) -> RemoteResult<Transform> {
    fn fixed_ratio(source: u16, output: u16) -> RemoteResult<i32> {
        if output == 0 {
            return Err(invalid_data("XRender output dimension is zero").into());
        }
        let denominator = i64::from(output);
        let fixed = ((i64::from(source) << 16) + denominator / 2) / denominator;
        i32::try_from(fixed)
            .map_err(|_| invalid_data("XRender scale transform exceeds 16.16 range").into())
    }

    Ok(Transform {
        matrix11: fixed_ratio(source_width, output_width)?,
        matrix12: 0,
        matrix13: 0,
        matrix21: 0,
        matrix22: fixed_ratio(source_height, output_height)?,
        matrix23: 0,
        matrix31: 0,
        matrix32: 0,
        matrix33: 1 << 16,
    })
}

fn decode_ximage(
    ximage: &XImage<'_>,
    width: u16,
    height: u16,
    layout: PixelLayout,
) -> RemoteResult<RgbImage> {
    let pixel_count = usize::from(width)
        .checked_mul(usize::from(height))
        .ok_or_else(|| invalid_data("X11 capture dimensions overflow"))?;
    let rgb_bytes = pixel_count
        .checked_mul(3)
        .ok_or_else(|| invalid_data("X11 capture buffer size overflow"))?;
    let mut rgb = Vec::new();
    rgb.try_reserve_exact(rgb_bytes)
        .map_err(|_| invalid_data("could not allocate the X11 capture buffer"))?;
    for y in 0..height {
        for x in 0..width {
            let (red, green, blue) = layout.decode(ximage.get_pixel(x, y));
            rgb.extend_from_slice(&[(red >> 8) as u8, (green >> 8) as u8, (blue >> 8) as u8]);
        }
    }
    RgbImage::from_raw(u32::from(width), u32::from(height), rgb)
        .ok_or_else(|| invalid_data("X11 capture returned an invalid pixel buffer").into())
}

fn acquire_overlay(conn: &RustConnection, root: Window) -> RemoteResult<Window> {
    Ok(composite::get_overlay_window(conn, root)?
        .reply()?
        .overlay_win)
}

fn compositor_selection(conn: &RustConnection, screen_num: usize) -> RemoteResult<Atom> {
    let selection = format!("_NET_WM_CM_S{screen_num}");
    Ok(conn.intern_atom(false, selection.as_bytes())?.reply()?.atom)
}

fn query_composite_overlay(conn: &RustConnection) -> RemoteResult<()> {
    let version =
        composite::query_version(conn, COMPOSITE_CLIENT_VERSION.0, COMPOSITE_CLIENT_VERSION.1)?
            .reply()?;
    if (version.major_version, version.minor_version) < COMPOSITE_OVERLAY_VERSION {
        return Err(invalid_data(format!(
            "X Composite {}.{} is too old; overlay capture requires {}.{}",
            version.major_version,
            version.minor_version,
            COMPOSITE_OVERLAY_VERSION.0,
            COMPOSITE_OVERLAY_VERSION.1
        ))
        .into());
    }
    Ok(())
}

fn query_xfixes(conn: &RustConnection) -> RemoteResult<()> {
    xfixes::query_version(conn, XFIXES_CLIENT_VERSION.0, XFIXES_CLIENT_VERSION.1)?.reply()?;
    Ok(())
}

fn install_capture_inhibitor(conn: &RustConnection, root: Window) -> RemoteResult<(Atom, Window)> {
    let atom = conn.intern_atom(false, REMOTE_CAPTURE_OWNER)?.reply()?.atom;
    let owner = conn.generate_id()?;
    conn.grab_server()?.check()?;
    let mut created = false;
    let result = (|| -> RemoteResult<()> {
        let existing = conn
            .get_property(false, root, atom, AtomEnum::WINDOW, 0, 1)?
            .reply()?
            .value32()
            .and_then(|mut values| values.next())
            .filter(|owner| *owner != x11rb::NONE);
        if let Some(existing) = existing {
            let live = match conn.get_window_attributes(existing) {
                Ok(cookie) => cookie.reply().is_ok(),
                Err(error) => return Err(error.into()),
            };
            if live {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "another jwm-remote host already captures this X11 screen",
                )
                .into());
            }
        }
        conn.create_window(
            0,
            owner,
            root,
            -1,
            -1,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new(),
        )?
        .check()?;
        created = true;
        conn.change_property32(PropMode::REPLACE, root, atom, AtomEnum::WINDOW, &[owner])?
            .check()?;
        conn.flush()?;
        Ok(())
    })();
    let ungrab_result: RemoteResult<()> = (|| {
        conn.ungrab_server()?.check()?;
        conn.flush()?;
        Ok(())
    })();
    if let Err(error) = result {
        if created {
            let _ = conn.destroy_window(owner);
        }
        let _ = conn.flush();
        return Err(error);
    }
    if let Err(error) = ungrab_result {
        let _ = conn.destroy_window(owner);
        let _ = conn.flush();
        return Err(error);
    }
    Ok((atom, owner))
}

fn composite_cursor(image: &mut RgbImage, cursor: &xfixes::GetCursorImageReply) {
    if cursor.width == 0 || cursor.height == 0 {
        return;
    }
    let left = i32::from(cursor.x) - i32::from(cursor.xhot);
    let top = i32::from(cursor.y) - i32::from(cursor.yhot);
    for (index, argb) in cursor.cursor_image.iter().copied().enumerate() {
        let cursor_x = (index % usize::from(cursor.width)) as i32;
        let cursor_y = (index / usize::from(cursor.width)) as i32;
        let x = left + cursor_x;
        let y = top + cursor_y;
        if x < 0 || y < 0 || x >= image.width() as i32 || y >= image.height() as i32 {
            continue;
        }
        let alpha = (argb >> 24) & 0xff;
        if alpha == 0 {
            continue;
        }
        let inverse = 255 - alpha;
        let source = [(argb >> 16) & 0xff, (argb >> 8) & 0xff, argb & 0xff];
        let pixel = image.get_pixel_mut(x as u32, y as u32);
        for channel in 0..3 {
            pixel[channel] = (source[channel] + (u32::from(pixel[channel]) * inverse + 127) / 255)
                .min(255) as u8;
        }
    }
}

fn composite_scaled_cursor(
    image: &mut RgbImage,
    cursor: &xfixes::GetCursorImageReply,
    source_width: u16,
    source_height: u16,
) {
    if image.width() == u32::from(source_width) && image.height() == u32::from(source_height) {
        composite_cursor(image, cursor);
        return;
    }
    if cursor.width == 0 || cursor.height == 0 || source_width == 0 || source_height == 0 {
        return;
    }
    let pixel_count = usize::from(cursor.width) * usize::from(cursor.height);
    if cursor.cursor_image.len() < pixel_count {
        return;
    }

    // XFixes supplies premultiplied ARGB. Keeping the cursor premultiplied
    // while filtering avoids dark/bright fringes around translucent edges.
    let cursor_image =
        RgbaImage::from_fn(u32::from(cursor.width), u32::from(cursor.height), |x, y| {
            let index = y as usize * usize::from(cursor.width) + x as usize;
            let argb = cursor.cursor_image[index];
            image::Rgba([
                ((argb >> 16) & 0xff) as u8,
                ((argb >> 8) & 0xff) as u8,
                (argb & 0xff) as u8,
                ((argb >> 24) & 0xff) as u8,
            ])
        });
    let scaled_width = scale_length(cursor.width, image.width(), source_width);
    let scaled_height = scale_length(cursor.height, image.height(), source_height);
    let cursor_image = image::imageops::resize(
        &cursor_image,
        scaled_width,
        scaled_height,
        FilterType::Triangle,
    );
    let left = scale_coordinate(
        i32::from(cursor.x) - i32::from(cursor.xhot),
        image.width(),
        source_width,
    );
    let top = scale_coordinate(
        i32::from(cursor.y) - i32::from(cursor.yhot),
        image.height(),
        source_height,
    );
    for (cursor_x, cursor_y, source) in cursor_image.enumerate_pixels() {
        let x = left + cursor_x as i32;
        let y = top + cursor_y as i32;
        if x < 0 || y < 0 || x >= image.width() as i32 || y >= image.height() as i32 {
            continue;
        }
        let alpha = u32::from(source[3]);
        if alpha == 0 {
            continue;
        }
        let inverse = 255 - alpha;
        let pixel = image.get_pixel_mut(x as u32, y as u32);
        for channel in 0..3 {
            pixel[channel] = (u32::from(source[channel])
                + (u32::from(pixel[channel]) * inverse + 127) / 255)
                .min(255) as u8;
        }
    }
}

fn scale_length(value: u16, output: u32, source: u16) -> u32 {
    ((u64::from(value) * u64::from(output) + u64::from(source) / 2) / u64::from(source)).max(1)
        as u32
}

fn scale_coordinate(value: i32, output: u32, source: u16) -> i32 {
    let numerator = i64::from(value) * i64::from(output);
    let half = i64::from(source) / 2;
    let rounded = if numerator >= 0 {
        (numerator + half) / i64::from(source)
    } else {
        (numerator - half) / i64::from(source)
    };
    rounded.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

impl Drop for X11Capture {
    fn drop(&mut self) {
        self.release_overlay();
        self.release_render_scaler();
        self.shm_readback.release(&self.conn);
        if let Ok(cookie) = self.conn.get_property(
            false,
            self.root,
            self.inhibitor_atom,
            AtomEnum::WINDOW,
            0,
            1,
        ) && let Ok(reply) = cookie.reply()
            && reply.value32().and_then(|mut values| values.next()) == Some(self.inhibitor_window)
        {
            if let Ok(cookie) = self.conn.delete_property(self.root, self.inhibitor_atom) {
                let _ = cookie.check();
            }
        }
        if let Ok(cookie) = self.conn.destroy_window(self.inhibitor_window) {
            let _ = cookie.check();
        }
        let _ = self.conn.sync();
    }
}

fn find_visual(screen: &Screen, visual_id: u32) -> Option<Visualtype> {
    screen
        .allowed_depths
        .iter()
        .flat_map(|depth| depth.visuals.iter())
        .find(|visual| visual.visual_id == visual_id)
        .copied()
}

#[must_use]
pub fn scaled_dimensions(width: u16, height: u16, max_width: u16) -> (u16, u16) {
    if max_width == 0 || width <= max_width || height == 0 {
        return (width, height);
    }
    let scaled_height = (u32::from(height) * u32::from(max_width) / u32::from(width))
        .clamp(1, u32::from(u16::MAX)) as u16;
    (max_width, scaled_height)
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn downscale_preserves_aspect_ratio_without_upscaling() {
        assert_eq!(scaled_dimensions(1920, 1080, 1280), (1280, 720));
        assert_eq!(scaled_dimensions(2560, 1440, 1920), (1920, 1080));
        assert_eq!(scaled_dimensions(1024, 768, 1280), (1024, 768));
        assert_eq!(scaled_dimensions(1024, 768, 0), (1024, 768));
    }

    #[test]
    fn tiny_aspect_ratios_keep_a_nonzero_height() {
        assert_eq!(scaled_dimensions(u16::MAX, 1, 1), (1, 1));
    }

    #[test]
    fn xrender_transform_maps_destination_back_to_source() {
        let transform = scale_transform(1920, 1080, 1280, 720).unwrap();
        assert_eq!(transform.matrix11, 98_304);
        assert_eq!(transform.matrix22, 98_304);
        assert_eq!(transform.matrix33, 65_536);
        assert_eq!(transform.matrix12, 0);
        assert_eq!(transform.matrix21, 0);
    }

    #[test]
    fn capture_source_parser_is_strict_but_accepts_compositor_alias() {
        assert_eq!("auto".parse(), Ok(CaptureSource::Auto));
        assert_eq!("compositor".parse(), Ok(CaptureSource::Overlay));
        assert!("window".parse::<CaptureSource>().is_err());
    }

    #[test]
    fn premultiplied_cursor_pixels_blend_and_clip() {
        let mut image = RgbImage::from_pixel(2, 1, image::Rgb([255, 255, 255]));
        let cursor = xfixes::GetCursorImageReply {
            x: 0,
            y: 0,
            width: 2,
            height: 1,
            // Half-transparent premultiplied red, then fully opaque blue.
            cursor_image: vec![0x8080_0000, 0xff00_00ff],
            ..Default::default()
        };
        composite_cursor(&mut image, &cursor);
        assert_eq!(image.get_pixel(0, 0).0, [255, 127, 127]);
        assert_eq!(image.get_pixel(1, 0).0, [0, 0, 255]);

        let clipped = xfixes::GetCursorImageReply {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            xhot: 2,
            cursor_image: vec![0xffff_ffff],
            ..Default::default()
        };
        composite_cursor(&mut image, &clipped);
        assert_eq!(image.get_pixel(0, 0).0, [255, 127, 127]);
    }

    #[test]
    fn scaled_cursor_uses_the_encoded_frame_coordinate_space() {
        let mut image = RgbImage::from_pixel(2, 1, image::Rgb([255, 255, 255]));
        let cursor = xfixes::GetCursorImageReply {
            x: 2,
            y: 0,
            width: 2,
            height: 2,
            cursor_image: vec![0xffff_0000; 4],
            ..Default::default()
        };
        composite_scaled_cursor(&mut image, &cursor, 4, 2);
        assert_eq!(image.get_pixel(0, 0).0, [255, 255, 255]);
        assert_eq!(image.get_pixel(1, 0).0, [255, 0, 0]);
    }

    #[test]
    fn shm_fd_requires_protocol_version_one_two() {
        assert!(!supports_shm_fd_version(0, 99));
        assert!(!supports_shm_fd_version(1, 1));
        assert!(supports_shm_fd_version(1, 2));
        assert!(supports_shm_fd_version(1, 3));
        assert!(supports_shm_fd_version(2, 0));
    }

    #[test]
    fn shm_buffer_size_uses_native_bits_and_scanline_padding() {
        let depth_24_in_32 = Format {
            depth: 24,
            bits_per_pixel: 32,
            scanline_pad: 32,
        };
        assert_eq!(
            shm_buffer_size(1280, 536, depth_24_in_32).unwrap(),
            2_744_320
        );
        assert_eq!(shm_buffer_size(1, 1, depth_24_in_32).unwrap(), 4);
        assert_eq!(shm_buffer_size(3, 1, depth_24_in_32).unwrap(), 12);

        let padded_16 = Format {
            depth: 16,
            bits_per_pixel: 16,
            scanline_pad: 32,
        };
        // Three 16-bit pixels occupy 48 bits and round up to an 8-byte row.
        assert_eq!(shm_buffer_size(3, 2, padded_16).unwrap(), 16);
        assert!(shm_buffer_size(0, 2, padded_16).is_err());
        assert!(shm_buffer_size(2, 0, padded_16).is_err());

        let invalid = Format {
            depth: 24,
            bits_per_pixel: 32,
            scanline_pad: 24,
        };
        assert!(shm_buffer_size(1, 1, invalid).is_err());
        let invalid = Format {
            depth: 24,
            bits_per_pixel: 12,
            scanline_pad: 32,
        };
        assert!(shm_buffer_size(1, 1, invalid).is_err());

        // Valid X11 dimensions can still exceed MIT-SHM's CARD32 size.
        assert!(shm_buffer_size(u16::MAX, u16::MAX, depth_24_in_32).is_err());
    }

    #[test]
    fn shm_reply_must_exactly_match_native_image_and_mapping() {
        assert_eq!(validate_shm_reply(24, 64, 128, 24, 64).unwrap(), 64);
        assert!(validate_shm_reply(24, 64, 128, 16, 64).is_err());
        assert!(validate_shm_reply(24, 64, 128, 24, 60).is_err());
        assert!(validate_shm_reply(24, 64, 128, 24, 68).is_err());
        assert!(validate_shm_reply(24, 64, 63, 24, 64).is_err());
    }

    #[test]
    fn shm_capacity_is_reused_and_only_grows() {
        assert!(shm_needs_growth(None, 64));
        assert!(!shm_needs_growth(Some(64), 64));
        assert!(!shm_needs_growth(Some(128), 64));
        assert!(shm_needs_growth(Some(63), 64));
    }

    #[test]
    fn shm_failure_uses_same_frame_core_before_disabling() {
        let calls = Cell::new(0);
        let outcome = resolve_readback(None, || {
            calls.set(calls.get() + 1);
            Ok::<_, &'static str>(5)
        });
        assert_eq!(calls.get(), 1);
        assert!(matches!(outcome, ReadbackOutcome::Image(5)));

        let calls = Cell::new(0);
        let outcome = resolve_readback(Some(Err("shm")), || {
            calls.set(calls.get() + 1);
            Ok::<_, &'static str>(7)
        });
        assert_eq!(calls.get(), 1);
        match outcome {
            ReadbackOutcome::CoreFallback { image, shm_error } => {
                assert_eq!(image, 7);
                assert_eq!(shm_error, "shm");
            }
            _ => panic!("SHM failure with a valid drawable must use core fallback"),
        }

        let calls = Cell::new(0);
        let outcome = resolve_readback(Some(Ok::<_, &'static str>(9)), || {
            calls.set(calls.get() + 1);
            Ok(10)
        });
        assert_eq!(calls.get(), 0);
        assert!(matches!(outcome, ReadbackOutcome::Image(9)));

        let calls = Cell::new(0);
        let outcome = resolve_readback(Some(Err("shm")), || {
            calls.set(calls.get() + 1);
            Err::<u8, _>("core")
        });
        assert_eq!(calls.get(), 1);
        assert!(matches!(outcome, ReadbackOutcome::Error("core")));
    }
}
