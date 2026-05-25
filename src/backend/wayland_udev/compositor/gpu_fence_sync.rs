use smithay::backend::renderer::gles::ffi;
use std::collections::HashMap;
use std::time::{Duration, Instant};

struct WindowFenceState {
    fence: ffi::types::GLsync,
    fence_time: Instant,
    fence_signaled: bool,
}

pub(crate) struct GpuFenceSyncManager {
    fences: HashMap<u64, WindowFenceState>,
    cleanup_interval: Duration,
    last_cleanup: Instant,
    fences_created: u64,
    fences_cleaned: u64,
    blocked_waits: u64,
}

impl GpuFenceSyncManager {
    pub(crate) fn new() -> Self {
        Self {
            fences: HashMap::new(),
            cleanup_interval: Duration::from_secs(1),
            last_cleanup: Instant::now(),
            fences_created: 0,
            fences_cleaned: 0,
            blocked_waits: 0,
        }
    }

    pub(crate) unsafe fn register_fence(&mut self, gl: &ffi::Gles2, window_id: u64) {
        if let Some(old) = self.fences.remove(&window_id) {
            unsafe { gl.DeleteSync(old.fence) };
        }

        let fence = unsafe { gl.FenceSync(ffi::SYNC_GPU_COMMANDS_COMPLETE, 0) };
        if !fence.is_null() {
            self.fences.insert(
                window_id,
                WindowFenceState {
                    fence,
                    fence_time: Instant::now(),
                    fence_signaled: false,
                },
            );
            self.fences_created += 1;
        }
    }

    pub(crate) unsafe fn update_fence_states(&mut self, gl: &ffi::Gles2) {
        for state in self.fences.values_mut() {
            if state.fence_signaled {
                continue;
            }
            let result = unsafe { gl.ClientWaitSync(state.fence, 0, 0) };
            if result == ffi::ALREADY_SIGNALED || result == ffi::CONDITION_SATISFIED {
                state.fence_signaled = true;
            }
        }
    }

    pub(crate) unsafe fn cleanup_old_fences(&mut self, gl: &ffi::Gles2) {
        let now = Instant::now();
        if now.duration_since(self.last_cleanup) < self.cleanup_interval {
            return;
        }
        self.last_cleanup = now;

        let mut to_remove = Vec::new();
        for (&window_id, state) in &self.fences {
            let age = now.duration_since(state.fence_time);
            if state.fence_signaled && age > Duration::from_millis(100) {
                to_remove.push(window_id);
            } else if !state.fence_signaled && age > Duration::from_secs(1) {
                to_remove.push(window_id);
            }
        }

        for window_id in to_remove {
            if let Some(state) = self.fences.remove(&window_id) {
                unsafe { gl.DeleteSync(state.fence) };
                self.fences_cleaned += 1;
            }
        }
    }

    pub(crate) fn is_fence_signaled(&self, window_id: u64) -> bool {
        match self.fences.get(&window_id) {
            Some(state) => state.fence_signaled,
            None => true,
        }
    }

    pub(crate) unsafe fn wait_fence(&mut self, gl: &ffi::Gles2, window_id: u64) {
        if let Some(state) = self.fences.get_mut(&window_id) {
            if state.fence_signaled {
                return;
            }
            self.blocked_waits += 1;
            let timeout_ns: u64 = 16_000_000;
            let result =
                unsafe { gl.ClientWaitSync(state.fence, ffi::SYNC_FLUSH_COMMANDS_BIT, timeout_ns) };
            if result == ffi::ALREADY_SIGNALED || result == ffi::CONDITION_SATISFIED {
                state.fence_signaled = true;
            }
        }
    }

    pub(crate) unsafe fn remove_window(&mut self, gl: &ffi::Gles2, window_id: u64) {
        if let Some(state) = self.fences.remove(&window_id) {
            unsafe { gl.DeleteSync(state.fence) };
        }
    }

    pub(crate) fn stats(&self) -> (u64, u64, u64, usize) {
        (
            self.fences_created,
            self.fences_cleaned,
            self.blocked_waits,
            self.fences.len(),
        )
    }
}
