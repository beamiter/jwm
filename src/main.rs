use gdk4::prelude::*;
use gtk4::gio::{self};
use gtk4::prelude::*;
use gtk4::{
    Application, ApplicationWindow, Builder, Button, EventControllerScroll,
    EventControllerScrollFlags, Label, Revealer, glib,
};
use log::{info, warn};
use std::cell::{Cell, RefCell};
use std::env;
use std::rc::Rc;
use std::time::Duration;

use xbar_core::logging::init as initialize_logging;
use xbar_core::{
    BarEffect, BarRuntime, BarSnapshot, LayoutId, ModelConfig, PlatformEffectHandler,
    RuntimeUpdate, TagId, TagState, ThemeMode, TransportRecoveryConfig, UserAction,
};
use xbar_linux_actions::ProcessActionHandler;

use gtk4::glib::ControlFlow;
use gtk4::glib::Propagation;

// ========= 常量 =========
const CPU_REDRAW_THRESHOLD: f64 = 0.01; // 1%
const MEM_REDRAW_THRESHOLD: f64 = 0.005; // 0.5%

// 胶囊颜色阈值（占用比例）
const LEVEL_WARN: f64 = 0.50; // 50%
const LEVEL_HIGH: f64 = 0.75; // 75%
const LEVEL_CRIT: f64 = 0.90; // 90%

// CSS 类 bit 掩码
const CLS_SELECTED: u8 = 1 << 0;
const CLS_OCCUPIED: u8 = 1 << 1;
const CLS_FILLED: u8 = 1 << 2;
const CLS_URGENT: u8 = 1 << 3;
const CLS_EMPTY: u8 = 1 << 4;

// ========= 状态 =========
#[allow(dead_code)]
struct AppState {
    snapshot: BarSnapshot,

    // Last values to control redraw
    last_cpu_usage: f64,
    last_mem_fraction: f64,

    // 上一帧每个 tab 的 class 掩码，用于差量更新
    last_class_masks: Vec<u8>,
}

impl AppState {
    fn new(snapshot: BarSnapshot) -> Self {
        Self {
            snapshot,
            last_cpu_usage: 0.0,
            last_mem_fraction: 0.0,
            last_class_masks: Vec::new(),
        }
    }
}

type SharedAppState = Rc<RefCell<AppState>>;

// ========= Metric 工具 =========
fn usage_to_level_class(ratio: f64) -> &'static str {
    if ratio >= LEVEL_CRIT {
        "level-crit"
    } else if ratio >= LEVEL_HIGH {
        "level-high"
    } else if ratio >= LEVEL_WARN {
        "level-warn"
    } else {
        "level-ok"
    }
}

// 统一更新“胶囊”标签：文本 + 颜色 class
fn set_metric_capsule(label: &Label, title: &str, ratio: f64) {
    let percent = (ratio * 100.0).round().clamp(0.0, 100.0) as i32;
    label.set_text(&format!("{} {}%", title, percent));

    for cls in ["level-ok", "level-warn", "level-high", "level-crit"] {
        label.remove_css_class(cls);
    }
    label.add_css_class(usage_to_level_class(ratio));
}

// ========= 主体应用 =========
struct TabBarApp {
    // GTK widgets
    builder: Builder,
    window: ApplicationWindow,
    tab_buttons: Vec<Button>,
    time_button: Button,
    volume_button: Button,
    theme_button: Button,
    monitor_label: Label,
    memory_label: Label,
    cpu_label: Label,

    // 新增：布局开关 + 展开选项
    layout_toggle: Button,
    layout_revealer: Revealer,
    layout_btn_tiled: Button,
    layout_btn_floating: Button,
    layout_btn_monocle: Button,

    // Shared state
    state: SharedAppState,
    runtime: RefCell<BarRuntime>,
    process_actions: RefCell<ProcessActionHandler>,

    // Cached UI-applied values for diff
    ui_last_monitor_num: Cell<i32>,
}

