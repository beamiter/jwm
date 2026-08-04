use anyhow::{Result, anyhow};
use cairo::ffi::{xcb_connection_t, xcb_visualtype_t};
use cairo::{
    Context, Filter, Format, ImageSurface, Operator, XCBConnection as CairoXCBConnection,
    XCBDrawable, XCBSurface, XCBVisualType,
};
use log::{debug, warn};
use pango::FontDescription;
use std::cell::{Cell, RefCell};
use std::env;
use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd};
use std::time::Duration;
use xbar_core::linux::{AlignedTimer, Epoll};
use xbar_core::presentation::{Point, PointerAction, PresentationLabels, Size};
use xbar_core::render::cairo::CairoBar;
use xbar_core::{
    BarPlacement, BarRuntime, DockProperty, DockPropertyValue, DockWindowSpec, MonitorGeometry,
    NotifierChange, RuntimeUpdate, ThemeMode, TransportNotifierSlot, TransportRecoveryConfig,
};
use xbar_linux_actions::{EffectRouter, GeometryRequest};
use xcb::{self, Xid, x};

const BAR_NAME: &str = "xcb_bar";
const X_TOKEN: u64 = 1;
const TIMER_TOKEN: u64 = 2;
const SHARED_TOKEN: u64 = 3;
const TRANSPORT_RETRY_INTERVAL: Duration = Duration::from_secs(2);

// Frosted-glass parameters approximating the macOS menu-bar material: the
// wallpaper strip behind the bar is downscaled, box-blurred (three passes of
// a box blur converge on a Gaussian), saturation-boosted, and upscaled.
const GLASS_DOWNSCALE: i32 = 4;
const GLASS_BLUR_RADIUS: usize = 6;
const GLASS_BLUR_PASSES: u32 = 3;
const GLASS_SATURATION: f32 = 1.8;
/// Extra wallpaper rows sampled below the bar so the blur has real
/// neighborhood data instead of clamped edge pixels.
const GLASS_PAD: u16 = 48;
/// Background tint applied when the config file does not choose one.
const DEFAULT_BACKGROUND_OPACITY: f64 = 0.55;

// ---------------- Cairo XCB bridge ----------------
struct CairoXcb {
    connection: CairoXCBConnection,
    visual: XCBVisualType,
    _visual_owner: Box<x::Visualtype>,
}

fn find_visual_by_id_and_depth(
    screen: &x::Screen,
    target_visual_id: u32,
    target_depth: u8,
) -> Option<x::Visualtype> {
    for depth in screen.allowed_depths() {
        if depth.depth() == target_depth {
            for visual in depth.visuals() {
                if visual.visual_id() == target_visual_id {
                    return Some(*visual);
                }
            }
        }
    }
    None
}

fn build_cairo_xcb(conn: &xcb::Connection, screen: &x::Screen, visual_id: u32, depth: u8) -> Result<CairoXcb> {
    let visual = find_visual_by_id_and_depth(screen, visual_id, depth)
        .ok_or_else(|| anyhow!("could not find the requested X visual"))?;
    let visual_owner = Box::new(visual);
    let visual_ptr = (&*visual_owner) as *const x::Visualtype as *mut xcb_visualtype_t;
    let visual = unsafe { XCBVisualType::from_raw_none(visual_ptr) };
    let raw_connection = conn.get_raw_conn();
    let connection =
        unsafe { CairoXCBConnection::from_raw_none(raw_connection.cast::<xcb_connection_t>()) };

    Ok(CairoXcb {
        connection,
        visual,
        _visual_owner: visual_owner,
    })
}

// ---------------- Compositor detection ----------------
/// True when a compositing manager owns the conventional `_NET_WM_CM_Sn`
/// selection. With a compositor the bar renders per-pixel alpha through a
/// 32-bit visual and lets the compositor blur what lies behind it; without
/// one it falls back to baking a frosted wallpaper strip itself.
fn compositor_active(conn: &xcb::Connection, screen_num: i32) -> bool {
    let Ok(atom) = intern_atom(conn, &format!("_NET_WM_CM_S{screen_num}")) else {
        return false;
    };
    let cookie = conn.send_request(&x::GetSelectionOwner { selection: atom });
    conn.wait_for_reply(cookie)
        .map(|reply| !reply.owner().is_none())
        .unwrap_or(false)
}

/// A 32-bit TrueColor visual for translucent rendering, if the server has one.
fn find_argb_visual(screen: &x::Screen) -> Option<x::Visualtype> {
    for depth in screen.allowed_depths() {
        if depth.depth() == 32 {
            for visual in depth.visuals() {
                if visual.class() == x::VisualClass::TrueColor {
                    return Some(*visual);
                }
            }
        }
    }
    None
}

