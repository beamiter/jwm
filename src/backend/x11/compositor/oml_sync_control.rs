use std::ffi::CString;
use std::time::{Duration, Instant};

/// GLX_OML_sync_control function pointers
pub struct OmlSyncControlFunctions {
    pub get_sync_values: Option<unsafe extern "C" fn(
        *mut x11::xlib::Display,
        x11::glx::GLXDrawable,
        *mut u64,  // ust
        *mut u64,  // msc
        *mut i64,  // sbc
    ) -> bool>,

    pub wait_for_msc: Option<unsafe extern "C" fn(
        *mut x11::xlib::Display,
        x11::glx::GLXDrawable,
        u64,       // target_msc
        u64,       // divisor
        u64,       // remainder
        *mut u64,  // ust
        *mut u64,  // msc
        *mut i64,  // sbc
    ) -> bool>,

    pub swap_buffers_msc: Option<unsafe extern "C" fn(
        *mut x11::xlib::Display,
        x11::glx::GLXDrawable,
        i64,       // target_msc
        i64,       // divisor
        i64,       // remainder
    ) -> i64>,
}

/// Per-window OML sync state
pub struct OmlSyncWindow {
    pub x11_win: u32,
    pub last_msc: u64,
    pub last_ust: u64,
    pub last_update: Instant,
    pub frame_delay_ns: u64,  // Nanoseconds between frames at current FPS
}

impl OmlSyncWindow {
    pub fn new(x11_win: u32, fps: f32) -> Self {
        let frame_delay_ns = if fps > 0.0 {
            (1_000_000_000.0 / fps as f64).round() as u64
        } else {
            16_666_667  // Default 60Hz
        };

        Self {
            x11_win,
            last_msc: 0,
            last_ust: 0,
            last_update: Instant::now(),
            frame_delay_ns,
        }
    }

    pub fn set_fps(&mut self, fps: f32) {
        self.frame_delay_ns = if fps > 0.0 {
            (1_000_000_000.0 / fps as f64).round() as u64
        } else {
            16_666_667  // Default 60Hz
        };
    }

    /// Estimate next MSC when this window should present
    pub fn estimate_next_msc(&self) -> u64 {
        if self.last_msc == 0 {
            return 0;  // First frame, let compositor decide
        }

        let elapsed = self.last_update.elapsed();
        let elapsed_ns = elapsed.as_nanos() as u64;
        let frames_elapsed = elapsed_ns / self.frame_delay_ns;

        self.last_msc.saturating_add(frames_elapsed.max(1))
    }
}

/// Global OML sync control manager
pub struct OmlSyncControl {
    funcs: OmlSyncControlFunctions,
    available: bool,
    xlib_display: *mut x11::xlib::Display,
    glx_drawable: x11::glx::GLXDrawable,
    windows: std::collections::HashMap<u32, OmlSyncWindow>,
}

impl OmlSyncControl {
    /// Load OML sync control extension functions
    pub fn load(
        xlib_display: *mut x11::xlib::Display,
        glx_drawable: x11::glx::GLXDrawable,
    ) -> Option<Self> {
        let funcs = OmlSyncControlFunctions {
            get_sync_values: unsafe {
                let name = CString::new("glXGetSyncValuesOML").unwrap();
                std::mem::transmute(x11::glx::glXGetProcAddress(name.as_ptr() as *const u8))
            },
            wait_for_msc: unsafe {
                let name = CString::new("glXWaitForMscOML").unwrap();
                std::mem::transmute(x11::glx::glXGetProcAddress(name.as_ptr() as *const u8))
            },
            swap_buffers_msc: unsafe {
                let name = CString::new("glXSwapBuffersMscOML").unwrap();
                std::mem::transmute(x11::glx::glXGetProcAddress(name.as_ptr() as *const u8))
            },
        };

        let available = funcs.get_sync_values.is_some()
            && funcs.wait_for_msc.is_some()
            && funcs.swap_buffers_msc.is_some();

        if !available {
            log::warn!("compositor: GLX_OML_sync_control not available, falling back to global vsync");
            return None;
        }

        log::info!("compositor: GLX_OML_sync_control available, using per-window MSC-based timing");

        Some(Self {
            funcs,
            available: true,
            xlib_display,
            glx_drawable,
            windows: Default::default(),
        })
    }

