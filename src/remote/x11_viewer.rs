//! Small X11 viewer used by the remote-control client.
//!
//! The viewer uses core X11 plus an optional MIT-SHM upload fast path. That
//! keeps it usable under JWM's x11rb and xcb backends without bringing a GUI
//! toolkit into the remote-control process.

use std::borrow::Cow;
use std::collections::{HashSet, VecDeque};
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::ptr::{self, NonNull};
use std::time::{Duration, Instant};

use image::RgbImage;
use x11rb::CURRENT_TIME;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::errors::ReplyError;
use x11rb::image::{BitsPerPixel, Image as XImage, ImageOrder, PixelLayout, ScanlinePad};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as _, CreateGCAux, CreateWindowAux, Cursor, EventMask, Format,
    Gcontext, GrabMode, GrabStatus, ImageFormat, ImageOrder as X11ImageOrder, Mapping, NotifyMode,
    Pixmap, PropMode, Rectangle, Screen, Setup, VisualClass, Visualtype, Window, WindowClass,
};
use x11rb::protocol::{ErrorKind, Event, shm};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::x11_utils::{TryParseFd, X11Error};

use super::frame::DecodedFrame;
use super::{RemoteError, RemoteResult};

const TITLE: &[u8] = b"JWM Remote";
const WM_CLASS: &[u8] = b"jwm-remote\0JwmRemote\0";
const XK_F12: u32 = 0xffc9;
const KEY_RELEASE_DEFER: Duration = Duration::from_millis(8);
const SHM_FD_VERSION: (u16, u16) = (1, 2);

/// One local viewer operation for the remote-control client to send upstream.
///
/// Keycodes are X11 server keycodes.  `Pointer` coordinates are in the remote
/// source/root coordinate space, even when the viewer window is letterboxed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewerEvent {
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

#[derive(Clone, Debug)]
struct StoredFrame {
    source_width: u16,
    source_height: u16,
    image: RgbImage,
}

#[derive(Clone, Copy, Debug)]
struct PendingKeyRelease {
    keycode: u8,
    time: u32,
    queued_at: Instant,
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

/// Plain X11 window that displays decoded remote frames and collects input.
pub struct Viewer {
    conn: RustConnection,
    window: Window,
    gc: Gcontext,
    backing: Pixmap,
    blank_cursor: Cursor,
    wm_protocols: Atom,
    wm_delete_window: Atom,
    depth: u8,
    pixel_layout: PixelLayout,
    native_pixel_writer: NativePixelWriter,
    native_image: Option<XImage<'static>>,
    shm_upload: ShmUpload,
    width: u16,
    height: u16,
    backing_valid: bool,
    grab_input: bool,
    forward_input: bool,
    forward_keyboard: bool,
    verified_keymap: Option<[u8; 32]>,
    grabbed: bool,
    f12_keycodes: Vec<u8>,
    held_keycodes: HashSet<u8>,
    pending_key_release: Option<PendingKeyRelease>,
    deferred_events: VecDeque<Event>,
    last_frame: Option<StoredFrame>,
    closed: bool,
    close_reported: bool,
}

impl Viewer {
    /// Open an X11 window on `display`, or on `$DISPLAY` when it is `None`.
    pub fn connect(
        display: Option<&str>,
        initial_width: u16,
        initial_height: u16,
        forward_input: bool,
        forward_keyboard: bool,
        verified_keymap: Option<[u8; 32]>,
        grab_input: bool,
    ) -> RemoteResult<Self> {
        if initial_width == 0 || initial_height == 0 {
            return Err(invalid_input("remote viewer dimensions must be nonzero").into());
        }

        let (conn, screen_num) = x11rb::connect(display)?;
        if forward_keyboard {
            let expected = verified_keymap
                .ok_or_else(|| invalid_data("keyboard input has no verified X11 keymap"))?;
            if super::x11_keymap::fingerprint(&conn)? != expected {
                return Err(invalid_data(
                    "local X11 keymap changed during connection; reconnect to negotiate input",
                )
                .into());
            }
        }
        let (root, root_depth, root_visual, black_pixel, pixel_layout, native_pixel_writer) = {
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
            )
        };
        // MIT-SHM requires QueryVersion to complete before any other request
        // from the extension. Segment allocation remains lazy until a frame is
        // actually uploaded.
        let shm_upload = if native_pixel_writer == NativePixelWriter::Bgrx32 {
            ShmUpload::connect(&conn, root_depth)
        } else {
            ShmUpload::Disabled
        };

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
            0,
            0,
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

