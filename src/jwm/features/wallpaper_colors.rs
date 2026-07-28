//! Accent colours taken from the wallpaper.
//!
//! DMS and Noctalia both retint their shell from the picture behind it; this
//! module does the same for the colours JWM already draws with — the focused
//! border, the two gradient stops, and the client glow, which are also what
//! the launcher ring, the toasts and the OSD progress bars read.
//!
//! The extraction is a pure function over pixels, so the awkward parts — a
//! photo that is nine tenths grey sky, a screenshot that is nearly black, a
//! monochrome wallpaper that must not be given an invented hue — are settled
//! in unit tests rather than by squinting at a running session. Only decoding
//! the file touches the world, and that runs on a worker thread: a 4K JPEG
//! costs a few hundred milliseconds, which is several dropped frames if it
//! happens on the event loop.

use std::path::Path;

/// Longest edge the wallpaper is scaled to before its colours are counted.
///
/// Colour proportions survive downscaling and nothing here needs detail, so
/// this is purely about how long the decode-and-count takes.
pub const SAMPLE_EDGE: u32 = 160;

/// Below this saturation an image is treated as monochrome and keeps its
/// neutrality: a black-and-white photograph quantises to hues that are pure
/// rounding noise, and turning that noise into a red or green accent looks
/// like a bug, not a theme.
const NEUTRAL_SATURATION: f32 = 0.06;

/// How far apart in hue the second colour must be to count as a different
/// colour rather than a shade of the first, in degrees.
const SECONDARY_HUE_GAP: f32 = 25.0;

/// Rotation applied to invent a second colour when the wallpaper only really
/// has one, in degrees.
const SECONDARY_HUE_SHIFT: f32 = 45.0;

/// Two accents drawn from one picture.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    /// The wallpaper's dominant colour, used wherever a single accent is
    /// drawn: the focused border, the glow, the first gradient stop.
    pub accent: [f32; 3],
    /// A second colour for the far end of the gradient, from the wallpaper
    /// when it has one and derived from the accent when it does not.
    pub secondary: [f32; 3],
}

/// The colours a retheme replaces, as they currently stand.
///
/// Each keeps its own alpha: the glow is deliberately translucent and the
/// border deliberately opaque, and that is a taste the wallpaper has no
/// business overriding.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CurrentColors {
    pub border_focused: [f32; 4],
    pub gradient_a: [f32; 4],
    pub gradient_b: [f32; 4],
    pub glow: [f32; 4],
}

/// Configuration keys a retheme writes, in the order [`color_changes`]
/// reports them.
pub const THEMED_KEYS: [&str; 4] = [
    "behavior.border_color_focused",
    "behavior.border_gradient_color_a",
    "behavior.border_gradient_color_b",
    "behavior.border_glow_color",
];

/// The configuration changes that put `palette` on screen.
///
/// Returns only what actually differs, and nothing at all when the colours
/// are already right: every applied change re-arranges monitors and redraws
/// decorations, which is not a price to pay for storing the same numbers.
#[must_use]
pub fn color_changes(
    palette: &Palette,
    current: &CurrentColors,
) -> Vec<(String, serde_json::Value)> {
    let wanted = [
        (
            THEMED_KEYS[0],
            with_alpha(palette.accent, current.border_focused[3]),
            current.border_focused,
        ),
        (
            THEMED_KEYS[1],
            with_alpha(palette.accent, current.gradient_a[3]),
            current.gradient_a,
        ),
        (
            THEMED_KEYS[2],
            with_alpha(palette.secondary, current.gradient_b[3]),
            current.gradient_b,
        ),
        (
            THEMED_KEYS[3],
            with_alpha(palette.accent, current.glow[3]),
            current.glow,
        ),
    ];
    wanted
        .into_iter()
        .filter(|(_, new, old)| !same_color(*new, *old))
        .map(|(key, new, _)| {
            (
                key.to_string(),
                serde_json::json!([new[0], new[1], new[2], new[3]]),
            )
        })
        .collect()
}

fn with_alpha(rgb: [f32; 3], alpha: f32) -> [f32; 4] {
    [rgb[0], rgb[1], rgb[2], alpha]
}

