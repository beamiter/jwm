use crate::backend::wayland::state::JwmWaylandState;
use crate::backend::wayland_dummy_ops::*;
use crate::backend::wayland_key_ops::UdevKeyOps;
use crate::sync_ext::MutexExt;

#[path = "../udev_kms.rs"]
mod kms;
use self::kms::KmsState;
use super::compositor::WaylandCompositor;
use crate::backend::api::{
    Backend, BackendDiagnostics, BackendEvent, Capabilities, ColorAllocator, CompositorAnnotation,
    CompositorBenchmark, CompositorControl, CompositorMedia, CompositorWindowEffects,
    CompositorWorkspaceEffects, CursorProvider, EventHandler, HitTarget, InputOps, KeyOps,
    DisplayControl, OutputInfo, OutputOps, PropertyOps, ResizeEdge, ScreenInfo, WindowOps,
    RenderScheduler, WindowType,
};
use crate::backend::common_define::{KeySym, Mods, OutputId, StdCursorKind, WindowId};
use crate::backend::error::BackendError;
use crate::config::CONFIG;

use std::any::Any;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use drm::control::{Device as ControlDevice, ModeTypeFlags, connector};

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::surface::render_elements_from_surface_tree;
use smithay::backend::renderer::gles::GlesTexture;
use smithay::backend::renderer::utils::{RendererSurfaceStateUserData, import_surface_tree};
use smithay::backend::renderer::{
    Bind, BufferType, Color32F, Offscreen, Renderer, Texture, buffer_has_alpha, buffer_type,
};
use smithay::utils::{Physical, Rectangle, Scale, Size, Transform};
use smithay::wayland::compositor::{
    RectangleKind, RegionAttributes, SurfaceAttributes, get_children, with_states,
};
use smithay::wayland::shell::xdg::SurfaceCachedState;
use smithay::wayland::viewporter::ViewportCachedState;

use smithay::backend::drm::{DrmNode, NodeType};
use smithay::backend::input::{
    AbsolutePositionEvent, Axis, Event as InputEventExt,
    GestureBeginEvent as GestureBeginEventTrait, GestureEndEvent as GestureEndEventTrait,
    GesturePinchUpdateEvent as GesturePinchUpdateEventTrait,
    GestureSwipeUpdateEvent as GestureSwipeUpdateEventTrait, InputEvent, KeyboardKeyEvent,
    PointerAxisEvent, PointerButtonEvent, PointerMotionEvent, TouchEvent as TouchEventTrait,
};
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::session::Event as SessionEvent;
use smithay::backend::session::Session;
use smithay::backend::session::libseat::{LibSeatSession, LibSeatSessionNotifier};
use smithay::backend::udev::{UdevBackend as SmithayUdevBackend, UdevEvent, primary_gpu};
use smithay::desktop::layer_map_for_output;
use smithay::desktop::utils::bbox_from_surface_tree;
use smithay::input::keyboard::{FilterResult, ModifiersState};
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent, RelativeMotionEvent};
use smithay::reexports::calloop::channel::{self, Sender};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{EventLoop, Interest, Mode, PostAction};
use smithay::reexports::input::Libinput;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::{Display, DisplayHandle};
use smithay::utils::{Logical, Point, SERIAL_COUNTER as SCOUNTER};
use smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitorSeat;
use smithay::wayland::shell::wlr_layer::{KeyboardInteractivity, Layer as WlrLayer};
use smithay::xwayland::{X11Wm, XWayland, XWaylandEvent};

fn allowed_shortcut_mods() -> Mods {
    Mods::SHIFT | Mods::CONTROL | Mods::ALT | Mods::SUPER | Mods::MOD2 | Mods::MOD3 | Mods::MOD5
}

fn gesture_swipe_should_intercept(
    fingers: u32,
    bindings: &[crate::config::GestureSwipeConfig],
) -> bool {
    fingers >= 3 && bindings.iter().any(|binding| binding.fingers == fingers)
}

fn region_fully_covers_rect(region: &RegionAttributes, target: Rectangle<i32, Logical>) -> bool {
    if target.size.w <= 0 || target.size.h <= 0 {
        return false;
    }

    let mut covered = Vec::new();
    for &(kind, rect) in &region.rects {
        let Some(rect) = rect.intersection(target) else {
            continue;
        };
        match kind {
            RectangleKind::Add => covered.push(rect),
            RectangleKind::Subtract => {
                covered = Rectangle::subtract_rects_many_in_place(covered, [rect]);
            }
        }
    }

    target.subtract_rects(covered).is_empty()
}

fn surface_declares_opaque_rect(
    surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    target: Rectangle<i32, Logical>,
) -> bool {
    with_states(surface, |states| {
        let mut cached = states.cached_state.get::<SurfaceAttributes>();
        let current = cached.current();
        let Some(region) = current.opaque_region.as_ref() else {
            return false;
        };
        region_fully_covers_rect(region, target)
    })
}

// libinput/evdev does not generate key repeat events for us (X11 does).
// Emulate the common behavior for WM shortcuts.
const KEY_REPEAT_DELAY: Duration = Duration::from_millis(400);
const KEY_REPEAT_INTERVAL: Duration = Duration::from_millis(50);
const KEY_REPEAT_TICK: Duration = Duration::from_millis(16);

#[derive(Clone, Copy, Debug)]
struct RepeatState {
    keycode: u8,
    mods_raw: u16,
    required_mods: Mods,
    last_time: u32,
    next_fire: Instant,
}

#[derive(Clone, Copy, Debug)]
struct ShortcutBinding {
    mods: Mods,
    keysym: KeySym,
    repeatable: bool,
}

struct SendWrapper<T>(T);

unsafe impl<T> Send for SendWrapper<T> {}

impl<T> std::ops::Deref for SendWrapper<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> std::ops::DerefMut for SendWrapper<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

struct SharedState {
    pointer_x: f64,
    pointer_y: f64,
    mods_state: u16,
    cursor_kind: StdCursorKind,
    cursor_dirty: bool,
    /// Cached key bindings (mods, keysym) for key event suppression.
    key_bindings: Vec<ShortcutBinding>,
    /// xkb keycode (0..=255) -> base (unmodified) keysym.
    keysym_table: Vec<crate::backend::common_define::KeySym>,
    /// xkb keycodes that were intercepted on press and should be intercepted on release.
    suppressed_keycodes: HashSet<u8>,

    repeat: Option<RepeatState>,
    outputs: Vec<OutputInfo>,
    output_key_to_id: HashMap<u64, OutputId>,
    next_output_raw: u64,
    device_paths: HashMap<u64, PathBuf>,
    preferred_device_id: Option<u64>,

    kms_needs_reinit: bool,
    session_active: bool,

    /// True while the WM screenshot region-select grab is active.
    /// Suppresses pointer/keyboard events from reaching Wayland clients.
    screenshot_grab_active: bool,
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            pointer_x: 0.0,
            pointer_y: 0.0,
            mods_state: 0,
            cursor_kind: StdCursorKind::LeftPtr,
            cursor_dirty: false,
            key_bindings: Vec::new(),
            keysym_table: vec![0; 256],
            suppressed_keycodes: HashSet::new(),

            repeat: None,
            outputs: Vec::new(),
            output_key_to_id: HashMap::new(),
            next_output_raw: 0,
            device_paths: HashMap::new(),
            preferred_device_id: None,

            kms_needs_reinit: false,
            session_active: true,
            screenshot_grab_active: false,
        }
    }
}

struct UdevOutputOps {
    shared: Arc<Mutex<SharedState>>,
}

impl OutputOps for UdevOutputOps {
    fn enumerate_outputs(&self) -> Vec<OutputInfo> {
        self.shared.lock_safe().outputs.clone()
    }

    fn screen_info(&self) -> ScreenInfo {
        let shared = self.shared.lock_safe();
        let mut w = 0i32;
        let mut h = 0i32;
        for o in &shared.outputs {
            w = w.max(o.x + o.width);
            h = h.max(o.y + o.height);
        }
        if w == 0 {
            w = 1920;
        }
        if h == 0 {
            h = 1080;
        }
        ScreenInfo {
            width: w,
            height: h,
        }
    }

    fn output_at(&self, x: i32, y: i32) -> Option<OutputId> {
        let shared = self.shared.lock_safe();
        shared
            .outputs
            .iter()
            .find(|o| x >= o.x && y >= o.y && x < (o.x + o.width) && y < (o.y + o.height))
            .map(|o| o.id)
    }
}

struct UdevInputOps {
    shared: Arc<Mutex<SharedState>>,
}

impl InputOps for UdevInputOps {
    fn set_cursor(
        &self,
        kind: crate::backend::common_define::StdCursorKind,
    ) -> Result<(), BackendError> {
        let mut shared = self.shared.lock_safe();
        if shared.cursor_kind != kind {
            shared.cursor_kind = kind;
            shared.cursor_dirty = true;
        }
        Ok(())
    }

    fn get_pointer_position(&self) -> Result<(f64, f64), BackendError> {
        let shared = self.shared.lock_safe();
        Ok((shared.pointer_x, shared.pointer_y))
    }

    fn grab_pointer(&self, mask: u32, _cursor: Option<u64>) -> Result<bool, BackendError> {
        if mask != 0 {
            // Non-zero mask → screenshot region-select grab.
            // Suppress events reaching Wayland clients. Keep the current cursor:
            // changing to a cursor-theme bitmap here routes through Smithay's
            // cursor renderer immediately after our raw GLES frame, and some
            // drivers are sensitive to inherited VAO/VBO state during that path.
            let mut shared = self.shared.lock_safe();
            shared.screenshot_grab_active = true;
        }
        Ok(true)
    }

    fn ungrab_pointer(&self) -> Result<(), BackendError> {
        let mut shared = self.shared.lock_safe();
        shared.screenshot_grab_active = false;
        if shared.cursor_kind != StdCursorKind::LeftPtr {
            shared.cursor_kind = StdCursorKind::LeftPtr;
            shared.cursor_dirty = true;
        }
        Ok(())
    }

    fn query_pointer_root(&self) -> Result<(i32, i32, u16, u16), BackendError> {
        let shared = self.shared.lock_safe();
        Ok((
            shared.pointer_x as i32,
            shared.pointer_y as i32,
            shared.mods_state,
            0,
        ))
    }
}

struct WaylandWindowOps {
    state: SendWrapper<*mut JwmWaylandState>,

    flush_tx: Sender<()>,
    flush_pending: Arc<AtomicBool>,
}

unsafe impl Send for WaylandWindowOps {}

impl WaylandWindowOps {
    unsafe fn with_state_mut<R>(&self, f: impl FnOnce(&mut JwmWaylandState) -> R) -> R {
        // Safety: We only ever call this from the compositor thread.
        unsafe { f(&mut *self.state.0) }
    }

    fn request_flush(&self) {
        if !self.flush_pending.swap(true, Ordering::SeqCst) {
            let _ = self.flush_tx.send(());
        }
    }
}

impl WindowOps for WaylandWindowOps {
    fn set_position(&self, _win: WindowId, _x: i32, _y: i32) -> Result<(), BackendError> {
        unsafe {
            self.with_state_mut(|state| {
                if let Some(geo) = state.window_geometry.get_mut(&_win) {
                    let bw = geo.border as i32;
                    geo.x = _x + bw;
                    geo.y = _y + bw;
                }
                state.needs_redraw = true;
            });
        }
        self.request_flush();
        Ok(())
    }

    fn configure(
        &self,
        win: WindowId,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        border: u32,
    ) -> Result<(), BackendError> {
        unsafe {
            self.with_state_mut(|state| {
                state.pending_initial_configure.remove(&win);
                let bw = border as i32;
                state.window_geometry.insert(
                    win,
                    crate::backend::api::Geometry {
                        x: x + bw,
                        y: y + bw,
                        w,
                        h,
                        border,
                    },
                );
                let choose_natural_size =
                    state.is_dialog_like_toplevel(win) && w == 800 && h == 600;
                if let Some(toplevel) = state.try_lookup_toplevel(win) {
                    toplevel.with_pending_state(|s| {
                        if choose_natural_size {
                            s.size = None;
                            JwmWaylandState::set_toplevel_tiled_state(s, false);
                        } else {
                            s.size = Some((w as i32, h as i32).into());
                            JwmWaylandState::set_toplevel_tiled_state(s, true);
                        }
                    });
                    if toplevel.is_initial_configure_sent() {
                        toplevel.send_pending_configure();
                    } else {
                        toplevel.send_configure();
                    }
                }
                if let Some(x11) = state.x11_surfaces.get(&win) {
                    let bw = border as i32;
                    let _ = x11.configure(Some(smithay::utils::Rectangle::new(
                        (x + bw, y + bw).into(),
                        (w as i32, h as i32).into(),
                    )));
                }
                state.reconstrain_popups_for_toplevel(win);
                state.needs_redraw = true;
            });
        }
        self.request_flush();
        Ok(())
    }

    fn set_decoration_style(
        &self,
        win: WindowId,
        _border_width: u32,
        border_color: crate::backend::common_define::Pixel,
    ) -> Result<(), BackendError> {
        unsafe {
            self.with_state_mut(|state| {
                // Convert Pixel (packed ARGB u32) to [f32; 4] RGBA for the renderer.
                let raw = border_color.0;
                let a = ((raw >> 24) & 0xFF) as f32 / 255.0;
                let r = ((raw >> 16) & 0xFF) as f32 / 255.0;
                let g = ((raw >> 8) & 0xFF) as f32 / 255.0;
                let b = (raw & 0xFF) as f32 / 255.0;
                state.window_border_color.insert(win, [r, g, b, a]);
                state.needs_redraw = true;
            });
        }
        self.request_flush();
        Ok(())
    }

    fn raise_window(&self, _win: WindowId) -> Result<(), BackendError> {
        unsafe {
            self.with_state_mut(|state| {
                if let Some(pos) = state.window_stack.iter().position(|w| *w == _win) {
                    state.window_stack.remove(pos);
                    state.window_stack.push(_win);
                }
                state.needs_redraw = true;
            });
        }
        self.request_flush();
        Ok(())
    }
    fn map_window(&self, _win: WindowId) -> Result<(), BackendError> {
        Ok(())
    }
    fn unmap_window(&self, _win: WindowId) -> Result<(), BackendError> {
        Ok(())
    }
    fn close_window(
        &self,
        _win: WindowId,
    ) -> Result<crate::backend::api::CloseResult, BackendError> {
        unsafe {
            self.with_state_mut(|state| {
                if let Some(toplevel) = state.try_lookup_toplevel(_win) {
                    toplevel.send_close();
                } else if let Some(x11) = state.x11_surfaces.get(&_win) {
                    let _ = x11.close();
                }
            });
        }
        self.request_flush();
        Ok(crate::backend::api::CloseResult::Graceful)
    }
    fn set_input_focus(&self, _win: WindowId) -> Result<(), BackendError> {
        unsafe {
            self.with_state_mut(|state| {
                let serial = SCOUNTER.next_serial();
                if let Some(surface) = state.surface_for_window(_win) {
                    // Don't focus surfaces belonging to the IME client — doing so
                    // triggers deactivate_input_method and kills the candidate popup.
                    if let Some(ref im_id) = state.im_client_id {
                        if surface.id().same_client_as(im_id) {
                            return;
                        }
                    }
                    if let Some(kbd) = state.seat.get_keyboard() {
                        kbd.set_focus(state, Some(surface), serial);
                    }
                }
            });
        }
        self.request_flush();
        Ok(())
    }
    fn set_input_focus_root(&self) -> Result<(), BackendError> {
        unsafe {
            self.with_state_mut(|state| {
                let serial = SCOUNTER.next_serial();
                if let Some(kbd) = state.seat.get_keyboard() {
                    kbd.set_focus(state, None, serial);
                }
            });
        }
        self.request_flush();
        Ok(())
    }
    fn get_window_attributes(
        &self,
        _win: WindowId,
    ) -> Result<crate::backend::api::WindowAttributes, BackendError> {
        let (viewable, or) = unsafe {
            self.with_state_mut(|state| {
                let viewable = state.mapped_windows.contains(&_win);
                let or = state
                    .x11_surfaces
                    .get(&_win)
                    .map(|x| x.is_override_redirect())
                    .unwrap_or(false);
                (viewable, or)
            })
        };
        Ok(crate::backend::api::WindowAttributes {
            override_redirect: or,
            map_state_viewable: viewable,
        })
    }
    fn get_geometry(&self, _win: WindowId) -> Result<crate::backend::api::Geometry, BackendError> {
        let geo = unsafe { self.with_state_mut(|state| state.window_geometry.get(&_win).copied()) };
        let mut geo = geo.unwrap_or_default();
        let bw = geo.border as i32;
        geo.x = geo.x - bw;
        geo.y = geo.y - bw;
        Ok(geo)
    }
    fn scan_windows(&self) -> Result<Vec<WindowId>, BackendError> {
        let wins = unsafe { self.with_state_mut(|state| state.window_stack.clone()) };
        Ok(wins)
    }
    fn flush(&self) -> Result<(), BackendError> {
        self.request_flush();
        Ok(())
    }
    fn kill_client(&self, win: WindowId) -> Result<(), BackendError> {
        unsafe {
            self.with_state_mut(|state| {
                if let Some(toplevel) = state.try_lookup_toplevel(win) {
                    toplevel.send_close();
                } else if let Some(x11) = state.x11_surfaces.get(&win) {
                    let _ = x11.close();
                }
            });
        }
        self.request_flush();
        Ok(())
    }
    fn apply_window_changes(
        &self,
        _win: WindowId,
        _changes: crate::backend::api::WindowChanges,
    ) -> Result<(), BackendError> {
        let current = self.get_geometry(_win)?;
        let x = _changes.x.unwrap_or(current.x);
        let y = _changes.y.unwrap_or(current.y);
        let w = _changes.width.unwrap_or(current.w);
        let h = _changes.height.unwrap_or(current.h);
        let border = _changes.border_width.unwrap_or(current.border);
        self.configure(_win, x, y, w, h, border)
    }
}

struct WaylandPropertyOps {
    state: SendWrapper<*mut JwmWaylandState>,

    flush_tx: Sender<()>,
    flush_pending: Arc<AtomicBool>,
}

unsafe impl Send for WaylandPropertyOps {}

impl WaylandPropertyOps {
    unsafe fn with_state_mut<R>(&self, f: impl FnOnce(&mut JwmWaylandState) -> R) -> R {
        unsafe { f(&mut *self.state.0) }
    }

    fn request_flush(&self) {
        if !self.flush_pending.swap(true, Ordering::SeqCst) {
            let _ = self.flush_tx.send(());
        }
    }
}

impl PropertyOps for WaylandPropertyOps {
    fn get_title(&self, win: WindowId) -> String {
        unsafe { self.with_state_mut(|state| state.window_title.get(&win).cloned()) }
            .unwrap_or_else(|| "Wayland Window".to_string())
    }

    fn get_class(&self, win: WindowId) -> (String, String) {
        let app_id = unsafe { self.with_state_mut(|state| state.window_app_id.get(&win).cloned()) }
            .unwrap_or_else(|| "app".to_string());
        (app_id.clone(), app_id)
    }

    fn get_window_types(&self, win: WindowId) -> Vec<WindowType> {
        // Best-effort classification so JWM can treat status bars/docks correctly.
        // For layer-shell surfaces, exclusive_zone is the canonical hint.
        let (title, app_id, layer_info, is_dialog_like) = unsafe {
            self.with_state_mut(|state| {
                (
                    state.window_title.get(&win).cloned().unwrap_or_default(),
                    state.window_app_id.get(&win).cloned().unwrap_or_default(),
                    state.window_layer_info.get(&win).copied(),
                    state.is_dialog_like_toplevel(win),
                )
            })
        };

        if is_dialog_like {
            return vec![WindowType::Dialog];
        }

        if let Some(info) = layer_info {
            if info.exclusive_zone != 0 {
                return vec![WindowType::Dock];
            }
            let anchored =
                info.anchor_top || info.anchor_bottom || info.anchor_left || info.anchor_right;
            if info.anchor_top && info.anchor_bottom && info.anchor_left && info.anchor_right {
                return vec![WindowType::Desktop];
            }
            if anchored {
                return vec![WindowType::Notification];
            }
            return vec![WindowType::Dialog];
        }

        let cfg = crate::config::CONFIG.load();
        let bar_name = cfg.status_bar_name();
        if !bar_name.is_empty() && (app_id == bar_name || title == bar_name) {
            return vec![WindowType::Dock];
        }

        vec![WindowType::Normal]
    }

    fn get_layer_surface_info(
        &self,
        win: WindowId,
    ) -> Option<crate::backend::api::LayerSurfaceInfo> {
        unsafe { self.with_state_mut(|state| state.window_layer_info.get(&win).copied()) }
    }

    fn is_fullscreen(&self, win: WindowId) -> bool {
        unsafe { self.with_state_mut(|state| state.window_is_fullscreen.get(&win).copied()) }
            .unwrap_or(false)
    }

    fn set_fullscreen_state(&self, win: WindowId, on: bool) -> Result<(), BackendError> {
        unsafe {
            self.with_state_mut(|state| {
                state.window_is_fullscreen.insert(win, on);
                if let Some(toplevel) = state.try_lookup_toplevel(win) {
                    toplevel.with_pending_state(|s| {
                        if on {
                            JwmWaylandState::set_toplevel_tiled_state(s, false);
                            s.states.set(xdg_toplevel::State::Fullscreen);
                        } else {
                            s.states.unset(xdg_toplevel::State::Fullscreen);
                            s.fullscreen_output = None;
                        }
                    });
                    toplevel.send_configure();
                } else if let Some(x11) = state.x11_surfaces.get(&win) {
                    let _ = x11.set_fullscreen(on);
                }
            });
        }
        self.request_flush();
        Ok(())
    }

