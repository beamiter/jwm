use std::time::{Duration, Instant};

use crate::backend::x11::compositor_common::oml_sync::OmlSyncWindow;

/// GLX_OML_sync_control function pointers.
///
/// Match the extension's C ABI: Xlib Bool is an int, not Rust bool, and all
/// UST/MSC/SBC counters are int64_t at this boundary.
/// <https://registry.khronos.org/OpenGL/extensions/OML/GLX_OML_sync_control.txt>
pub struct OmlSyncControlFunctions {
    pub get_sync_values: Option<
        unsafe extern "C" fn(
            *mut x11::xlib::Display,
            x11::glx::GLXDrawable,
            *mut i64, // ust
            *mut i64, // msc
            *mut i64, // sbc
        ) -> x11::xlib::Bool,
    >,

    pub wait_for_msc: Option<
        unsafe extern "C" fn(
            *mut x11::xlib::Display,
            x11::glx::GLXDrawable,
            i64,      // target_msc
            i64,      // divisor
            i64,      // remainder
            *mut i64, // ust
            *mut i64, // msc
            *mut i64, // sbc
        ) -> x11::xlib::Bool,
    >,

    pub swap_buffers_msc: Option<
        unsafe extern "C" fn(
            *mut x11::xlib::Display,
            x11::glx::GLXDrawable,
            i64, // target_msc
            i64, // divisor
            i64, // remainder
        ) -> i64,
    >,
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
    /// Build the manager from extension entry points resolved by the GLX
    /// platform adapter, which owns all GLX symbol resolution. Returns `None`
    /// when the extension is incomplete.
    pub fn new(
        funcs: OmlSyncControlFunctions,
        xlib_display: *mut x11::xlib::Display,
        glx_drawable: x11::glx::GLXDrawable,
    ) -> Option<Self> {
        let available = funcs.get_sync_values.is_some()
            && funcs.wait_for_msc.is_some()
            && funcs.swap_buffers_msc.is_some();

        if !available {
            log::warn!(
                "compositor: GLX_OML_sync_control not available, falling back to global vsync"
            );
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
        self.windows
            .insert(x11_win, OmlSyncWindow::new(x11_win, fps));
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

        let mut ust: i64 = 0;
        let mut msc: i64 = 0;
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

        if ret != 0 {
            // Keep the existing internal counter representation. UST has an
            // unspecified origin, so a signed value is not itself a failure.
            Some((ust as u64, msc as u64, sbc))
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
        // The C extension takes signed counters. Reject a value that would
        // otherwise cross the ABI as a negative target and raise GLX_BAD_VALUE.
        let target_msc = i64::try_from(target_msc).ok()?;

        let mut ust: i64 = 0;
        let mut msc: i64 = 0;
        let mut sbc: i64 = 0;

        let ret = unsafe {
            wait_fn(
                self.xlib_display,
                self.glx_drawable,
                target_msc,
                1, // divisor (wait for any MSC)
                0, // remainder
                &mut ust,
                &mut msc,
                &mut sbc,
            )
        };

        if ret != 0 {
            // Keep the existing internal counter representation. UST has an
            // unspecified origin, so a signed value is not itself a failure.
            Some((ust as u64, msc as u64, sbc))
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
                1, // divisor (any MSC will do)
                0, // remainder
            )
        };

        if sbc > 0 { Some(sbc) } else { None }
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
        let vblank_interval_ns = 16_666_667u64; // 60Hz

        // Simple heuristic: return ~half the vblank period
        // (actual sync needs more sophisticated timing)
        Some(Duration::from_nanos(vblank_interval_ns / 2))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C" fn mock_get_sync(
        _: *mut x11::xlib::Display,
        _: x11::glx::GLXDrawable,
        ust: *mut i64,
        msc: *mut i64,
        sbc: *mut i64,
    ) -> x11::xlib::Bool {
        unsafe {
            *ust = -5;
            *msc = 42;
            *sbc = 7;
        }
        // A C Bool is an integer truth value, not a Rust bool bit pattern.
        256
    }

    unsafe extern "C" fn mock_get_failure(
        _: *mut x11::xlib::Display,
        _: x11::glx::GLXDrawable,
        _: *mut i64,
        _: *mut i64,
        _: *mut i64,
    ) -> x11::xlib::Bool {
        0
    }

    static WAIT_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    unsafe extern "C" fn mock_wait(
        _: *mut x11::xlib::Display,
        _: x11::glx::GLXDrawable,
        target: i64,
        divisor: i64,
        remainder: i64,
        ust: *mut i64,
        msc: *mut i64,
        sbc: *mut i64,
    ) -> x11::xlib::Bool {
        WAIT_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if divisor != 1 || remainder != 0 {
            return 0;
        }
        unsafe {
            *ust = 123;
            *msc = target;
            *sbc = 9;
        }
        1
    }

    unsafe extern "C" fn mock_swap(
        _: *mut x11::xlib::Display,
        _: x11::glx::GLXDrawable,
        _: i64,
        _: i64,
        _: i64,
    ) -> i64 {
        1
    }

    fn mock_manager() -> OmlSyncControl {
        OmlSyncControl::new(
            OmlSyncControlFunctions {
                get_sync_values: Some(mock_get_sync),
                wait_for_msc: Some(mock_wait),
                swap_buffers_msc: Some(mock_swap),
            },
            std::ptr::null_mut(),
            0,
        )
        .unwrap()
    }

    #[test]
    fn sync_values_use_xlib_bool_and_preserve_signed_counter_bits() {
        let mut manager = mock_manager();
        assert_eq!(manager.get_sync_values(), Some(((-5i64) as u64, 42, 7)));
        manager.funcs.get_sync_values = Some(mock_get_failure);
        assert_eq!(manager.get_sync_values(), None);
    }

    #[test]
    fn wait_rejects_unsigned_target_outside_the_signed_c_abi() {
        let manager = mock_manager();
        WAIT_CALLS.store(0, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(manager.wait_for_msc((i64::MAX as u64) + 1), None);
        assert_eq!(WAIT_CALLS.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert_eq!(manager.wait_for_msc(42), Some((123, 42, 9)));
        assert_eq!(WAIT_CALLS.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

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