fn same_color(a: [f32; 4], b: [f32; 4]) -> bool {
    a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-4)
}

/// Extract a palette from tightly packed RGBA8 pixels.
///
/// Returns `None` when the image holds no usable colour — an all-black
/// splash, a fully transparent PNG — in which case the caller keeps whatever
/// colours are configured rather than falling back to something arbitrary.
#[must_use]
pub fn palette_from_rgba(rgba: &[u8]) -> Option<Palette> {
    let mut buckets: std::collections::HashMap<u16, Bucket> = std::collections::HashMap::new();
    for pixel in rgba.chunks_exact(4) {
        // Anything see-through is a corner or a soft edge, not a colour the
        // wallpaper actually shows.
        if pixel[3] < 128 {
            continue;
        }
        let rgb = [
            f32::from(pixel[0]) / 255.0,
            f32::from(pixel[1]) / 255.0,
            f32::from(pixel[2]) / 255.0,
        ];
        let [_, saturation, lightness] = rgb_to_hsl(rgb);
        let weight = pixel_weight(saturation, lightness);
        if weight <= 0.0 {
            continue;
        }
        // Four bits per channel: fine enough to keep a sunset apart from the
        // sand under it, coarse enough that a gradient sky counts as one
        // colour instead of ten thousand.
        let key = (u16::from(pixel[0] >> 4) << 8)
            | (u16::from(pixel[1] >> 4) << 4)
            | u16::from(pixel[2] >> 4);
        let bucket = buckets.entry(key).or_default();
        bucket.weight += f64::from(weight);
        for (sum, channel) in bucket.sums.iter_mut().zip(rgb) {
            *sum += f64::from(channel) * f64::from(weight);
        }
    }

    let mut ranked: Vec<(u16, Bucket)> = buckets.into_iter().collect();
    // The key breaks ties so the same wallpaper always themes the same way;
    // hash order alone would not.
    ranked.sort_by(|(left_key, left), (right_key, right)| {
        right
            .weight
            .partial_cmp(&left.weight)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(left_key.cmp(right_key))
    });
    let (_, dominant) = ranked.first()?;
    let accent = normalize(dominant.mean());

    let accent_hue = rgb_to_hsl(accent)[0];
    let secondary = ranked
        .iter()
        .skip(1)
        .map(|(_, bucket)| bucket.mean())
        .find(|rgb| {
            let [hue, saturation, _] = rgb_to_hsl(*rgb);
            saturation >= NEUTRAL_SATURATION && hue_distance(hue, accent_hue) >= SECONDARY_HUE_GAP
        })
        .map_or_else(|| shifted(accent), normalize);

    Some(Palette { accent, secondary })
}

/// Extract a palette from an image file.
///
/// Decoding and scaling are the expensive part; callers run this off the
/// event loop.
#[must_use]
pub fn palette_from_file(path: &Path) -> Option<Palette> {
    let image = match image::open(path) {
        Ok(image) => image,
        Err(error) => {
            log::warn!("wallpaper colors: {}: {error}", path.display());
            return None;
        }
    };
    let sample = image.thumbnail(SAMPLE_EDGE, SAMPLE_EDGE).to_rgba8();
    palette_from_rgba(sample.as_raw())
}

/// Start an extraction on a worker thread.
#[must_use]
pub fn start_extraction(
    path: std::path::PathBuf,
) -> super::connectivity::BackgroundJob<Option<Palette>> {
    super::connectivity::BackgroundJob::spawn(move || palette_from_file(&path))
}

/// Weighted colour sums for one quantised bucket.
#[derive(Debug, Default, Clone, Copy)]
struct Bucket {
    sums: [f64; 3],
    weight: f64,
}

impl Bucket {
    fn mean(self) -> [f32; 3] {
        if self.weight <= 0.0 {
            return [0.0; 3];
        }
        [
            (self.sums[0] / self.weight) as f32,
            (self.sums[1] / self.weight) as f32,
            (self.sums[2] / self.weight) as f32,
        ]
    }
}

