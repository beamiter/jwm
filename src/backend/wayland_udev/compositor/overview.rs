use super::*;
use smithay::backend::renderer::gles::ffi;

// ---------------------------------------------------------------------------
// 4x4 matrix helpers (column-major, OpenGL convention)
// ---------------------------------------------------------------------------

fn mat4_mul(a: &[f32; 16], b: &[f32; 16]) -> [f32; 16] {
    let mut r = [0.0f32; 16];
    for i in 0..4 {
        for j in 0..4 {
            r[i * 4 + j] = a[i * 4 + 0] * b[0 * 4 + j]
                + a[i * 4 + 1] * b[1 * 4 + j]
                + a[i * 4 + 2] * b[2 * 4 + j]
                + a[i * 4 + 3] * b[3 * 4 + j];
        }
    }
    r
}

fn mat4_identity() -> [f32; 16] {
    let mut m = [0.0f32; 16];
    m[0] = 1.0;
    m[5] = 1.0;
    m[10] = 1.0;
    m[15] = 1.0;
    m
}

fn mat4_translate(x: f32, y: f32, z: f32) -> [f32; 16] {
    let mut m = mat4_identity();
    m[12] = x;
    m[13] = y;
    m[14] = z;
    m
}

fn mat4_rotate_y(angle: f32) -> [f32; 16] {
    let c = angle.cos();
    let s = angle.sin();
    let mut m = mat4_identity();
    m[0] = c;
    m[2] = -s;
    m[8] = s;
    m[10] = c;
    m
}

fn mat4_scale(sx: f32, sy: f32, sz: f32) -> [f32; 16] {
    let mut m = [0.0f32; 16];
    m[0] = sx;
    m[5] = sy;
    m[10] = sz;
    m[15] = 1.0;
    m
}

