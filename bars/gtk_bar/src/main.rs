//! A GTK4 status bar for JWM.
//!
//! The widget tree is a direct translation of what the Cairo bars draw: every
//! cell comes from [`PresentationProjector`], in the order and with the text
//! `xbar_core`'s layout engine would give it, so this bar and `xcb_bar` show
//! the same icons, the same values, and the same macOS material.

mod pill;
mod theme;

use gdk4::prelude::*;
use gtk4::gio;
use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow, Label, Orientation, Overflow, Overlay, Picture, glib};
use log::{info, warn};
use std::cell::{Cell, RefCell};
use std::env;
use std::rc::{Rc, Weak};
use std::time::Duration;

use xbar_core::config::BarConfig;
use xbar_core::controls::PresentationProjector;
use xbar_core::glass::wallpaper::WallpaperFile;
use xbar_core::glass::{DEFAULT_BACKGROUND_OPACITY, GlassStrip, fallback_rgb};
use xbar_core::logging::init as initialize_logging;
use xbar_core::presentation::{Palette, PresentationConfig, PresentationLabels};
use xbar_core::{
    BarEffect, BarRuntime, BarSnapshot, PlatformEffectHandler, RuntimeUpdate, ThemeMode,
    TransportRecoveryConfig, UserAction,
};
use xbar_linux_actions::ProcessActionHandler;

use crate::pill::{Dispatch, PillRow, PillStyle};
use crate::theme::Metrics;

const BAR_NAME: &str = "gtk_bar";
const TRANSPORT_RETRY_INTERVAL: Duration = Duration::from_secs(2);
/// How often the shared-memory transport is drained. The bar has no file
/// descriptor to wait on inside the GTK main loop, so it polls.
const TRANSPORT_POLL_INTERVAL: Duration = Duration::from_millis(50);

struct BarApp {
    window: ApplicationWindow,
    /// Tags, the layout button, and any revealed layout options, in the order
    /// the layout engine places them.
    leading: PillRow,
    /// Status cells, reversed from the projector's right-to-left order.
    trailing: PillRow,
    client_name: Label,

    presentation: PresentationConfig,
    metrics: Metrics,
    /// Width the bar falls back to before, and after, the window manager
    /// describes a monitor: the whole screen.
    default_width: i32,

    runtime: RefCell<BarRuntime>,
    snapshot: RefCell<BarSnapshot>,
    process_actions: RefCell<ProcessActionHandler>,

    // --- Frosted glass ---
    /// Backdrop image behind the bar, empty when no wallpaper is configured.
    /// Without one the bar is simply translucent and the compositor supplies
    /// what shows through, which is what `xcb_bar` does under a compositor.
    glass_picture: Picture,
    glass: RefCell<Option<GlassStrip<WallpaperFile>>>,
    /// Generation of the strip currently uploaded into `glass_picture`.
    glass_generation: Cell<u64>,
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

        let mut presentation = config.presentation.clone();
        // Monochrome Nerd Font glyphs tinted by the text color read like macOS
        // template icons; only replace the stock emoji so a config that
        // overrides individual labels keeps its customization. `xcb_bar` makes
        // exactly this substitution, and the two must not diverge.
        if presentation.labels == PresentationLabels::default() {
            presentation.labels = PresentationLabels::nerd_font();
        }
        let metrics = Metrics::from_config(&presentation);
        let background_opacity = config
            .background_opacity
            .unwrap_or(DEFAULT_BACKGROUND_OPACITY);
        apply_stylesheet(&theme::stylesheet(
            &presentation,
            &config.font,
            background_opacity,
        ));

        let (screen_width, screen_height) = primary_screen_size();
        let default_width = logical_screen_width(screen_width);
        let window = ApplicationWindow::builder()
            .application(app)
            .title(BAR_NAME)
            .decorated(false)
            .resizable(true)
            // Span the screen from the first frame, as the Cairo bars do. The
            // window manager's own geometry replaces this as soon as it
            // arrives, but a bar must never flash at some arbitrary width.
            .default_width(default_width)
            .default_height(metrics.bar_height)
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
            let style = PillStyle {
                metrics,
                // Both themes use the same accent, so the progress line does
                // not have to be rebuilt when the theme is toggled.
                accent: Palette::for_theme(ThemeMode::Dark).accent,
                font: pill_font(&config.font, presentation.font_size),
            };

            let leading = PillRow::new(style.clone(), dispatch.clone());
            let trailing = PillRow::new(style, dispatch);

            let client_name = Label::new(None);
            client_name.add_css_class("client-name");
            client_name.set_single_line_mode(true);
            client_name.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            client_name.set_hexpand(true);
            client_name.set_halign(gtk4::Align::Center);

            let root = gtk4::Box::new(Orientation::Horizontal, metrics.item_gap);
            root.add_css_class("bar-root");
            root.append(leading.widget());
            root.append(&client_name);
            root.append(trailing.widget());
            trailing.widget().set_halign(gtk4::Align::End);

            // Slide a backdrop under the bar: the widgets keep their own tree,
            // and the frosted wallpaper is simply what shows through the
            // translucent background.
            let glass_picture = Picture::new();
            glass_picture.set_can_target(false);
            glass_picture.set_content_fit(gtk4::ContentFit::Fill);
            glass_picture.set_hexpand(true);
            glass_picture.set_vexpand(true);
            let clip = gtk4::Box::new(Orientation::Horizontal, 0);
            clip.add_css_class("glass-backdrop");
            clip.set_overflow(Overflow::Hidden);
            clip.append(&glass_picture);

