//! Frosted-glass backdrop, baked now only for the Tauri webview bridge.
//!
//! A bar cannot see through itself: whatever "glass" it shows is something it
//! draws.  Two ways exist to get there:
//!
//! * A compositor blurs what lies behind a translucent window.  Nothing in
//!   this module beyond [`fallback_rgb`] and [`DEFAULT_BACKGROUND_OPACITY`]
//!   is involved — the bar just needs per-pixel alpha.  Every non-Tauri bar
//!   takes this path, deciding once at startup whether a compositing manager
//!   owns `_NET_WM_CM_Sn` and the pipeline can deliver alpha; when either
//!   fails it paints an opaque [`fallback_rgb`] background instead.
//! * A webview cannot take that path: `backdrop-filter` blurs only what is
//!   inside the page, never the desktop behind the window.  So the Tauri
//!   bridge still bakes the backdrop — sample the wallpaper where the bar
//!   sits, blur and saturate that strip, and hand it to the page as an
//!   image.  That is what lives here, and it is this module's one remaining
//!   baking consumer.
//!
//! The baked path stays free of anything X11-specific or Cairo-specific:
//! [`frost`] is integer pixel math over a premultiplied `ARgb32` buffer —
//! the same byte layout
//! [`CairoBar::render_into_bgra`](crate::render::cairo::CairoBar::render_into_bgra)
//! already fills.  Where the wallpaper pixels come from is the one genuinely
//! platform-dependent step, so it is a trait: see [`WallpaperSource`], and
//! [`wallpaper::WallpaperFile`] for the file-backed implementation that works
//! on any windowing system.
//!
//! ```no_run
//! use xbar_core::glass::{GlassCache, GlassParams, StripRequest, WallpaperSource};
//!
//! fn redraw(source: &mut impl WallpaperSource, cache: &mut GlassCache) {
//!     let request = StripRequest::new(0, 0, 1920, 40).with_pad(cache.params().pad);
//!     if let Some(frosted) = cache.ensure(source, &request) {
//!         // Blit `frosted`, then draw the scene over it with alpha.
//!         let _ = frosted.data();
//!     }
//! }
//! ```

use std::fmt;
use std::path::PathBuf;

#[cfg(feature = "glass-wallpaper")]
mod encode;
#[cfg(feature = "glass-wallpaper")]
pub mod wallpaper;

#[cfg(feature = "render-cairo")]
mod cairo_interop;

#[cfg(feature = "render-cairo")]
pub use cairo_interop::GlassBackdrop;

/// Bytes per pixel in every buffer this module touches.
const BYTES_PER_PIXEL: usize = 4;

/// Opaque base color for a bar that has no wallpaper to frost, and the color
/// behind a wallpaper that does not cover the screen.
///
/// These are the neutral near-black and near-white the macOS materials fall
/// back to, dark enough and light enough that the bar's own text keeps its
/// contrast when the glass is gone.
#[must_use]
pub const fn fallback_rgb(theme: crate::ThemeMode) -> [u8; 3] {
    match theme {
        crate::ThemeMode::Dark => [28, 28, 30],
        crate::ThemeMode::Light => [246, 246, 248],
    }
}

/// Background alpha that reads as glass rather than as a tinted panel.
///
/// A frosted backdrop only looks like a material if some of it shows through
/// the bar's own background fill, so a frontend that frosts should default its
/// background opacity here instead of to fully opaque.
pub const DEFAULT_BACKGROUND_OPACITY: f64 = 0.55;

// ---------------- Errors ----------------

#[derive(Debug)]
pub enum GlassError {
    /// A zero-width, zero-height, or otherwise degenerate region.
    EmptyRegion,
    /// A caller-supplied buffer cannot hold the described image.
    BufferTooSmall {
        expected: usize,
        got: usize,
    },
    /// Dimensions that do not fit the platform's address space.
    Overflow,
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Decode {
        path: PathBuf,
        reason: String,
    },
    /// The source could not produce a strip this time; the frontend should
    /// fall back to a solid background rather than fail.
    Unavailable(String),
}

impl fmt::Display for GlassError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyRegion => write!(f, "empty glass region"),
            Self::BufferTooSmall { expected, got } => {
                write!(f, "buffer holds {got} bytes, need {expected}")
            }
            Self::Overflow => write!(f, "glass image dimensions overflow"),
            Self::Io { path, source } => write!(f, "failed to read {}: {source}", path.display()),
            Self::Decode { path, reason } => {
                write!(f, "failed to decode {}: {reason}", path.display())
            }
            Self::Unavailable(reason) => write!(f, "frosted glass unavailable: {reason}"),
        }
    }
}

