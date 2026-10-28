//! GTK4 widgets that render `xbar_core`'s presentation projection.
//!
//! Every bar in this repository shows the same cells because they all project
//! the same [`BarPresentation`]. The Cairo bars turn it into a scene; the GTK
//! bars turn it into the widgets here. Keeping that translation in one crate
//! is what stops `gtk_bar` and `relm_bar` from slowly growing two different
//! ideas of what a bar looks like — which is exactly what they had before.
//!
//! A frontend still owns its window, its main loop, and its runtime; this
//! crate owns what the bar looks like and how a click becomes a
//! [`UserAction`](xbar_core::UserAction).

mod dock;
pub mod pill;
pub mod theme;
mod window;

use std::cell::RefCell;

use gtk4::prelude::*;
use gtk4::{Label, Orientation};
use xbar_core::config::BarConfig;
use xbar_core::controls::BarPresentation;
use xbar_core::presentation::{NodeId, Palette, PresentationConfig, PresentationLabels};
use xbar_core::tag_contrast::{TagContrastMode, TagPalette};
use xbar_core::{BarSnapshot, ThemeMode};

pub use crate::pill::{Dispatch, DispatchCompletion, Pill, PillRow, PillStyle};
pub use crate::theme::Metrics;
pub use crate::window::BarWindowPlacement;

/// Everything derived from one bar configuration: the presentation the
/// projector reads, the geometry the widgets use, and the stylesheet.
///
/// Both GTK bars build this the same way so a config change lands in both, and
/// so neither can quietly disagree with the Cairo bars about a corner radius.
pub struct BarTheme {
    pub presentation: PresentationConfig,
    pub metrics: Metrics,
    pub pill_style: PillStyle,
    pub stylesheet: String,
    provider: RefCell<Option<(gdk4::Display, gtk4::CssProvider)>>,
}

impl BarTheme {
    /// `translucent` records whether the window can carry per-pixel alpha.
    /// Both material modes are installed once and selected by a live CSS class.
    #[must_use]
    pub fn from_config(config: &BarConfig, translucent: bool) -> Self {
        Self::from_config_at_scale(config, translucent, f64::from(primary_scale_factor()))
    }

    /// Derive a fresh set of widget metrics from the actual native surface's
    /// scale. Always start with the original config so successive monitor
    /// changes cannot divide an already converted physical thickness again.
    #[must_use]
    pub fn from_config_at_scale(config: &BarConfig, translucent: bool, scale: f64) -> Self {
        let mut presentation = config.presentation.clone();
        // Monochrome Nerd Font glyphs tinted by the text color read like macOS
        // template icons; only the stock emoji are replaced, so a config that
        // overrides individual labels keeps its customization. The Cairo bars
        // make exactly this substitution.
        if presentation.labels == PresentationLabels::default() {
            presentation.labels = PresentationLabels::nerd_font();
        }
        // JWM's reserved thickness is physical. GTK sizes widgets in logical
        // units; dividing before constructing metrics prevents HiDPI bars
        // from asking for the reserved area a second time.
        presentation.bar_height = presentation.logical_thickness(scale) as f32;
        if !presentation.edge.is_vertical() {
            // GTK includes the font's full ascent/descent in its minimum
            // request. A thin physical strip on a HiDPI output must fit the
            // actual font metrics, including configured icon fallbacks, or
            // GTK can enlarge the native window after a scale transition.
            let (font_size, font_height) = fit_font_height(
                &config.font,
                &presentation,
                (presentation.bar_height - 2.0).max(1.0),
            );
            // Keep the configured type size whenever it fits. Reduce spare
            // cross-axis padding before making thin-strip text smaller.
            presentation.font_size = font_size;
            presentation.vertical_padding = presentation
                .vertical_padding
                .max(0.0)
                .min(((presentation.bar_height - font_height - 2.0) * 0.5).max(0.0));
        }
        let metrics = Metrics::from_config(&presentation);
        let background_opacity = config.background_opacity;
        let stylesheet =
            theme::stylesheet(&presentation, &config.font, background_opacity, translucent);
        let pill_style = PillStyle {
            edge: presentation.edge,
            metrics,
            // Both themes use the same accent, so a progress line survives a
            // theme toggle without being rebuilt.
            accent: Palette::for_theme(ThemeMode::Dark).accent,
            font: pill_font(&config.font, presentation.font_size),
            network_icons: [
                presentation.labels.network.as_str(),
                presentation.labels.network_offline.as_str(),
            ]
            .map(bounded_icon),
            metric_icons: [
                NodeId::Cpu,
                NodeId::Memory,
                NodeId::Brightness,
                NodeId::Battery,
                NodeId::Audio,
            ]
            .map(|id| {
                let icons = presentation
                    .metric_icon_candidates(id)
                    .unwrap_or_default()
                    .into_iter()
                    .map(bounded_icon)
                    .collect();
                (id, icons)
            }),
        };

        Self {
            presentation,
            metrics,
            pill_style,
            stylesheet,
            provider: RefCell::new(None),
        }
    }

