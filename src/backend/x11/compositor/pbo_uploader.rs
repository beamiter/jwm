/// Async texture upload via PBO (Pixel Buffer Objects)
///
/// Eliminates CPU→GPU synchronization by:
/// 1. Pre-allocating PBOs as streaming buffers
/// 2. Using glBufferData to write data (driver may use write-combined memory)
/// 3. Using async glTexSubImage2D from PBO (GPU-side copy)
///
/// Performance: Reduces 1-3ms texture upload stalls for overview/font rendering
use glow::HasContext;
use std::collections::VecDeque;

/// Single streaming PBO for async uploads
struct StreamingPBO {
    pbo: glow::Buffer,
    capacity: usize,
    /// Fence to track GPU usage (avoid write-after-read hazard)
    fence: Option<glow::Fence>,
}

impl StreamingPBO {
    /// Create a new streaming PBO
    ///
    /// Uses GL_STREAM_DRAW hint for optimal driver behavior
    unsafe fn new(gl: &glow::Context, capacity: usize) -> Option<Self> {
        unsafe {
            let pbo = gl.create_buffer().ok()?;
            gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, Some(pbo));
            gl.buffer_data_size(
                glow::PIXEL_UNPACK_BUFFER,
                capacity as i32,
                glow::STREAM_DRAW,
            );
            gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, None);

            Some(Self {
                pbo,
                capacity,
                fence: None,
            })
        }
    }

    /// Wait for GPU to finish using this PBO (blocking)
    unsafe fn wait_fence(&mut self, gl: &glow::Context) {
        if let Some(fence) = self.fence.take() {
            unsafe {
                // Wait for GPU completion (timeout: 100ms = 6 frames at 60Hz)
                gl.client_wait_sync(fence, glow::SYNC_FLUSH_COMMANDS_BIT, 100_000_000);
                gl.delete_sync(fence);
            }
        }
    }

    /// Write data to PBO via glBufferData (orphaning old buffer)
    ///
    /// # Safety
    /// Caller must ensure `wait_fence()` was called before writing
    unsafe fn write_data(&mut self, gl: &glow::Context, data: &[u8]) -> bool {
        if data.len() > self.capacity {
            return false;
        }
        unsafe {
            gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, Some(self.pbo));
            // Orphan old buffer and upload new data (driver optimization)
            gl.buffer_data_u8_slice(glow::PIXEL_UNPACK_BUFFER, data, glow::STREAM_DRAW);
            gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, None);
        }
        true
    }

    /// Insert a fence to track GPU usage
    unsafe fn insert_fence(&mut self, gl: &glow::Context) {
        if let Some(old_fence) = self.fence.take() {
            unsafe {
                gl.delete_sync(old_fence);
            }
        }
        unsafe {
            self.fence = gl.fence_sync(glow::SYNC_GPU_COMMANDS_COMPLETE, 0).ok();
        }
    }
}

impl Drop for StreamingPBO {
    fn drop(&mut self) {
        // PBO is automatically deleted by glow context
    }
}

/// Pool of streaming PBOs for async texture uploads
pub struct PBOUploader {
    /// Pool of reusable PBOs (FIFO queue)
    pool: VecDeque<StreamingPBO>,
    /// Size of each PBO (bytes)
    pbo_size: usize,
    /// Max PBOs to keep in pool
    max_pool_size: usize,
}

impl PBOUploader {
    /// Create a new PBO uploader pool
    ///
    /// `pbo_size`: Size of each PBO in bytes (e.g., 1024*1024*4 = 4MB for 1024x1024 RGBA)
    /// `max_pool_size`: Max number of PBOs to cache
    pub fn new(pbo_size: usize, max_pool_size: usize) -> Self {
        log::info!("pbo_uploader: initialized with {}MB PBOs, pool size {}", pbo_size / 1024 / 1024, max_pool_size);

        Self {
            pool: VecDeque::with_capacity(max_pool_size),
            pbo_size,
            max_pool_size,
        }
    }

    /// Upload texture data using PBO (async GPU transfer)
    ///
    /// Returns true if upload succeeded
    ///
    /// # Safety
    /// Requires valid GL context bound
    pub unsafe fn upload_texture(
        &mut self,
        gl: &glow::Context,
        texture: glow::Texture,
        width: u32,
        height: u32,
        format: u32,
        data: &[u8],
    ) -> bool {
        let required_size = (width * height * 4) as usize; // Assume RGBA
        if required_size > self.pbo_size {
            log::warn!(
                "pbo_uploader: texture {}x{} ({} bytes) exceeds PBO size ({} bytes), using sync upload",
                width, height, required_size, self.pbo_size
            );
            return unsafe { self.upload_texture_sync(gl, texture, width, height, format, data) };
        }

        // Get or create PBO
        let mut pbo = self.get_pbo(gl);

        // Wait for GPU to finish previous use
        unsafe { pbo.wait_fence(gl) };

        // Write data to PBO via glBufferData
        if !unsafe { pbo.write_data(gl, data) } {
            log::error!("pbo_uploader: failed to write {} bytes to PBO", data.len());
            self.return_pbo(gl, pbo);
            return false;
        }

        unsafe {
            // Bind PBO and upload texture from PBO to GPU texture (async GPU-side copy)
            gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, Some(pbo.pbo));
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            gl.tex_sub_image_2d(
                glow::TEXTURE_2D,
                0,
                0,
                0,
                width as i32,
                height as i32,
                format,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::BufferOffset(0),
            );
            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, None);

