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
    // A fixed 12-bit histogram avoids per-pixel hashing and heap allocations.
    let mut buckets = [Bucket::default(); 4096];
    for pixel in rgba.chunks_exact(4) {
        // Low-coverage corners and soft edges are not representative. The
        // retained pixels contribute in proportion to their visible coverage.
        if pixel[3] < 128 {
            continue;
        }
        let rgb = [
            f32::from(pixel[0]) / 255.0,
            f32::from(pixel[1]) / 255.0,
            f32::from(pixel[2]) / 255.0,
        ];
        let [_, saturation, lightness] = rgb_to_hsl(rgb);
        let weight = pixel_weight(saturation, lightness) * (f32::from(pixel[3]) / 255.0);
        if weight <= 0.0 {
            continue;
        }
        // Four bits per channel: fine enough to keep a sunset apart from the
        // sand under it, coarse enough that a gradient sky counts as one
        // colour instead of ten thousand.
        let key = (u16::from(pixel[0] >> 4) << 8)
            | (u16::from(pixel[1] >> 4) << 4)
            | u16::from(pixel[2] >> 4);
        let bucket = &mut buckets[usize::from(key)];
        bucket.weight += f64::from(weight);
        for (sum, channel) in bucket.sums.iter_mut().zip(rgb) {
            *sum += f64::from(channel) * f64::from(weight);
        }
    }

    // Only two winners are needed. Ascending-key scans with strict weight
    // comparisons retain the old sort's deterministic lower-key tie break,
    // without allocating or sorting every occupied bucket.
    let mut dominant = None;
    for (key, bucket) in buckets.iter().enumerate() {
        if bucket.weight > 0.0
            && dominant.is_none_or(|(_, best): (usize, &Bucket)| bucket.weight > best.weight)
        {
            dominant = Some((key, bucket));
        }
    }
    let (dominant_key, dominant) = dominant?;
    let accent = normalize(dominant.mean());
    let accent_hue = rgb_to_hsl(accent)[0];
    let mut secondary = None;
    for (key, bucket) in buckets.iter().enumerate() {
        if key == dominant_key
            || bucket.weight <= 0.0
            || secondary.is_some_and(|(best, _): (f64, [f32; 3])| bucket.weight <= best)
        {
            continue;
        }
        let rgb = bucket.mean();
        let [hue, saturation, _] = rgb_to_hsl(rgb);
        if saturation >= NEUTRAL_SATURATION && hue_distance(hue, accent_hue) >= SECONDARY_HUE_GAP {
            secondary = Some((bucket.weight, rgb));
        }
    }
    let secondary = secondary.map_or_else(|| shifted(accent), |(_, rgb)| normalize(rgb));

    Some(Palette { accent, secondary })
}

/// Extract a palette from an image file.
///
/// Decoding and scaling are the expensive part; callers run this off the
/// event loop.
#[must_use]
pub fn palette_from_file(path: &Path) -> Option<Palette> {
    let image = match crate::backend::compositor_common::image_source::open_regular_image(path) {
        Ok(image) => image,
        Err(error) => {
            log::warn!("wallpaper colors: {}: {error}", path.display());
            return None;
        }
    };
    let sample = thumbnail_for_palette(image, SAMPLE_EDGE);
    palette_from_rgba(sample.as_raw())
}

/// Average visible colour contributions without mixing in hidden RGB from
/// transparent pixels. Straight-alpha thumbnailing averages those hidden
/// channels too, inventing hues even when all visible pixels share one hue.
fn thumbnail_for_palette(image: image::DynamicImage, edge: u32) -> image::RgbaImage {
    if !image.color().has_alpha() {
        return image.thumbnail(edge, edge).into_rgba8();
    }
    let mut sample = match image {
        image::DynamicImage::ImageRgba8(rgba) => premultiplied_thumbnail(&rgba, edge),
        image::DynamicImage::ImageLumaA8(luma) => premultiplied_thumbnail(&luma, edge),
        other => premultiplied_thumbnail(&other.into_rgba8(), edge),
    };
    for pixel in sample.pixels_mut() {
        let alpha = u16::from(pixel[3]);
        if alpha == 0 {
            continue;
        }
        for channel in &mut pixel.0[..3] {
            *channel = ((u16::from(*channel) * 255 + alpha / 2) / alpha).min(255) as u8;
        }
    }
    sample
}

/// Supply premultiplied pixels as the thumbnail reads them, avoiding a separate
/// full-image write pass and a full RGBA conversion for grayscale-alpha images.
struct PremultipliedView<'a, I>(&'a I);

impl<I> image::GenericImageView for PremultipliedView<'_, I>
where
    I: image::GenericImageView,
    I::Pixel: image::Pixel<Subpixel = u8>,
{
    type Pixel = image::Rgba<u8>;

    fn dimensions(&self) -> (u32, u32) {
        self.0.dimensions()
    }

    fn get_pixel(&self, x: u32, y: u32) -> Self::Pixel {
        use image::Pixel;
        let mut pixel = self.0.get_pixel(x, y).to_rgba();
        let alpha = u16::from(pixel[3]);
        for channel in &mut pixel.0[..3] {
            *channel = ((u16::from(*channel) * alpha + 127) / 255) as u8;
        }
        pixel
    }
}

fn premultiplied_thumbnail<I>(image: &I, edge: u32) -> image::RgbaImage
where
    I: image::GenericImageView,
    I::Pixel: image::Pixel<Subpixel = u8>,
{
    let (width, height) = image.dimensions();
    // Match DynamicImage::thumbnail's aspect-preserving rounded dimensions.
    let ratio = (f64::from(edge) / f64::from(width)).min(f64::from(edge) / f64::from(height));
    let target_w = ((f64::from(width) * ratio).round() as u32).max(1);
    let target_h = ((f64::from(height) * ratio).round() as u32).max(1);
    image::imageops::thumbnail(&PremultipliedView(image), target_w, target_h)
}

/// Start an extraction on a worker thread.
#[must_use]
pub fn start_extraction(
    path: std::path::PathBuf,
) -> super::connectivity::BackgroundJob<Option<Palette>> {
    super::connectivity::BackgroundJob::spawn(move || palette_from_file(&path))
}

