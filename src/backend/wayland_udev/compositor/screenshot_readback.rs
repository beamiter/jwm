use crate::backend::compositor_common::capture::flip_rgba_vertical;
use crate::backend::compositor_common::screenshot::{ScreenshotPermit, save_png_async};
use smithay::backend::renderer::gles::ffi;
use std::collections::VecDeque;
use std::path::PathBuf;

struct PendingReadback {
    pbo: u32,
    fence: ffi::types::GLsync,
    path: PathBuf,
    width: u32,
    height: u32,
    permit: ScreenshotPermit,
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
        permit: ScreenshotPermit,
    ) {
        let size = match crate::backend::compositor_common::capture::rgba_capture_len(width, height)
        {
            Ok(size) => size,
            Err(error) => {
                log::warn!("[compositor] screenshot readback refused: {error}");
                return;
            }
        };
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
                permit,
            });
        }
    }

    /// Complete ready jobs only; a zero timeout means this never waits for GPU work.
    pub(crate) unsafe fn drain_ready(&mut self, gl: &ffi::Gles2) {
        unsafe {
            self.drain_ready_with(gl, |path, pixels, width, height, permit| {
                save_png_async(
                    path,
                    pixels,
                    width,
                    height,
                    crate::backend::error::BackendErrorContext::new(
                        "wayland-udev",
                        crate::backend::error::ErrorBoundary::Renderer,
                        "screenshot: save PNG",
                    ),
                    permit,
                );
            });
        }
    }

    unsafe fn drain_ready_with(
        &mut self,
        gl: &ffi::Gles2,
        mut write: impl FnMut(PathBuf, Vec<u8>, u32, u32, ScreenshotPermit),
    ) {
        while let Some(front) = self.pending.front() {
            let state = unsafe { gl.ClientWaitSync(front.fence, 0, 0) };
            if state == ffi::TIMEOUT_EXPIRED {
                break;
            }
            let job = self.pending.pop_front().expect("front was checked");
            if state != ffi::ALREADY_SIGNALED && state != ffi::CONDITION_SATISFIED {
                // WAIT_FAILED is terminal. Retaining it would block every
                // later screenshot and keep the render loop awake forever.
                log::warn!("[compositor] screenshot fence wait failed: {state:#x}");
                unsafe {
                    gl.DeleteSync(job.fence);
                    gl.DeleteBuffers(1, &job.pbo);
                }
                continue;
            }
            let mut pixels = match crate::backend::compositor_common::capture::allocate_rgba_capture(
                job.width, job.height,
            ) {
                Ok(pixels) => pixels,
                Err(error) => {
                    log::warn!("[compositor] screenshot CPU allocation refused: {error}");
                    unsafe {
                        gl.DeleteSync(job.fence);
                        gl.DeleteBuffers(1, &job.pbo);
                    }
                    continue;
                }
            };
            let size = pixels.len();
            unsafe {
                gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, job.pbo);
                let ptr =
                    gl.MapBufferRange(ffi::PIXEL_PACK_BUFFER, 0, size as isize, ffi::MAP_READ_BIT);
                if ptr.is_null() {
                    log::warn!("[compositor] could not map completed screenshot PBO");
                } else {
                    pixels.copy_from_slice(std::slice::from_raw_parts(ptr as *const u8, size));
                    gl.UnmapBuffer(ffi::PIXEL_PACK_BUFFER);
                    flip_rgba_vertical(&mut pixels, job.width, job.height);
                    write(job.path, pixels, job.width, job.height, job.permit);
                }
                gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, 0);
                gl.DeleteSync(job.fence);
                gl.DeleteBuffers(1, &job.pbo);
            }
        }
    }

    /// Cancel all outstanding readbacks and release their raw GLES objects.
    /// Pending PNG jobs have not read any pixels yet, so a compositor disable
    /// deliberately cancels them rather than waiting on their fences.
    pub(crate) unsafe fn clear(&mut self, gl: &ffi::Gles2) {
        unsafe {
            gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, 0);
            while let Some(job) = self.pending.pop_front() {
                gl.DeleteSync(job.fence);
                if job.pbo != 0 {
                    gl.DeleteBuffers(1, &job.pbo);
                }
            }
        }
    }
}