    fn transient_for(&self, _win: WindowId) -> Option<WindowId> {
        unsafe {
            self.with_state_mut(|state| {
                // Wayland xdg toplevel parent
                if let Some(toplevel) = state.toplevels.get(&_win) {
                    if let Some(parent_surface) = toplevel.parent() {
                        return state.surface_to_window.get(&parent_surface.id()).copied();
                    }
                }
                // X11 transient_for – not directly exposed via Smithay X11Surface API,
                // but X11 windows rarely need this in JWM's tiling model.
                None
            })
        }
    }

    fn get_wm_hints(&self, _win: WindowId) -> Option<crate::backend::api::WmHints> {
        None
    }

    fn set_urgent_hint(&self, _win: WindowId, _urgent: bool) -> Result<(), BackendError> {
        Ok(())
    }

    fn fetch_normal_hints(
        &self,
        _win: WindowId,
    ) -> Result<Option<crate::backend::api::NormalHints>, BackendError> {
        Ok(None)
    }

    fn set_window_strut_top(
        &self,
        _win: WindowId,
        _top: u32,
        _start_x: u32,
        _end_x: u32,
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

pub struct UdevBackend {
    display_handle: DisplayHandle,
    event_loop: SendWrapper<EventLoop<'static, JwmWaylandState>>,
    state: Box<JwmWaylandState>,
    #[allow(dead_code)]
    socket_name: Option<String>,
    pending_events: Arc<Mutex<VecDeque<BackendEvent>>>,

    flush_tx: Sender<()>,
    flush_pending: Arc<AtomicBool>,

    shared: Arc<Mutex<SharedState>>,
    session: LibSeatSession,

    kms: Option<Rc<RefCell<KmsState>>>,

    window_ops: Box<dyn WindowOps>,
    input_ops: Box<dyn InputOps>,
    property_ops: Box<dyn PropertyOps>,
    output_ops: Box<dyn OutputOps>,
    key_ops: Box<dyn KeyOps>,
    cursor_provider: Box<dyn CursorProvider>,
    color_allocator: Box<dyn ColorAllocator>,

    compositor: Option<WaylandCompositor>,
    drag: Option<UdevDragState>,
    last_inactive_session_log: Option<Instant>,
    output_management_tx_seq: u64,
    last_output_management_tx: Option<crate::backend::api::OutputManagementTransactionStatus>,

    // Reusable per-frame scratch buffers (cleared+refilled each frame) to avoid
    // two heap allocations per frame in compositor_render_frame.
    scratch_tex_updates: Vec<(u64, u32, u32, u32, bool, bool, [f32; 4])>,
    scratch_full_scene: Vec<(u64, i32, i32, u32, u32)>,

    // Per-window offscreen textures used to composite subsurface-based clients
    // (e.g. Electron/CEF apps like feishu) into a single texture. Persisted
    // across frames and reused while the window size is unchanged. Tuple is
    // (texture, width, height).
    offscreen_window_textures: HashMap<u64, (GlesTexture, u32, u32)>,
}

// NOTE: smithay state + calloop handle types are not thread-safe.
// JWM runs the backend on the main thread only, so this is acceptable as long as
// `UdevBackend` is never moved across threads.
unsafe impl Send for UdevBackend {}

fn env_flag(name: &str) -> bool {
    std::env::var_os(name).as_deref() == Some(std::ffi::OsStr::new("1"))
}

fn spawn_env_import(vars: &[&str]) {
    if vars.is_empty() {
        return;
    }

    let mut dbus = std::process::Command::new("dbus-update-activation-environment");
    dbus.arg("--systemd");
    for var in vars {
        dbus.arg(var);
    }
    dbus.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Err(err) = dbus.spawn() {
        log::debug!("[env] dbus-update-activation-environment failed to spawn: {err}");
    }

    let mut systemctl = std::process::Command::new("systemctl");
    systemctl.arg("--user").arg("import-environment");
    for var in vars {
        systemctl.arg(var);
    }
    systemctl
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Err(err) = systemctl.spawn() {
        log::debug!("[env] systemctl --user import-environment failed to spawn: {err}");
    }
}

fn path_has_executable(bin: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(bin);
        candidate.is_file()
            && std::fs::metadata(&candidate)
                .map(|m| {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        m.permissions().mode() & 0o111 != 0
                    }
                    #[cfg(not(unix))]
                    {
                        true
                    }
                })
                .unwrap_or(false)
    })
}

fn ensure_fcitx_env_for_primary_session(nested: bool) {
    if nested || !path_has_executable("fcitx5") {
        return;
    }
    // Do not override an explicit user choice. These defaults make systemd/D-Bus
    // activated XWayland and toolkit helper processes pick up fcitx5 after an
    // exec restart even when the login environment was sparse.
    unsafe {
        if std::env::var_os("GTK_IM_MODULE").is_none() {
            std::env::set_var("GTK_IM_MODULE", "fcitx");
        }
        if std::env::var_os("QT_IM_MODULE").is_none() {
            std::env::set_var("QT_IM_MODULE", "fcitx");
        }
        if std::env::var_os("XMODIFIERS").is_none() {
            std::env::set_var("XMODIFIERS", "@im=fcitx");
        }
        if std::env::var_os("SDL_IM_MODULE").is_none() {
            std::env::set_var("SDL_IM_MODULE", "fcitx");
        }
    }
}

