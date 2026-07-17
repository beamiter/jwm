from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def replace_once(path: str, old: str, new: str) -> None:
    file_path = ROOT / path
    data = file_path.read_text(encoding="utf-8")
    count = data.count(old)
    if count != 1:
        raise RuntimeError(f"{path}: expected one match, found {count}")
    file_path.write_text(data.replace(old, new, 1), encoding="utf-8")


replace_once(
    "src/backend/api.rs",
    """    fn get_pointer_position(&self) -> Result<(f64, f64), BackendError>;

    fn grab_pointer(&self, mask: u32, cursor: Option<u64>) -> Result<bool, BackendError>;
""",
    """    fn get_pointer_position(&self) -> Result<(f64, f64), BackendError>;

    /// Return the top-level window directly under the pointer when the backend
    /// can query it independently of the currently grabbed event target.
    ///
    /// X11 active grabs report the grab window as the event target, so modal
    /// capture source selection uses this hook to recover the actual child.
    fn window_under_pointer(&self) -> Result<Option<WindowId>, BackendError> {
        Ok(None)
    }

    fn grab_pointer(&self, mask: u32, cursor: Option<u64>) -> Result<bool, BackendError>;
""",
)

replace_once(
    "src/backend/x11rb/backend.rs",
    """            env::var("JWM_DEBUG_DRAG")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(true)
""",
    """            env::var("JWM_DEBUG_DRAG")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false)
""",
)
replace_once(
    "src/backend/x11rb/backend.rs",
    """        fn hit_target_from_event_window(&self, event_window: u32) -> HitTarget {
            if event_window == self.root_x11 || self.overlay_x11 == Some(event_window) {
                HitTarget::Background { output: None }
            } else {
                HitTarget::Surface(self.ids.intern(event_window))
            }
        }
""",
    """        fn hit_target_from_event_window(&self, event_window: u32) -> HitTarget {
            if event_window == self.root_x11 || self.overlay_x11 == Some(event_window) {
                HitTarget::Background { output: None }
            } else {
                HitTarget::Surface(self.ids.intern(event_window))
            }
        }

        fn hit_target_from_pointer_event(&self, event_window: u32, child: u32) -> HitTarget {
            if (event_window == self.root_x11 || self.overlay_x11 == Some(event_window))
                && child != 0
                && self.overlay_x11 != Some(child)
            {
                HitTarget::Surface(self.ids.intern(child))
            } else {
                self.hit_target_from_event_window(event_window)
            }
        }
""",
)
replace_once(
    "src/backend/x11rb/backend.rs",
    """                    log::info!(
                        "[X11] ButtonPress: event=0x{:x} child=0x{:x} root=0x{:x} root_xy=({},{}) detail={}",
                        e.event,
                        e.child,
                        self.root_x11,
                        e.root_x,
                        e.root_y,
                        e.detail
                    );
                    Some(BackendEvent::ButtonPress {
                        target: self.hit_target_from_event_window(e.event),
""",
    """                    log::debug!(
                        "[X11] ButtonPress: event=0x{:x} child=0x{:x} root=0x{:x} root_xy=({},{}) detail={}",
                        e.event,
                        e.child,
                        self.root_x11,
                        e.root_x,
                        e.root_y,
                        e.detail
                    );
                    Some(BackendEvent::ButtonPress {
                        target: self.hit_target_from_pointer_event(e.event, e.child),
""",
)
replace_once(
    "src/backend/x11rb/backend.rs",
    """                XEvent::MotionNotify(e) => Some(BackendEvent::MotionNotify {
                    target: self.hit_target_from_event_window(e.event),
""",
    """                XEvent::MotionNotify(e) => Some(BackendEvent::MotionNotify {
                    target: self.hit_target_from_pointer_event(e.event, e.child),
""",
)
replace_once(
    "src/backend/x11rb/backend.rs",
    """                XEvent::ButtonRelease(e) => Some(BackendEvent::ButtonRelease {
                    target: self.hit_target_from_event_window(e.event),
""",
    """                XEvent::ButtonRelease(e) => Some(BackendEvent::ButtonRelease {
                    target: self.hit_target_from_pointer_event(e.event, e.child),
""",
)
replace_once(
    "src/backend/x11rb/backend.rs",
    """        fn get_pointer_position(&self) -> Result<(f64, f64), BackendError> {
            let reply = self.query_pointer()?;
            // X11  f64
            Ok((reply.root_x as f64, reply.root_y as f64))
        }

        fn grab_pointer(&self, mask_bits: u32, cursor: Option<u64>) -> Result<bool, BackendError> {
""",
    """        fn get_pointer_position(&self) -> Result<(f64, f64), BackendError> {
            let reply = self.query_pointer()?;
            // X11  f64
            Ok((reply.root_x as f64, reply.root_y as f64))
        }

        fn window_under_pointer(&self) -> Result<Option<WindowId>, BackendError> {
            let reply = self.query_pointer()?;
            Ok((reply.child != 0).then(|| self.ids.intern(reply.child)))
        }

        fn grab_pointer(&self, mask_bits: u32, cursor: Option<u64>) -> Result<bool, BackendError> {
""",
)

