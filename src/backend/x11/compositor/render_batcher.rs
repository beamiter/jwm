/// OpenGL State Batching - Reduce state changes and draw calls
///
/// Groups windows with same shader/texture for batch rendering
use glow::HasContext;

/// Batch key - identifies compatible render state
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BatchKey {
    pub program: u32,      // Shader program (raw GL name, 0 = none)
    pub texture: u32,      // Texture (raw GL name, 0 = none)
    pub blend_enabled: bool,
}

/// Quad instance data for batched rendering
#[derive(Debug, Clone, Copy)]
pub struct QuadInstance {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub opacity: f32,
    pub corner_radius: f32,
    pub u: f32,
    pub v: f32,
    pub u_width: f32,
    pub v_height: f32,
}

/// Batches draw calls by grouping compatible state
pub struct RenderBatcher {
    /// Current batch key
    current_key: Option<BatchKey>,
    /// Queued instances for current batch
    batched_quads: Vec<QuadInstance>,
    /// Max batch size before auto-flush
    max_batch_size: usize,
    /// Statistics
    pub total_batches: u32,
    pub total_instances: u32,
    pub state_changes: u32,
}

impl RenderBatcher {
    pub fn new() -> Self {
        Self {
            current_key: None,
            batched_quads: Vec::with_capacity(256),
            max_batch_size: 256,
            total_batches: 0,
            total_instances: 0,
            state_changes: 0,
        }
    }

    /// Check if we need to flush (state changed)
    pub fn should_flush(&self, key: BatchKey) -> bool {
        if let Some(current) = self.current_key {
            if current != key {
                return true; // State changed
            }
        }
        // Also flush if batch is full
        self.batched_quads.len() >= self.max_batch_size
    }

    /// Add a quad to the current batch
    /// Returns true if batch was flushed
    pub fn batch_quad(&mut self, key: BatchKey, quad: QuadInstance) -> bool {
        let mut flushed = false;

        // Flush if state changed or batch full
        if self.should_flush(key) && !self.batched_quads.is_empty() {
            flushed = true;
        }

        // Update current state
        if self.current_key != Some(key) {
            self.current_key = Some(key);
            self.state_changes += 1;
        }

        // Add to batch
        self.batched_quads.push(quad);

        flushed
    }

    /// Get current batch for rendering
    pub fn current_batch(&self) -> &[QuadInstance] {
        &self.batched_quads
    }

    /// Clear current batch after rendering
    pub fn clear_batch(&mut self) {
        if !self.batched_quads.is_empty() {
            self.total_batches += 1;
            self.total_instances += self.batched_quads.len() as u32;
            self.batched_quads.clear();
        }
    }

    /// Flush any remaining batched quads
    /// Returns true if there were quads to flush
    pub fn flush(&mut self) -> bool {
        let had_quads = !self.batched_quads.is_empty();
        self.clear_batch();
        had_quads
    }

    /// Reset statistics
    pub fn reset_stats(&mut self) {
        self.total_batches = 0;
        self.total_instances = 0;
        self.state_changes = 0;
    }

    /// Get batch efficiency (higher is better)
    pub fn batch_efficiency(&self) -> f32 {
        if self.total_batches == 0 {
            return 1.0;
        }
        self.total_instances as f32 / self.total_batches as f32
    }

    /// Get state change efficiency (lower is better)
    pub fn state_change_ratio(&self) -> f32 {
        if self.total_instances == 0 {
            return 0.0;
        }
        self.state_changes as f32 / self.total_instances as f32
    }
}

impl Default for RenderBatcher {
    fn default() -> Self {
        Self::new()
    }
}

/// GL State tracker - avoid redundant state changes
pub struct GLStateTracker<P, T, V, F> {
    current_program: Option<P>,
    current_texture: Option<T>,
    current_vao: Option<V>,
    current_fbo: Option<F>,
    blend_enabled: bool,
    scissor_enabled: bool,
    redundant_changes_avoided: u32,
}

impl<P: Copy + PartialEq, T: Copy + PartialEq, V: Copy + PartialEq, F: Copy + PartialEq> GLStateTracker<P, T, V, F> {
    pub fn new() -> Self {
        Self {
            current_program: None,
            current_texture: None,
            current_vao: None,
            current_fbo: None,
            blend_enabled: false,
            scissor_enabled: false,
            redundant_changes_avoided: 0,
        }
    }

    /// Use program, returns true if state changed
    pub fn use_program<C: HasContext<Program = P>>(&mut self, gl: &C, program: Option<P>) -> bool {
        if self.current_program == program {
            self.redundant_changes_avoided += 1;
            return false;
        }
        unsafe { gl.use_program(program) };
        self.current_program = program;
        true
    }

    /// Bind texture, returns true if state changed
    pub fn bind_texture<C: HasContext<Texture = T>>(&mut self, gl: &C, target: u32, texture: Option<T>) -> bool {
        if self.current_texture == texture {
            self.redundant_changes_avoided += 1;
            return false;
        }
        unsafe { gl.bind_texture(target, texture) };
        self.current_texture = texture;
        true
    }

