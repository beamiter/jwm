//! A GTK4 status bar for JWM.
//!
//! The widget tree is a direct translation of what the Cairo bars draw: every
//! cell comes from [`PresentationProjector`], in the order and with the text
//! `xbar_core`'s layout engine would give it, so this bar and `xcb_bar` show
//! the same icons, the same values, and the same macOS material. The widgets
//! themselves live in `xbar_gtk`, shared with `relm_bar`.

use gtk4::gio;
use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow, glib};
use log::{info, warn};
use std::cell::{Cell, RefCell};
use std::env;
use std::rc::{Rc, Weak};
use std::time::Duration;

use xbar_core::config::BarConfig;
use xbar_core::controls::PresentationProjector;
use xbar_core::glass::wallpaper::WallpaperFile;
use xbar_core::glass::{GlassStrip, fallback_rgb};
use xbar_core::logging::init as initialize_logging;
use xbar_core::{
    BarEffect, BarRuntime, BarSnapshot, PlatformEffectHandler, RuntimeUpdate,
    TransportRecoveryConfig, UserAction,
};
use xbar_gtk::{BarSurface, BarTheme, Dispatch, GlassBackdrop};
use xbar_linux_actions::ProcessActionHandler;

const BAR_NAME: &str = "gtk_bar";
const TRANSPORT_RETRY_INTERVAL: Duration = Duration::from_secs(2);
/// How often the shared-memory transport is drained. The bar has no file
/// descriptor to wait on inside the GTK main loop, so it polls.
const TRANSPORT_POLL_INTERVAL: Duration = Duration::from_millis(50);

struct BarApp {
    window: ApplicationWindow,
    surface: BarSurface,
    theme: BarTheme,
    /// Width the bar falls back to before, and after, the window manager
    /// describes a monitor: the whole screen.
    default_width: i32,

    runtime: RefCell<BarRuntime>,
    snapshot: RefCell<BarSnapshot>,
    process_actions: RefCell<ProcessActionHandler>,

    backdrop: GlassBackdrop,
    glass: RefCell<Option<GlassStrip<WallpaperFile>>>,
    /// Bar origin in physical pixels, as last assigned by the window manager.
    /// GTK4 exposes no window position of its own, so the geometry the WM
    /// hands us is what tells the backdrop where to sample.
    glass_origin: Cell<(i32, i32)>,
}

impl BarApp {
    fn new(app: &Application, shared_path: String) -> Rc<Self> {
        let config = BarConfig::load_default().unwrap_or_else(|error| {
            warn!("falling back to the default bar config: {error}");
            BarConfig::default()
        });
        let theme = BarTheme::from_config(&config);
        theme.install();

        let (screen_width, screen_height) = xbar_gtk::primary_screen_size();
        let default_width = xbar_gtk::logical_width(screen_width);
        let window = ApplicationWindow::builder()
            .application(app)
            .title(BAR_NAME)
            .decorated(false)
            .resizable(true)
            // Span the screen from the first frame, as the Cairo bars do. The
            // window manager's own geometry replaces this as soon as it
            // arrives, but a bar must never flash at some arbitrary width.
            .default_width(default_width)
            .default_height(theme.metrics.bar_height)
            .build();
        window.add_css_class("transparent-window");
        window.connect_realize(|window| {
            // Per-pixel alpha instead of an opaque strip: the compositor then
            // blends the wallpaper behind the bar's translucent tint.
            if let Some(surface) = window.surface() {
                surface.set_opaque_region(None);
            }
            window.queue_draw();
        });

        let mut runtime = if shared_path.is_empty() {
            BarRuntime::new(config.model_config())
        } else {
            let recovery =
                TransportRecoveryConfig::new(shared_path.clone(), TRANSPORT_RETRY_INTERVAL)
                    .expect("static transport recovery config is valid");
            BarRuntime::with_managed_transport(config.model_config(), recovery)
        }
        .expect("bar config yields a valid model config");
        let mut initial_update = runtime.tick();
        initial_update.merge(runtime.poll_transport());
        let snapshot = runtime.snapshot();

        let glass =
            config
                .glass
                .file_strip(screen_width, screen_height, fallback_rgb(config.theme));

        let instance = Rc::new_cyclic(|weak: &Weak<Self>| {
            let dispatch: Dispatch = {
                let weak = weak.clone();
                Rc::new(move |action| {
                    if let Some(app) = weak.upgrade() {
                        app.dispatch(action);
                    }
                })
            };
            let surface = BarSurface::new(&theme, dispatch);
            let backdrop = GlassBackdrop::new();
            window.set_child(Some(&backdrop.behind(&surface)));
            if glass.is_some() {
                window.add_css_class("glass");
            }

            Self {
                window,
                surface,
                theme,
                default_width,
                runtime: RefCell::new(runtime),
                snapshot: RefCell::new(snapshot),
                process_actions: RefCell::new(ProcessActionHandler::default()),
                backdrop,
                glass: RefCell::new(glass),
                glass_origin: Cell::new((0, 0)),
            }
        });

        instance.handle_runtime_update(initial_update);
        instance.sync_views();
        instance.spawn_timers();
        instance
    }

