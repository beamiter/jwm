use anyhow::Result;
use pango::FontDescription;
use std::cell::{Cell, RefCell};
use std::env;
use std::ffi::c_void;
use std::num::NonZeroU32;
use std::os::fd::AsFd as _;
use std::ptr::NonNull;
use std::sync::Arc;
use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ConfigureWindowAux, ConnectionExt as _,
    CreateWindowAux, EventMask, ImageFormat, Pixmap, PropMode, Window, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;
use x11rb::xcb_ffi::XCBConnection;

use raw_window_handle::{
    DisplayHandle, HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
    WindowHandle, XcbDisplayHandle, XcbWindowHandle,
};

use xbar_core::glass::wallpaper::WallpaperFile;
use xbar_core::glass::{
    DEFAULT_BACKGROUND_OPACITY, GlassBackdrop, GlassError, GlassImage, StripRequest,
    WallpaperSource, fallback_rgb,
};
use xbar_core::linux::{AlignedTimer, Epoll};
use xbar_core::presentation::{Point, PointerAction, PresentationLabels};
use xbar_core::render::cairo::{CairoBar, CpuCanvas};
use xbar_core::{
    BarPlacement, BarRuntime, DockProperty, DockPropertyValue, DockWindowSpec, MonitorGeometry,
    NotifierChange, RuntimeUpdate, TransportNotifierSlot, TransportRecoveryConfig,
};
use xbar_linux_actions::{EffectRouter, GeometryRequest};
use xbar_present_wgpu::{PresentRect, WgpuPresenter};

const BAR_NAME: &str = "x11rb_wgpu_bar";
const X_TOKEN: u64 = 1;
const TIMER_TOKEN: u64 = 2;
const SHARED_TOKEN: u64 = 3;
const TRANSPORT_RETRY_INTERVAL: Duration = Duration::from_secs(2);

// ============================================================================
// 1. RAW WINDOW HANDLE 用于 WGPU 识别 XCB
// ============================================================================
struct XcbTarget {
    conn: *mut c_void,
    screen: i32,
    window: u32,
    visual_id: u32,
}

// 解决 *mut c_void 的跨线程传递问题
unsafe impl Send for XcbTarget {}
unsafe impl Sync for XcbTarget {}

impl HasDisplayHandle for XcbTarget {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, raw_window_handle::HandleError> {
        let handle = XcbDisplayHandle::new(Some(NonNull::new(self.conn).unwrap()), self.screen);
        Ok(unsafe { DisplayHandle::borrow_raw(RawDisplayHandle::Xcb(handle)) })
    }
}

impl HasWindowHandle for XcbTarget {
    fn window_handle(&self) -> Result<WindowHandle<'_>, raw_window_handle::HandleError> {
        let mut handle = XcbWindowHandle::new(NonZeroU32::new(self.window).unwrap());
        handle.visual_id = NonZeroU32::new(self.visual_id);
        Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::Xcb(handle)) })
    }
}

// ============================================================================
// 3. X11 platform adapter and Cairo-to-wgpu presentation
// ============================================================================
fn intern_atom(conn: &XCBConnection, name: &str) -> Result<Atom> {
    Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
}

/// Write core-described dock properties with this connection. Atom names come
/// from `DockWindowSpec`; only interning and the property calls live here.
fn write_dock_properties(
    conn: &XCBConnection,
    win: Window,
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
                    .map(|value| intern_atom(conn, value))
                    .collect::<Result<Vec<Atom>>>()?;
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

// ---------------- Frosted glass ----------------
/// Root-window properties that name the wallpaper pixmap.
struct RootPixmapAtoms {
    xrootpmap: Atom,
    esetroot: Atom,
}

impl RootPixmapAtoms {
    fn intern(conn: &XCBConnection) -> Result<Self> {
        Ok(Self {
            xrootpmap: intern_atom(conn, "_XROOTPMAP_ID")?,
            esetroot: intern_atom(conn, "ESETROOT_PMAP_ID")?,
        })
    }

    fn matches(&self, atom: Atom) -> bool {
        atom == self.xrootpmap || atom == self.esetroot
    }
}

/// Wallpaper pixels captured from the X root pixmap.
///
/// This works only where some tool published `_XROOTPMAP_ID` — feh, hsetroot,
/// and the like do.  A compositor that draws the wallpaper from its own
/// configuration never publishes it, which is why `glass.wallpaper` exists;
/// this stays as the fallback for a plain X session.
///
/// Unlike the Cairo bars this one asks the server for the pixels directly:
/// there is no Cairo surface on the X connection to copy through, and a
/// `GetImage` reply is already in the byte order `GlassImage` wants.
struct RootPixmapSource<'a> {
    conn: &'a XCBConnection,
    root: Window,
    atoms: RootPixmapAtoms,
    revision: u64,
}