            let overlay = Overlay::new();
            overlay.set_child(Some(&clip));
            overlay.add_overlay(&root);
            window.set_child(Some(&overlay));

            if glass.is_some() {
                window.add_css_class("glass");
            }

            Self {
                window,
                leading,
                trailing,
                client_name,
                presentation,
                metrics,
                default_width,
                runtime: RefCell::new(runtime),
                snapshot: RefCell::new(snapshot),
                process_actions: RefCell::new(ProcessActionHandler::default()),
                glass_picture,
                glass: RefCell::new(glass),
                glass_generation: Cell::new(0),
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
                    .set_default_size(self.default_width, self.metrics.bar_height);
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

    /// Rebuild the widget tree's contents from one projection of the model.
    ///
    /// Everything visible is decided by `xbar_core`: which cells exist, their
    /// order, their glyphs, and their state. This function only spends that
    /// decision on widgets.
    fn sync_views(&self) {
        let snapshot = self.snapshot.borrow();
        let presentation = PresentationProjector::project(snapshot.view(), &self.presentation);

        self.window.remove_css_class("theme-dark");
        self.window.remove_css_class("theme-light");
        self.window.add_css_class(match presentation.theme {
            ThemeMode::Dark => "theme-dark",
            ThemeMode::Light => "theme-light",
        });

        let mut leading = presentation.tags;
        leading.push(presentation.layout_button);
        leading.extend(presentation.layout_choices);
        self.leading.sync(&leading);

        // The projector orders status cells right to left, anchored at the
        // right edge; a horizontal box packs them the other way around.
        let mut trailing = presentation.status;
        trailing.reverse();
        self.trailing.sync(&trailing);

        // The label stays in the tree even with nothing to say: it is the gap
        // that holds the status cluster against the right edge, which is where
        // the layout engine anchors it.
        self.client_name.set_text(
            presentation
                .client_name
                .as_ref()
                .map_or("", |client| client.value.as_str()),
        );
    }

    /// The bar's size in physical pixels, which is what the backdrop is
    /// sampled and uploaded in.
    ///
    /// This comes from the window rather than from the geometry the window
    /// manager asked for: the two agree once the resize lands, and using the
    /// real allocation means a backdrop is never stretched to a width the
    /// window does not have.
    fn bar_size_px(&self) -> (u32, u32) {
        let scale = self.window.scale_factor().max(1) as u32;
        (
            (self.window.width().max(1) as u32).saturating_mul(scale),
            (self.window.height().max(1) as u32).saturating_mul(scale),
        )
    }

    /// Re-frost the wallpaper under the bar and upload it when it changed.
    ///
    /// Everything here is a no-op on an unchanged wallpaper and geometry: the
    /// cache returns the same generation and the texture is left alone.
    fn refresh_glass(&self) {
        let mut glass = self.glass.borrow_mut();
        let Some(strip) = glass.as_mut() else {
            return;
        };
        // Only the origin comes from the window manager: GTK4 deliberately
        // exposes no window position, and the size is better read from the
        // window itself.
        let (x, y) = self.glass_origin.get();
        let (width, height) = self.bar_size_px();
        let Some((generation, image)) = strip.ensure(x, y, width, height) else {
            return;
        };
        if generation == self.glass_generation.get() {
            return;
        }

        let bytes = glib::Bytes::from_owned(image.to_rgba8());
        let texture = gdk4::MemoryTexture::new(
            image.width() as i32,
            image.height() as i32,
            gdk4::MemoryFormat::R8g8b8a8,
            &bytes,
            image.stride(),
        );
        self.glass_picture.set_paintable(Some(&texture));
        self.glass_generation.set(generation);
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
            .set_default_size(logical_width, self.metrics.bar_height);
    }

    fn show(&self) {
        self.window.present();
        if let Some(surface) = self.window.surface() {
            surface.set_opaque_region(None);
        }
        self.window.queue_draw();
    }
}

/// The configured font at the size the Cairo renderer forces onto it, which is
/// the description every pill width is measured against.
fn pill_font(font: &str, font_size: f32) -> gtk4::pango::FontDescription {
    let mut description = gtk4::pango::FontDescription::from_string(font);
    description.set_absolute_size(f64::from(font_size.max(1.0)) * f64::from(gtk4::pango::SCALE));
    description
}

fn apply_stylesheet(css: &str) {
    let provider = gtk4::CssProvider::new();
    provider.load_from_string(css);
    if let Some(display) = gdk4::Display::default() {
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
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

/// A physical screen width in the logical units GTK sizes windows in.
fn logical_screen_width(physical_width: u32) -> i32 {
    let scale = gdk4::Display::default()
        .and_then(|display| display.monitors().item(0))
        .and_then(|object| object.downcast::<gdk4::Monitor>().ok())
        .map_or(1, |monitor| monitor.scale_factor().max(1));
    (f64::from(physical_width) / f64::from(scale))
        .round()
        .clamp(1.0, f64::from(i32::MAX)) as i32
}

/// Physical size of the primary monitor, which is the canvas the compositor
/// lays the wallpaper out on.
fn primary_screen_size() -> (u32, u32) {
    let fallback = (1920, 1080);
    let Some(display) = gdk4::Display::default() else {
        return fallback;
    };
    let Some(monitor) = display
        .monitors()
        .item(0)
        .and_then(|object| object.downcast::<gdk4::Monitor>().ok())
    else {
        return fallback;
    };
    let geometry = monitor.geometry();
    let scale = monitor.scale_factor().max(1);
    (
        (geometry.width() * scale).max(1) as u32,
        (geometry.height() * scale).max(1) as u32,
    )
}
