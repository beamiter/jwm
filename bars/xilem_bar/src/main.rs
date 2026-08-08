// xilem_bar — xilem port of iced_bar.
//
// Feature parity goals with iced_bar:
//   * 9 nerd-font workspace tag buttons with selected/filled/urgent/occupied visuals
//   * Layout toggle button + 3-option selector
//   * Pills: CPU, memory, battery, brightness, volume, screenshot, time, monitor, scale
//   * Click semantics: tag → view-tag command; volume scroll/click/right-click;
//     brightness scroll/click/right-click; screenshot pill asks jwm for its own region capture over IPC
//   * Nonblocking transport polling with reconnect; 1Hz core runtime tick
//
// xilem is closure-based (no Message enum), so each interactive view directly
// mutates state. Xilem tasks schedule provider and transport polls on the UI
// runtime; transport access itself remains nonblocking.

use std::collections::HashMap;
use std::env;
use std::ffi::c_void;
use std::fs;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use log::{debug, info, warn};

use raw_window_handle::{
    DisplayHandle, HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
    WindowHandle, XcbDisplayHandle, XcbWindowHandle,
};
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{
    ColormapAlloc, ConnectionExt as _, CreateWindowAux, VisualClass, WindowClass,
};
use x11rb::xcb_ffi::XCBConnection;

use xbar_core::glass::{DEFAULT_BACKGROUND_OPACITY, fallback_rgb};
use xbar_core::logging::init as initialize_logging;
use xbar_core::{
    BarEffect, BarRuntime, DOCK_ITEM_GAP, DOCK_ITEM_HEIGHT, DOCK_ITEM_WIDTH, DOCK_SLOT_WIDTH,
    DockBridge, DockItemBinding, LayoutId, ModelConfig, MonitorGeometry, PlatformEffectHandler,
    RuntimeSchedule, RuntimeUpdate, ShellRoute, TagId, ThemeMode, TransportRecoveryConfig,
    UserAction, dock_wake_delay,
};
use xbar_linux_actions::ProcessActionHandler;

use masonry::accesskit::{Node, Role};
use masonry::core::{
    AccessCtx, ChildrenIds, ErasedAction, LayerType, LayoutCtx, MeasureCtx, NewWidget, PaintCtx,
    PropertiesMut, PropertiesRef, PropertySet, RegisterCtx, Update, UpdateCtx, Widget, WidgetId,
    WidgetMut, WidgetPod,
};
use masonry::imaging::Painter;
use masonry::kurbo::{Axis, Point, Size};
use masonry::layers::Tooltip;
use masonry::layout::{Dim, LenReq, Length};
use masonry::peniko::Color;
use masonry::properties::{
    Background, BorderColor, BorderWidth, ContentColor, CornerRadius, Dimensions, Padding,
};
use masonry::widgets::Label as MasonryLabel;
use masonry_winit::app::{AppDriver, DriverCtx, MasonryState, WgpuContext, WindowId};
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::window::WindowLevel;
use xilem::core::{
    MessageCtx, MessageProxy, MessageResult, Mut, NoElement, View, ViewId, ViewMarker,
    ViewPathTracker, fork,
};
use xilem::style::Style;
use xilem::view::{
    CrossAxisAlignment, FlexSpacer, PointerButton, button, button_any_pointer, flex, label,
    sized_box, task_raw,
};
use xilem::{EventLoop, ViewCtx, WidgetView, WindowOptions, Xilem};

// -------- Constants (mirror iced_bar) ----------------------------------------

const NERD_FONT: &str = "JetBrainsMono Nerd Font";

const TAG_ICONS: [&str; 9] = [
    "\u{F0A1E}",
    "\u{F0239}",
    "\u{F0A1B}",
    "\u{F0B79}",
    "\u{F024B}",
    "\u{F0388}",
    "\u{F0567}",
    "\u{F01F0}",
    "\u{F0297}",
];

const ICON_CPU: &str = "\u{F0FB1}";
const ICON_MEM: &str = "\u{F035B}";
const ICON_BAT_FULL: &str = "\u{F0079}";
const ICON_BAT_CHG: &str = "\u{F0084}";
const ICON_VOL_HIGH: &str = "\u{F057E}";
const ICON_VOL_MID: &str = "\u{F0580}";
const ICON_VOL_LOW: &str = "\u{F057F}";
const ICON_VOL_MUTE: &str = "\u{F075F}";
const ICON_BRIGHT: &str = "\u{F00DE}";
const ICON_SHOT: &str = "\u{F0104}";
const ICON_TIME: &str = "\u{F0954}";
const ICON_MON: &str = "\u{F0379}";
const ICON_M0: &str = "\u{F02DA}";
const ICON_M1: &str = "\u{F02DB}";
const ICON_SHELL: &str = "\u{F0F2A}";
const ICON_SUN: &str = "\u{F0599}";
const ICON_MOON: &str = "\u{F0594}";

const DEFAULT_TAB_WIDTH: f64 = 42.0;
const MIN_TAB_WIDTH: f64 = 40.0;
const MAX_TAB_WIDTH: f64 = 56.0;
const TAB_SPACING: f64 = 4.0;
const TAB_FONT_SIZE: f32 = 12.0;
const PILL_FONT_SIZE: f32 = 11.0;
const LEFT_SECTION_GAP: f64 = 1.0;
const CENTER_SECTION_GAP: f64 = 2.0;
const RIGHT_SECTION_GAP: f64 = 4.0;
const RIGHTMOST_SECTION_GAP: f64 = 8.0;
const TRANSPORT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const TRANSPORT_RETRY_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Clone, Copy)]
struct GeometryBridgeState {
    generation: u64,
    geometry: Option<MonitorGeometry>,
    scale_factor: f64,
}

impl Default for GeometryBridgeState {
    fn default() -> Self {
        Self {
            generation: 0,
            geometry: None,
            scale_factor: 1.0,
        }
    }
}

#[derive(Default)]
struct GeometryBridge {
    state: Mutex<GeometryBridgeState>,
}

impl GeometryBridge {
    fn update_geometry(&self, geometry: Option<MonitorGeometry>) {
        let mut state = self.state.lock().unwrap_or_else(|error| {
            warn!("xilem geometry bridge mutex was poisoned");
            error.into_inner()
        });
        state.generation = state.generation.wrapping_add(1).max(1);
        state.geometry = geometry;
    }

    fn update_scale_factor(&self, scale_factor: f64) {
        let mut state = self.state.lock().unwrap_or_else(|error| {
            warn!("xilem geometry bridge mutex was poisoned");
            error.into_inner()
        });
        state.scale_factor = scale_factor.max(f64::EPSILON);
    }

