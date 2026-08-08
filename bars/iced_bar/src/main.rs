use iced::futures::{SinkExt, Stream};
use iced::mouse;
use iced::time;
use iced::widget::container;
use iced::widget::{Space, button, rich_text, tooltip};
use iced::widget::{mouse_area, span};
use iced::{Font, stream, theme};

use iced::window::Id;
use iced::{
    Background, Border, Color, Element, Length, Padding, Size, Subscription, Task, Theme, border,
    color,
    widget::{Column, Row, text},
    window,
};

use log::{debug, info, warn};
use std::env;
use std::ffi::c_void;
use std::num::NonZeroU32;
use std::ptr::NonNull;
use std::sync::{Once, OnceLock};
use std::time::Duration;
use std::time::Instant;

use raw_window_handle::{
    DisplayHandle, HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
    WindowHandle, XcbDisplayHandle, XcbWindowHandle,
};
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{
    ColormapAlloc, ConnectionExt as _, CreateWindowAux, VisualClass, WindowClass,
};
use x11rb::xcb_ffi::XCBConnection;

use xbar_core::config::BarConfig;
use xbar_core::glass::{DEFAULT_BACKGROUND_OPACITY, fallback_rgb};
use xbar_core::logging::init as initialize_logging;
use xbar_core::{
    BarEffect, BarRuntime, DOCK_ITEM_HEIGHT, DOCK_ITEM_WIDTH, DOCK_SLOT_WIDTH, DockBridge,
    DockItemBinding, LayoutId, ModelConfig, PlatformEffectHandler, RuntimeSchedule, RuntimeUpdate,
    ShellRoute, TagId, TransportRecoveryConfig, UserAction,
};
use xbar_linux_actions::ProcessActionHandler;

static _START: Once = Once::new();

const NERD_FONT: Font = Font::new("JetBrainsMono Nerd Font");

// Nerd-font icons used across the bar (aligned with tauri_react_bar)
const TAG_ICONS: [&str; 9] = [
    "\u{F0A1E}", // terminal
    "\u{F0239}", // browser
    "\u{F0A1B}", // code
    "\u{F0B79}", // chat
    "\u{F024B}", // folder
    "\u{F0388}", // music
    "\u{F0567}", // video
    "\u{F01F0}", // mail
    "\u{F0297}", // gamepad
];

const ICON_CPU: &str = "\u{F4BC}";
const ICON_MEM: &str = "\u{F035B}";
const ICON_BAT_FULL: &str = "\u{F0079}";
const ICON_BAT_CHG: &str = "\u{F0084}";
const ICON_VOL_HIGH: &str = "\u{F057E}";
const ICON_VOL_MID: &str = "\u{F0580}";
const ICON_VOL_LOW: &str = "\u{F057F}";
const ICON_VOL_MUTE: &str = "\u{F075F}";
const ICON_BRIGHT: &str = "\u{F00DE}";
const ICON_SHELL: &str = "\u{F0F2A}";
const ICON_SHOT: &str = "\u{F0104}";
const ICON_TIME: &str = "\u{F0954}";
const ICON_MON: &str = "\u{F0379}";
const ICON_M0: &str = "\u{F02DA}";
const ICON_M1: &str = "\u{F02DB}";
const TRANSPORT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const TRANSPORT_RETRY_INTERVAL: Duration = Duration::from_secs(2);

// ---------------- Compositor coupling ----------------
//
// Whether the bar gets a translucent or a solid window is decided once, in
// `main`, because window transparency is a creation-time choice in iced. The
// decision does not follow a compositor that starts or stops afterwards — a
// restarted bar picks up the change — and owning `_NET_WM_CM_S{n}` only says
// compositing is on, not that anything blurs behind the bar.
static TRANSLUCENT: OnceLock<bool> = OnceLock::new();

/// True when a compositing manager owns the conventional `_NET_WM_CM_Sn`
/// selection. Any failure along the way reads as "no compositor" so the bar
/// lands on the solid side, which is always safe to paint.
fn compositor_active(conn: &XCBConnection, screen_num: usize) -> bool {
    let name = format!("_NET_WM_CM_S{screen_num}");
    let Ok(cookie) = conn.intern_atom(false, name.as_bytes()) else {
        return false;
    };
    let Ok(atom) = cookie.reply() else {
        return false;
    };
    conn.get_selection_owner(atom.atom)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|reply| reply.owner != x11rb::NONE)
        .unwrap_or(false)
}

/// Raw-handle wrapper for the probe window, so wgpu can build a surface on a
/// window iced knows nothing about.
struct ProbeTarget {
    conn: *mut c_void,
    screen: i32,
    window: u32,
    visual_id: u32,
}

unsafe impl Send for ProbeTarget {}
unsafe impl Sync for ProbeTarget {}

impl HasDisplayHandle for ProbeTarget {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, raw_window_handle::HandleError> {
        let conn = NonNull::new(self.conn).ok_or(raw_window_handle::HandleError::Unavailable)?;
        let handle = XcbDisplayHandle::new(Some(conn), self.screen);
        Ok(unsafe { DisplayHandle::borrow_raw(RawDisplayHandle::Xcb(handle)) })
    }
}