/// How much one pixel counts towards its bucket.
///
/// Coloured pixels count for much more than grey ones, because the sky or the
/// snow is usually the largest area and almost never the colour a person
/// would name the picture by. Near-black and near-white pixels carry no
/// usable hue at all and are dropped outright.
fn pixel_weight(saturation: f32, lightness: f32) -> f32 {
    if !(0.08..=0.95).contains(&lightness) {
        return 0.0;
    }
    (0.04 + saturation * 2.2) * (1.0 - (lightness - 0.5).abs() * 1.2)
}

/// Pull a sampled colour into the range that reads as an accent: bright
/// enough to see against a dark panel, dark enough to see a white label on.
/// Monochrome input stays monochrome.
fn normalize(rgb: [f32; 3]) -> [f32; 3] {
    let [hue, saturation, lightness] = rgb_to_hsl(rgb);
    if saturation < NEUTRAL_SATURATION {
        return hsl_to_rgb([hue, 0.0, lightness.clamp(0.55, 0.75)]);
    }
    hsl_to_rgb([
        hue,
        saturation.clamp(0.45, 0.95),
        lightness.clamp(0.45, 0.68),
    ])
}

/// The accent rotated around the colour wheel, for wallpapers that only have
/// one colour worth using.
fn shifted(accent: [f32; 3]) -> [f32; 3] {
    let [hue, saturation, lightness] = rgb_to_hsl(accent);
    if saturation < NEUTRAL_SATURATION {
        // A grey has no hue to rotate. Separating the gradient stops by
        // lightness keeps the gradient a gradient without inventing a colour
        // the wallpaper does not have.
        let lightness = if lightness > 0.62 {
            lightness - 0.18
        } else {
            lightness + 0.18
        };
        return hsl_to_rgb([hue, 0.0, lightness]);
    }
    normalize(hsl_to_rgb([
        (hue + SECONDARY_HUE_SHIFT) % 360.0,
        saturation,
        lightness,
    ]))
}

/// Shortest distance between two hues in degrees, 0..=180.
#[must_use]
pub fn hue_distance(a: f32, b: f32) -> f32 {
    let raw = (a - b).abs() % 360.0;
    raw.min(360.0 - raw)
}

/// RGB in 0..=1 to hue (degrees), saturation, lightness.
#[must_use]
pub fn rgb_to_hsl(rgb: [f32; 3]) -> [f32; 3] {
    let [red, green, blue] = rgb;
    let max = red.max(green).max(blue);
    let min = red.min(green).min(blue);
    let lightness = f32::midpoint(max, min);
    let span = max - min;
    if span <= f32::EPSILON {
        return [0.0, 0.0, lightness];
    }
    let saturation = if lightness > 0.5 {
        span / (2.0 - max - min)
    } else {
        span / (max + min)
    };
    let hue = if max == red {
        (green - blue) / span + if green < blue { 6.0 } else { 0.0 }
    } else if max == green {
        (blue - red) / span + 2.0
    } else {
        (red - green) / span + 4.0
    };
    [hue * 60.0, saturation, lightness]
}

/// Hue (degrees), saturation, lightness back to RGB in 0..=1.
#[must_use]
pub fn hsl_to_rgb(hsl: [f32; 3]) -> [f32; 3] {
    let [hue, saturation, lightness] = hsl;
    if saturation <= f32::EPSILON {
        return [lightness, lightness, lightness];
    }
    let q = if lightness < 0.5 {
        lightness * (1.0 + saturation)
    } else {
        lightness + saturation - lightness * saturation
    };
    let p = 2.0 * lightness - q;
    let hue = hue.rem_euclid(360.0) / 360.0;
    [
        hue_channel(p, q, hue + 1.0 / 3.0),
        hue_channel(p, q, hue),
        hue_channel(p, q, hue - 1.0 / 3.0),
    ]
}

fn hue_channel(p: f32, q: f32, t: f32) -> f32 {
    let t = t.rem_euclid(1.0);
    if t < 1.0 / 6.0 {
        p + (q - p) * 6.0 * t
    } else if t < 0.5 {
        q
    } else if t < 2.0 / 3.0 {
        p + (q - p) * (2.0 / 3.0 - t) * 6.0
    } else {
        p
    }
}