// ---------------- Frosted glass ----------------
/// Root-window properties that name the wallpaper pixmap.
struct RootPixmapAtoms {
    xrootpmap: x::Atom,
    esetroot: x::Atom,
}

impl RootPixmapAtoms {
    fn intern(conn: &xcb::Connection) -> Result<Self> {
        Ok(Self {
            xrootpmap: intern_atom(conn, "_XROOTPMAP_ID")?,
            esetroot: intern_atom(conn, "ESETROOT_PMAP_ID")?,
        })
    }

    fn matches(&self, atom: x::Atom) -> bool {
        atom == self.xrootpmap || atom == self.esetroot
    }
}

/// Cached blurred wallpaper strip for the bar's current root-space geometry.
struct GlassCache {
    atoms: RootPixmapAtoms,
    /// Opaque base color used when no wallpaper pixmap is available.
    fallback: (f64, f64, f64),
    key: Option<(i16, i16, u16, u16)>,
    surface: Option<ImageSurface>,
}

impl GlassCache {
    fn new(atoms: RootPixmapAtoms, fallback: (f64, f64, f64)) -> Self {
        Self {
            atoms,
            fallback,
            key: None,
            surface: None,
        }
    }

    fn invalidate(&mut self) {
        self.key = None;
        self.surface = None;
    }

    /// Return the frosted strip for the bar at root-space `(x, y)` with the
    /// given size, rebuilding it only when geometry or wallpaper changed.
    /// Every X error along the way degrades to `None` (solid background).
    #[allow(clippy::too_many_arguments)]
    fn ensure(
        &mut self,
        conn: &xcb::Connection,
        cairo_xcb: &CairoXcb,
        gc: x::Gcontext,
        root: x::Window,
        origin: (i16, i16),
        width: u16,
        height: u16,
    ) -> Option<&ImageSurface> {
        let key = (origin.0, origin.1, width, height);
        if self.key != Some(key) || self.surface.is_none() {
            self.surface = build_glass(conn, cairo_xcb, gc, &self.atoms, root, origin, width, height)
                .map_err(|error| debug!("frosted glass unavailable: {error}"))
                .ok();
            self.key = Some(key);
        }
        self.surface.as_ref()
    }
}

fn read_wallpaper_pixmap(conn: &xcb::Connection, root: x::Window, atom: x::Atom) -> Option<u32> {
    let cookie = conn.send_request(&x::GetProperty {
        delete: false,
        window: root,
        property: atom,
        r#type: x::ATOM_PIXMAP,
        long_offset: 0,
        long_length: 1,
    });
    let reply = conn.wait_for_reply(cookie).ok()?;
    if reply.r#type() != x::ATOM_PIXMAP {
        return None;
    }
    reply.value::<u32>().first().copied().filter(|id| *id != 0)
}

#[allow(clippy::too_many_arguments)]
fn build_glass(
    conn: &xcb::Connection,
    cairo_xcb: &CairoXcb,
    gc: x::Gcontext,
    atoms: &RootPixmapAtoms,
    root: x::Window,
    origin: (i16, i16),
    width: u16,
    height: u16,
) -> Result<ImageSurface> {
    if width == 0 || height == 0 {
        return Err(anyhow!("empty bar geometry"));
    }
    let pixmap_id = read_wallpaper_pixmap(conn, root, atoms.xrootpmap)
        .or_else(|| read_wallpaper_pixmap(conn, root, atoms.esetroot))
        .ok_or_else(|| anyhow!("no wallpaper pixmap property"))?;
    let wallpaper = <x::Pixmap as xcb::XidNew>::new(pixmap_id);

    let geometry = conn.wait_for_reply(conn.send_request(&x::GetGeometry {
        drawable: x::Drawable::Pixmap(wallpaper),
    }))?;
    let strip_height = height.saturating_add(GLASS_PAD);
    let src_x = i32::from(origin.0).clamp(0, i32::from(geometry.width()).saturating_sub(1));
    let src_y = i32::from(origin.1).clamp(0, i32::from(geometry.height()).saturating_sub(1));
    if i32::from(geometry.width()) - src_x < i32::from(width) {
        return Err(anyhow!("wallpaper pixmap narrower than the bar"));
    }
    let available_height =
        (i32::from(geometry.height()) - src_y).clamp(0, i32::from(strip_height)) as u16;
    if available_height < height {
        return Err(anyhow!("wallpaper pixmap shorter than the bar"));
    }

    // Copy the strip into a pixmap we own so all later Cairo traffic touches
    // only stable resources even if the wallpaper pixmap is freed under us.
    let strip = conn.generate_id();
    conn.send_and_check_request(&x::CreatePixmap {
        depth: geometry.depth(),
        pid: strip,
        drawable: x::Drawable::Pixmap(wallpaper),
        width,
        height: available_height,
    })?;
    let copied = conn.send_and_check_request(&x::CopyArea {
        src_drawable: x::Drawable::Pixmap(wallpaper),
        dst_drawable: x::Drawable::Pixmap(strip),
        gc,
        src_x: src_x as i16,
        src_y: src_y as i16,
        dst_x: 0,
        dst_y: 0,
        width,
        height: available_height,
    });
    let image = copied.map_err(anyhow::Error::from).and_then(|()| {
        let drawable = XCBDrawable(strip.resource_id());
        let xcb_surface = XCBSurface::create(
            &cairo_xcb.connection,
            &drawable,
            &cairo_xcb.visual,
            i32::from(width),
            i32::from(available_height),
        )?;
        let image = ImageSurface::create(
            Format::Rgb24,
            i32::from(width),
            i32::from(available_height),
        )?;
        let context = Context::new(&image)?;
        context.set_source_surface(&xcb_surface, 0.0, 0.0)?;
        context.paint()?;
        drop(context);
        Ok(image)
    });
    let _ = conn.send_and_check_request(&x::FreePixmap { pixmap: strip });
    let mut image = image?;
    image.flush();
    frost(&mut image, i32::from(height))
}

