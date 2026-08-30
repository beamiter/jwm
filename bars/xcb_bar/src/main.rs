use anyhow::{Result, anyhow};
use cairo::ffi::{xcb_connection_t, xcb_visualtype_t};
use cairo::{Context, XCBConnection as CairoXCBConnection, XCBDrawable, XCBSurface, XCBVisualType};
use log::{debug, warn};
use pango::FontDescription;
use std::cell::{Cell, RefCell};
use std::env;
use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd};
use std::time::{Duration, Instant};
use xbar_core::glass::DEFAULT_BACKGROUND_OPACITY;
use xbar_core::linux::{AlignedTimer, Epoll};
use xbar_core::presentation::{Point, Size};
use xbar_core::render::cairo::{CairoBar, PointerInput};
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
/// 32-bit visual and lets the compositor blend what lies behind it; without
/// one it paints the opaque fallback background. Sampled once at startup —
/// the visual is a creation-time decision, so a compositor toggled later
/// only takes effect on restart — and owning the selection promises
/// compositing, not that anything actually blurs behind the bar.
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

// ---------------- XCB back buffer ----------------
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BackBufferResize {
    width: u16,
    height: u16,
}

fn plan_back_buffer_resize(
    current_width: u16,
    current_height: u16,
    requested_width: u16,
    requested_height: u16,
) -> Option<BackBufferResize> {
    (current_width != requested_width || current_height != requested_height).then_some(
        BackBufferResize {
            width: requested_width,
            height: requested_height,
        },
    )
}

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
        let pixmap = Self::create_pixmap(conn, depth, win, width, height)?;
        Ok(Self {
            pixmap,
            width,
            height,
            depth,
            surface: None,
            context: None,
        })
    }

    fn create_pixmap(
        conn: &xcb::Connection,
        depth: u8,
        win: x::Window,
        width: u16,
        height: u16,
    ) -> Result<x::Pixmap> {
        let pixmap = conn.generate_id();
        conn.send_and_check_request(&x::CreatePixmap {
            depth,
            pid: pixmap,
            drawable: x::Drawable::Window(win),
            width,
            height,
        })?;
        Ok(pixmap)
    }

    fn free_pixmap(conn: &xcb::Connection, pixmap: x::Pixmap) -> Result<()> {
        conn.send_and_check_request(&x::FreePixmap { pixmap })?;
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
        let Some(resize) = plan_back_buffer_resize(self.width, self.height, width, height) else {
            return Ok(());
        };

        // Allocate first so a failed replacement leaves the complete old
        // buffer — including its live Cairo wrappers — untouched.
        let replacement = Self::create_pixmap(conn, self.depth, win, resize.width, resize.height)?;
        let previous = self.pixmap;

        // Cairo's context retains the surface, and the surface borrows the X
        // drawable. Release them in dependency order while `previous` is
        // still a valid server resource.
        drop(self.context.take());
        drop(self.surface.take());

        if let Err(error) = Self::free_pixmap(conn, previous) {
            // The old state remains selected when its release is rejected.
            // Avoid leaking the successfully-created replacement while the
            // caller reports the X11 failure.
            let _ = Self::free_pixmap(conn, replacement);
            return Err(error);
        }

        self.pixmap = replacement;
        self.width = resize.width;
        self.height = resize.height;
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
}

impl WindowAdapter<'_> {
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