    fn snapshot(&self) -> GeometryBridgeState {
        *self.state.lock().unwrap_or_else(|error| {
            warn!("xilem geometry bridge mutex was poisoned");
            error.into_inner()
        })
    }
}

#[derive(Clone, Copy)]
struct WindowBaseline {
    position: Option<PhysicalPosition<i32>>,
    logical_size: LogicalSize<f64>,
}

struct AppliedWindowState {
    baseline: WindowBaseline,
    generation: u64,
    scale_factor: f64,
}

struct GeometryDriver<D> {
    inner: D,
    bridge: Arc<GeometryBridge>,
    windows: HashMap<WindowId, AppliedWindowState>,
}

impl<D> GeometryDriver<D> {
    fn new(inner: D, bridge: Arc<GeometryBridge>) -> Self {
        Self {
            inner,
            bridge,
            windows: HashMap::new(),
        }
    }

    fn sync_window(&mut self, window_id: WindowId, ctx: &mut DriverCtx<'_, '_>) {
        let bridge = self.bridge.snapshot();
        let window = ctx.window(window_id).handle();
        let scale_factor = window.scale_factor().max(f64::EPSILON);
        self.bridge.update_scale_factor(scale_factor);

        let applied = self.windows.entry(window_id).or_insert_with(|| {
            let physical_size = window.inner_size();
            AppliedWindowState {
                baseline: WindowBaseline {
                    position: window.outer_position().ok(),
                    logical_size: physical_size.to_logical(scale_factor),
                },
                generation: 0,
                scale_factor,
            }
        });
        let scale_changed = applied.scale_factor.to_bits() != scale_factor.to_bits();
        if bridge.generation == 0 || (bridge.generation == applied.generation && !scale_changed) {
            applied.scale_factor = scale_factor;
            return;
        }

        let logical_height = applied.baseline.logical_size.height;
        let physical_height = (logical_height * scale_factor)
            .round()
            .clamp(1.0, f64::from(u32::MAX)) as u32;
        match bridge.geometry {
            Some(geometry) => {
                window.set_outer_position(PhysicalPosition::new(geometry.x, geometry.y));
                let _ = window
                    .request_inner_size(PhysicalSize::new(geometry.width.max(1), physical_height));
            }
            None => {
                if let Some(position) = applied.baseline.position {
                    window.set_outer_position(position);
                }
                let physical_width = (applied.baseline.logical_size.width * scale_factor)
                    .round()
                    .clamp(1.0, f64::from(u32::MAX)) as u32;
                let _ =
                    window.request_inner_size(PhysicalSize::new(physical_width, physical_height));
            }
        }
        applied.generation = bridge.generation;
        applied.scale_factor = scale_factor;
    }
}

impl<D: AppDriver> AppDriver for GeometryDriver<D> {
    fn on_action(
        &mut self,
        window_id: WindowId,
        ctx: &mut DriverCtx<'_, '_>,
        widget_id: WidgetId,
        action: ErasedAction,
    ) {
        self.inner.on_action(window_id, ctx, widget_id, action);
        self.sync_window(window_id, ctx);
    }

    fn on_async_action(
        &mut self,
        window_id: WindowId,
        ctx: &mut DriverCtx<'_, '_>,
        action: ErasedAction,
    ) {
        self.inner.on_async_action(window_id, ctx, action);
        self.sync_window(window_id, ctx);
    }

    fn on_start(&mut self, state: &mut MasonryState<'_>) {
        self.inner.on_start(state);
    }

    fn on_close_requested(&mut self, window_id: WindowId, ctx: &mut DriverCtx<'_, '_>) {
        self.inner.on_close_requested(window_id, ctx);
    }

    fn on_wgpu_ready(&mut self, wgpu: &WgpuContext<'_>) {
        self.inner.on_wgpu_ready(wgpu);
    }
}

fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::from_rgb8(r, g, b)
}
fn pill_padding() -> Padding {
    Padding::from_vh(Length::px(0.0), Length::px(2.0))
}

// -------- Compositor coupling ------------------------------------------------
//
// Whether the bar gets a translucent or a solid window is decided once, in
// `main`, because window transparency is a creation-time choice in winit. The
// decision does not follow a compositor that starts or stops afterwards — a
// restarted bar picks up the change — and owning `_NET_WM_CM_S{n}` only says
// compositing is on, not that anything blurs behind the bar.

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
/// window masonry knows nothing about.
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
/// alpha. Masonry picks its surface alpha mode internally and never reports
/// the outcome, so the bar asks the same question ahead of time: build a
/// throwaway depth-32 window, get the surface capabilities, and accept only
/// what masonry itself would blit transparently — `PostMultiplied` or
/// `PreMultiplied`. Anything less and a transparent window would compose
/// garbage, so the bar must stay solid.
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
    // Env-aware, like the instance masonry itself will build: a `WGPU_BACKEND`
    // override must steer the probe onto the same backend it steers vello onto.
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::from_env_or_default());
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
            let modes = surface.get_capabilities(&adapter).alpha_modes;
            Some(
                modes.contains(&wgpu::CompositeAlphaMode::PostMultiplied)
                    || modes.contains(&wgpu::CompositeAlphaMode::PreMultiplied),
            )
        })
        .unwrap_or(false);

    let _ = conn.destroy_window(window);
    let _ = conn.free_colormap(colormap);
    let _ = conn.flush();
    capable
}

// -------- App state ----------------------------------------------------------

#[derive(Debug, Default)]
struct WorkerSignal {
    pending: AtomicBool,
    dock_deadline: Mutex<Option<Instant>>,
    wake: tokio::sync::Notify,
}

impl WorkerSignal {
    fn publish_dock_deadline(&self, deadline: Option<Instant>) {
        let mut current = match self.dock_deadline.lock() {
            Ok(current) => current,
            Err(error) => error.into_inner(),
        };
        if *current != deadline {
            *current = deadline;
            self.wake.notify_one();
        }
    }

    fn next_wait(&self, now: Instant) -> Duration {
        let deadline = match self.dock_deadline.lock() {
            Ok(current) => *current,
            Err(error) => *error.into_inner(),
        };
        dock_wake_delay(now, TRANSPORT_POLL_INTERVAL, deadline)
    }

    fn drive_finished(&self) {
        self.pending.store(false, Ordering::Release);
        self.wake.notify_one();
    }
}

#[derive(Debug, Clone)]
enum WorkerEvent {
    Drive(Arc<WorkerSignal>),
}

struct XilemBar {
    tab_colors: [Color; 9],
    runtime: BarRuntime,
    schedule: RuntimeSchedule,
    process_actions: ProcessActionHandler,
    geometry_bridge: Arc<GeometryBridge>,
    dock: DockBridge,
    worker_signal: Arc<WorkerSignal>,
    theme: Theme,