impl HasWindowHandle for ProbeTarget {
    fn window_handle(&self) -> Result<WindowHandle<'_>, raw_window_handle::HandleError> {
        let window =
            NonZeroU32::new(self.window).ok_or(raw_window_handle::HandleError::Unavailable)?;
        let mut handle = XcbWindowHandle::new(window);
        handle.visual_id = NonZeroU32::new(self.visual_id);
        Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::Xcb(handle)) })
    }
}

/// Whether a wgpu surface on this display can actually deliver per-pixel
/// alpha. iced picks its surface alpha mode internally and never reports the
/// outcome, so the bar asks the same question ahead of time: build a throwaway
/// depth-32 window, get the surface capabilities, and accept only what
/// iced_wgpu itself would upgrade to a transparent surface — `PreMultiplied`.
/// Anything less and a transparent window would compose garbage, so the bar
/// must stay solid.
fn surface_alpha_capable(conn: &XCBConnection, screen_num: usize) -> bool {
    let Some(screen) = conn.setup().roots.get(screen_num) else {
        return false;
    };
    let Some(visual_id) = screen
        .allowed_depths
        .iter()
        .find(|depth| depth.depth == 32)
        .and_then(|depth| {
            depth
                .visuals
                .iter()
                .find(|visual| visual.class == VisualClass::TRUE_COLOR)
        })
        .map(|visual| visual.visual_id)
    else {
        return false;
    };
    let (Ok(colormap), Ok(window)) = (conn.generate_id(), conn.generate_id()) else {
        return false;
    };
    if conn
        .create_colormap(ColormapAlloc::NONE, colormap, screen.root, visual_id)
        .ok()
        .and_then(|cookie| cookie.check().ok())
        .is_none()
    {
        return false;
    }
    // The window is never mapped; it exists only so the surface has a real
    // depth-32 drawable to be judged against.
    let aux = CreateWindowAux::new()
        .background_pixel(0)
        .border_pixel(0)
        .override_redirect(1)
        .colormap(colormap);
    if conn
        .create_window(
            32,
            window,
            screen.root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            visual_id,
            &aux,
        )
        .ok()
        .and_then(|cookie| cookie.check().ok())
        .is_none()
    {
        let _ = conn.free_colormap(colormap);
        return false;
    }

    let target = ProbeTarget {
        conn: conn.get_raw_xcb_connection(),
        screen: screen_num as i32,
        window,
        visual_id,
    };
    // Env-aware, like the instance iced itself will build: a `WGPU_BACKEND`
    // override must steer the probe onto the same backend it steers iced onto.
    let instance =
        wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    let capable = instance
        .create_surface(target)
        .ok()
        .and_then(|surface| {
            let adapter = futures_lite::future::block_on(instance.request_adapter(
                &wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::LowPower,
                    compatible_surface: Some(&surface),
                    force_fallback_adapter: false,
                },
            ))
            .ok()?;
            Some(
                surface
                    .get_capabilities(&adapter)
                    .alpha_modes
                    .contains(&wgpu::CompositeAlphaMode::PreMultiplied),
            )
        })
        .unwrap_or(false);

    let _ = conn.destroy_window(window);
    let _ = conn.free_colormap(colormap);
    let _ = conn.flush();
    capable
}

fn main() -> iced::Result {
    let args: Vec<String> = env::args().collect();
    let application_id = "dev.iced.bar".to_string();
    let shared_path = args.iter().skip(1).last().cloned().unwrap_or_default();

    if let Err(e) = initialize_logging("iced_bar", &shared_path) {
        eprintln!("Failed to initialize logging: {}", e);
        std::process::exit(1);
    }

    // A session without a compositor — or without a surface that can carry
    // alpha — gets a solid window; anything else would paint a wash over
    // undefined memory. A failed X connection (native Wayland, no Xwayland)
    // counts as "no compositor" for the same reason.
    let translucent = match XCBConnection::connect(None) {
        Ok((conn, screen_num)) => {
            compositor_active(&conn, screen_num) && surface_alpha_capable(&conn, screen_num)
        }
        Err(_) => false,
    };
    info!("startup mode: translucent={translucent}");
    let _ = TRANSLUCENT.set(translucent);

    iced::application(IcedBar::new, IcedBar::update, IcedBar::view)
        .window(window::Settings {
            platform_specific: window::settings::PlatformSpecific {
                application_id,
                ..Default::default()
            },
            size: Size::from([800., 40.]),
            decorations: false,
            transparent: translucent,
            level: window::Level::AlwaysOnTop,
            ..Default::default()
        })
        .default_font(NERD_FONT)
        .subscription(IcedBar::subscription)
        .style(IcedBar::style)
        .title("iced_bar")
        .run()
}

#[allow(dead_code)]
enum Input {
    DoSomeWork,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
enum Message {
    TabSelected(usize),
    LayoutClicked(u32),
    ToggleLayoutSelector,
    ShowSecondsToggle,

    GetWindowId,
    WindowIdReceived(Option<Id>),

    GetScaleFactor(f32),
    InitialWindowSize(Size),
    InitialWindowPosition(Option<iced::Point>),
    WindowEvent(Id, window::Event),

    MouseEnterScreenShot,
    MouseExitScreenShot,
    LeftClick,
    RightClick,

