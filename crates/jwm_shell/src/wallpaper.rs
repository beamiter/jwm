//! Shared wallpaper sampling for native shell and legacy top bar.
use std::{
    rc::Rc,
    time::{Duration, Instant},
};
use x11rb::rust_connection::RustConnection;
use xbar_core::glass::{
    GlassError, GlassImage, StripRequest, WallpaperSource, wallpaper::WallpaperFile,
};
pub fn compositor_active(conn: &RustConnection, screen_num: usize) -> bool {
    use x11rb::protocol::xproto::ConnectionExt as _;
    let name = format!("_NET_WM_CM_S{screen_num}");
    let Ok(cookie) = conn.intern_atom(false, name.as_bytes()) else {
        return false;
    };
    let Ok(atom) = cookie.reply() else {
        return false;
    };
    conn.get_selection_owner(atom.atom)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|reply| reply.owner != x11rb::NONE)
        .unwrap_or(false)
}

pub enum OpaqueWallpaper {
    File(WallpaperFile),
    Root(RootPixmapSource),
    FileThenRoot {
        file: WallpaperFile,
        root: RootPixmapSource,
    },
}

impl OpaqueWallpaper {
    pub fn has_root_pixmap(&self) -> bool {
        match self {
            Self::File(_) => true,
            Self::Root(root) | Self::FileThenRoot { root, .. } => root.has_pixmap(),
        }
    }
}

impl WallpaperSource for OpaqueWallpaper {
    fn revision(&mut self) -> u64 {
        match self {
            Self::File(source) => source.revision(),
            Self::Root(source) => source.revision(),
            Self::FileThenRoot { file, root } => file.revision().wrapping_add(root.revision()),
        }
    }

    fn strip(&mut self, request: &StripRequest) -> Result<GlassImage, GlassError> {
        match self {
            Self::File(source) => source.strip(request),
            Self::Root(source) => source.strip(request),
            Self::FileThenRoot { file, root } => {
                file.strip(request).or_else(|_| root.strip(request))
            }
        }
    }
}

pub struct RootPixmapSource {
    conn: Rc<RustConnection>,
    root: u32,
    xrootpmap: u32,
    esetroot: u32,
    pixmap: u32,
    revision: u64,
    last_probe: Option<Instant>,
}

impl RootPixmapSource {
    pub fn new(conn: Rc<RustConnection>, screen_num: usize) -> Self {
        use x11rb::connection::Connection as _;
        let root = conn
            .setup()
            .roots
            .get(screen_num)
            .map(|screen| screen.root)
            .unwrap_or(0);
        let xrootpmap = intern_atom(&conn, b"_XROOTPMAP_ID");
        let esetroot = intern_atom(&conn, b"ESETROOT_PMAP_ID");
        if root != 0 {
            use x11rb::protocol::xproto::{
                ChangeWindowAttributesAux, ConnectionExt as _, EventMask,
            };
            let _ = conn.change_window_attributes(
                root,
                &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            );
            let _ = conn.flush();
        }
        Self {
            conn,
            root,
            xrootpmap,
            esetroot,
            pixmap: 0,
            revision: 0,
            last_probe: None,
        }
    }

    pub fn has_pixmap(&self) -> bool {
        self.pixmap != 0
    }

    fn pixmap_atom(&self, atom: u32) -> Option<u32> {
        use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _};
        if atom == 0 || self.root == 0 {
            return None;
        }
        let reply = self
            .conn
            .get_property(false, self.root, atom, AtomEnum::PIXMAP, 0, 1)
            .ok()?
            .reply()
            .ok()?;
        reply.value32()?.next().filter(|pixmap| *pixmap != 0)
    }

    fn current_pixmap(&self) -> Option<u32> {
        self.pixmap_atom(self.xrootpmap)
            .or_else(|| self.pixmap_atom(self.esetroot))
    }

    fn refresh_pixmap(&mut self) {
        let now = Instant::now();
        if self
            .last_probe
            .is_some_and(|last| now.saturating_duration_since(last) < Duration::from_millis(200))
        {
            return;
        }
        self.last_probe = Some(now);
        let pixmap = self.current_pixmap().unwrap_or(0);
        if pixmap != self.pixmap {
            self.pixmap = pixmap;
            self.revision = self.revision.wrapping_add(1);
        }
    }
}

fn intern_atom(conn: &RustConnection, name: &[u8]) -> u32 {
    use x11rb::protocol::xproto::ConnectionExt as _;
    conn.intern_atom(false, name)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|reply| reply.atom)
        .unwrap_or(0)
}

impl WallpaperSource for RootPixmapSource {
    fn revision(&mut self) -> u64 {
        // GlassBackdrop also asks for the revision while painting. Throttle
        // here, so high-rate pointer frames cannot bypass the host's poll
        // cadence and turn every redraw into X11 property round trips.
        self.refresh_pixmap();
        self.revision
    }

    fn strip(&mut self, request: &StripRequest) -> Result<GlassImage, GlassError> {
        use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat};

        self.refresh_pixmap();
        let pixmap = (self.pixmap != 0).then_some(self.pixmap).ok_or_else(|| {
            GlassError::Unavailable("root wallpaper pixmap is not published yet".into())
        })?;
        let geometry = self
            .conn
            .get_geometry(pixmap)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .ok_or_else(|| {
                GlassError::Unavailable("root wallpaper pixmap has no geometry".into())
            })?;
        let pad = request.height.saturating_add(request.pad);
        let x0 = request.x.max(0);
        let y0 = request.y.max(0);
        let x1 = request.x.saturating_add(request.width as i32).max(0);
        let y1 = request.y.saturating_add(pad as i32).max(0);
        let clip_x = x0.min(geometry.width as i32);
        let clip_y = y0.min(geometry.height as i32);
        let clip_right = x1.min(geometry.width as i32);
        let clip_bottom = y1.min(geometry.height as i32);
        if clip_x >= clip_right || clip_y >= clip_bottom {
            return Err(GlassError::EmptyRegion);
        }
        let width = (clip_right - clip_x) as u32;
        let height = (clip_bottom - clip_y) as u32;
        let x = i16::try_from(clip_x).map_err(|_| GlassError::Overflow)?;
        let y = i16::try_from(clip_y).map_err(|_| GlassError::Overflow)?;
        let w = u16::try_from(width).map_err(|_| GlassError::Overflow)?;
        let h = u16::try_from(height).map_err(|_| GlassError::Overflow)?;
        let reply = self
            .conn
            .get_image(ImageFormat::Z_PIXMAP, pixmap, x, y, w, h, u32::MAX)
            .map_err(|error| GlassError::Unavailable(error.to_string()))?
            .reply()
            .map_err(|error| GlassError::Unavailable(error.to_string()))?;
        let stride = if height == 0 {
            width as usize * 4
        } else {
            reply.data.len() / height as usize
        };
        let row = width as usize * 4;
        if stride < row || reply.data.len() < stride.saturating_mul(height as usize) {
            return Err(GlassError::Unavailable(
                "root wallpaper pixmap returned a short image".into(),
            ));
        }
        let mut image = GlassImage::from_bgra(width, height, stride, &reply.data)?;
        for pixel in image.data_mut().as_chunks_mut::<4>().0 {
            pixel[3] = 255;
        }
        Ok(image)
    }
}