impl Default for ScreenshotReadback {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::compositor_common::screenshot::{
        ScreenshotBusy, ScreenshotQueue, ScreenshotRequest,
    };
    use std::cell::RefCell;
    use std::ffi::c_void;

    thread_local! {
        static RELEASED: RefCell<(Vec<usize>, Vec<u32>)> = const { RefCell::new((Vec::new(), Vec::new())) };
    }

    unsafe extern "system" fn wait(fence: ffi::types::GLsync, _: u32, _: u64) -> u32 {
        match fence as usize {
            1 => ffi::WAIT_FAILED,
            2 => ffi::CONDITION_SATISFIED,
            _ => ffi::TIMEOUT_EXPIRED,
        }
    }
    unsafe extern "system" fn bind(_: u32, _: u32) {}
    unsafe extern "system" fn map(_: u32, _: isize, _: isize, _: u32) -> *mut c_void {
        // Exercise completion cleanup without starting any PNG writer.
        std::ptr::null_mut()
    }
    unsafe extern "system" fn delete_sync(fence: ffi::types::GLsync) {
        RELEASED.with(|released| released.borrow_mut().0.push(fence as usize));
    }
    unsafe extern "system" fn delete_buffers(count: i32, buffers: *const u32) {
        // SAFETY: the production path passes one live local u32 for this call.
        let names = unsafe { std::slice::from_raw_parts(buffers, count as usize) };
        RELEASED.with(|released| released.borrow_mut().1.extend_from_slice(names));
    }
    unsafe extern "system" fn generate(_: i32, buffer: *mut u32) {
        unsafe {
            *buffer = 7;
        }
    }
    unsafe extern "system" fn buffer_data(_: u32, _: isize, _: *const c_void, _: u32) {}
    unsafe extern "system" fn read_pixels(
        _: i32,
        _: i32,
        _: i32,
        _: i32,
        _: u32,
        _: u32,
        _: *mut c_void,
    ) {
    }
    unsafe extern "system" fn no_fence(_: u32, _: u32) -> ffi::types::GLsync {
        std::ptr::null()
    }
    unsafe extern "system" fn map_pixels(_: u32, _: isize, _: isize, _: u32) -> *mut c_void {
        static PIXELS: [u8; 4] = [1, 2, 3, 4];
        PIXELS.as_ptr() as *mut c_void
    }
    unsafe extern "system" fn unmap(_: u32) -> u8 {
        1
    }
    fn fake_gl() -> ffi::Gles2 {
        fake_gl_with_pixels(false)
    }
    fn fake_gl_with_pixels(mapped: bool) -> ffi::Gles2 {
        ffi::Gles2::load_with(|symbol| match symbol {
            "glClientWaitSync" => wait as *const c_void,
            "glBindBuffer" => bind as *const c_void,
            "glMapBufferRange" if mapped => map_pixels as *const c_void,
            "glMapBufferRange" => map as *const c_void,
            "glUnmapBuffer" => unmap as *const c_void,
            "glGenBuffers" => generate as *const c_void,
            "glBufferData" => buffer_data as *const c_void,
            "glReadPixels" => read_pixels as *const c_void,
            "glFenceSync" => no_fence as *const c_void,
            "glDeleteSync" => delete_sync as *const c_void,
            "glDeleteBuffers" => delete_buffers as *const c_void,
            _ => std::ptr::null(),
        })
    }
    fn permit(queue: &mut ScreenshotQueue) -> ScreenshotPermit {
        queue
            .request_full("unused-synthetic-test.png".into())
            .unwrap();
        let ScreenshotRequest::Full { permit, .. } = queue.take_all().pop_front().unwrap() else {
            unreachable!()
        };
        permit
    }
    fn job(id: usize, queue: &mut ScreenshotQueue) -> PendingReadback {
        PendingReadback {
            pbo: id as u32,
            fence: id as ffi::types::GLsync,
            path: PathBuf::from("unused-synthetic-test.png"),
            width: 1,
            height: 1,
            permit: permit(queue),
        }
    }

