//! Separate window grants from desktop capture, including all fallback paths.
use super::x11_capture::{
    CaptureArea, CaptureImageRecycler, CaptureMode, CaptureOutcome, CursorSample, X11Capture,
};
use super::{
    RemoteResult, messages::FrameLimits, window_capture::WindowCapture, x11_input::InputArea,
};
use std::time::Duration;

pub(super) enum HostCapture {
    Desktop(Box<X11Capture>),
    Window(Box<WindowCapture>),
    #[cfg(feature = "remote-wayland")]
    Wayland(Box<super::wayland_capture::WaylandCapture>),
}

impl HostCapture {
    pub(super) fn enable_pointer(&mut self) -> RemoteResult<()> {
        #[cfg(feature = "remote-wayland")]
        if let Self::Wayland(c) = self {
            return c.enable_pointer();
        }
        Err(std::io::Error::other("native pointer unavailable").into())
    }
    pub(super) fn grant_pointer(&mut self) -> RemoteResult<u64> {
        #[cfg(feature = "remote-wayland")]
        if let Self::Wayland(c) = self {
            return c.grant_pointer();
        }
        Err(std::io::Error::other("native pointer unavailable").into())
    }
    pub(super) fn pointer_revoked(&self) -> bool {
        #[cfg(feature = "remote-wayland")]
        if let Self::Wayland(c) = self {
            return c.pointer_revoked();
        }
        true
    }
    pub(super) fn pointer_frame(&self) -> Option<(u64, std::time::Instant)> {
        #[cfg(feature = "remote-wayland")]
        if let Self::Wayland(c) = self {
            return c.pointer_frame();
        }
        None
    }
    pub(super) fn pointer_action(
        &mut self,
        action: &super::wayland_pointer_wire::PointerAction,
        serial: u64,
    ) -> RemoteResult<()> {
        #[cfg(feature = "remote-wayland")]
        if let Self::Wayland(c) = self {
            return c.pointer_action(action, serial);
        }
        {
            let _ = (action, serial);
            Err(std::io::Error::other("native pointer unavailable").into())
        }
    }
    pub(super) fn revoke_pointer(&mut self) {
        #[cfg(feature = "remote-wayland")]
        if let Self::Wayland(c) = self {
            c.revoke_pointer();
        }
    }
    pub(super) fn discard_pointer_frame(&mut self) {
        #[cfg(feature = "remote-wayland")]
        if let Self::Wayland(c) = self {
            c.discard_pointer_frame();
        }
    }
    pub(super) fn enable_keyboard(&mut self) -> RemoteResult<()> {
        #[cfg(feature = "remote-wayland")]
        if let Self::Wayland(c) = self {
            return c.enable_keyboard();
        }
        Err(std::io::Error::other("keyboard lease requires a native Wayland source").into())
    }
    pub(super) fn arm_keyboard(&mut self, digest: [u8; 32]) -> RemoteResult<u64> {
        #[cfg(feature = "remote-wayland")]
        if let Self::Wayland(c) = self {
            return c.arm_keyboard(digest);
        }
        let _ = digest;
        Err(std::io::Error::other("native keyboard lease unavailable").into())
    }
    pub(super) fn keyboard_key(
        &mut self,
        key: &super::wayland_keyboard_wire::Key,
    ) -> RemoteResult<()> {
        #[cfg(feature = "remote-wayland")]
        if let Self::Wayland(c) = self {
            return c.keyboard_key(key);
        }
        let _ = key;
        Err(std::io::Error::other("native keyboard lease unavailable").into())
    }
    pub(super) fn revoke_keyboard(&mut self) {
        #[cfg(feature = "remote-wayland")]
        if let Self::Wayland(c) = self {
            c.revoke_keyboard();
        }
    }
    pub(super) fn keyboard_revoked(&self) -> bool {
        #[cfg(feature = "remote-wayland")]
        if let Self::Wayland(c) = self {
            return c.keyboard_revoked();
        }
        true
    }