fn open_active_libseat_session() -> Result<(LibSeatSession, LibSeatSessionNotifier), BackendError> {
    let timeout = std::env::var("JWM_LIBSEAT_ACTIVE_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or_else(|| Duration::from_secs(5));
    let retry_interval = Duration::from_millis(100);
    let deadline = Instant::now() + timeout;
    let mut attempts = 0usize;

    loop {
        let (session, notifier) =
            LibSeatSession::new().map_err(|err| BackendError::Other(Box::new(err)))?;
        if session.is_active() {
            if attempts > 0 {
                log::info!("[udev] libseat session became active after {attempts} retries");
            }
            return Ok((session, notifier));
        }

        attempts += 1;
        if attempts == 1 {
            log::warn!(
                "[udev] libseat session is inactive at startup; waiting up to {}ms before opening KMS/input",
                timeout.as_millis()
            );
        }

        if Instant::now() >= deadline {
            return Err(BackendError::Message(format!(
                "libseat session stayed inactive for {}ms; refusing to start half-active wayland-udev session",
                timeout.as_millis()
            )));
        }

        drop(notifier);
        drop(session);
        std::thread::sleep(retry_interval);
    }
}

#[derive(Debug, Clone, Copy)]
enum UdevDragAction {
    Move,
    Resize(ResizeEdge),
}

#[derive(Debug, Clone, Copy)]
struct UdevDragState {
    win: WindowId,
    start_geom: crate::backend::api::Geometry,
    start_root_x: f64,
    start_root_y: f64,
    action: UdevDragAction,
}

impl UdevBackend {
    fn request_flush(&self) {
        if !self.flush_pending.swap(true, Ordering::SeqCst) {
            let _ = self.flush_tx.send(());
        }
    }

    fn drop_kms(&mut self) {
        if let Some(old) = self.kms.take() {
            if let Some(token) = old.borrow_mut().registration_token.take() {
                let _ = self.event_loop.handle().remove(token);
            }
        }
    }

    fn sync_wayland_state_from_kms(&mut self) {
        let Some(kms) = self.kms.as_ref() else {
            self.state.outputs.clear();
            self.state.gamma_sizes.clear();
            return;
        };

        self.state.outputs = kms.borrow().outputs();
        self.state.gamma_sizes = kms.borrow_mut().gamma_sizes().into_iter().collect();
        attach_edid_caps_to_outputs(&self.state.outputs, &self.shared.lock_safe().outputs);

        if env_flag("JWM_DMABUF") {
            let kms_ref = kms.borrow();
            let render_formats = kms_ref.dmabuf_render_formats();
            let scanout_formats = kms_ref.dmabuf_render_formats();
            let main_device = kms_ref.dev_t();
            drop(kms_ref);
            if env_flag("JWM_DMABUF_FEEDBACK") {
                self.state.ensure_dmabuf_global_with_feedback(
                    &self.display_handle,
                    render_formats,
                    scanout_formats,
                    main_device,
                );
            } else {
                self.state.dmabuf_main_device = Some(main_device);
                self.state.dmabuf_render_formats = render_formats.clone();
                self.state
                    .ensure_dmabuf_global(&self.display_handle, render_formats);
                log::info!(
                    "[udev/wayland] dmabuf feedback disabled (set JWM_DMABUF_FEEDBACK=1 to enable)"
                );
            }
        } else {
            self.state.dmabuf_main_device = None;
            self.state.dmabuf_render_formats.clear();
            log::info!("[udev/wayland] linux-dmabuf disabled (set JWM_DMABUF=1 to enable)");
        }

        if let Some(ref screencopy_queue) = self.state.screencopy_pending {
            kms.borrow_mut()
                .set_screencopy_pending(screencopy_queue.clone());
        }
        if let Some(ref image_capture_queue) = self.state.image_capture_pending {
            kms.borrow_mut()
                .set_image_capture_pending(image_capture_queue.clone());
        }
        kms.borrow_mut()
            .set_capture_counters(self.state.capture_counters.clone());
    }

    fn recreate_compositor_for_current_kms(&mut self) {
        if self.compositor.is_none() {
            return;
        }

        self.compositor = None;
        if let Some(kms) = &self.kms {
            let mut kms_ref = kms.borrow_mut();
            let (w, h) = kms_ref.total_screen_size();
            let hdr_10bit = kms_ref.supports_10bit();
            match kms_ref.with_renderer(|gl| unsafe { WaylandCompositor::new(gl, w, h, hdr_10bit) })
            {
                Ok(Ok(compositor)) => self.compositor = Some(compositor),
                Ok(Err(e)) => {
                    log::error!("[udev] compositor recreate after KMS reinit failed: {e}")
                }
                Err(e) => log::error!("[udev] GL access for compositor recreate failed: {e:?}"),
            }
        }
        self.compositor_apply_config();
    }

    fn maybe_reinit_kms(&mut self) {
        if !self.shared.lock_safe().session_active {
            return;
        }

        let should = {
            let mut s = self.shared.lock_safe();
            if s.kms_needs_reinit {
                s.kms_needs_reinit = false;
                true
            } else {
                false
            }
        };
        if !should {
            return;
        }

        let selected = selected_kms_device(&self.shared);

        let Some((dev_id, dev_path)) = selected else {
            self.drop_kms();
            return;
        };

        let output_layout = output_layout_from_shared(&self.shared);

        let display_handle = self.display_handle.clone();
        match KmsState::new(
            &mut self.session,
            &dev_path,
            dev_id,
            &output_layout,
            &display_handle,
            self.flush_tx.clone(),
            self.flush_pending.clone(),
            self.event_loop.handle(),
        ) {
            Ok(new_kms) => {
                // Remove old notifier after new one is registered.
                self.drop_kms();

                self.kms = Some(new_kms);
                self.state.needs_redraw = true;
                self.sync_wayland_state_from_kms();

                // The rebuilt KMS state carries a fresh EGL context, so every GL
                // object the compositor created in the previous context (shaders,
                // textures, FBOs) is now dangling.
                self.recreate_compositor_for_current_kms();

                self.request_flush();
            }
            Err(err) => {
                log::warn!("KMS re-init failed (keeping previous state): {err}");
            }
        }
    }

    fn reconcile_session_active(&mut self) {
        let session_active = self.session.is_active();
        let was_active = self.shared.lock_safe().session_active;
        if session_active == was_active {
            return;
        }

        if session_active {
            self.shared.lock_safe().session_active = true;
            log::info!("[udev] session became active; rebuilding outputs and KMS");
            let seat_name = self.session.seat();
            refresh_preferred_device_id(&self.shared, &seat_name);
            match rebuild_outputs(&self.shared, &self.pending_events) {
                Ok(_) => {
                    sync_output_rects(&mut self.state, &self.shared);
                    self.pending_events
                        .lock_safe()
                        .push_back(BackendEvent::ScreenLayoutChanged);
                    queue_kms_reinit(&self.shared);
                    self.request_flush();
                }
                Err(err) => {
                    log::warn!("[udev] rebuild_outputs after session activation failed: {err:?}");
                }
            }
        } else if was_active {
            // Treat libseat's polled inactive state as advisory only. LightDM can
            // leave libseat reporting inactive during session handoff even after
            // KMS has been opened successfully. Real deactivation is handled by
            // the PauseSession notifier.
            let now = Instant::now();
            if self
                .last_inactive_session_log
                .is_none_or(|last| now.saturating_duration_since(last) >= Duration::from_secs(2))
            {
                self.last_inactive_session_log = Some(now);
                log::debug!(
                    "[udev] libseat still reports inactive; keeping current session active"
                );
            }
        }
    }

    pub fn new() -> Result<Self, BackendError> {
        let event_loop: EventLoop<'static, JwmWaylandState> =
            EventLoop::try_new().map_err(|e| BackendError::Other(Box::new(e)))?;
        let display: Rc<RefCell<Display<JwmWaylandState>>> = Rc::new(RefCell::new(
            Display::new().map_err(|e| BackendError::Other(Box::new(e)))?,
        ));
        let display_handle = display.borrow().handle();

        // Flush outgoing Wayland messages on demand (e.g. vblank-driven frame callbacks).
        // Coalesce requests to avoid piling up flush messages under heavy input/vblank.
        let (flush_tx, flush_rx) = channel::channel::<()>();
        let flush_pending = Arc::new(AtomicBool::new(false));
        {
            let display = display.clone();
            let flush_pending = flush_pending.clone();
            event_loop
                .handle()
                .insert_source(flush_rx, move |_, _, _state| {
                    if let Err(err) = display.borrow_mut().flush_clients() {
                        log::debug!("wayland flush_clients failed: {err:?}");
                    }
                    flush_pending.store(false, Ordering::SeqCst);
                })
                .map_err(|e| {
                    BackendError::Message(format!(
                        "calloop insert_source(wayland flush) failed: {e}"
                    ))
                })?;
        }

        // Wake the event loop on Wayland client requests, and dispatch them immediately.
        // We duplicate the display poll fd so the event source doesn't need to own `Display`.
        let wayland_poll_fd = {
            use std::os::fd::AsFd as _;
            smithay::reexports::rustix::io::dup(display.borrow().as_fd())
                .map_err(|e| BackendError::Other(Box::new(e)))?
        };
        {
            let display = display.clone();
            let flush_tx = flush_tx.clone();
            let flush_pending = flush_pending.clone();
            event_loop
                .handle()
                .insert_source(
                    Generic::new(wayland_poll_fd, Interest::READ, Mode::Level),
                    move |_, _, state| {
                        if let Err(err) = display.borrow_mut().dispatch_clients(state) {
                            log::warn!("wayland dispatch_clients failed: {err:?}");
                        }
                        if !flush_pending.swap(true, Ordering::SeqCst) {
                            let _ = flush_tx.send(());
                        }
                        Ok(PostAction::Continue)
                    },
                )
                .map_err(|e| {
                    BackendError::Message(format!(
                        "calloop insert_source(wayland display) failed: {e}"
                    ))
                })?;
        }

        // Safety net: if the WM doesn't configure a new toplevel quickly enough, clients can stall
        // forever waiting for the initial xdg_toplevel configure. Keep a small timeout-based
        // fallback to ensure we eventually send one.
        {
            let initial_configure_timeout = Duration::from_millis(250);
            let tick = Duration::from_millis(50);
            let timer = Timer::from_duration(tick);
            event_loop
                .handle()
                .insert_source(timer, move |_, _, state| {
                    state.signal_due_commit_timing_barriers();
                    state.ensure_initial_configure_timeout(initial_configure_timeout);
                    TimeoutAction::ToDuration(tick)
                })
                .map_err(|e| {
                    BackendError::Message(format!(
                        "calloop insert_source(initial configure timer) failed: {e}"
                    ))
                })?;
        }

        let shared = Arc::new(Mutex::new(SharedState::default()));
        let pending_events = Arc::new(Mutex::new(VecDeque::<BackendEvent>::new()));

        // Prepare key binding suppression table (like X11 grabs) for the udev/Wayland path.
        // We match against the same (mods, keysym) pair that JWM uses for shortcuts.
        {
            let key_bindings = CONFIG
                .load()
                .get_keys()
                .into_iter()
                .map(|k| ShortcutBinding {
                    mods: k.mask & allowed_shortcut_mods(),
                    keysym: k.key_sym,
                    repeatable: k.repeatable,
                })
                .collect::<Vec<_>>();

            let mut s = shared.lock_safe();
            s.key_bindings = key_bindings;
        }

        // libinput does not synthesize key-repeat events; emulate X11-style autorepeat
        // for WM shortcuts so holding (Alt+J) keeps cycling.
        {
            let shared = shared.clone();
            let pending_events = pending_events.clone();
            let timer = Timer::from_duration(KEY_REPEAT_TICK);
            event_loop
                .handle()
                .insert_source(timer, move |_, _, _state| {
                    let maybe_event = {
                        let mut s = shared.lock_safe();

                        let Some(mut rep) = s.repeat else {
                            return TimeoutAction::ToDuration(KEY_REPEAT_TICK);
                        };

                        let now = Instant::now();
                        if now < rep.next_fire {
                            // Not yet time; keep waiting.
                            s.repeat = Some(rep);
                            return TimeoutAction::ToDuration(KEY_REPEAT_TICK);
                        }

                        let current_mods =
                            Mods::from_bits_truncate(s.mods_state) & allowed_shortcut_mods();
                        if !current_mods.contains(rep.required_mods) {
                            // Modifiers released; stop repeating.
                            s.repeat = None;
                            return TimeoutAction::ToDuration(KEY_REPEAT_TICK);
                        }

                        // Generate one repeat event per tick at most.
                        rep.last_time = rep.last_time.saturating_add(
                            KEY_REPEAT_INTERVAL.as_millis().min(u128::from(u32::MAX)) as u32,
                        );
                        rep.next_fire = now + KEY_REPEAT_INTERVAL;
                        rep.mods_raw = s.mods_state;

                        let ev = BackendEvent::KeyPress {
                            keycode: rep.keycode,
                            state: rep.mods_raw,
                            time: rep.last_time,
                        };
                        s.repeat = Some(rep);
                        ev
                    };

                    pending_events.lock_safe().push_back(maybe_event);
                    TimeoutAction::ToDuration(KEY_REPEAT_TICK)
                })
                .map_err(|e| {
                    BackendError::Message(format!(
                        "calloop insert_source(key repeat timer) failed: {e}"
                    ))
                })?;
        }

        // Hot-reload of the user config: watch the parent directory (not the
        // file inode itself — editors save via tmpfile+rename, which would
        // make a file-inode watch silently die after the first save) and push
        // ConfigChanged into pending_events whenever the config filename
        // appears with a CLOSE_WRITE / MOVED_TO / CREATE event.
        {
            use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};

            let pending = pending_events.clone();
            let setup = || -> Result<(), BackendError> {
                let config_path = crate::config::Config::get_default_config_path();
                let watch_dir = config_path
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| config_path.clone());
                let config_file_name = config_path.file_name().map(|n| n.to_os_string());

                let inotify = Inotify::init(InitFlags::IN_NONBLOCK)
                    .map_err(|e| BackendError::Message(format!("inotify init failed: {e}")))?;
                inotify
                    .add_watch(
                        &watch_dir,
                        AddWatchFlags::IN_CLOSE_WRITE
                            | AddWatchFlags::IN_MOVED_TO
                            | AddWatchFlags::IN_CREATE,
                    )
                    .map_err(|e| {
                        BackendError::Message(format!("inotify watch {:?} failed: {e}", watch_dir))
                    })?;

                event_loop
                    .handle()
                    .insert_source(
                        calloop::generic::Generic::new(inotify, Interest::READ, Mode::Level),
                        move |_, inotify, _state| {
                            let events = inotify.read_events().unwrap_or_default();
                            let relevant =
                                events.iter().any(|ev| match (&config_file_name, &ev.name) {
                                    (Some(want), Some(got)) => got == want,
                                    _ => true,
                                });
                            if relevant {
                                pending.lock_safe().push_back(BackendEvent::ConfigChanged);
                            }
                            Ok(PostAction::Continue)
                        },
                    )
                    .map_err(|e| {
                        BackendError::Message(format!("calloop insert_source(inotify) failed: {e}"))
                    })?;
                Ok(())
            };
            if let Err(e) = setup() {
                log::warn!("[config] hot-reload disabled: {e}");
            } else {
                log::info!("[config] hot-reload enabled via inotify");
            }
        }

        let (mut session, notifier) = open_active_libseat_session()?;
        let seat_name = session.seat();
        let session_active_at_start = true;
        shared.lock_safe().session_active = session_active_at_start;

        let (wayland_state, socket_name) = JwmWaylandState::init(
            &display_handle,
            event_loop.handle(),
            pending_events.clone(),
            flush_tx.clone(),
            flush_pending.clone(),
            seat_name.clone(),
            true,
        )
        .map_err(|e| BackendError::Message(format!("wayland init failed: {e}")))?;

        if let Some(name) = socket_name.as_deref() {
            // Detect nested mode: if WAYLAND_DISPLAY was already set before we
            // create our own socket, a parent compositor owns it.  In that case
            // the inherited DBUS_SESSION_BUS_ADDRESS points to the parent session's
            // bus.  Children (bar, terminals) would connect to that bus where
            // GtkApplication may find conflicting existing registrations and either
            // hang or delegate window creation back to the parent compositor.
            // Clear D-Bus from our process env so that every subsequently spawned
            // child starts without a pre-existing bus address and either skips
            // D-Bus registration or starts its own session.
            let restarting = std::env::var_os("JWM_RESTARTING").is_some();
            let nested = !restarting && std::env::var_os("WAYLAND_DISPLAY").is_some();
            // SAFETY: JWM's backend is single-threaded and we set this once at startup.
            unsafe {
                std::env::set_var("WAYLAND_DISPLAY", name);
                std::env::set_var("XDG_CURRENT_DESKTOP", "jwm");
                std::env::set_var("XDG_SESSION_DESKTOP", "jwm");
                std::env::set_var("DESKTOP_SESSION", "jwm");
                std::env::set_var("XDG_SESSION_TYPE", "wayland");
                if nested {
                    log::info!(
                        "Nested Wayland session detected: clearing DBUS_SESSION_BUS_ADDRESS to isolate children from parent session bus"
                    );
                    std::env::set_var("DBUS_SESSION_BUS_ADDRESS", "");
                }
            }
            ensure_fcitx_env_for_primary_session(nested);
            spawn_env_import(&[
                "WAYLAND_DISPLAY",
                "XDG_CURRENT_DESKTOP",
                "XDG_SESSION_DESKTOP",
                "DESKTOP_SESSION",
                "XDG_SESSION_TYPE",
                "DBUS_SESSION_BUS_ADDRESS",
                "GTK_IM_MODULE",
                "QT_IM_MODULE",
                "XMODIFIERS",
                "SDL_IM_MODULE",
            ]);
        }
        let mut state = Box::new(wayland_state);

        // ---- Start XWayland ----
        // This spawns the Xwayland binary.  When it becomes ready (writes to
        // the displayfd), the calloop source fires `XWaylandEvent::Ready` and
        // we create the X11 window manager (`X11Wm`).
        {
            use std::process::Stdio;

            let (xwayland, xwayland_client) = XWayland::spawn(
                &display_handle,
                None, // auto-pick display number
                std::iter::empty::<(String, String)>(),
                std::iter::empty::<String>(), // no extra args
                true,                         // open abstract socket
                Stdio::null(),
                Stdio::null(),
                |_| {},
            )
            .map_err(|e| BackendError::Message(format!("XWayland spawn failed: {e}")))?;

            let xw_client = xwayland_client.clone();
            let loop_handle = event_loop.handle();
            let xw_loop_handle = loop_handle.clone();
            loop_handle
                .insert_source(xwayland, move |event, _, wl_state| {
                    match event {
                        XWaylandEvent::Ready {
                            x11_socket,
                            display_number,
                        } => {
                            log::info!("[xwayland] ready on DISPLAY=:{display_number}");
                            // SAFETY: single-threaded backend, set once.
                            unsafe {
                                std::env::set_var("DISPLAY", format!(":{display_number}"));
                            }
                            spawn_env_import(&["DISPLAY"]);
                            // `start_wm` requires `D: XwmHandler + XWaylandShellHandler + SeatHandler`.
                            // Our `JwmWaylandState` implements all three.
                            let dh = wl_state.display_handle.clone();
                            match X11Wm::start_wm(
                                xw_loop_handle.clone(),
                                &dh,
                                x11_socket,
                                xw_client.clone(),
                            ) {
                                Ok(wm) => {
                                    log::info!("[xwayland] X11Wm started");
                                    wl_state.x11_wm = Some(wm);
                                }
                                Err(e) => {
                                    log::error!("[xwayland] X11Wm::start_wm failed: {e:?}");
                                }
                            }
                        }
                        XWaylandEvent::Error => {
                            log::error!("[xwayland] XWayland exited with error");
                        }
                    }
                })
                .map_err(|e| {
                    BackendError::Message(format!("calloop insert_source(xwayland) failed: {e}"))
                })?;
        }

        let udev_backend = SmithayUdevBackend::new(&seat_name).map_err(|e| {
            BackendError::Other(Box::new(io::Error::new(
                io::ErrorKind::Other,
                format!("udev init failed: {e:?}"),
            )))
        })?;

        let mut libinput_context = Libinput::new_with_udev::<
            LibinputSessionInterface<LibSeatSession>,
        >(session.clone().into());
        libinput_context.udev_assign_seat(&seat_name).map_err(|e| {
            BackendError::Other(Box::new(io::Error::new(
                io::ErrorKind::Other,
                format!("libinput udev_assign_seat failed: {e:?}"),
            )))
        })?;
        let libinput_backend = LibinputInputBackend::new(libinput_context.clone());
        if !session_active_at_start {
            log::debug!(
                "[udev] session inactive at startup; keeping libinput running and retrying KMS"
            );
        }

        {
            let mut shared_guard = shared.lock_safe();
            for (device_id, path) in udev_backend.device_list() {
                shared_guard
                    .device_paths
                    .insert(device_id, path.to_path_buf());
            }
        }
        refresh_preferred_device_id(&shared, &seat_name);
        if session_active_at_start {
            rebuild_outputs(&shared, &pending_events)?;
        } else {
            log::debug!("[udev] session inactive at startup; delaying output scan");
        }

        // Keep a copy of output geometries in the Wayland state for popup constraining.
        sync_output_rects(&mut state, &shared);

        // Minimal visible output: initialize KMS and render a solid background.
        // If this fails (e.g. missing permissions / no DRM device), keep running headless.
        let kms = {
            let selected = selected_kms_device(&shared);

            match (session_active_at_start, selected) {
                (false, _) => {
                    log::debug!("[udev] session inactive at startup; delaying KMS init");
                    None
                }
                (true, Some((dev_id, p))) => {
                    let output_layout = output_layout_from_shared(&shared);

                    match KmsState::new(
                        &mut session,
                        &p,
                        dev_id,
                        &output_layout,
                        &display_handle,
                        flush_tx.clone(),
                        flush_pending.clone(),
                        event_loop.handle(),
                    ) {
                        Ok(kms) => Some(kms),
                        Err(err) => {
                            log::warn!("KMS init failed (running headless): {err}");
                            None
                        }
                    }
                }
                (true, None) => None,
            }
        };

        if let Some(kms) = &kms {
            state.outputs = kms.borrow().outputs();
            state.gamma_sizes = kms.borrow_mut().gamma_sizes().into_iter().collect();

            // Keep linux-dmabuf opt-in for now. Electron/VSCode binds
            // compositor globals very early, and this path has been observed to
            // trigger a native crash before the client creates an xdg_toplevel.
            if env_flag("JWM_DMABUF") {
                let kms_ref = kms.borrow();
                let render_formats = kms_ref.dmabuf_render_formats();
                let scanout_formats = kms_ref.dmabuf_render_formats();
                let main_device = kms_ref.dev_t();
                drop(kms_ref);
                if env_flag("JWM_DMABUF_FEEDBACK") {
                    state.ensure_dmabuf_global_with_feedback(
                        &display_handle,
                        render_formats,
                        scanout_formats,
                        main_device,
                    );
                } else {
                    state.dmabuf_main_device = Some(main_device);
                    state.dmabuf_render_formats = render_formats.clone();
                    state.ensure_dmabuf_global(&display_handle, render_formats);
                    log::info!(
                        "[udev/wayland] dmabuf feedback disabled (set JWM_DMABUF_FEEDBACK=1 to enable)"
                    );
                }
            } else {
                state.dmabuf_main_device = None;
                state.dmabuf_render_formats.clear();
                log::info!("[udev/wayland] linux-dmabuf disabled (set JWM_DMABUF=1 to enable)");
            }

            // wp-linux-drm-syncobj-v1 (explicit sync).
            //
            // Keep this opt-in for now. Some clients (notably Electron/VSCode)
            // exercise this path immediately after connecting, and a failure in
            // Smithay/driver syncobj lifetime handling can take the whole WM down
            // with a native fault rather than a Rust panic.
            {
                use smithay::wayland::drm_syncobj::{DrmSyncobjState, supports_syncobj_eventfd};
                let drm_fd = kms.borrow().drm_device_fd.clone();
                let explicit_sync_enabled = env_flag("JWM_EXPLICIT_SYNC");
                if explicit_sync_enabled && supports_syncobj_eventfd(&drm_fd) {
                    state.drm_syncobj_state = Some(DrmSyncobjState::new::<
                        crate::backend::wayland::state::JwmWaylandState,
                    >(&display_handle, drm_fd));
                    log::info!("[udev/wayland] wp-linux-drm-syncobj-v1 (explicit sync) enabled");
                } else if explicit_sync_enabled {
                    log::info!(
                        "[udev/wayland] DRM syncobj eventfd not supported, explicit sync disabled"
                    );
                } else {
                    log::info!(
                        "[udev/wayland] explicit sync disabled (set JWM_EXPLICIT_SYNC=1 to enable)"
                    );
                }
            }

            // Wire screencopy pending queue to KMS state.
            if let Some(ref screencopy_queue) = state.screencopy_pending {
                kms.borrow_mut()
                    .set_screencopy_pending(screencopy_queue.clone());
            }
            if let Some(ref image_capture_queue) = state.image_capture_pending {
                kms.borrow_mut()
                    .set_image_capture_pending(image_capture_queue.clone());
            }
            kms.borrow_mut()
                .set_capture_counters(state.capture_counters.clone());
        }

        {
            let pending_events = pending_events.clone();
            let shared = shared.clone();
            let flush_tx = flush_tx.clone();
            let flush_pending = flush_pending.clone();
            event_loop
                .handle()
                .insert_source(libinput_backend, move |event, _, state| {
                    // Notify idle tracker on any input activity
                    state.idle_notifier_state.notify_activity(&state.seat);

                    match event {
                        InputEvent::PointerMotion { event, .. } => {
                            let delta = event.delta();
                            let time = event.time_msec();
                            let (mut x, mut y, mut output, in_screenshot) = {
                                let mut s = shared.lock_safe();
                                s.pointer_x += delta.x;
                                s.pointer_y += delta.y;
                                // Clamp cursor to union bounding box of all outputs.
                                if !s.outputs.is_empty() {
                                    let min_x = s.outputs.iter().map(|o| o.x).min().unwrap_or(0) as f64;
                                    let min_y = s.outputs.iter().map(|o| o.y).min().unwrap_or(0) as f64;
                                    let max_x = s.outputs.iter().map(|o| o.x + o.width).max().unwrap_or(1920) as f64 - 1.0;
                                    let max_y = s.outputs.iter().map(|o| o.y + o.height).max().unwrap_or(1080) as f64 - 1.0;
                                    s.pointer_x = s.pointer_x.clamp(min_x, max_x);
                                    s.pointer_y = s.pointer_y.clamp(min_y, max_y);
                                }
                                let x = s.pointer_x;
                                let y = s.pointer_y;
                                let output = s
                                    .outputs
                                    .iter()
                                    .find(|o| (x as i32) >= o.x && (y as i32) >= o.y && (x as i32) < (o.x + o.width) && (y as i32) < (o.y + o.height))
                                    .map(|o| o.id);
                                (x, y, output, s.screenshot_grab_active)
                            };

                            let mut location: Point<f64, Logical> = (x, y).into();
                            if let Some(pointer) = state.seat.get_pointer() {
                                location = state.constrain_pointer_location(
                                    state.pointer_location,
                                    location,
                                    &pointer,
                                );
                                x = location.x;
                                y = location.y;
                                let mut s = shared.lock_safe();
                                s.pointer_x = x;
                                s.pointer_y = y;
                                output = output_at(&s.outputs, x, y);
                            }
                            state.pointer_location = location;
                            state.needs_redraw = true;

                            // If a popup grab is active, leaving the grab area dismisses the popups.
                            if let Some(grab_win) = state.popup_grab_toplevel {
                                if let Some(area) = state.popup_grab_area(grab_win) {
                                    let px = location.x.round() as i32;
                                    let py = location.y.round() as i32;
                                    let inside = px >= area.loc.x
                                        && py >= area.loc.y
                                        && px < area.loc.x + area.size.w
                                        && py < area.loc.y + area.size.h;
                                    if !inside {
                                        state.dismiss_popups_for_toplevel(grab_win);
                                        state.needs_redraw = true;
                                    }
                                }
                            }

                            let popup_hit = state.popup_surface_under(location).is_some();
                            let under = state.surface_under(location);
                            let hit = if popup_hit {
                                None
                            } else {
                                under.as_ref().and_then(|(win, _, _)| win.map(HitTarget::Surface))
                            };
                            let focus = under.map(|(_win, surface, origin)| (surface, origin));

                            if let Some(pointer) = state.seat.get_pointer() {
                                pointer.relative_motion(
                                    state,
                                    if in_screenshot { None } else { focus.clone() },
                                    &RelativeMotionEvent {
                                        delta,
                                        delta_unaccel: event.delta_unaccel(),
                                        utime: (time as u64) * 1000,
                                    },
                                );
                                pointer.motion(
                                    state,
                                    if in_screenshot { None } else { focus },
                                    &MotionEvent {
                                        location,
                                        serial: SCOUNTER.next_serial(),
                                        time,
                                    },
                                );
                                pointer.frame(state);
                            }

                            pending_events.lock_safe().push_back(BackendEvent::MotionNotify {
                                target: hit.unwrap_or(HitTarget::Background { output }),
                                root_x: x,
                                root_y: y,
                                time,
                            });
                        }
                        InputEvent::PointerMotionAbsolute { event, .. } => {
                            let time = event.time_msec();
                            let (mut x, mut y, mut output, in_screenshot) = {
                                let mut s = shared.lock_safe();
                                let (w, h, origin_x, origin_y) = output_bounds(&s.outputs);
                                let pos = event.position_transformed(smithay::utils::Size::from((w, h)));
                                s.pointer_x = origin_x as f64 + pos.x;
                                s.pointer_y = origin_y as f64 + pos.y;
                                let output = output_at(&s.outputs, s.pointer_x, s.pointer_y);
                                (s.pointer_x, s.pointer_y, output, s.screenshot_grab_active)
                            };

                            let mut location: Point<f64, Logical> = (x, y).into();
                            if let Some(pointer) = state.seat.get_pointer() {
                                location = state.constrain_pointer_location(
                                    state.pointer_location,
                                    location,
                                    &pointer,
                                );
                                x = location.x;
                                y = location.y;
                                let mut s = shared.lock_safe();
                                s.pointer_x = x;
                                s.pointer_y = y;
                                output = output_at(&s.outputs, x, y);
                            }
                            state.pointer_location = location;
                            state.needs_redraw = true;

                            // If a popup grab is active, leaving the grab area dismisses the popups.
                            if let Some(grab_win) = state.popup_grab_toplevel {
                                if let Some(area) = state.popup_grab_area(grab_win) {
                                    let px = location.x.round() as i32;
                                    let py = location.y.round() as i32;
                                    let inside = px >= area.loc.x
                                        && py >= area.loc.y
                                        && px < area.loc.x + area.size.w
                                        && py < area.loc.y + area.size.h;
                                    if !inside {
                                        state.dismiss_popups_for_toplevel(grab_win);
                                        state.needs_redraw = true;
                                    }
                                }
                            }

                            let popup_hit = state.popup_surface_under(location).is_some();
                            let under = state.surface_under(location);
                            let hit = if popup_hit {
                                None
                            } else {
                                under
                                    .as_ref()
                                    .and_then(|(win, _, _)| win.map(HitTarget::Surface))
                            };
                            let focus = under.map(|(_win, surface, origin)| (surface, origin));

                            if let Some(pointer) = state.seat.get_pointer() {
                                pointer.motion(
                                    state,
                                    if in_screenshot { None } else { focus },
                                    &MotionEvent {
                                        location,
                                        serial: SCOUNTER.next_serial(),
                                        time,
                                    },
                                );
                                pointer.frame(state);
                            }

                            pending_events.lock_safe().push_back(BackendEvent::MotionNotify {
                                target: hit.unwrap_or(HitTarget::Background { output }),
                                root_x: x,
                                root_y: y,
                                time,
                            });
                        }
                        InputEvent::PointerButton { event, .. } => {
                            let time = event.time_msec();
                            let button_code = event.button_code();
                            let pressed = matches!(event.state(), smithay::backend::input::ButtonState::Pressed);

                            // libinput / evdev button codes are Linux input codes (e.g. BTN_LEFT=272),
                            // while JWM expects X11-like button numbers (1=left, 2=middle, 3=right).
                            // Map the common codes explicitly.
                            let detail_btn: u8 = match button_code {
                                272 => 1, // BTN_LEFT
                                273 => 3, // BTN_RIGHT
                                274 => 2, // BTN_MIDDLE
                                275 => 8, // BTN_SIDE
                                276 => 9, // BTN_EXTRA
                                _ => (button_code & 0xFF) as u8,
                            };
                            let (x, y, output, in_screenshot) = {
                                let s = shared.lock_safe();
                                let x = s.pointer_x;
                                let y = s.pointer_y;
                                let _mods = s.mods_state;
                                let output = s
                                    .outputs
                                    .iter()
                                    .find(|o| (x as i32) >= o.x && (y as i32) >= o.y && (x as i32) < (o.x + o.width) && (y as i32) < (o.y + o.height))
                                    .map(|o| o.id);
                                (x, y, output, s.screenshot_grab_active)
                            };

                            let location: Point<f64, Logical> = (x, y).into();

                            // Minimal xdg_popup grab behavior: if a popup grab is active for a
                            // toplevel, a click outside all of its popups dismisses them.
                            if pressed {
                                if let Some(grab_win) = state.popup_grab_toplevel {
                                    let in_any_popup = state
                                        .popup_rects_for_toplevel(grab_win)
                                        .iter()
                                        .any(|(_surf, rect)| {
                                            let x0 = rect.loc.x as f64;
                                            let y0 = rect.loc.y as f64;
                                            let x1 = x0 + rect.size.w as f64;
                                            let y1 = y0 + rect.size.h as f64;
                                            location.x >= x0
                                                && location.y >= y0
                                                && location.x < x1
                                                && location.y < y1
                                        });

                                    if !in_any_popup {
                                        state.dismiss_popups_for_toplevel(grab_win);
                                        state.needs_redraw = true;
                                    }
                                }
                            }

                            let popup_hit = state.popup_surface_under(location).is_some();
                            let under = state.surface_under(location);
                            let hit = if popup_hit {
                                None
                            } else {
                                under
                                    .as_ref()
                                    .and_then(|(win, _, _)| win.map(HitTarget::Surface))
                            };
                            let focus = under.map(|(_win, surface, origin)| (surface, origin));

                            if !in_screenshot {
                                if let Some(pointer) = state.seat.get_pointer() {
                                    // Ensure focus is up-to-date before sending the button.
                                    pointer.motion(
                                        state,
                                        focus,
                                        &MotionEvent {
                                            location,
                                            serial: SCOUNTER.next_serial(),
                                            time,
                                        },
                                    );

                                    pointer.button(
                                        state,
                                        &ButtonEvent {
                                            serial: SCOUNTER.next_serial(),
                                            time,
                                            button: button_code,
                                            state: if pressed {
                                                smithay::backend::input::ButtonState::Pressed
                                            } else {
                                                smithay::backend::input::ButtonState::Released
                                            },
                                        },
                                    );
                                    pointer.frame(state);
                                }

                                // Focus follows click: if the user clicks a normal surface, it should
                                // receive keyboard focus. For layer-shell surfaces, only focus if it
                                // requested keyboard interactivity (OnDemand/Exclusive), otherwise keep
                                // the current focus (e.g. clicking a non-interactive panel shouldn't
                                // steal focus from the active app).
                                if pressed && !popup_hit {
                                    if let Some(kbd) = state.seat.get_keyboard() {
                                        if let Some((_win, surface, _origin)) = state.surface_under(location) {
                                            let layer_interactivity = state
                                                .layer_shell_state
                                                .layer_surfaces()
                                                .find(|l| l.wl_surface().id() == surface.id())
                                                .map(|l| l.with_cached_state(|d| d.keyboard_interactivity));

                                            let should_focus = match layer_interactivity {
                                                Some(KeyboardInteractivity::None) => false,
                                                Some(KeyboardInteractivity::OnDemand) => true,
                                                Some(KeyboardInteractivity::Exclusive) => true,
                                                None => true,
                                            };

                                            if should_focus {
                                                let win = state.surface_to_window.get(&surface.id()).copied();
                                                kbd.set_focus(
                                                    state,
                                                    Some(surface.clone()),
                                                    SCOUNTER.next_serial(),
                                                );
                                                state.set_active_toplevel(win);
                                            }
                                        }
                                    }
                                }
                            }

                            if popup_hit {
                                if std::env::var("JWM_DEBUG_BUTTONS")
                                    .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                                    .unwrap_or(false)
                                {
                                    log::info!(
                                        "[udev:btn->popup] button_code={} detail={} x={:.1} y={:.1}",
                                        button_code,
                                        detail_btn,
                                        x,
                                        y
                                    );
                                }
                            } else if pressed {
                                let mods_state = shared.lock_safe().mods_state;

                                let debug_buttons = std::env::var("JWM_DEBUG_BUTTONS")
                                    .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                                    .unwrap_or(false);
                                if debug_buttons {
                                    log::info!(
                                        "[udev:btn->wm] button_code={} detail={} mods_state=0x{:x} x={:.1} y={:.1} target={:?}",
                                        button_code,
                                        detail_btn,
                                        mods_state,
                                        x,
                                        y,
                                        hit.unwrap_or(HitTarget::Background { output })
                                    );
                                }
                                pending_events.lock_safe().push_back(BackendEvent::ButtonPress {
                                    target: hit.unwrap_or(HitTarget::Background { output }),
                                    state: mods_state,
                                    detail: detail_btn,
                                    time,
                                    root_x: x,
                                    root_y: y,
                                });
                            } else {
                                pending_events.lock_safe().push_back(BackendEvent::ButtonRelease {
                                    target: hit.unwrap_or(HitTarget::Background { output }),
                                    time,
                                });
                            }
                        }
                        InputEvent::Keyboard { event, .. } => {
                            let time = InputEventExt::time_msec(&event);
                            let keycode = event.key_code();
                            let state_key = event.state();
                            let serial = SCOUNTER.next_serial();
                            let pressed = matches!(state_key, smithay::backend::input::KeyState::Pressed);
                            let session_locked = state.session_locked;

                            let debug_keys = std::env::var("JWM_DEBUG_KEYS")
                                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                                .unwrap_or(false);

                            // Layer-shell surfaces can request exclusive keyboard interactivity
                            // (e.g. lock screens / OSD). If such a surface exists on Top/Overlay,
                            // route keyboard events directly to it and do not emit WM shortcuts.
                            let mut handled_by_exclusive_layer = false;
                            let mut shortcuts_inhibited = false;

                            // If nothing is focused, focus the surface under the pointer (best-effort).
                            if let Some(kbd) = state.seat.get_keyboard() {
                                if debug_keys && pressed {
                                    // Smithay exposes keycodes in the XKB/Wayland domain (evdev + 8).
                                    // Keep them as-is; adding +8 again will shift keys (e.g. Enter -> j).
                                    let xkb_keycode_u8 = u8::try_from(u32::from(keycode)).unwrap_or(0);
                                    let evdev_keycode = u32::from(keycode).saturating_sub(8);
                                    log::info!(
                                        "[udev:key] xkb_keycode={} evdev_keycode={} focus_before={} time={}",
                                        xkb_keycode_u8,
                                        evdev_keycode,
                                        kbd.current_focus().is_some(),
                                        time
                                    );
                                }

                                // Check exclusive layer-shell first.
                                let cfg = crate::config::CONFIG.load();
                                let bar_name = cfg.status_bar_name();

                                let screenshot_grab_active =
                                    shared.lock_safe().screenshot_grab_active;

                                let exclusive_surface = if session_locked || screenshot_grab_active
                                {
                                    None
                                } else {
                                    state.layer_shell_state.layer_surfaces().rev().find_map(
                                        |layer| {
                                        // Do not let the status bar block WM shortcuts.
                                        // Some bars mistakenly request `Exclusive` keyboard interactivity.
                                        if !bar_name.is_empty() {
                                            if let Some(win) = state
                                                .surface_to_window
                                                .get(&layer.wl_surface().id())
                                                .copied()
                                            {
                                                let title = state
                                                    .window_title
                                                    .get(&win)
                                                    .map(|s| s.as_str())
                                                    .unwrap_or("");
                                                let app_id = state
                                                    .window_app_id
                                                    .get(&win)
                                                    .map(|s| s.as_str())
                                                    .unwrap_or("");
                                                if title == bar_name || app_id == bar_name {
                                                    return None;
                                                }
                                            }
                                        }

                                        let exclusive = layer.with_cached_state(|data| {
                                            let exclusive_zone: i32 = data.exclusive_zone.into();
                                            data.keyboard_interactivity == KeyboardInteractivity::Exclusive
                                                && (data.layer == WlrLayer::Top
                                                    || data.layer == WlrLayer::Overlay)
                                                // Bars/docks often set a non-zero exclusive zone to reserve space.
                                                // Treat those as non-blocking for WM shortcuts.
                                                && exclusive_zone == 0
                                        });
                                        if !exclusive {
                                            return None;
                                        }

                                        // Only focus if the layer surface is actually mapped.
                                        let mapped = state.outputs.iter().any(|o| {
                                            let map = layer_map_for_output(o);
                                            map.layers().any(|l| l.layer_surface() == &layer)
                                        });
                                        if mapped {
                                            Some(layer.wl_surface().clone())
                                        } else {
                                            None
                                        }
                                        },
                                    )
                                };

                                if let Some(surface) = exclusive_surface {
                                    let is_ime = state.im_client_id.as_ref().map_or(false, |im_id| {
                                        surface.id().same_client_as(im_id)
                                    });

                                    if !is_ime {
                                        handled_by_exclusive_layer = true;

                                        if debug_keys && pressed {
                                            log::info!(
                                                "[udev:key] handled_by_exclusive_layer=true"
                                            );
                                        }
                                        kbd.set_focus(state, Some(surface), serial);

                                        let _ = kbd.input::<(), _>(
                                            state,
                                            keycode,
                                            state_key,
                                            serial,
                                            time,
                                            |_, modifiers, _handle| {
                                                let mods_bits = mods_from_smithay(modifiers).bits();
                                                if let Some(mut s) = shared.lock().ok() {
                                                    s.mods_state = mods_bits;
                                                }
                                                // Do not intercept any keys while an exclusive layer is active.
                                                FilterResult::Forward
                                            },
                                        );

                                        // Keep modifier state correct even if Smithay decides not to call
                                        // the filter closure (e.g. when no focus exists).
                                        let mods_bits =
                                            mods_from_smithay(&kbd.modifier_state()).bits();
                                        if let Some(mut s) = shared.lock().ok() {
                                            s.mods_state = mods_bits;
                                        }
                                    }
                                }

                                if handled_by_exclusive_layer {
                                    // Skip best-effort focus selection and WM shortcut emission.
                                } else {
                                    shortcuts_inhibited = state.seat.keyboard_shortcuts_inhibited();
                                    if session_locked {
                                        let (px, py) = {
                                            let s = shared.lock_safe();
                                            (s.pointer_x, s.pointer_y)
                                        };
                                        let location: Point<f64, Logical> = (px, py).into();
                                        if let Some((_win, surface, _origin)) =
                                            state.surface_under(location)
                                        {
                                            kbd.set_focus(state, Some(surface), serial);
                                        }
                                    }
                                    if kbd.current_focus().is_none() {
                                        let (px, py) = {
                                            let s = shared.lock_safe();
                                            (s.pointer_x, s.pointer_y)
                                        };
                                        let location: Point<f64, Logical> = (px, py).into();
                                        if let Some((_win, surface, _origin)) =
                                            state.surface_under(location)
                                        {
                                            kbd.set_focus(state, Some(surface), serial);
                                        }
                                    }

                                    let _ = kbd.input::<(), _>(
                                        state,
                                        keycode,
                                        state_key,
                                        serial,
                                        time,
                                        |_, modifiers, _handle| {
                                            let mods_bits = mods_from_smithay(modifiers).bits();

                                            // Smithay provides XKB/Wayland keycodes already (evdev + 8).
                                            // Use them directly for xkbcommon lookups.
                                            let xkb_keycode_u8 =
                                                u8::try_from(u32::from(keycode)).unwrap_or(0);

                                            let Some(mut s) = shared.lock().ok() else {
                                                return FilterResult::Forward;
                                            };

                                            s.mods_state = mods_bits;

                                            // Keep key press/release symmetric: if we intercepted a press,
                                            // also intercept its release so clients don't see a stray release.
                                            if !pressed {
                                                if s.suppressed_keycodes.remove(&xkb_keycode_u8) {
                                                    return FilterResult::Intercept(());
                                                }
                                                return FilterResult::Forward;
                                            }

                                            let keysym = s
                                                .keysym_table
                                                .get(xkb_keycode_u8 as usize)
                                                .copied()
                                                .unwrap_or(0);

                                            let clean_mods =
                                                crate::backend::common_define::Mods::from_bits_truncate(mods_bits)
                                                    & allowed_shortcut_mods();

                                            let is_wm_shortcut = s
                                                .key_bindings
                                                .iter()
                                                .any(|binding| {
                                                    binding.keysym == keysym
                                                        && binding.mods == clean_mods
                                                });
                                            let is_screenshot_control = s.screenshot_grab_active
                                                && matches!(
                                                    keysym,
                                                    crate::backend::common_define::keys::KEY_Escape
                                                        | crate::backend::common_define::keys::KEY_Return
                                                        | crate::backend::common_define::keys::KEY_s
                                                        | crate::backend::common_define::keys::KEY_c
                                                        | crate::backend::common_define::keys::KEY_p
                                                        | crate::backend::common_define::keys::KEY_f
                                                        | crate::backend::common_define::keys::KEY_l
                                                        | crate::backend::common_define::keys::KEY_a
                                                        | crate::backend::common_define::keys::KEY_r
                                                        | crate::backend::common_define::keys::KEY_o
                                                        | crate::backend::common_define::keys::KEY_z
                                                        | crate::backend::common_define::keys::KEY_BackSpace
                                                        | crate::backend::common_define::keys::KEY_Delete
                                                        | crate::backend::common_define::keys::KEY_Left
                                                        | crate::backend::common_define::keys::KEY_Right
                                                        | crate::backend::common_define::keys::KEY_Up
                                                        | crate::backend::common_define::keys::KEY_Down
                                                        | crate::backend::common_define::keys::KEY_1
                                                        | crate::backend::common_define::keys::KEY_2
                                                        | crate::backend::common_define::keys::KEY_3
                                                        | crate::backend::common_define::keys::KEY_4
                                                        | crate::backend::common_define::keys::KEY_5
                                                        | crate::backend::common_define::keys::KEY_6
                                                        | crate::backend::common_define::keys::KEY_7
                                                        | crate::backend::common_define::keys::KEY_8
                                            );
                                            let should_suppress = is_screenshot_control
                                                || (!session_locked
                                                    && !shortcuts_inhibited
                                                    && is_wm_shortcut);

                                            if should_suppress {
                                                s.suppressed_keycodes.insert(xkb_keycode_u8);
                                                FilterResult::Intercept(())
                                            } else {
                                                FilterResult::Forward
                                            }
                                        },
                                    );

                                    // Keep modifier state correct even if Smithay decides not to call
                                    // the filter closure (e.g. when no focus exists).
                                    let mods_bits = mods_from_smithay(&kbd.modifier_state()).bits();
                                    if let Some(mut s) = shared.lock().ok() {
                                        s.mods_state = mods_bits;
                                    }
                                }
                            } else if debug_keys && pressed {
                                log::warn!("[udev:key] seat.get_keyboard() returned None (no keyboard configured?)");
                            }

                            // JWM only uses press for shortcuts for now. When an active client
                            // inhibits compositor shortcuts, let that client receive the combo
                            // and keep the WM repeat state quiet.
                            let screenshot_grab_active = shared.lock_safe().screenshot_grab_active;
                            if session_locked
                                || (!screenshot_grab_active
                                    && (shortcuts_inhibited || handled_by_exclusive_layer))
                            {
                                shared.lock_safe().repeat = None;
                            }
                            if matches!(state_key, smithay::backend::input::KeyState::Pressed)
                                && !session_locked
                                && (screenshot_grab_active
                                    || (!shortcuts_inhibited && !handled_by_exclusive_layer))
                            {
                                // Smithay provides XKB/Wayland keycodes already (evdev + 8).
                                let keycode_u32 = u32::from(keycode);
                                let keycode_u8 = u8::try_from(keycode_u32).unwrap_or(0);
                                let mods_state = shared.lock_safe().mods_state;
                                pending_events.lock_safe().push_back(BackendEvent::KeyPress {
                                    keycode: keycode_u8,
                                    state: mods_state,
                                    time,
                                });

                                // Start (or reset) key repeat for bound shortcuts.
                                // This mirrors X11 autorepeat behavior for WM shortcuts.
                                {
                                    let mut s = shared.lock_safe();

                                    // Any new key press cancels previous repeat.
                                    s.repeat = None;

                                    let keysym = s
                                        .keysym_table
                                        .get(keycode_u8 as usize)
                                        .copied()
                                        .unwrap_or(0);
                                    let clean_mods = Mods::from_bits_truncate(mods_state) & allowed_shortcut_mods();
                                    let is_repeatable = s
                                        .key_bindings
                                        .iter()
                                        .find(|binding| {
                                            binding.keysym == keysym
                                                && binding.mods == clean_mods
                                        })
                                        .map(|binding| binding.repeatable)
                                        .unwrap_or(false);

                                    if is_repeatable {
                                        s.repeat = Some(RepeatState {
                                            keycode: keycode_u8,
                                            mods_raw: mods_state,
                                            required_mods: clean_mods,
                                            last_time: time,
                                            next_fire: Instant::now() + KEY_REPEAT_DELAY,
                                        });
                                    }
                                }

                                if debug_keys {
                                    log::info!(
                                        "[udev:key->wm] keycode={} mods_state=0x{:x} time={}",
                                        keycode_u8,
                                        mods_state,
                                        time
                                    );
                                }
                            }

                            // Stop repeating when the held shortcut key is released, or when
                            // modifiers are no longer satisfied (e.g. Alt released).
                            if matches!(state_key, smithay::backend::input::KeyState::Released) {
                                let keycode_u8 = u8::try_from(u32::from(keycode)).unwrap_or(0);
                                let mut s = shared.lock_safe();
                                if let Some(rep) = s.repeat {
                                    if rep.keycode == keycode_u8 {
                                        s.repeat = None;
                                    } else {
                                        let current_mods = Mods::from_bits_truncate(s.mods_state) & allowed_shortcut_mods();
                                        if !current_mods.contains(rep.required_mods) {
                                            s.repeat = None;
                                        }
                                    }
                                }
                            }
                        }
                        InputEvent::PointerAxis { event, .. } => {
                            let time = InputEventExt::time_msec(&event);
                            if let Some(pointer) = state.seat.get_pointer() {
                                let mut frame = AxisFrame::new(time).source(event.source());
                                for axis in [Axis::Horizontal, Axis::Vertical] {
                                    if let Some(val) = event.amount(axis) {
                                        frame = frame.value(axis, val);
                                    }
                                    if let Some(v120) = event.amount_v120(axis) {
                                        frame = frame.v120(axis, v120 as i32);
                                    }
                                    frame = frame.relative_direction(
                                        axis,
                                        event.relative_direction(axis),
                                    );
                                }
                                pointer.axis(state, frame);
                                pointer.frame(state);
                            }
                        }
                        InputEvent::TouchDown { event, .. } => {
                            let time = event.time_msec();
                            let slot = event.slot();
                            let (w, h, origin_x, origin_y) = {
                                let s = shared.lock_safe();
                                output_bounds(&s.outputs)
                            };
                            let pos = event.position_transformed(smithay::utils::Size::from((w, h)));
                            let pos: Point<f64, Logical> =
                                (origin_x as f64 + pos.x, origin_y as f64 + pos.y).into();
                            let focus = state.surface_under(pos).map(|(_win, surface, origin)| (surface, origin));
                            if let Some(touch) = state.seat.get_touch() {
                                touch.down(
                                    state,
                                    focus,
                                    &smithay::input::touch::DownEvent {
                                        slot,
                                        location: pos,
                                        serial: SCOUNTER.next_serial(),
                                        time,
                                    },
                                );
                            }
                        }
                        InputEvent::TouchMotion { event, .. } => {
                            let time = event.time_msec();
                            let slot = event.slot();
                            let (w, h, origin_x, origin_y) = {
                                let s = shared.lock_safe();
                                output_bounds(&s.outputs)
                            };
                            let pos = event.position_transformed(smithay::utils::Size::from((w, h)));
                            let pos: Point<f64, Logical> =
                                (origin_x as f64 + pos.x, origin_y as f64 + pos.y).into();
                            let focus = state.surface_under(pos).map(|(_win, surface, origin)| (surface, origin));
                            if let Some(touch) = state.seat.get_touch() {
                                touch.motion(
                                    state,
                                    focus,
                                    &smithay::input::touch::MotionEvent {
                                        slot,
                                        location: pos,
                                        time,
                                    },
                                );
                            }
                        }
                        InputEvent::TouchUp { event, .. } => {
                            let time = event.time_msec();
                            let slot = event.slot();
                            if let Some(touch) = state.seat.get_touch() {
                                touch.up(
                                    state,
                                    &smithay::input::touch::UpEvent {
                                        slot,
                                        serial: SCOUNTER.next_serial(),
                                        time,
                                    },
                                );
                            }
                        }
                        InputEvent::TouchFrame { .. } => {
                            if let Some(touch) = state.seat.get_touch() {
                                touch.frame(state);
                            }
                        }
                        InputEvent::GestureSwipeBegin { event, .. } => {
                            let fingers = event.fingers();
                            let cfg = crate::config::CONFIG.load();
                            let intercept = gesture_swipe_should_intercept(
                                fingers,
                                &cfg.behavior().gesture_swipe,
                            );
                            if intercept {
                                state.gesture_swipe = crate::backend::wayland::state::GestureSwipeTracker {
                                    fingers,
                                    intercept: true,
                                    dx: 0.0,
                                    dy: 0.0,
                                };
                            } else if let Some(pointer) = state.seat.get_pointer() {
                                pointer.gesture_swipe_begin(
                                    state,
                                    &smithay::input::pointer::GestureSwipeBeginEvent {
                                        serial: SCOUNTER.next_serial(),
                                        time: event.time_msec(),
                                        fingers,
                                    },
                                );
                            }
                        }
                        InputEvent::GestureSwipeUpdate { event, .. } => {
                            if state.gesture_swipe.intercept {
                                let d = event.delta();
                                state.gesture_swipe.dx += d.x;
                                state.gesture_swipe.dy += d.y;
                            } else if let Some(pointer) = state.seat.get_pointer() {
                                pointer.gesture_swipe_update(
                                    state,
                                    &smithay::input::pointer::GestureSwipeUpdateEvent {
                                        time: event.time_msec(),
                                        delta: event.delta(),
                                    },
                                );
                            }
                        }
                        InputEvent::GestureSwipeEnd { event, .. } => {
                            if state.gesture_swipe.intercept {
                                let cfg = crate::config::CONFIG.load();
                                let threshold = cfg.behavior().gesture_swipe_threshold;
                                let dx = state.gesture_swipe.dx;
                                let dy = state.gesture_swipe.dy;
                                let fingers = state.gesture_swipe.fingers;
                                let cancelled = event.cancelled();
                                state.gesture_swipe = Default::default();

                                if !cancelled {
                                    let direction: Option<&'static str> =
                                        if dx.abs() > dy.abs() {
                                            if dx.abs() >= threshold {
                                                if dx > 0.0 { Some("right") } else { Some("left") }
                                            } else { None }
                                        } else if dy.abs() >= threshold {
                                            if dy > 0.0 { Some("down") } else { Some("up") }
                                        } else { None };
                                    if let Some(dir) = direction {
                                        pending_events.lock_safe().push_back(
                                            BackendEvent::GestureSwipeAction {
                                                fingers,
                                                direction: dir,
                                            },
                                        );
                                    }
                                }
                            } else if let Some(pointer) = state.seat.get_pointer() {
                                pointer.gesture_swipe_end(
                                    state,
                                    &smithay::input::pointer::GestureSwipeEndEvent {
                                        serial: SCOUNTER.next_serial(),
                                        time: event.time_msec(),
                                        cancelled: event.cancelled(),
                                    },
                                );
                            }
                        }
                        InputEvent::GesturePinchBegin { event, .. } => {
                            if let Some(pointer) = state.seat.get_pointer() {
                                pointer.gesture_pinch_begin(
                                    state,
                                    &smithay::input::pointer::GesturePinchBeginEvent {
                                        serial: SCOUNTER.next_serial(),
                                        time: event.time_msec(),
                                        fingers: event.fingers(),
                                    },
                                );
                            }
                        }
                        InputEvent::GesturePinchUpdate { event, .. } => {
                            if let Some(pointer) = state.seat.get_pointer() {
                                pointer.gesture_pinch_update(
                                    state,
                                    &smithay::input::pointer::GesturePinchUpdateEvent {
                                        time: event.time_msec(),
                                        delta: event.delta(),
                                        scale: event.scale(),
                                        rotation: event.rotation(),
                                    },
                                );
                            }
                        }
                        InputEvent::GesturePinchEnd { event, .. } => {
                            if let Some(pointer) = state.seat.get_pointer() {
                                pointer.gesture_pinch_end(
                                    state,
                                    &smithay::input::pointer::GesturePinchEndEvent {
                                        serial: SCOUNTER.next_serial(),
                                        time: event.time_msec(),
                                        cancelled: event.cancelled(),
                                    },
                                );
                            }
                        }
                        InputEvent::GestureHoldBegin { event, .. } => {
                            if let Some(pointer) = state.seat.get_pointer() {
                                pointer.gesture_hold_begin(
                                    state,
                                    &smithay::input::pointer::GestureHoldBeginEvent {
                                        serial: SCOUNTER.next_serial(),
                                        time: event.time_msec(),
                                        fingers: event.fingers(),
                                    },
                                );
                            }
                        }
                        InputEvent::GestureHoldEnd { event, .. } => {
                            if let Some(pointer) = state.seat.get_pointer() {
                                pointer.gesture_hold_end(
                                    state,
                                    &smithay::input::pointer::GestureHoldEndEvent {
                                        serial: SCOUNTER.next_serial(),
                                        time: event.time_msec(),
                                        cancelled: event.cancelled(),
                                    },
                                );
                            }
                        }
                        _ => {}
                    }

                    // Input events can enqueue Wayland protocol messages; flush them promptly.
                    if !flush_pending.swap(true, Ordering::SeqCst) {
                        let _ = flush_tx.send(());
                    }
                })
                .map_err(|e| BackendError::Message(format!("calloop insert_source(libinput) failed: {e}")))?;
        }

        {
            let shared = shared.clone();
            let pending_events = pending_events.clone();
            let seat_name = seat_name.clone();
            let mut notifier_libinput_context = libinput_context.clone();
            event_loop
                .handle()
                .insert_source(notifier, move |event, &mut (), _state| match event {
                    SessionEvent::PauseSession => {
                        shared.lock_safe().session_active = false;
                        notifier_libinput_context.suspend();
                        pending_events
                            .lock_safe()
                            .push_back(BackendEvent::ScreenLayoutChanged);
                    }
                    SessionEvent::ActivateSession => {
                        shared.lock_safe().session_active = true;
                        if let Err(e) = notifier_libinput_context.resume() {
                            log::warn!("[udev] libinput resume after VT-switch failed: {e:?}");
                        }
                        refresh_preferred_device_id(&shared, &seat_name);
                        pending_events
                            .lock_safe()
                            .push_back(BackendEvent::ScreenLayoutChanged);
                        let _ = rebuild_outputs(&shared, &pending_events);
                        sync_output_rects(_state, &shared);
                        if let Some(grab_win) = _state.popup_grab_toplevel {
                            _state.reconstrain_popups_for_toplevel(grab_win);
                        }
                        queue_kms_reinit(&shared);
                    }
                })
                .map_err(|e| {
                    BackendError::Message(format!(
                        "calloop insert_source(libseat notifier) failed: {e}"
                    ))
                })?;
        }

        {
            let shared = shared.clone();
            let pending_events = pending_events.clone();
            let seat_name = seat_name.clone();
            event_loop
                .handle()
                .insert_source(udev_backend, move |event, _, _state| {
                    let device_lifecycle_changed = match event {
                        UdevEvent::Added { device_id, path } => {
                            shared
                                .lock_safe()
                                .device_paths
                                .insert(device_id, path.to_path_buf());
                            true
                        }
                        UdevEvent::Changed { device_id } => {
                            let _ = device_id;
                            false
                        }
                        UdevEvent::Removed { device_id } => {
                            shared.lock_safe().device_paths.remove(&device_id);
                            true
                        }
                    };
                    if device_lifecycle_changed {
                        refresh_preferred_device_id(&shared, &seat_name);
                    }

                    if !shared.lock_safe().session_active {
                        log::debug!("[udev] delaying output refresh while session is inactive");
                        return;
                    }

                    let outputs_changed =
                        rebuild_outputs(&shared, &pending_events).unwrap_or_else(|err| {
                            log::warn!("[udev] rebuild_outputs failed: {err:?}");
                            false
                        });

                    if outputs_changed {
                        sync_output_rects(_state, &shared);
                        if let Some(grab_win) = _state.popup_grab_toplevel {
                            _state.reconstrain_popups_for_toplevel(grab_win);
                        }
                        queue_kms_reinit(&shared);
                        pending_events
                            .lock_safe()
                            .push_back(BackendEvent::ScreenLayoutChanged);
                    }
                })
                .map_err(|e| {
                    BackendError::Message(format!("calloop insert_source(udev) failed: {e}"))
                })?;
        }

        let output_ops: Box<dyn OutputOps> = Box::new(UdevOutputOps {
            shared: shared.clone(),
        });
        let input_ops: Box<dyn InputOps> = Box::new(UdevInputOps {
            shared: shared.clone(),
        });

        let state_ptr: *mut JwmWaylandState = &mut *state;

        // Build the base keycode->keysym translation table used both by JWM (KeyOps) and by
        // the Smithay input filter (for shortcut suppression).
        let mut key_ops_impl = UdevKeyOps::new()?;
        {
            let mut s = shared.lock_safe();
            let mut non_zero = 0usize;
            for kc in 0u16..=255u16 {
                let u8_kc = kc as u8;
                let sym = key_ops_impl.keysym_from_keycode(u8_kc)?;
                s.keysym_table[u8_kc as usize] = sym;
                if sym != 0 {
                    non_zero += 1;
                }
            }

            // If almost everything is NoSymbol, shortcuts will never match.
            if non_zero < 32 {
                log::warn!(
                    "xkb keysym table looks mostly empty (non_zero={non_zero}/256); keyboard shortcuts likely won't work"
                );
            }
        }

        Ok(Self {
            display_handle,
            event_loop: SendWrapper(event_loop),
            state,
            socket_name,
            pending_events,

            flush_tx: flush_tx.clone(),
            flush_pending: flush_pending.clone(),

            shared,
            session,

            kms,
            window_ops: Box::new(WaylandWindowOps {
                state: SendWrapper(state_ptr),
                flush_tx: flush_tx.clone(),
                flush_pending: flush_pending.clone(),
            }),
            input_ops,
            property_ops: Box::new(WaylandPropertyOps {
                state: SendWrapper(state_ptr),
                flush_tx: flush_tx.clone(),
                flush_pending: flush_pending.clone(),
            }),
            output_ops,
            key_ops: Box::new(key_ops_impl),
            cursor_provider: Box::new(DummyCursorProvider),
            color_allocator: Box::new(DummyColorAllocator),

            compositor: None,
            drag: None,
            last_inactive_session_log: None,
            output_management_tx_seq: 0,
            last_output_management_tx: None,
            scratch_tex_updates: Vec::new(),
            scratch_full_scene: Vec::new(),
            offscreen_window_textures: HashMap::new(),
        })
    }
}