    #[test]
    fn failed_screenshot_fence_is_retired_before_later_ready_jobs() {
        RELEASED.with(|released| *released.borrow_mut() = (Vec::new(), Vec::new()));
        let gl = fake_gl();
        let mut readback = ScreenshotReadback::new();
        let mut queue = ScreenshotQueue::isolated_for_test();
        readback
            .pending
            .extend([job(1, &mut queue), job(2, &mut queue)]);
        assert_eq!(queue.in_flight_for_test(), 2);
        unsafe { readback.drain_ready(&gl) };
        assert!(!readback.has_pending());
        assert_eq!(queue.in_flight_for_test(), 0);
        RELEASED.with(|released| assert_eq!(*released.borrow(), (vec![1, 2], vec![1, 2])));
        unsafe { readback.clear(&gl) };
        RELEASED.with(|released| assert_eq!(*released.borrow(), (vec![1, 2], vec![1, 2])));
    }

    #[test]
    fn a_pending_screenshot_fence_waits_and_is_released_once_on_clear() {
        RELEASED.with(|released| *released.borrow_mut() = (Vec::new(), Vec::new()));
        let gl = fake_gl();
        let mut readback = ScreenshotReadback::new();
        let mut queue = ScreenshotQueue::isolated_for_test();
        readback.pending.push_back(job(3, &mut queue));
        unsafe { readback.drain_ready(&gl) };
        assert!(readback.has_pending());
        assert_eq!(queue.in_flight_for_test(), 1);
        RELEASED.with(|released| assert_eq!(*released.borrow(), (vec![], vec![])));
        unsafe { readback.clear(&gl) };
        assert!(!readback.has_pending());
        assert_eq!(queue.in_flight_for_test(), 0);
        RELEASED.with(|released| assert_eq!(*released.borrow(), (vec![3], vec![3])));
    }

    #[test]
    fn rejected_screenshot_extent_and_null_fence_release_admission() {
        RELEASED.with(|released| *released.borrow_mut() = (Vec::new(), Vec::new()));
        let gl = fake_gl();
        let mut queue = ScreenshotQueue::isolated_for_test();
        let mut readback = ScreenshotReadback::new();
        unsafe {
            readback.enqueue(&gl, "unused.png".into(), 0, 0, 0, 1, permit(&mut queue));
            readback.enqueue(&gl, "unused.png".into(), 0, 0, 1, 0, permit(&mut queue));
            readback.enqueue(
                &gl,
                "unused.png".into(),
                0,
                0,
                u32::MAX,
                u32::MAX,
                permit(&mut queue),
            );
        }
        assert_eq!(queue.in_flight_for_test(), 0);
        RELEASED.with(|released| assert_eq!(*released.borrow(), (vec![], vec![])));
        unsafe {
            readback.enqueue(&gl, "unused.png".into(), 0, 0, 1, 1, permit(&mut queue));
        }
        assert!(!readback.has_pending());
        assert_eq!(queue.in_flight_for_test(), 0);
        RELEASED.with(|released| assert_eq!(*released.borrow(), (vec![], vec![7])));
    }

    #[test]
    fn a_ready_screenshot_keeps_admission_until_its_writer_releases() {
        RELEASED.with(|released| *released.borrow_mut() = (Vec::new(), Vec::new()));
        let gl = fake_gl_with_pixels(true);
        let mut queue = ScreenshotQueue::isolated_for_test();
        let mut readback = ScreenshotReadback::new();
        readback.pending.push_back(job(2, &mut queue));
        for _ in 0..3 {
            queue.request_full("queued.png".into()).unwrap();
        }
        let mut writers = Vec::new();
        unsafe {
            readback.drain_ready_with(&gl, |_, pixels, width, height, permit| {
                assert_eq!((width, height), (1, 1));
                assert_eq!(pixels, [1, 2, 3, 4]);
                writers.push(permit);
            });
        }
        assert!(!readback.has_pending());
        assert_eq!(queue.in_flight_for_test(), 4);
        assert_eq!(
            queue.request_full("rejected.png".into()),
            Err(ScreenshotBusy)
        );
        // Clearing the GPU queue cannot release a permit already moved to encoding.
        unsafe {
            readback.clear(&gl);
        }
        assert_eq!(queue.in_flight_for_test(), 4);
        RELEASED.with(|released| assert_eq!(*released.borrow(), (vec![2], vec![2])));
        drop(writers);
        queue.request_full("replacement.png".into()).unwrap();
        queue.clear();
        assert_eq!(queue.in_flight_for_test(), 0);
    }
}
