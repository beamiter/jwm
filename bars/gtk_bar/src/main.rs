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
use std::cell::RefCell;
use std::env;
use std::rc::{Rc, Weak};
use std::time::Duration;

use xbar_core::config::BarConfig;
use xbar_core::controls::PresentationProjector;
use xbar_core::logging::init as initialize_logging;
use xbar_core::{
    BarEffect, BarRuntime, BarSnapshot, PlatformEffectHandler, RuntimeUpdate,
    TransportRecoveryConfig, UserAction,
};
use xbar_gtk::{BarSurface, BarTheme, Dispatch};
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
    /// The startup mode decision. The window was created for this mode, so it
    /// cannot change until the bar restarts.
    translucent: bool,

    runtime: RefCell<BarRuntime>,
    snapshot: RefCell<BarSnapshot>,
    process_actions: RefCell<ProcessActionHandler>,
}

impl BarApp {
    fn new(app: &Application, shared_path: String) -> Rc<Self> {
        let config = BarConfig::load_default().unwrap_or_else(|error| {
            warn!("falling back to the default bar config: {error}");
            BarConfig::default()
        });
        // Sampled once, before the window exists: whether a surface gets
        // per-pixel alpha is a creation-time decision, so a compositor that
        // arrives or leaves later is not seen until the bar restarts. GDK's
        // X11 backend watches the same _NET_WM_CM_S<n> selection every other
        // bar checks — and owning that selection only says compositing is on,
        // not that jwm will actually blur behind the bar.
        let translucent = gtk4::gdk::Display::default()
            .map(|display| display.is_composited() && display.is_rgba())
            .unwrap_or(false);
        let theme = BarTheme::from_config(&config, translucent);
        theme.install();

        let (screen_width, _) = xbar_gtk::primary_screen_size();
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
        if translucent {
            window.add_css_class("transparent-window");
            window.connect_realize(|window| {
                // Per-pixel alpha instead of an opaque strip: the compositor
                // then blends whatever is behind the bar through its
                // translucent tint. A solid bar keeps GTK's computed opaque
                // region, which is exactly what an opaque strip wants.
                if let Some(surface) = window.surface() {
                    surface.set_opaque_region(None);
                }
                window.queue_draw();
            });
        }

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

        let instance = Rc::new_cyclic(|weak: &Weak<Self>| {
            let dispatch: Dispatch = {
                let weak = weak.clone();
                Rc::new(move |action, completion| {
                    if let Some(app) = weak.upgrade() {
                        let accepted = app.dispatch(action);
                        if let Some(completion) = completion {
                            let _ = completion.send(accepted);
                        }
                    } else if let Some(completion) = completion {
                        let _ = completion.send(false);
                    }
                })
            };
            let surface = BarSurface::new(&theme, dispatch);
            window.set_child(Some(surface.widget()));

            Self {
                window,
                surface,
                theme,
                default_width,
                translucent,
                runtime: RefCell::new(runtime),
                snapshot: RefCell::new(snapshot),
                process_actions: RefCell::new(ProcessActionHandler::default()),
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
                app.surface.maintain();
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
                app.surface.maintain();
                glib::ControlFlow::Continue
            }
        });
    }

    fn dispatch(&self, action: UserAction) -> bool {
        let update = self.runtime.borrow_mut().dispatch(action);
        let accepted = !update.has_issues();
        self.handle_runtime_update(update);
        accepted
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
        self.surface.sync(&snapshot, presentation);
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
        if self.translucent {
            if let Some(surface) = self.window.surface() {
                surface.set_opaque_region(None);
            }
            self.window.queue_draw();
        }
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
