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
use crate::backend::compositor_common::recording_nv12::{
    NV12_PACK_FRAGMENT_BODY, nv12_packed_target_size,
};
#[cfg(test)]
use crate::backend::compositor_common::recording_nv12::{
    nv12_aligned_size, nv12_frame_bytes, recording_output_size,
};

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

/// Fragment stage of the NV12 packing pass.
///
/// The body — layout, colour matrix and vertical flip — is shared with the
/// Wayland backend so the two backends cannot disagree about what a recorded
/// frame means. Only the version header differs; the shader cache rewrites this
/// one into ESSL for the GLES path.
pub(crate) fn recording_pack_fragment() -> String {
    format!("#version 330 core\n{NV12_PACK_FRAGMENT_BODY}")
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
            &recording_pack_fragment(),
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
    fn a_cap_that_cannot_make_a_picture_falls_back_to_the_capture() {
        // Validation tells the user a too-small cap is ignored; make that true.
        assert_eq!(super::recording_output_size(1920, 1080, 1), (1920, 1080));
        assert_eq!(super::recording_output_size(1920, 1080, 32), (1920, 1080));
        // An extreme aspect ratio scales the width below the four-pixel luma
        // granularity. Snapping that down would give a zero-width video and
        // abort the recording while the window manager thought it had started.
        assert_eq!(super::recording_output_size(16, 1080, 64), (16, 1080));
        // A cap that can still express a picture is honoured as usual.
        assert_eq!(super::recording_output_size(1920, 1080, 64), (112, 64));
    }

    #[test]
    fn no_capped_size_is_ever_degenerate_for_a_real_region() {
        for region_w in [4u32, 16, 64, 320, 1920, 3440] {
            for region_h in [2u32, 64, 768, 1080, 1440, 2160] {
                for cap in [0u32, 1, 32, 64, 240, 720, 1080, 4320] {
                    let (w, h) = super::recording_output_size(region_w, region_h, cap);
                    let native = super::nv12_aligned_size(region_w, region_h);
                    if native.0 == 0 || native.1 == 0 {
                        continue; // the region itself is unencodable
                    }
                    assert!(
                        w > 0 && h > 0,
                        "{region_w}x{region_h} cap {cap} produced {w}x{h}"
                    );
                    assert_eq!(w % 4, 0);
                    assert_eq!(h % 2, 0);
                }
            }
        }
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
