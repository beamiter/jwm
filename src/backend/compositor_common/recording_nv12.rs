//! The NV12 packing pass, shared by both compositor backends.
//!
//! Recording reads the composited scene back off the GPU. Doing that as RGBA
//! costs 4 bytes per pixel and leaves the encoder a colour conversion to do;
//! packing NV12 first costs 1.5 bytes per pixel and leaves it nothing. The
//! layout and the colour matrix live here rather than in either backend because
//! two copies of this arithmetic would be two chances for the backends to
//! disagree about what a recorded frame means.
//!
//! The GLSL body is shared and each backend prepends its own version header:
//! the X11 compositor writes desktop GL and lets its shader cache rewrite the
//! source for the GLES path, while the Wayland compositor is GLES throughout.

/// Video dimensions the NV12 layout can express: four pixels share a luma
/// texel, and chroma is subsampled by two vertically.
pub const fn nv12_aligned_size(width: u32, height: u32) -> (u32, u32) {
    (width & !3, height & !1)
}

/// Byte size of one NV12 frame.
pub const fn nv12_frame_bytes(width: u32, height: u32) -> usize {
    (width as usize) * (height as usize) * 3 / 2
}

/// Dimensions of the RGBA8 target the packing pass renders into.
///
/// Four luma bytes ride in one RGBA texel, so the target is a quarter as wide
/// as the video and half again as tall — the Y plane, then NV12's interleaved
/// chroma plane. A row of either plane is `width` bytes, which is what keeps
/// the two contiguous in a single `glReadPixels`.
pub const fn nv12_packed_target_size(width: u32, height: u32) -> (u32, u32) {
    (width / 4, height + height / 2)
}

/// Whether a packed target of this size is within the driver's limits.
///
/// The packed layout is half again as tall as the video, so a 4K recording asks
/// for a 3240-row texture. Desktop GL and every real GLES3 driver allow far
/// more, but ES 3.0 only *requires* 2048, which a 1440p recording already
/// exceeds. Recording falls back to a plain RGBA readback rather than failing
/// when a driver really is that limited.
pub const fn nv12_target_fits(width: u32, height: u32, max_texture_size: u32) -> bool {
    let (packed_w, packed_h) = nv12_packed_target_size(width, height);
    packed_w > 0 && packed_h > 0 && packed_w <= max_texture_size && packed_h <= max_texture_size
}

/// The smallest height cap worth honouring. Below this the scaled picture is
/// not a recording anyone wants, so the cap is treated as absent.
pub const MIN_ENCODED_HEIGHT: u32 = 64;

/// Encoded size for a captured region, honouring a height cap.
///
/// Every downstream cost — readback, the copy out of mapped memory, the pipe,
/// the encoder — scales with the pixel count, so capping a 4K capture to 1080p
/// cuts all of them to a quarter. The scaling is free where the capture already
/// blits the region into a differently sized target.
///
/// A cap that cannot produce a usable picture falls back to the captured
/// resolution rather than refusing to record. That covers both a nonsensically
/// small cap and an extreme aspect ratio — a tall, narrow region under a modest
/// cap scales its width below the four-pixel luma granularity, and snapping that
/// down would otherwise yield a zero-width video.
pub fn recording_output_size(region_w: u32, region_h: u32, max_height: u32) -> (u32, u32) {
    if region_w == 0 || region_h == 0 {
        return (0, 0);
    }
    let native = nv12_aligned_size(region_w, region_h);
    if max_height == 0 || max_height < MIN_ENCODED_HEIGHT || region_h <= max_height {
        return native;
    }
    // Preserve aspect ratio; round the width rather than truncating so a 16:9
    // capture stays 16:9 to within the alignment snap.
    let scaled_w = (u64::from(region_w) * u64::from(max_height) + u64::from(region_h) / 2)
        / u64::from(region_h);
    let scaled = nv12_aligned_size(scaled_w.max(1) as u32, max_height);
    if scaled.0 == 0 || scaled.1 == 0 {
        return native;
    }
    scaled
}