    pub(super) fn disable_native_resize(&mut self) {
        if let Self::Window(c) = self {
            c.disable_native_resize();
        }
    }
    pub(super) fn request_native_resize(
        &mut self,
        expected: (u16, u16),
        target: (u16, u16),
    ) -> RemoteResult<(u16, u16)> {
        match self {
            Self::Window(c) => c.request_native_resize(expected, target),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(_) => Err(std::io::Error::other("Wayland source is read-only").into()),
            Self::Desktop(_) => {
                Err(std::io::Error::other("native resize requires a window grant").into())
            }
        }
    }

    pub(super) fn frame(&mut self) -> RemoteResult<CaptureOutcome> {
        match self {
            Self::Desktop(c) => c.frame(),
            Self::Window(c) => c.frame(),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(c) => c.frame(),
        }
    }
    pub(super) fn set_frame_limits(&mut self, limits: FrameLimits) -> bool {
        match self {
            Self::Desktop(c) => c.set_frame_limits(limits),
            Self::Window(c) => c.set_frame_limits(limits),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(c) => c.set_frame_limits(limits),
        }
    }
    pub(super) fn set_encoded_bounds(&mut self, width: u16, height: u16) -> bool {
        match self {
            Self::Desktop(c) => c.set_encoded_bounds(width, height),
            Self::Window(c) => c.set_encoded_bounds(width, height),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(c) => c.set_encoded_bounds(width, height),
        }
    }
    pub(super) fn geometry_epoch(&self) -> u64 {
        match self {
            Self::Desktop(c) => c.geometry_epoch(),
            Self::Window(c) => c.geometry_epoch(),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(c) => c.geometry_epoch(),
        }
    }
    pub(super) fn image_recycler(&self) -> CaptureImageRecycler {
        match self {
            Self::Desktop(c) => c.image_recycler(),
            Self::Window(c) => c.image_recycler(),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(c) => c.image_recycler(),
        }
    }
    pub(super) fn wait_for_activity(&mut self, timeout: Duration) -> RemoteResult<bool> {
        match self {
            Self::Desktop(c) => c.wait_for_activity(timeout),
            Self::Window(c) => c.wait_for_activity(timeout),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(c) => c.wait_for_activity(timeout),
        }
    }
    pub(super) fn mode(&self) -> CaptureMode {
        match self {
            Self::Desktop(c) => c.mode(),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(_) => {
                CaptureMode::WAYLAND | CaptureMode::SHM | CaptureMode::GEOMETRY_EVENTS
            }
            Self::Window(_) => {
                CaptureMode::WINDOW | CaptureMode::GEOMETRY_EVENTS | CaptureMode::COMPOSITOR_EVENTS
            }
        }
    }
    pub(super) fn input_area(&self) -> InputArea {
        match self {
            Self::Desktop(c) => c.input_area(),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(_) => InputArea {
                origin: (0, 0),
                size: Some((0, 0)),
            },
            Self::Window(_) => InputArea {
                origin: (0, 0),
                size: Some((0, 0)),
            },
        }
    }
    pub(super) fn enable_cursor_plane(&mut self) -> bool {
        match self {
            Self::Desktop(c) => c.enable_cursor_plane(),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(_) => false,
            Self::Window(_) => false,
        }
    }
    pub(super) fn cursor_plane_enabled(&self) -> bool {
        match self {
            Self::Desktop(c) => c.cursor_plane_enabled(),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(_) => false,
            Self::Window(_) => false,
        }
    }
    pub(super) fn poll_cursor_plane(&mut self) -> RemoteResult<Option<CursorSample>> {
        match self {
            Self::Desktop(c) => c.poll_cursor_plane(),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(_) => Ok(None),
            Self::Window(_) => Ok(None),
        }
    }
    pub(super) fn switch_area(&mut self, area: CaptureArea) -> RemoteResult<()> {
        match self {
            Self::Desktop(c) => c.switch_area(area),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(_) => {
                Err(std::io::Error::other("Wayland instance cannot switch sources").into())
            }
            Self::Window(_) => Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "window instance grants cannot switch sources",
            )
            .into()),
        }
    }
}
