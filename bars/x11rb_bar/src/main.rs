use anyhow::{Result, anyhow};
use cairo::ffi::{xcb_connection_t, xcb_visualtype_t};
use cairo::{Context, XCBConnection as CairoXCBConnection, XCBDrawable, XCBSurface, XCBVisualType};
use log::{debug, warn};
use pango::FontDescription;
use std::cell::{Cell, RefCell};
use std::env;
use std::os::fd::AsFd as _;
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::ConnectionExt as _;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ColormapAlloc, ConfigureWindowAux, CreateGCAux,
    CreateWindowAux, EventMask, Gcontext, Pixmap, PropMode, Screen, VisualClass, Visualtype,
    Window, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;
use x11rb::xcb_ffi::XCBConnection;
use xbar_core::glass::DEFAULT_BACKGROUND_OPACITY;
use xbar_core::linux::{AlignedTimer, Epoll};
use xbar_core::presentation::{Point, Size};
use xbar_core::render::cairo::{CairoBar, PointerInput};
use xbar_core::{
    BarPlacement, BarRuntime, DockProperty, DockPropertyValue, DockWindowSpec, MonitorGeometry,
    NotifierChange, RuntimeUpdate, TransportNotifierSlot, TransportRecoveryConfig,
};
use xbar_linux_actions::{EffectRouter, GeometryRequest};

const BAR_NAME: &str = "x11rb_bar";
const X_TOKEN: u64 = 1;
const TIMER_TOKEN: u64 = 2;
const SHARED_TOKEN: u64 = 3;
const TRANSPORT_RETRY_INTERVAL: Duration = Duration::from_secs(2);

// ---------------- Cairo XCB bridge ----------------
struct CairoXcb {
    connection: CairoXCBConnection,
    visual: XCBVisualType,
    _visual_owner: Box<xcb::x::Visualtype>,
}

fn find_visual_by_id_and_depth(
    screen: &Screen,
    target_visual_id: u32,
    target_depth: u8,
) -> Option<Visualtype> {
    screen
        .allowed_depths
        .iter()
        .find(|depth| depth.depth == target_depth)
        .and_then(|depth| {
            depth
                .visuals
                .iter()
                .find(|visual| visual.visual_id == target_visual_id)
        })
        .cloned()
}

fn build_cairo_xcb(
    conn: &XCBConnection,
    screen: &Screen,
    visual_id: u32,
    depth: u8,
) -> Result<CairoXcb> {
    let visual = find_visual_by_id_and_depth(screen, visual_id, depth)
        .ok_or_else(|| anyhow!("could not find the requested X visual"))?;
    let raw_visual = xcb::x::Visualtype::new(
        visual.visual_id,
        unsafe { std::mem::transmute::<u32, xcb::x::VisualClass>(u32::from(visual.class)) },
        visual.bits_per_rgb_value,
        visual.colormap_entries,
        visual.red_mask,
        visual.green_mask,
        visual.blue_mask,
    );
    let visual_owner = Box::new(raw_visual);
    let visual_ptr = (&*visual_owner) as *const xcb::x::Visualtype as *mut xcb_visualtype_t;
    let visual = unsafe { XCBVisualType::from_raw_none(visual_ptr) };
    let raw_connection = conn.get_raw_xcb_connection();
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
fn compositor_active(conn: &XCBConnection, screen_number: usize) -> bool {
    let owner = intern_atom(conn, &format!("_NET_WM_CM_S{screen_number}"))
        .and_then(|atom| Ok(conn.get_selection_owner(atom)?.reply()?.owner));
    match owner {
        Ok(owner) => owner != x11rb::NONE,
        Err(error) => {
            debug!("compositor selection lookup failed: {error}");
            false
        }
    }
}

/// A 32-bit TrueColor visual for translucent rendering, if the server has one.
fn find_argb_visual(screen: &Screen) -> Option<Visualtype> {
    screen
        .allowed_depths
        .iter()
        .find(|depth| depth.depth == 32)
        .and_then(|depth| {
            depth
                .visuals
                .iter()
                .find(|visual| visual.class == VisualClass::TRUE_COLOR)
        })
        .cloned()
}

// ---------------- X11 back buffer ----------------
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
    pixmap: Pixmap,
    width: u16,
    height: u16,
    depth: u8,
    surface: Option<XCBSurface>,
    context: Option<Context>,
}