fn mat4_perspective(fov: f32, aspect: f32, near: f32, far: f32) -> [f32; 16] {
    let f = 1.0 / (fov * 0.5).tan();
    let mut m = [0.0f32; 16];
    m[0] = f / aspect;
    m[5] = f;
    m[10] = (far + near) / (near - far);
    m[11] = -1.0;
    m[14] = (2.0 * far * near) / (near - far);
    m
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

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

impl WaylandCompositor {
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
    #[allow(dead_code)]
    pub(crate) fn create_overview_title_textures(&mut self, gl: &ffi::Gles2) {
        self.clear_overview_textures(gl);

        let max_label_width = (self.screen_w / 3).max(120);
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
    }

    /// Render the 3D hexagonal prism carousel overview.
    /// Each window becomes a face on a rotating prism; the selected face rotates to front.
    pub(crate) fn render_overview(&self, gl: &ffi::Gles2, projection: &[f32; 16]) {
        if self.overview_opacity <= 0.0 {
            return;
        }

        let n = self.overview_entries.len();
        if n == 0 {
            return;
        }

        unsafe {
            gl.Enable(ffi::BLEND);
            gl.BlendFunc(ffi::SRC_ALPHA, ffi::ONE_MINUS_SRC_ALPHA);

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

            if rect_loc >= 0 {
                gl.Uniform4f(
                    rect_loc,
                    0.0,
                    0.0,
                    self.screen_w as f32,
                    self.screen_h as f32,
                );
            }
            if proj_loc >= 0 {
                gl.UniformMatrix4fv(proj_loc, 1, ffi::FALSE as u8, projection.as_ptr());
            }
            if opacity_loc >= 0 {
                gl.Uniform1f(opacity_loc, self.overview_opacity);
            }

            gl.BindVertexArray(self.quad_vao);
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);

            // ------------------------------------------------------------------
            // 2. Compute prism geometry
            // ------------------------------------------------------------------
            let aspect = self.screen_w as f32 / self.screen_h as f32;
            let fov = 1.0f32; // ~57 degrees
            let near = 0.1f32;
            let far = 100.0f32;
            let persp = mat4_perspective(fov, aspect, near, far);

            let camera_z = -3.5f32;
            let prism_radius = 1.2f32;
            let face_angle = std::f32::consts::TAU / (n as f32);

            // Determine selected index for rotation target
            let selected_idx = self
                .overview_selection
                .and_then(|sel_id| {
                    self.overview_entries
                        .iter()
                        .position(|e| e.window_id == sel_id)
                })
                .unwrap_or(0);

            // Current rotation (animated in tick_overview_prism)
            let rotation = self.overview_rotation;

            // ------------------------------------------------------------------
            // 3. Build face data and sort back-to-front (painter's algorithm)
            // ------------------------------------------------------------------
            struct FaceData {
                index: usize,
                z: f32,
                mvp: [f32; 16],
                brightness: f32,
            }

            let mut faces: Vec<FaceData> = Vec::with_capacity(n);

            for i in 0..n {
                let angle = face_angle * (i as f32) + rotation;
                let x = prism_radius * angle.sin();
                let z = prism_radius * angle.cos();

                // Model transform: translate face to position, rotate to face outward
                let translate = mat4_translate(x, 0.0, z + camera_z);
                let rot = mat4_rotate_y(angle);

                // Scale face to reasonable aspect (0.7 width, aspect-corrected height)
                let face_scale = 0.7;
                let scale = mat4_scale(face_scale, face_scale / aspect, 1.0);

                // MVP = perspective * translate * rotate * scale
                let model = mat4_mul(&translate, &mat4_mul(&rot, &scale));
                let mvp = mat4_mul(&persp, &model);

                // Brightness based on Z proximity (closer = brighter)
                let norm_z = (z + prism_radius) / (2.0 * prism_radius);
                let brightness = (0.3 + 0.7 * norm_z).clamp(0.3, 1.0);

                faces.push(FaceData {
                    index: i,
                    z,
                    mvp,
                    brightness,
                });
            }

            // Sort back-to-front: smaller z (further) drawn first
            faces.sort_by(|a, b| a.z.partial_cmp(&b.z).unwrap_or(std::cmp::Ordering::Equal));

            // ------------------------------------------------------------------
            // 4. Render each face using cube_program
            // ------------------------------------------------------------------
            gl.UseProgram(self.cube_program);
            gl.Uniform1f(self.cube_uniforms.aspect, aspect);
            gl.BindVertexArray(self.quad_vao);

            let tex_loc =
                gl.GetUniformLocation(self.cube_program, b"u_texture\0".as_ptr() as *const _);

            for face in &faces {
                let entry = &self.overview_entries[face.index];
                let win = match self.windows.get(&entry.window_id) {
                    Some(w) => w,
                    None => continue,
                };
                let texture = match win.gl_texture {
                    Some(t) => t,
                    None => continue,
                };

                // Apply overview_opacity to brightness for fade-in/out
                let bright = face.brightness * self.overview_opacity;

                gl.UniformMatrix4fv(
                    self.cube_uniforms.mvp,
                    1,
                    ffi::FALSE as u8,
                    face.mvp.as_ptr(),
                );
                gl.Uniform4f(self.cube_uniforms.uv_rect, 0.0, 0.0, 1.0, 1.0);
                gl.Uniform1f(self.cube_uniforms.brightness, bright);

                gl.ActiveTexture(ffi::TEXTURE0);
                self.bind_window_texture(gl, texture);
                if tex_loc >= 0 {
                    gl.Uniform1i(tex_loc, 0);
                }

                gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
            }

            // ------------------------------------------------------------------
            // 5. Selection highlight border on front-most face
            // ------------------------------------------------------------------
            if let Some(front_face) = faces.last() {
                let entry = &self.overview_entries[front_face.index];
                let is_selected = self.overview_selection == Some(entry.window_id)
                    || entry.focused
                    || front_face.index == selected_idx;

                if is_selected {
                    // Draw a highlight border using the border_program in screen-space
                    // Project center of front face to get approximate screen position
                    let cx = self.screen_w as f32 * 0.5;
                    let cy = self.screen_h as f32 * 0.5;
                    let face_w = self.screen_w as f32 * 0.35;
                    let face_h = self.screen_h as f32 * 0.55;
                    let bx = cx - face_w * 0.5 - 4.0;
                    let by = cy - face_h * 0.5 - 4.0;
                    let bw = face_w + 8.0;
                    let bh = face_h + 8.0;

                    gl.UseProgram(self.border_program);
                    gl.UniformMatrix4fv(
                        self.border_uniforms.projection,
                        1,
                        ffi::FALSE as u8,
                        projection.as_ptr(),
                    );
                    gl.Uniform4f(self.border_uniforms.rect, bx, by, bw, bh);
                    gl.Uniform4f(
                        self.border_uniforms.border_color,
                        0.4,
                        0.7,
                        1.0,
                        self.overview_opacity * 0.9,
                    );
                    gl.Uniform2f(self.border_uniforms.size, bw, bh);
                    gl.Uniform1f(self.border_uniforms.radius, 12.0);
                    gl.Uniform1f(self.border_uniforms.border_width, 3.0);

                    gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
                }
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
                    let max_label_width = (self.screen_w / 3).max(120);
                    let text_w = ((title.len() as u32) * char_w).min(max_label_width);
                    let text_h = char_h + padding * 2;

                    // Scale up for readability (2x)
                    let scale = 2.0f32;
                    let label_w = text_w as f32 * scale;
                    let label_h = text_h as f32 * scale;
                    let label_x = (self.screen_w as f32 - label_w) * 0.5;
                    let label_y = self.screen_h as f32 * 0.82;

                    gl.UseProgram(self.program);
                    gl.UniformMatrix4fv(
                        self.win_uniforms.projection,
                        1,
                        ffi::FALSE as u8,
                        projection.as_ptr(),
                    );
                    gl.Uniform4f(self.win_uniforms.rect, label_x, label_y, label_w, label_h);
                    gl.Uniform1f(self.win_uniforms.opacity, self.overview_opacity * 0.95);
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

        let face_angle = std::f32::consts::TAU / (n as f32);

        let selected_idx = self
            .overview_selection
            .and_then(|sel_id| {
                self.overview_entries
                    .iter()
                    .position(|e| e.window_id == sel_id)
            })
            .unwrap_or(0);

        // Target: rotate so that the selected face ends up at angle=0 (facing camera)
        // Since face i is at angle face_angle*i + rotation, we want face_angle*selected_idx + rotation = 0
        // => target_rotation = -face_angle * selected_idx
        // Normalize to keep shortest path
        let raw_target = -face_angle * (selected_idx as f32);
        self.overview_target_rotation = raw_target;

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

        // Fade in/out
        let target_opacity = if self.overview_active { 1.0 } else { 0.0 };
        let opacity_blend = 1.0 - (-6.0 * dt).exp();
        self.overview_opacity += (target_opacity - self.overview_opacity) * opacity_blend;

        if self.overview_opacity > 0.001 || self.overview_active {
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
