/// Safe RAII wrapper for GLX context and related state.
/// Eliminates unsafe raw pointer management scattered throughout the codebase.

use std::ffi::CString;
use x11::{glx, xlib};

/// Safe wrapper around GLX context, display, and drawable.
/// Automatically cleans up on Drop.
pub struct GlxContext {
    /// X11 display connection (borrowed, not owned)
    display: *mut xlib::Display,
    /// GLX context
    context: glx::GLXContext,
    /// GLX drawable (usually overlay window)
    drawable: glx::GLXDrawable,
    /// Whether context is currently active
    is_active: bool,
}

impl GlxContext {
    /// Create a new GLX context wrapper
    pub fn new(display: *mut xlib::Display, context: glx::GLXContext, drawable: glx::GLXDrawable) -> Self {
        Self {
            display,
            context,
            drawable,
            is_active: false,
        }
    }

    /// Make this context current
    pub fn make_current(&mut self) -> Result<(), String> {
        unsafe {
            if glx::glXMakeContextCurrent(self.display, self.drawable, self.drawable, self.context) == 0 {
                return Err("glXMakeContextCurrent failed".into());
            }
        }
        self.is_active = true;
        Ok(())
    }

    /// Release the context (make no context current)
    pub fn release_current(&mut self) -> Result<(), String> {
        unsafe {
            if glx::glXMakeContextCurrent(self.display, 0, 0, std::ptr::null_mut()) == 0 {
                return Err("glXMakeContextCurrent(NULL) failed".into());
            }
        }
        self.is_active = false;
        Ok(())
    }

    /// Swap buffers (present frame)
    pub fn swap_buffers(&self) -> Result<(), String> {
        unsafe {
            glx::glXSwapBuffers(self.display, self.drawable);
        }
        Ok(())
    }

    /// Check if context is currently active
    pub fn is_active(&self) -> bool {
        self.is_active
    }

    /// Load a GLX extension function
    pub fn get_proc_address(name: &str) -> Option<unsafe extern "C" fn()> {
        let c_name = CString::new(name).ok()?;
        unsafe {
            glx::glXGetProcAddress(c_name.as_ptr() as *const u8)
        }
    }

    /// Get underlying display pointer
    pub fn display(&self) -> *mut xlib::Display {
        self.display
    }

    /// Get underlying GLX context
    pub fn context(&self) -> glx::GLXContext {
        self.context
    }

    /// Get underlying drawable
    pub fn drawable(&self) -> glx::GLXDrawable {
        self.drawable
    }
}

impl Drop for GlxContext {
    fn drop(&mut self) {
        // Release context if active
        let _ = self.release_current();

        // Destroy context
        unsafe {
            glx::glXDestroyContext(self.display, self.context);
        }
    }
}

/// RAII guard for making GLX context current.
/// Automatically restores previous context on drop.
pub struct GlxGuard {
    context: *mut glx::GLXContext,
    is_managed: bool,
}

impl GlxGuard {
    /// Make context current and return guard
    pub fn new(display: *mut xlib::Display, drawable: glx::GLXDrawable, context: glx::GLXContext) -> Result<Self, String> {
        unsafe {
            if glx::glXMakeContextCurrent(display, drawable, drawable, context) == 0 {
                return Err("glXMakeContextCurrent failed".into());
            }
        }
        Ok(Self {
            context: std::ptr::null_mut(),
            is_managed: true,
        })
    }
}

impl Drop for GlxGuard {
    fn drop(&mut self) {
        if self.is_managed {
            // Release context
            unsafe {
                glx::glXMakeContextCurrent(std::ptr::null_mut(), 0, 0, std::ptr::null_mut());
            }
        }
    }
}