/// Downscale, blur, saturate, and upscale the captured strip; the result is
/// exactly `width x target_height`.
fn frost(strip: &mut ImageSurface, target_height: i32) -> Result<ImageSurface> {
    let width = strip.width();
    let height = strip.height();
    let small_width = (width / GLASS_DOWNSCALE).max(1);
    let small_height = (height / GLASS_DOWNSCALE).max(1);

    let mut small = ImageSurface::create(Format::Rgb24, small_width, small_height)?;
    scale_paint(strip, &small, Filter::Good)?;
    small.flush();
    box_blur(&mut small)?;
    saturate(&mut small, GLASS_SATURATION)?;

    let output = ImageSurface::create(Format::Rgb24, width, target_height.max(1))?;
    scale_paint(&small, &output, Filter::Bilinear)?;
    Ok(output)
}

fn scale_paint(source: &ImageSurface, target: &ImageSurface, filter: Filter) -> Result<()> {
    let context = Context::new(target)?;
    context.scale(
        f64::from(target.width()) / f64::from(source.width()),
        f64::from(target.height()) / f64::from(source.height()),
    );
    context.set_source_surface(source, 0.0, 0.0)?;
    context.source().set_filter(filter);
    context.paint()?;
    Ok(())
}

/// Three-pass box blur over the B, G, R byte lanes of an `Rgb24` surface.
fn box_blur(surface: &mut ImageSurface) -> Result<()> {
    let width = surface.width() as usize;
    let height = surface.height() as usize;
    let stride = surface.stride() as usize;
    if width == 0 || height == 0 {
        return Ok(());
    }
    let mut data = surface
        .data()
        .map_err(|error| anyhow!("image data borrow failed: {error}"))?;
    let mut scratch = data.to_vec();
    let window = 2 * GLASS_BLUR_RADIUS + 1;

    for _ in 0..GLASS_BLUR_PASSES {
        // Horizontal pass: data -> scratch.
        for y in 0..height {
            let row = y * stride;
            for channel in 0..3 {
                let sample = |x: usize| i32::from(data[row + x.min(width - 1) * 4 + channel]);
                // Clamped window around x = 0: radius copies of the edge
                // pixel plus the first radius + 1 real samples.
                let mut sum: i32 = GLASS_BLUR_RADIUS as i32 * sample(0)
                    + (0..=GLASS_BLUR_RADIUS).map(sample).sum::<i32>();
                for x in 0..width {
                    scratch[row + x * 4 + channel] = (sum / window as i32) as u8;
                    sum += sample(x + GLASS_BLUR_RADIUS + 1)
                        - sample(x.saturating_sub(GLASS_BLUR_RADIUS));
                }
            }
        }
        // Vertical pass: scratch -> data.
        for x in 0..width {
            for channel in 0..3 {
                let column = x * 4 + channel;
                let sample = |y: usize| i32::from(scratch[y.min(height - 1) * stride + column]);
                let mut sum: i32 = GLASS_BLUR_RADIUS as i32 * sample(0)
                    + (0..=GLASS_BLUR_RADIUS).map(sample).sum::<i32>();
                for y in 0..height {
                    data[y * stride + column] = (sum / window as i32) as u8;
                    sum += sample(y + GLASS_BLUR_RADIUS + 1)
                        - sample(y.saturating_sub(GLASS_BLUR_RADIUS));
                }
            }
        }
    }
    Ok(())
}

