//! A wallpaper source backed by an image file.
//!
//! This is the portable one.  Reading the root pixmap works only on X11, only
//! when some tool published `_XROOTPMAP_ID`, and never under a compositor that
//! draws the wallpaper itself from a configured path — which is exactly what
//! JWM does.  Pointing a bar at the same file the compositor draws sidesteps
//! all three limits and works identically on Wayland.
//!
//! The file is laid out across the screen the way the compositor lays it out,
//! so [`WallpaperMode`] mirrors JWM's `behavior.wallpaper_mode`; a bar
//! configured with a different mode than the compositor would show a backdrop
//! that does not line up with its surroundings.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use image::{Limits, Rgba, RgbaImage, imageops::FilterType};

use super::{BYTES_PER_PIXEL, GlassError, GlassImage, StripRequest, WallpaperSource, pack};

/// Refuse absurd decodes rather than let a stray file exhaust memory.
const MAX_DIMENSION: u32 = 32_768;
const MAX_ALLOC: u64 = 512 * 1024 * 1024;

/// How a wallpaper file covers the screen.
///
/// The variants and the parse fallback match JWM's `parse_wallpaper_mode`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WallpaperMode {
    #[default]
    Fill,
    Fit,
    Stretch,
    Center,
}

impl WallpaperMode {
    /// Parse a config string.  Anything unrecognized is `Fill`, because that
    /// is the mode a compositor falls back to for the same string.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        match text.trim().to_ascii_lowercase().as_str() {
            "fit" => Self::Fit,
            "stretch" => Self::Stretch,
            "center" => Self::Center,
            _ => Self::Fill,
        }
    }
}

/// Wallpaper pixels read from a file and laid out across the screen.
///
/// The composed screen-sized canvas is cached, which costs
/// `screen_width * screen_height * 4` bytes; [`release_cache`] drops it for a
/// frontend that would rather pay the decode again.  The file's size and
/// modification time are restatted on every [`WallpaperSource::revision`]
/// call, so swapping the wallpaper refreshes the backdrop without any
/// platform-specific change notification.
///
/// [`release_cache`]: WallpaperFile::release_cache
#[derive(Debug)]
pub struct WallpaperFile {
    path: PathBuf,
    mode: WallpaperMode,
    screen: (u32, u32),
    background: [u8; 3],
    canvas: Option<GlassImage>,
    stamp: Option<u64>,
    revision: u64,
}

impl WallpaperFile {
    #[must_use]
    pub fn new(
        path: impl Into<PathBuf>,
        mode: WallpaperMode,
        screen_width: u32,
        screen_height: u32,
    ) -> Self {
        Self {
            path: path.into(),
            mode,
            screen: (screen_width, screen_height),
            background: [0, 0, 0],
            canvas: None,
            stamp: None,
            revision: 0,
        }
    }

    /// Color shown where the image does not reach, for `Fit` and `Center`.
    #[must_use]
    pub const fn with_background(mut self, rgb: [u8; 3]) -> Self {
        self.background = rgb;
        self
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Track a screen resolution change.  A stale screen size would place the
    /// wallpaper differently than the compositor does.
    pub fn set_screen(&mut self, width: u32, height: u32) {
        if self.screen != (width, height) {
            self.screen = (width, height);
            self.canvas = None;
            self.revision = self.revision.wrapping_add(1);
        }
    }

    /// Drop the composed canvas, trading memory for a re-decode on the next
    /// strip.
    pub fn release_cache(&mut self) {
        self.canvas = None;
    }

    fn canvas(&mut self) -> Result<&GlassImage, GlassError> {
        if self.canvas.is_none() {
            self.canvas = Some(compose(
                &self.path,
                self.mode,
                self.screen,
                self.background,
            )?);
        }
        self.canvas.as_ref().ok_or(GlassError::EmptyRegion)
    }
}

impl WallpaperSource for WallpaperFile {
    fn revision(&mut self) -> u64 {
        let stamp = file_stamp(&self.path);
        if self.stamp != Some(stamp) {
            self.stamp = Some(stamp);
            self.canvas = None;
            self.revision = self.revision.wrapping_add(1);
        }
        self.revision
    }

