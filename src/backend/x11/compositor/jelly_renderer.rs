//! Bounded geometry resources, supersampled 4x MSAA, and tissue-only bloom.

use super::shader_cache::ShaderCache;
use super::shaders;
use super::waterlily::VolumeCamera;
use crate::backend::compositor_common::jelly_geometry::JellyVertex;
use glow::HasContext;

const MAX_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_VERTICES: usize = 2_000_000;

struct Target {
    framebuffer: glow::Framebuffer,
    texture: glow::Texture,
}

struct Targets {
    width: i32,
    height: i32,
    raster_width: i32,
    raster_height: i32,
    glow_width: i32,
    glow_height: i32,
    anatomy: Target,
    emission: Target,
    glow_a: Target,
    glow_b: Target,
    multisample: glow::Framebuffer,
    multisample_color: glow::Renderbuffer,
}

/// Deletes every partially created resource if setup or resizing fails.
struct CreationGuard<'a> {
    gl: &'a glow::Context,
    textures: Vec<glow::Texture>,
    framebuffers: Vec<glow::Framebuffer>,
    renderbuffers: Vec<glow::Renderbuffer>,
    buffers: Vec<glow::Buffer>,
    arrays: Vec<glow::VertexArray>,
    programs: Vec<glow::Program>,
}
impl<'a> CreationGuard<'a> {
    fn new(gl: &'a glow::Context) -> Self {
        Self {
            gl,
            textures: Vec::new(),
            framebuffers: Vec::new(),
            renderbuffers: Vec::new(),
            buffers: Vec::new(),
            arrays: Vec::new(),
            programs: Vec::new(),
        }
    }
    fn commit(mut self) {
        self.textures.clear();
        self.framebuffers.clear();
        self.renderbuffers.clear();
        self.buffers.clear();
        self.arrays.clear();
        self.programs.clear();
    }
}
impl Drop for CreationGuard<'_> {
    fn drop(&mut self) {
        unsafe {
            for texture in self.textures.drain(..) {
                self.gl.delete_texture(texture);
            }
            for framebuffer in self.framebuffers.drain(..) {
                self.gl.delete_framebuffer(framebuffer);
            }
            for buffer in self.renderbuffers.drain(..) {
                self.gl.delete_renderbuffer(buffer);
            }
            for buffer in self.buffers.drain(..) {
                self.gl.delete_buffer(buffer);
            }
            for array in self.arrays.drain(..) {
                self.gl.delete_vertex_array(array);
            }
            for program in self.programs.drain(..) {
                self.gl.delete_program(program);
            }
        }
    }
}

