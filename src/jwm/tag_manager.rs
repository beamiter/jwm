//! 标签（Tag）管理模块
//!
//! 这个模块包含所有与窗口标签和工作区管理相关的功能

use crate::backend::api::Backend;
use crate::config::CONFIG;
use crate::core::models::{ClientKey, MonitorKey};
use crate::jwm::Jwm;
use crate::jwm::statusbar::StatusBarBuilder;
use crate::jwm::types::WMArgEnum;
use log::warn;

impl Jwm {
    /// 将当前选中的窗口移动到指定标签
    ///
    /// 参数 arg 应为 `UInt(tag_mask)`，表示目标标签掩码
    pub fn tag(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // info!("[tag]");
        if let WMArgEnum::UInt(ui) = *arg {
            if let Some(client_key) = self.get_selected_client_key() {
                self.move_client_to_tag(backend, client_key, ui)?;
            }
        }
        Ok(())
    }

    /// 把指定窗口移动到目标标签掩码（dwm `tag()` 语义：tags 是替换而非合并，
    /// 多标签窗口移动后只属于目标标签）。掩码先与 tagmask 求交，空掩码不动；
    /// 生效后同步 EWMH 标签属性、重新聚焦并重排。`Mod1+Shift+数字` 与 tags
    /// 概览面板的拖拽移动共用这一条路径，保证属性、arrange 与 IPC 一致。
    pub(crate) fn move_client_to_tag(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        target_tag: u32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let target_tag = target_tag & CONFIG.load().tagmask();
        if target_tag == 0 {
            return Ok(());
        }
        if let Some(client) = self.state.clients.get_mut(client_key) {
            client.state.tags = target_tag;
        }
        let _ = self.setclienttagprop(backend, client_key);

        self.focus(backend, None)?;
        self.arrange(backend, self.state.sel_mon);
        Ok(())
    }

    /// 将当前选中的窗口移动到指定显示器
    ///
    /// 参数 arg 应为 `Int(i)`，表示方向：+1 下一个，-1 上一个
    pub fn tagmon(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // info!("[tagmon]");

        let sel_client_key = self.get_selected_client_key();
        if sel_client_key.is_none() {
            return Ok(());
        }
        if self.state.monitor_order.len() <= 1 {
            return Ok(());
        }
        if let WMArgEnum::Int(i) = *arg {
            let target_mon = self.dirtomon(&i);
            if let (Some(client_key), Some(target_mon_key)) = (sel_client_key, target_mon) {
                self.sendmon(backend, Some(client_key), Some(target_mon_key));
            }
        }
        Ok(())
    }

    /// 将指定窗口发送到目标显示器
    ///
    /// 内部函数，由 tagmon 调用
    pub(crate) fn sendmon(
        &mut self,
        backend: &mut dyn Backend,
        client_key_opt: Option<ClientKey>,
        target_mon_opt: Option<MonitorKey>,
    ) {
        // info!("[sendmon]");

        let client_key = match client_key_opt {
            Some(key) => key,
            None => return,
        };

        let target_mon_key = match target_mon_opt {
            Some(key) => key,
            None => return,
        };

        let Some((target_monitor_rect, target_work_area)) =
            self.monitor_migration_areas(target_mon_key)
        else {
            return;
        };

        let (source_mon, win, is_hidden, dock_eligible) =
            if let Some(client) = self.state.clients.get(client_key) {
                if client.mon == Some(target_mon_key) {
                    return;
                }
                (
                    client.mon,
                    client.win,
                    client.state.is_hidden,
                    StatusBarBuilder::is_minimized_dock_eligible(client),
                )
            } else {
                return;
            };

        let source_monitor_num = source_mon
            .and_then(|mon_key| self.state.monitors.get(mon_key))
            .map(|monitor| monitor.num);
        let source_work_area = source_mon
            .and_then(|mon_key| self.monitor_migration_areas(mon_key))
            .map(|(_, work_area)| work_area);
        let target_monitor_num = self
            .state
            .monitors
            .get(target_mon_key)
            .map(|monitor| monitor.num);
        let target_dock_shelf = target_monitor_num
            .and_then(|monitor_num| self.minimized_dock_shelves.get(&monitor_num))
            .copied();

        if is_hidden {
            if let Some(source_monitor_num) = source_monitor_num {
                self.clear_minimized_preview_for(backend, source_monitor_num, Some(win));
            }
            // The source bar cannot withdraw this target after `client.mon`
            // changes: commands from that queue are intentionally rejected as
            // cross-monitor stale. Withdraw it while source ownership is still
            // unambiguous, then rebind to the target shelf below.
            backend.compositor_set_window_dock_geometry(win, None);
        }

        let _ = self.unfocus_client(backend, client_key, true);

        self.detach(client_key);
        self.detachstack(client_key);

        // 把该 client 从源显示器的选中记录(monitor.sel + 全部 pertag.sel)中清除,
        // 否则切回源显示器的旧 tag 时会读到一个已迁走的 key。
        if let Some(src) = source_mon {
            if let Some(m) = self.state.monitors.get_mut(src) {
                m.clear_selection_of(client_key);
            }
        }

        if let Some(client) = self.state.clients.get_mut(client_key) {
            client.mon = Some(target_mon_key);
        }

        if let Some(target_monitor) = self.state.monitors.get(target_mon_key) {
            let target_tags = target_monitor.get_active_tags();

            if let Some(client) = self.state.clients.get_mut(client_key) {
                client.state.tags = target_tags;
            }
        }

        if is_hidden {
            self.migrate_hidden_client_restore(
                backend,
                client_key,
                source_work_area,
                target_monitor_rect,
                target_work_area,
            );
        } else {
            // A floating window keeps its place relative to the work area and
            // a fullscreen one fills the target output, instead of staying
            // (mostly) on the monitor it just left.
            self.migrate_visible_client(
                backend,
                client_key,
                source_work_area,
                target_monitor_rect,
                target_work_area,
            );
        }

        self.attach_back(client_key);
        self.attachstack(client_key);

        let _ = self.setclienttagprop(backend, client_key);

        if is_hidden {
            if let Some(target) = target_dock_shelf {
                if dock_eligible {
                    backend.compositor_set_window_dock_geometry(win, Some(target));
                }
            }
            self.mark_bar_update_needed_if_visible(source_monitor_num);
            self.mark_bar_update_needed_if_visible(target_monitor_num);
        }

        let _ = self.focus(backend, None);
        self.arrange(backend, None);
        if is_hidden && let Err(error) = self.persist_minimized_restore_state(backend, client_key) {
            warn!("could not refresh minimized restore state after sendmon: {error}");
        }
    }

