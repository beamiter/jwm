//! Bounded DVR v1 SDR ingress for the native Wayland compositor.
//! The worker owns sockets and validation; GL only consumes the latest frame.
use super::{ThumbnailGlesState, WaylandCompositor, create_program, get_uniform_loc, shaders};
use desktop_visual::{OutputSize, Server};
use smithay::backend::renderer::gles::ffi;
use std::{path::PathBuf, sync::Arc};

const FRAGMENT_SHADER: &str = r#"#version 300 es
precision highp float;
uniform sampler2D u_texture;
uniform int u_alpha;
uniform int u_scene_linear;
in vec2 v_uv;
out vec4 frag_color;
vec4 premultiplied_texel(ivec2 p, ivec2 size) {
    vec4 c = texelFetch(u_texture, clamp(p, ivec2(0), size - 1), 0);
    return vec4(c.rgb * c.a, c.a);
}
vec3 srgb_inverse(vec3 c) {
    return mix(c / 12.92, pow(max((c + 0.055) / 1.055, 0.0), vec3(2.4)), step(0.04045, c));
}
void main() {
    vec4 c;
    if (u_alpha == 2) {
        ivec2 size = textureSize(u_texture, 0);
        vec2 p = v_uv * vec2(size) - 0.5;
        ivec2 base = ivec2(floor(p));
        vec2 f = fract(p);
        c = mix(mix(premultiplied_texel(base, size), premultiplied_texel(base + ivec2(1, 0), size), f.x),
                mix(premultiplied_texel(base + ivec2(0, 1), size), premultiplied_texel(base + ivec2(1, 1), size), f.x), f.y);
    } else {
        c = texture(u_texture, v_uv);
    }
    if (u_alpha == 1) c.a = 1.0;
    else if (u_alpha == 3) c.rgb = min(c.rgb, vec3(c.a));
    if (u_scene_linear == 1) {
        c.rgb = c.a > 0.0 ? srgb_inverse(c.rgb / c.a) * c.a : vec3(0.0);
    }
    frag_color = c;
}
"#;

pub(super) struct DesktopVisualLayer {
    server: Server,
    program: u32,
    texture: u32,
    texture_size: (u32, u32),
    texture_epoch: u64,
    output: OutputSize,
    alpha: i32,
    socket_path: PathBuf,
    output_name: Option<String>,
    rect: Option<[f32; 4]>,
    routes: Vec<DesktopVisualLayer>,
    route_configuration: Vec<(String, [f32; 4], PathBuf)>,
    waker: Arc<dyn Fn() + Send + Sync>,
    #[cfg(test)]
    wake_count: Arc<std::sync::atomic::AtomicUsize>,
}

impl DesktopVisualLayer {
    #[cfg(test)]
    pub(super) fn wake_count_for_test(&self) -> usize {
        self.wake_count.load(std::sync::atomic::Ordering::Acquire)
    }

    pub(super) fn visible(&self) -> bool {
        self.own_visible() || self.routes.iter().any(Self::visible)
    }

    fn own_visible(&self) -> bool {
        self.texture != 0
            && self.server.connected()
            && self.texture_epoch == self.server.connection_epoch()
    }

    pub(super) fn pending(&self) -> bool {
        self.server.has_pending() || self.routes.iter().any(Self::pending)
    }

    unsafe fn destroy(mut self, gl: &ffi::Gles2) {
        unsafe {
            for route in self.routes.drain(..) {
                route.destroy(gl);
            }
            if self.texture != 0 {
                gl.DeleteTextures(1, &self.texture);
                self.texture = 0;
            }
            gl.DeleteProgram(self.program);
        }
        // Server Drop joins its bounded worker and removes only its socket.
    }
}

impl WaylandCompositor {
    pub(crate) unsafe fn initialize_desktop_visual(
        &mut self,
        gl: &ffi::Gles2,
        signal: calloop::LoopSignal,
    ) -> Result<(), String> {
        if self.desktop_visual.is_some()
            || !std::env::var("JWM_DESKTOP_VISUAL_ENABLED")
                .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        {
            return Ok(());
        }
        let path = if let Some(path) = std::env::var_os("JWM_DESKTOP_VISUAL_SOCKET") {
            PathBuf::from(path)
        } else {
            PathBuf::from(
                std::env::var_os("XDG_RUNTIME_DIR")
                    .ok_or("DVR requires XDG_RUNTIME_DIR or JWM_DESKTOP_VISUAL_SOCKET")?,
            )
            .join("jwm-desktop-visual/visual.sock")
        };
        unsafe { self.initialize_desktop_visual_at(gl, path, signal) }
    }

