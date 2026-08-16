//! GPU side of screen recording: the cursor overlay and the NV12 packing pass.
//!
//! Recording used to read the composited scene back as RGBA and then do two
//! things to it on the CPU — blend the cursor sprite in, and hand 4 bytes per
//! pixel to ffmpeg, which converted them to NV12 before the encoder saw them.
//! Both belong on the GPU. Drawing the cursor is a textured quad, and packing
//! NV12 is one fullscreen pass, after which the readback carries 1.5 bytes per
//! pixel instead of 4 — 62.5% less traffic through the readback, the copy out
//! of mapped memory, the pipe, and ffmpeg's read.

use super::*;

/// Vertex stage for the cursor quad.
///
/// Positions come from a uniform rect in output pixels with y measured from the
/// top of the video, because that is how the X server reports the pointer. The
/// recording target holds a bottom-up image (a `glReadPixels` of it starts at
/// the bottom row), so y is flipped on the way into clip space.
pub(crate) const RECORDING_CURSOR_VERTEX: &str = r#"#version 330 core
uniform vec4 u_rect;
uniform vec2 u_target_size;
out vec2 v_uv;
void main() {
    vec2 corner = vec2(float(gl_VertexID & 1), float(gl_VertexID >> 1));
    v_uv = corner;
    vec2 px = u_rect.xy + corner * u_rect.zw;
    vec2 unit = vec2(px.x / u_target_size.x, 1.0 - px.y / u_target_size.y);
    gl_Position = vec4(unit * 2.0 - 1.0, 0.0, 1.0);
}
"#;

/// The XFixes cursor image is already alpha-premultiplied, so it is drawn with
/// `GL_ONE, GL_ONE_MINUS_SRC_ALPHA` and the shader passes it straight through.
pub(crate) const RECORDING_CURSOR_FRAGMENT: &str = r#"#version 330 core
uniform sampler2D u_cursor;
in vec2 v_uv;
out vec4 frag_color;
void main() {
    frag_color = texture(u_cursor, v_uv);
}
"#;

/// Vertex stage for the NV12 packing pass: a fullscreen triangle strip with no
/// attributes, matching the compositor's other fullscreen passes.
pub(crate) const RECORDING_PACK_VERTEX: &str = r#"#version 330 core
void main() {
    vec2 corner = vec2(float(gl_VertexID & 1), float(gl_VertexID >> 1));
    gl_Position = vec4(corner * 2.0 - 1.0, 0.0, 1.0);
}
"#;

/// Packs the composited scene into an NV12 buffer laid out inside an RGBA8
/// target of (width/4) x (height * 3/2) texels.
///
/// Four luma bytes ride in one RGBA texel, which is what makes this portable:
/// GLES3 only guarantees `GL_RGBA`/`GL_UNSIGNED_BYTE` for `glReadPixels`, so a
/// single-channel R8 target — the obvious way to write a luma plane — could not
/// be read back on the EGL path this compositor uses by default.
///
/// Rows below `u_luma_rows` hold the Y plane, one texel per four pixels. The
/// rows above hold NV12's interleaved chroma plane: one texel carries
/// (U,V) for two adjacent 2x2 chroma sites, so a chroma row is `width` bytes
/// wide exactly like a luma row, and the two regions stay contiguous.
///
/// `gl_FragCoord.y` counts from the bottom of the target, which is also the
/// order `glReadPixels` returns rows in, so the packed regions land in the
/// buffer the right way round without any further flipping. The *image* is
/// flipped here instead, which is what lets the encoder drop `-vf vflip`.
///
/// The matrix is BT.709 limited range, and the encoder tags the stream to
/// match. That is a deliberate change from what swscale did, which was BT.601
/// on an untagged HD stream: ffmpeg-based playback guessed BT.601 and looked
/// right, which is why it went unnoticed, but mpv, VLC and browsers apply the
/// usual "HD means BT.709" heuristic and showed every recording with a colour
/// shift — pure red came back as (255,23,0). Converting and tagging have to
/// change together; tagging alone turns a bug most players hide into one they
/// all show.
pub(crate) const RECORDING_PACK_FRAGMENT: &str = r#"#version 330 core
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
        // Average each 2x2 site in RGB before converting, which is what the
        // CPU converter this replaces also did.
        vec3 left = 0.25 * (fetch(vec2(x, row)) + fetch(vec2(x + 1.0, row))
                          + fetch(vec2(x, row + 1.0)) + fetch(vec2(x + 1.0, row + 1.0)));
        vec3 right = 0.25 * (fetch(vec2(x + 2.0, row)) + fetch(vec2(x + 3.0, row))
                           + fetch(vec2(x + 2.0, row + 1.0)) + fetch(vec2(x + 3.0, row + 1.0)));
        frag_color = vec4(chroma(left), chroma(right));
    }
}
"#;