/// Put an extraction just started for `wallpaper` into the theme slot.
///
/// `job` is `None` when the OS refused the worker a thread. Such a handle
/// never yields a value, so keeping it would report `pending` for the rest of
/// the session, and claiming the wallpaper would make every later config
/// apply skip it as already themed. Both are left empty instead — which also
/// drops any extraction still running for the previous picture, whose colours
/// no longer belong on screen — so the next config apply tries again.
fn claim_extraction(
    features: &mut super::FeatureStates,
    wallpaper: String,
    job: Option<super::connectivity::BackgroundJob<Option<Palette>>>,
) {
    match job {
        Some(job) => {
            features.themed_wallpaper = wallpaper;
            features.wallpaper_theme = Some(job);
        }
        None => {
            features.themed_wallpaper.clear();
            features.wallpaper_theme = None;
        }
    }
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

fn theme_wallpaper_path(
    behavior: &crate::config::BehaviorConfig,
    day: i32,
    primary: Option<(usize, u32)>,
) -> &str {
    let (idx, tags) = primary.unwrap_or((0, 0));
    crate::backend::compositor_common::wallpaper::resolve_wallpaper_for_tag_at(
        behavior,
        if behavior.wallpaper_rotation == "daily" && !behavior.wallpaper_rotation_paths.is_empty() {
            0
        } else {
            idx as u32
        },
        tags,
        day,
    )
    .0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThemeReconcile {
    Unchanged,
    Started,
    Cancelled,
    Cleared,
}

impl crate::jwm::Jwm {
    fn primary_wallpaper_monitor(&self) -> Option<(usize, u32)> {
        self.state
            .monitor_order
            .iter()
            .enumerate()
            .find_map(|(idx, key)| {
                self.state
                    .monitors
                    .get(*key)
                    .map(|monitor| (idx, monitor.get_active_tags()))
            })
    }

    fn current_wallpaper_theme_source(&self) -> (bool, String) {
        let cfg = crate::config::CONFIG.load();
        let behavior = cfg.behavior();
        let day = if behavior.wallpaper_rotation == "daily" {
            self.features
                .wallpaper_rotation
                .observed_day()
                .unwrap_or_else(super::wallpaper_rotation::local_day)
        } else {
            0
        };
        (
            behavior.wallpaper_colors,
            theme_wallpaper_path(behavior, day, self.primary_wallpaper_monitor()).to_string(),
        )
    }

    /// Start extracting colours when the wallpaper has changed and the
    /// feature is on. Called after every config apply, which is also where a
    /// wallpaper change lands.
    pub(crate) fn refresh_wallpaper_theme(&mut self) {
        let (enabled, wallpaper) = self.current_wallpaper_theme_source();
        let outcome = self.refresh_wallpaper_theme_for(enabled, wallpaper);
        self.publish_wallpaper_reconcile(outcome);
    }

    fn publish_wallpaper_reconcile(&mut self, outcome: ThemeReconcile) {
        if outcome != ThemeReconcile::Unchanged {
            let payload = self.wallpaper_theme_json();
            self.broadcast_ipc_event("theme/colors", payload);
        }
    }

    /// Re-read the current image after an explicit library refresh or file reload.
    /// Invalidating the request identity is cheap: all file access and decoding
    /// remain on the extraction worker. Unrelated config applies keep their cache.
    pub(crate) fn force_refresh_wallpaper_theme(&mut self) {
        self.invalidate_wallpaper_theme();
        self.refresh_wallpaper_theme();
    }

    pub(crate) fn invalidate_wallpaper_theme(&mut self) {
        let outcome = self.invalidate_wallpaper_theme_request();
        self.publish_wallpaper_reconcile(outcome);
    }

    fn invalidate_wallpaper_theme_request(&mut self) -> ThemeReconcile {
        // Drop the old receiver before replacing the request so late results
        // cannot overwrite the refreshed image's palette. Keep applied colours.
        let was_pending = self.features.wallpaper_theme.take().is_some();
        self.features.themed_wallpaper.clear();
        if was_pending {
            ThemeReconcile::Cancelled
        } else {
            ThemeReconcile::Unchanged
        }
    }

    /// Reconcile the pending extraction with the current wallpaper choice.
    fn refresh_wallpaper_theme_for(&mut self, enabled: bool, wallpaper: String) -> ThemeReconcile {
        let had_request =
            self.features.wallpaper_theme.is_some() || !self.features.themed_wallpaper.is_empty();
        if !enabled {
            let changed = had_request || self.features.wallpaper_palette.is_some();
            // Nothing is put back: the colours in memory stay until a reload
            // reads the file again. Undoing a retheme would mean remembering
            // what the user had, which the config file already does.
            self.features.wallpaper_theme = None;
            self.features.themed_wallpaper.clear();
            self.features.wallpaper_palette = None;
            return if changed {
                ThemeReconcile::Cleared
            } else {
                ThemeReconcile::Unchanged
            };
        }
        if wallpaper.trim().is_empty() {
            // Clearing the wallpaper is also a change of source. The old
            // worker may still finish, but its colours no longer belong to
            // the current choice. Keep the last applied palette just as for
            // an image with no usable colour, without adopting late work.
            self.features.wallpaper_theme = None;
            self.features.themed_wallpaper.clear();
            return if had_request {
                ThemeReconcile::Cancelled
            } else {
                ThemeReconcile::Unchanged
            };
        }
        if self.features.themed_wallpaper == wallpaper {
            return ThemeReconcile::Unchanged;
        }
        let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/"));
        let path = super::wallpaper::expand_home(&wallpaper, &home);
        self.adopt_extraction(wallpaper, start_extraction(path));
        if self.features.wallpaper_theme.is_some() {
            ThemeReconcile::Started
        } else if had_request {
            ThemeReconcile::Cancelled
        } else {
            ThemeReconcile::Unchanged
        }
    }

    /// Put an extraction just started for `wallpaper` into the theme slot.
    ///
    /// Claimed before the work finishes, so the config apply this extraction
    /// ends in does not start another one for the same picture. A worker the
    /// OS refused is not claimed at all; see `claim_extraction`.
    fn adopt_extraction(
        &mut self,
        wallpaper: String,
        job: super::connectivity::BackgroundJob<Option<Palette>>,
    ) {
        let job = job.started().then(|| self.track_background_job(job));
        claim_extraction(&mut self.features, wallpaper, job);
    }

    /// Adopt a finished extraction. Called from the frame tick.
    pub(crate) fn poll_wallpaper_theme(&mut self, backend: &mut dyn crate::backend::api::Backend) {
        if self.features.wallpaper_theme.is_none() {
            return;
        }
        let (enabled, wallpaper) = self.current_wallpaper_theme_source();
        self.poll_wallpaper_theme_for(backend, enabled, wallpaper);
    }

    fn poll_wallpaper_theme_for(
        &mut self,
        backend: &mut dyn crate::backend::api::Backend,
        enabled: bool,
        wallpaper: String,
    ) {
        // Output/tag changes can land before their refresh hook or while a
        // codec is finishing. Never adopt colours for a superseded source.
        if !enabled || self.features.themed_wallpaper != wallpaper {
            let outcome = self.refresh_wallpaper_theme_for(enabled, wallpaper);
            self.publish_wallpaper_reconcile(outcome);
            return;
        }
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
            // A file can appear or be repaired at the same path. Leave it
            // eligible for the next config apply; no apply is triggered here,
            // so a missing file cannot create an extraction/retheme loop.
            self.features.themed_wallpaper.clear();
            let payload = self.wallpaper_theme_json();
            self.broadcast_ipc_event("theme/colors", payload);
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
        }
        // An explicit refresh may return the same colours. Subscribers still
        // need completion to retire pending rather than wait for a change.
        let payload = self.wallpaper_theme_json();
        self.broadcast_ipc_event("theme/colors", payload);
    }

    /// The palette in use, for `get_wallpaper_colors`.
    pub(crate) fn wallpaper_theme_json(&self) -> serde_json::Value {
        let cfg = crate::config::CONFIG.load();
        self.wallpaper_theme_json_for(cfg.behavior(), super::wallpaper_rotation::local_day())
    }

    fn wallpaper_theme_json_for(
        &self,
        behavior: &crate::config::BehaviorConfig,
        day: i32,
    ) -> serde_json::Value {
        let taken = self.features.wallpaper_palette.as_ref();
        // These are resolved image requests, not proof that a backend decoded
        // them successfully. Keep the historical palette source separate.
        let current_wallpapers: Vec<_> = self
            .state
            .monitor_order
            .iter()
            .enumerate()
            .filter_map(|(idx, key)| self.state.monitors.get(*key).map(|monitor| (idx, monitor)))
            .enumerate()
            .map(|(slot, (idx, monitor))| {
                let (path, mode) =
                    crate::backend::compositor_common::wallpaper::resolve_wallpaper_for_tag_at(
                        behavior,
                        if behavior.wallpaper_rotation == "daily" {
                            slot as u32
                        } else {
                            idx as u32
                        },
                        monitor.get_active_tags(),
                        day,
                    );
                serde_json::json!({"monitor": idx, "path": path, "mode": mode})
            })
            .collect();
        let fallback = crate::backend::compositor_common::wallpaper::resolve_wallpaper_for_tag_at(
            behavior, 0, 0, day,
        )
        .0;
        let current_wallpaper = current_wallpapers
            .first()
            .and_then(|entry| entry["path"].as_str())
            .unwrap_or(fallback);
        let image_enabled = if current_wallpapers.is_empty() {
            !fallback.trim().is_empty()
        } else {
            current_wallpapers.iter().any(|entry| {
                entry["path"]
                    .as_str()
                    .is_some_and(|path| !path.trim().is_empty())
            })
        };
        serde_json::json!({
            // Legacy enabled means accent extraction, including for get_wallpaper.
            "enabled": behavior.wallpaper_colors,
            "colors_enabled": behavior.wallpaper_colors,
            "image_enabled": image_enabled,
            "rotation": behavior.wallpaper_rotation,
            "current_wallpaper": current_wallpaper,
            "current_wallpapers": current_wallpapers,
            "source_wallpaper": theme_wallpaper_path(behavior, day, self.primary_wallpaper_monitor()),
            "source_monitor": self.primary_wallpaper_monitor().map(|(idx, _)| idx),
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
    use crate::jwm::features::connectivity::BackgroundJob;
    use crate::jwm::features::monitor_lock::test_support::LockSpyBackend;

    fn sorted_reference_palette(rgba: &[u8]) -> Option<Palette> {
        // A fixed 12-bit histogram avoids per-pixel hashing and heap allocations.
        let mut buckets = [Bucket::default(); 4096];
        for pixel in rgba.chunks_exact(4) {
            // Low-coverage corners and soft edges are not representative. The
            // retained pixels contribute in proportion to their visible coverage.
            if pixel[3] < 128 {
                continue;
            }
            let rgb = [
                f32::from(pixel[0]) / 255.0,
                f32::from(pixel[1]) / 255.0,
                f32::from(pixel[2]) / 255.0,
            ];
            let [_, saturation, lightness] = rgb_to_hsl(rgb);
            let weight = pixel_weight(saturation, lightness) * (f32::from(pixel[3]) / 255.0);
            if weight <= 0.0 {
                continue;
            }
            // Four bits per channel: fine enough to keep a sunset apart from the
            // sand under it, coarse enough that a gradient sky counts as one
            // colour instead of ten thousand.
            let key = (u16::from(pixel[0] >> 4) << 8)
                | (u16::from(pixel[1] >> 4) << 4)
                | u16::from(pixel[2] >> 4);
            let bucket = &mut buckets[usize::from(key)];
            bucket.weight += f64::from(weight);
            for (sum, channel) in bucket.sums.iter_mut().zip(rgb) {
                *sum += f64::from(channel) * f64::from(weight);
            }
        }

        // Sort only the occupied bins. Key ordering makes ties deterministic.
        let mut ranked: Vec<(u16, Bucket)> = buckets
            .into_iter()
            .enumerate()
            .filter(|(_, bucket)| bucket.weight > 0.0)
            .map(|(key, bucket)| (key as u16, bucket))
            .collect();
        ranked.sort_unstable_by(|(left_key, left), (right_key, right)| {
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
                saturation >= NEUTRAL_SATURATION
                    && hue_distance(hue, accent_hue) >= SECONDARY_HUE_GAP
            })
            .map_or_else(|| shifted(accent), normalize);

        Some(Palette { accent, secondary })
    }

    fn buffered_reference_thumbnail(image: image::DynamicImage, edge: u32) -> image::RgbaImage {
        if !image.color().has_alpha() {
            return image.thumbnail(edge, edge).into_rgba8();
        }
        let mut rgba = image.into_rgba8();
        for pixel in rgba.pixels_mut() {
            let alpha = u16::from(pixel[3]);
            for channel in &mut pixel.0[..3] {
                *channel = ((u16::from(*channel) * alpha + 127) / 255) as u8;
            }
        }
        let mut sample = image::DynamicImage::ImageRgba8(rgba)
            .thumbnail(edge, edge)
            .into_rgba8();
        for pixel in sample.pixels_mut() {
            let alpha = u16::from(pixel[3]);
            if alpha == 0 {
                continue;
            }
            for channel in &mut pixel.0[..3] {
                *channel = ((u16::from(*channel) * 255 + alpha / 2) / alpha).min(255) as u8;
            }
        }
        sample
    }

    #[test]
    fn linear_palette_selection_exactly_matches_sorted_reference() {
        let mut seed = 42_u32;
        for size in [0, 1, 4, 64, 1024, 160 * 160] {
            for variation in 0..12 {
                let mut rgba = Vec::with_capacity(size * 4);
                for _ in 0..size {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    let mut pixel = seed.to_le_bytes();
                    if variation % 3 == 0 {
                        pixel[3] = 255;
                    }
                    if variation % 3 == 1 {
                        pixel[1] = pixel[0];
                        pixel[2] = pixel[0];
                    }
                    rgba.extend_from_slice(&pixel);
                }
                assert_eq!(
                    palette_from_rgba(&rgba),
                    sorted_reference_palette(&rgba),
                    "size {size}, variation {variation}"
                );
            }
        }
        // Equal weights must keep lower histogram keys as the winner.
        let tied = pixels(&[
            (500, [200, 40, 40]),
            (500, [40, 200, 40]),
            (500, [40, 40, 200]),
        ]);
        assert_eq!(palette_from_rgba(&tied), sorted_reference_palette(&tied));
    }

    #[test]
    fn fused_alpha_thumbnail_exactly_matches_buffered_reference() {
        for (width, height) in [(1, 1), (17, 13), (257, 193), (19, 1), (1, 19)] {
            let rgba = image::RgbaImage::from_fn(width, height, |x, y| {
                image::Rgba([
                    (x * 37 + y * 13) as u8,
                    (x * 19 + y * 41) as u8,
                    (x * 23 + y * 17) as u8,
                    (x * 31 + y * 43) as u8,
                ])
            });
            let luma = image::ImageBuffer::from_fn(width, height, |x, y| {
                image::LumaA([(x * 37 + y * 13) as u8, (x * 31 + y * 43) as u8])
            });
            for image in [
                image::DynamicImage::ImageRgba8(rgba),
                image::DynamicImage::ImageLumaA8(luma),
            ] {
                for edge in [0, 1, 7, 160] {
                    assert_eq!(
                        thumbnail_for_palette(image.clone(), edge),
                        buffered_reference_thumbnail(image.clone(), edge),
                        "{}x{} edge {edge}",
                        width,
                        height
                    );
                }
            }
        }
        let rgba16 = image::ImageBuffer::from_fn(23, 17, |x, y| {
            image::Rgba([
                (x * 1237) as u16,
                (y * 2543) as u16,
                (x * 3137) as u16,
                (y * 4337) as u16,
            ])
        });
        let image = image::DynamicImage::ImageRgba16(rgba16);
        assert_eq!(
            thumbnail_for_palette(image.clone(), 7),
            buffered_reference_thumbnail(image, 7)
        );
    }
    /// Run explicitly with --release --ignored --nocapture so both image and
    /// production code use the same optimized profile. Excludes file decoding
    /// and clones inputs before timing the consuming thumbnail operations.
    #[test]
    #[ignore = "performance comparison; run explicitly in release mode"]
    fn wallpaper_colors_performance_comparison() {
        use std::hint::black_box;
        use std::time::Instant;
        let mut seed = 42_u32;
        let rgba: Vec<u8> = (0..SAMPLE_EDGE * SAMPLE_EDGE)
            .flat_map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                [
                    (seed >> 24) as u8,
                    (seed >> 16) as u8,
                    (seed >> 8) as u8,
                    255,
                ]
            })
            .collect();
        assert_eq!(palette_from_rgba(&rgba), sorted_reference_palette(&rgba));
        for optimized in [false, true, true, false] {
            let start = Instant::now();
            for _ in 0..500 {
                black_box(if optimized {
                    palette_from_rgba(black_box(&rgba))
                } else {
                    sorted_reference_palette(black_box(&rgba))
                });
            }
            eprintln!(
                "histogram optimized={optimized}: {:.1} us/sample",
                start.elapsed().as_secs_f64() * 1e6 / 500.0
            );
        }
        for image in [
            image::DynamicImage::ImageRgba8(image::RgbaImage::from_fn(3840, 2160, |x, y| {
                image::Rgba([x as u8, y as u8, (x + y) as u8, (x * 13 + y * 7) as u8])
            })),
            image::DynamicImage::ImageLumaA8(image::ImageBuffer::from_fn(3840, 2160, |x, y| {
                image::LumaA([x as u8, (x * 13 + y * 7) as u8])
            })),
        ] {
            assert_eq!(
                thumbnail_for_palette(image.clone(), SAMPLE_EDGE),
                buffered_reference_thumbnail(image.clone(), SAMPLE_EDGE)
            );
            for optimized in [false, true, true, false] {
                let inputs: Vec<_> = (0..4).map(|_| image.clone()).collect();
                let start = Instant::now();
                for input in inputs {
                    black_box(if optimized {
                        thumbnail_for_palette(input, SAMPLE_EDGE)
                    } else {
                        buffered_reference_thumbnail(input, SAMPLE_EDGE)
                    });
                }
                eprintln!(
                    "thumbnail {:?} optimized={optimized}: {:.2} ms/image",
                    image.color(),
                    start.elapsed().as_secs_f64() * 1000.0 / 4.0
                );
            }
        }
    }

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
    fn daily_theme_tracks_primary_slot_and_off_restores_static_path() {
        let mut behavior = crate::config::Config::default().behavior().clone();
        behavior.wallpaper = "static.png".to_string();
        behavior.wallpaper_rotation = "daily".to_string();
        behavior.wallpaper_rotation_paths = vec!["a.png".to_string(), "b.png".to_string()];
        assert_eq!(theme_wallpaper_path(&behavior, 100, None), "a.png");
        assert_eq!(theme_wallpaper_path(&behavior, 101, None), "b.png");
        assert_eq!(theme_wallpaper_path(&behavior, 99, None), "b.png");
        behavior.wallpaper_rotation = "off".to_string();
        assert_eq!(theme_wallpaper_path(&behavior, 101, None), "static.png");
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
    fn transparent_hidden_colours_cannot_tint_the_thumbnail_palette() {
        let visible = [40, 180, 60, 255];
        let sample = |hidden| {
            let image = image::RgbaImage::from_fn(4, 2, |x, _| {
                image::Rgba(if x % 2 == 0 { visible } else { hidden })
            });
            thumbnail_for_palette(image::DynamicImage::ImageRgba8(image), 2)
        };
        // Both pictures display the same green stripes. Only invisible RGB
        // differs; the old straight-alpha resize produced yellow vs cyan.
        let red_hidden = sample([240, 20, 20, 0]);
        let blue_hidden = sample([20, 20, 240, 0]);
        assert_eq!(red_hidden.dimensions(), (2, 1));
        assert_eq!(red_hidden, blue_hidden);
        let actual = palette_from_rgba(red_hidden.as_raw()).expect("visible green");
        let reference = palette_from_rgba(&visible).expect("opaque green");
        for (actual, reference) in actual.accent.into_iter().zip(reference.accent) {
            assert!((actual - reference).abs() < 0.01);
        }
    }

    #[test]
    fn a_fully_transparent_thumbnail_has_no_palette() {
        let image = image::RgbaImage::from_fn(4, 2, |x, _| {
            image::Rgba(if x % 2 == 0 {
                [240, 20, 20, 0]
            } else {
                [20, 20, 240, 0]
            })
        });
        let sample = thumbnail_for_palette(image::DynamicImage::ImageRgba8(image), 2);
        assert!(palette_from_rgba(sample.as_raw()).is_none());
    }

    #[test]
    fn opaque_thumbnail_colours_remain_unchanged() {
        let image = image::DynamicImage::ImageRgba8(image::RgbaImage::from_fn(4, 2, |x, _| {
            image::Rgba(if x % 2 == 0 {
                [40, 180, 60, 255]
            } else {
                [180, 40, 60, 255]
            })
        }));
        let expected = image.thumbnail(2, 2).into_rgba8();
        assert_eq!(thumbnail_for_palette(image, 2), expected);
    }

    #[test]
    fn translucent_colours_count_by_visible_coverage() {
        // The hues have equal saturation and lightness. Two opaque blue
        // pixels cover more than three half-transparent red pixels.
        let buffer = [
            [200, 40, 40, 128],
            [200, 40, 40, 128],
            [200, 40, 40, 128],
            [40, 40, 200, 255],
            [40, 40, 200, 255],
        ]
        .concat();
        let palette = palette_from_rgba(&buffer).expect("palette");
        assert!(hue_distance(rgb_to_hsl(palette.accent)[0], 240.0) < 1.0);
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

    #[test]
    fn a_started_extraction_claims_its_wallpaper() {
        let mut features = crate::jwm::features::FeatureStates::default();
        let job = crate::jwm::features::connectivity::BackgroundJob::spawn(|| None);

        claim_extraction(&mut features, "~/a.png".to_string(), Some(job));

        assert_eq!(features.themed_wallpaper, "~/a.png");
        assert!(features.wallpaper_theme.is_some());
    }

    #[test]
    fn a_refused_extraction_leaves_the_wallpaper_unclaimed_for_a_retry() {
        // Regression: a worker the OS refused a thread for was stored and its
        // wallpaper claimed, so `get_wallpaper_colors` said `pending` forever
        // and no later config apply retried that wallpaper.
        let mut features = crate::jwm::features::FeatureStates::default();
        features.themed_wallpaper = "~/before.png".to_string();
        features.wallpaper_theme = Some(crate::jwm::features::connectivity::BackgroundJob::spawn(
            || None,
        ));

        claim_extraction(&mut features, "~/a.png".to_string(), None);

        // Nothing reads as pending, and the `themed_wallpaper == wallpaper`
        // guard in `refresh_wallpaper_theme` lets the same wallpaper through
        // on the next config apply.
        assert!(features.wallpaper_theme.is_none());
        assert!(features.themed_wallpaper.is_empty());
    }

    #[test]
    fn a_refused_worker_never_reaches_the_theme_slot() {
        // Through the same `Jwm` path `refresh_wallpaper_theme` hands every
        // extraction to, with a handle whose thread the OS refused.
        let mut backend = LockSpyBackend::new();
        let mut jwm =
            crate::jwm::Jwm::new_with_runtime_backend(&mut backend, "test").expect("test jwm");
        jwm.features.themed_wallpaper = "~/before.png".to_string();
        jwm.features.wallpaper_theme = Some(BackgroundJob::spawn(|| None));

        jwm.adopt_extraction("~/a.png".to_string(), BackgroundJob::refused());

        // Nothing reads as pending forever, and the wallpaper is left
        // unclaimed so the next config apply retries it.
        assert!(jwm.features.wallpaper_theme.is_none());
        assert!(jwm.features.themed_wallpaper.is_empty());
    }

    #[test]
    fn a_started_worker_claims_its_wallpaper_through_the_jwm_path() {
        let mut backend = LockSpyBackend::new();
        let mut jwm =
            crate::jwm::Jwm::new_with_runtime_backend(&mut backend, "test").expect("test jwm");

        jwm.adopt_extraction("~/a.png".to_string(), BackgroundJob::spawn(|| None));

        assert_eq!(jwm.features.themed_wallpaper, "~/a.png");
        assert!(jwm.features.wallpaper_theme.is_some());
    }

    #[test]
    fn clearing_the_wallpaper_discards_a_late_extraction() {
        for empty in ["", " \t\n"] {
            let mut backend = LockSpyBackend::new();
            let mut jwm =
                crate::jwm::Jwm::new_with_runtime_backend(&mut backend, "test").expect("test jwm");
            let previous = (
                "previous.png".to_string(),
                Palette {
                    accent: [0.2, 0.4, 0.8],
                    secondary: [0.6, 0.3, 0.8],
                },
            );
            jwm.features.wallpaper_palette = Some(previous.clone());
            let (job, finish) = BackgroundJob::controlled_for_test();
            jwm.adopt_extraction("pending.png".to_string(), job);

            jwm.refresh_wallpaper_theme_for(true, empty.to_string());
            finish(Some(Palette {
                accent: [0.8, 0.3, 0.2],
                secondary: [0.8, 0.6, 0.2],
            }));
            jwm.poll_wallpaper_theme(&mut backend);

            assert!(jwm.features.wallpaper_theme.is_none());
            assert!(jwm.features.themed_wallpaper.is_empty());
            assert_eq!(jwm.features.wallpaper_palette, Some(previous));
        }
    }

    #[test]
    fn an_unsuccessful_extraction_can_be_retried_at_the_same_path() {
        let mut backend = LockSpyBackend::new();
        let mut jwm =
            crate::jwm::Jwm::new_with_runtime_backend(&mut backend, "test").expect("test jwm");
        let previous = (
            "previous.png".to_string(),
            Palette {
                accent: [0.2, 0.4, 0.8],
                secondary: [0.6, 0.3, 0.8],
            },
        );
        jwm.features.wallpaper_palette = Some(previous.clone());
        jwm.adopt_extraction(
            "repairable.png".to_string(),
            BackgroundJob::completed_for_test(None),
        );

        jwm.poll_wallpaper_theme_for(&mut backend, true, "repairable.png".into());

        // The same-path guard must allow a later config apply to retry, while
        // failure itself keeps the existing colours and starts no new work.
        assert!(jwm.features.themed_wallpaper.is_empty());
        assert!(jwm.features.wallpaper_theme.is_none());
        assert_eq!(jwm.features.wallpaper_palette, Some(previous.clone()));
        let behavior = crate::config::Config::default().behavior().clone();
        let status = jwm.wallpaper_theme_json_for(&behavior, 100);
        assert_eq!(status["pending"], false);
        assert_eq!(status["wallpaper"], previous.0);
        assert_eq!(status["accent"], hex(previous.1.accent));
    }

    #[test]
    fn unrelated_apply_keeps_same_path_worker_and_explicit_invalidation_discards_it() {
        let mut backend = LockSpyBackend::new();
        let mut jwm =
            crate::jwm::Jwm::new_with_runtime_backend(&mut backend, "test").expect("test jwm");
        let previous = (
            "same.png".to_string(),
            Palette {
                accent: [0.8, 0.3, 0.2],
                secondary: [0.8, 0.6, 0.2],
            },
        );
        jwm.features.wallpaper_palette = Some(previous.clone());
        let (job, finish) = BackgroundJob::controlled_for_test();
        jwm.adopt_extraction("same.png".into(), job);
        jwm.refresh_wallpaper_theme_for(true, "same.png".into());
        finish(Some(previous.1));
        // If the ordinary apply replaced the worker, this controlled result
        // would not be available in the theme slot.
        assert_eq!(
            jwm.features.wallpaper_theme.as_ref().unwrap().take(),
            Some(Some(previous.1))
        );

        let (old, finish_old) = BackgroundJob::controlled_for_test();
        jwm.adopt_extraction("same.png".into(), old);
        jwm.invalidate_wallpaper_theme();
        finish_old(Some(Palette {
            accent: [0.2, 0.4, 0.8],
            secondary: [0.6, 0.3, 0.8],
        }));
        jwm.poll_wallpaper_theme(&mut backend);
        assert!(jwm.features.wallpaper_theme.is_none());
        assert!(jwm.features.themed_wallpaper.is_empty());
        assert_eq!(jwm.features.wallpaper_palette, Some(previous));
    }

    #[test]
    fn explicit_invalidation_reextracts_replaced_content_at_the_same_path() {
        let mut backend = LockSpyBackend::new();
        let mut jwm =
            crate::jwm::Jwm::new_with_runtime_backend(&mut backend, "test").expect("test jwm");
        let id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("jwm-theme-{}-{id}.png", std::process::id()));
        let wallpaper = path.to_string_lossy().into_owned();
        let take_palette = |jwm: &crate::jwm::Jwm| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                if let Some(result) = jwm.features.wallpaper_theme.as_ref().unwrap().take() {
                    break result.expect("image palette");
                }
                assert!(std::time::Instant::now() < deadline, "extraction timed out");
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        };
        image::RgbaImage::from_pixel(4, 4, image::Rgba([200, 40, 40, 255]))
            .save(&path)
            .expect("red image");
        jwm.refresh_wallpaper_theme_for(true, wallpaper.clone());
        let red = take_palette(&jwm);
        jwm.features.wallpaper_theme = None;
        jwm.features.wallpaper_palette = Some((wallpaper.clone(), red));
        image::RgbaImage::from_pixel(4, 4, image::Rgba([40, 40, 200, 255]))
            .save(&path)
            .expect("blue replacement");
        jwm.refresh_wallpaper_theme_for(true, wallpaper.clone());
        assert!(
            jwm.features.wallpaper_theme.is_none(),
            "unrelated apply must not decode again"
        );
        jwm.invalidate_wallpaper_theme();
        jwm.refresh_wallpaper_theme_for(true, wallpaper.clone());
        let blue = take_palette(&jwm);
        std::fs::remove_file(&path).expect("remove image");
        assert!(hue_distance(rgb_to_hsl(red.accent)[0], 0.0) < 1.0);
        assert!(hue_distance(rgb_to_hsl(blue.accent)[0], 240.0) < 1.0);
        assert_eq!(
            jwm.features.wallpaper_palette,
            Some((wallpaper, red)),
            "refresh keeps applied palette until a new result is adopted"
        );
    }

    #[test]
    fn wallpaper_status_keeps_legacy_palette_fields_separate_from_image_requests() {
        let mut backend = LockSpyBackend::new();
        let jwm =
            crate::jwm::Jwm::new_with_runtime_backend(&mut backend, "test").expect("test jwm");
        let mut behavior = crate::config::Config::default().behavior().clone();
        behavior.wallpaper_colors = false;
        behavior.wallpaper = "static.png".into();
        behavior.wallpaper_rotation = "off".into();
        let status = jwm.wallpaper_theme_json_for(&behavior, 100);
        assert_eq!(status["enabled"], false);
        assert_eq!(status["colors_enabled"], false);
        assert_eq!(status["image_enabled"], true);
        assert_eq!(status["current_wallpaper"], "static.png");
        assert_eq!(status["rotation"], "off");
        assert!(status["wallpaper"].is_null());
        assert!(status["accent"].is_null());

        behavior.wallpaper_rotation = "daily".into();
        behavior.wallpaper_rotation_paths = vec!["a.png".into(), "b.png".into()];
        let daily = jwm.wallpaper_theme_json_for(&behavior, 101);
        assert_eq!(daily["rotation"], "daily");
        assert_eq!(daily["current_wallpaper"], "b.png");
        assert_eq!(daily["enabled"], false);
        assert_eq!(daily["image_enabled"], true);
    }

    #[test]
    fn palette_file_rejects_oversized_regular_sources_and_fifo_without_waiting() {
        use std::os::unix::ffi::OsStrExt as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("jwm-palette-source-{}-{id}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let regular = dir.join("large.png");
        image::RgbaImage::from_pixel(4, 4, image::Rgba([40, 180, 60, 255]))
            .save(&regular)
            .unwrap();
        assert!(palette_from_file(&regular).is_some());
        std::fs::OpenOptions::new()
            .write(true)
            .open(&regular)
            .unwrap()
            .set_len(64 * 1024 * 1024 + 1)
            .unwrap();
        assert!(
            palette_from_file(&regular).is_none(),
            "valid image with excessive trailing bytes is rejected before decoding"
        );
        let fifo = dir.join("fifo.png");
        let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let worker_path = fifo.clone();
        let (sent, received) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _ = sent.send(palette_from_file(&worker_path).is_none());
        });
        let timely = received.recv_timeout(std::time::Duration::from_millis(250));
        if matches!(timely, Err(std::sync::mpsc::RecvTimeoutError::Timeout)) {
            // Release a regressed blocking open without making cleanup itself
            // depend on a reader being present at this exact instant.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while !worker.is_finished() && std::time::Instant::now() < deadline {
                if let Ok(writer) = std::fs::OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(&fifo)
                {
                    drop(writer);
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            received
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("FIFO palette worker did not finish after cleanup");
        }
        worker.join().expect("palette worker panicked");
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(
            matches!(timely, Ok(true)),
            "FIFO palette source was not rejected promptly: {timely:?}"
        );
    }

    #[test]
    fn primary_theme_source_resolves_tags_monitors_daily_picks_and_empty_outputs() {
        use crate::config::{WallpaperMonitorConfig, WallpaperTagConfig};
        use crate::core::models::WMMonitor;
        let mut backend = LockSpyBackend::new();
        let mut jwm = crate::jwm::Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        jwm.state.monitor_order.clear();
        let stale = jwm.state.monitors.insert(WMMonitor::new());
        jwm.state.monitors.remove(stale);
        jwm.state.monitor_order.push(stale);
        let mut first = WMMonitor::new();
        first.sel_tags = 0;
        first.tag_set[0] = 2;
        let first = jwm.state.monitors.insert(first);
        let second = jwm.state.monitors.insert(WMMonitor::new());
        jwm.state.monitor_order.extend([first, second]);
        jwm.state.sel_mon = Some(second);
        let mut behavior = crate::config::Config::default().behavior().clone();
        behavior.wallpaper = "global.png".into();
        behavior.wallpaper_rotation = "off".into();
        behavior.wallpaper_monitors = vec![WallpaperMonitorConfig {
            monitor: 1,
            path: "first.png".into(),
            mode: String::new(),
        }];
        behavior.wallpaper_tags = vec![WallpaperTagConfig {
            monitor: 1,
            tag: 1,
            path: "tag.png".into(),
            mode: String::new(),
        }];
        let primary = jwm.primary_wallpaper_monitor();
        assert_eq!(primary, Some((1, 2)));
        assert_eq!(theme_wallpaper_path(&behavior, 100, primary), "tag.png");
        jwm.state.monitors.get_mut(first).unwrap().tag_set[0] = 1;
        assert_eq!(
            theme_wallpaper_path(&behavior, 100, jwm.primary_wallpaper_monitor()),
            "first.png"
        );
        behavior.wallpaper_rotation = "daily".into();
        behavior.wallpaper_rotation_paths = vec!["a.png".into(), "b.png".into(), "c.png".into()];
        assert_eq!(
            theme_wallpaper_path(&behavior, 100, primary),
            "b.png",
            "first valid output uses dense rotation slot zero"
        );
        behavior.wallpaper_daily_pick = Some((100, "manual.png".into()));
        // Daily consumers resolve manual overrides against the dense output
        // slot too; the stale leading entry does not consume slot zero.
        behavior.wallpaper_tags[0].monitor = 0;
        assert_eq!(
            theme_wallpaper_path(&behavior, 100, primary),
            "tag.png",
            "daily manual pick preserves tag overrides"
        );
        behavior.wallpaper_tags.clear();
        behavior.wallpaper_monitors.clear();
        assert_eq!(theme_wallpaper_path(&behavior, 100, primary), "manual.png");
        jwm.state.monitor_order.clear();
        assert_eq!(jwm.primary_wallpaper_monitor(), None);
        assert_eq!(theme_wallpaper_path(&behavior, 100, None), "manual.png");
        behavior.wallpaper_rotation = "off".into();
        assert_eq!(theme_wallpaper_path(&behavior, 100, None), "global.png");
    }

    #[test]
    fn reconciliation_reports_start_cancel_clear_and_suppresses_same_path_events() {
        let mut backend = LockSpyBackend::new();
        let mut jwm = crate::jwm::Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let previous = (
            "previous.png".to_string(),
            Palette {
                accent: [0.2, 0.4, 0.8],
                secondary: [0.6, 0.3, 0.8],
            },
        );
        jwm.features.wallpaper_palette = Some(previous.clone());
        jwm.adopt_extraction("same.png".into(), BackgroundJob::controlled_for_test().0);
        assert_eq!(
            jwm.refresh_wallpaper_theme_for(true, "same.png".into()),
            ThemeReconcile::Unchanged
        );
        assert_eq!(
            jwm.refresh_wallpaper_theme_for(true, String::new()),
            ThemeReconcile::Cancelled
        );
        let mut behavior = crate::config::Config::default().behavior().clone();
        behavior.wallpaper_colors = true;
        let cancelled = jwm.wallpaper_theme_json_for(&behavior, 100);
        assert_eq!(cancelled["pending"], false);
        assert_eq!(cancelled["wallpaper"], "previous.png");
        assert_eq!(
            jwm.refresh_wallpaper_theme_for(true, String::new()),
            ThemeReconcile::Unchanged
        );
        assert_eq!(
            jwm.refresh_wallpaper_theme_for(false, String::new()),
            ThemeReconcile::Cleared
        );
        assert!(jwm.features.wallpaper_palette.is_none());
        assert_eq!(
            jwm.refresh_wallpaper_theme_for(false, String::new()),
            ThemeReconcile::Unchanged
        );

        jwm.features.wallpaper_palette = Some(previous.clone());
        jwm.adopt_extraction("same.png".into(), BackgroundJob::controlled_for_test().0);
        assert_eq!(
            jwm.invalidate_wallpaper_theme_request(),
            ThemeReconcile::Cancelled
        );
        assert_eq!(jwm.features.wallpaper_palette, Some(previous));
        assert_eq!(
            jwm.invalidate_wallpaper_theme_request(),
            ThemeReconcile::Unchanged
        );
        assert_eq!(
            jwm.refresh_wallpaper_theme_for(true, "new.png".into()),
            ThemeReconcile::Started
        );
        assert!(jwm.features.wallpaper_theme.is_some());
        assert_eq!(
            jwm.refresh_wallpaper_theme_for(true, "new.png".into()),
            ThemeReconcile::Unchanged
        );
        assert_eq!(
            jwm.refresh_wallpaper_theme_for(false, "new.png".into()),
            ThemeReconcile::Cleared
        );
        behavior.wallpaper_colors = false;
        let disabled = jwm.wallpaper_theme_json_for(&behavior, 100);
        assert_eq!(disabled["pending"], false);
        assert_eq!(disabled["enabled"], false);
        assert!(disabled["wallpaper"].is_null());
    }

    #[test]
    fn completed_old_source_is_discarded_and_new_source_is_requested_before_adoption() {
        let mut backend = LockSpyBackend::new();
        let mut jwm = crate::jwm::Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let previous = (
            "previous.png".to_string(),
            Palette {
                accent: [0.2, 0.4, 0.8],
                secondary: [0.6, 0.3, 0.8],
            },
        );
        let obsolete = Palette {
            accent: [0.8, 0.3, 0.2],
            secondary: [0.8, 0.6, 0.2],
        };
        jwm.features.wallpaper_palette = Some(previous.clone());
        let before = current_colors();
        jwm.adopt_extraction(
            "old.png".into(),
            BackgroundJob::completed_for_test(Some(obsolete)),
        );
        jwm.poll_wallpaper_theme_for(&mut backend, true, "new-primary.png".into());
        assert_eq!(jwm.features.themed_wallpaper, "new-primary.png");
        assert!(jwm.features.wallpaper_theme.is_some());
        assert_eq!(jwm.features.wallpaper_palette, Some(previous.clone()));
        assert_eq!(
            current_colors(),
            before,
            "obsolete palette never reaches config"
        );
        jwm.poll_wallpaper_theme_for(&mut backend, true, String::new());
        assert!(jwm.features.wallpaper_theme.is_none());
        assert!(jwm.features.themed_wallpaper.is_empty());
        assert_eq!(jwm.features.wallpaper_palette, Some(previous));
    }

    #[test]
    fn wallpaper_status_resolves_output_overrides_and_dense_daily_slots() {
        use crate::config::{WallpaperMonitorConfig, WallpaperTagConfig};
        use crate::core::models::WMMonitor;
        let mut backend = LockSpyBackend::new();
        let mut jwm =
            crate::jwm::Jwm::new_with_runtime_backend(&mut backend, "test").expect("test jwm");
        jwm.state.monitor_order.clear();
        let stale = jwm.state.monitors.insert(WMMonitor::new());
        jwm.state.monitors.remove(stale);
        jwm.state.monitor_order.push(stale);
        for tags in [1, 2] {
            let mut monitor = WMMonitor::new();
            monitor.sel_tags = 0;
            monitor.tag_set[0] = tags;
            let key = jwm.state.monitors.insert(monitor);
            jwm.state.monitor_order.push(key);
        }
        let mut behavior = crate::config::Config::default().behavior().clone();
        behavior.wallpaper = "global.png".into();
        behavior.wallpaper_rotation = "off".into();
        behavior.wallpaper_monitors = vec![WallpaperMonitorConfig {
            monitor: 1,
            path: "monitor.png".into(),
            mode: "fit".into(),
        }];
        behavior.wallpaper_tags = vec![WallpaperTagConfig {
            monitor: 2,
            tag: 1,
            path: "tag.png".into(),
            mode: "center".into(),
        }];
        let status = jwm.wallpaper_theme_json_for(&behavior, 100);
        assert_eq!(
            status["current_wallpapers"],
            serde_json::json!([
                {"monitor": 1, "path": "monitor.png", "mode": "fit"},
                {"monitor": 2, "path": "tag.png", "mode": "center"},
            ])
        );
        assert_eq!(status["current_wallpaper"], "monitor.png");
        behavior.wallpaper_rotation = "daily".into();
        behavior.wallpaper_rotation_paths = vec!["a.png".into(), "b.png".into(), "c.png".into()];
        behavior.wallpaper_mode = "fill".into();
        let daily = jwm.wallpaper_theme_json_for(&behavior, 100);
        assert_eq!(
            daily["current_wallpapers"],
            serde_json::json!([
                {"monitor": 1, "path": "b.png", "mode": "fill"},
                {"monitor": 2, "path": "c.png", "mode": "fill"},
            ])
        );
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
