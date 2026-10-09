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

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BatchKey {
    pub program: u32,
    pub texture: u32,
    pub blend_enabled: bool,
}

#[derive(Debug)]
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

    /// Queue a compatible quad, or return it unchanged when a flush is needed.
    /// Render and clear the previous batch, then retry the returned item.
    pub(crate) fn batch_quad(
        &mut self,
        key: BatchKey,
        quad: QuadInstance,
    ) -> Result<(), (BatchKey, QuadInstance)> {
        if self.should_flush(&key) {
            return Err((key, quad));
        }

        self.current_key = Some(key);
        self.batch.push(quad);
        self.quads_batched += 1;

        Ok(())
    }

    /// Get the current batch of quads.
    pub(crate) fn current_batch(&self) -> &[QuadInstance] {
        &self.batch
    }

    /// Clear the current batch after flushing.
    pub(crate) fn clear_batch(&mut self) {
        if !self.batch.is_empty() {
            self.batches_flushed += 1;
            self.batch.clear();
        }
        self.current_key = None;
    }

    /// Check if the given key would require a flush of the current batch.
    pub(crate) fn should_flush(&self, key: &BatchKey) -> bool {
        !self.batch.is_empty()
            && (self.current_key.as_ref() != Some(key) || self.batch.len() >= self.max_batch_size)
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

#[cfg(test)]
mod batch_contract_tests {
    use super::*;
    fn key(program: u32) -> BatchKey {
        BatchKey {
            program,
            texture: 1,
            blend_enabled: true,
        }
    }
    fn quad(marker: f32) -> QuadInstance {
        QuadInstance {
            x: marker,
            y: 0.0,
            w: 1.0,
            h: 1.0,
            opacity: 1.0,
            corner_radius: 0.0,
            uv: [0.0, 0.0, 1.0, 1.0],
        }
    }
    #[test]
    fn rejected_key_change_preserves_both_batches_for_retry() {
        let mut b = RenderBatcher::new();
        b.batch_quad(key(1), quad(1.0)).unwrap();
        let (new_key, new_quad) = b.batch_quad(key(2), quad(2.0)).unwrap_err();
        assert_eq!(new_key.program, 2);
        assert_eq!(new_quad.x, 2.0);
        assert_eq!(b.current_batch().len(), 1);
        assert_eq!(b.current_batch()[0].x, 1.0);
        assert_eq!(b.current_key.as_ref().unwrap().program, 1);
        b.clear_batch();
        b.batch_quad(new_key, new_quad).unwrap();
        assert_eq!(b.current_batch().len(), 1);
        assert_eq!(b.current_batch()[0].x, 2.0);
        assert_eq!(b.current_key.as_ref().unwrap().program, 2);
    }
    #[test]
    fn capacity_boundary_rejects_without_losing_or_duplicating_quad() {
        let mut b = RenderBatcher::new();
        for _ in 0..256 {
            b.batch_quad(key(1), quad(1.0)).unwrap();
        }
        assert!(b.should_flush(&key(1)));
        let (k, q) = b.batch_quad(key(1), quad(2.0)).unwrap_err();
        assert_eq!(b.current_batch().len(), 256);
        b.clear_batch();
        b.batch_quad(k, q).unwrap();
        assert_eq!(b.current_batch().len(), 1);
        assert_eq!(b.current_batch()[0].x, 2.0);
    }
    #[test]
    fn clearing_empty_batch_does_not_flush_or_count_again() {
        let mut b = RenderBatcher::new();
        b.batch_quad(key(1), quad(1.0)).unwrap();
        b.clear_batch();
        assert!(b.current_key.is_none());
        assert!(!b.should_flush(&key(2)));
        let completed = b.batches_flushed;
        b.clear_batch();
        assert_eq!(b.batches_flushed, completed);
        b.batch_quad(key(2), quad(2.0)).unwrap();
        assert_eq!(b.current_batch().len(), 1);
        b.reset_stats();
        assert_eq!(b.batches_flushed, 0);
    }
}
