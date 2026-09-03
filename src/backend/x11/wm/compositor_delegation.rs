//! Shared delegation of the compositor capability traits.
//!
//! Both X11 transports own an optional shared `Compositor<C>` plus a window-id
//! registry and forward the `backend::api` compositor capability traits into
//! them method by method. The forwarding rules — no-op without a compositor,
//! id translation at the boundary, and the minimize/restore re-registration
//! sequence — are transport-free policy, so they are generated once here
//! instead of being maintained as parallel impl blocks in each backend.

use crate::backend::api::CompositorRect;
use crate::backend::common_define::WindowId;
use std::collections::{HashMap, HashSet};

/// Durable WM-owned state that must survive an X11 compositor being disabled
/// or recreated.
///
/// The compositor owns textures and animations, but it is not the authority
/// for whether a client is minimized or where its current Dock slot is.  Both
/// X11 transports retain that small command snapshot even while their
/// compositor is absent and replay it before importing the live X pixmaps.
#[derive(Debug, Default)]
pub(crate) struct X11CompositorDesiredState {
    minimized_windows: HashSet<WindowId>,
    dock_targets: HashMap<WindowId, CompositorRect>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum X11CompositorReplayStep {
    DockTarget(u32, CompositorRect),
    EnsureMinimized(u32),
}

/// A resolved, deterministic snapshot ready for one newly created
/// compositor. Targets deliberately precede minimized adoption: when the
/// subsequent existing-window registration imports a hidden pixmap, the
/// compositor can settle it straight into a static Dock visual instead of
/// playing Genie from JWM's off-screen parking coordinate.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct X11CompositorReplayPlan {
    dock_targets: Vec<(u32, CompositorRect)>,
    minimized_windows: Vec<u32>,
}

impl X11CompositorDesiredState {
    pub(crate) fn set_minimized(&mut self, window: WindowId, minimized: bool) {
        if minimized {
            self.minimized_windows.insert(window);
        } else {
            self.minimized_windows.remove(&window);
            self.dock_targets.remove(&window);
        }
    }

    pub(crate) fn ensure_minimized(&mut self, window: WindowId) {
        self.minimized_windows.insert(window);
    }

    pub(crate) fn set_dock_target(&mut self, window: WindowId, target: Option<CompositorRect>) {
        match target.and_then(CompositorRect::normalized) {
            Some(target) => {
                self.dock_targets.insert(window, target);
            }
            None => {
                // A bar can temporarily omit an overflowed item or disappear.
                // That withdraws only the addressable geometry; it must not
                // turn the authoritative minimized lifecycle into a restore.
                self.dock_targets.remove(&window);
            }
        }
    }

    pub(crate) fn retire_window(&mut self, window: WindowId) {
        self.minimized_windows.remove(&window);
        self.dock_targets.remove(&window);
    }

    /// Resolve internal ids at replay time and prune entries whose registry
    /// identity is already gone. This prevents a destroyed client's state
    /// from being applied to an XID that the server has subsequently reused.
    pub(crate) fn replay_plan(
        &mut self,
        mut resolve: impl FnMut(WindowId) -> Option<u32>,
    ) -> X11CompositorReplayPlan {
        let mut windows = self
            .minimized_windows
            .iter()
            .copied()
            .chain(self.dock_targets.keys().copied())
            .collect::<Vec<_>>();
        windows.sort_unstable_by_key(|window| window.raw());
        windows.dedup_by_key(|window| window.raw());

        let mut plan = X11CompositorReplayPlan::default();
        let mut stale = Vec::new();
        for window in windows {
            let Some(x11_window) = resolve(window) else {
                stale.push(window);
                continue;
            };
            if let Some(target) = self.dock_targets.get(&window).copied() {
                plan.dock_targets.push((x11_window, target));
            }
            if self.minimized_windows.contains(&window) {
                plan.minimized_windows.push(x11_window);
            }
        }
        for window in stale {
            self.retire_window(window);
        }
        plan
    }
}

impl X11CompositorReplayPlan {
    fn steps(&self) -> impl Iterator<Item = X11CompositorReplayStep> + '_ {
        self.dock_targets
            .iter()
            .map(|&(window, target)| X11CompositorReplayStep::DockTarget(window, target))
            .chain(
                self.minimized_windows
                    .iter()
                    .copied()
                    .map(X11CompositorReplayStep::EnsureMinimized),
            )
    }

    /// Replay only durable state. The caller must register existing windows
    /// after this returns so a target-bearing minimized client arrives under
    /// `PendingMinimize` and converges to a static retained texture.
    pub(crate) fn apply<C>(&self, compositor: &mut crate::backend::x11::compositor::Compositor<C>)
    where
        C: crate::backend::x11::compositor::CompositorConnection,
    {
        for step in self.steps() {
            match step {
                X11CompositorReplayStep::DockTarget(window, target) => {
                    compositor.set_window_dock_geometry(window, Some(target));
                }
                X11CompositorReplayStep::EnsureMinimized(window) => {
                    compositor.ensure_minimized_window_visual(window);
                }
            }
        }
    }
}