fn mods_from_smithay(mods: &ModifiersState) -> Mods {
    let mut out = Mods::empty();

    if mods.shift {
        out |= Mods::SHIFT;
    }
    if mods.ctrl {
        out |= Mods::CONTROL;
    }
    if mods.alt {
        out |= Mods::ALT;
    }
    if mods.logo {
        out |= Mods::SUPER;
    }
    if mods.caps_lock {
        out |= Mods::CAPS;
    }
    if mods.num_lock {
        out |= Mods::NUMLOCK;
    }

    out
}

fn output_bounds(outputs: &[OutputInfo]) -> (i32, i32, i32, i32) {
    if outputs.is_empty() {
        return (1920, 1080, 0, 0);
    }

    let min_x = outputs.iter().map(|o| o.x).min().unwrap_or(0);
    let min_y = outputs.iter().map(|o| o.y).min().unwrap_or(0);
    let max_x = outputs
        .iter()
        .map(|o| o.x + o.width.max(1))
        .max()
        .unwrap_or(min_x + 1920);
    let max_y = outputs
        .iter()
        .map(|o| o.y + o.height.max(1))
        .max()
        .unwrap_or(min_y + 1080);

    ((max_x - min_x).max(1), (max_y - min_y).max(1), min_x, min_y)
}

