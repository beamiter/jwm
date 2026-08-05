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
    Atom, AtomEnum, ConfigureWindowAux, ConnectionExt as _, CreateWindowAux, EventMask, PropMode,
    Window, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;
use x11rb::xcb_ffi::XCBConnection;

use raw_window_handle::{
    DisplayHandle, HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
    WindowHandle, XcbDisplayHandle, XcbWindowHandle,
};

use xbar_core::glass::wallpaper::WallpaperFile;
use xbar_core::glass::{DEFAULT_BACKGROUND_OPACITY, GlassBackdrop, fallback_rgb};
use xbar_core::linux::{AlignedTimer, Epoll};
use xbar_core::presentation::{Point, PointerAction};
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
    /// Frosted backdrop, present only when a wallpaper was configured. This
    /// window is on the root visual and can never be genuinely translucent, so
    /// glass here always means a baked strip.
    glass: RefCell<Option<GlassBackdrop<WallpaperFile>>>,
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
    let backdrop = glass.as_mut().and_then(|glass| {
        let (x, y) = window.root_origin();
        glass.ensure(x, y, u32::from(width), u32::from(height))
    });
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

    let presentation = app_config.presentation.clone();
    let bar_height = presentation
        .bar_height
        .round()
        .clamp(1.0, f32::from(u16::MAX)) as u16;
    let font = FontDescription::from_string(&app_config.font);
    let mut bar = CairoBar::new(runtime, presentation, font);
    // A frosted backdrop only reads as a material if the bar's own background
    // lets some of it through, so glass changes what "no opacity configured"
    // should mean.
    let glass = app_config.glass.file_backdrop(
        u32::from(screen.width_in_pixels),
        u32::from(screen.height_in_pixels),
        fallback_rgb(app_config.theme),
    );
    match app_config.background_opacity {
        Some(opacity) => bar.renderer_mut().set_background_opacity(Some(opacity)),
        None if glass.is_some() => bar
            .renderer_mut()
            .set_background_opacity(Some(DEFAULT_BACKGROUND_OPACITY)),
        None => {}
    }

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

    let window = WindowAdapter {
        conn: &conn,
        screen,
        win,
        bar_height: Cell::new(bar_height),
        effects: RefCell::new(EffectRouter::default()),
        glass: RefCell::new(glass),
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