        conn.change_property8(
            PropMode::REPLACE,
            window,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            TITLE,
        )?
        .check()?;
        conn.change_property8(PropMode::REPLACE, window, net_wm_name, utf8_string, TITLE)?
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
        conn.map_window(window)?.check()?;
        conn.flush()?;

        let f12_keycodes = load_f12_keycodes(&conn)?;
        if grab_input && f12_keycodes.is_empty() {
            return Err(invalid_data(
                "--grab-input requires an unmodified F12 key in the local X11 keymap",
            )
            .into());
        }
        Ok(Self {
            conn,
            window,
            gc,
            backing,
            blank_cursor,
            wm_protocols,
            wm_delete_window,
            depth: root_depth,
            pixel_layout,
            native_pixel_writer,
            native_image: None,
            shm_upload,
            width: initial_width,
            height: initial_height,
            backing_valid: false,
            grab_input,
            forward_input,
            forward_keyboard,
            verified_keymap,
            grabbed: false,
            f12_keycodes,
            held_keycodes: HashSet::new(),
            pending_key_release: None,
            deferred_events: VecDeque::new(),
            last_frame: None,
            closed: false,
            close_reported: false,
        })
    }

    /// Drain all currently queued X11 events without blocking.
    ///
    /// With input grabbing enabled, the first button press grabs the local
    /// keyboard and pointer.  F12 is kept local, releases both grabs, and emits
    /// `ReleaseAll` so the remote host cannot retain a half-finished chord.
    pub fn poll_events(&mut self) -> RemoteResult<Vec<ViewerEvent>> {
        let mut result = Vec::new();
        if self.closed {
            self.report_close(&mut result)?;
            return Ok(result);
        }
        let mut redraw = false;
        let backing_dimensions = (self.width, self.height);

        loop {
            let event = if let Some(event) = self.deferred_events.pop_front() {
                event
            } else if let Some(event) = self.conn.poll_for_event()? {
                event
            } else {
                break;
            };
            if let Event::KeyPress(key) = &event
                && self.pending_key_release.is_some_and(|release| {
                    release.keycode == key.detail && release.time == key.time
                })
            {
                // Core X11 autorepeat is a synthetic release/press pair with
                // identical keycode and timestamp. Keep the remote key held
                // and let the host server generate its own repeat cadence.
                self.pending_key_release = None;
                continue;
            }
            self.flush_pending_key_release(&mut result);
            match event {
                Event::Expose(event) if event.window == self.window => {
                    redraw |= event.count == 0;
                }
                Event::ConfigureNotify(event) if event.window == self.window => {
                    let new_width = event.width.max(1);
                    let new_height = event.height.max(1);
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
                    self.closed = true;
                    self.report_close(&mut result)?;
                    break;
                }
                Event::DestroyNotify(event) if event.window == self.window => {
                    self.closed = true;
                    self.report_close(&mut result)?;
                    break;
                }
                Event::UnmapNotify(event) if event.window == self.window => {
                    self.release_local_grab()?;
                    self.release_all_input(&mut result);
                }
                Event::FocusOut(event)
                    if event.event == self.window && event.mode == NotifyMode::NORMAL =>
                {
                    self.release_local_grab()?;
                    self.release_all_input(&mut result);
                }
                Event::MotionNotify(event)
                    if event.event == self.window && self.forwarding_input() =>
                {
                    if let Some((x, y)) = self.map_pointer(event.event_x, event.event_y) {
                        push_pointer(&mut result, x, y);
                    }
                }
                Event::ButtonPress(event) if event.event == self.window => {
                    if self.grab_input && !self.grabbed && !self.acquire_local_grab()? {
                        continue;
                    }
                    if self.forwarding_input() {
                        if let Some((x, y)) = self.map_pointer(event.event_x, event.event_y) {
                            push_pointer(&mut result, x, y);
                            result.push(ViewerEvent::Button {
                                button: event.detail,
                                pressed: true,
                            });
                        }
                    }
                }
                Event::ButtonRelease(event)
                    if event.event == self.window && self.forwarding_input() =>
                {
                    if let Some((x, y)) = self.map_pointer(event.event_x, event.event_y) {
                        push_pointer(&mut result, x, y);
                        result.push(ViewerEvent::Button {
                            button: event.detail,
                            pressed: false,
                        });
                    }
                }
                Event::KeyPress(event) if event.event == self.window => {
                    if self.grab_input && self.grabbed && self.is_f12(event.detail) {
                        self.release_local_grab()?;
                        self.release_all_input(&mut result);
                    } else if self.forwarding_input()
                        && self.forward_keyboard
                        && !(self.grab_input && self.is_f12(event.detail))
                        && self.held_keycodes.insert(event.detail)
                    {
                        result.push(ViewerEvent::Key {
                            keycode: event.detail,
                            pressed: true,
                        });
                    }
                }
                Event::KeyRelease(event) if event.event == self.window => {
                    // Swallow the release corresponding to the local F12
                    // escape even though ungrabbing can leave keyboard focus
                    // on this window.
                    if self.forwarding_input()
                        && self.forward_keyboard
                        && !(self.grab_input && self.is_f12(event.detail))
                    {
                        self.pending_key_release = Some(PendingKeyRelease {
                            keycode: event.detail,
                            time: event.time,
                            queued_at: Instant::now(),
                        });
                    }
                }
                Event::MappingNotify(event) if event.request != Mapping::POINTER => {
                    if self.forward_keyboard {
                        let expected = self.verified_keymap.ok_or_else(|| {
                            invalid_data("keyboard input has no verified X11 keymap")
                        })?;
                        let current = super::x11_keymap::fingerprint(&self.conn)?;
                        if current != expected {
                            return Err(invalid_data(
                                "local X11 keymap changed; reconnect to re-verify keyboard input",
                            )
                            .into());
                        }
                    }
                    let f12_keycodes = load_f12_keycodes(&self.conn)?;
                    if self.grab_input && f12_keycodes.is_empty() {
                        self.release_local_grab()?;
                        self.release_all_input(&mut result);
                        return Err(invalid_data(
                            "local X11 keymap no longer has an unmodified F12 escape key",
                        )
                        .into());
                    }
                    self.f12_keycodes = f12_keycodes;
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

        if self
            .pending_key_release
            .is_some_and(|release| release.queued_at.elapsed() >= KEY_RELEASE_DEFER)
        {
            self.flush_pending_key_release(&mut result);
        }

        if (self.width, self.height) != backing_dimensions && !self.closed {
            // ConfigureNotify commonly arrives as a burst during interactive
            // resize. Drain the burst first and allocate only its final size.
            self.resize_backing()?;
        }
        if redraw && !self.closed {
            self.redraw()?;
        }
        if self.closed {
            self.report_close(&mut result)?;
        }
        Ok(result)
    }

    /// Draw and retain a decoded frame. The retained copy is used for Expose
    /// events and for rescaling after the window is resized. Returns `false`
    /// when the target window closed before the frame could be presented.
    pub fn draw(&mut self, frame: DecodedFrame) -> RemoteResult<bool> {
        if self.closed {
            return Ok(false);
        }
        validate_frame(&frame)?;
        self.last_frame = Some(StoredFrame {
            source_width: frame.source_width,
            source_height: frame.source_height,
            image: frame.image,
        });
        self.backing_valid = false;
        self.redraw()?;
        Ok(!self.closed)
    }

    /// Whether `keycode` currently resolves to the F12 keysym on this X server.
    #[must_use]
    pub fn is_f12(&self, keycode: u8) -> bool {
        self.f12_keycodes.contains(&keycode)
    }

    fn forwarding_input(&self) -> bool {
        self.forward_input && (!self.grab_input || self.grabbed)
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
        self.conn.flush()?;
        Ok(())
    }

    fn release_for_close(&mut self, events: &mut Vec<ViewerEvent>) -> RemoteResult<()> {
        self.release_local_grab()?;
        self.release_all_input(events);
        Ok(())
    }

    fn flush_pending_key_release(&mut self, events: &mut Vec<ViewerEvent>) {
        if let Some(release) = self.pending_key_release.take()
            && self.held_keycodes.remove(&release.keycode)
        {
            events.push(ViewerEvent::Key {
                keycode: release.keycode,
                pressed: false,
            });
        }
    }

    fn release_all_input(&mut self, events: &mut Vec<ViewerEvent>) {
        self.pending_key_release = None;
        self.held_keycodes.clear();
        events.push(ViewerEvent::ReleaseAll);
    }

    fn map_pointer(&self, x: i16, y: i16) -> Option<(u16, u16)> {
        let frame = self.last_frame.as_ref()?;
        let image_width = u16::try_from(frame.image.width()).ok()?;
        let image_height = u16::try_from(frame.image.height()).ok()?;
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
        let previous = std::mem::replace(&mut self.backing, replacement);
        self.backing_valid = false;
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
        let upload = if let Some(frame) = self.last_frame.as_ref() {
            let image_width = u16::try_from(frame.image.width())
                .map_err(|_| invalid_data("decoded frame width exceeds the X11 protocol range"))?;
            let image_height = u16::try_from(frame.image.height())
                .map_err(|_| invalid_data("decoded frame height exceeds the X11 protocol range"))?;
            let geometry = letterbox(self.width, self.height, image_width, image_height)
                .ok_or_else(|| invalid_data("cannot draw an empty remote frame"))?;
            let resized = if (geometry.width, geometry.height) == (image_width, image_height) {
                Cow::Borrowed(&frame.image)
            } else {
                Cow::Owned(image::imageops::resize(
                    &frame.image,
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
            Some((dst_x, dst_y))
        } else {
            None
        };
        let fill = self.conn.poly_fill_rectangle(
            self.backing,
            self.gc,
            &[Rectangle {
                x: 0,
                y: 0,
                width: self.width,
                height: self.height,
            }],
        )?;
        // Consume the fill's checked result before waiting for an SHM
        // Completion. Otherwise wait_for_event could dequeue this request's
        // X11 error and turn it into an unrelated deferred event.
        fill.check()?;
        if let Some((dst_x, dst_y)) = upload {
            let image = self
                .native_image
                .as_ref()
                .ok_or_else(|| invalid_data("native X11 image buffer was not allocated"))?;
            self.shm_upload.upload_or_core(
                &self.conn,
                image,
                self.backing,
                self.gc,
                dst_x,
                dst_y,
                &mut self.deferred_events,
            )?;
        }
        // Populate the retained pixmap completely before presenting it. If any
        // fill/upload fails, no CopyArea has been queued and the previous
        // window contents remain visible rather than a partially uploaded frame.
        self.conn.flush()?;
        self.backing_valid = true;
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
        self.finish_window_request(copy.check())?;
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
        self.shm_upload.release(&self.conn);
        if self.grabbed {
            let _ = self.conn.ungrab_keyboard(CURRENT_TIME);
            let _ = self.conn.ungrab_pointer(CURRENT_TIME);
        }
        let _ = self.conn.free_cursor(self.blank_cursor);
        let _ = self.conn.flush();
    }
}

enum ShmUpload {
    Disabled,
    Enabled(EnabledShmUpload),
}

impl ShmUpload {
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
            segment: None,
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
    segment: Option<ShmSegment>,
    reported_active: bool,
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
        deferred_events: &mut VecDeque<Event>,
    ) -> ShmUploadAttempt<RemoteError> {
        let image_size = match validate_native_upload(image, self.format, self.image_byte_order) {
            Ok(size) => size,
            Err(error) => return ShmUploadAttempt::Recoverable(error),
        };
        if let Err(error) = self.ensure_capacity(conn, image_size) {
            return ShmUploadAttempt::Recoverable(error);
        }

        let (segment_id, capacity) = {
            let Some(segment) = self.segment.as_mut() else {
                return ShmUploadAttempt::Recoverable(
                    invalid_data("MIT-SHM upload segment was not created").into(),
                );
            };
            if let Err(error) = segment.mapping.copy_from(image.data()) {
                return ShmUploadAttempt::Recoverable(error.into());
            }
            (segment.id, segment.mapping.len())
        };

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
        match cookie.check() {
            Ok(()) => {}
            // A server rejection means it never consumed the shared pixels,
            // so core PutImage may safely retry this frame.
            Err(ReplyError::X11Error(error)) => {
                return ShmUploadAttempt::Recoverable(ReplyError::X11Error(error).into());
            }
            // Connection failure leaves completion/consumption unknowable.
            Err(ReplyError::ConnectionError(error)) => {
                return ShmUploadAttempt::Fatal(error.into());
            }
        }
        if let Err(error) = wait_for_shm_completion(conn, completion, deferred_events) {
            return ShmUploadAttempt::Fatal(error.into());
        }

        if !self.reported_active {
            eprintln!(
                "jwm-remote: MIT-SHM {}.{} FD upload active ({capacity} bytes)",
                self.version.0, self.version.1
            );
            self.reported_active = true;
        }
        ShmUploadAttempt::Uploaded
    }

    fn ensure_capacity(&mut self, conn: &RustConnection, required: usize) -> RemoteResult<()> {
        let current_capacity = self.segment.as_ref().map(|segment| segment.mapping.len());
        if !shm_needs_growth(current_capacity, required) {
            return Ok(());
        }

        // Construct and map the replacement before releasing the old segment.
        // Every previous PutImage completion was consumed before this point.
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ShmCompletionKey {
    sequence: u16,
    drawable: Pixmap,
    minor_event: u16,
    major_event: u8,
    segment: shm::Seg,
    offset: u32,
}

fn is_matching_shm_completion(event: &shm::CompletionEvent, key: ShmCompletionKey) -> bool {
    event.sequence == key.sequence
        && event.drawable == key.drawable
        && event.minor_event == key.minor_event
        && event.major_event == key.major_event
        && event.shmseg == key.segment
        && event.offset == key.offset
}

fn wait_for_shm_completion(
    conn: &RustConnection,
    key: ShmCompletionKey,
    deferred_events: &mut VecDeque<Event>,
) -> Result<(), x11rb::errors::ConnectionError> {
    loop {
        let event = conn.wait_for_event()?;
        if let Event::ShmCompletion(completion) = &event
            && is_matching_shm_completion(completion, key)
        {
            return Ok(());
        }
        // Drawing is synchronous from the caller's perspective, but window,
        // input, and close events can arrive while the server finishes reading
        // the mapping. Preserve their original order for the next poll_events.
        deferred_events.push_back(event);
    }
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
        // Every successful PutImage waits for its matching completion before
        // returning. Check Detach before unmapping and closing the descriptor.
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
    for cookie in cookies {
        cookie.check()?;
    }
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

/// Map local window coordinates through the fitted image rectangle into the
/// original remote source space.  Coordinates in the black bars clamp to the
/// nearest source edge, making pointer motion continuous at the image border.
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

fn load_f12_keycodes(conn: &RustConnection) -> RemoteResult<Vec<u8>> {
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
            (keysyms.first() == Some(&XK_F12))
                .then(|| min.saturating_add(u8::try_from(offset).unwrap_or(u8::MAX)))
        })
        .collect())
}

fn validate_frame(frame: &DecodedFrame) -> RemoteResult<()> {
    if frame.source_width == 0 || frame.source_height == 0 {
        return Err(invalid_data("remote frame has an empty source geometry").into());
    }
    if frame.image.width() == 0 || frame.image.height() == 0 {
        return Err(invalid_data("remote frame has an empty image").into());
    }
    u16::try_from(frame.image.width())
        .map_err(|_| invalid_data("decoded frame width exceeds the X11 protocol range"))?;
    u16::try_from(frame.image.height())
        .map_err(|_| invalid_data("decoded frame height exceeds the X11 protocol range"))?;
    Ok(())
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
    use std::cell::Cell;

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
}
