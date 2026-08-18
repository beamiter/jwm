//! Read the final X11 compositor surface through the Composite overlay.
//!
//! This is intentionally kept in the out-of-process LAN MVP.  A slow encoder
//! or peer can therefore never stall JWM's display event loop.  Both the
//! x11rb and xcb JWM backends render into the same X Composite overlay, so one
//! small X11 client covers both transports.

use super::RemoteResult;
use image::{RgbImage, imageops::FilterType};
use std::io;
use x11rb::connection::Connection;
use x11rb::image::{Image as XImage, PixelLayout};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, PropMode, Screen, VisualClass, Visualtype,
    Window, WindowClass,
};
use x11rb::protocol::{composite, xfixes};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

const COMPOSITE_CLIENT_VERSION: (u32, u32) = (0, 4);
const COMPOSITE_OVERLAY_VERSION: (u32, u32) = (0, 3);
const XFIXES_CLIENT_VERSION: (u32, u32) = (5, 0);
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
                let _ = composite::release_overlay_window(&self.conn, self.root);
                let _ = self.conn.flush();
                self.overlay_acquired = false;
                self.drawable = self.root;
                self.capture_drawable(self.root)
            }
            Err(error) => Err(error),
        }
    }

    fn capture_drawable(&mut self, drawable: Window) -> RemoteResult<CapturedFrame> {
        let screen = self.screen()?;
        let geometry = self.conn.get_geometry(self.root)?.reply()?;
        let source_width = geometry.width;
        let source_height = geometry.height;
        if source_width == 0 || source_height == 0 {
            return Err(invalid_data("X11 root has an empty geometry").into());
        }
        super::frame::validate_dimensions(source_width, source_height)?;

        let (ximage, visual_id) =
            XImage::get(&self.conn, drawable, 0, 0, source_width, source_height)?;
        let visual = find_visual(screen, visual_id)
            .ok_or_else(|| invalid_data("X11 capture visual is not described by the screen"))?;
        if visual.class != VisualClass::TRUE_COLOR {
            return Err(invalid_data("remote capture requires an X11 TrueColor visual").into());
        }
        let layout = PixelLayout::from_visual_type(visual)?;

        let pixel_count = usize::from(source_width)
            .checked_mul(usize::from(source_height))
            .ok_or_else(|| invalid_data("X11 capture dimensions overflow"))?;
        let rgb_bytes = pixel_count
            .checked_mul(3)
            .ok_or_else(|| invalid_data("X11 capture buffer size overflow"))?;
        let mut rgb = Vec::new();
        rgb.try_reserve_exact(rgb_bytes)
            .map_err(|_| invalid_data("could not allocate the X11 capture buffer"))?;
        for y in 0..source_height {
            for x in 0..source_width {
                let (red, green, blue) = layout.decode(ximage.get_pixel(x, y));
                rgb.extend_from_slice(&[(red >> 8) as u8, (green >> 8) as u8, (blue >> 8) as u8]);
            }
        }

        let mut image = RgbImage::from_raw(u32::from(source_width), u32::from(source_height), rgb)
            .ok_or_else(|| invalid_data("X11 capture returned an invalid pixel buffer"))?;
        if self.cursor_available {
            match xfixes::get_cursor_image(&self.conn)?.reply() {
                Ok(cursor) => composite_cursor(&mut image, &cursor),
                Err(error) => {
                    eprintln!("remote: cursor capture stopped: {error}");
                    self.cursor_available = false;
                }
            }
        }
        let (output_width, output_height) =
            scaled_dimensions(source_width, source_height, self.max_width);
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
        if self.overlay_acquired {
            let _ = composite::release_overlay_window(&self.conn, self.root);
            let _ = self.conn.flush();
            self.overlay_acquired = false;
        }
        self.drawable = self.root;
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

impl Drop for X11Capture {
    fn drop(&mut self) {
        self.release_overlay();
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
}
