//! The bar's stylesheet, generated from the shared presentation config.
//!
//! Every number here also drives the Cairo bars through
//! [`xbar_core::presentation`]: pill height, corner radius, padding, font size
//! and the macOS material's colors. Generating the CSS instead of shipping a
//! hand-written file is what keeps this bar looking like `xcb_bar` when the
//! shared config changes — a static stylesheet would silently drift.

use gtk4::glib::translate::IntoGlib;
use gtk4::pango::{FontDescription, Style};
use xbar_core::ThemeMode;
use xbar_core::presentation::{Palette, PresentationConfig, Rgba};

/// CSS color literal for a renderer-neutral color.
fn css(color: Rgba) -> String {
    let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!(
        "rgba({},{},{},{:.4})",
        channel(color.red),
        channel(color.green),
        channel(color.blue),
        color.alpha.clamp(0.0, 1.0)
    )
}

/// The same color with its alpha replaced, used for the bar's background tint.
fn with_alpha(color: Rgba, alpha: f64) -> Rgba {
    Rgba::new(
        color.red,
        color.green,
        color.blue,
        alpha.clamp(0.0, 1.0) as f32,
    )
}

/// Geometry shared by the stylesheet and the widget tree.
#[derive(Debug, Clone, Copy)]
pub struct Metrics {
    /// Total bar height in logical pixels.
    pub bar_height: i32,
    /// Height of a pill, matching `bar_height - 2 * vertical_padding`.
    pub pill_height: f32,
    pub corner_radius: f32,
    pub item_gap: i32,
}

impl Metrics {
    #[must_use]
    pub fn from_config(config: &PresentationConfig) -> Self {
        let bar_height = config.bar_height.max(1.0);
        let vertical_padding = config.vertical_padding.max(0.0).min(bar_height * 0.5);
        let pill_height = (bar_height - 2.0 * vertical_padding).max(1.0);
        Self {
            bar_height: bar_height.round() as i32,
            pill_height,
            corner_radius: config.corner_radius.max(0.0).min(pill_height * 0.5),
            item_gap: config.item_gap.max(0.0).round() as i32,
        }
    }
}

/// Font family, weight and slant from the Pango description the Cairo bars
/// render with. The size deliberately comes from `presentation.font_size`
/// instead: that is the size `CairoRenderer` forces onto the description.
fn font_rules(font: &str, font_size: f32) -> String {
    let description = FontDescription::from_string(font);
    let family = description
        .family()
        .map(|family| family.to_string())
        .filter(|family| !family.is_empty())
        .unwrap_or_else(|| "sans-serif".to_owned());
    let weight = description.weight().into_glib().clamp(100, 1000);
    let style = match description.style() {
        Style::Italic => "italic",
        Style::Oblique => "oblique",
        _ => "normal",
    };
    format!(
        "  font-family: \"{family}\", sans-serif;\n  font-size: {font_size}px;\n  font-weight: {weight};\n  font-style: {style};\n"
    )
}