impl Targets {
    fn new(gl: &glow::Context, width: u32, height: u32) -> Result<Self, String> {
        if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_PIXELS {
            return Err("jelly render dimensions exceed bounded pixel budget".into());
        }
        let limit = unsafe { gl.get_parameter_i32(glow::MAX_TEXTURE_SIZE) };
        let width = i32::try_from(width).map_err(|_| "invalid jelly render width")?;
        let height = i32::try_from(height).map_err(|_| "invalid jelly render height")?;
        if width > limit || height > limit {
            return Err("jelly render dimensions exceed GPU limit".into());
        }
        // Fine trailing fibres can be smaller than one output pixel. A 2x
        // spatial resolve plus 4x MSAA integrates sixteen samples per output
        // pixel when it fits; large outputs retain genuine 4x edge AA within
        // the same explicit allocation ceiling.
        let supersample = if u64::from(width as u32) * u64::from(height as u32) * 4 <= MAX_PIXELS
            && width <= limit / 2
            && height <= limit / 2
        {
            2
        } else {
            1
        };
        let raster_width = width * supersample;
        let raster_height = height * supersample;
        let mut guard = CreationGuard::new(gl);
        let _state = StateGuard::new(gl);
        unsafe {
            gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, None);
            let mut target = |w: i32, h: i32| -> Result<Target, String> {
                let texture = gl.create_texture()?;
                guard.textures.push(texture);
                gl.active_texture(glow::TEXTURE0);
                gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::RGBA8 as i32,
                    w,
                    h,
                    0,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(None),
                );
                for p in [glow::TEXTURE_MIN_FILTER, glow::TEXTURE_MAG_FILTER] {
                    gl.tex_parameter_i32(glow::TEXTURE_2D, p, glow::LINEAR as i32);
                }
                for p in [glow::TEXTURE_WRAP_S, glow::TEXTURE_WRAP_T] {
                    gl.tex_parameter_i32(glow::TEXTURE_2D, p, glow::CLAMP_TO_EDGE as i32);
                }
                let framebuffer = gl.create_framebuffer()?;
                guard.framebuffers.push(framebuffer);
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
                gl.framebuffer_texture_2d(
                    glow::FRAMEBUFFER,
                    glow::COLOR_ATTACHMENT0,
                    glow::TEXTURE_2D,
                    Some(texture),
                    0,
                );
                if gl.check_framebuffer_status(glow::FRAMEBUFFER) != glow::FRAMEBUFFER_COMPLETE {
                    return Err("jelly render target incomplete".into());
                }
                Ok(Target {
                    framebuffer,
                    texture,
                })
            };
            let anatomy = target(raster_width, raster_height)?;
            let emission = target(raster_width, raster_height)?;
            let glow_width = (width + 3) / 4;
            let glow_height = (height + 3) / 4;
            let glow_a = target(glow_width, glow_height)?;
            let glow_b = target(glow_width, glow_height)?;
            let multisample = gl.create_framebuffer()?;
            guard.framebuffers.push(multisample);
            let multisample_color = gl.create_renderbuffer()?;
            guard.renderbuffers.push(multisample_color);
            gl.bind_renderbuffer(glow::RENDERBUFFER, Some(multisample_color));
            let samples = gl.get_parameter_i32(glow::MAX_SAMPLES).clamp(1, 4);
            gl.renderbuffer_storage_multisample(
                glow::RENDERBUFFER,
                samples,
                glow::RGBA8,
                raster_width,
                raster_height,
            );
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(multisample));
            gl.framebuffer_renderbuffer(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::RENDERBUFFER,
                Some(multisample_color),
            );
            if gl.check_framebuffer_status(glow::FRAMEBUFFER) != glow::FRAMEBUFFER_COMPLETE
                || gl.get_error() != glow::NO_ERROR
            {
                return Err("jelly multisample allocation failed".into());
            }
            guard.commit();
            Ok(Self {
                width,
                height,
                raster_width,
                raster_height,
                glow_width,
                glow_height,
                anatomy,
                emission,
                glow_a,
                glow_b,
                multisample,
                multisample_color,
            })
        }
    }
    fn destroy(self, gl: &glow::Context) {
        unsafe {
            for target in [self.anatomy, self.emission, self.glow_a, self.glow_b] {
                gl.delete_framebuffer(target.framebuffer);
                gl.delete_texture(target.texture);
            }
            gl.delete_framebuffer(self.multisample);
            gl.delete_renderbuffer(self.multisample_color);
        }
    }
}

pub(super) struct JellyRenderer {
    mesh_program: glow::Program,
    blur_program: glow::Program,
    composite_program: glow::Program,
    mesh_vao: glow::VertexArray,
    screen_vao: glow::VertexArray,
    buffer: glow::Buffer,
    targets: Targets,
    pub(super) frame_key: Option<(u64, u64, u32, u32)>,
    vertex_count: i32,
    bytes: Vec<u8>,
}

