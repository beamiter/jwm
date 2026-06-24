use crate::sync_ext::MutexExt;
use crate::backend::wayland::state::JwmWaylandState;
use crate::backend::wayland_dummy_ops::*;
use crate::backend::wayland_key_ops::UdevKeyOps;

#[path = "../udev_kms.rs"]
mod kms;
use self::kms::KmsState;
use super::compositor::WaylandCompositor;
use crate::backend::api::{
    Backend, BackendEvent, Capabilities, ColorAllocator, CursorProvider, EventHandler, HitTarget,
    InputOps, KeyOps, OutputInfo, OutputOps, PropertyOps, ResizeEdge, ScreenInfo, WindowOps,
    WindowType,
};
use crate::backend::common_define::{Mods, OutputId, StdCursorKind, WindowId};
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

use drm::control::{connector, Device as ControlDevice, ModeTypeFlags};

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::surface::render_elements_from_surface_tree;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::GlesTexture;
use smithay::backend::renderer::utils::{import_surface_tree, RendererSurfaceStateUserData};
use smithay::backend::renderer::{
    buffer_has_alpha, buffer_type, Bind, BufferType, Color32F, Offscreen, Renderer, Texture,
};
use smithay::utils::{Physical, Scale, Size, Transform};
use smithay::wayland::compositor::{get_children, with_states};
use smithay::wayland::shell::xdg::SurfaceCachedState;

use smithay::backend::input::{
    AbsolutePositionEvent, Axis, Event as InputEventExt, InputEvent, KeyboardKeyEvent,
    PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
    TouchEvent as TouchEventTrait,
    GestureBeginEvent as GestureBeginEventTrait,
    GestureEndEvent as GestureEndEventTrait,
    GestureSwipeUpdateEvent as GestureSwipeUpdateEventTrait,
    GesturePinchUpdateEvent as GesturePinchUpdateEventTrait,
};
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::Event as SessionEvent;
use smithay::backend::session::Session;
use smithay::backend::udev::{UdevBackend as SmithayUdevBackend, UdevEvent};
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
use smithay::wayland::shell::wlr_layer::{KeyboardInteractivity, Layer as WlrLayer};
use smithay::xwayland::{X11Wm, XWayland, XWaylandEvent};

