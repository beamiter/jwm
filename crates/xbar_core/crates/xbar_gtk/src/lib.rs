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

use gtk4::prelude::*;
use gtk4::{Label, Orientation};
use xbar_core::config::BarConfig;
use xbar_core::controls::BarPresentation;
use xbar_core::glass::DEFAULT_BACKGROUND_OPACITY;
use xbar_core::presentation::{Palette, PresentationConfig, PresentationLabels};
use xbar_core::{BarSnapshot, ThemeMode};

pub use crate::pill::{Dispatch, Pill, PillRow, PillStyle};
pub use crate::theme::Metrics;

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
}

impl BarTheme {
    /// `translucent` is the frontend's startup mode decision — compositor
    /// present and an RGBA surface available — and it is baked into the
    /// stylesheet here because `install()` only ever adds providers: a theme
    /// that could change mode would have to stack rules, not replace them.
    #[must_use]
    pub fn from_config(config: &BarConfig, translucent: bool) -> Self {
        let mut presentation = config.presentation.clone();
        // Monochrome Nerd Font glyphs tinted by the text color read like macOS
        // template icons; only the stock emoji are replaced, so a config that
        // overrides individual labels keeps its customization. The Cairo bars
        // make exactly this substitution.
        if presentation.labels == PresentationLabels::default() {
            presentation.labels = PresentationLabels::nerd_font();
        }
        let metrics = Metrics::from_config(&presentation);
        let background_opacity = config
            .background_opacity
            .unwrap_or(DEFAULT_BACKGROUND_OPACITY);
        let stylesheet =
            theme::stylesheet(&presentation, &config.font, background_opacity, translucent);
        let pill_style = PillStyle {
            metrics,
            // Both themes use the same accent, so a progress line survives a
            // theme toggle without being rebuilt.
            accent: Palette::for_theme(ThemeMode::Dark).accent,
            font: pill_font(&config.font, presentation.font_size),
        };

        Self {
            presentation,
            metrics,
            pill_style,
            stylesheet,
        }
    }

    /// Install this theme's stylesheet on the default display.
    pub fn install(&self) {
        let provider = gtk4::CssProvider::new();
        provider.load_from_string(&self.stylesheet);
        if let Some(display) = gdk4::Display::default() {
            gtk4::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
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

/// The bar's contents: tags and the layout controls on one side, the client
/// name in the middle, the status cluster against the far edge.
pub struct BarSurface {
    root: gtk4::Box,
    leading: PillRow,
    trailing: PillRow,
    client_name: Label,
    minimized_dock: dock::MinimizedDock,
}

impl BarSurface {
    #[must_use]
    pub fn new(theme: &BarTheme, dispatch: Dispatch) -> Self {
        let leading = PillRow::new(theme.pill_style.clone(), dispatch.clone());
        let trailing = PillRow::new(theme.pill_style.clone(), dispatch.clone());
        trailing.widget().set_halign(gtk4::Align::End);

        let client_name = Label::new(None);
        client_name.add_css_class("client-name");
        client_name.set_single_line_mode(true);
        client_name.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        client_name.set_hexpand(true);
        client_name.set_halign(gtk4::Align::Center);

        let root = gtk4::Box::new(Orientation::Horizontal, theme.metrics.item_gap);
        root.add_css_class("bar-root");
        let minimized_dock = dock::MinimizedDock::new(&root, theme, dispatch);
        root.append(leading.widget());
        root.append(&client_name);
        root.append(minimized_dock.widget());
        root.append(trailing.widget());

        Self {
            root,
            leading,
            trailing,
            client_name,
            minimized_dock,
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

        self.minimized_dock
            .sync(snapshot, &minimized_windows, minimized_overflow);
    }

    /// Service the minimized-window geometry/preview lease. Calling this from
    /// a fast native event loop is cheap; the Dock limits traffic itself.
    pub fn maintain(&self) {
        self.minimized_dock.maintain();
    }
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
    let scale = primary_monitor().map_or(1, |monitor| monitor.scale_factor().max(1));
    (f64::from(physical_width) / f64::from(scale))
        .round()
        .clamp(1.0, f64::from(i32::MAX)) as i32
}

fn primary_monitor() -> Option<gdk4::Monitor> {
    gdk4::Display::default()
        .and_then(|display| display.monitors().item(0))
        .and_then(|object| object.downcast::<gdk4::Monitor>().ok())
}