impl BackBuffer {
    fn new(conn: &XCBConnection, depth: u8, win: Window, width: u16, height: u16) -> Result<Self> {
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
        conn: &XCBConnection,
        depth: u8,
        win: Window,
        width: u16,
        height: u16,
    ) -> Result<Pixmap> {
        let pixmap = conn.generate_id()?;
        conn.create_pixmap(depth, pixmap, win, width, height)?
            .check()?;
        Ok(pixmap)
    }

    fn free_pixmap(conn: &XCBConnection, pixmap: Pixmap) -> Result<()> {
        conn.free_pixmap(pixmap)?.check()?;
        Ok(())
    }

    fn ensure_context<'a>(&'a mut self, cairo_xcb: &CairoXcb) -> Result<&'a Context> {
        if self.surface.is_none() {
            let drawable = XCBDrawable(self.pixmap);
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
        conn: &XCBConnection,
        win: Window,
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

    fn blit_to_window(&self, conn: &XCBConnection, win: Window, gc: Gcontext) -> Result<()> {
        conn.copy_area(self.pixmap, win, gc, 0, 0, 0, 0, self.width, self.height)?;
        Ok(())
    }
}

// ---------------- EWMH ----------------
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
    for property in properties {
        let name = intern_atom(conn, property.name)?;
        match &property.value {
            DockPropertyValue::Atoms(values) => {
                let values = values
                    .iter()
                    .map(|value| intern_atom(conn, value))
                    .collect::<Result<Vec<Atom>>>()?;
                conn.change_property32(PropMode::REPLACE, win, name, AtomEnum::ATOM, &values)?;
            }
            DockPropertyValue::Cardinals(values) => {
                conn.change_property32(PropMode::REPLACE, win, name, AtomEnum::CARDINAL, values)?;
            }
            DockPropertyValue::Utf8Text(text) => {
                let utf8_string = intern_atom(conn, "UTF8_STRING")?;
                conn.change_property8(PropMode::REPLACE, win, name, utf8_string, text.as_bytes())?;
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

// ---------------- Platform integration ----------------
struct WindowAdapter<'a> {
    conn: &'a XCBConnection,
    screen: &'a Screen,
    win: Window,
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
        self.conn.configure_window(
            self.win,
            &ConfigureWindowAux::new()
                .x(geometry.x)
                .y(geometry.y)
                .width(width)
                .height(u32::from(bar_height)),
        )?;
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
    gc: Gcontext,
    width: u16,
    height: u16,
    bar: &mut CairoBar,
) -> Result<()> {
    let size = Size::new(f32::from(width), f32::from(height));
    // One render path serves both modes: the renderer's background opacity,
    // fixed at startup, decides whether the scene lands as a translucent wash
    // for the compositor to blend or as the opaque fallback background.
    let context = back.ensure_context(cairo_xcb)?;
    bar.render(context, size)?;
    let _ = bar.runtime_mut().take_changes();
    back.flush();
    back.blit_to_window(window.conn, window.win, gc)?;
    window.conn.flush()?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn handle_x_event(
    event: x11rb::protocol::Event,
    cairo_xcb: &CairoXcb,
    window: &WindowAdapter<'_>,
    back: &mut BackBuffer,
    gc: Gcontext,
    current_width: &mut u16,
    current_height: &mut u16,
    bar: &mut CairoBar,
) -> Result<()> {
    let mut should_redraw = false;

    match event {
        x11rb::protocol::Event::Expose(event) => {
            if event.count == 0 {
                back.blit_to_window(window.conn, window.win, gc)?;
                window.conn.flush()?;
            }
        }
        x11rb::protocol::Event::ConfigureNotify(event) if event.window == window.win => {
            *current_width = event.width;
            *current_height = event.height;
            window.sync_bar_height(bar, event.height);
            back.resize_if_needed(window.conn, window.win, *current_width, *current_height)?;
            should_redraw = true;
        }
        x11rb::protocol::Event::EnterNotify(event) => {
            should_redraw = route_pointer_input(
                window,
                bar,
                PointerInput::Move(Point::new(
                    f32::from(event.event_x),
                    f32::from(event.event_y),
                )),
            )?;
        }
        x11rb::protocol::Event::LeaveNotify(_) => {
            should_redraw = route_pointer_input(window, bar, PointerInput::Leave)?;
        }
        x11rb::protocol::Event::MotionNotify(event) => {
            should_redraw = route_pointer_input(
                window,
                bar,
                PointerInput::Move(Point::new(
                    f32::from(event.event_x),
                    f32::from(event.event_y),
                )),
            )?;
        }
        x11rb::protocol::Event::ButtonPress(event) => {
            let point = Point::new(f32::from(event.event_x), f32::from(event.event_y));
            if let Some(input) = PointerInput::from_x11_button(point, event.detail, true) {
                should_redraw = route_pointer_input(window, bar, input)?;
            }
        }
        x11rb::protocol::Event::ButtonRelease(event) => {
            let point = Point::new(f32::from(event.event_x), f32::from(event.event_y));
            if let Some(input) = PointerInput::from_x11_button(point, event.detail, false) {
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

#[allow(clippy::too_many_arguments)]
fn drain_x_events(
    cairo_xcb: &CairoXcb,
    window: &WindowAdapter<'_>,
    back: &mut BackBuffer,
    gc: Gcontext,
    current_width: &mut u16,
    current_height: &mut u16,
    bar: &mut CairoBar,
) -> Result<()> {
    while let Some(event) = window.conn.poll_for_event()? {
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

    let (conn, screen_number) = XCBConnection::connect(None)?;
    let screen = conn
        .setup()
        .roots
        .get(screen_number)
        .ok_or_else(|| anyhow!("no X screen found"))?;
    // Prefer real translucency when a compositor can blend it; otherwise the
    // bar stays a plain opaque window. Checked once, here, because the visual
    // is a creation-time decision — and the selection only says compositing
    // is on, not that the compositor blurs behind the bar.
    let argb_visual = if compositor_active(&conn, screen_number) {
        find_argb_visual(screen)
    } else {
        None
    };
    let (window_depth, window_visual) = match &argb_visual {
        Some(visual) => (32, visual.visual_id),
        None => (screen.root_depth, screen.root_visual),
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

    let win = conn.generate_id()?;
    let mut current_width = screen.width_in_pixels;
    let mut current_height = bar_height;
    let event_mask = EventMask::EXPOSURE
        | EventMask::STRUCTURE_NOTIFY
        | EventMask::BUTTON_PRESS
        | EventMask::BUTTON_RELEASE
        | EventMask::POINTER_MOTION
        | EventMask::ENTER_WINDOW
        | EventMask::LEAVE_WINDOW;
    if translucent {
        // A depth-32 window needs an explicit border pixel and colormap for
        // its non-default visual, or CreateWindow fails with BadMatch.
        let colormap = conn.generate_id()?;
        conn.create_colormap(ColormapAlloc::NONE, colormap, screen.root, window_visual)?
            .check()?;
        conn.create_window(
            window_depth,
            win,
            screen.root,
            0,
            0,
            current_width,
            current_height,
            0,
            WindowClass::INPUT_OUTPUT,
            window_visual,
            &CreateWindowAux::new()
                .background_pixel(0)
                .border_pixel(0)
                .colormap(colormap)
                .event_mask(event_mask),
        )?
        .check()?;
    } else {
        conn.create_window(
            x11rb::COPY_FROM_PARENT as u8,
            win,
            screen.root,
            0,
            0,
            current_width,
            current_height,
            0,
            WindowClass::INPUT_OUTPUT,
            window_visual,
            &CreateWindowAux::new()
                .background_pixmap(x11rb::NONE)
                .event_mask(event_mask),
        )?
        .check()?;
    }

    // The GC lives on the bar window so its depth always matches the back
    // buffer, whichever visual was chosen.
    let gc = conn.generate_id()?;
    conn.create_gc(gc, win, &CreateGCAux::new())?;

    let spec = dock_spec(0, 0, u32::from(current_width), current_height);
    write_dock_properties(&conn, win, &spec.properties())?;
    conn.map_window(win)?;
    if !translucent {
        conn.change_window_attributes(
            win,
            &ChangeWindowAttributesAux::new().background_pixmap(x11rb::NONE),
        )?;
    }
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

    // Seed providers and consume any snapshot queued before startup.
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
    epoll.add(window.conn.as_fd(), X_TOKEN)?;
    epoll.add(timer.as_fd(), TIMER_TOKEN)?;
    let mut notifier_slot = TransportNotifierSlot::new(true);
    sync_notifier(&mut notifier_slot, bar.runtime(), &epoll)?;

    let mut ready_tokens = Vec::new();
    loop {
        ready_tokens.clear();
        let now = Instant::now();
        let dock_timeout = bar
            .next_dock_deadline(now)
            .map(|deadline| deadline.saturating_duration_since(now));
        ready_tokens.extend(epoll.wait_timeout(dock_timeout)?);
        if ready_tokens.is_empty() {
            // Dock retries and moving-preview anchors have sub-second
            // deadlines independent from the aligned provider timer.
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

#[cfg(test)]
mod tests {
    use super::{BackBufferResize, plan_back_buffer_resize};

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
}