    /// 设置窗口的标签属性（EWMH）
    ///
    /// 更新 _NET_WM_DESKTOP 等 X11 属性
    pub(crate) fn setclienttagprop(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(client) = self.state.clients.get(client_key) {
            let monitor_num = client
                .mon
                .and_then(|mk| self.state.monitors.get(mk))
                .map(|m| m.num as u32)
                .unwrap_or(0);

            backend.property_ops().set_client_info_props(
                client.win,
                client.state.tags,
                monitor_num,
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::api::{
        BackendDiagnostics, Capabilities, ColorAllocator, CompositorAnnotation,
        CompositorBenchmark, CompositorControl, CompositorMedia, CompositorRect,
        CompositorWindowEffects, CompositorWorkspaceEffects, CursorProvider, DisplayControl,
        EventHandler, InputOps, KeyOps, MaximizeAxes, MinimizedRestoreState, OutputIdentity,
        OutputInfo, OutputOps, PropertyOps, RenderScheduler, WindowOps, WindowType,
    };
    use crate::backend::common_define::{OutputId, WindowId};
    use crate::backend::error::BackendError;
    use crate::backend::wayland_dummy_ops::{
        DummyColorAllocator, DummyCursorProvider, DummyInputOps, DummyKeyOps, DummyOutputOps,
        DummyPropertyOps, DummyWindowOps,
    };
    use crate::core::maximize::{MaximizeOrigin, maximize_target};
    use crate::core::models::WMClient;
    use crate::core::types::Rect;
    use std::any::Any;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Default)]
    struct DockSpyWindowOps {
        positions: Mutex<Vec<(WindowId, i32, i32)>>,
        configurations: Mutex<Vec<(WindowId, i32, i32, u32, u32, u32)>>,
        /// One-shot: the next `configure` fails without being recorded.
        fail_next_configure: AtomicBool,
    }

    impl WindowOps for DockSpyWindowOps {
        fn set_position(&self, win: WindowId, x: i32, y: i32) -> Result<(), BackendError> {
            self.positions.lock().unwrap().push((win, x, y));
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
            if self.fail_next_configure.swap(false, Ordering::SeqCst) {
                return Err(BackendError::Message("injected configure failure".into()));
            }
            self.configurations
                .lock()
                .unwrap()
                .push((win, x, y, w, h, border));
            Ok(())
        }

        fn set_decoration_style(
            &self,
            win: WindowId,
            border_width: u32,
            border_color: crate::backend::common_define::Pixel,
        ) -> Result<(), BackendError> {
            DummyWindowOps.set_decoration_style(win, border_width, border_color)
        }

        fn raise_window(&self, win: WindowId) -> Result<(), BackendError> {
            DummyWindowOps.raise_window(win)
        }

        fn map_window(&self, win: WindowId) -> Result<(), BackendError> {
            DummyWindowOps.map_window(win)
        }

        fn unmap_window(&self, win: WindowId) -> Result<(), BackendError> {
            DummyWindowOps.unmap_window(win)
        }

        fn close_window(
            &self,
            win: WindowId,
        ) -> Result<crate::backend::api::CloseResult, BackendError> {
            DummyWindowOps.close_window(win)
        }

        fn set_input_focus(&self, win: WindowId) -> Result<(), BackendError> {
            DummyWindowOps.set_input_focus(win)
        }

        fn set_input_focus_root(&self) -> Result<(), BackendError> {
            DummyWindowOps.set_input_focus_root()
        }

        fn get_window_attributes(
            &self,
            win: WindowId,
        ) -> Result<crate::backend::api::WindowAttributes, BackendError> {
            DummyWindowOps.get_window_attributes(win)
        }

        fn get_geometry(
            &self,
            win: WindowId,
        ) -> Result<crate::backend::api::Geometry, BackendError> {
            DummyWindowOps.get_geometry(win)
        }

        fn scan_windows(&self) -> Result<Vec<WindowId>, BackendError> {
            DummyWindowOps.scan_windows()
        }

        fn flush(&self) -> Result<(), BackendError> {
            DummyWindowOps.flush()
        }

        fn kill_client(&self, win: WindowId) -> Result<(), BackendError> {
            DummyWindowOps.kill_client(win)
        }

        fn apply_window_changes(
            &self,
            win: WindowId,
            changes: crate::backend::api::WindowChanges,
        ) -> Result<(), BackendError> {
            DummyWindowOps.apply_window_changes(win, changes)
        }
    }

    #[derive(Default)]
    struct DockSpyPropertyOps {
        minimized_restores: Mutex<Vec<(WindowId, MinimizedRestoreState)>>,
        client_info: Mutex<Vec<(WindowId, u32, u32)>>,
        transient_parent: Mutex<Option<WindowId>>,
        window_types: Mutex<Vec<WindowType>>,
        /// Every accepted `set_maximized_state` publish, in order.
        maximized: Mutex<Vec<(WindowId, MaximizeAxes)>>,
        /// One-shot: the next `set_maximized_state` fails without being recorded.
        fail_next_maximized_write: AtomicBool,
    }

    impl PropertyOps for DockSpyPropertyOps {
        fn get_title(&self, win: WindowId) -> String {
            DummyPropertyOps.get_title(win)
        }

        fn get_class(&self, win: WindowId) -> (String, String) {
            DummyPropertyOps.get_class(win)
        }

        fn get_window_types(&self, _win: WindowId) -> Vec<WindowType> {
            self.window_types.lock().unwrap().clone()
        }

        fn is_fullscreen(&self, win: WindowId) -> bool {
            DummyPropertyOps.is_fullscreen(win)
        }

        fn set_fullscreen_state(&self, win: WindowId, on: bool) -> Result<(), BackendError> {
            DummyPropertyOps.set_fullscreen_state(win, on)
        }

        fn transient_for(&self, _win: WindowId) -> Option<WindowId> {
            *self.transient_parent.lock().unwrap()
        }

        fn get_wm_hints(&self, win: WindowId) -> Option<crate::backend::api::WmHints> {
            DummyPropertyOps.get_wm_hints(win)
        }

        fn set_urgent_hint(&self, win: WindowId, urgent: bool) -> Result<(), BackendError> {
            DummyPropertyOps.set_urgent_hint(win, urgent)
        }

        fn fetch_normal_hints(
            &self,
            win: WindowId,
        ) -> Result<Option<crate::backend::api::NormalHints>, BackendError> {
            DummyPropertyOps.fetch_normal_hints(win)
        }

        fn set_window_strut_top(
            &self,
            win: WindowId,
            top: u32,
            start_x: u32,
            end_x: u32,
        ) -> Result<(), BackendError> {
            DummyPropertyOps.set_window_strut_top(win, top, start_x, end_x)
        }

        fn set_window_type_dock(&self, win: WindowId) -> Result<(), BackendError> {
            DummyPropertyOps.set_window_type_dock(win)
        }

        fn clear_window_strut(&self, win: WindowId) -> Result<(), BackendError> {
            DummyPropertyOps.clear_window_strut(win)
        }

        fn get_wm_state(&self, win: WindowId) -> Result<i64, BackendError> {
            DummyPropertyOps.get_wm_state(win)
        }

        fn set_wm_state(&self, win: WindowId, state: i64) -> Result<(), BackendError> {
            DummyPropertyOps.set_wm_state(win, state)
        }

        fn set_minimized_restore_state(
            &self,
            win: WindowId,
            state: MinimizedRestoreState,
        ) -> Result<(), BackendError> {
            self.minimized_restores.lock().unwrap().push((win, state));
            Ok(())
        }

        fn set_client_info_props(
            &self,
            win: WindowId,
            tags: u32,
            monitor_num: u32,
        ) -> Result<(), BackendError> {
            self.client_info
                .lock()
                .unwrap()
                .push((win, tags, monitor_num));
            Ok(())
        }

        fn set_maximized_state(
            &self,
            win: WindowId,
            axes: MaximizeAxes,
        ) -> Result<(), BackendError> {
            if self.fail_next_maximized_write.swap(false, Ordering::SeqCst) {
                return Err(BackendError::Message(
                    "injected maximize write failure".into(),
                ));
            }
            self.maximized.lock().unwrap().push((win, axes));
            Ok(())
        }
    }

    struct DockSpyBackend {
        window_ops: DockSpyWindowOps,
        input_ops: DummyInputOps,
        property_ops: DockSpyPropertyOps,
        output_ops: DummyOutputOps,
        key_ops: DummyKeyOps,
        cursor_provider: DummyCursorProvider,
        color_allocator: DummyColorAllocator,
        dock_targets: Vec<(WindowId, Option<CompositorRect>)>,
        previews: Vec<(Option<WindowId>, Option<CompositorRect>)>,
    }

    impl DockSpyBackend {
        fn new() -> Self {
            Self {
                window_ops: DockSpyWindowOps::default(),
                input_ops: DummyInputOps,
                property_ops: DockSpyPropertyOps::default(),
                output_ops: DummyOutputOps,
                key_ops: DummyKeyOps,
                cursor_provider: DummyCursorProvider,
                color_allocator: DummyColorAllocator,
                dock_targets: Vec::new(),
                previews: Vec::new(),
            }
        }
    }

    impl CompositorBenchmark for DockSpyBackend {}
    impl BackendDiagnostics for DockSpyBackend {}
    impl CompositorControl for DockSpyBackend {}
    impl CompositorMedia for DockSpyBackend {}
    impl CompositorWorkspaceEffects for DockSpyBackend {}
    impl CompositorWindowEffects for DockSpyBackend {
        fn compositor_set_window_dock_geometry(
            &mut self,
            window: WindowId,
            target: Option<CompositorRect>,
        ) {
            self.dock_targets.push((window, target));
        }

        fn compositor_set_minimized_window_preview(
            &mut self,
            window: Option<WindowId>,
            anchor: Option<CompositorRect>,
        ) {
            self.previews.push((window, anchor));
        }
    }
    impl CompositorAnnotation for DockSpyBackend {}
    impl DisplayControl for DockSpyBackend {}
    impl RenderScheduler for DockSpyBackend {
        fn has_compositor(&self) -> bool {
            true
        }
    }

    impl Backend for DockSpyBackend {
        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }

        fn root_window(&self) -> Option<WindowId> {
            Some(WindowId::from_raw(0))
        }

        fn as_any(&self) -> &dyn Any {
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
    }

    #[test]
    fn hidden_sendmon_withdraws_source_dock_state_and_retargets_the_destination() {
        let mut backend = DockSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let source = jwm.state.monitor_order[0];
        jwm.add_monitor(OutputInfo {
            id: OutputId(1),
            name: "Virtual-2".into(),
            x: 1920,
            y: 0,
            width: 1920,
            height: 1080,
            scale: 1.0,
            refresh_rate: 60_000,
            hdr_capable: false,
            hdr_metadata: None,
            identity: OutputIdentity::connector_only("Virtual-2"),
        });
        let target = jwm.state.monitor_order[1];
        let source_num = jwm.state.monitors[source].num;
        let target_num = jwm.state.monitors[target].num;

        let window = WindowId::from_raw(0x202);
        let mut client = WMClient::new(window);
        client.mon = Some(source);
        client.state.tags = 1;
        client.state.is_hidden = true;
        client.state.minimized_order = 7;
        client.geometry.x = -1600;
        client.geometry.old_x = 120;
        client.geometry.w = 800;
        client.geometry.h = 600;
        let client_key = jwm.insert_client(client);
        jwm.attach_to_monitor(client_key, source);

        let target_shelf = CompositorRect::new(3500.0, 4.0, 180.0, 36.0);
        jwm.minimized_dock_shelves.insert(target_num, target_shelf);
        jwm.active_minimized_preview = Some((source_num, window));
        jwm.pending_bar_updates.clear();

        jwm.sendmon(&mut backend, Some(client_key), Some(target));

        assert_eq!(jwm.state.clients[client_key].mon, Some(target));
        assert!(jwm.state.clients[client_key].state.is_hidden);
        assert_eq!(jwm.active_minimized_preview, None);
        assert_eq!(backend.previews, vec![(None, None)]);
        assert_eq!(
            backend.dock_targets,
            vec![(window, None), (window, Some(target_shelf))]
        );
        assert!(jwm.pending_bar_updates.contains(&source_num));
        assert!(jwm.pending_bar_updates.contains(&target_num));
        let first_restore = backend
            .property_ops
            .minimized_restores
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("sendmon must refresh the restart snapshot");
        assert_eq!(first_restore.0, window);
        assert_eq!(first_restore.1.monitor_num, target_num);
        assert_eq!(
            first_restore.1.tags,
            jwm.state.clients[client_key].state.tags
        );
        let visible = jwm.state.clients[client_key]
            .geometry
            .hidden_restore_rect
            .expect("hidden client has a semantic restore slot");
        assert_eq!(
            (
                first_restore.1.visible_rect.x,
                first_restore.1.visible_rect.y,
                first_restore.1.visible_rect.w,
                first_restore.1.visible_rect.h,
            ),
            (visible.x, visible.y, visible.w, visible.h)
        );

        // A destination without a live bar shelf stays explicitly withdrawn;
        // it must not inherit the physical target from the previous monitor.
        backend.dock_targets.clear();
        backend.previews.clear();
        jwm.active_minimized_preview = Some((target_num, window));
        jwm.pending_bar_updates.clear();
        jwm.sendmon(&mut backend, Some(client_key), Some(source));

        assert_eq!(jwm.state.clients[client_key].mon, Some(source));
        assert_eq!(jwm.active_minimized_preview, None);
        assert_eq!(backend.previews, vec![(None, None)]);
        assert_eq!(backend.dock_targets, vec![(window, None)]);
        assert!(jwm.pending_bar_updates.contains(&source_num));
        assert!(jwm.pending_bar_updates.contains(&target_num));

        // Hidden clients that are not represented by the Dock must stay
        // targetless even when the destination has a live shelf. Moving by
        // XID through IPC is allowed for these clients, so sendmon itself is
        // the final eligibility gate.
        backend.dock_targets.clear();
        jwm.state.clients[client_key].state.skip_taskbar = true;
        jwm.sendmon(&mut backend, Some(client_key), Some(target));

        assert_eq!(jwm.state.clients[client_key].mon, Some(target));
        assert_eq!(backend.dock_targets, vec![(window, None)]);
    }

    /// A second output to the right of the primary one.
    fn add_right_monitor(jwm: &mut Jwm, source: MonitorKey) -> MonitorKey {
        let (source_monitor, _) = jwm.monitor_migration_areas(source).unwrap();
        jwm.add_monitor(OutputInfo {
            id: OutputId(2),
            name: "Right".into(),
            x: source_monitor.x + source_monitor.w,
            y: source_monitor.y,
            width: 1280,
            height: 720,
            scale: 1.0,
            refresh_rate: 60_000,
            hdr_capable: false,
            hdr_metadata: None,
            identity: OutputIdentity::connector_only("Right"),
        });
        jwm.state.monitor_order[1]
    }

    fn visible_client(jwm: &mut Jwm, mon: MonitorKey, raw: u64, rect: Rect) -> ClientKey {
        let mut client = WMClient::new(WindowId::from_raw(raw));
        client.mon = Some(mon);
        client.state.tags = 1;
        client.geometry.x = rect.x;
        client.geometry.y = rect.y;
        client.geometry.w = rect.w;
        client.geometry.h = rect.h;
        let key = jwm.insert_client(client);
        jwm.attach_to_monitor(key, mon);
        key
    }

    #[test]
    fn visible_floating_sendmon_keeps_its_place_on_the_target_work_area() {
        let mut backend = DockSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let source = jwm.state.monitor_order[0];
        let target = add_right_monitor(&mut jwm, source);
        let (_, source_work) = jwm.monitor_migration_areas(source).unwrap();
        let (_, target_work) = jwm.monitor_migration_areas(target).unwrap();

        let key = visible_client(
            &mut jwm,
            source,
            0x401,
            Rect::new(source_work.x + 100, source_work.y + 80, 500, 300),
        );
        jwm.state.clients[key].state.is_floating = true;

        jwm.sendmon(&mut backend, Some(key), Some(target));

        let client = &jwm.state.clients[key];
        assert_eq!(client.mon, Some(target));
        assert_eq!(
            (client.geometry.x, client.geometry.y, client.geometry.w, client.geometry.h),
            (target_work.x + 100, target_work.y + 80, 500, 300),
            "the window lands on the target, not a pixel inside its edge"
        );
        assert_eq!(
            (client.geometry.floating_x, client.geometry.floating_y),
            (client.geometry.x, client.geometry.y),
            "toggling floating later keeps the new place"
        );
    }

    #[test]
    fn a_floating_window_dropped_on_another_monitor_stays_where_it_was_dropped() {
        let mut backend = DockSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let source = jwm.state.monitor_order[0];
        let target = add_right_monitor(&mut jwm, source);
        let (target_monitor, _) = jwm.monitor_migration_areas(target).unwrap();

        // A drag already moved it onto the target; the release hands it over.
        let dropped = Rect::new(target_monitor.x + 100, target_monitor.y + 120, 400, 300);
        let key = visible_client(&mut jwm, source, 0x409, dropped);
        jwm.state.clients[key].state.is_floating = true;

        jwm.sendmon(&mut backend, Some(key), Some(target));

        let client = &jwm.state.clients[key];
        assert_eq!(
            (client.geometry.x, client.geometry.y, client.geometry.w, client.geometry.h),
            (dropped.x, dropped.y, dropped.w, dropped.h),
            "a drop is not shifted by the distance between the work areas again"
        );
    }

    #[test]
    fn unplugging_leaves_parked_windows_off_screen_and_the_bar_alone() {
        let mut backend = DockSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let primary = jwm.state.monitor_order[0];
        let right = add_right_monitor(&mut jwm, primary);
        let (right_monitor, _) = jwm.monitor_migration_areas(right).unwrap();
        let bar_key = visible_client(
            &mut jwm,
            right,
            0x40b,
            Rect::new(right_monitor.x, right_monitor.y, 1280, 30),
        );
        {
            let bar = &mut jwm.state.clients[bar_key];
            bar.state.is_dock = true;
            bar.state.is_floating = true;
        }
        // Work areas as the migration sees them, with the bar in place.
        let (_, right_work) = jwm.monitor_migration_areas(right).unwrap();
        let (_, primary_work) = jwm.monitor_migration_areas(primary).unwrap();

        // Floating, on tag 2 while tag 1 is shown: parked off-screen.
        let restore = Rect::new(right_work.x + 50, right_work.y + 60, 300, 200);
        let parked = visible_client(&mut jwm, right, 0x40a, restore);
        {
            let client = &mut jwm.state.clients[parked];
            client.state.tags = 0b10;
            client.state.is_floating = true;
            client.geometry.hidden_restore_rect = Some(restore);
            client.geometry.hidden_x = Some(-5000);
            client.geometry.x = -5000;
        }
        backend.window_ops.configurations.lock().unwrap().clear();

        jwm.handle_output_removed(&mut backend, OutputId(2)).unwrap();

        let client = &jwm.state.clients[parked];
        let moved = client.geometry.hidden_restore_rect.expect("still parked");
        assert_eq!(
            (moved.x, moved.y),
            (primary_work.x + 50, primary_work.y + 60),
            "its return rectangle follows it to the surviving output"
        );
        assert!(
            backend
                .window_ops
                .configurations
                .lock()
                .unwrap()
                .iter()
                .filter(|(win, ..)| *win == client.win)
                .all(|&(_, x, ..)| x < primary_work.x),
            "a parked window is never configured on screen by the migration"
        );
    }

    #[test]
    fn visible_fullscreen_sendmon_fills_the_target_and_returns_to_it() {
        let mut backend = DockSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let source = jwm.state.monitor_order[0];
        let target = add_right_monitor(&mut jwm, source);
        let (_, source_work) = jwm.monitor_migration_areas(source).unwrap();
        let (target_monitor, target_work) = jwm.monitor_migration_areas(target).unwrap();

        let key = visible_client(
            &mut jwm,
            source,
            0x402,
            Rect::new(source_work.x + 60, source_work.y + 40, 400, 250),
        );
        // Floating, so leaving fullscreen returns to a rectangle rather than
        // to a tile the layout recomputes.
        jwm.state.clients[key].state.is_floating = true;
        jwm.setfullscreen(&mut backend, key, true).unwrap();

        jwm.sendmon(&mut backend, Some(key), Some(target));
        {
            let client = &jwm.state.clients[key];
            assert!(client.state.is_fullscreen);
            assert_eq!(
                (client.geometry.x, client.geometry.y, client.geometry.w, client.geometry.h),
                (target_monitor.x, target_monitor.y, target_monitor.w, target_monitor.h),
                "fullscreen fills the output it moved to"
            );
        }

        jwm.setfullscreen(&mut backend, key, false).unwrap();
        let client = &jwm.state.clients[key];
        assert_eq!(
            (client.geometry.x, client.geometry.y, client.geometry.w, client.geometry.h),
            (target_work.x + 60, target_work.y + 40, 400, 250),
            "leaving fullscreen returns to the translated pre-fullscreen rect, \
             not a monitor-sized window on the old output"
        );
    }

    #[test]
    fn a_layout_change_leaves_fullscreen_windows_on_other_tags_alone() {
        use crate::core::layout::LayoutEnum;
        use crate::jwm::WMArgEnum;
        use std::rc::Rc;

        let mut backend = DockSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let mon = jwm.state.monitor_order[0];
        let (_, work) = jwm.monitor_migration_areas(mon).unwrap();
        let here = visible_client(&mut jwm, mon, 0x403, Rect::new(work.x, work.y, 300, 200));
        let away = visible_client(&mut jwm, mon, 0x404, Rect::new(work.x, work.y, 300, 200));
        jwm.setfullscreen(&mut backend, here, true).unwrap();
        jwm.setfullscreen(&mut backend, away, true).unwrap();
        // The second video lives on tag 2, which is not being viewed.
        jwm.state.clients[away].state.tags = 0b10;

        jwm.setlayout(&mut backend, &WMArgEnum::Layout(Rc::new(LayoutEnum::MONOCLE)))
            .unwrap();

        assert!(
            !jwm.state.clients[here].state.is_fullscreen,
            "the visible fullscreen window still yields to the new layout"
        );
        assert!(
            jwm.state.clients[away].state.is_fullscreen,
            "a fullscreen window on another tag keeps its state"
        );
    }

    #[test]
    fn togglefloating_leaves_fullscreen_and_pip_windows_as_they_are() {
        use crate::jwm::WMArgEnum;

        let mut backend = DockSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let mon = jwm.state.monitor_order[0];
        let (_, work) = jwm.monitor_migration_areas(mon).unwrap();
        let key = visible_client(&mut jwm, mon, 0x405, Rect::new(work.x, work.y, 300, 200));
        jwm.setfullscreen(&mut backend, key, true).unwrap();
        jwm.focus(&mut backend, Some(key)).unwrap();
        let before = jwm.state.clients[key].geometry.clone();

        jwm.togglefloating(&mut backend, &WMArgEnum::Int(0)).unwrap();

        let client = &jwm.state.clients[key];
        assert!(client.state.is_fullscreen);
        assert!(client.state.is_floating, "fullscreen keeps owning is_floating");
        assert_eq!(client.geometry.floating_w, before.floating_w);
        assert_eq!(client.geometry.floating_h, before.floating_h);
    }

    #[test]
    fn focus_none_drops_focus_to_the_root() {
        use crate::jwm::WMArgEnum;

        let mut backend = DockSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let mon = jwm.state.monitor_order[0];
        let (_, work) = jwm.monitor_migration_areas(mon).unwrap();
        let key = visible_client(&mut jwm, mon, 0x406, Rect::new(work.x, work.y, 300, 200));
        jwm.focus(&mut backend, Some(key)).unwrap();
        assert_eq!(jwm.get_selected_client_key(), Some(key));

        jwm.focus_none(&mut backend, &WMArgEnum::Int(0)).unwrap();

        assert_eq!(jwm.get_selected_client_key(), None);
    }

    #[test]
    fn loopview_switches_tags_through_the_view_path() {
        use crate::jwm::WMArgEnum;

        let mut backend = DockSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let mon = jwm.state.monitor_order[0];
        {
            let monitor = &mut jwm.state.monitors[mon];
            monitor.view_tag(1, false);
            let pertag = monitor.pertag.as_mut().unwrap();
            pertag.gaps[1] = 5;
            pertag.gaps[2] = 40;
            monitor.layout.gap = 5;
        }

        jwm.loopview(&mut backend, &WMArgEnum::Int(1)).unwrap();

        let monitor = &jwm.state.monitors[mon];
        assert_eq!(monitor.get_active_tags(), 0b10);
        assert_eq!(monitor.pertag.as_ref().unwrap().cur_tag, 2);
        assert_eq!(monitor.layout.gap, 40, "tag 2's own gap, not tag 1's");

        jwm.loopview(&mut backend, &WMArgEnum::Int(-1)).unwrap();
        assert_eq!(jwm.state.monitors[mon].layout.gap, 5);
    }

    #[test]
    fn togglesticky_adopts_the_current_tags_and_publishes_them() {
        use crate::jwm::WMArgEnum;

        let mut backend = DockSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let mon = jwm.state.monitor_order[0];
        let (_, work) = jwm.monitor_migration_areas(mon).unwrap();
        jwm.state.monitors[mon].view_tag(0b100, false);
        let key = visible_client(&mut jwm, mon, 0x407, Rect::new(work.x, work.y, 300, 200));
        jwm.state.clients[key].state.tags = 0b100;
        jwm.focus(&mut backend, Some(key)).unwrap();
        backend.property_ops.client_info.lock().unwrap().clear();

        jwm.togglesticky(&mut backend, &WMArgEnum::Int(0)).unwrap();

        let client = &jwm.state.clients[key];
        assert!(client.state.is_sticky);
        assert_eq!(client.state.tags, 0b100);
        assert!(
            backend
                .property_ops
                .client_info
                .lock()
                .unwrap()
                .iter()
                .any(|&(win, tags, _)| win == client.win && tags == 0b100),
            "the desktop property follows the sticky toggle"
        );

        jwm.togglesticky(&mut backend, &WMArgEnum::Int(0)).unwrap();
        assert!(!jwm.state.clients[key].state.is_sticky);
    }

    #[test]
    fn a_transient_of_an_unmanaged_parent_still_floats_after_the_rules() {
        let mut backend = DockSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let mon = jwm.state.monitor_order[0];
        let (_, work) = jwm.monitor_migration_areas(mon).unwrap();
        let key = visible_client(&mut jwm, mon, 0x408, Rect::new(work.x, work.y, 300, 200));
        // A parent the WM does not manage (override-redirect, not mapped yet).
        *backend.property_ops.transient_parent.lock().unwrap() = Some(WindowId::from_raw(0x9ff));

        jwm.handle_transient_for(&mut backend, key).unwrap();

        assert!(jwm.state.clients[key].state.is_floating);
    }

    #[test]
    fn hidden_floating_sendmon_translates_negative_origin_restore_and_persists_it() {
        let mut backend = DockSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let source = jwm.state.monitor_order[0];
        jwm.add_monitor(OutputInfo {
            id: OutputId(2),
            name: "Left".into(),
            x: -1280,
            y: -180,
            width: 1280,
            height: 800,
            scale: 1.0,
            refresh_rate: 60_000,
            hdr_capable: false,
            hdr_metadata: None,
            identity: OutputIdentity::connector_only("Left"),
        });
        let target = jwm.state.monitor_order[1];
        let (_, source_work) = jwm.monitor_migration_areas(source).unwrap();
        let (_, target_work) = jwm.monitor_migration_areas(target).unwrap();

        let window = WindowId::from_raw(0x303);
        let visible = Rect::new(source_work.x + 140, source_work.y + 90, 540, 360);
        let mut client = WMClient::new(window);
        client.mon = Some(source);
        client.state.tags = 1;
        client.state.is_hidden = true;
        client.state.is_floating = true;
        client.state.is_pip = true;
        client.state.minimized_order = 19;
        client.geometry.hidden_restore_rect = Some(visible);
        client.geometry.floating_x = visible.x;
        client.geometry.floating_y = visible.y;
        client.geometry.floating_w = visible.w;
        client.geometry.floating_h = visible.h;
        client.geometry.x = -5000;
        client.geometry.y = visible.y;
        client.geometry.w = visible.w;
        client.geometry.h = visible.h;
        let client_key = jwm.insert_client(client);
        jwm.attach_to_monitor(client_key, source);

        jwm.sendmon(&mut backend, Some(client_key), Some(target));

        let client = &jwm.state.clients[client_key];
        let migrated = client.geometry.hidden_restore_rect.unwrap();
        assert_eq!(migrated.x, target_work.x + 140);
        assert_eq!(migrated.y, target_work.y + 90);
        assert_eq!((migrated.w, migrated.h), (540, 360));
        assert_eq!(
            (
                client.geometry.floating_x,
                client.geometry.floating_y,
                client.geometry.floating_w,
                client.geometry.floating_h,
            ),
            (migrated.x, migrated.y, migrated.w, migrated.h)
        );
        assert!(client.geometry.x.saturating_add(client.total_width()) <= jwm.desktop_left_edge());
        let parked = backend
            .window_ops
            .configurations
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(configured, ..)| *configured == window)
            .copied()
            .expect("sendmon configures the real window at its new parking rectangle");
        assert_eq!(
            (parked.1, parked.2, parked.3, parked.4),
            (
                client.geometry.x,
                client.geometry.y,
                client.geometry.w as u32,
                client.geometry.h as u32,
            )
        );

        let (_, snapshot) = backend
            .property_ops
            .minimized_restores
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap();
        assert_eq!(snapshot.monitor_num, jwm.state.monitors[target].num);
        assert_eq!(snapshot.tags, client.state.tags);
        assert_eq!(
            (
                snapshot.visible_rect.x,
                snapshot.visible_rect.y,
                snapshot.visible_rect.w,
                snapshot.visible_rect.h,
            ),
            (migrated.x, migrated.y, migrated.w, migrated.h)
        );
        let floating = snapshot.floating_rect.unwrap();
        assert_eq!(
            (floating.x, floating.y, floating.w, floating.h),
            (migrated.x, migrated.y, migrated.w, migrated.h)
        );
    }

