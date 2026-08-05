use log::{info, warn};
use relm4::{
    Component, ComponentController, ComponentParts, ComponentSender, Controller, SimpleComponent,
    gtk::{
        self,
        glib::{self, ControlFlow},
        prelude::*,
    },
};
use std::time::Duration;

use xbar_core::glass::wallpaper::WallpaperFile;
use xbar_core::glass::{GlassStrip, fallback_rgb};
use xbar_core::{
    BarEffect, BarRuntime, BarSnapshot, LayoutId, ModelConfig, PlatformEffectHandler,
    RuntimeUpdate, ShellRoute, TagId, TransportRecoveryConfig, UserAction,
};
use xbar_linux_actions::ProcessActionHandler;

use crate::components::{
    LayoutSelectorInput, LayoutSelectorModel, LayoutSelectorOutput, LayoutSelectorState,
    StatusStripInput, StatusStripModel, StatusStripOutput, StatusStripState, WorkspacesInput,
    WorkspacesModel, WorkspacesOutput, WorkspacesState,
};

#[derive(Debug)]
pub enum AppInput {
    TabSelected(usize),
    LayoutChanged(u32),
    ToggleLayoutPanel,
    ToggleSeconds,
    ToggleTheme,
    ToggleMute,
    VolumeStep(i32),
    Screenshot,
    OpenShell,
    PollTransport,
    Tick,
}

pub struct AppModel {
    runtime: BarRuntime,
    snapshot: BarSnapshot,
    process_actions: ProcessActionHandler,

    root_window: gtk::ApplicationWindow,
    workspaces: Controller<WorkspacesModel>,
    layout_selector: Controller<LayoutSelectorModel>,
    status_strip: Controller<StatusStripModel>,

    // --- Frosted glass ---
    /// Backdrop image behind the panel, empty when no wallpaper is configured.
    glass_picture: gtk::Picture,
    glass: Option<GlassStrip<WallpaperFile>>,
    /// Generation of the strip currently held in `glass_picture`.
    glass_generation: u64,
    /// Bar origin in physical pixels, as last assigned by the window manager.
    /// GTK4 exposes no window position of its own.
    glass_origin: (i32, i32),
}

#[relm4::component(pub)]
impl SimpleComponent for AppModel {
    type Init = String;
    type Input = AppInput;
    type Output = ();

    view! {
        #[root]
        gtk::ApplicationWindow {
            set_title: Some("relm_bar"),
            set_decorated: false,
            set_default_size: (1000, 40),
            set_resizable: true,
            add_css_class: "transparent-window",
            set_child: Some(&glass_overlay),
        }
    }

    fn init(
        shared_path: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        crate::components::load_css();

        let workspaces = WorkspacesModel::builder()
            .launch(WorkspacesState::default())
            .forward(sender.input_sender(), |output| match output {
                WorkspacesOutput::Selected(index) => AppInput::TabSelected(index),
            });

        let layout_selector = LayoutSelectorModel::builder()
            .launch(LayoutSelectorState::default())
            .forward(sender.input_sender(), |output| match output {
                LayoutSelectorOutput::TogglePanel => AppInput::ToggleLayoutPanel,
                LayoutSelectorOutput::SelectLayout(index) => AppInput::LayoutChanged(index),
            });

        let status_strip = StatusStripModel::builder()
            .launch(StatusStripState::default())
            .forward(sender.input_sender(), |output| match output {
                StatusStripOutput::ToggleSeconds => AppInput::ToggleSeconds,
                StatusStripOutput::ToggleTheme => AppInput::ToggleTheme,
                StatusStripOutput::ToggleMute => AppInput::ToggleMute,
                StatusStripOutput::VolumeStep(step) => AppInput::VolumeStep(step),
                StatusStripOutput::Screenshot => AppInput::Screenshot,
                StatusStripOutput::OpenShell => AppInput::OpenShell,
            });

        let top_hbox = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        top_hbox.set_margin_top(0);
        top_hbox.set_margin_bottom(0);
        top_hbox.set_margin_start(1);
        top_hbox.set_margin_end(1);
        top_hbox.add_css_class("panel-root");

        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);

        top_hbox.append(workspaces.widget());
        top_hbox.append(layout_selector.widget());
        top_hbox.append(&spacer);
        top_hbox.append(status_strip.widget());