    /// The startup decision from `main`: a translucent window washes its
    /// background to `background_opacity`, a solid one paints it opaque.
    translucent: bool,
    /// Alpha of the bar's background while translucent; ignored when solid.
    background_opacity: f64,
}

impl XilemBar {
    fn new(geometry_bridge: Arc<GeometryBridge>, translucent: bool) -> Self {
        let args: Vec<String> = env::args().collect();
        let shared_path = args.iter().skip(1).last().cloned().unwrap_or_default();

        let theme_mode = load_theme_mode();
        let config = ModelConfig {
            brightness_step: 10,
            initial_theme: theme_mode,
            show_seconds: true,
            ..ModelConfig::default()
        };
        let runtime = if shared_path.is_empty() {
            BarRuntime::new(config)
        } else {
            let recovery = TransportRecoveryConfig::new(shared_path, TRANSPORT_RETRY_INTERVAL)
                .expect("static transport recovery config is valid");
            BarRuntime::with_managed_transport(config, recovery)
        };
        let runtime = runtime.expect("xilem bar model configuration is valid");
        // Only `background_opacity` applies here; the rest of this bar's
        // appearance is its own theme.
        let app_config = xbar_core::config::BarConfig::load_default().unwrap_or_else(|error| {
            warn!("falling back to the default bar config: {error}");
            xbar_core::config::BarConfig::default()
        });
        let background_opacity = app_config
            .background_opacity
            .unwrap_or(DEFAULT_BACKGROUND_OPACITY);
        Self {
            tab_colors: [
                rgb(0xFF, 0x6B, 0x6B),
                rgb(0x4E, 0xCD, 0xC4),
                rgb(0x45, 0xB7, 0xD1),
                rgb(0x96, 0xCE, 0xB4),
                rgb(0xFE, 0xCA, 0x57),
                rgb(0xFF, 0x9F, 0xF3),
                rgb(0x54, 0xA0, 0xFF),
                rgb(0x5F, 0x27, 0xCD),
                rgb(0x00, 0xD2, 0xD3),
            ],
            runtime,
            schedule: RuntimeSchedule::default(),
            process_actions: ProcessActionHandler::default(),
            geometry_bridge,
            dock: DockBridge::new(),
            worker_signal: Arc::new(WorkerSignal::default()),
            theme: Theme::from_mode(theme_mode),
            translucent,
            background_opacity,
        }
    }

    fn toggle_theme(&mut self) {
        let update = self.runtime.dispatch(UserAction::ToggleTheme);
        self.handle_runtime_update(update);
        save_theme_mode(self.runtime.view().theme);
    }

    fn dispatch(&mut self, action: UserAction) {
        let update = self.runtime.dispatch(action);
        self.handle_runtime_update(update);
    }

    fn dispatch_wm(&mut self, action: UserAction) {
        if !self.runtime.view().wm_available {
            debug!("ignoring WM action while the WM projection is unavailable");
            return;
        }
        self.dispatch(action);
    }

    fn service_dock(&mut self) {
        let scale_factor = self.geometry_bridge.snapshot().scale_factor;
        let snapshot = self.runtime.snapshot();
        self.dock.synchronize(
            &snapshot,
            self.runtime.transport_generation(),
            scale_factor,
            40.0,
            6.0,
        );
        let now = Instant::now();
        for action in self.dock.pending_actions(now) {
            let update = self.runtime.dispatch(action);
            let accepted = !update.has_issues();
            self.handle_runtime_update(update);
            if accepted {
                self.dock.acknowledge(action, now);
            } else {
                self.dock.record_failure(now);
                break;
            }
        }
        self.worker_signal
            .publish_dock_deadline(self.dock.next_retry_deadline(Instant::now()));
    }

    fn restore_minimized(&mut self, binding: DockItemBinding) {
        let _ = self.dock.request_restore(binding);
        self.service_dock();
    }

    fn hover_minimized(&mut self, binding: DockItemBinding, hovered: bool) {
        let changed = if hovered {
            self.dock.enter(binding)
        } else {
            self.dock.leave(binding)
        };
        if changed {
            // Rebuild immediately so the hovered card and its two neighbours
            // magnify in the same frame, and send/withdraw the preview lease.
            self.service_dock();
        }
    }

    fn handle_runtime_update(&mut self, update: RuntimeUpdate) {
        for issue in update.issues {
            warn!("xbar runtime issue: {issue:?}");
        }
        for effect in update.platform_effects {
            match effect {
                effect @ (BarEffect::Screenshot | BarEffect::OpenAudioControl) => {
                    if let Err(error) = self.process_actions.handle(effect) {
                        warn!("failed to handle platform effect: {error}");
                    }
                }
                BarEffect::ApplyMonitorGeometry(geometry) => {
                    self.geometry_bridge.update_geometry(Some(geometry));
                }
                BarEffect::ClearMonitorGeometry => self.geometry_bridge.update_geometry(None),
                unhandled => warn!("unhandled xbar platform effect: {unhandled:?}"),
            }
        }
        self.theme = Theme::from_mode(self.runtime.view().theme);
    }

    fn on_worker(&mut self, ev: WorkerEvent) {
        match ev {
            WorkerEvent::Drive(signal) => {
                let update = self.schedule.service(&mut self.runtime);
                self.handle_runtime_update(update);
                self.service_dock();
                signal.drive_finished();
            }
        }
    }

    // -------- View helpers -----------------------------------------------------

    fn tag_visuals(&self, index: usize) -> (Color, f64, Color) {
        let tag_color = *self.tab_colors.get(index).unwrap_or(&rgb(0x66, 0x66, 0x66));
        let view = self.runtime.view();
        if view.wm_available
            && let Some(s) = view.tags.get(index)
        {
            if s.urgent {
                return (self.theme.urgent_bg, 2.0, self.theme.urgent_border);
            }
            if s.filled {
                return (tag_color, 2.0, tag_color);
            }
            if s.selected {
                return (with_alpha(tag_color, 0.7), 1.5, tag_color);
            }
            if s.occupied {
                return (
                    with_alpha(tag_color, 0.22),
                    1.5,
                    with_alpha(tag_color, 0.95),
                );
            }
        }
        (self.theme.tag_inactive_bg, 1.0, with_alpha(tag_color, 0.9))
    }

    fn is_tag_active(&self, index: usize) -> bool {
        let view = self.runtime.view();
        view.wm_available
            && view
                .tags
                .get(index)
                .is_some_and(|s| s.filled || s.selected || s.urgent || s.occupied)
    }

