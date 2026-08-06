use anyhow::{Result, anyhow};
use pango::FontDescription;
use std::cell::{Cell, RefCell};
use std::env;
use std::ffi::c_void;
use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd};
use std::ptr::NonNull;
use std::sync::Arc;
use std::time::Duration;

use xcb::{self, Xid, x};

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

const BAR_NAME: &str = "xcb_wgpu_bar";
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
        let handle = XcbWindowHandle::new(std::num::NonZeroU32::new(self.window).unwrap());
        Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::Xcb(handle)) })
    }
}

// ============================================================================
// 3. XCB platform adapter and Cairo-to-wgpu presentation
// ============================================================================
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
///
/// Unlike the Cairo bars this one asks the server for the pixels directly:
/// there is no Cairo surface on the X connection to copy through, and a
/// `GetImage` reply is already in the byte order `GlassImage` wants.
struct RootPixmapSource<'a> {
    conn: &'a xcb::Connection,
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

/// Read the wallpaper under the bar out of the root pixmap.
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
    // 32 bits per pixel is what every TrueColor visual of these depths uses,
    // and it is the only layout `GlassImage` can take without a conversion.
    if geometry.depth() != 24 && geometry.depth() != 32 {
        return Err(anyhow!("unsupported wallpaper depth {}", geometry.depth()));
    }
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

    let image = conn.wait_for_reply(conn.send_request(&x::GetImage {
        format: x::ImageFormat::ZPixmap,
        drawable: x::Drawable::Pixmap(wallpaper),
        x: src_x as i16,
        y: src_y as i16,
        width,
        height: available_height,
        plane_mask: u32::MAX,
    }))?;
    Ok(GlassImage::from_bgra(
        u32::from(width),
        u32::from(available_height),
        usize::from(width) * 4,
        image.data(),
    )?)
}

fn change_property_32(
    conn: &xcb::Connection,
    win: x::Window,
    property: x::Atom,
    property_type: x::Atom,
    data: &[u32],
) -> Result<()> {
    // The u32 element type makes xcb emit format=32. Converting these values
    // to bytes would silently emit a malformed format=8 EWMH property.
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

struct WindowAdapter<'a> {
    conn: &'a xcb::Connection,
    screen: &'a x::Screen,
    win: x::Window,
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
        let cookie = self.conn.send_request(&x::TranslateCoordinates {
            src_window: self.win,
            dst_window: self.screen.root(),
            src_x: 0,
            src_y: 0,
        });
        match self.conn.wait_for_reply(cookie) {
            Ok(reply) => (i32::from(reply.dst_x()), i32::from(reply.dst_y())),
            Err(error) => {
                log::debug!("translate coordinates failed: {error}");
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

    let (conn, screen_num) = xcb::Connection::connect(None)?;
    let setup = conn.get_setup();
    let screen = setup
        .roots()
        .nth(screen_num as usize)
        .ok_or_else(|| anyhow!("no X screen found"))?;

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

    let win = conn.generate_id();
    let mut current_width = screen.width_in_pixels();
    let mut current_height = bar_height;

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
        visual: screen.root_visual(),
        value_list: &[
            x::Cw::BackPixmap(x::Pixmap::none()),
            x::Cw::EventMask(
                x::EventMask::EXPOSURE
                    | x::EventMask::STRUCTURE_NOTIFY
                    | x::EventMask::BUTTON_PRESS
                    | x::EventMask::POINTER_MOTION
                    | x::EventMask::ENTER_WINDOW
                    | x::EventMask::LEAVE_WINDOW,
            ),
        ],
    })?;

    let spec = dock_spec(0, 0, u32::from(current_width), current_height);
    write_dock_properties(&conn, win, &spec.properties())?;

    // 绑定 WGPU
    let target = Arc::new(XcbTarget {
        conn: conn.get_raw_conn() as *mut c_void,
        screen: screen_num,
        window: win.resource_id(),
    });
    let mut gpu =
        WgpuPresenter::new_blocking(target, u32::from(current_width), u32::from(current_height))?;
    let mut canvas = CpuCanvas::new();

    conn.send_and_check_request(&x::MapWindow { window: win })?;
    conn.flush()?;

    // Watch the root window so wallpaper swaps rebuild the frosted strip.
    conn.send_and_check_request(&x::ChangeWindowAttributes {
        window: screen.root(),
        value_list: &[x::Cw::EventMask(x::EventMask::PROPERTY_CHANGE)],
    })?;

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
                X_TOKEN => loop {
                    let Some(x_event) = conn.poll_for_event()? else {
                        break;
                    };
                    let should_redraw = match x_event {
                        xcb::Event::X(x::Event::Expose(event)) => event.count() == 0,
                        xcb::Event::X(x::Event::ConfigureNotify(event))
                            if event.window() == win =>
                        {
                            current_width = event.width();
                            current_height = event.height();
                            window.sync_bar_height(&mut bar, event.height());
                            gpu.resize(u32::from(current_width), u32::from(current_height));
                            true
                        }
                        xcb::Event::X(x::Event::EnterNotify(event)) => bar.pointer_motion(
                            Point::new(f32::from(event.event_x()), f32::from(event.event_y())),
                        ),
                        xcb::Event::X(x::Event::MotionNotify(event)) => bar.pointer_motion(
                            Point::new(f32::from(event.event_x()), f32::from(event.event_y())),
                        ),
                        xcb::Event::X(x::Event::LeaveNotify(_)) => bar.pointer_leave(),
                        xcb::Event::X(x::Event::PropertyNotify(event))
                            if event.window() == screen.root() =>
                        {
                            window.wallpaper_changed(event.atom())
                        }
                        xcb::Event::X(x::Event::ButtonPress(event)) => {
                            if let Some(input) = PointerAction::from_x11_button(event.detail()) {
                                let update = bar.pointer_action(
                                    Point::new(
                                        f32::from(event.event_x()),
                                        f32::from(event.event_y()),
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
                },
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