impl TabBarApp {
    fn new(app: &Application, shared_path: String) -> Rc<Self> {
        // 加载 UI
        let builder = Builder::from_string(include_str!("resources/main_layout.ui"));

        // 主窗口
        let window: ApplicationWindow = builder
            .object("main_window")
            .expect("Failed to get main_window from builder");
        window.set_application(Some(app));

        // 确保窗口无装饰并支持透明度
        window.set_decorated(false);

        // 连接 realize 信号以在窗口创建时设置透明度
        window.connect_realize(|win| {
            if let Some(surface) = win.surface() {
                surface.set_opaque_region(None);
            }

            // 添加 app-paintable CSS 类以支持自定义绘制
            win.add_css_class("transparent-window");

            // 强制重绘以确保样式应用
            win.queue_draw();
        });

        // 标签按钮
        let mut tab_buttons = Vec::new();
        for i in 0..9 {
            let button_id = format!("tab_button_{}", i);
            let button: Button = builder
                .object(&button_id)
                .unwrap_or_else(|| panic!("Failed to get {button_id} from builder"));
            tab_buttons.push(button);
        }

        // 其他组件
        let time_button: Button = builder
            .object("time_label")
            .expect("Failed to get time_label from builder");
        let volume_button: Button = builder
            .object("volume_button")
            .expect("Failed to get volume_button from builder");
        let theme_button: Button = builder
            .object("theme_button")
            .expect("Failed to get theme_button from builder");
        let monitor_label: Label = builder
            .object("monitor_label")
            .expect("Failed to get monitor_label from builder");
        let memory_label: Label = builder
            .object("memory_label")
            .expect("Failed to get memory_label from builder");
        let cpu_label: Label = builder
            .object("cpu_label")
            .expect("Failed to get cpu_label from builder");

        // 布局开关 + 选项
        let layout_toggle: Button = builder
            .object("layout_toggle")
            .expect("Failed to get layout_toggle");
        let layout_revealer: Revealer = builder
            .object("layout_revealer")
            .expect("Failed to get layout_revealer");
        let layout_btn_tiled: Button = builder
            .object("layout_option_tiled")
            .expect("Failed to get layout_option_tiled");
        let layout_btn_floating: Button = builder
            .object("layout_option_floating")
            .expect("Failed to get layout_option_floating");
        let layout_btn_monocle: Button = builder
            .object("layout_option_monocle")
            .expect("Failed to get layout_option_monocle");

        let mut runtime = if shared_path.is_empty() {
            BarRuntime::new(ModelConfig::default())
        } else {
            let recovery =
                TransportRecoveryConfig::new(shared_path.clone(), Duration::from_secs(2))
                    .expect("static transport recovery config is valid");
            BarRuntime::with_managed_transport(ModelConfig::default(), recovery)
        }
        .expect("default xbar_core model config is valid");
        let mut initial_update = runtime.tick();
        initial_update.merge(runtime.poll_transport());

        // UI state is an owned projection; providers and transport stay in runtime.
        let state: SharedAppState = Rc::new(RefCell::new(AppState::new(runtime.snapshot())));

        // 样式
        Self::apply_styles();

        let app_instance = Rc::new(Self {
            builder,
            window,
            tab_buttons,
            time_button,
            volume_button,
            theme_button,
            monitor_label,
            memory_label,
            cpu_label,
            layout_toggle,
            layout_revealer,
            layout_btn_tiled,
            layout_btn_floating,
            layout_btn_monocle,
            state,
            runtime: RefCell::new(runtime),
            process_actions: RefCell::new(ProcessActionHandler::default()),
            ui_last_monitor_num: Cell::new(i32::MIN),
        });

        // Default theme: dark
        app_instance.window.add_css_class("theme-dark");
        app_instance.window.remove_css_class("theme-light");

        // 为 CPU/内存标签添加基础胶囊样式
        app_instance.cpu_label.add_css_class("metric-label");
        app_instance.memory_label.add_css_class("metric-label");

        // 事件绑定
        Self::setup_event_handlers(app_instance.clone());

        // Provider and clock refresh stays in the core runtime.
        {
            let app_clone = app_instance.clone();
            glib::timeout_add_seconds_local(1, move || {
                let update = app_clone.runtime.borrow_mut().tick();
                app_clone.handle_runtime_update(update);
                ControlFlow::Continue
            });
        }

        // Shared transport polling is non-blocking and owned by the UI runtime.
        {
            let app_clone = app_instance.clone();
            glib::timeout_add_local(Duration::from_millis(50), move || {
                let update = app_clone.runtime.borrow_mut().poll_transport();
                app_clone.handle_runtime_update(update);
                ControlFlow::Continue
            });
        }

        app_instance.handle_runtime_update(initial_update);

        // 首次时间显示
        app_instance.update_time_display();
        // 首次布局 UI 同步（默认 closed）
        app_instance.update_layout_ui();
        // 首次音量/主题 UI 同步
        app_instance.update_volume_display();
        app_instance.update_theme_display();

        // 首次 tab 样式同步（设置初始 empty 状态）
        app_instance.update_tab_styles();

        app_instance
    }