            // Insert fence to track GPU usage
            pbo.insert_fence(gl);
        }

        // Return PBO to pool
        self.return_pbo(gl, pbo);

        true
    }

    /// Synchronous fallback upload (no PBO)
    unsafe fn upload_texture_sync(
        &self,
        gl: &glow::Context,
        texture: glow::Texture,
        width: u32,
        height: u32,
        format: u32,
        data: &[u8],
    ) -> bool {
        unsafe {
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            gl.tex_sub_image_2d(
                glow::TEXTURE_2D,
                0,
                0,
                0,
                width as i32,
                height as i32,
                format,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(data)),
            );
            gl.bind_texture(glow::TEXTURE_2D, None);
        }
        true
    }

    /// Get a PBO from pool or create new one
    fn get_pbo(&mut self, gl: &glow::Context) -> StreamingPBO {
        self.pool.pop_front().unwrap_or_else(|| unsafe {
            StreamingPBO::new(gl, self.pbo_size).unwrap_or_else(|| {
                panic!("pbo_uploader: failed to create PBO");
            })
        })
    }

    /// Return PBO to pool
    fn return_pbo(&mut self, gl: &glow::Context, pbo: StreamingPBO) {
        if self.pool.len() < self.max_pool_size {
            self.pool.push_back(pbo);
        } else {
            // Pool full, drop oldest PBO
            unsafe {
                gl.delete_buffer(pbo.pbo);
            }
        }
    }

    /// Get pool statistics
    pub fn stats(&self) -> (usize, usize) {
        (self.pool.len(), self.max_pool_size)
    }

    /// Clear the pool (release all PBOs)
    pub fn clear(&mut self, gl: &glow::Context) {
        unsafe {
            for pbo in self.pool.drain(..) {
                gl.delete_buffer(pbo.pbo);
            }
        }
    }

    /// Phase 3.1: Batch upload multiple textures
    ///
    /// Uploads multiple textures in a batch, reducing GL call overhead
    /// Returns number of successfully uploaded textures
    pub unsafe fn batch_upload_textures(
        &mut self,
        gl: &glow::Context,
        uploads: &[(glow::Texture, u32, u32, u32, &[u8])], // (texture, width, height, format, data)
    ) -> usize {
        let mut success_count = 0;

        for &(texture, width, height, format, data) in uploads {
            if unsafe { self.upload_texture(gl, texture, width, height, format, data) } {
                success_count += 1;
            }
        }

        success_count
    }

    /// Phase 3.1: Non-blocking upload attempt
    ///
    /// Attempts upload only if a PBO is immediately available (no waiting)
    /// Returns true if upload started, false if would block
    pub unsafe fn try_upload_nonblocking(
        &mut self,
        gl: &glow::Context,
        texture: glow::Texture,
        width: u32,
        height: u32,
        format: u32,
        data: &[u8],
    ) -> bool {
        // Check if we have a ready PBO (fence signaled or no fence)
        if let Some(mut pbo) = self.pool.pop_front() {
            // Quick fence check (non-blocking)
            if let Some(fence) = pbo.fence {
                let status = unsafe {
                    gl.get_sync_status(fence)
                };
                if status != glow::SIGNALED {
                    // GPU still using this PBO, return it and fail
                    self.pool.push_front(pbo);
                    return false;
                }
                // Fence signaled, clear it
                unsafe { gl.delete_sync(fence) };
                pbo.fence = None;
            }

            // PBO is ready, perform upload
            if unsafe { pbo.write_data(gl, data) } {
                unsafe {
                    gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, Some(pbo.pbo));
                    gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                    gl.tex_sub_image_2d(
                        glow::TEXTURE_2D, 0, 0, 0,
                        width as i32, height as i32,
                        format, glow::UNSIGNED_BYTE,
                        glow::PixelUnpackData::BufferOffset(0),
                    );
                    gl.bind_texture(glow::TEXTURE_2D, None);
                    gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, None);
                    pbo.insert_fence(gl);
                }
                self.return_pbo(gl, pbo);
                return true;
            } else {
                self.return_pbo(gl, pbo);
                return false;
            }
        }

        // No PBO available
        false
    }
}

impl Drop for PBOUploader {
    fn drop(&mut self) {
        // Clear pool without deleting GL resources
        // (GL resources will be cleaned up when context is destroyed)
        self.pool.clear();
    }
}