fn allowed_shortcut_mods() -> Mods {
    Mods::SHIFT | Mods::CONTROL | Mods::ALT | Mods::SUPER | Mods::MOD2 | Mods::MOD3 | Mods::MOD5
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
    key_bindings: Vec<(
        crate::backend::common_define::Mods,
        crate::backend::common_define::KeySym,
    )>,
    /// xkb keycode (0..=255) -> base (unmodified) keysym.
    keysym_table: Vec<crate::backend::common_define::KeySym>,
    /// xkb keycodes that were intercepted on press and should be intercepted on release.
    suppressed_keycodes: HashSet<u8>,

    repeat: Option<RepeatState>,
    outputs: Vec<OutputInfo>,
    output_key_to_id: HashMap<u64, OutputId>,
    next_output_raw: u64,
    device_paths: HashMap<u64, PathBuf>,

    kms_needs_reinit: bool,

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

            kms_needs_reinit: false,
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
            // Suppress events reaching Wayland clients and switch to crosshair cursor.
            let mut shared = self.shared.lock_safe();
            shared.screenshot_grab_active = true;
            if shared.cursor_kind != StdCursorKind::Crosshair {
                shared.cursor_kind = StdCursorKind::Crosshair;
                shared.cursor_dirty = true;
            }
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
                if let Some(toplevel) = state.try_lookup_toplevel(win) {
                    toplevel.with_pending_state(|s| {
                        s.size = Some((w as i32, h as i32).into());
                    });
                    toplevel.send_pending_configure();
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
        let (title, app_id, layer_info) = unsafe {
            self.with_state_mut(|state| {
                (
                    state.window_title.get(&win).cloned().unwrap_or_default(),
                    state.window_app_id.get(&win).cloned().unwrap_or_default(),
                    state.window_layer_info.get(&win).copied(),
                )
            })
        };

        if let Some(info) = layer_info {
            if info.exclusive_zone != 0 {
                return vec![WindowType::Dock];
            }
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

    fn maybe_reinit_kms(&mut self) {
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

        let selected = {
            let s = self.shared.lock_safe();
            s.device_paths
                .iter()
                .min_by_key(|(id, _)| *id)
                .map(|(id, p)| (*id, p.clone()))
        };

        let Some((dev_id, dev_path)) = selected else {
            // No DRM devices; drop KMS if any.
            if let Some(old) = self.kms.take() {
                if let Some(token) = old.borrow_mut().registration_token.take() {
                    let _ = self.event_loop.handle().remove(token);
                }
            }
            return;
        };

        let output_layout: std::collections::HashMap<u64, (i32, i32)> = {
            let s = self.shared.lock_safe();
            let mut id_to_key: HashMap<OutputId, u64> = HashMap::new();
            for (key, id) in &s.output_key_to_id {
                id_to_key.insert(*id, *key);
            }
            let mut layout = std::collections::HashMap::new();
            for o in &s.outputs {
                if let Some(key) = id_to_key.get(&o.id) {
                    layout.insert(*key, (o.x, o.y));
                }
            }
            layout
        };

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
                if let Some(old) = self.kms.take() {
                    if let Some(token) = old.borrow_mut().registration_token.take() {
                        let _ = self.event_loop.handle().remove(token);
                    }
                }

                self.kms = Some(new_kms);
                self.state.needs_redraw = true;
                self.state.outputs = self
                    .kms
                    .as_ref()
                    .map(|k| k.borrow().outputs())
                    .unwrap_or_default();
                self.state.gamma_sizes = self
                    .kms
                    .as_ref()
                    .map(|k| k.borrow_mut().gamma_sizes().into_iter().collect())
                    .unwrap_or_default();
                // Wire screencopy pending queue to KMS state.
                if let Some(ref screencopy_queue) = self.state.screencopy_pending {
                    if let Some(ref kms) = self.kms {
                        kms.borrow_mut()
                            .set_screencopy_pending(screencopy_queue.clone());
                    }
                }
                if let Some(ref image_capture_queue) = self.state.image_capture_pending {
                    if let Some(ref kms) = self.kms {
                        kms.borrow_mut()
                            .set_image_capture_pending(image_capture_queue.clone());
                    }
                }

                // The rebuilt KMS state carries a fresh EGL context, so every GL
                // object the compositor created in the previous context (shaders,
                // textures, FBOs) is now dangling. If the compositor was enabled,
                // recreate it against the new renderer; otherwise effects render
                // from invalid handles (black screen / GL errors) after the
                // VT-switch back. Its Drop is intentionally empty, so dropping the
                // stale compositor issues no GL calls on the new context.
                if self.compositor.is_some() {
                    self.compositor = None;
                    if let Some(kms) = &self.kms {
                        let mut kms_ref = kms.borrow_mut();
                        let (w, h) = kms_ref.total_screen_size();
                        let hdr_10bit = kms_ref.supports_10bit();
                        match kms_ref
                            .with_renderer(|gl| unsafe { WaylandCompositor::new(gl, w, h, hdr_10bit) })
                        {
                            Ok(Ok(compositor)) => self.compositor = Some(compositor),
                            Ok(Err(e)) => log::error!(
                                "[udev] compositor recreate after KMS reinit failed: {e}"
                            ),
                            Err(e) => log::error!(
                                "[udev] GL access for compositor recreate failed: {e:?}"
                            ),
                        }
                    }
                    // Restore config-driven compositor settings (blur, etc.).
                    self.compositor_apply_config();
                }

                self.request_flush();
            }
            Err(err) => {
                log::warn!("KMS re-init failed (keeping previous state): {err}");
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
                .map(|k| (k.mask & allowed_shortcut_mods(), k.key_sym))
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
            use nix::sys::inotify::{AddWatchFlags, Inotify, InitFlags};

            let pending = pending_events.clone();
            let setup = || -> Result<(), BackendError> {
                let config_path = crate::config::Config::get_default_config_path();
                let watch_dir = config_path
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| config_path.clone());
                let config_file_name = config_path.file_name().map(|n| n.to_os_string());

                let inotify = Inotify::init(InitFlags::IN_NONBLOCK).map_err(|e| {
                    BackendError::Message(format!("inotify init failed: {e}"))
                })?;
                inotify
                    .add_watch(
                        &watch_dir,
                        AddWatchFlags::IN_CLOSE_WRITE
                            | AddWatchFlags::IN_MOVED_TO
                            | AddWatchFlags::IN_CREATE,
                    )
                    .map_err(|e| {
                        BackendError::Message(format!(
                            "inotify watch {:?} failed: {e}",
                            watch_dir
                        ))
                    })?;

                event_loop
                    .handle()
                    .insert_source(
                        calloop::generic::Generic::new(
                            inotify,
                            Interest::READ,
                            Mode::Level,
                        ),
                        move |_, inotify, _state| {
                            let events = inotify.read_events().unwrap_or_default();
                            let relevant = events.iter().any(|ev| {
                                match (&config_file_name, &ev.name) {
                                    (Some(want), Some(got)) => got == want,
                                    _ => true,
                                }
                            });
                            if relevant {
                                pending
                                    .lock_safe()
                                    .push_back(BackendEvent::ConfigChanged);
                            }
                            Ok(PostAction::Continue)
                        },
                    )
                    .map_err(|e| {
                        BackendError::Message(format!(
                            "calloop insert_source(inotify) failed: {e}"
                        ))
                    })?;
                Ok(())
            };
            if let Err(e) = setup() {
                log::warn!("[config] hot-reload disabled: {e}");
            } else {
                log::info!("[config] hot-reload enabled via inotify");
            }
        }

        let (mut session, notifier) =
            LibSeatSession::new().map_err(|e| BackendError::Other(Box::new(e)))?;
        let seat_name = session.seat();

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
            let nested = std::env::var_os("WAYLAND_DISPLAY").is_some();
            // SAFETY: JWM's backend is single-threaded and we set this once at startup.
            unsafe {
                std::env::set_var("WAYLAND_DISPLAY", name);
                std::env::set_var("XDG_CURRENT_DESKTOP", "jwm");
                std::env::set_var("XDG_SESSION_TYPE", "wayland");
                if nested {
                    log::info!("Nested Wayland session detected: clearing DBUS_SESSION_BUS_ADDRESS to isolate children from parent session bus");
                    std::env::set_var("DBUS_SESSION_BUS_ADDRESS", "");
                }
            }
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

        {
            let mut shared_guard = shared.lock_safe();
            for (device_id, path) in udev_backend.device_list() {
                shared_guard
                    .device_paths
                    .insert(device_id, path.to_path_buf());
            }
        }
        rebuild_outputs(&shared, &pending_events)?;

        // Keep a copy of output geometries in the Wayland state for popup constraining.
        {
            let s = shared.lock_safe();
            state.output_rects = s
                .outputs
                .iter()
                .map(|o| {
                    smithay::utils::Rectangle::new((o.x, o.y).into(), (o.width, o.height).into())
                })
                .collect();
        }

        // Minimal visible output: initialize KMS and render a solid background.
        // If this fails (e.g. missing permissions / no DRM device), keep running headless.
        let kms = {
            let selected = {
                let s = shared.lock_safe();
                s.device_paths
                    .iter()
                    .min_by_key(|(id, _)| *id)
                    .map(|(id, p)| (*id, p.clone()))
            };

            match selected {
                Some((dev_id, p)) => {
                    let output_layout: std::collections::HashMap<u64, (i32, i32)> = {
                        let s = shared.lock_safe();
                        let mut id_to_key: HashMap<OutputId, u64> = HashMap::new();
                        for (key, id) in &s.output_key_to_id {
                            id_to_key.insert(*id, *key);
                        }
                        let mut layout = std::collections::HashMap::new();
                        for o in &s.outputs {
                            if let Some(key) = id_to_key.get(&o.id) {
                                layout.insert(*key, (o.x, o.y));
                            }
                        }
                        layout
                    };

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
                None => None,
            }
        };

        if let Some(kms) = &kms {
            state.outputs = kms.borrow().outputs();
            state.gamma_sizes = kms.borrow_mut().gamma_sizes().into_iter().collect();

            // Advertise linux-dmabuf formats with scanout preference tranches.
            // This tells clients which formats can bypass the compositor (direct scanout).
            {
                let kms_ref = kms.borrow();
                let render_formats = kms_ref.dmabuf_render_formats();
                let scanout_formats = kms_ref.dmabuf_render_formats();
                let main_device = kms_ref.dev_t();
                drop(kms_ref);
                state.ensure_dmabuf_global_with_feedback(
                    &display_handle,
                    render_formats,
                    scanout_formats,
                    main_device,
                );
            }

            // wp-linux-drm-syncobj-v1 (explicit sync) – required for NVIDIA
            {
                use smithay::wayland::drm_syncobj::{DrmSyncobjState, supports_syncobj_eventfd};
                let drm_fd = kms.borrow().drm_device_fd.clone();
                if supports_syncobj_eventfd(&drm_fd) {
                    state.drm_syncobj_state = Some(
                        DrmSyncobjState::new::<crate::backend::wayland::state::JwmWaylandState>(
                            &display_handle,
                            drm_fd,
                        ),
                    );
                    log::info!("[udev/wayland] wp-linux-drm-syncobj-v1 (explicit sync) enabled");
                } else {
                    log::info!("[udev/wayland] DRM syncobj eventfd not supported, explicit sync disabled");
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
                            let (x, y, output, in_screenshot) = {
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

                            let location: Point<f64, Logical> = (x, y).into();
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

                            let under = state.surface_under(location);
                            let hit = under
                                .as_ref()
                                .and_then(|(win, _, _)| win.map(HitTarget::Surface));
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
                            let (x, y, output, in_screenshot) = {
                                let mut s = shared.lock_safe();
                                let (w, h, origin_x, origin_y, output) = if let Some(first) = s.outputs.first() {
                                    (first.width.max(1) as i32, first.height.max(1) as i32, first.x, first.y, Some(first.id))
                                } else {
                                    (1920, 1080, 0, 0, None)
                                };
                                let pos = event.position_transformed(smithay::utils::Size::from((w, h)));
                                s.pointer_x = origin_x as f64 + pos.x;
                                s.pointer_y = origin_y as f64 + pos.y;
                                (s.pointer_x, s.pointer_y, output, s.screenshot_grab_active)
                            };

                            let location: Point<f64, Logical> = (x, y).into();
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

                            let under = state.surface_under(location);
                            let hit = under
                                .as_ref()
                                .and_then(|(win, _, _)| win.map(HitTarget::Surface));
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

                            let under = state.surface_under(location);
                            let hit = under
                                .as_ref()
                                .and_then(|(win, _, _)| win.map(HitTarget::Surface));
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
                                if pressed {
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

                            if pressed {
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

                            let debug_keys = std::env::var("JWM_DEBUG_KEYS")
                                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                                .unwrap_or(false);

                            // Layer-shell surfaces can request exclusive keyboard interactivity
                            // (e.g. lock screens / OSD). If such a surface exists on Top/Overlay,
                            // route keyboard events directly to it and do not emit WM shortcuts.
                            let mut handled_by_exclusive_layer = false;

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

                                let exclusive_surface = state
                                    .layer_shell_state
                                    .layer_surfaces()
                                    .rev()
                                    .find_map(|layer| {
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
                                    });

                                if let Some(surface) = exclusive_surface {
                                    let is_ime = state.im_client_id.as_ref().map_or(false, |im_id| {
                                        surface.id().same_client_as(im_id)
                                    });

                                    if !is_ime {
                                    handled_by_exclusive_layer = true;

                                    if debug_keys && pressed {
                                        log::info!("[udev:key] handled_by_exclusive_layer=true");
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
                                    let mods_bits = mods_from_smithay(&kbd.modifier_state()).bits();
                                    if let Some(mut s) = shared.lock().ok() {
                                        s.mods_state = mods_bits;
                                    }
                                    }
                                }

                                if handled_by_exclusive_layer {
                                    // Skip best-effort focus selection and WM shortcut emission.
                                } else {
                                if kbd.current_focus().is_none() {
                                    let (px, py) = {
                                        let s = shared.lock_safe();
                                        (s.pointer_x, s.pointer_y)
                                    };
                                    let location: Point<f64, Logical> = (px, py).into();
                                    if let Some((_win, surface, _origin)) = state.surface_under(location) {
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
                                        let xkb_keycode_u8 = u8::try_from(u32::from(keycode)).unwrap_or(0);

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

                                        let clean_mods = crate::backend::common_define::Mods::from_bits_truncate(mods_bits)
                                            & allowed_shortcut_mods();

                                        let should_suppress = s
                                            .key_bindings
                                            .iter()
                                            .any(|(m, ks)| *ks == keysym && *m == clean_mods)
                                            || (s.screenshot_grab_active && matches!(
                                                keysym,
                                                crate::backend::common_define::keys::KEY_Escape
                                                | crate::backend::common_define::keys::KEY_Return
                                                | crate::backend::common_define::keys::KEY_s
                                                | crate::backend::common_define::keys::KEY_c
                                            ));

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

                            // JWM only uses press for shortcuts for now.
                            // Always dispatch to WM regardless of exclusive layer (e.g. fcitx5
                            // IME panel); the layer still receives the key via FilterResult above.
                            if matches!(state_key, smithay::backend::input::KeyState::Pressed)
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
                                    let is_bound = s
                                        .key_bindings
                                        .iter()
                                        .any(|(m, ks)| *ks == keysym && *m == clean_mods);

                                    if is_bound {
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
                            let (w, h) = {
                                let s = shared.lock_safe();
                                if let Some(first) = s.outputs.first() {
                                    (first.width.max(1) as i32, first.height.max(1) as i32)
                                } else {
                                    (1920, 1080)
                                }
                            };
                            let pos = event.position_transformed(smithay::utils::Size::from((w, h)));
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
                            let (w, h) = {
                                let s = shared.lock_safe();
                                if let Some(first) = s.outputs.first() {
                                    (first.width.max(1) as i32, first.height.max(1) as i32)
                                } else {
                                    (1920, 1080)
                                }
                            };
                            let pos = event.position_transformed(smithay::utils::Size::from((w, h)));
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
                            // 3+ fingers: WM claims the gesture; clients see nothing.
                            if fingers >= 3 {
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
            event_loop
                .handle()
                .insert_source(notifier, move |event, &mut (), _state| match event {
                    SessionEvent::PauseSession => {
                        libinput_context.suspend();
                        pending_events
                            .lock_safe()
                            .push_back(BackendEvent::ScreenLayoutChanged);
                    }
                    SessionEvent::ActivateSession => {
                        if let Err(e) = libinput_context.resume() {
                            log::warn!("[udev] libinput resume after VT-switch failed: {e:?}");
                        }
                        pending_events
                            .lock_safe()
                            .push_back(BackendEvent::ScreenLayoutChanged);
                        let _ = rebuild_outputs(&shared, &pending_events);
                        {
                            let s = shared.lock_safe();
                            _state.output_rects = s
                                .outputs
                                .iter()
                                .map(|o| {
                                    smithay::utils::Rectangle::new(
                                        (o.x, o.y).into(),
                                        (o.width, o.height).into(),
                                    )
                                })
                                .collect();
                        }
                        if let Some(grab_win) = _state.popup_grab_toplevel {
                            _state.reconstrain_popups_for_toplevel(grab_win);
                        }
                        shared.lock_safe().kms_needs_reinit = true;
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
            event_loop
                .handle()
                .insert_source(udev_backend, move |event, _, _state| {
                    match event {
                        UdevEvent::Added { device_id, path } => {
                            shared
                                .lock_safe()
                                .device_paths
                                .insert(device_id, path.to_path_buf());
                        }
                        UdevEvent::Changed { device_id } => {
                            let _ = device_id;
                        }
                        UdevEvent::Removed { device_id } => {
                            shared.lock_safe().device_paths.remove(&device_id);
                        }
                    }
                    let _ = rebuild_outputs(&shared, &pending_events);
                    {
                        let s = shared.lock_safe();
                        _state.output_rects = s
                            .outputs
                            .iter()
                            .map(|o| {
                                smithay::utils::Rectangle::new(
                                    (o.x, o.y).into(),
                                    (o.width, o.height).into(),
                                )
                            })
                            .collect();
                    }
                    if let Some(grab_win) = _state.popup_grab_toplevel {
                        _state.reconstrain_popups_for_toplevel(grab_win);
                    }
                    shared.lock_safe().kms_needs_reinit = true;
                    pending_events
                        .lock_safe()
                        .push_back(BackendEvent::ScreenLayoutChanged);
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

    fn request_render(&mut self) {
        self.state.needs_redraw = true;
    }

    fn take_screenshot_to_file(&mut self, path: &std::path::Path) -> Result<bool, BackendError> {
        if let Some(kms) = &self.kms {
            kms.borrow_mut().request_screenshot(path.to_path_buf());
            // Force a redraw so the screenshot is captured on the next frame.
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
        w: u32,
        h: u32,
    ) -> Result<bool, BackendError> {
        if let Some(kms) = &self.kms {
            kms.borrow_mut()
                .request_screenshot_region(path.to_path_buf(), x, y, w, h);
            self.state.needs_redraw = true;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn has_compositor(&self) -> bool {
        self.compositor.is_some()
    }

    fn has_partial_damage(&self) -> bool {
        self.compositor.as_ref().map_or(false, |c| c.partial_damage_enabled())
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

    fn compositor_needs_render(&self) -> bool {
        self.compositor.as_ref().map_or(false, |c| c.needs_render())
    }

    fn set_compositor_enabled(&mut self, enabled: bool) -> Result<bool, BackendError> {
        if enabled && self.compositor.is_none() {
            if let Some(kms) = &self.kms {
                let mut kms_ref = kms.borrow_mut();
                let (w, h) = kms_ref.total_screen_size();
                let hdr_10bit = kms_ref.supports_10bit();
                match kms_ref.with_renderer(|gl| unsafe { WaylandCompositor::new(gl, w, h, hdr_10bit) }) {
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
        // Rate-limit diagnostic logging: log at most once per second.
        static LAST_CRF_LOG: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let crf_now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let crf_log_this = !scene.is_empty() && {
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
                    let class = self.state.window_app_id.get(&win).map(|s| s.as_str()).unwrap_or("");
                    let children = surface_opt
                        .as_ref()
                        .map(|s| get_children(s).len())
                        .unwrap_or(0);
                    log::info!(
                        "[crf] win={win_id:#x} class={class:?} x11={is_x11} surface={} subsurfaces={children} size={w}x{h}",
                        surface_opt.is_some()
                    );
                }
                if let Some(surface) = surface_opt {
                    // Electron/CEF clients (e.g. feishu) render their content into
                    // wl_subsurfaces while the root toplevel surface carries no
                    // buffer. Reading only the root surface therefore yields a
                    // transparent window. When subsurfaces are present, composite
                    // the whole surface tree into a single per-window offscreen
                    // texture so all existing per-window effects keep working.
                    if !get_children(&surface).is_empty() {
                        let (gx, gy, cw, ch) = with_states(&surface, |states| {
                            let mut cached = states.cached_state.get::<SurfaceCachedState>();
                            match cached.current().geometry {
                                Some(r) if r.size.w > 0 && r.size.h > 0 => {
                                    (r.loc.x, r.loc.y, r.size.w, r.size.h)
                                }
                                _ => (0, 0, w as i32, h as i32),
                            }
                        });
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
                                        offscreen.insert(win_id, (t, cw as u32, ch as u32));
                                    }
                                    Err(e) => {
                                        log::error!(
                                            "[crf] win={win_id:#x} offscreen create {cw}x{ch} failed: {e:?}"
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
                            let phys: Size<i32, Physical> = (cw, ch).into();
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
                                log::info!(
                                    "[crf] win={win_id:#x} composited subsurfaces -> tex={tid} {cw}x{ch}"
                                );
                            }
                            // render_output flips Y in its projection, so the
                            // offscreen is stored top-to-bottom => y_inverted=false.
                            // Content is already cropped to geometry => full UV.
                            tex_updates.push((win_id, tid, w, h, true, false, [0.0, 0.0, 1.0, 1.0]));
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
                                log::info!("[crf] win={win_id:#x} import_ok={} has_buf={has_buf} buf={buf_type_str} tex={:?} y_inv={:?} alpha={:?}",
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
                            cached.current().geometry.map(|r| (r.loc.x, r.loc.y)).unwrap_or((0, 0))
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
                        tex_updates.push((win_id, tid, w, h, has_alpha, y_inverted, content_uv));
                    }
                }
            }
        }
        if crf_log_this || (!scene.is_empty() && tex_updates.len() != scene.len()) {
            log::info!(
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
                let popup_win_id = 0xFE00_0000_0000_0000u64 | (popup_surface.id().protocol_id() as u64);
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
                            cached.current().geometry.map(|r| (r.loc.x, r.loc.y)).unwrap_or((0, 0))
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
                        tex_updates.push((popup_win_id, tid, w, h, has_alpha, y_inverted, content_uv));
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
                    let bbox = bbox_from_surface_tree(im_surface, Point::<i32, Logical>::from((0, 0)));
                    let (cw, ch) = (bbox.size.w.max(1), bbox.size.h.max(1));
                    let composited = kms_ref.with_gles_renderer(|renderer| {
                        let _ = import_surface_tree(renderer, im_surface);
                        let elements: Vec<kms::KmsRenderElement> = render_elements_from_surface_tree(
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
                                    log::error!("[ime] popup offscreen create {cw}x{ch} failed: {e:?}");
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
                        let mut dt = OutputDamageTracker::new(phys, Scale::from(1.0f64), Transform::Normal);
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
                        tex_updates.push((im_win_id, tid, cw as u32, ch as u32, true, false, [0.0, 0.0, 1.0, 1.0]));
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
                            tex_updates.push((im_win_id, tid, w, h, has_alpha, y_inverted, [0.0, 0.0, 1.0, 1.0]));
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
                compositor.update_window_texture(win_id, tid, w, h, has_alpha, y_inverted, content_uv);
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
            // Feed vblank presentation time for frame pacing
            if let Some(presented_at) = kms.borrow_mut().take_presentation_time() {
                compositor.on_vblank_presented(presented_at);
            }
            let rendered = kms
                .borrow_mut()
                .with_renderer(|gl| compositor.render_frame(gl, &full_scene, focused_window))
                .unwrap_or(false);
            if crf_log_this {
                log::info!("[crf] render_frame returned {rendered}");
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

    fn compositor_apply_config(&mut self) {
        if let Some(c) = self.compositor.as_mut() {
            c.apply_config();
        }
    }

    fn compositor_set_color_temperature(&mut self, temp: f32) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_color_temperature(temp);
        }
    }

    fn compositor_set_saturation(&mut self, sat: f32) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_saturation(sat);
        }
    }

    fn compositor_set_brightness(&mut self, val: f32) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_brightness(val);
        }
    }

    fn compositor_set_contrast(&mut self, val: f32) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_contrast(val);
        }
    }

    fn compositor_set_invert_colors(&mut self, invert: bool) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_invert_colors(invert);
        }
    }

    fn compositor_set_grayscale(&mut self, gs: bool) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_grayscale(gs);
        }
    }

    fn compositor_set_debug_hud(&mut self, enabled: bool) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_debug_hud(enabled);
        }
    }

    fn compositor_set_transition_mode(&mut self, mode: &str) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_transition_mode(mode);
        }
    }

    fn compositor_fps(&self) -> f32 {
        self.compositor.as_ref().map_or(0.0, |c| c.fps())
    }

    fn compositor_set_frame_extents(
        &mut self,
        window: WindowId,
        left: u32,
        right: u32,
        top: u32,
        bottom: u32,
    ) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_frame_extents(window.raw(), left, right, top, bottom);
        }
    }

    fn compositor_set_window_shaped(&mut self, window: WindowId, shaped: bool) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_window_shaped(window.raw(), shaped);
        }
    }

    fn compositor_notify_tag_switch(
        &mut self,
        duration: Duration,
        direction: i32,
        exclude_top: u32,
        mon_rect: (i32, i32, u32, u32),
    ) {
        if let Some(c) = self.compositor.as_mut() {
            c.notify_tag_switch(duration, direction, exclude_top, mon_rect);
        }
    }

    fn compositor_force_full_redraw(&mut self) {
        if let Some(c) = self.compositor.as_mut() {
            c.force_full_redraw();
        }
    }

    fn compositor_set_mouse_position(&mut self, x: f32, y: f32) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_mouse_position(x, y);
        }
    }

    fn compositor_deactivate_edge_glow(&mut self) {
        if let Some(c) = self.compositor.as_mut() {
            c.deactivate_edge_glow();
        }
    }

    fn compositor_unsuppress_edge_glow(&mut self) {
        if let Some(c) = self.compositor.as_mut() {
            c.unsuppress_edge_glow();
        }
    }

    fn compositor_set_window_urgent(&mut self, window: WindowId, urgent: bool) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_window_urgent(window.raw(), urgent);
        }
    }

    fn compositor_set_window_pip(&mut self, window: WindowId, pip: bool) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_window_pip(window.raw(), pip);
        }
    }

    fn compositor_set_magnifier(&mut self, enabled: bool) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_magnifier(enabled);
        }
    }

    fn compositor_set_overview_mode(
        &mut self,
        active: bool,
        windows: &[(WindowId, f32, f32, f32, f32, bool, String)],
    ) {
        if let Some(c) = self.compositor.as_mut() {
            let entries: Vec<(u64, f32, f32, f32, f32, bool, String)> = windows
                .iter()
                .map(|(id, x, y, w, h, focused, title)| {
                    (id.raw(), *x, *y, *w, *h, *focused, title.clone())
                })
                .collect();
            c.set_overview_mode(active, &entries);
        }
    }

    fn compositor_set_overview_monitor(&mut self, x: i32, y: i32, w: u32, h: u32) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_overview_monitor(x, y, w, h);
        }
    }

    fn compositor_set_monitors(&mut self, monitors: &[(u32, i32, i32, u32, u32, u32)]) {
        // Build per-monitor (id, hz) pairs from the live output list.
        // Wayland blur is a single global pass shared by every monitor, so the
        // compositor will use the highest Hz to budget blur strength — but we
        // still record all rates for parity with X11 / future per-output use.
        let monitor_hz_pairs: Vec<(u32, u32)> = {
            let shared = self.shared.lock_safe();
            monitors
                .iter()
                .map(|&(id, mx, my, _, _, _)| {
                    let hz = shared
                        .outputs
                        .iter()
                        .find(|o| o.x == mx && o.y == my)
                        .map(|o| (o.refresh_rate / 1000).max(1))
                        .unwrap_or(60);
                    (id, hz)
                })
                .collect()
        };
        if let Some(c) = self.compositor.as_mut() {
            c.set_monitors(monitors);
            c.apply_per_monitor_refresh_rates(&monitor_hz_pairs);
        }
    }

    fn compositor_set_overview_selection(&mut self, window: WindowId) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_overview_selection(window.raw());
        }
    }

    fn compositor_notify_window_move_start(&mut self, window: WindowId) {
        if let Some(c) = self.compositor.as_mut() {
            c.notify_window_move_start(window.raw());
        }
    }

    fn compositor_notify_window_move_delta(&mut self, window: WindowId, dx: f32, dy: f32) {
        if let Some(c) = self.compositor.as_mut() {
            c.notify_window_move_delta(window.raw(), dx, dy);
        }
    }

    fn compositor_notify_window_move_end(&mut self, window: WindowId) {
        if let Some(c) = self.compositor.as_mut() {
            c.notify_window_move_end(window.raw());
        }
    }

    fn compositor_set_dock_position(&mut self, x: f32, y: f32) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_dock_position(x, y);
        }
    }

    fn compositor_set_expose_mode(
        &mut self,
        active: bool,
        windows: Vec<(WindowId, i32, i32, u32, u32)>,
    ) {
        if let Some(c) = self.compositor.as_mut() {
            let entries: Vec<(u64, i32, i32, u32, u32)> = windows
                .iter()
                .map(|(id, x, y, w, h)| (id.raw(), *x, *y, *w, *h))
                .collect();
            c.set_expose_mode(active, entries);
        }
    }

    fn compositor_set_snap_preview(&mut self, preview: Option<(f32, f32, f32, f32)>) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_snap_preview(preview);
        }
    }

    fn compositor_clear_snap_preview_immediate(&mut self) {
        if let Some(c) = self.compositor.as_mut() {
            c.clear_snap_preview_immediate();
        }
    }

    fn compositor_set_peek_mode(&mut self, active: bool) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_peek_mode(active);
        }
    }

    fn compositor_set_window_groups(&mut self, groups: Vec<(u32, Vec<(u32, String, bool)>)>) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_window_groups(groups);
        }
    }

    fn compositor_zoom_to_fit(&mut self, window: Option<u32>) {
        if let Some(c) = self.compositor.as_mut() {
            c.zoom_to_fit(window);
        }
    }

    fn compositor_expose_click(&mut self, x: f32, y: f32) -> Option<WindowId> {
        if let Some(c) = self.compositor.as_ref() {
            c.expose_click(x, y).map(WindowId::from_raw)
        } else {
            None
        }
    }

    fn compositor_set_colorblind_mode(&mut self, mode: &str) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_colorblind_mode(mode);
        }
    }

    fn compositor_set_annotation_mode(&mut self, active: bool) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_annotation_mode(active);
        }
    }

    fn compositor_annotation_add_point(&mut self, x: f32, y: f32) {
        if let Some(c) = self.compositor.as_mut() {
            c.annotation_add_point(x, y);
        }
    }

    fn compositor_annotation_begin_stroke(&mut self) {
        if let Some(c) = self.compositor.as_mut() {
            c.annotation_new_stroke();
        }
    }

    fn compositor_start_recording(&mut self, path: &str) {
        if let Some(c) = self.compositor.as_mut() {
            c.start_recording(path);
        }
    }

    fn compositor_stop_recording(&mut self) {
        if let Some(c) = self.compositor.as_mut() {
            c.stop_recording();
        }
    }

    fn compositor_notify_audio_timing(&mut self, window: WindowId, fps: f32, buffer_latency_ms: u32) {
        if let Some(c) = self.compositor.as_mut() {
            c.notify_audio_timing(window.raw(), fps, buffer_latency_ms);
        }
    }

    fn compositor_get_metrics(&self) -> Option<crate::backend::api::CompositorMetrics> {
        self.compositor.as_ref().map(|c| c.get_metrics())
    }

    fn compositor_capture_thumbnail(&self, window: WindowId, max_size: u32) -> Option<(Vec<u8>, u32, u32)> {
        let compositor = self.compositor.as_ref()?;
        let kms = self.kms.as_ref()?;
        kms.borrow_mut()
            .with_renderer(|gl| unsafe { compositor.capture_thumbnail(gl, window.raw(), max_size) })
            .ok()?
    }

    fn compositor_request_live_thumbnail(&mut self, window: u32, max_size: u32) -> Option<(Vec<u8>, u32, u32)> {
        let compositor = self.compositor.as_ref()?;
        let kms = self.kms.as_ref()?;
        kms.borrow_mut()
            .with_renderer(|gl| unsafe { compositor.capture_thumbnail(gl, window as u64, max_size) })
            .ok()?
    }

    fn query_vrr_capabilities(&self, output: OutputId) -> Option<crate::backend::api::VrrCapabilities> {
        let kms = self.kms.as_ref()?;
        let shared = self.shared.lock_safe();
        let output_idx = shared.outputs.iter().position(|o| o.id == output)?;
        drop(shared);
        kms.borrow_mut().query_vrr_for_output(output_idx)
    }

    fn set_vrr_enabled(&mut self, output: OutputId, enabled: bool) -> Result<(), BackendError> {
        let kms = self.kms.as_ref().ok_or(BackendError::Unsupported("no KMS"))?;
        let shared = self.shared.lock_safe();
        let output_idx = shared.outputs.iter().position(|o| o.id == output)
            .ok_or(BackendError::NotFound("output not found"))?;
        drop(shared);
        kms.borrow_mut()
            .set_vrr_for_output(output_idx, enabled)
            .map_err(|e| BackendError::Message(e))
    }

    fn compositor_tearing_hint_count(&self) -> usize {
        self.state
            .tearing_hints
            .as_ref()
            .map(|m| m.lock_safe().len())
            .unwrap_or(0)
    }

    fn compositor_session_lock_surface_count(&self) -> usize {
        self.state.lock_surfaces.len()
    }

    fn compositor_session_locked(&self) -> bool {
        self.state.session_locked
    }

    fn run(&mut self, handler: &mut dyn EventHandler) -> Result<(), BackendError> {
        // Initialize compositor from config if KMS is ready and compositor not yet created.
        if self.kms.is_some() && self.compositor.is_none() {
            let wanted = crate::config::CONFIG.load().compositor_enabled();
            if wanted {
                match self.set_compositor_enabled(true) {
                    Ok(true) => log::info!("[run] Compositor initialized from config"),
                    Ok(false) => log::warn!("[run] Compositor wanted but set_compositor_enabled returned false (KMS not ready?)"),
                    Err(e) => log::warn!("[run] Failed to initialize compositor: {e}"),
                }
            }
        }

        loop {
            let mut handled_any = false;
            loop {
                let next = { self.pending_events.lock_safe().pop_front() };
                match next {
                    Some(BackendEvent::OutputPowerSet { ref output_name, on }) => {
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
                    Some(BackendEvent::GammaSet { ref output_name, gamma_size, ref ramp }) => {
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
                                    all_ok = false;
                                }
                            }
                        } else {
                            all_ok = false;
                        }
                        // Refresh advertised outputs and trigger a relayout.
                        self.state.outputs = self
                            .kms
                            .as_ref()
                            .map(|k| k.borrow().outputs())
                            .unwrap_or_default();
                        self.state.gamma_sizes = self
                            .kms
                            .as_ref()
                            .map(|k| k.borrow_mut().gamma_sizes().into_iter().collect())
                            .unwrap_or_default();
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
            let can_present = self.kms.as_ref().map_or(true, |k| {
                !k.borrow().any_frame_pending()
            });

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
            let timeout = if has_pending_events {
                Some(std::time::Duration::ZERO)
            } else if needs_tick || kms_pending || self.state.needs_redraw {
                Some(std::time::Duration::from_millis(16))
            } else {
                None
            };
            self.event_loop
                .dispatch(timeout, &mut *self.state)
                .map_err(|e| BackendError::Other(Box::new(e)))?;
        }

        Ok(())
    }
}

