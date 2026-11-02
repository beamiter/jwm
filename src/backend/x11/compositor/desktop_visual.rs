//! Optional, input-transparent, out-of-process desktop visual extension.
//!
//! Unlike the legacy WaterLily adapter this accepts ordinary RGBA only. All
//! socket IO, validation, row compaction and deadlines live off the render
//! thread in `desktop_visual`; the compositor only takes a completed frame.
use super::{Compositor, CompositorConnection, DirtyRect};
use desktop_visual::{OutputSize, Server};
use glow::HasContext;
use std::path::PathBuf;
use std::sync::Arc;

const FRAGMENT_SHADER: &str = r#"#version 330 core
uniform sampler2D u_texture;
uniform int u_alpha;
in vec2 v_uv;
out vec4 frag_color;
vec4 straight_texel(ivec2 p, ivec2 size) {
    vec4 c = texelFetch(u_texture, clamp(p, ivec2(0), size - 1), 0);
    return vec4(c.rgb * c.a, c.a);
}
void main() {
    vec4 c;
    if (u_alpha == 2) {
        // Premultiply BEFORE interpolation. Otherwise arbitrary RGB in fully
        // transparent source pixels bleeds into the desktop at scaled edges.
        ivec2 size = textureSize(u_texture, 0);
        vec2 p = v_uv * vec2(size) - 0.5;
        ivec2 base = ivec2(floor(p));
        vec2 f = fract(p);
        c = mix(mix(straight_texel(base, size),
                    straight_texel(base + ivec2(1, 0), size), f.x),
                mix(straight_texel(base + ivec2(0, 1), size),
                    straight_texel(base + ivec2(1, 1), size), f.x), f.y);
    } else {
        c = texture(u_texture, v_uv);
    }
    // Display-referred sRGB, like the X11 compositor's ordinary SDR overlays.
    // Always output premultiplied source-over; opaque payload alpha is ignored.
    if (u_alpha == 1) c.a = 1.0;
    else if (u_alpha == 3) c.rgb = min(c.rgb, vec3(c.a));
    frag_color = c;
}
"#;

pub(super) struct DesktopVisualLayer {
    server: Server,
    program: glow::Program,
    projection: Option<glow::UniformLocation>,
    rect: Option<glow::UniformLocation>,
    sampler: Option<glow::UniformLocation>,
    alpha: Option<glow::UniformLocation>,
    texture: Option<glow::Texture>,
    alpha_mode: i32,
    texture_size: (u32, u32),
    texture_epoch: u64,
    output: OutputSize,
    pub(super) dirty: bool,
}

impl DesktopVisualLayer {
    pub(super) fn enabled() -> bool {
        std::env::var("JWM_DESKTOP_VISUAL_ENABLED")
            .is_ok_and(|s| s == "1" || s.eq_ignore_ascii_case("true"))
    }

    fn socket_path() -> Result<PathBuf, String> {
        if let Some(path) = std::env::var_os("JWM_DESKTOP_VISUAL_SOCKET") {
            return Ok(path.into());
        }
        // No shared /tmp fallback. Server validates ownership and permissions.
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .ok_or("desktop visual requires XDG_RUNTIME_DIR or JWM_DESKTOP_VISUAL_SOCKET")?;
        Ok(PathBuf::from(runtime).join("jwm-desktop-visual/visual.sock"))
    }

    pub(super) fn new<C: CompositorConnection>(
        gl: &glow::Context,
        width: u32,
        height: u32,
    ) -> Result<Self, String> {
        let output = OutputSize { width, height };
        let server = Server::bind(Self::socket_path()?, output).map_err(|e| e.to_string())?;
        // Never load shaders or executable code supplied by an extension.
        let program = unsafe {
            Compositor::<C>::create_program(gl, super::shaders::VERTEX_SHADER, FRAGMENT_SHADER)?
        };
        unsafe {
            Ok(Self {
                server,
                program,
                projection: gl.get_uniform_location(program, "u_projection"),
                rect: gl.get_uniform_location(program, "u_rect"),
                sampler: gl.get_uniform_location(program, "u_texture"),
                alpha: gl.get_uniform_location(program, "u_alpha"),
                texture: None,
                alpha_mode: 1,
                texture_size: (0, 0),
                texture_epoch: 0,
                output,
                dirty: false,
            })
        }
    }

    pub(super) fn pending(&self) -> bool {
        self.dirty || self.server.has_pending()
    }

    fn visible(&self) -> bool {
        self.texture.is_some()
            && self.server.connected()
            && self.texture_epoch == self.server.connection_epoch()
    }

    pub(super) fn set_loop_signal(&self, signal: calloop::LoopSignal) {
        self.server.set_waker(Arc::new(move || signal.wakeup()));
    }

