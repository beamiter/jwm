//! Small X11 viewer used by the remote-control client.
//!
//! The viewer uses core X11 plus an optional MIT-SHM upload fast path. That
//! keeps it usable under JWM's x11rb and xcb backends without bringing a GUI
//! toolkit into the remote-control process.

use std::borrow::Cow;
use std::collections::{HashSet, VecDeque};
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::ptr::{self, NonNull};
use std::sync::Arc;
use std::time::{Duration, Instant};

use image::RgbImage;
use x11rb::CURRENT_TIME;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::errors::{ConnectionError, ReplyError, ReplyOrIdError};
use x11rb::image::{BitsPerPixel, Image as XImage, ImageOrder, PixelLayout, ScanlinePad};
use x11rb::protocol::render::{
    self, CreatePictureAux, PictOp, Pictformat, Picture, Repeat, Transform,
};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ClientMessageEvent, ConnectionExt as _, CreateGCAux, CreateWindowAux, Cursor,
    EventMask, Format, Gcontext, GrabMode, GrabStatus, ImageFormat, ImageOrder as X11ImageOrder,
    KeyButMask, Mapping, NotifyMode, Pixmap, PropMode, Rectangle, Screen, Setup, VisualClass,
    Visualtype, Window, WindowClass,
};
use x11rb::protocol::{ErrorKind, Event, shm};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::x11_utils::{TryParseFd, X11Error};

use super::cursor::{CursorPosition, CursorShape};
use super::frame::{DamageRegions, DecodedFrame, FrameDamage, RecyclableDecodedFrame};
use super::pacing::EventPollBudget;
use super::triangle_patch::TrianglePatch;
use super::usability::viewer_title;
use super::{RemoteError, RemoteResult};

const WM_CLASS: &[u8] = b"jwm-remote\0JwmRemote\0";
/// Default base key of the Ctrl+Super grab-release shortcut.
pub const DEFAULT_ESCAPE_KEY: &str = "F12";
const KEY_RELEASE_DEFER: Duration = Duration::from_millis(8);
const MAX_PENDING_PRESENTATIONS: usize = 3;
const SHM_UPLOAD_SLOTS: usize = 3;
const PRESENTATION_TIMEOUT: Duration = Duration::from_secs(5);
const SHM_FD_VERSION: (u16, u16) = (1, 2);
const RENDER_CLIENT_VERSION: (u32, u32) = (0, 11);
const RENDER_MIN_VERSION: (u32, u32) = (0, 10);
const XFIXED_ONE: i32 = 1 << 16;
const RENDER_FILTER_BILINEAR: &[u8] = b"bilinear";

/// One local viewer operation for the remote-control client to send upstream.
///
/// Keycodes are X11 server keycodes.  `Pointer` coordinates are in the remote
/// source/root coordinate space, even when the viewer window is letterboxed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewerEvent {
    WaylandPointer {
        frame_sequence: u64,
        geometry_epoch: u64,
        x: u16,
        y: u16,
        local_x: i16,
        local_y: i16,
        kind: WaylandPointerEventKind,
    },
    WaylandPointerCancel,
    /// An unmappable local left-button edge; never a remote action.
    WaylandPointerButtonCancel {
        pressed: bool,
    },
    WaylandPointerRevoke,
    CycleSource,
    NarrowClipboard,
    Close,
    Pointer {
        x: u16,
        y: u16,
    },
    Key {
        keycode: u8,
        pressed: bool,
    },
    Button {
        button: u8,
        pressed: bool,
    },
    /// Release every key and button held by the remote input injector.
    ReleaseAll,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaylandPointerEventKind {
    Motion,
    LeftDown,
    LeftUp,
}

#[derive(Default)]
struct ForwardedButtons(HashSet<u8>);

impl ForwardedButtons {
    fn press(&mut self, button: u8, inside_image: bool) -> bool {
        if !inside_image {
            return false;
        }
        self.0.insert(button);
        true
    }

    fn release(&mut self, button: u8) -> bool {
        self.0.remove(&button)
    }

    fn clear(&mut self) {
        self.0.clear();
    }
}

#[derive(Debug)]
struct StoredFrame {
    sequence: u64,
    geometry_epoch: u64,
    source_width: u16,
    source_height: u16,
    image: StoredImage,
}

#[derive(Debug)]
enum StoredImage {
    Owned(RgbImage),
    Recyclable(RecyclableDecodedFrame),
}

impl StoredFrame {
    fn from_decoded(frame: DecodedFrame) -> Self {
        Self {
            sequence: frame.sequence,
            geometry_epoch: 0,
            source_width: frame.source_width,
            source_height: frame.source_height,
            image: StoredImage::Owned(frame.image),
        }
    }

    fn from_recyclable(frame: RecyclableDecodedFrame) -> Self {
        Self {
            sequence: frame.sequence(),
            geometry_epoch: frame.geometry_epoch(),
            source_width: frame.source_width(),
            source_height: frame.source_height(),
            image: StoredImage::Recyclable(frame),
        }
    }

    fn dimensions(&self) -> (u32, u32) {
        match &self.image {
            StoredImage::Owned(image) => image.dimensions(),
            StoredImage::Recyclable(frame) => frame.dimensions(),
        }
    }

    fn damage(&self) -> Option<FrameDamage> {
        match &self.image {
            StoredImage::Owned(_) => None,
            StoredImage::Recyclable(frame) => frame.damage(),
        }
    }