    fn apply_styles() {
        let provider = gtk4::CssProvider::new();
        provider.load_from_string(include_str!("styles.css"));
        if let Some(display) = gtk4::gdk::Display::default() {
            gtk4::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
    }

    fn setup_event_handlers(app: Rc<Self>) {
        // 标签按钮点击
        for (i, button) in app.tab_buttons.iter().enumerate() {
            button.connect_clicked({
                let app = app.clone();
                move |_| {
                    Self::handle_tab_selected(app.clone(), i);
                }
            });
        }

        // 布局开关
        app.layout_toggle.connect_clicked({
            let app = app.clone();
            move |_| {
                app.dispatch(UserAction::ToggleLayoutSelector);
            }
        });

        // 布局选项
        app.layout_btn_tiled.connect_clicked({
            let app = app.clone();
            move |_| {
                Self::handle_layout_clicked(app.clone(), 0);
            }
        });
        app.layout_btn_floating.connect_clicked({
            let app = app.clone();
            move |_| {
                Self::handle_layout_clicked(app.clone(), 1);
            }
        });
        app.layout_btn_monocle.connect_clicked({
            let app = app.clone();
            move |_| {
                Self::handle_layout_clicked(app.clone(), 2);
            }
        });

        // 时间按钮
        app.time_button.connect_clicked({
            let app = app.clone();
            move |_| {
                Self::handle_toggle_seconds(app.clone());
            }
        });

        // 截图按钮
        if let Some(screenshot_button) = app.builder.object::<Button>("screenshot_button") {
            screenshot_button.connect_clicked({
                let app = app.clone();
                move |_| {
                    Self::handle_screenshot(app.clone());
                }
            });
        }

        // 主题切换按钮
        app.theme_button.connect_clicked({
            let app = app.clone();
            move |_| {
                Self::handle_toggle_theme(app.clone());
            }
        });

        // 音量按钮：点击静音/取消静音；滚轮调节音量
        app.volume_button.connect_clicked({
            let app = app.clone();
            move |_| {
                Self::handle_toggle_mute(app.clone());
            }
        });
        {
            let app = app.clone();
            let controller = EventControllerScroll::new(EventControllerScrollFlags::VERTICAL);
            let app_for_scroll = app.clone();
            controller.connect_scroll(move |_, _dx, dy| {
                if dy == 0.0 {
                    return Propagation::Proceed;
                }

                let step = if dy < 0.0 { 3 } else { -3 };
                Self::handle_adjust_volume(app_for_scroll.clone(), step);
                Propagation::Stop
            });
            app.volume_button.add_controller(controller);
        }
    }

    // ========= 交互 =========
    fn handle_tab_selected(app: Rc<Self>, index: usize) {
        info!("Tab selected: {}", index);
        let Some(tag) = TagId::new(index) else {
            warn!("Ignoring out-of-range tag index {index}");
            return;
        };
        let state = app.state.borrow();
        if !state.snapshot.wm_available {
            warn!("Ignoring tag input until the first WM snapshot arrives");
            return;
        }
        let monitor = state.snapshot.monitor;
        drop(state);
        app.dispatch(UserAction::ViewTagOn { tag, monitor });
    }

    fn dispatch(&self, action: UserAction) {
        let update = self.runtime.borrow_mut().dispatch(action);
        self.handle_runtime_update(update);
    }

    fn handle_runtime_update(&self, update: RuntimeUpdate) {
        for issue in &update.issues {
            warn!("xbar runtime issue: {issue:?}");
        }
        let needs_redraw = update.needs_redraw();
        for effect in update.platform_effects {
            self.handle_platform_effect(effect);
        }

        if needs_redraw {
            let snapshot = self.runtime.borrow().snapshot();
            self.state.borrow_mut().snapshot = snapshot;
            self.update_all_views();
        }
    }

    fn handle_platform_effect(&self, effect: BarEffect) {
        match effect {
            BarEffect::ApplyMonitorGeometry(geometry) => self.resize_window_to_monitor(
                geometry.x,
                geometry.y,
                geometry.width,
                geometry.height,
            ),
            BarEffect::ClearMonitorGeometry => self.window.set_default_size(1000, 40),
            effect @ (BarEffect::Screenshot | BarEffect::OpenAudioControl) => {
                if let Err(error) = self.process_actions.borrow_mut().handle(effect) {
                    warn!("Failed to handle platform effect: {error}");
                }
            }
            BarEffect::WindowManager(command) => {
                warn!("No WM transport available for command: {command:?}");
            }
            BarEffect::ToggleMute
            | BarEffect::AdjustVolume(_)
            | BarEffect::AdjustBrightness(_)
            | BarEffect::RefreshBattery => {
                warn!("No enabled runtime adapter handled effect: {effect:?}");
            }
        }
    }

    fn update_all_views(&self) {
        self.update_ui();
        self.update_layout_ui();
        self.update_time_display();
        self.update_volume_display();
        self.update_theme_display();
        self.update_metrics();
    }

    fn handle_layout_clicked(app: Rc<Self>, layout_index: u32) {
        let state = app.state.borrow();
        if !state.snapshot.wm_available {
            warn!("Ignoring layout input until the first WM snapshot arrives");
            return;
        }
        let monitor = state.snapshot.monitor;
        drop(state);
        app.dispatch(UserAction::SetLayoutOn {
            layout: LayoutId(layout_index),
            monitor,
        });
    }

    fn handle_toggle_seconds(app: Rc<Self>) {
        app.dispatch(UserAction::ToggleSeconds);
    }

    fn handle_screenshot(app: Rc<Self>) {
        info!("Taking screenshot");
        app.dispatch(UserAction::Screenshot);
    }

    fn handle_toggle_theme(app: Rc<Self>) {
        app.dispatch(UserAction::ToggleTheme);
    }

    fn handle_toggle_mute(app: Rc<Self>) {
        app.dispatch(UserAction::ToggleMute);
    }

    fn handle_adjust_volume(app: Rc<Self>, step: i32) {
        app.dispatch(UserAction::AdjustVolume(step));
    }

    // ========= UI 更新 =========
    fn update_ui(&self) {
        if let Ok(st) = self.state.try_borrow() {
            // monitor_label 差量
            let monitor_num = if st.snapshot.wm_available {
                st.snapshot.monitor.0
            } else {
                i32::MIN
            };
            if self.ui_last_monitor_num.get() != monitor_num {
                let monitor_icon = if st.snapshot.wm_available {
                    Self::monitor_num_to_icon(monitor_num)
                } else {
                    "•"
                };
                self.monitor_label.set_text(monitor_icon);
                self.ui_last_monitor_num.set(monitor_num);
            }
        }
        self.update_tab_styles();
    }

    fn update_tab_styles(&self) {
        if let Ok(mut st) = self.state.try_borrow_mut() {
            if st.last_class_masks.len() != self.tab_buttons.len() {
                st.last_class_masks = vec![0u8; self.tab_buttons.len()];
            }

            let active_tab = st
                .snapshot
                .wm_available
                .then_some(st.snapshot.active_tag)
                .flatten()
                .map(TagId::index);
            for (i, button) in self.tab_buttons.iter().enumerate() {
                let tag_opt = st
                    .snapshot
                    .wm_available
                    .then(|| st.snapshot.tags.get(i))
                    .flatten();
                let desired_mask = Self::classes_mask_for(tag_opt, active_tab == Some(i));
                let prev_mask = st.last_class_masks[i];

                if desired_mask == prev_mask {
                    continue;
                }

                // 移除所有相关 class
                for c in &["selected", "occupied", "filled", "urgent", "empty"] {
                    button.remove_css_class(c);
                }
                // 添加必要 class
                if desired_mask & CLS_URGENT != 0 {
                    button.add_css_class("urgent");
                }
                if desired_mask & CLS_FILLED != 0 {
                    button.add_css_class("filled");
                }
                if desired_mask & CLS_SELECTED != 0 {
                    button.add_css_class("selected");
                }
                if desired_mask & CLS_OCCUPIED != 0 {
                    button.add_css_class("occupied");
                }
                if desired_mask & CLS_EMPTY != 0 {
                    button.add_css_class("empty");
                }

                st.last_class_masks[i] = desired_mask;
            }
        }
    }

    // 新增：布局 UI 更新（切换 open/closed、高亮当前布局、更新 toggle 文本）
    fn update_layout_ui(&self) {
        if let Ok(st) = self.state.try_borrow() {
            // 开关按钮文本：显示当前布局符号
            self.layout_toggle.set_label(if st.snapshot.wm_available {
                &st.snapshot.layout_symbol
            } else {
                " ? "
            });

            // revealer 展开/收起
            self.layout_revealer
                .set_reveal_child(st.snapshot.layout_selector_open);

            // 开关按钮 open/closed 类
            self.layout_toggle.remove_css_class("open");
            self.layout_toggle.remove_css_class("closed");
            self.layout_toggle
                .add_css_class(if st.snapshot.layout_selector_open {
                    "open"
                } else {
                    "closed"
                });

            // 当前布局高亮
            let is_tiled = st.snapshot.layout_symbol.contains("[]=");
            let is_floating = st.snapshot.layout_symbol.contains("><>");
            let is_monocle = st.snapshot.layout_symbol.contains("[M]");

            for b in [
                &self.layout_btn_tiled,
                &self.layout_btn_floating,
                &self.layout_btn_monocle,
            ] {
                b.remove_css_class("current");
            }
            if is_tiled {
                self.layout_btn_tiled.add_css_class("current");
            } else if is_floating {
                self.layout_btn_floating.add_css_class("current");
            } else if is_monocle {
                self.layout_btn_monocle.add_css_class("current");
            }
        }
    }

    fn update_time_display(&self) {
        if let Ok(st) = self.state.try_borrow() {
            self.time_button.set_label(&st.snapshot.time);
        }
    }

    fn update_volume_display(&self) {
        if let Ok(st) = self.state.try_borrow() {
            match st.snapshot.audio.volume_percent {
                Some(volume) => self.update_volume_display_inner(
                    i32::from(volume.rounded()),
                    st.snapshot.audio.muted,
                ),
                None => self.volume_button.set_label("Vol --%"),
            }
        }
    }

    fn update_volume_display_inner(&self, volume: i32, muted: bool) {
        let label = if muted {
            format!("Muted {}%", volume)
        } else {
            format!("Vol {}%", volume)
        };
        self.volume_button.set_label(&label);
    }

    fn update_theme_display(&self) {
        let is_dark = if let Ok(st) = self.state.try_borrow() {
            st.snapshot.theme == ThemeMode::Dark
        } else {
            true
        };

        self.window.remove_css_class("theme-dark");
        self.window.remove_css_class("theme-light");
        self.window
            .add_css_class(if is_dark { "theme-dark" } else { "theme-light" });

        self.theme_button.set_label(if is_dark { "☾" } else { "☀" });
    }

    fn update_metrics(&self) {
        if let Ok(mut st) = self.state.try_borrow_mut() {
            let mem_ratio = st
                .snapshot
                .system
                .memory_percent
                .map(|value| value.as_f64() / 100.0)
                .unwrap_or(0.0)
                .clamp(0.0, 1.0);
            let prev_mem = st.last_mem_fraction;
            let mem_level_changed =
                usage_to_level_class(mem_ratio) != usage_to_level_class(prev_mem);
            if (mem_ratio - prev_mem).abs() > MEM_REDRAW_THRESHOLD || mem_level_changed {
                st.last_mem_fraction = mem_ratio;
                set_metric_capsule(&self.memory_label, "MEM", mem_ratio);
            }

            let cpu_ratio = st
                .snapshot
                .system
                .cpu_percent
                .map(|value| value.as_f64() / 100.0)
                .unwrap_or(0.0)
                .clamp(0.0, 1.0);
            let prev_cpu = st.last_cpu_usage;
            let cpu_level_changed =
                usage_to_level_class(cpu_ratio) != usage_to_level_class(prev_cpu);
            if (cpu_ratio - prev_cpu).abs() > CPU_REDRAW_THRESHOLD || cpu_level_changed {
                st.last_cpu_usage = cpu_ratio;
                set_metric_capsule(&self.cpu_label, "CPU", cpu_ratio);
            }
        }
    }

    // ========= 工具 =========
    fn monitor_num_to_icon(monitor_num: i32) -> &'static str {
        match monitor_num {
            0 => "1",
            1 => "2",
            2 => "3",
            _ => "•",
        }
    }

