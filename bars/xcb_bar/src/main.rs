use anyhow::{Result, anyhow};
use cairo::ffi::{xcb_connection_t, xcb_visualtype_t};
use cairo::{
    Context, Format, ImageSurface, XCBConnection as CairoXCBConnection, XCBDrawable, XCBSurface,
    XCBVisualType,
};
use log::{debug, warn};
use pango::FontDescription;
use std::cell::{Cell, RefCell};
use std::env;
use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd};
use std::time::Duration;
use xbar_core::glass::wallpaper::WallpaperFile;
use xbar_core::glass::{
    DEFAULT_BACKGROUND_OPACITY, GlassBackdrop, GlassError, GlassImage, StripRequest,
    WallpaperSource, fallback_rgb,
};
use xbar_core::linux::{AlignedTimer, Epoll};
use xbar_core::presentation::{Point, PointerAction, PresentationLabels, Size};
use xbar_core::render::cairo::CairoBar;
use xbar_core::{
    BarPlacement, BarRuntime, DockProperty, DockPropertyValue, DockWindowSpec, MonitorGeometry,
    NotifierChange, RuntimeUpdate, TransportNotifierSlot, TransportRecoveryConfig,
};
use xbar_linux_actions::{EffectRouter, GeometryRequest};
use xcb::{self, Xid, x};

const BAR_NAME: &str = "xcb_bar";
const X_TOKEN: u64 = 1;
const TIMER_TOKEN: u64 = 2;
const SHARED_TOKEN: u64 = 3;
const TRANSPORT_RETRY_INTERVAL: Duration = Duration::from_secs(2);

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

fn build_cairo_xcb(
    conn: &xcb::Connection,
    screen: &x::Screen,
    visual_id: u32,
    depth: u8,
) -> Result<CairoXcb> {
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

/// Wallpaper pixels captured from the X root pixmap.
///
/// This works only where some tool published `_XROOTPMAP_ID` — feh, hsetroot,
/// and the like do.  A compositor that draws the wallpaper from its own
/// configuration never publishes it, which is why `glass.wallpaper` exists;
/// this stays as the fallback for a plain X session.
struct RootPixmapSource<'a> {
    conn: &'a xcb::Connection,
    cairo_xcb: &'a CairoXcb,
    gc: x::Gcontext,
    root: x::Window,
    atoms: RootPixmapAtoms,
    revision: u64,
}

impl RootPixmapSource<'_> {
    /// React to a root property change; true when the wallpaper changed and
    /// the frosted strip must be rebuilt.
    fn note_property(&mut self, atom: x::Atom) -> bool {
        if self.atoms.matches(atom) {
            self.revision = self.revision.wrapping_add(1);
            true
        } else {
            false
        }
    }
}

impl WallpaperSource for RootPixmapSource<'_> {
    fn revision(&mut self) -> u64 {
        self.revision
    }

    fn strip(&mut self, request: &StripRequest) -> Result<GlassImage, GlassError> {
        capture_root_strip(self, request)
            .map_err(|error| GlassError::Unavailable(error.to_string()))
    }
}

/// Where this bar's wallpaper comes from.
///
/// A configured file is preferred because it is the only source that survives
/// a compositor-drawn wallpaper, and the root pixmap covers the sessions that
/// still publish one.
enum WallpaperOrigin<'a> {
    File(WallpaperFile),
    RootPixmap(RootPixmapSource<'a>),
}

impl WallpaperSource for WallpaperOrigin<'_> {
    fn revision(&mut self) -> u64 {
        match self {
            Self::File(source) => source.revision(),
            Self::RootPixmap(source) => source.revision(),
        }
    }

    fn strip(&mut self, request: &StripRequest) -> Result<GlassImage, GlassError> {
        match self {
            Self::File(source) => source.strip(request),
            Self::RootPixmap(source) => source.strip(request),
        }
    }
}