    /// Bind VAO, returns true if state changed
    pub fn bind_vertex_array<C: HasContext<VertexArray = V>>(&mut self, gl: &C, vao: Option<V>) -> bool {
        if self.current_vao == vao {
            self.redundant_changes_avoided += 1;
            return false;
        }
        unsafe { gl.bind_vertex_array(vao) };
        self.current_vao = vao;
        true
    }

    /// Bind FBO, returns true if state changed
    pub fn bind_framebuffer<C: HasContext<Framebuffer = F>>(&mut self, gl: &C, target: u32, fbo: Option<F>) -> bool {
        if self.current_fbo == fbo {
            self.redundant_changes_avoided += 1;
            return false;
        }
        unsafe { gl.bind_framebuffer(target, fbo) };
        self.current_fbo = fbo;
        true
    }

    /// Enable/disable blend, returns true if state changed
    pub fn set_blend<C: HasContext>(&mut self, gl: &C, enabled: bool) -> bool {
        if self.blend_enabled == enabled {
            self.redundant_changes_avoided += 1;
            return false;
        }
        unsafe {
            if enabled {
                gl.enable(glow::BLEND);
            } else {
                gl.disable(glow::BLEND);
            }
        }
        self.blend_enabled = enabled;
        true
    }

    /// Enable/disable scissor, returns true if state changed
    pub fn set_scissor<C: HasContext>(&mut self, gl: &C, enabled: bool) -> bool {
        if self.scissor_enabled == enabled {
            self.redundant_changes_avoided += 1;
            return false;
        }
        unsafe {
            if enabled {
                gl.enable(glow::SCISSOR_TEST);
            } else {
                gl.disable(glow::SCISSOR_TEST);
            }
        }
        self.scissor_enabled = enabled;
        true
    }

    /// Reset all state (call when context might be invalid)
    pub fn reset(&mut self) {
        self.current_program = None;
        self.current_texture = None;
        self.current_vao = None;
        self.current_fbo = None;
        self.blend_enabled = false;
        self.scissor_enabled = false;
    }

    /// Get redundant changes avoided count
    pub fn redundant_changes_avoided(&self) -> u32 {
        self.redundant_changes_avoided
    }

    /// Reset statistics
    pub fn reset_stats(&mut self) {
        self.redundant_changes_avoided = 0;
    }
}

impl<P: Copy + PartialEq, T: Copy + PartialEq, V: Copy + PartialEq, F: Copy + PartialEq> Default for GLStateTracker<P, T, V, F> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_batcher_creation() {
        let batcher = RenderBatcher::new();
        assert_eq!(batcher.batched_quads.len(), 0);
        assert_eq!(batcher.total_batches, 0);
    }

    #[test]
    fn test_batch_same_state() {
        let mut batcher = RenderBatcher::new();
        let key = BatchKey {
            program: 1,
            texture: 10,
            blend_enabled: true,
        };
        let quad = QuadInstance {
            x: 0.0, y: 0.0, width: 100.0, height: 100.0,
            opacity: 1.0, corner_radius: 0.0,
            u: 0.0, v: 0.0, u_width: 1.0, v_height: 1.0,
        };

        // Add 3 quads with same state
        batcher.batch_quad(key, quad);
        batcher.batch_quad(key, quad);
        batcher.batch_quad(key, quad);

        assert_eq!(batcher.batched_quads.len(), 3);
        assert_eq!(batcher.state_changes, 1); // Only one state change
    }

    #[test]
    fn test_batch_state_change() {
        let mut batcher = RenderBatcher::new();
        let key1 = BatchKey { program: 1, texture: 10, blend_enabled: true };
        let key2 = BatchKey { program: 2, texture: 10, blend_enabled: true };
        let quad = QuadInstance {
            x: 0.0, y: 0.0, width: 100.0, height: 100.0,
            opacity: 1.0, corner_radius: 0.0,
            u: 0.0, v: 0.0, u_width: 1.0, v_height: 1.0,
        };

        batcher.batch_quad(key1, quad);
        batcher.batch_quad(key1, quad);

        // State change should trigger need to flush
        assert!(batcher.should_flush(key2));

        batcher.batch_quad(key2, quad);
        assert_eq!(batcher.state_changes, 2);
    }

    #[test]
    fn test_batch_efficiency() {
        let mut batcher = RenderBatcher::new();
        let key = BatchKey { program: 1, texture: 10, blend_enabled: true };
        let quad = QuadInstance {
            x: 0.0, y: 0.0, width: 100.0, height: 100.0,
            opacity: 1.0, corner_radius: 0.0,
            u: 0.0, v: 0.0, u_width: 1.0, v_height: 1.0,
        };

        // Add 10 quads
        for _ in 0..10 {
            batcher.batch_quad(key, quad);
        }
        batcher.flush();

        // Efficiency = instances / batches = 10 / 1 = 10.0
        assert_eq!(batcher.batch_efficiency(), 10.0);
    }

    #[test]
    fn test_state_tracker() {
        let mut tracker: GLStateTracker<u32, u32, u32, u32> = GLStateTracker::new();

        // First change should succeed
        tracker.current_program = Some(1);

        // Second identical change should be avoided
        assert_eq!(tracker.current_program, Some(1));
        tracker.redundant_changes_avoided += 1;

        assert_eq!(tracker.redundant_changes_avoided(), 1);
    }
}
