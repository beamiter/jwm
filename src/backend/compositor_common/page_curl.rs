//! Book page-turn geometry, shared by the X11 and Wayland compositors.
//!
//! The old workspace is a page hinged on the spine — the monitor edge the
//! turn starts from (left when moving to the next tag, right when moving
//! back). Instead of rotating rigidly like `flip`, the page bends along a
//! circular arc: the surface is parametrized by arc length so the paper
//! never stretches, and the tangent angle grows linearly from the spine to
//! the free edge, which is exactly a circle segment.
//!
//! The curl is zero at both ends of the animation — the first and last
//! frame are the flat workspace — and peaks mid-turn. The free edge leads
//! the spine but is clamped at the landed position, so the page settles
//! tip-first the way paper does. The page bends *away* from the viewer,
//! which keeps the whole surface behind the screen plane: perspective
//! shrinks the receding crest instead of blowing it up through the camera.

use std::f32::consts::PI;

/// Tangent-angle spread (radians) between spine and free edge at the peak
/// of the turn. Larger values roll the page tighter.
const CURL_MAX: f32 = 1.15;

/// Strips the page is tessellated into. Enough that the arc's silhouette
/// and shading read as a smooth curve, few enough to stay cheap.
pub const PAGE_CURL_STRIPS: usize = 24;

/// One vertical strip of the bent page, in card-model space: x spans
/// [-aspect, aspect] across the monitor, y is [-1, 1], z points at the
/// viewer. The strip is the chord between two points of the arc; a renderer
/// places its unit card quad with
/// `translate(mid_x, 0, mid_z) * rotate_y(-angle) * scale(scale_x, 1, 1)`.
#[derive(Clone, Copy, Debug)]
pub struct CurlStrip {
    pub mid_x: f32,
    pub mid_z: f32,
    /// Chord angle from +x toward +z, radians.
    pub angle: f32,
    /// Chord length as a fraction of the full page width.
    pub scale_x: f32,
    /// Texture-u at the strip's local -x edge …
    pub u0: f32,
    /// … and the u span to its +x edge (always positive).
    pub du: f32,
    /// Cosine of the strip's turn angle: 1 lying flat unturned, 0 edge-on,
    /// -1 fully turned over (the viewer sees the page's back).
    pub facing: f32,
}