    /// Ask the window manager to open its own shell surface.
    OpenShell,

    TransportPoll,

    // Audio
    AudioToggleMute,
    AudioAdjust(i32),

    // Brightness
    BrightnessAdjust(i32),

    DockHover {
        binding: DockItemBinding,
        hovered: bool,
    },
    DockRestore {
        binding: DockItemBinding,
    },
}

struct IcedBar {
    tabs: [&'static str; 9],
    tab_colors: [Color; 9],
    runtime: BarRuntime,
    schedule: RuntimeSchedule,
    process_actions: ProcessActionHandler,
    current_window_id: Option<Id>,
    window_metrics_received: u8,
    scale_factor: f32,
    initial_window_size: Size,
    initial_window_position: Option<iced::Point>,
    is_hovered: bool,
    mouse_position: Option<iced::Point>,
    background: Color,
    dock: DockBridge,
}

impl Default for IcedBar {
    fn default() -> Self {
        IcedBar::new()
    }
}

impl IcedBar {
    const DEFAULT_COLOR: Color = color!(0x666666);
    const TAB_WIDTH: f32 = 38.0;
    const TAB_HEIGHT: f32 = 32.0;
    const TAB_SPACING: f32 = 8.0;
    const PILL_HEIGHT: f32 = 26.0;

    fn new() -> Self {
        let args: Vec<String> = env::args().collect();
        let shared_path = args.iter().skip(1).last().cloned().unwrap_or_default();

        let config = ModelConfig {
            show_seconds: true,
            ..ModelConfig::default()
        };
        let runtime = if shared_path.is_empty() {
            BarRuntime::new(config)
        } else {
            let recovery = TransportRecoveryConfig::new(shared_path, TRANSPORT_RETRY_INTERVAL)
                .expect("static transport recovery config is valid");
            BarRuntime::with_managed_transport(config, recovery)
        }
        .expect("iced bar model configuration is valid");

        // The bar's background is `fallback_rgb` for the configured theme in
        // both modes; only the alpha differs. Translucent windows wash it to
        // the configured opacity so the compositor's blur shows through, solid
        // windows paint it fully opaque and ignore `background_opacity`.
        let bar_config = BarConfig::load_default().unwrap_or_else(|error| {
            warn!("falling back to the default bar config: {error}");
            BarConfig::default()
        });
        let translucent = TRANSLUCENT.get().copied().unwrap_or(false);
        let [r, g, b] = fallback_rgb(bar_config.theme);
        let alpha = if translucent {
            bar_config
                .background_opacity
                .unwrap_or(DEFAULT_BACKGROUND_OPACITY) as f32
        } else {
            1.0
        };

        Self {
            tabs: TAG_ICONS,
            tab_colors: [
                color!(0xFF6B6B), // red
                color!(0x4ECDC4), // cyan
                color!(0x45B7D1), // blue
                color!(0x96CEB4), // green
                color!(0xFECA57), // yellow
                color!(0xFF9FF3), // pink
                color!(0x54A0FF), // light blue
                color!(0x5F27CD), // purple
                color!(0x00D2D3), // teal
            ],
            runtime,
            schedule: RuntimeSchedule::default(),
            process_actions: ProcessActionHandler::default(),
            current_window_id: None,
            window_metrics_received: 0,
            scale_factor: 1.0,
            initial_window_size: Size::new(800.0, 40.0),
            initial_window_position: None,
            is_hovered: false,
            mouse_position: None,
            background: Color::from_rgba8(r, g, b, alpha),
            dock: DockBridge::new(),
        }
    }

