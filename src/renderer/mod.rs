/// Rendering abstraction layer for multi-backend support (OpenGL, Vulkan, etc.)
///
/// This module provides a trait-based abstraction over GPU rendering APIs,
/// allowing the compositor to work with different backends (GLX, Vulkan).
pub mod types;
pub use types::*;

/// Main renderer trait that abstracts over different GPU APIs
pub trait Renderer: Send {
    // === Lifecycle ===

    /// Begin a new frame
    fn begin_frame(&mut self, screen_w: u32, screen_h: u32);

    /// End the current frame (but don't present yet)
    fn end_frame(&mut self);

    /// Present the rendered frame to the screen
    fn swap_buffers(&mut self);

    // === State Management ===

    /// Set the rendering viewport
    fn set_viewport(&mut self, x: i32, y: i32, w: u32, h: u32);

    /// Set scissor test region (None = disable scissor test)
    fn set_scissor(&mut self, rect: Option<Rect>);

    /// Clear the current framebuffer
    fn clear(&mut self, color: Color);

    /// Set blend mode (premultiplied alpha is standard for compositing)
    fn set_blend_mode(&mut self, premultiplied_alpha: bool);

    // === Texture Management ===

    /// Create a texture (optionally with initial data)
    fn create_texture(&mut self, w: u32, h: u32, data: Option<&[u8]>) -> TextureId;

    /// Delete a texture
    fn delete_texture(&mut self, tex: TextureId);

    /// Bind a texture to a slot
    fn bind_texture(&mut self, slot: u32, tex: TextureId);

    /// Update a region of a texture
    fn update_texture_region(
        &mut self,
        tex: TextureId,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        data: &[u8],
    );

    // === Framebuffer Objects (FBOs) ===

    /// Create an FBO with attached color texture
    /// Returns (fbo_id, texture_id)
    fn create_fbo(&mut self, w: u32, h: u32) -> (FboId, TextureId);

    /// Delete an FBO
    fn delete_fbo(&mut self, fbo: FboId);

    /// Bind an FBO (None = bind default framebuffer)
    fn bind_fbo(&mut self, fbo: Option<FboId>);

    /// Blit from one FBO to another
    fn blit_fbo(&mut self, src: FboId, dst: Option<FboId>, src_rect: Rect, dst_rect: Rect);

    // === Drawing ===

    /// Draw a textured quad with effects
    fn draw_textured_quad(&mut self, params: &DrawParams);

    /// Draw a shadow quad
    fn draw_shadow(&mut self, params: &ShadowParams);

    /// Run blur passes and return the blurred texture
    fn draw_blur_pass(&mut self, params: &BlurPassParams) -> TextureId;

    /// Draw particles (point sprites)
    fn draw_points(&mut self, data: &[f32], point_size: f32, projection: &[f32; 16]);

    // === Shaders ===

    /// Load and compile a shader program
    /// Returns shader ID for later use
    fn load_shader(&mut self, name: &str, vert: &str, frag: &str) -> ShaderId;

    /// Delete a shader program
    fn delete_shader(&mut self, id: ShaderId);

    // === Synchronization ===

    /// Insert a GPU fence for async synchronization
    /// Returns fence ID (None if fences not supported)
    fn insert_fence(&mut self) -> Option<u64>;

    /// Wait for a fence to complete
    fn wait_fence(&mut self, fence: u64, timeout_ns: u64) -> FenceStatus;

    /// Delete a fence
    fn delete_fence(&mut self, fence: u64);

    // === Queries ===

    /// Check if HDR output is supported
    fn supports_hdr(&self) -> bool {
        false
    }

    /// Get maximum texture size supported
    fn max_texture_size(&self) -> u32;

    /// Get renderer name/description
    fn renderer_name(&self) -> &str;
}