/// Bend the page for eased progress `t` (0..1). `direction >= 0` hinges the
/// spine on the left edge (turning to the next tag), negative on the right.
/// Strips are returned spine-to-tip; sort by `mid_z` for painter's order.
pub fn page_curl_strips(aspect: f32, direction: f32, t: f32, strips: usize) -> Vec<CurlStrip> {
    let t = t.clamp(0.0, 1.0);
    let d = if direction < 0.0 { -1.0f32 } else { 1.0 };
    let w = 2.0 * aspect.max(1.0e-3);
    let spine_x = -d * aspect;

    // Spine angle sweeps 0 → π; the free edge leads by the curl but never
    // past the landed position, so the page flattens from the tip inward
    // as the turn completes.
    let theta = PI * t;
    let beta = (CURL_MAX * (PI * t).sin()).clamp(0.0, PI - theta);

    // Position of the point at arc length `s` from the spine, in the
    // unmirrored frame: cx grows away from the spine, cz away from the
    // viewer. With a linear tangent angle the integrals are closed-form.
    let point = |s: f32| -> (f32, f32) {
        if beta < 1.0e-4 {
            (s * theta.cos(), s * theta.sin())
        } else {
            let r = w / beta;
            let a = theta + beta * s / w;
            (r * (a.sin() - theta.sin()), r * (theta.cos() - a.cos()))
        }
    };

    let n = strips.max(1);
    (0..n)
        .map(|i| {
            let s0 = w * i as f32 / n as f32;
            let s1 = w * (i + 1) as f32 / n as f32;
            let (ca, za) = point(s0);
            let (cb, zb) = point(s1);
            // Mirror into world x and orient the chord so the quad's normal
            // faces the viewer while the page is unturned.
            let (xa, xb) = (spine_x + d * ca, spine_x + d * cb);
            let (dx, dz) = (d * (xb - xa), -d * (zb - za));
            let chord = (dx * dx + dz * dz).sqrt();
            let alpha_mid = theta + beta * (s0 + s1) * 0.5 / w;
            CurlStrip {
                mid_x: (xa + xb) * 0.5,
                mid_z: -(za + zb) * 0.5,
                angle: dz.atan2(dx),
                scale_x: chord / w,
                u0: if d > 0.0 { s0 / w } else { 1.0 - s1 / w },
                du: (s1 - s0) / w,
                facing: alpha_mid.cos(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ASPECT: f32 = 16.0 / 9.0;

    fn strip_end(strip: &CurlStrip, sign: f32) -> (f32, f32) {
        let half = strip.scale_x * ASPECT * sign;
        (
            strip.mid_x + half * strip.angle.cos(),
            strip.mid_z + half * strip.angle.sin(),
        )
    }

    #[test]
    fn flat_at_both_ends_of_the_animation() {
        for t in [0.0, 1.0] {
            for strip in page_curl_strips(ASPECT, 1.0, t, PAGE_CURL_STRIPS) {
                assert!(strip.mid_z.abs() < 1.0e-4, "t={t} lifted a flat page");
                assert!(strip.angle.abs() < 1.0e-4 || (strip.angle.abs() - PI).abs() < 1.0e-4);
                assert!((strip.scale_x - 1.0 / PAGE_CURL_STRIPS as f32).abs() < 1.0e-4);
            }
        }
    }

    #[test]
    fn first_frame_matches_the_workspace_pixel_for_pixel() {
        let strips = page_curl_strips(ASPECT, 1.0, 0.0, 4);
        // Strip 0 starts at the left edge, strips tile the width contiguously
        // and uv follows along.
        let (x0, _) = strip_end(&strips[0], -1.0);
        assert!((x0 + ASPECT).abs() < 1.0e-4);
        let (x_last, _) = strip_end(&strips[3], 1.0);
        assert!((x_last - ASPECT).abs() < 1.0e-4);
        for (i, strip) in strips.iter().enumerate() {
            assert!((strip.u0 - i as f32 / 4.0).abs() < 1.0e-5);
            assert!((strip.du - 0.25).abs() < 1.0e-5);
            assert!(strip.facing > 0.999);
        }
    }

    #[test]
    fn the_page_curls_away_from_the_viewer() {
        for step in 1..20 {
            let t = step as f32 / 20.0;
            for strip in page_curl_strips(ASPECT, 1.0, t, PAGE_CURL_STRIPS) {
                assert!(
                    strip.mid_z <= 1.0e-4,
                    "t={t} bent toward the camera: {}",
                    strip.mid_z
                );
            }
        }
    }

    #[test]
    fn strips_stay_connected_while_bending() {
        for t in [0.2, 0.5, 0.8] {
            let strips = page_curl_strips(ASPECT, 1.0, t, PAGE_CURL_STRIPS);
            for pair in strips.windows(2) {
                let (x_end, z_end) = strip_end(&pair[0], 1.0);
                let (x_start, z_start) = strip_end(&pair[1], -1.0);
                assert!(
                    (x_end - x_start).abs() < 1.0e-3 && (z_end - z_start).abs() < 1.0e-3,
                    "t={t} tore the page: ({x_end},{z_end}) vs ({x_start},{z_start})"
                );
            }
        }
    }

    #[test]
    fn paper_does_not_stretch() {
        for t in [0.25, 0.5, 0.75] {
            let total: f32 = page_curl_strips(ASPECT, 1.0, t, PAGE_CURL_STRIPS)
                .iter()
                .map(|s| s.scale_x)
                .sum();
            // Chords are a hair shorter than arcs, never longer.
            assert!(total <= 1.0 + 1.0e-4, "t={t} stretched to {total}");
            assert!(total > 0.98, "t={t} shrank to {total}");
        }
    }

    #[test]
    fn turning_back_mirrors_turning_forward() {
        let fwd = page_curl_strips(ASPECT, 1.0, 0.4, 8);
        let back = page_curl_strips(ASPECT, -1.0, 0.4, 8);
        for (f, b) in fwd.iter().zip(&back) {
            assert!((f.mid_x + b.mid_x).abs() < 1.0e-4);
            assert!((f.mid_z - b.mid_z).abs() < 1.0e-4);
            assert!((f.facing - b.facing).abs() < 1.0e-4);
            assert!((f.du - b.du).abs() < 1.0e-5);
        }
        // Mirrored uv: the strip nearest the right-edge spine samples the
        // right edge of the texture.
        assert!((back[0].u0 + back[0].du - 1.0).abs() < 1.0e-5);
    }

    #[test]
    fn the_page_ends_fully_off_screen() {
        for strip in page_curl_strips(ASPECT, 1.0, 1.0, PAGE_CURL_STRIPS) {
            assert!(strip.mid_x <= -ASPECT + 1.0e-3);
            assert!(strip.facing < -0.999, "the landed page shows its back");
        }
    }
}
