//! Thin presentation dispatch; authentication, decoding and ACKs stay in client.rs.
use super::{
    RemoteResult,
    client::ClientOptions,
    cursor::{CursorPosition, CursorShape},
    frame::RecyclableDecodedFrame,
    x11_keymap::KeymapTable,
    x11_viewer::{self, ViewerEvent},
};
use std::{os::fd::BorrowedFd, sync::Arc, time::Instant};
pub(super) fn wayland_initial_size(
    size: Option<super::window_size::WindowSize>,
) -> super::window_size::WindowSize {
    size.unwrap_or(super::window_size::WindowSize {
        width: 800,
        height: 600,
    })
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ViewerBackend {
    #[default]
    X11,
    Wayland,
}
impl ViewerBackend {
    /// Keep the native one-shot lease fail-closed at both public-client and
    /// concrete-viewer boundaries. A native viewer must never silently ignore
    /// an X11-only input policy, even when a caller skips client preflight.
    pub(super) fn validate_seamless_input(
        self,
        seamless: bool,
        grab: bool,
        view_only: bool,
    ) -> RemoteResult<()> {
        if seamless && (!grab || view_only || self != Self::X11) {
            return Err(std::io::Error::other(
                "seamless input requires X11, --grab-input and input control",
            )
            .into());
        }
        Ok(())
    }
}
impl std::str::FromStr for ViewerBackend {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, String> {
        match value {
            "x11" => Ok(Self::X11),
            "wayland" => Ok(Self::Wayland),
            _ => Err("viewer backend must be x11 or wayland".into()),
        }
    }
}
impl std::fmt::Display for ViewerBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::X11 => "x11",
            Self::Wayland => "wayland",
        })
    }
}
pub(super) enum Viewer {
    X11(Box<x11_viewer::Viewer>),
    #[cfg(feature = "remote-wayland")]
    Wayland(Box<super::wayland_viewer::WaylandViewer>),
}
impl Viewer {
    pub(super) fn connect(
        options: &ClientOptions,
        width: u16,
        height: u16,
        pointer: bool,
        keyboard: bool,
        keymap: Option<KeymapTable>,
    ) -> RemoteResult<Self> {
        options.viewer_backend.validate_seamless_input(
            options.seamless_input,
            options.grab_input,
            options.view_only,
        )?;
        if options.seamless_input && !(pointer && keyboard) {
            return Err(std::io::Error::other("seamless input requires negotiated pointer and keyboard control; check host input permission and keymap compatibility").into());
        }
        match options.viewer_backend {
            ViewerBackend::X11 => {
                let mut viewer = x11_viewer::Viewer::connect_with_window_geometry(
                    options.display.as_deref(),
                    width,
                    height,
                    options.window_size,
                    options.window_position,
                    options.scaling,
                    pointer,
                    keyboard,
                    keymap,
                    options.grab_input && (pointer || keyboard),
                    &options.escape_key,
                    &options.peer_label,
                )?;
                viewer.set_seamless_input(options.seamless_input);
                Ok(Self::X11(Box::new(viewer)))
            }
            ViewerBackend::Wayland => {
                #[cfg(feature = "remote-wayland")]
                {
                    #[cfg(test)]
                    if let Some(stream) = TEST_WAYLAND_STREAM.with(|slot| slot.borrow_mut().take())
                    {
                        return Ok(Self::Wayland(Box::new(
                            super::wayland_viewer::WaylandViewer::connect_test_stream(
                                stream,
                                width,
                                height,
                                Some(wayland_initial_size(options.window_size)),
                                &options.peer_label,
                            )?,
                        )));
                    }
                    Ok(Self::Wayland(Box::new(
                        super::wayland_viewer::WaylandViewer::connect(
                            options.viewer_wayland_display.as_deref(),
                            width,
                            height,
                            Some(wayland_initial_size(options.window_size)),
                            &options.peer_label,
                        )?,
                    )))
                }
                #[cfg(not(feature = "remote-wayland"))]
                {
                    Err(std::io::Error::other("Wayland viewer support is not compiled").into())
                }
            }
        }
    }
    pub(super) fn poll_events(&mut self) -> RemoteResult<Vec<ViewerEvent>> {
        match self {
            Self::X11(v) => v.poll_events(),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(v) => v.poll_events(),
        }
    }
    pub(super) fn viewport(&self) -> (u16, u16) {
        match self {
            Self::X11(v) => v.viewport(),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(v) => v.viewport(),
        }
    }
    pub(super) fn draw_recyclable(&mut self, frame: RecyclableDecodedFrame) -> RemoteResult<bool> {
        match self {
            Self::X11(v) => v.draw_recyclable(frame),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(v) => v.draw_recyclable(frame),
        }
    }
    pub(super) fn can_submit_frame(&self) -> bool {
        match self {
            Self::X11(v) => v.can_submit_frame(),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(v) => v.can_submit_frame(),
        }
    }
    pub(super) fn take_presented_sequences(&mut self) -> RemoteResult<Vec<u64>> {
        match self {
            Self::X11(v) => v.take_presented_sequences(),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(v) => v.take_presented_sequences(),
        }
    }
    pub(super) fn prepare_wait(&mut self) -> RemoteResult<()> {
        match self {
            Self::X11(v) => v.prepare_wait(),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(v) => v.prepare_wait(),
        }
    }
    pub(super) fn has_pending_events(&mut self) -> RemoteResult<bool> {
        match self {
            Self::X11(v) => v.has_pending_events(),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(v) => v.has_pending_events(),
        }
    }
    pub(super) fn connection_fd(&self) -> BorrowedFd<'_> {
        match self {
            Self::X11(v) => v.connection_fd(),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(v) => v.connection_fd(),
        }
    }
    pub(super) fn next_event_deadline(&self) -> Option<Instant> {
        match self {
            Self::X11(v) => v.next_event_deadline(),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(v) => v.next_event_deadline(),
        }
    }
    pub(super) fn user_close_requested(&self) -> bool {
        match self {
            Self::X11(v) => v.user_close_requested(),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(v) => v.user_close_requested(),
        }
    }
    pub(super) fn prepare_frame_geometry(&mut self, epoch: u64) -> RemoteResult<bool> {
        match self {
            Self::X11(v) => v.prepare_frame_geometry(epoch),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(v) => v.prepare_frame_geometry(epoch),
        }
    }
    pub(super) fn refresh_pending_redraw(&mut self) -> RemoteResult<()> {
        match self {
            Self::X11(v) => v.refresh_pending_redraw(),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(v) => v.refresh_pending_redraw(),
        }
    }
    pub(super) fn invalidate_source(&mut self, label: String) -> RemoteResult<()> {
        match self {
            Self::X11(v) => v.invalidate_source(label),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(v) => v.invalidate_source(label),
        }
    }
    pub(super) fn set_source_label(&mut self, label: String) {
        match self {
            Self::X11(v) => v.set_source_label(label),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(v) => v.set_source_label(label),
        }
    }
    pub(super) fn source_presented(&mut self, epoch: u64) {
        match self {
            Self::X11(v) => v.source_presented(epoch),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(v) => v.source_presented(epoch),
        }
    }
    pub(super) fn user_released_grab(&self) -> bool {
        match self {
            Self::X11(v) => v.user_released_grab(),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(_) => false,
        }
    }
    pub(super) fn reconnect_allowed_after_cleanup(&mut self) -> RemoteResult<bool> {
        match self {
            Self::X11(v) => v.reconnect_allowed_after_cleanup(),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(_) => Ok(false),
        }
    }
    pub(super) fn enable_cursor_plane(&mut self) -> RemoteResult<()> {
        match self {
            Self::X11(v) => v.enable_cursor_plane(),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(_) => {
                Err(std::io::Error::other("Wayland viewer cursor plane is unsupported").into())
            }
        }
    }
    pub(super) fn set_cursor_update(
        &mut self,
        shape: Arc<CursorShape>,
        position: CursorPosition,
    ) -> RemoteResult<()> {
        match self {
            Self::X11(v) => v.set_cursor_update(shape, position),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(_) => {
                let _ = (shape, position);
                Err(std::io::Error::other("Wayland viewer cursor plane is unsupported").into())
            }
        }
    }
    pub(super) fn enable_wayland_pointer(
        &mut self,
        scroll_enabled: bool,
        horizontal_enabled: bool,
    ) -> RemoteResult<()> {
        match self {
            Self::X11(v) => {
                let _ = (scroll_enabled, horizontal_enabled);
                v.enable_wayland_pointer()
            }
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(v) => v.enable_wayland_pointer(scroll_enabled, horizontal_enabled),
        }
    }
    /// Native scroll admission fence; legacy X11 input keeps its existing gate.
    pub(super) fn wayland_pointer_ready(&mut self, sequence: Option<u64>) -> RemoteResult<()> {
        match self {
            Self::X11(_) => {
                let _ = sequence;
                Ok(())
            }
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(v) => v.wayland_pointer_ready(sequence),
        }
    }
    pub(super) fn enable_source_cycle(&mut self, enabled: bool) {
        match self {
            Self::X11(v) => v.enable_source_cycle(enabled),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(_) => {
                let _ = enabled;
            }
        }
    }
    pub(super) fn enable_clipboard_narrowing(&mut self, enabled: bool) {
        match self {
            Self::X11(v) => v.enable_clipboard_narrowing(enabled),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(_) => {
                let _ = enabled;
            }
        }
    }
    pub(super) fn set_clipboard_status(&mut self, enabled: bool) {
        match self {
            Self::X11(v) => v.set_clipboard_status(enabled),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(_) => {
                let _ = enabled;
            }
        }
    }
    pub(super) fn set_clipboard_policy(&mut self, policy: String) {
        match self {
            Self::X11(v) => v.set_clipboard_policy(policy),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(_) => {
                let _ = policy;
            }
        }
    }
    pub(super) fn set_clipboard_activity(&mut self, activity: Option<String>) {
        match self {
            Self::X11(v) => v.set_clipboard_activity(activity),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(_) => {
                let _ = activity;
            }
        }
    }
    pub(super) fn set_wayland_pointer_status(&mut self, status: &'static str) {
        match self {
            Self::X11(v) => v.set_wayland_pointer_status(status),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(v) => v.set_wayland_pointer_status(status),
        }
    }
    #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
    pub(super) fn wayland_control_clean_for_resume(&self) -> bool {
        match self {
            Self::X11(_) => false,
            Self::Wayland(viewer) => viewer.wayland_control_clean_for_resume(),
        }
    }
    pub(super) fn wayland_control_repair_busy(&self) -> bool {
        match self {
            Self::X11(_) => false,
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(v) => v.wayland_control_repair_busy(),
        }
    }
    pub(super) fn wayland_pointer_repair_busy(&self) -> bool {
        match self {
            Self::X11(_) => false,
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(v) => v.wayland_pointer_repair_busy(),
        }
    }
    pub(super) fn wayland_pointer_presented(&mut self, sequence: u64) {
        match self {
            Self::X11(v) => v.wayland_pointer_presented(sequence),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(v) => v.wayland_pointer_presented(sequence),
        }
    }
    #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
    pub(super) fn observe_control_state(&self, sequence: Option<u64>) {
        let sequence = match self {
            Self::Wayland(viewer) => sequence.filter(|seq| viewer.local_scroll_ready(*seq)),
            Self::X11(_) => None,
        };
        TEST_WAYLAND_CONTROL_OBSERVER.with(|slot| {
            if let Some(observer) = slot.borrow().as_ref() {
                *observer.lock().unwrap() = sequence;
            }
        });
    }
    #[cfg(feature = "remote-wayland")]
    pub(super) fn enable_wayland_control(
        &mut self,
        scroll_enabled: bool,
        horizontal_enabled: bool,
    ) -> RemoteResult<[u8; 32]> {
        match self {
            Self::X11(_) => Err("combined control requires native Wayland viewer".into()),
            Self::Wayland(v) => v.enable_wayland_control(scroll_enabled, horizontal_enabled),
        }
    }
    #[cfg(feature = "remote-wayland")]
    pub(super) fn check_wayland_control(&mut self, initial: bool) -> RemoteResult<()> {
        match self {
            Self::X11(_) => Err("combined control requires native Wayland viewer".into()),
            Self::Wayland(v) => v.check_wayland_control(initial),
        }
    }
    #[cfg(feature = "remote-wayland")]
    pub(super) fn disable_wayland_control(&mut self) {
        match self {
            Self::X11(_) => {}
            Self::Wayland(v) => v.disable_wayland_control(),
        }
    }
    #[cfg(feature = "remote-wayland")]
    pub(super) fn enable_wayland_keyboard(
        &mut self,
        display: Option<&str>,
    ) -> RemoteResult<[u8; 32]> {
        match self {
            Self::X11(v) => v.enable_wayland_keyboard(display),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(v) => v.enable_wayland_keyboard(display),
        }
    }
    #[cfg(feature = "remote-wayland")]
    pub(super) fn check_wayland_keyboard(&mut self, initial: bool) -> RemoteResult<()> {
        match self {
            Self::X11(v) => v.check_wayland_keyboard(initial),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(v) => v.check_wayland_keyboard(initial),
        }
    }
    #[cfg(feature = "remote-wayland")]
    pub(super) fn disable_wayland_keyboard(&mut self) {
        match self {
            Self::X11(v) => v.disable_wayland_keyboard(),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(v) => v.disable_wayland_keyboard(),
        }
    }
    #[cfg(test)]
    pub(super) fn observe_keyboard_state(&self, armed: bool, ended: bool) -> RemoteResult<()> {
        match self {
            Self::X11(v) => v.observe_keyboard_state(armed, ended),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(_) => {
                #[cfg(feature = "backend-wayland-udev")]
                TEST_WAYLAND_KEYBOARD_OBSERVER.with(|slot| {
                    if let Some(observer) = slot.borrow().as_ref() {
                        observer.store(
                            if ended { 2 } else { u8::from(armed) },
                            std::sync::atomic::Ordering::Release,
                        );
                    }
                });
                Ok(())
            }
        }
    }
    #[cfg(test)]
    pub(super) fn observe_scroll(&self, count: u64) -> RemoteResult<()> {
        match self {
            Self::X11(viewer) => viewer.observe_scroll(count),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(_) => Ok(()),
        }
    }
    #[cfg(test)]
    pub(super) fn observe_pointer_ready(&self, sequence: Option<u64>) -> RemoteResult<()> {
        match self {
            Self::X11(v) => v.observe_pointer_ready(sequence),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(viewer) => {
                let sequence = sequence.filter(|sequence| viewer.local_scroll_ready(*sequence));
                #[cfg(not(feature = "backend-wayland-udev"))]
                let _ = sequence;
                #[cfg(feature = "backend-wayland-udev")]
                TEST_WAYLAND_POINTER_OBSERVER.with(|slot| {
                    if let Some(observer) = slot.borrow().as_ref() {
                        *observer.lock().unwrap() = sequence;
                    }
                });
                Ok(())
            }
        }
    }
    #[cfg(test)]
    pub(super) fn observe_pointer_candidate(
        &self,
        candidate: Option<u64>,
        left_down: bool,
        sequence: u64,
    ) -> RemoteResult<()> {
        match self {
            Self::X11(v) => v.observe_pointer_candidate(candidate, left_down, sequence),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(_) => {
                #[cfg(feature = "backend-wayland-udev")]
                TEST_WAYLAND_POINTER_CANDIDATE_OBSERVER.with(|slot| {
                    if let Some(observer) = slot.borrow().as_ref() {
                        *observer.lock().unwrap() = (candidate, left_down, sequence);
                    }
                });
                Ok(())
            }
        }
    }
}