fn rebuild_outputs(
    shared: &Arc<Mutex<SharedState>>,
    pending_events: &Arc<Mutex<VecDeque<BackendEvent>>>,
) -> Result<(), BackendError> {
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
        let scanned = scan_drm_outputs(dev_id, &path)?;
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

    {
        let mut q = pending_events.lock_safe();
        for (id, old) in &old_by_id {
            if !new_by_id.contains_key(id) {
                q.push_back(BackendEvent::OutputRemoved(*id));
            } else {
                let new = new_by_id.get(id).unwrap();
                if new.width != old.width
                    || new.height != old.height
                    || new.x != old.x
                    || new.y != old.y
                    || new.refresh_rate != old.refresh_rate
                    || new.name != old.name
                {
                    q.push_back(BackendEvent::OutputChanged(new.clone()));
                }
            }
        }
        for (id, new) in &new_by_id {
            if !old_by_id.contains_key(id) {
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
    Ok(())
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
        let hdr_capable = query_connector_hdr_capable(&card, *conn_handle);

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
                hdr_capable,
            },
        ));
    }

    Ok(outputs)
}

fn query_connector_hdr_capable<D: drm::control::Device>(
    dev: &D,
    conn_handle: connector::Handle,
) -> bool {
    use crate::backend::edid::parse_edid_hdr_from_bytes;

    let Ok(props) = dev.get_properties(conn_handle) else {
        return false;
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
            return false;
        }
        let Ok(blob) = dev.get_property_blob(*value) else {
            return false;
        };
        return parse_edid_hdr_from_bytes(&blob).is_some();
    }
    false
}
