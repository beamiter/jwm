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
use xbar_core::presentation::{DockMetrics, Palette, PresentationConfig, Rgba};

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
    /// Total short-axis thickness in logical pixels.
    pub bar_height: i32,
    /// Height of a pill, matching `bar_height - 2 * vertical_padding`.
    pub pill_height: f32,
    pub corner_radius: f32,
    pub item_gap: i32,
    /// Resting compositor thumbnail dimensions.
    pub dock_item_width: i32,
    pub dock_item_height: i32,
    /// A fixed peak-width slot prevents horizontal layout movement while the
    /// pointer crosses the Dock. Vertical magnification may breathe into the
    /// bar's padding, just like the renderer-neutral scene.
    pub dock_slot_width: i32,
    pub dock_slot_height: i32,
    pub dock_item_gap: i32,
    pub dock_separator_width: i32,
    pub dock_hover_scale: f32,
}

impl Metrics {
    #[must_use]
    pub fn from_config(config: &PresentationConfig) -> Self {
        let bar_height = config.bar_height.max(1.0);
        let vertical_padding = config.vertical_padding.max(0.0).min(bar_height * 0.5);
        let pill_height = (bar_height - 2.0 * vertical_padding).max(1.0);
        // The widget content receives the bar after root padding. Reuse
        // the renderer's short-axis cap so both horizontal HiDPI strips and
        // vertical sidebars keep magnified cards inside that allocation.
        let dock = DockMetrics::new(config, pill_height);
        let dock_item_width = dock
            .map_or(1.0, |metrics| metrics.item_width)
            .floor()
            .max(1.0);
        let dock_item_height = dock
            .map_or(1.0, |metrics| metrics.item_height)
            .floor()
            .max(1.0);
        let dock_hover_scale = dock.map_or(1.0, |metrics| metrics.peak_scale);
        let dock_slot_extent = dock.map_or(1.0, |metrics| metrics.slot_extent);
        Self {
            bar_height: bar_height.round() as i32,
            pill_height,
            corner_radius: config.corner_radius.max(0.0).min(pill_height * 0.5),
            item_gap: config.item_gap.max(0.0).round() as i32,
            dock_item_width: dock_item_width.round().max(1.0) as i32,
            dock_item_height: dock_item_height.round().max(1.0) as i32,
            dock_slot_width: if config.edge.is_vertical() {
                (dock_item_width * dock_hover_scale).ceil()
            } else {
                dock_slot_extent.ceil()
            }
            .max(1.0) as i32,
            dock_slot_height: if config.edge.is_vertical() {
                dock_slot_extent.ceil()
            } else {
                (dock_item_height * dock_hover_scale).ceil()
            }
            .max(1.0) as i32,
            dock_hover_scale,
            dock_item_gap: config.dock_item_gap.max(0.0).round() as i32,
            dock_separator_width: config.dock_separator_width.max(1.0).round() as i32,
        }
    }
}