#[cfg(all(test, feature = "remote-wayland"))]
thread_local! { static TEST_WAYLAND_STREAM: std::cell::RefCell<Option<std::os::unix::net::UnixStream>> = const { std::cell::RefCell::new(None) }; }
/// A once-only test transport. Never selected from production environment state.
#[cfg(all(test, feature = "remote-wayland"))]
pub(crate) fn with_wayland_test_stream<T>(
    stream: std::os::unix::net::UnixStream,
    run: impl FnOnce() -> T,
) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_WAYLAND_STREAM.with(|slot| {
                slot.borrow_mut().take();
            });
        }
    }
    TEST_WAYLAND_STREAM.with(|slot| {
        assert!(slot.borrow().is_none());
        *slot.borrow_mut() = Some(stream);
    });
    let _reset = Reset;
    run()
}

// Test-only observation of real client lease state, never an authorization input.
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
thread_local! { static TEST_WAYLAND_KEYBOARD_OBSERVER: std::cell::RefCell<Option<Arc<std::sync::atomic::AtomicU8>>> = const { std::cell::RefCell::new(None) }; }
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
pub(crate) fn with_wayland_keyboard_observer<T>(
    observer: Arc<std::sync::atomic::AtomicU8>,
    run: impl FnOnce() -> T,
) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_WAYLAND_KEYBOARD_OBSERVER.with(|slot| {
                slot.borrow_mut().take();
            });
        }
    }
    TEST_WAYLAND_KEYBOARD_OBSERVER.with(|slot| {
        assert!(slot.borrow().is_none());
        *slot.borrow_mut() = Some(observer);
    });
    let _reset = Reset;
    run()
}