/// Implements the compositor capability traits for an X11 transport backend.
///
/// The backend type must expose `compositor: Option<Compositor<_>>`, an `ids`
/// registry with `x11(WindowId) -> Result<u32, BackendError>`, `window_ops`
/// and `property_ops` trait objects, and a `benchmark_auto_exit` flag.
/// `intern_raw` names the registry method interning a raw `u32` window id —
/// the registries predate a shared trait and name it differently.
macro_rules! delegate_compositor_capabilities {
    ($backend:ty, intern_raw = $intern_raw:ident) => {
        impl $backend {
            /// Try one admission without spinning frames. `NoSnapshot` and a
            /// transient compositor generation mismatch both leave the
            /// request AwaitingAdmission; a later capture/render or explicit
            /// repeated request services it again.
            fn service_iconify_admission_for(
                &mut self,
                window: crate::backend::common_define::WindowId,
            ) -> Result<(), crate::backend::error::BackendError> {
                if self.iconify.phase(window)
                    != Some(
                        crate::backend::x11::wm::iconify::IconifyPhase::AwaitingAdmission,
                    )
                {
                    return Ok(());
                }
                let x11_window = match self.ids.x11(window) {
                    Ok(window) => window,
                    Err(error) => {
                        self.iconify.retire(window);
                        return Err(error);
                    }
                };
                let generation = {
                    let Some(compositor) = self.compositor.as_mut() else {
                        return Ok(());
                    };
                    match compositor.reserve_iconic_snapshot(x11_window) {
                        Ok(generation) => generation,
                        Err(_) => return Ok(()),
                    }
                };

                let unmap_result = self.window_ops.unmap_managed_window(
                    window,
                    crate::backend::api::ManagedUnmapReason::IconifyRetain { generation },
                );
                crate::backend::x11::wm::iconify::finish_checked_unmap(
                    &mut self.iconify,
                    window,
                    generation,
                    unmap_result,
                    || {
                    if let Some(compositor) = self.compositor.as_mut() {
                        if compositor
                            .release_iconic_snapshot_reservation(x11_window, generation)
                        {
                            // The coordinator remains AwaitingAdmission. Keep
                            // one demand armed in case another capture evicts
                            // these newly recapturable pixels before the next
                            // admission-service pass.
                            compositor.request_iconic_snapshot_recapture(x11_window);
                        }
                    }
                    },
                )?;
                Ok(())
            }

            /// Admission may become ready inside the render that re-redirects
            /// a fullscreen client or imports a late static capture.
            fn service_pending_iconify_admissions(&mut self) {
                for window in self.iconify.awaiting_windows() {
                    if let Err(error) = self.service_iconify_admission_for(window) {
                        // The compositor is already live when this batch runs
                        // (either immediately after construction or after a
                        // frame imported late pixels). One failed checked
                        // UnmapWindow must neither make set_compositor_enabled
                        // report a false transactional failure nor starve the
                        // remaining awaiting clients. The per-window request
                        // stays AwaitingAdmission and a later frame can retry.
                        log::warn!(
                            "could not service pending Iconic admission for {window:?}: {error}"
                        );
                    }
                }
            }

            /// Advance/retire coordinator state before the compositor bridge
            /// applies the corresponding visual operation.
            fn observe_iconify_event(&mut self, event: &crate::backend::api::BackendEvent) {
                match event {
                    crate::backend::api::BackendEvent::WindowManagerUnmapped {
                        window,
                        reason:
                            crate::backend::api::ManagedUnmapReason::IconifyRetain {
                                generation,
                            },
                    } => {
                        self.iconify.acknowledge(*window, *generation);
                    }
                    crate::backend::api::BackendEvent::WindowUnmapped {
                        window,
                        from_configure: false,
                    }
                    | crate::backend::api::BackendEvent::WindowDestroyed(window) => {
                        self.iconify.retire(*window);
                    }
                    _ => {}
                }
            }

            /// Runtime compositor teardown is transactional with respect to
            /// snapshot ownership: map every physically iconified client
            /// first, roll successful maps back to checked unmaps if a later
            /// map fails, and release pins only once all maps succeeded.
            fn prepare_iconify_compositor_disable(
                &mut self,
            ) -> Result<(), crate::backend::error::BackendError> {
                if self.compositor.is_none() {
                    return Ok(());
                }
                let candidates = self.iconify.remap_before_compositor_loss();
                let mut resolved = Vec::with_capacity(candidates.len());
                for (window, generation) in candidates {
                    resolved.push((window, self.ids.x11(window)?, generation));
                }

                let window_ops = &self.window_ops;
                let compositor = self.compositor.as_mut().expect("checked above");
                crate::backend::x11::wm::iconify::prepare_compositor_loss_transaction(
                    &mut self.iconify,
                    &resolved,
                    |window| window_ops.map_window(window),
                    |window| match window_ops.get_window_attributes(window) {
                        Ok(attributes) if attributes.map_state_viewable => {
                            crate::backend::x11::wm::iconify::ViewabilityVerification::ConfirmedViewable
                        }
                        Ok(_) => crate::backend::x11::wm::iconify::ViewabilityVerification::ConfirmedNotViewable(
                            crate::backend::error::BackendError::Message(format!(
                                "X11 window {window:?} was not viewable after checked MapWindow"
                            )),
                        ),
                        Err(error) => {
                            crate::backend::x11::wm::iconify::ViewabilityVerification::QueryError(
                                error,
                            )
                        }
                    },
                    |mapped_window, mapped_generation| {
                        if let Err(rollback_error) = window_ops.unmap_managed_window(
                            mapped_window,
                            crate::backend::api::ManagedUnmapReason::IconifyRetain {
                                generation: mapped_generation,
                            },
                        ) {
                            log::warn!(
                                "could not roll back Iconic remap for {mapped_window:?} after compositor-disable failure: {rollback_error}"
                            );
                        }
                    },
                    |x11_window, generation| {
                        if compositor.has_iconic_snapshot_reservation(x11_window, generation) {
                            let released = compositor
                                .release_iconic_snapshot_reservation(x11_window, generation);
                            debug_assert!(released);
                        }
                    },
                )
            }
        }

        impl crate::backend::api::CompositorBenchmark for $backend {
            fn compositor_benchmark_start(&mut self, frames: u32, warmup: u32) -> bool {
                if let Some(compositor) = self.compositor.as_mut() {
                    compositor.benchmark_start(frames, warmup)
                } else {
                    false
                }
            }

            fn compositor_benchmark_stop(&mut self) -> Option<String> {
                self.compositor
                    .as_mut()
                    .and_then(|compositor| compositor.benchmark_stop())
            }

            fn compositor_benchmark_report(&self) -> Option<String> {
                self.compositor
                    .as_ref()
                    .and_then(|compositor| compositor.benchmark_report())
            }

            fn compositor_benchmark_is_complete(&self) -> bool {
                self.compositor
                    .as_ref()
                    .is_some_and(|compositor| compositor.benchmark_is_complete())
            }

            fn compositor_benchmark_set_auto_exit(&mut self, enabled: bool) {
                self.benchmark_auto_exit = enabled;
            }
        }

        impl crate::backend::api::BackendDiagnostics for $backend {
            fn compositor_fps(&self) -> f32 {
                self.compositor
                    .as_ref()
                    .map_or(0.0, |compositor| compositor.frame_stats_fps())
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
        }

        impl crate::backend::api::CompositorControl for $backend {
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

            fn compositor_toggle_waterlily_effect(&mut self) -> Option<bool> {
                self.compositor
                    .as_mut()
                    .map(|compositor| compositor.toggle_waterlily_effect())
            }

            fn compositor_set_waterlily_case(&mut self, case: &str) -> Option<bool> {
                self.compositor
                    .as_mut()
                    .map(|compositor| compositor.set_waterlily_case(case))
            }

            fn compositor_set_waterlily_palette(&mut self, palette: &str) -> Option<bool> {
                self.compositor
                    .as_mut()
                    .map(|compositor| compositor.set_waterlily_palette(palette))
            }

            fn compositor_waterlily_status(&self) -> Option<crate::backend::api::WaterlilyStatus> {
                self.compositor
                    .as_ref()
                    .map(|compositor| compositor.waterlily_status())
            }

            fn compositor_set_transition_mode(&mut self, mode: &str) {
                if let Some(compositor) = self.compositor.as_mut() {
                    compositor.set_transition_mode(mode);
                }
            }

            fn compositor_apply_config(&mut self) {
                if let Some(compositor) = self.compositor.as_mut() {
                    compositor.apply_config();
                }
            }
        }

        impl crate::backend::api::CompositorMedia for $backend {
            fn take_screenshot_to_file(
                &mut self,
                path: &std::path::Path,
            ) -> Result<bool, crate::backend::error::BackendError> {
                if let Some(compositor) = self.compositor.as_mut() {
                    compositor.request_screenshot(path.to_path_buf());
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
            ) -> Result<bool, crate::backend::error::BackendError> {
                if let Some(compositor) = self.compositor.as_mut() {
                    compositor.request_screenshot_region(path.to_path_buf(), x, y, width, height);
                    Ok(true)
                } else {
                    Ok(false)
                }
            }

            fn compositor_capture_thumbnail(
                &self,
                window: crate::backend::common_define::WindowId,
                max_size: u32,
            ) -> Option<(Vec<u8>, u32, u32)> {
                let x11_window = self.ids.x11(window).ok()?;
                self.compositor
                    .as_ref()?
                    .capture_window_thumbnail(x11_window, max_size)
            }

            fn compositor_notify_audio_timing(
                &mut self,
                window: crate::backend::common_define::WindowId,
                fps: f32,
                buffer_latency_ms: u32,
            ) {
                if let Some(compositor) = self.compositor.as_mut() {
                    if let Ok(x11_window) = self.ids.x11(window) {
                        compositor.notify_audio_timing(x11_window, fps, buffer_latency_ms);
                    }
                }
            }

            fn compositor_start_recording(&mut self, path: &str) {
                if let Some(compositor) = self.compositor.as_mut() {
                    compositor.start_recording(path);
                }
            }

            fn compositor_start_recording_region(
                &mut self,
                path: &str,
                region: (i32, i32, u32, u32),
            ) {
                if let Some(compositor) = self.compositor.as_mut() {
                    compositor.start_recording_region(path, region);
                }
            }

            fn compositor_set_recording_region(&mut self, region: (i32, i32, u32, u32)) {
                if let Some(compositor) = self.compositor.as_mut() {
                    compositor.set_recording_region(region);
                }
            }

            fn compositor_set_recording_region_overlay(
                &mut self,
                region: Option<(i32, i32, u32, u32)>,
            ) {
                if let Some(compositor) = self.compositor.as_mut() {
                    compositor.set_recording_region_overlay(region);
                }
            }

            fn compositor_stop_recording(&mut self) {
                if let Some(compositor) = self.compositor.as_mut() {
                    compositor.stop_recording();
                }
            }

            fn compositor_recording_stats(
                &self,
            ) -> Option<$crate::backend::api::RecordingStats> {
                self.compositor.as_ref()?.recording_stats()
            }

            fn compositor_request_live_thumbnail(
                &mut self,
                window: u32,
                max_size: u32,
            ) -> Option<(Vec<u8>, u32, u32)> {
                self.compositor
                    .as_ref()?
                    .request_live_thumbnail(window, max_size)
            }
        }

        impl crate::backend::api::CompositorWorkspaceEffects for $backend {
            fn compositor_set_system_ui(
                &mut self,
                overlay: Option<crate::backend::api::SystemUiOverlay>,
            ) {
                if let Some(compositor) = self.compositor.as_mut() {
                    compositor.set_system_ui(overlay);
                }
            }

            fn compositor_set_system_ui_hover(&mut self, row: Option<usize>) {
                if let Some(compositor) = self.compositor.as_mut() {
                    let _ = compositor.set_system_ui_hover(row);
                }
            }

            fn compositor_system_ui_hit_test(
                &self,
                x: f64,
                y: f64,
            ) -> crate::backend::api::SystemUiHitTarget {
                self.compositor.as_ref().map_or(
                    crate::backend::api::SystemUiHitTarget::Unavailable,
                    |compositor| compositor.system_ui_hit_test(x, y),
                )
            }

            fn compositor_push_toast(&mut self, toast: crate::backend::api::ToastNotification) {
                if let Some(compositor) = self.compositor.as_mut() {
                    compositor.push_toast(toast);
                }
            }

            fn compositor_show_osd(&mut self, kind: crate::backend::api::OsdKind, percent: u8) {
                if let Some(compositor) = self.compositor.as_mut() {
                    compositor.show_osd(kind, percent);
                }
            }

            fn compositor_show_media_osd(&mut self, label: &str) {
                if let Some(compositor) = self.compositor.as_mut() {
                    compositor.show_media_osd(label);
                }
            }

            fn compositor_click_toast(&mut self, x: f32, y: f32) -> crate::backend::api::ToastClick {
                self.compositor
                    .as_mut()
                    .map_or(crate::backend::api::ToastClick::Miss, |compositor| {
                        compositor.click_toast(x, y)
                    })
            }

            fn compositor_notify_tag_switch(
                &mut self,
                duration: std::time::Duration,
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

            fn compositor_set_screenshot_freeze(&mut self, active: bool) {
                if let Some(compositor) = self.compositor.as_mut() {
                    compositor.set_screenshot_freeze(active);
                }
            }

            fn compositor_set_overview_mode(
                &mut self,
                active: bool,
                windows: &[(
                    crate::backend::common_define::WindowId,
                    f32,
                    f32,
                    f32,
                    f32,
                    bool,
                    String,
                )],
            ) {
                if let Some(compositor) = self.compositor.as_mut() {
                    let windows = windows
                        .iter()
                        .filter_map(|(window, x, y, width, height, selected, title)| {
                            self.ids.x11(*window).ok().map(|x11_window| {
                                (
                                    x11_window,
                                    *x,
                                    *y,
                                    *width,
                                    *height,
                                    *selected,
                                    title.clone(),
                                )
                            })
                        })
                        .collect();
                    compositor.set_overview_mode(active, windows);
                }
            }

            fn compositor_set_overview_monitor(&mut self, x: i32, y: i32, width: u32, height: u32) {
                if let Some(compositor) = self.compositor.as_mut() {
                    compositor.set_overview_monitor(x, y, width, height);
                }
            }

            fn compositor_set_monitors(&mut self, monitors: &[(u32, i32, i32, u32, u32, u32)]) {
                if let Some(compositor) = self.compositor.as_mut() {
                    compositor.set_monitors(monitors);
                }
            }

            fn compositor_set_overview_selection(
                &mut self,
                window: crate::backend::common_define::WindowId,
            ) {
                if let Some(compositor) = self.compositor.as_mut() {
                    if let Ok(x11_window) = self.ids.x11(window) {
                        compositor.set_overview_selection(x11_window);
                    }
                }
            }

            fn compositor_set_expose_mode(
                &mut self,
                active: bool,
                windows: Vec<(crate::backend::common_define::WindowId, i32, i32, u32, u32)>,
            ) {
                if let Some(compositor) = self.compositor.as_mut() {
                    let windows = windows
                        .iter()
                        .filter_map(|(window, x, y, width, height)| {
                            self.ids
                                .x11(*window)
                                .ok()
                                .map(|x11_window| (x11_window, *x, *y, *width, *height))
                        })
                        .collect();
                    compositor.set_expose_mode(active, windows);
                }
            }

            fn compositor_expose_click(
                &mut self,
                x: f32,
                y: f32,
            ) -> Option<crate::backend::common_define::WindowId> {
                let x11_window = self.compositor.as_mut()?.expose_click(x, y)?;
                Some(self.ids.$intern_raw(x11_window))
            }

            fn compositor_expose_move(&mut self, dir: crate::backend::api::ExposeNavDirection) {
                if let Some(compositor) = self.compositor.as_mut() {
                    compositor.expose_move_selection(dir);
                }
            }

            fn compositor_expose_selected(
                &mut self,
            ) -> Option<crate::backend::common_define::WindowId> {
                let x11_window = self.compositor.as_mut()?.expose_selected()?;
                Some(self.ids.$intern_raw(x11_window))
            }

            fn compositor_expose_select(
                &mut self,
                win: Option<crate::backend::common_define::WindowId>,
            ) {
                if let Some(compositor) = self.compositor.as_mut() {
                    let x11_window = win.and_then(|window| self.ids.x11(window).ok());
                    compositor.expose_select_id(x11_window);
                }
            }
        }

        impl crate::backend::api::CompositorWindowEffects for $backend {
            fn compositor_set_frame_extents(
                &mut self,
                window: crate::backend::common_define::WindowId,
                left: u32,
                right: u32,
                top: u32,
                bottom: u32,
            ) {
                if let (Some(compositor), Ok(x11_window)) =
                    (self.compositor.as_mut(), self.ids.x11(window))
                {
                    compositor.set_frame_extents(x11_window, left, right, top, bottom);
                }
            }

            fn compositor_set_window_shaped(
                &mut self,
                window: crate::backend::common_define::WindowId,
                shaped: bool,
            ) {
                if let (Some(compositor), Ok(x11_window)) =
                    (self.compositor.as_mut(), self.ids.x11(window))
                {
                    compositor.set_window_shaped(x11_window, shaped);
                }
            }

            fn compositor_set_window_urgent(
                &mut self,
                window: crate::backend::common_define::WindowId,
                urgent: bool,
            ) {
                if let (Some(compositor), Ok(x11_window)) =
                    (self.compositor.as_mut(), self.ids.x11(window))
                {
                    compositor.set_window_urgent(x11_window, urgent);
                }
            }

            fn compositor_set_window_pip(
                &mut self,
                window: crate::backend::common_define::WindowId,
                pip: bool,
            ) {
                if let (Some(compositor), Ok(x11_window)) =
                    (self.compositor.as_mut(), self.ids.x11(window))
                {
                    compositor.set_window_pip(x11_window, pip);
                }
            }

            fn compositor_set_window_minimized(
                &mut self,
                window: crate::backend::common_define::WindowId,
                minimized: bool,
            ) {
                self.compositor_desired.set_minimized(window, minimized);
                let Ok(x11_window) = self.ids.x11(window) else {
                    return;
                };
                if self.compositor.is_none() {
                    return;
                }

                if minimized {
                    if let Some(compositor) = self.compositor.as_mut() {
                        compositor.minimize_window(x11_window);
                    }
                    return;
                }

                // Record the lifecycle direction before any fallible X11
                // query. If geometry is temporarily unavailable, a later
                // MapNotify/lazy AddWindow must still be recognized as the
                // explicit restore rather than a late texture for the prior
                // minimize request.
                if let Some(compositor) = self.compositor.as_mut() {
                    compositor.prepare_window_restore(x11_window);
                }

                // Restoration runs after arrange/show_client, so this synchronous
                // geometry query observes the final on-screen position rather than
                // the temporary off-screen minimize location.
                let Ok(geometry) = self.window_ops.get_geometry(window) else {
                    return;
                };
                let (_, class_name) = self.property_ops.get_class(window);
                let override_redirect = self
                    .window_ops
                    .get_window_attributes(window)
                    .is_ok_and(|attributes| attributes.override_redirect);
                let shaped = self.window_ops.get_window_shaped(window);
                let frame_extents = self.property_ops.get_gtk_frame_extents(window);

                if let Some(compositor) = self.compositor.as_mut() {
                    // Reimport the live texture before starting the reverse genie.
                    // update_geometry also refreshes an already tracked window when
                    // the effect was disabled.
                    compositor
                        .add_window(x11_window, geometry.x, geometry.y, geometry.w, geometry.h);
                    compositor.update_geometry(
                        x11_window,
                        geometry.x,
                        geometry.y,
                        geometry.w,
                        geometry.h,
                        geometry.border,
                    );
                    if !class_name.is_empty() {
                        compositor.set_window_class(x11_window, &class_name);
                    }
                    compositor.set_window_override_redirect(x11_window, override_redirect);
                    compositor.set_window_shaped(x11_window, shaped);
                    if let Some([left, right, top, bottom]) = frame_extents {
                        compositor.set_frame_extents(x11_window, left, right, top, bottom);
                    }
                }
            }

            fn compositor_forget_minimized_window_visual(
                &mut self,
                window: crate::backend::common_define::WindowId,
            ) {
                // JWM still owns the client's hidden state. This command only
                // retires compositor demand, so a later eligibility re-entry
                // can publish geometry and statically adopt the same client.
                self.compositor_desired.retire_window(window);
                if let (Some(compositor), Ok(x11_window)) =
                    (self.compositor.as_mut(), self.ids.x11(window))
                {
                    compositor.forget_minimized_window_visual(x11_window);
                }
            }

            fn compositor_ensure_minimized_window_visual(
                &mut self,
                window: crate::backend::common_define::WindowId,
            ) {
                self.compositor_desired.ensure_minimized(window);
                let Ok(x11_window) = self.ids.x11(window) else {
                    return;
                };
                let needs_import = self.compositor.as_mut().is_some_and(|compositor| {
                    compositor.ensure_minimized_window_visual(x11_window)
                });
                if !needs_import {
                    return;
                }
                let Ok(geometry) = self.window_ops.get_geometry(window) else {
                    return;
                };
                if let Some(compositor) = self.compositor.as_mut() {
                    compositor
                        .add_window(x11_window, geometry.x, geometry.y, geometry.w, geometry.h);
                }
            }

            fn compositor_set_window_dock_geometry(
                &mut self,
                window: crate::backend::common_define::WindowId,
                target: Option<crate::backend::api::CompositorRect>,
            ) {
                let target = target.and_then(crate::backend::api::CompositorRect::normalized);
                self.compositor_desired.set_dock_target(window, target);
                let Ok(x11_window) = self.ids.x11(window) else {
                    return;
                };
                let needs_import = self.compositor.as_mut().is_some_and(|compositor| {
                    compositor.set_window_dock_geometry(x11_window, target)
                });
                if !needs_import {
                    return;
                }
                let Ok(geometry) = self.window_ops.get_geometry(window) else {
                    return;
                };
                if let Some(compositor) = self.compositor.as_mut() {
                    compositor
                        .add_window(x11_window, geometry.x, geometry.y, geometry.w, geometry.h);
                }
            }

            fn compositor_set_minimized_window_preview(
                &mut self,
                window: Option<crate::backend::common_define::WindowId>,
                anchor: Option<crate::backend::api::CompositorRect>,
            ) {
                let request = window.zip(anchor).and_then(|(window, anchor)| {
                    self.ids.x11(window).ok().map(|window| (window, anchor))
                });
                let needs_import = self
                    .compositor
                    .as_mut()
                    .is_some_and(|compositor| compositor.set_minimized_window_preview(request));
                if !needs_import {
                    return;
                }
                let Some(window) = window else {
                    return;
                };
                let Ok(x11_window) = self.ids.x11(window) else {
                    return;
                };
                let Ok(geometry) = self.window_ops.get_geometry(window) else {
                    return;
                };
                if let Some(compositor) = self.compositor.as_mut() {
                    // Static convergence deliberately bypasses minimize_window:
                    // add_window consumes PendingMinimize and caches the pixels
                    // without starting a second Genie timeline.
                    compositor
                        .add_window(x11_window, geometry.x, geometry.y, geometry.w, geometry.h);
                }
            }

            fn compositor_force_full_redraw(&mut self) {
                if let Some(compositor) = self.compositor.as_mut() {
                    compositor.force_full_redraw();
                }
            }

            fn compositor_set_mouse_position(&mut self, x: f32, y: f32) {
                if let Some(compositor) = self.compositor.as_mut() {
                    compositor.set_mouse_position(x, y);
                }
            }

            fn compositor_deactivate_edge_glow(&mut self) {
                if let Some(compositor) = self.compositor.as_mut() {
                    compositor.deactivate_edge_glow();
                }
            }

            fn compositor_unsuppress_edge_glow(&mut self) {
                if let Some(compositor) = self.compositor.as_mut() {
                    compositor.unsuppress_edge_glow();
                }
            }

            fn compositor_notify_window_move_start(
                &mut self,
                window: crate::backend::common_define::WindowId,
            ) {
                if let (Some(compositor), Ok(x11_window)) =
                    (self.compositor.as_mut(), self.ids.x11(window))
                {
                    compositor.notify_window_move_start(x11_window);
                }
            }

            fn compositor_notify_window_move_delta(
                &mut self,
                window: crate::backend::common_define::WindowId,
                dx: f32,
                dy: f32,
            ) {
                if let (Some(compositor), Ok(x11_window)) =
                    (self.compositor.as_mut(), self.ids.x11(window))
                {
                    compositor.notify_window_move_delta(x11_window, dx, dy);
                }
            }

            fn compositor_notify_window_move_end(
                &mut self,
                window: crate::backend::common_define::WindowId,
            ) {
                if let (Some(compositor), Ok(x11_window)) =
                    (self.compositor.as_mut(), self.ids.x11(window))
                {
                    compositor.notify_window_move_end(x11_window);
                }
            }

            fn compositor_request_window_iconify(
                &mut self,
                window: crate::backend::common_define::WindowId,
            ) -> Result<(), crate::backend::error::BackendError> {
                if self.iconify.request(window) {
                    if let (Some(compositor), Ok(x11_window)) =
                        (self.compositor.as_mut(), self.ids.x11(window))
                    {
                        compositor.request_iconic_snapshot_recapture(x11_window);
                    }
                    self.service_iconify_admission_for(window)?;
                }
                Ok(())
            }

            fn compositor_cancel_window_iconify(
                &mut self,
                window: crate::backend::common_define::WindowId,
            ) -> Result<(), crate::backend::error::BackendError> {
                match self.iconify.begin_cancel(window) {
                    crate::backend::x11::wm::iconify::CancelPlan::Nothing
                    | crate::backend::x11::wm::iconify::CancelPlan::RemovedAwaiting => Ok(()),
                    crate::backend::x11::wm::iconify::CancelPlan::MapBeforeRemoving {
                        generation,
                    } => {
                        let map_result = self.window_ops.map_window(window);
                        let window_ops = &self.window_ops;
                        crate::backend::x11::wm::iconify::finish_checked_cancel(
                            &mut self.iconify,
                            window,
                            generation,
                            map_result,
                            || match window_ops.get_window_attributes(window) {
                                Ok(attributes) if attributes.map_state_viewable => {
                                    crate::backend::x11::wm::iconify::ViewabilityVerification::ConfirmedViewable
                                }
                                Ok(_) => crate::backend::x11::wm::iconify::ViewabilityVerification::ConfirmedNotViewable(
                                    crate::backend::error::BackendError::Message(format!(
                                        "X11 window {window:?} was not viewable after checked MapWindow"
                                    )),
                                ),
                                Err(error) => {
                                    crate::backend::x11::wm::iconify::ViewabilityVerification::QueryError(
                                        error,
                                    )
                                }
                            },
                            |mapped_window, mapped_generation| {
                                if let Err(rollback_error) = window_ops.unmap_managed_window(
                                    mapped_window,
                                    crate::backend::api::ManagedUnmapReason::IconifyRetain {
                                        generation: mapped_generation,
                                    },
                                ) {
                                    log::warn!(
                                        "could not roll back Iconic restore map for {mapped_window:?}: {rollback_error}"
                                    );
                                }
                            },
                        )?;
                        // Do not release the pinned CPU snapshot here. Mapping
                        // is not live-import success; the compositor keeps the
                        // only durable pixels until start_genie_restore takes
                        // ownership and retires the snapshot itself.
                        Ok(())
                    }
                }
            }

            fn compositor_set_dock_position(&mut self, x: f32, y: f32) {
                if let Some(compositor) = self.compositor.as_mut() {
                    compositor.set_dock_position(x, y);
                }
            }

            fn compositor_set_peek_mode(&mut self, active: bool) {
                if let Some(compositor) = self.compositor.as_mut() {
                    compositor.set_peek_mode(active);
                }
            }

            fn compositor_set_window_groups(
                &mut self,
                groups: Vec<crate::backend::compositor_common::window_tabs::TabGroup>,
            ) {
                // No id translation here, and deliberately none needed: a group
                // is a rectangle and a list of titles. The window manager owns
                // which window each cell stands for, so no window id crosses
                // this boundary to be mistranslated.
                if let Some(compositor) = self.compositor.as_mut() {
                    compositor.set_window_groups(groups);
                }
            }

            fn compositor_zoom_to_fit(&mut self, window: Option<crate::backend::common_define::WindowId>) {
                if let Some(compositor) = self.compositor.as_mut() {
                    // The compositor's window table is keyed by XID, so the
                    // window manager's handle must be translated here; an
                    // unknown handle zooms nothing instead of matching a
                    // window that happens to share the raw value.
                    let x11_window = window.and_then(|win| self.ids.x11(win).ok());
                    compositor.zoom_to_fit(x11_window);
                }
            }
        }

        impl crate::backend::api::CompositorAnnotation for $backend {
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

            fn compositor_set_annotation_color(&mut self, rgba: [f32; 4]) {
                if let Some(c) = self.compositor.as_mut() {
                    c.set_annotation_color(rgba);
                }
            }

            fn compositor_set_annotation_line_width(&mut self, width: f32) {
                if let Some(c) = self.compositor.as_mut() {
                    c.set_annotation_line_width(width);
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

            fn compositor_annotation_add_quad(
                &mut self,
                quad: crate::backend::compositor_common::annotation_overlay::AnnotationQuad,
            ) {
                if let Some(c) = self.compositor.as_mut() {
                    c.annotation_add_quad(quad);
                }
            }

            fn compositor_annotation_add_text(
                &mut self,
                label: crate::backend::compositor_common::annotation_overlay::AnnotationLabel,
            ) {
                if let Some(c) = self.compositor.as_mut() {
                    c.annotation_add_text(label);
                }
            }

            fn compositor_set_screenshot_toolbar(
                &mut self,
                toolbar: Option<
                    crate::backend::compositor_common::screenshot_toolbar::ScreenshotToolbar,
                >,
            ) {
                // Rectangles, icons and flags only — the window manager keeps
                // the hit test, so nothing here can resolve to the wrong tool.
                if let Some(c) = self.compositor.as_mut() {
                    c.set_screenshot_toolbar(toolbar);
                }
            }
        }
    };
}

pub(crate) use delegate_compositor_capabilities;

#[cfg(test)]
mod desired_state_tests {
    use super::{X11CompositorDesiredState, X11CompositorReplayStep};
    use crate::backend::api::CompositorRect;
    use crate::backend::common_define::WindowId;

    fn window(raw: u64) -> WindowId {
        WindowId::from_raw(raw)
    }

    #[test]
    fn disabled_compositor_accumulates_normalized_dock_and_minimized_state() {
        let mut desired = X11CompositorDesiredState::default();
        let target = CompositorRect::new(-48.0, 1024.0, 40.0, 40.0);

        desired.set_dock_target(window(3), Some(target));
        desired.set_minimized(window(3), true);
        desired.set_dock_target(
            window(4),
            Some(CompositorRect::new(f32::NAN, 1.0, 40.0, 40.0)),
        );
        desired.ensure_minimized(window(4));

        let plan = desired.replay_plan(|window| Some(100 + window.raw() as u32));
        assert_eq!(plan.dock_targets, vec![(103, target)]);
        assert_eq!(plan.minimized_windows, vec![103, 104]);
    }

    #[test]
    fn replay_orders_every_target_before_any_static_minimized_adoption() {
        let mut desired = X11CompositorDesiredState::default();
        let first = CompositorRect::new(10.0, 900.0, 40.0, 40.0);
        let second = CompositorRect::new(60.0, 900.0, 40.0, 40.0);
        desired.set_dock_target(window(9), Some(second));
        desired.set_minimized(window(9), true);
        desired.set_dock_target(window(2), Some(first));
        desired.set_minimized(window(2), true);

        let plan = desired.replay_plan(|window| Some(window.raw() as u32));
        assert_eq!(
            plan.steps().collect::<Vec<_>>(),
            vec![
                X11CompositorReplayStep::DockTarget(2, first),
                X11CompositorReplayStep::DockTarget(9, second),
                X11CompositorReplayStep::EnsureMinimized(2),
                X11CompositorReplayStep::EnsureMinimized(9),
            ]
        );
    }

    #[test]
    fn geometry_withdrawal_preserves_minimized_state_but_restore_removes_both() {
        let mut desired = X11CompositorDesiredState::default();
        let target = CompositorRect::new(10.0, 900.0, 40.0, 40.0);
        desired.set_dock_target(window(7), Some(target));
        desired.set_minimized(window(7), true);

        desired.set_dock_target(window(7), None);
        let plan = desired.replay_plan(|_| Some(77));
        assert!(plan.dock_targets.is_empty());
        assert_eq!(plan.minimized_windows, vec![77]);

        desired.set_dock_target(window(7), Some(target));
        desired.set_minimized(window(7), false);
        let plan = desired.replay_plan(|_| Some(77));
        assert!(plan.dock_targets.is_empty());
        assert!(plan.minimized_windows.is_empty());
    }

    #[test]
    fn replay_prunes_destroyed_internal_identity_before_xid_reuse() {
        let mut desired = X11CompositorDesiredState::default();
        let target = CompositorRect::new(10.0, 900.0, 40.0, 40.0);
        desired.set_dock_target(window(1), Some(target));
        desired.set_minimized(window(1), true);
        desired.set_dock_target(window(2), Some(target));
        desired.set_minimized(window(2), true);

        let plan = desired.replay_plan(|candidate| (candidate == window(2)).then_some(0x200));
        assert_eq!(plan.dock_targets, vec![(0x200, target)]);
        assert_eq!(plan.minimized_windows, vec![0x200]);

        // A second replay must not resolve or revive the retired identity.
        let mut resolved = Vec::new();
        let _ = desired.replay_plan(|window| {
            resolved.push(window);
            Some(0x300)
        });
        assert_eq!(resolved, vec![window(2)]);
    }

    #[test]
    fn explicit_visual_forget_idempotently_removes_disabled_compositor_snapshot() {
        let mut desired = X11CompositorDesiredState::default();
        desired.set_dock_target(
            window(5),
            Some(CompositorRect::new(10.0, 900.0, 40.0, 40.0)),
        );
        desired.set_minimized(window(5), true);
        desired.retire_window(window(5));
        desired.retire_window(window(5));

        let mut resolutions = 0;
        let plan = desired.replay_plan(|_| {
            resolutions += 1;
            Some(55)
        });
        assert_eq!(resolutions, 0, "forgotten state must not reach replay");
        assert!(plan.dock_targets.is_empty());
        assert!(plan.minimized_windows.is_empty());
    }
}