impl std::error::Error for GlassError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

// ---------------- Parameters ----------------

/// Frosting recipe.
///
/// The defaults approximate the macOS menu-bar material: blur wide enough that
/// no wallpaper detail survives, plus a saturation boost, because a plain
/// Gaussian of a photo reads as dirty gray rather than as glass.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GlassParams {
    /// Blur on a `1/downscale` copy.  A box blur costs the same per pixel
    /// whatever its radius, so shrinking first is what makes a wide blur cheap
    /// — and the upscale hides the lost resolution.
    pub downscale: u32,
    /// Box-blur radius in downscaled pixels.
    pub blur_radius: u32,
    /// Box-blur passes.  Three converge close enough on a Gaussian that the
    /// difference is invisible under a saturation boost.
    pub blur_passes: u32,
    /// Distance to push each channel away from its luma.  `1.0` is identity.
    pub saturation: f32,
    /// Extra wallpaper rows sampled below the bar so the blur has real
    /// neighborhood data instead of clamped edge pixels.  These rows are
    /// blurred and then cropped away, never shown.
    pub pad: u32,
}

impl Default for GlassParams {
    fn default() -> Self {
        Self {
            downscale: 4,
            blur_radius: 6,
            blur_passes: 3,
            saturation: 1.8,
            pad: 48,
        }
    }
}

impl GlassParams {
    /// Clamp every field into the range the algorithms accept, so a config
    /// file cannot produce a division by zero or an allocation storm.
    #[must_use]
    pub fn sanitized(mut self) -> Self {
        self.downscale = self.downscale.clamp(1, 64);
        self.blur_radius = self.blur_radius.min(512);
        self.blur_passes = self.blur_passes.min(8);
        self.saturation = if self.saturation.is_finite() {
            self.saturation.clamp(0.0, 8.0)
        } else {
            1.0
        };
        self.pad = self.pad.min(4096);
        self
    }
}

// ---------------- Image ----------------

/// A tightly packed premultiplied `ARgb32` image: native-endian
/// `0xAARRGGBB` words, which is `B, G, R, A` bytes on little-endian.
///
/// The stride is always `width * 4`.  Cairo's stride for a 32-bit format is
/// the same, and every CPU frontend in this repository either uses that layout
/// already or converts once at upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlassImage {
    width: u32,
    height: u32,
    data: Vec<u8>,
}

impl GlassImage {
    /// A fully transparent image.
    pub fn new(width: u32, height: u32) -> Result<Self, GlassError> {
        let len = byte_len(width, height)?;
        Ok(Self {
            width,
            height,
            data: vec![0; len],
        })
    }

    /// Copy `data`, which holds `height` rows of `stride` bytes in this
    /// module's pixel layout.  Rows may be padded; the copy is tightly packed.
    pub fn from_bgra(
        width: u32,
        height: u32,
        stride: usize,
        data: &[u8],
    ) -> Result<Self, GlassError> {
        let tight = byte_len(width, height)?;
        let row = width as usize * BYTES_PER_PIXEL;
        if stride < row {
            return Err(GlassError::BufferTooSmall {
                expected: row,
                got: stride,
            });
        }
        let required = stride
            .checked_mul(height as usize)
            .ok_or(GlassError::Overflow)?;
        if data.len() < required {
            return Err(GlassError::BufferTooSmall {
                expected: required,
                got: data.len(),
            });
        }
        let mut packed = Vec::with_capacity(tight);
        for y in 0..height as usize {
            let start = y * stride;
            packed.extend_from_slice(&data[start..start + row]);
        }
        Ok(Self {
            width,
            height,
            data: packed,
        })
    }

    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// Bytes per row, always `width * 4`.
    #[must_use]
    pub const fn stride(&self) -> usize {
        self.width as usize * BYTES_PER_PIXEL
    }

    #[must_use]
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    #[must_use]
    pub fn data_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }

    #[must_use]
    pub fn into_data(self) -> Vec<u8> {
        self.data
    }

    /// Straight (un-premultiplied) `R, G, B, A` bytes, tightly packed.
    ///
    /// This is what every GPU and toolkit texture upload outside Cairo wants —
    /// `gdk::MemoryTexture`, `egui::ColorImage`, `iced::widget::image`, a wgpu
    /// texture, a PNG encoder.  A frosted strip is opaque, so the division is
    /// a no-op in practice; it is still done, because a caller that frosted a
    /// wallpaper with alpha would otherwise get darkened edges.
    #[must_use]
    pub fn to_rgba8(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.data.len());
        for pixel in self.data.chunks_exact(BYTES_PER_PIXEL) {
            let [alpha, red, green, blue] = unpack(read_word(pixel, 0));
            let straighten = |channel: u8| match alpha {
                0 => 0,
                0xff => channel,
                alpha => ((u32::from(channel) * 255 + u32::from(alpha) / 2) / u32::from(alpha))
                    .min(255) as u8,
            };
            out.extend_from_slice(&[straighten(red), straighten(green), straighten(blue), alpha]);
        }
        out
    }

    #[must_use]
    fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }
}