/// Push colors away from their luma, mimicking the saturation boost of the
/// macOS glass material.
fn saturate(surface: &mut ImageSurface, saturation: f32) -> Result<()> {
    let width = surface.width() as usize;
    let height = surface.height() as usize;
    let stride = surface.stride() as usize;
    let mut data = surface
        .data()
        .map_err(|error| anyhow!("image data borrow failed: {error}"))?;
    for y in 0..height {
        for x in 0..width {
            let offset = y * stride + x * 4;
            let bytes: [u8; 4] = data[offset..offset + 4].try_into().expect("pixel slice");
            let pixel = u32::from_ne_bytes(bytes);
            let red = ((pixel >> 16) & 0xff) as f32;
            let green = ((pixel >> 8) & 0xff) as f32;
            let blue = (pixel & 0xff) as f32;
            let luma = 0.2126 * red + 0.7152 * green + 0.0722 * blue;
            let adjust =
                |value: f32| (luma + (value - luma) * saturation).clamp(0.0, 255.0) as u32;
            let pixel = (adjust(red) << 16) | (adjust(green) << 8) | adjust(blue);
            data[offset..offset + 4].copy_from_slice(&pixel.to_ne_bytes());
        }
    }
    Ok(())
}

// ---------------- XCB back buffer ----------------
struct BackBuffer {
    pixmap: x::Pixmap,
    width: u16,
    height: u16,
    depth: u8,
    surface: Option<XCBSurface>,
    context: Option<Context>,
    /// ARGB32 intermediate the scene renders into, so its translucent
    /// background can be composited over the frosted wallpaper strip.
    scene_surface: Option<ImageSurface>,
    scene_context: Option<Context>,
}

impl BackBuffer {
    fn new(
        conn: &xcb::Connection,
        depth: u8,
        win: x::Window,
        width: u16,
        height: u16,
    ) -> Result<Self> {
        let pixmap = conn.generate_id();
        conn.send_and_check_request(&x::CreatePixmap {
            depth,
            pid: pixmap,
            drawable: x::Drawable::Window(win),
            width,
            height,
        })?;
        Ok(Self {
            pixmap,
            width,
            height,
            depth,
            surface: None,
            context: None,
            scene_surface: None,
            scene_context: None,
        })
    }

    fn ensure_scene_context(&mut self) -> Result<&Context> {
        if self.scene_surface.is_none() {
            let surface = ImageSurface::create(
                Format::ARgb32,
                i32::from(self.width),
                i32::from(self.height),
            )?;
            self.scene_context = Some(Context::new(&surface)?);
            self.scene_surface = Some(surface);
        }
        self.scene_context
            .as_ref()
            .ok_or_else(|| anyhow!("scene context was not initialized"))
    }

    /// Fill the pixmap with the frosted strip (or the fallback color) and
    /// draw the rendered scene over it.
    fn compose(
        &mut self,
        cairo_xcb: &CairoXcb,
        glass: Option<&ImageSurface>,
        fallback: (f64, f64, f64),
    ) -> Result<()> {
        self.ensure_context(cairo_xcb)?;
        let context = self
            .context
            .as_ref()
            .ok_or_else(|| anyhow!("Cairo context was not initialized"))?;
        let scene = self
            .scene_surface
            .as_ref()
            .ok_or_else(|| anyhow!("scene surface was not initialized"))?;
        context.save()?;
        context.set_operator(Operator::Source);
        match glass {
            Some(glass) => context.set_source_surface(glass, 0.0, 0.0)?,
            None => context.set_source_rgb(fallback.0, fallback.1, fallback.2),
        }
        context.paint()?;
        context.set_operator(Operator::Over);
        context.set_source_surface(scene, 0.0, 0.0)?;
        context.paint()?;
        context.restore()?;
        Ok(())
    }

    fn ensure_context<'a>(&'a mut self, cairo_xcb: &CairoXcb) -> Result<&'a Context> {
        if self.surface.is_none() {
            let drawable = XCBDrawable(self.pixmap.resource_id());
            let surface = XCBSurface::create(
                &cairo_xcb.connection,
                &drawable,
                &cairo_xcb.visual,
                i32::from(self.width),
                i32::from(self.height),
            )?;
            let context = Context::new(&surface)?;
            self.surface = Some(surface);
            self.context = Some(context);
        }
        self.context
            .as_ref()
            .ok_or_else(|| anyhow!("Cairo context was not initialized"))
    }

    fn flush(&self) {
        if let Some(surface) = &self.surface {
            surface.flush();
        }
    }

    fn resize_if_needed(
        &mut self,
        conn: &xcb::Connection,
        win: x::Window,
        width: u16,
        height: u16,
    ) -> Result<()> {
        if self.width == width && self.height == height {
            return Ok(());
        }

        conn.send_and_check_request(&x::FreePixmap {
            pixmap: self.pixmap,
        })?;
        let pixmap = conn.generate_id();
        conn.send_and_check_request(&x::CreatePixmap {
            depth: self.depth,
            pid: pixmap,
            drawable: x::Drawable::Window(win),
            width,
            height,
        })?;
        self.pixmap = pixmap;
        self.width = width;
        self.height = height;
        self.surface = None;
        self.context = None;
        self.scene_surface = None;
        self.scene_context = None;
        Ok(())
    }

    fn blit_to_window(
        &self,
        conn: &xcb::Connection,
        win: x::Window,
        gc: x::Gcontext,
    ) -> Result<()> {
        conn.send_and_check_request(&x::CopyArea {
            src_drawable: x::Drawable::Pixmap(self.pixmap),
            dst_drawable: x::Drawable::Window(win),
            gc,
            src_x: 0,
            src_y: 0,
            dst_x: 0,
            dst_y: 0,
            width: self.width,
            height: self.height,
        })?;
        Ok(())
    }
}

