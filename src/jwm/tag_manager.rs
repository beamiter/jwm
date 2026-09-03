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
        EventHandler, InputOps, KeyOps, MinimizedRestoreState, OutputIdentity, OutputInfo,
        OutputOps, PropertyOps, RenderScheduler, WindowOps, WindowType,
    };
    use crate::backend::common_define::{OutputId, WindowId};
    use crate::backend::error::BackendError;
    use crate::backend::wayland_dummy_ops::{
        DummyColorAllocator, DummyCursorProvider, DummyInputOps, DummyKeyOps, DummyOutputOps,
        DummyPropertyOps, DummyWindowOps,
    };
    use crate::core::models::WMClient;
    use crate::core::types::Rect;
    use std::any::Any;
    use std::sync::Mutex;

    #[derive(Default)]
    struct DockSpyWindowOps {
        positions: Mutex<Vec<(WindowId, i32, i32)>>,
        configurations: Mutex<Vec<(WindowId, i32, i32, u32, u32, u32)>>,
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
}