/// Video dimensions the NV12 layout can express: four pixels share a luma
/// texel, and chroma is subsampled by two vertically.
pub(crate) const fn nv12_aligned_size(width: u32, height: u32) -> (u32, u32) {
    (width & !3, height & !1)
}

/// Byte size of one NV12 frame.
pub(crate) const fn nv12_frame_bytes(width: u32, height: u32) -> usize {
    (width as usize) * (height as usize) * 3 / 2
}

/// Dimensions of the RGBA8 target the packing pass renders into.
pub(crate) const fn nv12_packed_target_size(width: u32, height: u32) -> (u32, u32) {
    (width / 4, height + height / 2)
}

/// Encoded size for a captured region, honouring a height cap.
///
/// Every downstream cost — readback, the copy out of mapped memory, the pipe,
/// the encoder — scales with the pixel count, so capping a 4K capture to 1080p
/// cuts all of them to a quarter. The scaling itself is free because the
/// capture blit already resamples the region into the output target.
///
/// A cap of zero, or one no smaller than the region, records at the captured
/// resolution. The result is always snapped to what the NV12 layout can express.
pub(crate) fn recording_output_size(region_w: u32, region_h: u32, max_height: u32) -> (u32, u32) {
    if region_w == 0 || region_h == 0 {
        return (0, 0);
    }
    if max_height == 0 || region_h <= max_height {
        return nv12_aligned_size(region_w, region_h);
    }
    // Preserve aspect ratio; round the width rather than truncating so a 16:9
    // capture stays 16:9 to within the alignment snap.
    let scaled_w = (u64::from(region_w) * u64::from(max_height) + u64::from(region_h) / 2)
        / u64::from(region_h);
    nv12_aligned_size(scaled_w.max(1) as u32, max_height)
}

/// Whether a packed NV12 target of this size is within the driver's limits.
///
/// The packed layout is half again as tall as the video, so a 4K recording asks
/// for a 3240-row texture. Desktop GL and every real GLES3 driver allow far
/// more, but ES 3.0 only *requires* 2048, which a 1440p recording already
/// exceeds. Recording falls back to the plain RGBA readback rather than
/// failing when a driver really is that limited.
pub(crate) const fn nv12_target_fits(width: u32, height: u32, max_texture_size: u32) -> bool {
    let (packed_w, packed_h) = nv12_packed_target_size(width, height);
    packed_w > 0 && packed_h > 0 && packed_w <= max_texture_size && packed_h <= max_texture_size
}

/// Where the cursor image lands in the recorded frame, as `[x, y, w, h]` in
/// output pixels with y measured from the top.
///
/// `top_left` is the cursor image's corner in root coordinates — already
/// hotspot-adjusted — and `region` is the part of the root being recorded. The
/// cursor is scaled by the same factor the capture blit scales the scene, so it
/// stays the right size when a small region is recorded into a larger output.
pub(crate) fn recording_cursor_rect(
    top_left: (i32, i32),
    cursor_size: (u32, u32),
    region: (i32, i32, u32, u32),
    target: (u32, u32),
) -> Option<[f32; 4]> {
    let (region_x, region_y, region_w, region_h) = region;
    let (target_w, target_h) = target;
    let (cursor_w, cursor_h) = cursor_size;
    if region_w == 0 || region_h == 0 || target_w == 0 || target_h == 0 {
        return None;
    }
    if cursor_w == 0 || cursor_h == 0 {
        return None;
    }
    let scale_x = target_w as f32 / region_w as f32;
    let scale_y = target_h as f32 / region_h as f32;
    Some([
        (top_left.0 - region_x) as f32 * scale_x,
        (top_left.1 - region_y) as f32 * scale_y,
        cursor_w as f32 * scale_x,
        cursor_h as f32 * scale_y,
    ])
}