/// Fragment stage of the packing pass, without a version header.
///
/// Rows below `u_luma_rows` hold the Y plane, one texel per four pixels. The
/// rows above hold NV12's interleaved chroma plane: one texel carries (U,V) for
/// two adjacent 2x2 chroma sites, so a chroma row is `width` bytes wide exactly
/// like a luma row and the two regions stay contiguous.
///
/// `gl_FragCoord.y` counts from the bottom of the target, which is also the
/// order `glReadPixels` returns rows in, so the packed regions land in the
/// buffer the right way round without any further flipping. The *image* is
/// flipped here instead, by sampling the bottom-up capture target upside down,
/// which is what lets the encoder drop `-vf vflip`.
///
/// The matrix is BT.709 limited range and the encoder tags the stream to match.
/// That is a deliberate change from the BT.601-on-an-untagged-stream that
/// swscale used to do: ffmpeg-based playback guessed BT.601 and looked right,
/// but mpv, VLC and browsers apply the usual "HD means BT.709" heuristic and
/// showed every recording with a colour shift. Converting and tagging have to
/// change together; tagging alone turns a bug most players hide into one they
/// all show.
pub const NV12_PACK_FRAGMENT_BODY: &str = r#"
uniform sampler2D u_source;
uniform vec2 u_video_size;
uniform float u_luma_rows;
out vec4 frag_color;

vec3 fetch(vec2 pixel) {
    vec2 clamped = clamp(pixel, vec2(0.0), u_video_size - 1.0);
    // Flip: the source target is bottom-up, video rows run top-down.
    vec2 uv = vec2(clamped.x + 0.5, u_video_size.y - 0.5 - clamped.y) / u_video_size;
    return texture(u_source, uv).rgb;
}

// BT.709 limited range: the Rec.709 luma weights scaled by 219/255 with a
// 16/255 pedestal, and the chroma axes scaled by 224/255 about 128/255.
float luma(vec3 c) {
    return 0.182586 * c.r + 0.614231 * c.g + 0.062007 * c.b + 0.062745;
}

vec2 chroma(vec3 c) {
    return vec2(
        -0.100644 * c.r - 0.338572 * c.g + 0.439216 * c.b + 0.501961,
         0.439216 * c.r - 0.398942 * c.g - 0.040274 * c.b + 0.501961
    );
}

void main() {
    vec2 texel = floor(gl_FragCoord.xy);
    if (texel.y < u_luma_rows) {
        float x = texel.x * 4.0;
        frag_color = vec4(
            luma(fetch(vec2(x, texel.y))),
            luma(fetch(vec2(x + 1.0, texel.y))),
            luma(fetch(vec2(x + 2.0, texel.y))),
            luma(fetch(vec2(x + 3.0, texel.y)))
        );
    } else {
        float row = (texel.y - u_luma_rows) * 2.0;
        float x = texel.x * 4.0;
        // Average each 2x2 site in RGB before converting, matching what the
        // CPU converter this replaces also did.
        vec3 left = 0.25 * (fetch(vec2(x, row)) + fetch(vec2(x + 1.0, row))
                          + fetch(vec2(x, row + 1.0)) + fetch(vec2(x + 1.0, row + 1.0)));
        vec3 right = 0.25 * (fetch(vec2(x + 2.0, row)) + fetch(vec2(x + 3.0, row))
                           + fetch(vec2(x + 2.0, row + 1.0)) + fetch(vec2(x + 3.0, row + 1.0)));
        frag_color = vec4(chroma(left), chroma(right));
    }
}
"#;