fn route_pointer_input(
    window: &WindowAdapter<'_>,
    bar: &mut CairoBar,
    input: PointerInput,
) -> Result<bool> {
    let update = bar.handle_pointer(input);
    let pointer_redraw = update.needs_redraw();
    let runtime_redraw = window.apply_runtime_update(update.into_runtime())?;
    Ok(pointer_redraw || runtime_redraw)
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
    // One render path serves both modes: the renderer's background opacity,
    // fixed at startup, decides whether the scene lands as a translucent wash
    // for the compositor to blend or as the opaque fallback background.
    loop {
        {
            let context = back.ensure_context(cairo_xcb)?;
            bar.render(context, size)?;
        }
        back.flush();
        back.blit_to_window(window.conn, window.win, gc)?;
        window.conn.flush()?;

        let update = bar.take_pending_runtime();
        let needs_redraw = if update.is_empty() {
            false
        } else {
            window.apply_runtime_update(update)?
        };
        let _ = bar.runtime_mut().take_changes();
        if !needs_redraw {
            break;
        }
    }
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
            should_redraw = route_pointer_input(
                window,
                bar,
                PointerInput::Move(Point::new(
                    f32::from(event.event_x()),
                    f32::from(event.event_y()),
                )),
            )?;
        }
        xcb::Event::X(x::Event::LeaveNotify(_)) => {
            should_redraw = route_pointer_input(window, bar, PointerInput::Leave)?;
        }
        xcb::Event::X(x::Event::MotionNotify(event)) => {
            should_redraw = route_pointer_input(
                window,
                bar,
                PointerInput::Move(Point::new(
                    f32::from(event.event_x()),
                    f32::from(event.event_y()),
                )),
            )?;
        }
        xcb::Event::X(x::Event::ButtonPress(event)) => {
            let point = Point::new(f32::from(event.event_x()), f32::from(event.event_y()));
            if let Some(input) = PointerInput::from_x11_button(point, event.detail(), true) {
                should_redraw = route_pointer_input(window, bar, input)?;
            }
        }
        xcb::Event::X(x::Event::ButtonRelease(event)) => {
            let point = Point::new(f32::from(event.event_x()), f32::from(event.event_y()));
            if let Some(input) = PointerInput::from_x11_button(point, event.detail(), false) {
                should_redraw = route_pointer_input(window, bar, input)?;
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

fn destroys_window(event: &xcb::Event, window: x::Window) -> bool {
    matches!(
        event,
        xcb::Event::X(x::Event::DestroyNotify(event)) if event.window() == window
    )
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
) -> Result<bool> {
    loop {
        match window.conn.poll_for_event() {
            Ok(Some(event)) => {
                if destroys_window(&event, window.win) {
                    return Ok(false);
                }
                handle_x_event(
                    event,
                    cairo_xcb,
                    window,
                    back,
                    gc,
                    current_width,
                    current_height,
                    bar,
                )?;
            }
            Ok(None) => return Ok(true),
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
    // bar stays a plain opaque window. Checked once, here, because the visual
    // is a creation-time decision — and the selection only says compositing
    // is on, not that the compositor blurs behind the bar.
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
    // The macOS-style template icons every bar renders from.
    presentation.apply_nerd_font_icons_if_stock();
    let bar_height = presentation
        .bar_height
        .round()
        .clamp(1.0, f32::from(u16::MAX)) as u16;
    let font = FontDescription::from_string(&app_config.font);
    let mut bar = CairoBar::new(runtime, presentation, font);
    // Under a compositor the background is a translucent wash the desktop
    // shows through, with an explicit config value still winning; without one
    // the palette background must paint fully opaque, so the configured
    // opacity is deliberately ignored rather than washed over nothing.
    if translucent {
        let opacity = app_config
            .background_opacity
            .unwrap_or(DEFAULT_BACKGROUND_OPACITY);
        bar.renderer_mut().set_background_opacity(Some(opacity));
    } else {
        bar.renderer_mut().set_background_opacity(None);
    }

    let win = conn.generate_id();
    let mut current_width = screen.width_in_pixels();
    let mut current_height = bar_height;
    let event_mask = x::EventMask::EXPOSURE
        | x::EventMask::STRUCTURE_NOTIFY
        | x::EventMask::BUTTON_PRESS
        | x::EventMask::BUTTON_RELEASE
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

    let window = WindowAdapter {
        conn: &conn,
        screen,
        win,
        bar_height: Cell::new(bar_height),
        effects: RefCell::new(EffectRouter::default()),
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
    'event_loop: loop {
        ready_tokens.clear();
        let now = Instant::now();
        let dock_timeout = bar
            .next_dock_deadline(now)
            .map(|deadline| deadline.saturating_duration_since(now));
        ready_tokens.extend(epoll.wait_timeout(dock_timeout)?);
        if ready_tokens.is_empty() {
            // A Dock deadline is independent from the aligned one-second
            // provider timer. Service it promptly so a final magnified anchor
            // and a command-ring backpressure retry are not stranded until
            // the next clock tick.
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
            continue;
        }
        for token in &ready_tokens {
            match *token {
                X_TOKEN => {
                    if !drain_x_events(
                        &cairo_xcb,
                        &window,
                        &mut back,
                        gc,
                        &mut current_width,
                        &mut current_height,
                        &mut bar,
                    )? {
                        break 'event_loop;
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{BackBufferResize, destroys_window, plan_back_buffer_resize};
    use xcb::{XidNew as _, x};

    #[test]
    fn identical_dimensions_keep_the_existing_back_buffer() {
        assert_eq!(plan_back_buffer_resize(1920, 42, 1920, 42), None);
    }

    #[test]
    fn either_dimension_change_plans_one_complete_replacement() {
        assert_eq!(
            plan_back_buffer_resize(1920, 42, 2560, 42),
            Some(BackBufferResize {
                width: 2560,
                height: 42,
            })
        );
        assert_eq!(
            plan_back_buffer_resize(2560, 42, 2560, 48),
            Some(BackBufferResize {
                width: 2560,
                height: 48,
            })
        );
    }

    #[test]
    fn only_the_bar_windows_destroy_event_stops_the_loop() {
        let target = x::Window::new(42);
        let other = x::Window::new(7);
        let destroyed = |event, window| {
            xcb::Event::X(x::Event::DestroyNotify(x::DestroyNotifyEvent::new(
                event, window,
            )))
        };

        assert!(destroys_window(&destroyed(other, target), target));
        assert!(!destroys_window(&destroyed(target, other), target));
    }
}
