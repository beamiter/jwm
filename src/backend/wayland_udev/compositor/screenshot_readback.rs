use crate::backend::compositor_common::capture::flip_rgba_vertical;
use crate::backend::compositor_common::screenshot::save_png_async;
use smithay::backend::renderer::gles::ffi;
use std::collections::VecDeque;
use std::path::PathBuf;

struct PendingReadback {
    pbo: u32,
    fence: ffi::types::GLsync,
    path: PathBuf,
    width: u32,
    height: u32,
}

/// One-shot screenshot readback which does not block the submitting frame.
pub(crate) struct ScreenshotReadback {
    pending: VecDeque<PendingReadback>,
}

impl ScreenshotReadback {
    pub(crate) fn new() -> Self {
        Self {
            pending: VecDeque::new(),
        }
    }

    pub(crate) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub(crate) unsafe fn enqueue(
        &mut self,
        gl: &ffi::Gles2,
        path: PathBuf,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    ) {
        if width == 0 || height == 0 {
            return;
        }
        let size = (width as usize)
            .saturating_mul(height as usize)
            .saturating_mul(4);
        if size > isize::MAX as usize {
            log::warn!("[compositor] screenshot is too large for PBO readback");
            return;
        }
        unsafe {
            let mut pbo = 0;
            gl.GenBuffers(1, &mut pbo);
            gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, pbo);
            gl.BufferData(
                ffi::PIXEL_PACK_BUFFER,
                size as isize,
                std::ptr::null(),
                ffi::STREAM_READ,
            );
            gl.ReadPixels(
                x,
                y,
                width as i32,
                height as i32,
                ffi::RGBA,
                ffi::UNSIGNED_BYTE,
                std::ptr::null_mut(),
            );
            gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, 0);
            let fence = gl.FenceSync(ffi::SYNC_GPU_COMMANDS_COMPLETE, 0);
            if fence.is_null() {
                log::warn!(
                    "[compositor] screenshot fence unavailable; dropping asynchronous capture"
                );
                gl.DeleteBuffers(1, &pbo);
                return;
            }
            self.pending.push_back(PendingReadback {
                pbo,
                fence,
                path,
                width,
                height,
            });
        }
    }

    /// Complete ready jobs only; a zero timeout means this never waits for GPU work.
    pub(crate) unsafe fn drain_ready(&mut self, gl: &ffi::Gles2) {
        while let Some(front) = self.pending.front() {
            let state = unsafe { gl.ClientWaitSync(front.fence, 0, 0) };
            if state != ffi::ALREADY_SIGNALED && state != ffi::CONDITION_SATISFIED {
                break;
            }
            let job = self.pending.pop_front().expect("front was checked");
            let size = (job.width as usize) * (job.height as usize) * 4;
            unsafe {
                gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, job.pbo);
                let ptr =
                    gl.MapBufferRange(ffi::PIXEL_PACK_BUFFER, 0, size as isize, ffi::MAP_READ_BIT);
                if ptr.is_null() {
                    log::warn!("[compositor] could not map completed screenshot PBO");
                } else {
                    let mut pixels = std::slice::from_raw_parts(ptr as *const u8, size).to_vec();
                    gl.UnmapBuffer(ffi::PIXEL_PACK_BUFFER);
                    flip_rgba_vertical(&mut pixels, job.width, job.height);
                    save_png_async(
                        job.path,
                        pixels,
                        job.width,
                        job.height,
                        crate::backend::error::BackendErrorContext::new(
                            "wayland-udev",
                            crate::backend::error::ErrorBoundary::Renderer,
                            "screenshot: save PNG",
                        ),
                    );
                }
                gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, 0);
                gl.DeleteSync(job.fence);
                gl.DeleteBuffers(1, &job.pbo);
            }
        }
    }
}

impl Default for ScreenshotReadback {
    fn default() -> Self {
        Self::new()
    }
}
