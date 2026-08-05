//! Encoding a frosted strip for consumers that take an image *file* rather
//! than a buffer.
//!
//! Webview frontends are the reason this exists: a page cannot be handed a
//! pointer to pixels, and `backdrop-filter` cannot reach past the window to
//! blur the desktop, so the bar hands the page the frosted strip as a
//! `data:` URL and lets CSS composite its own tint over it.

use image::{ExtendedColorType, ImageEncoder, codecs::png::PngEncoder};

use super::{GlassError, GlassImage};

impl GlassImage {
    /// Encode as a PNG.
    ///
    /// PNG rather than JPEG because the strip carries alpha and because a
    /// blurred image compresses well enough that the difference does not
    /// matter at bar sizes.
    pub fn to_png(&self) -> Result<Vec<u8>, GlassError> {
        let mut png = Vec::new();
        PngEncoder::new(&mut png)
            .write_image(
                &self.to_rgba8(),
                self.width(),
                self.height(),
                ExtendedColorType::Rgba8,
            )
            .map_err(|error| GlassError::Unavailable(format!("PNG encode failed: {error}")))?;
        Ok(png)
    }

    /// Encode as a `data:image/png;base64,…` URL, ready for a CSS
    /// `background-image` or an `<img src>`.
    ///
    /// The result is roughly a third larger than [`to_png`](Self::to_png), so
    /// a frontend should rebuild it only when the strip's generation changes.
    pub fn to_png_data_uri(&self) -> Result<String, GlassError> {
        let png = self.to_png()?;
        let mut uri = String::with_capacity(30 + png.len().div_ceil(3) * 4);
        uri.push_str("data:image/png;base64,");
        base64_into(&png, &mut uri);
        Ok(uri)
    }

    /// A stylesheet putting this strip behind `selector`, under a tint.
    ///
    /// The tint is a solid `linear-gradient` rather than a `background-color`
    /// because CSS paints the *first* background layer on top of the later
    /// ones: a color would sit under the image and never be seen.  The
    /// element's own background is cleared for the same reason.
    ///
    /// `alpha` is how much of the element's own color survives; see
    /// [`DEFAULT_BACKGROUND_OPACITY`](super::DEFAULT_BACKGROUND_OPACITY).
    pub fn to_css_backdrop(
        &self,
        selector: &str,
        tint: [u8; 3],
        alpha: f32,
    ) -> Result<String, GlassError> {
        let uri = self.to_png_data_uri()?;
        let alpha = if alpha.is_finite() {
            alpha.clamp(0.0, 1.0)
        } else {
            1.0
        };
        let [red, green, blue] = tint;
        let tint = format!("rgba({red}, {green}, {blue}, {alpha})");
        Ok(format!(
            "{selector} {{ background-color: transparent; \
             background-image: linear-gradient({tint}, {tint}), url(\"{uri}\"); \
             background-size: 100% 100%; background-repeat: no-repeat; }}"
        ))
    }
}

/// Standard base64 with padding.
///
/// Hand-rolled because one call site does not justify a dependency, and
/// because the alphabet has not changed since 1987.
fn base64_into(data: &[u8], out: &mut String) {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let symbol = |bits: u32| ALPHABET[(bits & 0x3f) as usize] as char;

    let mut chunks = data.chunks_exact(3);
    for chunk in &mut chunks {
        let group = u32::from(chunk[0]) << 16 | u32::from(chunk[1]) << 8 | u32::from(chunk[2]);
        out.push(symbol(group >> 18));
        out.push(symbol(group >> 12));
        out.push(symbol(group >> 6));
        out.push(symbol(group));
    }
    match chunks.remainder() {
        [first] => {
            let group = u32::from(*first) << 16;
            out.push(symbol(group >> 18));
            out.push(symbol(group >> 12));
            out.push_str("==");
        }
        [first, second] => {
            let group = u32::from(*first) << 16 | u32::from(*second) << 8;
            out.push(symbol(group >> 18));
            out.push(symbol(group >> 12));
            out.push(symbol(group >> 6));
            out.push('=');
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base64(data: &[u8]) -> String {
        let mut out = String::new();
        base64_into(data, &mut out);
        out
    }

    #[test]
    fn base64_matches_the_rfc_test_vectors() {
        // RFC 4648 section 10, which is also every padding case.
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        // The high bits exercise the last two alphabet entries.
        assert_eq!(base64(&[0xff, 0xef, 0xbe]), "/+++");
    }

    #[test]
    fn a_strip_round_trips_through_png() {
        let mut image = GlassImage::new(3, 2).unwrap();
        for (index, pixel) in image.data_mut().chunks_exact_mut(4).enumerate() {
            let word = super::super::pack([0xff, index as u8 * 20, 40, 60]);
            pixel.copy_from_slice(&word.to_ne_bytes());
        }

        let png = image.to_png().unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "not a PNG");

        let decoded = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(decoded.dimensions(), (3, 2));
        assert_eq!(decoded.as_raw().as_slice(), image.to_rgba8().as_slice());

        let uri = image.to_png_data_uri().unwrap();
        assert!(uri.starts_with("data:image/png;base64,"));
        assert_eq!(
            uri.len(),
            "data:image/png;base64,".len() + png.len().div_ceil(3) * 4
        );
    }

    #[test]
    fn the_css_backdrop_layers_the_tint_over_the_image() {
        let image = GlassImage::new(2, 1).unwrap();
        let css = image
            .to_css_backdrop(".button-row", [255, 255, 255], 0.55)
            .unwrap();

        // The element's own opaque background has to go, or nothing shows
        // through it.
        assert!(css.starts_with(".button-row {"));
        assert!(css.contains("background-color: transparent"));

        // Layer order is the whole point: tint first, image second.
        let tint = css.find("linear-gradient").expect("tint layer");
        let backdrop = css.find("url(").expect("image layer");
        assert!(tint < backdrop, "the tint must be the topmost layer");
        assert!(css.contains("rgba(255, 255, 255, 0.55), rgba(255, 255, 255, 0.55)"));

        // A data URL carries characters CSS would otherwise read as syntax,
        // so it stays quoted; and both layers cover the element exactly.
        assert!(css.contains("url(\"data:image/png;base64,"));
        assert!(css.contains("background-size: 100% 100%"));
        assert!(css.contains("background-repeat: no-repeat"));

        // A nonsense alpha must not produce nonsense CSS.
        let css = image.to_css_backdrop("body", [0, 0, 0], f32::NAN).unwrap();
        assert!(css.contains("rgba(0, 0, 0, 1)"));
    }
}
