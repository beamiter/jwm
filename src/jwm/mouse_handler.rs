//! 鼠标交互处理模块
//!
//! 这个模块包含鼠标拖动窗口和调整窗口大小的功能。
//!
//! 所有交互式拖拽都先经过一个"死区"（`behavior.drag_threshold_px`）：
//! 按下后指针位移未超过阈值前，窗口不浮动也不移动；一次没有位移的
//! 单击不放不会破坏平铺布局。越过阈值后按 [`DragMode`] 升级。

use crate::backend::api::{Backend, InteractionAction, ResizeEdge};
use crate::backend::common_define::WindowId;
use crate::core::models::{ClientKey, MonitorKey};
use crate::jwm::Jwm;
use crate::jwm::geometry::GeometryConstraints;
use crate::jwm::types::WMArgEnum;
use log::{debug, error};

/// How an in-flight pointer drag will act on its window once it crosses the
/// drag threshold.
#[derive(Debug, Clone, Copy)]
pub(crate) enum DragMode {
    /// Float the window (keeping its geometry) and move it with the pointer.
    MoveFloat,
    /// Keep the window tiled; the drag only picks a new layout slot, shown
    /// as a snap preview and committed on release.
    Reorder,
    /// Float the window (keeping its geometry) and resize it from this edge.
    Resize(ResizeEdge),
}

/// A pointer drag the WM is watching. Armed on button press (or a client's
/// `_NET_WM_MOVERESIZE` request) with a track-only pointer grab; dormant
/// until the pointer travels `behavior.drag_threshold_px` from the press.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DragCtl {
    pub client: ClientKey,
    pub win: WindowId,
    pub mode: DragMode,
    pub start_root: (f64, f64),
    /// Set once the threshold was crossed and the mode's side effects ran.
    pub activated: bool,
    /// Pre-drag state, so `_NET_WM_MOVERESIZE_CANCEL` (e.g. Esc in a GTK
    /// drag) can put everything back.
    pub was_floating: bool,
    pub orig_geom: (i32, i32, i32, i32),
    pub orig_index: Option<usize>,
    pub mon: Option<MonitorKey>,
    /// Maximize state when the drag was armed; activation unmaximizes in place
    /// and `cancel_pointer_drag` reinstates it.
    pub orig_maximize: crate::core::maximize::MaximizeSnapshot,
}

impl Jwm {
    /// 开始鼠标拖动窗口（Alt+左键拖动）
    ///
    /// - 越过拖拽死区后，平铺窗口自动切换为浮动
    /// - 全屏窗口不能拖动
    pub fn movemouse(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let client_key = self.get_selected_client_key().ok_or("No client selected")?;

        // 提升窗口堆叠顺序
        self.restack(backend, self.state.sel_mon)?;

        self.start_pointer_drag(backend, client_key, DragMode::MoveFloat)
    }

    /// 开始鼠标调整窗口大小（Alt+右键拖动）
    ///
    /// - 越过拖拽死区后，平铺窗口自动切换为浮动
    /// - 全屏窗口不能调整大小
    /// - 根据鼠标位置智能选择调整边缘/角
    pub fn resizemouse(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let client_key = self.get_selected_client_key().ok_or("No client selected")?;

        let (is_fullscreen, win_id) = if let Some(c) = self.state.clients.get(client_key) {
            (c.state.is_fullscreen, c.win)
        } else {
            return Ok(());
        };

        if is_fullscreen {
            return Ok(());
        }

        self.restack(backend, self.state.sel_mon)?;

        // Wayland/udev 通常不能 warp 指针，所以根据鼠标落点选择更直观的 resize 边/角：
        // - 靠近边：Top/Bottom/Left/Right
        // - 靠近角：TopLeft/TopRight/BottomLeft/BottomRight
        // - 中间区域：退化为象限选择（避免出现"怎么拖都不动"的感觉）
        let geom = backend.window_ops().get_geometry(win_id)?;
        let (px, py) = backend.input_ops().get_pointer_position()?;

        let w = (geom.w as f64).max(1.0);
        let h = (geom.h as f64).max(1.0);

        let rel_x = px - geom.x as f64;
        let rel_y = py - geom.y as f64;

        // Dynamic grip size: small windows still get a usable edge area.
        let threshold = 24.0_f64.min(w / 3.0).min(h / 3.0).max(8.0);

        let near_left = rel_x <= threshold;
        let near_right = rel_x >= (w - threshold);
        let near_top = rel_y <= threshold;
        let near_bottom = rel_y >= (h - threshold);

        let edge = if near_top && near_left {
            ResizeEdge::TopLeft
        } else if near_top && near_right {
            ResizeEdge::TopRight
        } else if near_bottom && near_left {
            ResizeEdge::BottomLeft
        } else if near_bottom && near_right {
            ResizeEdge::BottomRight
        } else if near_top {
            ResizeEdge::Top
        } else if near_bottom {
            ResizeEdge::Bottom
        } else if near_left {
            ResizeEdge::Left
        } else if near_right {
            ResizeEdge::Right
        } else {
            // Not near any border: pick a quadrant as a reasonable default.
            let left = rel_x < (w / 2.0);
            let top = rel_y < (h / 2.0);
            match (top, left) {
                (true, true) => ResizeEdge::TopLeft,
                (true, false) => ResizeEdge::TopRight,
                (false, true) => ResizeEdge::BottomLeft,
                (false, false) => ResizeEdge::BottomRight,
            }
        };

        self.start_pointer_drag(backend, client_key, DragMode::Resize(edge))
    }

