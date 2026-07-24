//! Shared delegation of the compositor capability traits.
//!
//! Both X11 transports own an optional shared `Compositor<C>` plus a window-id
//! registry and forward the `backend::api` compositor capability traits into
//! them method by method. The forwarding rules — no-op without a compositor,
//! id translation at the boundary, and the minimize/restore re-registration
//! sequence — are transport-free policy, so they are generated once here
//! instead of being maintained as parallel impl blocks in each backend.

/// Implements the compositor capability traits for an X11 transport backend.
///
/// The backend type must expose `compositor: Option<Compositor<_>>`, an `ids`
/// registry with `x11(WindowId) -> Result<u32, BackendError>`, `window_ops`
/// and `property_ops` trait objects, and a `benchmark_auto_exit` flag.
/// `intern_raw` names the registry method interning a raw `u32` window id —
/// the registries predate a shared trait and name it differently.
macro_rules! delegate_compositor_capabilities {
    ($backend:ty, intern_raw = $intern_raw:ident) => {
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
                    // add_window cancels and frees a detached genie copy for this
                    // XID, or cancels an in-flight fallback fade. update_geometry
                    // also refreshes an already tracked window when the effect was
                    // disabled.
                    compositor
                        .add_window(x11_window, geometry.x, geometry.y, geometry.w, geometry.h);
                    compositor.update_geometry(
                        x11_window, geometry.x, geometry.y, geometry.w, geometry.h,
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
                groups: Vec<(u32, Vec<(u32, String, bool)>)>,
            ) {
                if let Some(compositor) = self.compositor.as_mut() {
                    compositor.set_window_groups(groups);
                }
            }

            fn compositor_zoom_to_fit(&mut self, window: Option<u32>) {
                if let Some(compositor) = self.compositor.as_mut() {
                    compositor.zoom_to_fit(window);
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
        }
    };
}

pub(crate) use delegate_compositor_capabilities;