    fn workspace_tab_width(&self) -> f64 {
        let Some(monitor_width) = self
            .runtime
            .view()
            .geometry
            .map(|geometry| {
                f64::from(geometry.width) / self.geometry_bridge.snapshot().scale_factor
            })
            .filter(|w| *w > 0.0)
        else {
            return DEFAULT_TAB_WIDTH;
        };

        // Keep the left workspace group proportional to the monitor width,
        // while clamping individual tab width to avoid extreme sizes.
        let group_width = (monitor_width * 0.20).clamp(360.0, 500.0);
        let total_spacing = TAB_SPACING * (TAG_ICONS.len().saturating_sub(1) as f64);
        ((group_width - total_spacing) / TAG_ICONS.len() as f64).clamp(MIN_TAB_WIDTH, MAX_TAB_WIDTH)
    }
}

fn with_alpha(mut c: Color, a: f32) -> Color {
    c.components[3] = a;
    c
}

// -------- Reusable view builders --------------------------------------------

fn flat<V>(inner: V) -> impl WidgetView<XilemBar>
where
    V: WidgetView<XilemBar> + 'static,
{
    sized_box(inner)
        .padding(pill_padding())
        .height(Dim::Stretch)
}

// Catppuccin Mocha (dark) / Latte (light) palettes, swapped at runtime.
#[derive(Copy, Clone)]
struct Theme {
    fg: Color,
    subtle: Color,
    tag_inactive_bg: Color,
    tag_inactive_text: Color,
    tag_active_text: Color,
    urgent_bg: Color,
    urgent_border: Color,
    green: Color,
    yellow: Color,
    orange: Color,
    red: Color,
    teal: Color,
    blue: Color,
    mauve: Color,
    pink: Color,
}

impl Theme {
    const fn mocha() -> Self {
        Self {
            fg: Color::from_rgba8(0xCD, 0xD6, 0xF4, 255),
            subtle: Color::from_rgba8(0x9C, 0xA0, 0xB0, 255),
            tag_inactive_bg: Color::from_rgba8(0x45, 0x47, 0x5A, 217),
            tag_inactive_text: Color::from_rgba8(0xCD, 0xD6, 0xF4, 255),
            tag_active_text: Color::WHITE,
            urgent_bg: Color::from_rgba8(0xDB, 0x36, 0x45, 255),
            urgent_border: Color::from_rgba8(0xBC, 0x21, 0x30, 255),
            green: Color::from_rgba8(0xA6, 0xE3, 0xA1, 255),
            yellow: Color::from_rgba8(0xF9, 0xE2, 0xAF, 255),
            orange: Color::from_rgba8(0xFA, 0xB3, 0x87, 255),
            red: Color::from_rgba8(0xF3, 0x8B, 0xA8, 255),
            teal: Color::from_rgba8(0x94, 0xE2, 0xD5, 255),
            blue: Color::from_rgba8(0x89, 0xB4, 0xFA, 255),
            mauve: Color::from_rgba8(0xCB, 0xA6, 0xF7, 255),
            pink: Color::from_rgba8(0xF5, 0xC2, 0xE7, 255),
        }
    }
    const fn latte() -> Self {
        Self {
            fg: Color::from_rgba8(0x4C, 0x4F, 0x69, 255),
            subtle: Color::from_rgba8(0x6C, 0x6F, 0x85, 255),
            tag_inactive_bg: Color::from_rgba8(0xCC, 0xD0, 0xDA, 217),
            tag_inactive_text: Color::from_rgba8(0x4C, 0x4F, 0x69, 255),
            tag_active_text: Color::WHITE,
            urgent_bg: Color::from_rgba8(0xD2, 0x0F, 0x39, 255),
            urgent_border: Color::from_rgba8(0xA8, 0x0B, 0x2E, 255),
            green: Color::from_rgba8(0x40, 0xA0, 0x2B, 255),
            yellow: Color::from_rgba8(0xDF, 0x8E, 0x1D, 255),
            orange: Color::from_rgba8(0xFE, 0x64, 0x0B, 255),
            red: Color::from_rgba8(0xD2, 0x0F, 0x39, 255),
            teal: Color::from_rgba8(0x17, 0x92, 0x99, 255),
            blue: Color::from_rgba8(0x1E, 0x66, 0xF5, 255),
            mauve: Color::from_rgba8(0x88, 0x39, 0xEF, 255),
            pink: Color::from_rgba8(0xEA, 0x76, 0xCB, 255),
        }
    }
    const fn from_mode(mode: ThemeMode) -> Self {
        match mode {
            ThemeMode::Dark => Self::mocha(),
            ThemeMode::Light => Self::latte(),
        }
    }
}

fn theme_config_path() -> Option<PathBuf> {
    let home = env::var_os("HOME")?;
    let mut p = PathBuf::from(home);
    p.push(".config/xilem_bar/theme");
    Some(p)
}

fn load_theme_mode() -> ThemeMode {
    let Some(p) = theme_config_path() else {
        return ThemeMode::Dark;
    };
    match fs::read_to_string(&p).ok().as_deref().map(str::trim) {
        Some("light") => ThemeMode::Light,
        _ => ThemeMode::Dark,
    }
}

fn save_theme_mode(mode: ThemeMode) {
    let Some(p) = theme_config_path() else {
        return;
    };
    if let Some(dir) = p.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let s = match mode {
        ThemeMode::Dark => "dark",
        ThemeMode::Light => "light",
    };
    if let Err(e) = fs::write(&p, s) {
        warn!("failed to persist theme mode to {}: {e}", p.display());
    }
}

fn usage_color(theme: &Theme, usage: f32) -> Color {
    if usage <= 30.0 {
        theme.green
    } else if usage <= 60.0 {
        theme.yellow
    } else if usage <= 80.0 {
        theme.orange
    } else {
        theme.red
    }
}