fn byte_len(width: u32, height: u32) -> Result<usize, GlassError> {
    (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(BYTES_PER_PIXEL))
        .ok_or(GlassError::Overflow)
}

// ---------------- Wallpaper sources ----------------

/// The region of wallpaper a bar needs, in root/screen coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StripRequest {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// Rows a source should add below `height` when the wallpaper has them.
    pub pad: u32,
}

impl StripRequest {
    #[must_use]
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
            pad: 0,
        }
    }

    #[must_use]
    pub const fn with_pad(mut self, pad: u32) -> Self {
        self.pad = pad;
        self
    }

    /// Rows a source should return when it can.  Returning fewer is fine — the
    /// blur then works with less context — but never fewer than `height`.
    #[must_use]
    pub const fn padded_height(&self) -> u32 {
        self.height.saturating_add(self.pad)
    }
}

/// Where the pixels behind the bar come from.
///
/// Implementations are expected to be cheap to call repeatedly: [`GlassCache`]
/// consults [`WallpaperSource::revision`] every frame and only asks for a new
/// strip when geometry or revision changed.
pub trait WallpaperSource {
    /// A token that changes whenever the wallpaper changed.  The default never
    /// changes, which is right for a source that is fixed for the process
    /// lifetime and wrong for anything watching a file or an X property.
    fn revision(&mut self) -> u64 {
        0
    }

    /// The wallpaper covering `request`, at least `request.height` rows tall.
    fn strip(&mut self, request: &StripRequest) -> Result<GlassImage, GlassError>;
}

impl<T: WallpaperSource + ?Sized> WallpaperSource for &mut T {
    fn revision(&mut self) -> u64 {
        (**self).revision()
    }

    fn strip(&mut self, request: &StripRequest) -> Result<GlassImage, GlassError> {
        (**self).strip(request)
    }
}

// ---------------- Cache ----------------

/// Keeps the frosted strip until the bar moves, resizes, or the wallpaper
/// changes.  Frosting costs a full blur, so doing it per frame would be a
/// waste on a bar that redraws once a second.
#[derive(Debug)]
pub struct GlassCache {
    params: GlassParams,
    key: Option<(StripRequest, u64)>,
    frosted: Option<GlassImage>,
    generation: u64,
    fallback: Option<[u8; 3]>,
}

impl GlassCache {
    #[must_use]
    pub fn new(params: GlassParams) -> Self {
        Self {
            params: params.sanitized(),
            key: None,
            frosted: None,
            generation: 0,
            fallback: None,
        }
    }

    /// Substitute a solid color when no wallpaper can be read.
    ///
    /// A frontend drawing into an opaque window wants this: without a backdrop
    /// the scene's translucent background has nothing to blend with, and a
    /// surface with no alpha channel would show the raw fill rather than the
    /// intended tint.  A frontend that can be genuinely translucent should
    /// leave it unset and skip rendering glass entirely instead.
    #[must_use]
    pub const fn with_fallback(mut self, rgb: [u8; 3]) -> Self {
        self.fallback = Some(rgb);
        self
    }

    #[must_use]
    pub const fn params(&self) -> &GlassParams {
        &self.params
    }

    pub fn set_params(&mut self, params: GlassParams) {
        let params = params.sanitized();
        if params != self.params {
            self.params = params;
            self.invalidate();
        }
    }

    /// Counter bumped every time a new strip is built.  A frontend that keeps
    /// a converted copy — a Cairo surface, a GPU texture — rebuilds it exactly
    /// when this changes instead of every frame.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub fn invalidate(&mut self) {
        self.key = None;
        self.frosted = None;
    }

    /// The strip built by the last [`ensure`](Self::ensure), without touching
    /// the source.  A frontend converting the strip into its own texture reads
    /// it here after checking [`generation`](Self::generation).
    #[must_use]
    pub const fn frosted(&self) -> Option<&GlassImage> {
        self.frosted.as_ref()
    }

    /// The frosted strip for `request`, rebuilt only when it must be.
    ///
    /// A source failure yields the configured fallback color, or `None` when
    /// there is none, and is not retried until something changes: a bar
    /// without a wallpaper should settle on a solid background once, not
    /// re-attempt a failing capture on every redraw.
    pub fn ensure<S: WallpaperSource + ?Sized>(
        &mut self,
        source: &mut S,
        request: &StripRequest,
    ) -> Option<&GlassImage> {
        let key = (*request, source.revision());
        if self.key != Some(key) {
            self.key = Some(key);
            self.frosted = match source
                .strip(request)
                .and_then(|strip| frost(&strip, &self.params, request.height))
            {
                Ok(frosted) => Some(frosted),
                Err(error) => {
                    log::debug!("{error}");
                    self.fallback
                        .and_then(|rgb| solid(rgb, request.width, request.height).ok())
                }
            };
            self.generation = self.generation.wrapping_add(1);
        }
        self.frosted.as_ref()
    }
}

