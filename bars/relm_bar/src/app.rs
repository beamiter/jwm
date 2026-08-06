use log::{info, warn};
use relm4::{
    ComponentParts, ComponentSender, SimpleComponent,
    gtk::{
        self,
        glib::{self, ControlFlow},
        prelude::*,
    },
};
use std::rc::Rc;
use std::time::Duration;

use xbar_core::config::BarConfig;
use xbar_core::controls::PresentationProjector;
use xbar_core::glass::wallpaper::WallpaperFile;
use xbar_core::glass::{GlassStrip, fallback_rgb};
use xbar_core::{
    BarEffect, BarRuntime, BarSnapshot, ModelConfig, PlatformEffectHandler, RuntimeUpdate,
    TransportRecoveryConfig, UserAction,
};
use xbar_gtk::{BarSurface, BarTheme, Dispatch, GlassBackdrop};
use xbar_linux_actions::ProcessActionHandler;

const BAR_NAME: &str = "relm_bar";
const TRANSPORT_RETRY_INTERVAL: Duration = Duration::from_secs(2);
/// How often the shared-memory transport is drained. The bar has no file
/// descriptor to wait on inside the GTK main loop, so it polls.
const TRANSPORT_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug)]
pub enum AppInput {
    /// One projected control was activated. The projection carries the action
    /// already, so this bar never has to name a control to know what a click
    /// on it means.
    Activate(UserAction),
    PollTransport,
    Tick,
}

pub struct AppModel {
    runtime: BarRuntime,
    snapshot: BarSnapshot,
    process_actions: ProcessActionHandler,

    root_window: gtk::ApplicationWindow,
    theme: BarTheme,
    surface: BarSurface,
    /// Width the bar falls back to before, and after, the window manager
    /// describes a monitor: the whole screen.
    default_width: i32,

    // --- Frosted glass ---
    backdrop: GlassBackdrop,
    /// Empty when no wallpaper is configured; the bar is then simply
    /// translucent and the compositor supplies what shows through.
    glass: Option<GlassStrip<WallpaperFile>>,
    /// Bar origin in physical pixels, as last assigned by the window manager.
    /// GTK4 exposes no window position of its own.
    glass_origin: (i32, i32),
}

impl SimpleComponent for AppModel {
    type Init = String;
    type Input = AppInput;
    type Output = ();
    type Root = gtk::ApplicationWindow;
    /// The widget tree is `xbar_gtk`'s, rebuilt from each projection rather
    /// than declared here, so there is nothing for relm4 to track.
    type Widgets = ();

    fn init_root() -> Self::Root {
        gtk::ApplicationWindow::builder()
            .title(BAR_NAME)
            .decorated(false)
            .resizable(true)
            .build()
    }

    fn init(
        shared_path: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let config = BarConfig::load_default().unwrap_or_else(|error| {
            warn!("falling back to the default bar config: {error}");
            BarConfig::default()
        });
        let theme = BarTheme::from_config(&config);
        theme.install();

        // relm4 delivers input through the main loop, so a control can hand
        // its action straight over without re-entering the runtime mid-click.
        let dispatch: Dispatch = {
            let sender = sender.clone();
            Rc::new(move |action| sender.input(AppInput::Activate(action)))
        };
        let surface = BarSurface::new(&theme, dispatch);
        let backdrop = GlassBackdrop::new();
        root.set_child(Some(&backdrop.behind(&surface)));
        root.add_css_class("transparent-window");

        let (screen_width, screen_height) = xbar_gtk::primary_screen_size();
        let default_width = xbar_gtk::logical_width(screen_width);
        // Span the screen from the first frame, as the Cairo bars do. The
        // window manager's own geometry replaces this as soon as it arrives,
        // but a bar must never flash at some arbitrary width.
        root.set_default_size(default_width, theme.metrics.bar_height);

        root.connect_realize(|window| {
            // Per-pixel alpha instead of an opaque strip: the compositor then
            // blends the wallpaper behind the bar's translucent tint.
            if let Some(surface) = window.surface() {
                surface.set_opaque_region(None);
            }
            window.queue_draw();
        });

        let glass =
            config
                .glass
                .file_strip(screen_width, screen_height, fallback_rgb(config.theme));
        if glass.is_some() {
            root.add_css_class("glass");
        }

        let mut runtime = if shared_path.is_empty() {
            BarRuntime::new(config.model_config())
        } else {
            let recovery = TransportRecoveryConfig::new(shared_path, TRANSPORT_RETRY_INTERVAL)
                .expect("static transport recovery config is valid");
            BarRuntime::with_managed_transport(config.model_config(), recovery)
        }
        .unwrap_or_else(|error| {
            warn!("falling back to the default model config: {error}");
            BarRuntime::new(ModelConfig::default()).expect("the default model config is valid")
        });
        let mut initial_update = runtime.tick();
        initial_update.merge(runtime.poll_transport());
        let snapshot = runtime.snapshot();

        let mut model = AppModel {
            runtime,
            snapshot,
            process_actions: ProcessActionHandler::default(),
            root_window: root.clone(),
            theme,
            surface,
            default_width,
            backdrop,
            glass,
            glass_origin: (0, 0),
        };

        model.handle_runtime_update(initial_update);
        model.sync_views();
        spawn_runtime_timers(sender);
        root.present();

        ComponentParts { model, widgets: () }
    }

