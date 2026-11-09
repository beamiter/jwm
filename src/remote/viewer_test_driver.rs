//! CI-only XTEST driver for the runner's private Xvfb, never ambient DISPLAY.
use super::RemoteResult;
use std::time::{Duration, Instant};
use x11rb::CURRENT_TIME;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    self, AtomEnum, ClientMessageEvent, ConfigureWindowAux, ConnectionExt as _, EventMask,
    ImageFormat, InputFocus, Window,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

pub(super) struct ViewerTestDriver {
    conn: RustConnection,
    root: Window,
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
        Ok(Self { conn, root })
    }
    fn until<T>(&self, mut poll: impl FnMut() -> RemoteResult<Option<T>>) -> RemoteResult<T> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(value) = poll()? {
                return Ok(value);
            }
            if Instant::now() >= deadline {
                return Err("viewer UI/presentation wait timed out".into());
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
        Ok(String::from_utf8(reply.value)?)
    }
    pub(super) fn wait_window(&self, peer_label: &str) -> RemoteResult<Window> {
        let prefix = format!("{peer_label} - JWM Remote");
        self.until(|| {
            let mut found = None;
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
        self.until(|| Ok(self.title(window)?.contains(substring).then_some(())))
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
        self.until(|| {
            if !self.title(window)?.contains("Pointer: Ready") {
                return Ok(None);
            }
            let reply = self
                .conn
                .get_property(false, window, atom, AtomEnum::CARDINAL, 0, 2)?
                .reply()?;
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
        self.until(|| {
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
            Ok((channel(visual.red_mask) >= 220
                && channel(visual.green_mask) <= 30
                && channel(visual.blue_mask) <= 30)
                .then_some(()))
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
    pub(super) fn click_at_current(&self) -> RemoteResult<()> {
        self.conn
            .xtest_fake_input(
                xproto::BUTTON_PRESS_EVENT,
                1,
                CURRENT_TIME,
                self.root,
                0,
                0,
                0,
            )?
            .check()?;
        self.conn
            .xtest_fake_input(
                xproto::BUTTON_RELEASE_EVENT,
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
    pub(super) fn focus(&self, window: Window) -> RemoteResult<()> {
        self.conn
            .set_input_focus(InputFocus::POINTER_ROOT, window, CURRENT_TIME)?
            .check()?;
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