    fn prepare_worker() -> impl Stream<Item = Message> {
        stream::channel(10, async |mut output| {
            let _ = output.send(Message::GetWindowId).await;
        })
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        let task = match message {
            Message::TabSelected(tab_index) => {
                info!("Tab selected: {}", tab_index);
                TagId::new(tab_index)
                    .map_or_else(Task::none, |tag| self.dispatch_wm(UserAction::ViewTag(tag)))
            }

            Message::LayoutClicked(layout_index) => {
                info!("Layout selected: {}", layout_index);
                self.dispatch_wm(UserAction::SetLayout(LayoutId(layout_index)))
            }

            Message::ToggleLayoutSelector => self.dispatch(UserAction::ToggleLayoutSelector),

            Message::GetWindowId => {
                info!("GetWindowId");
                window::latest().map(Message::WindowIdReceived)
            }

            Message::WindowIdReceived(window_id) => {
                if let Some(wid) = window_id {
                    info!("WindowIdReceived: {:?}", wid);
                    self.current_window_id = Some(wid);
                    self.window_metrics_received = 0;
                    Task::batch([
                        window::scale_factor(wid).map(Message::GetScaleFactor),
                        window::size(wid).map(Message::InitialWindowSize),
                        window::position(wid).map(Message::InitialWindowPosition),
                    ])
                } else {
                    warn!("WindowId not available yet");
                    Task::none()
                }
            }

            Message::MouseEnterScreenShot => {
                self.is_hovered = true;
                Task::none()
            }

            Message::MouseExitScreenShot => {
                self.is_hovered = false;
                self.mouse_position = None;
                Task::none()
            }

            Message::ShowSecondsToggle => {
                let toggle = self.dispatch(UserAction::ToggleSeconds);
                let update = self.runtime.tick();
                let tick = self.handle_runtime_update(update);
                Task::batch([toggle, tick])
            }

            Message::OpenShell => self.dispatch(UserAction::OpenShellHub(ShellRoute::Hub)),

            Message::LeftClick => self.dispatch(UserAction::Screenshot),

            Message::RightClick => Task::none(),

            Message::AudioToggleMute => self.dispatch(UserAction::ToggleMute),

            Message::AudioAdjust(delta) => self.dispatch(UserAction::AdjustVolume(delta)),

            Message::BrightnessAdjust(delta) => self.dispatch(UserAction::AdjustBrightness(delta)),

            Message::DockHover { binding, hovered } => {
                if hovered {
                    let _ = self.dock.enter(binding);
                } else {
                    let _ = self.dock.leave(binding);
                }
                Task::none()
            }

            Message::DockRestore { binding } => {
                let _ = self.dock.request_restore(binding);
                Task::none()
            }

            Message::GetScaleFactor(scale_factor) => {
                info!("scale_factor: {}", scale_factor);
                self.scale_factor = scale_factor;
                self.window_metrics_received |= 0b001;
                self.runtime
                    .view()
                    .geometry
                    .zip(self.current_window_id)
                    .map_or_else(Task::none, |(geometry, window_id)| {
                        self.apply_monitor_geometry(window_id, geometry)
                    })
            }

            Message::InitialWindowSize(size) => {
                self.initial_window_size = size;
                self.window_metrics_received |= 0b010;
                Task::none()
            }

            Message::InitialWindowPosition(position) => {
                self.initial_window_position = position;
                self.window_metrics_received |= 0b100;
                Task::none()
            }

            Message::WindowEvent(window_id, window::Event::Rescaled(scale_factor))
                if Some(window_id) == self.current_window_id =>
            {
                self.scale_factor = scale_factor;
                self.runtime
                    .view()
                    .geometry
                    .map_or_else(Task::none, |geometry| {
                        self.apply_monitor_geometry(window_id, geometry)
                    })
            }

            Message::WindowEvent(_, _) => Task::none(),

            Message::TransportPoll => {
                let update = self.schedule.service(&mut self.runtime);
                self.handle_runtime_update(update)
            }
        };
        Task::batch([task, self.service_dock()])
    }

    fn service_dock(&mut self) -> Task<Message> {
        let snapshot = self.runtime.snapshot();
        self.dock.synchronize(
            &snapshot,
            self.runtime.transport_generation(),
            f64::from(self.scale_factor),
            40.0,
            4.0,
        );
        let now = Instant::now();
        let mut tasks = Vec::new();
        for action in self.dock.pending_actions(now) {
            let update = self.runtime.dispatch(action);
            let accepted = !update.has_issues();
            tasks.push(self.handle_runtime_update(update));
            if accepted {
                self.dock.acknowledge(action, now);
            } else {
                self.dock.record_failure(now);
                break;
            }
        }
        Task::batch(tasks)
    }

    fn dispatch(&mut self, action: UserAction) -> Task<Message> {
        let update = self.runtime.dispatch(action);
        self.handle_runtime_update(update)
    }

    fn dispatch_wm(&mut self, action: UserAction) -> Task<Message> {
        if !self.runtime.view().wm_available {
            debug!("ignoring WM action while the WM projection is unavailable");
            return Task::none();
        }
        self.dispatch(action)
    }

    fn handle_runtime_update(&mut self, update: RuntimeUpdate) -> Task<Message> {
        for issue in update.issues {
            warn!("xbar runtime issue: {issue:?}");
        }

        let mut tasks = Vec::new();
        for effect in update.platform_effects {
            match effect {
                BarEffect::ApplyMonitorGeometry(geometry) => {
                    if let Some(window_id) = self.current_window_id {
                        tasks.push(self.apply_monitor_geometry(window_id, geometry));
                    }
                }
                BarEffect::ClearMonitorGeometry => {
                    if let Some(window_id) = self.current_window_id {
                        tasks.push(window::resize(window_id, self.initial_window_size));
                        if let Some(position) = self.initial_window_position {
                            tasks.push(window::move_to(window_id, position));
                        }
                    }
                }
                effect @ (BarEffect::Screenshot | BarEffect::OpenAudioControl) => {
                    if let Err(error) = self.process_actions.handle(effect) {
                        warn!("failed to handle platform effect: {error}");
                    }
                }
                unhandled => warn!("unhandled xbar platform effect: {unhandled:?}"),
            }
        }
        Task::batch(tasks)
    }

    fn apply_monitor_geometry(
        &self,
        window_id: Id,
        geometry: xbar_core::MonitorGeometry,
    ) -> Task<Message> {
        let scale_factor = self.scale_factor.max(f32::EPSILON);
        Task::batch([
            window::move_to(
                window_id,
                iced::Point::new(
                    geometry.x as f32 / scale_factor,
                    geometry.y as f32 / scale_factor,
                ),
            ),
            window::resize(
                window_id,
                Size::new(geometry.width as f32 / scale_factor, 40.0),
            ),
        ])
    }