    /// Install this theme's stylesheet on the default display.
    pub fn install(&self) {
        if self.provider.borrow().is_some() {
            return;
        }
        let provider = gtk4::CssProvider::new();
        provider.load_from_string(&self.stylesheet);
        if let Some(display) = gdk4::Display::default() {
            gtk4::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
            *self.provider.borrow_mut() = Some((display, provider));
        }
    }
}

/// Style copies carry only projected-size glyphs, never a duplicate of an
/// unbounded user configuration for each retained widget.
fn bounded_icon(icon: &str) -> String {
    let mut end = icon
        .len()
        .min(xbar_core::controls::MAX_PROJECTED_CONTROL_TEXT_BYTES);
    while !icon.is_char_boundary(end) {
        end -= 1;
    }
    icon[..end].to_owned()
}

impl Drop for BarTheme {
    fn drop(&mut self) {
        if let Some((display, provider)) = self.provider.get_mut().take() {
            gtk4::style_context_remove_provider_for_display(&display, &provider);
        }
    }
}

/// The configured font at the size the Cairo renderer forces onto it, which is
/// the description every pill width is measured against.
fn pill_font(font: &str, font_size: f32) -> gtk4::pango::FontDescription {
    let mut description = gtk4::pango::FontDescription::from_string(font);
    description.set_absolute_size(f64::from(font_size.max(1.0)) * f64::from(gtk4::pango::SCALE));
    description
}

fn fit_font_height(font: &str, config: &PresentationConfig, available: f32) -> (f32, f32) {
    let labels = &config.labels;
    let mut samples = config.tag_labels.clone();
    samples.extend([
        "Ag 100% 23:59 界".to_owned(),
        labels.clock.clone(),
        labels.screenshot.clone(),
        labels.theme_dark.clone(),
        labels.theme_light.clone(),
        labels.monitor.clone(),
        labels.cpu.clone(),
        labels.memory.clone(),
        labels.audio.clone(),
        labels.muted.clone(),
        labels.brightness.clone(),
        labels.battery.clone(),
        labels.charging.clone(),
        labels.network.clone(),
        labels.network_offline.clone(),
        labels.media_playing.clone(),
        labels.media_paused.clone(),
    ]);
    samples.extend(labels.shell_routes.iter().cloned());
    let probe = gtk4::Label::new(None);
    let layout = probe.create_pango_layout(Some(&samples.join(" ").replace('\n', " ")));
    layout.set_single_paragraph_mode(true);
    let mut size = config.font_size.max(1.0);
    let mut height = 1.0;
    for _ in 0..12 {
        layout.set_font_description(Some(&pill_font(font, size)));
        let (ink, logical) = layout.pixel_extents();
        let top = i64::from(ink.y().min(logical.y()));
        let bottom = (i64::from(ink.y()) + i64::from(ink.height()))
            .max(i64::from(logical.y()) + i64::from(logical.height()));
        height = (bottom - top).max(1) as f32;
        if height <= available || size <= 1.0 {
            break;
        }
        size = (size * available / height).min(size - 0.5).max(1.0);
    }
    (size, height)
}

/// The bar's contents: tags and the layout controls on one side, the client
/// name in the middle, the status cluster against the far edge.
pub struct BarSurface {
    root: gtk4::Box,
    leading: PillRow,
    trailing: PillRow,
    client_name: Label,
    minimized_dock: dock::MinimizedDock,
    tag_contrast: TagContrastMode,
}