// ---------------- EWMH ----------------
fn intern_atom(conn: &xcb::Connection, name: &str) -> Result<x::Atom> {
    let cookie = conn.send_request(&x::InternAtom {
        only_if_exists: false,
        name: name.as_bytes(),
    });
    Ok(conn.wait_for_reply(cookie)?.atom())
}

/// Write core-described dock properties with this connection. Atom names come
/// from `DockWindowSpec`; only interning and the property calls live here.
fn write_dock_properties(
    conn: &xcb::Connection,
    win: x::Window,
    properties: &[DockProperty],
) -> Result<()> {
    let atom_type = intern_atom(conn, "ATOM")?;
    let cardinal_type = intern_atom(conn, "CARDINAL")?;
    for property in properties {
        let name = intern_atom(conn, property.name)?;
        match &property.value {
            DockPropertyValue::Atoms(values) => {
                let values = values
                    .iter()
                    .map(|value| Ok(intern_atom(conn, value)?.resource_id()))
                    .collect::<Result<Vec<u32>>>()?;
                change_property_32(conn, win, name, atom_type, &values)?;
            }
            DockPropertyValue::Cardinals(values) => {
                change_property_32(conn, win, name, cardinal_type, values)?;
            }
            DockPropertyValue::Utf8Text(text) => {
                let utf8_string = intern_atom(conn, "UTF8_STRING")?;
                change_property_8(conn, win, name, utf8_string, text.as_bytes())?;
            }
        }
    }
    Ok(())
}

fn dock_spec(x: i32, y: i32, width: u32, bar_height: u16) -> DockWindowSpec {
    DockWindowSpec::top(
        BAR_NAME,
        BarPlacement {
            x,
            y,
            width,
            height: u32::from(bar_height),
        },
    )
}

fn change_property_32(
    conn: &xcb::Connection,
    win: x::Window,
    property: x::Atom,
    property_type: x::Atom,
    data: &[u32],
) -> Result<()> {
    // Passing u32 values directly is significant: xcb derives the protocol
    // format from the element type, so this emits format=32 rather than the
    // format=8 request produced by the former byte conversion.
    conn.send_and_check_request(&x::ChangeProperty {
        mode: x::PropMode::Replace,
        window: win,
        property,
        r#type: property_type,
        data,
    })?;
    Ok(())
}

fn change_property_8(
    conn: &xcb::Connection,
    win: x::Window,
    property: x::Atom,
    property_type: x::Atom,
    data: &[u8],
) -> Result<()> {
    conn.send_and_check_request(&x::ChangeProperty {
        mode: x::PropMode::Replace,
        window: win,
        property,
        r#type: property_type,
        data,
    })?;
    Ok(())
}

// ---------------- Platform integration ----------------
struct WindowAdapter<'a> {
    conn: &'a xcb::Connection,
    screen: &'a x::Screen,
    win: x::Window,
    bar_height: Cell<u16>,
    effects: RefCell<EffectRouter>,
    glass: RefCell<GlassCache>,
    /// True when the window uses a 32-bit visual under a compositor: the bar
    /// then emits real per-pixel alpha and skips the baked frost strip.
    translucent: bool,
}