// Read-only test observation: None means not currently Ready; Some is the
// exact actual-presented frame sequence accepted by the client lease state.
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
thread_local! { static TEST_WAYLAND_POINTER_OBSERVER: std::cell::RefCell<Option<Arc<std::sync::Mutex<Option<u64>>>>> = const { std::cell::RefCell::new(None) }; }
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
pub(crate) fn with_wayland_pointer_observer<T>(
    observer: Arc<std::sync::Mutex<Option<u64>>>,
    run: impl FnOnce() -> T,
) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_WAYLAND_POINTER_OBSERVER.with(|slot| {
                slot.borrow_mut().take();
            });
        }
    }
    TEST_WAYLAND_POINTER_OBSERVER.with(|slot| {
        assert!(slot.borrow().is_none());
        *slot.borrow_mut() = Some(observer);
    });
    let _reset = Reset;
    run()
}

// Read-only observation of the independent combined lease.
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
thread_local! { static TEST_WAYLAND_CONTROL_OBSERVER: std::cell::RefCell<Option<Arc<std::sync::Mutex<Option<u64>>>>> = const { std::cell::RefCell::new(None) }; }
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
pub(crate) fn with_wayland_control_observer<T>(
    observer: Arc<std::sync::Mutex<Option<u64>>>,
    run: impl FnOnce() -> T,
) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_WAYLAND_CONTROL_OBSERVER.with(|slot| {
                slot.borrow_mut().take();
            });
        }
    }
    TEST_WAYLAND_CONTROL_OBSERVER.with(|slot| {
        assert!(slot.borrow().is_none());
        *slot.borrow_mut() = Some(observer);
    });
    let _reset = Reset;
    run()
}