        // Slide a backdrop under the panel: the panel keeps its own widget
        // tree, and the frosted wallpaper is simply what shows through its
        // translucent background. The clip box repeats `.panel-root`'s margin
        // and corner radius so the backdrop stops where the panel does.
        let glass_picture = gtk::Picture::new();
        glass_picture.set_can_target(false);
        // `set_content_fit(Fill)` would be the modern spelling, but relm4
        // pins gtk4-rs below the 4.8 feature level that exposes it. Dropping
        // the aspect ratio is the same instruction in the older API: the
        // backdrop is built at exactly the bar's pixel size and must land on
        // it one-to-one, never letterboxed.
        #[allow(deprecated)]
        glass_picture.set_keep_aspect_ratio(false);
        glass_picture.set_hexpand(true);
        glass_picture.set_vexpand(true);
        let glass_clip = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        glass_clip.add_css_class("glass-backdrop");
        glass_clip.set_overflow(gtk::Overflow::Hidden);
        glass_clip.append(&glass_picture);
        let glass_overlay = gtk::Overlay::new();
        glass_overlay.set_child(Some(&glass_clip));
        glass_overlay.add_overlay(&top_hbox);

        // Only the `[glass]` section of the shared bar config applies here;
        // the rest of this bar's appearance lives in its CSS.
        let app_config = xbar_core::config::BarConfig::load_default().unwrap_or_else(|error| {
            warn!("falling back to the default bar config: {error}");
            xbar_core::config::BarConfig::default()
        });
        let (screen_width, screen_height) = primary_screen_size();
        let glass = app_config.glass.file_strip(
            screen_width,
            screen_height,
            fallback_rgb(app_config.theme),
        );
        if glass.is_some() {
            root.add_css_class("glass");
        }

        root.set_title(Some("relm_bar"));

        root.connect_realize(|window| {
            window.set_title(Some("relm_bar"));
            if let Some(surface) = window.surface() {
                surface.set_opaque_region(None);
            }
            window.add_css_class("transparent-window");
            window.queue_draw();
        });

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
        let snapshot = runtime.snapshot();

        let mut model = AppModel {
            runtime,
            snapshot,
            process_actions: ProcessActionHandler::default(),
            root_window: root.clone(),
            workspaces,
            layout_selector,
            status_strip,
            glass_picture,
            glass,
            glass_generation: 0,
            glass_origin: (0, 0),
        };

        root.add_css_class("theme-dark");
        root.remove_css_class("theme-light");

        model.handle_runtime_update(initial_update);
        model.sync_all_views();

        spawn_runtime_timers(sender.clone());

        // Present window to ensure proper initialization
        root.present();