impl BarSurface {
    #[must_use]
    pub fn new(theme: &BarTheme, dispatch: Dispatch) -> Self {
        let vertical = theme.presentation.edge.is_vertical();
        let orientation = if vertical {
            Orientation::Vertical
        } else {
            Orientation::Horizontal
        };
        let leading = PillRow::new(theme.pill_style.clone(), dispatch.clone());
        let trailing = PillRow::new(theme.pill_style.clone(), dispatch.clone());
        if vertical {
            trailing.widget().set_valign(gtk4::Align::End);
        } else {
            trailing.widget().set_halign(gtk4::Align::End);
        }

        let client_name = Label::new(None);
        client_name.add_css_class("client-name");
        client_name.set_single_line_mode(!vertical);
        client_name.set_wrap(vertical);
        client_name.set_lines(if vertical { 3 } else { 1 });
        client_name.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        client_name.set_hexpand(!vertical);
        client_name.set_vexpand(vertical);
        client_name.set_max_width_chars(if vertical { 8 } else { -1 });
        client_name.set_halign(gtk4::Align::Center);
        client_name.set_valign(gtk4::Align::Center);

        let root = gtk4::Box::new(orientation, theme.metrics.item_gap);
        root.add_css_class("bar-root");
        root.add_css_class(theme.presentation.edge.as_str());
        root.add_css_class(if vertical { "vertical" } else { "horizontal" });
        let minimized_dock = dock::MinimizedDock::new(&root, theme, dispatch);
        root.append(&scrollable_controls(
            leading.widget(),
            vertical,
            theme.metrics,
        ));
        root.append(&client_name);
        root.append(minimized_dock.widget());
        root.append(&scrollable_controls(
            trailing.widget(),
            vertical,
            theme.metrics,
        ));

        Self {
            root,
            leading,
            trailing,
            client_name,
            minimized_dock,
            tag_contrast: theme.presentation.tag_contrast,
        }
    }

    #[must_use]
    pub const fn widget(&self) -> &gtk4::Box {
        &self.root
    }

    /// Show one projection of the model.
    ///
    /// Which cells exist, their order, their glyphs and their state are all
    /// decided by `xbar_core`; this only spends that decision on widgets.
    pub fn sync(&self, snapshot: &BarSnapshot, presentation: BarPresentation) {
        if snapshot.blur_status_bar.unwrap_or(false) {
            self.root.add_css_class("frosted");
        } else {
            self.root.remove_css_class("frosted");
        }
        self.root.remove_css_class("backdrop-dark");
        self.root.remove_css_class("backdrop-light");
        self.root.add_css_class(
            match xbar_core::background::wallpaper_theme(
                snapshot.theme,
                snapshot.wallpaper_luminance,
            ) {
                ThemeMode::Dark => "backdrop-dark",
                ThemeMode::Light => "backdrop-light",
            },
        );
        let palette = TagPalette::for_wallpaper(
            snapshot.theme,
            snapshot.wallpaper_luminance,
            self.tag_contrast,
        );
        let dark = TagPalette::for_wallpaper(snapshot.theme, None, TagContrastMode::Dark);
        self.root.remove_css_class("tags-dark");
        self.root.remove_css_class("tags-light");
        self.root.add_css_class(if palette.empty == dark.empty {
            "tags-dark"
        } else {
            "tags-light"
        });
        let BarPresentation {
            tags,
            layout_button,
            layout_choices,
            client_name,
            minimized_windows,
            minimized_overflow,
            status,
            ..
        } = presentation;

        let mut leading = tags;
        leading.push(layout_button);
        leading.extend(layout_choices);
        self.leading.sync(&leading);

        // The projector orders status cells right to left, anchored at the
        // right edge; a horizontal box packs them the other way around.
        let mut trailing = status;
        trailing.reverse();
        self.trailing.sync(&trailing);

        // The label stays in the tree even with nothing to say: it is the gap
        // that holds the status cluster against the right edge, which is where
        // the layout engine anchors it.
        self.client_name.set_text(
            client_name
                .as_ref()
                .map_or("", |client| client.value.as_str()),
        );
        self.client_name
            .set_tooltip_text(client_name.as_ref().map(|client| client.value.as_str()));

        self.minimized_dock
            .sync(snapshot, &minimized_windows, minimized_overflow);
    }

    /// Service the minimized-window geometry/preview lease. Calling this from
    /// a fast native event loop is cheap; the Dock limits traffic itself.
    pub fn maintain(&self) {
        self.minimized_dock.maintain();
    }

    /// Release the old Dock's anchors before a scale change replaces widgets.
    pub fn retire(&self) {
        self.minimized_dock.retire();
    }

    #[must_use]
    pub fn focused_control(&self) -> Option<xbar_core::presentation::NodeId> {
        self.leading
            .focused_control()
            .or_else(|| self.trailing.focused_control())
            .or_else(|| {
                self.minimized_dock
                    .focused_window()
                    .map(xbar_core::presentation::NodeId::MinimizedWindow)
            })
    }

