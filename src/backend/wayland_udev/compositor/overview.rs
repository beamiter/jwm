use super::*;
use crate::backend::compositor_common::math::{
    mat4_mul, rotate_y_matrix, scale_matrix, translate_matrix,
};
use crate::backend::compositor_common::prism::{
    MAX_PRISM_SIDES, MIN_PRISM_SIDES, PrismCamera, PrismKind, build_prism_pieces,
};
use smithay::backend::renderer::gles::ffi;

/// Share of the owning monitor's height covered by the front face. Keep this
/// identical to X11: both backends now frame the same prism geometry.
const PRISM_FACE_FILL: f32 = 0.56;
/// Screen-space baseline of the front face, leaving room for its title.
const PRISM_BASE_LINE: f32 = 0.84;
const TITLE_SCALE: f32 = 2.0;
const TITLE_MARGIN: f32 = 8.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrismEntryAvailability {
    Live,
    MissingWindow,
    MissingTexture,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrismFillerReason {
    Unoccupied,
    MissingWindow,
    MissingTexture,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrismFaceSource {
    Live {
        entry_index: usize,
    },
    Filler {
        entry_index: Option<usize>,
        reason: PrismFillerReason,
    },
}

/// Resolve every geometric face slot to either live window content or an
/// explicit filler. The returned vector is always exactly `sides` long, so a
/// transiently missing surface can never turn the solid back into an open fan.
fn prism_face_plan(sides: usize, availability: &[PrismEntryAvailability]) -> Vec<PrismFaceSource> {
    (0..sides)
        .map(|slot| match availability.get(slot).copied() {
            Some(PrismEntryAvailability::Live) => PrismFaceSource::Live { entry_index: slot },
            Some(PrismEntryAvailability::MissingWindow) => PrismFaceSource::Filler {
                entry_index: Some(slot),
                reason: PrismFillerReason::MissingWindow,
            },
            Some(PrismEntryAvailability::MissingTexture) => PrismFaceSource::Filler {
                entry_index: Some(slot),
                reason: PrismFillerReason::MissingTexture,
            },
            None => PrismFaceSource::Filler {
                entry_index: None,
                reason: PrismFillerReason::Unoccupied,
            },
        })
        .collect()
}

fn max_title_texture_width(monitor_width: u32) -> u32 {
    let available = (monitor_width.saturating_sub((TITLE_MARGIN * 2.0) as u32) as f32 / TITLE_SCALE)
        .floor() as u32;
    (monitor_width / 3).max(120).min(available.max(1))
}

/// Rotation that brings the selected prism face squarely toward the camera.
///
/// One- and two-window overviews deliberately reuse the first faces of a
/// triangle, matching the shared prism geometry.
pub(super) fn prism_target_rotation(entry_count: usize, selected_index: usize) -> f32 {
    if entry_count == 0 {
        return 0.0;
    }

    let sides = entry_count.clamp(MIN_PRISM_SIDES, MAX_PRISM_SIDES);
    -((selected_index % sides) as f32) * std::f32::consts::TAU / sides as f32
}

/// Defensive renderer-side bound for callers that have not already applied
/// the overview policy's six-window sliding subset.
pub(super) fn prism_entry_range(
    entry_count: usize,
    selected_index: usize,
) -> std::ops::Range<usize> {
    if entry_count <= MAX_PRISM_SIDES {
        return 0..entry_count;
    }

    let selected_index = selected_index.min(entry_count - 1);
    let start = selected_index
        .saturating_sub(MAX_PRISM_SIDES / 2)
        .min(entry_count - MAX_PRISM_SIDES);
    start..start + MAX_PRISM_SIDES
}

// ---------------------------------------------------------------------------
// Minimal 6x10 bitmap font (ASCII 32-126, 95 chars x 10 bytes = 950 bytes)
// Each byte: lower 6 bits represent pixel columns left-to-right for one row.
// ---------------------------------------------------------------------------

#[allow(dead_code)]
#[rustfmt::skip]
const FONT_6X10: &[u8; 950] = &[
    // 32: space
    0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,
    // 33: !
    0x04,0x04,0x04,0x04,0x04,0x04,0x00,0x04,0x00,0x00,
    // 34: "
    0x0A,0x0A,0x0A,0x00,0x00,0x00,0x00,0x00,0x00,0x00,
    // 35: #
    0x0A,0x0A,0x1F,0x0A,0x1F,0x0A,0x0A,0x00,0x00,0x00,
    // 36: $
    0x04,0x0F,0x14,0x0E,0x05,0x1E,0x04,0x00,0x00,0x00,
    // 37: %
    0x18,0x19,0x02,0x04,0x08,0x13,0x03,0x00,0x00,0x00,
    // 38: &
    0x08,0x14,0x14,0x08,0x15,0x12,0x0D,0x00,0x00,0x00,
    // 39: '
    0x04,0x04,0x08,0x00,0x00,0x00,0x00,0x00,0x00,0x00,
    // 40: (
    0x02,0x04,0x08,0x08,0x08,0x04,0x02,0x00,0x00,0x00,
    // 41: )
    0x08,0x04,0x02,0x02,0x02,0x04,0x08,0x00,0x00,0x00,
    // 42: *
    0x00,0x04,0x15,0x0E,0x15,0x04,0x00,0x00,0x00,0x00,
    // 43: +
    0x00,0x04,0x04,0x1F,0x04,0x04,0x00,0x00,0x00,0x00,
    // 44: ,
    0x00,0x00,0x00,0x00,0x00,0x04,0x04,0x08,0x00,0x00,
    // 45: -
    0x00,0x00,0x00,0x1F,0x00,0x00,0x00,0x00,0x00,0x00,
    // 46: .
    0x00,0x00,0x00,0x00,0x00,0x00,0x04,0x00,0x00,0x00,
    // 47: /
    0x01,0x01,0x02,0x04,0x08,0x10,0x10,0x00,0x00,0x00,
    // 48: 0
    0x0E,0x11,0x13,0x15,0x19,0x11,0x0E,0x00,0x00,0x00,
    // 49: 1
    0x04,0x0C,0x04,0x04,0x04,0x04,0x0E,0x00,0x00,0x00,
    // 50: 2
    0x0E,0x11,0x01,0x06,0x08,0x10,0x1F,0x00,0x00,0x00,
    // 51: 3
    0x0E,0x11,0x01,0x06,0x01,0x11,0x0E,0x00,0x00,0x00,
    // 52: 4
    0x02,0x06,0x0A,0x12,0x1F,0x02,0x02,0x00,0x00,0x00,
    // 53: 5
    0x1F,0x10,0x1E,0x01,0x01,0x11,0x0E,0x00,0x00,0x00,
    // 54: 6
    0x06,0x08,0x10,0x1E,0x11,0x11,0x0E,0x00,0x00,0x00,
    // 55: 7
    0x1F,0x01,0x02,0x04,0x08,0x08,0x08,0x00,0x00,0x00,
    // 56: 8
    0x0E,0x11,0x11,0x0E,0x11,0x11,0x0E,0x00,0x00,0x00,
    // 57: 9
    0x0E,0x11,0x11,0x0F,0x01,0x02,0x0C,0x00,0x00,0x00,
    // 58: :
    0x00,0x00,0x04,0x00,0x00,0x04,0x00,0x00,0x00,0x00,
    // 59: ;
    0x00,0x00,0x04,0x00,0x00,0x04,0x04,0x08,0x00,0x00,
    // 60: <
    0x02,0x04,0x08,0x10,0x08,0x04,0x02,0x00,0x00,0x00,
    // 61: =
    0x00,0x00,0x1F,0x00,0x1F,0x00,0x00,0x00,0x00,0x00,
    // 62: >
    0x08,0x04,0x02,0x01,0x02,0x04,0x08,0x00,0x00,0x00,
    // 63: ?
    0x0E,0x11,0x01,0x02,0x04,0x00,0x04,0x00,0x00,0x00,
    // 64: @
    0x0E,0x11,0x17,0x15,0x17,0x10,0x0E,0x00,0x00,0x00,
    // 65: A
    0x0E,0x11,0x11,0x1F,0x11,0x11,0x11,0x00,0x00,0x00,
    // 66: B
    0x1E,0x11,0x11,0x1E,0x11,0x11,0x1E,0x00,0x00,0x00,
    // 67: C
    0x0E,0x11,0x10,0x10,0x10,0x11,0x0E,0x00,0x00,0x00,
    // 68: D
    0x1E,0x11,0x11,0x11,0x11,0x11,0x1E,0x00,0x00,0x00,
    // 69: E
    0x1F,0x10,0x10,0x1E,0x10,0x10,0x1F,0x00,0x00,0x00,
    // 70: F
    0x1F,0x10,0x10,0x1E,0x10,0x10,0x10,0x00,0x00,0x00,
    // 71: G
    0x0E,0x11,0x10,0x17,0x11,0x11,0x0F,0x00,0x00,0x00,
    // 72: H
    0x11,0x11,0x11,0x1F,0x11,0x11,0x11,0x00,0x00,0x00,
    // 73: I
    0x0E,0x04,0x04,0x04,0x04,0x04,0x0E,0x00,0x00,0x00,
    // 74: J
    0x07,0x02,0x02,0x02,0x02,0x12,0x0C,0x00,0x00,0x00,
    // 75: K
    0x11,0x12,0x14,0x18,0x14,0x12,0x11,0x00,0x00,0x00,
    // 76: L
    0x10,0x10,0x10,0x10,0x10,0x10,0x1F,0x00,0x00,0x00,
    // 77: M
    0x11,0x1B,0x15,0x15,0x11,0x11,0x11,0x00,0x00,0x00,
    // 78: N
    0x11,0x19,0x15,0x13,0x11,0x11,0x11,0x00,0x00,0x00,
    // 79: O
    0x0E,0x11,0x11,0x11,0x11,0x11,0x0E,0x00,0x00,0x00,
    // 80: P
    0x1E,0x11,0x11,0x1E,0x10,0x10,0x10,0x00,0x00,0x00,
    // 81: Q
    0x0E,0x11,0x11,0x11,0x15,0x12,0x0D,0x00,0x00,0x00,
    // 82: R
    0x1E,0x11,0x11,0x1E,0x14,0x12,0x11,0x00,0x00,0x00,
    // 83: S
    0x0E,0x11,0x10,0x0E,0x01,0x11,0x0E,0x00,0x00,0x00,
    // 84: T
    0x1F,0x04,0x04,0x04,0x04,0x04,0x04,0x00,0x00,0x00,
    // 85: U
    0x11,0x11,0x11,0x11,0x11,0x11,0x0E,0x00,0x00,0x00,
    // 86: V
    0x11,0x11,0x11,0x11,0x0A,0x0A,0x04,0x00,0x00,0x00,
    // 87: W
    0x11,0x11,0x11,0x15,0x15,0x1B,0x11,0x00,0x00,0x00,
    // 88: X
    0x11,0x11,0x0A,0x04,0x0A,0x11,0x11,0x00,0x00,0x00,
    // 89: Y
    0x11,0x11,0x0A,0x04,0x04,0x04,0x04,0x00,0x00,0x00,
    // 90: Z
    0x1F,0x01,0x02,0x04,0x08,0x10,0x1F,0x00,0x00,0x00,
    // 91: [
    0x0E,0x08,0x08,0x08,0x08,0x08,0x0E,0x00,0x00,0x00,
    // 92: backslash
    0x10,0x10,0x08,0x04,0x02,0x01,0x01,0x00,0x00,0x00,
    // 93: ]
    0x0E,0x02,0x02,0x02,0x02,0x02,0x0E,0x00,0x00,0x00,
    // 94: ^
    0x04,0x0A,0x11,0x00,0x00,0x00,0x00,0x00,0x00,0x00,
    // 95: _
    0x00,0x00,0x00,0x00,0x00,0x00,0x1F,0x00,0x00,0x00,
    // 96: `
    0x08,0x04,0x02,0x00,0x00,0x00,0x00,0x00,0x00,0x00,
    // 97: a
    0x00,0x00,0x0E,0x01,0x0F,0x11,0x0F,0x00,0x00,0x00,
    // 98: b
    0x10,0x10,0x1E,0x11,0x11,0x11,0x1E,0x00,0x00,0x00,
    // 99: c
    0x00,0x00,0x0E,0x11,0x10,0x11,0x0E,0x00,0x00,0x00,
    // 100: d
    0x01,0x01,0x0F,0x11,0x11,0x11,0x0F,0x00,0x00,0x00,
    // 101: e
    0x00,0x00,0x0E,0x11,0x1F,0x10,0x0E,0x00,0x00,0x00,
    // 102: f
    0x06,0x08,0x1E,0x08,0x08,0x08,0x08,0x00,0x00,0x00,
    // 103: g
    0x00,0x00,0x0F,0x11,0x11,0x0F,0x01,0x0E,0x00,0x00,
    // 104: h
    0x10,0x10,0x1E,0x11,0x11,0x11,0x11,0x00,0x00,0x00,
    // 105: i
    0x04,0x00,0x0C,0x04,0x04,0x04,0x0E,0x00,0x00,0x00,
    // 106: j
    0x02,0x00,0x06,0x02,0x02,0x02,0x12,0x0C,0x00,0x00,
    // 107: k
    0x10,0x10,0x12,0x14,0x18,0x14,0x12,0x00,0x00,0x00,
    // 108: l
    0x0C,0x04,0x04,0x04,0x04,0x04,0x0E,0x00,0x00,0x00,
    // 109: m
    0x00,0x00,0x1A,0x15,0x15,0x15,0x15,0x00,0x00,0x00,
    // 110: n
    0x00,0x00,0x1E,0x11,0x11,0x11,0x11,0x00,0x00,0x00,
    // 111: o
    0x00,0x00,0x0E,0x11,0x11,0x11,0x0E,0x00,0x00,0x00,
    // 112: p
    0x00,0x00,0x1E,0x11,0x11,0x1E,0x10,0x10,0x00,0x00,
    // 113: q
    0x00,0x00,0x0F,0x11,0x11,0x0F,0x01,0x01,0x00,0x00,
    // 114: r
    0x00,0x00,0x16,0x19,0x10,0x10,0x10,0x00,0x00,0x00,
    // 115: s
    0x00,0x00,0x0F,0x10,0x0E,0x01,0x1E,0x00,0x00,0x00,
    // 116: t
    0x08,0x08,0x1E,0x08,0x08,0x09,0x06,0x00,0x00,0x00,
    // 117: u
    0x00,0x00,0x11,0x11,0x11,0x11,0x0F,0x00,0x00,0x00,
    // 118: v
    0x00,0x00,0x11,0x11,0x11,0x0A,0x04,0x00,0x00,0x00,
    // 119: w
    0x00,0x00,0x11,0x11,0x15,0x15,0x0A,0x00,0x00,0x00,
    // 120: x
    0x00,0x00,0x11,0x0A,0x04,0x0A,0x11,0x00,0x00,0x00,
    // 121: y
    0x00,0x00,0x11,0x11,0x11,0x0F,0x01,0x0E,0x00,0x00,
    // 122: z
    0x00,0x00,0x1F,0x02,0x04,0x08,0x1F,0x00,0x00,0x00,
    // 123: {
    0x02,0x04,0x04,0x08,0x04,0x04,0x02,0x00,0x00,0x00,
    // 124: |
    0x04,0x04,0x04,0x04,0x04,0x04,0x04,0x00,0x00,0x00,
    // 125: }
    0x08,0x04,0x04,0x02,0x04,0x04,0x08,0x00,0x00,0x00,
    // 126: ~
    0x00,0x00,0x08,0x15,0x02,0x00,0x00,0x00,0x00,0x00,
];

#[derive(Debug, Clone, PartialEq)]
struct OverviewStripWindowSegment {
    y_ratio: f32,
    height_ratio: f32,
    focused: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct OverviewStripSegment {
    x_ratio: f32,
    width_ratio: f32,
    focused: bool,
    windows: Vec<OverviewStripWindowSegment>,
}

fn overview_strip_segments(entries: &[OverviewEntry]) -> Vec<OverviewStripSegment> {
    let mut segments: Vec<OverviewStripSegment> = Vec::new();

    for entry in entries {
        let x_ratio = entry.x.clamp(0.0, 1.0);
        let width_ratio = entry.w.clamp(0.0, 1.0 - x_ratio);
        if width_ratio <= 0.0001 {
            continue;
        }

        let window = OverviewStripWindowSegment {
            y_ratio: entry.y.clamp(0.0, 1.0),
            height_ratio: entry.h.clamp(0.0, 1.0).max(0.0001),
            focused: entry.focused,
        };

        if let Some(segment) = segments.iter_mut().find(|segment| {
            (segment.x_ratio - x_ratio).abs() < 0.0005
                && (segment.width_ratio - width_ratio).abs() < 0.0005
        }) {
            segment.focused |= entry.focused;
            segment.windows.push(window);
        } else {
            segments.push(OverviewStripSegment {
                x_ratio,
                width_ratio,
                focused: entry.focused,
                windows: vec![window],
            });
        }
    }

    segments.sort_by(|a, b| {
        a.x_ratio
            .partial_cmp(&b.x_ratio)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    segments
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

impl WaylandCompositor {
    fn project_overview_point(
        mvp: &[f32; 16],
        model_pt: [f32; 3],
        vp_w: f32,
        vp_h: f32,
        vp_x: f32,
        vp_y: f32,
    ) -> Option<(f32, f32)> {
        let [mx, my, mz] = model_pt;
        let clip_x = mvp[0] * mx + mvp[4] * my + mvp[8] * mz + mvp[12];
        let clip_y = mvp[1] * mx + mvp[5] * my + mvp[9] * mz + mvp[13];
        let clip_w = mvp[3] * mx + mvp[7] * my + mvp[11] * mz + mvp[15];
        if clip_w.abs() <= f32::EPSILON || !clip_w.is_finite() {
            return None;
        }
        let ndc_x = clip_x / clip_w;
        let ndc_y = clip_y / clip_w;
        if !ndc_x.is_finite() || !ndc_y.is_finite() {
            return None;
        }
        let sx = (ndc_x * 0.5 + 0.5) * vp_w + vp_x;
        let sy = (1.0 - (ndc_y * 0.5 + 0.5)) * vp_h + vp_y;
        Some((sx, sy))
    }

    /// Rasterize a title string into RGBA pixels using the built-in bitmap font.
    /// Returns (pixels, width, height) or None if title is empty.
    #[allow(dead_code)]
    pub(crate) fn render_title_to_pixels(
        title: &str,
        max_width: u32,
    ) -> Option<(Vec<u8>, u32, u32)> {
        if title.is_empty() {
            return None;
        }

        const CHAR_W: u32 = 6;
        const CHAR_H: u32 = 10;
        const PADDING: u32 = 2;

        let chars: Vec<u8> = title.bytes().collect();
        let text_width = (chars.len() as u32) * CHAR_W;
        let img_w = text_width.min(max_width);
        let img_h = CHAR_H + PADDING * 2;
        let max_chars = (img_w / CHAR_W) as usize;
        let render_chars = chars.len().min(max_chars);

        let mut pixels = vec![0u8; (img_w * img_h * 4) as usize];

        for (ci, &ch) in chars[..render_chars].iter().enumerate() {
            let glyph_idx = if ch >= 32 && ch <= 126 {
                (ch - 32) as usize
            } else {
                0 // render space for non-ASCII
            };
            let glyph = &FONT_6X10[glyph_idx * 10..(glyph_idx + 1) * 10];

            for row in 0..CHAR_H {
                let bits = glyph[row as usize];
                for col in 0..CHAR_W {
                    let px = (ci as u32) * CHAR_W + col;
                    let py = row + PADDING;
                    if px >= img_w {
                        break;
                    }
                    // Bit 5 is leftmost pixel, bit 0 is rightmost
                    let bit = (bits >> (CHAR_W - 1 - col)) & 1;
                    if bit != 0 {
                        let offset = ((py * img_w + px) * 4) as usize;
                        pixels[offset] = 255; // R
                        pixels[offset + 1] = 255; // G
                        pixels[offset + 2] = 255; // B
                        pixels[offset + 3] = 255; // A
                    }
                }
            }
        }

        Some((pixels, img_w, img_h))
    }

    /// Create GL textures for overview entry titles.
    /// Stores texture IDs in `self.overview_title_textures`.
    pub(crate) fn create_overview_title_textures(&mut self, gl: &ffi::Gles2) {
        self.clear_overview_textures(gl);

        let max_label_width = max_title_texture_width(self.overview_monitor.2.max(1));
        let mut textures = Vec::with_capacity(self.overview_entries.len());

        for entry in &self.overview_entries {
            if let Some((pixels, w, h)) =
                Self::render_title_to_pixels(&entry.title, max_label_width)
            {
                let mut tex = 0u32;
                unsafe {
                    gl.GenTextures(1, &mut tex);
                    gl.BindTexture(ffi::TEXTURE_2D, tex);
                    gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MIN_FILTER, ffi::LINEAR as i32);
                    gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MAG_FILTER, ffi::LINEAR as i32);
                    gl.TexParameteri(
                        ffi::TEXTURE_2D,
                        ffi::TEXTURE_WRAP_S,
                        ffi::CLAMP_TO_EDGE as i32,
                    );
                    gl.TexParameteri(
                        ffi::TEXTURE_2D,
                        ffi::TEXTURE_WRAP_T,
                        ffi::CLAMP_TO_EDGE as i32,
                    );
                    gl.TexImage2D(
                        ffi::TEXTURE_2D,
                        0,
                        ffi::RGBA as i32,
                        w as i32,
                        h as i32,
                        0,
                        ffi::RGBA,
                        ffi::UNSIGNED_BYTE,
                        pixels.as_ptr() as *const _,
                    );
                }
                textures.push(tex);
            } else {
                textures.push(0);
            }
        }

        self.overview_title_textures = textures;
        self.overview_titles_dirty = false;
    }

    fn render_overview_scroll_strip(
        &self,
        gl: &ffi::Gles2,
        projection: &[f32; 16],
        segments: &[OverviewStripSegment],
    ) {
        if segments.is_empty() {
            return;
        }

        let (mon_x, mon_y, mon_w, mon_h) = self.overview_monitor;
        let mw = mon_w.max(1) as f32;
        let mh = mon_h.max(1) as f32;
        let margin = (mw * 0.055).clamp(24.0, 72.0);
        let strip_x = mon_x as f32 + margin;
        let strip_w = (mw - margin * 2.0).max(24.0);
        let strip_h = 12.0f32;
        let strip_y = mon_y as f32 + (mh - 34.0).max(12.0);
        let opacity = self.overview_opacity.clamp(0.0, 1.0);

        unsafe {
            gl.Disable(ffi::SCISSOR_TEST);
            gl.Viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
            gl.UseProgram(self.border_program);
            gl.UniformMatrix4fv(
                self.border_uniforms.projection,
                1,
                ffi::FALSE as u8,
                projection.as_ptr(),
            );
            gl.BindVertexArray(self.quad_vao);

            self.draw_overview_strip_rect(
                gl,
                strip_x,
                strip_y,
                strip_w,
                strip_h,
                6.0,
                [0.02, 0.025, 0.035, 0.48 * opacity],
            );

            let gap = 3.0f32.min(strip_w / 80.0);
            for segment in segments {
                let x = strip_x + segment.x_ratio * strip_w + gap * 0.5;
                let w = (segment.width_ratio * strip_w - gap).max(1.0);
                if w <= 1.0 {
                    continue;
                }

                let color = if segment.focused {
                    [0.34, 0.68, 1.0, 0.88 * opacity]
                } else {
                    [0.30, 0.36, 0.46, 0.72 * opacity]
                };
                self.draw_overview_strip_rect(gl, x, strip_y + 2.0, w, strip_h - 4.0, 4.0, color);

                if segment.windows.len() > 1 {
                    for window in &segment.windows {
                        let wx = x + window.y_ratio * w + 0.75;
                        let ww = (window.height_ratio * w - 1.5).max(1.0);
                        let tick_color = if window.focused {
                            [0.92, 0.97, 1.0, 0.95 * opacity]
                        } else {
                            [0.78, 0.84, 0.92, 0.62 * opacity]
                        };
                        self.draw_overview_strip_rect(
                            gl,
                            wx,
                            strip_y + 4.0,
                            ww,
                            strip_h - 8.0,
                            2.0,
                            tick_color,
                        );
                    }
                }
            }
        }
    }

    fn draw_overview_strip_rect(
        &self,
        gl: &ffi::Gles2,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radius: f32,
        color: [f32; 4],
    ) {
        if w <= 0.0 || h <= 0.0 || color[3] <= 0.0 {
            return;
        }

        unsafe {
            gl.Uniform4f(self.border_uniforms.rect, x, y, w, h);
            gl.Uniform4f(
                self.border_uniforms.border_color,
                color[0],
                color[1],
                color[2],
                color[3],
            );
            gl.Uniform2f(self.border_uniforms.size, w, h);
            let radius = radius.min(w * 0.5).min(h * 0.5);
            gl.Uniform1f(self.border_uniforms.radius, radius);
            gl.Uniform1f(self.border_uniforms.radius_top, radius);
            gl.Uniform1f(self.border_uniforms.border_width, w.max(h));
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
        }
    }

    /// Render the 3D prism carousel overview.
    ///
    /// Three through six windows form the matching regular solid (four is a
    /// real cube); one or two windows occupy the readable faces of a triangle.
    /// The selected face rotates to the front.
    pub(crate) fn render_overview(
        &mut self,
        gl: &ffi::Gles2,
        projection: &[f32; 16],
        scene_linear_output: bool,
    ) {
        if self.overview_opacity <= 0.0 {
            return;
        }

        let n = self.overview_entries.len();
        if n == 0 {
            return;
        }
        if self.overview_titles_dirty || self.overview_title_textures.len() != n {
            self.create_overview_title_textures(gl);
        }
        let strip_segments = overview_strip_segments(&self.overview_entries);

        unsafe {
            self.enable_premultiplied_blend(gl);
            // The shared `PrismPiece` order is the depth contract. Keep the
            // overlay independent of state left by a previous renderer pass.
            gl.Disable(ffi::DEPTH_TEST);
            gl.Disable(ffi::CULL_FACE);

            // ------------------------------------------------------------------
            // 1. Dark vignette backdrop
            // ------------------------------------------------------------------
            gl.UseProgram(self.overview_bg_program);
            let rect_loc =
                gl.GetUniformLocation(self.overview_bg_program, b"u_rect\0".as_ptr() as *const _);
            let proj_loc = gl.GetUniformLocation(
                self.overview_bg_program,
                b"u_projection\0".as_ptr() as *const _,
            );
            let opacity_loc = gl.GetUniformLocation(
                self.overview_bg_program,
                b"u_opacity\0".as_ptr() as *const _,
            );
            let scene_linear_loc = gl.GetUniformLocation(
                self.overview_bg_program,
                b"u_scene_linear\0".as_ptr() as *const _,
            );

            let (mon_x, mon_y, mon_w, mon_h) = self.overview_monitor;
            let mw = mon_w.max(1) as f32;
            let mh = mon_h.max(1) as f32;

            if rect_loc >= 0 {
                gl.Uniform4f(rect_loc, mon_x as f32, mon_y as f32, mw, mh);
            }
            if proj_loc >= 0 {
                gl.UniformMatrix4fv(proj_loc, 1, ffi::FALSE as u8, projection.as_ptr());
            }
            if opacity_loc >= 0 {
                gl.Uniform1f(opacity_loc, self.overview_opacity);
            }
            if scene_linear_loc >= 0 {
                gl.Uniform1i(scene_linear_loc, i32::from(scene_linear_output));
            }

            gl.BindVertexArray(self.quad_vao);
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);

            let scissor_y = self.screen_h as i32 - (mon_y + mon_h as i32);
            gl.Enable(ffi::SCISSOR_TEST);
            gl.Scissor(mon_x, scissor_y, mon_w as i32, mon_h as i32);
            gl.Viewport(mon_x, scissor_y, mon_w as i32, mon_h as i32);

            // ------------------------------------------------------------------
            // 2. Compute prism geometry
            // ------------------------------------------------------------------
            // A regular n-gon needs apothem = half_width / tan(PI / n).
            // This used to be hard-coded to `sqrt(3) * half_width`, which is
            // only valid for a hexagon: four windows consequently formed four
            // disconnected panels instead of a cube. The shared camera owns
            // the canonical 3..=6-side geometry and framing for both backends.
            let sides = n.clamp(MIN_PRISM_SIDES, MAX_PRISM_SIDES);
            let face_aspect = mw / mh;
            let camera = PrismCamera::frame(face_aspect, sides, PRISM_FACE_FILL, 0.27, 0.0);
            let anim_scale = self.overview_opacity.clamp(0.0, 1.0);
            let lift = camera.lift_for_base_line(PRISM_BASE_LINE) * anim_scale;
            let base_model = mat4_mul(
                &translate_matrix(0.0, lift, 0.0),
                &mat4_mul(
                    &rotate_y_matrix(self.overview_rotation),
                    &scale_matrix(anim_scale, anim_scale, anim_scale),
                ),
            );

            // Determine selected index for rotation target
            let selected_idx = self
                .overview_selection
                .and_then(|sel_id| {
                    self.overview_entries
                        .iter()
                        .position(|e| e.window_id == sel_id)
                })
                .unwrap_or(0);

            // ------------------------------------------------------------------
            // 3. Resolve every slot and retain the shared painter order
            // ------------------------------------------------------------------
            let availability: Vec<PrismEntryAvailability> = self
                .overview_entries
                .iter()
                .map(|entry| match self.windows.get(&entry.window_id) {
                    Some(window) if window.gl_texture.is_some() => PrismEntryAvailability::Live,
                    Some(_) => PrismEntryAvailability::MissingTexture,
                    None => PrismEntryAvailability::MissingWindow,
                })
                .collect();
            let face_plan = prism_face_plan(sides, &availability);
            let pieces = build_prism_pieces(&camera, &base_model);

            // ------------------------------------------------------------------
            // 4. Draw faces and caps in their original depth-sorted order
            // ------------------------------------------------------------------
            gl.UseProgram(self.cube_program);
            gl.Uniform1f(self.cube_uniforms.aspect, face_aspect);
            gl.Uniform3f(
                self.cube_uniforms.camera,
                camera.eye[0],
                camera.eye[1],
                camera.eye[2],
            );
            gl.Uniform1f(self.cube_uniforms.alpha, anim_scale);
            gl.Uniform1f(self.cube_uniforms.edge, 1.0);
            gl.Uniform1f(self.cube_uniforms.lit, 1.0);
            gl.Uniform1i(
                self.cube_uniforms.scene_linear,
                i32::from(scene_linear_output),
            );
            gl.Uniform1i(self.cube_uniforms.texture, 0);

            gl.UseProgram(self.overview_cap_program);
            gl.Uniform1f(self.overview_cap_uniforms.radius, camera.circumradius);
            gl.Uniform1f(self.overview_cap_uniforms.sides, sides as f32);
            gl.Uniform3f(
                self.overview_cap_uniforms.camera,
                camera.eye[0],
                camera.eye[1],
                camera.eye[2],
            );
            gl.Uniform3f(self.overview_cap_uniforms.accent, 0.32, 0.62, 1.0);
            gl.Uniform1i(
                self.overview_cap_uniforms.scene_linear,
                i32::from(scene_linear_output),
            );

            gl.ActiveTexture(ffi::TEXTURE0);
            gl.BindVertexArray(self.quad_vao);

            let mut drawn_live_faces = 0usize;
            let mut drawn_filler_faces = 0usize;
            let mut drawn_caps = 0usize;
            let mut missing_window_faces = 0usize;
            let mut missing_texture_faces = 0usize;
            let mut unoccupied_faces = 0usize;

            for piece in &pieces {
                match piece.kind {
                    PrismKind::Face { slot } => {
                        let source = face_plan[slot];
                        let entry_index = match source {
                            PrismFaceSource::Live { entry_index } => Some(entry_index),
                            PrismFaceSource::Filler { entry_index, .. } => entry_index,
                        };
                        let selected = entry_index == Some(selected_idx);
                        let brightness = if piece.facing > 0.0 {
                            0.70 + 0.30 * piece.facing
                        } else {
                            0.42
                        };

                        gl.UseProgram(self.cube_program);
                        gl.UniformMatrix4fv(
                            self.cube_uniforms.mvp,
                            1,
                            ffi::FALSE as u8,
                            piece.mvp.as_ptr(),
                        );
                        gl.UniformMatrix4fv(
                            self.cube_uniforms.model,
                            1,
                            ffi::FALSE as u8,
                            piece.model.as_ptr(),
                        );
                        gl.Uniform1f(self.cube_uniforms.brightness, brightness);
                        gl.Uniform4f(
                            self.cube_uniforms.accent,
                            0.32,
                            0.62,
                            1.0,
                            if selected { 1.0 } else { 0.15 },
                        );
                        gl.Uniform1f(
                            self.cube_uniforms.desat,
                            if selected {
                                0.0
                            } else if piece.facing > 0.0 {
                                0.30
                            } else {
                                0.65
                            },
                        );

                        match source {
                            PrismFaceSource::Live { entry_index } => {
                                let entry = &self.overview_entries[entry_index];
                                let win = self
                                    .windows
                                    .get(&entry.window_id)
                                    .expect("live overview face must retain its window");
                                let texture = win
                                    .gl_texture
                                    .expect("live overview face must retain its texture");
                                let [cu, cv, cw, ch] = win.content_uv;
                                let (uv_x, uv_y, uv_w, uv_h) = if win.y_inverted {
                                    (cu, cv, cw, ch)
                                } else {
                                    (cu, cv + ch, cw, -ch)
                                };
                                gl.Uniform1i(self.cube_uniforms.filler, 0);
                                gl.Uniform1i(
                                    self.cube_uniforms.has_alpha,
                                    i32::from(win.has_alpha),
                                );
                                gl.Uniform4f(self.cube_uniforms.uv_rect, uv_x, uv_y, uv_w, uv_h);
                                self.bind_window_texture(gl, texture);
                                drawn_live_faces += 1;
                            }
                            PrismFaceSource::Filler { reason, .. } => {
                                gl.Uniform1i(self.cube_uniforms.filler, 1);
                                gl.Uniform1i(self.cube_uniforms.has_alpha, 0);
                                gl.Uniform4f(self.cube_uniforms.uv_rect, 0.0, 0.0, 1.0, 1.0);
                                gl.BindTexture(ffi::TEXTURE_2D, 0);
                                drawn_filler_faces += 1;
                                match reason {
                                    PrismFillerReason::Unoccupied => unoccupied_faces += 1,
                                    PrismFillerReason::MissingWindow => missing_window_faces += 1,
                                    PrismFillerReason::MissingTexture => missing_texture_faces += 1,
                                }
                            }
                        }

                        gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
                    }
                    PrismKind::Cap { top } => {
                        // Only the cap whose outward normal faces the camera is
                        // visible on an opaque closed solid. Both shared pieces
                        // remain in the painter sequence, so a camera below the
                        // prism naturally selects the bottom one instead.
                        if piece.facing <= 0.02 {
                            continue;
                        }
                        gl.UseProgram(self.overview_cap_program);
                        gl.UniformMatrix4fv(
                            self.overview_cap_uniforms.mvp,
                            1,
                            ffi::FALSE as u8,
                            piece.mvp.as_ptr(),
                        );
                        gl.UniformMatrix4fv(
                            self.overview_cap_uniforms.model,
                            1,
                            ffi::FALSE as u8,
                            piece.model.as_ptr(),
                        );
                        gl.Uniform1f(self.overview_cap_uniforms.y, if top { 1.0 } else { -1.0 });
                        let (r, g, b) = if top {
                            (0.085, 0.105, 0.155)
                        } else {
                            (0.055, 0.065, 0.095)
                        };
                        gl.Uniform4f(self.overview_cap_uniforms.color, r, g, b, 0.90 * anim_scale);
                        let vertices = i32::try_from(sides).unwrap_or(6) + 2;
                        gl.DrawArrays(ffi::TRIANGLE_FAN, 0, vertices);
                        drawn_caps += 1;
                    }
                }
            }

            gl.Disable(ffi::SCISSOR_TEST);
            gl.Viewport(0, 0, self.screen_w as i32, self.screen_h as i32);

            if drawn_live_faces == 0 || missing_window_faces > 0 || missing_texture_faces > 0 {
                static LAST_OVERVIEW_LOG: std::sync::atomic::AtomicU64 =
                    std::sync::atomic::AtomicU64::new(0);
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let prev = LAST_OVERVIEW_LOG.load(std::sync::atomic::Ordering::Relaxed);
                if now > prev {
                    LAST_OVERVIEW_LOG.store(now, std::sync::atomic::Ordering::Relaxed);
                    log::info!(
                        "[overview] entries={} sides={} live={} filler={} unoccupied={} caps={} missing_window={} missing_texture={}",
                        self.overview_entries.len(),
                        sides,
                        drawn_live_faces,
                        drawn_filler_faces,
                        unoccupied_faces,
                        drawn_caps,
                        missing_window_faces,
                        missing_texture_faces
                    );
                }
            }

            // ------------------------------------------------------------------
            // 5. Locate the selected face for its flat title overlay
            // ------------------------------------------------------------------
            // Selection itself is now drawn by the face shader as a bevel in
            // the face plane. The former axis-aligned screen-space rectangle
            // visibly detached from a face as the prism turned.
            let vp_x = mon_x as f32;
            let vp_y = mon_y as f32;
            let mut selected_title_anchor = None;
            for piece in pieces.iter().rev() {
                if piece.kind != (PrismKind::Face { slot: selected_idx }) {
                    continue;
                }

                let corners = [
                    [-face_aspect, -1.0, 0.0],
                    [face_aspect, -1.0, 0.0],
                    [-face_aspect, 1.0, 0.0],
                    [face_aspect, 1.0, 0.0],
                ];
                let mut min_x = f32::MAX;
                let mut min_y = f32::MAX;
                let mut max_x = f32::MIN;
                let mut max_y = f32::MIN;
                for corner in corners {
                    let Some((sx, sy)) =
                        Self::project_overview_point(&piece.mvp, corner, mw, mh, vp_x, vp_y)
                    else {
                        continue;
                    };
                    min_x = min_x.min(sx);
                    min_y = min_y.min(sy);
                    max_x = max_x.max(sx);
                    max_y = max_y.max(sy);
                }
                if min_x == f32::MAX || min_y == f32::MAX {
                    break;
                }
                selected_title_anchor = Some(((min_x + max_x) * 0.5, max_y + 10.0));
                break;
            }

            // ------------------------------------------------------------------
            // 6. Title label below selected window
            // ------------------------------------------------------------------
            if !self.overview_title_textures.is_empty()
                && selected_idx < self.overview_title_textures.len()
            {
                let title_tex = self.overview_title_textures[selected_idx];
                if title_tex != 0 {
                    // Render title centered below the prism using the window program
                    let title = &self.overview_entries[selected_idx].title;
                    let char_w = 6u32;
                    let char_h = 10u32;
                    let padding = 2u32;
                    let max_label_width = max_title_texture_width(mon_w);
                    let text_w = ((title.len() as u32) * char_w).min(max_label_width);
                    let text_h = char_h + padding * 2;

                    // Scale up for readability. The atlas width helper already
                    // reserves both monitor margins at this display scale.
                    let scale = TITLE_SCALE;
                    let label_w = text_w as f32 * scale;
                    let label_h = text_h as f32 * scale;
                    let (anchor_x, anchor_y) = selected_title_anchor
                        .unwrap_or((mon_x as f32 + mw * 0.5, mon_y as f32 + mh * PRISM_BASE_LINE));
                    let min_x = mon_x as f32 + TITLE_MARGIN;
                    let max_x = (mon_x as f32 + mw - label_w - TITLE_MARGIN).max(min_x);
                    let min_y = mon_y as f32 + TITLE_MARGIN;
                    let max_y = (mon_y as f32 + mh - label_h - TITLE_MARGIN).max(min_y);
                    let label_x = (anchor_x - label_w * 0.5).clamp(min_x, max_x);
                    let label_y = anchor_y.clamp(min_y, max_y);

                    gl.UseProgram(self.program);
                    gl.UniformMatrix4fv(
                        self.win_uniforms.projection,
                        1,
                        ffi::FALSE as u8,
                        projection.as_ptr(),
                    );
                    gl.Uniform4f(self.win_uniforms.rect, label_x, label_y, label_w, label_h);
                    // Title atlases contain transparent background pixels. The
                    // window shader uses a negative opacity to preserve source
                    // alpha; a positive value intentionally forces RGB clients
                    // opaque and would turn the entire label quad black.
                    gl.Uniform1f(self.win_uniforms.opacity, -self.overview_opacity * 0.95);
                    gl.Uniform1f(self.win_uniforms.radius, 4.0);
                    gl.Uniform2f(self.win_uniforms.size, label_w, label_h);
                    gl.Uniform1f(self.win_uniforms.dim, 1.0);
                    gl.Uniform4f(self.win_uniforms.uv_rect, 0.0, 0.0, 1.0, 1.0);
                    gl.Uniform1f(self.win_uniforms.ripple_progress, -1.0);
                    gl.Uniform1f(self.win_uniforms.ripple_amplitude, 0.0);

                    gl.ActiveTexture(ffi::TEXTURE0);
                    gl.BindTexture(ffi::TEXTURE_2D, title_tex);
                    gl.TexParameteri(
                        ffi::TEXTURE_2D,
                        ffi::TEXTURE_MIN_FILTER,
                        ffi::NEAREST as i32,
                    );
                    gl.TexParameteri(
                        ffi::TEXTURE_2D,
                        ffi::TEXTURE_MAG_FILTER,
                        ffi::NEAREST as i32,
                    );
                    gl.Uniform1i(self.win_uniforms.texture, 0);

                    gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
                }
            }

            self.render_overview_scroll_strip(gl, projection, &strip_segments);
        }
    }

    /// Animate the prism rotation toward the target selection.
    /// Call each frame with delta-time in seconds.
    #[allow(dead_code)]
    pub(crate) fn tick_overview_prism(&mut self, dt: f32) {
        if !self.overview_active {
            return;
        }

        // Compute target rotation based on selected entry index
        let n = self.overview_entries.len();
        if n == 0 {
            return;
        }

        let selected_idx = self
            .overview_selection
            .and_then(|sel_id| {
                self.overview_entries
                    .iter()
                    .position(|e| e.window_id == sel_id)
            })
            .unwrap_or(0);

        self.overview_target_rotation = prism_target_rotation(n, selected_idx);

        // Ensure shortest rotation path (wrap around)
        let mut diff = self.overview_target_rotation - self.overview_rotation;
        while diff > std::f32::consts::PI {
            diff -= std::f32::consts::TAU;
        }
        while diff < -std::f32::consts::PI {
            diff += std::f32::consts::TAU;
        }
        let effective_target = self.overview_rotation + diff;

        // Exponential ease-out toward target
        let blend = 1.0 - (-8.0 * dt).exp();
        self.overview_rotation += (effective_target - self.overview_rotation) * blend;

        // Snap when close enough
        if (effective_target - self.overview_rotation).abs() < 0.001 {
            self.overview_rotation = effective_target;
        }

        // Opacity is advanced once by tick_overview; keep this routine solely
        // responsible for prism rotation so activation speed is independent
        // of how many overview sub-effects are enabled.
        if (effective_target - self.overview_rotation).abs() >= 0.001 {
            self.needs_render = true;
        }
    }

    /// Delete overview title textures to free GPU memory.
    #[allow(dead_code)]
    pub(crate) fn clear_overview_textures(&mut self, gl: &ffi::Gles2) {
        if self.overview_title_textures.is_empty() {
            return;
        }
        unsafe {
            for &tex in &self.overview_title_textures {
                if tex != 0 {
                    gl.DeleteTextures(1, &tex);
                }
            }
        }
        self.overview_title_textures.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(window_id: u64, x: f32, y: f32, w: f32, h: f32, focused: bool) -> OverviewEntry {
        OverviewEntry {
            window_id,
            x,
            y,
            w,
            h,
            focused,
            title: format!("win-{window_id}"),
        }
    }

    #[test]
    fn overview_strip_segments_group_windows_by_column_geometry() {
        let segments = overview_strip_segments(&[
            entry(1, 0.5, 0.0, 0.25, 1.0, true),
            entry(2, 0.0, 0.0, 0.5, 0.5, false),
            entry(3, 0.0, 0.5, 0.5, 0.5, false),
            entry(4, 0.0, 0.0, 0.0, 0.0, false),
        ]);

        assert_eq!(segments.len(), 2);
        assert!((segments[0].x_ratio - 0.0).abs() < 0.0001);
        assert!((segments[0].width_ratio - 0.5).abs() < 0.0001);
        assert_eq!(segments[0].windows.len(), 2);
        assert!(!segments[0].focused);
        assert!((segments[1].x_ratio - 0.5).abs() < 0.0001);
        assert_eq!(segments[1].windows.len(), 1);
        assert!(segments[1].focused);
        assert!(segments[1].windows[0].focused);
    }

    #[test]
    fn prism_target_rotation_faces_the_selected_slot() {
        assert_eq!(prism_target_rotation(0, 4), 0.0);
        assert_eq!(prism_target_rotation(1, 0), 0.0);
        assert!((prism_target_rotation(4, 2) + std::f32::consts::PI).abs() < 1.0e-6);
        assert!((prism_target_rotation(6, 5) + 5.0 * std::f32::consts::TAU / 6.0).abs() < 1.0e-6);
    }

    #[test]
    fn prism_face_plan_closes_one_and_two_entry_prisms_with_fillers() {
        let one = prism_face_plan(3, &[PrismEntryAvailability::Live]);
        assert_eq!(
            one,
            vec![
                PrismFaceSource::Live { entry_index: 0 },
                PrismFaceSource::Filler {
                    entry_index: None,
                    reason: PrismFillerReason::Unoccupied,
                },
                PrismFaceSource::Filler {
                    entry_index: None,
                    reason: PrismFillerReason::Unoccupied,
                },
            ]
        );

        let two = prism_face_plan(
            3,
            &[PrismEntryAvailability::Live, PrismEntryAvailability::Live],
        );
        assert_eq!(two.len(), 3);
        assert_eq!(
            two[2],
            PrismFaceSource::Filler {
                entry_index: None,
                reason: PrismFillerReason::Unoccupied,
            }
        );
    }

    #[test]
    fn prism_face_plan_degrades_missing_live_resources_without_losing_entries() {
        let plan = prism_face_plan(
            4,
            &[
                PrismEntryAvailability::Live,
                PrismEntryAvailability::MissingWindow,
                PrismEntryAvailability::MissingTexture,
            ],
        );

        assert_eq!(plan.len(), 4);
        assert_eq!(plan[0], PrismFaceSource::Live { entry_index: 0 });
        assert_eq!(
            plan[1],
            PrismFaceSource::Filler {
                entry_index: Some(1),
                reason: PrismFillerReason::MissingWindow,
            }
        );
        assert_eq!(
            plan[2],
            PrismFaceSource::Filler {
                entry_index: Some(2),
                reason: PrismFillerReason::MissingTexture,
            }
        );
        assert_eq!(
            plan[3],
            PrismFaceSource::Filler {
                entry_index: None,
                reason: PrismFillerReason::Unoccupied,
            }
        );
    }

    #[test]
    fn prism_face_plan_keeps_all_six_live_slots() {
        let plan = prism_face_plan(6, &[PrismEntryAvailability::Live; 6]);
        assert_eq!(plan.len(), 6);
        assert!(
            plan.iter()
                .enumerate()
                .all(|(entry_index, source)| { *source == PrismFaceSource::Live { entry_index } })
        );
    }

    #[test]
    fn prism_entry_range_bounds_rogue_backend_payloads_around_focus() {
        assert_eq!(prism_entry_range(0, 0), 0..0);
        assert_eq!(prism_entry_range(4, 2), 0..4);
        assert_eq!(prism_entry_range(10, 0), 0..6);
        assert_eq!(prism_entry_range(10, 7), 4..10);
        assert_eq!(prism_entry_range(10, 99), 4..10);
    }

    #[test]
    fn title_atlas_cap_reserves_scaled_monitor_margins() {
        for width in [200, 256, 1920, 7680] {
            let atlas_width = max_title_texture_width(width);
            assert!(
                atlas_width as f32 * TITLE_SCALE + TITLE_MARGIN * 2.0 <= width as f32,
                "{width}px monitor produced a {atlas_width}px atlas"
            );
        }
    }
}