impl RootPixmapSource<'_> {
    /// React to a root property change; true when the wallpaper changed and
    /// the frosted strip must be rebuilt.
    fn note_property(&mut self, atom: Atom) -> bool {
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
    fn note_property(&mut self, atom: Atom) -> bool {
        match self {
            Self::RootPixmap(source) => source.note_property(atom),
            Self::File(_) => false,
        }
    }
}

fn read_wallpaper_pixmap(conn: &XCBConnection, root: Window, atom: Atom) -> Option<Pixmap> {
    let reply = conn
        .get_property(false, root, atom, AtomEnum::PIXMAP, 0, 1)
        .ok()?
        .reply()
        .ok()?;
    if reply.type_ != Atom::from(AtomEnum::PIXMAP) {
        return None;
    }
    reply.value32()?.next().filter(|id| *id != x11rb::NONE)
}

/// Read the wallpaper under the bar out of the root pixmap.
///
/// The result is raw wallpaper, not yet frosted: blurring it is
/// `xbar_core::glass`'s job, and everything here is the X11 half that cannot
/// move into a platform-neutral crate.
fn capture_root_strip(source: &RootPixmapSource<'_>, request: &StripRequest) -> Result<GlassImage> {
    let width = u16::try_from(request.width)?;
    let height = u16::try_from(request.height)?;
    if width == 0 || height == 0 {
        anyhow::bail!("empty bar geometry");
    }
    let conn = source.conn;
    let wallpaper = read_wallpaper_pixmap(conn, source.root, source.atoms.xrootpmap)
        .or_else(|| read_wallpaper_pixmap(conn, source.root, source.atoms.esetroot))
        .ok_or_else(|| anyhow::anyhow!("no wallpaper pixmap property"))?;

    let geometry = conn.get_geometry(wallpaper)?.reply()?;
    // 32 bits per pixel is what every TrueColor visual of these depths uses,
    // and it is the only layout `GlassImage` can take without a conversion.
    if geometry.depth != 24 && geometry.depth != 32 {
        anyhow::bail!("unsupported wallpaper depth {}", geometry.depth);
    }
    let strip_height = u16::try_from(request.padded_height().min(u32::from(u16::MAX)))?;
    let src_x = request
        .x
        .clamp(0, i32::from(geometry.width).saturating_sub(1));
    let src_y = request
        .y
        .clamp(0, i32::from(geometry.height).saturating_sub(1));
    if i32::from(geometry.width) - src_x < i32::from(width) {
        anyhow::bail!("wallpaper pixmap narrower than the bar");
    }
    let available_height =
        (i32::from(geometry.height) - src_y).clamp(0, i32::from(strip_height)) as u16;
    if available_height < height {
        anyhow::bail!("wallpaper pixmap shorter than the bar");
    }

    let image = conn
        .get_image(
            ImageFormat::Z_PIXMAP,
            wallpaper,
            src_x as i16,
            src_y as i16,
            width,
            available_height,
            u32::MAX,
        )?
        .reply()?;
    Ok(GlassImage::from_bgra(
        u32::from(width),
        u32::from(available_height),
        usize::from(width) * 4,
        &image.data,
    )?)
}

fn change_property_32(
    conn: &XCBConnection,
    win: Window,
    property: Atom,
    property_type: Atom,
    data: &[u32],
) -> Result<()> {
    conn.change_property32(PropMode::REPLACE, win, property, property_type, data)?
        .check()?;
    Ok(())
}

fn change_property_8(
    conn: &XCBConnection,
    win: Window,
    property: Atom,
    property_type: Atom,
    data: &[u8],
) -> Result<()> {
    conn.change_property8(PropMode::REPLACE, win, property, property_type, data)?
        .check()?;
    Ok(())
}