/// Rules that differ per theme. Fill precedence follows `LayoutEngine`:
/// urgent, then selected, then hover, then occupied — so those selectors are
/// emitted in that order and the cascade resolves them the same way.
fn theme_rules(theme: ThemeMode, background_opacity: f64, translucent: bool) -> String {
    let palette = Palette::for_theme(theme);
    let scope = match theme {
        ThemeMode::Dark => "window.theme-dark",
        ThemeMode::Light => "window.theme-light",
    };
    // Translucent bars tint the bar with the configured opacity and let the
    // compositor supply what shows through; solid bars paint the palette
    // background wall to wall at alpha 1.0, so a configured opacity can never
    // leak into an uncomposited window. The palette background is the same
    // color the Cairo bars fall back to, which keeps every bar's solid mode
    // byte-identical.
    let alpha = if translucent { background_opacity } else { 1.0 };
    let background = css(with_alpha(palette.background, alpha));
    // In solid mode the window itself carries the theme color: the theme
    // class scopes make a runtime theme toggle recolor the whole strip
    // without regenerating the stylesheet.
    let window_background = if translucent {
        String::new()
    } else {
        format!("{scope} {{ background: {background}; }}\n")
    };

    format!(
        "\
{window_background}{scope} .bar-root {{ background-color: {background}; }}
{scope} .pill {{ background-color: {pill}; color: {text}; }}
{scope} .pill.occupied {{ background-color: {occupied}; }}
{scope} .pill:hover {{ background-color: {hovered}; }}
{scope} .pill.selected,
{scope} .pill.selected:hover {{ background-color: {selected}; color: {selected_text}; }}
{scope} .pill.urgent,
{scope} .pill.urgent:hover {{
  background-color: {urgent};
  border-color: {urgent_stroke};
  color: {selected_text};
}}
{scope} .client-name {{ color: {muted}; }}
",
        pill = css(palette.pill),
        occupied = css(palette.occupied),
        hovered = css(palette.hovered),
        selected = css(palette.selected),
        urgent = css(palette.urgent),
        urgent_stroke = css(palette.urgent_stroke),
        text = css(palette.text),
        selected_text = css(palette.selected_text),
        muted = css(palette.muted_text),
    )
}

/// The complete stylesheet for one bar configuration and one window mode.
///
/// The mode is baked in at generation: `install()` only ever adds providers,
/// so regenerating for a mode change would stack rules instead of replacing
/// them — and the mode is a startup decision anyway.
#[must_use]
pub fn stylesheet(
    config: &PresentationConfig,
    font: &str,
    background_opacity: f64,
    translucent: bool,
) -> String {
    let metrics = Metrics::from_config(config);
    let horizontal_padding = config.horizontal_padding.max(0.0);
    let vertical_padding = ((config.bar_height.max(1.0) - metrics.pill_height) * 0.5).max(0.0);
    // A pill carries a 1px border at all times so the urgent stroke can appear
    // without changing its box, which means padding and height are both stated
    // one pixel short per side: the pill's outer box then matches the rect the
    // layout engine hands the same control.
    let pill_padding = (config.pill_horizontal_padding.max(0.0) - 1.0).max(0.0);
    let inner_height = (metrics.pill_height - 2.0).max(1.0);

    // Translucent windows must not carry GTK's default opaque background —
    // per-pixel alpha is what lets the compositor blend behind the bar. Solid
    // windows keep the background: the per-theme scopes above paint it.
    let window_background = if translucent {
        "background: transparent;\n  "
    } else {
        ""
    };

    format!(
        "\
/* Generated by gtk_bar::theme — edit the shared xbar config, not this text. */

window,
window.transparent-window,
window.theme-light,
window.theme-dark {{
  {window_background}background-image: none;
  border: none;
  box-shadow: none;
}}

/* The bar is a full-bleed strip, exactly like the background node the Cairo
 * renderer paints; its color comes from the theme scopes below. */
.bar-root {{
  padding: {vertical_padding}px {horizontal_padding}px;
}}

.pill {{
  min-width: 0;
  min-height: {inner_height}px;
  padding: 0 {pill_padding}px;
  border: 1px solid transparent;
  border-radius: {corner_radius}px;
  background-image: none;
  box-shadow: none;
  outline: none;
  text-shadow: none;
{font}}}

/* GTK's stock button feedback would add a shade and an inset shadow the Cairo
 * bars never draw: a press changes nothing but which pill is selected. */
.pill:active,
.pill:checked,
.pill:focus,
.pill:focus-visible {{
  box-shadow: none;
  outline: none;
}}

.client-name {{
{font}}}

{dark}
{light}",
        font = font_rules(font, config.font_size.max(1.0)),
        corner_radius = metrics.corner_radius,
        dark = theme_rules(ThemeMode::Dark, background_opacity, translucent),
        light = theme_rules(ThemeMode::Light, background_opacity, translucent),
    )
}