    fn spawn_timers(self: &Rc<Self>) {
        // Providers and the clock are refreshed by the core runtime.
        glib::timeout_add_seconds_local(1, {
            let app = Rc::downgrade(self);
            move || {
                let Some(app) = app.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                let update = app.runtime.borrow_mut().tick();
                app.handle_runtime_update(update);
                // Cheap unless the wallpaper file actually changed.
                app.refresh_glass();
                glib::ControlFlow::Continue
            }
        });

        glib::timeout_add_local(TRANSPORT_POLL_INTERVAL, {
            let app = Rc::downgrade(self);
            move || {
                let Some(app) = app.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                let update = app.runtime.borrow_mut().poll_transport();
                app.handle_runtime_update(update);
                glib::ControlFlow::Continue
            }
        });
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
            *self.snapshot.borrow_mut() = self.runtime.borrow().snapshot();
            self.sync_views();
        }
    }

    fn handle_platform_effect(&self, effect: BarEffect) {
        match effect {
            BarEffect::ApplyMonitorGeometry(geometry) => {
                self.glass_origin.set((geometry.x, geometry.y));
                self.refresh_glass();
                self.resize_window_to_monitor(geometry.width);
            }
            BarEffect::ClearMonitorGeometry => {
                // Back to spanning the screen, which is where the bar started.
                self.window
                    .set_default_size(self.default_width, self.theme.metrics.bar_height);
            }
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

    fn sync_views(&self) {
        let snapshot = self.snapshot.borrow();
        let presentation =
            PresentationProjector::project(snapshot.view(), &self.theme.presentation);
        xbar_gtk::apply_theme_class(&self.window, presentation.theme);
        self.surface.sync(presentation);
    }

    fn refresh_glass(&self) {
        let mut glass = self.glass.borrow_mut();
        let Some(strip) = glass.as_mut() else {
            return;
        };
        // Only the origin comes from the window manager: GTK4 deliberately
        // exposes no window position, and the size is better read from the
        // window itself.
        self.backdrop.refresh(
            strip,
            self.glass_origin.get(),
            xbar_gtk::window_size_px(&self.window),
        );
    }

    fn resize_window_to_monitor(&self, expected_width: u32) {
        let scale_factor = self.window.scale_factor().max(1);
        let logical_width = (f64::from(expected_width) / f64::from(scale_factor))
            .round()
            .clamp(1.0, f64::from(i32::MAX)) as i32;

        // GTK4 deliberately exposes no general window-position API: on Wayland
        // the compositor/JWM owns placement. Size is logical, while xbar_core
        // monitor geometry is physical pixels.
        self.window
            .set_default_size(logical_width, self.theme.metrics.bar_height);
    }

    fn show(&self) {
        self.window.present();
        if let Some(surface) = self.window.surface() {
            surface.set_opaque_region(None);
        }
        self.window.queue_draw();
    }
}

fn main() -> glib::ExitCode {
    let shared_path = env::args().skip(1).last().unwrap_or_default();

    if let Err(error) = initialize_logging(BAR_NAME, &shared_path) {
        eprintln!("Failed to initialize logging: {error}");
        std::process::exit(1);
    }

    info!("Starting {BAR_NAME}");

    // Get monitor ID from environment or shared path to create unique application ID
    let monitor_id = env::var("JWM_MONITOR_ID").unwrap_or_else(|_| {
        // Extract monitor ID from shared path like "/dev/shm/jwm_bar_mon_1"
        shared_path
            .split('_')
            .next_back()
            .and_then(|value| value.parse::<i32>().ok())
            .map(|id| id.to_string())
            .unwrap_or_else(|| "0".to_string())
    });

    let app_id = format!("dev.gtk.bar.mon{monitor_id}");
    info!("Application ID: {app_id}");

    let app = Application::builder()
        .application_id(&app_id)
        .flags(gio::ApplicationFlags::HANDLES_OPEN | gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();

    // The bar has to outlive `activate`: its timers and its pills hold weak
    // references so the widget tree cannot keep the bar alive by itself, which
    // makes the application the one owner of it.
    let instance: RefCell<Option<Rc<BarApp>>> = RefCell::new(None);
    app.connect_activate({
        let shared_path = shared_path.clone();
        move |app| {
            if instance.borrow().is_some() {
                return;
            }
            let bar = BarApp::new(app, shared_path.clone());
            bar.show();
            *instance.borrow_mut() = Some(bar);
        }
    });

    app.connect_open(move |app, files, hint| {
        info!(
            "App received {} files to open with hint: {hint}",
            files.len()
        );
        app.activate();
    });

    app.connect_command_line(move |app, command_line| {
        info!("Command line arguments: {:?}", command_line.arguments());
        app.activate();
        0.into()
    });

    app.run()
}