    /// Arm a deferred pointer drag on `client_key`: capture the pre-drag
    /// state, grab the pointer in track-only mode, and wait for the motion
    /// handler to cross the drag threshold. On backends without track
    /// support the drag is silently abandoned.
    pub(crate) fn start_pointer_drag(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        mode: DragMode,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(client) = self.state.clients.get(client_key) else {
            return Ok(());
        };
        if client.state.is_fullscreen {
            return Ok(());
        }
        let win = client.win;
        let was_floating = client.state.is_floating;
        let orig_geom = (
            client.geometry.x,
            client.geometry.y,
            client.geometry.w,
            client.geometry.h,
        );
        let mon = client.mon;
        let orig_index = mon.and_then(|mk| {
            self.get_monitor_clients(mk)
                .iter()
                .position(|&k| k == client_key)
        });
        let orig_maximize = self.maximize_snapshot(client_key);
        let start_root = backend
            .input_ops()
            .get_pointer_position()
            .unwrap_or(self.last_mouse_root);
        let intent = match mode {
            DragMode::Resize(edge) => InteractionAction::Resize(edge),
            DragMode::MoveFloat | DragMode::Reorder => InteractionAction::Move,
        };

        if backend.begin_track(win, intent)? {
            debug!("Armed pointer drag for window {:?} as {:?}", win, mode);
            self.drag_ctl = Some(DragCtl {
                client: client_key,
                win,
                mode,
                start_root,
                activated: false,
                was_floating,
                orig_geom,
                orig_index,
                mon,
                orig_maximize,
            });
        }
        Ok(())
    }

    /// The armed drag crossed the drag threshold: run its mode's side
    /// effects (float the window, hand the pointer to the backend's real
    /// move/resize). Reorder drags stay a track-only grab for their whole
    /// life — the window never leaves the tiling.
    pub(crate) fn activate_pointer_drag(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some((client, win, mode)) = self.drag_ctl.as_ref().map(|c| (c.client, c.win, c.mode))
        else {
            return Ok(());
        };
        if let Some(ctl) = self.drag_ctl.as_mut() {
            ctl.activated = true;
        }
        // A maximized window leaves maximize where it stands before the
        // pointer takes it over: its rect becomes the plain floating rect the
        // drag moves or resizes, and the arrange refit no longer owns it.
        // `cancel_pointer_drag` reinstates the snapshot taken at arm time.
        let unmaximize_first = matches!(mode, DragMode::MoveFloat | DragMode::Resize(_))
            && self
                .state
                .clients
                .get(client)
                .is_some_and(|c| c.state.is_maximize_realized());
        if unmaximize_first {
            self.unmaximize_in_place(backend, client)?;
        }
        match mode {
            DragMode::MoveFloat => {
                self.enable_floating_keep_geometry(backend, client)?;
                backend.begin_move(win)?;
                // Notify compositor of window move start (for wobbly windows effect)
                if backend.has_compositor() {
                    backend.compositor_notify_window_move_start(win);
                }
            }
            DragMode::Resize(edge) => {
                self.enable_floating_keep_geometry(backend, client)?;
                backend.begin_resize(win, edge)?;
            }
            DragMode::Reorder => {}
        }
        Ok(())
    }

