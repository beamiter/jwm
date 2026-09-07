use crate::backend::api::*;
use crate::backend::common_define::*;
use crate::backend::error::BackendError;
// 移除了 use std::any::Any;

// ------------------------------------------------------------------
// 空的 WindowOps
// ------------------------------------------------------------------
pub struct DummyWindowOps;
impl WindowOps for DummyWindowOps {
    // ... 代码内容保持不变，只是为了去除 warning ...
    fn set_position(&self, _win: WindowId, _x: i32, _y: i32) -> Result<(), BackendError> {
        Ok(())
    }
    fn configure(
        &self,
        _win: WindowId,
        _x: i32,
        _y: i32,
        _w: u32,
        _h: u32,
        _border: u32,
    ) -> Result<(), BackendError> {
        Ok(())
    }
    fn set_decoration_style(
        &self,
        _win: WindowId,
        _border_width: u32,
        _border_color: Pixel,
    ) -> Result<(), BackendError> {
        Ok(())
    }
    fn raise_window(&self, _win: WindowId) -> Result<(), BackendError> {
        Ok(())
    }
    fn map_window(&self, _win: WindowId) -> Result<(), BackendError> {
        Ok(())
    }
    fn unmap_window(&self, _win: WindowId) -> Result<(), BackendError> {
        Ok(())
    }
    fn close_window(&self, _win: WindowId) -> Result<CloseResult, BackendError> {
        Ok(CloseResult::Graceful)
    }
    fn set_input_focus(&self, _win: WindowId) -> Result<(), BackendError> {
        Ok(())
    }
    fn set_input_focus_root(&self) -> Result<(), BackendError> {
        Ok(())
    }
    fn get_window_attributes(&self, _win: WindowId) -> Result<WindowAttributes, BackendError> {
        Ok(WindowAttributes {
            override_redirect: false,
            map_state_viewable: true,
        })
    }
    fn get_geometry(&self, _win: WindowId) -> Result<Geometry, BackendError> {
        Ok(Geometry::default())
    }
    fn scan_windows(&self) -> Result<Vec<WindowId>, BackendError> {
        Ok(vec![])
    }
    fn flush(&self) -> Result<(), BackendError> {
        Ok(())
    }
    fn kill_client(&self, _win: WindowId) -> Result<(), BackendError> {
        Ok(())
    }
    fn apply_window_changes(
        &self,
        _win: WindowId,
        _changes: WindowChanges,
    ) -> Result<(), BackendError> {
        Ok(())
    }
}

// ... InputOps, PropertyOps, OutputOps, KeyOps, CursorProvider, ColorAllocator 保持不变
// 只要确保没有引用 std::any::Any 即可
pub struct DummyInputOps;
impl InputOps for DummyInputOps {
    fn set_cursor(&self, _kind: StdCursorKind) -> Result<(), BackendError> {
        Ok(())
    }
    fn get_pointer_position(&self) -> Result<(f64, f64), BackendError> {
        Ok((0.0, 0.0))
    }
    fn grab_pointer(&self, _mask: u32, _cursor: Option<u64>) -> Result<bool, BackendError> {
        Ok(true)
    }
    fn ungrab_pointer(&self) -> Result<(), BackendError> {
        Ok(())
    }
    fn query_pointer_root(&self) -> Result<(i32, i32, u16, u16), BackendError> {
        Ok((0, 0, 0, 0))
    }
}

/// Pointer/modifier snapshot a nested backend publishes for
/// [`SharedInputOps`]: root-relative pointer position and the modifier mask
/// as JWM `Mods` bits (what `UdevKeyOps::clean_mods` decodes).
///
/// The pointer is `f64` because that is what the nested backends accumulate
/// and what `InputOps::get_pointer_position` hands back — `UdevInputOps`
/// answers from the identical pair of fields, and only `query_pointer_root`
/// truncates.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SharedInputSnapshot {
    pub pointer_x: f64,
    pub pointer_y: f64,
    pub mods: u16,
}

/// Input ops for the nested (X11 / winit) Wayland backends. `DummyInputOps`
/// answers `query_pointer_root` with a zero modifier mask, which the window
/// switcher reads as "nothing held": the panel is built and committed in the
/// same call, so Alt+Tab can never be held nested. The nested backends track
/// the modifier state in their shared state already; this type lets them
/// expose it through the same hook the udev backend answers from.
pub struct SharedInputOps {
    snapshot: Box<dyn Fn() -> SharedInputSnapshot + Send + Sync>,
}