struct WindowAdapter<'a> {
    conn: &'a XCBConnection,
    screen: &'a x11rb::protocol::xproto::Screen,
    win: Window,
    bar_height: Cell<u16>,
    effects: RefCell<EffectRouter>,
    /// This window is on the root visual and its wgpu surface presents opaque
    /// on X11, so glass here always means a baked strip — never the per-pixel
    /// alpha the Cairo bars emit under a compositor.
    glass: RefCell<GlassBackdrop<WallpaperOrigin<'a>>>,
}

impl WindowAdapter<'_> {
    /// Bar origin in root coordinates, resolved through the server so
    /// reparenting window managers cannot skew wallpaper sampling.
    fn root_origin(&self) -> (i32, i32) {
        let origin = self
            .conn
            .translate_coordinates(self.win, self.screen.root, 0, 0)
            .map_err(anyhow::Error::from)
            .and_then(|cookie| cookie.reply().map_err(anyhow::Error::from));
        match origin {
            Ok(reply) => (i32::from(reply.dst_x), i32::from(reply.dst_y)),
            Err(error) => {
                log::debug!("translate coordinates failed: {error}");
                (0, 0)
            }
        }
    }

    /// React to a root property change; true when the wallpaper changed and
    /// the frosted strip must be rebuilt.
    fn wallpaper_changed(&self, atom: Atom) -> bool {
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
                    width: u32::from(self.screen.width_in_pixels),
                    height: u32::from(self.screen.height_in_pixels),
                },
            };
            self.apply_geometry(geometry)
        })
    }

    fn apply_geometry(&self, geometry: MonitorGeometry) -> Result<()> {
        let width = geometry.width.max(1);
        let bar_height = self.bar_height.get();
        self.conn
            .configure_window(
                self.win,
                &ConfigureWindowAux::new()
                    .x(geometry.x)
                    .y(geometry.y)
                    .width(width)
                    .height(u32::from(bar_height)),
            )?
            .check()?;
        let spec = dock_spec(geometry.x, geometry.y, width, bar_height);
        write_dock_properties(self.conn, self.win, &spec.strut_properties())?;
        self.conn.flush()?;
        Ok(())
    }
}

