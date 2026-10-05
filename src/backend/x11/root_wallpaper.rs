//! Upload a composed wallpaper onto the X11 root when no compositor is drawing.

/// Convert packed RGBA8 into little-endian X11 ZPixmap BGRA (`[B,G,R,A]`).
#[must_use]
pub(crate) fn rgba_to_bgra_le(rgba: &[u8]) -> Vec<u8> {
    let mut packed = Vec::with_capacity(rgba.len());
    for pixel in rgba.chunks_exact(4) {
        packed.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
    }
    packed
}

/// How many rows of a `width`-pixel BGRA image fit in one PutImage request.
#[must_use]
pub(crate) fn put_image_rows(max_request_bytes: usize, width: u32) -> u32 {
    let row = (width as usize).saturating_mul(4).max(1);
    // Leave a conservative header so the request stays under the server limit.
    let usable = max_request_bytes.saturating_sub(64);
    (usable / row).max(1) as u32
}

#[cfg(test)]
mod tests {
    use super::{put_image_rows, rgba_to_bgra_le};

    #[test]
    fn rgba_swaps_to_x11_bgra() {
        assert_eq!(rgba_to_bgra_le(&[1, 2, 3, 4]), vec![3, 2, 1, 4]);
    }

    #[test]
    fn put_image_always_sends_at_least_one_row() {
        assert_eq!(put_image_rows(8, 1920), 1);
        assert!(put_image_rows(1_000_000, 1920) > 1);
    }
}