/// Observes only the production client's candidate, local held bit and action
/// sequence. It cannot create a candidate, presentation acknowledgment or grant.
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
type PointerCandidateObservation = Arc<std::sync::Mutex<(Option<u64>, bool, u64)>>;
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
thread_local! { static TEST_WAYLAND_POINTER_CANDIDATE_OBSERVER: std::cell::RefCell<Option<PointerCandidateObservation>> = const { std::cell::RefCell::new(None) }; }
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
pub(crate) fn with_wayland_pointer_candidate_observer<T>(
    observer: PointerCandidateObservation,
    run: impl FnOnce() -> T,
) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_WAYLAND_POINTER_CANDIDATE_OBSERVER.with(|slot| {
                slot.borrow_mut().take();
            });
        }
    }
    TEST_WAYLAND_POINTER_CANDIDATE_OBSERVER.with(|slot| {
        assert!(slot.borrow().is_none());
        *slot.borrow_mut() = Some(observer);
    });
    let _reset = Reset;
    run()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn seamless_input_requires_explicit_x11_control_at_every_entry_point() {
        for backend in [ViewerBackend::X11, ViewerBackend::Wayland] {
            for seamless in [false, true] {
                for grab in [false, true] {
                    for view_only in [false, true] {
                        assert_eq!(
                            backend
                                .validate_seamless_input(seamless, grab, view_only)
                                .is_ok(),
                            !seamless || (backend == ViewerBackend::X11 && grab && !view_only),
                            "{backend:?}, seamless={seamless}, grab={grab}, view_only={view_only}",
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn wayland_initial_size_is_shared_by_hello_and_window() {
        assert_eq!(wayland_initial_size(None).dimensions(), (800, 600));
        let explicit = super::super::window_size::WindowSize {
            width: 640,
            height: 480,
        };
        assert_eq!(wayland_initial_size(Some(explicit)), explicit);
    }
}