/// `#rrggbb` for a colour, for consumers that speak CSS — the status bar and
/// anything else subscribing to the theme.
#[must_use]
pub fn hex(rgb: [f32; 3]) -> String {
    let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!(
        "#{:02x}{:02x}{:02x}",
        channel(rgb[0]),
        channel(rgb[1]),
        channel(rgb[2])
    )
}

impl crate::jwm::Jwm {
    /// Start extracting colours when the wallpaper has changed and the
    /// feature is on. Called after every config apply, which is also where a
    /// wallpaper change lands.
    pub(crate) fn refresh_wallpaper_theme(&mut self) {
        let (enabled, wallpaper) = {
            let cfg = crate::config::CONFIG.load();
            let behavior = cfg.behavior();
            (behavior.wallpaper_colors, behavior.wallpaper.clone())
        };
        if !enabled {
            // Nothing is put back: the colours in memory stay until a reload
            // reads the file again. Undoing a retheme would mean remembering
            // what the user had, which the config file already does.
            self.features.wallpaper_theme = None;
            self.features.themed_wallpaper.clear();
            self.features.wallpaper_palette = None;
            return;
        }
        if wallpaper.trim().is_empty() || self.features.themed_wallpaper == wallpaper {
            return;
        }
        let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/"));
        let path = super::wallpaper::expand_home(&wallpaper, &home);
        // Claimed before the work starts, so the config apply this extraction
        // ends in does not start another one for the same picture.
        self.features.themed_wallpaper = wallpaper;
        self.features.wallpaper_theme = Some(start_extraction(path));
    }

    /// Adopt a finished extraction. Called from the frame tick.
    pub(crate) fn poll_wallpaper_theme(&mut self, backend: &mut dyn crate::backend::api::Backend) {
        let Some(result) = self
            .features
            .wallpaper_theme
            .as_ref()
            .and_then(super::connectivity::BackgroundJob::take)
        else {
            return;
        };
        self.features.wallpaper_theme = None;
        let Some(palette) = result else {
            log::info!(
                "wallpaper colors: {} has no colour to take; keeping the configured palette",
                self.features.themed_wallpaper
            );
            return;
        };
        let adopted = (self.features.themed_wallpaper.clone(), palette);
        let unchanged = self.features.wallpaper_palette.as_ref() == Some(&adopted);
        self.features.wallpaper_palette = Some(adopted);

        let changes = color_changes(&palette, &current_colors());
        if !changes.is_empty() {
            let mut updated = (**crate::config::CONFIG.load()).clone();
            if let Err(error) = updated.set_values(&changes) {
                log::warn!("wallpaper colors: {error}");
                return;
            }
            crate::config::CONFIG.store(std::sync::Arc::new(updated));
            self.apply_config_changes(backend);
        }
        if !unchanged {
            log::info!(
                "wallpaper colors: {} \u{2192} {} / {}",
                self.features.themed_wallpaper,
                hex(palette.accent),
                hex(palette.secondary)
            );
            let payload = self.wallpaper_theme_json();
            self.broadcast_ipc_event("theme/colors", payload);
        }
    }

    /// The palette in use, for `get_wallpaper_colors`.
    pub(crate) fn wallpaper_theme_json(&self) -> serde_json::Value {
        let taken = self.features.wallpaper_palette.as_ref();
        serde_json::json!({
            "enabled": crate::config::CONFIG.load().behavior().wallpaper_colors,
            // The wallpaper these colours came from, which is not always the
            // one on screen: one with no colour to take leaves them alone.
            "wallpaper": taken.map(|(wallpaper, _)| wallpaper.clone()),
            "accent": taken.map(|(_, palette)| hex(palette.accent)),
            "secondary": taken.map(|(_, palette)| hex(palette.secondary)),
            "pending": self.features.wallpaper_theme.is_some(),
        })
    }
}