impl JellyRenderer {
    pub(super) fn new(
        gl: &glow::Context,
        cache: &ShaderCache,
        width: u32,
        height: u32,
    ) -> Result<Self, String> {
        let _state = StateGuard::new(gl);
        let mut guard = CreationGuard::new(gl);
        let mesh_program = cache.get_or_compile(
            gl,
            "jelly_mesh",
            shaders::JELLY_MESH_VERTEX_SHADER,
            shaders::JELLY_MESH_FRAGMENT_SHADER,
        )?;
        guard.programs.push(mesh_program);
        let blur_program = cache.get_or_compile(
            gl,
            "jelly_blur",
            shaders::JELLY_SCREEN_VERTEX_SHADER,
            shaders::JELLY_BLUR_FRAGMENT_SHADER,
        )?;
        guard.programs.push(blur_program);
        let composite_program = cache.get_or_compile(
            gl,
            "jelly_composite",
            shaders::JELLY_SCREEN_VERTEX_SHADER,
            shaders::JELLY_COMPOSITE_FRAGMENT_SHADER,
        )?;
        guard.programs.push(composite_program);
        unsafe {
            let mesh_vao = gl.create_vertex_array()?;
            guard.arrays.push(mesh_vao);
            let screen_vao = gl.create_vertex_array()?;
            guard.arrays.push(screen_vao);
            let buffer = gl.create_buffer()?;
            guard.buffers.push(buffer);
            gl.bind_vertex_array(Some(mesh_vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(buffer));
            for (location, count, offset) in [(0, 3, 0), (1, 3, 12), (2, 4, 24)] {
                gl.enable_vertex_attrib_array(location);
                gl.vertex_attrib_pointer_f32(location, count, glow::FLOAT, false, 40, offset);
            }
            let targets = Targets::new(gl, width, height)?;
            guard.commit();
            Ok(Self {
                mesh_program,
                blur_program,
                composite_program,
                mesh_vao,
                screen_vao,
                buffer,
                targets,
                frame_key: None,
                vertex_count: 0,
                bytes: Vec::new(),
            })
        }
    }

    pub(super) fn upload(
        &mut self,
        gl: &glow::Context,
        vertices: &[JellyVertex],
    ) -> Result<(), String> {
        if vertices.len() > MAX_VERTICES || !vertices.len().is_multiple_of(3) {
            return Err("jelly mesh exceeds bounded triangle budget".into());
        }
        self.bytes.clear();
        for vertex in vertices {
            for value in vertex
                .position
                .iter()
                .chain(&vertex.normal)
                .chain(&vertex.color)
            {
                if !value.is_finite() {
                    return Err("jelly mesh contains nonfinite attribute".into());
                }
                self.bytes.extend_from_slice(&value.to_ne_bytes());
            }
        }
        let _state = StateGuard::new(gl);
        unsafe {
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.buffer));
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, &self.bytes, glow::DYNAMIC_DRAW);
            if gl.get_error() != glow::NO_ERROR {
                return Err("jelly mesh GPU upload failed".into());
            }
        }
        self.vertex_count = vertices.len() as i32;
        Ok(())
    }

    pub(super) fn render(
        &mut self,
        gl: &glow::Context,
        camera: &VolumeCamera,
        size: [u32; 2],
        opacity: f32,
    ) -> Result<(), String> {
        let saved = StateGuard::new(gl);
        if self.targets.width != size[0] as i32 || self.targets.height != size[1] as i32 {
            let replacement = Targets::new(gl, size[0], size[1])?;
            std::mem::replace(&mut self.targets, replacement).destroy(gl);
        }
        let t = &self.targets;
        unsafe {
            gl.disable(glow::DEPTH_TEST);
            gl.disable(glow::CULL_FACE);
            gl.disable(glow::SCISSOR_TEST);
            gl.disable(glow::DITHER);
            if !gl.version().is_embedded {
                gl.enable(glow::MULTISAMPLE);
            }
            gl.enable(glow::BLEND);
            gl.depth_mask(false);
            gl.color_mask(true, true, true, true);
            gl.blend_equation(glow::FUNC_ADD);
            gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
            gl.use_program(Some(self.mesh_program));
            let u = |name: &str| gl.get_uniform_location(self.mesh_program, name);
            for (name, v) in [
                ("u_camera_position", camera.position),
                ("u_camera_right", camera.right),
                ("u_camera_up", camera.up),
                ("u_camera_forward", camera.forward),
                ("u_box_half_extents", camera.box_half_extents),
            ] {
                gl.uniform_3_f32(u(name).as_ref(), v[0], v[1], v[2]);
            }
            gl.uniform_1_f32(u("u_tan_half_fov").as_ref(), camera.tan_half_fov);
            gl.uniform_1_f32(u("u_aspect").as_ref(), t.width as f32 / t.height as f32);
            gl.uniform_1_f32(u("u_opacity").as_ref(), opacity);
            gl.bind_vertex_array(Some(self.mesh_vao));
            gl.viewport(0, 0, t.raster_width, t.raster_height);
            for (pass, target) in [(0, &t.anatomy), (1, &t.emission)] {
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(t.multisample));
                gl.clear_color(0.0, 0.0, 0.0, 0.0);
                gl.clear(glow::COLOR_BUFFER_BIT);
                gl.uniform_1_i32(u("u_emission_pass").as_ref(), pass);
                gl.draw_arrays(glow::TRIANGLES, 0, self.vertex_count);
                gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(t.multisample));
                gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(target.framebuffer));
                gl.blit_framebuffer(
                    0,
                    0,
                    t.raster_width,
                    t.raster_height,
                    0,
                    0,
                    t.raster_width,
                    t.raster_height,
                    glow::COLOR_BUFFER_BIT,
                    glow::NEAREST,
                );
            }
            gl.disable(glow::BLEND);
            gl.bind_vertex_array(Some(self.screen_vao));
            gl.viewport(0, 0, t.glow_width, t.glow_height);
            gl.use_program(Some(self.blur_program));
            gl.uniform_1_i32(
                gl.get_uniform_location(self.blur_program, "u_input")
                    .as_ref(),
                0,
            );
            gl.active_texture(glow::TEXTURE0);
            // Prefilter before the large reduction: a direct bilinear blit
            // takes only four source samples and can miss a subpixel fibre.
            // Mip filtering integrates its emission into stable soft light.
            gl.bind_texture(glow::TEXTURE_2D, Some(t.emission.texture));
            gl.generate_mipmap(glow::TEXTURE_2D);
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                glow::LINEAR_MIPMAP_LINEAR as i32,
            );
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(t.glow_a.framebuffer));
            gl.uniform_2_f32(
                gl.get_uniform_location(self.blur_program, "u_direction")
                    .as_ref(),
                0.0,
                0.0,
            );
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            for (input, output, direction) in [
                (&t.glow_a, &t.glow_b, [1.0 / t.glow_width as f32, 0.0]),
                (&t.glow_b, &t.glow_a, [0.0, 1.0 / t.glow_height as f32]),
            ] {
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(output.framebuffer));
                gl.bind_texture(glow::TEXTURE_2D, Some(input.texture));
                gl.uniform_2_f32(
                    gl.get_uniform_location(self.blur_program, "u_direction")
                        .as_ref(),
                    direction[0],
                    direction[1],
                );
                gl.draw_arrays(glow::TRIANGLES, 0, 3);
            }
            gl.bind_framebuffer(glow::FRAMEBUFFER, saved.draw_framebuffer);
            gl.viewport(0, 0, t.width, t.height);
            // Offscreen work must ignore desktop damage clipping, but the
            // final source-over pass must obey the caller's damaged region.
            // The scissor rectangle itself has never been changed here.
            if saved
                .capabilities
                .iter()
                .any(|(cap, enabled)| *cap == glow::SCISSOR_TEST && *enabled)
            {
                gl.enable(glow::SCISSOR_TEST);
            }
            gl.enable(glow::BLEND);
            gl.use_program(Some(self.composite_program));
            gl.uniform_1_i32(
                gl.get_uniform_location(self.composite_program, "u_anatomy")
                    .as_ref(),
                0,
            );
            gl.uniform_1_i32(
                gl.get_uniform_location(self.composite_program, "u_glow")
                    .as_ref(),
                1,
            );
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(t.anatomy.texture));
            gl.active_texture(glow::TEXTURE1);
            gl.bind_texture(glow::TEXTURE_2D, Some(t.glow_a.texture));
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            if gl.get_error() != glow::NO_ERROR {
                return Err("jelly geometry or bloom draw failed".into());
            }
        }
        Ok(())
    }

    pub(super) fn destroy(self, gl: &glow::Context) {
        self.targets.destroy(gl);
        unsafe {
            gl.delete_buffer(self.buffer);
            gl.delete_vertex_array(self.mesh_vao);
            gl.delete_vertex_array(self.screen_vao);
            gl.delete_program(self.mesh_program);
            gl.delete_program(self.blur_program);
            gl.delete_program(self.composite_program);
        }
    }
}