    fn update(&mut self, message: Self::Input, _sender: ComponentSender<Self>) {
        match message {
            AppInput::Activate(action) => self.dispatch(action),
            AppInput::PollTransport => {
                let update = self.runtime.poll_transport();
                self.handle_runtime_update(update);
            }
            AppInput::Tick => {
                let update = self.runtime.tick();
                self.handle_runtime_update(update);
                // Cheap unless the wallpaper file actually changed.
                self.refresh_glass();
            }
        }
    }
}

impl AppModel {
    fn dispatch(&mut self, action: UserAction) {
        let update = self.runtime.dispatch(action);
        self.handle_runtime_update(update);
    }

    fn handle_runtime_update(&mut self, update: RuntimeUpdate) {
        for issue in &update.issues {
            warn!("xbar runtime issue: {issue:?}");
        }
        let needs_redraw = update.needs_redraw();
        for effect in update.platform_effects {
            self.handle_platform_effect(effect);
        }

        if needs_redraw {
            self.snapshot = self.runtime.snapshot();
            self.sync_views();
        }
    }

    fn handle_platform_effect(&mut self, effect: BarEffect) {
        match effect {
            BarEffect::ApplyMonitorGeometry(geometry) => {
                self.glass_origin = (geometry.x, geometry.y);
                self.refresh_glass();
                let scale_factor = self.root_window.scale_factor().max(1);
                let logical_width = (f64::from(geometry.width) / f64::from(scale_factor))
                    .round()
                    .clamp(1.0, f64::from(i32::MAX)) as i32;
                // GTK4/Wayland leaves placement to the compositor. Convert the
                // core's physical width into GTK logical units.
                self.root_window
                    .set_default_size(logical_width, self.theme.metrics.bar_height);
            }
            BarEffect::ClearMonitorGeometry => {
                // Back to spanning the screen, which is where the bar started.
                self.root_window
                    .set_default_size(self.default_width, self.theme.metrics.bar_height);
            }
            effect @ (BarEffect::Screenshot | BarEffect::OpenAudioControl) => {
                if let Err(error) = self.process_actions.handle(effect) {
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
        let presentation =
            PresentationProjector::project(self.snapshot.view(), &self.theme.presentation);
        xbar_gtk::apply_theme_class(&self.root_window, presentation.theme);
        self.surface.sync(presentation);
    }

    fn refresh_glass(&mut self) {
        let size = xbar_gtk::window_size_px(&self.root_window);
        let origin = self.glass_origin;
        let Some(strip) = self.glass.as_mut() else {
            return;
        };
        self.backdrop.refresh(strip, origin, size);
    }
}

fn spawn_runtime_timers(sender: ComponentSender<AppModel>) {
    let transport_sender = sender.clone();
    glib::timeout_add_local(TRANSPORT_POLL_INTERVAL, move || {
        transport_sender.input(AppInput::PollTransport);
        ControlFlow::Continue
    });

    glib::timeout_add_seconds_local(1, move || {
        sender.input(AppInput::Tick);
        ControlFlow::Continue
    });
}

/// Kept as the crate's own entry-point log line, so a session log says which
/// bar came up before any window exists.
pub fn log_startup(app_id: &str) {
    info!("Starting {BAR_NAME} from the shared presentation projection");
    info!("Application ID: {app_id}");
}
