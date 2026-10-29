use anyhow::{Result, anyhow};
use pango::FontDescription;
use std::cell::{Cell, RefCell};
use std::env;
use std::ffi::c_void;
use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd};
use std::ptr::NonNull;
use std::sync::Arc;
use std::time::{Duration, Instant};

use xcb::{self, Xid, x};

use raw_window_handle::{
    DisplayHandle, HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
    WindowHandle, XcbDisplayHandle, XcbWindowHandle,
};

use xbar_core::linux::{AlignedTimer, Epoll};
use xbar_core::presentation::Point;
use xbar_core::render::cairo::{CairoBar, CpuCanvas, PointerInput};
use xbar_core::{
    BarEdge, BarPlacement, BarRuntime, DockProperty, DockPropertyValue, DockWindowSpec,
    MonitorGeometry, NotifierChange, RuntimeUpdate, TransportNotifierSlot, TransportRecoveryConfig,
};
use xbar_linux_actions::{EffectRouter, GeometryRequest};
use xbar_present_wgpu::{PresentRect, WgpuPresenter};

const BAR_NAME: &str = "xcb_wgpu_bar";
const X_TOKEN: u64 = 1;
// Bound X work per turn so provider/IPC readiness still gets serviced.
const X_EVENT_BUDGET: usize = 256;
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
        let mut handle = XcbWindowHandle::new(std::num::NonZeroU32::new(self.window).unwrap());
        handle.visual_id = std::num::NonZeroU32::new(self.visual_id);
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

// ---------------- Compositor detection ----------------
/// True when a compositing manager owns the conventional `_NET_WM_CM_Sn`
/// selection. With one the bar asks for a depth-32 window and paints real
/// alpha for the compositor to blend; without one it paints a solid bar.
/// Sampled once at startup because the visual is a creation-time choice — a
/// compositor toggled afterwards needs a bar restart — and owning the
/// selection only promises compositing, not that anything blurs behind us.
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
    edge: BarEdge,
    managed: bool,
    effects: RefCell<EffectRouter>,
}

