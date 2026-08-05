//! The Cairo side of the frosted backdrop: conversions between [`GlassImage`]
//! and image surfaces, plus the [`GlassBackdrop`] a Cairo frontend keeps.
//!
//! Both sides already agree on the pixel layout — native-endian `ARgb32`
//! words — so the conversions only reconcile stride, which Cairo chooses for
//! itself.

use cairo::{Format, ImageSurface};

use super::{BYTES_PER_PIXEL, GlassError, GlassImage, GlassParams, GlassStrip, WallpaperSource};

impl GlassImage {
    /// Copy a Cairo image surface.
    ///
    /// `Rgb24` is accepted alongside `ARgb32`: it has the same 32-bit pixels
    /// with an unused alpha byte, and it is what a wallpaper capture off an
    /// opaque drawable produces.  The alpha byte is forced opaque so the
    /// result composites the same either way.
    pub fn from_image_surface(surface: &mut ImageSurface) -> Result<Self, GlassError> {
        let format = surface.format();
        if !matches!(format, Format::ARgb32 | Format::Rgb24) {
            return Err(GlassError::Unavailable(format!(
                "unsupported Cairo format {format:?}"
            )));
        }
        let width = u32::try_from(surface.width()).map_err(|_| GlassError::Overflow)?;
        let height = u32::try_from(surface.height()).map_err(|_| GlassError::Overflow)?;
        let stride = usize::try_from(surface.stride()).map_err(|_| GlassError::Overflow)?;

        surface.flush();
        let data = surface
            .data()
            .map_err(|error| GlassError::Unavailable(format!("surface data borrow: {error}")))?;
        let mut image = Self::from_bgra(width, height, stride, &data)?;
        if format == Format::Rgb24 {
            for pixel in image.data.chunks_exact_mut(BYTES_PER_PIXEL) {
                pixel.copy_from_slice(&(super::read_word(pixel, 0) | 0xff00_0000).to_ne_bytes());
            }
        }
        Ok(image)
    }

    /// Copy into a fresh `ARgb32` surface.
    ///
    /// Frontends that cache the result should key it on
    /// [`GlassCache::generation`](super::GlassCache::generation) so the copy
    /// happens per rebuild rather than per frame.
    pub fn to_image_surface(&self) -> Result<ImageSurface, GlassError> {
        let width = i32::try_from(self.width()).map_err(|_| GlassError::Overflow)?;
        let height = i32::try_from(self.height()).map_err(|_| GlassError::Overflow)?;
        let mut surface = ImageSurface::create(Format::ARgb32, width, height)
            .map_err(|error| GlassError::Unavailable(format!("surface create: {error}")))?;
        let stride = usize::try_from(surface.stride()).map_err(|_| GlassError::Overflow)?;
        let row = self.stride();
        {
            let mut data = surface.data().map_err(|error| {
                GlassError::Unavailable(format!("surface data borrow: {error}"))
            })?;
            for y in 0..self.height() as usize {
                let target = y * stride;
                data[target..target + row].copy_from_slice(&self.data[y * row..(y + 1) * row]);
            }
        }
        surface.mark_dirty();
        Ok(surface)
    }
}

/// Everything a Cairo frontend needs to draw glass: the wallpaper source, the
/// frosting cache, and the surface the renderer paints from.
///
/// Feed the result to
/// [`CairoBar::render_over`](crate::render::cairo::CairoBar::render_over) or
/// [`CpuCanvas::render_over`](crate::render::cairo::CpuCanvas::render_over).
/// The surface is rebuilt only when the cache reports a new strip, so a redraw
/// that changed nothing re-uploads nothing.
#[derive(Debug)]
pub struct GlassBackdrop<S> {
    strip: GlassStrip<S>,
    surface: Option<(u64, ImageSurface)>,
}

impl<S: WallpaperSource> GlassBackdrop<S> {
    #[must_use]
    pub fn new(source: S, params: GlassParams) -> Self {
        Self {
            strip: GlassStrip::new(source, params),
            surface: None,
        }
    }

    /// See [`GlassCache::with_fallback`](super::GlassCache::with_fallback).
    #[must_use]
    pub fn with_fallback(mut self, rgb: [u8; 3]) -> Self {
        self.strip = self.strip.with_fallback(rgb);
        self
    }

    /// The wallpaper source, for frontends that must tell it something —
    /// a screen resolution change, an X property change.
    pub const fn source_mut(&mut self) -> &mut S {
        self.strip.source_mut()
    }

    pub const fn cache_mut(&mut self) -> &mut super::GlassCache {
        self.strip.cache_mut()
    }

