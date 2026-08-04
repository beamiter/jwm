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

use xbar_core::linux::{AlignedTimer, Epoll};
use xbar_core::presentation::{Point, PointerAction};
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

fn redraw(
    gpu: &mut WgpuPresenter,
    canvas: &mut CpuCanvas,
    width: u16,
    height: u16,
    bar: &mut CairoBar,
) -> Result<()> {
    let frame = canvas.render(bar, u32::from(width), u32::from(height), 1.0)?;
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

    let presentation = app_config.presentation.clone();
    let bar_height = presentation
        .bar_height
        .round()
        .clamp(1.0, f32::from(u16::MAX)) as u16;
    let font = FontDescription::from_string(&app_config.font);
    let mut bar = CairoBar::new(runtime, presentation, font);
    if let Some(opacity) = app_config.background_opacity {
        bar.renderer_mut().set_background_opacity(Some(opacity));
    }

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