/// Restore actual GL state exactly, leaving the compositor's cached state
/// truthful on success, resizing, and every early error return.
struct StateGuard<'a> {
    gl: &'a glow::Context,
    draw_framebuffer: Option<glow::Framebuffer>,
    read_framebuffer: Option<glow::Framebuffer>,
    renderbuffer: Option<glow::Renderbuffer>,
    program: Option<glow::Program>,
    vao: Option<glow::VertexArray>,
    buffer: Option<glow::Buffer>,
    unpack_buffer: Option<glow::Buffer>,
    active_texture: u32,
    textures: [Option<glow::Texture>; 2],
    viewport: [i32; 4],
    color_mask: [i32; 4],
    clear_color: [f32; 4],
    capabilities: [(u32, bool); 5],
    multisample: Option<bool>,
    depth_write: bool,
    blend: [u32; 6],
}
impl<'a> StateGuard<'a> {
    fn new(gl: &'a glow::Context) -> Self {
        unsafe {
            let active_texture = gl.get_parameter_i32(glow::ACTIVE_TEXTURE) as u32;
            gl.active_texture(glow::TEXTURE0);
            let texture0 = gl.get_parameter_texture(glow::TEXTURE_BINDING_2D);
            gl.active_texture(glow::TEXTURE1);
            let texture1 = gl.get_parameter_texture(glow::TEXTURE_BINDING_2D);
            gl.active_texture(active_texture);
            let mut viewport = [0; 4];
            gl.get_parameter_i32_slice(glow::VIEWPORT, &mut viewport);
            let mut color_mask = [0; 4];
            gl.get_parameter_i32_slice(glow::COLOR_WRITEMASK, &mut color_mask);
            let mut clear_color = [0.0; 4];
            gl.get_parameter_f32_slice(glow::COLOR_CLEAR_VALUE, &mut clear_color);
            Self {
                gl,
                draw_framebuffer: gl.get_parameter_framebuffer(glow::DRAW_FRAMEBUFFER_BINDING),
                read_framebuffer: gl.get_parameter_framebuffer(glow::READ_FRAMEBUFFER_BINDING),
                renderbuffer: gl.get_parameter_renderbuffer(glow::RENDERBUFFER_BINDING),
                program: gl.get_parameter_program(glow::CURRENT_PROGRAM),
                vao: gl.get_parameter_vertex_array(glow::VERTEX_ARRAY_BINDING),
                buffer: gl.get_parameter_buffer(glow::ARRAY_BUFFER_BINDING),
                unpack_buffer: gl.get_parameter_buffer(glow::PIXEL_UNPACK_BUFFER_BINDING),
                active_texture,
                textures: [texture0, texture1],
                viewport,
                color_mask,
                clear_color,
                capabilities: [
                    glow::DEPTH_TEST,
                    glow::CULL_FACE,
                    glow::SCISSOR_TEST,
                    glow::DITHER,
                    glow::BLEND,
                ]
                .map(|cap| (cap, gl.is_enabled(cap))),
                multisample: (!gl.version().is_embedded).then(|| gl.is_enabled(glow::MULTISAMPLE)),
                depth_write: gl.get_parameter_i32(glow::DEPTH_WRITEMASK) != 0,
                blend: [
                    glow::BLEND_SRC_RGB,
                    glow::BLEND_DST_RGB,
                    glow::BLEND_SRC_ALPHA,
                    glow::BLEND_DST_ALPHA,
                    glow::BLEND_EQUATION_RGB,
                    glow::BLEND_EQUATION_ALPHA,
                ]
                .map(|p| gl.get_parameter_i32(p) as u32),
            }
        }
    }
}
impl Drop for StateGuard<'_> {
    fn drop(&mut self) {
        unsafe {
            let gl = self.gl;
            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, self.draw_framebuffer);
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, self.read_framebuffer);
            gl.bind_renderbuffer(glow::RENDERBUFFER, self.renderbuffer);
            gl.use_program(self.program);
            gl.bind_vertex_array(self.vao);
            gl.bind_buffer(glow::ARRAY_BUFFER, self.buffer);
            gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, self.unpack_buffer);
            for (unit, texture) in self.textures.iter().enumerate() {
                gl.active_texture(glow::TEXTURE0 + unit as u32);
                gl.bind_texture(glow::TEXTURE_2D, *texture);
            }
            gl.active_texture(self.active_texture);
            gl.viewport(
                self.viewport[0],
                self.viewport[1],
                self.viewport[2],
                self.viewport[3],
            );
            gl.color_mask(
                self.color_mask[0] != 0,
                self.color_mask[1] != 0,
                self.color_mask[2] != 0,
                self.color_mask[3] != 0,
            );
            gl.clear_color(
                self.clear_color[0],
                self.clear_color[1],
                self.clear_color[2],
                self.clear_color[3],
            );
            gl.depth_mask(self.depth_write);
            gl.blend_func_separate(self.blend[0], self.blend[1], self.blend[2], self.blend[3]);
            gl.blend_equation_separate(self.blend[4], self.blend[5]);
            for (capability, enabled) in self.capabilities {
                if enabled {
                    gl.enable(capability);
                } else {
                    gl.disable(capability);
                }
            }
            if let Some(enabled) = self.multisample {
                if enabled {
                    gl.enable(glow::MULTISAMPLE);
                } else {
                    gl.disable(glow::MULTISAMPLE);
                }
            }
        }
    }
}

