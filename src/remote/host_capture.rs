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
}

impl HostCapture {
    pub(super) fn frame(&mut self) -> RemoteResult<CaptureOutcome> {
        match self {
            Self::Desktop(c) => c.frame(),
            Self::Window(c) => c.frame(),
        }
    }
    pub(super) fn set_frame_limits(&mut self, limits: FrameLimits) -> bool {
        match self {
            Self::Desktop(c) => c.set_frame_limits(limits),
            Self::Window(c) => c.set_frame_limits(limits),
        }
    }
    pub(super) fn set_encoded_bounds(&mut self, width: u16, height: u16) -> bool {
        match self {
            Self::Desktop(c) => c.set_encoded_bounds(width, height),
            Self::Window(c) => c.set_encoded_bounds(width, height),
        }
    }
    pub(super) fn geometry_epoch(&self) -> u64 {
        match self {
            Self::Desktop(c) => c.geometry_epoch(),
            Self::Window(c) => c.geometry_epoch(),
        }
    }
    pub(super) fn image_recycler(&self) -> CaptureImageRecycler {
        match self {
            Self::Desktop(c) => c.image_recycler(),
            Self::Window(c) => c.image_recycler(),
        }
    }
    pub(super) fn wait_for_activity(&mut self, timeout: Duration) -> RemoteResult<bool> {
        match self {
            Self::Desktop(c) => c.wait_for_activity(timeout),
            Self::Window(c) => c.wait_for_activity(timeout),
        }
    }
    pub(super) fn mode(&self) -> CaptureMode {
        match self {
            Self::Desktop(c) => c.mode(),
            Self::Window(_) => {
                CaptureMode::WINDOW | CaptureMode::GEOMETRY_EVENTS | CaptureMode::COMPOSITOR_EVENTS
            }
        }
    }
    pub(super) fn input_area(&self) -> InputArea {
        match self {
            Self::Desktop(c) => c.input_area(),
            Self::Window(_) => InputArea {
                origin: (0, 0),
                size: Some((0, 0)),
            },
        }
    }
    pub(super) fn enable_cursor_plane(&mut self) -> bool {
        match self {
            Self::Desktop(c) => c.enable_cursor_plane(),
            Self::Window(_) => false,
        }
    }
    pub(super) fn cursor_plane_enabled(&self) -> bool {
        match self {
            Self::Desktop(c) => c.cursor_plane_enabled(),
            Self::Window(_) => false,
        }
    }
    pub(super) fn poll_cursor_plane(&mut self) -> RemoteResult<Option<CursorSample>> {
        match self {
            Self::Desktop(c) => c.poll_cursor_plane(),
            Self::Window(_) => Ok(None),
        }
    }
    pub(super) fn switch_area(&mut self, area: CaptureArea) -> RemoteResult<()> {
        match self {
            Self::Desktop(c) => c.switch_area(area),
            Self::Window(_) => Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "window instance grants cannot switch sources",
            )
            .into()),
        }
    }
}