impl WindowAdapter<'_> {
    /// Bar origin in root coordinates, resolved through the server so
    /// reparenting window managers cannot skew wallpaper sampling.
    fn root_origin(&self) -> (i16, i16) {
        let cookie = self.conn.send_request(&x::TranslateCoordinates {
            src_window: self.win,
            dst_window: self.screen.root(),
            src_x: 0,
            src_y: 0,
        });
        match self.conn.wait_for_reply(cookie) {
            Ok(reply) => (reply.dst_x(), reply.dst_y()),
            Err(error) => {
                debug!("translate coordinates failed: {error}");
                (0, 0)
            }
        }
    }

    /// React to a root property change; true when the wallpaper changed and
    /// the frosted strip must be rebuilt.
    fn wallpaper_changed(&self, atom: x::Atom) -> bool {
        let mut glass = self.glass.borrow_mut();
        if glass.atoms.matches(atom) {
            glass.invalidate();
            true
        } else {
            false
        }
    }

    fn sync_bar_height(&self, bar: &mut CairoBar, height: u16) {
        // A window manager may enforce its configured dock height instead of
        // the size requested when the window was created. Keep both future
        // geometry requests and the presentation viewport fill in sync with
        // that final server-side height.
        self.bar_height.set(height);
        bar.config_mut().bar_height = f32::from(height);
    }

    fn apply_runtime_update(&self, update: RuntimeUpdate) -> Result<bool> {
        self.effects.borrow_mut().route(update, |request| {
            let geometry = match request {
                GeometryRequest::Apply(geometry) => geometry,
                GeometryRequest::Clear => MonitorGeometry {
                    x: 0,
                    y: 0,
                    width: u32::from(self.screen.width_in_pixels()),
                    height: u32::from(self.screen.height_in_pixels()),
                },
            };
            self.apply_geometry(geometry)
        })
    }

    fn apply_geometry(&self, geometry: MonitorGeometry) -> Result<()> {
        let width = geometry.width.max(1);
        let bar_height = self.bar_height.get();
        self.conn.send_and_check_request(&x::ConfigureWindow {
            window: self.win,
            value_list: &[
                x::ConfigWindow::X(geometry.x),
                x::ConfigWindow::Y(geometry.y),
                x::ConfigWindow::Width(width),
                x::ConfigWindow::Height(u32::from(bar_height)),
            ],
        })?;
        let spec = dock_spec(geometry.x, geometry.y, width, bar_height);
        write_dock_properties(self.conn, self.win, &spec.strut_properties())?;
        self.conn.flush()?;
        Ok(())
    }
}

