//! CI-only XTEST driver for the runner's private Xvfb, never ambient DISPLAY.
use super::RemoteResult;
use std::time::{Duration, Instant};
use x11rb::CURRENT_TIME;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    self, AtomEnum, ClientMessageEvent, ConfigureWindowAux, ConnectionExt as _, EventMask,
    ImageFormat, InputFocus, MapState, Window,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

#[derive(Clone, Copy, Debug)]
pub(super) struct PointerObservation {
    pub(super) candidate_frame: Option<u64>,
    pub(super) left_down: bool,
    pub(super) action_sequence: u64,
}

pub(super) struct ViewerTestDriver {
    conn: RustConnection,
    root: Window,
    display: String,
    last_observed: std::cell::RefCell<String>,
}
impl ViewerTestDriver {
    pub(super) fn connect() -> RemoteResult<Self> {
        if std::env::var("JWM_REMOTE_TEST_VIEWER_OWNED").as_deref() != Ok("1") {
            return Err("viewer tests require the owned-Xvfb runner".into());
        }
        let display = std::env::var("JWM_REMOTE_TEST_VIEWER_DISPLAY")?;
        if !display.starts_with(':') {
            return Err("viewer test display must be local".into());
        }
        let (conn, screen) = x11rb::connect(Some(&display))?;
        let root = conn.setup().roots[screen].root;
        conn.xtest_get_version(2, 2)?.reply()?;
        Ok(Self {
            conn,
            root,
            display,
            last_observed: std::cell::RefCell::new(String::new()),
        })
    }
    pub(super) fn snapshot_keymap(&self) -> RemoteResult<String> {
        use xkbcommon::xkb;
        let (conn, _) = xcb::Connection::connect_with_extensions(
            Some(&self.display),
            &[xcb::Extension::Xkb],
            &[],
        )?;
        let (mut major, mut minor, mut event, mut error) = (0, 0, 0, 0);
        if !xkb::x11::setup_xkb_extension(
            &conn,
            1,
            0,
            xkb::x11::SetupXkbExtensionFlags::NoFlags,
            &mut major,
            &mut minor,
            &mut event,
            &mut error,
        ) {
            return Err("owned XKB unavailable".into());
        }
        let device = xkb::x11::get_core_keyboard_device_id(&conn);
        if device < 0 {
            return Err("owned core keyboard unavailable".into());
        }
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let map =
            xkb::x11::keymap_new_from_device(&context, &conn, device, xkb::KEYMAP_COMPILE_NO_FLAGS);
        if map.get_raw_ptr().is_null() {
            return Err("owned keymap unavailable".into());
        }
        let text = map.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
        super::keyboard_keymap::canonical_digest(text.as_bytes())?;
        Ok(text)
    }
    pub(super) fn assert_keys_released(&self) -> RemoteResult<()> {
        let reply = self.conn.query_keymap()?.reply()?;
        if reply.keys.iter().any(|byte| *byte != 0) {
            return Err("owned XTEST keyboard still has pressed keys before authorization".into());
        }
        Ok(())
    }
    pub(super) fn key_press(&self, evdev: u16) -> RemoteResult<()> {
        self.key(evdev, true)
    }
    pub(super) fn key_release(&self, evdev: u16) -> RemoteResult<()> {
        self.key(evdev, false)
    }
    fn key(&self, evdev: u16, pressed: bool) -> RemoteResult<()> {
        let code = u8::try_from(evdev.checked_add(8).ok_or("keycode overflow")?)?;
        self.conn
            .xtest_fake_input(
                if pressed {
                    xproto::KEY_PRESS_EVENT
                } else {
                    xproto::KEY_RELEASE_EVENT
                },
                code,
                CURRENT_TIME,
                self.root,
                0,
                0,
                0,
            )?
            .check()?;
        Ok(())
    }
    pub(super) fn wait_keyboard_armed(&self, window: Window) -> RemoteResult<()> {
        self.wait_keyboard_state(window, 1)
    }
    pub(super) fn wait_keyboard_revoked(&self, window: Window) -> RemoteResult<()> {
        self.wait_keyboard_state(window, 2)
    }
    fn wait_keyboard_state(&self, window: Window, expected: u32) -> RemoteResult<()> {
        let atom = self
            .conn
            .intern_atom(false, b"_JWM_TEST_KEYBOARD_STATE")?
            .reply()?
            .atom;
        self.until("keyboard state", || {
            let reply = self
                .conn
                .get_property(false, window, atom, AtomEnum::CARDINAL, 0, 1)?
                .reply()?;
            *self.last_observed.borrow_mut() = format!(
                "window={window:#x}, expected keyboard={expected}, type={}, format={}, data={:?}",
                reply.type_, reply.format, reply.value
            );
            if reply.type_ == u32::from(AtomEnum::NONE) {
                return Ok(None);
            }
            if reply.format != 32 || reply.value.len() != 4 || reply.bytes_after != 0 {
                return Err("invalid keyboard test observation".into());
            }
            Ok(
                (reply.value32().and_then(|mut words| words.next()) == Some(expected))
                    .then_some(()),
            )
        })
    }
    fn until<T>(
        &self,
        phase: &str,
        mut poll: impl FnMut() -> RemoteResult<Option<T>>,
    ) -> RemoteResult<T> {
        self.last_observed.borrow_mut().clear();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(value) = poll()? {
                return Ok(value);
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "viewer wait {phase} timed out; last observed: {}",
                    self.last_observed.borrow()
                )
                .into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    pub(super) fn title(&self, window: Window) -> RemoteResult<String> {
        let reply = self
            .conn
            .get_property(false, window, AtomEnum::WM_NAME, AtomEnum::ANY, 0, 1024)?
            .reply()?;
        if reply.bytes_after != 0 {
            return Err("oversized viewer title".into());
        }
        let title = String::from_utf8(reply.value)?;
        *self.last_observed.borrow_mut() = format!("window={window:#x}, WM_NAME={title:?}");
        Ok(title)
    }
    pub(super) fn wait_window(&self, peer_label: &str) -> RemoteResult<Window> {
        let prefix = format!("{peer_label} - JWM Remote");
        self.until("window discovery", || {
            let mut found = None;
            *self.last_observed.borrow_mut() = format!("no root child matches prefix={prefix:?}");
            for window in self.conn.query_tree(self.root)?.reply()?.children {
                if self.title(window)?.starts_with(&prefix) {
                    if found.is_some() {
                        return Err("ambiguous viewer peer label".into());
                    }
                    found = Some(window);
                }
            }
            Ok(found)
        })
    }
    pub(super) fn wait_title(&self, window: Window, substring: &str) -> RemoteResult<()> {
        self.until("title substring", || {
            Ok(self.title(window)?.contains(substring).then_some(()))
        })
    }
    pub(super) fn wait_ready_after(
        &self,
        window: Window,
        previous: Option<u64>,
    ) -> RemoteResult<u64> {
        let atom = self
            .conn
            .intern_atom(false, b"_JWM_TEST_POINTER_READY")?
            .reply()?
            .atom;
        self.until("pointer Ready frame", || {
            if !self.title(window)?.contains("Pointer: Ready") {
                return Ok(None);
            }
            let reply = self
                .conn
                .get_property(false, window, atom, AtomEnum::CARDINAL, 0, 2)?
                .reply()?;
            *self.last_observed.borrow_mut() = format!("window={window:#x}, previous={previous:?}, Ready property type={}, format={}, data={:?}", reply.type_, reply.format, reply.value);
            if reply.type_ == u32::from(AtomEnum::NONE) {
                return Ok(None);
            }
            if reply.format != 32 || reply.value.len() != 8 || reply.bytes_after != 0 {
                return Err("invalid test Ready observation".into());
            }
            let mut words = reply.value32().ok_or("missing Ready words")?;
            let sequence = u64::from(words.next().ok_or("missing Ready low word")?)
                | (u64::from(words.next().ok_or("missing Ready high word")?) << 32);
            Ok(previous
                .is_none_or(|old| sequence > old)
                .then_some(sequence))
        })
    }
    /// Read the real X drawable, not the decoded frame or an acknowledgement.
    pub(super) fn wait_red(&self, window: Window) -> RemoteResult<()> {
        self.wait_color(window, [true, false, false])
    }
    pub(super) fn wait_green(&self, window: Window) -> RemoteResult<()> {
        self.wait_color(window, [false, true, false])
    }
    fn wait_color(&self, window: Window, high: [bool; 3]) -> RemoteResult<()> {
        self.until("expected actual framebuffer color", || {
            let geometry = self.conn.get_geometry(window)?.reply()?;
            let image = self
                .conn
                .get_image(
                    ImageFormat::Z_PIXMAP,
                    window,
                    (geometry.width / 2) as i16,
                    (geometry.height / 2) as i16,
                    1,
                    1,
                    u32::MAX,
                )?
                .reply()?;
            let format = self
                .conn
                .setup()
                .pixmap_formats
                .iter()
                .find(|f| f.depth == image.depth)
                .ok_or("missing pixel format")?;
            if format.bits_per_pixel != 32 || image.data.len() != 4 {
                return Err("test requires 32-bit packed pixels".into());
            }
            let bytes: [u8; 4] = image.data.try_into().map_err(|_| "invalid pixel")?;
            let pixel = if self.conn.setup().image_byte_order == xproto::ImageOrder::LSB_FIRST {
                u32::from_le_bytes(bytes)
            } else {
                u32::from_be_bytes(bytes)
            };
            let visual = self
                .conn
                .setup()
                .roots
                .iter()
                .flat_map(|r| &r.allowed_depths)
                .flat_map(|d| &d.visuals)
                .find(|v| v.visual_id == image.visual)
                .ok_or("missing visual")?;
            let channel = |mask: u32| {
                if mask == 0 {
                    0
                } else {
                    (pixel & mask) >> mask.trailing_zeros()
                }
            };
            *self.last_observed.borrow_mut() = format!("window={window:#x}, size={}x{}, depth={}, bpp={}, pixel={pixel:#010x}, masks=({:#x},{:#x},{:#x}), rgb=({},{},{})", geometry.width, geometry.height, image.depth, format.bits_per_pixel, visual.red_mask, visual.green_mask, visual.blue_mask, channel(visual.red_mask), channel(visual.green_mask), channel(visual.blue_mask));
            let rgb = [channel(visual.red_mask), channel(visual.green_mask), channel(visual.blue_mask)];
            Ok(rgb.into_iter().zip(high).all(|(value, high)| if high { value >= 220 } else { value <= 30 }).then_some(()))
        })
    }
    pub(super) fn motion(&self, window: Window, x: i16, y: i16) -> RemoteResult<()> {
        let point = self
            .conn
            .translate_coordinates(window, self.root, x, y)?
            .reply()?;
        self.conn
            .xtest_fake_input(
                xproto::MOTION_NOTIFY_EVENT,
                0,
                CURRENT_TIME,
                self.root,
                point.dst_x,
                point.dst_y,
                0,
            )?
            .check()?;
        Ok(())
    }
    pub(super) fn pointer_observation(
        &self,
        window: Window,
    ) -> RemoteResult<Option<PointerObservation>> {
        let atom = self
            .conn
            .intern_atom(false, b"_JWM_TEST_POINTER_CANDIDATE")?
            .reply()?
            .atom;
        let reply = self
            .conn
            .get_property(false, window, atom, AtomEnum::CARDINAL, 0, 6)?
            .reply()?;
        if reply.type_ == u32::from(AtomEnum::NONE) {
            return Ok(None);
        }
        if reply.format != 32 || reply.value.len() != 24 || reply.bytes_after != 0 {
            return Err("invalid pointer candidate observation".into());
        }
        let words: Vec<u32> = reply.value32().ok_or("missing candidate words")?.collect();
        if words[0] > 1 || words[3] > 1 {
            return Err("invalid candidate flags".into());
        }
        *self.last_observed.borrow_mut() = format!("window={window:#x}, candidate words={words:?}");
        Ok(Some(PointerObservation {
            candidate_frame: (words[0] == 1)
                .then_some(u64::from(words[1]) | (u64::from(words[2]) << 32)),
            left_down: words[3] == 1,
            action_sequence: u64::from(words[4]) | (u64::from(words[5]) << 32),
        }))
    }
    pub(super) fn wait_pointer_candidate(
        &self,
        window: Window,
        frame: u64,
    ) -> RemoteResult<PointerObservation> {
        self.until("pointer candidate", || {
            Ok(self
                .pointer_observation(window)?
                .filter(|s| s.left_down && s.candidate_frame == Some(frame)))
        })
    }
    pub(super) fn wait_pointer_released(
        &self,
        window: Window,
        expected_sequence: u64,
    ) -> RemoteResult<PointerObservation> {
        self.until("pointer released", || {
            Ok(self.pointer_observation(window)?.filter(|s| {
                !s.left_down
                    && s.candidate_frame.is_none()
                    && s.action_sequence == expected_sequence
            }))
        })
    }
    pub(super) fn button_down(&self) -> RemoteResult<()> {
        self.button(true)
    }
    pub(super) fn button_up(&self) -> RemoteResult<()> {
        self.button(false)
    }
    fn button(&self, pressed: bool) -> RemoteResult<()> {
        self.conn
            .xtest_fake_input(
                if pressed {
                    xproto::BUTTON_PRESS_EVENT
                } else {
                    xproto::BUTTON_RELEASE_EVENT
                },
                1,
                CURRENT_TIME,
                self.root,
                0,
                0,
                0,
            )?
            .check()?;
        Ok(())
    }
    /// Balanced wheel edges only on the explicitly owned Xvfb connection.
    pub(super) fn wheel_notches(&self, up: bool, count: u8) -> RemoteResult<()> {
        self.wheel_buttons(if up { 4 } else { 5 }, count)
    }
    /// Horizontal physical detents, injected only into this owned test server.
    pub(super) fn horizontal_notches(&self, left: bool, count: u8) -> RemoteResult<()> {
        self.wheel_buttons(if left { 6 } else { 7 }, count)
    }
    fn wheel_buttons(&self, button: u8, count: u8) -> RemoteResult<()> {
        if !(1..=8).contains(&count) {
            return Err("owned wheel batch must contain 1..8 notches".into());
        }
        let mut cookies = Vec::new();
        for _ in 0..count {
            for pressed in [true, false] {
                cookies.push(self.conn.xtest_fake_input(
                    if pressed {
                        xproto::BUTTON_PRESS_EVENT
                    } else {
                        xproto::BUTTON_RELEASE_EVENT
                    },
                    button,
                    CURRENT_TIME,
                    self.root,
                    0,
                    0,
                    0,
                )?);
            }
        }
        self.conn.flush()?;
        for cookie in cookies {
            cookie.check()?;
        }
        Ok(())
    }
    pub(super) fn wait_scroll_observed(&self, window: Window, minimum: u64) -> RemoteResult<u64> {
        let atom = self
            .conn
            .intern_atom(false, b"_JWM_TEST_SCROLL_OBSERVED")?
            .reply()?
            .atom;
        self.until("viewer processed wheel notches", || {
            let reply = self
                .conn
                .get_property(false, window, atom, AtomEnum::CARDINAL, 0, 2)?
                .reply()?;
            if reply.type_ == u32::from(AtomEnum::NONE) {
                return Ok(None);
            }
            if reply.format != 32 || reply.value.len() != 8 || reply.bytes_after != 0 {
                return Err("invalid scroll observation".into());
            }
            let words: Vec<_> = reply.value32().ok_or("missing scroll words")?.collect();
            let count = u64::from(words[0]) | (u64::from(words[1]) << 32);
            *self.last_observed.borrow_mut() =
                format!("scroll observed={count}, minimum={minimum}");
            Ok((count >= minimum).then_some(count))
        })
    }
    pub(super) fn click_at_current(&self) -> RemoteResult<()> {
        self.button_down()?;
        self.button_up()
    }
    pub(super) fn focus(&self, window: Window) -> RemoteResult<()> {
        // Discovery observes WM_NAME before viewer initialization necessarily
        // reaches MapWindow (for example while XKB is still initializing).
        // Focus only the exact discovered owned window after the X server
        // confirms it and its ancestors are Viewable. Never rediscover another
        // target, swallow BadWindow, or retry a failed SetInputFocus request.
        self.until("exact owned window Viewable before focus", || {
            let attrs = self.conn.get_window_attributes(window)?.reply()?;
            *self.last_observed.borrow_mut() =
                format!("window={window:#x}, map_state={:?}", attrs.map_state);
            Ok((attrs.map_state == MapState::VIEWABLE).then_some(()))
        })?;
        let pointer = self.conn.query_pointer(self.root)?.reply()?;
        let focus = self.conn.get_input_focus()?.reply()?;
        eprintln!(
            "owned focus before request window={window:#x}: pointer=({},{}), pointer_child={:#x}, focus={:#x}, revert_to={:?}",
            pointer.root_x, pointer.root_y, pointer.child, focus.focus, focus.revert_to
        );
        self.conn
            .set_input_focus(InputFocus::POINTER_ROOT, window, CURRENT_TIME)?
            .check()?;
        Ok(())
    }
    /// Deterministic owned-X startup: keep PointerRoot/ancestor pointer focus
    /// outside the future viewer before it is mapped and explicitly focused.
    pub(super) fn initialize_outside_focus(&self) -> RemoteResult<()> {
        let geometry = self.conn.get_geometry(self.root)?.reply()?;
        let x = i16::try_from(geometry.width.checked_sub(1).ok_or("empty root")?)?;
        let y = i16::try_from(geometry.height.checked_sub(1).ok_or("empty root")?)?;
        self.motion(self.root, x, y)?;
        self.unfocus()?;
        let pointer = self.conn.query_pointer(self.root)?.reply()?;
        let focus = self.conn.get_input_focus()?.reply()?;
        eprintln!(
            "owned startup root={:#x}, pointer=({},{}), focus={:#x}, revert_to={:?}",
            self.root, pointer.root_x, pointer.root_y, focus.focus, focus.revert_to
        );
        if (pointer.root_x, pointer.root_y) != (x, y) || focus.focus != self.root {
            return Err("owned startup focus/pointer setup not confirmed".into());
        }
        Ok(())
    }
    pub(super) fn unfocus(&self) -> RemoteResult<()> {
        self.focus(self.root)
    }
    pub(super) fn resize(&self, window: Window, width: u16, height: u16) -> RemoteResult<()> {
        self.conn
            .configure_window(
                window,
                &ConfigureWindowAux::new()
                    .width(u32::from(width))
                    .height(u32::from(height)),
            )?
            .check()?;
        Ok(())
    }
    pub(super) fn resize_restore(
        &self,
        window: Window,
        width: u16,
        height: u16,
        restore_width: u16,
        restore_height: u16,
    ) -> RemoteResult<()> {
        self.conn.grab_server()?.check()?;
        let configured = (|| -> RemoteResult<()> {
            // Queue both mutations before any reply wait. The viewer cannot
            // interleave X requests while this short server grab is active.
            let first = self.conn.configure_window(
                window,
                &ConfigureWindowAux::new()
                    .width(u32::from(width))
                    .height(u32::from(height)),
            )?;
            let second = self.conn.configure_window(
                window,
                &ConfigureWindowAux::new()
                    .width(u32::from(restore_width))
                    .height(u32::from(restore_height)),
            )?;
            self.conn.flush()?;
            first.check()?;
            second.check()?;
            Ok(())
        })();
        let ungrabbed = self.conn.ungrab_server()?.check();
        configured?;
        ungrabbed?;
        Ok(())
    }
    pub(super) fn close(&self, window: Window) -> RemoteResult<()> {
        let protocols = self.conn.intern_atom(false, b"WM_PROTOCOLS")?.reply()?.atom;
        let delete = self
            .conn
            .intern_atom(false, b"WM_DELETE_WINDOW")?
            .reply()?
            .atom;
        self.conn
            .send_event(
                false,
                window,
                EventMask::NO_EVENT,
                ClientMessageEvent::new(32, window, protocols, [delete, CURRENT_TIME, 0, 0, 0]),
            )?
            .check()?;
        Ok(())
    }
}