    fn strip(&mut self, request: &StripRequest) -> Result<GlassImage, GlassError> {
        if request.width == 0 || request.height == 0 {
            return Err(GlassError::EmptyRegion);
        }
        let canvas = self.canvas()?;
        crop(canvas, request)
    }
}

/// Size and modification time folded into one token.  Both change on a
/// rewrite; either alone can miss an edit that preserved the other.
fn file_stamp(path: &Path) -> u64 {
    let Ok(metadata) = std::fs::metadata(path) else {
        return 0;
    };
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_nanos() as u64);
    metadata
        .len()
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .rotate_left(31)
        ^ modified
}

fn crop(canvas: &GlassImage, request: &StripRequest) -> Result<GlassImage, GlassError> {
    let x = request.x.max(0) as u32;
    let y = request.y.max(0) as u32;
    if u64::from(x) + u64::from(request.width) > u64::from(canvas.width()) {
        return Err(GlassError::Unavailable(format!(
            "wallpaper is {} wide, bar needs {} at x={x}",
            canvas.width(),
            request.width
        )));
    }
    if u64::from(y) + u64::from(request.height) > u64::from(canvas.height()) {
        return Err(GlassError::Unavailable(format!(
            "wallpaper is {} tall, bar needs {} at y={y}",
            canvas.height(),
            request.height
        )));
    }
    // Padding below the bar is best effort: take what the screen still has.
    let height = request.padded_height().min(canvas.height() - y);

    let mut strip = GlassImage::new(request.width, height)?;
    let row = request.width as usize * BYTES_PER_PIXEL;
    for line in 0..height as usize {
        let source = (y as usize + line) * canvas.stride() + x as usize * BYTES_PER_PIXEL;
        let target = line * row;
        strip.data_mut()[target..target + row]
            .copy_from_slice(&canvas.data()[source..source + row]);
    }
    Ok(strip)
}

fn compose(
    path: &Path,
    mode: WallpaperMode,
    screen: (u32, u32),
    background: [u8; 3],
) -> Result<GlassImage, GlassError> {
    let (screen_width, screen_height) = screen;
    if screen_width == 0 || screen_height == 0 {
        return Err(GlassError::EmptyRegion);
    }
    let decoded = decode(path)?;
    let placement = destination_rect(mode, screen, (decoded.width(), decoded.height()));

    let mut canvas = GlassImage::new(screen_width, screen_height)?;
    let fill = pack([0xff, background[0], background[1], background[2]]).to_ne_bytes();
    for pixel in canvas.data_mut().chunks_exact_mut(BYTES_PER_PIXEL) {
        pixel.copy_from_slice(&fill);
    }

    let scaled = if (placement.width, placement.height) == (decoded.width(), decoded.height()) {
        decoded
    } else {
        // Triangle rather than Lanczos: the result is about to be blurred into
        // unrecognizability, so sharpness buys nothing and costs milliseconds.
        image::imageops::resize(
            &decoded,
            placement.width.max(1),
            placement.height.max(1),
            FilterType::Triangle,
        )
    };
    blit(&mut canvas, &scaled, placement.x, placement.y);
    Ok(canvas)
}

fn decode(path: &Path) -> Result<RgbaImage, GlassError> {
    let mut reader = image::ImageReader::open(path)
        .map_err(|source| GlassError::Io {
            path: path.to_owned(),
            source,
        })?
        .with_guessed_format()
        .map_err(|source| GlassError::Io {
            path: path.to_owned(),
            source,
        })?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_ALLOC);
    reader.limits(limits);
    let decoded = reader.decode().map_err(|error| GlassError::Decode {
        path: path.to_owned(),
        reason: error.to_string(),
    })?;
    Ok(decoded.into_rgba8())
}

