//! Pixel-capture rules shared by every compositor backend.

/// A top-left-origin capture rectangle that has been clipped to an output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CaptureRegion {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Clip a requested top-left-origin rectangle to the output bounds.
///
/// Negative origins shrink the rectangle instead of shifting its right/bottom
/// edge, so all backends capture identical pixels for the same selection.
pub fn clip_region(
    output_width: u32,
    output_height: u32,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
) -> Option<CaptureRegion> {
    let right = i64::from(x).saturating_add(i64::from(width));
    let bottom = i64::from(y).saturating_add(i64::from(height));
    let left = i64::from(x).clamp(0, i64::from(output_width));
    let top = i64::from(y).clamp(0, i64::from(output_height));
    let right = right.clamp(0, i64::from(output_width));
    let bottom = bottom.clamp(0, i64::from(output_height));
    (right > left && bottom > top).then_some(CaptureRegion {
        x: left as u32,
        y: top as u32,
        width: (right - left) as u32,
        height: (bottom - top) as u32,
    })
}

/// Convert RGBA pixels read by OpenGL (bottom-left origin) to normal image
/// order in place, without allocating a second full-frame buffer.
pub fn flip_rgba_vertical(pixels: &mut [u8], width: u32, height: u32) {
    let row_bytes = width as usize * 4;
    if row_bytes == 0 || pixels.len() < row_bytes * height as usize {
        return;
    }
    let mut row = vec![0; row_bytes];
    for y in 0..height as usize / 2 {
        let top = y * row_bytes;
        let bottom = (height as usize - 1 - y) * row_bytes;
        row.copy_from_slice(&pixels[top..top + row_bytes]);
        pixels.copy_within(bottom..bottom + row_bytes, top);
        pixels[bottom..bottom + row_bytes].copy_from_slice(&row);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clips_negative_origin_without_shifting_extent() {
        assert_eq!(
            clip_region(100, 100, -10, -5, 30, 20),
            Some(CaptureRegion { x: 0, y: 0, width: 20, height: 15 })
        );
    }

    #[test]
    fn flips_rows_in_place() {
        let mut pixels = vec![1, 1, 1, 1, 2, 2, 2, 2];
        flip_rgba_vertical(&mut pixels, 1, 2);
        assert_eq!(pixels, vec![2, 2, 2, 2, 1, 1, 1, 1]);
    }
}