    /// The backdrop for a bar occupying `width x height` device pixels at
    /// `(x, y)` in screen coordinates.
    ///
    /// `None` means no wallpaper was available and no fallback color was
    /// configured; the frontend should render without a backdrop rather than
    /// treat it as an error.
    pub fn ensure(&mut self, x: i32, y: i32, width: u32, height: u32) -> Option<&ImageSurface> {
        let Some((generation, image)) = self.strip.ensure(x, y, width, height) else {
            self.surface = None;
            return None;
        };
        if self
            .surface
            .as_ref()
            .is_none_or(|(cached, _)| *cached != generation)
        {
            let surface = image
                .to_image_surface()
                .map_err(|error| log::debug!("frosted strip could not become a surface: {error}"))
                .ok()?;
            self.surface = Some((generation, surface));
        }
        self.surface.as_ref().map(|(_, surface)| surface)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glass::StripRequest;

    /// A source that hands back one flat image and counts its calls.
    struct Flat {
        revision: u64,
        calls: usize,
    }

    impl WallpaperSource for Flat {
        fn revision(&mut self) -> u64 {
            self.revision
        }

        fn strip(&mut self, request: &StripRequest) -> Result<GlassImage, GlassError> {
            self.calls += 1;
            GlassImage::new(request.width, request.padded_height())
        }
    }

    #[test]
    fn the_surface_is_rebuilt_only_when_the_strip_is() {
        let mut backdrop = GlassBackdrop::new(
            Flat {
                revision: 1,
                calls: 0,
            },
            GlassParams::default(),
        );

        let first = backdrop.ensure(0, 0, 64, 16).map(|surface| surface.width());
        assert_eq!(first, Some(64));
        assert_eq!(backdrop.source_mut().calls, 1);

        assert!(backdrop.ensure(0, 0, 64, 16).is_some());
        assert_eq!(
            backdrop.source_mut().calls,
            1,
            "an unchanged frame must not re-frost"
        );

        backdrop.source_mut().revision = 2;
        assert!(backdrop.ensure(0, 0, 64, 16).is_some());
        assert_eq!(backdrop.source_mut().calls, 2);
    }

    #[test]
    fn a_failing_source_yields_nothing_or_the_configured_solid() {
        struct Failing;
        impl WallpaperSource for Failing {
            fn strip(&mut self, _: &StripRequest) -> Result<GlassImage, GlassError> {
                Err(GlassError::Unavailable("no wallpaper".into()))
            }
        }

        let mut bare = GlassBackdrop::new(Failing, GlassParams::default());
        assert!(bare.ensure(0, 0, 64, 16).is_none());

        let mut solid =
            GlassBackdrop::new(Failing, GlassParams::default()).with_fallback([9, 8, 7]);
        let surface = solid.ensure(0, 0, 64, 16).expect("fallback surface");
        assert_eq!((surface.width(), surface.height()), (64, 16));

        // A resized bar gets a resized fallback rather than a stale one.
        let resized = solid.ensure(0, 0, 32, 16).expect("fallback surface");
        assert_eq!(resized.width(), 32);

        // Reading the pixels needs exclusive access, so let go of the backdrop
        // before inspecting its last surface.
        let mut copy = solid
            .ensure(0, 0, 32, 16)
            .cloned()
            .expect("fallback surface");
        drop(solid);
        let image = GlassImage::from_image_surface(&mut copy).unwrap();
        let word = u32::from_ne_bytes(image.data()[..4].try_into().unwrap());
        assert_eq!(word, super::super::pack([0xff, 9, 8, 7]));
    }

    #[test]
    fn a_surface_round_trips_through_a_glass_image() {
        let mut original = ImageSurface::create(Format::ARgb32, 5, 3).unwrap();
        {
            let stride = original.stride() as usize;
            let mut data = original.data().unwrap();
            for y in 0..3usize {
                for x in 0..5usize {
                    let word = 0xff00_0000 | ((x as u32) << 16) | ((y as u32) << 8);
                    data[y * stride + x * BYTES_PER_PIXEL..][..4]
                        .copy_from_slice(&word.to_ne_bytes());
                }
            }
        }

        let image = GlassImage::from_image_surface(&mut original).unwrap();
        assert_eq!((image.width(), image.height()), (5, 3));

        let mut copy = image.to_image_surface().unwrap();
        let restored = GlassImage::from_image_surface(&mut copy).unwrap();
        assert_eq!(image, restored);
    }

    #[test]
    fn an_rgb24_capture_becomes_opaque() {
        // Cairo leaves the unused byte of an Rgb24 surface at whatever the
        // allocation held; a backdrop must not inherit that as alpha.
        let mut surface = ImageSurface::create(Format::Rgb24, 2, 2).unwrap();
        let image = GlassImage::from_image_surface(&mut surface).unwrap();
        for pixel in image.data().chunks_exact(BYTES_PER_PIXEL) {
            let word = u32::from_ne_bytes(pixel.try_into().unwrap());
            assert_eq!(word >> 24, 0xff);
        }
    }
}