impl SharedInputOps {
    pub fn new(snapshot: impl Fn() -> SharedInputSnapshot + Send + Sync + 'static) -> Self {
        Self {
            snapshot: Box::new(snapshot),
        }
    }
}

impl InputOps for SharedInputOps {
    fn set_cursor(&self, _kind: StdCursorKind) -> Result<(), BackendError> {
        Ok(())
    }
    fn get_pointer_position(&self) -> Result<(f64, f64), BackendError> {
        let snapshot = (self.snapshot)();
        Ok((snapshot.pointer_x, snapshot.pointer_y))
    }
    fn grab_pointer(&self, _mask: u32, _cursor: Option<u64>) -> Result<bool, BackendError> {
        Ok(true)
    }
    fn ungrab_pointer(&self) -> Result<(), BackendError> {
        Ok(())
    }
    fn query_pointer_root(&self) -> Result<(i32, i32, u16, u16), BackendError> {
        let snapshot = (self.snapshot)();
        Ok((
            snapshot.pointer_x as i32,
            snapshot.pointer_y as i32,
            snapshot.mods,
            0,
        ))
    }
}

/// Whether a key event has to be mirrored to the WM as a `KeyRelease`.
///
/// The nested (X11 / winit) backends forward only *presses*, and only of the
/// keys that match a WM binding — enough for every shortcut except the one
/// gesture that ends on a key going up. While a system-UI overlay is on
/// screen it owns the keyboard the way an X11 keyboard grab would, and
/// `Jwm::on_key_release` is the only path that can commit the held-modifier
/// window switcher, so releases have to reach the WM for as long as the
/// overlay is up — including the release of a key whose press predates it,
/// which is exactly the case for the Alt that opened the switcher.
pub fn mirrors_key_release_to_wm(pressed: bool, system_ui_grab_active: bool) -> bool {
    !pressed && system_ui_grab_active
}

/// Whole vertical wheel clicks accumulated from one axis event, as a signed
/// count plus the fractional remainder to carry into the next event.
///
/// libinput reports wheel rotation in two units: `v120` (120 per click) for
/// wheel devices — high-resolution wheels report a fraction per event — and,
/// for smooth/touchpad scrolling, a pixel-style amount taken here at 15 per
/// click. Both share the cursor's coordinate system, where positive means
/// down (libinput.h: "the positive direction being down or right"), so a
/// negative count is X11's button 4 (up) and a positive one button 5 (down).
/// The remainder is what lets a touchpad's stream of tiny events add up to
/// whole clicks instead of each rounding to zero on its own.
pub fn wheel_steps(accumulator: f64, amount_v120: Option<f64>, amount: Option<f64>) -> (i32, f64) {
    let clicks = match amount_v120 {
        Some(v120) => v120 / 120.0,
        None => amount.unwrap_or(0.0) / 15.0,
    };
    let total = accumulator + clicks;
    let whole = total.trunc() as i32;
    (whole, total - f64::from(whole))
}

/// The X11 button detail a signed click count from [`wheel_steps`] reports:
/// 4 for up, 5 for down — the numbers every WM wheel branch matches on.
pub fn wheel_click_detail(clicks: i32) -> u8 {
    if clicks < 0 { 4 } else { 5 }
}

