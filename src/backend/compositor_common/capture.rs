//! Pixel-capture rules shared by every compositor backend.

/// Per-capture CPU/readback budget, independent of the request-count budget.
/// Dimensions also have to fit GLsizei before crossing a graphics FFI boundary.
pub const MAX_RGBA_CAPTURE_BYTES: usize = 512 * 1024 * 1024;

pub fn rgba_capture_len(width: u32, height: u32) -> std::io::Result<usize> {
    let invalid = || {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "capture dimensions are empty, unrepresentable, or exceed the 512 MiB RGBA budget",
        )
    };
    if width == 0 || height == 0 || width > i32::MAX as u32 || height > i32::MAX as u32 {
        return Err(invalid());
    }
    let len = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(invalid)?;
    if len > MAX_RGBA_CAPTURE_BYTES {
        return Err(invalid());
    }
    Ok(len)
}

pub fn allocate_rgba_capture(width: u32, height: u32) -> std::io::Result<Vec<u8>> {
    let len = rgba_capture_len(width, height)?;
    let mut pixels = Vec::new();
    pixels
        .try_reserve_exact(len)
        .map_err(std::io::Error::other)?;
    pixels.resize(len, 0);
    Ok(pixels)
}

pub fn copy_rgba_capture(source: &[u8], width: u32, height: u32) -> std::io::Result<Vec<u8>> {
    let len = rgba_capture_len(width, height)?;
    if source.len() != len {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "capture mapping has an unexpected RGBA length",
        ));
    }
    let mut pixels = allocate_rgba_capture(width, height)?;
    pixels.copy_from_slice(source);
    Ok(pixels)
}

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
    let Some(row_bytes) = usize::try_from(width)
        .ok()
        .and_then(|width| width.checked_mul(4))
    else {
        return;
    };
    let Some(height) = usize::try_from(height).ok() else {
        return;
    };
    let Some(required_bytes) = row_bytes.checked_mul(height) else {
        return;
    };
    if row_bytes == 0 || pixels.len() < required_bytes {
        return;
    }
    for y in 0..height / 2 {
        let top = y * row_bytes;
        let bottom = (height - 1 - y) * row_bytes;
        let (upper, lower) = pixels.split_at_mut(bottom);
        upper[top..top + row_bytes].swap_with_slice(&mut lower[..row_bytes]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clips_negative_origin_without_shifting_extent() {
        assert_eq!(
            clip_region(100, 100, -10, -5, 30, 20),
            Some(CaptureRegion {
                x: 0,
                y: 0,
                width: 20,
                height: 15
            })
        );
    }

    #[test]
    fn flips_rows_in_place() {
        let mut pixels = vec![1, 1, 1, 1, 2, 2, 2, 2];
        flip_rgba_vertical(&mut pixels, 1, 2);
        assert_eq!(pixels, vec![2, 2, 2, 2, 1, 1, 1, 1]);
    }

    #[test]
    fn flips_odd_rows_without_touching_middle_or_trailing_storage() {
        let mut pixels = [1; 28];
        pixels[8..16].fill(2);
        pixels[16..24].fill(3);
        pixels[24..].fill(9);
        flip_rgba_vertical(&mut pixels, 2, 3);
        assert_eq!(&pixels[..8], &[3; 8]);
        assert_eq!(&pixels[8..16], &[2; 8]);
        assert_eq!(&pixels[16..24], &[1; 8]);
        assert_eq!(&pixels[24..], &[9; 4]);
    }

    #[test]
    fn rejects_unrepresentable_capture_sizes_without_overflow() {
        let mut pixels = [1, 2, 3, 4];
        flip_rgba_vertical(&mut pixels, u32::MAX, u32::MAX);
        assert_eq!(pixels, [1, 2, 3, 4]);
    }
}

#[cfg(test)]
mod capture_budget_tests {
    use super::*;

    #[test]
    fn capture_sizes_are_checked_before_allocating_or_calling_gl() {
        assert_eq!(rgba_capture_len(1920, 1080).unwrap(), 8_294_400);
        assert_eq!(
            rgba_capture_len(16384, 8192).unwrap(),
            MAX_RGBA_CAPTURE_BYTES
        );
        for (width, height) in [
            (0, 1),
            (1, 0),
            (32768, 32768),
            (u32::MAX, 1),
            (1, u32::MAX),
            (u32::MAX, u32::MAX),
            (16385, 8192),
        ] {
            assert!(
                rgba_capture_len(width, height).is_err(),
                "accepted {width}x{height}"
            );
        }
        // Old u32 multiplication wrapped to zero in release at this extent.
        assert_eq!(32768u32.wrapping_mul(32768).wrapping_mul(4), 0);
    }

    #[test]
    fn small_capture_allocation_and_mapping_length_are_exact() {
        assert_eq!(allocate_rgba_capture(2, 1).unwrap(), [0; 8]);
        assert_eq!(
            copy_rgba_capture(&[1, 2, 3, 4], 1, 1).unwrap(),
            [1, 2, 3, 4]
        );
        assert!(copy_rgba_capture(&[1, 2, 3], 1, 1).is_err());
        assert!(copy_rgba_capture(&[1, 2, 3, 4, 5], 1, 1).is_err());
    }
}