    fn classes_mask_for(tag: Option<&TagState>, is_active_index: bool) -> u8 {
        if let Some(t) = tag {
            if t.urgent {
                CLS_URGENT
            } else if t.filled {
                CLS_FILLED
            } else if t.selected && t.occupied {
                CLS_SELECTED | CLS_OCCUPIED
            } else if t.selected || is_active_index {
                CLS_SELECTED
            } else if t.occupied {
                CLS_OCCUPIED
            } else {
                CLS_EMPTY
            }
        } else {
            if is_active_index {
                CLS_SELECTED
            } else {
                CLS_EMPTY
            }
        }
    }

    #[allow(dead_code)]
    fn resize_window_to_monitor(
        &self,
        _expected_x: i32,
        _expected_y: i32,
        expected_width: u32,
        _expected_height: u32,
    ) {
        let scale_factor = self.window.scale_factor().max(1);
        let logical_width = (f64::from(expected_width) / f64::from(scale_factor))
            .round()
            .clamp(1.0, f64::from(i32::MAX)) as i32;

        // GTK4 deliberately exposes no general window-position API: on
        // Wayland the compositor/JWM owns placement. Size is logical, while
        // xbar_core monitor geometry is physical pixels.
        self.window.set_default_size(logical_width, 40);
    }