impl WallpaperOrigin<'_> {
    /// React to a root property change; true when the wallpaper changed and
    /// the frosted strip must be rebuilt.  A file source restats the wallpaper
    /// itself and ignores X properties entirely.
    fn note_property(&mut self, atom: x::Atom) -> bool {
        match self {
            Self::RootPixmap(source) => source.note_property(atom),
            Self::File(_) => false,
        }
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

/// Copy the wallpaper under the bar out of the root pixmap.
///
/// The result is raw wallpaper, not yet frosted: blurring it is
/// `xbar_core::glass`'s job, and everything here is the X11 half that cannot
/// move into a platform-neutral crate.
fn capture_root_strip(source: &RootPixmapSource<'_>, request: &StripRequest) -> Result<GlassImage> {
    let width = u16::try_from(request.width)?;
    let height = u16::try_from(request.height)?;
    if width == 0 || height == 0 {
        return Err(anyhow!("empty bar geometry"));
    }
    let conn = source.conn;
    let pixmap_id = read_wallpaper_pixmap(conn, source.root, source.atoms.xrootpmap)
        .or_else(|| read_wallpaper_pixmap(conn, source.root, source.atoms.esetroot))
        .ok_or_else(|| anyhow!("no wallpaper pixmap property"))?;
    let wallpaper = <x::Pixmap as xcb::XidNew>::new(pixmap_id);

    let geometry = conn.wait_for_reply(conn.send_request(&x::GetGeometry {
        drawable: x::Drawable::Pixmap(wallpaper),
    }))?;
    let strip_height = u16::try_from(request.padded_height().min(u32::from(u16::MAX)))?;
    let src_x = request
        .x
        .clamp(0, i32::from(geometry.width()).saturating_sub(1));
    let src_y = request
        .y
        .clamp(0, i32::from(geometry.height()).saturating_sub(1));
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
        gc: source.gc,
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
            &source.cairo_xcb.connection,
            &drawable,
            &source.cairo_xcb.visual,
            i32::from(width),
            i32::from(available_height),
        )?;
        let image =
            ImageSurface::create(Format::Rgb24, i32::from(width), i32::from(available_height))?;
        let context = Context::new(&image)?;
        context.set_source_surface(&xcb_surface, 0.0, 0.0)?;
        context.paint()?;
        drop(context);
        Ok(image)
    });
    let _ = conn.send_and_check_request(&x::FreePixmap { pixmap: strip });
    let mut image = image?;
    image.flush();
    Ok(GlassImage::from_image_surface(&mut image)?)
}

// ---------------- XCB back buffer ----------------
struct BackBuffer {
    pixmap: x::Pixmap,
    width: u16,
    height: u16,
    depth: u8,
    surface: Option<XCBSurface>,
    context: Option<Context>,
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
        })
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
    glass: RefCell<GlassBackdrop<WallpaperOrigin<'a>>>,
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
        self.glass.borrow_mut().source_mut().note_property(atom)
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
    } else {
        // Nobody will blur behind an opaque window, so the bar bakes its own
        // backdrop and the scene blends over it in one pass.
        let mut glass = window.glass.borrow_mut();
        let origin = window.root_origin();
        let backdrop = glass.ensure(
            i32::from(origin.0),
            i32::from(origin.1),
            u32::from(width),
            u32::from(height),
        );
        let context = back.ensure_context(cairo_xcb)?;
        bar.render_over(context, size, backdrop)?;
    }
    let _ = bar.runtime_mut().take_changes();

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
    conn.send_and_check_request(&x::ChangeWindowAttributes {
        window: screen.root(),
        value_list: &[x::Cw::EventMask(x::EventMask::PROPERTY_CHANGE)],
    })?;
    let fallback = fallback_rgb(app_config.theme);

    // A configured wallpaper wins: it is the only source that still works when
    // the compositor draws the wallpaper itself instead of publishing a root
    // pixmap, which is what JWM does.
    let source = match app_config.glass.file_source(
        u32::from(screen.width_in_pixels()),
        u32::from(screen.height_in_pixels()),
        fallback,
    ) {
        Some(file) => WallpaperOrigin::File(file),
        None => WallpaperOrigin::RootPixmap(RootPixmapSource {
            conn: &conn,
            cairo_xcb: &cairo_xcb,
            gc,
            root: screen.root(),
            atoms: RootPixmapAtoms::intern(&conn)?,
            revision: 0,
        }),
    };

    let window = WindowAdapter {
        conn: &conn,
        screen,
        win,
        bar_height: Cell::new(bar_height),
        effects: RefCell::new(EffectRouter::default()),
        glass: RefCell::new(
            GlassBackdrop::new(source, app_config.glass.params()).with_fallback(fallback),
        ),
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