    fn subscription(&self) -> Subscription<Message> {
        if self.current_window_id.is_none() {
            Subscription::run(Self::prepare_worker)
        } else {
            let window_events = window::events().map(|(id, event)| Message::WindowEvent(id, event));
            // Keep this wake active even while initial size/position/scale
            // queries are in flight. Dock geometry and preview retries must
            // never silently fall back to the one-second provider tick.
            let wait = self
                .dock
                .next_wake_delay(Instant::now(), TRANSPORT_POLL_INTERVAL);
            let shared = time::every(wait).map(|_| Message::TransportPoll);
            Subscription::batch(vec![shared, window_events])
        }
    }

    fn style(&self, theme: &Theme) -> theme::Style {
        theme::Style {
            background_color: self.background,
            text_color: theme.palette().background.base.text,
        }
    }

    fn monitor_num_to_icon(monitor_num: i32) -> String {
        match monitor_num {
            0 => ICON_M0.to_string(),
            1 => ICON_M1.to_string(),
            n => format!("M{}", n),
        }
    }

    fn volume_icon(volume: i32, muted: bool, has_device: bool) -> &'static str {
        if !has_device || muted || volume <= 0 {
            ICON_VOL_MUTE
        } else if volume < 34 {
            ICON_VOL_LOW
        } else if volume < 67 {
            ICON_VOL_MID
        } else {
            ICON_VOL_HIGH
        }
    }

    // -------- Workspace pills --------
    fn tag_visuals(&self, index: usize) -> (Color, f32, Color) {
        let tag_color = self
            .tab_colors
            .get(index)
            .copied()
            .unwrap_or(Self::DEFAULT_COLOR);

        let view = self.runtime.view();
        if view.wm_available
            && let Some(status) = view.tags.get(index)
        {
            if status.urgent {
                return (
                    Color::from_rgba(0.86, 0.21, 0.27, 1.0),
                    2.0,
                    Color::from_rgba(0.74, 0.13, 0.19, 1.0),
                );
            } else if status.filled {
                return (tag_color.scale_alpha(1.0), 2.0, tag_color);
            } else if status.selected {
                return (tag_color.scale_alpha(0.7), 1.5, tag_color);
            } else if status.occupied {
                return (tag_color.scale_alpha(0.3), 1.0, tag_color.scale_alpha(0.6));
            }
        }

        // default
        (Color::WHITE.scale_alpha(0.9), 1.0, color!(0xDEE2E6))
    }

    fn workspace_button<'a>(
        &self,
        index: usize,
        label: &'a str,
    ) -> iced::widget::Button<'a, Message> {
        let (bg, border_w, border_c) = self.tag_visuals(index);
        let view = self.runtime.view();
        let is_selected = view.wm_available
            && view
                .tags
                .get(index)
                .is_some_and(|s| s.filled || s.selected || s.urgent);

        let text_color = if is_selected {
            // Yellow tag uses dark text for legibility
            if index == 4 {
                color!(0x333333)
            } else {
                Color::WHITE
            }
        } else {
            color!(0x333333)
        };