/// The themed colours as they currently stand in the live config.
fn current_colors() -> CurrentColors {
    let cfg = crate::config::CONFIG.load();
    let behavior = cfg.behavior();
    CurrentColors {
        border_focused: behavior.border_color_focused,
        gradient_a: behavior.border_gradient_color_a,
        gradient_b: behavior.border_gradient_color_b,
        glow: behavior.border_glow_color,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an RGBA buffer from (count, [r, g, b]) runs.
    fn pixels(runs: &[(usize, [u8; 3])]) -> Vec<u8> {
        let mut buffer = Vec::new();
        for (count, [red, green, blue]) in runs {
            for _ in 0..*count {
                buffer.extend_from_slice(&[*red, *green, *blue, 255]);
            }
        }
        buffer
    }

    #[test]
    fn hsl_round_trips() {
        for rgb in [
            [0.9, 0.2, 0.1],
            [0.1, 0.7, 0.4],
            [0.2, 0.3, 0.95],
            [0.5, 0.5, 0.5],
            [0.0, 0.0, 0.0],
            [1.0, 1.0, 1.0],
        ] {
            let back = hsl_to_rgb(rgb_to_hsl(rgb));
            for (original, returned) in rgb.iter().zip(back) {
                assert!(
                    (original - returned).abs() < 1e-3,
                    "{rgb:?} came back as {back:?}"
                );
            }
        }
    }

    #[test]
    fn colours_are_published_as_css_hex() {
        assert_eq!(hex([0.0, 0.0, 0.0]), "#000000");
        assert_eq!(hex([1.0, 1.0, 1.0]), "#ffffff");
        assert_eq!(hex([1.0, 0.5, 0.0]), "#ff8000");
        // Out-of-range input clamps rather than wrapping to a wrong colour.
        assert_eq!(hex([1.4, -0.2, 0.5]), "#ff0080");
    }

    #[test]
    fn hue_distance_wraps_around_the_wheel() {
        assert!((hue_distance(10.0, 350.0) - 20.0).abs() < 1e-3);
        assert!((hue_distance(350.0, 10.0) - 20.0).abs() < 1e-3);
        assert!((hue_distance(0.0, 180.0) - 180.0).abs() < 1e-3);
    }

    #[test]
    fn a_small_saturated_area_beats_a_large_grey_one() {
        // The usual photograph: a huge flat sky, a little colour that is what
        // anyone would call the picture by.
        let buffer = pixels(&[(9000, [130, 132, 135]), (1000, [220, 90, 20])]);
        let palette = palette_from_rgba(&buffer).expect("palette");
        let [hue, saturation, _] = rgb_to_hsl(palette.accent);
        assert!(saturation > 0.4, "grey won: {:?}", palette.accent);
        assert!(hue_distance(hue, 22.0) < 25.0, "not the orange: {hue}");
    }

    #[test]
    fn a_monochrome_wallpaper_is_not_given_a_hue() {
        let buffer = pixels(&[(500, [80, 80, 80]), (500, [180, 180, 180])]);
        let palette = palette_from_rgba(&buffer).expect("palette");
        for colour in [palette.accent, palette.secondary] {
            assert!(
                rgb_to_hsl(colour)[1] < 0.02,
                "invented a hue: {colour:?} \u{2192} {:?}",
                rgb_to_hsl(colour)
            );
        }
        // Still a gradient, separated by lightness rather than by hue.
        let gap = (rgb_to_hsl(palette.accent)[2] - rgb_to_hsl(palette.secondary)[2]).abs();
        assert!(gap > 0.1, "the grey gradient has no gradient: {gap}");
    }

    #[test]
    fn a_second_colour_is_taken_from_the_wallpaper_when_it_has_one() {
        let buffer = pixels(&[(2000, [200, 40, 40]), (1500, [40, 60, 200])]);
        let palette = palette_from_rgba(&buffer).expect("palette");
        let accent_hue = rgb_to_hsl(palette.accent)[0];
        let secondary_hue = rgb_to_hsl(palette.secondary)[0];
        assert!(hue_distance(accent_hue, 0.0) < 20.0, "accent {accent_hue}");
        assert!(
            hue_distance(secondary_hue, 228.0) < 25.0,
            "secondary {secondary_hue}"
        );
    }

    #[test]
    fn a_single_hue_wallpaper_still_gets_a_gradient() {
        let buffer = pixels(&[(3000, [40, 90, 200])]);
        let palette = palette_from_rgba(&buffer).expect("palette");
        let gap = hue_distance(
            rgb_to_hsl(palette.accent)[0],
            rgb_to_hsl(palette.secondary)[0],
        );
        assert!(gap > 20.0, "the two gradient stops are the same: {gap}");
    }

    #[test]
    fn an_image_with_no_usable_colour_yields_nothing() {
        assert!(palette_from_rgba(&pixels(&[(1000, [0, 0, 0])])).is_none());
        assert!(palette_from_rgba(&pixels(&[(1000, [255, 255, 255])])).is_none());
        assert!(palette_from_rgba(&[]).is_none());
        // Fully transparent pixels are not colour either.
        assert!(palette_from_rgba(&[200, 40, 40, 0, 200, 40, 40, 0]).is_none());
    }

    #[test]
    fn the_same_pixels_always_theme_the_same_way() {
        let buffer = pixels(&[
            (500, [200, 40, 40]),
            (500, [40, 200, 60]),
            (500, [40, 60, 200]),
        ]);
        let first = palette_from_rgba(&buffer).expect("palette");
        for _ in 0..8 {
            assert_eq!(palette_from_rgba(&buffer).expect("palette"), first);
        }
    }

    #[test]
    fn accents_land_in_a_visible_range() {
        // Nearly black and nearly white wallpapers must still produce a
        // border that can be seen against both a dark and a light panel.
        for run in [[18, 22, 40], [230, 236, 246]] {
            let palette = palette_from_rgba(&pixels(&[(2000, run)])).expect("palette");
            let lightness = rgb_to_hsl(palette.accent)[2];
            assert!(
                (0.4..=0.8).contains(&lightness),
                "{run:?} gave lightness {lightness}"
            );
        }
    }

    fn current() -> CurrentColors {
        CurrentColors {
            border_focused: [0.4, 0.6, 0.9, 1.0],
            gradient_a: [0.24, 0.65, 1.0, 1.0],
            gradient_b: [0.72, 0.35, 1.0, 1.0],
            glow: [0.0, 0.55, 1.0, 0.38],
        }
    }

    #[test]
    fn every_themed_colour_keeps_its_own_alpha() {
        let palette = Palette {
            accent: [0.8, 0.3, 0.2],
            secondary: [0.2, 0.4, 0.8],
        };
        let changes = color_changes(&palette, &current());
        assert_eq!(changes.len(), THEMED_KEYS.len());
        for (key, value) in &changes {
            let alpha = value[3].as_f64().expect("alpha") as f32;
            let expected = match key.as_str() {
                "behavior.border_glow_color" => 0.38,
                _ => 1.0,
            };
            assert!((alpha - expected).abs() < 1e-4, "{key} alpha {alpha}");
        }
    }

    #[test]
    fn the_gradient_ends_take_different_colours() {
        let palette = Palette {
            accent: [0.8, 0.3, 0.2],
            secondary: [0.2, 0.4, 0.8],
        };
        let changes = color_changes(&palette, &current());
        let find = |key: &str| {
            changes
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
                .expect("key")
        };
        assert_ne!(
            find("behavior.border_gradient_color_a"),
            find("behavior.border_gradient_color_b")
        );
        assert_eq!(
            find("behavior.border_gradient_color_a"),
            find("behavior.border_color_focused")
        );
    }

    #[test]
    fn colours_that_are_already_right_are_not_written_again() {
        let current = current();
        let palette = Palette {
            accent: [
                current.gradient_a[0],
                current.gradient_a[1],
                current.gradient_a[2],
            ],
            secondary: [
                current.gradient_b[0],
                current.gradient_b[1],
                current.gradient_b[2],
            ],
        };
        // The focused border and the glow differ from the accent here, so
        // only those two are rewritten.
        let changes = color_changes(&palette, &current);
        let keys: Vec<&str> = changes.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "behavior.border_color_focused",
                "behavior.border_glow_color"
            ]
        );

        let settled = CurrentColors {
            border_focused: [0.24, 0.65, 1.0, 1.0],
            glow: [0.24, 0.65, 1.0, 0.38],
            ..current
        };
        assert!(color_changes(&palette, &settled).is_empty());
    }
}