/// The same conversion in software, for tests to check the shader against.
///
/// Deliberately a separate implementation of the same specification rather than
/// a port of the shader: a test that shares its arithmetic with the code under
/// test cannot catch an error in that arithmetic.
#[cfg(test)]
pub fn reference_nv12(rgb: &[[u8; 3]], width: usize, height: usize) -> Vec<u8> {
    let at = |x: usize, y: usize| rgb[y * width + x];
    let y_of = |c: [u8; 3]| {
        let (r, g, b) = (
            f64::from(c[0]) / 255.0,
            f64::from(c[1]) / 255.0,
            f64::from(c[2]) / 255.0,
        );
        (16.0 + 219.0 * (0.2126 * r + 0.7152 * g + 0.0722 * b)).round()
    };
    let mut out = vec![0u8; width * height * 3 / 2];
    for y in 0..height {
        for x in 0..width {
            out[y * width + x] = y_of(at(x, y)).clamp(0.0, 255.0) as u8;
        }
    }
    let chroma_base = width * height;
    for cy in 0..height / 2 {
        for cx in 0..width / 2 {
            let mut sum = [0.0f64; 3];
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let c = at(2 * cx + dx, 2 * cy + dy);
                for channel in 0..3 {
                    sum[channel] += f64::from(c[channel]) / 255.0;
                }
            }
            let (r, g, b) = (sum[0] / 4.0, sum[1] / 4.0, sum[2] / 4.0);
            let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            let cb = 128.0 + 224.0 * ((b - luma) / 1.8556);
            let cr = 128.0 + 224.0 * ((r - luma) / 1.5748);
            let index = chroma_base + cy * width + cx * 2;
            out[index] = cb.round().clamp(0.0, 255.0) as u8;
            out[index + 1] = cr.round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_packed_target_holds_exactly_one_nv12_frame() {
        for (w, h) in [(1920, 1080), (2560, 1440), (3840, 2160), (1280, 720)] {
            let (packed_w, packed_h) = nv12_packed_target_size(w, h);
            assert_eq!(
                (packed_w * packed_h * 4) as usize,
                nv12_frame_bytes(w, h),
                "packed target does not match the frame size at {w}x{h}"
            );
        }
    }

    #[test]
    fn nv12_is_markedly_smaller_than_rgba() {
        assert_eq!(nv12_frame_bytes(1920, 1080), 3_110_400);
        assert_eq!(1920 * 1080 * 4 - nv12_frame_bytes(1920, 1080), 5_184_000);
    }

    #[test]
    fn sizes_snap_down_to_what_the_layout_can_express() {
        assert_eq!(nv12_aligned_size(1920, 1080), (1920, 1080));
        assert_eq!(nv12_aligned_size(1366, 768), (1364, 768));
        for (w, h) in [(1u32, 1u32), (3, 3), (7, 5), (1919, 1079)] {
            let (aw, ah) = nv12_aligned_size(w, h);
            assert!(aw <= w && ah <= h);
            assert_eq!(aw % 4, 0);
            assert_eq!(ah % 2, 0);
        }
    }

    #[test]
    fn a_height_cap_scales_down_and_keeps_the_aspect_ratio() {
        assert_eq!(recording_output_size(3840, 2160, 0), (3840, 2160));
        assert_eq!(recording_output_size(3840, 2160, 1080), (1920, 1080));
        assert_eq!(recording_output_size(3840, 2160, 720), (1280, 720));
        assert_eq!(recording_output_size(2560, 1440, 1080), (1920, 1080));
    }

    #[test]
    fn a_cap_that_cannot_make_a_picture_falls_back_to_the_capture() {
        assert_eq!(recording_output_size(1920, 1080, 1), (1920, 1080));
        assert_eq!(recording_output_size(1920, 1080, 32), (1920, 1080));
        assert_eq!(recording_output_size(16, 1080, 64), (16, 1080));
        assert_eq!(recording_output_size(1920, 1080, 64), (112, 64));
    }

    #[test]
    fn no_capped_size_is_ever_degenerate_for_a_real_region() {
        for region_w in [4u32, 16, 64, 320, 1920, 3440] {
            for region_h in [2u32, 64, 768, 1080, 1440, 2160] {
                for cap in [0u32, 1, 32, 64, 240, 720, 1080, 4320] {
                    let (w, h) = recording_output_size(region_w, region_h, cap);
                    if nv12_aligned_size(region_w, region_h).0 == 0 {
                        continue;
                    }
                    assert!(w > 0 && h > 0, "{region_w}x{region_h} cap {cap} -> {w}x{h}");
                    assert_eq!(w % 4, 0);
                    assert_eq!(h % 2, 0);
                }
            }
        }
    }

    #[test]
    fn a_driver_at_the_gles3_minimum_cannot_hold_a_1440p_packing_target() {
        // ES 3.0 only requires 2048; 1440p needs 2160 rows.
        assert!(!nv12_target_fits(2560, 1440, 2048));
        assert!(nv12_target_fits(1920, 1080, 2048));
        assert!(nv12_target_fits(3840, 2160, 8192));
    }
}