    pub fn is_available(&self) -> bool {
        self.available
    }

    /// Register a window for OML sync tracking
    pub fn register_window(&mut self, x11_win: u32, fps: f32) {
        self.windows.insert(x11_win, OmlSyncWindow::new(x11_win, fps));
    }

    /// Unregister a window
    pub fn unregister_window(&mut self, x11_win: u32) {
        self.windows.remove(&x11_win);
    }

    /// Update window's target FPS
    pub fn set_window_fps(&mut self, x11_win: u32, fps: f32) {
        if let Some(win) = self.windows.get_mut(&x11_win) {
            win.set_fps(fps);
        }
    }

    /// Get current sync values (UST, MSC, SBC)
    pub fn get_sync_values(&self) -> Option<(u64, u64, i64)> {
        if !self.available {
            return None;
        }

        let get_sync = self.funcs.get_sync_values?;

        let mut ust: u64 = 0;
        let mut msc: u64 = 0;
        let mut sbc: i64 = 0;

        let ret = unsafe {
            get_sync(
                self.xlib_display,
                self.glx_drawable,
                &mut ust,
                &mut msc,
                &mut sbc,
            )
        };

        if ret {
            Some((ust, msc, sbc))
        } else {
            None
        }
    }

    /// Wait for a specific MSC (vblank counter)
    pub fn wait_for_msc(&self, target_msc: u64) -> Option<(u64, u64, i64)> {
        if !self.available {
            return None;
        }

        let wait_fn = self.funcs.wait_for_msc?;

        let mut ust: u64 = 0;
        let mut msc: u64 = 0;
        let mut sbc: i64 = 0;

        let ret = unsafe {
            wait_fn(
                self.xlib_display,
                self.glx_drawable,
                target_msc,
                1,  // divisor (wait for any MSC)
                0,  // remainder
                &mut ust,
                &mut msc,
                &mut sbc,
            )
        };

        if ret {
            Some((ust, msc, sbc))
        } else {
            None
        }
    }

    /// Swap buffers at specific MSC
    pub fn swap_buffers_msc(&self, target_msc: i64) -> Option<i64> {
        if !self.available {
            return None;
        }

        let swap_fn = self.funcs.swap_buffers_msc?;

        let sbc = unsafe {
            swap_fn(
                self.xlib_display,
                self.glx_drawable,
                target_msc,
                1,  // divisor (any MSC will do)
                0,  // remainder
            )
        };

        if sbc > 0 {
            Some(sbc)
        } else {
            None
        }
    }

    /// Estimate next MSC for a window (for independent frame pacing)
    pub fn estimate_next_msc_for_window(&self, x11_win: u32) -> u64 {
        self.windows
            .get(&x11_win)
            .map(|w| w.estimate_next_msc())
            .unwrap_or(0)
    }

    /// Update MSC tracking for a window after it's presented
    pub fn on_window_presented(&mut self, x11_win: u32, new_msc: u64, new_ust: u64) {
        if let Some(win) = self.windows.get_mut(&x11_win) {
            win.last_msc = new_msc;
            win.last_ust = new_ust;
            win.last_update = Instant::now();
        }
    }

    /// Calculate time until next vblank (rough estimate)
    pub fn time_until_next_vblank(&self) -> Option<Duration> {
        let (_ust, _msc, _sbc) = self.get_sync_values()?;

        // Estimate based on last known vblank interval
        // Typical is 16.67ms for 60Hz, but this is a rough estimate
        // In real implementation, should track actual vblank period
        let vblank_interval_ns = 16_666_667u64;  // 60Hz

        // Simple heuristic: return ~half the vblank period
        // (actual sync needs more sophisticated timing)
        Some(Duration::from_nanos(vblank_interval_ns / 2))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_oml_sync_window_fps() {
        let mut win = OmlSyncWindow::new(1, 30.0);
        assert_eq!(win.frame_delay_ns, 33_333_333);

        win.set_fps(60.0);
        assert_eq!(win.frame_delay_ns, 16_666_667);

        win.set_fps(24.0);
        assert_eq!(win.frame_delay_ns, 41_666_667);
    }

    #[test]
    fn test_oml_sync_window_msc_estimation() {
        let win = OmlSyncWindow::new(1, 60.0);
        // With no previous MSC, should return 0
        assert_eq!(win.estimate_next_msc(), 0);
    }
}
