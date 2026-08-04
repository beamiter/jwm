// xilem_bar — xilem port of iced_bar.
//
// Feature parity goals with iced_bar:
//   * 9 nerd-font workspace tag buttons with selected/filled/urgent/occupied visuals
//   * Layout toggle button + 3-option selector
//   * Pills: CPU, memory, battery, brightness, volume, screenshot, time, monitor, scale
//   * Click semantics: tag → view-tag command; volume scroll/click/right-click;
//     brightness scroll/click/right-click; screenshot pill spawns `flameshot gui`
//   * Nonblocking transport polling with reconnect; 1Hz core runtime tick
//
// xilem is closure-based (no Message enum), so each interactive view directly
// mutates state. Xilem tasks schedule provider and transport polls on the UI
// runtime; transport access itself remains nonblocking.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use log::{debug, warn};

use xbar_core::logging::init as initialize_logging;
use xbar_core::{
    BarEffect, BarRuntime, LayoutId, ModelConfig, MonitorGeometry, PlatformEffectHandler,
    RuntimeSchedule, RuntimeUpdate, ShellRoute, TagId, ThemeMode, TransportRecoveryConfig,
    UserAction,
};
use xbar_linux_actions::ProcessActionHandler;

use masonry::core::{ErasedAction, WidgetId};
use masonry::kurbo::Axis;
use masonry::layout::{Dim, Length};
use masonry::peniko::Color;
use masonry::properties::{Dimensions, Padding};
use masonry_winit::app::{AppDriver, DriverCtx, MasonryState, WgpuContext, WindowId};
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::window::WindowLevel;
use xilem::core::{MessageProxy, NoElement, View, fork};
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
// -------- App state ----------------------------------------------------------

#[derive(Debug, Default)]
struct WorkerSignal {
    pending: AtomicBool,
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
    theme: Theme,
}

impl XilemBar {
    fn new(geometry_bridge: Arc<GeometryBridge>) -> Self {
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
            theme: Theme::from_mode(theme_mode),
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
                signal.pending.store(false, Ordering::Release);
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
    bar_bg: Color,
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
            bar_bg: Color::from_rgba8(0x1E, 0x20, 0x32, 128),
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
            bar_bg: Color::from_rgba8(0xEF, 0xF1, 0xF5, 160),
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

// -------- Top-level view -----------------------------------------------------

fn app_logic(state: &mut XilemBar) -> impl WidgetView<XilemBar> + use<> {
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
            ),
        ),
    )
    .cross_axis_alignment(CrossAxisAlignment::Stretch)
    .gap(Length::ZERO)
}

// -------- Background workers -------------------------------------------------

// One coalesced driver task keeps at most one event in the winit queue. Polls
// retain 100ms transport latency, while every tenth poll also advances the
// core clock/providers.
fn driver_task() -> impl View<XilemBar, (), ViewCtx, Element = NoElement> + use<> {
    task_raw(
        |proxy: MessageProxy<WorkerEvent>, _state: &mut XilemBar| async move {
            let signal = Arc::new(WorkerSignal::default());
            let mut iv = tokio::time::interval(TRANSPORT_POLL_INTERVAL);
            iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                iv.tick().await;
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
        },
        |state: &mut XilemBar, ev: WorkerEvent| state.on_worker(ev),
    )
}

// Top-level view: fork attaches the background tasks to the visible tree.
fn root(state: &mut XilemBar) -> impl WidgetView<XilemBar> + use<> {
    let bar_bg = state.theme.bar_bg;
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

    let opts = WindowOptions::new("xilem_bar")
        .with_initial_inner_size(LogicalSize::new(800.0, 26.0))
        .with_decorations(false)
        .with_transparent(true)
        .with_window_level(WindowLevel::AlwaysOnTop)
        .with_resizable(false);

    let _ = NERD_FONT;
    let geometry_bridge = Arc::new(GeometryBridge::default());
    let app = Xilem::new_simple(XilemBar::new(Arc::clone(&geometry_bridge)), root, opts)
        .with_default_base_color(Color::TRANSPARENT);
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
