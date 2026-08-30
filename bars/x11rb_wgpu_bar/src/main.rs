use anyhow::Result;
use pango::FontDescription;
use std::cell::{Cell, RefCell};
use std::env;
use std::ffi::c_void;
use std::num::NonZeroU32;
use std::os::fd::AsFd as _;
use std::ptr::NonNull;
use std::sync::Arc;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ColormapAlloc, ConfigureWindowAux, ConnectionExt as _, CreateWindowAux,
    EventMask, PropMode, Screen, VisualClass, Visualtype, Window, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;
use x11rb::xcb_ffi::XCBConnection;

use raw_window_handle::{
    DisplayHandle, HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
    WindowHandle, XcbDisplayHandle, XcbWindowHandle,
};

use xbar_core::glass::DEFAULT_BACKGROUND_OPACITY;
use xbar_core::linux::{AlignedTimer, Epoll};
use xbar_core::presentation::Point;
use xbar_core::render::cairo::{CairoBar, CpuCanvas, PointerInput};
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

// ---------------- Compositor detection ----------------
/// True when a compositing manager owns the conventional `_NET_WM_CM_Sn`
/// selection. With one the bar asks for a depth-32 window and paints real
/// alpha for the compositor to blend; without one it paints a solid bar.
/// Sampled once at startup because the visual is a creation-time choice — a
/// compositor toggled afterwards needs a bar restart — and owning the
/// selection only promises compositing, not that anything blurs behind us.
fn compositor_active(conn: &XCBConnection, screen_number: usize) -> bool {
    let owner = intern_atom(conn, &format!("_NET_WM_CM_S{screen_number}"))
        .and_then(|atom| Ok(conn.get_selection_owner(atom)?.reply()?.owner));
    match owner {
        Ok(owner) => owner != x11rb::NONE,
        Err(error) => {
            log::debug!("compositor selection lookup failed: {error}");
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
    gpu: &mut WgpuPresenter,
    canvas: &mut CpuCanvas,
    window: &WindowAdapter<'_>,
    width: u16,
    height: u16,
    bar: &mut CairoBar,
) -> Result<()> {
    loop {
        let frame = canvas.render(bar, u32::from(width), u32::from(height), 1.0)?;
        let damage = frame.damage.map(|rect| PresentRect {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
        });
        gpu.present_bgra(frame.data, frame.stride, damage)?;

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
    // The macOS-style template icons every bar renders from.
    presentation.apply_nerd_font_icons_if_stock();
    let bar_height = presentation
        .bar_height
        .round()
        .clamp(1.0, f32::from(u16::MAX)) as u16;
    let font = FontDescription::from_string(&app_config.font);
    let mut bar = CairoBar::new(runtime, presentation, font);

    // Only ask wgpu for an alpha-carrying surface when a compositor can blend
    // it and the server has an ARGB visual to build the window on; the surface
    // negotiation below still has the final say.
    let argb_visual = if compositor_active(&conn, screen_num) {
        find_argb_visual(screen)
    } else {
        None
    };
    let want_alpha = argb_visual.is_some();

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
    let window_visual = match &argb_visual {
        Some(visual) => {
            // A depth-32 window needs an explicit border pixel and colormap
            // for its non-default visual, or CreateWindow fails with BadMatch.
            let colormap = conn.generate_id()?;
            conn.create_colormap(ColormapAlloc::NONE, colormap, screen.root, visual.visual_id)?
                .check()?;
            conn.create_window(
                32,
                win,
                screen.root,
                0,
                0,
                current_width,
                current_height,
                0,
                WindowClass::INPUT_OUTPUT,
                visual.visual_id,
                &CreateWindowAux::new()
                    .background_pixel(0)
                    .border_pixel(0)
                    .colormap(colormap)
                    .event_mask(event_mask),
            )?
            .check()?;
            visual.visual_id
        }
        None => {
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
                    .event_mask(event_mask),
            )?
            .check()?;
            screen.root_visual
        }
    };

    let spec = dock_spec(0, 0, u32::from(current_width), current_height);
    write_dock_properties(&conn, win, &spec.properties())?;

    // 绑定 WGPU
    let target = Arc::new(XcbTarget {
        conn: conn.get_raw_xcb_connection(),
        screen: screen_num as i32,
        window: win,
        visual_id: window_visual,
    });
    let mut gpu = WgpuPresenter::new_blocking(
        target,
        u32::from(current_width),
        u32::from(current_height),
        want_alpha,
    )?;
    let mut canvas = CpuCanvas::new();

    // Only when the surface really carries alpha may the background go
    // translucent; anywhere short of that the bar must paint fully opaque —
    // a 0.55 wash over an undefined clear is never acceptable.
    let translucent = want_alpha && gpu.is_transparent();
    bar.renderer_mut().set_background_opacity(if translucent {
        Some(
            app_config
                .background_opacity
                .unwrap_or(DEFAULT_BACKGROUND_OPACITY),
        )
    } else {
        None
    });

    conn.map_window(win)?.check()?;
    conn.flush()?;

    let window = WindowAdapter {
        conn: &conn,
        screen,
        win,
        bar_height: Cell::new(bar_height),
        effects: RefCell::new(EffectRouter::default()),
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
                    &mut gpu,
                    &mut canvas,
                    &window,
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
                            Event::EnterNotify(event) => route_pointer_input(
                                &window,
                                &mut bar,
                                PointerInput::Move(Point::new(
                                    f32::from(event.event_x),
                                    f32::from(event.event_y),
                                )),
                            )?,
                            Event::MotionNotify(event) => route_pointer_input(
                                &window,
                                &mut bar,
                                PointerInput::Move(Point::new(
                                    f32::from(event.event_x),
                                    f32::from(event.event_y),
                                )),
                            )?,
                            Event::LeaveNotify(_) => {
                                route_pointer_input(&window, &mut bar, PointerInput::Leave)?
                            }
                            Event::ButtonPress(event) => {
                                let point =
                                    Point::new(f32::from(event.event_x), f32::from(event.event_y));
                                if let Some(input) =
                                    PointerInput::from_x11_button(point, event.detail, true)
                                {
                                    route_pointer_input(&window, &mut bar, input)?
                                } else {
                                    false
                                }
                            }
                            Event::ButtonRelease(event) => {
                                let point =
                                    Point::new(f32::from(event.event_x), f32::from(event.event_y));
                                if let Some(input) =
                                    PointerInput::from_x11_button(point, event.detail, false)
                                {
                                    route_pointer_input(&window, &mut bar, input)?
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