    /// Explicit endpoint shared by production setup and isolated GL/socket
    /// acceptance. Requires the compositor's current GLES context.
    pub(super) unsafe fn initialize_desktop_visual_at(
        &mut self,
        gl: &ffi::Gles2,
        path: PathBuf,
        signal: calloop::LoopSignal,
    ) -> Result<(), String> {
        if self.desktop_visual.is_some() {
            return Err("DVR endpoint already initialized".into());
        }
        let output = OutputSize {
            width: self.screen_w,
            height: self.screen_h,
        };
        let server = Server::bind(path.clone(), output).map_err(|error| error.to_string())?;
        let program = unsafe { create_program(gl, shaders::VERTEX_SHADER, FRAGMENT_SHADER)? };
        #[cfg(test)]
        let wake_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        #[cfg(test)]
        let observed_wake_count = Arc::clone(&wake_count);
        let waker: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            signal.wakeup();
            // Observe completion only; never inject a notification or alter the
            // production handoff. Server::wake increments revision before this.
            #[cfg(test)]
            observed_wake_count.fetch_add(1, std::sync::atomic::Ordering::Release);
        });
        server.set_waker(waker.clone());
        self.desktop_visual = Some(DesktopVisualLayer {
            server,
            program,
            texture: 0,
            texture_size: (0, 0),
            texture_epoch: 0,
            output,
            alpha: 1,
            socket_path: path,
            output_name: None,
            rect: None,
            routes: Vec::new(),
            route_configuration: Vec::new(),
            waker,
            #[cfg(test)]
            wake_count,
        });
        Ok(())
    }

    /// Independent v1 endpoints negotiate the selected output's extent.
    /// The map is administrator configuration, never producer-selected geometry.
    pub(crate) unsafe fn configure_desktop_visual_outputs(
        &mut self,
        gl: &ffi::Gles2,
        outputs: &[(String, [f32; 4])],
    ) {
        let configured: std::collections::BTreeMap<String, PathBuf> =
            std::env::var("JWM_DESKTOP_VISUAL_OUTPUTS")
                .ok()
                .and_then(|value| serde_json::from_str(&value).ok())
                .unwrap_or_default();
        unsafe {
            self.configure_desktop_visual_outputs_at(gl, outputs, &configured);
        }
    }

    pub(super) unsafe fn configure_desktop_visual_outputs_at(
        &mut self,
        gl: &ffi::Gles2,
        outputs: &[(String, [f32; 4])],
        configured: &std::collections::BTreeMap<String, PathBuf>,
    ) {
        let Some(root) = self.desktop_visual.as_mut() else {
            return;
        };
        let desired: Vec<_> = outputs
            .iter()
            .filter_map(|(name, rect)| {
                configured
                    .get(name)
                    .map(|path| (name.clone(), *rect, path.clone()))
            })
            .take(4)
            .collect();
        if desired == root.route_configuration {
            return;
        }
        root.route_configuration = desired;
        let mut changed = false;
        // Drop absent/moved/reconfigured outputs, even when their extents are
        // equal: an old connection never migrates onto a different target.
        let mut retained = Vec::new();
        for layer in root.routes.drain(..) {
            if root.route_configuration.iter().any(|(name, rect, path)| {
                Some(name) == layer.output_name.as_ref()
                    && Some(*rect) == layer.rect
                    && path == &layer.socket_path
            }) {
                retained.push(layer);
            } else {
                unsafe { layer.destroy(gl) };
                changed = true;
            }
        }
        root.routes = retained;
        for (name, rect) in outputs
            .iter()
            .filter(|(name, _)| configured.contains_key(name))
            .take(4)
        {
            let Some(path) = configured.get(name) else {
                continue;
            };
            if root
                .routes
                .iter()
                .any(|layer| layer.output_name.as_ref() == Some(name))
            {
                continue;
            }
            let output = OutputSize {
                width: rect[2] as u32,
                height: rect[3] as u32,
            };
            let server = match Server::bind(path.clone(), output) {
                Ok(server) => server,
                Err(error) => {
                    log::warn!("[wayland/DVR] output {name} endpoint rejected: {error}");
                    continue;
                }
            };
            let program =
                match unsafe { create_program(gl, shaders::VERTEX_SHADER, FRAGMENT_SHADER) } {
                    Ok(program) => program,
                    Err(error) => {
                        log::warn!("[wayland/DVR] output {name} shader: {error}");
                        continue;
                    }
                };
            server.set_waker(root.waker.clone());
            root.routes.push(DesktopVisualLayer {
                server,
                program,
                texture: 0,
                texture_size: (0, 0),
                texture_epoch: 0,
                output,
                alpha: 1,
                socket_path: path.clone(),
                output_name: Some(name.clone()),
                rect: Some(*rect),
                routes: Vec::new(),
                route_configuration: Vec::new(),
                waker: root.waker.clone(),
                #[cfg(test)]
                wake_count: Arc::clone(&root.wake_count),
            });
            changed = true;
        }
        if changed {
            self.force_full_redraw();
        }
    }

    pub(super) unsafe fn release_desktop_visual(&mut self, gl: &ffi::Gles2) {
        if let Some(layer) = self.desktop_visual.take() {
            unsafe { layer.destroy(gl) };
        }
    }

    pub(super) unsafe fn poll_desktop_visual(&mut self, gl: &ffi::Gles2) {
        let Some(layer) = self.desktop_visual.as_mut() else {
            return;
        };
        let canvas = OutputSize {
            width: self.screen_w,
            height: self.screen_h,
        };
        let mut routes = std::mem::take(&mut layer.routes);
        let mut changed = false;
        for layer in std::iter::once(&mut *layer).chain(routes.iter_mut()) {
            let output = layer
                .rect
                .map(|r| OutputSize {
                    width: r[2] as u32,
                    height: r[3] as u32,
                })
                .unwrap_or(canvas);
            unsafe {
                if output != layer.output {
                    if let Err(error) = layer.server.set_output_size(output) {
                        log::warn!("[wayland/DVR] output resize rejected: {error}");
                        return;
                    }
                    layer.output = output;
                    if layer.texture != 0 {
                        gl.DeleteTextures(1, &layer.texture);
                        layer.texture = 0;
                        changed = true;
                    }
                }
                if layer.texture != 0
                    && (!layer.server.connected()
                        || layer.texture_epoch != layer.server.connection_epoch())
                {
                    gl.DeleteTextures(1, &layer.texture);
                    layer.texture = 0;
                    changed = true;
                }
                if let Some(frame) = layer.server.try_take_latest()
                    && layer.server.connected()
                    && frame.connection_epoch == layer.server.connection_epoch()
                {
                    let (w, h) = (frame.header.width, frame.header.height);
                    let mut max_size = 0;
                    gl.GetIntegerv(super::GL_MAX_TEXTURE_SIZE, &mut max_size);
                    if w <= max_size.max(0) as u32 && h <= max_size.max(0) as u32 {
                        let _state = ThumbnailGlesState::begin(gl);
                        let mut row_length = 0;
                        let mut skip_rows = 0;
                        let mut skip_pixels = 0;
                        gl.GetIntegerv(ffi::UNPACK_ROW_LENGTH, &mut row_length);
                        gl.GetIntegerv(ffi::UNPACK_SKIP_ROWS, &mut skip_rows);
                        gl.GetIntegerv(ffi::UNPACK_SKIP_PIXELS, &mut skip_pixels);
                        gl.PixelStorei(ffi::UNPACK_ROW_LENGTH, 0);
                        gl.PixelStorei(ffi::UNPACK_SKIP_ROWS, 0);
                        gl.PixelStorei(ffi::UNPACK_SKIP_PIXELS, 0);
                        for _ in 0..8 {
                            if gl.GetError() == ffi::NO_ERROR {
                                break;
                            }
                        }
                        let reuse = layer.texture != 0 && layer.texture_size == (w, h);
                        let mut texture = layer.texture;
                        if !reuse {
                            gl.GenTextures(1, &mut texture);
                        }
                        gl.BindTexture(ffi::TEXTURE_2D, texture);
                        for key in [ffi::TEXTURE_MIN_FILTER, ffi::TEXTURE_MAG_FILTER] {
                            gl.TexParameteri(ffi::TEXTURE_2D, key, ffi::LINEAR as i32);
                        }
                        for key in [ffi::TEXTURE_WRAP_S, ffi::TEXTURE_WRAP_T] {
                            gl.TexParameteri(ffi::TEXTURE_2D, key, ffi::CLAMP_TO_EDGE as i32);
                        }
                        if reuse {
                            gl.TexSubImage2D(
                                ffi::TEXTURE_2D,
                                0,
                                0,
                                0,
                                w as i32,
                                h as i32,
                                ffi::RGBA,
                                ffi::UNSIGNED_BYTE,
                                frame.rgba.as_ptr().cast(),
                            );
                        } else {
                            gl.TexImage2D(
                                ffi::TEXTURE_2D,
                                0,
                                ffi::RGBA8 as i32,
                                w as i32,
                                h as i32,
                                0,
                                ffi::RGBA,
                                ffi::UNSIGNED_BYTE,
                                frame.rgba.as_ptr().cast(),
                            );
                        }
                        let uploaded = texture != 0 && gl.GetError() == ffi::NO_ERROR;
                        gl.PixelStorei(ffi::UNPACK_ROW_LENGTH, row_length);
                        gl.PixelStorei(ffi::UNPACK_SKIP_ROWS, skip_rows);
                        gl.PixelStorei(ffi::UNPACK_SKIP_PIXELS, skip_pixels);
                        if uploaded {
                            if !reuse && layer.texture != 0 {
                                gl.DeleteTextures(1, &layer.texture);
                            }
                            layer.texture = texture;
                            layer.texture_size = (w, h);
                            layer.texture_epoch = frame.connection_epoch;
                            layer.alpha = frame.header.alpha as i32;
                            changed = true;
                        } else {
                            if texture != 0 {
                                gl.DeleteTextures(1, &texture);
                            }
                            if reuse {
                                layer.texture = 0;
                                changed = true;
                            }
                            log::warn!("[wayland/DVR] RGBA upload failed");
                        }
                    }
                }
            }
        }
        layer.routes = routes;
        if changed {
            self.force_full_redraw();
        }
    }

    pub(super) unsafe fn render_desktop_visual(
        &mut self,
        gl: &ffi::Gles2,
        projection: &[f32; 16],
        scene_linear: bool,
    ) {
        let Some(root) = self.desktop_visual.as_ref() else {
            return;
        };
        for layer in std::iter::once(root)
            .chain(root.routes.iter())
            .filter(|layer| layer.own_visible())
        {
            let rect = layer
                .rect
                .unwrap_or([0.0, 0.0, self.screen_w as f32, self.screen_h as f32]);
            unsafe {
                let program = layer.program;
                gl.UseProgram(program);
                gl.UniformMatrix4fv(
                    get_uniform_loc(gl, program, "u_projection"),
                    1,
                    ffi::FALSE,
                    projection.as_ptr(),
                );
                gl.Uniform4f(
                    get_uniform_loc(gl, program, "u_rect"),
                    rect[0],
                    rect[1],
                    rect[2],
                    rect[3],
                );
                gl.Uniform1i(get_uniform_loc(gl, program, "u_texture"), 0);
                gl.Uniform1i(get_uniform_loc(gl, program, "u_alpha"), layer.alpha);
                gl.Uniform1i(
                    get_uniform_loc(gl, program, "u_scene_linear"),
                    i32::from(scene_linear),
                );
                gl.ActiveTexture(ffi::TEXTURE0);
                gl.BindTexture(ffi::TEXTURE_2D, layer.texture);
                gl.Enable(ffi::BLEND);
                gl.BlendFunc(ffi::ONE, ffi::ONE_MINUS_SRC_ALPHA);
                gl.BindVertexArray(self.quad_vao);
                self.draw_arrays(gl, ffi::TRIANGLE_STRIP, 0, 4);
                gl.BindVertexArray(0);
                gl.UseProgram(0);
            }
        }
    }
}