    fn show(&self) {
        // 在显示前确保所有样式已应用
        self.update_tab_styles();
        self.update_theme_display();

        self.window.present();

        // 在窗口显示后启用透明度支持并强制重绘
        if let Some(surface) = self.window.surface() {
            surface.set_opaque_region(None);
        }
        self.window.queue_draw();
    }
}

// ========= main =========
fn main() -> glib::ExitCode {
    let args: Vec<String> = env::args().collect();
    let shared_path = args.iter().skip(1).last().cloned().unwrap_or_default();

    if let Err(e) = initialize_logging("gtk_bar", &shared_path) {
        eprintln!("Failed to initialize logging: {}", e);
        std::process::exit(1);
    }

    info!("Starting GTK4 Bar (layout selector optimized like iced_bar)");

    // Get monitor ID from environment or shared path to create unique application ID
    let monitor_id = env::var("JWM_MONITOR_ID").unwrap_or_else(|_| {
        // Extract monitor ID from shared path like "/dev/shm/jwm_bar_mon_1"
        shared_path
            .split('_')
            .next_back()
            .and_then(|s| s.parse::<i32>().ok())
            .map(|id| id.to_string())
            .unwrap_or_else(|| "0".to_string())
    });

    let app_id = format!("dev.gtk.bar.mon{}", monitor_id);
    info!("Application ID: {}", app_id);

    // GTK 应用
    let app = Application::builder()
        .application_id(&app_id)
        .flags(gio::ApplicationFlags::HANDLES_OPEN | gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();

    let shared_path_clone = shared_path.clone();
    app.connect_activate(move |app| {
        let app_instance = TabBarApp::new(app, shared_path_clone.clone());
        app_instance.show();
    });

    // 文件打开处理
    app.connect_open(move |app, files, hint| {
        info!(
            "App received {} files to open with hint: {}",
            files.len(),
            hint
        );
        for file in files {
            if let Some(path) = file.path() {
                info!("File to open: {:?}", path);
            }
        }
        app.activate();
    });

    // 命令行处理
    app.connect_command_line(move |app, command_line| {
        let args = command_line.arguments();
        info!("Command line arguments: {:?}", args);
        app.activate();
        0.into()
    });

    app.run()
}