    fn image(&self) -> &RgbImage {
        match &self.image {
            StoredImage::Owned(image) => image,
            StoredImage::Recyclable(frame) => frame.image(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct PendingKeyRelease {
    keycode: u8,
    time: u32,
    queued_at: Instant,
}

/// Deduplicate forwarded key presses and core X11 autorepeat pairs.
#[derive(Default)]
struct ViewerKeyboardState {
    held: HashSet<u8>,
    pending_release: Option<PendingKeyRelease>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyPressAction {
    Ignore,
    Forward,
    ReleaseGrab,
}

impl ViewerKeyboardState {
    fn press(&mut self, keycode: u8, escape: bool, forward_keyboard: bool) -> KeyPressAction {
        if (!forward_keyboard && !escape) || !self.held.insert(keycode) {
            return KeyPressAction::Ignore;
        }
        if escape {
            KeyPressAction::ReleaseGrab
        } else if forward_keyboard {
            KeyPressAction::Forward
        } else {
            KeyPressAction::Ignore
        }
    }

    fn defer_release(&mut self, keycode: u8, time: u32, now: Instant) {
        if self.held.contains(&keycode) {
            self.pending_release = Some(PendingKeyRelease {
                keycode,
                time,
                queued_at: now,
            });
        }
    }

    fn consume_repeat(&mut self, keycode: u8, time: u32) -> bool {
        if self
            .pending_release
            .is_some_and(|release| release.keycode == keycode && release.time == time)
        {
            self.pending_release = None;
            true
        } else {
            false
        }
    }

    fn flush_release(&mut self, events: &mut Vec<ViewerEvent>, forward_keyboard: bool) {
        if let Some(release) = self.pending_release.take()
            && self.held.remove(&release.keycode)
            && forward_keyboard
        {
            events.push(ViewerEvent::Key {
                keycode: release.keycode,
                pressed: false,
            });
        }
    }
}

fn reset_input_geometry(
    keyboard: &mut ViewerKeyboardState,
    buttons: &mut ForwardedButtons,
    presented: &mut Option<u64>,
) {
    *keyboard = ViewerKeyboardState::default();
    buttons.clear();
    *presented = None;
}

// An external keyboard grab diverts releases even though the ordinary focus
// has not changed. Our own grab's synthetic focus notifications must not tear
// down the grab we just acquired.
fn focus_out_releases_input(mode: NotifyMode, owns_grab: bool) -> bool {
    mode == NotifyMode::NORMAL || (mode == NotifyMode::GRAB && !owns_grab)
}

fn clear_forwarded_input(
    keyboard: &mut ViewerKeyboardState,
    buttons: &mut ForwardedButtons,
    events: &mut Vec<ViewerEvent>,
) {
    // A focus/grab boundary ends all held keys and deferred releases.
    *keyboard = ViewerKeyboardState::default();
    buttons.clear();
    events.push(ViewerEvent::ReleaseAll);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativePixelWriter {
    /// Native depth-24 pixels stored as little-endian 0x00RRGGBB words.
    Bgrx32,
    /// Any valid but less common TrueColor layout handled through PixelLayout.
    Generic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BackingWork {
    PopulateAndCopy,
    CopyOnly,
}

/// Only two shapes are retained: the current position's shape and the newest
/// arrival. Unknown shape references hide rather than displaying stale pixels.
#[derive(Default)]
struct CursorPlane {
    enabled: bool,
    render: Option<CursorRender>,
    shapes: VecDeque<Arc<CursorShape>>,
    position: Option<CursorPosition>,
    drawn: Option<Letterbox>,
}

struct CursorRender {
    format: Pictformat,
    target: Picture,
    source: Option<CursorRenderSource>,
}

struct CursorRenderSource {
    id: u64,
    pixmap: Pixmap,
    picture: Picture,
}

/// Capability probing happens before advertising cursor separation to the host.
/// An unavailable Render implementation keeps the composited-video fallback.
pub(crate) fn supports_cursor_plane(display: Option<&str>) -> RemoteResult<bool> {
    let (conn, screen) = xbar_core::x11_connection::connect(display)?;
    Ok(cursor_formats(&conn, screen)?.is_some())
}

fn cursor_formats(
    conn: &RustConnection,
    screen: usize,
) -> RemoteResult<Option<(Pictformat, Pictformat)>> {
    if conn
        .extension_information(render::X11_EXTENSION_NAME)?
        .is_none()
    {
        return Ok(None);
    }
    let version = match render::query_version(conn, 0, 11)?.reply() {
        Ok(version) => version,
        Err(ReplyError::X11Error(_)) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !supports_render_version(version.major_version, version.minor_version) {
        return Ok(None);
    }
    let formats = match render::query_pict_formats(conn)?.reply() {
        Ok(formats) => formats,
        Err(ReplyError::X11Error(_)) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let root = &conn.setup().roots[screen];
    let target = formats.screens.get(screen).and_then(|screen| {
        screen
            .depths
            .iter()
            .flat_map(|depth| depth.visuals.iter())
            .find(|visual| visual.visual == root.root_visual)
            .map(|visual| visual.format)
    });
    let argb = formats
        .formats
        .iter()
        .find(|format| {
            let d = &format.direct;
            format.type_ == render::PictType::DIRECT
                && format.depth == 32
                && d.red_shift == 16
                && d.red_mask == 255
                && d.green_shift == 8
                && d.green_mask == 255
                && d.blue_shift == 0
                && d.blue_mask == 255
                && d.alpha_shift == 24
                && d.alpha_mask == 255
        })
        .map(|format| format.id);
    if !root.allowed_depths.iter().any(|depth| depth.depth == 32)
        || !conn
            .setup()
            .pixmap_formats
            .iter()
            .any(|format| format.depth == 32 && format.bits_per_pixel == 32)
    {
        return Ok(None);
    }
    Ok(argb.zip(target))
}

fn cursor_source_needs_upload(current: Option<u64>, incoming: u64) -> bool {
    current != Some(incoming)
}

impl CursorRender {
    fn prepare_shape(
        &mut self,
        conn: &RustConnection,
        window: Window,
        shape: &CursorShape,
    ) -> RemoteResult<()> {
        if !cursor_source_needs_upload(self.source.as_ref().map(|source| source.id), shape.id) {
            return Ok(());
        }
        let pixmap = conn.generate_id()?;
        conn.create_pixmap(32, pixmap, window, shape.width, shape.height)?
            .check()?;
        let picture = conn.generate_id()?;
        let gc = conn.generate_id()?;
        let setup: RemoteResult<()> = (|| {
            conn.create_gc(gc, pixmap, &CreateGCAux::new())?.check()?;
            let mut image = XImage::allocate_native(shape.width, shape.height, 32, conn.setup())?;
            for y in 0..shape.height {
                for x in 0..shape.width {
                    let i = (usize::from(y) * usize::from(shape.width) + usize::from(x)) * 4;
                    let p = &shape.rgba[i..i + 4];
                    image.put_pixel(
                        x,
                        y,
                        u32::from(p[3]) << 24
                            | u32::from(p[0]) << 16
                            | u32::from(p[1]) << 8
                            | u32::from(p[2]),
                    );
                }
            }
            for cookie in image.put(conn, pixmap, gc, 0, 0)? {
                cookie.check()?;
            }
            render::create_picture(conn, picture, pixmap, self.format, &CreatePictureAux::new())?
                .check()?;
            Ok(())
        })();
        let _ = conn.free_gc(gc);
        if let Err(error) = setup {
            let _ = conn.free_pixmap(pixmap);
            return Err(error);
        }
        if let Some(old) = self.source.replace(CursorRenderSource {
            id: shape.id,
            pixmap,
            picture,
        }) {
            free_render_picture_unchecked(conn, old.picture);
            free_render_pixmap_unchecked(conn, old.pixmap);
        }
        Ok(())
    }

    fn release(&mut self, conn: &RustConnection) {
        if let Some(source) = self.source.take() {
            free_render_picture_unchecked(conn, source.picture);
            free_render_pixmap_unchecked(conn, source.pixmap);
        }
        free_render_picture_unchecked(conn, self.target);
    }
}

impl CursorPlane {
    fn insert_shape(&mut self, shape: Arc<CursorShape>) {
        self.shapes.retain(|old| old.id != shape.id);
        while self.shapes.len() >= 2 {
            let current = self.position.as_ref().map(|p| p.shape_id);
            let index = self
                .shapes
                .iter()
                .position(|s| Some(s.id) != current)
                .unwrap_or(0);
            self.shapes.remove(index);
        }
        self.shapes.push_back(shape);
    }

    fn accept_position(&mut self, position: CursorPosition) -> bool {
        if self
            .position
            .as_ref()
            .is_some_and(|old| old.sequence >= position.sequence)
        {
            return false;
        }
        self.position = Some(position);
        true
    }
}

#[derive(Debug)]
struct CursorPlacement {
    left: i64,
    top: i64,
    scale_x: i64,
    scale_y: i64,
    offset_x: i64,
    offset_y: i64,
    clip: Letterbox,
}

fn cursor_placement(
    position: &CursorPosition,
    shape: &CursorShape,
    epoch: u64,
    source: (u16, u16),
    geometry: Letterbox,
) -> Option<CursorPlacement> {
    if !position.visible
        || position.geometry_epoch != epoch
        || (position.source_width, position.source_height) != source
        || source.0 == 0
        || source.1 == 0
        || geometry.width == 0
        || geometry.height == 0
        || shape.width == 0
        || shape.height == 0
    {
        return None;
    }
    let left = i64::from(geometry.x)
        + ((i64::from(position.x) - i64::from(shape.hotspot_x)) * i64::from(geometry.width))
            .div_euclid(i64::from(source.0));
    let top = i64::from(geometry.y)
        + ((i64::from(position.y) - i64::from(shape.hotspot_y)) * i64::from(geometry.height))
            .div_euclid(i64::from(source.1));
    let source_left = i64::from(position.x) - i64::from(shape.hotspot_x);
    let source_top = i64::from(position.y) - i64::from(shape.hotspot_y);
    let ceil_div = |n: i64, d: i64| -(-n).div_euclid(d);
    let right = (i64::from(geometry.x)
        + ceil_div(
            (source_left + i64::from(shape.width)) * i64::from(geometry.width),
            i64::from(source.0),
        ))
    .min(i64::from(geometry.x) + i64::from(geometry.width));
    let bottom = (i64::from(geometry.y)
        + ceil_div(
            (source_top + i64::from(shape.height)) * i64::from(geometry.height),
            i64::from(source.1),
        ))
    .min(i64::from(geometry.y) + i64::from(geometry.height));
    let x = left.max(i64::from(geometry.x));
    let y = top.max(i64::from(geometry.y));
    // X11 destination coordinates are signed 16-bit.
    if right <= x || bottom <= y || x > i64::from(i16::MAX) || y > i64::from(i16::MAX) {
        return None;
    }
    Some(CursorPlacement {
        left,
        top,
        scale_x: i64::from(source.0) * i64::from(XFIXED_ONE) / i64::from(geometry.width),
        scale_y: i64::from(source.1) * i64::from(XFIXED_ONE) / i64::from(geometry.height),
        offset_x: (x - i64::from(geometry.x)) * i64::from(source.0) * i64::from(XFIXED_ONE)
            / i64::from(geometry.width)
            - source_left * i64::from(XFIXED_ONE),
        offset_y: (y - i64::from(geometry.y)) * i64::from(source.1) * i64::from(XFIXED_ONE)
            / i64::from(geometry.height)
            - source_top * i64::from(XFIXED_ONE),
        clip: Letterbox {
            x: x as u16,
            y: y as u16,
            width: (right - x) as u16,
            height: (bottom - y) as u16,
        },
    })
}

fn cursor_transform(placement: &CursorPlacement) -> RemoteResult<Transform> {
    debug_assert!(placement.left <= i64::from(placement.clip.x));
    debug_assert!(placement.top <= i64::from(placement.clip.y));
    let fixed = |value: i64| {
        i32::try_from(value)
            .map_err(|_| invalid_data("cursor transform exceeds XRender fixed-point range"))
    };
    Ok(Transform {
        matrix11: fixed(placement.scale_x)?,
        matrix12: 0,
        matrix13: fixed(placement.offset_x)?,
        matrix21: 0,
        matrix22: fixed(placement.scale_y)?,
        matrix23: fixed(placement.offset_y)?,
        matrix31: 0,
        matrix32: 0,
        matrix33: XFIXED_ONE,
    })
}

/// Plain X11 window that displays decoded remote frames and collects input.
pub struct Viewer {
    conn: RustConnection,
    window: Window,
    gc: Gcontext,
    backing: Pixmap,
    blank_cursor: Cursor,
    cursor_plane: CursorPlane,
    cursor_screen: usize,
    wm_protocols: Atom,
    wm_delete_window: Atom,
    net_wm_name: Atom,
    utf8_string: Atom,
    clipboard_enabled: bool,
    clipboard_policy: String,
    /// Transfer progress or outcome shown after the clipboard state.
    clipboard_activity: Option<String>,
    depth: u8,
    pixel_layout: PixelLayout,
    native_pixel_writer: NativePixelWriter,
    native_image: Option<XImage<'static>>,
    native_patch: Option<XImage<'static>>,
    triangle_patch: TrianglePatch,
    shm_upload: ShmUpload,
    presentation_atom: Atom,
    presentations: VecDeque<PendingPresentation>,
    recovery: Option<AccelerationRecovery>,
    presentation_serial: u64,
    render_upscale: RenderUpscale,
    width: u16,
    height: u16,
    backing_valid: bool,
    pending_resize: bool,
    pending_redraw: bool,
    /// Letterbox the retained backing pixmap was last cleared for.
    ///
    /// The pixmap survives between frames and an upload only ever touches the
    /// image rectangle, so the bars around it stay correct until the
    /// letterbox itself moves. Clearing the whole window every frame cost a
    /// full-window fill plus a synchronous round trip for pixels the very next
    /// upload overwrote.
    backing_letterbox: Option<Letterbox>,
    grab_input: bool,
    forward_input: bool,
    source_switching: bool,
    source_cycle_enabled: bool,
    clipboard_narrowing_enabled: bool,
    clipboard_shortcut: ViewerKeyboardState,
    switching_label: Option<String>,
    source_label: Option<String>,
    presented_geometry: Option<u64>,
    pointer_enabled: bool,
    wayland_pointer: bool,
    pointer_presented: Option<u64>,
    pointer_status: Option<&'static str>,
    forward_keyboard: bool,
    #[cfg(feature = "remote-wayland")]
    wayland_keymap: Option<super::keyboard_keymap::KeyboardKeymap>,
    verified_keymap: Option<super::x11_keymap::KeymapTable>,
    grabbed: bool,
    escape_keycodes: Vec<u8>,
    escape_keysym: u32,
    escape_name: String,
    /// The machine this window shows, first in its title.
    peer_label: String,
    keyboard: ViewerKeyboardState,
    held_buttons: ForwardedButtons,
    deferred_events: VecDeque<Event>,
    last_frame: Option<StoredFrame>,
    closed: bool,
    user_close_requested: bool,
    user_released_grab: bool,
    user_restricted_clipboard: bool,
    close_reported: bool,
}

fn local_retry_intent_allows(
    events: &[ViewerEvent],
    closed: bool,
    released: bool,
    restricted: bool,
    backlog: bool,
) -> bool {
    !closed
        && !released
        && !restricted
        && !backlog
        && !events
            .iter()
            .any(|event| matches!(event, ViewerEvent::Close | ViewerEvent::NarrowClipboard))
}

/// Bound viewer resources independently of negotiated capture/startup budgets.
/// This is a hard ceiling, not a guarantee that large images fit available RAM.
fn validate_viewer_dimensions(width: u32, height: u32) -> RemoteResult<()> {
    if width == 0
        || height == 0
        || width > u32::from(super::frame::MAX_DIMENSION)
        || height > u32::from(super::frame::MAX_DIMENSION)
        || u64::from(width) * u64::from(height) > super::frame::MAX_PIXELS
    {
        return Err(invalid_data(
            "viewer dimensions must be nonzero, at most 16384 per axis and 67108864 pixels",
        )
        .into());
    }
    Ok(())
}

fn configured_viewer_dimensions(width: u16, height: u16) -> RemoteResult<(u16, u16)> {
    // Preserve the existing normalization for transient zero-sized X events.
    let dimensions = (width.max(1), height.max(1));
    validate_viewer_dimensions(u32::from(dimensions.0), u32::from(dimensions.1))?;
    Ok(dimensions)
}

/// Share of the local monitor a new viewer window may ask for, per axis.
const INITIAL_WINDOW_PERCENT: u32 = 90;

/// Size a new viewer window: the first frame, shrunk to fit inside
/// `INITIAL_WINDOW_PERCENT` of `monitor` with its aspect kept, never enlarged.
fn fit_initial_window(frame: (u16, u16), monitor: (u16, u16)) -> (u16, u16) {
    let (frame_width, frame_height) = (u64::from(frame.0), u64::from(frame.1));
    let bound_width = u64::from(monitor.0) * u64::from(INITIAL_WINDOW_PERCENT) / 100;
    let bound_height = u64::from(monitor.1) * u64::from(INITIAL_WINDOW_PERCENT) / 100;
    if bound_width == 0
        || bound_height == 0
        || frame_width == 0
        || frame_height == 0
        || (frame_width <= bound_width && frame_height <= bound_height)
    {
        return frame;
    }
    // Whichever axis binds harder sets the scale.
    let (width, height) = if bound_width * frame_height <= bound_height * frame_width {
        (bound_width, frame_height * bound_width / frame_width)
    } else {
        (frame_width * bound_height / frame_height, bound_height)
    };
    (
        u16::try_from(width.max(1)).unwrap_or(frame.0),
        u16::try_from(height.max(1)).unwrap_or(frame.1),
    )
}

/// Explicit size is independent of the first decoded frame and its budget.
fn startup_window_size(
    frame: (u16, u16),
    monitor: (u16, u16),
    size: Option<super::window_size::WindowSize>,
) -> (u16, u16) {
    size.map_or_else(
        || fit_initial_window(frame, monitor),
        |size| size.dimensions(),
    )
}

/// Origin that centres a `size` window on `monitor` (`x, y, width, height`).
fn centered_origin(monitor: (i16, i16, u16, u16), size: (u16, u16)) -> (i16, i16) {
    let offset = |origin: i16, extent: u16, length: u16| {
        let centred = i32::from(origin) + (i32::from(extent) - i32::from(length)).max(0) / 2;
        i16::try_from(centred).unwrap_or(origin)
    };
    (
        offset(monitor.0, monitor.2, size.0),
        offset(monitor.1, monitor.3, size.1),
    )
}

/// The local monitor a new viewer should fit: the one under the pointer,
/// else the primary, else the whole screen. Best effort -- any failure here
/// only costs the old full-size window.
fn placement_monitor(
    conn: &RustConnection,
    root: Window,
    screen_size: (u16, u16),
) -> (i16, i16, u16, u16) {
    use x11rb::protocol::randr::ConnectionExt as _;
    let whole_screen = (0, 0, screen_size.0, screen_size.1);
    let Some(monitors) = conn
        .randr_get_monitors(root, true)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
    else {
        return whole_screen;
    };
    let pointer = conn
        .query_pointer(root)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|reply| (i32::from(reply.root_x), i32::from(reply.root_y)));
    let contains = |monitor: &&x11rb::protocol::randr::MonitorInfo, (x, y): (i32, i32)| {
        let left = i32::from(monitor.x);
        let top = i32::from(monitor.y);
        x >= left
            && y >= top
            && x < left + i32::from(monitor.width)
            && y < top + i32::from(monitor.height)
    };
    monitors
        .monitors
        .iter()
        .filter(|monitor| monitor.width > 0 && monitor.height > 0)
        .find(|monitor| pointer.is_some_and(|point| contains(monitor, point)))
        .or_else(|| monitors.monitors.iter().find(|monitor| monitor.primary))
        .map(|monitor| (monitor.x, monitor.y, monitor.width, monitor.height))
        .unwrap_or(whole_screen)
}

/// Query the prospective viewer bounds before requesting the first video frame.
/// Uses the same monitor selection as window creation; a later ConfigureNotify
/// remains authoritative if the WM tiles or resizes the window differently.
pub(crate) fn initial_viewport(
    display: Option<&str>,
    size: Option<super::window_size::WindowSize>,
) -> RemoteResult<(u16, u16)> {
    if let Some(size) = size {
        size.validate()?;
        return Ok(size.dimensions());
    }
    let (conn, screen_num) = xbar_core::x11_connection::connect(display)?;
    let screen = conn
        .setup()
        .roots
        .get(screen_num)
        .ok_or_else(|| invalid_data("X11 selected an unavailable screen"))?;
    let monitor = placement_monitor(
        &conn,
        screen.root,
        (screen.width_in_pixels, screen.height_in_pixels),
    );
    if monitor.2 == 0 || monitor.3 == 0 {
        return Err(invalid_data("local monitor has an empty dimension").into());
    }
    Ok((monitor.2, monitor.3))
}

/// Select only currently visible monitor bounds for an explicit position.
fn position_target(
    conn: &RustConnection,
    root: Window,
    screen_size: (u16, u16),
    position: super::window_position::WindowPosition,
) -> RemoteResult<super::window_position::PlacementTarget> {
    use x11rb::protocol::randr::ConnectionExt as _;
    let monitors = conn
        .randr_get_monitors(root, true)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|reply| {
            reply
                .monitors
                .iter()
                .map(|m| (m.x, m.y, m.width, m.height))
                .collect::<Vec<_>>()
        });
    let fallback = placement_monitor(conn, root, screen_size);
    Ok(super::window_position::select_target(
        monitors.as_deref(),
        fallback,
        screen_size,
        position,
    )?)
}

pub(crate) fn initial_viewport_with_position(
    display: Option<&str>,
    size: Option<super::window_size::WindowSize>,
    position: Option<super::window_position::WindowPosition>,
) -> RemoteResult<(u16, u16)> {
    let Some(position) = position else {
        return initial_viewport(display, size);
    };
    if let Some(size) = size {
        size.validate()?;
        return Ok(size.dimensions());
    }
    let (conn, screen_num) = xbar_core::x11_connection::connect(display)?;
    let screen = conn
        .setup()
        .roots
        .get(screen_num)
        .ok_or_else(|| invalid_data("X11 selected an unavailable screen"))?;
    let target = position_target(
        &conn,
        screen.root,
        (screen.width_in_pixels, screen.height_in_pixels),
        position,
    )?;
    Ok((target.bounds.2, target.bounds.3))
}

fn position_hints(origin: (i16, i16), accepted: bool) -> Option<x11rb::properties::WmSizeHints> {
    accepted.then(|| {
        let mut hints = x11rb::properties::WmSizeHints::new();
        hints.position = Some((
            x11rb::properties::WmSizeHintsSpecification::UserSpecified,
            i32::from(origin.0),
            i32::from(origin.1),
        ));
        hints
    })
}

impl Viewer {
    /// Open an X11 window on `display`, or on `$DISPLAY` when it is `None`.
    pub fn connect(
        display: Option<&str>,
        initial_width: u16,
        initial_height: u16,
        pointer_enabled: bool,
        forward_keyboard: bool,
        verified_keymap: Option<super::x11_keymap::KeymapTable>,
        grab_input: bool,
        escape_key: &str,
        peer_label: &str,
    ) -> RemoteResult<Self> {
        Self::connect_with_window_size(
            display,
            initial_width,
            initial_height,
            None,
            pointer_enabled,
            forward_keyboard,
            verified_keymap,
            grab_input,
            escape_key,
            peer_label,
        )
    }

    /// Internal startup with an explicit, validated local client-size request.
    pub(crate) fn connect_with_window_size(
        display: Option<&str>,
        initial_width: u16,
        initial_height: u16,
        window_size: Option<super::window_size::WindowSize>,
        pointer_enabled: bool,
        forward_keyboard: bool,
        verified_keymap: Option<super::x11_keymap::KeymapTable>,
        grab_input: bool,
        escape_key: &str,
        peer_label: &str,
    ) -> RemoteResult<Self> {
        Self::connect_with_window_geometry(
            display,
            initial_width,
            initial_height,
            window_size,
            None,
            pointer_enabled,
            forward_keyboard,
            verified_keymap,
            grab_input,
            escape_key,
            peer_label,
        )
    }

    /// Explicit startup geometry is advisory; the window manager remains authoritative.
    pub(crate) fn connect_with_window_geometry(
        display: Option<&str>,
        initial_width: u16,
        initial_height: u16,
        window_size: Option<super::window_size::WindowSize>,
        window_position: Option<super::window_position::WindowPosition>,
        pointer_enabled: bool,
        forward_keyboard: bool,
        verified_keymap: Option<super::x11_keymap::KeymapTable>,
        grab_input: bool,
        escape_key: &str,
        peer_label: &str,
    ) -> RemoteResult<Self> {
        if let Some(size) = window_size {
            size.validate()?;
        }
        let forward_input = pointer_enabled || forward_keyboard;
        validate_viewer_dimensions(u32::from(initial_width), u32::from(initial_height))?;

        let (conn, screen_num) = xbar_core::x11_connection::connect(display)?;
        if forward_keyboard {
            let expected = verified_keymap
                .as_ref()
                .ok_or_else(|| invalid_data("keyboard input has no verified X11 keymap"))?;
            if !super::x11_keymap::KeymapTable::read(&conn)?.still_covers(expected) {
                return Err(invalid_data(
                    "local X11 keymap changed during connection; reconnect to negotiate input",
                )
                .into());
            }
        }
        let (
            root,
            root_depth,
            root_visual,
            black_pixel,
            pixel_layout,
            native_pixel_writer,
            screen_size,
        ) = {
            let screen = conn
                .setup()
                .roots
                .get(screen_num)
                .ok_or_else(|| invalid_data("X11 selected an unavailable screen"))?;
            let (depth, visual) = find_visual(screen, screen.root_visual)
                .ok_or_else(|| invalid_data("X11 root visual is not described by the screen"))?;
            if visual.class != VisualClass::TRUE_COLOR {
                return Err(invalid_data("remote viewer requires an X11 TrueColor visual").into());
            }
            (
                screen.root,
                depth,
                screen.root_visual,
                screen.black_pixel,
                PixelLayout::from_visual_type(visual)?,
                select_native_pixel_writer(conn.setup(), depth, visual),
                (screen.width_in_pixels, screen.height_in_pixels),
            )
        };
        // Recheck placement because the local topology may have changed.
        // Without an explicit size, fit the first frame to the local monitor
        // as before. Explicit size is independent of decoded-frame dimensions;
        // the actual viewport report follows if the WM overrides our request.
        let target = window_position
            .map(|position| position_target(&conn, root, screen_size, position))
            .transpose()?;
        let monitor = target.map_or_else(
            || placement_monitor(&conn, root, screen_size),
            |target| target.bounds,
        );
        let (initial_width, initial_height) = startup_window_size(
            (initial_width, initial_height),
            (monitor.2, monitor.3),
            window_size,
        );
        validate_viewer_dimensions(u32::from(initial_width), u32::from(initial_height))?;
        let ((window_x, window_y), accepted_position) = match (window_position, target) {
            (Some(position), Some(target)) => {
                let placement = super::window_position::place(
                    target,
                    (initial_width, initial_height),
                    position,
                );
                if !placement.1 {
                    eprintln!(
                        "jwm-remote: requested window position {position} is not fully visible or cannot be verified; using local fallback {},{}",
                        placement.0.0, placement.0.1
                    );
                }
                placement
            }
            _ => (
                centered_origin(monitor, (initial_width, initial_height)),
                false,
            ),
        };
        // MIT-SHM requires QueryVersion to complete before any other request
        // from the extension. Segment allocation remains lazy until a frame is
        // actually uploaded.
        let shm_upload = if native_pixel_writer == NativePixelWriter::Bgrx32 {
            ShmUpload::connect(&conn, root_depth)
        } else {
            ShmUpload::Disabled
        };
        let mut render_upscale =
            RenderUpscale::connect(&conn, screen_num, root, root_depth, root_visual)?;

        let presentation_atom = intern_atom(&conn, b"_JWM_REMOTE_PRESENTED")?;
        let wm_protocols = intern_atom(&conn, b"WM_PROTOCOLS")?;
        let wm_delete_window = intern_atom(&conn, b"WM_DELETE_WINDOW")?;
        let net_wm_name = intern_atom(&conn, b"_NET_WM_NAME")?;
        let utf8_string = intern_atom(&conn, b"UTF8_STRING")?;

        let window = conn.generate_id()?;
        let gc = conn.generate_id()?;
        let blank_cursor = create_blank_cursor(&conn, root)?;
        let event_mask = EventMask::EXPOSURE
            | EventMask::STRUCTURE_NOTIFY
            | EventMask::FOCUS_CHANGE
            | EventMask::POINTER_MOTION
            | EventMask::BUTTON_PRESS
            | EventMask::BUTTON_RELEASE
            | EventMask::KEY_PRESS
            | EventMask::KEY_RELEASE;
        let mut window_attributes = CreateWindowAux::new()
            .background_pixel(black_pixel)
            .border_pixel(black_pixel)
            .event_mask(event_mask);
        if forward_input && !grab_input {
            window_attributes = window_attributes.cursor(blank_cursor);
        }
        conn.create_window(
            root_depth,
            window,
            root,
            window_x,
            window_y,
            initial_width,
            initial_height,
            0,
            WindowClass::INPUT_OUTPUT,
            root_visual,
            &window_attributes,
        )?
        .check()?;
        conn.create_gc(
            gc,
            window,
            &CreateGCAux::new()
                .foreground(black_pixel)
                .background(black_pixel)
                .graphics_exposures(0),
        )?
        .check()?;
        let backing = conn.generate_id()?;
        conn.create_pixmap(root_depth, backing, window, initial_width, initial_height)?
            .check()?;
        render_upscale.attach_backing(&conn, backing)?;

        let title = viewer_title(
            peer_label,
            pointer_enabled,
            forward_keyboard,
            grab_input,
            false,
            false,
            None,
            escape_key,
        );
        conn.change_property8(
            PropMode::REPLACE,
            window,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            title.as_bytes(),
        )?
        .check()?;
        conn.change_property8(
            PropMode::REPLACE,
            window,
            net_wm_name,
            utf8_string,
            title.as_bytes(),
        )?
        .check()?;
        conn.change_property8(
            PropMode::REPLACE,
            window,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            WM_CLASS,
        )?
        .check()?;
        conn.change_property32(
            PropMode::REPLACE,
            window,
            wm_protocols,
            AtomEnum::ATOM,
            &[wm_delete_window],
        )?
        .check()?;
        if let Some(hints) = position_hints((window_x, window_y), accepted_position) {
            hints.set_normal_hints(&conn, window)?.check()?;
        }
        conn.map_window(window)?.check()?;
        conn.flush()?;

        let escape_keysym = escape_keysym_from_name(escape_key)?;
        let escape_keycodes = load_escape_keycodes(&conn, escape_keysym)?;
        if grab_input && escape_keycodes.is_empty() {
            return Err(invalid_data(format!(
                "--grab-input requires an unmodified {escape_key} key in the local X11 keymap; \
                     pick another with --escape-key"
            ))
            .into());
        }
        Ok(Self {
            conn,
            window,
            gc,
            backing,
            blank_cursor,
            cursor_plane: CursorPlane::default(),
            cursor_screen: screen_num,
            wm_protocols,
            wm_delete_window,
            net_wm_name,
            utf8_string,
            clipboard_enabled: false,
            clipboard_policy: "off".into(),
            clipboard_activity: None,
            depth: root_depth,
            pixel_layout,
            native_pixel_writer,
            native_image: None,
            native_patch: None,
            triangle_patch: TrianglePatch::default(),
            shm_upload,
            presentation_atom,
            presentations: VecDeque::new(),
            recovery: None,
            presentation_serial: 0,
            render_upscale,
            width: initial_width,
            height: initial_height,
            backing_valid: false,
            pending_resize: false,
            pending_redraw: false,
            backing_letterbox: None,
            grab_input,
            forward_input,
            source_switching: false,
            source_cycle_enabled: false,
            clipboard_narrowing_enabled: false,
            clipboard_shortcut: ViewerKeyboardState::default(),
            switching_label: None,
            source_label: None,
            presented_geometry: None,
            pointer_enabled,
            wayland_pointer: false,
            pointer_presented: None,
            pointer_status: None,
            forward_keyboard,
            verified_keymap,
            #[cfg(feature = "remote-wayland")]
            wayland_keymap: None,
            grabbed: false,
            escape_keycodes,
            escape_keysym,
            escape_name: escape_key.to_string(),
            peer_label: peer_label.to_string(),
            keyboard: ViewerKeyboardState::default(),
            held_buttons: ForwardedButtons::default(),
            deferred_events: VecDeque::new(),
            last_frame: None,
            closed: false,
            user_close_requested: false,
            user_released_grab: false,
            user_restricted_clipboard: false,
            close_reported: false,
        })
    }

    /// Explicit window closure, latched before fallible local input release.
    pub fn user_close_requested(&self) -> bool {
        self.user_close_requested
    }

    /// Sticky escape intent; releasing a grab is not session cancellation.
    pub fn user_released_grab(&self) -> bool {
        self.user_released_grab
    }

    /// Final local-only event pass after transport workers stop. Never forward
    /// these events to a replacement session. A backlog is ambiguous, so it
    /// disables retry instead of draining unboundedly through a close/escape.
    pub(crate) fn reconnect_allowed_after_cleanup(&mut self) -> RemoteResult<bool> {
        let events = self.poll_events();
        // Always try to release even when polling failed. Drop remains the
        // fallback on connection errors; such errors never authorize retry.
        let released = self.release_local_grab();
        let events = events?;
        released?;
        Ok(local_retry_intent_allows(
            &events,
            self.user_close_requested,
            self.user_released_grab,
            self.user_restricted_clipboard,
            self.has_pending_events()?,
        ))
    }

    /// Drain a bounded batch of X11 events without blocking.
    /// The caller sends returned input before servicing deferred rendering.
    ///
    /// With input grabbing enabled, the first button press grabs the local
    /// keyboard and pointer. Ctrl+Super plus the escape key releases both
    /// grabs and emits
    /// `ReleaseAll` so the remote host cannot retain a half-finished chord.
    #[cfg(feature = "remote-wayland")]
    pub(super) fn enable_wayland_keyboard(
        &mut self,
        display: Option<&str>,
    ) -> RemoteResult<[u8; 32]> {
        if self.pointer_enabled || self.grab_input || self.forward_keyboard {
            return Err(invalid_data(
                "Wayland keyboard lease cannot share legacy input permission",
            )
            .into());
        }
        let guard =
            super::keyboard_keymap::KeyboardKeymap::connect(display, &self.conn, self.window)?;
        let digest = guard.digest();
        self.wayland_keymap = Some(guard);
        self.forward_keyboard = true;
        self.forward_input = true;
        Ok(digest)
    }
    #[cfg(feature = "remote-wayland")]
    pub(super) fn check_wayland_keyboard(&mut self, initial: bool) -> RemoteResult<()> {
        self.wayland_keymap
            .as_mut()
            .ok_or_else(|| invalid_data("Wayland keyboard lease is disabled"))?
            .check(initial)
    }
    #[cfg(feature = "remote-wayland")]
    pub(super) fn disable_wayland_keyboard(&mut self) {
        self.wayland_keymap = None;
        self.forward_keyboard = false;
        self.forward_input = false;
        self.keyboard = ViewerKeyboardState::default();
    }

    pub(super) fn enable_wayland_pointer(&mut self) -> RemoteResult<()> {
        if self.pointer_enabled || self.forward_keyboard || self.grab_input {
            return Err(
                invalid_data("scoped pointer cannot share legacy input or keyboard").into(),
            );
        }
        self.wayland_pointer = true;
        self.set_wayland_pointer_status("Pointer: Waiting for authorization");
        Ok(())
    }
    pub(super) fn set_wayland_pointer_status(&mut self, status: &'static str) {
        if self.pointer_status != Some(status) {
            self.pointer_status = Some(status);
            self.update_title();
        }
    }
    pub(super) fn wayland_pointer_presented(&mut self, sequence: u64) {
        self.pointer_presented = Some(sequence);
    }
    fn scoped_pointer_event(&self, x: i16, y: i16, kind: WaylandPointerEventKind) -> ViewerEvent {
        let cancelled = match kind {
            WaylandPointerEventKind::Motion => ViewerEvent::WaylandPointerCancel,
            WaylandPointerEventKind::LeftDown => {
                ViewerEvent::WaylandPointerButtonCancel { pressed: true }
            }
            WaylandPointerEventKind::LeftUp => {
                ViewerEvent::WaylandPointerButtonCancel { pressed: false }
            }
        };
        let Some(frame) = self.last_frame.as_ref() else {
            return cancelled;
        };
        if self.pending_resize
            || self.pointer_presented != Some(frame.sequence)
            || self.presented_geometry != Some(frame.geometry_epoch)
            || !self.pointer_is_inside_image(x, y)
        {
            return cancelled;
        }
        let Some((sx, sy)) = map_scoped_pointer_to_source(
            x,
            y,
            self.width,
            self.height,
            frame.image().width() as u16,
            frame.image().height() as u16,
            frame.source_width,
            frame.source_height,
        ) else {
            return cancelled;
        };
        ViewerEvent::WaylandPointer {
            frame_sequence: frame.sequence,
            geometry_epoch: frame.geometry_epoch,
            x: sx,
            y: sy,
            local_x: x,
            local_y: y,
            kind,
        }
    }

    pub fn poll_events(&mut self) -> RemoteResult<Vec<ViewerEvent>> {
        let mut result = Vec::new();
        #[cfg(feature = "remote-wayland")]
        if let Some(guard) = self.wayland_keymap.as_mut() {
            guard.check(false)?;
        }
        if self.closed {
            self.report_close(&mut result)?;
            return Ok(result);
        }
        let mut redraw = false;
        let backing_dimensions = (self.width, self.height);

        let mut budget = EventPollBudget::new();
        while budget.take() {
            let event = if let Some(event) = self.deferred_events.pop_front() {
                event
            } else if let Some(event) = self.conn.poll_for_event()? {
                event
            } else {
                break;
            };
            if self.consume_presentation_event(&event)? {
                continue;
            }
            if let Event::KeyPress(key) = &event
                && (self.clipboard_shortcut.consume_repeat(key.detail, key.time)
                    || self.keyboard.consume_repeat(key.detail, key.time))
            {
                // Core X11 autorepeat is a synthetic release/press pair with
                // identical keycode and timestamp. Keep the remote key held
                // and let the host server generate its own repeat cadence.
                continue;
            }
            self.keyboard
                .flush_release(&mut result, self.forward_keyboard);
            self.clipboard_shortcut.flush_release(&mut result, false);
            match event {
                Event::Expose(event) if event.window == self.window => {
                    redraw |= event.count == 0;
                }
                Event::ConfigureNotify(event) if event.window == self.window => {
                    let (new_width, new_height) =
                        configured_viewer_dimensions(event.width, event.height)?;
                    if (new_width, new_height) != (self.width, self.height) {
                        self.width = new_width;
                        self.height = new_height;
                        redraw = true;
                    }
                }
                Event::ClientMessage(event)
                    if event.window == self.window
                        && event.format == 32
                        && event.type_ == self.wm_protocols
                        && event.data.as_data32()[0] == self.wm_delete_window =>
                {
                    self.user_close_requested = true;
                    self.closed = true;
                    self.report_close(&mut result)?;
                    break;
                }
                Event::DestroyNotify(event) if event.window == self.window => {
                    self.user_close_requested = true;
                    self.closed = true;
                    self.report_close(&mut result)?;
                    break;
                }
                Event::UnmapNotify(event) if event.window == self.window => {
                    self.clipboard_shortcut = ViewerKeyboardState::default();
                    self.release_local_grab()?;
                    self.release_all_input(&mut result);
                }
                Event::FocusOut(event)
                    if event.event == self.window
                        && focus_out_releases_input(event.mode, self.grabbed) =>
                {
                    self.clipboard_shortcut = ViewerKeyboardState::default();
                    self.release_local_grab()?;
                    self.release_all_input(&mut result);
                }
                Event::MotionNotify(event)
                    if event.event == self.window && self.wayland_pointer =>
                {
                    result.push(self.scoped_pointer_event(
                        event.event_x,
                        event.event_y,
                        WaylandPointerEventKind::Motion,
                    ));
                }
                Event::ButtonPress(event) if event.event == self.window && self.wayland_pointer => {
                    result.push(if event.detail == 1 {
                        self.scoped_pointer_event(
                            event.event_x,
                            event.event_y,
                            WaylandPointerEventKind::LeftDown,
                        )
                    } else {
                        ViewerEvent::WaylandPointerCancel
                    });
                }
                Event::ButtonRelease(event)
                    if event.event == self.window && self.wayland_pointer =>
                {
                    result.push(if event.detail == 1 {
                        self.scoped_pointer_event(
                            event.event_x,
                            event.event_y,
                            WaylandPointerEventKind::LeftUp,
                        )
                    } else {
                        ViewerEvent::WaylandPointerCancel
                    });
                }
                Event::MotionNotify(event)
                    if event.event == self.window && self.forwarding_input() =>
                {
                    if let Some((x, y)) = self.map_pointer(event.event_x, event.event_y) {
                        push_pointer(&mut result, x, y);
                    }
                }
                Event::ButtonPress(event) if event.event == self.window => {
                    let inside_image = self.pointer_is_inside_image(event.event_x, event.event_y);
                    if !inside_image {
                        continue;
                    }
                    if self.source_switching
                        || (self.grab_input && !self.grabbed && !self.acquire_local_grab()?)
                    {
                        continue;
                    }
                    if self.forwarding_input() {
                        if let Some((x, y)) = self.map_pointer(event.event_x, event.event_y) {
                            self.held_buttons.press(event.detail, inside_image);
                            push_pointer(&mut result, x, y);
                            result.push(ViewerEvent::Button {
                                button: event.detail,
                                pressed: true,
                            });
                        }
                    }
                }
                Event::ButtonRelease(event)
                    if event.event == self.window
                        && self.forwarding_input()
                        && self.held_buttons.release(event.detail) =>
                {
                    if let Some((x, y)) = self.map_pointer(event.event_x, event.event_y) {
                        push_pointer(&mut result, x, y);
                    }
                    // A drag can finish outside the image. Always release an
                    // edge we forwarded; a rejected black-bar click has none.
                    result.push(ViewerEvent::Button {
                        button: event.detail,
                        pressed: false,
                    });
                }
                Event::KeyPress(event)
                    if event.event == self.window
                        && (self.clipboard_shortcut.held.contains(&event.detail)
                            || (self.clipboard_narrowing_enabled
                                && self.is_escape(event.detail)
                                && clipboard_narrow_modifiers_match(event.state))) =>
                {
                    // Independent of forwarding permission and source geometry:
                    // view-only viewers can restrict clipboard sharing too.
                    if self.clipboard_shortcut.press(event.detail, true, false)
                        == KeyPressAction::ReleaseGrab
                    {
                        // Intent during teardown must not be lost and later
                        // reconnect using the original, broader clipboard grant.
                        self.user_restricted_clipboard = true;
                        self.release_all_input(&mut result);
                        result.push(ViewerEvent::NarrowClipboard);
                    }
                }
                Event::KeyRelease(event)
                    if event.event == self.window
                        && self.clipboard_shortcut.held.contains(&event.detail) =>
                {
                    // Keep consuming the edge after off/pending disables the
                    // shortcut, and distinguish a real release from autorepeat.
                    self.clipboard_shortcut
                        .defer_release(event.detail, event.time, Instant::now());
                }
                Event::KeyPress(event)
                    if event.event == self.window
                        && self.source_cycle_enabled
                        && self.is_escape(event.detail)
                        && source_cycle_modifiers_match(event.state) =>
                {
                    if !self.source_switching {
                        self.release_local_grab()?;
                        self.release_all_input(&mut result);
                        self.source_switching = true;
                        result.push(ViewerEvent::CycleSource);
                    }
                }
                Event::KeyPress(event) if event.event == self.window && self.forwarding_input() => {
                    // Local escape tracking is independent of permission to
                    // forward keyboard input. Super is X11's Mod4, as in JWM.
                    let escape = self.grab_input
                        && self.grabbed
                        && self.is_escape(event.detail)
                        && escape_modifiers_match(event.state);
                    match self
                        .keyboard
                        .press(event.detail, escape, self.forward_keyboard)
                    {
                        KeyPressAction::ReleaseGrab => {
                            self.user_released_grab = true;
                            self.release_local_grab()?;
                            self.release_all_input(&mut result);
                        }
                        KeyPressAction::Forward => result.push(ViewerEvent::Key {
                            keycode: event.detail,
                            pressed: true,
                        }),
                        KeyPressAction::Ignore => {}
                    }
                }
                Event::KeyRelease(event)
                    if event.event == self.window && self.forwarding_input() =>
                {
                    self.keyboard
                        .defer_release(event.detail, event.time, Instant::now());
                }
                Event::MappingNotify(event) if event.request != Mapping::POINTER => {
                    #[cfg(feature = "remote-wayland")]
                    if self.wayland_keymap.is_some() {
                        return Err(
                            invalid_data("local keyboard map changed; lease revoked").into()
                        );
                    }
                    if self.forward_keyboard {
                        let expected = self.verified_keymap.as_ref().ok_or_else(|| {
                            invalid_data("keyboard input has no verified X11 keymap")
                        })?;
                        // A spare keycode rebound by a typing tool is not
                        // one the host verified, so it does not end input.
                        let current = super::x11_keymap::KeymapTable::read(&self.conn)?;
                        if !current.still_covers(expected) {
                            return Err(invalid_data(
                                "local X11 keymap changed; reconnect to re-verify keyboard input",
                            )
                            .into());
                        }
                    }
                    let escape_keycodes = load_escape_keycodes(&self.conn, self.escape_keysym)?;
                    if self.grab_input && escape_keycodes.is_empty() {
                        self.release_local_grab()?;
                        self.release_all_input(&mut result);
                        return Err(invalid_data(format!(
                            "local X11 keymap no longer has an unmodified {} escape key",
                            self.escape_name
                        ))
                        .into());
                    }
                    self.escape_keycodes = escape_keycodes;
                }
                Event::Error(error) if is_closed_window_error(&error, self.window) => {
                    self.closed = true;
                    self.report_close(&mut result)?;
                    break;
                }
                Event::Error(error) => {
                    return Err(io::Error::other(format!(
                        "X11 viewer received a protocol error: {error:?}"
                    ))
                    .into());
                }
                _ => {}
            }
        }

        // At a batch boundary the next unread event may be the paired repeat
        // KeyPress. Give the next pass its normal lookahead before expiring a
        // release, even if scheduling/rendering took longer than the defer.
        if !budget.exhausted()
            && self
                .keyboard
                .pending_release
                .is_some_and(|release| release.queued_at.elapsed() >= KEY_RELEASE_DEFER)
        {
            self.keyboard
                .flush_release(&mut result, self.forward_keyboard);
        }

        if !budget.exhausted()
            && self
                .clipboard_shortcut
                .pending_release
                .is_some_and(|release| release.queued_at.elapsed() >= KEY_RELEASE_DEFER)
        {
            self.clipboard_shortcut.flush_release(&mut result, false);
        }

        if (self.width, self.height) != backing_dimensions && !self.closed {
            // Coalesce this batch, but do not allocate/render before its input
            // reaches the host. Continuous event floods yield at the budget.
            self.pending_resize = true;
        }
        self.pending_redraw |= redraw;
        if self.closed {
            self.report_close(&mut result)?;
        }
        Ok(result)
    }

    fn apply_pending_resize(&mut self) -> RemoteResult<()> {
        if self.pending_resize && !self.closed {
            self.resize_backing()?;
            self.pending_resize = false;
        }
        Ok(())
    }

    /// Service Expose/resize only after the client flushes collected input.
    /// A new frame instead folds this work into its own draw.
    pub(crate) fn refresh_pending_redraw(&mut self) -> RemoteResult<()> {
        if !self.can_submit_frame() {
            return Ok(());
        }
        self.apply_pending_resize()?;
        if self.pending_redraw && !self.closed {
            self.redraw()?;
            self.queue_presentation(None)?;
            self.pending_redraw = false;
        }
        Ok(())
    }

    /// Flush requests that can cause X11 events before the caller blocks on
    /// the connection file descriptor.
    pub fn prepare_wait(&self) -> RemoteResult<()> {
        self.conn.flush()?;
        Ok(())
    }

    /// Return whether an X11 event is already available without blocking.
    ///
    /// x11rb can read events into its internal queue while waiting for a
    /// checked request or an MIT-SHM completion. In that case the raw socket
    /// is no longer readable, so an external poller must inspect the library
    /// queue before sleeping. The event is put back into our FIFO so the
    /// normal event path (including asynchronous X11 errors) still owns it.
    pub fn has_pending_events(&mut self) -> RemoteResult<bool> {
        if self.closed || !self.deferred_events.is_empty() {
            return Ok(true);
        }
        if let Some(event) = self.conn.poll_for_event()? {
            self.deferred_events.push_back(event);
            return Ok(true);
        }
        Ok([&self.keyboard, &self.clipboard_shortcut]
            .into_iter()
            .any(|keyboard| {
                keyboard
                    .pending_release
                    .is_some_and(|release| release.queued_at.elapsed() >= KEY_RELEASE_DEFER)
            }))
    }

    /// File descriptor used by the event-driven client loop.
    #[must_use]
    pub fn connection_fd(&self) -> BorrowedFd<'_> {
        self.conn.stream().as_fd()
    }

    /// Earliest local input deadline that must wake the event loop even when
    /// the X server has no new event (currently the autorepeat release filter).
    #[must_use]
    pub fn next_event_deadline(&self) -> Option<Instant> {
        let key = self
            .keyboard
            .pending_release
            .and_then(|release| release.queued_at.checked_add(KEY_RELEASE_DEFER));
        let shortcut = self
            .clipboard_shortcut
            .pending_release
            .and_then(|release| release.queued_at.checked_add(KEY_RELEASE_DEFER));
        let presentation = self.presentations.front().map(|pending| pending.deadline);
        key.into_iter().chain(shortcut).chain(presentation).min()
    }

    /// The session enables this only for its originally negotiated clipboard.
    /// Disabling never drops a held shortcut's release/autorepeat tracking.
    pub(crate) fn enable_clipboard_narrowing(&mut self, enabled: bool) {
        self.clipboard_narrowing_enabled = enabled;
    }

    pub(crate) fn enable_source_cycle(&mut self, enabled: bool) {
        self.source_cycle_enabled = enabled;
    }

    pub(crate) fn set_source_label(&mut self, label: String) {
        self.source_label = Some(label);
        self.update_title();
    }

    pub(crate) fn begin_source_switch(&mut self, label: String) {
        self.source_switching = true;
        self.switching_label = Some(label);
        self.update_title();
    }

    pub(crate) fn source_presented(&mut self, geometry_epoch: u64) {
        self.presented_geometry = Some(geometry_epoch);
        let title_changed = self.source_switching || self.switching_label.is_some();
        self.source_switching = false;
        if let Some(label) = self.switching_label.take() {
            self.source_label = Some(label);
        }
        if title_changed {
            self.update_title();
        }
    }

    /// A native resize ends the old coordinate/input lifetime even before the
    /// replacement frame finishes its asynchronous X11 presentation.
    pub(crate) fn prepare_frame_geometry(&mut self, geometry_epoch: u64) -> RemoteResult<bool> {
        if !self
            .last_frame
            .as_ref()
            .is_some_and(|frame| frame.geometry_epoch != geometry_epoch)
        {
            return Ok(false);
        }
        self.release_local_grab()?;
        reset_input_geometry(
            &mut self.keyboard,
            &mut self.held_buttons,
            &mut self.presented_geometry,
        );
        Ok(true)
    }

    pub(crate) fn invalidate_source(&mut self, label: String) -> RemoteResult<()> {
        self.release_local_grab()?;
        self.keyboard = ViewerKeyboardState::default();
        self.held_buttons.clear();
        self.begin_source_switch(label);
        self.presented_geometry = None;
        // Never drop ownership of SHM slots or fences here. Obsolete copies
        // still drain through their real completion events, but earn no ACK.
        invalidate_presentations(&mut self.presentations);
        clear_stored_frame(&mut self.last_frame);
        self.pending_redraw = false;
        self.backing_valid = false;
        self.backing_letterbox = None;
        self.cursor_plane.shapes.clear();
        self.cursor_plane.position = None;
        self.cursor_plane.drawn = None;
        if let Some(render) = &mut self.cursor_plane.render
            && let Some(source) = render.source.take()
        {
            render::free_picture(&self.conn, source.picture)?;
            self.conn.free_pixmap(source.pixmap)?;
        }
        Ok(())
    }

    pub(crate) fn can_submit_frame(&self) -> bool {
        self.recovery.is_none() && self.presentations.len() < MAX_PENDING_PRESENTATIONS
    }

    /// Drain only server-processed copies whose shared source is no longer owned
    /// by X11. This is an X11 processing fence, not a physical scanout timestamp.
    pub(crate) fn take_presented_sequences(&mut self) -> RemoteResult<Vec<u64>> {
        let mut completed = Vec::new();
        if self.recovery.is_some() {
            if self
                .presentations
                .iter()
                .any(|pending| !pending.ready(&self.shm_upload))
            {
                self.check_presentation_deadline()?;
                return Ok(completed);
            }
            // All old copies have crossed the server fence and all successful
            // SHM requests have completed. Rejected requests relinquished only
            // their own slot. No accelerator-owned mapping is detached early.
            let sequence = self
                .presentations
                .iter()
                .rev()
                .find_map(|pending| pending.sequence);
            let recovery = self.recovery.take().unwrap();
            self.presentations.clear();
            if recovery.shm {
                self.shm_upload.disable(&self.conn);
            }
            if recovery.render {
                self.render_upscale.disable(
                    &self.conn,
                    &invalid_data("asynchronous XRender composite rejected").into(),
                );
            }
            self.backing_valid = false;
            self.backing_letterbox = None;
            if self.last_frame.is_some() {
                self.redraw()?;
                self.queue_presentation(sequence)?;
            }
            return Ok(completed);
        }
        while let Some(pending) = self.presentations.front() {
            if pending.ready(&self.shm_upload) {
                if let Some(sequence) = self.presentations.pop_front().unwrap().sequence {
                    completed.push(sequence);
                }
            } else {
                self.check_presentation_deadline()?;
                break;
            }
        }
        Ok(completed)
    }

    fn check_presentation_deadline(&self) -> RemoteResult<()> {
        if self
            .presentations
            .front()
            .is_some_and(|pending| Instant::now() >= pending.deadline)
        {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "X11 frame presentation did not complete",
            )
            .into());
        }
        Ok(())
    }

    fn consume_presentation_event(&mut self, event: &Event) -> RemoteResult<bool> {
        match event {
            Event::ShmCompletion(event) => {
                self.shm_upload.complete(event);
                Ok(true)
            }
            Event::ClientMessage(event)
                if event.window == self.window
                    && event.type_ == self.presentation_atom
                    && event.format == 32 =>
            {
                let data = event.data.as_data32();
                let serial = u64::from(data[0]) | (u64::from(data[1]) << 32);
                if let Some(pending) = self.presentations.iter_mut().find(|pending| {
                    pending.serial == serial && pending.request_sequence == event.sequence
                }) {
                    pending.copied = true;
                }
                Ok(true)
            }
            Event::Error(error) => {
                let shm = self.shm_upload.reject(error);
                let render = self.presentations.iter().any(|pending| {
                    pending
                        .render_request
                        .is_some_and(|request| request.matches(error))
                });
                if shm || render {
                    let recovery = self.recovery.get_or_insert(AccelerationRecovery::default());
                    recovery.shm |= shm;
                    // A rejected SHM source upload also invalidates Render's
                    // cached sequence. CPU/core recovery must not composite it.
                    recovery.render |= render || shm;
                    eprintln!(
                        "jwm-remote: asynchronous accelerator request rejected; retrying retained frame: {error:?}"
                    );
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            _ => Ok(false),
        }
    }

    fn queue_presentation(&mut self, sequence: Option<u64>) -> RemoteResult<()> {
        self.presentation_serial = self.presentation_serial.wrapping_add(1);
        let serial = self.presentation_serial;
        let event = ClientMessageEvent::new(
            32,
            self.window,
            self.presentation_atom,
            [serial as u32, (serial >> 32) as u32, 0, 0, 0],
        );
        let cookie = self
            .conn
            .send_event(false, self.window, EventMask::NO_EVENT, event)?;
        let request_sequence = cookie.sequence_number() as u16;
        drop(cookie);
        self.presentations.push_back(PendingPresentation {
            sequence,
            serial,
            request_sequence,
            copied: false,
            uploads: self.shm_upload.pending_keys(),
            render_request: self.render_upscale.take_request(),
            deadline: Instant::now() + PRESENTATION_TIMEOUT,
        });
        self.conn.flush()?;
        Ok(())
    }

    /// Draw and retain a decoded frame, waiting for server processing as before.
    /// Input/window events remain queued for the next poll_events call. The
    /// internal client uses draw_recyclable for nonblocking presentation instead.
    pub fn draw(&mut self, frame: DecodedFrame) -> RemoteResult<bool> {
        if !self.draw_stored(StoredFrame::from_decoded(frame))? {
            return Ok(false);
        }
        let deadline = Instant::now() + PRESENTATION_TIMEOUT;
        let mut deferred = VecDeque::new();
        let result = (|| {
            loop {
                if let Some(event) = if let Some(event) = self.deferred_events.pop_front() {
                    Some(event)
                } else {
                    self.conn.poll_for_event()?
                } {
                    if !self.consume_presentation_event(&event)? {
                        if let Event::Error(error) = &event {
                            if is_closed_window_error(error, self.window) {
                                self.closed = true;
                                return Ok(false);
                            }
                            return Err(ReplyError::X11Error(error.clone()).into());
                        }
                        // Bound public synchronous-call event buffering even if
                        // another client floods input while the server stalls.
                        if deferred.len() >= 4096 {
                            return Err(
                                invalid_data("X11 event flood during synchronous draw").into()
                            );
                        }
                        deferred.push_back(event);
                    }
                } else {
                    std::thread::sleep(Duration::from_millis(1));
                }
                self.take_presented_sequences()?;
                if self.presentations.is_empty() {
                    if let Some(frame) = self.last_frame.as_ref() {
                        self.presented_geometry = Some(frame.geometry_epoch);
                    }
                    return Ok(!self.closed);
                }
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "X11 synchronous draw did not complete",
                    )
                    .into());
                }
            }
        })();
        deferred.append(&mut self.deferred_events);
        self.deferred_events = deferred;
        result
    }

    /// Internal client path that keeps the decoded RGB allocation leased while
    /// the frame remains the viewer's Expose/resize source.
    pub(crate) fn draw_recyclable(
        &mut self,
        mut frame: RecyclableDecodedFrame,
    ) -> RemoteResult<bool> {
        if let Some(StoredFrame {
            image: StoredImage::Recyclable(previous),
            ..
        }) = self.last_frame.as_mut()
        {
            frame.reuse_canvas_from(previous);
        }
        self.draw_stored(StoredFrame::from_recyclable(frame))
    }

    fn draw_stored(&mut self, frame: StoredFrame) -> RemoteResult<bool> {
        if self.closed {
            clear_stored_frame(&mut self.last_frame);
            return Ok(false);
        }
        if !self.can_submit_frame() {
            return Err(invalid_data("X11 presentation queue is full").into());
        }
        let sequence = frame.sequence;
        validate_frame(&frame)?;
        self.apply_pending_resize()?;
        // A mailbox can skip decoded snapshots. Only apply damage when its
        // exact predecessor is already displayed; otherwise the full image
        // includes every missed delta and safely restores convergence.
        let incremental = self.try_incremental_frame(&frame)?;
        replace_stored_frame(&mut self.last_frame, frame);
        if !incremental {
            self.backing_valid = false;
        }
        if let Err(error) = self.redraw() {
            clear_stored_frame(&mut self.last_frame);
            return Err(error);
        }
        self.pending_redraw = false;
        if self.closed {
            clear_stored_frame(&mut self.last_frame);
        }
        if !self.closed {
            self.queue_presentation(Some(sequence))?;
        }
        Ok(!self.closed)
    }

    /// Patch retained backing (or the XRender source) only when its exact
    /// predecessor and geometry match. SHM ownership and copy fences remain
    /// unchanged: every patch is ordered before the composite/copy and ACK.
    fn try_incremental_frame(&mut self, frame: &StoredFrame) -> RemoteResult<bool> {
        let Some(previous) = self.last_frame.as_ref() else {
            return Ok(false);
        };
        let Some(damage) = frame.damage() else {
            return Ok(false);
        };
        if !self.backing_valid || !damage_base_matches(previous, frame, damage) {
            return Ok(false);
        }
        let (w, h) = frame.dimensions();
        let Some(geometry) = letterbox(self.width, self.height, w as u16, h as u16) else {
            return Ok(false);
        };
        if self.backing_letterbox != Some(geometry) {
            return Ok(false);
        }
        // An accelerator source is a separate cache from the clean backing.
        // Never patch it using damage based on a different source sequence.
        let render_enabled = matches!(self.render_upscale, RenderUpscale::Enabled(_));
        let render_source = match &mut self.render_upscale {
            RenderUpscale::Enabled(render)
                if should_use_render_upscale(geometry, w as u16, h as u16) =>
            {
                render.source.as_mut().filter(|source| {
                    render_source_accepts_damage(source.key, damage, (w as u16, h as u16))
                })
            }
            _ => None,
        };
        if should_use_render_upscale(geometry, w as u16, h as u16)
            && render_enabled
            && render_source.is_none()
        {
            return Ok(false);
        }
        let source_patch = render_source.is_some();
        let mut regions = DamageRegions::default();
        let dimensions = if source_patch {
            (w as u16, h as u16)
        } else {
            (geometry.width, geometry.height)
        };
        for &rect in damage.regions.as_slice() {
            regions.add(
                if source_patch {
                    rect
                } else {
                    scaled_damage_rect(rect, (w as u16, h as u16), dimensions)
                },
                dimensions,
            );
        }
        // Preflight all CPU patches before uploading anything. Full-region or
        // over-budget damage retains the existing one-shot full resize.
        let partial_triangle = !source_patch
            && (w, h) != (u32::from(geometry.width), u32::from(geometry.height))
            && self
                .triangle_patch
                .prepare((w, h), dimensions, regions.as_slice());
        let rgb = if regions.as_slice().is_empty() || partial_triangle {
            None
        } else if source_patch || (w, h) == (u32::from(geometry.width), u32::from(geometry.height))
        {
            Some(Cow::Borrowed(frame.image()))
        } else {
            Some(Cow::Owned(image::imageops::resize(
                frame.image(),
                u32::from(geometry.width),
                u32::from(geometry.height),
                image::imageops::FilterType::Triangle,
            )))
        };
        for &(x, y, width, height) in regions.as_slice() {
            let image = ensure_native_image(
                &mut self.native_patch,
                width,
                height,
                self.depth,
                self.conn.setup(),
            )?;
            let (pixels, source_x, source_y) = if partial_triangle {
                (
                    self.triangle_patch
                        .render(frame.image(), dimensions, (x, y, width, height)),
                    0,
                    0,
                )
            } else {
                (
                    rgb.as_ref().expect("nonempty damage has pixels").as_ref(),
                    x,
                    y,
                )
            };
            write_native_region(
                image,
                pixels,
                source_x,
                source_y,
                self.pixel_layout,
                self.native_pixel_writer,
            )?;
            let (drawable, offset_x, offset_y) = match render_source.as_ref() {
                Some(source) => (source.pixmap, 0, 0),
                None => (self.backing, geometry.x, geometry.y),
            };
            let dst_x = i16::try_from(u32::from(offset_x) + u32::from(x))
                .map_err(|_| invalid_data("damage X offset exceeds X11 range"))?;
            let dst_y = i16::try_from(u32::from(offset_y) + u32::from(y))
                .map_err(|_| invalid_data("damage Y offset exceeds X11 range"))?;
            self.shm_upload.upload_or_core(
                &self.conn,
                image,
                drawable,
                self.gc,
                dst_x,
                dst_y,
                &mut self.deferred_events,
            )?;
        }
        if let Some(source) = render_source {
            source.key.sequence = frame.sequence;
            // Let populate_backing recomposite the now-current cached source.
            // This also preserves the existing same-frame Render fallback.
            return Ok(false);
        }
        Ok(true)
    }

    /// Current viewable size in pixels.
    ///
    /// The host uses this to stop encoding detail the window cannot show.
    #[must_use]
    pub fn viewport(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    /// Keep the visible label tied to the negotiated clipboard capability.
    pub fn set_clipboard_status(&mut self, enabled: bool) {
        self.clipboard_enabled = enabled;
        self.update_title();
    }

    pub fn set_clipboard_policy(&mut self, policy: String) {
        self.clipboard_policy = policy;
        self.update_title();
    }

    /// Show a copy travelling or just arrived, such as `receiving 3 files 40%`.
    pub fn set_clipboard_activity(&mut self, activity: Option<String>) {
        if self.clipboard_activity != activity {
            self.clipboard_activity = activity;
            self.update_title();
        }
    }

    // Decoration must never interrupt input release or turn a successful grab
    // into a half-failed operation. Transport/input errors keep their own path.
    fn update_title(&self) {
        if let Err(error) = self.try_update_title() {
            eprintln!("jwm-remote: could not refresh the status title: {error}");
        }
    }

    fn try_update_title(&self) -> RemoteResult<()> {
        let mut title = viewer_title(
            &self.peer_label,
            self.pointer_enabled,
            self.forward_keyboard,
            self.grab_input,
            self.grabbed,
            self.clipboard_enabled,
            self.clipboard_activity.as_deref(),
            &self.escape_name,
        );
        if let Some(status) = self.pointer_status {
            title.push_str("; ");
            title.push_str(status);
        }
        if self.clipboard_enabled {
            title.push_str("; sharing ");
            title.push_str(&self.clipboard_policy);
        }
        append_source_title(
            &mut title,
            self.source_label.as_deref(),
            self.switching_label.as_deref(),
        );
        self.conn
            .change_property8(
                PropMode::REPLACE,
                self.window,
                AtomEnum::WM_NAME,
                AtomEnum::STRING,
                title.as_bytes(),
            )?
            .check()?;
        self.conn
            .change_property8(
                PropMode::REPLACE,
                self.window,
                self.net_wm_name,
                self.utf8_string,
                title.as_bytes(),
            )?
            .check()?;
        self.conn.flush()?;
        Ok(())
    }

    /// Whether `keycode` currently resolves to the escape keysym here.
    #[must_use]
    pub fn is_escape(&self, keycode: u8) -> bool {
        self.escape_keycodes.contains(&keycode)
    }

    fn forwarding_input(&self) -> bool {
        self.forward_input
            && !self.source_switching
            && self
                .last_frame
                .as_ref()
                .is_some_and(|frame| self.presented_geometry == Some(frame.geometry_epoch))
            && (!self.grab_input || self.grabbed)
    }

    fn acquire_local_grab(&mut self) -> RemoteResult<bool> {
        if self.grabbed {
            return Ok(true);
        }

        let keyboard = self
            .conn
            .grab_keyboard(
                false,
                self.window,
                CURRENT_TIME,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            )?
            .reply()?;
        if keyboard.status != GrabStatus::SUCCESS {
            eprintln!(
                "remote: could not grab viewer keyboard ({:?})",
                keyboard.status
            );
            return Ok(false);
        }

        let pointer_reply: RemoteResult<_> = (|| {
            Ok(self
                .conn
                .grab_pointer(
                    false,
                    self.window,
                    EventMask::POINTER_MOTION | EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE,
                    GrabMode::ASYNC,
                    GrabMode::ASYNC,
                    x11rb::NONE,
                    self.blank_cursor,
                    CURRENT_TIME,
                )?
                .reply()?)
        })();
        let pointer = match pointer_reply {
            Ok(reply) => reply,
            Err(error) => {
                let _ = self.conn.ungrab_keyboard(CURRENT_TIME);
                let _ = self.conn.flush();
                return Err(error);
            }
        };
        if pointer.status != GrabStatus::SUCCESS {
            let _ = self.conn.ungrab_keyboard(CURRENT_TIME);
            let _ = self.conn.flush();
            eprintln!(
                "remote: could not grab viewer pointer ({:?})",
                pointer.status
            );
            return Ok(false);
        }

        self.grabbed = true;
        self.update_title();
        self.conn.flush()?;
        Ok(true)
    }

    fn release_local_grab(&mut self) -> RemoteResult<()> {
        if !self.grabbed {
            return Ok(());
        }

        // Queue both requests before checking either one, so an error from one
        // device cannot leave the other device grabbed.
        let keyboard = self.conn.ungrab_keyboard(CURRENT_TIME)?;
        let pointer = self.conn.ungrab_pointer(CURRENT_TIME)?;
        let keyboard_result = keyboard.check();
        let pointer_result = pointer.check();
        self.grabbed = false;
        keyboard_result?;
        pointer_result?;
        self.update_title();
        self.conn.flush()?;
        Ok(())
    }

    fn release_for_close(&mut self, events: &mut Vec<ViewerEvent>) -> RemoteResult<()> {
        self.release_local_grab()?;
        self.release_all_input(events);
        Ok(())
    }

    fn release_all_input(&mut self, events: &mut Vec<ViewerEvent>) {
        if self.wayland_pointer {
            events.push(ViewerEvent::WaylandPointerRevoke);
        }
        clear_forwarded_input(&mut self.keyboard, &mut self.held_buttons, events);
    }

    fn pointer_is_inside_image(&self, x: i16, y: i16) -> bool {
        let Some(frame) = self.last_frame.as_ref() else {
            return false;
        };
        let Ok(width) = u16::try_from(frame.image().width()) else {
            return false;
        };
        let Ok(height) = u16::try_from(frame.image().height()) else {
            return false;
        };
        letterbox(self.width, self.height, width, height)
            .is_some_and(|geometry| pointer_inside_letterbox(x, y, geometry))
    }

    fn map_pointer(&self, x: i16, y: i16) -> Option<(u16, u16)> {
        let frame = self.last_frame.as_ref()?;
        let image_width = u16::try_from(frame.image().width()).ok()?;
        let image_height = u16::try_from(frame.image().height()).ok()?;
        map_pointer_to_source(
            x,
            y,
            self.width,
            self.height,
            image_width,
            image_height,
            frame.source_width,
            frame.source_height,
        )
    }

    fn resize_backing(&mut self) -> RemoteResult<()> {
        validate_viewer_dimensions(u32::from(self.width), u32::from(self.height))?;
        let replacement = self.conn.generate_id()?;
        let create_result = self
            .conn
            .create_pixmap(
                self.depth,
                replacement,
                self.window,
                self.width,
                self.height,
            )?
            .check();
        self.finish_window_request(create_result)?;
        if self.closed {
            return Ok(());
        }
        if let Err(error) = self.render_upscale.replace_backing(&self.conn, replacement) {
            // A fatal connection error leaves it unknowable whether a new
            // Picture wraps `replacement`; connection teardown owns cleanup.
            return Err(error);
        }
        let previous = std::mem::replace(&mut self.backing, replacement);
        self.backing_valid = false;
        // A fresh pixmap has undefined contents; the bars are no longer drawn.
        self.backing_letterbox = None;
        // RenderUpscale::replace_backing has already freed the Picture that
        // wrapped `previous`, so the drawable can now be released safely.
        self.conn.free_pixmap(previous)?.check()?;
        Ok(())
    }

    fn redraw(&mut self) -> RemoteResult<()> {
        if self.closed {
            return Ok(());
        }
        if backing_work(self.backing_valid) == BackingWork::CopyOnly {
            return self.copy_backing();
        }

        self.populate_backing()?;
        self.copy_backing()
    }

    fn populate_backing(&mut self) -> RemoteResult<()> {
        if let Some(frame) = self.last_frame.as_ref() {
            let image_width = u16::try_from(frame.image().width())
                .map_err(|_| invalid_data("decoded frame width exceeds the X11 protocol range"))?;
            let image_height = u16::try_from(frame.image().height())
                .map_err(|_| invalid_data("decoded frame height exceeds the X11 protocol range"))?;
            let geometry = letterbox(self.width, self.height, image_width, image_height)
                .ok_or_else(|| invalid_data("cannot draw an empty remote frame"))?;
            let mut render_failure = None;

            if should_use_render_upscale(geometry, image_width, image_height) {
                let image = if self.render_upscale.source_needs_upload(
                    frame.sequence,
                    image_width,
                    image_height,
                ) {
                    let image = ensure_native_image(
                        &mut self.native_image,
                        image_width,
                        image_height,
                        self.depth,
                        self.conn.setup(),
                    )?;
                    write_native_pixels(
                        image,
                        frame.image(),
                        self.pixel_layout,
                        self.native_pixel_writer,
                    )?;
                    Some(&*image)
                } else {
                    None
                };
                match self.render_upscale.prepare_source(
                    &self.conn,
                    &mut self.shm_upload,
                    image,
                    self.gc,
                    frame.sequence,
                    image_width,
                    image_height,
                    &mut self.deferred_events,
                )? {
                    RenderProgress::Ready => {
                        clear_backing_if_moved(
                            &self.conn,
                            self.backing,
                            self.gc,
                            self.width,
                            self.height,
                            &mut self.backing_letterbox,
                            geometry,
                        )?;
                        match self.render_upscale.composite(
                            &self.conn,
                            geometry,
                            image_width,
                            image_height,
                        )? {
                            RenderProgress::Ready => {
                                self.conn.flush()?;
                                self.backing_valid = true;
                                return Ok(());
                            }
                            RenderProgress::Recoverable(error) => {
                                render_failure = Some(error);
                            }
                            RenderProgress::Unavailable => {}
                        }
                    }
                    RenderProgress::Recoverable(error) => render_failure = Some(error),
                    RenderProgress::Unavailable => {}
                }
            }

            let resized = if (geometry.width, geometry.height) == (image_width, image_height) {
                Cow::Borrowed(frame.image())
            } else {
                Cow::Owned(image::imageops::resize(
                    frame.image(),
                    u32::from(geometry.width),
                    u32::from(geometry.height),
                    image::imageops::FilterType::Triangle,
                ))
            };
            let image = ensure_native_image(
                &mut self.native_image,
                geometry.width,
                geometry.height,
                self.depth,
                self.conn.setup(),
            )?;
            write_native_pixels(
                image,
                resized.as_ref(),
                self.pixel_layout,
                self.native_pixel_writer,
            )?;
            let dst_x = i16::try_from(geometry.x)
                .map_err(|_| invalid_data("letterbox X offset exceeds the X11 coordinate range"))?;
            let dst_y = i16::try_from(geometry.y)
                .map_err(|_| invalid_data("letterbox Y offset exceeds the X11 coordinate range"))?;
            clear_backing_if_moved(
                &self.conn,
                self.backing,
                self.gc,
                self.width,
                self.height,
                &mut self.backing_letterbox,
                geometry,
            )?;
            let cpu_result = self.shm_upload.upload_or_core(
                &self.conn,
                image,
                self.backing,
                self.gc,
                dst_x,
                dst_y,
                &mut self.deferred_events,
            );
            // Commit the same-frame CPU fallback before disabling Render. If
            // core presentation fails, preserve Render state and report the
            // core error instead of misdiagnosing the connection.
            finish_render_fallback(cpu_result, render_failure, |error| {
                self.render_upscale.disable(&self.conn, error);
            })?;
        } else {
            fill_backing(&self.conn, self.backing, self.gc, self.width, self.height)?;
            self.backing_letterbox = None;
        }
        // Upload and CopyArea requests share one ordered connection. Errors
        // are handled before the presentation fence, so failed frames cannot ACK.
        self.conn.flush()?;
        self.backing_valid = true;
        Ok(())
    }

    /// Enable an authoritative remote cursor, including in view-only sessions.
    pub(crate) fn enable_cursor_plane(&mut self) -> RemoteResult<()> {
        if self.cursor_plane.enabled {
            return Ok(());
        }
        let (format, target_format) =
            cursor_formats(&self.conn, self.cursor_screen)?.ok_or_else(|| {
                invalid_data("negotiated cursor plane requires XRender ARGB32 support")
            })?;
        let target = self.conn.generate_id()?;
        render::create_picture(
            &self.conn,
            target,
            self.window,
            target_format,
            &CreatePictureAux::new(),
        )?
        .check()?;
        self.cursor_plane.render = Some(CursorRender {
            format,
            target,
            source: None,
        });
        self.conn
            .change_window_attributes(
                self.window,
                &x11rb::protocol::xproto::ChangeWindowAttributesAux::new()
                    .cursor(self.blank_cursor),
            )?
            .check()?;
        self.cursor_plane.enabled = true;
        self.refresh_cursor()
    }

    pub(crate) fn set_cursor_update(
        &mut self,
        shape: Arc<CursorShape>,
        position: CursorPosition,
    ) -> RemoteResult<()> {
        if shape.width == 0
            || shape.height == 0
            || shape.width > 256
            || shape.height > 256
            || shape.hotspot_x >= shape.width
            || shape.hotspot_y >= shape.height
            || shape.rgba.len() != usize::from(shape.width) * usize::from(shape.height) * 4
            || position.shape_id != shape.id
        {
            return Err(invalid_data("invalid remote cursor shape").into());
        }
        if self.cursor_plane.accept_position(position) {
            self.cursor_plane.insert_shape(shape);
            self.refresh_cursor()?;
        }
        Ok(())
    }

    fn refresh_cursor(&mut self) -> RemoteResult<()> {
        if self.closed
            || !self.cursor_plane.enabled
            || !self.backing_valid
            || self.pending_resize
            || self.recovery.is_some()
        {
            return Ok(());
        }
        if let Some(rect) = self.cursor_plane.drawn.take() {
            drop(self.conn.copy_area(
                self.backing,
                self.window,
                self.gc,
                rect.x as i16,
                rect.y as i16,
                rect.x as i16,
                rect.y as i16,
                rect.width,
                rect.height,
            )?);
        }
        self.paint_cursor()?;
        self.conn.flush()?;
        Ok(())
    }

    /// Queue an independent OVER operation after the clean backing copy. Cursor
    /// motion never reads back pixels, uploads a frame, or waits for a reply.
    /// The server processes source upload, transform and composite in order.
    fn paint_cursor(&mut self) -> RemoteResult<()> {
        if !self.cursor_plane.enabled || self.recovery.is_some() {
            return Ok(());
        }
        let Some(frame) = self.last_frame.as_ref() else {
            return Ok(());
        };
        let Some(position) = self.cursor_plane.position.as_ref() else {
            return Ok(());
        };
        let Some(shape) = self
            .cursor_plane
            .shapes
            .iter()
            .find(|s| s.id == position.shape_id)
        else {
            return Ok(());
        };
        let Some(geometry) = self.backing_letterbox else {
            return Ok(());
        };
        let Some(placement) = cursor_placement(
            position,
            shape,
            frame.geometry_epoch,
            (frame.source_width, frame.source_height),
            geometry,
        ) else {
            return Ok(());
        };
        let renderer = self
            .cursor_plane
            .render
            .as_mut()
            .ok_or_else(|| invalid_data("negotiated cursor renderer is missing"))?;
        renderer.prepare_shape(&self.conn, self.window, shape)?;
        let source = renderer.source.as_ref().unwrap();
        let transform = cursor_transform(&placement)?;
        drop(render::set_picture_transform(
            &self.conn,
            source.picture,
            transform,
        )?);
        let rect = placement.clip;
        drop(render::composite(
            &self.conn,
            PictOp::OVER,
            source.picture,
            x11rb::NONE,
            renderer.target,
            0,
            0,
            0,
            0,
            rect.x as i16,
            rect.y as i16,
            rect.width,
            rect.height,
        )?);
        self.cursor_plane.drawn = Some(rect);
        Ok(())
    }

    fn copy_backing(&mut self) -> RemoteResult<()> {
        let copy = self.conn.copy_area(
            self.backing,
            self.window,
            self.gc,
            0,
            0,
            0,
            0,
            self.width,
            self.height,
        )?;
        drop(copy); // Keep input polling live; asynchronous errors precede the presentation fence.
        self.cursor_plane.drawn = None;
        self.paint_cursor()?;
        if self.closed {
            return Ok(());
        }
        self.conn.flush()?;
        Ok(())
    }

    fn finish_window_request(&mut self, result: Result<(), ReplyError>) -> RemoteResult<()> {
        match result {
            Ok(()) => Ok(()),
            Err(ReplyError::X11Error(error)) if is_closed_window_error(&error, self.window) => {
                self.closed = true;
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }

    fn report_close(&mut self, events: &mut Vec<ViewerEvent>) -> RemoteResult<()> {
        // The closed window can no longer need its Expose source. Return a
        // recyclable RGB allocation even if input-release cleanup then fails.
        clear_stored_frame(&mut self.last_frame);
        if self.close_reported {
            return Ok(());
        }
        self.release_for_close(events)?;
        events.push(ViewerEvent::Close);
        self.close_reported = true;
        Ok(())
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        if let Some(renderer) = self.cursor_plane.render.as_mut() {
            renderer.release(&self.conn);
        }
        self.render_upscale.release(&self.conn);
        self.shm_upload.release(&self.conn);
        if self.grabbed {
            let _ = self.conn.ungrab_keyboard(CURRENT_TIME);
            let _ = self.conn.ungrab_pointer(CURRENT_TIME);
        }
        let _ = self.conn.free_cursor(self.blank_cursor);
        let _ = self.conn.flush();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RenderSourceKey {
    sequence: u64,
    width: u16,
    height: u16,
}

struct RenderSource {
    key: RenderSourceKey,
    pixmap: Pixmap,
    picture: Picture,
    transform: Option<(u16, u16)>,
}

struct EnabledRenderUpscale {
    major_opcode: u8,
    pending_request: Option<AcceleratorRequest>,
    root: Window,
    depth: u8,
    format: Pictformat,
    target: Option<Picture>,
    source: Option<RenderSource>,
    reported_active: bool,
}

enum RenderUpscale {
    Disabled,
    Enabled(EnabledRenderUpscale),
}

enum RenderFailure {
    Recoverable(RemoteError),
    Fatal(RemoteError),
}

enum RenderProgress {
    Ready,
    Unavailable,
    Recoverable(RemoteError),
}

impl RenderFailure {
    fn from_connection(error: ConnectionError) -> Self {
        Self::Fatal(error.into())
    }

    fn from_reply(error: ReplyError) -> Self {
        match error {
            ReplyError::X11Error(error) => Self::Recoverable(ReplyError::X11Error(error).into()),
            ReplyError::ConnectionError(error) => Self::Fatal(error.into()),
        }
    }

    fn from_id(error: ReplyOrIdError) -> Self {
        match error {
            ReplyOrIdError::IdsExhausted => Self::Recoverable(error.into()),
            ReplyOrIdError::X11Error(error) => {
                Self::Recoverable(ReplyOrIdError::X11Error(error).into())
            }
            ReplyOrIdError::ConnectionError(error) => Self::Fatal(error.into()),
        }
    }

    fn from_remote(error: RemoteError) -> Self {
        let error = match error.downcast::<ReplyError>() {
            Ok(error) => return Self::from_reply(*error),
            Err(error) => error,
        };
        let error = match error.downcast::<ReplyOrIdError>() {
            Ok(error) => return Self::from_id(*error),
            Err(error) => error,
        };
        match error.downcast::<ConnectionError>() {
            Ok(error) => Self::from_connection(*error),
            Err(error) => Self::Recoverable(error),
        }
    }
}

impl RenderUpscale {
    fn take_request(&mut self) -> Option<AcceleratorRequest> {
        match self {
            Self::Disabled => None,
            Self::Enabled(upscale) => upscale.pending_request.take(),
        }
    }
    fn connect(
        conn: &RustConnection,
        screen_num: usize,
        root: Window,
        depth: u8,
        root_visual: u32,
    ) -> RemoteResult<Self> {
        let Some(extension) = conn.extension_information(render::X11_EXTENSION_NAME)? else {
            eprintln!("jwm-remote: XRender unavailable for viewer upscaling; using CPU scaling");
            return Ok(Self::Disabled);
        };

        let version =
            match render::query_version(conn, RENDER_CLIENT_VERSION.0, RENDER_CLIENT_VERSION.1)?
                .reply()
            {
                Ok(version) => version,
                Err(ReplyError::X11Error(error)) => {
                    eprintln!(
                        "jwm-remote: XRender version query rejected ({error:?}); using CPU scaling"
                    );
                    return Ok(Self::Disabled);
                }
                Err(ReplyError::ConnectionError(error)) => return Err(error.into()),
            };
        if !supports_render_version(version.major_version, version.minor_version) {
            eprintln!(
                "jwm-remote: XRender {}.{} lacks 0.10 picture transforms; using CPU scaling",
                version.major_version, version.minor_version
            );
            return Ok(Self::Disabled);
        }

        let formats = match render::query_pict_formats(conn)?.reply() {
            Ok(formats) => formats,
            Err(ReplyError::X11Error(error)) => {
                eprintln!(
                    "jwm-remote: XRender format query rejected ({error:?}); using CPU scaling"
                );
                return Ok(Self::Disabled);
            }
            Err(ReplyError::ConnectionError(error)) => return Err(error.into()),
        };
        let format = formats.screens.get(screen_num).and_then(|screen| {
            screen
                .depths
                .iter()
                .filter(|candidate| candidate.depth == depth)
                .flat_map(|candidate| candidate.visuals.iter())
                .find_map(|candidate| (candidate.visual == root_visual).then_some(candidate.format))
        });
        let Some(format) = format else {
            eprintln!("jwm-remote: XRender has no root visual format; using CPU scaling");
            return Ok(Self::Disabled);
        };

        Ok(Self::Enabled(EnabledRenderUpscale {
            major_opcode: extension.major_opcode,
            pending_request: None,
            root,
            depth,
            format,
            target: None,
            source: None,
            reported_active: false,
        }))
    }

    fn attach_backing(&mut self, conn: &RustConnection, backing: Pixmap) -> RemoteResult<()> {
        let attempt = match self {
            Self::Disabled => return Ok(()),
            Self::Enabled(upscale) => upscale.create_target(conn, backing),
        };
        match attempt {
            Ok(target) => {
                if let Self::Enabled(upscale) = self {
                    upscale.target = Some(target);
                }
                Ok(())
            }
            Err(RenderFailure::Recoverable(error)) => {
                self.disable(conn, &error);
                Ok(())
            }
            Err(RenderFailure::Fatal(error)) => Err(error),
        }
    }

    /// Point the destination picture at `backing`, releasing the picture for
    /// the old Viewer backing before its pixmap can be freed by the caller.
    fn replace_backing(&mut self, conn: &RustConnection, backing: Pixmap) -> RemoteResult<()> {
        let attempt = match self {
            Self::Disabled => return Ok(()),
            Self::Enabled(upscale) => upscale.create_target(conn, backing),
        };
        let target = match attempt {
            Ok(target) => target,
            Err(RenderFailure::Recoverable(error)) => {
                self.disable(conn, &error);
                return Ok(());
            }
            Err(RenderFailure::Fatal(error)) => return Err(error),
        };

        let old = match self {
            Self::Disabled => return Ok(()),
            Self::Enabled(upscale) => upscale.target.replace(target),
        };
        if let Some(old) = old
            && let Err(error) = free_render_picture(conn, old)
        {
            return self.recover_or_fail(conn, error);
        }
        Ok(())
    }

    fn source_needs_upload(&self, sequence: u64, width: u16, height: u16) -> bool {
        let key = RenderSourceKey {
            sequence,
            width,
            height,
        };
        match self {
            Self::Disabled => false,
            Self::Enabled(upscale) => {
                render_source_needs_upload(upscale.source.as_ref().map(|source| source.key), key)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_source(
        &mut self,
        conn: &RustConnection,
        upload: &mut ShmUpload,
        image: Option<&XImage<'_>>,
        gc: Gcontext,
        sequence: u64,
        width: u16,
        height: u16,
        deferred_events: &mut VecDeque<Event>,
    ) -> RemoteResult<RenderProgress> {
        let key = RenderSourceKey {
            sequence,
            width,
            height,
        };
        let attempt = match self {
            Self::Disabled => return Ok(RenderProgress::Unavailable),
            Self::Enabled(upscale) => {
                upscale.ensure_source(conn, upload, image, gc, key, deferred_events)
            }
        };
        match attempt {
            Ok(()) => Ok(RenderProgress::Ready),
            Err(RenderFailure::Recoverable(error)) => Ok(RenderProgress::Recoverable(error)),
            Err(RenderFailure::Fatal(error)) => Err(error),
        }
    }

    fn composite(
        &mut self,
        conn: &RustConnection,
        geometry: Letterbox,
        source_width: u16,
        source_height: u16,
    ) -> RemoteResult<RenderProgress> {
        let attempt = match self {
            Self::Disabled => return Ok(RenderProgress::Unavailable),
            Self::Enabled(upscale) => {
                upscale.composite(conn, geometry, source_width, source_height)
            }
        };
        match attempt {
            Ok(()) => Ok(RenderProgress::Ready),
            Err(RenderFailure::Recoverable(error)) => Ok(RenderProgress::Recoverable(error)),
            Err(RenderFailure::Fatal(error)) => Err(error),
        }
    }

    fn recover_or_fail(&mut self, conn: &RustConnection, error: RenderFailure) -> RemoteResult<()> {
        match error {
            RenderFailure::Recoverable(error) => {
                self.disable(conn, &error);
                Ok(())
            }
            RenderFailure::Fatal(error) => Err(error),
        }
    }

    fn disable(&mut self, conn: &RustConnection, error: &RemoteError) {
        eprintln!("jwm-remote: XRender viewer upscaling stopped ({error}); using CPU scaling");
        self.release(conn);
        *self = Self::Disabled;
    }

    fn release(&mut self, conn: &RustConnection) {
        if let Self::Enabled(upscale) = self {
            if let Some(target) = upscale.target.take() {
                free_render_picture_unchecked(conn, target);
            }
            if let Some(source) = upscale.source.take() {
                free_render_source_unchecked(conn, source);
            }
        }
    }
}

impl EnabledRenderUpscale {
    fn create_target(
        &self,
        conn: &RustConnection,
        backing: Pixmap,
    ) -> Result<Picture, RenderFailure> {
        let picture = conn.generate_id().map_err(RenderFailure::from_id)?;
        render::create_picture(
            conn,
            picture,
            backing,
            self.format,
            &CreatePictureAux::new(),
        )
        .map_err(RenderFailure::from_connection)?
        .check()
        .map_err(RenderFailure::from_reply)?;
        Ok(picture)
    }

    #[allow(clippy::too_many_arguments)]
    fn ensure_source(
        &mut self,
        conn: &RustConnection,
        upload: &mut ShmUpload,
        image: Option<&XImage<'_>>,
        gc: Gcontext,
        key: RenderSourceKey,
        deferred_events: &mut VecDeque<Event>,
    ) -> Result<(), RenderFailure> {
        if !render_source_needs_upload(self.source.as_ref().map(|source| source.key), key) {
            return Ok(());
        }
        let image = image.ok_or_else(|| {
            RenderFailure::Recoverable(
                invalid_data("XRender source upload buffer is missing").into(),
            )
        })?;
        if let Some(source) = self.source.as_mut()
            && !render_source_needs_replacement(Some(source.key), key)
        {
            upload
                .upload_or_core(conn, image, source.pixmap, gc, 0, 0, deferred_events)
                .map_err(RenderFailure::from_remote)?;
            // Requests on this connection are ordered; subsequent composites
            // consume this upload. Async protocol failures terminate before ACK.
            source.key = key;
            return Ok(());
        }

        let pixmap = conn.generate_id().map_err(RenderFailure::from_id)?;
        conn.create_pixmap(self.depth, pixmap, self.root, key.width, key.height)
            .map_err(RenderFailure::from_connection)?
            .check()
            .map_err(RenderFailure::from_reply)?;
        let picture = match conn.generate_id().map_err(RenderFailure::from_id) {
            Ok(picture) => picture,
            Err(error) => {
                free_render_pixmap_unchecked(conn, pixmap);
                return Err(error);
            }
        };
        let create = render::create_picture(
            conn,
            picture,
            pixmap,
            self.format,
            &CreatePictureAux::new().repeat(Repeat::PAD),
        )
        .map_err(RenderFailure::from_connection)
        .and_then(|cookie| cookie.check().map_err(RenderFailure::from_reply));
        if let Err(error) = create {
            free_render_pixmap_unchecked(conn, pixmap);
            return Err(error);
        }
        let filter = render::set_picture_filter(conn, picture, RENDER_FILTER_BILINEAR, &[])
            .map_err(RenderFailure::from_connection)
            .and_then(|cookie| cookie.check().map_err(RenderFailure::from_reply));
        if let Err(error) = filter {
            free_render_picture_unchecked(conn, picture);
            free_render_pixmap_unchecked(conn, pixmap);
            return Err(error);
        }
        if let Err(error) = upload.upload_or_core(conn, image, pixmap, gc, 0, 0, deferred_events) {
            free_render_picture_unchecked(conn, picture);
            free_render_pixmap_unchecked(conn, pixmap);
            return Err(RenderFailure::from_remote(error));
        }

        let old = self.source.replace(RenderSource {
            key,
            pixmap,
            picture,
            transform: None,
        });
        if let Some(old) = old {
            free_render_source(conn, old)?;
        }
        Ok(())
    }

    fn composite(
        &mut self,
        conn: &RustConnection,
        geometry: Letterbox,
        source_width: u16,
        source_height: u16,
    ) -> Result<(), RenderFailure> {
        let source = self.source.as_mut().ok_or_else(|| {
            RenderFailure::Recoverable(invalid_data("XRender source picture is missing").into())
        })?;
        let target = self.target.ok_or_else(|| {
            RenderFailure::Recoverable(invalid_data("XRender target picture is missing").into())
        })?;
        let transform_key = (geometry.width, geometry.height);
        if render_transform_needs_update(source.transform, transform_key) {
            let transform =
                scale_transform(source_width, source_height, geometry.width, geometry.height)
                    .map_err(RenderFailure::Recoverable)?;
            render::set_picture_transform(conn, source.picture, transform)
                .map_err(RenderFailure::from_connection)?
                .check()
                .map_err(RenderFailure::from_reply)?;
            source.transform = Some(transform_key);
        }
        let cookie = render::composite(
            conn,
            PictOp::SRC,
            source.picture,
            x11rb::NONE,
            target,
            0,
            0,
            0,
            0,
            i16::try_from(geometry.x).map_err(|_| {
                RenderFailure::Recoverable(
                    invalid_data("XRender letterbox X offset exceeds protocol range").into(),
                )
            })?,
            i16::try_from(geometry.y).map_err(|_| {
                RenderFailure::Recoverable(
                    invalid_data("XRender letterbox Y offset exceeds protocol range").into(),
                )
            })?,
            geometry.width,
            geometry.height,
        )
        .map_err(RenderFailure::from_connection)?;
        self.pending_request = Some(AcceleratorRequest {
            sequence: cookie.sequence_number() as u16,
            major: self.major_opcode,
            minor: u16::from(render::COMPOSITE_REQUEST),
        });
        drop(cookie);
        if !self.reported_active {
            eprintln!(
                "jwm-remote: XRender viewer upscale {}x{} -> {}x{} active",
                source_width, source_height, geometry.width, geometry.height
            );
            self.reported_active = true;
        }
        Ok(())
    }
}

fn free_render_picture(conn: &RustConnection, picture: Picture) -> Result<(), RenderFailure> {
    render::free_picture(conn, picture)
        .map_err(RenderFailure::from_connection)?
        .check()
        .map_err(RenderFailure::from_reply)
}

fn free_render_source(conn: &RustConnection, source: RenderSource) -> Result<(), RenderFailure> {
    let picture_result = free_render_picture(conn, source.picture);
    let pixmap_result = conn
        .free_pixmap(source.pixmap)
        .map_err(RenderFailure::from_connection)
        .and_then(|cookie| cookie.check().map_err(RenderFailure::from_reply));
    match (picture_result, pixmap_result) {
        (Err(RenderFailure::Fatal(error)), _) | (_, Err(RenderFailure::Fatal(error))) => {
            Err(RenderFailure::Fatal(error))
        }
        (Err(RenderFailure::Recoverable(error)), _)
        | (_, Err(RenderFailure::Recoverable(error))) => Err(RenderFailure::Recoverable(error)),
        (Ok(()), Ok(())) => Ok(()),
    }
}

fn free_render_picture_unchecked(conn: &RustConnection, picture: Picture) {
    if let Ok(cookie) = render::free_picture(conn, picture) {
        cookie.ignore_error();
    }
}

fn free_render_pixmap_unchecked(conn: &RustConnection, pixmap: Pixmap) {
    if let Ok(cookie) = conn.free_pixmap(pixmap) {
        cookie.ignore_error();
    }
}

fn free_render_source_unchecked(conn: &RustConnection, source: RenderSource) {
    free_render_picture_unchecked(conn, source.picture);
    free_render_pixmap_unchecked(conn, source.pixmap);
}

fn supports_render_version(major: u32, minor: u32) -> bool {
    (major, minor) >= RENDER_MIN_VERSION
}

fn render_source_needs_upload(current: Option<RenderSourceKey>, incoming: RenderSourceKey) -> bool {
    current != Some(incoming)
}

fn render_source_needs_replacement(
    current: Option<RenderSourceKey>,
    incoming: RenderSourceKey,
) -> bool {
    current
        .is_none_or(|current| (current.width, current.height) != (incoming.width, incoming.height))
}

fn render_transform_needs_update(current: Option<(u16, u16)>, incoming: (u16, u16)) -> bool {
    current != Some(incoming)
}

fn finish_render_fallback<E>(
    cpu_result: Result<(), E>,
    render_error: Option<E>,
    disable: impl FnOnce(&E),
) -> Result<(), E> {
    cpu_result?;
    if let Some(error) = render_error.as_ref() {
        disable(error);
    }
    Ok(())
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
        matrix33: XFIXED_ONE,
    })
}

fn presentation_ready(
    copied: bool,
    uploads: &[ShmCompletionKey],
    mut pending: impl FnMut(ShmCompletionKey) -> bool,
) -> bool {
    copied && uploads.iter().all(|key| !pending(*key))
}

#[derive(Default)]
struct AccelerationRecovery {
    shm: bool,
    render: bool,
}

#[derive(Clone, Copy)]
struct AcceleratorRequest {
    sequence: u16,
    major: u8,
    minor: u16,
}

impl AcceleratorRequest {
    fn matches(self, error: &X11Error) -> bool {
        self.sequence == error.sequence
            && self.major == error.major_opcode
            && self.minor == error.minor_opcode
    }
}

struct PendingPresentation {
    sequence: Option<u64>,
    serial: u64,
    render_request: Option<AcceleratorRequest>,
    request_sequence: u16,
    copied: bool,
    uploads: Vec<ShmCompletionKey>,
    deadline: Instant,
}

fn invalidate_presentations(presentations: &mut VecDeque<PendingPresentation>) {
    for pending in presentations {
        pending.sequence = None;
    }
}

impl PendingPresentation {
    fn ready(&self, upload: &ShmUpload) -> bool {
        presentation_ready(self.copied, &self.uploads, |key| upload.pending(key))
    }
}

enum ShmUpload {
    Disabled,
    Enabled(EnabledShmUpload),
}

impl ShmUpload {
    fn reject(&mut self, error: &X11Error) -> bool {
        if let Self::Enabled(upload) = self {
            for slot in &mut upload.slots {
                if reject_upload_key(&mut slot.completion, error) {
                    // A protocol rejection does not read the shared pixels and
                    // produces no completion. Other slots remain server-owned.
                    return true;
                }
            }
        }
        false
    }

    fn complete(&mut self, event: &shm::CompletionEvent) {
        if let Self::Enabled(upload) = self {
            for slot in &mut upload.slots {
                if slot
                    .completion
                    .is_some_and(|key| is_matching_shm_completion(event, key))
                {
                    slot.completion = None;
                    break;
                }
            }
        }
    }

    fn pending(&self, key: ShmCompletionKey) -> bool {
        matches!(self, Self::Enabled(upload) if upload.slots.iter().any(|slot| slot.completion == Some(key)))
    }

    fn pending_keys(&self) -> Vec<ShmCompletionKey> {
        match self {
            Self::Disabled => Vec::new(),
            Self::Enabled(upload) => upload
                .slots
                .iter()
                .filter_map(|slot| slot.completion)
                .collect(),
        }
    }

    fn connect(conn: &RustConnection, depth: u8) -> Self {
        match Self::try_connect(conn, depth) {
            Ok(Some(upload)) => Self::Enabled(upload),
            Ok(None) => Self::Disabled,
            Err(error) => {
                eprintln!(
                    "jwm-remote: MIT-SHM FD upload unavailable ({error}); using core PutImage"
                );
                Self::Disabled
            }
        }
    }

    fn try_connect(conn: &RustConnection, depth: u8) -> RemoteResult<Option<EnabledShmUpload>> {
        let Some(extension) = conn.extension_information(shm::X11_EXTENSION_NAME)? else {
            eprintln!("jwm-remote: MIT-SHM unavailable for upload; using core PutImage");
            return Ok(None);
        };

        // The protocol requires this reply before any other MIT-SHM request.
        let version = shm::query_version(conn)?.reply()?;
        if !supports_shm_fd_version(version.major_version, version.minor_version) {
            eprintln!(
                "jwm-remote: MIT-SHM {}.{} lacks 1.2 FD segments for upload; using core PutImage",
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
        // Validate both protocol enums and the exact native allocation formula
        // before announcing SHM as available.
        native_buffer_size(1, 1, format)?;
        let image_byte_order = conn.setup().image_byte_order.try_into()?;
        Ok(Some(EnabledShmUpload {
            format,
            image_byte_order,
            version: (version.major_version, version.minor_version),
            major_opcode: extension.major_opcode,
            slots: Vec::new(),
            reported_active: false,
        }))
    }

    #[allow(clippy::too_many_arguments)]
    fn upload_or_core(
        &mut self,
        conn: &RustConnection,
        image: &XImage<'_>,
        drawable: Pixmap,
        gc: Gcontext,
        dst_x: i16,
        dst_y: i16,
        deferred_events: &mut VecDeque<Event>,
    ) -> RemoteResult<()> {
        let shm_attempt = match self {
            Self::Disabled => None,
            Self::Enabled(upload) => {
                Some(upload.upload(conn, image, drawable, gc, dst_x, dst_y, deferred_events))
            }
        };
        match resolve_upload(shm_attempt, || {
            core_upload(conn, image, drawable, gc, dst_x, dst_y)
        }) {
            UploadOutcome::Uploaded => Ok(()),
            UploadOutcome::CoreFallback { shm_error } => {
                self.disable(conn);
                eprintln!(
                    "jwm-remote: MIT-SHM FD upload stopped ({shm_error}); using core PutImage"
                );
                Ok(())
            }
            UploadOutcome::Error(error) => Err(error),
        }
    }

    fn disable(&mut self, conn: &RustConnection) {
        if let Self::Enabled(mut upload) = std::mem::replace(self, Self::Disabled) {
            upload.release(conn);
        }
    }

    fn release(&mut self, conn: &RustConnection) {
        self.disable(conn);
    }
}

struct EnabledShmUpload {
    format: Format,
    image_byte_order: ImageOrder,
    version: (u16, u16),
    major_opcode: u8,
    slots: Vec<ShmSlot>,
    reported_active: bool,
}

struct ShmSlot {
    segment: ShmSegment,
    completion: Option<ShmCompletionKey>,
}

impl EnabledShmUpload {
    #[allow(clippy::too_many_arguments)]
    fn upload(
        &mut self,
        conn: &RustConnection,
        image: &XImage<'_>,
        drawable: Pixmap,
        gc: Gcontext,
        dst_x: i16,
        dst_y: i16,
        _deferred_events: &mut VecDeque<Event>,
    ) -> ShmUploadAttempt<RemoteError> {
        let image_size = match validate_native_upload(image, self.format, self.image_byte_order) {
            Ok(size) => size,
            Err(error) => return ShmUploadAttempt::Recoverable(error),
        };
        let slot = match self.available_slot(conn, image_size) {
            Ok(Some(slot)) => slot,
            // All mappings remain owned by the server. Core upload is safe and
            // bounded and must not disable or detach the outstanding SHM pool.
            Ok(None) => {
                return match core_upload(conn, image, drawable, gc, dst_x, dst_y) {
                    Ok(()) => ShmUploadAttempt::Uploaded,
                    Err(error) => ShmUploadAttempt::Fatal(error),
                };
            }
            Err(error) => return ShmUploadAttempt::Recoverable(error),
        };
        let segment = &mut self.slots[slot].segment;
        if let Err(error) = segment.mapping.copy_from(image.data()) {
            return ShmUploadAttempt::Recoverable(error.into());
        }
        let segment_id = segment.id;
        let capacity = segment.mapping.len();

        let cookie = match shm::put_image(
            conn,
            drawable,
            gc,
            image.width(),
            image.height(),
            0,
            0,
            image.width(),
            image.height(),
            dst_x,
            dst_y,
            image.depth(),
            u8::from(ImageFormat::Z_PIXMAP),
            true,
            segment_id,
            0,
        ) {
            Ok(cookie) => cookie,
            // A transport failure does not establish whether the request is
            // outstanding. Do not overwrite or detach the mapping and do not
            // attempt another upload on this connection.
            Err(error) => return ShmUploadAttempt::Fatal(error.into()),
        };
        let completion = ShmCompletionKey {
            sequence: cookie.sequence_number() as u16,
            drawable,
            minor_event: u16::from(shm::PUT_IMAGE_REQUEST),
            major_event: self.major_opcode,
            segment: segment_id,
            offset: 0,
        };
        self.slots[slot].completion = Some(completion);
        // Dropping (not ignore_error) sends protocol failures to the event loop.
        // Never overwrite this slot until its exact completion is consumed.
        drop(cookie);
        if !self.reported_active {
            eprintln!(
                "jwm-remote: MIT-SHM {}.{} FD upload active ({capacity} bytes)",
                self.version.0, self.version.1
            );
            self.reported_active = true;
        }
        ShmUploadAttempt::Uploaded
    }

    fn available_slot(
        &mut self,
        conn: &RustConnection,
        required: usize,
    ) -> RemoteResult<Option<usize>> {
        if let Some(index) = self.slots.iter().position(|slot| slot.completion.is_none()) {
            if shm_needs_growth(Some(self.slots[index].segment.mapping.len()), required) {
                let replacement = ShmSegment::create(conn, required)?;
                let old = std::mem::replace(&mut self.slots[index].segment, replacement);
                old.release(conn);
            }
            return Ok(Some(index));
        }
        if self.slots.len() == SHM_UPLOAD_SLOTS {
            return Ok(None);
        }
        let segment = ShmSegment::create(conn, required)?;
        self.slots.push(ShmSlot {
            segment,
            completion: None,
        });
        Ok(Some(self.slots.len() - 1))
    }

    fn release(&mut self, conn: &RustConnection) {
        for slot in self.slots.drain(..) {
            // Detach is ordered after every outstanding PutImage on this connection.
            // The server owns its FD-backed mapping independently of this mapping.
            slot.segment.release(conn);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ShmCompletionKey {
    sequence: u16,
    drawable: Pixmap,
    minor_event: u16,
    major_event: u8,
    segment: shm::Seg,
    offset: u32,
}

fn reject_upload_key(completion: &mut Option<ShmCompletionKey>, error: &X11Error) -> bool {
    if completion.is_some_and(|key| {
        AcceleratorRequest {
            sequence: key.sequence,
            major: key.major_event,
            minor: key.minor_event,
        }
        .matches(error)
    }) {
        *completion = None;
        true
    } else {
        false
    }
}

fn is_matching_shm_completion(event: &shm::CompletionEvent, key: ShmCompletionKey) -> bool {
    event.sequence == key.sequence
        && event.drawable == key.drawable
        && event.minor_event == key.minor_event
        && event.major_event == key.major_event
        && event.shmseg == key.segment
        && event.offset == key.offset
}

struct ShmSegment {
    id: shm::Seg,
    mapping: MappedRegion,
    // Kept until after munmap so descriptor/mapping teardown order is explicit.
    _file: File,
}

impl ShmSegment {
    fn create(conn: &RustConnection, capacity: usize) -> RemoteResult<Self> {
        let size = u32::try_from(capacity)
            .map_err(|_| invalid_data("MIT-SHM upload exceeds the protocol size limit"))?;
        if size == 0 {
            return Err(invalid_data("MIT-SHM upload buffer is empty").into());
        }
        let id = conn.generate_id()?;
        // The client writes upload pixels; the server only needs read access.
        let cookie = shm::create_segment(conn, id, size, true)?;
        // Follow the validated capture-side lifecycle: a raw X11/transport
        // error does not prove the server created the segment. Once a success
        // reply exists, every parse/FD/mmap failure must detach the live XID.
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
                detach_shm_segment(conn, id);
                Err(error)
            }
        }
    }

    fn release(self, conn: &RustConnection) {
        // Detach is ordered after all PutImage requests. The server owns its
        // separate FD-backed mapping, so unmapping this client view is safe even
        // during error teardown. Never wait on a stalled server during Drop.
        detach_shm_segment(conn, self.id);
        drop(self);
    }
}

fn detach_shm_segment(conn: &RustConnection, segment: shm::Seg) {
    if let Ok(cookie) = shm::detach(conn, segment) {
        cookie.ignore_error();
    }
}

struct MappedRegion {
    address: NonNull<u8>,
    length: usize,
}

impl MappedRegion {
    fn new(file: &File, length: usize) -> io::Result<Self> {
        if length == 0 {
            return Err(invalid_input("cannot mmap an empty MIT-SHM upload segment"));
        }
        // SAFETY: fstat established that `file` covers `length`, and this
        // mapping is owned until the matching munmap in Drop.
        let mapped = unsafe {
            libc::mmap(
                ptr::null_mut(),
                length,
                libc::PROT_READ | libc::PROT_WRITE,
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

    fn copy_from(&mut self, source: &[u8]) -> io::Result<()> {
        if source.len() > self.length {
            return Err(invalid_data(format!(
                "MIT-SHM upload exceeds its mapping: {} > {}",
                source.len(),
                self.length
            )));
        }
        // SAFETY: &mut self provides exclusive access, the mapping is valid
        // for `self.length`, and the previous matching Completion was consumed
        // before this method can be called again.
        let destination =
            unsafe { std::slice::from_raw_parts_mut(self.address.as_ptr(), source.len()) };
        destination.copy_from_slice(source);
        Ok(())
    }
}

impl Drop for MappedRegion {
    fn drop(&mut self) {
        // SAFETY: this exact pair came from the successful mmap in `new` and
        // is unmapped exactly once here.
        unsafe {
            libc::munmap(self.address.as_ptr().cast(), self.length);
        }
    }
}

enum ShmUploadAttempt<E> {
    Uploaded,
    Recoverable(E),
    Fatal(E),
}

enum UploadOutcome<E> {
    Uploaded,
    CoreFallback { shm_error: E },
    Error(E),
}

fn resolve_upload<E>(
    shm_attempt: Option<ShmUploadAttempt<E>>,
    core: impl FnOnce() -> Result<(), E>,
) -> UploadOutcome<E> {
    match shm_attempt {
        None => match core() {
            Ok(()) => UploadOutcome::Uploaded,
            Err(error) => UploadOutcome::Error(error),
        },
        Some(ShmUploadAttempt::Uploaded) => UploadOutcome::Uploaded,
        Some(ShmUploadAttempt::Recoverable(shm_error)) => match core() {
            Ok(()) => UploadOutcome::CoreFallback { shm_error },
            // Preserve the core error and leave SHM enabled until Viewer Drop.
            // A drawable failure must not be misdiagnosed as an SHM failure.
            Err(error) => UploadOutcome::Error(error),
        },
        Some(ShmUploadAttempt::Fatal(error)) => UploadOutcome::Error(error),
    }
}

fn core_upload(
    conn: &RustConnection,
    image: &XImage<'_>,
    drawable: Pixmap,
    gc: Gcontext,
    dst_x: i16,
    dst_y: i16,
) -> RemoteResult<()> {
    let cookies = image.put(conn, drawable, gc, dst_x, dst_y)?;
    drop(cookies); // Errors flow through poll_events before presentation acknowledgment.
    Ok(())
}

fn supports_shm_fd_version(major: u16, minor: u16) -> bool {
    (major, minor) >= SHM_FD_VERSION
}

fn shm_needs_growth(current_capacity: Option<usize>, required: usize) -> bool {
    current_capacity.is_none_or(|capacity| capacity < required)
}

fn native_buffer_size(width: u16, height: u16, format: Format) -> RemoteResult<usize> {
    if width == 0 || height == 0 {
        return Err(invalid_data("MIT-SHM upload dimensions must be nonzero").into());
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
        .ok_or_else(|| invalid_data("MIT-SHM upload scanline size overflow"))?;
    let padded_units = row_bits
        .checked_add(scanline_pad - 1)
        .ok_or_else(|| invalid_data("MIT-SHM upload scanline padding overflow"))?
        / scanline_pad;
    let padded_bits = padded_units
        .checked_mul(scanline_pad)
        .ok_or_else(|| invalid_data("MIT-SHM upload padded scanline size overflow"))?;
    let size = (padded_bits / 8)
        .checked_mul(usize::from(height))
        .ok_or_else(|| invalid_data("MIT-SHM upload image size overflow"))?;
    u32::try_from(size)
        .map_err(|_| invalid_data("MIT-SHM upload exceeds the protocol size limit"))?;
    Ok(size)
}

fn validate_native_upload(
    image: &XImage<'_>,
    format: Format,
    image_byte_order: ImageOrder,
) -> RemoteResult<usize> {
    let expected_bpp = BitsPerPixel::try_from(format.bits_per_pixel)
        .map_err(|_| invalid_data("X11 reported invalid native bits-per-pixel"))?;
    let expected_pad = ScanlinePad::try_from(format.scanline_pad)
        .map_err(|_| invalid_data("X11 reported invalid native scanline padding"))?;
    if image.depth() != format.depth
        || image.bits_per_pixel() != expected_bpp
        || image.scanline_pad() != expected_pad
        || image.byte_order() != image_byte_order
    {
        return Err(
            invalid_data("MIT-SHM upload image is not in the negotiated native layout").into(),
        );
    }
    let expected_size = native_buffer_size(image.width(), image.height(), format)?;
    if image.data().len() != expected_size {
        return Err(invalid_data(format!(
            "MIT-SHM upload buffer has {} bytes; expected {expected_size}",
            image.data().len()
        ))
        .into());
    }
    Ok(expected_size)
}

fn select_native_pixel_writer(setup: &Setup, depth: u8, visual: Visualtype) -> NativePixelWriter {
    let format = setup
        .pixmap_formats
        .iter()
        .find(|format| format.depth == depth)
        .copied();
    select_native_pixel_writer_from_format(depth, visual, format, setup.image_byte_order)
}

fn backing_work(valid: bool) -> BackingWork {
    if valid {
        BackingWork::CopyOnly
    } else {
        BackingWork::PopulateAndCopy
    }
}

/// Clear the retained backing only when the letterbox actually moved.
///
/// Steady-state frames therefore issue no fill request at all: the bars around
/// the image are already correct, and the image rectangle is about to be
/// overwritten in full. Taking the fields individually rather than `&mut self`
/// keeps this callable while the frame and the native image are borrowed.
fn clear_backing_if_moved(
    conn: &RustConnection,
    backing: Pixmap,
    gc: Gcontext,
    width: u16,
    height: u16,
    cleared: &mut Option<Letterbox>,
    geometry: Letterbox,
) -> RemoteResult<()> {
    if !backing_needs_clear(*cleared, geometry) {
        return Ok(());
    }
    fill_backing(conn, backing, gc, width, height)?;
    // Record only after the fill succeeded, so a failure repeats it rather
    // than leaving stale bars on screen forever.
    *cleared = Some(geometry);
    Ok(())
}

/// Whether the retained backing still has correct bars for `geometry`.
fn backing_needs_clear(cleared: Option<Letterbox>, geometry: Letterbox) -> bool {
    cleared != Some(geometry)
}

fn fill_backing(
    conn: &RustConnection,
    backing: Pixmap,
    gc: Gcontext,
    width: u16,
    height: u16,
) -> RemoteResult<()> {
    let fill = conn.poly_fill_rectangle(
        backing,
        gc,
        &[Rectangle {
            x: 0,
            y: 0,
            width,
            height,
        }],
    )?;
    drop(fill); // Ordered before uploads; failures surface before the presentation fence.
    Ok(())
}

fn select_native_pixel_writer_from_format(
    depth: u8,
    visual: Visualtype,
    format: Option<Format>,
    image_byte_order: X11ImageOrder,
) -> NativePixelWriter {
    let standard_format = format.is_some_and(|format| {
        format.depth == 24 && format.bits_per_pixel == 32 && format.scanline_pad == 32
    });
    if depth == 24
        && visual.class == VisualClass::TRUE_COLOR
        && visual.bits_per_rgb_value == 8
        && visual.colormap_entries == 256
        && visual.red_mask == 0x00ff_0000
        && visual.green_mask == 0x0000_ff00
        && visual.blue_mask == 0x0000_00ff
        && standard_format
        && image_byte_order == X11ImageOrder::LSB_FIRST
    {
        NativePixelWriter::Bgrx32
    } else {
        NativePixelWriter::Generic
    }
}

fn ensure_native_image<'a>(
    image: &'a mut Option<XImage<'static>>,
    width: u16,
    height: u16,
    depth: u8,
    setup: &Setup,
) -> RemoteResult<&'a mut XImage<'static>> {
    validate_viewer_dimensions(u32::from(width), u32::from(height))?;
    let needs_replacement = image.as_ref().is_none_or(|image| {
        (image.width(), image.height(), image.depth()) != (width, height, depth)
    });
    if needs_replacement {
        // Allocate first so a local allocation/format error leaves the previous
        // reusable buffer intact.
        let replacement = XImage::allocate_native(width, height, depth, setup)?;
        *image = Some(replacement);
    }
    image
        .as_mut()
        .ok_or_else(|| invalid_data("native X11 image buffer was not allocated").into())
}

fn damage_base_matches(previous: &StoredFrame, frame: &StoredFrame, damage: FrameDamage) -> bool {
    previous.geometry_epoch == frame.geometry_epoch
        && previous.sequence == damage.base_sequence
        && previous.dimensions() == frame.dimensions()
        && (previous.source_width, previous.source_height)
            == (frame.source_width, frame.source_height)
}

fn render_source_accepts_damage(
    key: RenderSourceKey,
    damage: FrameDamage,
    dimensions: (u16, u16),
) -> bool {
    key.sequence == damage.base_sequence && (key.width, key.height) == dimensions
}

/// Conservative destination coverage for image's separable Triangle filter.
/// The source support is max(source/destination, 1), including antialiasing
/// when shrinking. An extra destination pixel protects floating-point boundary
/// rounding. Identity scaling needs no halo. A single rectangle bounds work.
fn scaled_damage_rect(
    rect: (u16, u16, u16, u16),
    source: (u16, u16),
    destination: (u16, u16),
) -> (u16, u16, u16, u16) {
    fn axis(start: u16, length: u16, source: u16, destination: u16) -> (u16, u16) {
        if source == destination {
            return (start, length);
        }
        let ratio = f64::from(source) / f64::from(destination);
        let support = ratio.max(1.0);
        let left = ((f64::from(start) - support) / ratio).floor() - 1.0;
        let right = ((f64::from(start) + f64::from(length) + support) / ratio).ceil() + 1.0;
        let left = left.clamp(0.0, f64::from(destination)) as u16;
        let right = right.clamp(f64::from(left), f64::from(destination)) as u16;
        (left, right - left)
    }
    let (x, width) = axis(rect.0, rect.2, source.0, destination.0);
    let (y, height) = axis(rect.1, rect.3, source.1, destination.1);
    (x, y, width, height)
}

/// Convert only a bounded source rectangle into a compact upload image.
fn write_native_region(
    image: &mut XImage<'_>,
    rgb: &RgbImage,
    x: u16,
    y: u16,
    pixel_layout: PixelLayout,
    writer: NativePixelWriter,
) -> RemoteResult<()> {
    let width = image.width();
    let height = image.height();
    if u32::from(x) + u32::from(width) > rgb.width()
        || u32::from(y) + u32::from(height) > rgb.height()
    {
        return Err(invalid_data("damage rectangle exceeds decoded image").into());
    }
    let fast = writer == NativePixelWriter::Bgrx32
        && image.depth() == 24
        && image.bits_per_pixel() == BitsPerPixel::B32
        && image.scanline_pad() == ScanlinePad::Pad32
        && image.byte_order() == ImageOrder::LsbFirst;
    if fast && image.data().len() == usize::from(width) * usize::from(height) * 4 {
        let source_stride = rgb.width() as usize * 3;
        let target_stride = usize::from(width) * 4;
        for row in 0..usize::from(height) {
            let start = (usize::from(y) + row) * source_stride + usize::from(x) * 3;
            let source = &rgb.as_raw()[start..start + usize::from(width) * 3];
            let target = &mut image.data_mut()[row * target_stride..(row + 1) * target_stride];
            for (rgb, bgrx) in source.chunks_exact(3).zip(target.chunks_exact_mut(4)) {
                bgrx.copy_from_slice(&[rgb[2], rgb[1], rgb[0], 0]);
            }
        }
    } else {
        for row in 0..height {
            for column in 0..width {
                let rgb = rgb.get_pixel(
                    u32::from(x) + u32::from(column),
                    u32::from(y) + u32::from(row),
                );
                image.put_pixel(
                    column,
                    row,
                    pixel_layout.encode((
                        u16::from(rgb[0]) * 257,
                        u16::from(rgb[1]) * 257,
                        u16::from(rgb[2]) * 257,
                    )),
                );
            }
        }
    }
    Ok(())
}

fn write_native_pixels(
    image: &mut XImage<'_>,
    rgb: &RgbImage,
    pixel_layout: PixelLayout,
    writer: NativePixelWriter,
) -> RemoteResult<()> {
    let width = u16::try_from(rgb.width())
        .map_err(|_| invalid_data("viewer image width exceeds the X11 protocol range"))?;
    let height = u16::try_from(rgb.height())
        .map_err(|_| invalid_data("viewer image height exceeds the X11 protocol range"))?;
    if (image.width(), image.height()) != (width, height) {
        return Err(
            invalid_data("native X11 buffer dimensions do not match the viewer image").into(),
        );
    }

    if writer == NativePixelWriter::Bgrx32
        && image.depth() == 24
        && image.bits_per_pixel() == BitsPerPixel::B32
        && image.scanline_pad() == ScanlinePad::Pad32
        && image.byte_order() == ImageOrder::LsbFirst
    {
        let source = rgb.as_raw();
        let expected_source = usize::from(width)
            .checked_mul(usize::from(height))
            .and_then(|pixels| pixels.checked_mul(3))
            .ok_or_else(|| invalid_data("viewer RGB buffer size overflow"))?;
        let expected_native = usize::from(width)
            .checked_mul(usize::from(height))
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| invalid_data("viewer native buffer size overflow"))?;
        if source.len() == expected_source && image.data().len() == expected_native {
            for (source, destination) in source
                .chunks_exact(3)
                .zip(image.data_mut().chunks_exact_mut(4))
            {
                destination[0] = source[2];
                destination[1] = source[1];
                destination[2] = source[0];
                destination[3] = 0;
            }
            return Ok(());
        }
    }

    for (x, y, rgb) in rgb.enumerate_pixels() {
        let pixel = pixel_layout.encode((
            u16::from(rgb[0]) * 257,
            u16::from(rgb[1]) * 257,
            u16::from(rgb[2]) * 257,
        ));
        image.put_pixel(x as u16, y as u16, pixel);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Letterbox {
    x: u16,
    y: u16,
    width: u16,
    height: u16,
}

fn pointer_inside_letterbox(x: i16, y: i16, geometry: Letterbox) -> bool {
    let (x, y) = (i32::from(x), i32::from(y));
    let (left, top) = (i32::from(geometry.x), i32::from(geometry.y));
    x >= left
        && y >= top
        && x < left + i32::from(geometry.width)
        && y < top + i32::from(geometry.height)
}

fn should_use_render_upscale(geometry: Letterbox, image_width: u16, image_height: u16) -> bool {
    geometry.width > image_width && geometry.height > image_height
}

fn letterbox(
    window_width: u16,
    window_height: u16,
    image_width: u16,
    image_height: u16,
) -> Option<Letterbox> {
    if window_width == 0 || window_height == 0 || image_width == 0 || image_height == 0 {
        return None;
    }

    let window_width_u32 = u32::from(window_width);
    let window_height_u32 = u32::from(window_height);
    let image_width_u32 = u32::from(image_width);
    let image_height_u32 = u32::from(image_height);
    let (width, height) =
        if window_width_u32 * image_height_u32 <= window_height_u32 * image_width_u32 {
            (
                window_width_u32,
                (window_width_u32 * image_height_u32 / image_width_u32).max(1),
            )
        } else {
            (
                (window_height_u32 * image_width_u32 / image_height_u32).max(1),
                window_height_u32,
            )
        };
    let width = width as u16;
    let height = height as u16;
    Some(Letterbox {
        x: (window_width - width) / 2,
        y: (window_height - height) / 2,
        width,
        height,
    })
}

/// Scoped grants reject black bars and outside coordinates before mapping.
/// They never inherit the legacy drag-friendly clamping policy.
fn map_scoped_pointer_to_source(
    x: i16,
    y: i16,
    ww: u16,
    wh: u16,
    iw: u16,
    ih: u16,
    sw: u16,
    sh: u16,
) -> Option<(u16, u16)> {
    let bounds = letterbox(ww, wh, iw, ih)?;
    if !pointer_inside_letterbox(x, y, bounds) {
        return None;
    }
    map_pointer_to_source(x, y, ww, wh, iw, ih, sw, sh)
}

/// Legacy pointer mapping clamps outside coordinates for held drags.
fn map_pointer_to_source(
    x: i16,
    y: i16,
    window_width: u16,
    window_height: u16,
    image_width: u16,
    image_height: u16,
    source_width: u16,
    source_height: u16,
) -> Option<(u16, u16)> {
    if source_width == 0 || source_height == 0 {
        return None;
    }
    let geometry = letterbox(window_width, window_height, image_width, image_height)?;
    Some((
        map_axis(x, geometry.x, geometry.width, source_width),
        map_axis(y, geometry.y, geometry.height, source_height),
    ))
}

fn map_axis(position: i16, offset: u16, displayed: u16, source: u16) -> u16 {
    if displayed <= 1 || source <= 1 {
        return 0;
    }
    let relative =
        (i64::from(position) - i64::from(offset)).clamp(0, i64::from(displayed - 1)) as u64;
    (relative * u64::from(source - 1) / u64::from(displayed - 1)) as u16
}

fn push_pointer(events: &mut Vec<ViewerEvent>, x: u16, y: u16) {
    if let Some(ViewerEvent::Pointer {
        x: previous_x,
        y: previous_y,
    }) = events.last_mut()
    {
        *previous_x = x;
        *previous_y = y;
    } else {
        events.push(ViewerEvent::Pointer { x, y });
    }
}

fn is_closed_window_error(error: &X11Error, window: Window) -> bool {
    matches!(error.error_kind, ErrorKind::Drawable | ErrorKind::Window) && error.bad_value == window
}

/// Bring an open viewer of `peer_label` on `display` to the front.
///
/// Returns whether one was found. A keyboard shortcut that opens a remote
/// desktop is pressed again to get back to it, and a second session to the
/// same machine would only fight the first over its input.
pub fn focus_existing_viewer(display: Option<&str>, peer_label: &str) -> RemoteResult<bool> {
    let (conn, root) = open_root(display)?;
    let Some(window) = find_viewer_window(&conn, root, peer_label)? else {
        return Ok(false);
    };
    let active_window = intern_atom(&conn, b"_NET_ACTIVE_WINDOW")?;
    // Source 2: a pager-like request on the user's behalf, which window
    // managers honour without focus-stealing prevention.
    let event = ClientMessageEvent::new(32, window, active_window, [2, CURRENT_TIME, 0, 0, 0]);
    conn.send_event(
        false,
        root,
        EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
        event,
    )?;
    conn.flush()?;
    Ok(true)
}

/// Close an open viewer of `peer_label` the way its close button does.
///
/// Returns whether one was found. The viewer releases any held input, tells
/// the host it is leaving and exits; a host started by `jwm-remote ssh`
/// stops with it. Used to switch to another monitor of the same machine,
/// since one host captures a screen at a time.
pub fn close_existing_viewer(display: Option<&str>, peer_label: &str) -> RemoteResult<bool> {
    let (conn, root) = open_root(display)?;
    let Some(window) = find_viewer_window(&conn, root, peer_label)? else {
        return Ok(false);
    };
    let wm_protocols = intern_atom(&conn, b"WM_PROTOCOLS")?;
    let wm_delete_window = intern_atom(&conn, b"WM_DELETE_WINDOW")?;
    let event = ClientMessageEvent::new(
        32,
        window,
        wm_protocols,
        [wm_delete_window, CURRENT_TIME, 0, 0, 0],
    );
    conn.send_event(false, window, EventMask::NO_EVENT, event)?;
    conn.flush()?;
    Ok(true)
}

fn open_root(display: Option<&str>) -> RemoteResult<(RustConnection, Window)> {
    let (conn, screen_num) = xbar_core::x11_connection::connect(display)?;
    let root = conn
        .setup()
        .roots
        .get(screen_num)
        .ok_or_else(|| invalid_data("X11 selected an unavailable screen"))?
        .root;
    Ok((conn, root))
}

/// The viewer window showing `peer_label`, if one is open.
fn find_viewer_window(
    conn: &RustConnection,
    root: Window,
    peer_label: &str,
) -> RemoteResult<Option<Window>> {
    let client_list = intern_atom(conn, b"_NET_CLIENT_LIST")?;
    let net_wm_name = intern_atom(conn, b"_NET_WM_NAME")?;
    let utf8_string = intern_atom(conn, b"UTF8_STRING")?;
    // Managed clients, plus top-level windows for a session with no window
    // manager to keep that list.
    let mut candidates: Vec<Window> = conn
        .get_property(false, root, client_list, AtomEnum::WINDOW, 0, 4096)?
        .reply()?
        .value32()
        .map(Iterator::collect)
        .unwrap_or_default();
    candidates.extend(conn.query_tree(root)?.reply()?.children);
    for window in candidates {
        let class = conn
            .get_property(false, window, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 64)?
            .reply()
            .map(|reply| reply.value)
            .unwrap_or_default();
        if class != WM_CLASS {
            continue;
        }
        let title = conn
            .get_property(false, window, net_wm_name, utf8_string, 0, 1024)?
            .reply()
            .map(|reply| reply.value)
            .unwrap_or_default();
        if is_viewer_of(&title, peer_label) {
            return Ok(Some(window));
        }
    }
    Ok(None)
}

/// Whether a viewer window title belongs to `peer_label`.
fn is_viewer_of(title: &[u8], peer_label: &str) -> bool {
    title.starts_with(format!("{peer_label} - JWM Remote").as_bytes())
}

fn intern_atom(conn: &RustConnection, name: &[u8]) -> RemoteResult<Atom> {
    Ok(conn.intern_atom(false, name)?.reply()?.atom)
}

fn create_blank_cursor(conn: &RustConnection, drawable: Window) -> RemoteResult<Cursor> {
    let pixmap = conn.generate_id()?;
    let gc = conn.generate_id()?;
    let cursor = conn.generate_id()?;
    conn.create_pixmap(1, pixmap, drawable, 1, 1)?.check()?;
    conn.create_gc(gc, pixmap, &CreateGCAux::new().foreground(0))?
        .check()?;
    conn.poly_fill_rectangle(
        pixmap,
        gc,
        &[Rectangle {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        }],
    )?
    .check()?;
    conn.create_cursor(cursor, pixmap, pixmap, 0, 0, 0, 0, 0, 0, 0, 0)?
        .check()?;
    conn.free_gc(gc)?.check()?;
    conn.free_pixmap(pixmap)?.check()?;
    Ok(cursor)
}

fn find_visual(screen: &Screen, visual_id: u32) -> Option<(u8, Visualtype)> {
    screen.allowed_depths.iter().find_map(|depth| {
        depth
            .visuals
            .iter()
            .find(|visual| visual.visual_id == visual_id)
            .copied()
            .map(|visual| (depth.depth, visual))
    })
}

/// Match JWM's Ctrl+Super shortcut, ignoring locks and held mouse buttons.
fn append_source_title(title: &mut String, current: Option<&str>, switching: Option<&str>) {
    if let Some(label) = switching {
        title.push_str(&format!(" | switching to {label}"));
    } else if let Some(label) = current {
        title.push_str(&format!(" | source {label}"));
    }
}

fn source_cycle_modifiers_match(state: KeyButMask) -> bool {
    let modifiers = KeyButMask::SHIFT | KeyButMask::CONTROL | KeyButMask::MOD1 | KeyButMask::MOD4;
    state & modifiers == KeyButMask::SHIFT | KeyButMask::CONTROL | KeyButMask::MOD4
}

fn clipboard_narrow_modifiers_match(state: KeyButMask) -> bool {
    let modifiers = KeyButMask::SHIFT | KeyButMask::CONTROL | KeyButMask::MOD1 | KeyButMask::MOD4;
    state & modifiers == KeyButMask::CONTROL | KeyButMask::MOD1 | KeyButMask::MOD4
}

fn escape_modifiers_match(state: KeyButMask) -> bool {
    let modifiers = KeyButMask::SHIFT | KeyButMask::CONTROL | KeyButMask::MOD1 | KeyButMask::MOD4;
    state & modifiers == KeyButMask::CONTROL | KeyButMask::MOD4
}

/// Check an escape-key name before anything slow happens.
///
/// Resolution otherwise waited until the viewer window was created, which is
/// after the network connect and the handshake, so a typo surfaced as a
/// failure well into an apparently working session.
pub fn validate_escape_key(name: &str) -> RemoteResult<()> {
    escape_keysym_from_name(name).map(|_| ())
}

/// Resolve an X keysym name such as `F12`, `Pause` or `Scroll_Lock`.
fn escape_keysym_from_name(name: &str) -> RemoteResult<u32> {
    let keysym = xkbcommon::xkb::keysym_from_name(name, xkbcommon::xkb::KEYSYM_NO_FLAGS);
    if keysym.raw() == xkbcommon::xkb::keysyms::KEY_NoSymbol {
        return Err(invalid_input(format!(
            "unknown escape key {name:?}; use an X keysym name such as F12, Pause or Scroll_Lock"
        ))
        .into());
    }
    Ok(keysym.raw())
}

fn load_escape_keycodes(conn: &RustConnection, keysym: u32) -> RemoteResult<Vec<u8>> {
    let setup = conn.setup();
    let min = setup.min_keycode;
    let count = setup.max_keycode.saturating_sub(min).saturating_add(1);
    let reply = conn.get_keyboard_mapping(min, count)?.reply()?;
    let per_keycode = usize::from(reply.keysyms_per_keycode);
    if per_keycode == 0 {
        return Ok(Vec::new());
    }

    Ok(reply
        .keysyms
        .chunks_exact(per_keycode)
        .enumerate()
        .filter_map(|(offset, keysyms)| {
            (keysyms.first() == Some(&keysym))
                .then(|| min.saturating_add(u8::try_from(offset).unwrap_or(u8::MAX)))
        })
        .collect())
}

fn validate_frame(frame: &StoredFrame) -> RemoteResult<()> {
    validate_viewer_dimensions(
        u32::from(frame.source_width),
        u32::from(frame.source_height),
    )?;
    validate_viewer_dimensions(frame.image().width(), frame.image().height())
}

fn replace_stored_frame(slot: &mut Option<StoredFrame>, next: StoredFrame) {
    let previous = slot.replace(next);
    // In the recyclable path this may take the decode-pool mutex. Callers do
    // not hold the receiver's latest-frame mutex while retiring this value.
    drop(previous);
}

fn clear_stored_frame(slot: &mut Option<StoredFrame>) {
    drop(slot.take());
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::frame::{
        DecodedFrame, RecyclableDecodedFrame, decode_pool_snapshot, new_decode_buffer_pool,
    };
    use std::cell::Cell;
    use std::sync::Arc;

    #[test]
    fn viewer_resource_bounds_preserve_8k_and_reject_oversized_geometry() {
        for (w, h) in [
            (1, 1),
            (7680, 4320),
            (8192, 8192),
            (16384, 4096),
            (4096, 16384),
        ] {
            assert!(validate_viewer_dimensions(w, h).is_ok(), "{w}x{h}");
        }
        for (w, h) in [
            (0, 1),
            (1, 0),
            (16385, 1),
            (1, 16385),
            (16384, 4097),
            (4097, 16384),
            (65535, 65535),
            (u32::MAX, u32::MAX),
        ] {
            assert!(validate_viewer_dimensions(w, h).is_err(), "{w}x{h}");
        }
        assert_eq!(configured_viewer_dimensions(0, 0).unwrap(), (1, 1));
        assert_eq!(configured_viewer_dimensions(0, 4320).unwrap(), (1, 4320));
        assert!(configured_viewer_dimensions(65535, 0).is_err());
    }

    #[test]
    fn viewer_frame_bounds_cover_direct_source_and_decoded_callers() {
        let mut frame = StoredFrame::from_decoded(DecodedFrame {
            sequence: 1,
            source_width: 7680,
            source_height: 4320,
            image: RgbImage::new(1, 1),
        });
        assert!(validate_frame(&frame).is_ok());
        frame.source_width = 16385;
        assert!(validate_frame(&frame).is_err());
        frame.source_width = 16384;
        frame.source_height = 4097;
        assert!(validate_frame(&frame).is_err());
        frame.source_width = 1;
        frame.source_height = 1;
        // Only about 48 KiB: no boundary test allocates a huge frame.
        frame.image = StoredImage::Owned(RgbImage::new(16385, 1));
        assert!(validate_frame(&frame).is_err());
        frame.image = StoredImage::Owned(RgbImage::new(0, 1));
        assert!(validate_frame(&frame).is_err());
    }

    #[test]
    fn viewer_resource_checks_precede_mutation_and_allocation() {
        let source = include_str!("x11_viewer.rs");
        let configure = source
            .split("Event::ConfigureNotify(event) if")
            .nth(1)
            .unwrap();
        assert!(
            configure.find("configured_viewer_dimensions(").unwrap()
                < configure.find("self.width =").unwrap()
        );
        let connect = source
            .split("pub(crate) fn connect_with_window_geometry(")
            .nth(1)
            .unwrap();
        assert!(
            connect.find("validate_viewer_dimensions(").unwrap()
                < connect.find("xbar_core::x11_connection::connect(").unwrap()
        );
        let startup = connect
            .split("let (initial_width, initial_height) = startup_window_size(")
            .nth(1)
            .unwrap();
        assert!(
            startup.find("validate_viewer_dimensions(").unwrap()
                < startup.find("conn.create_window(").unwrap()
        );
        let resize = source.split("fn resize_backing(&mut self)").nth(1).unwrap();
        assert!(
            resize.find("validate_viewer_dimensions(").unwrap()
                < resize.find("self.conn.generate_id()").unwrap()
        );
        let native = source.split("fn ensure_native_image<'a>(").nth(1).unwrap();
        assert!(
            native.find("validate_viewer_dimensions(").unwrap()
                < native.find("XImage::allocate_native(").unwrap()
        );
        let draw = source.split("fn draw_stored(&mut self").nth(1).unwrap();
        assert!(
            draw.find("validate_frame(&frame)").unwrap()
                < draw.find("self.apply_pending_resize()").unwrap()
        );
    }

    fn standard_visual() -> Visualtype {
        Visualtype {
            visual_id: 7,
            class: VisualClass::TRUE_COLOR,
            bits_per_rgb_value: 8,
            colormap_entries: 256,
            red_mask: 0x00ff_0000,
            green_mask: 0x0000_ff00,
            blue_mask: 0x0000_00ff,
        }
    }

    fn standard_format() -> Format {
        Format {
            depth: 24,
            bits_per_pixel: 32,
            scanline_pad: 32,
        }
    }

    fn test_cursor_shape(id: u64) -> Arc<CursorShape> {
        Arc::new(CursorShape {
            id,
            width: 4,
            height: 4,
            hotspot_x: 1,
            hotspot_y: 1,
            rgba: [255, 0, 0, 255].repeat(16),
        })
    }

    fn test_cursor_position(sequence: u64) -> CursorPosition {
        CursorPosition {
            sequence,
            geometry_epoch: 7,
            shape_id: 1,
            source_width: 100,
            source_height: 100,
            x: 0,
            y: 0,
            visible: true,
        }
    }

    #[test]
    #[ignore = "requires an isolated X11 server without a WM in JWM_TEST_X11_DISPLAY"]
    fn explicit_position_and_fallback_hints_on_x11() {
        use super::super::{window_position::WindowPosition, window_size::WindowSize};
        use x11rb::properties::{WmSizeHints, WmSizeHintsSpecification};
        let display = std::env::var("JWM_TEST_X11_DISPLAY").unwrap();
        let (conn, screen_num) = xbar_core::x11_connection::connect(Some(&display)).unwrap();
        let screen = &conn.setup().roots[screen_num];
        let bounds = placement_monitor(
            &conn,
            screen.root,
            (screen.width_in_pixels, screen.height_in_pixels),
        );
        let position = WindowPosition {
            x: bounds.0.max(0),
            y: bounds.1.max(0),
        };
        let target = position_target(
            &conn,
            screen.root,
            (screen.width_in_pixels, screen.height_in_pixels),
            position,
        )
        .unwrap();
        assert!(
            target.position_visible,
            "isolated server must advertise a visible RandR monitor"
        );
        let size = WindowSize {
            width: target.bounds.2.min(64),
            height: target.bounds.3.min(48),
        };
        let viewer = Viewer::connect_with_window_geometry(
            Some(&display),
            1,
            1,
            Some(size),
            Some(position),
            false,
            false,
            None,
            false,
            "F12",
            "position test",
        )
        .unwrap();
        let geometry = viewer
            .conn
            .get_geometry(viewer.window)
            .unwrap()
            .reply()
            .unwrap();
        assert_eq!((geometry.x, geometry.y), (position.x, position.y));
        assert_eq!((geometry.width, geometry.height), size.dimensions());
        let hints = WmSizeHints::get_normal_hints(&viewer.conn, viewer.window)
            .unwrap()
            .reply()
            .unwrap()
            .unwrap();
        assert!(matches!(
            hints.position,
            Some((WmSizeHintsSpecification::UserSpecified, _, _))
        ));
        drop(viewer);
        let outside = WindowPosition {
            x: -32768,
            y: -32768,
        };
        let target = position_target(
            &conn,
            screen.root,
            (screen.width_in_pixels, screen.height_in_pixels),
            outside,
        )
        .unwrap();
        let expected = super::super::window_position::place(target, size.dimensions(), outside);
        assert!(!expected.1);
        let viewer = Viewer::connect_with_window_geometry(
            Some(&display),
            1,
            1,
            Some(size),
            Some(outside),
            false,
            false,
            None,
            false,
            "F12",
            "fallback test",
        )
        .unwrap();
        let geometry = viewer
            .conn
            .get_geometry(viewer.window)
            .unwrap()
            .reply()
            .unwrap();
        assert_eq!((geometry.x, geometry.y), expected.0);
        assert_eq!((geometry.width, geometry.height), size.dimensions());
        assert!(
            WmSizeHints::get_normal_hints(&viewer.conn, viewer.window)
                .unwrap()
                .reply()
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn explicit_position_only_sets_user_hint_when_visible() {
        assert!(position_hints((0, 0), false).is_none());
        let hints = position_hints((120, 80), true).unwrap();
        assert!(matches!(
            hints.position,
            Some((
                x11rb::properties::WmSizeHintsSpecification::UserSpecified,
                120,
                80
            ))
        ));
        assert!(hints.size.is_none());
        assert!(hints.min_size.is_none());
        assert!(hints.max_size.is_none());
        let size = Some(super::super::window_size::WindowSize {
            width: 1280,
            height: 720,
        });
        let position = Some(super::super::window_position::WindowPosition {
            x: -32768,
            y: 32767,
        });
        assert_eq!(
            initial_viewport_with_position(Some("invalid display"), size, position).unwrap(),
            (1280, 720)
        );
        assert_eq!(
            initial_viewport_with_position(Some("invalid display"), size, None).unwrap(),
            (1280, 720)
        );
    }

    #[test]
    fn explicit_startup_size_is_independent_of_frame_and_default_is_unchanged() {
        let size = Some(super::super::window_size::WindowSize {
            width: 1280,
            height: 720,
        });
        for frame in [(1, 1), (640, 480), (8192, 8192)] {
            for monitor in [(1920, 1080), (800, 600)] {
                assert_eq!(startup_window_size(frame, monitor, size), (1280, 720));
                assert_eq!(
                    startup_window_size(frame, monitor, None),
                    fit_initial_window(frame, monitor)
                );
            }
        }
        // Explicit viewport needs no display inspection; invalid values fail first.
        assert_eq!(
            initial_viewport(Some("invalid display"), size).unwrap(),
            (1280, 720)
        );
        assert!(
            initial_viewport(
                Some("invalid display"),
                Some(super::super::window_size::WindowSize {
                    width: 0,
                    height: 1
                })
            )
            .is_err()
        );
    }

    #[test]
    fn source_title_shows_initial_current_and_pending_target() {
        let mut title = "Peer".to_string();
        append_source_title(&mut title, Some("Main"), None);
        assert_eq!(title, "Peer | source Main");
        let mut title = "Peer".to_string();
        append_source_title(&mut title, Some("Main"), Some("Side"));
        assert_eq!(title, "Peer | switching to Side");
        let mut title = "Peer".to_string();
        append_source_title(&mut title, Some("Side"), None);
        assert_eq!(title, "Peer | source Side");
    }

    #[test]
    fn geometry_transition_clears_held_and_deferred_input_until_presentation() {
        let mut keyboard = ViewerKeyboardState::default();
        assert_eq!(keyboard.press(42, false, true), KeyPressAction::Forward);
        keyboard.defer_release(42, 9, Instant::now());
        let mut buttons = ForwardedButtons::default();
        buttons.press(1, true);
        let mut presented = Some(3);
        reset_input_geometry(&mut keyboard, &mut buttons, &mut presented);
        assert!(keyboard.held.is_empty());
        assert!(keyboard.pending_release.is_none());
        assert!(buttons.0.is_empty());
        assert!(presented.is_none());
    }

    #[test]
    fn clipboard_narrow_shortcut_is_distinct_and_ignores_locks() {
        let chord = KeyButMask::CONTROL | KeyButMask::MOD4 | KeyButMask::MOD1;
        assert!(clipboard_narrow_modifiers_match(chord));
        assert!(clipboard_narrow_modifiers_match(
            chord | KeyButMask::LOCK | KeyButMask::MOD2 | KeyButMask::BUTTON1
        ));
        assert!(!clipboard_narrow_modifiers_match(chord | KeyButMask::SHIFT));
        assert!(!clipboard_narrow_modifiers_match(
            KeyButMask::CONTROL | KeyButMask::MOD4
        ));
        assert!(!escape_modifiers_match(chord));
        assert!(!source_cycle_modifiers_match(chord));
    }

    #[test]
    fn clipboard_narrow_shortcut_view_only_consumes_repeat_and_release() {
        let mut shortcut = ViewerKeyboardState::default();
        let mut remote = ViewerKeyboardState::default();
        let mut buttons = ForwardedButtons::default();
        let mut events = Vec::new();
        assert_eq!(shortcut.press(96, true, false), KeyPressAction::ReleaseGrab);
        clear_forwarded_input(&mut remote, &mut buttons, &mut events);
        assert_eq!(events, vec![ViewerEvent::ReleaseAll]);
        assert_eq!(shortcut.press(96, true, false), KeyPressAction::Ignore);
        shortcut.defer_release(96, 42, Instant::now());
        assert!(shortcut.consume_repeat(96, 42));
        assert_eq!(shortcut.press(96, true, false), KeyPressAction::Ignore);
        shortcut.defer_release(96, 43, Instant::now());
        assert!(!shortcut.consume_repeat(96, 44));
        shortcut.flush_release(&mut events, false);
        assert_eq!(events, vec![ViewerEvent::ReleaseAll]);
        assert!(shortcut.held.is_empty());
        assert_eq!(shortcut.press(96, true, false), KeyPressAction::ReleaseGrab);
    }

    #[test]
    fn source_cycle_shortcut_is_distinct_from_release_and_ignores_locks() {
        let cycle = KeyButMask::CONTROL | KeyButMask::MOD4 | KeyButMask::SHIFT;
        assert!(source_cycle_modifiers_match(cycle));
        assert!(source_cycle_modifiers_match(
            cycle | KeyButMask::LOCK | KeyButMask::BUTTON1
        ));
        assert!(!escape_modifiers_match(cycle));
        assert!(!source_cycle_modifiers_match(
            KeyButMask::CONTROL | KeyButMask::MOD4
        ));
        assert!(!source_cycle_modifiers_match(cycle | KeyButMask::MOD1));
    }

    #[test]
    fn source_invalidation_retains_async_fences_and_upload_ownership() {
        let key = ShmCompletionKey {
            sequence: 71,
            drawable: 10,
            minor_event: u16::from(shm::PUT_IMAGE_REQUEST),
            major_event: 130,
            segment: 20,
            offset: 0,
        };
        let deadline = Instant::now() + PRESENTATION_TIMEOUT;
        let mut queue = VecDeque::from([PendingPresentation {
            sequence: Some(42),
            serial: 7,
            render_request: None,
            request_sequence: 9,
            copied: false,
            uploads: vec![key],
            deadline,
        }]);
        invalidate_presentations(&mut queue);
        assert_eq!(queue.len(), 1);
        let pending = queue.front().unwrap();
        assert_eq!(pending.sequence, None);
        assert_eq!(pending.serial, 7);
        assert_eq!(pending.request_sequence, 9);
        assert_eq!(pending.deadline, deadline);
        assert_eq!(pending.uploads, [key]);
        assert!(!pending.ready(&ShmUpload::Disabled));
        queue.front_mut().unwrap().copied = true;
        let pending = queue.front().unwrap();
        assert!(!presentation_ready(
            pending.copied,
            &pending.uploads,
            |_| true
        ));
        assert!(presentation_ready(pending.copied, &pending.uploads, |_| {
            false
        }));
    }

    #[test]
    fn cursor_placement_scales_hotspot_clips_and_rejects_stale_geometry() {
        let shape = test_cursor_shape(1);
        let mut position = test_cursor_position(1);
        let geometry = Letterbox {
            x: 20,
            y: 10,
            width: 200,
            height: 200,
        };
        let placement = cursor_placement(&position, &shape, 7, (100, 100), geometry).unwrap();
        assert_eq!((placement.left, placement.top), (18, 8));
        assert_eq!(
            placement.clip,
            Letterbox {
                x: 20,
                y: 10,
                width: 6,
                height: 6
            }
        );
        assert!(cursor_placement(&position, &shape, 8, (100, 100), geometry).is_none());
        assert!(cursor_placement(&position, &shape, 7, (101, 100), geometry).is_none());
        position.visible = false;
        assert!(cursor_placement(&position, &shape, 7, (100, 100), geometry).is_none());
        position.visible = true;
        position.x = i32::MIN;
        assert!(cursor_placement(&position, &shape, 7, (100, 100), geometry).is_none());
        position.x = i32::MAX;
        assert!(cursor_placement(&position, &shape, 7, (100, 100), geometry).is_none());
    }

    #[test]
    fn cursor_motion_reuses_argb_source_and_queues_without_readback() {
        assert!(cursor_source_needs_upload(None, 1));
        assert!(!cursor_source_needs_upload(Some(1), 1));
        assert!(cursor_source_needs_upload(Some(1), 2));
        let source = include_str!("x11_viewer.rs");
        let paint = source
            .split("fn paint_cursor(&mut self)")
            .nth(1)
            .unwrap()
            .split("fn copy_backing")
            .next()
            .unwrap();
        assert!(!paint.contains("XImage::get"));
        assert!(!paint.contains(".reply()"));
        assert!(!paint.contains(".check()"));
        assert!(paint.contains("PictOp::OVER"));
    }

    #[test]
    fn cursor_transform_preserves_clipped_hotspot_at_fractional_scale() {
        let shape = test_cursor_shape(1);
        let position = test_cursor_position(1);
        let geometry = Letterbox {
            x: 20,
            y: 10,
            width: 150,
            height: 150,
        };
        let placement = cursor_placement(&position, &shape, 7, (100, 100), geometry).unwrap();
        let transform = cursor_transform(&placement).unwrap();
        assert_eq!(placement.clip.width, 5);
        assert_eq!(transform.matrix11, 4 * XFIXED_ONE / 6);
        assert_eq!(transform.matrix13, XFIXED_ONE);
        assert_eq!(transform.matrix23, transform.matrix13);
        assert_eq!(transform.matrix33, XFIXED_ONE);
    }

    #[test]
    fn cursor_cache_is_bounded_and_positions_never_regress() {
        let mut plane = CursorPlane::default();
        assert!(plane.accept_position(test_cursor_position(9)));
        assert!(!plane.accept_position(test_cursor_position(9)));
        assert!(!plane.accept_position(test_cursor_position(8)));
        plane.insert_shape(test_cursor_shape(1));
        for id in 2..100 {
            plane.insert_shape(test_cursor_shape(id));
            assert_eq!(plane.shapes.len(), 2);
            assert!(plane.shapes.iter().any(|s| s.id == 1));
            assert!(plane.shapes.iter().any(|s| s.id == id));
        }
        assert!(plane.accept_position(test_cursor_position(10)));
    }

    #[test]
    #[ignore = "requires an isolated X11 server in JWM_TEST_X11_DISPLAY"]
    fn independent_cursor_restores_clean_backing_on_x11() {
        let display = std::env::var("JWM_TEST_X11_DISPLAY").unwrap();
        let mut viewer = Viewer::connect(
            Some(&display),
            100,
            100,
            true,
            false,
            None,
            false,
            "F12",
            "cursor plane test",
        )
        .unwrap();
        viewer.enable_cursor_plane().unwrap();
        viewer
            .draw(DecodedFrame {
                sequence: 1,
                source_width: 100,
                source_height: 100,
                image: RgbImage::from_pixel(100, 100, image::Rgb([10, 20, 30])),
            })
            .unwrap();
        let mut position = test_cursor_position(1);
        position.geometry_epoch = 0;
        position.x = 20;
        position.y = 20;
        viewer
            .set_cursor_update(test_cursor_shape(1), position)
            .unwrap();
        let rect = viewer.cursor_plane.drawn.unwrap();
        let (clean, _) = XImage::get(
            &viewer.conn,
            viewer.backing,
            rect.x as i16,
            rect.y as i16,
            1,
            1,
        )
        .unwrap();
        let (drawn, _) = XImage::get(
            &viewer.conn,
            viewer.window,
            rect.x as i16,
            rect.y as i16,
            1,
            1,
        )
        .unwrap();
        assert_ne!(clean.get_pixel(0, 0), drawn.get_pixel(0, 0));
        position.sequence = 2;
        position.visible = false;
        viewer
            .set_cursor_update(test_cursor_shape(1), position)
            .unwrap();
        let (restored, _) = XImage::get(
            &viewer.conn,
            viewer.window,
            rect.x as i16,
            rect.y as i16,
            1,
            1,
        )
        .unwrap();
        assert_eq!(clean.get_pixel(0, 0), restored.get_pixel(0, 0));
        assert!(viewer.cursor_plane.drawn.is_none());
    }

    #[test]
    fn a_viewer_is_recognised_by_its_peer_label_only() {
        let title = b"ubuntu@desk (DP-0) - JWM Remote - keyboard + pointer; clipboard off";
        assert!(is_viewer_of(title, "ubuntu@desk (DP-0)"));
        // Another monitor, or another machine whose name shares a prefix.
        assert!(!is_viewer_of(title, "ubuntu@desk (DP-2)"));
        assert!(!is_viewer_of(title, "ubuntu@desk"));
        assert!(!is_viewer_of(
            b"ubuntu@desk2 - JWM Remote - view-only",
            "ubuntu@desk"
        ));
    }

    #[test]
    fn a_wide_host_opens_a_window_that_fits_the_local_monitor() {
        // Dual 2560x1440 desktop viewed from a 2560x1600 laptop.
        assert_eq!(fit_initial_window((5120, 1440), (2560, 1600)), (2304, 648));
        // A tall stacked host binds on height instead.
        assert_eq!(fit_initial_window((2560, 2880), (1920, 1080)), (864, 972));
        // A frame that already fits is never enlarged.
        assert_eq!(fit_initial_window((1280, 720), (2560, 1600)), (1280, 720));
        // No usable monitor keeps the old behaviour.
        assert_eq!(fit_initial_window((5120, 1440), (0, 0)), (5120, 1440));
    }

    #[test]
    fn a_new_viewer_is_centred_on_its_monitor() {
        assert_eq!(
            centered_origin((2560, 0, 2560, 1600), (2304, 648)),
            (2688, 476)
        );
        // A window larger than the monitor starts at its corner.
        assert_eq!(
            centered_origin((-1920, 0, 1920, 1080), (4000, 2000)),
            (-1920, 0)
        );
    }

    #[test]
    fn escape_shortcut_requires_ctrl_and_super_but_ignores_locks_and_buttons() {
        let chord = KeyButMask::CONTROL | KeyButMask::MOD4;
        assert!(escape_modifiers_match(chord));
        assert!(escape_modifiers_match(
            chord | KeyButMask::LOCK | KeyButMask::MOD2 | KeyButMask::BUTTON1
        ));
        for state in [
            KeyButMask::default(),
            KeyButMask::CONTROL,
            KeyButMask::MOD4,
            chord | KeyButMask::SHIFT,
            chord | KeyButMask::MOD1,
        ] {
            assert!(!escape_modifiers_match(state), "{state:?}");
        }
    }

    #[test]
    fn plain_escape_key_double_taps_do_not_release_input() {
        for forward_keyboard in [false, true] {
            let mut keyboard = ViewerKeyboardState::default();
            let mut events = Vec::new();
            for time in [1, 2] {
                assert_eq!(
                    keyboard.press(96, false, forward_keyboard),
                    if forward_keyboard {
                        KeyPressAction::Forward
                    } else {
                        KeyPressAction::Ignore
                    }
                );
                keyboard.defer_release(96, time, Instant::now());
                keyboard.flush_release(&mut events, forward_keyboard);
            }
            assert_eq!(events.len(), if forward_keyboard { 2 } else { 0 });
            assert!(events.iter().all(|event| *event
                == ViewerEvent::Key {
                    keycode: 96,
                    pressed: false,
                }));
            assert!(keyboard.held.is_empty());
        }
    }

    #[test]
    fn escape_chord_releases_pointer_only_and_keyboard_sessions() {
        for forward_keyboard in [false, true] {
            let mut keyboard = ViewerKeyboardState::default();
            let mut buttons = ForwardedButtons::default();
            let mut events = Vec::new();
            // Ctrl and Super may already have been sent to the host.
            keyboard.press(37, false, forward_keyboard);
            keyboard.press(133, false, forward_keyboard);
            assert!(buttons.press(1, true));
            assert_eq!(
                keyboard.press(96, true, forward_keyboard),
                KeyPressAction::ReleaseGrab
            );
            // The same boundary used by poll_events releases every remote
            // key/button without forwarding the shortcut's F12 press.
            clear_forwarded_input(&mut keyboard, &mut buttons, &mut events);
            assert_eq!(events, [ViewerEvent::ReleaseAll]);
            assert!(keyboard.held.is_empty());
            assert!(keyboard.pending_release.is_none());
            assert!(!buttons.release(1));
        }
    }

    #[test]
    fn held_plain_escape_key_does_not_release_on_autorepeat() {
        let mut keyboard = ViewerKeyboardState::default();
        let now = Instant::now();
        assert_eq!(keyboard.press(96, false, true), KeyPressAction::Forward);
        let mut events = Vec::new();
        for time in 1..=40 {
            keyboard.defer_release(96, time, now);
            assert!(keyboard.consume_repeat(96, time));
            assert_eq!(keyboard.press(96, false, true), KeyPressAction::Ignore);
            keyboard.flush_release(&mut events, true);
        }
        assert!(events.is_empty());
        keyboard.defer_release(96, 41, now);
        keyboard.flush_release(&mut events, true);
        assert_eq!(
            events,
            [ViewerEvent::Key {
                keycode: 96,
                pressed: false
            }]
        );
    }

    #[test]
    fn pointer_only_non_shortcut_keys_have_no_forwarded_or_repeat_state() {
        let mut keyboard = ViewerKeyboardState::default();
        let now = Instant::now();
        for keycode in [38, 96] {
            assert_eq!(
                keyboard.press(keycode, false, false),
                KeyPressAction::Ignore
            );
            keyboard.defer_release(keycode, 1, now);
            assert!(keyboard.held.is_empty());
            assert!(keyboard.pending_release.is_none());
            assert!(!keyboard.consume_repeat(keycode, 1));
        }
    }

    #[test]
    fn keyboard_repeats_preserve_one_press_and_one_final_release() {
        let mut keyboard = ViewerKeyboardState::default();
        let now = Instant::now();
        assert_eq!(keyboard.press(38, false, true), KeyPressAction::Forward);
        keyboard.defer_release(38, 1, now);
        assert!(!keyboard.consume_repeat(39, 1), "keycodes must match too");
        assert!(keyboard.consume_repeat(38, 1));
        let mut events = Vec::new();
        keyboard.flush_release(&mut events, true);
        assert!(events.is_empty());
        keyboard.defer_release(38, 2, now);
        keyboard.flush_release(&mut events, true);
        keyboard.flush_release(&mut events, true);
        assert_eq!(
            events,
            [ViewerEvent::Key {
                keycode: 38,
                pressed: false,
            }]
        );
        assert!(keyboard.held.is_empty());
    }

    #[test]
    fn external_keyboard_grab_releases_chord_and_discards_deferred_edges() {
        let now = Instant::now();
        let mut keyboard = ViewerKeyboardState::default();
        let mut buttons = ForwardedButtons::default();
        let mut events = Vec::new();
        assert_eq!(keyboard.press(37, false, true), KeyPressAction::Forward);
        assert_eq!(keyboard.press(96, false, true), KeyPressAction::Forward);
        keyboard.defer_release(96, 9, now);
        assert!(buttons.press(1, true));
        // Another client grabs the keyboard while Ctrl + button are held.
        // Releases during that grab will never reach this viewer.
        // poll_events flushes a pending release before matching FocusOut.
        keyboard.flush_release(&mut events, true);
        if focus_out_releases_input(NotifyMode::GRAB, false) {
            clear_forwarded_input(&mut keyboard, &mut buttons, &mut events);
        }
        assert_eq!(
            events,
            [
                ViewerEvent::Key {
                    keycode: 96,
                    pressed: false
                },
                ViewerEvent::ReleaseAll
            ]
        );
        assert!(keyboard.held.is_empty());
        assert!(!keyboard.consume_repeat(96, 9));
        keyboard.flush_release(&mut events, true);
        assert!(!buttons.release(1));
        assert_eq!(
            events,
            [
                ViewerEvent::Key {
                    keycode: 96,
                    pressed: false
                },
                ViewerEvent::ReleaseAll
            ]
        );
        assert_eq!(keyboard.press(37, false, true), KeyPressAction::Forward);
        assert_eq!(keyboard.press(96, false, true), KeyPressAction::Forward);
    }

    #[test]
    fn own_grab_notifications_do_not_release_new_grab() {
        assert!(focus_out_releases_input(NotifyMode::NORMAL, false));
        assert!(focus_out_releases_input(NotifyMode::NORMAL, true));
        assert!(!focus_out_releases_input(NotifyMode::GRAB, true));
        for owns_grab in [false, true] {
            assert!(!focus_out_releases_input(NotifyMode::UNGRAB, owns_grab));
            assert!(!focus_out_releases_input(
                NotifyMode::WHILE_GRABBED,
                owns_grab
            ));
        }
    }

    #[test]
    fn clearing_keyboard_state_discards_pending_release_and_held_keys() {
        let mut keyboard = ViewerKeyboardState::default();
        let now = Instant::now();
        keyboard.press(96, false, true);
        keyboard.defer_release(96, 1, now);
        // This is the same reset used by focus loss, unmap and grab release.
        keyboard = ViewerKeyboardState::default();
        let mut events = Vec::new();
        keyboard.flush_release(&mut events, true);
        assert!(events.is_empty());
        assert_eq!(
            keyboard.press(96, false, true),
            KeyPressAction::Forward,
            "a new grab must not retain a key held during the old grab"
        );
    }

    #[test]
    fn escape_key_names_resolve_through_xkb() {
        assert_eq!(escape_keysym_from_name("F12").unwrap(), 0xffc9);
        // The default shortcut uses F12 as its base key.
        assert_eq!(escape_keysym_from_name(DEFAULT_ESCAPE_KEY).unwrap(), 0xffc9);
        assert!(escape_keysym_from_name("Pause").is_ok());
        assert!(escape_keysym_from_name("Scroll_Lock").is_ok());

        for bad in ["", "NotAKey", "f12 "] {
            assert!(
                escape_keysym_from_name(bad).is_err(),
                "{bad:?} must not resolve to a keysym"
            );
        }
    }

    #[test]
    fn the_backing_is_cleared_only_when_the_letterbox_moves() {
        let geometry = Letterbox {
            x: 4,
            y: 8,
            width: 100,
            height: 50,
        };

        // A fresh or resized pixmap has undefined contents.
        assert!(backing_needs_clear(None, geometry));
        // Steady-state frames must issue no fill at all: the bars are already
        // right and the image rectangle is about to be fully overwritten.
        assert!(!backing_needs_clear(Some(geometry), geometry));

        // Any movement of the image rectangle leaves stale pixels outside it.
        for moved in [
            Letterbox { x: 5, ..geometry },
            Letterbox { y: 9, ..geometry },
            Letterbox {
                width: 99,
                ..geometry
            },
            Letterbox {
                height: 49,
                ..geometry
            },
        ] {
            assert!(
                backing_needs_clear(Some(geometry), moved),
                "{moved:?} must repaint the bars"
            );
        }
    }

    #[test]
    fn letterbox_preserves_aspect_ratio_in_both_directions() {
        assert_eq!(
            letterbox(1000, 1000, 1920, 1080),
            Some(Letterbox {
                x: 0,
                y: 219,
                width: 1000,
                height: 562,
            })
        );
        assert_eq!(
            letterbox(1600, 600, 800, 1200),
            Some(Letterbox {
                x: 600,
                y: 0,
                width: 400,
                height: 600,
            })
        );
        assert_eq!(
            letterbox(640, 360, 1280, 720),
            Some(Letterbox {
                x: 0,
                y: 0,
                width: 640,
                height: 360,
            })
        );
    }

    #[test]
    fn pointer_mapping_uses_remote_source_dimensions() {
        let args = (1000, 1000, 1280, 720, 1920, 1080);
        assert_eq!(
            map_pointer_to_source(0, 219, args.0, args.1, args.2, args.3, args.4, args.5),
            Some((0, 0))
        );
        assert_eq!(
            map_pointer_to_source(999, 780, args.0, args.1, args.2, args.3, args.4, args.5),
            Some((1919, 1079))
        );
        assert_eq!(
            map_pointer_to_source(500, 500, args.0, args.1, args.2, args.3, args.4, args.5),
            Some((960, 540))
        );
    }

    #[test]
    fn black_bar_presses_are_rejected_but_forwarded_drag_releases_survive() {
        let geometry = letterbox(1000, 1000, 1920, 1080).unwrap();
        assert!(!pointer_inside_letterbox(500, 0, geometry));
        assert!(!pointer_inside_letterbox(500, 999, geometry));
        assert!(!pointer_inside_letterbox(-1, 500, geometry));
        assert!(pointer_inside_letterbox(0, 219, geometry));
        assert!(pointer_inside_letterbox(999, 780, geometry));
        let mut buttons = ForwardedButtons::default();
        assert!(!buttons.press(1, pointer_inside_letterbox(500, 0, geometry)));
        assert!(
            !buttons.release(1),
            "a rejected press cannot inject an unmatched release"
        );
        assert!(buttons.press(1, true));
        assert_eq!(
            map_pointer_to_source(-1, 0, 1000, 1000, 1920, 1080, 1920, 1080),
            Some((0, 0))
        );
        assert!(
            buttons.release(1),
            "a forwarded drag must release outside the image"
        );
        assert!(!buttons.release(1));
    }

    #[test]
    fn wheel_repeat_and_release_all_keep_button_tracking_balanced() {
        let mut buttons = ForwardedButtons::default();
        for button in 4..=7 {
            for _ in 0..3 {
                assert!(buttons.press(button, true));
                assert!(buttons.release(button));
            }
            assert!(!buttons.press(button, false));
            assert!(!buttons.release(button));
            assert!(buttons.press(button, true));
            assert!(
                buttons.press(button, true),
                "wheel impulses are never deduplicated as a held press"
            );
            assert!(buttons.release(button));
            assert!(!buttons.release(button));
        }
        assert!(buttons.press(1, true));
        assert!(
            buttons.press(1, true),
            "duplicate press is not a second held count"
        );
        assert!(buttons.release(1));
        assert!(!buttons.release(1));
        assert!(buttons.press(2, true));
        buttons.clear();
        assert!(
            !buttons.release(2),
            "ReleaseAll already discharged this edge"
        );
        assert!(
            !buttons.release(99),
            "unforwarded buttons must not inject a release"
        );
    }

    #[test]
    fn resize_maps_each_pointer_at_its_event_geometry_before_deferred_draw() {
        let before = map_pointer_to_source(500, 500, 1000, 1000, 1280, 800, 2560, 1600).unwrap();
        let after = map_pointer_to_source(500, 500, 800, 1000, 1280, 800, 2560, 1600).unwrap();
        assert_ne!(before, after);
        let mut events = vec![
            ViewerEvent::Pointer {
                x: before.0,
                y: before.1,
            },
            ViewerEvent::Button {
                button: 1,
                pressed: true,
            },
        ];
        push_pointer(&mut events, after.0, after.1);
        assert_eq!(
            events[0],
            ViewerEvent::Pointer {
                x: before.0,
                y: before.1
            }
        );
        assert_eq!(
            events[2],
            ViewerEvent::Pointer {
                x: after.0,
                y: after.1
            }
        );
    }

    #[test]
    fn pointer_mapping_clamps_letterbox_and_negative_coordinates() {
        assert_eq!(
            map_pointer_to_source(-50, 0, 1000, 1000, 1920, 1080, 1920, 1080),
            Some((0, 0))
        );
        assert_eq!(
            map_pointer_to_source(1200, 1000, 1000, 1000, 1920, 1080, 1920, 1080),
            Some((1919, 1079))
        );
    }

    #[test]
    fn empty_geometries_do_not_map() {
        assert_eq!(letterbox(0, 100, 100, 100), None);
        assert_eq!(
            map_pointer_to_source(0, 0, 100, 100, 100, 100, 0, 100),
            None
        );
    }

    #[test]
    fn only_target_window_errors_are_terminal() {
        let error = |error_kind, bad_value| X11Error {
            error_kind,
            error_code: 0,
            sequence: 0,
            bad_value,
            minor_opcode: 0,
            major_opcode: 0,
            extension_name: None,
            request_name: None,
        };

        assert!(is_closed_window_error(&error(ErrorKind::Drawable, 42), 42));
        assert!(is_closed_window_error(&error(ErrorKind::Window, 42), 42));
        assert!(!is_closed_window_error(&error(ErrorKind::Drawable, 7), 42));
        assert!(!is_closed_window_error(&error(ErrorKind::Alloc, 42), 42));
    }

    #[test]
    fn bgrx_fast_path_requires_the_exact_native_visual_and_format() {
        assert_eq!(
            select_native_pixel_writer_from_format(
                24,
                standard_visual(),
                Some(standard_format()),
                X11ImageOrder::LSB_FIRST,
            ),
            NativePixelWriter::Bgrx32
        );

        let mut unusual_visual = standard_visual();
        unusual_visual.red_mask = 0x0000_00ff;
        unusual_visual.blue_mask = 0x00ff_0000;
        assert_eq!(
            select_native_pixel_writer_from_format(
                24,
                unusual_visual,
                Some(standard_format()),
                X11ImageOrder::LSB_FIRST,
            ),
            NativePixelWriter::Generic
        );
        assert_eq!(
            select_native_pixel_writer_from_format(
                24,
                standard_visual(),
                Some(standard_format()),
                X11ImageOrder::MSB_FIRST,
            ),
            NativePixelWriter::Generic
        );
        assert_eq!(
            select_native_pixel_writer_from_format(
                24,
                standard_visual(),
                Some(Format {
                    bits_per_pixel: 24,
                    ..standard_format()
                }),
                X11ImageOrder::LSB_FIRST,
            ),
            NativePixelWriter::Generic
        );
        assert_eq!(
            select_native_pixel_writer_from_format(
                32,
                standard_visual(),
                None,
                X11ImageOrder::LSB_FIRST,
            ),
            NativePixelWriter::Generic
        );
    }

    #[test]
    fn scaled_source_damage_requires_exact_cached_predecessor() {
        let key = RenderSourceKey {
            sequence: 7,
            width: 640,
            height: 480,
        };
        let damage = FrameDamage {
            base_sequence: 7,
            regions: DamageRegions::from_rect(Some((20, 30, 16, 16))),
        };
        assert!(render_source_accepts_damage(key, damage, (640, 480)));
        assert!(!render_source_accepts_damage(
            RenderSourceKey { sequence: 6, ..key },
            damage,
            (640, 480)
        ));
        assert!(!render_source_accepts_damage(key, damage, (800, 600)));
        assert!(render_source_accepts_damage(
            key,
            FrameDamage {
                regions: DamageRegions::from_rect(None),
                ..damage
            },
            (640, 480)
        ));
    }

    #[test]
    fn scaled_damage_covers_every_triangle_changed_pixel() {
        // Exhaust all single-pixel changes in small odd/even geometries,
        // including enlargement, shrinking, edges and one-pixel axes.
        for (sw, sh) in [(1, 1), (1, 7), (9, 1), (7, 9), (8, 6)] {
            for (dw, dh) in [(1, 1), (1, 5), (6, 1), (3, 4), (11, 13), (8, 6)] {
                let baseline = RgbImage::from_fn(sw, sh, |x, y| {
                    image::Rgb([(x * 23) as u8, (y * 19) as u8, 71])
                });
                let before = image::imageops::resize(
                    &baseline,
                    dw,
                    dh,
                    image::imageops::FilterType::Triangle,
                );
                for y in 0..sh {
                    for x in 0..sw {
                        let mut changed = baseline.clone();
                        changed.put_pixel(x, y, image::Rgb([255, 255, 255]));
                        let after = image::imageops::resize(
                            &changed,
                            dw,
                            dh,
                            image::imageops::FilterType::Triangle,
                        );
                        let (rx, ry, rw, rh) = scaled_damage_rect(
                            (x as u16, y as u16, 1, 1),
                            (sw as u16, sh as u16),
                            (dw as u16, dh as u16),
                        );
                        let mut patched = before.clone();
                        for py in u32::from(ry)..u32::from(ry + rh) {
                            for px in u32::from(rx)..u32::from(rx + rw) {
                                patched.put_pixel(px, py, *after.get_pixel(px, py));
                            }
                        }
                        assert_eq!(patched, after, "{sw}x{sh} -> {dw}x{dh}, changed ({x},{y})");
                    }
                }
            }
        }
    }

    #[test]
    fn multiple_scaled_regions_reconstruct_full_triangle_output() {
        let baseline = RgbImage::from_pixel(101, 73, image::Rgb([17, 29, 43]));
        let mut changed = baseline.clone();
        let mut source = DamageRegions::default();
        for (x, y) in [(0, 0), (20, 30), (24, 30), (100, 72)] {
            changed.put_pixel(x, y, image::Rgb([255, 191, 0]));
            source.add((x as u16, y as u16, 1, 1), (101, 73));
        }
        assert_eq!(source.as_slice().len(), 4);
        for (dw, dh) in [(101, 73), (49, 35), (203, 147), (1, 1)] {
            let mut patched =
                image::imageops::resize(&baseline, dw, dh, image::imageops::FilterType::Triangle);
            let after =
                image::imageops::resize(&changed, dw, dh, image::imageops::FilterType::Triangle);
            let mut regions = DamageRegions::default();
            for &rect in source.as_slice() {
                regions.add(
                    scaled_damage_rect(rect, (101, 73), (dw as u16, dh as u16)),
                    (dw as u16, dh as u16),
                );
            }
            let mut sampler = TrianglePatch::default();
            let destination = (dw as u16, dh as u16);
            let partial = sampler.prepare(changed.dimensions(), destination, regions.as_slice());
            for &rect @ (x, y, w, h) in regions.as_slice() {
                let patch = partial.then(|| sampler.render(&changed, destination, rect));
                for py in 0..u32::from(h) {
                    for px in 0..u32::from(w) {
                        let pixel = match patch {
                            Some(image) => *image.get_pixel(px, py),
                            None => *after.get_pixel(u32::from(x) + px, u32::from(y) + py),
                        };
                        patched.put_pixel(u32::from(x) + px, u32::from(y) + py, pixel);
                    }
                }
            }
            assert_eq!(patched, after);
        }
    }

    #[test]
    fn scaled_damage_is_bounded_and_identity_is_exact() {
        let rect = (100, 200, 16, 16);
        assert_eq!(scaled_damage_rect(rect, (1920, 1080), (1920, 1080)), rect);
        let (_, _, w, h) = scaled_damage_rect(rect, (1920, 1080), (1280, 720));
        assert!(w < 20 && h < 20);
        assert_eq!(
            scaled_damage_rect((0, 0, 1920, 1080), (1920, 1080), (1280, 720)),
            (0, 0, 1280, 720)
        );
    }

    #[test]
    fn damage_region_matches_full_conversion_without_extra_pixels() {
        let rgb = RgbImage::from_fn(7, 5, |x, y| image::Rgb([x as u8 * 20, y as u8 * 30, 99]));
        let layout = PixelLayout::from_visual_type(standard_visual()).unwrap();
        let mut fast = XImage::allocate(
            3,
            2,
            ScanlinePad::Pad32,
            24,
            BitsPerPixel::B32,
            ImageOrder::LsbFirst,
        );
        let mut generic = fast.clone();
        write_native_region(&mut fast, &rgb, 2, 1, layout, NativePixelWriter::Bgrx32).unwrap();
        write_native_region(&mut generic, &rgb, 2, 1, layout, NativePixelWriter::Generic).unwrap();
        assert_eq!(fast.data(), generic.data());
        assert_eq!(fast.data().len(), 3 * 2 * 4);
        assert_eq!(&fast.data()[0..4], &[99, 30, 40, 0]);
        assert!(
            write_native_region(&mut fast, &rgb, 6, 1, layout, NativePixelWriter::Bgrx32).is_err()
        );
    }

    #[test]
    fn skipped_snapshot_and_geometry_change_reject_incremental_damage() {
        let make = |sequence, source_width| {
            StoredFrame::from_decoded(DecodedFrame {
                sequence,
                source_width,
                source_height: 4,
                image: RgbImage::new(4, 4),
            })
        };
        let previous = make(7, 4);
        let next = make(9, 4);
        assert!(!damage_base_matches(
            &previous,
            &next,
            FrameDamage {
                base_sequence: 8,
                regions: DamageRegions::from_rect(None)
            }
        ));
        assert!(damage_base_matches(
            &previous,
            &next,
            FrameDamage {
                base_sequence: 7,
                regions: DamageRegions::from_rect(Some((0, 0, 1, 1)))
            }
        ));
        let mut moved_source = make(9, 4);
        moved_source.geometry_epoch = previous.geometry_epoch + 1;
        assert!(!damage_base_matches(
            &previous,
            &moved_source,
            FrameDamage {
                base_sequence: 7,
                regions: DamageRegions::from_rect(Some((0, 0, 1, 1))),
            }
        ));
        assert!(!damage_base_matches(
            &previous,
            &make(9, 8),
            FrameDamage {
                base_sequence: 7,
                regions: DamageRegions::from_rect(None)
            }
        ));
    }

    #[test]
    fn bgrx_fast_path_is_byte_exact_with_pixel_layout_encoding() {
        let rgb = RgbImage::from_raw(
            3,
            2,
            vec![
                0, 0, 0, 255, 255, 255, 17, 34, 51, 99, 1, 240, 128, 64, 32, 7, 201, 42,
            ],
        )
        .unwrap();
        let layout = PixelLayout::from_visual_type(standard_visual()).unwrap();
        let mut fast = XImage::allocate(
            3,
            2,
            ScanlinePad::Pad32,
            24,
            BitsPerPixel::B32,
            ImageOrder::LsbFirst,
        );
        let mut generic = fast.clone();
        write_native_pixels(&mut fast, &rgb, layout, NativePixelWriter::Bgrx32).unwrap();
        write_native_pixels(&mut generic, &rgb, layout, NativePixelWriter::Generic).unwrap();

        assert_eq!(fast.data(), generic.data());
        assert_eq!(&fast.data()[8..12], &[51, 34, 17, 0]);
        assert_eq!(&fast.data()[12..16], &[240, 1, 99, 0]);
    }

    #[test]
    fn native_image_storage_is_reused_only_for_matching_geometry() {
        let setup = Setup {
            image_byte_order: X11ImageOrder::LSB_FIRST,
            pixmap_formats: vec![standard_format()],
            ..Default::default()
        };
        let mut image = None;
        ensure_native_image(&mut image, 3, 2, 24, &setup)
            .unwrap()
            .data_mut()[0] = 91;
        assert_eq!(
            ensure_native_image(&mut image, 3, 2, 24, &setup)
                .unwrap()
                .data()[0],
            91
        );

        let replacement = ensure_native_image(&mut image, 2, 2, 24, &setup).unwrap();
        assert_eq!((replacement.width(), replacement.height()), (2, 2));
        assert_eq!(replacement.data()[0], 0);
    }

    #[test]
    fn expose_copies_valid_backing_while_new_frames_and_resizes_repopulate() {
        assert_eq!(backing_work(true), BackingWork::CopyOnly);
        assert_eq!(backing_work(false), BackingWork::PopulateAndCopy);
    }

    #[test]
    fn expose_source_lease_survives_until_viewer_replacement_and_close() {
        let pool = new_decode_buffer_pool();
        let recyclable = |sequence, value| {
            RecyclableDecodedFrame::from_decoded(
                DecodedFrame {
                    sequence,
                    source_width: 2,
                    source_height: 1,
                    image: RgbImage::from_pixel(2, 1, image::Rgb([value, 2, 3])),
                },
                Arc::clone(&pool),
            )
        };
        let mut slot = Some(StoredFrame::from_recyclable(recyclable(1, 10)));

        // Expose copies the retained backing, and a resize can still inspect
        // the last RGB frame. Neither operation returns its lease early.
        assert_eq!(backing_work(true), BackingWork::CopyOnly);
        assert_eq!(slot.as_ref().unwrap().image().get_pixel(0, 0).0[0], 10);
        assert_eq!(decode_pool_snapshot(&pool).0, 0);

        replace_stored_frame(&mut slot, StoredFrame::from_recyclable(recyclable(2, 20)));
        assert_eq!(slot.as_ref().unwrap().sequence, 2);
        assert_eq!(decode_pool_snapshot(&pool).0, 1);

        // report_close uses this same helper before emitting Close, returning
        // the retained allocation even if subsequent input cleanup fails.
        clear_stored_frame(&mut slot);
        assert!(slot.is_none());
        assert_eq!(decode_pool_snapshot(&pool).0, 2);
    }

    #[test]
    fn render_upscale_requires_strict_growth_on_both_axes() {
        let geometry = |width, height| Letterbox {
            x: 0,
            y: 0,
            width,
            height,
        };
        assert!(should_use_render_upscale(geometry(1920, 1080), 1280, 720));
        assert!(!should_use_render_upscale(geometry(1280, 720), 1280, 720));
        assert!(!should_use_render_upscale(geometry(1920, 720), 1280, 720));
        assert!(!should_use_render_upscale(geometry(640, 360), 1280, 720));
    }

    #[test]
    fn render_requires_version_zero_ten_or_newer() {
        assert!(!supports_render_version(0, 9));
        assert!(supports_render_version(0, 10));
        assert!(supports_render_version(0, 11));
        assert!(supports_render_version(1, 0));
    }

    #[test]
    fn render_source_cache_reuploads_frames_but_replaces_only_for_geometry() {
        let first = RenderSourceKey {
            sequence: 7,
            width: 1280,
            height: 720,
        };
        assert!(render_source_needs_upload(None, first));
        assert!(render_source_needs_replacement(None, first));
        assert!(!render_source_needs_upload(Some(first), first));
        assert!(!render_source_needs_replacement(Some(first), first));

        let next_frame = RenderSourceKey {
            sequence: 8,
            ..first
        };
        assert!(render_source_needs_upload(Some(first), next_frame));
        assert!(!render_source_needs_replacement(Some(first), next_frame));

        let resized_source = RenderSourceKey {
            width: 960,
            height: 540,
            ..next_frame
        };
        assert!(render_source_needs_upload(Some(next_frame), resized_source));
        assert!(render_source_needs_replacement(
            Some(next_frame),
            resized_source
        ));
    }

    #[test]
    fn render_transform_maps_destination_back_to_encoded_source() {
        let transform = scale_transform(1280, 720, 1920, 1080).unwrap();
        assert_eq!(transform.matrix11, 43_691);
        assert_eq!(transform.matrix22, 43_691);
        assert_eq!(transform.matrix33, XFIXED_ONE);
        assert_eq!(transform.matrix12, 0);
        assert_eq!(transform.matrix21, 0);

        assert!(render_transform_needs_update(None, (1920, 1080)));
        assert!(!render_transform_needs_update(
            Some((1920, 1080)),
            (1920, 1080)
        ));
        assert!(render_transform_needs_update(
            Some((1920, 1080)),
            (2560, 1440)
        ));
    }

    #[test]
    fn render_is_disabled_only_after_same_frame_cpu_fallback_succeeds() {
        let disables = Cell::new(0);
        finish_render_fallback(Ok::<_, &'static str>(()), Some("render"), |_| {
            disables.set(disables.get() + 1);
        })
        .unwrap();
        assert_eq!(disables.get(), 1);

        let result = finish_render_fallback(Err("core"), Some("render"), |_| {
            disables.set(disables.get() + 1);
        });
        assert_eq!(result, Err("core"));
        assert_eq!(disables.get(), 1);
    }

    #[test]
    fn shm_fd_upload_requires_protocol_version_one_two() {
        assert!(!supports_shm_fd_version(0, 99));
        assert!(!supports_shm_fd_version(1, 1));
        assert!(supports_shm_fd_version(1, 2));
        assert!(supports_shm_fd_version(1, 3));
        assert!(supports_shm_fd_version(2, 0));
    }

    #[test]
    fn shm_upload_size_uses_native_stride_and_only_grows() {
        let standard = standard_format();
        assert_eq!(native_buffer_size(1, 1, standard).unwrap(), 4);
        assert_eq!(native_buffer_size(3, 2, standard).unwrap(), 24);

        let padded_16 = Format {
            depth: 16,
            bits_per_pixel: 16,
            scanline_pad: 32,
        };
        // Three 16-bit pixels occupy 48 bits and round up to an 8-byte row.
        assert_eq!(native_buffer_size(3, 2, padded_16).unwrap(), 16);
        assert!(native_buffer_size(0, 2, padded_16).is_err());
        assert!(native_buffer_size(2, 0, padded_16).is_err());
        assert!(native_buffer_size(u16::MAX, u16::MAX, standard).is_err());
        assert!(
            native_buffer_size(
                1,
                1,
                Format {
                    scanline_pad: 24,
                    ..standard
                }
            )
            .is_err()
        );
        assert!(
            native_buffer_size(
                1,
                1,
                Format {
                    bits_per_pixel: 12,
                    ..standard
                }
            )
            .is_err()
        );

        assert!(shm_needs_growth(None, 24));
        assert!(!shm_needs_growth(Some(24), 24));
        assert!(!shm_needs_growth(Some(48), 24));
        assert!(shm_needs_growth(Some(23), 24));
    }

    #[test]
    fn shm_upload_requires_an_exact_native_ximage() {
        let format = standard_format();
        let image = XImage::allocate(
            3,
            2,
            ScanlinePad::Pad32,
            24,
            BitsPerPixel::B32,
            ImageOrder::LsbFirst,
        );
        assert_eq!(
            validate_native_upload(&image, format, ImageOrder::LsbFirst).unwrap(),
            24
        );
        assert!(validate_native_upload(&image, format, ImageOrder::MsbFirst).is_err());
        assert!(
            validate_native_upload(
                &image,
                Format {
                    depth: 32,
                    ..format
                },
                ImageOrder::LsbFirst
            )
            .is_err()
        );

        // Image::new accepts trailing bytes, while ShmPutImage must use the
        // exact native stride*height region promised to the server.
        let oversized = XImage::new(
            3,
            2,
            ScanlinePad::Pad32,
            24,
            BitsPerPixel::B32,
            ImageOrder::LsbFirst,
            Cow::Owned(vec![0; 28]),
        )
        .unwrap();
        assert!(validate_native_upload(&oversized, format, ImageOrder::LsbFirst).is_err());
    }

    #[test]
    #[ignore = "requires a dedicated JWM_TEST_X11_DISPLAY Xvfb server"]
    fn source_switch_preserves_pending_shm_ownership_on_x11() {
        let display = std::env::var("JWM_TEST_X11_DISPLAY")
            .expect("set JWM_TEST_X11_DISPLAY to an isolated Xvfb server");
        let mut viewer = Viewer::connect(
            Some(&display),
            640,
            480,
            true,
            false,
            None,
            false,
            "F12",
            "source switch SHM test",
        )
        .unwrap();
        assert!(
            matches!(viewer.shm_upload, ShmUpload::Enabled(_)),
            "this regression requires MIT-SHM FD support"
        );
        viewer.set_source_label("Old".into());
        let pool = super::super::frame::new_decode_buffer_pool();
        let frame = |sequence, source_epoch| {
            RecyclableDecodedFrame::new(
                sequence,
                640,
                480,
                RgbImage::from_pixel(640, 480, image::Rgb([source_epoch as u8, 40, 70])),
                Arc::clone(&pool),
            )
            .with_source_epoch(source_epoch)
            .with_geometry_epoch(1)
        };
        for sequence in 1..=3 {
            assert!(viewer.draw_recyclable(frame(sequence, 1)).unwrap());
        }
        let owned = viewer.shm_upload.pending_keys();
        assert_eq!(owned.len(), SHM_UPLOAD_SLOTS);
        let segments = match &viewer.shm_upload {
            ShmUpload::Enabled(upload) => upload
                .slots
                .iter()
                .map(|slot| (slot.segment.id, slot.segment.mapping.len()))
                .collect::<Vec<_>>(),
            ShmUpload::Disabled => unreachable!(),
        };
        let serials = viewer
            .presentations
            .iter()
            .map(|pending| pending.serial)
            .collect::<Vec<_>>();
        assert!(viewer.take_presented_sequences().unwrap().is_empty());

        // No completion event has been consumed. Invalidating the source must
        // suppress old ACKs without detaching, freeing, or recycling X-owned slots.
        viewer.invalidate_source("New".into()).unwrap();
        assert_eq!(viewer.shm_upload.pending_keys(), owned);
        assert_eq!(
            viewer
                .presentations
                .iter()
                .map(|pending| pending.serial)
                .collect::<Vec<_>>(),
            serials
        );
        assert!(
            viewer
                .presentations
                .iter()
                .all(|pending| pending.sequence.is_none())
        );
        assert!(viewer.last_frame.is_none());
        assert!(!viewer.can_submit_frame());
        assert!(viewer.source_switching);
        assert!(!viewer.forwarding_input());
        assert!(!viewer.grabbed);
        match &viewer.shm_upload {
            ShmUpload::Enabled(upload) => assert_eq!(
                upload
                    .slots
                    .iter()
                    .map(|slot| (slot.segment.id, slot.segment.mapping.len()))
                    .collect::<Vec<_>>(),
                segments
            ),
            ShmUpload::Disabled => panic!("source invalidation disabled live SHM slots"),
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while !viewer.presentations.is_empty() {
            viewer.poll_events().unwrap();
            assert!(
                viewer.take_presented_sequences().unwrap().is_empty(),
                "an obsolete source frame must never produce an ACK candidate"
            );
            assert!(Instant::now() < deadline, "old source completion timed out");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(viewer.shm_upload.pending_keys().is_empty());
        assert!(viewer.can_submit_frame());
        assert!(viewer.draw_recyclable(frame(4, 2)).unwrap());
        assert!(!viewer.forwarding_input());
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            viewer.poll_events().unwrap();
            let completed = viewer.take_presented_sequences().unwrap();
            if !completed.is_empty() {
                assert_eq!(completed, [4]);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "new source presentation timed out"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        match &viewer.last_frame.as_ref().unwrap().image {
            StoredImage::Recyclable(frame) => assert_eq!(frame.source_epoch(), 2),
            StoredImage::Owned(_) => panic!("new source lost its scoped frame"),
        }
        // Mirror the client's ACK-after-completion step, not mere decode/submit.
        viewer.source_presented(1);
        assert!(!viewer.source_switching);
        assert_eq!(viewer.source_label.as_deref(), Some("New"));
        assert!(viewer.forwarding_input());
        assert!(!viewer.grabbed);
        assert!(viewer.shm_upload.pending_keys().is_empty());
    }

    #[test]
    #[ignore = "requires a dedicated JWM_TEST_X11_DISPLAY Xvfb server"]
    fn asynchronous_shm_frames_complete_in_order_on_x11() {
        let display = std::env::var("JWM_TEST_X11_DISPLAY")
            .expect("set JWM_TEST_X11_DISPLAY to an isolated Xvfb server");
        let mut viewer = Viewer::connect(
            Some(&display),
            640,
            480,
            true,
            false,
            None,
            false,
            "F12",
            "async SHM test",
        )
        .unwrap();
        assert!(
            matches!(viewer.shm_upload, ShmUpload::Enabled(_)),
            "this regression requires MIT-SHM FD support"
        );
        // Establish a presented geometry before testing forwarded input while
        // later frames occupy the asynchronous ring.
        assert!(
            viewer
                .draw(DecodedFrame {
                    sequence: 0,
                    source_width: 640,
                    source_height: 480,
                    image: RgbImage::from_pixel(640, 480, image::Rgb([0, 40, 70])),
                })
                .unwrap()
        );
        for sequence in 1..=3 {
            assert!(viewer.can_submit_frame());
            assert!(
                viewer
                    .draw_stored(StoredFrame::from_decoded(DecodedFrame {
                        sequence,
                        source_width: 640,
                        source_height: 480,
                        image: RgbImage::from_pixel(640, 480, image::Rgb([sequence as u8, 40, 70])),
                    }))
                    .unwrap()
            );
        }
        assert!(!viewer.can_submit_frame());
        assert_eq!(viewer.shm_upload.pending_keys().len(), SHM_UPLOAD_SLOTS);
        assert!(viewer.take_presented_sequences().unwrap().is_empty());
        // Input must still be serviced while all three upload slots are owned
        // by X11. Put this before completions in the local event FIFO.
        viewer.deferred_events.push_front(Event::MotionNotify(
            x11rb::protocol::xproto::MotionNotifyEvent {
                event: viewer.window,
                event_x: 100,
                event_y: 100,
                ..Default::default()
            },
        ));
        let mut input_seen = false;
        let mut completed = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while completed.len() < 3 {
            input_seen |= viewer
                .poll_events()
                .unwrap()
                .iter()
                .any(|event| matches!(event, ViewerEvent::Pointer { .. }));
            completed.extend(viewer.take_presented_sequences().unwrap());
            assert!(
                Instant::now() < deadline,
                "presentation completion timed out"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(input_seen);
        assert_eq!(completed, [1, 2, 3]);
        assert!(viewer.can_submit_frame());
        assert!(viewer.shm_upload.pending_keys().is_empty());
        // Resize invalidates the retained backing but never frees a busy SHM
        // mapping. A fourth frame exercises reusable pool slots and redraw.
        viewer.width = 800;
        viewer.height = 600;
        viewer.pending_resize = true;
        assert!(
            viewer
                .draw_stored(StoredFrame::from_decoded(DecodedFrame {
                    sequence: 4,
                    source_width: 640,
                    source_height: 480,
                    image: RgbImage::from_pixel(640, 480, image::Rgb([80, 90, 100])),
                }))
                .unwrap()
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            viewer.poll_events().unwrap();
            let completed = viewer.take_presented_sequences().unwrap();
            if !completed.is_empty() {
                assert_eq!(completed, [4]);
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        // The public API preserves its synchronous contract and is not stuck
        // after the internal ring's three-slot capacity.
        for sequence in 5..=8 {
            assert!(
                viewer
                    .draw(DecodedFrame {
                        sequence,
                        source_width: 640,
                        source_height: 480,
                        image: RgbImage::from_pixel(640, 480, image::Rgb([80, 90, 100])),
                    })
                    .unwrap()
            );
            assert!(viewer.presentations.is_empty());
        }
        // A real server rejection has no SHM completion. It must retire only
        // that request and repaint through CPU/core, not strand the ring or ACK
        // an Expose-only repaint as a new network frame.
        viewer
            .shm_upload
            .upload_or_core(
                &viewer.conn,
                viewer.native_image.as_ref().unwrap(),
                viewer.backing,
                x11rb::NONE,
                0,
                0,
                &mut viewer.deferred_events,
            )
            .unwrap();
        viewer.queue_presentation(None).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !viewer.presentations.is_empty() {
            viewer.poll_events().unwrap();
            assert!(viewer.take_presented_sequences().unwrap().is_empty());
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(matches!(viewer.shm_upload, ShmUpload::Disabled));
        assert!(matches!(viewer.render_upscale, RenderUpscale::Disabled));
    }

    #[test]
    fn injected_accelerator_errors_release_only_the_rejected_request() {
        let key = ShmCompletionKey {
            sequence: 71,
            drawable: 10,
            minor_event: u16::from(shm::PUT_IMAGE_REQUEST),
            major_event: 130,
            segment: 20,
            offset: 0,
        };
        let error = X11Error {
            error_kind: ErrorKind::Match,
            error_code: 8,
            sequence: key.sequence,
            bad_value: 0,
            minor_opcode: key.minor_event,
            major_opcode: key.major_event,
            extension_name: None,
            request_name: None,
        };
        let mut rejected = Some(key);
        let mut still_owned = Some(ShmCompletionKey {
            sequence: 72,
            segment: 21,
            ..key
        });
        assert!(!reject_upload_key(&mut still_owned, &error));
        assert!(still_owned.is_some());
        assert!(reject_upload_key(&mut rejected, &error));
        assert!(rejected.is_none());
        assert!(!reject_upload_key(&mut rejected, &error));
        assert!(!presentation_ready(
            true,
            &[key, still_owned.unwrap()],
            |candidate| Some(candidate) == still_owned
        ));
        let render = AcceleratorRequest {
            sequence: 99,
            major: 131,
            minor: u16::from(render::COMPOSITE_REQUEST),
        };
        let render_error = X11Error {
            sequence: render.sequence,
            major_opcode: render.major,
            minor_opcode: render.minor,
            ..error.clone()
        };
        assert!(render.matches(&render_error));
        assert!(!render.matches(&error));
        assert!(!render.matches(&X11Error {
            sequence: 100,
            ..render_error.clone()
        }));
        assert!(!render.matches(&X11Error {
            minor_opcode: 0,
            ..render_error.clone()
        }));
        assert!(!render.matches(&X11Error {
            major_opcode: 132,
            ..render_error
        }));
    }

    #[test]
    fn presentation_waits_for_both_copy_fence_and_owned_uploads() {
        let key = ShmCompletionKey {
            sequence: 71,
            drawable: 10,
            minor_event: u16::from(shm::PUT_IMAGE_REQUEST),
            major_event: 130,
            segment: 20,
            offset: 0,
        };
        assert!(!presentation_ready(false, &[], |_| false));
        assert!(presentation_ready(true, &[], |_| false));
        assert!(!presentation_ready(false, &[key], |_| false));
        assert!(!presentation_ready(true, &[key], |_| true));
        assert!(presentation_ready(true, &[key], |_| false));
        let later = ShmCompletionKey {
            sequence: 72,
            ..key
        };
        assert!(!presentation_ready(true, &[key, later], |pending| pending == later));
        // A later frame's unrelated slot must not delay this frame's ACK.
        assert!(presentation_ready(true, &[key], |pending| pending == later));
    }

    #[test]
    fn shm_completion_matching_is_strict() {
        let key = ShmCompletionKey {
            sequence: 71,
            drawable: 10,
            minor_event: u16::from(shm::PUT_IMAGE_REQUEST),
            major_event: 130,
            segment: 20,
            offset: 0,
        };
        let event = shm::CompletionEvent {
            sequence: key.sequence,
            drawable: key.drawable,
            minor_event: key.minor_event,
            major_event: key.major_event,
            shmseg: key.segment,
            offset: key.offset,
            ..Default::default()
        };
        assert!(is_matching_shm_completion(&event, key));
        for changed in [
            shm::CompletionEvent {
                sequence: 72,
                ..event
            },
            shm::CompletionEvent {
                drawable: 11,
                ..event
            },
            shm::CompletionEvent {
                minor_event: 4,
                ..event
            },
            shm::CompletionEvent {
                major_event: 131,
                ..event
            },
            shm::CompletionEvent {
                shmseg: 21,
                ..event
            },
            shm::CompletionEvent { offset: 4, ..event },
        ] {
            assert!(!is_matching_shm_completion(&changed, key));
        }
    }

    #[test]
    fn recoverable_shm_failure_uses_core_before_disabling() {
        let core_calls = Cell::new(0);
        let outcome = resolve_upload(None, || {
            core_calls.set(core_calls.get() + 1);
            Ok::<_, &'static str>(())
        });
        assert_eq!(core_calls.get(), 1);
        assert!(matches!(outcome, UploadOutcome::Uploaded));

        let core_calls = Cell::new(0);
        let outcome = resolve_upload(Some(ShmUploadAttempt::Uploaded), || {
            core_calls.set(core_calls.get() + 1);
            Ok::<_, &'static str>(())
        });
        assert_eq!(core_calls.get(), 0);
        assert!(matches!(outcome, UploadOutcome::Uploaded));

        let core_calls = Cell::new(0);
        let outcome = resolve_upload(Some(ShmUploadAttempt::Recoverable("shm")), || {
            core_calls.set(core_calls.get() + 1);
            Ok::<_, &'static str>(())
        });
        assert_eq!(core_calls.get(), 1);
        assert!(matches!(
            outcome,
            UploadOutcome::CoreFallback { shm_error: "shm" }
        ));

        let core_calls = Cell::new(0);
        let outcome = resolve_upload(Some(ShmUploadAttempt::Recoverable("shm")), || {
            core_calls.set(core_calls.get() + 1);
            Err::<(), _>("core")
        });
        assert_eq!(core_calls.get(), 1);
        assert!(matches!(outcome, UploadOutcome::Error("core")));

        let core_calls = Cell::new(0);
        let outcome = resolve_upload(Some(ShmUploadAttempt::Fatal("connection")), || {
            core_calls.set(core_calls.get() + 1);
            Ok::<_, &'static str>(())
        });
        assert_eq!(core_calls.get(), 0);
        assert!(matches!(outcome, UploadOutcome::Error("connection")));
    }
    #[test]
    fn late_clipboard_shortcut_or_sticky_intent_prevents_reconnect() {
        assert!(local_retry_intent_allows(&[], false, false, false, false));
        assert!(!local_retry_intent_allows(
            &[ViewerEvent::NarrowClipboard],
            false,
            false,
            false,
            false
        ));
        assert!(!local_retry_intent_allows(&[], false, false, true, false));
        assert!(!local_retry_intent_allows(
            &[ViewerEvent::ReleaseAll],
            false,
            false,
            true,
            false
        ));
    }
}

#[cfg(test)]
mod scoped_pointer_coordinate_tests {
    use super::*;
    #[test]
    fn scoped_pointer_rejects_black_bars_and_outside_without_clamping() {
        assert!(map_scoped_pointer_to_source(-1, 50, 100, 100, 100, 50, 200, 100).is_none());
        assert!(map_scoped_pointer_to_source(50, 0, 100, 100, 100, 50, 200, 100).is_none());
        assert!(map_scoped_pointer_to_source(50, 99, 100, 100, 100, 50, 200, 100).is_none());
        assert!(map_scoped_pointer_to_source(100, 50, 100, 100, 100, 50, 200, 100).is_none());
        assert_eq!(
            map_scoped_pointer_to_source(0, 25, 100, 100, 100, 50, 200, 100),
            Some((0, 0))
        );
        assert_eq!(
            map_scoped_pointer_to_source(99, 74, 100, 100, 100, 50, 200, 100),
            Some((199, 99))
        );
    }
}