fn output_management_snapshot(
    outputs: &[OutputInfo],
    soft_disabled: &std::collections::HashSet<String>,
) -> Vec<crate::backend::api::OutputManagementOutputSnapshot> {
    let mut snapshot: Vec<_> = outputs
        .iter()
        .map(
            |output| crate::backend::api::OutputManagementOutputSnapshot {
                name: output.name.clone(),
                stable_key: output.identity.stable_key.clone(),
                enabled: !soft_disabled.contains(&output.name),
                x: output.x,
                y: output.y,
                width: output.width,
                height: output.height,
                scale: output.scale,
                refresh_rate: output.refresh_rate,
            },
        )
        .collect();
    snapshot.sort_by(|a, b| a.name.cmp(&b.name));
    snapshot
}

fn output_management_failure(
    output_name: impl Into<String>,
    reason: String,
) -> crate::backend::api::OutputManagementFailure {
    let mut failure = crate::backend::api::OutputManagementFailure {
        output_name: output_name.into(),
        reason,
        field: None,
        drm_property: None,
        requested_value: None,
    };

    let lower = failure.reason.to_ascii_lowercase();
    if lower.contains("mode") || lower.contains("use_mode") {
        failure.field = Some("mode".into());
        failure.drm_property = Some("MODE_ID".into());
    } else if lower.contains("transform") {
        failure.field = Some("transform".into());
        failure.drm_property = Some("rotation/reflection".into());
    } else if lower.contains("scale") {
        failure.field = Some("scale".into());
    } else if lower.contains("kms") || lower.contains("drm") {
        failure.field = Some("backend".into());
        failure.drm_property = Some("DRM_DEVICE".into());
    }

    failure
}

fn output_at(outputs: &[OutputInfo], x: f64, y: f64) -> Option<OutputId> {
    outputs
        .iter()
        .find(|o| {
            let x = x as i32;
            let y = y as i32;
            x >= o.x && y >= o.y && x < (o.x + o.width) && y < (o.y + o.height)
        })
        .map(|o| o.id)
}

impl CompositorBenchmark for UdevBackend {}

impl BackendDiagnostics for UdevBackend {
    fn compositor_fps(&self) -> f32 {
        self.compositor
            .as_ref()
            .map_or(0.0, |compositor| compositor.fps())
    }

    fn compositor_get_metrics(&self) -> Option<crate::backend::api::CompositorMetrics> {
        self.compositor
            .as_ref()
            .map(|compositor| compositor.get_metrics())
    }

    fn compositor_blur_status(&self) -> Option<crate::backend::api::BlurStatus> {
        self.compositor
            .as_ref()
            .map(|compositor| compositor.get_blur_status())
    }

    fn compositor_direct_scanout_status(&self) -> Option<crate::backend::api::DirectScanoutStatus> {
        let compositor = self.compositor.as_ref()?;
        let kms_outputs = self
            .kms
            .as_ref()
            .map(|kms| kms.borrow().direct_scanout_output_statuses())
            .unwrap_or_default();
        Some(compositor.get_direct_scanout_status(kms_outputs))
    }

    fn compositor_presentation_timing_status(
        &self,
    ) -> Option<crate::backend::api::PresentationTimingStatus> {
        self.kms
            .as_ref()
            .map(|kms| kms.borrow().presentation_timing_status())
    }

    fn compositor_output_management_status(
        &self,
    ) -> Option<crate::backend::api::OutputManagementStatus> {
        let mut soft_disabled: Vec<String> =
            self.state.soft_disabled_outputs.iter().cloned().collect();
        soft_disabled.sort();
        Some(crate::backend::api::OutputManagementStatus {
            pending_ack_count: self.state.pending_output_acks.len(),
            soft_disabled_outputs: soft_disabled,
            last_transaction: self.last_output_management_tx.clone(),
            last_rejected: self.state.last_output_management_rejection.clone(),
        })
    }

    fn compositor_capture_status(&self) -> Option<crate::backend::api::CaptureStatus> {
        let screencopy_pending = self
            .state
            .screencopy_pending
            .as_ref()
            .map(|queue| queue.lock_safe().len())
            .unwrap_or(0);
        let (image_pending, image_output_pending, image_toplevel_pending) = self
            .state
            .image_capture_pending
            .as_ref()
            .map(|queue| {
                let queue = queue.lock_safe();
                let output_count = queue
                    .iter()
                    .filter(|pending| {
                        matches!(
                            &pending.source,
                            crate::backend::wayland_udev::image_copy_capture::CaptureSource::Output(_)
                        )
                    })
                    .count();
                let toplevel_count = queue
                    .iter()
                    .filter(|pending| {
                        matches!(
                            &pending.source,
                            crate::backend::wayland_udev::image_copy_capture::CaptureSource::Toplevel(_)
                        )
                    })
                    .count();
                (queue.len(), output_count, toplevel_count)
            })
            .unwrap_or((0, 0, 0));
        let dmabuf_format_count = self.state.dmabuf_render_formats.len();
        let counters = self.state.capture_counters.lock_safe().clone();

        Some(crate::backend::api::CaptureStatus {
            screencopy: crate::backend::api::CaptureProtocolStatus {
                enabled: self.state.screencopy_pending.is_some(),
                pending_frames: screencopy_pending,
            },
            image_copy_capture: crate::backend::api::CaptureProtocolStatus {
                enabled: self.state.image_capture_pending.is_some(),
                pending_frames: image_pending,
            },
            image_copy_output_pending_frames: image_output_pending,
            image_copy_toplevel_pending_frames: image_toplevel_pending,
            screencopy_queued_total: counters.screencopy_queued_total,
            screencopy_failed_total: counters.screencopy_failed_total,
            screencopy_fulfilled_total: counters.screencopy_fulfilled_total,
            screencopy_render_failed_total: counters.screencopy_render_failed_total,
            image_copy_sessions_total: counters.image_copy_sessions_total,
            image_copy_queued_total: counters.image_copy_queued_total,
            image_copy_failed_total: counters.image_copy_failed_total,
            image_copy_fulfilled_total: counters.image_copy_fulfilled_total,
            image_copy_render_failed_total: counters.image_copy_render_failed_total,
            image_copy_output_queued_total: counters.image_copy_output_queued_total,
            image_copy_toplevel_queued_total: counters.image_copy_toplevel_queued_total,
            last_queued_unix_ms: counters.last_queued_unix_ms,
            last_fulfilled_unix_ms: counters.last_fulfilled_unix_ms,
            last_failed_unix_ms: counters.last_failed_unix_ms,
            last_failure_reason: counters.last_failure_reason.clone(),
            dmabuf_advertised: self.state.dmabuf_main_device.is_some() && dmabuf_format_count > 0,
            dmabuf_format_count,
            cursor_capture_supported: self.state.image_capture_pending.is_some(),
            sensitive_content_masking: false,
            policy: "allow_all_visible_content".into(),
        })
    }

    fn compositor_xwayland_status(&self) -> Option<crate::backend::api::XWaylandStatus> {
        Some(crate::backend::api::XWaylandStatus {
            available: true,
            wm_ready: self.state.x11_wm.is_some(),
            display: std::env::var("DISPLAY").ok(),
            mapped_window_count: self.state.x11_surfaces.len(),
            associated_surface_count: self.state.x11_surfaces.len(),
            pending_association_count: self
                .state
                .x11_surface_to_window
                .len()
                .saturating_sub(self.state.x11_surfaces.len()),
        })
    }

    fn compositor_protocol_bind_counts(&self) -> Vec<crate::backend::api::ProtocolBindStatus> {
        self.state.protocol_bind_counts_snapshot()
    }

    fn compositor_tearing_hint_count(&self) -> usize {
        self.state
            .tearing_hints
            .as_ref()
            .map(|hints| hints.lock_safe().len())
            .unwrap_or(0)
    }

    fn compositor_session_lock_surface_count(&self) -> usize {
        self.state.lock_surfaces.len()
    }

    fn compositor_session_locked(&self) -> bool {
        self.state.session_locked
    }

    fn compositor_color_managed_surfaces(
        &self,
    ) -> Vec<crate::backend::api::ColorManagedSurfaceInfo> {
        let Some(color_manager) = self.state.color_manager.as_ref() else {
            return Vec::new();
        };
        color_manager
            .snapshot_surface_descriptions()
            .into_iter()
            .map(
                |(object_id, record)| crate::backend::api::ColorManagedSurfaceInfo {
                    surface_object_id: format!("{:?}", object_id),
                    identity: record.identity,
                    tf_named: record.params.tf_named,
                    tf_power: record.params.tf_power,
                    primaries_named: record.params.primaries_named,
                    primaries: record.params.primaries,
                    min_lum: record.params.min_lum,
                    max_lum: record.params.max_lum,
                    reference_lum: record.params.reference_lum,
                    mastering_primaries: record.params.mastering_primaries,
                    mastering_min_lum: record.params.mastering_min_lum,
                    mastering_max_lum: record.params.mastering_max_lum,
                    max_cll: record.params.max_cll,
                    max_fall: record.params.max_fall,
                },
            )
            .collect()
    }
}

impl CompositorControl for UdevBackend {
    fn compositor_apply_config(&mut self) {
        if let Some(compositor) = self.compositor.as_mut() {
            compositor.apply_config();
        }
    }

    fn compositor_set_color_temperature(&mut self, temperature: f32) {
        if let Some(compositor) = self.compositor.as_mut() {
            compositor.set_color_temperature(temperature);
        }
    }

    fn compositor_set_saturation(&mut self, saturation: f32) {
        if let Some(compositor) = self.compositor.as_mut() {
            compositor.set_saturation(saturation);
        }
    }

    fn compositor_set_brightness(&mut self, brightness: f32) {
        if let Some(compositor) = self.compositor.as_mut() {
            compositor.set_brightness(brightness);
        }
    }

    fn compositor_set_contrast(&mut self, contrast: f32) {
        if let Some(compositor) = self.compositor.as_mut() {
            compositor.set_contrast(contrast);
        }
    }

    fn compositor_set_invert_colors(&mut self, invert: bool) {
        if let Some(compositor) = self.compositor.as_mut() {
            compositor.set_invert_colors(invert);
        }
    }

    fn compositor_set_grayscale(&mut self, grayscale: bool) {
        if let Some(compositor) = self.compositor.as_mut() {
            compositor.set_grayscale(grayscale);
        }
    }

    fn compositor_set_debug_hud(&mut self, enabled: bool) {
        if let Some(compositor) = self.compositor.as_mut() {
            compositor.set_debug_hud(enabled);
        }
    }

    fn compositor_set_debug_hud_extended(&mut self, enabled: bool) {
        if let Some(compositor) = self.compositor.as_mut() {
            compositor.set_debug_hud_extended(enabled);
        }
    }

    fn compositor_set_transition_mode(&mut self, mode: &str) {
        if let Some(compositor) = self.compositor.as_mut() {
            compositor.set_transition_mode(mode);
        }
    }
}

