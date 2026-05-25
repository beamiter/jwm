use smithay::backend::renderer::gles::ffi;

pub(crate) struct GLStateTracker {
    current_program: u32,
    current_texture: u32,
    current_vao: u32,
    current_fbo: u32,
    blend_enabled: Option<bool>,
    scissor_enabled: Option<bool>,
    redundant_avoided: u64,
}

impl GLStateTracker {
    pub(crate) fn new() -> Self {
        Self {
            current_program: 0,
            current_texture: 0,
            current_vao: 0,
            current_fbo: 0,
            blend_enabled: None,
            scissor_enabled: None,
            redundant_avoided: 0,
        }
    }

    /// Use a shader program. Returns true if the state actually changed.
    /// Skips the GL call if the program is already bound.
    pub(crate) unsafe fn use_program(&mut self, gl: &ffi::Gles2, program: u32) -> bool {
        if self.current_program == program {
            self.redundant_avoided += 1;
            return false;
        }
        unsafe { gl.UseProgram(program) };
        self.current_program = program;
        true
    }

    pub(crate) unsafe fn bind_texture(&mut self, gl: &ffi::Gles2, texture: u32) -> bool {
        if self.current_texture == texture {
            self.redundant_avoided += 1;
            return false;
        }
        unsafe { gl.BindTexture(ffi::TEXTURE_2D, texture) };
        self.current_texture = texture;
        true
    }

    pub(crate) unsafe fn bind_vao(&mut self, gl: &ffi::Gles2, vao: u32) -> bool {
        if self.current_vao == vao {
            self.redundant_avoided += 1;
            return false;
        }
        unsafe { gl.BindVertexArray(vao) };
        self.current_vao = vao;
        true
    }

    pub(crate) unsafe fn bind_fbo(&mut self, gl: &ffi::Gles2, fbo: u32) -> bool {
        if self.current_fbo == fbo {
            self.redundant_avoided += 1;
            return false;
        }
        unsafe { gl.BindFramebuffer(ffi::FRAMEBUFFER, fbo) };
        self.current_fbo = fbo;
        true
    }

    pub(crate) unsafe fn set_blend(&mut self, gl: &ffi::Gles2, enabled: bool) -> bool {
        if self.blend_enabled == Some(enabled) {
            self.redundant_avoided += 1;
            return false;
        }
        unsafe {
            if enabled {
                gl.Enable(ffi::BLEND);
            } else {
                gl.Disable(ffi::BLEND);
            }
        }
        self.blend_enabled = Some(enabled);
        true
    }

    pub(crate) unsafe fn set_scissor(&mut self, gl: &ffi::Gles2, enabled: bool) -> bool {
        if self.scissor_enabled == Some(enabled) {
            self.redundant_avoided += 1;
            return false;
        }
        unsafe {
            if enabled {
                gl.Enable(ffi::SCISSOR_TEST);
            } else {
                gl.Disable(ffi::SCISSOR_TEST);
            }
        }
        self.scissor_enabled = Some(enabled);
        true
    }

    /// Reset all tracked state to unknown. Call at frame start when GL state is uncertain.
    pub(crate) fn reset(&mut self) {
        self.current_program = 0;
        self.current_texture = 0;
        self.current_vao = 0;
        self.current_fbo = 0;
        self.blend_enabled = None;
        self.scissor_enabled = None;
    }

    pub(crate) fn redundant_changes_avoided(&self) -> u64 {
        self.redundant_avoided
    }

    pub(crate) fn reset_stats(&mut self) {
        self.redundant_avoided = 0;
    }
}

#[derive(Clone, PartialEq)]
pub(crate) struct BatchKey {
    pub program: u32,
    pub texture: u32,
    pub blend_enabled: bool,
}

pub(crate) struct QuadInstance {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub opacity: f32,
    pub corner_radius: f32,
    pub uv: [f32; 4],
}

pub(crate) struct RenderBatcher {
    current_key: Option<BatchKey>,
    batch: Vec<QuadInstance>,
    batches_flushed: u64,
    quads_batched: u64,
    max_batch_size: usize,
}

impl RenderBatcher {
    pub(crate) fn new() -> Self {
        Self {
            current_key: None,
            batch: Vec::with_capacity(128),
            batches_flushed: 0,
            quads_batched: 0,
            max_batch_size: 256,
        }
    }

    /// Add a quad to the current batch. If the key differs from the current batch key,
    /// returns true indicating the caller should flush the previous batch first.
    /// Stores the new key and pushes the quad.
    pub(crate) fn batch_quad(&mut self, key: BatchKey, quad: QuadInstance) -> bool {
        let needs_flush = match &self.current_key {
            Some(current) => current != &key,
            None => false,
        };

        if needs_flush {
            self.batches_flushed += 1;
        }

        self.current_key = Some(key);
        self.batch.push(quad);
        self.quads_batched += 1;

        needs_flush
    }

    /// Get the current batch of quads.
    pub(crate) fn current_batch(&self) -> &[QuadInstance] {
        &self.batch
    }

    /// Clear the current batch after flushing.
    pub(crate) fn clear_batch(&mut self) {
        self.batch.clear();
    }

    /// Check if the given key would require a flush of the current batch.
    pub(crate) fn should_flush(&self, key: &BatchKey) -> bool {
        match &self.current_key {
            Some(current) => current != key,
            None => false,
        }
    }

    /// Returns the batch efficiency ratio: quads_batched / (batches_flushed * max_batch_size).
    /// Returns 0.0 if no batches have been flushed.
    pub(crate) fn batch_efficiency(&self) -> f32 {
        if self.batches_flushed == 0 {
            return 0.0;
        }
        self.quads_batched as f32 / (self.batches_flushed as f32 * self.max_batch_size as f32)
    }

    pub(crate) fn reset_stats(&mut self) {
        self.batches_flushed = 0;
        self.quads_batched = 0;
    }
}