/// Font family, weight and slant from the Pango description the Cairo bars
/// render with. The size deliberately comes from `presentation.font_size`
/// instead: that is the size `CairoRenderer` forces onto the description.
pub(crate) fn font_rules(font: &str, font_size: f32) -> String {
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
fn theme_rules(theme: ThemeMode, background_opacity: Option<f64>, translucent: bool) -> String {
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
    let alpha =
        xbar_core::background::bar_background_opacity(None, background_opacity, translucent);
    let frosted_alpha =
        xbar_core::background::bar_background_opacity(Some(true), background_opacity, translucent);
    let background = css(with_alpha(palette.background, alpha));
    let frosted_background = css(with_alpha(palette.background, frosted_alpha));
    let mut backdrop_text = String::new();
    for (mode, opacity) in [(":not(.frosted)", alpha), (".frosted", frosted_alpha)] {
        for (class, luminance) in [("backdrop-dark", 0), ("backdrop-light", 255)] {
            let ink = Palette::for_background(theme, Some(luminance), opacity);
            backdrop_text.push_str(&format!(
                "{scope} .bar-root{mode}.{class} .pill:not(.tag):not(.selected):not(.urgent) {{ color: {text}; }}\n\
                 {scope} .bar-root{mode}.{class} .client-name {{ color: {muted}; }}\n",
                text = css(ink.text),
                muted = css(ink.muted_text),
            ));
        }
    }
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
{scope} .bar-root.frosted {{ background-color: {frosted_background}; }}
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
{backdrop_text}
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

/// Generate tag styles from the shared policy, keeping CSS free of contrast math.
fn tag_rules(theme: ThemeMode) -> String {
    use xbar_core::tag_contrast::{TagContrastMode, TagPalette};
    let scope = match theme {
        ThemeMode::Dark => "window.theme-dark",
        ThemeMode::Light => "window.theme-light",
    };
    let mut result = String::new();
    for (class, mode) in [
        ("tags-dark", TagContrastMode::Dark),
        ("tags-light", TagContrastMode::Light),
    ] {
        let palette = TagPalette::for_wallpaper(theme, None, mode);
        for (state, colors) in [
            (":not(.selected):not(.urgent)", palette.empty),
            (".occupied:not(.selected):not(.urgent)", palette.occupied),
            (":hover:not(.selected):not(.urgent)", palette.hovered),
            (".selected:not(.urgent)", palette.selected),
            (".urgent", palette.urgent),
        ] {
            let border = colors
                .stroke
                .map_or_else(|| "transparent".to_owned(), |stroke| css(stroke.color));
            // Keep GTK box sizing stable: only the inset part of a thicker
            // stroke changes, never the one-pixel layout border.
            let inset = colors
                .stroke
                .map_or(0.0, |stroke| (stroke.width - 1.0).max(0.0));
            result.push_str(&format!("{scope} .bar-root.{class} .pill.tag{state} {{ background-color: {}; color: {}; border: 1px solid {border}; box-shadow: inset 0 0 0 {inset}px {border}; text-shadow: none; }}\n", css(colors.fill), css(colors.foreground)));
        }
    }
    result
}

/// The complete stylesheet for one bar configuration and one window mode.
///
/// Alpha capability is fixed at startup. Both material modes share this one
/// provider, so hot reload changes only the root class and cannot stack styles.
#[must_use]
pub fn stylesheet(
    config: &PresentationConfig,
    font: &str,
    background_opacity: Option<f64>,
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
    let dock_radius = config
        .dock_corner_radius
        .max(0.0)
        .min(config.dock_item_size.max(1.0) * 0.5);
    let dock_padding = config.dock_shelf_padding.max(0.0);
    let dock_hover_scale = metrics.dock_hover_scale;

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
.bar-root.desktop-header {{ padding: 0; background: transparent; }}
.bar-root.vertical {{
  padding: {horizontal_padding}px {vertical_padding}px;
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
.pill:focus-visible,
.minimized-card:focus-visible {{
  outline: 2px solid {focus_color};
  outline-offset: -2px;
}}
.bar-root.vertical .pill {{
  padding: 2px 1px;
}}

.client-name {{
{font}}}

/* Fixed peak-size slots give GTK's retained widgets the same stable Dock
 * geometry as the renderer-neutral scene. The compositor supplies the real
 * cached thumbnail at the resting rectangle; this card is its fallback and
 * its pointer target. */
.dock-shelf {{
  padding: 0 {dock_padding}px;
  background: transparent;
}}
.bar-root.vertical .dock-shelf {{
  padding: {dock_padding}px 0;
}}

.dock-separator {{
  opacity: 0.48;
}}

.dock-slot,
.dock-empty-slot {{
  padding: 0;
}}

.minimized-card {{
  min-width: {dock_item_width}px;
  min-height: {dock_item_height}px;
  padding: 0;
  border: 1px solid rgba(255,255,255,0.22);
  border-radius: {dock_radius}px;
  background-image: linear-gradient(to bottom, rgba(255,255,255,0.22), rgba(255,255,255,0.08));
  box-shadow: 0 2px 5px rgba(0,0,0,0.28);
  transform: scale(1);
  transition: 180ms cubic-bezier(0.20, 0.78, 0.22, 1.08);
}}

.minimized-card.magnified {{
  transform: scale({dock_hover_scale});
}}

.minimized-card.urgent {{
  border-color: rgba(255,99,88,0.96);
  box-shadow: 0 0 0 1px rgba(255,99,88,0.42), 0 3px 8px rgba(0,0,0,0.32);
}}

.minimized-card.unavailable {{
  opacity: 0.52;
}}

.dock-overflow {{
{font}}}

{dark}
{light}
{dark_tags}
{light_tags}
window.theme-dark .bar-root.desktop-header, window.theme-light .bar-root.desktop-header {{ background: transparent; padding: 0; }}",
        font = font_rules(font, config.font_size.max(1.0)),
        focus_color = css(Palette::for_theme(ThemeMode::Dark).accent),
        corner_radius = metrics.corner_radius,
        dock_item_width = (metrics.dock_item_width - 2).max(1),
        dock_item_height = (metrics.dock_item_height - 2).max(1),
        dock_radius = dock_radius,
        dock_padding = dock_padding,
        dock_hover_scale = dock_hover_scale,
        dark = theme_rules(ThemeMode::Dark, background_opacity, translucent),
        light = theme_rules(ThemeMode::Light, background_opacity, translucent),
        dark_tags = tag_rules(ThemeMode::Dark),
        light_tags = tag_rules(ThemeMode::Light),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dock_slots_and_peak_cards_fit_small_logical_thickness_on_every_edge() {
        for edge in [
            xbar_core::BarEdge::Top,
            xbar_core::BarEdge::Bottom,
            xbar_core::BarEdge::Left,
            xbar_core::BarEdge::Right,
        ] {
            for thickness in [12.0, 18.0, 21.0, 42.0, 64.0] {
                let config = PresentationConfig {
                    edge,
                    bar_height: thickness,
                    ..PresentationConfig::default()
                };
                let metrics = Metrics::from_config(&config);
                let (card_cross, slot_cross) = if edge.is_vertical() {
                    (metrics.dock_item_width, metrics.dock_slot_width)
                } else {
                    (metrics.dock_item_height, metrics.dock_slot_height)
                };
                assert!(
                    card_cross as f32 * metrics.dock_hover_scale <= metrics.pill_height,
                    "{edge} at {thickness}"
                );
                assert!(
                    slot_cross as f32 <= metrics.pill_height.ceil(),
                    "{edge} at {thickness}"
                );
            }
        }
    }

    #[test]
    fn adaptive_tags_keep_fixed_geometry_and_urgent_precedence() {
        for theme in [ThemeMode::Dark, ThemeMode::Light] {
            let rules = tag_rules(theme);
            assert_eq!(rules.matches("border: 1px solid").count(), 10);
            assert!(rules.contains("box-shadow: inset 0 0 0 1px"));
            assert!(!rules.contains("border: 2px"));
            assert!(!rules.contains(".bar-root {"));
            assert!(rules.contains(".pill.tag.selected:not(.urgent)"));
            assert!(rules.contains(".pill.tag.urgent"));
        }
    }

    #[test]
    fn root_material_classes_preserve_override_and_opaque_fallback() {
        for theme in [ThemeMode::Dark, ThemeMode::Light] {
            let clear = css(with_alpha(Palette::for_theme(theme).background, 0.0));
            let frost = css(with_alpha(Palette::for_theme(theme).background, 0.55));
            let rules = theme_rules(theme, None, true);
            assert!(rules.contains(&format!(".bar-root {{ background-color: {clear}; }}")));
            assert!(rules.contains(&format!(
                ".bar-root.frosted {{ background-color: {frost}; }}"
            )));
            for (translucent, expected) in [(true, 0.3), (false, 1.0)] {
                let background = css(with_alpha(Palette::for_theme(theme).background, expected));
                let rules = theme_rules(theme, Some(0.3), translucent);
                assert!(
                    rules.contains(&format!(".bar-root {{ background-color: {background}; }}"))
                );
                assert!(rules.contains(&format!(
                    ".bar-root.frosted {{ background-color: {background}; }}"
                )));
            }
        }
    }
}