impl CompositorMedia for UdevBackend {
    fn take_screenshot_to_file(
        &mut self,
        path: &std::path::Path,
    ) -> Result<bool, BackendError> {
        if let Some(kms) = &self.kms {
            kms.borrow_mut().request_screenshot(path.to_path_buf());
            self.state.needs_redraw = true;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn take_screenshot_region_to_file(
        &mut self,
        path: &std::path::Path,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    ) -> Result<bool, BackendError> {
        if let Some(kms) = &self.kms {
            kms.borrow_mut()
                .request_screenshot_region(path.to_path_buf(), x, y, width, height);
            self.state.needs_redraw = true;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn compositor_start_recording(&mut self, path: &str) {
        if let Some(compositor) = self.compositor.as_mut() {
            compositor.start_recording(path);
        }
    }

    fn compositor_stop_recording(&mut self) {
        if let Some(compositor) = self.compositor.as_mut() {
            compositor.stop_recording();
        }
    }

    fn compositor_notify_audio_timing(
        &mut self,
        window: WindowId,
        fps: f32,
        buffer_latency_ms: u32,
    ) {
        if let Some(compositor) = self.compositor.as_mut() {
            compositor.notify_audio_timing(window.raw(), fps, buffer_latency_ms);
        }
    }

    fn compositor_capture_thumbnail(
        &self,
        window: WindowId,
        max_size: u32,
    ) -> Option<(Vec<u8>, u32, u32)> {
        let compositor = self.compositor.as_ref()?;
        let kms = self.kms.as_ref()?;
        kms.borrow_mut()
            .with_renderer(|renderer| unsafe {
                compositor.capture_thumbnail(renderer, window.raw(), max_size)
            })
            .ok()?
    }

    fn compositor_request_live_thumbnail(
        &mut self,
        window: u32,
        max_size: u32,
    ) -> Option<(Vec<u8>, u32, u32)> {
        let compositor = self.compositor.as_ref()?;
        let kms = self.kms.as_ref()?;
        kms.borrow_mut()
            .with_renderer(|renderer| unsafe {
                compositor.capture_thumbnail(renderer, u64::from(window), max_size)
            })
            .ok()?
    }
}

impl CompositorWorkspaceEffects for UdevBackend {
    fn compositor_notify_tag_switch(
        &mut self,
        duration: Duration,
        direction: i32,
        exclude_top: u32,
        monitor_rect: (i32, i32, u32, u32),
    ) {
        if let Some(compositor) = self.compositor.as_mut() {
            compositor.notify_tag_switch(duration, direction, exclude_top, monitor_rect);
        }
    }

    fn compositor_set_magnifier(&mut self, enabled: bool) {
        if let Some(compositor) = self.compositor.as_mut() {
            compositor.set_magnifier(enabled);
        }
    }

    fn compositor_set_snap_preview(&mut self, preview: Option<(f32, f32, f32, f32)>) {
        if let Some(compositor) = self.compositor.as_mut() {
            compositor.set_snap_preview(preview);
        }
    }

    fn compositor_clear_snap_preview_immediate(&mut self) {
        if let Some(compositor) = self.compositor.as_mut() {
            compositor.clear_snap_preview_immediate();
        }
    }

    fn compositor_set_overview_mode(
        &mut self,
        active: bool,
        windows: &[(WindowId, f32, f32, f32, f32, bool, String)],
    ) {
        if let Some(compositor) = self.compositor.as_mut() {
            let entries = windows
                .iter()
                .map(|(window, x, y, width, height, selected, title)| {
                    (window.raw(), *x, *y, *width, *height, *selected, title.clone())
                })
                .collect::<Vec<_>>();
            compositor.set_overview_mode(active, &entries);
            self.request_render();
        }
    }

    fn compositor_set_overview_monitor(&mut self, x: i32, y: i32, width: u32, height: u32) {
        if let Some(compositor) = self.compositor.as_mut() {
            compositor.set_overview_monitor(x, y, width, height);
            self.request_render();
        }
    }

    fn compositor_set_monitors(&mut self, monitors: &[(u32, i32, i32, u32, u32, u32)]) {
        let refresh_rates = {
            let shared = self.shared.lock_safe();
            monitors
                .iter()
                .map(|&(id, x, y, _, _, _)| {
                    let hz = shared
                        .outputs
                        .iter()
                        .find(|output| output.x == x && output.y == y)
                        .map(|output| (output.refresh_rate / 1000).max(1))
                        .unwrap_or(60);
                    (id, hz)
                })
                .collect::<Vec<_>>()
        };
        if let Some(compositor) = self.compositor.as_mut() {
            compositor.set_monitors(monitors);
            compositor.apply_per_monitor_refresh_rates(&refresh_rates);
        }
    }

    fn compositor_set_overview_selection(&mut self, window: WindowId) {
        if let Some(compositor) = self.compositor.as_mut() {
            compositor.set_overview_selection(window.raw());
            self.request_render();
        }
    }

    fn compositor_set_expose_mode(
        &mut self,
        active: bool,
        windows: Vec<(WindowId, i32, i32, u32, u32)>,
    ) {
        if let Some(compositor) = self.compositor.as_mut() {
            let entries = windows
                .iter()
                .map(|(window, x, y, width, height)| {
                    (window.raw(), *x, *y, *width, *height)
                })
                .collect();
            compositor.set_expose_mode(active, entries);
        }
    }

    fn compositor_expose_click(&mut self, x: f32, y: f32) -> Option<WindowId> {
        self.compositor
            .as_ref()?
            .expose_click(x, y)
            .map(WindowId::from_raw)
    }
}

impl CompositorWindowEffects for UdevBackend {
    fn compositor_set_frame_extents(
        &mut self,
        window: WindowId,
        left: u32,
        right: u32,
        top: u32,
        bottom: u32,
    ) {
        if let Some(compositor) = self.compositor.as_mut() {
            compositor.set_frame_extents(window.raw(), left, right, top, bottom);
        }
    }

    fn compositor_set_window_shaped(&mut self, window: WindowId, shaped: bool) {
        if let Some(compositor) = self.compositor.as_mut() {
            compositor.set_window_shaped(window.raw(), shaped);
        }
    }

    fn compositor_set_window_urgent(&mut self, window: WindowId, urgent: bool) {
        if let Some(compositor) = self.compositor.as_mut() {
            compositor.set_window_urgent(window.raw(), urgent);
        }
    }

    fn compositor_set_window_pip(&mut self, window: WindowId, pip: bool) {
        if let Some(compositor) = self.compositor.as_mut() {
            compositor.set_window_pip(window.raw(), pip);
        }
    }

    fn compositor_force_full_redraw(&mut self) { if let Some(c) = self.compositor.as_mut() { c.force_full_redraw(); } }
    fn compositor_set_mouse_position(&mut self, x: f32, y: f32) { if let Some(c) = self.compositor.as_mut() { c.set_mouse_position(x, y); } }
    fn compositor_deactivate_edge_glow(&mut self) { if let Some(c) = self.compositor.as_mut() { c.deactivate_edge_glow(); } }
    fn compositor_unsuppress_edge_glow(&mut self) { if let Some(c) = self.compositor.as_mut() { c.unsuppress_edge_glow(); } }
    fn compositor_notify_window_move_start(&mut self, window: WindowId) { if let Some(c) = self.compositor.as_mut() { c.notify_window_move_start(window.raw()); } }
    fn compositor_notify_window_move_delta(&mut self, window: WindowId, dx: f32, dy: f32) { if let Some(c) = self.compositor.as_mut() { c.notify_window_move_delta(window.raw(), dx, dy); } }
    fn compositor_notify_window_move_end(&mut self, window: WindowId) { if let Some(c) = self.compositor.as_mut() { c.notify_window_move_end(window.raw()); } }
    fn compositor_set_dock_position(&mut self, x: f32, y: f32) { if let Some(c) = self.compositor.as_mut() { c.set_dock_position(x, y); } }
    fn compositor_set_peek_mode(&mut self, active: bool) { if let Some(c) = self.compositor.as_mut() { c.set_peek_mode(active); } }
    fn compositor_set_window_groups(&mut self, groups: Vec<(u32, Vec<(u32, String, bool)>)>) { if let Some(c) = self.compositor.as_mut() { c.set_window_groups(groups); } }
    fn compositor_zoom_to_fit(&mut self, window: Option<u32>) { if let Some(c) = self.compositor.as_mut() { c.zoom_to_fit(window); } }
}

impl CompositorAnnotation for UdevBackend {
    fn compositor_set_colorblind_mode(&mut self, mode: &str) { if let Some(c) = self.compositor.as_mut() { c.set_colorblind_mode(mode); } }
    fn compositor_set_annotation_mode(&mut self, active: bool) { if let Some(c) = self.compositor.as_mut() { c.set_annotation_mode(active); } }
    fn compositor_set_annotation_color(&mut self, rgba: [f32; 4]) { if let Some(c) = self.compositor.as_mut() { c.set_annotation_color(rgba[0], rgba[1], rgba[2], rgba[3]); } }
    fn compositor_set_annotation_line_width(&mut self, width: f32) { if let Some(c) = self.compositor.as_mut() { c.set_annotation_line_width(width); } }
    fn compositor_annotation_add_point(&mut self, x: f32, y: f32) { if let Some(c) = self.compositor.as_mut() { c.annotation_add_point(x, y); } }
    fn compositor_annotation_begin_stroke(&mut self) { if let Some(c) = self.compositor.as_mut() { c.annotation_new_stroke(); } }
}

impl DisplayControl for UdevBackend {
    fn query_vrr_capabilities(&self, output: OutputId) -> Option<crate::backend::api::VrrCapabilities> {
        let kms = self.kms.as_ref()?;
        let shared = self.shared.lock_safe();
        let index = shared.outputs.iter().position(|candidate| candidate.id == output)?;
        drop(shared);
        kms.borrow_mut().query_vrr_for_output(index)
    }
    fn query_kms_color_pipeline_caps(&self, output: OutputId) -> Option<crate::backend::api::KmsColorPipelineCaps> {
        let kms = self.kms.as_ref()?;
        let shared = self.shared.lock_safe();
        let index = shared.outputs.iter().position(|candidate| candidate.id == output)?;
        drop(shared);
        kms.borrow_mut().query_color_pipeline_caps_for_output(index)
    }
    fn set_vrr_enabled(&mut self, output: OutputId, enabled: bool) -> Result<(), BackendError> {
        let kms = self.kms.as_ref().ok_or(BackendError::Unsupported("no KMS"))?;
        let shared = self.shared.lock_safe();
        let index = shared.outputs.iter().position(|candidate| candidate.id == output).ok_or(BackendError::NotFound("output not found"))?;
        drop(shared);
        kms.borrow_mut().set_vrr_for_output(index, enabled).map_err(BackendError::Message)
    }
    fn set_hdr_metadata(&mut self, output: OutputId, enabled: bool) -> Result<(), BackendError> {
        let kms = self.kms.as_ref().ok_or(BackendError::Unsupported("no KMS"))?;
        let shared = self.shared.lock_safe();
        let info = shared.outputs.iter().find(|candidate| candidate.id == output).ok_or(BackendError::NotFound("output not found"))?;
        let index = shared.outputs.iter().position(|candidate| candidate.id == output).ok_or(BackendError::NotFound("output not found"))?;
        let caps = info.hdr_metadata.clone();
        drop(shared);
        if enabled {
            let caps = caps.ok_or(BackendError::Unsupported("output does not advertise HDR in EDID"))?;
            let peak = crate::config::CONFIG.load().behavior().hdr_peak_nits.round().clamp(0.0, u16::MAX as f32) as u16;
            let blob = crate::backend::hdr_metadata::build_from_edid(&caps, peak);
            kms.borrow_mut().set_hdr_metadata_for_output(index, Some(&blob)).map_err(BackendError::Message)
        } else {
            kms.borrow_mut().set_hdr_metadata_for_output(index, None).map_err(BackendError::Message)
        }
    }
}

impl RenderScheduler for UdevBackend {
    fn request_render(&mut self) {
        self.state.needs_redraw = true;
        if let Some(kms) = &self.kms { kms.borrow_mut().request_render(); }
        self.request_flush();
    }
    fn has_compositor(&self) -> bool { self.compositor.is_some() }
    fn compositor_needs_render(&self) -> bool {
        self.state.needs_redraw || self.compositor.as_ref().is_some_and(|c| c.needs_render())
    }
}

impl Backend for UdevBackend {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            can_warp_pointer: false,
            supports_client_list: false,
        }
    }

    fn root_window(&self) -> Option<WindowId> {
        Some(WindowId::from_raw(0))
    }

    fn check_existing_wm(&self) -> Result<(), BackendError> {
        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn on_focused_client_changed(&mut self, win: Option<WindowId>) -> Result<(), BackendError> {
        match win {
            Some(w) => self.window_ops.set_input_focus(w)?,
            None => self.window_ops.set_input_focus_root()?,
        }

        self.state.set_active_toplevel(win);
        self.state.needs_redraw = true;
        self.request_flush();
        Ok(())
    }

    fn begin_move(&mut self, win: WindowId) -> Result<(), BackendError> {
        let geom = self.window_ops.get_geometry(win)?;
        let (rx, ry) = self.input_ops.get_pointer_position()?;
        let _ = self.input_ops.set_cursor(StdCursorKind::Hand);
        let _ = self.input_ops.grab_pointer(0, None)?;
        self.drag = Some(UdevDragState {
            win,
            start_geom: geom,
            start_root_x: rx,
            start_root_y: ry,
            action: UdevDragAction::Move,
        });
        self.state.needs_redraw = true;
        self.request_flush();
        Ok(())
    }

    fn begin_resize(&mut self, win: WindowId, edge: ResizeEdge) -> Result<(), BackendError> {
        let geom = self.window_ops.get_geometry(win)?;
        let (rx, ry) = self.input_ops.get_pointer_position()?;
        let cursor_kind = match edge {
            ResizeEdge::Top | ResizeEdge::Bottom => StdCursorKind::VDoubleArrow,
            ResizeEdge::Left | ResizeEdge::Right => StdCursorKind::HDoubleArrow,
            ResizeEdge::TopLeft => StdCursorKind::TopLeftCorner,
            ResizeEdge::TopRight => StdCursorKind::TopRightCorner,
            ResizeEdge::BottomLeft => StdCursorKind::BottomLeftCorner,
            ResizeEdge::BottomRight => StdCursorKind::BottomRightCorner,
        };
        let _ = self.input_ops.set_cursor(cursor_kind);
        let _ = self.input_ops.grab_pointer(0, None)?;
        self.drag = Some(UdevDragState {
            win,
            start_geom: geom,
            start_root_x: rx,
            start_root_y: ry,
            action: UdevDragAction::Resize(edge),
        });
        self.state.needs_redraw = true;
        self.request_flush();
        Ok(())
    }

    fn handle_motion(&mut self, x: f64, y: f64, _time: u32) -> Result<bool, BackendError> {
        let Some(state) = self.drag else {
            return Ok(false);
        };

        let dx = (x - state.start_root_x) as i32;
        let dy = (y - state.start_root_y) as i32;

        match state.action {
            UdevDragAction::Move => {
                let new_x = state.start_geom.x + dx;
                let new_y = state.start_geom.y + dy;
                self.window_ops.set_position(state.win, new_x, new_y)?;
            }
            UdevDragAction::Resize(edge) => {
                let mut new_x = state.start_geom.x;
                let mut new_y = state.start_geom.y;
                let mut new_w = state.start_geom.w as i32;
                let mut new_h = state.start_geom.h as i32;

                match edge {
                    ResizeEdge::Top => {
                        new_y = state.start_geom.y + dy;
                        new_h = state.start_geom.h as i32 - dy;
                    }
                    ResizeEdge::Bottom => {
                        new_h = state.start_geom.h as i32 + dy;
                    }
                    ResizeEdge::Left => {
                        new_x = state.start_geom.x + dx;
                        new_w = state.start_geom.w as i32 - dx;
                    }
                    ResizeEdge::Right => {
                        new_w = state.start_geom.w as i32 + dx;
                    }
                    ResizeEdge::TopLeft => {
                        new_x = state.start_geom.x + dx;
                        new_w = state.start_geom.w as i32 - dx;
                        new_y = state.start_geom.y + dy;
                        new_h = state.start_geom.h as i32 - dy;
                    }
                    ResizeEdge::TopRight => {
                        new_w = state.start_geom.w as i32 + dx;
                        new_y = state.start_geom.y + dy;
                        new_h = state.start_geom.h as i32 - dy;
                    }
                    ResizeEdge::BottomLeft => {
                        new_x = state.start_geom.x + dx;
                        new_w = state.start_geom.w as i32 - dx;
                        new_h = state.start_geom.h as i32 + dy;
                    }
                    ResizeEdge::BottomRight => {
                        new_w = state.start_geom.w as i32 + dx;
                        new_h = state.start_geom.h as i32 + dy;
                    }
                }

                // Keep the window in a valid state.
                new_w = new_w.max(1);
                new_h = new_h.max(1);

                self.window_ops.configure(
                    state.win,
                    new_x,
                    new_y,
                    new_w as u32,
                    new_h as u32,
                    state.start_geom.border,
                )?;
            }
        }

        Ok(true)
    }

    fn handle_button_release(&mut self, _time: u32) -> Result<bool, BackendError> {
        if self.drag.is_some() {
            self.drag = None;
            let _ = self.input_ops.ungrab_pointer();
            let _ = self.input_ops.set_cursor(StdCursorKind::LeftPtr);
            self.state.needs_redraw = true;
            self.request_flush();
            return Ok(true);
        }
        Ok(false)
    }

    fn window_ops(&self) -> &dyn WindowOps {
        &*self.window_ops
    }
    fn input_ops(&self) -> &dyn InputOps {
        &*self.input_ops
    }

    fn property_ops(&self) -> &dyn PropertyOps {
        &*self.property_ops
    }
    fn output_ops(&self) -> &dyn OutputOps {
        &*self.output_ops
    }
    fn key_ops(&self) -> &dyn KeyOps {
        &*self.key_ops
    }
    fn key_ops_mut(&mut self) -> &mut dyn KeyOps {
        &mut *self.key_ops
    }
    fn cursor_provider(&mut self) -> &mut dyn CursorProvider {
        &mut *self.cursor_provider
    }
    fn color_allocator(&mut self) -> &mut dyn ColorAllocator {
        &mut *self.color_allocator
    }

    fn has_partial_damage(&self) -> bool {
        self.compositor
            .as_ref()
            .map_or(false, |c| c.partial_damage_enabled())
    }

    fn set_partial_damage(&mut self, enabled: bool) -> Result<bool, BackendError> {
        match self.compositor.as_mut() {
            Some(c) => {
                c.set_partial_damage(enabled);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn set_compositor_enabled(&mut self, enabled: bool) -> Result<bool, BackendError> {
        if enabled && self.compositor.is_none() {
            if let Some(kms) = &self.kms {
                let mut kms_ref = kms.borrow_mut();
                let (w, h) = kms_ref.total_screen_size();
                let hdr_10bit = kms_ref.supports_10bit();
                match kms_ref
                    .with_renderer(|gl| unsafe { WaylandCompositor::new(gl, w, h, hdr_10bit) })
                {
                    Ok(Ok(compositor)) => {
                        self.compositor = Some(compositor);
                        return Ok(true);
                    }
                    Ok(Err(e)) => {
                        log::error!("Failed to create wayland compositor: {}", e);
                        return Ok(false);
                    }
                    Err(e) => {
                        log::error!("Failed to access GL context: {:?}", e);
                        return Ok(false);
                    }
                }
            }
            Ok(false)
        } else if !enabled && self.compositor.is_some() {
            self.compositor = None;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn compositor_render_frame(
        &mut self,
        scene: &[(u64, i32, i32, u32, u32)],
        focused_window: Option<u64>,
    ) -> Result<bool, BackendError> {
        if self.compositor.is_none() || self.kms.is_none() {
            return Ok(false);
        }

        // Evict compositor state for windows whose client surface was destroyed.
        // Without this the per-window maps grow unbounded for the process lifetime.
        if !self.state.compositor_dead_windows.is_empty() {
            if let Some(compositor) = self.compositor.as_mut() {
                for dead in self.state.compositor_dead_windows.drain(..) {
                    compositor.remove_window(dead);
                    self.offscreen_window_textures.remove(&dead);
                }
            } else {
                self.state.compositor_dead_windows.clear();
            }
        }

        // Phase 1: Import surface textures into GL cache (borrow kms only, no compositor borrow).
        // Reuse a persistent scratch buffer (taken out so we can freely borrow
        // other self fields below) instead of allocating one each frame.
        let mut tex_updates = std::mem::take(&mut self.scratch_tex_updates);
        tex_updates.clear();
        // Taken out so the per-window loop can borrow it mutably while `self.kms`
        // is also borrowed; restored at the end of Phase 1.
        let mut offscreen = std::mem::take(&mut self.offscreen_window_textures);
        let mut flush_after_resize_configure = false;
        // Rate-limit diagnostic logging: log at most once per second.
        static LAST_CRF_LOG: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let crf_now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        // Per-window texture diagnostics are useful when debugging a client,
        // but walking every surface and formatting its state once per second
        // is unnecessary work in normal sessions.
        let crf_log_this = log::log_enabled!(log::Level::Debug) && !scene.is_empty() && {
            let prev = LAST_CRF_LOG.load(std::sync::atomic::Ordering::Relaxed);
            if crf_now_secs > prev {
                LAST_CRF_LOG.store(crf_now_secs, std::sync::atomic::Ordering::Relaxed);
                true
            } else {
                false
            }
        };

        if let Some(kms) = &self.kms {
            let mut kms_ref = kms.borrow_mut();
            for &(win_id, _, _, w, h) in scene {
                let win = crate::backend::common_define::WindowId::from_raw(win_id);
                let surface_opt = self.state.surface_for_window(win);
                if crf_log_this {
                    let is_x11 = self.state.x11_surfaces.contains_key(&win);
                    let class = self
                        .state
                        .window_app_id
                        .get(&win)
                        .map(|s| s.as_str())
                        .unwrap_or("");
                    let children = surface_opt
                        .as_ref()
                        .map(|s| get_children(s).len())
                        .unwrap_or(0);
                    log::debug!(
                        "[crf] win={win_id:#x} class={class:?} x11={is_x11} surface={} subsurfaces={children} size={w}x{h}",
                        surface_opt.is_some()
                    );
                }
                if let Some(surface) = surface_opt {
                    let has_viewport_state = with_states(&surface, |states| {
                        let mut cached = states.cached_state.get::<ViewportCachedState>();
                        let current = cached.current();
                        current.src.is_some() || current.dst.is_some()
                    });
                    // Electron/CEF clients (e.g. feishu) render their content into
                    // wl_subsurfaces while the root toplevel surface carries no
                    // buffer. Reading only the root surface therefore yields a
                    // transparent window. When subsurfaces are present, composite
                    // the whole surface tree into a single per-window offscreen
                    // texture so all existing per-window effects keep working.
                    //
                    // Clients that use wp_viewport (notably iced/wgpu) also need
                    // this path: the raw texture fast path bypasses Smithay's
                    // viewport/fractional-scale handling and can leave the
                    // visible content stale even though buffers keep committing.
                    if !get_children(&surface).is_empty() || has_viewport_state {
                        let (gx, gy, cw, ch) = with_states(&surface, |states| {
                            let mut cached = states.cached_state.get::<SurfaceCachedState>();
                            match cached.current().geometry {
                                Some(r) if r.size.w > 0 && r.size.h > 0 => {
                                    (r.loc.x, r.loc.y, r.size.w, r.size.h)
                                }
                                _ => (0, 0, w as i32, h as i32),
                            }
                        });
                        let committed_w = cw.max(1) as u32;
                        let committed_h = ch.max(1) as u32;
                        let wait_for_configured_size = has_viewport_state
                            && (committed_w.abs_diff(w) > 4 || committed_h.abs_diff(h) > 4);
                        if wait_for_configured_size {
                            self.state.enforce_toplevel_configure_size(win, &surface);
                            self.state.needs_redraw = true;
                            flush_after_resize_configure = true;
                            if crf_log_this {
                                log::debug!(
                                    "[crf] win={win_id:#x} viewport size mismatch committed={}x{} scene={}x{}; keeping previous texture while deferring update",
                                    committed_w,
                                    committed_h,
                                    w,
                                    h
                                );
                            }
                            continue;
                        }
                        let (target_w, target_h) = (cw.max(1), ch.max(1));
                        let composited = kms_ref.with_gles_renderer(|renderer| {
                            let _ = import_surface_tree(renderer, &surface);
                            let elements: Vec<kms::KmsRenderElement> =
                                render_elements_from_surface_tree(
                                    renderer,
                                    &surface,
                                    Point::<i32, Physical>::from((-gx, -gy)),
                                    Scale::from(1.0f64),
                                    1.0f32,
                                    Kind::Unspecified,
                                );
                            let need_new = match offscreen.get(&win_id) {
                                Some((_, ow, oh)) => {
                                    *ow != target_w as u32 || *oh != target_h as u32
                                }
                                None => true,
                            };
                            if need_new {
                                match Offscreen::<GlesTexture>::create_buffer(
                                    renderer,
                                    Fourcc::Abgr8888,
                                    Size::<i32, _>::from((target_w, target_h)),
                                ) {
                                    Ok(t) => {
                                        offscreen.insert(
                                            win_id,
                                            (t, target_w as u32, target_h as u32),
                                        );
                                    }
                                    Err(e) => {
                                        log::error!(
                                            "[crf] win={win_id:#x} offscreen create {target_w}x{target_h} failed: {e:?}"
                                        );
                                        return None;
                                    }
                                }
                            }
                            let (tex, _, _) = offscreen.get_mut(&win_id)?;
                            let mut target = match renderer.bind(tex) {
                                Ok(t) => t,
                                Err(e) => {
                                    log::error!("[crf] win={win_id:#x} offscreen bind failed: {e:?}");
                                    return None;
                                }
                            };
                            let phys: Size<i32, Physical> = (target_w, target_h).into();
                            let mut dt = OutputDamageTracker::new(
                                phys,
                                Scale::from(1.0f64),
                                Transform::Normal,
                            );
                            // age=0 forces a full redraw; transparent clear so
                            // uncovered areas stay see-through.
                            if let Err(e) = dt.render_output(
                                renderer,
                                &mut target,
                                0,
                                &elements,
                                Color32F::new(0.0, 0.0, 0.0, 0.0),
                            ) {
                                log::error!("[crf] win={win_id:#x} offscreen render failed: {e:?}");
                                return None;
                            }
                            drop(target);
                            Some(tex.tex_id())
                        });
                        if let Some(tid) = composited {
                            if crf_log_this {
                                log::debug!(
                                    "[crf] win={win_id:#x} composited surface tree -> tex={tid} {target_w}x{target_h}"
                                );
                            }
                            let opaque_target = Rectangle::<i32, Logical>::new(
                                (gx, gy).into(),
                                (cw.max(1), ch.max(1)).into(),
                            );
                            let has_alpha = !surface_declares_opaque_rect(&surface, opaque_target);
                            // render_output flips Y in its projection, so the
                            // offscreen is stored top-to-bottom => y_inverted=false.
                            // Content is already cropped to geometry => full UV.
                            tex_updates.push((
                                win_id,
                                tid,
                                target_w as u32,
                                target_h as u32,
                                has_alpha,
                                false,
                                [0.0, 0.0, 1.0, 1.0],
                            ));
                        }
                        continue;
                    }
                    let tid = kms_ref.with_gles_renderer(|renderer| {
                        let import_result = import_surface_tree(renderer, &surface);
                        let ctx_id = renderer.context_id();
                        let tex = with_states(&surface, |states| {
                            let rsd = states.data_map.get::<RendererSurfaceStateUserData>();
                            // Single lock per surface: derive both the texture
                            // info (hot path) and the rate-limited buffer-type
                            // diagnostic from the same guard so the logging gate
                            // never takes a second lock on the same surface.
                            let (tex_info, log_buf) = match rsd {
                                Some(d) => {
                                    let locked = d.lock_safe();
                                    let tex_info = locked.texture::<GlesTexture>(ctx_id).map(|t| {
                                        let has_alpha = locked
                                            .buffer()
                                            .and_then(|b| buffer_has_alpha(&**b))
                                            .unwrap_or(true);
                                        let tex_size = t.size();
                                        (t.tex_id(), t.is_y_inverted(), has_alpha, tex_size)
                                    });
                                    let log_buf = if crf_log_this {
                                        let buf = locked.buffer();
                                        let has_buf = buf.is_some();
                                        let type_str = buf
                                            .and_then(|b| buffer_type(&**b))
                                            .map(|t| match t {
                                                BufferType::Shm => "shm",
                                                BufferType::Dma => "dma",
                                                BufferType::Egl => "egl",
                                                BufferType::SinglePixel => "single",
                                                _ => "other",
                                            })
                                            .unwrap_or("unknown");
                                        Some((has_buf, type_str))
                                    } else {
                                        None
                                    };
                                    (tex_info, log_buf)
                                }
                                None => (None, crf_log_this.then_some((false, "no_rsd"))),
                            };
                            if crf_log_this {
                                let (has_buf, buf_type_str) = log_buf.unwrap_or((false, "no_rsd"));
                                log::debug!("[crf] win={win_id:#x} import_ok={} has_buf={has_buf} buf={buf_type_str} tex={:?} y_inv={:?} alpha={:?}",
                                    import_result.is_ok(),
                                    tex_info.map(|(id, _, _, _)| id),
                                    tex_info.map(|(_, yi, _, _)| yi),
                                    tex_info.map(|(_, _, alpha, _)| alpha));
                            }
                            tex_info
                        });
                        tex
                    });
                    if let Some((tid, y_inverted, has_alpha, tex_size)) = tid {
                        // Compute content UV sub-rect from xdg geometry offset and texture size
                        let geo_offset = with_states(&surface, |states| {
                            let mut cached = states.cached_state.get::<SurfaceCachedState>();
                            cached
                                .current()
                                .geometry
                                .map(|r| (r.loc.x, r.loc.y))
                                .unwrap_or((0, 0))
                        });
                        let tex_w = tex_size.w as f32;
                        let tex_h = tex_size.h as f32;
                        let content_uv = if tex_w > 0.0 && tex_h > 0.0 {
                            let u = geo_offset.0 as f32 / tex_w;
                            let v = geo_offset.1 as f32 / tex_h;
                            let uw = w as f32 / tex_w;
                            let uh = h as f32 / tex_h;
                            [u, v, uw.min(1.0 - u), uh.min(1.0 - v)]
                        } else {
                            [0.0, 0.0, 1.0, 1.0]
                        };
                        let opaque_target = Rectangle::<i32, Logical>::new(
                            geo_offset.into(),
                            (w.max(1) as i32, h.max(1) as i32).into(),
                        );
                        let has_alpha =
                            has_alpha && !surface_declares_opaque_rect(&surface, opaque_target);
                        if crf_log_this {
                            log::debug!(
                                "[crf] win={win_id:#x} tex_size={}x{} scene={}x{} uv={:?} effective_alpha={has_alpha}",
                                tex_size.w,
                                tex_size.h,
                                w,
                                h,
                                content_uv
                            );
                        }
                        tex_updates.push((
                            win_id,
                            tid,
                            tex_size.w.max(1) as u32,
                            tex_size.h.max(1) as u32,
                            has_alpha,
                            y_inverted,
                            content_uv,
                        ));
                    }
                }
            }
        }
        if flush_after_resize_configure {
            self.request_flush();
        }
        if crf_log_this || (!scene.is_empty() && tex_updates.len() != scene.len()) {
            log::debug!(
                "[crf] tex_updates={} scene={}",
                tex_updates.len(),
                scene.len()
            );
        }

        // Phase 1b: Import xdg_popup surface textures and append to scene.
        let xdg_popups = self.state.xdg_popup_positions();
        let mut full_scene = std::mem::take(&mut self.scratch_full_scene);
        full_scene.clear();
        full_scene.extend_from_slice(scene);
        if let Some(kms) = &self.kms {
            let mut kms_ref = kms.borrow_mut();
            for (popup_surface, abs_x, abs_y, pw, ph) in &xdg_popups {
                let popup_win_id =
                    0xFE00_0000_0000_0000u64 | (popup_surface.id().protocol_id() as u64);
                let tid = kms_ref.with_gles_renderer(|renderer| {
                    let _ = import_surface_tree(renderer, popup_surface);
                    let ctx_id = renderer.context_id();
                    with_states(popup_surface, |states| {
                        let rsd = states.data_map.get::<RendererSurfaceStateUserData>();
                        rsd.and_then(|d| {
                            let locked = d.lock_safe();
                            locked.texture::<GlesTexture>(ctx_id).map(|t| {
                                let has_alpha = locked
                                    .buffer()
                                    .and_then(|b| buffer_has_alpha(&**b))
                                    .unwrap_or(true);
                                let tex_size = t.size();
                                (t.tex_id(), t.is_y_inverted(), has_alpha, tex_size)
                            })
                        })
                    })
                });
                if let Some((tid, y_inverted, has_alpha, tex_size)) = tid {
                    let w = tex_size.w as u32;
                    let h = tex_size.h as u32;
                    if w > 0 && h > 0 {
                        let geo_offset = with_states(popup_surface, |states| {
                            let mut cached = states.cached_state.get::<SurfaceCachedState>();
                            cached
                                .current()
                                .geometry
                                .map(|r| (r.loc.x, r.loc.y))
                                .unwrap_or((0, 0))
                        });
                        let tex_w = tex_size.w as f32;
                        let tex_h = tex_size.h as f32;
                        let content_uv = if tex_w > 0.0 && tex_h > 0.0 {
                            let u = geo_offset.0 as f32 / tex_w;
                            let v = geo_offset.1 as f32 / tex_h;
                            let uw = *pw as f32 / tex_w;
                            let uh = *ph as f32 / tex_h;
                            [u, v, uw.min(1.0 - u), uh.min(1.0 - v)]
                        } else {
                            [0.0, 0.0, 1.0, 1.0]
                        };
                        if crf_log_this {
                            log::info!(
                                "[crf] popup win={popup_win_id:#x} tex_size={}x{} scene={}x{} uv={:?}",
                                tex_size.w,
                                tex_size.h,
                                pw,
                                ph,
                                content_uv
                            );
                        }
                        tex_updates.push((
                            popup_win_id,
                            tid,
                            w,
                            h,
                            has_alpha,
                            y_inverted,
                            content_uv,
                        ));
                        full_scene.push((popup_win_id, *abs_x, *abs_y, *pw, *ph));
                    }
                }
            }
        }

        // Phase 1c: Import IME popup surface textures and append to scene.
        let im_popups = self.state.im_popup_positions();
        // Anchor the candidate box below the text cursor; if it would overflow the
        // parent window's bottom edge, flip it above the cursor instead, and clamp
        // horizontally so it stays on-screen. `off_x`/`off_y` carry the composited
        // tree's bbox origin for the subsurface path (0,0 for the plain path).
        let place_popup = |a: &crate::backend::wayland::state::ImPopupAnchor,
                           w: i32,
                           h: i32,
                           off_x: i32,
                           off_y: i32|
         -> (i32, i32) {
            let tx = (a.x + off_x).min(a.area_right - w).max(a.area_left);
            let below = a.cursor_bottom + off_y;
            let ty = if below + h <= a.area_bottom {
                below
            } else {
                (a.cursor_top - h).max(a.area_top)
            };
            (tx, ty)
        };
        if let Some(kms) = &self.kms {
            let mut kms_ref = kms.borrow_mut();
            for anchor in &im_popups {
                let im_surface = &anchor.surface;
                let im_win_id = 0xFF00_0000_0000_0000u64 | (im_surface.id().protocol_id() as u64);

                // fcitx5 (and other IMEs) draw the candidate list into wl_subsurfaces
                // of the input-popup surface; the root surface only carries a tiny
                // pre-edit box. Reading just the root texture therefore renders a
                // near-invisible window. When subsurfaces are present, composite the
                // whole tree into one offscreen texture, mirroring the toplevel path.
                if !get_children(im_surface).is_empty() {
                    let bbox =
                        bbox_from_surface_tree(im_surface, Point::<i32, Logical>::from((0, 0)));
                    let (cw, ch) = (bbox.size.w.max(1), bbox.size.h.max(1));
                    let composited = kms_ref.with_gles_renderer(|renderer| {
                        let _ = import_surface_tree(renderer, im_surface);
                        let elements: Vec<kms::KmsRenderElement> =
                            render_elements_from_surface_tree(
                                renderer,
                                im_surface,
                                Point::<i32, Physical>::from((-bbox.loc.x, -bbox.loc.y)),
                                Scale::from(1.0f64),
                                1.0f32,
                                Kind::Unspecified,
                            );
                        let need_new = match offscreen.get(&im_win_id) {
                            Some((_, ow, oh)) => *ow != cw as u32 || *oh != ch as u32,
                            None => true,
                        };
                        if need_new {
                            match Offscreen::<GlesTexture>::create_buffer(
                                renderer,
                                Fourcc::Abgr8888,
                                Size::<i32, _>::from((cw, ch)),
                            ) {
                                Ok(t) => {
                                    offscreen.insert(im_win_id, (t, cw as u32, ch as u32));
                                }
                                Err(e) => {
                                    log::error!(
                                        "[ime] popup offscreen create {cw}x{ch} failed: {e:?}"
                                    );
                                    return None;
                                }
                            }
                        }
                        let (tex, _, _) = offscreen.get_mut(&im_win_id)?;
                        let mut target = match renderer.bind(tex) {
                            Ok(t) => t,
                            Err(e) => {
                                log::error!("[ime] popup offscreen bind failed: {e:?}");
                                return None;
                            }
                        };
                        let phys: Size<i32, Physical> = (cw, ch).into();
                        let mut dt =
                            OutputDamageTracker::new(phys, Scale::from(1.0f64), Transform::Normal);
                        if let Err(e) = dt.render_output(
                            renderer,
                            &mut target,
                            0,
                            &elements,
                            Color32F::new(0.0, 0.0, 0.0, 0.0),
                        ) {
                            log::error!("[ime] popup offscreen render failed: {e:?}");
                            return None;
                        }
                        drop(target);
                        Some(tex.tex_id())
                    });
                    if let Some(tid) = composited {
                        // bbox.loc shifts the tree origin relative to the root surface;
                        // fold it into the anchor so subsurfaces land correctly, then
                        // clamp so the candidate box stays on the parent's monitor.
                        let (sx, sy) = place_popup(anchor, cw, ch, bbox.loc.x, bbox.loc.y);
                        log::info!(
                            "[ime] popup composited tex={tid} {cw}x{ch} children={} abs=({sx},{sy})",
                            get_children(im_surface).len()
                        );
                        tex_updates.push((
                            im_win_id,
                            tid,
                            cw as u32,
                            ch as u32,
                            true,
                            false,
                            [0.0, 0.0, 1.0, 1.0],
                        ));
                        full_scene.push((im_win_id, sx, sy, cw as u32, ch as u32));
                    }
                    continue;
                }

                let tid = kms_ref.with_gles_renderer(|renderer| {
                    let _ = import_surface_tree(renderer, im_surface);
                    let ctx_id = renderer.context_id();
                    with_states(im_surface, |states| {
                        let rsd = states.data_map.get::<RendererSurfaceStateUserData>();
                        rsd.and_then(|d| {
                            let locked = d.lock_safe();
                            locked.texture::<GlesTexture>(ctx_id).map(|t| {
                                let has_alpha = locked
                                    .buffer()
                                    .and_then(|b| buffer_has_alpha(&**b))
                                    .unwrap_or(true);
                                let tex_size = t.size();
                                (t.tex_id(), t.is_y_inverted(), has_alpha, tex_size)
                            })
                        })
                    })
                });
                match tid {
                    Some((tid, y_inverted, has_alpha, tex_size)) => {
                        let w = tex_size.w as u32;
                        let h = tex_size.h as u32;
                        let (px, py) = place_popup(anchor, w as i32, h as i32, 0, 0);
                        log::info!("[ime] popup texture tex={tid} size={w}x{h} abs=({px},{py})");
                        if w > 0 && h > 0 {
                            tex_updates.push((
                                im_win_id,
                                tid,
                                w,
                                h,
                                has_alpha,
                                y_inverted,
                                [0.0, 0.0, 1.0, 1.0],
                            ));
                            full_scene.push((im_win_id, px, py, w, h));
                        }
                    }
                    None => {
                        log::warn!(
                            "[ime] popup {:?} has no texture (no buffer committed yet)",
                            im_surface.id()
                        );
                    }
                }
            }
        }

        // Phase 2: Update compositor window textures then render into FBO.
        let result = if let (Some(compositor), Some(kms)) = (&mut self.compositor, &self.kms) {
            for &(win_id, tid, w, h, has_alpha, y_inverted, content_uv) in &tex_updates {
                compositor
                    .update_window_texture(win_id, tid, w, h, has_alpha, y_inverted, content_uv);
            }
            // Sync window class/app_id for per-class rules (frosted glass strength, etc.)
            for &(win_id, _, _, _, _) in &full_scene {
                let wid = WindowId::from_raw(win_id);
                if let Some(app_id) = self.state.window_app_id.get(&wid) {
                    if !app_id.is_empty() {
                        compositor.set_window_class(win_id, app_id);
                    }
                }
            }
            // Decide once per frame whether the per-output CRTC GAMMA_LUT
            // will do the OETF at scanout and whether each CRTC's CTM will do
            // the sRGB→output-primaries gamut map. The per-surface
            // ColorTransform pass below reads `decision.hw_ctm_active` to
            // choose its target params (sRGB when true, the overlapping
            // output's primaries otherwise), so this must run first.
            let decision = kms.borrow_mut().refresh_color_pipeline_offload(&self.state);
            let cm_render_gate = crate::config::CONFIG
                .load()
                .behavior()
                .color_management_render_path;
            if cm_render_gate {
                use crate::backend::edid::EdidHdrCapabilities;
                use crate::backend::wayland_udev::color_management::{
                    params_from_edid, srgb_params,
                };
                use crate::backend::wayland_udev::color_pipeline::ColorTransform;
                use smithay::utils::{Logical, Point, Rectangle};

                // Take the surface→params map once per frame instead of
                // acquiring the wp-color-management mutex per-window.
                let surface_params_map = self
                    .state
                    .color_manager
                    .as_ref()
                    .map(|cm| cm.snapshot_surface_params())
                    .unwrap_or_default();

                // When the per-output CTM will convert sRGB→native primaries
                // at scanout (3.3c), every surface should converge on sRGB
                // primaries in the FBO. Build the per-output cache only when
                // we still need it for the "match the overlapping output"
                // fallback.
                let srgb_target = srgb_params();
                let output_cache: Vec<(Rectangle<i32, Logical>, _)> = if decision.hw_ctm_active {
                    Vec::new()
                } else {
                    self.state
                        .outputs
                        .iter()
                        .filter_map(|o| {
                            let mode = o.current_mode()?;
                            let scale = o.current_scale().fractional_scale();
                            let logical_size = mode.size.to_f64().to_logical(scale).to_i32_round();
                            let logical_size = o.current_transform().transform_size(logical_size);
                            let rect =
                                Rectangle::<i32, Logical>::new(o.current_location(), logical_size);
                            let params = o
                                .user_data()
                                .get::<EdidHdrCapabilities>()
                                .map(params_from_edid)
                                .unwrap_or_else(srgb_params);
                            Some((rect, params))
                        })
                        .collect()
                };

                for &(win_id, x, y, w, h) in &full_scene {
                    let wid = WindowId::from_raw(win_id);
                    let surface = self.state.surface_for_window(wid);
                    let surface_params = surface
                        .as_ref()
                        .and_then(|s| surface_params_map.get(&s.id()));

                    let xform = if let Some(sp) = surface_params {
                        if decision.hw_ctm_active {
                            ColorTransform::build(sp, &srgb_target)
                        } else {
                            let win_rect = Rectangle::<i32, Logical>::new(
                                Point::from((x, y)),
                                (w.max(1) as i32, h.max(1) as i32).into(),
                            );
                            let output_params = output_cache
                                .iter()
                                .max_by_key(|(rect, _)| {
                                    rect.intersection(win_rect)
                                        .map(|r| r.size.w.max(0) as i64 * r.size.h.max(0) as i64)
                                        .unwrap_or(0)
                                })
                                .map(|(_, p)| p);
                            output_params.and_then(|op| ColorTransform::build(sp, op))
                        }
                    } else {
                        None
                    };
                    compositor.set_window_color_transform(win_id, xform);
                }
            } else {
                compositor.clear_all_color_transforms();
            }
            // Feed vblank presentation time for frame pacing
            if let Some(presented_at) = kms.borrow_mut().take_presentation_time() {
                compositor.on_vblank_presented(presented_at);
            }
            let rendered = kms
                .borrow_mut()
                .with_renderer(|gl| {
                    compositor.render_frame(
                        gl,
                        &full_scene,
                        focused_window,
                        decision.hw_encode_active,
                        decision.shader_tf,
                        decision.shader_gamma,
                    )
                })
                .unwrap_or(false);
            if crf_log_this {
                log::debug!("[crf] render_frame returned {rendered}");
            }
            if rendered {
                kms.borrow_mut().request_render();
            }
            rendered
        } else {
            false
        };

        // Return the scratch buffers for reuse on the next frame.
        self.scratch_tex_updates = tex_updates;
        self.scratch_full_scene = full_scene;
        self.offscreen_window_textures = offscreen;
        Ok(result)
    }

    fn run(&mut self, handler: &mut dyn EventHandler) -> Result<(), BackendError> {
        // Initialize compositor from config if KMS is ready and compositor not yet created.
        if self.kms.is_some() && self.compositor.is_none() {
            let wanted = crate::config::CONFIG.load().compositor_enabled();
            if wanted {
                match self.set_compositor_enabled(true) {
                    Ok(true) => log::info!("[run] Compositor initialized from config"),
                    Ok(false) => log::warn!(
                        "[run] Compositor wanted but set_compositor_enabled returned false (KMS not ready?)"
                    ),
                    Err(e) => log::warn!("[run] Failed to initialize compositor: {e}"),
                }
            }
        }

        loop {
            let mut handled_any = false;
            loop {
                let next = { self.pending_events.lock_safe().pop_front() };
                match next {
                    Some(BackendEvent::OutputPowerSet {
                        ref output_name,
                        on,
                    }) => {
                        handled_any = true;
                        if let Some(ref kms) = self.kms {
                            let mut kms = kms.borrow_mut();
                            let idx = kms.output_index_by_name(output_name);
                            if let Some(idx) = idx {
                                if let Err(e) = kms.set_dpms_for_output(idx, on) {
                                    log::warn!("[dpms] set_dpms_for_output failed: {e}");
                                }
                            }
                        }
                    }
                    Some(BackendEvent::GammaSet {
                        ref output_name,
                        gamma_size,
                        ref ramp,
                    }) => {
                        handled_any = true;
                        if let Some(ref kms) = self.kms {
                            let mut kms = kms.borrow_mut();
                            let idx = kms.output_index_by_name(output_name);
                            if let Some(idx) = idx {
                                if let Err(e) = kms.set_gamma_for_output(idx, gamma_size, ramp) {
                                    log::warn!("[gamma] set_gamma_for_output failed: {e}");
                                }
                            }
                        }
                    }
                    Some(BackendEvent::OutputConfigure { changes }) => {
                        handled_any = true;
                        let mut all_ok = true;
                        self.output_management_tx_seq =
                            self.output_management_tx_seq.saturating_add(1);
                        let tx_id = self.output_management_tx_seq;
                        let requested_at_unix_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis()
                            .min(u128::from(u64::MAX))
                            as u64;
                        let soft_disabled_before = self.state.soft_disabled_outputs.clone();
                        let outputs_before = output_management_snapshot(
                            &self.shared.lock_safe().outputs,
                            &soft_disabled_before,
                        );
                        let mut failed_outputs = Vec::new();
                        if let Some(ref kms) = self.kms {
                            let mut kms = kms.borrow_mut();
                            for change in &changes {
                                if !change.enabled {
                                    // Soft disable: mark the output so the renderer
                                    // skips frame submission; we keep the DrmOutput
                                    // alive (a full DRM teardown is unfinished).
                                    log::info!(
                                        "[output-mgmt] soft-disabling output '{}'",
                                        change.name
                                    );
                                    self.state.soft_disabled_outputs.insert(change.name.clone());
                                    continue;
                                }
                                // Re-enabling clears the soft-disable flag.
                                self.state.soft_disabled_outputs.remove(&change.name);
                                if let Err(e) = kms.configure_output(
                                    &change.name,
                                    change.mode,
                                    change.position,
                                    change.transform,
                                    change.scale,
                                ) {
                                    log::warn!(
                                        "[output-mgmt] configure_output('{}') failed: {e}",
                                        change.name
                                    );
                                    let mut failure =
                                        output_management_failure(change.name.clone(), e);
                                    if let Some((w, h, refresh)) = change.mode {
                                        failure.requested_value =
                                            Some(format!("{w}x{h}@{refresh}"));
                                    } else if let Some(scale) = change.scale {
                                        failure.requested_value = Some(scale.to_string());
                                    } else if let Some(transform) = change.transform {
                                        failure.requested_value = Some(transform.to_string());
                                    }
                                    failed_outputs.push(failure);
                                    all_ok = false;
                                }
                            }
                        } else {
                            failed_outputs.push(output_management_failure(
                                "*",
                                "no KMS backend available".into(),
                            ));
                            all_ok = false;
                        }
                        let mut rollback_attempted = false;
                        let rollback_succeeded = true;
                        let mut rollback_reason = None;
                        if !all_ok {
                            rollback_attempted = true;
                            self.state.soft_disabled_outputs = soft_disabled_before;
                            rollback_reason = Some(
                                "restored soft-disabled output set; DRM mode rollback is handled inside configure_output when possible"
                                    .to_string(),
                            );
                        }
                        if rollback_attempted && !rollback_succeeded {
                            log::warn!(
                                "[output-mgmt] transaction {tx_id} rollback failed: {:?}",
                                rollback_reason
                            );
                        }
                        // Refresh advertised outputs and trigger a relayout.
                        self.sync_wayland_state_from_kms();
                        let outputs_after = output_management_snapshot(
                            &self.shared.lock_safe().outputs,
                            &self.state.soft_disabled_outputs,
                        );
                        self.last_output_management_tx =
                            Some(crate::backend::api::OutputManagementTransactionStatus {
                                id: tx_id,
                                requested_at_unix_ms,
                                success: all_ok,
                                changes: changes.clone(),
                                outputs_before,
                                outputs_after,
                                failed_outputs,
                                rollback_attempted,
                                rollback_succeeded,
                                rollback_reason,
                            });
                        self.state.needs_redraw = true;
                        self.state
                            .pending_events
                            .lock_safe()
                            .push_back(BackendEvent::ScreenLayoutChanged);
                        // Resolve the FIFO-matched ack now that the modeset has
                        // been attempted. The dispatch handler that pushed this
                        // event also pushed a matching ack callback in the same
                        // synchronous call, so popping the head is safe.
                        if let Some(pending) = self.state.pending_output_acks.pop_front() {
                            (pending.on_complete)(all_ok);
                        }
                    }
                    Some(ev) => {
                        handled_any = true;
                        handler.handle_event(self, ev)?;
                    }
                    None => break,
                }
            }

            self.reconcile_session_active();
            self.maybe_reinit_kms();

            // Make cursor changes visible even if nothing else requests a redraw.
            // Read cursor_kind in the same lock scope so the render path below
            // doesn't have to re-acquire the shared lock every frame.
            let (cursor_dirty, cursor_kind) = {
                let mut shared = self.shared.lock_safe();
                let dirty = shared.cursor_dirty;
                shared.cursor_dirty = false;
                (dirty, shared.cursor_kind)
            };
            if cursor_dirty {
                self.state.needs_redraw = true;
                self.request_flush();
            }

            // Only run compositor + animation work when KMS can actually
            // accept a new frame. This prevents the GPU from doing expensive
            // rendering that will be discarded because the previous page-flip
            // hasn't completed yet.
            let session_active = self.shared.lock_safe().session_active;
            let can_present = session_active
                && self
                    .kms
                    .as_ref()
                    .map_or(true, |k| !k.borrow().any_frame_pending());

            let needs_redraw = self.state.needs_redraw;
            if needs_redraw && can_present {
                if let Some(c) = self.compositor.as_mut() {
                    c.force_full_redraw();
                }
            }

            if (handled_any || handler.needs_tick() || needs_redraw) && can_present {
                handler.update(self)?;
            }

            if let Some(kms) = &self.kms {
                if self.state.needs_redraw && can_present {
                    kms.borrow_mut().request_render();
                    self.state.needs_redraw = false;
                }
                if can_present {
                    kms.borrow_mut().render_if_needed(
                        &*self.state,
                        cursor_kind,
                        self.compositor.as_ref(),
                    );
                }
            }

            if handler.should_exit() {
                break;
            }

            // Determine calloop timeout:
            // - Zero-poll only for queued Wayland events that need draining
            // - 16ms when animations need ticking (capped at vsync rate)
            // - Block otherwise (vblank DRM event will wake us)
            let has_pending_events = !self.pending_events.lock_safe().is_empty();
            let needs_tick = handler.needs_tick();
            let kms_pending = self.kms.as_ref().map_or(false, |k| k.borrow().needs_render);
            let render_work_pending =
                session_active && (needs_tick || kms_pending || self.state.needs_redraw);
            let poll_session_activation = !session_active && self.kms.is_none();
            let timeout = if has_pending_events {
                Some(std::time::Duration::ZERO)
            } else if poll_session_activation {
                Some(std::time::Duration::from_millis(100))
            } else if render_work_pending {
                Some(std::time::Duration::from_millis(16))
            } else {
                None
            };
            self.event_loop
                .dispatch(timeout, &mut *self.state)
                .map_err(|e| BackendError::Other(Box::new(e)))?;

            if let Some(location) = self.state.pending_pointer_warp.take() {
                let (x, y, output) = {
                    let mut shared = self.shared.lock_safe();
                    shared.pointer_x = location.x;
                    shared.pointer_y = location.y;
                    (
                        shared.pointer_x,
                        shared.pointer_y,
                        output_at(&shared.outputs, shared.pointer_x, shared.pointer_y),
                    )
                };
                let hit = self
                    .state
                    .surface_under(location)
                    .as_ref()
                    .and_then(|(win, _, _)| win.map(HitTarget::Surface));
                self.pending_events
                    .lock_safe()
                    .push_back(BackendEvent::MotionNotify {
                        target: hit.unwrap_or(HitTarget::Background { output }),
                        root_x: x,
                        root_y: y,
                        time: 0,
                    });
                self.state.needs_redraw = true;
            }
        }

        Ok(())
    }
}

fn selected_kms_device(shared: &Arc<Mutex<SharedState>>) -> Option<(u64, PathBuf)> {
    let s = shared.lock_safe();
    let output_device_ids = current_output_device_ids(&s);
    let has_outputs = |device_id: u64| output_device_ids.contains(&device_id);

    if let Some(device_id) = s.preferred_device_id {
        if let Some(path) = s.device_paths.get(&device_id) {
            if has_outputs(device_id) {
                return Some((device_id, path.clone()));
            }
        }
    }

    if let Some((device_id, path)) = s
        .device_paths
        .iter()
        .filter(|(id, _)| has_outputs(**id))
        .min_by_key(|(id, _)| *id)
    {
        return Some((*device_id, path.clone()));
    }

    if let Some(device_id) = s.preferred_device_id {
        if let Some(path) = s.device_paths.get(&device_id) {
            return Some((device_id, path.clone()));
        }
    }

    s.device_paths
        .iter()
        .min_by_key(|(id, _)| *id)
        .map(|(id, path)| (*id, path.clone()))
}

fn current_output_device_ids(shared: &SharedState) -> HashSet<u64> {
    let id_to_key: HashMap<OutputId, u64> = shared
        .output_key_to_id
        .iter()
        .map(|(key, id)| (*id, *key))
        .collect();

    shared
        .outputs
        .iter()
        .filter_map(|output| id_to_key.get(&output.id).map(|key| *key >> 32))
        .collect()
}

fn refresh_preferred_device_id(shared: &Arc<Mutex<SharedState>>, seat_name: &str) {
    let device_paths = shared.lock_safe().device_paths.clone();
    let preferred = preferred_kms_device_id(seat_name, &device_paths);

    let mut s = shared.lock_safe();
    if s.preferred_device_id != preferred {
        match preferred {
            Some(id) => log::info!("[udev] preferred KMS device set to dev_id={id}"),
            None => log::debug!("[udev] no preferred KMS device; using stable fallback order"),
        }
    }
    s.preferred_device_id = preferred;
}

fn preferred_kms_device_id(seat_name: &str, device_paths: &HashMap<u64, PathBuf>) -> Option<u64> {
    if let Some(path) = std::env::var_os("JWM_DRM_DEVICE").map(PathBuf::from) {
        if let Some(device_id) = match_drm_node_to_device_id(&path, device_paths) {
            return Some(device_id);
        }
        log::warn!(
            "[udev] JWM_DRM_DEVICE={:?} did not match a DRM device for seat {}; falling back",
            path,
            seat_name
        );
    }

    match primary_gpu(seat_name) {
        Ok(Some(path)) => match_drm_node_to_device_id(&path, device_paths),
        Ok(None) => None,
        Err(err) => {
            log::debug!("[udev] primary_gpu({seat_name}) failed: {err:?}");
            None
        }
    }
}

fn match_drm_node_to_device_id(path: &Path, device_paths: &HashMap<u64, PathBuf>) -> Option<u64> {
    let node = DrmNode::from_path(path).ok()?;
    let mut candidates = Vec::with_capacity(2);
    if let Some(Ok(primary)) = node.node_with_type(NodeType::Primary) {
        candidates.push(primary.dev_id() as u64);
    }
    candidates.push(node.dev_id() as u64);
    candidates.dedup();

    candidates
        .into_iter()
        .find(|device_id| device_paths.contains_key(device_id))
}

fn output_layout_from_shared(shared: &Arc<Mutex<SharedState>>) -> HashMap<u64, (i32, i32)> {
    let s = shared.lock_safe();
    let id_to_key: HashMap<OutputId, u64> = s
        .output_key_to_id
        .iter()
        .map(|(key, id)| (*id, *key))
        .collect();

    s.outputs
        .iter()
        .filter_map(|output| {
            id_to_key
                .get(&output.id)
                .map(|key| (*key, (output.x, output.y)))
        })
        .collect()
}

fn sync_output_rects(state: &mut JwmWaylandState, shared: &Arc<Mutex<SharedState>>) {
    let s = shared.lock_safe();
    state.output_rects = s
        .outputs
        .iter()
        .map(|o| smithay::utils::Rectangle::new((o.x, o.y).into(), (o.width, o.height).into()))
        .collect();
}

fn queue_kms_reinit(shared: &Arc<Mutex<SharedState>>) {
    shared.lock_safe().kms_needs_reinit = true;
}

#[cfg(test)]
mod udev_backend_selection_tests {
    use super::*;
    use crate::config::{ArgumentConfig, GestureSwipeConfig};

    fn swipe_binding(fingers: u32, direction: &str) -> GestureSwipeConfig {
        GestureSwipeConfig {
            fingers,
            direction: direction.to_string(),
            function: "scrolling_focus_column".to_string(),
            argument: ArgumentConfig::Int(1),
        }
    }

    #[test]
    fn gesture_swipe_intercept_requires_configured_finger_count() {
        assert!(!gesture_swipe_should_intercept(3, &[]));
        assert!(!gesture_swipe_should_intercept(
            2,
            &[swipe_binding(2, "left")]
        ));
        assert!(!gesture_swipe_should_intercept(
            4,
            &[swipe_binding(3, "left")]
        ));
        assert!(gesture_swipe_should_intercept(
            3,
            &[swipe_binding(3, "left")]
        ));
    }

    fn test_output(id: OutputId, name: &str) -> OutputInfo {
        OutputInfo {
            id,
            name: name.to_string(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            scale: 1.0,
            refresh_rate: 60_000,
            hdr_capable: false,
            hdr_metadata: None,
            identity: crate::backend::api::OutputIdentity::connector_only(name),
        }
    }

    #[test]
    fn selected_kms_device_ignores_stale_output_keys() {
        let shared = Arc::new(Mutex::new(SharedState::default()));
        {
            let mut s = shared.lock_safe();
            s.device_paths.insert(1, PathBuf::from("/dev/dri/card1"));
            s.device_paths.insert(2, PathBuf::from("/dev/dri/card2"));
            s.preferred_device_id = Some(1);

            // Device 1 has only a historical output key. The current output
            // list belongs to device 2, so device 2 must win.
            s.output_key_to_id.insert(1u64 << 32, OutputId(10));
            s.output_key_to_id.insert(2u64 << 32, OutputId(20));
            s.outputs.push(test_output(OutputId(20), "HDMI-A-1"));
        }

        let selected = selected_kms_device(&shared).map(|(device_id, _)| device_id);
        assert_eq!(selected, Some(2));
    }

    #[test]
    fn current_output_device_ids_only_uses_live_outputs() {
        let mut s = SharedState::default();
        s.output_key_to_id.insert(1u64 << 32, OutputId(10));
        s.output_key_to_id.insert(2u64 << 32, OutputId(20));
        s.outputs.push(test_output(OutputId(20), "HDMI-A-1"));

        let ids = current_output_device_ids(&s);
        assert!(!ids.contains(&1));
        assert!(ids.contains(&2));
    }

    #[test]
    fn output_info_equivalent_covers_scale_and_hdr() {
        let a = test_output(OutputId(1), "HDMI-A-1");
        let mut b = a.clone();
        assert!(output_info_equivalent(&a, &b));

        b.scale = 1.25;
        assert!(!output_info_equivalent(&a, &b));

        b = a.clone();
        b.hdr_capable = true;
        assert!(!output_info_equivalent(&a, &b));
    }
}

fn rebuild_outputs(
    shared: &Arc<Mutex<SharedState>>,
    pending_events: &Arc<Mutex<VecDeque<BackendEvent>>>,
) -> Result<bool, BackendError> {
    let (old_outputs, device_paths, mut key_to_id, mut next_raw) = {
        let s = shared.lock_safe();
        (
            s.outputs.clone(),
            s.device_paths.clone(),
            s.output_key_to_id.clone(),
            s.next_output_raw,
        )
    };

    let mut new_outputs: Vec<(u64, OutputInfo)> = Vec::new();
    let mut x_cursor = 0i32;
    let mut device_paths: Vec<(u64, PathBuf)> = device_paths.into_iter().collect();
    device_paths.sort_by_key(|(id, _)| *id);
    for (dev_id, path) in device_paths {
        let scanned = match scan_drm_outputs(dev_id, &path) {
            Ok(scanned) => scanned,
            Err(err) => {
                log::warn!("[udev] failed to scan DRM outputs for {:?}: {err:?}", path);
                continue;
            }
        };
        for (key, mut info) in scanned {
            info.x = x_cursor;
            info.y = 0;
            x_cursor += info.width.max(1);
            new_outputs.push((key, info));
        }
    }

    let mut final_outputs: Vec<OutputInfo> = Vec::new();
    for (key, mut info) in new_outputs {
        let id = key_to_id.entry(key).or_insert_with(|| {
            let raw = next_raw;
            next_raw = next_raw.wrapping_add(1);
            OutputId(raw)
        });
        info.id = *id;
        final_outputs.push(info);
    }

    let mut old_by_id: HashMap<OutputId, OutputInfo> = HashMap::new();
    for o in old_outputs {
        old_by_id.insert(o.id, o);
    }

    let mut new_by_id: HashMap<OutputId, OutputInfo> = HashMap::new();
    for o in &final_outputs {
        new_by_id.insert(o.id, o.clone());
    }

    let mut changed = false;
    {
        let mut q = pending_events.lock_safe();
        for (id, old) in &old_by_id {
            if !new_by_id.contains_key(id) {
                changed = true;
                q.push_back(BackendEvent::OutputRemoved(*id));
            } else {
                let new = new_by_id.get(id).unwrap();
                if !output_info_equivalent(old, new) {
                    changed = true;
                    q.push_back(BackendEvent::OutputChanged(new.clone()));
                }
            }
        }
        for (id, new) in &new_by_id {
            if !old_by_id.contains_key(id) {
                changed = true;
                q.push_back(BackendEvent::OutputAdded(new.clone()));
            }
        }
    }

    {
        let mut s = shared.lock_safe();
        s.outputs = final_outputs;
        s.output_key_to_id = key_to_id;
        s.next_output_raw = next_raw;
    }
    Ok(changed)
}

fn output_info_equivalent(a: &OutputInfo, b: &OutputInfo) -> bool {
    a.name == b.name
        && a.x == b.x
        && a.y == b.y
        && a.width == b.width
        && a.height == b.height
        && a.scale.to_bits() == b.scale.to_bits()
        && a.refresh_rate == b.refresh_rate
        && a.hdr_capable == b.hdr_capable
        && output_identity_equivalent(&a.identity, &b.identity)
        && hdr_metadata_equivalent(a.hdr_metadata.as_ref(), b.hdr_metadata.as_ref())
}

fn output_identity_equivalent(
    a: &crate::backend::api::OutputIdentity,
    b: &crate::backend::api::OutputIdentity,
) -> bool {
    a.connector == b.connector
        && a.vendor == b.vendor
        && a.product_code == b.product_code
        && a.serial_number == b.serial_number
        && a.monitor_name == b.monitor_name
        && a.monitor_serial == b.monitor_serial
        && a.stable_key == b.stable_key
}

fn hdr_metadata_equivalent(
    a: Option<&crate::backend::edid::EdidHdrCapabilities>,
    b: Option<&crate::backend::edid::EdidHdrCapabilities>,
) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            a.max_luminance_nits.to_bits() == b.max_luminance_nits.to_bits()
                && a.min_luminance_nits.to_bits() == b.min_luminance_nits.to_bits()
                && a.supports_bt2020 == b.supports_bt2020
                && a.supports_pq == b.supports_pq
                && a.supports_hlg == b.supports_hlg
        }
        _ => false,
    }
}

fn scan_drm_outputs(dev_id: u64, path: &Path) -> Result<Vec<(u64, OutputInfo)>, BackendError> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| BackendError::Other(Box::new(e)))?;

    #[derive(Debug)]
    struct DrmCard(std::fs::File);
    impl std::os::unix::io::AsFd for DrmCard {
        fn as_fd(&self) -> std::os::unix::io::BorrowedFd<'_> {
            self.0.as_fd()
        }
    }
    impl drm::Device for DrmCard {}
    impl drm::control::Device for DrmCard {}

    let card = DrmCard(file);

    let res = card.resource_handles().map_err(|e| {
        BackendError::Other(Box::new(io::Error::new(
            io::ErrorKind::Other,
            format!("drm resources failed: {e:?}"),
        )))
    })?;

    let mut outputs = Vec::new();
    for conn_handle in res.connectors() {
        let conn = card.get_connector(*conn_handle, true).map_err(|e| {
            BackendError::Other(Box::new(io::Error::new(
                io::ErrorKind::Other,
                format!("drm get_connector failed: {e:?}"),
            )))
        })?;

        if conn.state() != connector::State::Connected {
            continue;
        }
        if conn.modes().is_empty() {
            continue;
        }

        let mode = conn
            .modes()
            .iter()
            .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
            .or_else(|| conn.modes().first())
            .unwrap();

        let width = mode.size().0 as i32;
        let height = mode.size().1 as i32;
        let refresh_rate = mode.vrefresh().saturating_mul(1000);

        let name = format!("{:?}-{}", conn.interface(), conn.interface_id());
        let key = ((dev_id as u64) << 32) | (u32::from(*conn_handle) as u64);
        let (edid_identity, hdr_metadata) = query_connector_edid_metadata(&card, *conn_handle);
        let stable_key = match &edid_identity {
            Some(identity) => format!(
                "{}:{}:{:04x}:{:08x}",
                name, identity.vendor, identity.product_code, identity.serial_number
            ),
            None => name.clone(),
        };
        let identity = crate::backend::api::OutputIdentity {
            connector: name.clone(),
            vendor: edid_identity.as_ref().map(|i| i.vendor.clone()),
            product_code: edid_identity.as_ref().map(|i| i.product_code),
            serial_number: edid_identity.as_ref().map(|i| i.serial_number),
            monitor_name: edid_identity.as_ref().and_then(|i| i.monitor_name.clone()),
            monitor_serial: edid_identity
                .as_ref()
                .and_then(|i| i.monitor_serial.clone()),
            stable_key,
        };

        outputs.push((
            key,
            OutputInfo {
                id: OutputId(0),
                name,
                x: 0,
                y: 0,
                width,
                height,
                scale: 1.0,
                refresh_rate,
                hdr_capable: hdr_metadata.is_some(),
                hdr_metadata,
                identity,
            },
        ));
    }

    Ok(outputs)
}

/// Attach each output's parsed EDID HDR static-metadata block (CTA-861) onto
/// the matching `smithay::output::Output` user_data. The wp-color-management
/// Dispatch reads it back via `Output::from_resource(&wl_output)`. Lookup is by
/// output name, which both the KMS and the shared OutputInfo path build via
/// `format!("{:?}-{}", interface, interface_id)`.
fn attach_edid_caps_to_outputs(
    smithay_outputs: &[smithay::output::Output],
    shared_outputs: &[OutputInfo],
) {
    use crate::backend::edid::EdidHdrCapabilities;
    for info in shared_outputs {
        let Some(caps) = info.hdr_metadata.clone() else {
            continue;
        };
        if let Some(out) = smithay_outputs.iter().find(|o| o.name() == info.name) {
            out.user_data()
                .insert_if_missing_threadsafe::<EdidHdrCapabilities, _>(|| caps);
        }
    }
}

fn query_connector_edid_metadata<D: drm::control::Device>(
    dev: &D,
    conn_handle: connector::Handle,
) -> (
    Option<crate::backend::edid::EdidIdentity>,
    Option<crate::backend::edid::EdidHdrCapabilities>,
) {
    use crate::backend::edid::{parse_edid_hdr_from_bytes, parse_edid_identity_from_bytes};

    let Some(props) = dev.get_properties(conn_handle).ok() else {
        return (None, None);
    };
    let (handles, values) = props.as_props_and_values();
    for (prop_handle, value) in handles.iter().zip(values.iter()) {
        let Ok(info) = dev.get_property(*prop_handle) else {
            continue;
        };
        if info.name().to_str() != Ok("EDID") {
            continue;
        }
        if *value == 0 {
            return (None, None);
        }
        let Some(blob) = dev.get_property_blob(*value).ok() else {
            return (None, None);
        };
        return (
            parse_edid_identity_from_bytes(&blob),
            parse_edid_hdr_from_bytes(&blob),
        );
    }
    (None, None)
}