fn redraw(
    gpu: &mut WgpuPresenter,
    canvas: &mut CpuCanvas,
    window: &WindowAdapter<'_>,
    width: u16,
    height: u16,
    bar: &mut CairoBar,
) -> Result<()> {
    let mut glass = window.glass.borrow_mut();
    let (origin_x, origin_y) = window.root_origin();
    let backdrop = glass.ensure(origin_x, origin_y, u32::from(width), u32::from(height));
    let frame = canvas.render_over(bar, u32::from(width), u32::from(height), 1.0, backdrop)?;
    let _ = bar.runtime_mut().take_changes();
    let damage = frame.damage.map(|rect| PresentRect {
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
    });
    gpu.present_bgra(frame.data, frame.stride, damage)?;
    Ok(())
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

// ============================================================================
// 4. MAIN LOOP
// ============================================================================
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

    let (conn, screen_num) = XCBConnection::connect(None)?;
    let setup = conn.setup();
    let screen = &setup.roots[screen_num];

    let mut presentation = app_config.presentation.clone();
    // Monochrome Nerd Font glyphs tinted by the text color read like macOS
    // template icons; only replace the stock emoji so a config that overrides
    // individual labels keeps its customization. Every other bar makes exactly
    // this substitution.
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
    let fallback = fallback_rgb(app_config.theme);

    let win = conn.generate_id()?;
    let mut current_width = screen.width_in_pixels;
    let mut current_height = bar_height;

    conn.create_window(
        screen.root_depth,
        win,
        screen.root,
        0,
        0,
        current_width,
        current_height,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new()
            .background_pixmap(AtomEnum::NONE)
            .event_mask(
                EventMask::EXPOSURE
                    | EventMask::STRUCTURE_NOTIFY
                    | EventMask::BUTTON_PRESS
                    | EventMask::POINTER_MOTION
                    | EventMask::ENTER_WINDOW
                    | EventMask::LEAVE_WINDOW,
            ),
    )?
    .check()?;

    let spec = dock_spec(0, 0, u32::from(current_width), current_height);
    write_dock_properties(&conn, win, &spec.properties())?;

    // 绑定 WGPU
    let target = Arc::new(XcbTarget {
        conn: conn.get_raw_xcb_connection(),
        screen: screen_num as i32,
        window: win,
        visual_id: screen.root_visual,
    });
    let mut gpu =
        WgpuPresenter::new_blocking(target, u32::from(current_width), u32::from(current_height))?;
    let mut canvas = CpuCanvas::new();

    conn.map_window(win)?.check()?;
    conn.flush()?;

    // Watch the root window so wallpaper swaps rebuild the frosted strip.
    conn.change_window_attributes(
        screen.root,
        &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )?;

    // A configured wallpaper wins: it is the only source that still works when
    // the compositor draws the wallpaper itself instead of publishing a root
    // pixmap, which is what JWM does.
    let source = match app_config.glass.file_source(
        u32::from(screen.width_in_pixels),
        u32::from(screen.height_in_pixels),
        fallback,
    ) {
        Some(file) => WallpaperOrigin::File(file),
        None => WallpaperOrigin::RootPixmap(RootPixmapSource {
            conn: &conn,
            root: screen.root,
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
    };

    let mut initial_update = bar.tick();
    initial_update.merge(bar.poll_transport());
    window.apply_runtime_update(initial_update)?;

    redraw(
        &mut gpu,
        &mut canvas,
        &window,
        current_width,
        current_height,
        &mut bar,
    )?;

    let timer = AlignedTimer::new(Duration::from_secs(1))?;
    let mut epoll = Epoll::new()?;
    epoll.add(window.conn.as_fd(), X_TOKEN)?;
    epoll.add(timer.as_fd(), TIMER_TOKEN)?;
    let mut notifier_slot = TransportNotifierSlot::new(true);
    sync_notifier(&mut notifier_slot, bar.runtime(), &epoll)?;

    let mut ready_tokens = Vec::new();
    loop {
        ready_tokens.clear();
        ready_tokens.extend(epoll.wait()?);
        for token in &ready_tokens {
            match *token {
                X_TOKEN => {
                    while let Some(x_event) = conn.poll_for_event()? {
                        let should_redraw = match x_event {
                            Event::Expose(event) => event.count == 0,
                            Event::ConfigureNotify(event) if event.window == win => {
                                current_width = event.width;
                                current_height = event.height;
                                window.sync_bar_height(&mut bar, event.height);
                                gpu.resize(u32::from(current_width), u32::from(current_height));
                                true
                            }
                            Event::EnterNotify(event) => bar.pointer_motion(Point::new(
                                f32::from(event.event_x),
                                f32::from(event.event_y),
                            )),
                            Event::MotionNotify(event) => bar.pointer_motion(Point::new(
                                f32::from(event.event_x),
                                f32::from(event.event_y),
                            )),
                            Event::LeaveNotify(_) => bar.pointer_leave(),
                            Event::PropertyNotify(event) if event.window == screen.root => {
                                window.wallpaper_changed(event.atom)
                            }
                            Event::ButtonPress(event) => {
                                if let Some(input) = PointerAction::from_x11_button(event.detail) {
                                    let update = bar.pointer_action(
                                        Point::new(
                                            f32::from(event.event_x),
                                            f32::from(event.event_y),
                                        ),
                                        input,
                                    );
                                    window.apply_runtime_update(update)?
                                } else {
                                    false
                                }
                            }
                            _ => false,
                        };
                        if should_redraw {
                            redraw(
                                &mut gpu,
                                &mut canvas,
                                &window,
                                current_width,
                                current_height,
                                &mut bar,
                            )?;
                        }
                    }
                }
                TIMER_TOKEN => {
                    if timer.drain()? > 0 {
                        let mut update = bar.tick();
                        update.merge(bar.poll_transport());
                        let needs_redraw = window.apply_runtime_update(update)?;
                        sync_notifier(&mut notifier_slot, bar.runtime(), &epoll)?;
                        if needs_redraw {
                            redraw(
                                &mut gpu,
                                &mut canvas,
                                &window,
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
                                &mut gpu,
                                &mut canvas,
                                &window,
                                current_width,
                                current_height,
                                &mut bar,
                            )?;
                        }
                    }
                }
                token => log::debug!("unexpected epoll token: {token}"),
            }
        }
    }
}