    pub(super) fn destroy(self, gl: &glow::Context) {
        unsafe {
            if let Some(texture) = self.texture {
                gl.delete_texture(texture);
            }
            gl.delete_program(self.program);
        }
        // Dropping Server stops its worker and removes only its own socket.
    }
}

impl<C: CompositorConnection> Compositor<C> {
    pub(super) fn desktop_visual_pending(&self) -> bool {
        self.desktop_visual.as_ref().is_some_and(|layer| {
            layer.pending()
                || layer.output.width != self.screen_w
                || layer.output.height != self.screen_h
        })
    }

    pub(super) fn desktop_visual_visible(&self) -> bool {
        self.desktop_visual
            .as_ref()
            .is_some_and(DesktopVisualLayer::visible)
    }

    pub(super) fn poll_desktop_visual(&mut self) {
        let Some(layer) = self.desktop_visual.as_mut() else {
            return;
        };
        let output = OutputSize {
            width: self.screen_w,
            height: self.screen_h,
        };
        if layer.output != output {
            if let Err(error) = layer.server.set_output_size(output) {
                log::warn!("compositor: invalid desktop visual output: {error}");
                return;
            }
            layer.output = output;
            if let Some(texture) = layer.texture.take() {
                unsafe { self.gl.delete_texture(texture) };
                layer.dirty = true;
            }
        }
        if layer.texture_epoch != layer.server.connection_epoch() {
            if let Some(texture) = layer.texture.take() {
                unsafe { self.gl.delete_texture(texture) };
                layer.dirty = true;
            }
        }
        if let Some(frame) = layer.server.try_take_latest() {
            // Peer can disconnect between publication and polling. Never keep
            // a dead extension's stale overlay on the desktop.
            if layer.server.connected() {
                let width = frame.header.width;
                let height = frame.header.height;
                let max_size =
                    unsafe { self.gl.get_parameter_i32(glow::MAX_TEXTURE_SIZE) }.max(0) as u32;
                if width <= max_size && height <= max_size {
                    unsafe {
                        let reuse =
                            layer.texture.is_some() && layer.texture_size == (width, height);
                        let texture = if reuse {
                            Ok(layer.texture.unwrap())
                        } else {
                            self.gl.create_texture()
                        };
                        if let Ok(texture) = texture {
                            // Attribute upload errors to this operation, not an earlier pass.
                            for _ in 0..8 {
                                if self.gl.get_error() == glow::NO_ERROR {
                                    break;
                                }
                            }
                            // Bound upload state explicitly: a left-over PBO
                            // would interpret our pointer as a byte offset.
                            let active = self.gl.get_parameter_i32(glow::ACTIVE_TEXTURE) as u32;
                            let pbo = self
                                .gl
                                .get_parameter_buffer(glow::PIXEL_UNPACK_BUFFER_BINDING);
                            let alignment = self.gl.get_parameter_i32(glow::UNPACK_ALIGNMENT);
                            let row_length = self.gl.get_parameter_i32(glow::UNPACK_ROW_LENGTH);
                            let skip_pixels = self.gl.get_parameter_i32(glow::UNPACK_SKIP_PIXELS);
                            let skip_rows = self.gl.get_parameter_i32(glow::UNPACK_SKIP_ROWS);
                            self.gl.active_texture(glow::TEXTURE0);
                            let old_texture =
                                self.gl.get_parameter_texture(glow::TEXTURE_BINDING_2D);
                            self.gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, None);
                            self.gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
                            self.gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
                            self.gl.pixel_store_i32(glow::UNPACK_SKIP_PIXELS, 0);
                            self.gl.pixel_store_i32(glow::UNPACK_SKIP_ROWS, 0);
                            self.gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                            for name in [glow::TEXTURE_MIN_FILTER, glow::TEXTURE_MAG_FILTER] {
                                self.gl.tex_parameter_i32(
                                    glow::TEXTURE_2D,
                                    name,
                                    glow::LINEAR as i32,
                                );
                            }
                            for name in [glow::TEXTURE_WRAP_S, glow::TEXTURE_WRAP_T] {
                                self.gl.tex_parameter_i32(
                                    glow::TEXTURE_2D,
                                    name,
                                    glow::CLAMP_TO_EDGE as i32,
                                );
                            }
                            if reuse {
                                self.gl.tex_sub_image_2d(
                                    glow::TEXTURE_2D,
                                    0,
                                    0,
                                    0,
                                    width as i32,
                                    height as i32,
                                    glow::RGBA,
                                    glow::UNSIGNED_BYTE,
                                    glow::PixelUnpackData::Slice(Some(&frame.rgba)),
                                );
                            } else {
                                self.gl.tex_image_2d(
                                    glow::TEXTURE_2D,
                                    0,
                                    glow::RGBA8 as i32,
                                    width as i32,
                                    height as i32,
                                    0,
                                    glow::RGBA,
                                    glow::UNSIGNED_BYTE,
                                    glow::PixelUnpackData::Slice(Some(&frame.rgba)),
                                );
                            }
                            let uploaded = self.gl.get_error() == glow::NO_ERROR;
                            self.gl.bind_texture(glow::TEXTURE_2D, old_texture);
                            self.gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, alignment);
                            self.gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, row_length);
                            self.gl
                                .pixel_store_i32(glow::UNPACK_SKIP_PIXELS, skip_pixels);
                            self.gl.pixel_store_i32(glow::UNPACK_SKIP_ROWS, skip_rows);
                            self.gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, pbo);
                            self.gl.active_texture(active);
                            if uploaded {
                                if !reuse {
                                    if let Some(previous) = layer.texture.replace(texture) {
                                        self.gl.delete_texture(previous);
                                    }
                                }
                                layer.texture_size = (width, height);
                                layer.texture_epoch = frame.connection_epoch;
                                layer.alpha_mode = frame.header.alpha as i32;
                                layer.dirty = true;
                            } else {
                                self.gl.delete_texture(texture);
                                if reuse {
                                    layer.texture = None;
                                    layer.dirty = true;
                                }
                                log::warn!("compositor: desktop visual texture upload failed");
                            }
                        }
                    }
                }
            }
        }
        if !layer.server.connected() {
            if let Some(texture) = layer.texture.take() {
                unsafe { self.gl.delete_texture(texture) };
                layer.dirty = true;
            }
        }
        if layer.dirty {
            let rect = DirtyRect::new(0, 0, self.screen_w, self.screen_h);
            self.damage_tracker
                .mark_region_dirty(rect.x, rect.y, rect.width, rect.height);
            self.dirty_region_tracker.mark_dirty(rect);
        }
    }

    pub(super) fn render_desktop_visual(&mut self, projection: &[f32; 16]) {
        let Some(layer) = self.desktop_visual.as_ref().filter(|layer| layer.visible()) else {
            return;
        };
        unsafe {
            let blend = self.gl.is_enabled(glow::BLEND);
            let src_rgb = self.gl.get_parameter_i32(glow::BLEND_SRC_RGB) as u32;
            let dst_rgb = self.gl.get_parameter_i32(glow::BLEND_DST_RGB) as u32;
            let src_alpha = self.gl.get_parameter_i32(glow::BLEND_SRC_ALPHA) as u32;
            let dst_alpha = self.gl.get_parameter_i32(glow::BLEND_DST_ALPHA) as u32;
            let eq_rgb = self.gl.get_parameter_i32(glow::BLEND_EQUATION_RGB) as u32;
            let eq_alpha = self.gl.get_parameter_i32(glow::BLEND_EQUATION_ALPHA) as u32;
            let active = self.gl.get_parameter_i32(glow::ACTIVE_TEXTURE) as u32;
            self.gl.active_texture(glow::TEXTURE0);
            let old_texture = self.gl.get_parameter_texture(glow::TEXTURE_BINDING_2D);
            self.gl.enable(glow::BLEND);
            self.gl.blend_equation(glow::FUNC_ADD);
            self.gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
            self.gl_state_tracker
                .use_program(&self.gl, Some(layer.program));
            self.gl
                .uniform_matrix_4_f32_slice(layer.projection.as_ref(), false, projection);
            self.gl.uniform_4_f32(
                layer.rect.as_ref(),
                0.0,
                0.0,
                self.screen_w as f32,
                self.screen_h as f32,
            );
            self.gl.uniform_1_i32(layer.sampler.as_ref(), 0);
            self.gl
                .uniform_1_i32(layer.alpha.as_ref(), layer.alpha_mode);
            self.gl.bind_texture(glow::TEXTURE_2D, layer.texture);
            self.gl_state_tracker
                .bind_vertex_array(&self.gl, Some(self.quad_vao));
            self.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            self.gl_state_tracker.bind_vertex_array(&self.gl, None);
            self.gl_state_tracker.use_program(&self.gl, None);
            self.gl.bind_texture(glow::TEXTURE_2D, old_texture);
            self.gl.active_texture(active);
            self.gl
                .blend_func_separate(src_rgb, dst_rgb, src_alpha, dst_alpha);
            self.gl.blend_equation_separate(eq_rgb, eq_alpha);
            if !blend {
                self.gl.disable(glow::BLEND);
            }
        }
    }
}
