//! Small X11 viewer used by the remote-control client.
//!
//! The viewer intentionally uses only core X11.  That keeps it usable under
//! JWM's x11rb and xcb backends without bringing a GUI toolkit into the
//! remote-control process.

use std::borrow::Cow;
use std::collections::HashSet;
use std::io;
use std::time::{Duration, Instant};

use image::RgbImage;
use x11rb::CURRENT_TIME;
use x11rb::connection::Connection;
use x11rb::image::{Image as XImage, PixelLayout};
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as _, CreateGCAux, CreateWindowAux, Cursor, EventMask, Gcontext,
    GrabMode, GrabStatus, Mapping, NotifyMode, Pixmap, PropMode, Rectangle, Screen, VisualClass,
    Visualtype, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use super::RemoteResult;
use super::frame::DecodedFrame;

const TITLE: &[u8] = b"JWM Remote";
const WM_CLASS: &[u8] = b"jwm-remote\0JwmRemote\0";
const XK_F12: u32 = 0xffc9;
const KEY_RELEASE_DEFER: Duration = Duration::from_millis(8);

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
    width: u16,
    height: u16,
    grab_input: bool,
    forward_input: bool,
    forward_keyboard: bool,
    verified_keymap: Option<[u8; 32]>,
    grabbed: bool,
    f12_keycodes: Vec<u8>,
    held_keycodes: HashSet<u8>,
    pending_key_release: Option<PendingKeyRelease>,
    last_frame: Option<StoredFrame>,
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
        let (root, root_depth, root_visual, black_pixel, pixel_layout) = {
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
            )
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
            width: initial_width,
            height: initial_height,
            grab_input,
            forward_input,
            forward_keyboard,
            verified_keymap,
            grabbed: false,
            f12_keycodes,
            held_keycodes: HashSet::new(),
            pending_key_release: None,
            last_frame: None,
        })
    }

    /// Drain all currently queued X11 events without blocking.
    ///
    /// With input grabbing enabled, the first button press grabs the local
    /// keyboard and pointer.  F12 is kept local, releases both grabs, and emits
    /// `ReleaseAll` so the remote host cannot retain a half-finished chord.
    pub fn poll_events(&mut self) -> RemoteResult<Vec<ViewerEvent>> {
        let mut result = Vec::new();
        let mut redraw = false;

        while let Some(event) = self.conn.poll_for_event()? {
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
                        self.resize_backing()?;
                        redraw = true;
                    }
                }
                Event::ClientMessage(event)
                    if event.window == self.window
                        && event.format == 32
                        && event.type_ == self.wm_protocols
                        && event.data.as_data32()[0] == self.wm_delete_window =>
                {
                    self.release_for_close(&mut result)?;
                    result.push(ViewerEvent::Close);
                }
                Event::DestroyNotify(event) if event.window == self.window => {
                    self.release_for_close(&mut result)?;
                    result.push(ViewerEvent::Close);
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

        if redraw {
            self.redraw()?;
        }
        Ok(result)
    }

    /// Draw and retain a decoded frame.  The retained copy is used for Expose
    /// events and for rescaling after the window is resized.
    pub fn draw(&mut self, frame: DecodedFrame) -> RemoteResult<()> {
        validate_frame(&frame)?;
        self.last_frame = Some(StoredFrame {
            source_width: frame.source_width,
            source_height: frame.source_height,
            image: frame.image,
        });
        self.redraw()
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
        self.conn
            .create_pixmap(
                self.depth,
                replacement,
                self.window,
                self.width,
                self.height,
            )?
            .check()?;
        let previous = std::mem::replace(&mut self.backing, replacement);
        self.conn.free_pixmap(previous)?.check()?;
        Ok(())
    }

    fn redraw(&self) -> RemoteResult<()> {
        let Some(frame) = self.last_frame.as_ref() else {
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
            fill.check()?;
            copy.check()?;
            self.conn.flush()?;
            return Ok(());
        };
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
        let mut image = XImage::allocate_native(
            geometry.width,
            geometry.height,
            self.depth,
            self.conn.setup(),
        )?;
        for (x, y, rgb) in resized.enumerate_pixels() {
            let pixel = self.pixel_layout.encode((
                u16::from(rgb[0]) * 257,
                u16::from(rgb[1]) * 257,
                u16::from(rgb[2]) * 257,
            ));
            image.put_pixel(x as u16, y as u16, pixel);
        }

        let dst_x = i16::try_from(geometry.x)
            .map_err(|_| invalid_data("letterbox X offset exceeds the X11 coordinate range"))?;
        let dst_y = i16::try_from(geometry.y)
            .map_err(|_| invalid_data("letterbox Y offset exceeds the X11 coordinate range"))?;
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
        let image_cookies = image.put(&self.conn, self.backing, self.gc, dst_x, dst_y)?;
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
        fill.check()?;
        for cookie in image_cookies {
            cookie.check()?;
        }
        copy.check()?;
        self.conn.flush()?;
        Ok(())
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        if self.grabbed {
            let _ = self.conn.ungrab_keyboard(CURRENT_TIME);
            let _ = self.conn.ungrab_pointer(CURRENT_TIME);
        }
        let _ = self.conn.free_cursor(self.blank_cursor);
        let _ = self.conn.flush();
    }
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
}