replace_once(
    "src/backend/xcb/backend.rs",
    """            env::var("JWM_DEBUG_DRAG")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(true)
""",
    """            env::var("JWM_DEBUG_DRAG")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false)
""",
)
replace_once(
    "src/backend/xcb/backend.rs",
    """    fn get_pointer_position(&self) -> XcbResult<(f64, f64)> {
        let (x, y, _, _) = self.query_pointer_root()?;
        Ok((x as f64, y as f64))
    }

    fn grab_pointer(&self, mask: u32, cursor: Option<u64>) -> XcbResult<bool> {
""",
    """    fn get_pointer_position(&self) -> XcbResult<(f64, f64)> {
        let (x, y, _, _) = self.query_pointer_root()?;
        Ok((x as f64, y as f64))
    }

    fn window_under_pointer(&self) -> XcbResult<Option<WindowId>> {
        let cookie = self
            .conn
            .send_request(&x::QueryPointer { window: self.root });
        let reply = self.conn.wait_for_reply(cookie).map_err(xcb_err)?;
        let child = reply.child();
        Ok((child != x::WINDOW_NONE).then(|| self.ids.intern(child)))
    }

    fn grab_pointer(&self, mask: u32, cursor: Option<u64>) -> XcbResult<bool> {
""",
)

replace_once(
    "src/jwm/features/capture.rs",
    """#[derive(Debug, Default)]
pub struct CaptureInteractionState {
    pub screenshot: CaptureTarget,
    pub recording: CaptureTarget,
}
""",
    """#[derive(Debug, Default)]
pub struct CaptureInteractionState {
    pub screenshot: CaptureTarget,
    pub recording: CaptureTarget,
    swallow_button_release: bool,
}

impl CaptureInteractionState {
    pub(crate) fn swallow_next_button_release(&mut self) {
        self.swallow_button_release = true;
    }

    pub(crate) fn take_swallowed_button_release(&mut self) -> bool {
        std::mem::take(&mut self.swallow_button_release)
    }
}
""",
)
replace_once(
    "src/jwm/features/capture.rs",
    """    fn window_capture_rect(&self, backend: &mut dyn Backend, hit: HitTarget) -> Option<Rect> {
        let HitTarget::Surface(window) = hit else {
            return None;
        };
""",
    """    fn window_capture_rect(&self, backend: &mut dyn Backend, hit: HitTarget) -> Option<Rect> {
        let window = match hit {
            HitTarget::Surface(window) => Some(window),
            HitTarget::Background { .. } => backend
                .input_ops()
                .window_under_pointer()
                .ok()
                .flatten(),
        }?;
""",
)
replace_once(
    "src/jwm/features/capture.rs",
    """    #[test]
    fn intersection_clips_partially_visible_windows() {
""",
    """    #[test]
    fn swallowed_button_release_is_one_shot() {
        let mut state = CaptureInteractionState::default();
        assert!(!state.take_swallowed_button_release());
        state.swallow_next_button_release();
        assert!(state.take_swallowed_button_release());
        assert!(!state.take_swallowed_button_release());
    }

    #[test]
    fn intersection_clips_partially_visible_windows() {
""",
)

replace_once(
    "src/jwm/input_handler.rs",
    """            } else {
                self.cancel_recording_region_interaction(backend);
            }
""",
    """            } else {
                self.features.capture.swallow_next_button_release();
                self.cancel_recording_region_interaction(backend);
            }
""",
)
replace_once(
    "src/jwm/input_handler.rs",
    """            } else {
                // Right-click or other button → cancel
                self.cancel_screenshot_select(backend);
            }
""",
    """            } else {
                // Right-click or other button → cancel without leaking the release.
                self.features.capture.swallow_next_button_release();
                self.cancel_screenshot_select(backend);
            }
""",
)

replace_once(
    "src/jwm/event_dispatcher.rs",
    """    fn on_button_release(&mut self, backend: &mut dyn Backend, _target: HitTarget, _time: u32) {
        if self.features.system_ui.is_active() {
""",
    """    fn on_button_release(&mut self, backend: &mut dyn Backend, _target: HitTarget, _time: u32) {
        if self.features.capture.take_swallowed_button_release() {
            return;
        }
        if self.features.system_ui.is_active() {
""",
)

print("Applied capture hit-testing and modal input fixes")