/// A one-color image standing in for an unreadable wallpaper.
fn solid(rgb: [u8; 3], width: u32, height: u32) -> Result<GlassImage, GlassError> {
    if width == 0 || height == 0 {
        return Err(GlassError::EmptyRegion);
    }
    let mut image = GlassImage::new(width, height)?;
    let word = pack([0xff, rgb[0], rgb[1], rgb[2]]).to_ne_bytes();
    for pixel in image.data_mut().chunks_exact_mut(BYTES_PER_PIXEL) {
        pixel.copy_from_slice(&word);
    }
    Ok(image)
}

// ---------------- Frontend bundle ----------------

/// A wallpaper source and its cache, which is all a frontend needs to keep.
///
/// Toolkit frontends upload the strip into their own texture: hold onto the
/// returned generation, and re-upload only when it changes.
///
/// ```no_run
/// # use xbar_core::glass::{GlassStrip, GlassParams};
/// # fn upload(_: Vec<u8>, _: u32, _: u32) {}
/// # fn demo<S: xbar_core::glass::WallpaperSource>(strip: &mut GlassStrip<S>, last: &mut u64) {
/// if let Some((generation, image)) = strip.ensure(0, 0, 1920, 40)
///     && generation != *last
/// {
///     upload(image.to_rgba8(), image.width(), image.height());
///     *last = generation;
/// }
/// # }
/// ```
#[derive(Debug)]
pub struct GlassStrip<S> {
    source: S,
    cache: GlassCache,
}

impl<S: WallpaperSource> GlassStrip<S> {
    #[must_use]
    pub fn new(source: S, params: GlassParams) -> Self {
        Self {
            source,
            cache: GlassCache::new(params),
        }
    }

    /// See [`GlassCache::with_fallback`].
    #[must_use]
    pub fn with_fallback(mut self, rgb: [u8; 3]) -> Self {
        self.cache = self.cache.with_fallback(rgb);
        self
    }

    /// The wallpaper source, for frontends that must tell it something —
    /// a screen resolution change, an X property change.
    pub const fn source_mut(&mut self) -> &mut S {
        &mut self.source
    }

    pub const fn cache(&self) -> &GlassCache {
        &self.cache
    }

    pub const fn cache_mut(&mut self) -> &mut GlassCache {
        &mut self.cache
    }

    /// The frosted strip for a bar occupying `width x height` device pixels at
    /// `(x, y)` in screen coordinates, with the generation it belongs to.
    pub fn ensure(
        &mut self,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    ) -> Option<(u64, &GlassImage)> {
        let request = StripRequest::new(x, y, width, height).with_pad(self.cache.params().pad);
        self.cache.ensure(&mut self.source, &request)?;
        Some((self.cache.generation(), self.cache.frosted()?))
    }
}

// ---------------- Frosting ----------------

/// Blur and saturate `strip`, returning a `strip.width x target_height` image.
///
/// `strip` is expected to be `target_height` rows plus whatever padding the
/// source could supply.  The padding participates in the blur and is then
/// cropped: the result stays aligned with the wallpaper pixel-for-pixel, which
/// is the whole illusion — a backdrop that has drifted or stretched relative
/// to what surrounds the bar reads as a picture of glass rather than glass.
pub fn frost(
    strip: &GlassImage,
    params: &GlassParams,
    target_height: u32,
) -> Result<GlassImage, GlassError> {
    if strip.is_empty() || target_height == 0 {
        return Err(GlassError::EmptyRegion);
    }
    let params = params.sanitized();

    let mut planes = Planes::downscale_from(strip, params.downscale);
    planes.blur(params.blur_radius, params.blur_passes);
    planes.saturate(params.saturation);
    let full_height = strip.height.max(target_height);
    let expanded = planes.upscale(strip.width, full_height)?;
    crop_top(expanded, target_height)
}

fn crop_top(mut image: GlassImage, height: u32) -> Result<GlassImage, GlassError> {
    if image.height == height {
        return Ok(image);
    }
    image.data.truncate(byte_len(image.width, height)?);
    image.height = height;
    Ok(image)
}