    #[test]
    fn output_changed_rebases_hidden_restore_and_private_property() {
        let mut backend = DockSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let output_id = jwm.state.output_map[monitor];
        let (_, old_work) = jwm.monitor_migration_areas(monitor).unwrap();

        let window = WindowId::from_raw(0x404);
        let visible = Rect::new(old_work.x + 180, old_work.y + 120, 420, 280);
        let mut client = WMClient::new(window);
        client.mon = Some(monitor);
        client.state.tags = 0b101;
        client.state.is_hidden = true;
        client.state.is_floating = true;
        client.state.minimized_order = 31;
        client.geometry.hidden_restore_rect = Some(visible);
        client.geometry.floating_x = visible.x;
        client.geometry.floating_y = visible.y;
        client.geometry.floating_w = visible.w;
        client.geometry.floating_h = visible.h;
        client.geometry.x = -5000;
        client.geometry.y = visible.y;
        client.geometry.w = visible.w;
        client.geometry.h = visible.h;
        let client_key = jwm.insert_client(client);
        jwm.attach_to_monitor(client_key, monitor);

        jwm.handle_output_changed(
            &mut backend,
            OutputInfo {
                id: output_id,
                name: "Moved".into(),
                x: -1600,
                y: -240,
                width: 1400,
                height: 900,
                scale: 1.25,
                refresh_rate: 75_000,
                hdr_capable: false,
                hdr_metadata: None,
                identity: OutputIdentity::connector_only("Moved"),
            },
        )
        .unwrap();

        let (_, new_work) = jwm.monitor_migration_areas(monitor).unwrap();
        let client = &jwm.state.clients[client_key];
        let migrated = client.geometry.hidden_restore_rect.unwrap();
        assert_eq!(migrated.x, new_work.x + 180);
        assert_eq!(migrated.y, new_work.y + 120);
        assert_eq!((migrated.w, migrated.h), (420, 280));
        assert!(client.geometry.x.saturating_add(client.total_width()) <= jwm.desktop_left_edge());

        let (_, snapshot) = backend
            .property_ops
            .minimized_restores
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("OutputChanged refreshes the private snapshot");
        assert_eq!(snapshot.monitor_num, jwm.state.monitors[monitor].num);
        assert_eq!(snapshot.tags, 0b101);
        assert_eq!(
            (
                snapshot.visible_rect.x,
                snapshot.visible_rect.y,
                snapshot.visible_rect.w,
                snapshot.visible_rect.h,
            ),
            (migrated.x, migrated.y, migrated.w, migrated.h)
        );
    }