fn redraw(
    cairo_xcb: &CairoXcb,
    window: &WindowAdapter<'_>,
    back: &mut BackBuffer,
    gc: x::Gcontext,
    width: u16,
    height: u16,
    bar: &mut CairoBar,
) -> Result<()> {
    let size = Size::new(f32::from(width), f32::from(height));
    if window.translucent {
        // The compositor blends and blurs behind the bar; render the scene's
        // per-pixel alpha straight into the window-depth back buffer.
        let context = back.ensure_context(cairo_xcb)?;
        bar.render(context, size)?;
        let _ = bar.runtime_mut().take_changes();
    } else {
        let context = back.ensure_scene_context()?;
        bar.render(context, size)?;
        let _ = bar.runtime_mut().take_changes();
        if let Some(scene) = &back.scene_surface {
            scene.flush();
        }

        let mut glass = window.glass.borrow_mut();
        let origin = window.root_origin();
        let fallback = glass.fallback;
        let strip = glass.ensure(
            window.conn,
            cairo_xcb,
            gc,
            window.screen.root(),
            origin,
            width,
            height,
        );
        back.compose(cairo_xcb, strip, fallback)?;
    }

    back.flush();
    back.blit_to_window(window.conn, window.win, gc)?;
    window.conn.flush()?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn handle_x_event(
    event: xcb::Event,
    cairo_xcb: &CairoXcb,
    window: &WindowAdapter<'_>,
    back: &mut BackBuffer,
    gc: x::Gcontext,
    current_width: &mut u16,
    current_height: &mut u16,
    bar: &mut CairoBar,
) -> Result<()> {
    let mut should_redraw = false;

    match event {
        xcb::Event::X(x::Event::Expose(event)) => {
            if event.count() == 0 {
                back.blit_to_window(window.conn, window.win, gc)?;
                window.conn.flush()?;
            }
        }
        xcb::Event::X(x::Event::ConfigureNotify(event)) if event.window() == window.win => {
            *current_width = event.width();
            *current_height = event.height();
            window.sync_bar_height(bar, event.height());
            back.resize_if_needed(window.conn, window.win, *current_width, *current_height)?;
            should_redraw = true;
        }
        xcb::Event::X(x::Event::EnterNotify(event)) => {
            should_redraw = bar.pointer_motion(Point::new(
                f32::from(event.event_x()),
                f32::from(event.event_y()),
            ));
        }
        xcb::Event::X(x::Event::LeaveNotify(_)) => {
            should_redraw = bar.pointer_leave();
        }
        xcb::Event::X(x::Event::MotionNotify(event)) => {
            should_redraw = bar.pointer_motion(Point::new(
                f32::from(event.event_x()),
                f32::from(event.event_y()),
            ));
        }
        xcb::Event::X(x::Event::PropertyNotify(event))
            if event.window() == window.screen.root() =>
        {
            should_redraw = window.wallpaper_changed(event.atom());
        }
        xcb::Event::X(x::Event::ButtonPress(event)) => {
            let button = event.detail();
            if let Some(input) = PointerAction::from_x11_button(button) {
                let update = bar.pointer_action(
                    Point::new(f32::from(event.event_x()), f32::from(event.event_y())),
                    input,
                );
                should_redraw = window.apply_runtime_update(update)?;
            }
        }
        _ => {}
    }

    if should_redraw {
        redraw(
            cairo_xcb,
            window,
            back,
            gc,
            *current_width,
            *current_height,
            bar,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn drain_x_events(
    cairo_xcb: &CairoXcb,
    window: &WindowAdapter<'_>,
    back: &mut BackBuffer,
    gc: x::Gcontext,
    current_width: &mut u16,
    current_height: &mut u16,
    bar: &mut CairoBar,
) -> Result<()> {
    loop {
        match window.conn.poll_for_event() {
            Ok(Some(event)) => handle_x_event(
                event,
                cairo_xcb,
                window,
                back,
                gc,
                current_width,
                current_height,
                bar,
            )?,
            Ok(None) => return Ok(()),
            Err(error) => return Err(error.into()),
        }
    }
}

fn sync_notifier(
    slot: &mut TransportNotifierSlot,
    runtime: &BarRuntime,
    epoll: &Epoll,
) -> Result<()> {
    if let NotifierChange::Replaced { fd, .. } = slot.sync(runtime)? {
        epoll.add(fd, SHARED_TOKEN)?;
    }
    Ok(())
}

fn main() -> Result<()> {
    let shared_path = env::args().skip(1).last().unwrap_or_default();
    xbar_core::logging::init(BAR_NAME, &shared_path)?;
    let app_config = xbar_core::config::BarConfig::load_default()?;

    let runtime = if shared_path.is_empty() {
        BarRuntime::new(app_config.model_config())?
    } else {
        let recovery = TransportRecoveryConfig::new(shared_path.clone(), TRANSPORT_RETRY_INTERVAL)?;
        BarRuntime::with_managed_transport(app_config.model_config(), recovery)?
    };

    let (conn, screen_num) = xcb::Connection::connect(None)?;
    let setup = conn.get_setup();
    let screen = setup
        .roots()
        .nth(screen_num as usize)
        .ok_or_else(|| anyhow!("no X screen found"))?;

    // Prefer real translucency when a compositor can blend it; otherwise the
    // bar bakes its own frosted wallpaper strip below.
    let argb_visual = if compositor_active(&conn, screen_num) {
        find_argb_visual(screen)
    } else {
        None
    };
    let (window_depth, window_visual) = match &argb_visual {
        Some(visual) => (32, visual.visual_id()),
        None => (screen.root_depth(), screen.root_visual()),
    };
    let translucent = argb_visual.is_some();
    let cairo_xcb = build_cairo_xcb(&conn, screen, window_visual, window_depth)?;

    let mut presentation = app_config.presentation.clone();
    // Monochrome Nerd Font glyphs tinted by the text color read like macOS
    // template icons; only replace the stock emoji so a config that overrides
    // individual labels keeps its customization.
    if presentation.labels == PresentationLabels::default() {
        presentation.labels = PresentationLabels::nerd_font();
    }
    let bar_height = presentation
        .bar_height
        .round()
        .clamp(1.0, f32::from(u16::MAX)) as u16;
    let font = FontDescription::from_string(&app_config.font);
    let mut bar = CairoBar::new(runtime, presentation, font);
    // The frosted pipeline needs a translucent background tint by default;
    // an explicit config value still wins (1.0 restores a solid bar).
    let opacity = app_config
        .background_opacity
        .unwrap_or(DEFAULT_BACKGROUND_OPACITY);
    bar.renderer_mut().set_background_opacity(Some(opacity));

    let win = conn.generate_id();
    let mut current_width = screen.width_in_pixels();
    let mut current_height = bar_height;
    let event_mask = x::EventMask::EXPOSURE
        | x::EventMask::STRUCTURE_NOTIFY
        | x::EventMask::BUTTON_PRESS
        | x::EventMask::POINTER_MOTION
        | x::EventMask::ENTER_WINDOW
        | x::EventMask::LEAVE_WINDOW;
    if translucent {
        // A depth-32 window needs an explicit border pixel and colormap for
        // its non-default visual, or CreateWindow fails with BadMatch.
        let colormap = conn.generate_id();
        conn.send_and_check_request(&x::CreateColormap {
            alloc: x::ColormapAlloc::None,
            mid: colormap,
            window: screen.root(),
            visual: window_visual,
        })?;
        conn.send_and_check_request(&x::CreateWindow {
            depth: window_depth,
            wid: win,
            parent: screen.root(),
            x: 0,
            y: 0,
            width: current_width,
            height: current_height,
            border_width: 0,
            class: x::WindowClass::InputOutput,
            visual: window_visual,
            value_list: &[
                x::Cw::BackPixel(0),
                x::Cw::BorderPixel(0),
                x::Cw::EventMask(event_mask),
                x::Cw::Colormap(colormap),
            ],
        })?;
    } else {
        conn.send_and_check_request(&x::CreateWindow {
            depth: x::COPY_FROM_PARENT as u8,
            wid: win,
            parent: screen.root(),
            x: 0,
            y: 0,
            width: current_width,
            height: current_height,
            border_width: 0,
            class: x::WindowClass::InputOutput,
            visual: window_visual,
            value_list: &[
                x::Cw::BackPixmap(x::Pixmap::none()),
                x::Cw::EventMask(event_mask),
            ],
        })?;
    }

    // The GC lives on the bar window so its depth always matches the back
    // buffer, whichever visual was chosen.
    let gc = conn.generate_id();
    conn.send_and_check_request(&x::CreateGc {
        cid: gc,
        drawable: x::Drawable::Window(win),
        value_list: &[],
    })?;

    let spec = dock_spec(0, 0, u32::from(current_width), current_height);
    write_dock_properties(&conn, win, &spec.properties())?;
    conn.send_and_check_request(&x::MapWindow { window: win })?;
    conn.flush()?;

    // Watch the root window so wallpaper swaps rebuild the frosted strip.
    let glass_atoms = RootPixmapAtoms::intern(&conn)?;
    conn.send_and_check_request(&x::ChangeWindowAttributes {
        window: screen.root(),
        value_list: &[x::Cw::EventMask(x::EventMask::PROPERTY_CHANGE)],
    })?;
    let fallback = match app_config.theme {
        ThemeMode::Dark => (28.0 / 255.0, 28.0 / 255.0, 30.0 / 255.0),
        ThemeMode::Light => (246.0 / 255.0, 246.0 / 255.0, 248.0 / 255.0),
    };

    let window = WindowAdapter {
        conn: &conn,
        screen,
        win,
        bar_height: Cell::new(bar_height),
        effects: RefCell::new(EffectRouter::default()),
        glass: RefCell::new(GlassCache::new(glass_atoms, fallback)),
        translucent,
    };
    let mut back = BackBuffer::new(
        window.conn,
        window_depth,
        window.win,
        current_width,
        current_height,
    )?;

    // Seed providers and consume any snapshot that was queued before startup.
    let mut initial_update = bar.tick();
    initial_update.merge(bar.poll_transport());
    window.apply_runtime_update(initial_update)?;
    redraw(
        &cairo_xcb,
        &window,
        &mut back,
        gc,
        current_width,
        current_height,
        &mut bar,
    )?;

    let timer = AlignedTimer::new(Duration::from_secs(1))?;
    let mut epoll = Epoll::new()?;
    // SAFETY: the connection outlives the epoll registration and owns its
    // descriptor for the whole program.
    let conn_fd = unsafe { BorrowedFd::borrow_raw(window.conn.as_raw_fd()) };
    epoll.add(conn_fd, X_TOKEN)?;
    epoll.add(timer.as_fd(), TIMER_TOKEN)?;
    let mut notifier_slot = TransportNotifierSlot::new(true);
    sync_notifier(&mut notifier_slot, bar.runtime(), &epoll)?;

    let mut ready_tokens = Vec::new();
    loop {
        ready_tokens.clear();
        ready_tokens.extend(epoll.wait()?);
        for token in &ready_tokens {
            match *token {
                X_TOKEN => drain_x_events(
                    &cairo_xcb,
                    &window,
                    &mut back,
                    gc,
                    &mut current_width,
                    &mut current_height,
                    &mut bar,
                )?,
                TIMER_TOKEN => {
                    if timer.drain()? > 0 {
                        let mut update = bar.tick();
                        update.merge(bar.poll_transport());
                        let needs_redraw = window.apply_runtime_update(update)?;
                        sync_notifier(&mut notifier_slot, bar.runtime(), &epoll)?;
                        if needs_redraw {
                            redraw(
                                &cairo_xcb,
                                &window,
                                &mut back,
                                gc,
                                current_width,
                                current_height,
                                &mut bar,
                            )?;
                        }
                    }
                }
                SHARED_TOKEN => {
                    if let Some(notifier) = notifier_slot.notifier() {
                        notifier.drain()?;
                        let update = bar.poll_transport();
                        let needs_redraw = window.apply_runtime_update(update)?;
                        sync_notifier(&mut notifier_slot, bar.runtime(), &epoll)?;
                        if needs_redraw {
                            redraw(
                                &cairo_xcb,
                                &window,
                                &mut back,
                                gc,
                                current_width,
                                current_height,
                                &mut bar,
                            )?;
                        }
                    } else {
                        warn!("received shared token without an owned notifier");
                    }
                }
                token => debug!("unexpected epoll token: {token}"),
            }
        }
    }
}