impl<C: CompositorConnection> Compositor<C> {
    /// Compile the recording passes and look up their uniforms.
    ///
    /// Done when a recording starts rather than at compositor startup: most
    /// sessions never record, and `ShaderCache` keeps the result for any later
    /// recording in the same session. Returns whether recording can proceed.
    pub(super) fn build_recording_programs(&mut self) -> bool {
        if self.recording_cursor_program.is_some() && self.recording_pack_program.is_some() {
            return true;
        }
        let cursor = self.shader_cache.get_or_compile(
            &self.gl,
            "recording_cursor",
            RECORDING_CURSOR_VERTEX,
            RECORDING_CURSOR_FRAGMENT,
        );
        let pack = self.shader_cache.get_or_compile(
            &self.gl,
            "recording_pack_nv12",
            RECORDING_PACK_VERTEX,
            RECORDING_PACK_FRAGMENT,
        );
        let (cursor, pack) = match (cursor, pack) {
            (Ok(cursor), Ok(pack)) => (cursor, pack),
            (cursor, pack) => {
                for error in [cursor.err(), pack.err()].into_iter().flatten() {
                    log::warn!("compositor: recording shader failed to compile: {error}");
                }
                return false;
            }
        };
        unsafe {
            self.recording_cursor_rect = self.gl.get_uniform_location(cursor, "u_rect");
            self.recording_cursor_target = self.gl.get_uniform_location(cursor, "u_target_size");
            self.recording_cursor_sampler_loc = self.gl.get_uniform_location(cursor, "u_cursor");
            self.recording_pack_source = self.gl.get_uniform_location(pack, "u_source");
            self.recording_pack_video_size = self.gl.get_uniform_location(pack, "u_video_size");
            self.recording_pack_luma_rows = self.gl.get_uniform_location(pack, "u_luma_rows");
        }
        self.recording_cursor_program = Some(cursor);
        self.recording_pack_program = Some(pack);
        true
    }

    /// Release everything the recording passes own. The programs themselves
    /// belong to `ShaderCache` and are deliberately left for it to reuse.
    pub(super) fn release_recording_gpu(&mut self) {
        unsafe {
            if let Some((fbo, texture)) = self.recording_nv12_fbo.take() {
                self.gl.delete_framebuffer(fbo);
                self.gl.delete_texture(texture);
            }
            if let Some(texture) = self.recording_cursor_texture.take() {
                self.gl.delete_texture(texture);
            }
        }
        self.recording_cursor_texture_serial = None;
    }

    /// Draw the sampled cursor into the recording target.
    ///
    /// The X server draws the pointer as a sprite that XComposite never
    /// redirects, so it is absent from the composited scene and has to be added
    /// here. `region` is the part of the screen being recorded, in root
    /// coordinates, and the cursor is scaled into the output the same way the
    /// capture blit scales the scene.
    pub(super) fn draw_recording_cursor(
        &mut self,
        cursor: &RecordingCursor,
        region: (i32, i32, u32, u32),
        target: (u32, u32),
    ) {
        let Some(program) = self.recording_cursor_program else {
            return;
        };
        let (target_w, target_h) = target;
        let Some(rect) = recording_cursor_rect(cursor.top_left(), cursor.size(), region, target)
        else {
            return;
        };

        if self.recording_cursor_texture_serial != Some(cursor.serial()) {
            let uploaded = unsafe { self.upload_recording_cursor(cursor) };
            if !uploaded {
                return;
            }
            self.recording_cursor_texture_serial = Some(cursor.serial());
        }
        let Some(texture) = self.recording_cursor_texture else {
            return;
        };

        unsafe {
            self.gl.use_program(Some(program));
            self.gl.uniform_4_f32(
                self.recording_cursor_rect.as_ref(),
                rect[0],
                rect[1],
                rect[2],
                rect[3],
            );
            self.gl.uniform_2_f32(
                self.recording_cursor_target.as_ref(),
                target_w as f32,
                target_h as f32,
            );
            self.gl
                .uniform_1_i32(self.recording_cursor_sampler_loc.as_ref(), 0);
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            self.gl.enable(glow::BLEND);
            // The XFixes image is premultiplied.
            self.gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
            self.gl.bind_vertex_array(Some(self.quad_vao));
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            self.gl.disable(glow::BLEND);
            self.gl.bind_texture(glow::TEXTURE_2D, None);
        }
    }