/// Exercise actual GL resources and state restoration from the shared headless
/// harness, including the real geometry, MSAA resolve, mip reduction and bloom.
#[cfg(all(test, feature = "wayland-backends"))]
pub(crate) fn check_pipeline_for_test(gl: &glow::Context, cache_path: std::path::PathBuf) {
    use crate::backend::compositor_common::jelly_geometry::build_jelly_mesh;
    use crate::backend::compositor_common::waterlily::JellyPose;
    let saved = StateGuard::new(gl);
    let cache = ShaderCache::new(cache_path);
    let camera = VolumeCamera {
        position: [0.0, 0.1, -1.5],
        right: [1.0, 0.0, 0.0],
        up: [0.0, 1.0, 0.0],
        forward: [0.0, 0.0, 1.0],
        tan_half_fov: 0.35,
        box_half_extents: [0.5; 3],
    };
    let pose = JellyPose {
        center: [0.0, 0.15, 0.0],
        radius: 0.15,
        squeeze: 0.96,
        theta: 0.7,
        axis_shift: 0.01,
        mouth_y: 0.15,
    };
    let vertices = build_jelly_mesh(&[pose], 1, camera.position);
    let output = Targets::new(gl, 64, 64).unwrap();
    let mut renderer = JellyRenderer::new(gl, &cache, 64, 64).unwrap();
    renderer.upload(gl, &vertices).unwrap();
    unsafe {
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(output.glow_a.framebuffer));
        // Deliberately hostile inherited state: it must neither clip the
        // anatomy nor leak changes back into the following compositor pass.
        gl.enable(glow::DEPTH_TEST);
        gl.enable(glow::CULL_FACE);
        gl.enable(glow::SCISSOR_TEST);
        gl.disable(glow::BLEND);
        gl.scissor(0, 0, 16, 16);
        gl.viewport(2, 3, 11, 12);
        gl.depth_mask(true);
        gl.color_mask(false, true, false, true);
        gl.blend_func(glow::SRC_ALPHA, glow::ONE);
        gl.active_texture(glow::TEXTURE5);
        let before = StateGuard::new(gl);
        let mut frames = Vec::new();
        for _ in 0..2 {
            // The target is 16x16. Rendering at this size also exercises the
            // renderer's transactional resize after its initial 64x64 setup.
            gl.disable(glow::SCISSOR_TEST);
            gl.color_mask(true, true, true, true);
            gl.clear_color(0.0, 0.0, 0.0, 0.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
            gl.enable(glow::SCISSOR_TEST);
            gl.color_mask(false, true, false, true);
            renderer.render(gl, &camera, [16, 16], 1.0).unwrap();
            let mut pixels = vec![0_u8; 16 * 16 * 4];
            gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
            gl.read_pixels(
                0,
                0,
                16,
                16,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut pixels)),
            );
            frames.push(pixels);
        }
        assert_eq!(
            frames[0], frames[1],
            "unchanged mesh and bloom must be repeatable"
        );
        assert!(
            frames[0].chunks_exact(4).any(|p| p[3] > 4),
            "actual anatomy must draw visible tissue"
        );
        for p in frames[0].chunks_exact(4) {
            assert!(
                p[..3].iter().all(|c| *c <= p[3].saturating_add(1)),
                "geometry/bloom must remain premultiplied: {p:?}"
            );
        }
        let after = StateGuard::new(gl);
        assert_eq!(before.draw_framebuffer, after.draw_framebuffer);
        assert_eq!(before.read_framebuffer, after.read_framebuffer);
        assert_eq!(before.viewport, after.viewport);
        assert_eq!(before.color_mask, after.color_mask);
        assert_eq!(before.capabilities, after.capabilities);
        assert_eq!(before.active_texture, after.active_texture);
        assert_eq!(before.textures, after.textures);
        assert_eq!(before.blend, after.blend);
        assert_eq!(before.depth_write, after.depth_write);
        assert_eq!(before.vao, after.vao);
        assert_eq!(before.program, after.program);
        assert_eq!(
            gl.get_error(),
            glow::NO_ERROR,
            "real geometry pipeline must not produce GL errors"
        );
        drop(after);
        drop(before);

        // A partial-damage repaint must leave every pixel outside the saved
        // scissor rectangle byte-identical, even though all offscreen targets
        // were fully regenerated and blurred.
        gl.disable(glow::SCISSOR_TEST);
        gl.color_mask(true, true, true, true);
        gl.clear_color(10.0 / 255.0, 20.0 / 255.0, 30.0 / 255.0, 1.0);
        gl.clear(glow::COLOR_BUFFER_BIT);
        gl.scissor(0, 0, 8, 16);
        gl.enable(glow::SCISSOR_TEST);
        renderer.render(gl, &camera, [16, 16], 1.0).unwrap();
        let mut partial = vec![0_u8; 16 * 16 * 4];
        gl.read_pixels(
            0,
            0,
            16,
            16,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelPackData::Slice(Some(&mut partial)),
        );
        for y in 0..16 {
            for x in 8..16 {
                let offset = (y * 16 + x) * 4;
                assert_eq!(
                    &partial[offset..offset + 4],
                    &[10, 20, 30, 255],
                    "jelly composition must preserve pixels outside damage"
                );
            }
        }
        assert!(
            partial
                .chunks_exact(4)
                .any(|pixel| pixel != [10, 20, 30, 255]),
            "partial-damage test must actually draw anatomy inside damage"
        );
        let before_failure = StateGuard::new(gl);
        assert!(
            renderer.render(gl, &camera, [8192, 8192], 1.0).is_err(),
            "oversized targets must fail before unbounded allocation"
        );
        let after_failure = StateGuard::new(gl);
        assert_eq!(
            before_failure.draw_framebuffer,
            after_failure.draw_framebuffer
        );
        assert_eq!(before_failure.viewport, after_failure.viewport);
        assert_eq!(before_failure.capabilities, after_failure.capabilities);
        assert_eq!(before_failure.blend, after_failure.blend);
        assert_eq!(before_failure.program, after_failure.program);
        assert_eq!(gl.get_error(), glow::NO_ERROR);
        drop(after_failure);
        drop(before_failure);
    }
    renderer.destroy(gl);
    output.destroy(gl);
    drop(saved);
}