impl WindowAdapter<'_> {
    fn sync_bar_size(
        &self,
        bar: &mut CairoBar,
        width: u16,
        height: u16,
        x: i16,
        y: i16,
    ) -> Result<()> {
        // Geometry is authoritative, but only the short axis controls cell
        // thickness. A vertical resize must never turn output height into
        // bar_height and consequently into enormous hit targets.
        let thickness = if self.edge.is_vertical() {
            width
        } else {
            height
        }
        .max(1);
        self.bar_height.set(thickness);
        bar.config_mut().bar_height = f32::from(thickness);
        self.sync_struts(BarPlacement {
            x: i32::from(x),
            y: i32::from(y),
            width: u32::from(width),
            height: u32::from(height),
        })
    }

    fn root_geometry(&self) -> Result<MonitorGeometry> {
        let cookie = self.conn.send_request(&x::GetGeometry {
            drawable: x::Drawable::Window(self.screen.root()),
        });
        let geometry = self.conn.wait_for_reply(cookie)?;
        Ok(MonitorGeometry {
            x: 0,
            y: 0,
            width: u32::from(geometry.width()),
            height: u32::from(geometry.height()),
        })
    }

    fn sync_struts(&self, placement: BarPlacement) -> Result<()> {
        if !self.managed {
            let spec =
                DockWindowSpec::for_edge(BAR_NAME, placement, self.root_geometry()?, self.edge);
            write_dock_properties(self.conn, self.win, &spec.strut_properties())?;
            self.conn.flush()?;
        }
        Ok(())
    }

    fn apply_runtime_update(&self, update: RuntimeUpdate) -> Result<bool> {
        self.effects.borrow_mut().route(update, |request| {
            let thickness = f64::from(self.bar_height.get());
            let placement = match request {
                // Shared snapshots already contain the final padded origin.
                GeometryRequest::Apply(geometry) => {
                    BarPlacement::from_bar_geometry(geometry, thickness, 1.0, self.edge)?
                }
                GeometryRequest::Clear => {
                    BarPlacement::at_edge(self.root_geometry()?, thickness, 1.0, self.edge)?
                }
            };
            self.apply_geometry(placement)
        })
    }

    fn apply_geometry(&self, placement: BarPlacement) -> Result<()> {
        self.conn.send_and_check_request(&x::ConfigureWindow {
            window: self.win,
            value_list: &[
                x::ConfigWindow::X(placement.x),
                x::ConfigWindow::Y(placement.y),
                x::ConfigWindow::Width(placement.width),
                x::ConfigWindow::Height(placement.height),
            ],
        })?;
        self.sync_struts(placement)?;
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

fn destroys_window(event: &xcb::Event, window: x::Window) -> bool {
    matches!(
        event,
        xcb::Event::X(x::Event::DestroyNotify(event)) if event.window() == window
    )
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

    let (conn, screen_num) = xcb::Connection::connect(None)?;
    let setup = conn.get_setup();
    let screen = setup
        .roots()
        .nth(screen_num as usize)
        .ok_or_else(|| anyhow!("no X screen found"))?;

    let mut presentation = app_config.presentation.clone();
    let desktop_header = presentation.apply_desktop_header_profile();
    // The macOS-style template icons every bar renders from.
    presentation.apply_nerd_font_icons_if_stock();
    let edge = presentation.edge;
    let bar_height = presentation
        .logical_thickness(1.0)
        .round()
        .clamp(1.0, f64::from(u16::MAX)) as u16;
    let font = FontDescription::from_string(&app_config.font);
    let mut bar = CairoBar::new(runtime, presentation, font);
    bar.renderer_mut().set_capsule(desktop_header);

    // Only ask wgpu for an alpha-carrying surface when a compositor can blend
    // it and the server has an ARGB visual to build the window on. That native
    // alpha channel is authoritative even when GLES reports an Opaque WSI.
    let argb_visual = if compositor_active(&conn, screen_num) {
        find_argb_visual(screen)
    } else {
        None
    };
    let want_alpha = argb_visual.is_some();

    let win = conn.generate_id();
    let root_geometry = MonitorGeometry {
        x: 0,
        y: 0,
        width: u32::from(screen.width_in_pixels()),
        height: u32::from(screen.height_in_pixels()),
    };
    let initial_placement = BarPlacement::at_edge(root_geometry, f64::from(bar_height), 1.0, edge)?;
    let mut current_width = u16::try_from(initial_placement.width)?;
    let mut current_height = u16::try_from(initial_placement.height)?;
    let initial_x = i16::try_from(initial_placement.x)?;
    let initial_y = i16::try_from(initial_placement.y)?;
    let event_mask = x::EventMask::EXPOSURE
        | x::EventMask::STRUCTURE_NOTIFY
        | x::EventMask::BUTTON_PRESS
        | x::EventMask::BUTTON_RELEASE
        | x::EventMask::POINTER_MOTION
        | x::EventMask::ENTER_WINDOW
        | x::EventMask::LEAVE_WINDOW;
    let window_visual = match &argb_visual {
        Some(visual) => {
            // A depth-32 window needs an explicit border pixel and colormap
            // for its non-default visual, or CreateWindow fails with BadMatch.
            let colormap = conn.generate_id();
            conn.send_and_check_request(&x::CreateColormap {
                alloc: x::ColormapAlloc::None,
                mid: colormap,
                window: screen.root(),
                visual: visual.visual_id(),
            })?;
            conn.send_and_check_request(&x::CreateWindow {
                depth: 32,
                wid: win,
                parent: screen.root(),
                x: 0,
                y: 0,
                width: current_width,
                height: current_height,
                border_width: 0,
                class: x::WindowClass::InputOutput,
                visual: visual.visual_id(),
                value_list: &[
                    x::Cw::BackPixel(0),
                    x::Cw::BorderPixel(0),
                    x::Cw::EventMask(event_mask),
                    x::Cw::Colormap(colormap),
                ],
            })?;
            visual.visual_id()
        }
        None => {
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
                    x::Cw::EventMask(event_mask),
                ],
            })?;
            screen.root_visual()
        }
    };

    let spec = DockWindowSpec::for_edge(BAR_NAME, initial_placement, root_geometry, edge);
    let properties: Vec<_> = spec
        .properties()
        .into_iter()
        .chain(std::iter::once(DockProperty {
            name: "_NET_WM_PID",
            value: DockPropertyValue::Cardinals(vec![std::process::id()]),
        }))
        .filter(|property| shared_path.is_empty() || !property.name.starts_with("_NET_WM_STRUT"))
        .collect();
    write_dock_properties(&conn, win, &properties)?;
    // The compositor uses WM_CLASS for the header's material and outline.
    // The title alone identifies the window to JWM but does not set that class.
    let wm_class = format!("{BAR_NAME}\0{BAR_NAME}\0");
    change_property_8(
        &conn,
        win,
        x::ATOM_WM_CLASS,
        x::ATOM_STRING,
        wm_class.as_bytes(),
    )?;

    // 绑定 WGPU
    let target = Arc::new(XcbTarget {
        conn: conn.get_raw_conn() as *mut c_void,
        screen: screen_num,
        window: win.resource_id(),
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
    bar.follow_wm_background(
        app_config
            .background_opacity
            .or(desktop_header.then_some(0.58)),
        translucent,
    );

    conn.send_and_check_request(&x::MapWindow { window: win })?;
    conn.flush()?;

    let window = WindowAdapter {
        conn: &conn,
        screen,
        win,
        bar_height: Cell::new(bar_height),
        edge,
        managed: !shared_path.is_empty(),
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
        // A synchronous X reply can move events into the library's queue
        // while leaving the socket quiet. Inspect that queue before sleeping.
        let mut queued_x_event = conn.poll_for_event()?;
        ready_tokens.extend(
            epoll.wait_timeout_with_pending(
                dock_timeout,
                queued_x_event.as_ref().map(|_| X_TOKEN),
            )?,
        );
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
                    for _ in 0..X_EVENT_BUDGET {
                        let x_event = match queued_x_event.take() {
                            Some(event) => Some(event),
                            None => conn.poll_for_event()?,
                        };
                        let Some(x_event) = x_event else {
                            break;
                        };
                        if destroys_window(&x_event, win) {
                            break 'event_loop;
                        }
                        let should_redraw = match x_event {
                            xcb::Event::X(x::Event::Expose(event)) => event.count() == 0,
                            xcb::Event::X(x::Event::ConfigureNotify(event))
                                if event.window() == win =>
                            {
                                current_width = event.width();
                                current_height = event.height();
                                window.sync_bar_size(
                                    &mut bar,
                                    event.width(),
                                    event.height(),
                                    event.x(),
                                    event.y(),
                                )?;
                                gpu.resize(u32::from(current_width), u32::from(current_height));
                                true
                            }
                            xcb::Event::X(x::Event::EnterNotify(event)) => route_pointer_input(
                                &window,
                                &mut bar,
                                PointerInput::Move(Point::new(
                                    f32::from(event.event_x()),
                                    f32::from(event.event_y()),
                                )),
                            )?,
                            xcb::Event::X(x::Event::MotionNotify(event)) => route_pointer_input(
                                &window,
                                &mut bar,
                                PointerInput::Move(Point::new(
                                    f32::from(event.event_x()),
                                    f32::from(event.event_y()),
                                )),
                            )?,
                            xcb::Event::X(x::Event::LeaveNotify(_)) => {
                                route_pointer_input(&window, &mut bar, PointerInput::Leave)?
                            }
                            xcb::Event::X(x::Event::ButtonPress(event)) => {
                                let point = Point::new(
                                    f32::from(event.event_x()),
                                    f32::from(event.event_y()),
                                );
                                if let Some(input) =
                                    PointerInput::from_x11_button(point, event.detail(), true)
                                {
                                    route_pointer_input(&window, &mut bar, input)?
                                } else {
                                    false
                                }
                            }
                            xcb::Event::X(x::Event::ButtonRelease(event)) => {
                                let point = Point::new(
                                    f32::from(event.event_x()),
                                    f32::from(event.event_y()),
                                );
                                if let Some(input) =
                                    PointerInput::from_x11_button(point, event.detail(), false)
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::destroys_window;
    use xcb::{XidNew as _, x};

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