    /// `_NET_WM_MOVERESIZE_CANCEL` (e.g. Esc during a GTK header-bar drag):
    /// end the grab and put the window back where the drag found it.
    pub(crate) fn cancel_pointer_drag(&mut self, backend: &mut dyn Backend) {
        let ctl = self.drag_ctl.take();
        let _ = backend.handle_button_release(0);
        if backend.has_compositor() {
            backend.compositor_set_snap_preview(None);
        }
        let Some(ctl) = ctl else {
            return;
        };
        if !ctl.activated {
            return;
        }
        if matches!(ctl.mode, DragMode::MoveFloat) && backend.has_compositor() {
            backend.compositor_notify_window_move_end(ctl.win);
        }
        match ctl.mode {
            // The tiling was never touched.
            DragMode::Reorder => {}
            DragMode::MoveFloat | DragMode::Resize(_) => {
                if ctl.was_floating {
                    let (x, y, w, h) = ctl.orig_geom;
                    self.resize_client(backend, ctl.client, x, y, w, h, false);
                } else {
                    if let Some(c) = self.state.clients.get_mut(ctl.client) {
                        c.state.is_floating = false;
                        c.state.is_drag_floating = false;
                    }
                    if let (Some(mk), Some(idx)) = (ctl.mon, ctl.orig_index) {
                        if let Some(list) = self.state.monitor_clients.get_mut(mk) {
                            list.retain(|&k| k != ctl.client);
                            list.insert(idx.min(list.len()), ctl.client);
                        }
                    }
                    self.arrange(backend, ctl.mon);
                }
                // Activation dropped maximize in place; the cancelled drag
                // gives it back along with the geometry.
                if ctl.orig_maximize.axes.any()
                    && let Err(e) =
                        self.reinstate_maximize_snapshot(backend, ctl.client, ctl.orig_maximize)
                {
                    error!(
                        "Could not reinstate maximize on {:?} after a cancelled drag: {}",
                        ctl.win, e
                    );
                }
            }
        }
    }

    /// 检查窗口拖动/调整大小后是否需要切换显示器
    ///
    /// 如果窗口移动到了另一个显示器，自动将其迁移过去
    pub(crate) fn check_monitor_consistency(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // 类似于之前的 check_monitor_change_after_resize
        let client_key = match self.get_selected_client_key() {
            Some(k) => k,
            None => return Ok(()),
        };

        let (x, y) = match self.state.clients.get(client_key) {
            Some(client) => (client.geometry.x, client.geometry.y),
            None => return Ok(()),
        };

        let target_monitor = self.recttomon(backend, x, y);
        if let Some(target_mon_key) = target_monitor {
            if Some(target_mon_key) != self.state.sel_mon {
                // Nobody can see a window behind a lock shade. Moving the
                // window there and selecting that monitor would also hand it
                // the focus (`focus(None)` picks the head of the monitor's
                // stack, where `sendmon` just put it), so the drop stays on
                // its own monitor instead.
                if self.monitor_key_is_locked(target_mon_key) {
                    self.pull_back_from_locked_output(backend, client_key);
                    return Ok(());
                }
                self.sendmon(backend, Some(client_key), Some(target_mon_key));
                self.state.sel_mon = Some(target_mon_key);
                self.focus(backend, None)?;
            }
        }
        Ok(())
    }