    #[test]
    fn last_output_orphan_converges_when_a_negative_origin_output_returns() {
        let mut backend = DockSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let removed = jwm.state.monitor_order[0];
        let removed_id = jwm.state.output_map[removed];
        let (_, work) = jwm.monitor_migration_areas(removed).unwrap();

        let window = WindowId::from_raw(0x505);
        let visible = Rect::new(work.x + 260, work.y + 130, 640, 420);
        let mut client = WMClient::new(window);
        client.mon = Some(removed);
        client.state.tags = 0b10;
        client.state.is_hidden = true;
        client.state.is_floating = true;
        client.state.minimized_order = 77;
        client.geometry.hidden_restore_rect = Some(visible);
        client.geometry.floating_x = visible.x;
        client.geometry.floating_y = visible.y;
        client.geometry.floating_w = visible.w;
        client.geometry.floating_h = visible.h;
        client.geometry.x = -5000;
        client.geometry.y = visible.y;
        client.geometry.w = visible.w;
        client.geometry.h = visible.h;
        let client_key = jwm.insert_client(client);
        jwm.attach_to_monitor(client_key, removed);

        jwm.handle_output_removed(&mut backend, removed_id).unwrap();
        assert!(jwm.state.monitors.is_empty());
        assert_eq!(jwm.state.clients[client_key].mon, None);
        let orphan_snapshot = backend
            .property_ops
            .minimized_restores
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap()
            .1;
        assert_eq!(orphan_snapshot.monitor_num, -1);
        assert_eq!(orphan_snapshot.tags, 0b10);

        jwm.handle_output_added(
            &mut backend,
            OutputInfo {
                id: OutputId(9),
                name: "Replacement".into(),
                x: -1500,
                y: -320,
                width: 1100,
                height: 700,
                scale: 1.0,
                refresh_rate: 60_000,
                hdr_capable: false,
                hdr_metadata: None,
                identity: OutputIdentity::connector_only("Replacement"),
            },
        )
        .unwrap();

        let replacement = jwm.state.monitor_order[0];
        let (_, target_work) = jwm.monitor_migration_areas(replacement).unwrap();
        let desktop_left = jwm.desktop_left_edge();
        let client = &jwm.state.clients[client_key];
        assert_eq!(client.mon, Some(replacement));
        assert!(client.state.is_hidden);
        let restored = client.geometry.hidden_restore_rect.unwrap();
        assert!(restored.x >= target_work.x);
        assert!(restored.y >= target_work.y);
        assert!(restored.x + restored.w <= target_work.x + target_work.w);
        assert!(restored.y + restored.h <= target_work.y + target_work.h);
        assert!(client.geometry.x + client.total_width() <= desktop_left);

        let replacement_snapshot = backend
            .property_ops
            .minimized_restores
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap()
            .1;
        assert_eq!(
            replacement_snapshot.monitor_num,
            jwm.state.monitors[replacement].num
        );
        assert_eq!(replacement_snapshot.tags, 0b10);
        assert_eq!(
            (
                replacement_snapshot.visible_rect.x,
                replacement_snapshot.visible_rect.y,
                replacement_snapshot.visible_rect.w,
                replacement_snapshot.visible_rect.h,
            ),
            (restored.x, restored.y, restored.w, restored.h)
        );
    }