pub struct DummyPropertyOps;
impl PropertyOps for DummyPropertyOps {
    fn get_title(&self, _win: WindowId) -> String {
        "Wayland Window".to_string()
    }
    fn get_class(&self, _win: WindowId) -> (String, String) {
        ("app".into(), "App".into())
    }
    fn get_window_types(&self, _win: WindowId) -> Vec<WindowType> {
        vec![WindowType::Normal]
    }
    fn is_fullscreen(&self, _win: WindowId) -> bool {
        false
    }
    fn set_fullscreen_state(&self, _win: WindowId, _on: bool) -> Result<(), BackendError> {
        Ok(())
    }
    fn transient_for(&self, _win: WindowId) -> Option<WindowId> {
        None
    }
    fn get_wm_hints(&self, _win: WindowId) -> Option<WmHints> {
        None
    }
    fn set_urgent_hint(&self, _win: WindowId, _urgent: bool) -> Result<(), BackendError> {
        Ok(())
    }
    fn fetch_normal_hints(&self, _win: WindowId) -> Result<Option<NormalHints>, BackendError> {
        Ok(None)
    }
    fn set_window_strut_top(
        &self,
        _win: WindowId,
        _top: u32,
        _sx: u32,
        _ex: u32,
    ) -> Result<(), BackendError> {
        Ok(())
    }
    fn set_window_type_dock(&self, _win: WindowId) -> Result<(), BackendError> {
        Ok(())
    }
    fn clear_window_strut(&self, _win: WindowId) -> Result<(), BackendError> {
        Ok(())
    }
    fn get_wm_state(&self, _win: WindowId) -> Result<i64, BackendError> {
        Ok(1)
    }
    fn set_wm_state(&self, _win: WindowId, _state: i64) -> Result<(), BackendError> {
        Ok(())
    }
    fn set_client_info_props(
        &self,
        _win: WindowId,
        _tags: u32,
        _monitor_num: u32,
    ) -> Result<(), BackendError> {
        Ok(())
    }
}

pub struct DummyOutputOps;
impl OutputOps for DummyOutputOps {
    fn enumerate_outputs(&self) -> Vec<OutputInfo> {
        vec![OutputInfo {
            id: crate::backend::common_define::OutputId(0),
            name: "Virtual-1".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            scale: 1.0,
            refresh_rate: 60000,
            hdr_capable: true,
            hdr_metadata: None,
            identity: crate::backend::api::OutputIdentity::connector_only("Virtual-1"),
        }]
    }
    fn screen_info(&self) -> ScreenInfo {
        ScreenInfo {
            width: 1920,
            height: 1080,
        }
    }
    fn output_at(&self, _x: i32, _y: i32) -> Option<crate::backend::common_define::OutputId> {
        Some(crate::backend::common_define::OutputId(0))
    }
}

pub struct DummyKeyOps;
impl KeyOps for DummyKeyOps {
    fn grab_keys(&self, _root: WindowId, _bindings: &[(Mods, KeySym)]) -> Result<(), BackendError> {
        Ok(())
    }
    fn clear_key_grabs(&self, _root: WindowId) -> Result<(), BackendError> {
        Ok(())
    }
    fn clean_mods(&self, _raw: u16) -> Mods {
        Mods::empty()
    }
    fn keysym_from_keycode(&mut self, keycode: u8) -> Result<KeySym, BackendError> {
        Ok(keycode as u32)
    }
    fn clear_cache(&mut self) {}
}

pub struct DummyCursorProvider;
impl CursorProvider for DummyCursorProvider {
    fn preload_common(&mut self) -> Result<(), BackendError> {
        Ok(())
    }
    fn get(&mut self, _kind: StdCursorKind) -> Result<CursorHandle, BackendError> {
        Ok(CursorHandle(0))
    }
    fn apply(&mut self, _win: WindowId, _kind: StdCursorKind) -> Result<(), BackendError> {
        Ok(())
    }
    fn cleanup(&mut self) -> Result<(), BackendError> {
        Ok(())
    }
}

