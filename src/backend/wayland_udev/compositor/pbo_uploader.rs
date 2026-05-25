use smithay::backend::renderer::gles::ffi;
use std::collections::VecDeque;
use std::time::Instant;

struct StreamingPBO {
    buffer: u32,
    capacity: usize,
    fence: Option<ffi::types::GLsync>,
    last_use: Instant,
}

pub(crate) struct PBOUploader {
    pool: VecDeque<StreamingPBO>,
    pbo_size: usize,
    max_pool_size: usize,
    uploads_count: u64,
    fallback_count: u64,
}

impl PBOUploader {
    pub(crate) fn new(pbo_size: usize, max_pool_size: usize) -> Self {
        Self {
            pool: VecDeque::new(),
            pbo_size,
            max_pool_size,
            uploads_count: 0,
            fallback_count: 0,
        }
    }

    /// Upload texture data asynchronously via PBO.
    /// Returns true on success (PBO path used), false if fell back to direct upload.
    /// Falls back to direct TexSubImage2D if data exceeds pbo_size.
    pub(crate) unsafe fn upload_texture(
        &mut self,
        gl: &ffi::Gles2,
        texture: u32,
        width: u32,
        height: u32,
        format: u32,
        data: &[u8],
    ) -> bool {
        if data.len() > self.pbo_size {
            // Data too large for PBO, fall back to direct upload
            gl.BindTexture(ffi::TEXTURE_2D, texture);
            gl.TexSubImage2D(
                ffi::TEXTURE_2D,
                0,
                0,
                0,
                width as i32,
                height as i32,
                format,
                ffi::UNSIGNED_BYTE,
                data.as_ptr() as *const _,
            );
            self.fallback_count += 1;
            return false;
        }

        // Acquire a PBO from the pool or create a new one
        let pbo = self.acquire_pbo(gl);

        // Bind PBO and upload data
        gl.BindBuffer(ffi::PIXEL_UNPACK_BUFFER, pbo.buffer);
        gl.BufferData(
            ffi::PIXEL_UNPACK_BUFFER,
            self.pbo_size as isize,
            std::ptr::null(),
            ffi::STREAM_DRAW,
        );

        // Map buffer and copy data
        let ptr = gl.MapBufferRange(
            ffi::PIXEL_UNPACK_BUFFER,
            0,
            data.len() as isize,
            ffi::MAP_WRITE_BIT | ffi::MAP_INVALIDATE_BUFFER_BIT,
        );

        if ptr.is_null() {
            // Map failed, fall back to direct upload
            gl.BindBuffer(ffi::PIXEL_UNPACK_BUFFER, 0);
            gl.BindTexture(ffi::TEXTURE_2D, texture);
            gl.TexSubImage2D(
                ffi::TEXTURE_2D,
                0,
                0,
                0,
                width as i32,
                height as i32,
                format,
                ffi::UNSIGNED_BYTE,
                data.as_ptr() as *const _,
            );
            // Return PBO to pool
            self.pool.push_back(pbo);
            self.fallback_count += 1;
            return false;
        }

        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr as *mut u8, data.len());
        gl.UnmapBuffer(ffi::PIXEL_UNPACK_BUFFER);

        // Upload from PBO to texture (offset 0 in the bound PBO)
        gl.BindTexture(ffi::TEXTURE_2D, texture);
        gl.TexSubImage2D(
            ffi::TEXTURE_2D,
            0,
            0,
            0,
            width as i32,
            height as i32,
            format,
            ffi::UNSIGNED_BYTE,
            std::ptr::null(),
        );

        // Unbind PBO
        gl.BindBuffer(ffi::PIXEL_UNPACK_BUFFER, 0);

        // Register fence for this PBO
        let fence = gl.FenceSync(ffi::SYNC_GPU_COMMANDS_COMPLETE, 0);
        let mut pbo = pbo;
        pbo.fence = if fence.is_null() { None } else { Some(fence) };
        pbo.last_use = Instant::now();

        // Return PBO to pool
        self.pool.push_back(pbo);
        self.uploads_count += 1;

        true
    }

    /// Try to reclaim PBOs whose fences have signaled.
    pub(crate) unsafe fn try_reclaim(&mut self, gl: &ffi::Gles2) {
        for pbo in self.pool.iter_mut() {
            if let Some(fence) = pbo.fence {
                let result = gl.ClientWaitSync(fence, 0, 0);
                if result == ffi::ALREADY_SIGNALED || result == ffi::CONDITION_SATISFIED {
                    gl.DeleteSync(fence);
                    pbo.fence = None;
                }
            }
        }
    }

    /// Returns (uploads_count, fallback_count, pool_size).
    pub(crate) fn stats(&self) -> (u64, u64, usize) {
        (self.uploads_count, self.fallback_count, self.pool.len())
    }

    /// Delete all PBO buffers and fences.
    pub(crate) unsafe fn clear(&mut self, gl: &ffi::Gles2) {
        while let Some(pbo) = self.pool.pop_front() {
            if let Some(fence) = pbo.fence {
                gl.DeleteSync(fence);
            }
            gl.DeleteBuffers(1, &pbo.buffer);
        }
        self.uploads_count = 0;
        self.fallback_count = 0;
    }

    /// Acquire a PBO from the pool (one without a pending fence) or create a new one.
    unsafe fn acquire_pbo(&mut self, gl: &ffi::Gles2) -> StreamingPBO {
        // Try to find a PBO without a pending fence
        let mut found_idx = None;
        for (i, pbo) in self.pool.iter().enumerate() {
            if pbo.fence.is_none() {
                found_idx = Some(i);
                break;
            }
        }

        if let Some(idx) = found_idx {
            return self.pool.remove(idx).unwrap();
        }

        // If pool is at capacity, wait on the oldest PBO's fence
        if self.pool.len() >= self.max_pool_size {
            let mut pbo = self.pool.pop_front().unwrap();
            if let Some(fence) = pbo.fence.take() {
                // Blocking wait with 16ms timeout
                gl.ClientWaitSync(fence, ffi::SYNC_FLUSH_COMMANDS_BIT, 16_000_000);
                gl.DeleteSync(fence);
            }
            return pbo;
        }

        // Create a new PBO
        let mut buffer = 0u32;
        gl.GenBuffers(1, &mut buffer);
        StreamingPBO {
            buffer,
            capacity: self.pbo_size,
            fence: None,
            last_use: Instant::now(),
        }
    }
}