    #[test]
    fn hot_unplug_translates_a_hidden_floating_client_to_the_survivor() {
        let mut backend = DockSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let source = jwm.state.monitor_order[0];
        let source_id = jwm.state.output_map[source];
        jwm.add_monitor(OutputInfo {
            id: OutputId(12),
            name: "Survivor".into(),
            x: 1920,
            y: -100,
            width: 1280,
            height: 800,
            scale: 1.0,
            refresh_rate: 60_000,
            hdr_capable: false,
            hdr_metadata: None,
            identity: OutputIdentity::connector_only("Survivor"),
        });
        let survivor = jwm.state.monitor_order[1];
        let (_, source_work) = jwm.monitor_migration_areas(source).unwrap();
        let (_, target_work) = jwm.monitor_migration_areas(survivor).unwrap();

        let window = WindowId::from_raw(0x606);
        let visible = Rect::new(source_work.x + 120, source_work.y + 80, 500, 340);
        let mut client = WMClient::new(window);
        client.mon = Some(source);
        client.state.tags = 1;
        client.state.is_hidden = true;
        client.state.is_floating = true;
        client.state.minimized_order = 91;
        client.geometry.hidden_restore_rect = Some(visible);
        client.geometry.floating_x = visible.x;
        client.geometry.floating_y = visible.y;
        client.geometry.floating_w = visible.w;
        client.geometry.floating_h = visible.h;
        client.geometry.x = -5000;
        client.geometry.y = visible.y;
        client.geometry.w = visible.w;
        client.geometry.h = visible.h;
        let client_key = jwm.insert_client(client);
        jwm.attach_to_monitor(client_key, source);

        jwm.handle_output_removed(&mut backend, source_id).unwrap();

        let desktop_left = jwm.desktop_left_edge();
        let client = &jwm.state.clients[client_key];
        assert_eq!(client.mon, Some(survivor));
        let migrated = client.geometry.hidden_restore_rect.unwrap();
        assert_eq!(migrated.x, target_work.x + 120);
        assert_eq!(migrated.y, target_work.y + 80);
        assert_eq!((migrated.w, migrated.h), (500, 340));
        assert!(client.geometry.x + client.total_width() <= desktop_left);

        let snapshot = backend
            .property_ops
            .minimized_restores
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap()
            .1;
        assert_eq!(snapshot.monitor_num, jwm.state.monitors[survivor].num);
        assert_eq!(
            (
                snapshot.visible_rect.x,
                snapshot.visible_rect.y,
                snapshot.visible_rect.w,
                snapshot.visible_rect.h,
            ),
            (migrated.x, migrated.y, migrated.w, migrated.h)
        );
    }

    #[test]
    fn adding_a_far_left_output_reparks_hidden_clients_on_unchanged_monitors() {
        let mut backend = DockSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let original = jwm.state.monitor_order[0];
        let (_, original_work) = jwm.monitor_migration_areas(original).unwrap();
        let window = WindowId::from_raw(0x707);
        let visible = Rect::new(original_work.x + 100, original_work.y + 80, 600, 400);
        let mut client = WMClient::new(window);
        client.mon = Some(original);
        client.state.tags = 1;
        client.state.is_hidden = true;
        client.state.minimized_order = 101;
        client.geometry.hidden_restore_rect = Some(visible);
        client.geometry.x = -1300;
        client.geometry.y = visible.y;
        client.geometry.w = visible.w;
        client.geometry.h = visible.h;
        let client_key = jwm.insert_client(client);
        jwm.attach_to_monitor(client_key, original);

        jwm.handle_output_added(
            &mut backend,
            OutputInfo {
                id: OutputId(13),
                name: "FarLeft".into(),
                x: -4200,
                y: 20,
                width: 1200,
                height: 800,
                scale: 1.0,
                refresh_rate: 60_000,
                hdr_capable: false,
                hdr_metadata: None,
                identity: OutputIdentity::connector_only("FarLeft"),
            },
        )
        .unwrap();

        let client = &jwm.state.clients[client_key];
        assert_eq!(client.mon, Some(original));
        assert_eq!(client.geometry.hidden_restore_rect, Some(visible));
        assert!(client.geometry.x + client.total_width() <= -4200);
        assert!(
            backend
                .window_ops
                .positions
                .lock()
                .unwrap()
                .iter()
                .any(|&(positioned, x, _)| positioned == window && x == client.geometry.x)
        );
    }

    #[test]
    fn minimized_scratchpad_rejoins_the_dock_after_last_output_replacement() {
        let mut backend = DockSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let removed = jwm.state.monitor_order[0];
        let removed_id = jwm.state.output_map[removed];
        let (_, work) = jwm.monitor_migration_areas(removed).unwrap();

        let window = WindowId::from_raw(0x808);
        let visible = Rect::new(work.x + 180, work.y + 110, 620, 400);
        let mut scratchpad = WMClient::new(window);
        scratchpad.name = "scratch-term".into();
        scratchpad.mon = Some(removed);
        scratchpad.state.tags = 0;
        scratchpad.state.is_hidden = true;
        scratchpad.state.is_floating = true;
        scratchpad.state.minimized_order = 111;
        scratchpad.geometry.hidden_restore_rect = Some(visible);
        scratchpad.geometry.floating_x = visible.x;
        scratchpad.geometry.floating_y = visible.y;
        scratchpad.geometry.floating_w = visible.w;
        scratchpad.geometry.floating_h = visible.h;
        scratchpad.geometry.x = -5000;
        scratchpad.geometry.y = visible.y;
        scratchpad.geometry.w = visible.w;
        scratchpad.geometry.h = visible.h;
        let scratchpad_key = jwm.insert_client(scratchpad);
        jwm.attach_to_monitor(scratchpad_key, removed);
        jwm.scratchpads
            .insert("scratch-term".into(), scratchpad_key);

        jwm.handle_output_removed(&mut backend, removed_id).unwrap();
        assert_eq!(jwm.state.clients[scratchpad_key].mon, None);

        jwm.handle_output_added(
            &mut backend,
            OutputInfo {
                id: OutputId(14),
                name: "Replacement".into(),
                x: -900,
                y: 0,
                width: 1440,
                height: 900,
                scale: 1.0,
                refresh_rate: 60_000,
                hdr_capable: false,
                hdr_metadata: None,
                identity: OutputIdentity::connector_only("Replacement"),
            },
        )
        .unwrap();

        let replacement = jwm.state.monitor_order[0];
        let monitor_num = jwm.state.monitors[replacement].num;
        let client = &jwm.state.clients[scratchpad_key];
        assert_eq!(client.mon, Some(replacement));
        assert_eq!(client.state.tags, 0);
        assert!(client.state.is_hidden);
        assert!(jwm.state.monitor_clients[replacement].contains(&scratchpad_key));

        let projection = crate::jwm::statusbar::StatusBarBuilder::get_minimized_windows(
            &jwm.state.clients,
            &jwm.state.monitor_clients[replacement],
            monitor_num,
        );
        assert!(projection.iter().any(|item| item.window_id == window.raw()));

        let snapshot = backend
            .property_ops
            .minimized_restores
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap()
            .1;
        assert_eq!(snapshot.tags, 0);
        assert_eq!(snapshot.monitor_num, monitor_num);
    }

    #[test]
    fn hidden_property_driven_floating_changes_refresh_the_restart_snapshot() {
        let mut backend = DockSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];

        let parent_window = WindowId::from_raw(0x901);
        let mut parent = WMClient::new(parent_window);
        parent.mon = Some(monitor);
        parent.state.tags = 1;
        let parent_key = jwm.insert_client(parent);
        jwm.attach_to_monitor(parent_key, monitor);

        let window = WindowId::from_raw(0x902);
        let visible = Rect::new(140, 90, 640, 420);
        let mut client = WMClient::new(window);
        client.mon = Some(monitor);
        client.state.tags = 1;
        client.state.is_hidden = true;
        client.state.minimized_order = 203;
        client.geometry.hidden_restore_rect = Some(visible);
        client.geometry.x = -3000;
        client.geometry.y = visible.y;
        client.geometry.w = visible.w;
        client.geometry.h = visible.h;
        let client_key = jwm.insert_client(client);
        jwm.attach_to_monitor(client_key, monitor);

        *backend.property_ops.transient_parent.lock().unwrap() = Some(parent_window);
        jwm.handle_transient_for_change(&mut backend, client_key)
            .unwrap();

        assert!(jwm.state.clients[client_key].state.is_floating);
        let transient_snapshot = backend
            .property_ops
            .minimized_restores
            .lock()
            .unwrap()
            .last()
            .copied()
            .expect("hidden transient change must refresh V1")
            .1;
        assert!(transient_snapshot.is_floating);
        assert_eq!(transient_snapshot.minimized_order, 203);

        // Window-type changes use a separate PropertyNotify path. Exercise a
        // floating transition that leaves Dock eligibility unchanged; the V1
        // refresh must not depend on the visual eligibility reconciler doing
        // work.
        jwm.state.clients[client_key].state.is_floating = false;
        *backend.property_ops.transient_parent.lock().unwrap() = None;
        *backend.property_ops.window_types.lock().unwrap() = vec![WindowType::Dialog];
        backend
            .property_ops
            .minimized_restores
            .lock()
            .unwrap()
            .clear();

        jwm.handle_window_type_change(&mut backend, client_key)
            .unwrap();