/// Separated color channels.
///
/// Blur and saturation both work channel-by-channel, and doing that on planes
/// rather than on packed words keeps the index arithmetic honest: the alpha
/// lane cannot be blurred by accident, and nothing depends on which byte of a
/// native-endian word holds red.
#[derive(Debug)]
struct Planes {
    width: usize,
    height: usize,
    channels: [Vec<u8>; 3],
}

impl Planes {
    /// Box-average `image` down by `factor`, splitting into planes on the way.
    fn downscale_from(image: &GlassImage, factor: u32) -> Self {
        let source_width = image.width as usize;
        let source_height = image.height as usize;
        let width = (source_width / factor.max(1) as usize).max(1);
        let height = (source_height / factor.max(1) as usize).max(1);
        let mut channels = [
            vec![0u8; width * height],
            vec![0u8; width * height],
            vec![0u8; width * height],
        ];

        for y in 0..height {
            let y0 = y * source_height / height;
            let y1 = (((y + 1) * source_height) / height)
                .max(y0 + 1)
                .min(source_height);
            for x in 0..width {
                let x0 = x * source_width / width;
                let x1 = (((x + 1) * source_width) / width)
                    .max(x0 + 1)
                    .min(source_width);
                let mut sums = [0u32; 3];
                let mut count = 0u32;
                for sy in y0..y1 {
                    let row = sy * source_width * BYTES_PER_PIXEL;
                    for sx in x0..x1 {
                        let offset = row + sx * BYTES_PER_PIXEL;
                        let word = read_word(&image.data, offset);
                        let [_, r, g, b] = unpack(word);
                        sums[0] += u32::from(r);
                        sums[1] += u32::from(g);
                        sums[2] += u32::from(b);
                        count += 1;
                    }
                }
                let index = y * width + x;
                for channel in 0..3 {
                    channels[channel][index] = (sums[channel] / count.max(1)) as u8;
                }
            }
        }

        Self {
            width,
            height,
            channels,
        }
    }

    fn blur(&mut self, radius: u32, passes: u32) {
        if radius == 0 || passes == 0 || self.width == 0 || self.height == 0 {
            return;
        }
        let radius = (radius as usize).min(self.width.max(self.height));
        let mut scratch = vec![0u8; self.width * self.height];
        for plane in &mut self.channels {
            for _ in 0..passes {
                box_blur_rows(plane, &mut scratch, self.width, self.height, radius);
                box_blur_columns(plane, &scratch, self.width, self.height, radius);
            }
        }
    }

    /// Push each channel away from the pixel's luma.
    fn saturate(&mut self, amount: f32) {
        if (amount - 1.0).abs() < f32::EPSILON {
            return;
        }
        let [red, green, blue] = &mut self.channels;
        for index in 0..red.len() {
            let r = f32::from(red[index]);
            let g = f32::from(green[index]);
            let b = f32::from(blue[index]);
            let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            let adjust = |value: f32| (luma + (value - luma) * amount).clamp(0.0, 255.0) as u8;
            red[index] = adjust(r);
            green[index] = adjust(g);
            blue[index] = adjust(b);
        }
    }

    /// Bilinearly expand back to `width x height` as an opaque image.
    fn upscale(&self, width: u32, height: u32) -> Result<GlassImage, GlassError> {
        let mut output = GlassImage::new(width, height)?;
        if self.width == 0 || self.height == 0 {
            return Ok(output);
        }
        let target_width = width as usize;
        let target_height = height as usize;
        // Sample at pixel centers so the first and last output pixels sit
        // inside the source rather than half a texel outside it.
        let x_ratio = self.width as f32 / target_width.max(1) as f32;
        let y_ratio = self.height as f32 / target_height.max(1) as f32;

        for y in 0..target_height {
            let source_y = ((y as f32 + 0.5) * y_ratio - 0.5).max(0.0);
            let y0 = source_y as usize;
            let y1 = (y0 + 1).min(self.height - 1);
            let wy = source_y - y0 as f32;
            for x in 0..target_width {
                let source_x = ((x as f32 + 0.5) * x_ratio - 0.5).max(0.0);
                let x0 = source_x as usize;
                let x1 = (x0 + 1).min(self.width - 1);
                let wx = source_x - x0 as f32;

                let mut rgb = [0u8; 3];
                for (channel, plane) in self.channels.iter().enumerate() {
                    let top = lerp(
                        f32::from(plane[y0 * self.width + x0]),
                        f32::from(plane[y0 * self.width + x1]),
                        wx,
                    );
                    let bottom = lerp(
                        f32::from(plane[y1 * self.width + x0]),
                        f32::from(plane[y1 * self.width + x1]),
                        wx,
                    );
                    rgb[channel] = lerp(top, bottom, wy).round().clamp(0.0, 255.0) as u8;
                }
                let word = pack([0xff, rgb[0], rgb[1], rgb[2]]);
                write_word(
                    output.data_mut(),
                    (y * target_width + x) * BYTES_PER_PIXEL,
                    word,
                );
            }
        }
        Ok(output)
    }
}