/// Where the scaled image lands on the screen, in screen coordinates.
///
/// Offsets are signed because `Fill` and `Center` legitimately place the image
/// off-canvas; [`blit`] clips.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Placement {
    x: i64,
    y: i64,
    width: u32,
    height: u32,
}

fn destination_rect(mode: WallpaperMode, screen: (u32, u32), image: (u32, u32)) -> Placement {
    let (screen_width, screen_height) = (f64::from(screen.0), f64::from(screen.1));
    let (image_width, image_height) = (f64::from(image.0), f64::from(image.1));
    if image_width <= 0.0 || image_height <= 0.0 {
        return Placement {
            x: 0,
            y: 0,
            width: screen.0,
            height: screen.1,
        };
    }

    let scaled = |scale: f64| {
        let width = (image_width * scale).round().max(1.0);
        let height = (image_height * scale).round().max(1.0);
        Placement {
            x: ((screen_width - width) / 2.0).round() as i64,
            y: ((screen_height - height) / 2.0).round() as i64,
            width: width as u32,
            height: height as u32,
        }
    };

    match mode {
        WallpaperMode::Stretch => Placement {
            x: 0,
            y: 0,
            width: screen.0,
            height: screen.1,
        },
        WallpaperMode::Fill => {
            scaled((screen_width / image_width).max(screen_height / image_height))
        }
        WallpaperMode::Fit => {
            scaled((screen_width / image_width).min(screen_height / image_height))
        }
        WallpaperMode::Center => scaled(1.0),
    }
}