        assert!(jwm.state.clients[client_key].state.is_floating);
        let type_snapshot = backend
            .property_ops
            .minimized_restores
            .lock()
            .unwrap()
            .last()
            .copied()
            .expect("hidden window-type change must refresh V1")
            .1;
        assert!(type_snapshot.is_floating);
        assert_eq!(type_snapshot.minimized_order, 203);
    }

    #[test]
    fn dynamic_normal_to_desktop_transition_stays_structurally_borderless() {
        let mut backend = DockSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let window = WindowId::from_raw(0x903);
        let mut client = WMClient::new(window);
        client.mon = Some(monitor);
        client.state.tags = jwm.state.monitors[monitor].get_active_tags();
        let client_key = jwm.insert_client(client);
        jwm.attach_to_monitor(client_key, monitor);
        let mut peer = WMClient::new(WindowId::from_raw(0x904));
        peer.mon = Some(monitor);
        peer.state.tags = jwm.state.monitors[monitor].get_active_tags();
        let peer_key = jwm.insert_client(peer);
        jwm.attach_to_monitor(peer_key, monitor);

        *backend.property_ops.window_types.lock().unwrap() = vec![WindowType::Normal];
        jwm.handle_window_type_change(&mut backend, client_key)
            .unwrap();
        assert_eq!(
            jwm.state.clients[client_key].geometry.border_w,
            crate::config::CONFIG.load().border_px() as i32,
            "the normal state establishes the ordinary server border"
        );

        *backend.property_ops.window_types.lock().unwrap() = vec![WindowType::Desktop];
        jwm.handle_window_type_change(&mut backend, client_key)
            .unwrap();

        let client = &jwm.state.clients[client_key];
        assert!(client.state.is_floating);
        assert!(client.state.never_focus);
        assert_eq!(client.geometry.border_w, 0);
    }

    // ---- maximize: geometry sites (sendmon, output removal, refit, drag,
    // snap, drop, togglefloating, scratchpad, rollback) -------------------

    /// A floating window at `rect` whose floating slot is that same rect.
    fn floating_client(jwm: &mut Jwm, mon: MonitorKey, raw: u64, rect: Rect) -> ClientKey {
        let key = visible_client(jwm, mon, raw, rect);
        let client = &mut jwm.state.clients[key];
        client.state.is_floating = true;
        client.geometry.floating_x = rect.x;
        client.geometry.floating_y = rect.y;
        client.geometry.floating_w = rect.w;
        client.geometry.floating_h = rect.h;
        key
    }

    fn live_rect(client: &WMClient) -> Rect {
        Rect::new(
            client.geometry.x,
            client.geometry.y,
            client.geometry.w,
            client.geometry.h,
        )
    }

    fn floating_rect(client: &WMClient) -> Rect {
        Rect::new(
            client.geometry.floating_x,
            client.geometry.floating_y,
            client.geometry.floating_w,
            client.geometry.floating_h,
        )
    }

    /// Every configuration recorded for `win`, as content rects.
    fn configurations_of(backend: &DockSpyBackend, win: WindowId) -> Vec<Rect> {
        backend
            .window_ops
            .configurations
            .lock()
            .unwrap()
            .iter()
            .filter(|(configured, ..)| *configured == win)
            .map(|&(_, x, y, w, h, _)| Rect::new(x, y, w as i32, h as i32))
            .collect()
    }

    /// Every maximize publish recorded for `win`, in order.
    fn maximize_writes_of(backend: &DockSpyBackend, win: WindowId) -> Vec<MaximizeAxes> {
        backend
            .property_ops
            .maximized
            .lock()
            .unwrap()
            .iter()
            .filter(|(written, _)| *written == win)
            .map(|&(_, axes)| axes)
            .collect()
    }

    /// Reserve `pixels` more at the top of the monitor's work area, the way a
    /// strut or a docked panel would.
    fn shrink_work_area_top(jwm: &mut Jwm, mon: MonitorKey, pixels: i32) {
        let geometry = &mut jwm.state.monitors[mon].geometry;
        geometry.w_y += pixels;
        geometry.w_h -= pixels;
    }

    fn contains(outer: Rect, inner: Rect) -> bool {
        inner.x >= outer.x
            && inner.y >= outer.y
            && inner.x + inner.w <= outer.x + outer.w
            && inner.y + inner.h <= outer.y + outer.h
    }

    #[test]
    fn maximized_sendmon_fills_the_target_work_area_and_carries_the_restore_rect() {
        let mut backend = DockSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let source = jwm.state.monitor_order[0];
        let target = add_right_monitor(&mut jwm, source);
        let (_, source_work) = jwm.monitor_migration_areas(source).unwrap();
        let (_, target_work) = jwm.monitor_migration_areas(target).unwrap();

        let restore = Rect::new(source_work.x + 60, source_work.y + 40, 400, 250);
        let key = floating_client(&mut jwm, source, 0x410, restore);
        let win = WindowId::from_raw(0x410);
        jwm.state.clients[key].geometry.border_w = 2;
        assert!(
            jwm.set_client_maximized(
                &mut backend,
                key,
                MaximizeAxes::BOTH,
                MaximizeOrigin::Client
            )
            .unwrap()
        );

        jwm.sendmon(&mut backend, Some(key), Some(target));

        let translated = Rect::new(target_work.x + 60, target_work.y + 40, 400, 250);
        let client = &jwm.state.clients[key];
        assert_eq!(client.mon, Some(target));
        assert_eq!(client.state.maximized_axes(), MaximizeAxes::BOTH);
        assert_eq!(
            live_rect(client),
            maximize_target(translated, target_work, MaximizeAxes::BOTH, 2),
            "a maximized window fills the work area of the output it moved to"
        );
        assert_eq!(client.geometry.maximize_restore_rect, Some(translated));
        assert_eq!(floating_rect(client), translated);
        assert_eq!(
            configurations_of(&backend, win).last().copied(),
            Some(live_rect(client)),
            "the real window was told about the target rect"
        );

        jwm.set_client_maximized(
            &mut backend,
            key,
            MaximizeAxes::NONE,
            MaximizeOrigin::Client,
        )
        .unwrap();
        let client = &jwm.state.clients[key];
        assert_eq!(client.state.maximized_axes(), MaximizeAxes::NONE);
        assert_eq!(
            live_rect(client),
            translated,
            "unmaximizing lands on the translated pre-maximize rect"
        );
    }

    #[test]
    fn output_removal_refits_a_maximized_window_to_the_surviving_monitor() {
        let mut backend = DockSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let primary = jwm.state.monitor_order[0];
        let right = add_right_monitor(&mut jwm, primary);
        let (_, right_work) = jwm.monitor_migration_areas(right).unwrap();

        let restore = Rect::new(right_work.x + 50, right_work.y + 60, 300, 200);
        let key = floating_client(&mut jwm, right, 0x411, restore);
        jwm.set_client_maximized(
            &mut backend,
            key,
            MaximizeAxes::BOTH,
            MaximizeOrigin::Client,
        )
        .unwrap();

        jwm.handle_output_removed(&mut backend, OutputId(2))
            .unwrap();

        let (_, primary_work) = jwm.monitor_migration_areas(primary).unwrap();
        let client = &jwm.state.clients[key];
        assert_eq!(client.mon, Some(primary));
        assert_eq!(client.state.maximized_axes(), MaximizeAxes::BOTH);
        assert_eq!(
            live_rect(client),
            maximize_target(
                client.geometry.maximize_restore_rect.unwrap(),
                primary_work,
                MaximizeAxes::BOTH,
                client.geometry.border_w,
            ),
            "the survivor's work area is filled"
        );
        let moved_restore = client
            .geometry
            .maximize_restore_rect
            .expect("still maximized");
        assert!(
            contains(primary_work, moved_restore),
            "unmaximizing later lands on the surviving output: {moved_restore:?}"
        );
    }

    #[test]
    fn work_area_change_refits_maximized_windows_on_arrange_without_touching_the_restore_rect() {
        let mut backend = DockSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let mon = jwm.state.monitor_order[0];
        let (_, work) = jwm.monitor_migration_areas(mon).unwrap();

        let restore = Rect::new(work.x + 100, work.y + 80, 500, 300);
        let maximized = floating_client(&mut jwm, mon, 0x412, restore);
        let maximized_win = WindowId::from_raw(0x412);
        jwm.set_client_maximized(
            &mut backend,
            maximized,
            MaximizeAxes::BOTH,
            MaximizeOrigin::Client,
        )
        .unwrap();
        let plain_rect = Rect::new(work.x + 700, work.y + 200, 400, 300);
        let plain = floating_client(&mut jwm, mon, 0x413, plain_rect);

        shrink_work_area_top(&mut jwm, mon, 40);
        jwm.arrange(&mut backend, Some(mon));

        let new_work = jwm.maximize_work_area(mon).unwrap();
        assert_ne!(new_work, work, "the strut moved the work area");
        let target = maximize_target(restore, new_work, MaximizeAxes::BOTH, 0);
        let client = &jwm.state.clients[maximized];
        assert_eq!(live_rect(client), target);
        assert_eq!(client.geometry.maximize_restore_rect, Some(restore));
        assert_eq!(floating_rect(client), restore);
        let other = &jwm.state.clients[plain];
        assert_eq!(live_rect(other), plain_rect);
        assert_eq!(floating_rect(other), plain_rect);

        // Idempotent: the refit itself writes nothing for a window already at
        // its target, and a further arrange never moves it (the only
        // configures left are the in-place ones every visible float gets
        // from the show pass).
        backend.window_ops.configurations.lock().unwrap().clear();
        jwm.refit_maximized_clients(&mut backend, mon);
        assert!(configurations_of(&backend, maximized_win).is_empty());
        jwm.arrange(&mut backend, Some(mon));
        assert!(
            configurations_of(&backend, maximized_win)
                .iter()
                .all(|&rect| rect == target),
            "a second arrange does not move the maximized window"
        );
        assert_eq!(
            jwm.state.clients[maximized].geometry.maximize_restore_rect,
            Some(restore)
        );
    }

    #[test]
    fn refit_is_idempotent_and_skips_hidden_fullscreen_and_pip_clients() {
        let mut backend = DockSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let mon = jwm.state.monitor_order[0];
        let (_, work) = jwm.monitor_migration_areas(mon).unwrap();

        let maximize = |jwm: &mut Jwm, backend: &mut DockSpyBackend, raw: u64| {
            let rect = Rect::new(work.x + 100, work.y + 80, 500, 300);
            let key = floating_client(jwm, mon, raw, rect);
            jwm.set_client_maximized(backend, key, MaximizeAxes::BOTH, MaximizeOrigin::User)
                .unwrap();
            key
        };
        let hidden = maximize(&mut jwm, &mut backend, 0x414);
        {
            let client = &mut jwm.state.clients[hidden];
            let visible = live_rect(client);
            client.state.is_hidden = true;
            client.state.minimized_order = 3;
            client.geometry.hidden_restore_rect = Some(visible);
            client.geometry.hidden_x = Some(-5000);
            client.geometry.x = -5000;
        }
        let fullscreen = maximize(&mut jwm, &mut backend, 0x415);
        jwm.setfullscreen(&mut backend, fullscreen, true).unwrap();
        let pip = maximize(&mut jwm, &mut backend, 0x416);
        assert!(jwm.set_client_pip(&mut backend, pip, true).unwrap());

        shrink_work_area_top(&mut jwm, mon, 40);
        let keys = [hidden, fullscreen, pip];
        let before: Vec<WMClient> = keys
            .iter()
            .map(|&key| jwm.state.clients[key].clone())
            .collect();
        backend.window_ops.configurations.lock().unwrap().clear();

        jwm.refit_maximized_clients(&mut backend, mon);
        for (key, before) in keys.iter().zip(&before) {
            assert_eq!(&jwm.state.clients[*key], before, "the refit skipped it");
            assert!(configurations_of(&backend, before.win).is_empty());
        }

        jwm.arrange(&mut backend, Some(mon));
        let new_work = jwm.maximize_work_area(mon).unwrap();
        for (key, before) in keys.iter().zip(&before) {
            let client = &jwm.state.clients[*key];
            assert_eq!(client.state.maximized_axes(), MaximizeAxes::BOTH);
            assert_eq!(
                client.geometry.maximize_restore_rect,
                before.geometry.maximize_restore_rect
            );
            assert_eq!(floating_rect(client), floating_rect(before));
            assert_eq!(
                client.geometry.hidden_restore_rect,
                before.geometry.hidden_restore_rect
            );
            assert_eq!(live_rect(client), live_rect(before));
            let refit_target = maximize_target(
                before.geometry.maximize_restore_rect.unwrap(),
                new_work,
                MaximizeAxes::BOTH,
                before.geometry.border_w,
            );
            assert!(
                !configurations_of(&backend, before.win).contains(&refit_target),
                "{:?} was refitted although maximize does not own it",
                before.win
            );
        }
        assert!(configurations_of(&backend, before[0].win).is_empty());
        assert!(configurations_of(&backend, before[1].win).is_empty());
    }

    #[test]
    fn pip_exit_on_a_maximized_window_returns_maximized_and_reestablishes_the_floating_slot() {
        let mut backend = DockSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let mon = jwm.state.monitor_order[0];
        let (_, work) = jwm.monitor_migration_areas(mon).unwrap();
        let restore = Rect::new(work.x + 120, work.y + 90, 640, 360);
        let key = floating_client(&mut jwm, mon, 0x417, restore);
        jwm.set_client_maximized(&mut backend, key, MaximizeAxes::BOTH, MaximizeOrigin::User)
            .unwrap();
        let target = live_rect(&jwm.state.clients[key]);

        assert!(jwm.set_client_pip(&mut backend, key, true).unwrap());
        assert!(jwm.set_client_pip(&mut backend, key, false).unwrap());

        let client = &jwm.state.clients[key];
        assert!(!client.state.is_pip);
        assert_eq!(client.state.maximized_axes(), MaximizeAxes::BOTH);
        assert_eq!(live_rect(client), target);
        assert_eq!(client.geometry.maximize_restore_rect, Some(restore));
        assert_eq!(
            floating_rect(client),
            restore,
            "the floating slot is the pre-maximize rect again, not the maximized one"
        );
    }

    #[test]
    fn fullscreen_exit_after_a_work_area_change_refits_the_maximized_rect() {
        let mut backend = DockSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let mon = jwm.state.monitor_order[0];
        let (_, work) = jwm.monitor_migration_areas(mon).unwrap();
        let restore = Rect::new(work.x + 120, work.y + 90, 640, 360);
        let key = floating_client(&mut jwm, mon, 0x418, restore);
        jwm.state.clients[key].geometry.border_w = 2;
        jwm.set_client_maximized(&mut backend, key, MaximizeAxes::BOTH, MaximizeOrigin::User)
            .unwrap();

        jwm.setfullscreen(&mut backend, key, true).unwrap();
        jwm.state.monitors[mon].geometry.w_h -= 40;
        jwm.setfullscreen(&mut backend, key, false).unwrap();

        let new_work = jwm.maximize_work_area(mon).unwrap();
        let client = &jwm.state.clients[key];
        assert!(!client.state.is_fullscreen);
        assert_eq!(client.state.maximized_axes(), MaximizeAxes::BOTH);
        assert_eq!(
            live_rect(client),
            maximize_target(restore, new_work, MaximizeAxes::BOTH, 2),
            "leaving fullscreen lands on the maximized rect of today's work area"
        );
        assert_eq!(client.geometry.maximize_restore_rect, Some(restore));
    }

    #[test]
    fn togglefloating_unmaximizes_before_tiling_and_keeps_the_restore_as_floating_rect() {
        use crate::jwm::WMArgEnum;

        let mut backend = DockSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let mon = jwm.state.monitor_order[0];
        let (_, work) = jwm.monitor_migration_areas(mon).unwrap();
        let restore = Rect::new(work.x + 100, work.y + 80, 500, 300);
        let key = floating_client(&mut jwm, mon, 0x419, restore);
        let win = WindowId::from_raw(0x419);
        jwm.focus(&mut backend, Some(key)).unwrap();
        jwm.set_client_maximized(&mut backend, key, MaximizeAxes::BOTH, MaximizeOrigin::User)
            .unwrap();

        jwm.togglefloating(&mut backend, &WMArgEnum::Int(0))
            .unwrap();

        let client = &jwm.state.clients[key];
        assert_eq!(client.state.maximized_axes(), MaximizeAxes::NONE);
        assert_eq!(client.geometry.maximize_restore_rect, None);
        assert!(!client.state.is_floating);
        assert_eq!(
            floating_rect(client),
            restore,
            "the pre-maximize rect, not the maximized one, is remembered"
        );
        assert_eq!(
            maximize_writes_of(&backend, win).last(),
            Some(&MaximizeAxes::NONE)
        );

        jwm.togglefloating(&mut backend, &WMArgEnum::Int(0))
            .unwrap();
        let client = &jwm.state.clients[key];
        assert!(client.state.is_floating);
        assert_eq!(client.state.maximized_axes(), MaximizeAxes::NONE);
        assert_eq!(live_rect(client), restore);
    }

    #[test]
    fn togglefloating_on_a_promoted_window_returns_it_to_the_tiling() {
        use crate::jwm::WMArgEnum;

        let mut backend = DockSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let mon = jwm.state.monitor_order[0];
        let (_, work) = jwm.monitor_migration_areas(mon).unwrap();
        let key = visible_client(&mut jwm, mon, 0x41a, Rect::new(work.x, work.y, 300, 200));
        jwm.focus(&mut backend, Some(key)).unwrap();

        jwm.togglemaximize(&mut backend, &WMArgEnum::Int(0))
            .unwrap();
        {
            let client = &jwm.state.clients[key];
            assert_eq!(client.state.maximized_axes(), MaximizeAxes::BOTH);
            assert!(client.state.is_floating);
            assert!(client.state.maximize_restore_tiled);
        }

        jwm.togglefloating(&mut backend, &WMArgEnum::Int(0))
            .unwrap();

        let client = &jwm.state.clients[key];
        assert_eq!(client.state.maximized_axes(), MaximizeAxes::NONE);
        assert!(!client.state.is_floating, "one call re-tiles it");
        assert!(!client.state.maximize_restore_tiled);
    }

    #[test]
    fn snap_window_maximize_toggles_a_real_maximize_on_the_work_area() {
        use crate::jwm::WMArgEnum;

        let mut backend = DockSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let mon = jwm.state.monitor_order[0];
        // A reserved top strip, so the work area and the monitor differ.
        shrink_work_area_top(&mut jwm, mon, 30);
        let work = jwm.maximize_work_area(mon).unwrap();
        let monitor_y = jwm.state.monitors[mon].geometry.m_y;
        assert_ne!(work.y, monitor_y);

        let previous = Rect::new(work.x + 100, work.y + 80, 500, 300);
        let key = floating_client(&mut jwm, mon, 0x41b, previous);
        let win = WindowId::from_raw(0x41b);
        jwm.focus(&mut backend, Some(key)).unwrap();
        let maximize = WMArgEnum::StringVec(vec!["maximize".into()]);

        jwm.snap_window(&mut backend, &maximize).unwrap();
        let client = &jwm.state.clients[key];
        assert_eq!(client.state.maximized_axes(), MaximizeAxes::BOTH);
        assert_eq!(client.geometry.y, work.y, "the bar strip stays uncovered");
        assert_eq!(client.geometry.maximize_restore_rect, Some(previous));
        assert_eq!(maximize_writes_of(&backend, win), vec![MaximizeAxes::BOTH]);

        jwm.snap_window(&mut backend, &maximize).unwrap();
        let client = &jwm.state.clients[key];
        assert_eq!(client.state.maximized_axes(), MaximizeAxes::NONE);
        assert_eq!(client.geometry.maximize_restore_rect, None);
        assert_eq!(live_rect(client), previous);

        // Snapping stays a floating-geometry operation.
        let tiled = visible_client(&mut jwm, mon, 0x41c, Rect::new(work.x, work.y, 300, 200));
        jwm.focus(&mut backend, Some(tiled)).unwrap();
        jwm.snap_window(&mut backend, &maximize).unwrap();
        let client = &jwm.state.clients[tiled];
        assert_eq!(client.state.maximized_axes(), MaximizeAxes::NONE);
        assert!(!client.state.is_floating);
        assert!(maximize_writes_of(&backend, WindowId::from_raw(0x41c)).is_empty());
    }

    #[test]
    fn snapping_a_maximized_window_to_a_half_drops_maximize_in_place() {
        use crate::jwm::WMArgEnum;

        let mut backend = DockSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let mon = jwm.state.monitor_order[0];
        let (_, work) = jwm.monitor_migration_areas(mon).unwrap();
        let key = floating_client(
            &mut jwm,
            mon,
            0x41d,
            Rect::new(work.x + 100, work.y + 80, 500, 300),
        );
        let win = WindowId::from_raw(0x41d);
        let bw = 2;
        jwm.state.clients[key].geometry.border_w = bw;
        jwm.focus(&mut backend, Some(key)).unwrap();
        jwm.set_client_maximized(&mut backend, key, MaximizeAxes::BOTH, MaximizeOrigin::User)
            .unwrap();

        jwm.snap_window(&mut backend, &WMArgEnum::StringVec(vec!["left".into()]))
            .unwrap();

        // The classic left half of the monitor (snap_rect's Left).
        let (mx, my, mw, mh) = jwm.monitor_rect(mon);
        let half = Rect::new(mx, my, mw as i32 / 2, mh as i32);
        let client = &jwm.state.clients[key];
        assert_eq!(client.state.maximized_axes(), MaximizeAxes::NONE);
        assert_eq!(client.geometry.maximize_restore_rect, None);
        assert_eq!(
            maximize_writes_of(&backend, win).last(),
            Some(&MaximizeAxes::NONE)
        );
        assert_eq!(
            live_rect(client),
            Rect::new(half.x + bw, half.y + bw, half.w - 2 * bw, half.h - 2 * bw)
        );
        assert_eq!(floating_rect(client), live_rect(client));
    }

    #[test]
    fn top_edge_drop_plans_and_applies_a_work_area_maximize() {
        use crate::core::layout::LayoutEnum;
        use crate::jwm::WMArgEnum;
        use std::rc::Rc;

        let mut backend = DockSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let mon = jwm.state.monitor_order[0];
        jwm.setlayout(&mut backend, &WMArgEnum::Layout(Rc::new(LayoutEnum::FLOAT)))
            .unwrap();
        let (_, work) = jwm.monitor_migration_areas(mon).unwrap();
        let before_drop = Rect::new(work.x + 200, work.y + 150, 600, 400);
        let key = floating_client(&mut jwm, mon, 0x41e, before_drop);
        jwm.focus(&mut backend, Some(key)).unwrap();

        let (mx, my, mw, _) = jwm.monitor_rect(mon);
        let plan = jwm
            .plan_drag_snap(mon, mx + mw as i32 / 2, my + 1)
            .expect("the top edge is a drop zone");
        let work_area = jwm.monitor_work_area(mon).unwrap();
        assert_eq!(plan.maximize_monitor(), Some(mon));
        assert_eq!(plan.preview_rect(), work_area);

        jwm.apply_drag_snap(&mut backend, key, plan);

        let client = &jwm.state.clients[key];
        assert_eq!(client.state.maximized_axes(), MaximizeAxes::BOTH);
        assert_eq!(client.geometry.maximize_restore_rect, Some(before_drop));
        assert_eq!(
            live_rect(client),
            maximize_target(before_drop, work_area, MaximizeAxes::BOTH, 0)
        );
    }

    #[test]
    fn applying_a_layout_does_not_reclaim_a_maximized_drag_float() {
        use crate::core::layout::LayoutEnum;
        use crate::jwm::WMArgEnum;
        use std::rc::Rc;

        let mut backend = DockSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let mon = jwm.state.monitor_order[0];
        // Start on the float layout, which never reclaims, so each tiling
        // layout applied below is a real change (re-applying the current
        // layout is a no-op).
        jwm.setlayout(&mut backend, &WMArgEnum::Layout(Rc::new(LayoutEnum::FLOAT)))
            .unwrap();
        let (_, work) = jwm.monitor_migration_areas(mon).unwrap();
        let key = floating_client(
            &mut jwm,
            mon,
            0x41f,
            Rect::new(work.x + 100, work.y + 80, 500, 300),
        );
        jwm.state.clients[key].state.is_drag_floating = true;
        jwm.set_client_maximized(&mut backend, key, MaximizeAxes::BOTH, MaximizeOrigin::User)
            .unwrap();

        jwm.setlayout(&mut backend, &WMArgEnum::Layout(Rc::new(LayoutEnum::TILE)))
            .unwrap();
        let client = &jwm.state.clients[key];
        assert!(client.state.is_floating, "maximize owns it, not the layout");
        assert_eq!(client.state.maximized_axes(), MaximizeAxes::BOTH);

        jwm.set_client_maximized(&mut backend, key, MaximizeAxes::NONE, MaximizeOrigin::User)
            .unwrap();
        assert!(jwm.state.clients[key].state.is_drag_floating);
        jwm.setlayout(
            &mut backend,
            &WMArgEnum::Layout(Rc::new(LayoutEnum::MONOCLE)),
        )
        .unwrap();
        assert!(
            !jwm.state.clients[key].state.is_floating,
            "an unmaximized drag float is reclaimed as before"
        );
    }

    #[test]
    fn drag_activation_unmaximizes_in_place_and_cancel_reinstates() {
        use crate::jwm::mouse_handler::{DragCtl, DragMode};

        let mut backend = DockSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let mon = jwm.state.monitor_order[0];
        let (_, work) = jwm.monitor_migration_areas(mon).unwrap();
        let restore = Rect::new(work.x + 100, work.y + 80, 500, 300);
        let key = floating_client(&mut jwm, mon, 0x420, restore);
        let win = WindowId::from_raw(0x420);
        jwm.focus(&mut backend, Some(key)).unwrap();
        jwm.set_client_maximized(&mut backend, key, MaximizeAxes::BOTH, MaximizeOrigin::User)
            .unwrap();
        let target = live_rect(&jwm.state.clients[key]);

        jwm.drag_ctl = Some(DragCtl {
            client: key,
            win,
            mode: DragMode::MoveFloat,
            start_root: (0.0, 0.0),
            activated: false,
            was_floating: true,
            orig_geom: (target.x, target.y, target.w, target.h),
            orig_index: None,
            mon: Some(mon),
            orig_maximize: jwm.maximize_snapshot(key),
        });

        jwm.activate_pointer_drag(&mut backend).unwrap();
        {
            let client = &jwm.state.clients[key];
            assert_eq!(client.state.maximized_axes(), MaximizeAxes::NONE);
            assert_eq!(client.geometry.maximize_restore_rect, None);
            assert_eq!(live_rect(client), target, "unmaximized where it stands");
            assert_eq!(
                maximize_writes_of(&backend, win).last(),
                Some(&MaximizeAxes::NONE)
            );
        }

        jwm.cancel_pointer_drag(&mut backend);

        let client = &jwm.state.clients[key];
        assert_eq!(client.state.maximized_axes(), MaximizeAxes::BOTH);
        assert_eq!(client.geometry.maximize_restore_rect, Some(restore));
        assert_eq!(live_rect(client), target);
        assert_eq!(floating_rect(client), restore);
        assert_eq!(
            maximize_writes_of(&backend, win).last(),
            Some(&MaximizeAxes::BOTH)
        );
    }

    #[test]
    fn scratchpad_reveal_of_a_maximized_window_unmaximizes_first() {
        let mut backend = DockSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let mon = jwm.state.monitor_order[0];
        let (_, work) = jwm.monitor_migration_areas(mon).unwrap();
        let restore = Rect::new(work.x + 150, work.y + 100, 600, 400);
        let maximized = maximize_target(restore, work, MaximizeAxes::BOTH, 0);

        let window = WindowId::from_raw(0x421);
        let mut scratchpad = WMClient::new(window);
        scratchpad.name = "scratch-term".into();
        scratchpad.mon = Some(mon);
        scratchpad.state.tags = 1;
        scratchpad.state.is_floating = true;
        scratchpad.state.is_hidden = true;
        scratchpad.state.minimized_order = 5;
        scratchpad.state.set_maximized_axes(MaximizeAxes::BOTH);
        scratchpad.geometry.maximize_restore_rect = Some(restore);
        scratchpad.geometry.floating_x = restore.x;
        scratchpad.geometry.floating_y = restore.y;
        scratchpad.geometry.floating_w = restore.w;
        scratchpad.geometry.floating_h = restore.h;
        scratchpad.geometry.hidden_restore_rect = Some(maximized);
        scratchpad.geometry.hidden_x = Some(-5000);
        scratchpad.geometry.x = -5000;
        scratchpad.geometry.y = maximized.y;
        scratchpad.geometry.w = maximized.w;
        scratchpad.geometry.h = maximized.h;
        let key = jwm.insert_client(scratchpad);
        jwm.attach_to_monitor(key, mon);
        jwm.scratchpads.insert("scratch-term".into(), key);

        assert!(jwm.reveal_and_focus(&mut backend, window).unwrap());

        let area = jwm.monitor_work_area(mon).unwrap();
        let width = area.w * 4 / 5;
        let height = area.h * 4 / 5;
        let placement = Rect::new(
            area.x + (area.w - width) / 2,
            area.y + (area.h - height) / 2,
            width,
            height,
        );
        let client = &jwm.state.clients[key];
        assert!(!client.state.is_hidden);
        assert_eq!(client.state.maximized_axes(), MaximizeAxes::NONE);
        assert_eq!(client.geometry.maximize_restore_rect, None);
        assert_eq!(
            maximize_writes_of(&backend, window).last(),
            Some(&MaximizeAxes::NONE)
        );
        assert_eq!(live_rect(client), placement);
        let desktop_left = jwm.desktop_left_edge();
        assert!(
            configurations_of(&backend, window)
                .iter()
                .all(|&rect| rect == placement || rect.x + rect.w <= desktop_left),
            "the unmaximize never put the parked window on screen"
        );
    }

    #[test]
    fn failed_maximize_configure_rolls_back_state_atoms_and_geometry() {
        let mut backend = DockSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let mon = jwm.state.monitor_order[0];
        let (_, work) = jwm.monitor_migration_areas(mon).unwrap();
        let previous = Rect::new(work.x + 100, work.y + 80, 500, 300);
        let key = floating_client(&mut jwm, mon, 0x422, previous);
        let win = WindowId::from_raw(0x422);
        let before = jwm.state.clients[key].clone();

        backend
            .window_ops
            .fail_next_configure
            .store(true, Ordering::SeqCst);
        assert!(
            jwm.set_client_maximized(&mut backend, key, MaximizeAxes::BOTH, MaximizeOrigin::User)
                .is_err()
        );

        assert_eq!(
            maximize_writes_of(&backend, win),
            vec![MaximizeAxes::BOTH, MaximizeAxes::NONE],
            "the published state is repaired"
        );
        assert_eq!(jwm.state.clients[key], before);
        assert_eq!(
            configurations_of(&backend, win).last().copied(),
            Some(previous)
        );

        // A publish that fails never lands, and the transaction still ends
        // on the previous state and geometry.
        backend.property_ops.maximized.lock().unwrap().clear();
        backend
            .property_ops
            .fail_next_maximized_write
            .store(true, Ordering::SeqCst);
        assert!(
            jwm.set_client_maximized(&mut backend, key, MaximizeAxes::BOTH, MaximizeOrigin::User)
                .is_err()
        );
        assert!(!maximize_writes_of(&backend, win).contains(&MaximizeAxes::BOTH));
        assert_eq!(jwm.state.clients[key], before);
        assert_eq!(
            configurations_of(&backend, win).last().copied(),
            Some(previous)
        );
    }
}