        let widgets = view_output!();
        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>) {
        match msg {
            AppInput::TabSelected(index) => {
                info!("Tab selected: {}", index);
                let Some(tag) = TagId::new(index) else {
                    warn!("Ignoring out-of-range tag index {index}");
                    return;
                };
                if self.snapshot.wm_available {
                    self.dispatch(UserAction::ViewTagOn {
                        tag,
                        monitor: self.snapshot.monitor,
                    });
                } else {
                    warn!("Ignoring tag input until the first WM snapshot arrives");
                }
            }
            AppInput::LayoutChanged(layout_index) => {
                info!("Layout changed: {}", layout_index);
                if self.snapshot.wm_available {
                    self.dispatch(UserAction::SetLayoutOn {
                        layout: LayoutId(layout_index),
                        monitor: self.snapshot.monitor,
                    });
                } else {
                    warn!("Ignoring layout input until the first WM snapshot arrives");
                }
            }
            AppInput::ToggleLayoutPanel => {
                self.dispatch(UserAction::ToggleLayoutSelector);
            }
            AppInput::ToggleSeconds => {
                self.dispatch(UserAction::ToggleSeconds);
            }
            AppInput::ToggleTheme => {
                self.dispatch(UserAction::ToggleTheme);
            }
            AppInput::ToggleMute => {
                self.dispatch(UserAction::ToggleMute);
            }
            AppInput::VolumeStep(step) => {
                self.dispatch(UserAction::AdjustVolume(step));
            }
            AppInput::Screenshot => {
                info!("Taking screenshot");
                self.dispatch(UserAction::Screenshot);
            }
            AppInput::OpenShell => {
                if self.snapshot.wm_available {
                    info!("Opening the JWM shell");
                    self.dispatch(UserAction::OpenShellHub(ShellRoute::Hub));
                } else {
                    warn!("Ignoring shell input until the first WM snapshot arrives");
                }
            }
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
            self.sync_all_views();
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
                // GTK4/Wayland leaves placement to the compositor. Convert
                // the core's physical width into GTK logical units.
                self.root_window.set_default_size(logical_width, 40);
            }
            BarEffect::ClearMonitorGeometry => self.root_window.set_default_size(1000, 40),
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

    /// The bar's size in physical pixels, which is what the backdrop is
    /// sampled and uploaded in.
    ///
    /// This comes from the window rather than from the geometry the window
    /// manager asked for: the two agree once the resize lands, and using the
    /// real allocation means a backdrop is never stretched to a width the
    /// window does not have.
    fn bar_size_px(&self) -> (u32, u32) {
        let scale = self.root_window.scale_factor().max(1) as u32;
        (
            (self.root_window.width().max(1) as u32).saturating_mul(scale),
            (self.root_window.height().max(1) as u32).saturating_mul(scale),
        )
    }

    /// Re-frost the wallpaper under the bar and upload it when it changed.
    ///
    /// Everything here is a no-op on an unchanged wallpaper and geometry: the
    /// cache returns the same generation and the texture is left alone.
    fn refresh_glass(&mut self) {
        let (width, height) = self.bar_size_px();
        let (x, y) = self.glass_origin;
        let Some(strip) = self.glass.as_mut() else {
            return;
        };
        let Some((generation, image)) = strip.ensure(x, y, width, height) else {
            return;
        };
        if generation == self.glass_generation {
            return;
        }

        let bytes = glib::Bytes::from_owned(image.to_rgba8());
        let texture = gtk::gdk::MemoryTexture::new(
            image.width() as i32,
            image.height() as i32,
            gtk::gdk::MemoryFormat::R8g8b8a8,
            &bytes,
            image.stride(),
        );
        self.glass_picture.set_paintable(Some(&texture));
        self.glass_generation = generation;
    }

    fn sync_all_views(&self) {
        self.sync_root_theme();
        self.sync_workspaces_view();
        self.sync_layout_view();
        self.sync_status_view();
    }

    fn sync_root_theme(&self) {
        self.root_window.remove_css_class("theme-dark");
        self.root_window.remove_css_class("theme-light");

        if self.snapshot.theme == xbar_core::ThemeMode::Dark {
            self.root_window.add_css_class("theme-dark");
        } else {
            self.root_window.add_css_class("theme-light");
        }
    }

    fn sync_workspaces_view(&self) {
        let active_tab = self
            .snapshot
            .wm_available
            .then_some(self.snapshot.active_tag)
            .flatten()
            .map(TagId::index)
            .unwrap_or(usize::MAX);
        let tag_status_vec = if self.snapshot.wm_available {
            self.snapshot.tags.clone()
        } else {
            Vec::new()
        };
        self.workspaces.emit(WorkspacesInput::Sync(WorkspacesState {
            active_tab,
            tag_status_vec,
        }));
    }

    fn sync_layout_view(&self) {
        let layout_symbol = if self.snapshot.wm_available {
            self.snapshot.layout_symbol.clone()
        } else {
            " ? ".to_owned()
        };
        self.layout_selector
            .emit(LayoutSelectorInput::Sync(LayoutSelectorState {
                layout_symbol,
                layout_open: self.snapshot.layout_selector_open,
            }));
    }

    fn sync_status_view(&self) {
        let monitor_num = self
            .snapshot
            .wm_available
            .then(|| u8::try_from(self.snapshot.monitor.0).ok())
            .flatten()
            .unwrap_or(u8::MAX);
        self.status_strip
            .emit(StatusStripInput::Sync(StatusStripState {
                current_time: self.snapshot.time.clone(),
                current_volume: self
                    .snapshot
                    .audio
                    .volume_percent
                    .map(|value| i32::from(value.rounded())),
                current_muted: self.snapshot.audio.muted,
                cpu_usage: self
                    .snapshot
                    .system
                    .cpu_percent
                    .map(|value| value.as_f64() / 100.0)
                    .unwrap_or_default(),
                memory_usage: self
                    .snapshot
                    .system
                    .memory_percent
                    .map(|value| value.as_f64() / 100.0)
                    .unwrap_or_default(),
                theme_dark: self.snapshot.theme == xbar_core::ThemeMode::Dark,
                monitor_num,
                wm_available: self.snapshot.wm_available,
            }));
    }
}

fn spawn_runtime_timers(sender: ComponentSender<AppModel>) {
    let transport_sender = sender.clone();
    glib::timeout_add_local(Duration::from_millis(50), move || {
        transport_sender.input(AppInput::PollTransport);
        ControlFlow::Continue
    });

    let tick_sender = sender;
    glib::timeout_add_seconds_local(1, move || {
        tick_sender.input(AppInput::Tick);
        ControlFlow::Continue
    });
}

/// Physical size of the primary monitor, which is the canvas the compositor
/// lays the wallpaper out on.
fn primary_screen_size() -> (u32, u32) {
    let fallback = (1920, 1080);
    let Some(display) = gtk::gdk::Display::default() else {
        return fallback;
    };
    let Some(monitor) = display
        .monitors()
        .item(0)
        .and_then(|object| object.downcast::<gtk::gdk::Monitor>().ok())
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