/// One horizontal box-blur pass, `plane` into `scratch`.
fn box_blur_rows(plane: &[u8], scratch: &mut [u8], width: usize, height: usize, radius: usize) {
    let window = (2 * radius + 1) as u32;
    for y in 0..height {
        let row = y * width;
        let sample = |x: usize| u32::from(plane[row + x.min(width - 1)]);
        // The window that would hang off the left edge is filled with copies
        // of the edge pixel, so a bright border does not bleed dark.
        let mut sum: u32 = radius as u32 * sample(0) + (0..=radius).map(sample).sum::<u32>();
        for x in 0..width {
            scratch[row + x] = (sum / window) as u8;
            sum += sample(x + radius + 1);
            sum -= sample(x.saturating_sub(radius));
        }
    }
}

/// One vertical box-blur pass, `scratch` back into `plane`.
fn box_blur_columns(plane: &mut [u8], scratch: &[u8], width: usize, height: usize, radius: usize) {
    let window = (2 * radius + 1) as u32;
    for x in 0..width {
        let sample = |y: usize| u32::from(scratch[y.min(height - 1) * width + x]);
        let mut sum: u32 = radius as u32 * sample(0) + (0..=radius).map(sample).sum::<u32>();
        for y in 0..height {
            plane[y * width + x] = (sum / window) as u8;
            sum += sample(y + radius + 1);
            sum -= sample(y.saturating_sub(radius));
        }
    }
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn read_word(data: &[u8], offset: usize) -> u32 {
    let bytes: [u8; 4] = data[offset..offset + 4].try_into().expect("4-byte pixel");
    u32::from_ne_bytes(bytes)
}

fn write_word(data: &mut [u8], offset: usize, word: u32) {
    data[offset..offset + 4].copy_from_slice(&word.to_ne_bytes());
}

/// Split a native-endian `ARgb32` word into `[a, r, g, b]`.
const fn unpack(word: u32) -> [u8; 4] {
    [
        ((word >> 24) & 0xff) as u8,
        ((word >> 16) & 0xff) as u8,
        ((word >> 8) & 0xff) as u8,
        (word & 0xff) as u8,
    ]
}

const fn pack([a, r, g, b]: [u8; 4]) -> u32 {
    ((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | b as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an image from a per-pixel `[r, g, b]` function.
    fn image_from(width: u32, height: u32, f: impl Fn(u32, u32) -> [u8; 3]) -> GlassImage {
        let mut image = GlassImage::new(width, height).unwrap();
        for y in 0..height {
            for x in 0..width {
                let [r, g, b] = f(x, y);
                let offset = (y as usize * width as usize + x as usize) * BYTES_PER_PIXEL;
                write_word(image.data_mut(), offset, pack([0xff, r, g, b]));
            }
        }
        image
    }

    fn pixel(image: &GlassImage, x: u32, y: u32) -> [u8; 4] {
        let offset = (y as usize * image.width as usize + x as usize) * BYTES_PER_PIXEL;
        unpack(read_word(image.data(), offset))
    }

    struct FixedSource {
        image: GlassImage,
        revision: u64,
        calls: usize,
    }

    impl WallpaperSource for FixedSource {
        fn revision(&mut self) -> u64 {
            self.revision
        }

        fn strip(&mut self, _request: &StripRequest) -> Result<GlassImage, GlassError> {
            self.calls += 1;
            Ok(self.image.clone())
        }
    }

    #[test]
    fn frosting_a_uniform_strip_changes_nothing_but_its_height() {
        let strip = image_from(64, 40, |_, _| [40, 90, 160]);
        let params = GlassParams::default();
        let frosted = frost(&strip, &params, 16).unwrap();

        assert_eq!((frosted.width(), frosted.height()), (64, 16));
        for y in 0..16 {
            for x in 0..64 {
                let [a, r, g, b] = pixel(&frosted, x, y);
                assert_eq!(a, 0xff, "backdrop must stay opaque");
                // Saturation moves a flat color; the blur must not.
                assert!(r > 0 && g > 0 && b > 0);
            }
        }
        // A uniform input has one luma, so saturation is a pure per-channel
        // push away from it and stays deterministic.
        assert_eq!(pixel(&frosted, 0, 0), pixel(&frosted, 63, 15));
    }

    /// The padding rows exist to feed the blur, not to be squeezed into view:
    /// output row 0 must still describe wallpaper row 0.
    #[test]
    fn padding_rows_are_cropped_rather_than_squashed() {
        // Top eight rows white, the padded remainder black.
        let strip = image_from(8, 40, |_, y| if y < 8 { [255; 3] } else { [0; 3] });
        let params = GlassParams {
            downscale: 1,
            blur_radius: 0,
            blur_passes: 0,
            saturation: 1.0,
            pad: 32,
        };
        let frosted = frost(&strip, &params, 8).unwrap();

        assert_eq!(frosted.height(), 8);
        for y in 0..8 {
            assert_eq!(
                pixel(&frosted, 0, y),
                [0xff, 255, 255, 255],
                "row {y} came from below the bar"
            );
        }
    }

    #[test]
    fn blur_spreads_an_edge_without_shifting_it() {
        let strip = image_from(32, 8, |x, _| if x < 16 { [255; 3] } else { [0; 3] });
        let params = GlassParams {
            downscale: 1,
            blur_radius: 4,
            blur_passes: 1,
            saturation: 1.0,
            pad: 0,
        };
        let frosted = frost(&strip, &params, 8).unwrap();

        // Far from the edge the plateaus survive; across it the ramp is
        // monotone and centered.
        assert_eq!(pixel(&frosted, 0, 4)[1], 255);
        assert_eq!(pixel(&frosted, 31, 4)[1], 0);
        let mut previous = 255;
        for x in 10..22 {
            let value = pixel(&frosted, x, 4)[1];
            assert!(value <= previous, "ramp is not monotone at x={x}");
            previous = value;
        }
        let left = pixel(&frosted, 15, 4)[1] as i32;
        let right = pixel(&frosted, 16, 4)[1] as i32;
        assert!(
            (left + right - 255).abs() <= 32,
            "edge drifted: {left}/{right}"
        );
    }

    #[test]
    fn saturation_of_one_is_identity_and_zero_is_gray() {
        let strip = image_from(4, 4, |_, _| [200, 60, 30]);
        let identity = GlassParams {
            downscale: 1,
            blur_radius: 0,
            blur_passes: 0,
            saturation: 1.0,
            pad: 0,
        };
        assert_eq!(
            pixel(&frost(&strip, &identity, 4).unwrap(), 0, 0),
            [0xff, 200, 60, 30]
        );

        let gray = frost(
            &strip,
            &GlassParams {
                saturation: 0.0,
                ..identity
            },
            4,
        )
        .unwrap();
        let [_, r, g, b] = pixel(&gray, 0, 0);
        assert_eq!(r, g);
        assert_eq!(g, b);
    }

    #[test]
    fn downscale_averages_its_box() {
        // A checkerboard of two colors averages to their midpoint.
        let strip = image_from(
            8,
            8,
            |x, y| {
                if (x + y) % 2 == 0 { [200; 3] } else { [100; 3] }
            },
        );
        let frosted = frost(
            &strip,
            &GlassParams {
                downscale: 8,
                blur_radius: 0,
                blur_passes: 0,
                saturation: 1.0,
                pad: 0,
            },
            8,
        )
        .unwrap();
        assert_eq!(pixel(&frosted, 4, 4), [0xff, 150, 150, 150]);
    }

    #[test]
    fn cache_rebuilds_only_when_geometry_or_wallpaper_changes() {
        let mut source = FixedSource {
            image: image_from(64, 40, |_, _| [10, 20, 30]),
            revision: 1,
            calls: 0,
        };
        let mut cache = GlassCache::new(GlassParams::default());
        let request = StripRequest::new(0, 0, 64, 16).with_pad(24);

        assert!(cache.ensure(&mut source, &request).is_some());
        assert_eq!(source.calls, 1);
        let generation = cache.generation();

        assert!(cache.ensure(&mut source, &request).is_some());
        assert_eq!(source.calls, 1, "an unchanged frame must not re-frost");
        assert_eq!(cache.generation(), generation);

        // Moving the bar is a new key.
        let moved = StripRequest::new(0, 24, 64, 16).with_pad(24);
        assert!(cache.ensure(&mut source, &moved).is_some());
        assert_eq!(source.calls, 2);

        // So is a wallpaper swap under a stable geometry.
        source.revision = 2;
        assert!(cache.ensure(&mut source, &moved).is_some());
        assert_eq!(source.calls, 3);
        assert_ne!(cache.generation(), generation);
    }

    #[test]
    fn a_failing_source_degrades_to_none_without_retrying() {
        struct Failing(usize);
        impl WallpaperSource for Failing {
            fn strip(&mut self, _: &StripRequest) -> Result<GlassImage, GlassError> {
                self.0 += 1;
                Err(GlassError::Unavailable("no wallpaper".into()))
            }
        }

        let mut source = Failing(0);
        let mut cache = GlassCache::new(GlassParams::default());
        let request = StripRequest::new(0, 0, 64, 16);
        assert!(cache.ensure(&mut source, &request).is_none());
        assert!(cache.ensure(&mut source, &request).is_none());
        assert_eq!(source.0, 1);
    }

    #[test]
    fn degenerate_geometry_is_an_error_rather_than_a_panic() {
        let strip = image_from(8, 8, |_, _| [1, 2, 3]);
        assert!(matches!(
            frost(&strip, &GlassParams::default(), 0),
            Err(GlassError::EmptyRegion)
        ));
        assert!(matches!(
            frost(&GlassImage::new(0, 8).unwrap(), &GlassParams::default(), 8),
            Err(GlassError::EmptyRegion)
        ));
    }

    #[test]
    fn padded_rows_are_copied_out_of_a_strided_buffer() {
        // Two rows of two pixels with four bytes of row padding.
        let stride = 2 * BYTES_PER_PIXEL + 4;
        let mut buffer = vec![0u8; stride * 2];
        for y in 0..2 {
            for x in 0..2 {
                write_word(
                    &mut buffer,
                    y * stride + x * BYTES_PER_PIXEL,
                    pack([0xff, (x + y) as u8, 0, 0]),
                );
            }
        }
        let image = GlassImage::from_bgra(2, 2, stride, &buffer).unwrap();
        assert_eq!(image.stride(), 2 * BYTES_PER_PIXEL);
        assert_eq!(pixel(&image, 1, 1), [0xff, 2, 0, 0]);

        assert!(matches!(
            GlassImage::from_bgra(2, 2, stride, &buffer[..stride]),
            Err(GlassError::BufferTooSmall { .. })
        ));
    }

    #[test]
    fn rgba_output_is_straight_and_channel_ordered() {
        let mut image = GlassImage::new(2, 1).unwrap();
        // Opaque orange, then a half-transparent white stored premultiplied.
        write_word(image.data_mut(), 0, pack([0xff, 255, 128, 0]));
        write_word(
            image.data_mut(),
            BYTES_PER_PIXEL,
            pack([128, 128, 128, 128]),
        );

        let rgba = image.to_rgba8();
        assert_eq!(&rgba[..4], &[255, 128, 0, 255]);
        let [red, green, blue, alpha] = <[u8; 4]>::try_from(&rgba[4..8]).unwrap();
        assert_eq!(alpha, 128);
        // 128/128 straightens back to full white, within rounding.
        for channel in [red, green, blue] {
            assert!(channel >= 254, "un-premultiply lost energy: {channel}");
        }
    }

    #[test]
    fn a_strip_falls_back_to_a_solid_and_reports_its_generation() {
        struct Failing;
        impl WallpaperSource for Failing {
            fn strip(&mut self, _: &StripRequest) -> Result<GlassImage, GlassError> {
                Err(GlassError::Unavailable("no wallpaper".into()))
            }
        }

        let mut bare = GlassStrip::new(Failing, GlassParams::default());
        assert!(bare.ensure(0, 0, 8, 4).is_none());

        let mut solid = GlassStrip::new(Failing, GlassParams::default()).with_fallback([9, 8, 7]);
        let (generation, image) = solid.ensure(0, 0, 8, 4).expect("fallback strip");
        assert_eq!((image.width(), image.height()), (8, 4));
        assert_eq!(&image.to_rgba8()[..4], &[9, 8, 7, 255]);

        // A repeated frame is the same generation, so a frontend keeps its
        // uploaded texture.
        assert_eq!(solid.ensure(0, 0, 8, 4).map(|(g, _)| g), Some(generation));
        assert_ne!(solid.ensure(0, 0, 16, 4).map(|(g, _)| g), Some(generation));
    }

    #[test]
    fn parameters_are_clamped_into_a_workable_range() {
        let params = GlassParams {
            downscale: 0,
            blur_radius: u32::MAX,
            blur_passes: 99,
            saturation: f32::NAN,
            pad: u32::MAX,
        }
        .sanitized();
        assert_eq!(params.downscale, 1);
        assert_eq!(params.blur_radius, 512);
        assert_eq!(params.blur_passes, 8);
        assert_eq!(params.saturation, 1.0);
        assert_eq!(params.pad, 4096);
    }
}