        let radius = 6.0;
        button(
            rich_text![span(label.to_string()).color(text_color)]
                .size(18)
                .on_link_click(std::convert::identity),
        )
        .padding([4, 6])
        .width(Self::TAB_WIDTH)
        .height(Self::TAB_HEIGHT)
        .style(move |_theme: &Theme, status: button::Status| {
            let mut background = bg;
            let mut border_width = border_w;

            match status {
                button::Status::Hovered => {
                    border_width = (border_w + 1.0).min(3.0);
                    if background.a > 0.0 {
                        background.a = (background.a + 0.08).min(1.0);
                    } else {
                        background = Color::from_rgba(1.0, 1.0, 1.0, 0.10);
                    }
                }
                button::Status::Pressed => {
                    if background.a > 0.0 {
                        background.a = (background.a + 0.12).min(1.0);
                    } else {
                        background = Color::from_rgba(0.9, 0.9, 0.9, 0.10);
                    }
                }
                _ => {}
            }

            button::Style {
                background: Some(Background::Color(background)),
                text_color,
                border: Border {
                    color: border_c,
                    width: border_width,
                    radius: border::Radius::from(radius),
                },
                ..Default::default()
            }
        })
        .on_press(Message::TabSelected(index))
    }

    // -------- Pills --------

    fn pill_style(bg: Color, border_c: Color, text_color: Color) -> container::Style {
        container::Style {
            background: Some(Background::Color(bg)),
            text_color: Some(text_color),
            border: Border {
                radius: border::radius(12.0),
                width: 1.0,
                color: border_c,
            },
            ..Default::default()
        }
    }

    fn usage_colors(usage_percent: f32) -> (Color, Color) {
        match usage_percent {
            u if u <= 30.0 => (color!(0x1FBF51).scale_alpha(0.9), Color::WHITE),
            u if u <= 60.0 => (color!(0xF4C20D).scale_alpha(0.9), color!(0x000000)),
            u if u <= 80.0 => (color!(0xFF8C1A).scale_alpha(0.9), Color::WHITE),
            _ => (color!(0xE53935).scale_alpha(0.9), Color::WHITE),
        }
    }

    fn battery_colors(percent: f32) -> (Color, Color) {
        if percent > 50.0 {
            (color!(0x1FBF51).scale_alpha(0.9), Color::WHITE)
        } else if percent > 20.0 {
            (color!(0xF4C20D).scale_alpha(0.9), color!(0x000000))
        } else {
            (color!(0xE53935).scale_alpha(0.9), Color::WHITE)
        }
    }

    fn usage_pill<'a>(&self, icon: &'a str, value: f32) -> Element<'a, Message> {
        let (bg, fg) = Self::usage_colors(value);
        container(text(format!("{}  {:.0}%", icon, value)).size(14).color(fg))
            .padding([3, 10])
            .height(Self::PILL_HEIGHT)
            .style(move |_theme: &Theme| Self::pill_style(bg, bg, fg))
            .into()
    }

    fn battery_pill<'a>(&self) -> Element<'a, Message> {
        let battery = self.runtime.view().battery;
        let pct = battery.percent.map_or(100.0, |value| value.as_f32());
        let charging = battery.charging;
        let icon = if charging {
            ICON_BAT_CHG
        } else {
            ICON_BAT_FULL
        };
        let (bg, fg) = Self::battery_colors(pct);
        container(text(format!("{}  {:.0}%", icon, pct)).size(14).color(fg))
            .padding([3, 10])
            .height(Self::PILL_HEIGHT)
            .style(move |_theme: &Theme| Self::pill_style(bg, bg, fg))
            .into()
    }

    fn brightness_pill<'a>(&self) -> Element<'a, Message> {
        let label = match self.runtime.view().brightness.percent {
            Some(percent) => format!("{}  {}%", ICON_BRIGHT, percent.rounded()),
            None => format!("{}  --", ICON_BRIGHT),
        };
        let bg = color!(0xFDE047).scale_alpha(0.92);
        let fg = color!(0x1F2937);
        let pill = container(text(label).size(14).color(fg))
            .padding([3, 10])
            .height(Self::PILL_HEIGHT)
            .style(move |_theme: &Theme| Self::pill_style(bg, color!(0xFACC15), fg));

        mouse_area(pill)
            .on_press(Message::BrightnessAdjust(5))
            .on_right_press(Message::BrightnessAdjust(-5))
            .on_scroll(|delta| {
                let d = match delta {
                    mouse::ScrollDelta::Lines { y, .. } => y,
                    mouse::ScrollDelta::Pixels { y, .. } => y,
                };
                if d > 0.0 {
                    Message::BrightnessAdjust(5)
                } else {
                    Message::BrightnessAdjust(-5)
                }
            })
            .into()
    }

    fn volume_pill<'a>(&self) -> Element<'a, Message> {
        let audio = self.runtime.view().audio;
        let (volume, has_device) = audio
            .volume_percent
            .map_or((0, false), |percent| (i32::from(percent.rounded()), true));
        let muted = audio.muted;

        let icon = Self::volume_icon(volume, muted, has_device);
        let label = if has_device {
            format!("{}  {}%", icon, volume)
        } else {
            format!("{}  --", icon)
        };

        let (bg, border_c, fg) = if muted || !has_device {
            (
                color!(0x787878).scale_alpha(0.85),
                color!(0x888888),
                color!(0xEEEEEE),
            )
        } else {
            (
                color!(0x14B8A6).scale_alpha(0.9),
                color!(0x14B8A6),
                Color::WHITE,
            )
        };

        let pill = container(text(label).size(14).color(fg))
            .padding([3, 10])
            .height(Self::PILL_HEIGHT)
            .style(move |_theme: &Theme| Self::pill_style(bg, border_c, fg));

        mouse_area(pill)
            .on_press(Message::AudioToggleMute)
            .on_scroll(|delta| {
                let d = match delta {
                    mouse::ScrollDelta::Lines { y, .. } => y,
                    mouse::ScrollDelta::Pixels { y, .. } => y,
                };
                if d > 0.0 {
                    Message::AudioAdjust(5)
                } else {
                    Message::AudioAdjust(-5)
                }
            })
            .into()
    }

    /// Entry point into JWM's own shell surface.
    ///
    /// One pill: it opens the hub, and the hub is itself the page that routes
    /// to applications, notifications, clipboard, calendar and wallpaper. The
    /// bar renders none of those — it only names the page it wants.
    fn shell_pill<'a>(&self) -> Element<'a, Message> {
        let available = self.runtime.view().wm_available;
        let pill = container(text(ICON_SHELL.to_string()).size(15).color(Color::WHITE))
            .padding([3, 10])
            .height(Self::PILL_HEIGHT)
            .style(move |_theme: &Theme| {
                // Grayed out rather than hidden: the shell lives in the window
                // manager, so an unreachable one has to look unreachable.
                let bg = if available {
                    color!(0x7C6CFF).scale_alpha(0.90)
                } else {
                    color!(0x555B66).scale_alpha(0.70)
                };
                Self::pill_style(bg, bg, Color::WHITE)
            });

        let area = mouse_area(pill);
        if available {
            area.on_press(Message::OpenShell).into()
        } else {
            area.into()
        }
    }

    fn screenshot_pill<'a>(&self) -> Element<'a, Message> {
        let is_hovered = self.is_hovered;
        let pill = container(text(ICON_SHOT.to_string()).size(15).color(Color::WHITE))
            .padding([3, 10])
            .height(Self::PILL_HEIGHT)
            .style(move |_theme: &Theme| {
                let bg = if is_hovered {
                    color!(0xFF8800).scale_alpha(0.95)
                } else {
                    color!(0x00CCCC).scale_alpha(0.90)
                };
                Self::pill_style(bg, bg, Color::WHITE)
            });

        mouse_area(pill)
            .on_enter(Message::MouseEnterScreenShot)
            .on_exit(Message::MouseExitScreenShot)
            .on_press(Message::LeftClick)
            .into()
    }

    fn time_pill<'a>(&self) -> Element<'a, Message> {
        let bg = color!(0x4DA3FF).scale_alpha(0.9);
        let pill = container(
            text(format!("{}  {}", ICON_TIME, self.runtime.view().time))
                .size(14)
                .color(Color::WHITE),
        )
        .padding([3, 10])
        .height(Self::PILL_HEIGHT)
        .style(move |_theme: &Theme| Self::pill_style(bg, bg, Color::WHITE));

        mouse_area(pill).on_press(Message::ShowSecondsToggle).into()
    }

    fn monitor_pill<'a>(&self, monitor_num: i32) -> Element<'a, Message> {
        let bg = color!(0x9B59B6).scale_alpha(0.9);
        container(
            text(format!(
                "{}  {}",
                ICON_MON,
                Self::monitor_num_to_icon(monitor_num)
            ))
            .size(14)
            .color(Color::WHITE),
        )
        .padding([3, 10])
        .height(Self::PILL_HEIGHT)
        .style(move |_theme: &Theme| Self::pill_style(bg, bg, Color::WHITE))
        .into()
    }

    fn scale_pill<'a>(&self, scale: Option<f32>) -> Element<'a, Message> {
        let bg = color!(0x787878).scale_alpha(0.88);
        let label = match scale {
            Some(s) => format!("s: {:.2}", s),
            None => "s: --".to_string(),
        };
        container(text(label).size(14).color(Color::WHITE))
            .padding([3, 10])
            .height(Self::PILL_HEIGHT)
            .style(move |_theme: &Theme| Self::pill_style(bg, bg, Color::WHITE))
            .into()
    }

    fn layout_toggle_button<'a>(&self) -> iced::widget::Button<'a, Message> {
        let view = self.runtime.view();
        let is_open = view.layout_selector_open;
        let color_open = color!(0x3CB371);
        let color_close = color!(0xD35400);

        let pill_color = if is_open { color_open } else { color_close };
        let label = view.layout_symbol.to_owned();

        button(rich_text![span(label).color(Color::WHITE)].on_link_click(std::convert::identity))
            .padding([3, 10])
            .height(Self::PILL_HEIGHT)
            .style(move |_theme: &Theme, status: button::Status| {
                let mut bg = pill_color.scale_alpha(0.85);
                let mut border_w = 1.0;

                if matches!(status, button::Status::Hovered) {
                    bg.a = 1.0;
                    border_w = 2.0;
                }

                button::Style {
                    background: Some(Background::Color(bg)),
                    text_color: Color::WHITE,
                    border: Border {
                        color: pill_color,
                        width: border_w,
                        radius: border::Radius::from(12.0),
                    },
                    ..Default::default()
                }
            })
            .on_press(Message::ToggleLayoutSelector)
    }

    fn layout_options_row(&self) -> Element<'_, Message> {
        let layouts: [(&str, u32); 3] = [("[]=", 0), ("><>", 1), ("[M]", 2)];
        let current = self.runtime.view().layout_symbol;

        let mut row = Row::new().spacing(6);
        for (sym, idx) in layouts {
            let is_current = sym == current;

            let btn = button(text(sym).color(Color::WHITE))
                .padding([3, 10])
                .height(Self::PILL_HEIGHT)
                .style(move |_theme: &Theme, status: button::Status| {
                    let base = if is_current {
                        color!(0x3CB371)
                    } else {
                        color!(0x4169E1)
                    };

                    let mut bg = base.scale_alpha(0.85);
                    let mut border_w = if is_current { 2.0 } else { 1.0 };

                    if matches!(status, button::Status::Hovered) {
                        bg.a = 1.0;
                        border_w += 1.0;
                    }

                    button::Style {
                        background: Some(Background::Color(bg)),
                        text_color: Color::WHITE,
                        border: Border {
                            color: base,
                            width: border_w,
                            radius: border::Radius::from(12.0),
                        },
                        ..Default::default()
                    }
                })
                .on_press(Message::LayoutClicked(idx));

            row = row.push(btn);
        }

        row.into()
    }

    fn minimized_dock(&self) -> Element<'_, Message> {
        let mut items = Row::new().spacing(4).align_y(iced::Alignment::Center);
        for minimized in self.dock.visible_windows() {
            let token = minimized.token;
            let Some(binding) = self.dock.item_binding(token) else {
                continue;
            };
            let magnification = self.dock.scale_for(token);
            let urgent = minimized.urgent();
            let title = if minimized.title.trim().is_empty() {
                minimized.app_id.clone()
            } else {
                minimized.title.clone()
            };
            let fill = if urgent {
                Color::from_rgba(0.85, 0.20, 0.30, 0.96)
            } else {
                Color::from_rgba(0.28, 0.47, 0.79, 0.96)
            };
            let card = container(
                text(minimized.initial().to_string())
                    .size(11)
                    .color(Color::WHITE),
            )
            .width(DOCK_ITEM_WIDTH * magnification)
            .height(DOCK_ITEM_HEIGHT * magnification)
            .align_x(iced::Alignment::Center)
            .align_y(iced::Alignment::Center)
            .style(move |_theme: &Theme| container::Style {
                background: Some(Background::Color(fill)),
                border: Border {
                    color: if urgent {
                        Color::from_rgb(1.0, 0.82, 0.84)
                    } else {
                        Color::from_rgba(1.0, 1.0, 1.0, 0.42)
                    },
                    width: 1.0,
                    radius: border::radius(5.0),
                },
                ..Default::default()
            });
            let titled = tooltip(
                card,
                container(text(title).size(12)).padding([4, 8]),
                tooltip::Position::Top,
            );
            let interactive = mouse_area(titled)
                .on_enter(Message::DockHover {
                    binding,
                    hovered: true,
                })
                .on_exit(Message::DockHover {
                    binding,
                    hovered: false,
                })
                .on_press(Message::DockRestore { binding });
            items = items.push(
                container(interactive)
                    .width(DOCK_SLOT_WIDTH)
                    .height(32)
                    .align_x(iced::Alignment::Center)
                    .align_y(iced::Alignment::Center),
            );
        }
        if self.dock.overflow() || self.dock.collapsed() {
            items = items.push(
                container(text("+").size(12).color(Color::WHITE.scale_alpha(0.72)))
                    .width(12)
                    .align_x(iced::Alignment::Center),
            );
        }
        container(items)
            .width(self.dock.shelf_width())
            .height(32)
            .padding(Padding {
                top: 0.0,
                right: 2.0,
                bottom: 0.0,
                left: 5.0,
            })
            .style(|_theme: &Theme| container::Style {
                border: Border {
                    color: Color::from_rgba(1.0, 1.0, 1.0, 0.20),
                    width: 0.0,
                    radius: border::radius(0.0),
                },
                ..Default::default()
            })
            .into()
    }

    fn view_work_space(&self) -> Element<'_, Message> {
        // Workspace tag buttons
        let mut tags_row = Row::new().spacing(Self::TAB_SPACING * 0.5);
        for (index, label) in self.tabs.iter().enumerate() {
            tags_row = tags_row
                .push(self.workspace_button(index, label))
                .align_y(iced::Alignment::Center);
        }

        let layout_button = self.layout_toggle_button();
        let layout_selector = if self.runtime.view().layout_selector_open {
            self.layout_options_row()
        } else {
            Row::new().into()
        };

        // System info pills
        let system = self.runtime.view().system;
        let cpu_usage = system.cpu_percent.map_or(0.0, |value| value.as_f32());
        let memory_usage = system.memory_percent.map_or(0.0, |value| value.as_f32());

        let cpu_pill = self.usage_pill(ICON_CPU, cpu_usage);
        let memory_pill = self.usage_pill(ICON_MEM, memory_usage);
        let battery_pill = self.battery_pill();

        let brightness_pill = self.brightness_pill();
        let volume_pill = self.volume_pill();
        let shell_pill = self.shell_pill();
        let screenshot_pill = self.screenshot_pill();
        let time_pill = self.time_pill();

        let monitor_num = self.runtime.view().monitor.0;
        let monitor_pill = self.monitor_pill(monitor_num);

        let scale_pill = self.scale_pill(Some(self.scale_factor));
        let minimized_dock = self.minimized_dock();

        Row::new()
            .push(tags_row)
            .push(Space::new().width(6).height(Length::Fill))
            .push(layout_button)
            .push(Space::new().width(6).height(Length::Fill))
            .push(layout_selector)
            .push(Space::new().width(Length::Fill).height(Length::Fill))
            .push(cpu_pill)
            .push(Space::new().width(6).height(Length::Fill))
            .push(memory_pill)
            .push(Space::new().width(6).height(Length::Fill))
            .push(battery_pill)
            .push(Space::new().width(8).height(Length::Fill))
            .push(brightness_pill)
            .push(Space::new().width(6).height(Length::Fill))
            .push(volume_pill)
            .push(Space::new().width(6).height(Length::Fill))
            .push(shell_pill)
            .push(Space::new().width(6).height(Length::Fill))
            .push(screenshot_pill)
            .push(Space::new().width(6).height(Length::Fill))
            .push(time_pill)
            .push(Space::new().width(6).height(Length::Fill))
            .push(monitor_pill)
            .push(Space::new().width(6).height(Length::Fill))
            .push(scale_pill)
            .push(Space::new().width(4).height(Length::Fill))
            .push(minimized_dock)
            .align_y(iced::Alignment::Center)
            .into()
    }

    fn view(&self) -> Element<'_, Message> {
        let work_space_row = self.view_work_space();

        Column::new()
            .padding(4)
            .spacing(Self::TAB_SPACING)
            .push(work_space_row)
            .into()
    }
}