pub struct DummyColorAllocator;
impl ColorAllocator for DummyColorAllocator {
    fn set_scheme(&mut self, _t: SchemeType, _s: ColorScheme) {}
    fn allocate_schemes_pixels(&mut self) -> Result<(), BackendError> {
        Ok(())
    }
    fn get_border_pixel_of(&mut self, _t: SchemeType) -> Result<Pixel, BackendError> {
        Ok(Pixel(0))
    }
    fn free_all_theme_pixels(&mut self) -> Result<(), BackendError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DummyInputOps, SharedInputOps, SharedInputSnapshot, mirrors_key_release_to_wm,
        wheel_click_detail, wheel_steps,
    };
    use crate::backend::api::InputOps;
    use crate::backend::common_define::Mods;
    use std::sync::{Arc, Mutex};

    fn ops_over(shared: &Arc<Mutex<SharedInputSnapshot>>) -> SharedInputOps {
        SharedInputOps::new({
            let shared = Arc::clone(shared);
            move || *shared.lock().unwrap()
        })
    }

    #[test]
    fn shared_input_ops_report_the_backend_modifier_state() {
        let shared = Arc::new(Mutex::new(SharedInputSnapshot::default()));
        let ops = ops_over(&shared);
        assert_eq!(ops.query_pointer_root().unwrap(), (0, 0, 0, 0));

        *shared.lock().unwrap() = SharedInputSnapshot {
            pointer_x: 30.5,
            pointer_y: 40.5,
            mods: (Mods::ALT | Mods::SHIFT).bits(),
        };
        let (x, y, mask, _) = ops.query_pointer_root().unwrap();
        // `query_pointer_root` is the integer root query, `UdevInputOps`
        // truncates the same way, and the sub-pixel position survives on the
        // float accessor the pointer paths use.
        assert_eq!((x, y), (30, 40));
        assert_eq!(Mods::from_bits_truncate(mask), Mods::ALT | Mods::SHIFT);
        assert_eq!(ops.get_pointer_position().unwrap(), (30.5, 40.5));
    }

    #[test]
    fn shared_input_ops_mask_survives_what_the_window_switcher_does_to_it() {
        // The hold-style switcher decides whether to wait for a key release
        // from `clean_mods(mask) & release_commit_mods()`, and `clean_mods` on
        // the Smithay backends is `Mods::from_bits_truncate`. This side owns
        // only the delivery: an Alt+Shift+Tab gesture must arrive with both
        // bits intact, so the policy sees ALT held. Which modifiers commit is
        // the WM's half, pinned by `features::switcher`'s own tests; a
        // backend must not reach into policy to answer a delivery question.
        let shared = Arc::new(Mutex::new(SharedInputSnapshot {
            pointer_x: 0.0,
            pointer_y: 0.0,
            mods: (Mods::ALT | Mods::SHIFT).bits(),
        }));
        let ops = ops_over(&shared);
        let (_, _, mask, _) = ops.query_pointer_root().unwrap();
        let delivered = Mods::from_bits_truncate(mask);
        assert_eq!(delivered, Mods::ALT | Mods::SHIFT);
        assert!(
            delivered.contains(Mods::ALT),
            "an empty set takes the tap-commit branch, so the panel never holds"
        );
    }

    #[test]
    fn dummy_input_ops_still_report_nothing_held() {
        // The placeholder keeps its historical answer; backends that want the
        // switcher to see a held modifier must install `SharedInputOps`.
        assert_eq!(DummyInputOps.query_pointer_root().unwrap(), (0, 0, 0, 0));
    }

    /// Neither nested backend can be constructed in a unit test — each needs
    /// a live X11 / winit session — so the two halves the switcher depends on
    /// are pinned against their source instead: the constructor must end up
    /// installing [`SharedInputOps`] rather than leaving the zero-mask
    /// placeholder in place, and the input handler must queue key releases.
    #[cfg(feature = "backend-wayland-nested")]
    #[test]
    fn the_nested_backends_publish_the_mask_and_queue_key_releases() {
        // Needles are assembled here so this test cannot pass by matching
        // itself, and each haystack is narrowed to the region that has to
        // carry it.
        let field = format!("input{}ops", "_");
        let placeholder = format!("Dummy{}Ops", "Input");
        let live = format!("Shared{}Ops::new", "Input");
        let gate = format!("mirrors_key_{}_to_wm", "release");
        let release = format!("BackendEvent::Key{}", "Release");

        for (name, source) in [
            ("wayland_x11", include_str!("wayland_x11/backend.rs")),
            ("wayland_winit", include_str!("wayland_winit/backend.rs")),
        ] {
            let start = source
                .find("let mut backend = Self {")
                .unwrap_or_else(|| panic!("{name}: no constructor to scan"));
            let end = start
                + source[start..]
                    .find("Ok(backend)")
                    .unwrap_or_else(|| panic!("{name}: the constructor never returns"));
            let installed = source[start..end]
                .rfind(&format!("{field} = "))
                .map(|at| &source[start + at..end])
                .unwrap_or_else(|| panic!("{name}: the constructor never re-installs {field}"));
            assert!(
                installed.contains(&live),
                "{name}: {field} must end up answering from the shared state"
            );
            assert!(
                !installed.contains(&placeholder),
                "{name}: {field} is left on the zero-mask placeholder, which reads to \
                 the window switcher as nothing held"
            );

            // The handler is a free function, so its body ends at the first
            // closing brace in column 0. Stopping there matters: everything
            // after it is `impl` blocks that also mention `KeyRelease`, and a
            // haystack running to the end of the file would keep passing
            // after the mirror was moved out of the handler that has to make
            // it — the drift this test exists to catch.
            let handler_at = source
                .find("fn process_input_event_windowed")
                .unwrap_or_else(|| panic!("{name}: no input handler to scan"));
            let handler_end = source[handler_at..]
                .find("\n}\n")
                .unwrap_or_else(|| panic!("{name}: the input handler never closes"));
            let handler = &source[handler_at..handler_at + handler_end];
            assert!(
                handler.contains(&gate) && handler.contains(&release),
                "{name}: releases never reach the WM, so a held modifier can never come up"
            );

            // The wheel is the other input the host compositor only delivers
            // as an axis event: without this arm it reached neither the
            // clients nor, during a panel grab, the WM.
            let axis_arm = format!("InputEvent::Pointer{}", "Axis");
            let steps = format!("wheel_{}", "steps");
            assert!(
                handler.contains(&axis_arm) && handler.contains(&steps),
                "{name}: the wheel reaches nobody — the client gets no axis \
                 frame and a panel grab gets no button 4/5"
            );
        }
    }

    #[test]
    fn key_releases_reach_the_wm_only_while_an_overlay_is_up() {
        assert!(mirrors_key_release_to_wm(false, true));
        // No overlay: the WM ignores releases anyway, so do not queue them.
        assert!(!mirrors_key_release_to_wm(false, false));
        // Presses keep their own path, overlay or not.
        assert!(!mirrors_key_release_to_wm(true, true));
        assert!(!mirrors_key_release_to_wm(true, false));
    }

    #[test]
    fn wheel_v120_maps_whole_clicks_to_x11_details() {
        // 120 v120 units per click, up being negative — libinput shares the
        // cursor's coordinates, where positive is down — so -120 is one X11
        // button-4 press and +120 one button-5 press.
        assert_eq!(wheel_steps(0.0, Some(-120.0), None), (-1, 0.0));
        assert_eq!(wheel_steps(0.0, Some(120.0), None), (1, 0.0));
        assert_eq!(wheel_steps(0.0, Some(-240.0), None), (-2, 0.0));
        assert_eq!(wheel_click_detail(-2), 4);
        assert_eq!(wheel_click_detail(-1), 4);
        assert_eq!(wheel_click_detail(1), 5);
        assert_eq!(wheel_click_detail(3), 5);
        // The v120 unit wins over the pixel amount when both are present.
        assert_eq!(wheel_steps(0.0, Some(-120.0), Some(-1.0)), (-1, 0.0));
        // No rotation on the vertical axis emits nothing.
        assert_eq!(wheel_steps(0.0, None, None), (0, 0.0));
    }

    #[test]
    fn wheel_amount_fallback_clicks_at_fifteen_per_click() {
        // Exactly one click, just under it, just over it.
        assert_eq!(wheel_steps(0.0, None, Some(-15.0)), (-1, 0.0));
        let (clicks, remainder) = wheel_steps(0.0, None, Some(-14.0));
        assert_eq!(clicks, 0);
        assert!((remainder - (-14.0 / 15.0)).abs() < 1e-12);
        let (clicks, remainder) = wheel_steps(0.0, None, Some(-16.0));
        assert_eq!(clicks, -1);
        assert!((remainder - (-1.0 / 15.0)).abs() < 1e-12);
    }

    #[test]
    fn wheel_remainder_carries_across_events() {
        // Two half-click touchpad events add up to one click instead of each
        // rounding to zero on its own.
        let (clicks, remainder) = wheel_steps(0.0, Some(-60.0), None);
        assert_eq!((clicks, remainder), (0, -0.5));
        let (clicks, remainder) = wheel_steps(remainder, Some(-60.0), None);
        assert_eq!((clicks, remainder), (-1, 0.0));
        // A direction reversal spends the carried half-click first.
        let (clicks, remainder) = wheel_steps(-0.5, Some(120.0), None);
        assert_eq!((clicks, remainder), (0, 0.5));
    }
}