    /// # Safety
    /// Requires a current GL context.
    unsafe fn upload_recording_cursor(&mut self, cursor: &RecordingCursor) -> bool {
        let (width, height) = cursor.size();
        let pixels = cursor.to_rgba8();
        if pixels.len() < (width as usize) * (height as usize) * 4 {
            return false;
        }
        unsafe {
            let texture = match self.recording_cursor_texture {
                Some(texture) => texture,
                None => match self.gl.create_texture() {
                    Ok(texture) => {
                        self.recording_cursor_texture = Some(texture);
                        texture
                    }
                    Err(error) => {
                        log::warn!("compositor: recording cursor texture: {error}");
                        return false;
                    }
                },
            };
            self.gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            self.gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                width as i32,
                height as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(&pixels)),
            );
            for (name, value) in [
                (glow::TEXTURE_MIN_FILTER, glow::LINEAR),
                (glow::TEXTURE_MAG_FILTER, glow::LINEAR),
                (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
                (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
            ] {
                self.gl
                    .tex_parameter_i32(glow::TEXTURE_2D, name, value as i32);
            }
            self.gl.bind_texture(glow::TEXTURE_2D, None);
        }
        true
    }

    /// Render the recording target into its packed NV12 form.
    ///
    /// Leaves the packed FBO bound so the caller can read it back directly.
    pub(super) fn pack_recording_nv12(&mut self, video: (u32, u32)) -> bool {
        let (Some(program), Some((packed_fbo, _)), Some((_, source_texture))) = (
            self.recording_pack_program,
            self.recording_nv12_fbo,
            self.recording_fbo,
        ) else {
            return false;
        };
        let (video_w, video_h) = video;
        let (packed_w, packed_h) = nv12_packed_target_size(video_w, video_h);
        unsafe {
            self.gl
                .bind_framebuffer(glow::FRAMEBUFFER, Some(packed_fbo));
            self.gl.viewport(0, 0, packed_w as i32, packed_h as i32);
            self.gl.disable(glow::BLEND);
            self.gl.use_program(Some(program));
            self.gl
                .uniform_1_i32(self.recording_pack_source.as_ref(), 0);
            self.gl.uniform_2_f32(
                self.recording_pack_video_size.as_ref(),
                video_w as f32,
                video_h as f32,
            );
            self.gl
                .uniform_1_f32(self.recording_pack_luma_rows.as_ref(), video_h as f32);
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(source_texture));
            self.gl.bind_vertex_array(Some(self.quad_vao));
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            self.gl.bind_texture(glow::TEXTURE_2D, None);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::{
        nv12_aligned_size, nv12_frame_bytes, nv12_packed_target_size, recording_cursor_rect,
    };

    #[test]
    fn no_height_cap_records_at_the_captured_resolution() {
        assert_eq!(super::recording_output_size(3840, 2160, 0), (3840, 2160));
        // A cap at or above the capture changes nothing.
        assert_eq!(super::recording_output_size(1920, 1080, 1080), (1920, 1080));
        assert_eq!(super::recording_output_size(1920, 1080, 2160), (1920, 1080));
    }

    #[test]
    fn a_height_cap_scales_down_and_keeps_the_aspect_ratio() {
        // The case that matters: 4K to 1080p is a quarter of the pixels, so a
        // quarter of the readback, pipe and encoder work.
        assert_eq!(super::recording_output_size(3840, 2160, 1080), (1920, 1080));
        assert_eq!(super::recording_output_size(3840, 2160, 720), (1280, 720));
        assert_eq!(super::recording_output_size(2560, 1440, 1080), (1920, 1080));
        let (w, h) = super::recording_output_size(3840, 2160, 1080);
        assert_eq!(
            (w as usize) * (h as usize) * 4,
            3840 * 2160,
            "1080p from 4K should be exactly a quarter of the pixels"
        );
    }

    #[test]
    fn a_scaled_size_is_still_something_the_nv12_layout_can_express() {
        // Odd captures and awkward ratios must still land on the alignment.
        for (rw, rh, cap) in [
            (1366u32, 768u32, 480u32),
            (1023, 767, 300),
            (3440, 1440, 900),
        ] {
            let (w, h) = super::recording_output_size(rw, rh, cap);
            assert_eq!(w % 4, 0, "{rw}x{rh} cap {cap} gave width {w}");
            assert_eq!(h % 2, 0, "{rw}x{rh} cap {cap} gave height {h}");
            assert!(h <= cap);
        }
        assert_eq!(super::recording_output_size(0, 1080, 720), (0, 0));
    }

    #[test]
    fn a_fullscreen_recording_places_the_cursor_at_its_root_position() {
        // Hotspot at (300, 200) with the image's corner two pixels up and left.
        let rect = recording_cursor_rect((298, 198), (24, 24), (0, 0, 1920, 1080), (1920, 1080))
            .expect("a fullscreen recording can place a cursor");
        assert_eq!(rect, [298.0, 198.0, 24.0, 24.0]);
    }

    #[test]
    fn a_region_recording_offsets_and_scales_the_cursor_with_the_scene() {
        // A 960x540 region blown up to a 1920x1080 output doubles everything,
        // the same way the capture blit scales the scene into the target.
        let rect = recording_cursor_rect((500, 400), (24, 24), (400, 300, 960, 540), (1920, 1080))
            .expect("a region recording can place a cursor");
        assert_eq!(rect, [200.0, 200.0, 48.0, 48.0]);
    }

    #[test]
    fn a_degenerate_region_or_cursor_draws_nothing() {
        assert!(recording_cursor_rect((0, 0), (24, 24), (0, 0, 0, 1080), (1920, 1080)).is_none());
        assert!(recording_cursor_rect((0, 0), (24, 24), (0, 0, 1920, 1080), (0, 1080)).is_none());
        assert!(recording_cursor_rect((0, 0), (0, 0), (0, 0, 1920, 1080), (1920, 1080)).is_none());
    }

    #[test]
    fn the_packed_target_holds_exactly_one_nv12_frame() {
        for (w, h) in [(1920, 1080), (2560, 1440), (3840, 2160), (1280, 720)] {
            let (packed_w, packed_h) = nv12_packed_target_size(w, h);
            // Four bytes per RGBA texel, and the frame is 1.5 bytes per pixel.
            assert_eq!(
                (packed_w * packed_h * 4) as usize,
                nv12_frame_bytes(w, h),
                "packed target does not match the frame size at {w}x{h}"
            );
        }
    }

    #[test]
    fn nv12_is_markedly_smaller_than_rgba() {
        // The whole point of the pass: 1.5 bytes per pixel instead of 4.
        let rgba = 1920 * 1080 * 4;
        assert_eq!(nv12_frame_bytes(1920, 1080), 3_110_400);
        assert_eq!(rgba - nv12_frame_bytes(1920, 1080), 5_184_000);
    }

    #[test]
    fn sizes_snap_down_to_what_the_layout_can_express() {
        assert_eq!(nv12_aligned_size(1920, 1080), (1920, 1080));
        // Four pixels share a luma texel; chroma halves the height.
        assert_eq!(nv12_aligned_size(1366, 768), (1364, 768));
        assert_eq!(nv12_aligned_size(1023, 767), (1020, 766));
        // Snapping never rounds up past the region the user selected.
        for (w, h) in [(1u32, 1u32), (3, 3), (7, 5), (1919, 1079)] {
            let (aw, ah) = nv12_aligned_size(w, h);
            assert!(aw <= w && ah <= h);
            assert_eq!(aw % 4, 0);
            assert_eq!(ah % 2, 0);
        }
    }
}