/// Draw `source` onto `canvas` at `(x, y)`, clipped, premultiplying alpha.
fn blit(canvas: &mut GlassImage, source: &RgbaImage, x: i64, y: i64) {
    let canvas_width = i64::from(canvas.width());
    let canvas_height = i64::from(canvas.height());
    let stride = canvas.stride();

    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = (x + i64::from(source.width())).min(canvas_width);
    let y1 = (y + i64::from(source.height())).min(canvas_height);

    for target_y in y0..y1 {
        for target_x in x0..x1 {
            let Rgba([red, green, blue, alpha]) =
                *source.get_pixel((target_x - x) as u32, (target_y - y) as u32);
            let premultiply =
                |channel: u8| ((u32::from(channel) * u32::from(alpha) + 127) / 255).min(255) as u8;
            let word = if alpha == 0xff {
                pack([0xff, red, green, blue])
            } else {
                pack([
                    alpha,
                    premultiply(red),
                    premultiply(green),
                    premultiply(blue),
                ])
            };
            let offset = target_y as usize * stride + target_x as usize * BYTES_PER_PIXEL;
            canvas.data_mut()[offset..offset + BYTES_PER_PIXEL]
                .copy_from_slice(&word.to_ne_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glass::{GlassParams, frost};

    fn placement(mode: WallpaperMode, screen: (u32, u32), image: (u32, u32)) -> Placement {
        destination_rect(mode, screen, image)
    }

    #[test]
    fn modes_place_the_image_the_way_the_compositor_does() {
        let screen = (1000, 500);

        // Fill covers both axes and overflows the other one symmetrically.
        let fill = placement(WallpaperMode::Fill, screen, (1000, 1000));
        assert_eq!((fill.width, fill.height), (1000, 1000));
        assert_eq!((fill.x, fill.y), (0, -250));

        // Fit keeps the whole image and letterboxes.
        let fit = placement(WallpaperMode::Fit, screen, (1000, 1000));
        assert_eq!((fit.width, fit.height), (500, 500));
        assert_eq!((fit.x, fit.y), (250, 0));

        assert_eq!(
            placement(WallpaperMode::Stretch, screen, (10, 10)),
            Placement {
                x: 0,
                y: 0,
                width: 1000,
                height: 500
            }
        );

        let center = placement(WallpaperMode::Center, screen, (200, 100));
        assert_eq!((center.width, center.height), (200, 100));
        assert_eq!((center.x, center.y), (400, 200));
    }

    #[test]
    fn unknown_mode_strings_fall_back_to_fill() {
        assert_eq!(WallpaperMode::parse("FIT"), WallpaperMode::Fit);
        assert_eq!(WallpaperMode::parse(" center "), WallpaperMode::Center);
        assert_eq!(WallpaperMode::parse("tile"), WallpaperMode::Fill);
        assert_eq!(WallpaperMode::parse(""), WallpaperMode::Fill);
    }

    #[test]
    fn a_clipped_blit_stays_inside_the_canvas() {
        let mut canvas = GlassImage::new(4, 4).unwrap();
        let source = RgbaImage::from_pixel(4, 4, Rgba([10, 20, 30, 255]));
        // Half off the top-left corner.
        blit(&mut canvas, &source, -2, -2);

        let word = |x: usize, y: usize| {
            let offset = y * canvas.stride() + x * BYTES_PER_PIXEL;
            u32::from_ne_bytes(canvas.data()[offset..offset + 4].try_into().unwrap())
        };
        assert_eq!(word(0, 0), pack([0xff, 10, 20, 30]));
        assert_eq!(word(1, 1), pack([0xff, 10, 20, 30]));
        // The uncovered quadrant kept the background.
        assert_eq!(word(3, 3), 0);
    }

    #[test]
    fn cropping_refuses_a_bar_that_leaves_the_wallpaper() {
        let canvas = GlassImage::new(100, 50).unwrap();
        assert!(crop(&canvas, &StripRequest::new(90, 0, 20, 10)).is_err());
        assert!(crop(&canvas, &StripRequest::new(0, 45, 20, 10)).is_err());

        // Padding below the bar is clamped to what exists rather than failing.
        let strip = crop(&canvas, &StripRequest::new(0, 40, 20, 10).with_pad(48)).unwrap();
        assert_eq!((strip.width(), strip.height()), (20, 10));
    }

    /// The end-to-end path a bar actually runs: decode a file, lay it out,
    /// crop the bar's strip, frost it.
    #[test]
    fn a_file_becomes_a_frosted_strip() {
        let path = std::env::temp_dir().join(format!(
            "xbar-glass-{}-{:?}.png",
            std::process::id(),
            std::thread::current().id()
        ));
        // Left half red, right half blue, so a mis-cropped strip is obvious.
        let mut source = RgbaImage::new(64, 64);
        for (x, _, pixel) in source.enumerate_pixels_mut() {
            *pixel = if x < 32 {
                Rgba([200, 0, 0, 255])
            } else {
                Rgba([0, 0, 200, 255])
            };
        }
        source.save(&path).unwrap();

        let mut wallpaper = WallpaperFile::new(&path, WallpaperMode::Stretch, 128, 64);
        let first = wallpaper.revision();
        assert_eq!(
            wallpaper.revision(),
            first,
            "an untouched file must keep its revision"
        );

        let request = StripRequest::new(0, 0, 128, 16).with_pad(16);
        let strip = wallpaper.strip(&request).unwrap();
        assert_eq!((strip.width(), strip.height()), (128, 32));

        let frosted = frost(&strip, &GlassParams::default(), request.height).unwrap();
        assert_eq!((frosted.width(), frosted.height()), (128, 16));
        let sample = |x: u32| {
            let offset = x as usize * BYTES_PER_PIXEL;
            let word = u32::from_ne_bytes(frosted.data()[offset..offset + 4].try_into().unwrap());
            [((word >> 16) & 0xff) as u8, (word & 0xff) as u8]
        };
        let [left_red, left_blue] = sample(4);
        let [right_red, right_blue] = sample(123);
        assert!(left_red > left_blue, "left edge lost its red");
        assert!(right_blue > right_red, "right edge lost its blue");

        // Losing the file bumps the revision and then fails to produce a
        // strip, so the bar falls back to a solid background instead of
        // panicking or serving a stale backdrop forever.
        std::fs::remove_file(&path).unwrap();
        assert_ne!(wallpaper.revision(), first);
        assert!(wallpaper.strip(&request).is_err());
    }
}