    /// Put a floating window whose origin was dropped on a locked output
    /// back inside its own monitor's work area, and remember that rect as
    /// its floating geometry. Its monitor keeps it, but a window left
    /// straddling the shade would sit on that monitor out of sight.
    ///
    /// Tiled, fullscreen and maximized windows are left alone: the layout,
    /// fullscreen and the maximize refit own their rects. So is a window
    /// whose own monitor is locked, which has nowhere visible to go.
    pub(crate) fn pull_back_from_locked_output(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) {
        let Some(client) = self.state.clients.get(client_key) else {
            return;
        };
        if !client.state.is_floating
            || client.state.is_fullscreen
            || client.state.is_maximize_realized()
        {
            return;
        }
        let Some(home) = client.mon.filter(|&mon| !self.monitor_key_is_locked(mon)) else {
            return;
        };
        let Some(area) = self.monitor_work_area(home) else {
            return;
        };
        let (mut x, mut y) = (client.geometry.x, client.geometry.y);
        let (w, h) = (client.geometry.w, client.geometry.h);
        GeometryConstraints::clamp_rect_to_boundary(
            &mut x,
            &mut y,
            client.total_width(),
            client.total_height(),
            &area,
        );
        if (x, y) == (client.geometry.x, client.geometry.y) {
            return;
        }
        debug!(
            "Pulling {:?} back from a locked output to ({x}, {y})",
            client.win
        );
        self.resize_client(backend, client_key, x, y, w, h, false);
        if let Some(client) = self.state.clients.get_mut(client_key) {
            client.geometry.floating_x = client.geometry.x;
            client.geometry.floating_y = client.geometry.y;
            client.geometry.floating_w = client.geometry.w;
            client.geometry.floating_h = client.geometry.h;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::{DragCtl, DragMode};
    use crate::backend::api::{
        Backend, BackendDiagnostics, Capabilities, CloseResult, ColorAllocator,
        CompositorAnnotation, CompositorBenchmark, CompositorControl, CompositorMedia,
        CompositorWindowEffects, CompositorWorkspaceEffects, CursorProvider, DisplayControl,
        EventHandler, Geometry, HitTarget, InputOps, KeyOps, OutputOps, PropertyOps,
        RenderScheduler, WindowAttributes, WindowChanges, WindowOps,
    };
    use crate::backend::common_define::{Pixel, WindowId};
    use crate::backend::error::BackendError;
    use crate::backend::wayland_dummy_ops::{
        DummyColorAllocator, DummyCursorProvider, DummyInputOps, DummyKeyOps, DummyPropertyOps,
    };
    use crate::core::controller::WMController;
    use crate::core::maximize::MaximizeSnapshot;
    use crate::core::models::WMClient;
    use crate::jwm::Jwm;
    use crate::jwm::monitor::test_support::{SpyOutputOps, output};
    use crate::jwm::types::WMArgEnum;

    /// Window ops that remember the last rect configured per window and
    /// answer `get_geometry` with it, the way a server reports where a drag
    /// left a window.
    #[derive(Default)]
    struct DropWindowOps {
        rects: Mutex<HashMap<WindowId, Geometry>>,
    }

    impl DropWindowOps {
        fn place(&self, win: WindowId, x: i32, y: i32, w: u32, h: u32) {
            self.rects.lock().expect("window rects lock").insert(
                win,
                Geometry {
                    x,
                    y,
                    w,
                    h,
                    border: 0,
                },
            );
        }
    }

    impl WindowOps for DropWindowOps {
        fn set_position(&self, _win: WindowId, _x: i32, _y: i32) -> Result<(), BackendError> {
            Ok(())
        }
        fn configure(
            &self,
            win: WindowId,
            x: i32,
            y: i32,
            w: u32,
            h: u32,
            _border: u32,
        ) -> Result<(), BackendError> {
            self.place(win, x, y, w, h);
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
        fn get_geometry(&self, win: WindowId) -> Result<Geometry, BackendError> {
            self.rects
                .lock()
                .expect("window rects lock")
                .get(&win)
                .copied()
                .ok_or_else(|| BackendError::Message(format!("no rect for {win:?}")))
        }
        fn scan_windows(&self) -> Result<Vec<WindowId>, BackendError> {
            Ok(Vec::new())
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

    /// A composited two-output backend whose pointer drags always end on
    /// release, so `on_button_release` takes the drag path.
    struct DropSpyBackend {
        window_ops: DropWindowOps,
        input_ops: DummyInputOps,
        property_ops: DummyPropertyOps,
        output_ops: SpyOutputOps,
        key_ops: DummyKeyOps,
        cursor_provider: DummyCursorProvider,
        color_allocator: DummyColorAllocator,
    }

    impl CompositorBenchmark for DropSpyBackend {}
    impl BackendDiagnostics for DropSpyBackend {}
    impl CompositorControl for DropSpyBackend {}
    impl CompositorMedia for DropSpyBackend {}
    impl CompositorWorkspaceEffects for DropSpyBackend {}
    impl CompositorWindowEffects for DropSpyBackend {}
    impl CompositorAnnotation for DropSpyBackend {}
    impl DisplayControl for DropSpyBackend {}
    impl RenderScheduler for DropSpyBackend {
        fn has_compositor(&self) -> bool {
            true
        }
    }

    impl Backend for DropSpyBackend {
        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }
        fn root_window(&self) -> Option<WindowId> {
            Some(WindowId::from_raw(0))
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn check_existing_wm(&self) -> Result<(), BackendError> {
            Ok(())
        }
        fn window_ops(&self) -> &dyn WindowOps {
            &self.window_ops
        }
        fn input_ops(&self) -> &dyn InputOps {
            &self.input_ops
        }
        fn property_ops(&self) -> &dyn PropertyOps {
            &self.property_ops
        }
        fn output_ops(&self) -> &dyn OutputOps {
            &self.output_ops
        }
        fn key_ops(&self) -> &dyn KeyOps {
            &self.key_ops
        }
        fn key_ops_mut(&mut self) -> &mut dyn KeyOps {
            &mut self.key_ops
        }
        fn cursor_provider(&mut self) -> &mut dyn CursorProvider {
            &mut self.cursor_provider
        }
        fn color_allocator(&mut self) -> &mut dyn ColorAllocator {
            &mut self.color_allocator
        }
        fn run(&mut self, _handler: &mut dyn EventHandler) -> Result<(), BackendError> {
            Ok(())
        }
        fn handle_button_release(&mut self, _time: u32) -> Result<bool, BackendError> {
            Ok(true)
        }
    }

    /// Regression: the snap and reorder plans refuse a locked monitor, so a
    /// floating drag released over one falls through to the plain drop,
    /// whose monitor check moved the window behind the shade, selected that
    /// monitor and focused the window there. The real release now leaves
    /// the window, the selection and the focus on the unlocked monitor and
    /// pulls the window back inside it.
    #[test]
    fn a_floating_drop_on_a_locked_output_stays_on_its_own_monitor() {
        let mut backend = DropSpyBackend {
            window_ops: DropWindowOps::default(),
            input_ops: DummyInputOps,
            property_ops: DummyPropertyOps,
            output_ops: SpyOutputOps {
                outputs: vec![output(1, 0, 0, 1920, 1080), output(2, 1920, 0, 1920, 1080)],
            },
            key_ops: DummyKeyOps,
            cursor_provider: DummyCursorProvider,
            color_allocator: DummyColorAllocator,
        };
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").expect("test jwm");
        let (shown, shaded) = (jwm.state.monitor_order[0], jwm.state.monitor_order[1]);
        assert_eq!(jwm.state.monitors[shaded].num, 1);

        let win = WindowId::from_raw(0x5e01);
        let mut client = WMClient::new(win);
        client.mon = Some(shown);
        client.state.tags = jwm.state.monitors[shown].get_active_tags();
        client.state.is_floating = true;
        (client.geometry.x, client.geometry.y) = (200, 150);
        (client.geometry.w, client.geometry.h) = (600, 400);
        (client.geometry.floating_x, client.geometry.floating_y) = (200, 150);
        (client.geometry.floating_w, client.geometry.floating_h) = (600, 400);
        let key = jwm.insert_client(client);
        jwm.attach_to_monitor(key, shown);
        jwm.focus(&mut backend, Some(key)).expect("focus");
        assert_eq!(jwm.get_selected_client_key(), Some(key));
        jwm.lock_monitor(&mut backend, &WMArgEnum::Int(1))
            .expect("monitor 1 locks");

        // The drag carried the window just below the shaded output's top
        // edge; that is where the server has it on release.
        backend.window_ops.place(win, 2200, 20, 600, 400);
        jwm.last_mouse_root = (2300.0, 40.0);
        jwm.drag_ctl = Some(DragCtl {
            client: key,
            win,
            mode: DragMode::MoveFloat,
            start_root: (500.0, 170.0),
            activated: true,
            was_floating: true,
            orig_geom: (200, 150, 600, 400),
            orig_index: Some(0),
            mon: Some(shown),
            orig_maximize: MaximizeSnapshot::default(),
        });
        <Jwm as WMController>::on_button_release(
            &mut jwm,
            &mut backend,
            HitTarget::Background { output: None },
            0,
        );

        assert!(jwm.drag_ctl.is_none());
        assert_eq!(jwm.state.sel_mon, Some(shown), "the shade was selected");
        assert_eq!(jwm.get_selected_client_key(), Some(key));
        assert!(
            !jwm.client_is_on_locked_monitor(key),
            "focus behind the shade"
        );
        let client = &jwm.state.clients[key];
        assert_eq!(client.mon, Some(shown));
        assert!(jwm.state.monitor_clients[shown].contains(&key));
        assert!(!jwm.state.monitor_clients[shaded].contains(&key));

        // Pulled back inside the unlocked output, at the drop's height,
        // and remembered as its floating rect.
        let area = jwm.monitor_work_area(shown).expect("work area");
        let (x, y, w, h) = client.rect();
        assert_eq!((w, h), (600, 400));
        assert!(
            x >= area.x && x + client.total_width() <= area.x + area.w,
            "x {x} leaves {area:?}"
        );
        assert!(
            y >= area.y && y + client.total_height() <= area.y + area.h,
            "y {y} leaves {area:?}"
        );
        assert_eq!(
            (
                client.geometry.floating_x,
                client.geometry.floating_y,
                client.geometry.floating_w,
                client.geometry.floating_h,
            ),
            (x, y, w, h)
        );
        let server = backend.window_ops.get_geometry(win).expect("server rect");
        assert_eq!((server.x, server.y), (x, y), "the server has it back");
    }

    /// Regression: when the selection had left the dragged window mid-drag,
    /// the release settled the window on the output under its origin with
    /// no lock check, so a drop over a locked output sent the window behind
    /// the shade. It now stays on its own monitor, pulled back inside it,
    /// and the selection is left alone.
    #[test]
    fn an_unselected_floating_drop_on_a_locked_output_stays_on_its_own_monitor() {
        let mut backend = DropSpyBackend {
            window_ops: DropWindowOps::default(),
            input_ops: DummyInputOps,
            property_ops: DummyPropertyOps,
            output_ops: SpyOutputOps {
                outputs: vec![output(1, 0, 0, 1920, 1080), output(2, 1920, 0, 1920, 1080)],
            },
            key_ops: DummyKeyOps,
            cursor_provider: DummyCursorProvider,
            color_allocator: DummyColorAllocator,
        };
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").expect("test jwm");
        let (shown, shaded) = (jwm.state.monitor_order[0], jwm.state.monitor_order[1]);
        assert_eq!(jwm.state.monitors[shaded].num, 1);

        let tags = jwm.state.monitors[shown].get_active_tags();
        let floating_client = |raw: u64, x: i32| {
            let mut client = WMClient::new(WindowId::from_raw(raw));
            client.mon = Some(shown);
            client.state.tags = tags;
            client.state.is_floating = true;
            (client.geometry.x, client.geometry.y) = (x, 150);
            (client.geometry.w, client.geometry.h) = (600, 400);
            (client.geometry.floating_x, client.geometry.floating_y) = (x, 150);
            (client.geometry.floating_w, client.geometry.floating_h) = (600, 400);
            client
        };
        let win = WindowId::from_raw(0x5e02);
        let key = jwm.insert_client(floating_client(0x5e02, 200));
        jwm.attach_to_monitor(key, shown);
        let other = jwm.insert_client(floating_client(0x5e03, 900));
        jwm.attach_to_monitor(other, shown);
        jwm.focus(&mut backend, Some(key)).expect("focus");
        jwm.lock_monitor(&mut backend, &WMArgEnum::Int(1))
            .expect("monitor 1 locks");

        jwm.drag_ctl = Some(DragCtl {
            client: key,
            win,
            mode: DragMode::MoveFloat,
            start_root: (500.0, 170.0),
            activated: true,
            was_floating: true,
            orig_geom: (200, 150, 600, 400),
            orig_index: Some(0),
            mon: Some(shown),
            orig_maximize: MaximizeSnapshot::default(),
        });
        // The selection moves on while the pointer still carries the window.
        jwm.focus(&mut backend, Some(other)).expect("focus");
        assert_eq!(jwm.get_selected_client_key(), Some(other));

        backend.window_ops.place(win, 2200, 20, 600, 400);
        jwm.last_mouse_root = (2300.0, 40.0);
        <Jwm as WMController>::on_button_release(
            &mut jwm,
            &mut backend,
            HitTarget::Background { output: None },
            0,
        );

        assert!(jwm.drag_ctl.is_none());
        assert_eq!(jwm.state.sel_mon, Some(shown));
        assert_eq!(jwm.get_selected_client_key(), Some(other));
        let client = &jwm.state.clients[key];
        assert_eq!(client.mon, Some(shown), "the window went behind the shade");
        assert!(jwm.state.monitor_clients[shown].contains(&key));
        assert!(!jwm.state.monitor_clients[shaded].contains(&key));

        let area = jwm.monitor_work_area(shown).expect("work area");
        let (x, y, w, h) = client.rect();
        assert_eq!((w, h), (600, 400));
        assert!(
            x >= area.x && x + client.total_width() <= area.x + area.w,
            "x {x} leaves {area:?}"
        );
        assert!(
            y >= area.y && y + client.total_height() <= area.y + area.h,
            "y {y} leaves {area:?}"
        );
        assert_eq!(
            (
                client.geometry.floating_x,
                client.geometry.floating_y,
                client.geometry.floating_w,
                client.geometry.floating_h,
            ),
            (x, y, w, h)
        );
        let server = backend.window_ops.get_geometry(win).expect("server rect");
        assert_eq!((server.x, server.y), (x, y), "the server has it back");
    }
}