    /// Preserve keyboard position across an actual-output scale change.
    pub fn restore_focus(&self, control: Option<xbar_core::presentation::NodeId>) {
        let Some(control) = control else {
            return;
        };
        if self.leading.focus_control(control) || self.trailing.focus_control(control) {
            return;
        }
        if let xbar_core::presentation::NodeId::MinimizedWindow(window) = control {
            self.minimized_dock.focus_window(window);
        }
    }
}

/// A control group gives up its long-axis allocation before it makes the
/// native window larger than its output. Focus navigation scrolls controls
/// into view, and overlay scrollbars preserve a narrow sidebar's width.
fn scrollable_controls(
    child: &gtk4::Box,
    vertical: bool,
    metrics: Metrics,
) -> gtk4::ScrolledWindow {
    let scroll = gtk4::ScrolledWindow::new();
    scroll.set_child(Some(child));
    scroll.set_overlay_scrolling(true);
    scroll.set_kinetic_scrolling(true);
    if vertical {
        scroll.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        scroll.set_propagate_natural_height(true);
        scroll.set_min_content_height(metrics.pill_height.ceil().max(1.0) as i32);
        scroll.set_hexpand(true);
    } else {
        scroll.set_policy(gtk4::PolicyType::Automatic, gtk4::PolicyType::Never);
        scroll.set_propagate_natural_width(true);
        scroll.set_min_content_width(metrics.pill_height.ceil().max(1.0) as i32);
        scroll.set_vexpand(true);
    }
    scroll
}

/// Requested GTK dimensions for a raw output. GTK4/Wayland leaves positioning
/// to its compositor; the same edge-aware dimensions are used on both paths.
#[must_use]
pub fn output_logical_size(config: &PresentationConfig, scale: i32) -> (i32, i32) {
    let (width, height) = primary_screen_size();
    let geometry = xbar_core::MonitorGeometry {
        x: 0,
        y: 0,
        width,
        height,
    };
    let scale = f64::from(scale.max(1));
    let placement = xbar_core::BarPlacement::at_edge(
        geometry,
        config.logical_thickness(scale),
        scale,
        config.edge,
    )
    .expect("validated bar thickness and GTK scale");
    logical_placement_size(placement, scale)
}

/// JWM snapshots already contain the bar origin and vertical width.
#[must_use]
pub fn monitor_logical_size(
    geometry: xbar_core::MonitorGeometry,
    config: &PresentationConfig,
    scale: i32,
) -> (i32, i32) {
    let scale = f64::from(scale.max(1));
    let placement = xbar_core::BarPlacement::from_bar_geometry(
        geometry,
        config.logical_thickness(scale),
        scale,
        config.edge,
    )
    .expect("validated bar thickness and GTK scale");
    logical_placement_size(placement, scale)
}

fn logical_placement_size(placement: xbar_core::BarPlacement, scale: f64) -> (i32, i32) {
    let logical = |physical| {
        (f64::from(physical) / scale)
            .round()
            .clamp(1.0, f64::from(i32::MAX)) as i32
    };
    (logical(placement.width), logical(placement.height))
}

/// Put the bar's theme class on its window, replacing whichever was there.
pub fn apply_theme_class(window: &impl IsA<gtk4::Widget>, theme: ThemeMode) {
    let window = window.as_ref();
    window.remove_css_class("theme-dark");
    window.remove_css_class("theme-light");
    window.add_css_class(match theme {
        ThemeMode::Dark => "theme-dark",
        ThemeMode::Light => "theme-light",
    });
}

/// Physical size of the primary monitor, which is the width a bar spans until
/// the window manager hands it real geometry.
#[must_use]
pub fn primary_screen_size() -> (u32, u32) {
    let fallback = (1920, 1080);
    let Some(monitor) = primary_monitor() else {
        return fallback;
    };
    let geometry = monitor.geometry();
    let scale = monitor.scale_factor().max(1);
    (
        (geometry.width() * scale).max(1) as u32,
        (geometry.height() * scale).max(1) as u32,
    )
}

/// A physical screen width in the logical units GTK sizes windows in.
#[must_use]
pub fn logical_width(physical_width: u32) -> i32 {
    let scale = primary_scale_factor();
    (f64::from(physical_width) / f64::from(scale))
        .round()
        .clamp(1.0, f64::from(i32::MAX)) as i32
}

#[must_use]
pub fn primary_scale_factor() -> i32 {
    primary_monitor().map_or(1, |monitor| monitor.scale_factor().max(1))
}

fn primary_monitor() -> Option<gdk4::Monitor> {
    gdk4::Display::default()
        .and_then(|display| display.monitors().item(0))
        .and_then(|object| object.downcast::<gdk4::Monitor>().ok())
}