fn battery_color(theme: &Theme, pct: f32) -> Color {
    if pct > 50.0 {
        theme.green
    } else if pct > 20.0 {
        theme.yellow
    } else {
        theme.red
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

fn monitor_num_to_icon(n: i32) -> String {
    match n {
        0 => ICON_M0.to_string(),
        1 => ICON_M1.to_string(),
        n => format!("M{}", n),
    }
}

// -------- Sub-views ----------------------------------------------------------

fn workspace_tag(state: &mut XilemBar, index: usize) -> impl WidgetView<XilemBar> + use<> {
    let label_str = TAG_ICONS[index].to_string();
    let (bg, border_w, border_c) = state.tag_visuals(index);
    let is_active = state.is_tag_active(index);
    let tab_width = state.workspace_tab_width();
    let text_color = if is_active {
        state.theme.tag_active_text
    } else {
        state.theme.tag_inactive_text
    };

    let inner = label(label_str).text_size(TAB_FONT_SIZE).color(text_color);
    sized_box(button(inner, move |s: &mut XilemBar| {
        if let Some(tag) = TagId::new(index) {
            s.dispatch_wm(UserAction::ViewTag(tag));
        }
    }))
    .dims(Dimensions::new(
        Dim::Fixed(Length::px(tab_width)),
        Dim::Stretch,
    ))
    .background(bg)
    .border(border_c, Length::px(border_w))
    .corner_radius(Length::px(3.0))
}

fn workspace_row(state: &mut XilemBar) -> impl WidgetView<XilemBar> + use<> {
    flex(
        Axis::Horizontal,
        (
            workspace_tag(state, 0),
            workspace_tag(state, 1),
            workspace_tag(state, 2),
            workspace_tag(state, 3),
            workspace_tag(state, 4),
            workspace_tag(state, 5),
            workspace_tag(state, 6),
            workspace_tag(state, 7),
            workspace_tag(state, 8),
        ),
    )
    .cross_axis_alignment(CrossAxisAlignment::Stretch)
    .gap(Length::px(TAB_SPACING))
}

fn layout_toggle(state: &mut XilemBar) -> impl WidgetView<XilemBar> + use<> {
    let open = state.runtime.view().layout_selector_open;
    let fg = if open {
        state.theme.green
    } else {
        state.theme.orange
    };
    let label_str = state.runtime.view().layout_symbol.to_owned();

    flat(button(
        label(label_str).text_size(PILL_FONT_SIZE).color(fg),
        |s: &mut XilemBar| {
            s.dispatch(UserAction::ToggleLayoutSelector);
        },
    ))
}

fn layout_options(state: &mut XilemBar) -> impl WidgetView<XilemBar> + use<> {
    let current = state.runtime.view().layout_symbol.to_owned();
    let theme = state.theme;
    let mk = move |sym: &'static str, idx: u32, current: String| {
        let is_current = sym == current;
        let fg = if is_current { theme.green } else { theme.blue };
        flat(button(
            label(sym).text_size(PILL_FONT_SIZE).color(fg),
            move |s: &mut XilemBar| {
                s.dispatch_wm(UserAction::SetLayout(LayoutId(idx)));
            },
        ))
    };

    flex(
        Axis::Horizontal,
        (
            mk("[]=", 0, current.clone()),
            mk("><>", 1, current.clone()),
            mk("[M]", 2, current),
        ),
    )
    .cross_axis_alignment(CrossAxisAlignment::Stretch)
    .gap(Length::px(4.0))
}

fn usage_pill_view(
    theme: &Theme,
    icon: &'static str,
    value: f32,
) -> impl WidgetView<XilemBar> + use<> {
    let accent = usage_color(theme, value);
    let fg = theme.fg;
    flat(
        flex(
            Axis::Horizontal,
            (
                label(icon.to_string())
                    .text_size(PILL_FONT_SIZE)
                    .color(accent),
                label(format!("{:.0}%", value))
                    .text_size(PILL_FONT_SIZE)
                    .color(fg),
            ),
        )
        .gap(Length::px(3.0)),
    )
}

fn battery_pill_view(state: &XilemBar) -> impl WidgetView<XilemBar> + use<> {
    let battery = state.runtime.view().battery;
    let pct = battery.percent.map_or(100.0, |percent| percent.as_f32());
    let charging = battery.charging;
    let icon = if charging {
        ICON_BAT_CHG
    } else {
        ICON_BAT_FULL
    };
    let accent = battery_color(&state.theme, pct);
    let fg = state.theme.fg;
    flat(
        flex(
            Axis::Horizontal,
            (
                label(icon.to_string())
                    .text_size(PILL_FONT_SIZE)
                    .color(accent),
                label(format!("{:.0}%", pct))
                    .text_size(PILL_FONT_SIZE)
                    .color(fg),
            ),
        )
        .gap(Length::px(3.0)),
    )
}

fn brightness_pill_view(state: &XilemBar) -> impl WidgetView<XilemBar> + use<> {
    let pct = state.runtime.view().brightness.percent;
    let pct_str = match pct {
        Some(percent) => format!("{}%", percent.rounded()),
        None => "--".to_string(),
    };
    let accent = state.theme.yellow;
    let fg = state.theme.fg;
    // Left-click brightens (+10), right-click dims (-10).
    let inner = flex(
        Axis::Horizontal,
        (
            label(ICON_BRIGHT.to_string())
                .text_size(PILL_FONT_SIZE)
                .color(accent),
            label(pct_str).text_size(PILL_FONT_SIZE).color(fg),
        ),
    )
    .gap(Length::px(3.0));

    flat(button_any_pointer(
        inner,
        |s: &mut XilemBar, btn: Option<PointerButton>| {
            let action = if matches!(btn, Some(PointerButton::Secondary)) {
                UserAction::BrightnessDown
            } else {
                UserAction::BrightnessUp
            };
            s.dispatch(action);
        },
    ))
}

fn volume_pill_view(state: &XilemBar) -> impl WidgetView<XilemBar> + use<> {
    let audio = state.runtime.view().audio;
    let (vol, has_dev) = audio
        .volume_percent
        .map_or((0, false), |percent| (i32::from(percent.rounded()), true));
    let muted = audio.muted;
    let icon = volume_icon(vol, muted, has_dev);
    let pct_str = if has_dev {
        format!("{}%", vol)
    } else {
        "--".to_string()
    };
    let accent = if muted || !has_dev {
        state.theme.subtle
    } else {
        state.theme.teal
    };
    let fg = state.theme.fg;
    let inner = flex(
        Axis::Horizontal,
        (
            label(icon.to_string())
                .text_size(PILL_FONT_SIZE)
                .color(accent),
            label(pct_str).text_size(PILL_FONT_SIZE).color(fg),
        ),
    )
    .gap(Length::px(3.0));
    // Left-click +5, right-click −5, middle-click toggles mute.
    flat(button_any_pointer(
        inner,
        |s: &mut XilemBar, btn: Option<PointerButton>| {
            let action = match btn {
                Some(PointerButton::Secondary) => UserAction::VolumeDown,
                Some(PointerButton::Auxiliary) => UserAction::ToggleMute,
                _ => UserAction::VolumeUp,
            };
            s.dispatch(action);
        },
    ))
}

/// Entry point into JWM's own shell surface.
///
/// One pill is enough: it opens the hub, and the hub itself is the page that
/// routes to applications, notifications, clipboard, calendar and wallpaper.
/// The bar renders none of that — it only names the page.
fn shell_pill_view(state: &XilemBar) -> impl WidgetView<XilemBar> + use<> {
    let accent = if state.runtime.view().wm_available {
        state.theme.teal
    } else {
        state.theme.subtle
    };
    let inner = label(ICON_SHELL.to_string())
        .text_size(PILL_FONT_SIZE)
        .color(accent);
    flat(button(inner, |s: &mut XilemBar| {
        // dispatch_wm, not dispatch: the shell lives in the window manager, so
        // a click with no WM projection has nowhere to go.
        s.dispatch_wm(UserAction::OpenShellHub(ShellRoute::Hub));
    }))
}

fn screenshot_pill_view(state: &XilemBar) -> impl WidgetView<XilemBar> + use<> {
    let inner = label(ICON_SHOT.to_string())
        .text_size(PILL_FONT_SIZE)
        .color(state.theme.pink);
    flat(button(inner, |s: &mut XilemBar| {
        s.dispatch(UserAction::Screenshot);
    }))
}

fn theme_pill_view(state: &XilemBar) -> impl WidgetView<XilemBar> + use<> {
    // Show the icon for the mode you'll switch TO: sun if currently dark, moon if light.
    let (icon, accent) = match state.runtime.view().theme {
        ThemeMode::Dark => (ICON_SUN, state.theme.yellow),
        ThemeMode::Light => (ICON_MOON, state.theme.mauve),
    };
    flat(button(
        label(icon.to_string())
            .text_size(PILL_FONT_SIZE)
            .color(accent),
        |s: &mut XilemBar| s.toggle_theme(),
    ))
}

fn time_pill_view(state: &XilemBar) -> impl WidgetView<XilemBar> + use<> {
    let accent = state.theme.blue;
    let fg = state.theme.fg;
    let inner = flex(
        Axis::Horizontal,
        (
            label(ICON_TIME.to_string())
                .text_size(PILL_FONT_SIZE)
                .color(accent),
            label(state.runtime.view().time.to_owned())
                .text_size(PILL_FONT_SIZE)
                .color(fg),
        ),
    )
    .gap(Length::px(3.0));
    flat(button(inner, |s: &mut XilemBar| {
        s.dispatch(UserAction::ToggleSeconds);
    }))
}

fn monitor_pill_view(theme: &Theme, monitor_num: i32) -> impl WidgetView<XilemBar> + use<> {
    let accent = theme.mauve;
    let fg = theme.fg;
    flat(
        flex(
            Axis::Horizontal,
            (
                label(ICON_MON.to_string())
                    .text_size(PILL_FONT_SIZE)
                    .color(accent),
                label(monitor_num_to_icon(monitor_num))
                    .text_size(PILL_FONT_SIZE)
                    .color(fg),
            ),
        )
        .gap(Length::px(3.0)),
    )
}

fn scale_pill_view(theme: &Theme, scale: f32) -> impl WidgetView<XilemBar> + use<> {
    flat(flex(
        Axis::Horizontal,
        label(format!("s: {:.2}", scale))
            .text_size(PILL_FONT_SIZE)
            .color(theme.subtle),
    ))
}

// Xilem's public button view intentionally exposes only its press callback.
// This transparent Masonry container retains that button as its child and
// turns the container's HoveredChanged update into a normal Xilem message.
// Keeping the press action source in the child means keyboard/accessibility
// activation and the existing restore path remain exactly as before.
struct HoverSensorWidget {
    child: WidgetPod<dyn Widget>,
    title: String,
}

impl HoverSensorWidget {
    fn new(child: NewWidget<impl Widget + ?Sized>, title: String) -> Self {
        Self {
            child: child.erased().to_pod(),
            title,
        }
    }

    fn child_mut<'a>(this: &'a mut WidgetMut<'_, Self>) -> WidgetMut<'a, dyn Widget> {
        this.ctx.get_mut(&mut this.widget.child)
    }

    fn set_title(this: &mut WidgetMut<'_, Self>, title: String) {
        this.widget.title = title;
    }
}

#[derive(Debug)]
struct HoverChanged(bool);

impl Widget for HoverSensorWidget {
    type Action = HoverChanged;

    fn register_children(&mut self, ctx: &mut RegisterCtx<'_>) {
        ctx.register_child(&mut self.child);
    }

    fn update(&mut self, ctx: &mut UpdateCtx<'_>, _props: &mut PropertiesMut<'_>, event: &Update) {
        if let Update::HoveredChanged(hovered) = event {
            if *hovered {
                // The bar has no spare vertical area, so present the complete
                // title in a compact overlay immediately to the card's left.
                let text = NewWidget::new(MasonryLabel::new(self.title.clone())).with_props(
                    PropertySet::one(ContentColor::new(Color::from_rgb8(0xf5, 0xf5, 0xf7))),
                );
                let tooltip = NewWidget::new(Tooltip::new(text)).with_props(PropertySet::from((
                    Padding::from_vh(Length::px(2.0), Length::px(6.0)),
                    Background::Color(Color::from_rgba8(0x24, 0x24, 0x2a, 0xf2)),
                    BorderColor::new(Color::from_rgba8(0xff, 0xff, 0xff, 0x38)),
                    BorderWidth::all(Length::px(1.0)),
                    CornerRadius::all(Length::px(5.0)),
                )));
                let card = ctx.bounding_box();
                let estimated_width = self.title.chars().count() as f64 * 7.0 + 14.0;
                let position = Point::new((card.x0 - estimated_width - 6.0).max(0.0), 1.0);
                ctx.create_attached_layer(
                    LayerType::Tooltip(self.title.clone()),
                    tooltip,
                    position,
                );
            }
            ctx.submit_action::<Self::Action>(HoverChanged(*hovered));
        }
    }

    fn measure(
        &mut self,
        ctx: &mut MeasureCtx<'_>,
        _props: &PropertiesRef<'_>,
        axis: Axis,
        _len_req: LenReq,
        cross_length: Option<Length>,
    ) -> Length {
        ctx.redirect_measurement(&mut self.child, axis, cross_length)
    }

    fn layout(&mut self, ctx: &mut LayoutCtx<'_>, _props: &PropertiesRef<'_>, size: Size) {
        ctx.run_layout(&mut self.child, size);
        ctx.place_child(&mut self.child, Point::ORIGIN);
        ctx.derive_baselines(&self.child);
    }

    fn paint(
        &mut self,
        _ctx: &mut PaintCtx<'_>,
        _props: &PropertiesRef<'_>,
        _painter: &mut Painter<'_>,
    ) {
    }

    fn accessibility_role(&self) -> Role {
        Role::GenericContainer
    }

    fn accessibility(
        &mut self,
        _ctx: &mut AccessCtx<'_>,
        _props: &PropertiesRef<'_>,
        _node: &mut Node,
    ) {
    }

    fn children_ids(&self) -> ChildrenIds {
        ChildrenIds::from_slice(&[self.child.id()])
    }
}

fn hover_sensor<State, Action, V, F>(
    inner: V,
    title: String,
    on_hover: F,
) -> HoverSensor<V, F, State, Action>
where
    State: 'static,
    Action: 'static,
    V: WidgetView<State, Action>,
    F: Fn(&mut State, bool) -> Action + Send + Sync + 'static,
{
    HoverSensor {
        inner,
        title,
        on_hover,
        phantom: std::marker::PhantomData,
    }
}

struct HoverSensor<V, F, State, Action> {
    inner: V,
    title: String,
    on_hover: F,
    phantom: std::marker::PhantomData<fn() -> (State, Action)>,
}

const HOVER_SENSOR_CONTENT_VIEW_ID: ViewId = ViewId::new(0x791f9be3);

impl<V, F, State, Action> ViewMarker for HoverSensor<V, F, State, Action> {}

impl<V, F, State, Action> View<State, Action, ViewCtx> for HoverSensor<V, F, State, Action>
where
    State: 'static,
    Action: 'static,
    V: WidgetView<State, Action>,
    F: Fn(&mut State, bool) -> Action + Send + Sync + 'static,
{
    type Element = xilem::Pod<HoverSensorWidget>;
    type ViewState = V::ViewState;

    fn build(&self, ctx: &mut ViewCtx, app_state: &mut State) -> (Self::Element, Self::ViewState) {
        let (child, child_state) = ctx.with_id(HOVER_SENSOR_CONTENT_VIEW_ID, |ctx| {
            self.inner.build(ctx, app_state)
        });
        (
            ctx.with_action_widget(|ctx| {
                ctx.create_pod(HoverSensorWidget::new(child.new_widget, self.title.clone()))
            }),
            child_state,
        )
    }

    fn rebuild(
        &self,
        prev: &Self,
        view_state: &mut Self::ViewState,
        ctx: &mut ViewCtx,
        mut element: Mut<'_, Self::Element>,
        app_state: &mut State,
    ) {
        if self.title != prev.title {
            HoverSensorWidget::set_title(&mut element, self.title.clone());
        }
        ctx.with_id(HOVER_SENSOR_CONTENT_VIEW_ID, |ctx| {
            View::<State, Action, _>::rebuild(
                &self.inner,
                &prev.inner,
                view_state,
                ctx,
                HoverSensorWidget::child_mut(&mut element).downcast(),
                app_state,
            );
        });
    }

    fn teardown(
        &self,
        view_state: &mut Self::ViewState,
        ctx: &mut ViewCtx,
        mut element: Mut<'_, Self::Element>,
    ) {
        ctx.with_id(HOVER_SENSOR_CONTENT_VIEW_ID, |ctx| {
            View::<State, Action, _>::teardown(
                &self.inner,
                view_state,
                ctx,
                HoverSensorWidget::child_mut(&mut element).downcast(),
            );
        });
        ctx.teardown_action_source(element);
    }

    fn message(
        &self,
        view_state: &mut Self::ViewState,
        message: &mut MessageCtx,
        mut element: Mut<'_, Self::Element>,
        app_state: &mut State,
    ) -> MessageResult<Action> {
        match message.take_first() {
            Some(HOVER_SENSOR_CONTENT_VIEW_ID) => self.inner.message(
                view_state,
                message,
                HoverSensorWidget::child_mut(&mut element).downcast(),
                app_state,
            ),
            None => match message.take_message::<HoverChanged>() {
                Some(hovered) => MessageResult::Action((self.on_hover)(app_state, hovered.0)),
                None => MessageResult::Stale,
            },
            _ => MessageResult::Stale,
        }
    }
}

fn minimized_item(state: &XilemBar, index: usize) -> Option<impl WidgetView<XilemBar> + use<>> {
    let minimized = state.dock.visible_windows().nth(index)?.clone();
    let token = minimized.token;
    let binding = state.dock.item_binding(token)?;
    let scale = state.dock.scale_for(token);
    let background = if minimized.urgent() {
        state.theme.urgent_bg
    } else {
        with_alpha(state.theme.blue, 0.92)
    };
    let border = if minimized.urgent() {
        state.theme.urgent_border
    } else {
        with_alpha(state.theme.fg, 0.55)
    };
    let title = if minimized.title.trim().is_empty() {
        minimized.app_id.clone()
    } else {
        minimized.title.clone()
    };
    let tooltip_title = title.clone();
    let card = sized_box(hover_sensor(
        button(
            label(minimized.initial().to_string())
                .text_size(PILL_FONT_SIZE)
                .color(Color::WHITE),
            move |state: &mut XilemBar| {
                info!("restore minimized window {title}");
                state.restore_minimized(binding);
            },
        ),
        tooltip_title,
        move |state: &mut XilemBar, hovered| {
            state.hover_minimized(binding, hovered);
        },
    ))
    .dims(Dimensions::new(
        Dim::Fixed(Length::px(f64::from(DOCK_ITEM_WIDTH * scale))),
        Dim::Fixed(Length::px(f64::from(DOCK_ITEM_HEIGHT * scale))),
    ))
    .background(background)
    .border(border, Length::px(1.0))
    .corner_radius(Length::px(5.0));
    Some(
        sized_box(card)
            .dims(Dimensions::new(
                Dim::Fixed(Length::px(f64::from(DOCK_SLOT_WIDTH))),
                Dim::Stretch,
            ))
            .padding(Padding::from_vh(
                Length::px(f64::from((40.0 - DOCK_ITEM_HEIGHT * scale) * 0.5)),
                Length::px(f64::from((DOCK_SLOT_WIDTH - DOCK_ITEM_WIDTH * scale) * 0.5)),
            )),
    )
}

fn minimized_dock(state: &XilemBar) -> impl WidgetView<XilemBar> + use<> {
    let subtle = with_alpha(state.theme.subtle, 0.45);
    let overflow = state.dock.overflow() || state.dock.collapsed();
    let cards: Vec<_> = (0..16)
        .filter_map(|index| minimized_item(state, index))
        .collect();
    let overflow_label = overflow.then(|| {
        sized_box(
            label("+")
                .text_size(PILL_FONT_SIZE)
                .color(state.theme.subtle),
        )
        .width(Dim::Fixed(Length::px(12.0)))
        .height(Dim::Stretch)
    });
    sized_box(
        flex(
            Axis::Horizontal,
            (
                flex(Axis::Horizontal, cards)
                    .cross_axis_alignment(CrossAxisAlignment::Center)
                    .gap(Length::px(f64::from(DOCK_ITEM_GAP))),
                overflow_label,
            ),
        )
        .cross_axis_alignment(CrossAxisAlignment::Center)
        .gap(Length::px(f64::from(DOCK_ITEM_GAP))),
    )
    .width(Dim::Fixed(Length::px(f64::from(state.dock.shelf_width()))))
    .height(Dim::Stretch)
    .padding(Padding::from_vh(Length::ZERO, Length::px(2.0)))
    .border(subtle, Length::px(1.0))
    .corner_radius(Length::px(5.0))
}

// -------- Top-level view -----------------------------------------------------

fn app_logic(state: &mut XilemBar) -> impl WidgetView<XilemBar> + use<> {
    state.service_dock();
    let view = state.runtime.view();
    let cpu = view.system.cpu_percent.map_or(0.0, |value| value.as_f32());
    let mem = view
        .system
        .memory_percent
        .map_or(0.0, |value| value.as_f32());
    let monitor_num = view.monitor.0;

    let theme = state.theme;
    let tags = workspace_row(state);
    let lt_btn = layout_toggle(state);
    let lt_options: Option<_> = if state.runtime.view().layout_selector_open {
        Some(layout_options(state))
    } else {
        None
    };

    flex(
        Axis::Horizontal,
        (
            (
                tags,
                FlexSpacer::Fixed(Length::px(LEFT_SECTION_GAP)),
                lt_btn,
                FlexSpacer::Fixed(Length::px(LEFT_SECTION_GAP)),
                lt_options,
                FlexSpacer::Flex(1.0),
            ),
            (
                usage_pill_view(&theme, ICON_CPU, cpu),
                FlexSpacer::Fixed(Length::px(CENTER_SECTION_GAP)),
                usage_pill_view(&theme, ICON_MEM, mem),
                FlexSpacer::Fixed(Length::px(CENTER_SECTION_GAP)),
                battery_pill_view(state),
                FlexSpacer::Fixed(Length::px(CENTER_SECTION_GAP)),
                brightness_pill_view(state),
                FlexSpacer::Fixed(Length::px(CENTER_SECTION_GAP)),
                volume_pill_view(state),
            ),
            (
                FlexSpacer::Fixed(Length::px(RIGHT_SECTION_GAP)),
                shell_pill_view(state),
                FlexSpacer::Fixed(Length::px(RIGHT_SECTION_GAP)),
                screenshot_pill_view(state),
                FlexSpacer::Fixed(Length::px(RIGHT_SECTION_GAP)),
                theme_pill_view(state),
                FlexSpacer::Fixed(Length::px(RIGHTMOST_SECTION_GAP)),
                time_pill_view(state),
                FlexSpacer::Fixed(Length::px(RIGHTMOST_SECTION_GAP)),
                monitor_pill_view(&theme, monitor_num),
                FlexSpacer::Fixed(Length::px(RIGHTMOST_SECTION_GAP)),
                scale_pill_view(&theme, 1.0),
                FlexSpacer::Fixed(Length::px(4.0)),
                minimized_dock(state),
            ),
        ),
    )
    .cross_axis_alignment(CrossAxisAlignment::Stretch)
    .gap(Length::ZERO)
}

// -------- Background workers -------------------------------------------------

// One coalesced driver task keeps at most one event in the winit queue. The
// ordinary transport fallback remains 100ms, but a moving Dock anchor can
// shorten the next sleep to its 50ms protocol deadline.
fn driver_task() -> impl View<XilemBar, (), ViewCtx, Element = NoElement> + use<> {
    task_raw(
        |proxy: MessageProxy<WorkerEvent>, state: &mut XilemBar| {
            let signal = Arc::clone(&state.worker_signal);
            async move {
                loop {
                    if signal.pending.load(Ordering::Acquire) {
                        signal.wake.notified().await;
                        continue;
                    }
                    let wait = signal.next_wait(Instant::now());
                    tokio::select! {
                        () = tokio::time::sleep(wait) => {}
                        () = signal.wake.notified() => {}
                    }
                    if signal
                        .pending
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                        && proxy
                            .message(WorkerEvent::Drive(Arc::clone(&signal)))
                            .is_err()
                    {
                        signal.pending.store(false, Ordering::Release);
                        break;
                    }
                }
            }
        },
        |state: &mut XilemBar, ev: WorkerEvent| state.on_worker(ev),
    )
}

// Top-level view: fork attaches the background tasks to the visible tree.
fn root(state: &mut XilemBar) -> impl WidgetView<XilemBar> + use<> {
    // The background is `fallback_rgb` for the live theme in both modes —
    // re-derived on every rebuild so a ToggleTheme click repaints it — and
    // only the alpha differs: washed to the configured opacity when the
    // compositor blends what lies behind the window, opaque when it does not.
    let [r, g, b] = fallback_rgb(state.runtime.view().theme);
    let bar_bg = if state.translucent {
        with_alpha(rgb(r, g, b), state.background_opacity as f32)
    } else {
        rgb(r, g, b)
    };
    fork(
        sized_box(app_logic(state))
            .padding(Padding::from_vh(Length::px(0.0), Length::px(6.0)))
            .background(bar_bg),
        (driver_task(),),
    )
}

// -------- main ---------------------------------------------------------------

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    let shared_path = args.iter().skip(1).last().cloned().unwrap_or_default();
    let _ = initialize_logging("xilem_bar", &shared_path);

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

    let opts = WindowOptions::new("xilem_bar")
        .with_initial_inner_size(LogicalSize::new(800.0, 26.0))
        .with_decorations(false)
        .with_transparent(translucent)
        .with_window_level(WindowLevel::AlwaysOnTop)
        .with_resizable(false);

    // The widget layer paints the authoritative background; the base color
    // only covers what nothing else draws, so it is clear on a translucent
    // window and the solid fallback color on an opaque one.
    let base_color = if translucent {
        Color::TRANSPARENT
    } else {
        let [r, g, b] = fallback_rgb(load_theme_mode());
        rgb(r, g, b)
    };

    let _ = NERD_FONT;
    let geometry_bridge = Arc::new(GeometryBridge::default());
    let app = Xilem::new_simple(
        XilemBar::new(Arc::clone(&geometry_bridge), translucent),
        root,
        opts,
    )
    .with_default_base_color(base_color);
    let event_loop = EventLoop::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let (driver, windows) =
        app.into_driver_and_windows(move |event| proxy.send_event(event).map_err(|error| error.0));
    masonry_winit::app::run_with(
        event_loop,
        windows,
        GeometryDriver::new(driver, geometry_bridge),
        masonry::theme::default_property_set(),
    )?;
    Ok(())
}
