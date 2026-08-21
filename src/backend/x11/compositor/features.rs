// Feature control methods
#[allow(unused_imports)]
use super::math::ortho;
use super::prism::{MAX_PRISM_SIDES, MIN_PRISM_SIDES};
#[allow(unused_imports)]
use super::*;
use crate::backend::compositor_common::recording_nv12::{
    nv12_frame_bytes, nv12_packed_target_size, nv12_target_fits, recording_output_size,
};
#[allow(unused_imports)]
use glow::HasContext;
#[allow(unused_imports)]
use std::collections::HashMap;
#[allow(unused_imports)]
use std::ffi::CString;
#[allow(unused_imports)]
use std::sync::Arc;
#[allow(unused_imports)]
use std::sync::mpsc;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StaticMinimizedCapturePlan {
    Ignore,
    ArmAndImport,
    RetryImport,
    RecaptureRetained,
}

const fn static_minimized_capture_plan(
    dock_addressable: bool,
    minimized: bool,
    cached_visual: bool,
    active_animation: bool,
    restore_pending: bool,
    static_capture_pending: bool,
    iconic_recapture_due: bool,
) -> StaticMinimizedCapturePlan {
    if !dock_addressable || !minimized || restore_pending {
        StaticMinimizedCapturePlan::Ignore
    } else if iconic_recapture_due && (cached_visual || active_animation) {
        // A retained full-resolution owner satisfies visual presentation but
        // not true-Iconic durability. An explicit demand/capacity epoch may
        // sample it once without importing another X pixmap.
        StaticMinimizedCapturePlan::RecaptureRetained
    } else if cached_visual || active_animation {
        StaticMinimizedCapturePlan::Ignore
    } else if static_capture_pending {
        // The previous synchronous geometry/pixmap import may have failed.
        // A later explicit ensure or Dock lease renewal is a bounded retry;
        // successful cache settlement removes the pending marker and makes
        // the next request Ignore.
        StaticMinimizedCapturePlan::RetryImport
    } else {
        StaticMinimizedCapturePlan::ArmAndImport
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PreviewSourcePlan {
    awaiting_source: bool,
    request_full_source: bool,
}

const fn preview_source_plan(any_drawable_source: bool, full_source: bool) -> PreviewSourcePlan {
    PreviewSourcePlan {
        // A bounded snapshot paints immediately; only a total source miss
        // pauses the show animation.
        awaiting_source: !any_drawable_source,
        // Hover still requests full pixels in parallel and upgrades in place.
        request_full_source: !full_source,
    }
}

const fn minimized_preview_source_is_drawable(
    full_source: bool,
    gpu_snapshot: bool,
    _cpu_snapshot: bool,
) -> bool {
    // OpenGL cannot sample the durable CPU copy directly.  It is only a
    // candidate for a separately armed one-shot upload, never a reason to
    // keep fullscreen composition enabled indefinitely.
    full_source || gpu_snapshot
}

const fn scene_window_is_tracked(
    has_live_texture: bool,
    has_active_genie: bool,
    has_durable_minimized_owner: bool,
) -> bool {
    // A Genie owns the detached live texture while the WM's independent Hide
    // timeline may still carry the same XID in the scene. Treat that texture
    // as tracked so scene reconciliation cannot import and immediately free a
    // duplicate TFP binding on every animation frame. Keep the same protection
    // after the Genie settles into the retained minimized lifecycle: stale
    // core Hide/stacking entries must not manufacture a new live owner.
    has_live_texture || has_active_genie || has_durable_minimized_owner
}

const fn durable_minimized_scene_owner(minimized: bool, explicit_restore_pending: bool) -> bool {
    // An explicit restore deliberately reopens scene-driven import as a retry
    // path when the synchronous backend import failed. All other minimized
    // states own their pixels outside the ordinary live-window map.
    minimized && !explicit_restore_pending
}

const fn overview_request_allowed(enabled: bool, requested_active: bool) -> bool {
    // Disabling the feature blocks only entry/refresh requests. Exit must
    // remain callable so an already-open modal can always release its state.
    enabled || !requested_active
}

/// Whether an active recording should force a composite this frame.
///
/// Client damage and animations already reach the render gate on their own, so
/// this only decides the two cases that produce a new recorded frame without
/// producing any damage: the cursor sprite moving, and the idle heartbeat.
pub(super) const fn recording_capture_warranted(
    frame_due: bool,
    idle_due: bool,
    cursor_moved: bool,
) -> bool {
    frame_due && (idle_due || cursor_moved)
}

/// Whether a motionless screen is owed a capture anyway.
///
/// Dropping unchanged frames is safe for the encoded file only up to a point:
/// ffmpeg stamps each frame it receives with its arrival wall clock and the
/// constant output rate duplicates it to fill the gap, but the file still ends
/// at the *last* frame it was given. Without a heartbeat, a recording left on a
/// still desktop for its final ten seconds would simply be ten seconds short.
fn recording_idle_capture_due(
    last: Option<std::time::Instant>,
    now: std::time::Instant,
    heartbeat: std::time::Duration,
) -> bool {
    last.is_none_or(|last| now.duration_since(last) >= heartbeat)
}

/// Advance the recording capture clock by exactly one interval rather than
/// restarting it from `now`.
///
/// Re-anchoring on the present discards the time the frame itself took, so each
/// capture slips a little later than the last and the cadence quantizes up to
/// the next whole render period — the reason a 30 fps recording on a 60 Hz
/// display sampled closer to 20. Falling more than one interval behind
/// resynchronizes to `now` instead of bursting to catch up.
fn advance_recording_deadline(
    last: Option<std::time::Instant>,
    interval: std::time::Duration,
    now: std::time::Instant,
) -> std::time::Instant {
    match last.map(|last| last + interval) {
        Some(next) if next + interval > now => next,
        _ => now,
    }
}

impl<C: CompositorConnection> Compositor<C> {
    pub(crate) fn set_system_ui(&mut self, overlay: Option<crate::backend::api::SystemUiOverlay>) {
        // Opening a panel springs it out of the bar; closing one forgets its
        // geometry so the next open springs again rather than resuming.
        if self.system_ui.is_none() || overlay.is_none() {
            self.system_ui_island.close();
        }
        // A different panel is a different surface: its width starts over, and
        // its selection appears where it belongs instead of sliding in from a
        // row of the panel it replaced. An update to the *same* panel — a
        // keystroke, a finished scan — keeps both, which is the whole point of
        // carrying them.
        let identity = overlay.as_ref().map_or("", |o| o.title.as_str());
        if identity != self.system_ui_identity {
            self.system_ui_identity = identity.to_string();
            self.system_ui_width_floor = 0.0;
            self.system_ui_highlight.reset();
        }
        self.system_ui = overlay;
        self.needs_render = true;
    }

    pub(crate) fn push_toast(&mut self, toast: crate::backend::api::ToastNotification) {
        let removed = self.toast_stack.push(toast, std::time::Instant::now());
        self.free_toast_textures(&removed);
        self.needs_render = true;
    }

    pub(crate) fn show_osd(&mut self, kind: crate::backend::api::OsdKind, percent: u8) {
        self.osd_slot.show(kind, percent, std::time::Instant::now());
        self.needs_render = true;
    }

    pub(crate) fn show_media_osd(&mut self, label: &str) {
        self.osd_slot.show_media(label, std::time::Instant::now());
        self.needs_render = true;
    }

    pub(crate) fn free_toast_textures(&mut self, ids: &[u64]) {
        for id in ids {
            if let Some(slots) = self.toast_textures.remove(id) {
                for slot in slots.into_iter().flatten() {
                    unsafe { self.gl.delete_texture(slot.0) };
                }
            }
        }
    }
    pub(crate) fn has_partial_damage(&self) -> bool {
        self.partial_damage_enabled
    }

    pub(crate) fn set_partial_damage(&mut self, enabled: bool) -> bool {
        if self.partial_damage_enabled == enabled {
            return false;
        }
        self.partial_damage_enabled = enabled;
        self.damage_tracker.mark_all_dirty();
        self.dirty_region_tracker.mark_all_dirty();
        self.needs_render = true;
        true
    }

    pub(crate) fn set_mouse_position(&mut self, x: f32, y: f32) {
        self.mouse_x = x;
        self.mouse_y = y;
        self.forward_waterlily_pointer(x, y);
        if self.edge_glow {
            self.edge_glow_tick(x, y);
        }
        if self.magnifier_enabled || self.window_tilt {
            self.needs_render = true;
        }
        if self.expose_active {
            self.expose_set_hover(x, y);
        }
    }

    /// Core edge-glow state machine (called from mouse events and render tick).
    ///
    /// - Mouse at edge (unsuppressed) → activate.
    /// - Mouse away or suppressed     → deactivate immediately.
    pub(super) fn edge_glow_tick(&mut self, mx: f32, my: f32) {
        let sw = self.screen_w as f32;
        let sh = self.screen_h as f32;
        let min_dist = mx.min(sw - mx).min(my).min(sh - my);
        let at_edge = min_dist < self.edge_glow_width;

        if at_edge && !self.edge_glow_suppressed {
            if !self.edge_glow_active {
                self.edge_glow_active = true;
                self.needs_render = true;
            }
        } else if self.edge_glow_active {
            self.edge_glow_active = false;
            self.needs_render = true;
        }
    }

    /// Immediately deactivate the edge glow and suppress re-activation
    /// until the pointer leaves the window (returns to root/desktop).
    pub(crate) fn deactivate_edge_glow(&mut self) {
        if self.edge_glow {
            self.edge_glow_suppressed = true;
            if self.edge_glow_active {
                self.edge_glow_active = false;
                self.needs_render = true;
            }
        }
    }

    /// Clear the edge-glow suppression (pointer returned to desktop).
    pub(crate) fn unsuppress_edge_glow(&mut self) {
        self.edge_glow_suppressed = false;
    }

    pub(crate) fn set_window_urgent(&mut self, x11_win: u32, urgent: bool) {
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            wt.is_urgent = urgent;
            self.needs_render = true;
        }
        if let Some(metadata) = self.minimized_window_metadata.get_mut(&x11_win) {
            metadata.is_urgent = urgent;
        }
    }

    pub(crate) fn set_window_pip(&mut self, x11_win: u32, pip: bool) {
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            wt.is_pip = pip;
            self.needs_render = true;
        }
        if let Some(metadata) = self.minimized_window_metadata.get_mut(&x11_win) {
            metadata.is_pip = pip;
        }
    }

    /// Notify the compositor about audio stream timing for a window.
    /// This lets the compositor schedule frame presentation to match
    /// each window's independent audio clock, preventing desync.
    pub(crate) fn notify_audio_timing(&mut self, x11_win: u32, fps: f32, buffer_latency_ms: u32) {
        self.audio_sync
            .register_stream(x11_win, fps, buffer_latency_ms);
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            wt.audio_sync_target = Some(fps);
        }
        if let Some(metadata) = self.minimized_window_metadata.get_mut(&x11_win) {
            metadata.audio_sync_target = Some(fps);
        }
        // Register with OML for per-window vblank timing too
        if let Some(oml) = &mut self.oml {
            oml.register_window(x11_win, fps);
        }
    }

    /// Register a window for Present extension support
    #[allow(dead_code)]
    pub(crate) fn register_window_present(&mut self, x11_win: u32) {
        if let Some(present_mgr) = &mut self.present_mgr {
            match present_mgr.register_window(x11_win) {
                Ok(()) => {
                    log::debug!("compositor: window 0x{:x} registered with Present", x11_win);
                }
                Err(e) => {
                    log::warn!(
                        "compositor: failed to register 0x{:x} with Present: {}",
                        x11_win,
                        e
                    );
                }
            }
        }
    }

    /// Present a window's pixmap at a specific MSC (for Present-enabled windows)
    #[allow(dead_code)]
    pub(crate) fn present_pixmap(&self, x11_win: u32, pixmap: u32, target_msc: u64, serial: u32) {
        if let Some(present_mgr) = &self.present_mgr {
            match present_mgr.present_pixmap(x11_win, pixmap, target_msc, serial) {
                Ok(()) => {
                    log::debug!(
                        "compositor: presented pixmap for 0x{:x} (serial={}, msc={})",
                        x11_win,
                        serial,
                        target_msc
                    );
                }
                Err(e) => {
                    log::debug!(
                        "compositor: present_pixmap failed for 0x{:x}: {}",
                        x11_win,
                        e
                    );
                }
            }
        }
    }

    pub(crate) fn set_magnifier(&mut self, enabled: bool) {
        self.magnifier_enabled = enabled;
        self.ensure_postprocess_fbo();
        self.needs_render = true;
    }

    pub(crate) fn set_overview_monitor(&mut self, x: i32, y: i32, w: u32, h: u32) {
        self.overview_mon_x = x;
        self.overview_mon_y = y;
        self.overview_mon_w = w;
        self.overview_mon_h = h;
    }

    pub(crate) fn set_overview_mode(
        &mut self,
        active: bool,
        windows: Vec<(u32, f32, f32, f32, f32, bool, String)>,
    ) {
        if !overview_request_allowed(self.overview_enabled, active) {
            return;
        }
        if !active && self.overview_active && !self.overview_closing {
            // Begin exit animation — don't clear state yet
            self.overview_closing = true;
            self.overview_exit_progress = 1.0;
            self.needs_render = true;
            return;
        }
        self.clear_overview_snapshots();
        self.clear_overview_title_textures();
        self.overview_active = active;
        self.overview_closing = false;
        let n = windows.len();
        // The prism takes the shape of the window count: 4 windows really do
        // give a cube. Fewer than three sides has no volume, more than six
        // makes the front face too small to read.
        let sides = n.clamp(MIN_PRISM_SIDES, MAX_PRISM_SIDES);
        let face_w = self.screen_w as f32 * 0.8;
        let face_h = self.screen_h as f32 * 0.8;
        self.overview_windows = windows
            .into_iter()
            .enumerate()
            .map(|(i, (win, _x, _y, _w, _h, sel, title))| OverviewEntry {
                x11_win: win,
                target_w: face_w,
                target_h: face_h,
                is_selected: sel,
                snapshot_texture: None,
                title,
                title_texture: None,
                face_index: i % sides,
            })
            .collect();
        self.overview_prism_sides = sides;
        self.overview_total_clients = n;
        self.overview_slide_offset = 0;
        // Face the selected window straight away. The prism used to always
        // start at face 0, which pointed the camera at the wrong window
        // whenever the focused client was not the first one in the layout.
        let selected_face = self
            .overview_windows
            .iter()
            .find(|entry| entry.is_selected)
            .map_or(0, |entry| entry.face_index);
        let facing = -(selected_face as f32) * std::f32::consts::TAU / sides as f32;
        self.overview_prism_target_angle = facing;
        self.overview_prism_current_angle = facing;
        self.overview_prism_last_tick = None;
        self.overview_prism_spin = 0.0;
        if active {
            self.refresh_overview_snapshots();
            self.create_overview_title_textures();
            self.overview_entry_progress = 0.0;
            self.overview_exit_progress = 1.0;
            self.overview_opacity = 0.0;
        } else {
            self.overview_entry_progress = 1.0;
            self.overview_exit_progress = 1.0;
            self.overview_opacity = 0.0;
        }
        self.needs_render = true;
    }

    pub(crate) fn set_overview_selection(&mut self, x11_win: u32) {
        let mut selected_face = 0usize;
        for entry in &mut self.overview_windows {
            let sel = entry.x11_win == x11_win;
            entry.is_selected = sel;
            if sel {
                selected_face = entry.face_index;
            }
        }
        // Rotate prism so selected face faces the camera.
        let sides = self.overview_prism_sides.max(MIN_PRISM_SIDES) as f32;
        let new_target = -(selected_face as f32) * std::f32::consts::TAU / sides;
        // Normalize angular difference to shortest path (within -PI..PI).
        let mut diff = new_target - self.overview_prism_target_angle;
        while diff > std::f32::consts::PI {
            diff -= 2.0 * std::f32::consts::PI;
        }
        while diff < -std::f32::consts::PI {
            diff += 2.0 * std::f32::consts::PI;
        }
        self.overview_prism_target_angle += diff;
        self.needs_render = true;
    }

    pub(crate) fn notify_window_move_start(&mut self, x11_win: u32) {
        if self.motion_trail_enabled
            && let Some(wt) = self.windows.get_mut(&x11_win)
        {
            wt.motion_trail.begin_drag(wt.x as f32, wt.y as f32);
        }
        if !self.wobbly_windows {
            return;
        }
        let grid_n =
            crate::backend::compositor_common::effects::wobbly_node_count(self.wobbly_grid_size);
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            // Determine anchor node: closest grid node to mouse position
            let rel_x = ((self.mouse_x - wt.x as f32).max(0.0)).min(wt.w as f32);
            let rel_y = ((self.mouse_y - wt.y as f32).max(0.0)).min(wt.h as f32);
            let (anchor_row, anchor_col) =
                WobblyState::anchor_for_point(grid_n, rel_x, rel_y, wt.w as f32, wt.h as f32);

            wt.wobbly = Some(WobblyState::new(
                grid_n,
                anchor_row,
                anchor_col,
                wt.w as f32,
                wt.h as f32,
            ));
        } else {
            log::warn!(
                "[wobbly] move_start: window 0x{:x} not tracked by compositor",
                x11_win
            );
        }
    }

    pub(crate) fn notify_window_move_delta(&mut self, x11_win: u32, dx: f32, dy: f32) {
        // Phase 3.1: Record position for motion trail
        self.update_motion_trail(x11_win, dx, dy);

        if self.wobbly_windows {
            if let Some(wt) = self.windows.get_mut(&x11_win) {
                if let Some(ref mut w) = wt.wobbly {
                    // The window has already moved to the new position.
                    // Anchor node stays at [0,0] (moves with the window).
                    // All OTHER nodes get a reverse impulse to simulate inertia.
                    w.apply_window_move_delta(dx, dy);
                }
            }
        }

        // During interactive move/resize, request full-frame redraw when backdrop
        // blur is active so translucent windows see real-time updated background.
        let blur_active =
            self.blur_enabled && self.scene_fbo.is_some() && !self.blur_fbos.is_empty() && {
                let cfg = crate::config::CONFIG.load();
                let status_bar_name = cfg.status_bar_name();
                self.windows
                    .values()
                    .any(|wt| self.needs_backdrop_blur(wt, status_bar_name))
            };
        if blur_active {
            self.damage_tracker.mark_all_dirty();
            self.dirty_region_tracker.mark_all_dirty(); // P5C: Sync rect tracker
        }
        self.needs_render = true;
    }

    pub(crate) fn notify_window_move_end(&mut self, x11_win: u32) {
        // Release anchor — let all nodes spring back via tick_wobbly
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            wt.motion_trail.end_drag();
            if let Some(ref mut w) = wt.wobbly {
                w.end_drag();
            }
        }
        // Keep the trail alive briefly after release and let wall-clock expiry
        // fade it out instead of making it disappear on the button-up frame.
        self.needs_render = true;
    }

    #[allow(dead_code)]
    pub(crate) fn tracked_window_count(&self) -> usize {
        self.windows.len()
    }

    /// Set dock/taskbar position for genie minimize target.
    pub(crate) fn set_dock_position(&mut self, x: f32, y: f32) {
        self.dock_position = (x, y);
    }

    /// Set the exact Dock slot for one managed window.  A bar can publish the
    /// fallback region first and refine it after laying out the new item; an
    /// in-flight mesh follows the updated slot instead of finishing at stale
    /// geometry.
    pub(crate) fn set_window_dock_geometry(
        &mut self,
        x11_win: u32,
        target: Option<crate::backend::api::CompositorRect>,
    ) -> bool {
        let target = target.and_then(crate::backend::api::CompositorRect::normalized);
        match target {
            Some(target) => {
                self.genie_targets.insert(x11_win, target);
                if let Some(animation) = self
                    .genie_active
                    .iter_mut()
                    .find(|animation| animation.x11_win == x11_win)
                {
                    animation.target = target;
                }
                if let Some(visual) = self.minimized_visuals.get_mut(&x11_win) {
                    visual.target = Some(target);
                }
                self.touch_minimized_visual(x11_win, std::time::Instant::now());
                self.touch_minimized_snapshot(x11_win);
                self.arm_minimized_gpu_upload(x11_win);
            }
            None => {
                self.pending_minimized_gpu_uploads.remove(&x11_win);
                self.genie_targets.remove(&x11_win);
                // Keep the bounded CPU copy across a bar restart, but reclaim
                // its upload while no Dock consumer can address the window.
                self.remove_minimized_gpu_snapshot(x11_win);
                // Geometry withdrawal can race the synchronous pixmap query
                // used by static recapture. Cancel only that pending import;
                // the authoritative minimized marker remains, and a future
                // addressable target will rearm it without replaying Genie.
                if self.minimized_window_intents.get(&x11_win)
                    == Some(&MinimizedWindowIntent::PendingMinimize)
                    && !self.minimized_visuals.contains_key(&x11_win)
                    && !self
                        .genie_active
                        .iter()
                        .any(|animation| animation.x11_win == x11_win)
                {
                    self.minimized_window_intents.remove(&x11_win);
                }
                self.pending_static_minimized_captures.remove(&x11_win);
                let fallback = self.genie_target_for(x11_win);
                if let Some(animation) = self
                    .genie_active
                    .iter_mut()
                    .find(|animation| animation.x11_win == x11_win)
                {
                    animation.target = fallback;
                }
                if let Some(visual) = self.minimized_visuals.get_mut(&x11_win) {
                    visual.target = None;
                }
                if self
                    .dock_preview
                    .is_some_and(|preview| preview.x11_win == x11_win)
                {
                    self.set_minimized_window_preview(None);
                }
            }
        }
        self.needs_render = true;
        self.arm_static_minimized_capture(x11_win, target.is_some())
    }

    /// Adopt a WM-hidden window into the compositor's durable minimized
    /// lifecycle without playing a Genie from its deliberately off-screen
    /// parking geometry. A Dock target gates the actual import; this lets a
    /// later bar layout command be the first point at which the pixels become
    /// addressable.
    pub(crate) fn ensure_minimized_window_visual(&mut self, x11_win: u32) -> bool {
        self.ensure_minimized_snapshot_generation(x11_win);
        self.request_iconic_snapshot_recapture(x11_win);
        self.minimized_windows.insert(x11_win);
        self.arm_static_minimized_capture(x11_win, self.genie_targets.contains_key(&x11_win))
    }

    /// Return true when the backend must synchronously import the X pixmap.
    /// If it is already tracked, settle it here so a duplicate AddWindow does
    /// not allocate a second native pixmap/damage pair.
    fn arm_static_minimized_capture(&mut self, x11_win: u32, dock_addressable: bool) -> bool {
        let active_animation = self
            .genie_active
            .iter()
            .any(|animation| animation.x11_win == x11_win);
        let restore_pending = self.minimized_window_intents.get(&x11_win)
            == Some(&MinimizedWindowIntent::ExplicitRestore);
        let plan = static_minimized_capture_plan(
            dock_addressable,
            self.minimized_windows.contains(&x11_win),
            self.minimized_visuals.contains_key(&x11_win),
            active_animation,
            restore_pending,
            self.pending_static_minimized_captures.contains(&x11_win),
            self.iconic_snapshot_recapture_due(x11_win),
        );
        match plan {
            StaticMinimizedCapturePlan::Ignore => return false,
            StaticMinimizedCapturePlan::RecaptureRetained => {
                // The explicit demand was armed by ensure/minimize/iconify.
                // Wake render, but never issue GL work from this WM-facing
                // feature bridge: only the post-make-current render service
                // may consume the gate.
                self.needs_render = true;
                return false;
            }
            StaticMinimizedCapturePlan::ArmAndImport | StaticMinimizedCapturePlan::RetryImport => {}
        }

        self.pending_static_minimized_captures.insert(x11_win);
        self.minimized_window_intents
            .insert(x11_win, MinimizedWindowIntent::PendingMinimize);
        if self.windows.contains_key(&x11_win) {
            self.settle_late_minimized_window(x11_win);
            false
        } else {
            true
        }
    }

    pub(super) fn current_minimized_cpu_snapshot_available(&self, x11_win: u32) -> bool {
        self.minimized_snapshot_generations
            .get(&x11_win)
            .is_some_and(|generation| {
                self.minimized_snapshots
                    .peek(&x11_win)
                    .is_some_and(|snapshot| snapshot.generation() == *generation)
            })
    }

    pub(super) fn current_minimized_gpu_snapshot_available(&self, x11_win: u32) -> bool {
        self.minimized_snapshot_generations
            .get(&x11_win)
            .is_some_and(|generation| {
                self.minimized_gpu_snapshots
                    .get(&x11_win)
                    .is_some_and(|snapshot| snapshot.generation == *generation)
            })
    }

    pub(super) fn minimized_full_preview_source_available(&self, x11_win: u32) -> bool {
        self.minimized_visuals.contains_key(&x11_win)
            || self
                .genie_active
                .iter()
                .any(|animation| animation.x11_win == x11_win)
            || self.windows.contains_key(&x11_win)
    }

    pub(super) fn minimized_preview_source_available(&self, x11_win: u32) -> bool {
        self.minimized_full_preview_source_available(x11_win)
            || self.current_minimized_gpu_snapshot_available(x11_win)
            || self.current_minimized_cpu_snapshot_available(x11_win)
    }

    /// Whether pixels already live in a GL-sampleable owner this frame.
    /// CPU-only snapshots remain candidates for one explicitly armed upload,
    /// but cannot by themselves defeat fullscreen unredirect.
    pub(super) fn minimized_preview_drawable_source_available(&self, x11_win: u32) -> bool {
        minimized_preview_source_is_drawable(
            self.minimized_full_preview_source_available(x11_win),
            self.current_minimized_gpu_snapshot_available(x11_win),
            self.current_minimized_cpu_snapshot_available(x11_win),
        )
    }

    /// Start the preview timeline only once pixels exist. A hidden-surface
    /// import can take longer than a frame, especially after an LRU eviction;
    /// consuming the show animation and lease while there is nothing to draw
    /// would make the eventual texture pop in or expire unseen.
    pub(super) fn resume_minimized_preview_after_capture(&mut self, x11_win: u32) {
        let Some(preview) = self
            .dock_preview
            .as_mut()
            .filter(|preview| preview.x11_win == x11_win && preview.awaiting_source)
        else {
            return;
        };
        let now = std::time::Instant::now();
        preview.started = now;
        preview.lease_deadline = now + std::time::Duration::from_secs(4);
        preview.start_opacity = 0.0;
        preview.start_scale = 0.86;
        preview.direction = crate::backend::compositor_common::genie::PreviewDirection::Show;
        preview.opacity = 0.0;
        preview.scale = 0.86;
        preview.awaiting_source = false;
        self.needs_render = true;
    }

    /// Animate the compositor-owned preview rather than asking every bar
    /// toolkit to transport and upload window pixels independently.
    pub(crate) fn set_minimized_window_preview(
        &mut self,
        request: Option<(u32, crate::backend::api::CompositorRect)>,
    ) -> bool {
        use crate::backend::compositor_common::genie::{
            PreviewDirection, preview_motion, preview_request_reuses_timeline,
        };

        let request = request
            .and_then(|(x11_win, anchor)| anchor.normalized().map(|anchor| (x11_win, anchor)))
            .filter(|(x11_win, _)| {
                self.minimized_preview_source_available(*x11_win)
                    || self.minimized_windows.contains(x11_win)
            });
        let now = std::time::Instant::now();
        match request {
            Some((x11_win, anchor)) => {
                self.touch_minimized_visual(x11_win, now);
                self.touch_minimized_snapshot(x11_win);
                self.arm_minimized_gpu_upload(x11_win);
                let source_plan = preview_source_plan(
                    self.minimized_preview_source_available(x11_win),
                    self.minimized_full_preview_source_available(x11_win),
                );
                if self.dock_preview.is_some_and(|preview| {
                    preview_request_reuses_timeline(preview.x11_win == x11_win, preview.direction)
                }) {
                    let (awaiting_source, anchor_changed) = {
                        let preview = self
                            .dock_preview
                            .as_mut()
                            .expect("matching Dock preview disappeared");
                        let anchor_changed = preview.anchor != anchor;
                        preview.anchor = anchor;
                        preview.lease_deadline = now + std::time::Duration::from_secs(4);
                        (preview.awaiting_source, anchor_changed)
                    };
                    if anchor_changed {
                        // Keep the existing show timeline/opacity. Replacing
                        // the preview here would restart its fade on every
                        // bounded macOS-style magnification anchor refresh.
                        self.needs_render = true;
                    }
                    // The first renewal after an active preview's source was
                    // evicted services that retained intent. If that
                    // synchronous import fails, later lease renewals retry;
                    // successful cache settlement makes subsequent renewals
                    // source-backed and therefore import-free.
                    return (awaiting_source || source_plan.request_full_source)
                        && self.arm_static_minimized_capture(x11_win, true);
                }
                let awaiting_source = source_plan.awaiting_source;
                self.dock_preview = Some(DockPreview {
                    x11_win,
                    anchor,
                    started: now,
                    lease_deadline: now + std::time::Duration::from_secs(4),
                    start_opacity: 0.0,
                    start_scale: 0.86,
                    direction: PreviewDirection::Show,
                    opacity: 0.0,
                    scale: 0.86,
                    awaiting_source,
                });
                let needs_import = source_plan.request_full_source
                    && self.arm_static_minimized_capture(x11_win, true);
                self.needs_render = true;
                needs_import
            }
            None => {
                let Some(preview) = self.dock_preview.as_mut() else {
                    return false;
                };
                if preview.direction == PreviewDirection::Hide {
                    return false;
                }
                if preview.awaiting_source {
                    self.dock_preview = None;
                    self.needs_render = true;
                    return false;
                }
                let (opacity, scale, _) = preview_motion(
                    preview.start_opacity,
                    preview.start_scale,
                    preview.direction,
                    now.duration_since(preview.started).as_secs_f32(),
                );
                preview.started = now;
                preview.start_opacity = opacity;
                preview.start_scale = scale;
                preview.opacity = opacity;
                preview.scale = scale;
                preview.direction = PreviewDirection::Hide;
                self.needs_render = true;
                false
            }
        }
    }

    #[allow(dead_code)]
    pub(crate) fn has_window(&self, x11_win: u32) -> bool {
        let has_durable_minimized_owner = durable_minimized_scene_owner(
            self.minimized_windows.contains(&x11_win),
            self.minimized_window_intents.get(&x11_win)
                == Some(&super::MinimizedWindowIntent::ExplicitRestore),
        );
        scene_window_is_tracked(
            self.windows.contains_key(&x11_win),
            self.genie_active
                .iter()
                .any(|animation| animation.x11_win == x11_win),
            has_durable_minimized_owner,
        )
    }

    // =====================================================================
    // Phase 6: Accessibility & Utility
    // =====================================================================

    pub(crate) fn set_colorblind_mode(&mut self, mode: &str) {
        let m = match mode {
            "deuteranopia" => 1,
            "protanopia" => 2,
            "tritanopia" => 3,
            _ => 0,
        };
        if self.colorblind_mode != m {
            self.colorblind_mode = m;
            self.ensure_postprocess_fbo();
            self.needs_render = true;
        }
    }

    pub(crate) fn zoom_to_fit(&mut self, window: Option<u32>) {
        if let Some(win) = window {
            if self.zoom_to_fit_window == Some(win) {
                self.zoom_to_fit_window = None;
                self.zoom_to_fit_target = 1.0;
            } else {
                self.zoom_to_fit_window = Some(win);
                if let Some(wt) = self.windows.get(&win) {
                    if wt.w > 0 && wt.h > 0 {
                        let sx = self.screen_w as f32 / wt.w as f32;
                        let sy = self.screen_h as f32 / wt.h as f32;
                        self.zoom_to_fit_target = sx.min(sy);
                    }
                }
            }
            self.needs_render = true;
        } else {
            self.zoom_to_fit_window = None;
            self.zoom_to_fit_target = 1.0;
            self.needs_render = true;
        }
    }

    // =====================================================================
    // Phase 7: Diagnostics
    // =====================================================================

    pub(crate) fn reload_shader_from_file(
        &mut self,
        name: &str,
        path: &std::path::Path,
    ) -> Result<(), String> {
        // Box blur is an optimization mode implemented with the regular blur
        // programs, not a standalone GL program. Compiling a replacement for
        // it would leave the new program unowned and leak it.
        if name == "box_blur" {
            return Err(
                "box_blur has no standalone shader program; reload blur_down or blur_up instead"
                    .to_string(),
            );
        }

        let file_content =
            std::fs::read_to_string(path).map_err(|e| format!("read shader file: {e}"))?;

        let (vs_src, fs_src) = match name {
            "window" => (shaders::VERTEX_SHADER, file_content.as_str()),
            "shadow" => (shaders::VERTEX_SHADER, file_content.as_str()),
            "border" => (shaders::VERTEX_SHADER, file_content.as_str()),
            "blur_down" => (shaders::BLUR_DOWN_VERTEX, file_content.as_str()),
            "blur_up" => (shaders::BLUR_DOWN_VERTEX, file_content.as_str()),
            "postprocess" => (shaders::BLUR_DOWN_VERTEX, file_content.as_str()),
            "hud" => (shaders::VERTEX_SHADER, file_content.as_str()),
            "hud_text" => (shaders::VERTEX_SHADER, file_content.as_str()),
            "transition" => (shaders::BLUR_DOWN_VERTEX, file_content.as_str()),
            "portal" => (shaders::BLUR_DOWN_VERTEX, file_content.as_str()),
            "edge_glow" => (shaders::VERTEX_SHADER, file_content.as_str()),
            "tilt" => (shaders::TILT_VERTEX_SHADER, file_content.as_str()),
            "wobbly" => (shaders::WOBBLY_VERTEX_SHADER, file_content.as_str()),
            "particle" => (shaders::PARTICLE_VERTEX_SHADER, file_content.as_str()),
            "genie" => (shaders::GENIE_VERTEX_SHADER, file_content.as_str()),
            "overview_bg" => (shaders::VERTEX_SHADER, file_content.as_str()),
            "overview_face" => (shaders::OVERVIEW_FACE_VERTEX_SHADER, file_content.as_str()),
            "overview_cap" => (shaders::OVERVIEW_CAP_VERTEX_SHADER, file_content.as_str()),
            _ if name.ends_with("_vs") => {
                log::warn!(
                    "compositor: shader reload requires both vertex and fragment shaders to be specified"
                );
                return Err(format!(
                    "shader {} needs corresponding fragment shader",
                    name
                ));
            }
            _ => return Err(format!("unknown shader: {name}")),
        };

        match unsafe { Self::create_program(&self.gl, vs_src, fs_src) } {
            Ok(new_program) => {
                unsafe {
                    match name {
                        "window" => {
                            let uniforms = WindowUniforms {
                                projection: self
                                    .gl
                                    .get_uniform_location(new_program, "u_projection"),
                                rect: self.gl.get_uniform_location(new_program, "u_rect"),
                                texture: self.gl.get_uniform_location(new_program, "u_texture"),
                                opacity: self.gl.get_uniform_location(new_program, "u_opacity"),
                                radius: self.gl.get_uniform_location(new_program, "u_radius"),
                                size: self.gl.get_uniform_location(new_program, "u_size"),
                                dim: self.gl.get_uniform_location(new_program, "u_dim"),
                                desat: self.gl.get_uniform_location(new_program, "u_desat"),
                                uv_rect: self.gl.get_uniform_location(new_program, "u_uv_rect"),
                                ripple_progress: self
                                    .gl
                                    .get_uniform_location(new_program, "u_ripple_progress"),
                                ripple_amplitude: self
                                    .gl
                                    .get_uniform_location(new_program, "u_ripple_amplitude"),
                            };
                            let old_program = std::mem::replace(&mut self.program, new_program);
                            self.win_uniforms = uniforms;
                            self.gl.delete_program(old_program);
                        }
                        "shadow" => {
                            let uniforms = ShadowUniforms {
                                projection: self
                                    .gl
                                    .get_uniform_location(new_program, "u_projection"),
                                rect: self.gl.get_uniform_location(new_program, "u_rect"),
                                shadow_color: self
                                    .gl
                                    .get_uniform_location(new_program, "u_shadow_color"),
                                size: self.gl.get_uniform_location(new_program, "u_size"),
                                radius: self.gl.get_uniform_location(new_program, "u_radius"),
                                spread: self.gl.get_uniform_location(new_program, "u_spread"),
                            };
                            let old_program =
                                std::mem::replace(&mut self.shadow_program, new_program);
                            self.shadow_uniforms = uniforms;
                            self.gl.delete_program(old_program);
                        }
                        "border" => {
                            let uniforms = BorderUniforms {
                                projection: self
                                    .gl
                                    .get_uniform_location(new_program, "u_projection"),
                                rect: self.gl.get_uniform_location(new_program, "u_rect"),
                                border_color: self
                                    .gl
                                    .get_uniform_location(new_program, "u_border_color"),
                                size: self.gl.get_uniform_location(new_program, "u_size"),
                                radius: self.gl.get_uniform_location(new_program, "u_radius"),
                                radius_top: self
                                    .gl
                                    .get_uniform_location(new_program, "u_radius_top"),
                                border_width: self
                                    .gl
                                    .get_uniform_location(new_program, "u_border_width"),
                            };
                            let old_program =
                                std::mem::replace(&mut self.border_program, new_program);
                            self.border_uniforms = uniforms;
                            self.gl.delete_program(old_program);
                        }
                        "blur_down" => {
                            let uniforms = BlurUniforms {
                                projection: self
                                    .gl
                                    .get_uniform_location(new_program, "u_projection"),
                                rect: self.gl.get_uniform_location(new_program, "u_rect"),
                                texture: self.gl.get_uniform_location(new_program, "u_texture"),
                                halfpixel: self.gl.get_uniform_location(new_program, "u_halfpixel"),
                            };
                            let old_program =
                                std::mem::replace(&mut self.blur_down_program, new_program);
                            self.blur_down_uniforms = uniforms;
                            self.gl.delete_program(old_program);
                        }
                        "blur_up" => {
                            let uniforms = BlurUniforms {
                                projection: self
                                    .gl
                                    .get_uniform_location(new_program, "u_projection"),
                                rect: self.gl.get_uniform_location(new_program, "u_rect"),
                                texture: self.gl.get_uniform_location(new_program, "u_texture"),
                                halfpixel: self.gl.get_uniform_location(new_program, "u_halfpixel"),
                            };
                            let old_program =
                                std::mem::replace(&mut self.blur_up_program, new_program);
                            self.blur_up_uniforms = uniforms;
                            self.gl.delete_program(old_program);
                        }
                        "postprocess" => {
                            let uniforms = PostprocessUniforms {
                                projection: self
                                    .gl
                                    .get_uniform_location(new_program, "u_projection"),
                                rect: self.gl.get_uniform_location(new_program, "u_rect"),
                                texture: self.gl.get_uniform_location(new_program, "u_texture"),
                                color_temp: self
                                    .gl
                                    .get_uniform_location(new_program, "u_color_temp"),
                                saturation: self
                                    .gl
                                    .get_uniform_location(new_program, "u_saturation"),
                                brightness: self
                                    .gl
                                    .get_uniform_location(new_program, "u_brightness"),
                                contrast: self.gl.get_uniform_location(new_program, "u_contrast"),
                                invert: self.gl.get_uniform_location(new_program, "u_invert"),
                                grayscale: self.gl.get_uniform_location(new_program, "u_grayscale"),
                                hdr_enabled: self
                                    .gl
                                    .get_uniform_location(new_program, "u_hdr_enabled"),
                                hdr_peak_nits: self
                                    .gl
                                    .get_uniform_location(new_program, "u_hdr_peak_nits"),
                                tone_mapping_method: self
                                    .gl
                                    .get_uniform_location(new_program, "u_tone_mapping_method"),
                                eotf_mode: self.gl.get_uniform_location(new_program, "u_eotf_mode"),
                                output_colorspace: self
                                    .gl
                                    .get_uniform_location(new_program, "u_output_colorspace"),
                            };
                            let magnifier_uniforms = MagnifierUniforms {
                                magnifier_enabled: self
                                    .gl
                                    .get_uniform_location(new_program, "u_magnifier_enabled"),
                                magnifier_center: self
                                    .gl
                                    .get_uniform_location(new_program, "u_magnifier_center"),
                                magnifier_radius: self
                                    .gl
                                    .get_uniform_location(new_program, "u_magnifier_radius"),
                                magnifier_zoom: self
                                    .gl
                                    .get_uniform_location(new_program, "u_magnifier_zoom"),
                                colorblind_mode: self
                                    .gl
                                    .get_uniform_location(new_program, "u_colorblind_mode"),
                            };
                            let old_program =
                                std::mem::replace(&mut self.postprocess_program, new_program);
                            self.postprocess_uniforms = uniforms;
                            self.magnifier_uniforms = magnifier_uniforms;
                            self.gl.delete_program(old_program);
                        }
                        "hud" => {
                            let uniforms = HudUniforms {
                                projection: self
                                    .gl
                                    .get_uniform_location(new_program, "u_projection"),
                                rect: self.gl.get_uniform_location(new_program, "u_rect"),
                                bg_color: self.gl.get_uniform_location(new_program, "u_bg_color"),
                                size: self.gl.get_uniform_location(new_program, "u_size"),
                            };
                            let old_program = std::mem::replace(&mut self.hud_program, new_program);
                            self.hud_uniforms = uniforms;
                            self.gl.delete_program(old_program);
                        }
                        "hud_text" => {
                            let uniforms = HudTextUniforms {
                                projection: self
                                    .gl
                                    .get_uniform_location(new_program, "u_projection"),
                                rect: self.gl.get_uniform_location(new_program, "u_rect"),
                                texture: self.gl.get_uniform_location(new_program, "u_texture"),
                                opacity: self.gl.get_uniform_location(new_program, "u_opacity"),
                            };
                            let old_program =
                                std::mem::replace(&mut self.hud_text_program, new_program);
                            self.hud_text_uniforms = uniforms;
                            self.gl.delete_program(old_program);
                        }
                        "transition" => {
                            let uniforms = TransitionUniforms {
                                projection: self
                                    .gl
                                    .get_uniform_location(new_program, "u_projection"),
                                rect: self.gl.get_uniform_location(new_program, "u_rect"),
                                texture: self.gl.get_uniform_location(new_program, "u_texture"),
                                opacity: self.gl.get_uniform_location(new_program, "u_opacity"),
                                uv_rect: self.gl.get_uniform_location(new_program, "u_uv_rect"),
                            };
                            let old_program =
                                std::mem::replace(&mut self.transition_program, new_program);
                            self.transition_uniforms = uniforms;
                            self.gl.delete_program(old_program);
                        }
                        "portal" => {
                            let uniforms = PortalUniforms {
                                projection: self
                                    .gl
                                    .get_uniform_location(new_program, "u_projection"),
                                rect: self.gl.get_uniform_location(new_program, "u_rect"),
                                texture: self.gl.get_uniform_location(new_program, "u_texture"),
                                progress: self.gl.get_uniform_location(new_program, "u_progress"),
                                glow: self.gl.get_uniform_location(new_program, "u_glow"),
                                center: self.gl.get_uniform_location(new_program, "u_center"),
                                uv_rect: self.gl.get_uniform_location(new_program, "u_uv_rect"),
                            };
                            let old_program =
                                std::mem::replace(&mut self.portal_program, new_program);
                            self.portal_uniforms = uniforms;
                            self.gl.delete_program(old_program);
                        }
                        "edge_glow" => {
                            let uniforms = EdgeGlowUniforms {
                                projection: self
                                    .gl
                                    .get_uniform_location(new_program, "u_projection"),
                                rect: self.gl.get_uniform_location(new_program, "u_rect"),
                                glow_color: self
                                    .gl
                                    .get_uniform_location(new_program, "u_glow_color"),
                                glow_width: self
                                    .gl
                                    .get_uniform_location(new_program, "u_glow_width"),
                                mouse: self.gl.get_uniform_location(new_program, "u_mouse"),
                                screen_size: self
                                    .gl
                                    .get_uniform_location(new_program, "u_screen_size"),
                                time: self.gl.get_uniform_location(new_program, "u_time"),
                            };
                            let old_program =
                                std::mem::replace(&mut self.edge_glow_program, new_program);
                            self.edge_glow_uniforms = uniforms;
                            self.gl.delete_program(old_program);
                        }
                        "tilt" => {
                            let uniforms = TiltUniforms {
                                projection: self
                                    .gl
                                    .get_uniform_location(new_program, "u_projection"),
                                rect: self.gl.get_uniform_location(new_program, "u_rect"),
                                texture: self.gl.get_uniform_location(new_program, "u_texture"),
                                opacity: self.gl.get_uniform_location(new_program, "u_opacity"),
                                radius: self.gl.get_uniform_location(new_program, "u_radius"),
                                size: self.gl.get_uniform_location(new_program, "u_size"),
                                dim: self.gl.get_uniform_location(new_program, "u_dim"),
                                uv_rect: self.gl.get_uniform_location(new_program, "u_uv_rect"),
                                tilt: self.gl.get_uniform_location(new_program, "u_tilt"),
                                perspective: self
                                    .gl
                                    .get_uniform_location(new_program, "u_perspective"),
                                grid_size: self.gl.get_uniform_location(new_program, "u_grid_size"),
                                light_dir: self.gl.get_uniform_location(new_program, "u_light_dir"),
                            };
                            let old_program =
                                std::mem::replace(&mut self.tilt_program, new_program);
                            self.tilt_uniforms = uniforms;
                            self.gl.delete_program(old_program);
                        }
                        "wobbly" => {
                            let uniforms = WobblyUniforms {
                                projection: self
                                    .gl
                                    .get_uniform_location(new_program, "u_projection"),
                                rect: self.gl.get_uniform_location(new_program, "u_rect"),
                                texture: self.gl.get_uniform_location(new_program, "u_texture"),
                                opacity: self.gl.get_uniform_location(new_program, "u_opacity"),
                                radius: self.gl.get_uniform_location(new_program, "u_radius"),
                                size: self.gl.get_uniform_location(new_program, "u_size"),
                                dim: self.gl.get_uniform_location(new_program, "u_dim"),
                                uv_rect: self.gl.get_uniform_location(new_program, "u_uv_rect"),
                                grid_offsets: self
                                    .gl
                                    .get_uniform_location(new_program, "u_grid_offsets"),
                                grid_n: self.gl.get_uniform_location(new_program, "u_grid_n"),
                            };
                            let old_program =
                                std::mem::replace(&mut self.wobbly_program, new_program);
                            self.wobbly_uniforms = uniforms;
                            self.gl.delete_program(old_program);
                        }
                        "particle" => {
                            let uniforms = ParticleUniforms {
                                projection: self
                                    .gl
                                    .get_uniform_location(new_program, "u_projection"),
                                point_size: self
                                    .gl
                                    .get_uniform_location(new_program, "u_point_size"),
                            };
                            let old_program =
                                std::mem::replace(&mut self.particle_program, new_program);
                            self.particle_uniforms = uniforms;
                            self.gl.delete_program(old_program);
                        }
                        "genie" => {
                            let uniforms = GenieUniforms {
                                projection: self
                                    .gl
                                    .get_uniform_location(new_program, "u_projection"),
                                rect: self.gl.get_uniform_location(new_program, "u_rect"),
                                texture: self.gl.get_uniform_location(new_program, "u_texture"),
                                opacity: self.gl.get_uniform_location(new_program, "u_opacity"),
                                radius: self.gl.get_uniform_location(new_program, "u_radius"),
                                size: self.gl.get_uniform_location(new_program, "u_size"),
                                dim: self.gl.get_uniform_location(new_program, "u_dim"),
                                uv_rect: self.gl.get_uniform_location(new_program, "u_uv_rect"),
                                progress: self.gl.get_uniform_location(new_program, "u_progress"),
                                dock_pos: self.gl.get_uniform_location(new_program, "u_dock_pos"),
                                dock_size: self.gl.get_uniform_location(new_program, "u_dock_size"),
                                grid_size: self.gl.get_uniform_location(new_program, "u_grid_size"),
                            };
                            let old_program =
                                std::mem::replace(&mut self.genie_program, new_program);
                            self.genie_uniforms = uniforms;
                            self.gl.delete_program(old_program);
                        }
                        "overview_bg" => {
                            let uniforms = OverviewBgUniforms {
                                projection: self
                                    .gl
                                    .get_uniform_location(new_program, "u_projection"),
                                rect: self.gl.get_uniform_location(new_program, "u_rect"),
                                opacity: self.gl.get_uniform_location(new_program, "u_opacity"),
                                angle: self.gl.get_uniform_location(new_program, "u_angle"),
                                time: self.gl.get_uniform_location(new_program, "u_time"),
                                ground: self.gl.get_uniform_location(new_program, "u_ground"),
                                accent: self.gl.get_uniform_location(new_program, "u_accent"),
                            };
                            let old_program =
                                std::mem::replace(&mut self.overview_bg_program, new_program);
                            self.overview_bg_uniforms = uniforms;
                            self.gl.delete_program(old_program);
                        }
                        "overview_face" => {
                            let uniforms = OverviewFaceUniforms {
                                mvp: self.gl.get_uniform_location(new_program, "u_mvp"),
                                model: self.gl.get_uniform_location(new_program, "u_model"),
                                aspect: self.gl.get_uniform_location(new_program, "u_aspect"),
                                texture: self.gl.get_uniform_location(new_program, "u_texture"),
                                uv_rect: self.gl.get_uniform_location(new_program, "u_uv_rect"),
                                camera: self.gl.get_uniform_location(new_program, "u_camera"),
                                accent: self.gl.get_uniform_location(new_program, "u_accent"),
                                brightness: self
                                    .gl
                                    .get_uniform_location(new_program, "u_brightness"),
                                alpha: self.gl.get_uniform_location(new_program, "u_alpha"),
                                desat: self.gl.get_uniform_location(new_program, "u_desat"),
                                reflect: self.gl.get_uniform_location(new_program, "u_reflect"),
                                glass: self.gl.get_uniform_location(new_program, "u_glass"),
                                edge: self.gl.get_uniform_location(new_program, "u_edge"),
                                time: self.gl.get_uniform_location(new_program, "u_time"),
                            };
                            let old_program =
                                std::mem::replace(&mut self.overview_face_program, new_program);
                            self.overview_face_uniforms = uniforms;
                            self.gl.delete_program(old_program);
                        }
                        "overview_cap" => {
                            let uniforms = OverviewCapUniforms {
                                mvp: self.gl.get_uniform_location(new_program, "u_mvp"),
                                radius: self.gl.get_uniform_location(new_program, "u_radius"),
                                y: self.gl.get_uniform_location(new_program, "u_y"),
                                sides: self.gl.get_uniform_location(new_program, "u_sides"),
                                color: self.gl.get_uniform_location(new_program, "u_color"),
                                accent: self.gl.get_uniform_location(new_program, "u_accent"),
                                time: self.gl.get_uniform_location(new_program, "u_time"),
                                reflect: self.gl.get_uniform_location(new_program, "u_reflect"),
                            };
                            let old_program =
                                std::mem::replace(&mut self.overview_cap_program, new_program);
                            self.overview_cap_uniforms = uniforms;
                            self.gl.delete_program(old_program);
                        }
                        _ => {
                            // Keep this defensive arm leak-free if a new name
                            // is added above without a swap implementation.
                            self.gl.delete_program(new_program);
                            return Err(format!(
                                "shader reload is not implemented for program: {name}"
                            ));
                        }
                    }
                }
                self.needs_render = true;
                log::info!("compositor: shader reload succeeded for {name}");
                Ok(())
            }
            Err(e) => {
                log::warn!("compositor: shader reload failed for {name}: {e}");
                Err(e)
            }
        }
    }

    pub(crate) fn enable_shader_hot_reload(&mut self, shader_dir: &str) {
        if shader_dir.is_empty() {
            log::warn!("compositor: shader_dir is empty, cannot enable hot-reload");
            return;
        }
        let dir = std::path::PathBuf::from(shader_dir);
        if !dir.exists() {
            if let Err(e) = std::fs::create_dir_all(&dir) {
                log::warn!("compositor: failed to create shader_dir '{shader_dir}': {e}");
                return;
            }
        }
        self.shader_hot_reload_enabled = true;
        self.shader_dir = shader_dir.to_string();
        self.shader_file_mtimes.clear();
        log::info!("compositor: shader hot-reload enabled, watching '{shader_dir}'");
    }

    pub(crate) fn poll_shader_hot_reload(&mut self) {
        if !self.shader_hot_reload_enabled || self.shader_dir.is_empty() {
            return;
        }

        const SHADER_NAMES: &[&str] = &[
            "window",
            "shadow",
            "border",
            "blur_down",
            "blur_up",
            "postprocess",
            "hud",
            "hud_text",
            "transition",
            "portal",
            "edge_glow",
            "tilt",
            "wobbly",
            "particle",
            "genie",
            "overview_bg",
            "overview_face",
            "overview_cap",
        ];

        let dir = std::path::PathBuf::from(&self.shader_dir);
        let mut to_reload: Vec<(String, std::path::PathBuf)> = Vec::new();

        for &name in SHADER_NAMES {
            let path = dir.join(format!("{name}.frag"));
            if !path.exists() {
                continue;
            }
            let mtime = match std::fs::metadata(&path).and_then(|m| m.modified()) {
                Ok(t) => t,
                Err(_) => continue,
            };
            let changed = match self.shader_file_mtimes.get(name) {
                Some(&prev) => mtime != prev,
                None => true,
            };
            if changed {
                self.shader_file_mtimes.insert(name.to_string(), mtime);
                to_reload.push((name.to_string(), path));
            }
        }

        for (name, path) in to_reload {
            match self.reload_shader_from_file(&name, &path) {
                Ok(()) => log::info!("compositor: hot-reloaded shader '{name}'"),
                Err(e) => log::warn!("compositor: hot-reload failed for '{name}': {e}"),
            }
        }
    }

    pub(crate) fn start_recording(&mut self, output_path: &str) {
        self.start_recording_region(output_path, (0, 0, self.screen_w, self.screen_h));
    }

    pub(crate) fn start_recording_region(
        &mut self,
        output_path: &str,
        region: (i32, i32, u32, u32),
    ) {
        if self.recording_active {
            return;
        }
        self.set_recording_region(region);
        let (_, _, region_w, region_h) = self.recording_region;
        // Scale to the configured height cap if there is one, then snap to what
        // the NV12 layout can express: four pixels share a luma texel and chroma
        // is subsampled vertically. The snap costs at most three columns and one
        // row, and 4:2:0 already requires even dimensions.
        let (w, h) = recording_output_size(region_w, region_h, self.recording_max_height);
        if w == 0 || h == 0 {
            log::warn!("compositor: recording region {region_w}x{region_h} is too small to encode");
            return;
        }
        self.recording_output_size = (w, h);
        let fps = self.recording_fps.clamp(1, 240);

        let recording_fbo = match unsafe { Self::create_recording_fbo(&self.gl, w, h) } {
            Ok(fbo) => fbo,
            Err(error) => {
                log::warn!("compositor: failed to create recording framebuffer: {error}");
                return;
            }
        };
        if !self.build_recording_programs() {
            unsafe {
                self.gl.delete_framebuffer(recording_fbo.0);
                self.gl.delete_texture(recording_fbo.1);
            }
            return;
        }
        // Convert on the GPU when the driver can hold the packed target, which
        // every real desktop driver can. The plain RGBA readback stays as the
        // fallback for a driver at the ES 3.0 minimum texture size.
        let max_texture_size =
            unsafe { self.gl.get_parameter_i32(glow::MAX_TEXTURE_SIZE) }.max(0) as u32;
        let (packed_w, packed_h) = nv12_packed_target_size(w, h);
        let nv12_fbo = nv12_target_fits(w, h, max_texture_size)
            .then(|| unsafe { Self::create_recording_fbo(&self.gl, packed_w, packed_h) })
            .and_then(|created| match created {
                Ok(fbo) => Some(fbo),
                Err(error) => {
                    log::warn!("compositor: NV12 packing framebuffer unavailable: {error}");
                    None
                }
            });
        // Owned by the compositor from here on, so every later bail-out frees
        // it through `release_recording_gpu` instead of having to remember to.
        self.recording_nv12_fbo = nv12_fbo;
        self.recording_nv12 = self.recording_nv12_fbo.is_some();
        if !self.recording_nv12 {
            log::warn!(
                "compositor: recording {w}x{h} needs a {packed_w}x{packed_h} packing target but \
                 the driver caps textures at {max_texture_size}; falling back to RGBA capture"
            );
        }

        // Everything downstream — PBO size, sink size, ffmpeg input format —
        // follows from which capture path was chosen above.
        let capture_frame_bytes = if self.recording_nv12 {
            nv12_frame_bytes(w, h)
        } else {
            (w as usize) * (h as usize) * 4
        };

        let stderr_file = std::fs::File::create("/tmp/jwm-ffmpeg.log")
            .unwrap_or_else(|_| std::fs::File::create("/dev/null").unwrap());

        use crate::backend::compositor_common::media::VAAPI_DEVICE;
        use crate::backend::compositor_common::media::{
            RecordingEncoder, append_recording_audio_input, append_recording_audio_output,
            append_recording_log_args, append_software_encoder_pacing, deprioritize_encoder,
            recording_audio_available, select_recording_encoder,
        };
        use crate::backend::compositor_common::recording_sink::RecordingSink;
        let encoder = select_recording_encoder(&self.recording_encoder);
        let (audio_enabled, audio_device, audio_bitrate) = {
            let cfg = crate::config::CONFIG.load();
            let behavior = cfg.behavior();
            (
                behavior.recording_audio_enabled,
                behavior.recording_audio_device.clone(),
                behavior.recording_audio_bitrate.clone(),
            )
        };
        let with_audio = audio_enabled && recording_audio_available(&audio_device);
        if audio_enabled && !with_audio {
            log::warn!(
                "compositor: recording microphone '{}' unavailable; continuing video-only",
                audio_device
            );
        }
        // Ubuntu/Debian's ffmpeg builds expose the software H.264 encoder as
        // libx264.  libopenh264 is not generally compiled in and made the
        // recorder accept the IPC request while ffmpeg immediately exited.
        let codec_name = encoder.codec_name("libx264");
        let bitrate = &self.recording_bitrate;
        let quality_str = self.recording_quality.to_string();
        log::info!(
            "compositor: recording encoder={codec_name}, capture={region_w}x{region_h}, size={w}x{h}, fps={fps}, bitrate={bitrate}, qp={quality_str}, output={output_path}"
        );

        let size_str = format!("{w}x{h}");
        let fps_str = fps.to_string();
        let mut args: Vec<String> = Vec::new();

        append_recording_log_args(&mut args);
        if matches!(encoder, RecordingEncoder::Vaapi) {
            args.extend(["-vaapi_device", VAAPI_DEVICE].map(str::to_string));
        }
        // Input: use wall clock timestamps so video duration matches real time.
        // The nominal `-r` is moved to the output side; ffmpeg duplicates/drops
        // frames automatically to produce a constant-frame-rate file.
        args.extend(
            [
                "-use_wallclock_as_timestamps",
                "1",
                "-f",
                "rawvideo",
                "-pix_fmt",
                if self.recording_nv12 { "nv12" } else { "rgba" },
                "-s",
                size_str.as_str(),
                "-i",
                "pipe:0",
            ]
            .map(str::to_string),
        );
        if with_audio {
            append_recording_audio_input(&mut args, &audio_device);
        }
        // The packing shader samples the capture target upside down, so on that
        // path the bytes already arrive the right way up and no `vflip` is
        // needed. The RGBA fallback reads a bottom-up framebuffer and still
        // does. VAAPI uploads either way, and nv12 needs no conversion with it.
        let mut filters: Vec<&str> = Vec::new();
        if !self.recording_nv12 {
            filters.push("vflip");
        }
        if matches!(encoder, RecordingEncoder::Vaapi) {
            if !self.recording_nv12 {
                filters.push("format=nv12");
            }
            filters.push("hwupload");
        }
        if !filters.is_empty() {
            args.extend(["-vf".to_string(), filters.join(",")]);
        }
        args.push("-c:v".into());
        args.push(codec_name.into());
        match encoder {
            RecordingEncoder::Vaapi => {
                args.extend(["-rc_mode", "CQP", "-qp", quality_str.as_str()].map(str::to_string))
            }
            _ => args.extend(["-b:v", bitrate.as_str()].map(str::to_string)),
        }
        if matches!(encoder, RecordingEncoder::Software) {
            append_software_encoder_pacing(&mut args);
        }
        if with_audio {
            append_recording_audio_output(&mut args, &audio_bitrate);
        }
        // Pin the chroma format for the software encoder only. Left to
        // negotiate from RGBA input, libx264 picks yuv444p / High 4:4:4
        // Predictive: roughly twice the encoding work per frame and a profile
        // many players refuse. The hardware encoders are the opposite case —
        // they convert from RGB themselves, on the GPU, so naming a pixel
        // format here only forces a CPU swscale pass in front of them. Dropping
        // it measured 17% off NVENC's process CPU with byte-identical colour.
        // VAAPI already converts in its own filter chain.
        if matches!(encoder, RecordingEncoder::Software) {
            args.extend(["-pix_fmt", "yuv420p"].map(str::to_string));
        }
        if self.recording_nv12 {
            // Must travel with the shader's BT.709 matrix. Tagging without
            // converting, or converting without tagging, both shift colour.
            args.extend(
                [
                    "-colorspace",
                    "bt709",
                    "-color_primaries",
                    "bt709",
                    "-color_trc",
                    "bt709",
                    "-color_range",
                    "tv",
                ]
                .map(str::to_string),
            );
        }
        args.extend(
            [
                "-r",
                fps_str.as_str(),
                "-movflags",
                "+faststart",
                "-y",
                output_path,
            ]
            .map(str::to_string),
        );

        let mut command = std::process::Command::new("ffmpeg");
        command
            .args(&args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(stderr_file);
        deprioritize_encoder(&mut command);
        let child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                log::warn!("compositor: failed to start ffmpeg: {e}");
                unsafe {
                    self.gl.delete_framebuffer(recording_fbo.0);
                    self.gl.delete_texture(recording_fbo.1);
                }
                self.release_recording_gpu();
                self.recording_nv12 = false;
                return;
            }
        };

        unsafe {
            for pbo in &mut self.recording_pbo {
                if let Ok(buf) = self.gl.create_buffer() {
                    self.gl.bind_buffer(glow::PIXEL_PACK_BUFFER, Some(buf));
                    self.gl.buffer_data_size(
                        glow::PIXEL_PACK_BUFFER,
                        capture_frame_bytes as i32,
                        glow::STREAM_READ,
                    );
                    self.gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
                    *pbo = Some(buf);
                }
            }
        }

        // The encoder is fed from its own thread: a full RGBA frame is orders of
        // magnitude larger than a pipe buffer, so writing it from here is what
        // used to freeze the whole session whenever ffmpeg fell behind.
        self.recording_sink = Some(RecordingSink::spawn(
            child,
            capture_frame_bytes,
            "compositor",
        ));
        self.recording_fbo = Some(recording_fbo);
        // The cursor is sampled on its own connection for as long as the
        // recording lasts, so the capture path never waits on the X server.
        self.recording_cursor_sampler = Some(RecordingCursorSampler::start(
            std::time::Duration::from_secs_f64(1.0 / f64::from(fps)),
        ));
        self.recording_active = true;
        self.recording_last_frame = None;
        self.recording_last_cursor = None;
        self.recording_started_at = Some(std::time::Instant::now());
        self.recording_current_pbo = 0;
        self.recording_captured_frames = 0;
        log::info!(
            "compositor: recording started to {output_path} (microphone={})",
            if with_audio {
                audio_device.as_str()
            } else {
                "off"
            }
        );
    }

    pub(crate) fn set_recording_region(&mut self, region: (i32, i32, u32, u32)) {
        let (x, y, width, height) = region;
        let x = x.clamp(0, self.screen_w.saturating_sub(1) as i32);
        let y = y.clamp(0, self.screen_h.saturating_sub(1) as i32);
        let width = width.max(1).min(self.screen_w.saturating_sub(x as u32));
        let height = height.max(1).min(self.screen_h.saturating_sub(y as u32));
        self.recording_region = (x, y, width, height);
        self.needs_render = true;
    }

    pub(crate) fn set_recording_region_overlay(&mut self, region: Option<(i32, i32, u32, u32)>) {
        self.recording_region_overlay = region;
        self.force_full_redraw();
    }

    pub(crate) fn stop_recording(&mut self) {
        // `capture_recording_frame` clears recording_active when the ffmpeg
        // pipe breaks.  The child and PBOs still need cleanup in that case;
        // returning solely on the flag leaks a zombie ffmpeg process.
        if !self.recording_active && self.recording_sink.is_none() {
            return;
        }
        let was_active = self.recording_active;
        self.recording_active = false;

        // Drain the last asynchronous ReadPixels before closing ffmpeg. This
        // keeps the final frame instead of silently truncating every recording.
        if was_active && self.recording_captured_frames > 0 {
            let last_pbo = self.recording_current_pbo ^ 1;
            self.write_recording_pbo(last_pbo);
        }

        unsafe {
            for pbo in &mut self.recording_pbo {
                if let Some(buf) = pbo.take() {
                    self.gl.delete_buffer(buf);
                }
            }
        }
        self.recording_last_cursor = None;
        self.recording_started_at = None;
        self.release_recording_gpu();
        // Joins its worker, which the condvar wakes immediately.
        self.recording_cursor_sampler = None;
        if let Some((fbo, texture)) = self.recording_fbo.take() {
            unsafe {
                self.gl.delete_framebuffer(fbo);
                self.gl.delete_texture(texture);
            }
        }

        // Hand the encoder off rather than waiting for it. ffmpeg's exit path
        // flushes the encoder and, with `+faststart`, rewrites the entire MP4 to
        // move the moov atom to the front — seconds of work on a long recording,
        // during which the compositor would render and accept input for nobody.
        // The writer thread reaps the child; failures surface in the log.
        if let Some(sink) = self.recording_sink.take() {
            log::info!("compositor: recording stopped ({})", sink.finish());
        } else {
            log::info!("compositor: recording stopped");
        }
    }

    /// Interval between captured frames at the configured recording rate.
    fn recording_frame_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs_f64(1.0 / self.recording_frame_rate() as f64)
    }

    fn recording_frame_rate(&self) -> u32 {
        self.recording_fps.clamp(1, 240)
    }

    /// Whether the next recording frame is due. This is what keeps recording
    /// from turning the compositor into a free-running renderer: a full-screen
    /// recomposite is only worth doing when a frame will actually be captured
    /// from it. Reporting the whole recording as "needs render" instead pinned
    /// both X11 loops to a 1 ms dispatch timeout and recomposited the entire
    /// screen ~1000 times a second to feed a 30 fps encoder.
    pub(crate) fn recording_frame_due(&self) -> bool {
        self.recording_frame_deadline()
            .is_some_and(|remaining| remaining.is_zero())
    }

    /// What the recording in progress is actually achieving.
    pub(crate) fn recording_stats(&self) -> Option<crate::backend::api::RecordingStats> {
        let started = self.recording_started_at?;
        if !self.recording_active {
            return None;
        }
        Some(crate::backend::api::RecordingStats {
            output_size: self.recording_output_size,
            captured: self.recording_captured_frames,
            dropped: self
                .recording_sink
                .as_ref()
                .map_or(0, |sink| sink.dropped_frames()),
            elapsed_secs: started.elapsed().as_secs_f64(),
        })
    }

    /// How long a screen with nothing happening on it may go without being
    /// captured. See `recording_idle_capture_due` for why it cannot be
    /// unbounded.
    const RECORDING_IDLE_HEARTBEAT: std::time::Duration = std::time::Duration::from_millis(500);

    /// Whether a capture is owed even though nothing on screen has changed.
    pub(crate) fn recording_heartbeat_due(&self) -> bool {
        recording_idle_capture_due(
            self.recording_last_frame,
            std::time::Instant::now(),
            Self::RECORDING_IDLE_HEARTBEAT,
        )
    }

    /// Whether the pointer has moved since the frame we last captured.
    ///
    /// The cursor is a server-side sprite drawn in after compositing, so it is
    /// invisible to the compositor's damage tracking: without this, moving the
    /// mouse across an otherwise still desktop would record a cursor that jumps
    /// only twice a second.
    pub(super) fn recording_cursor_moved(&self) -> bool {
        let Some(sampler) = self.recording_cursor_sampler.as_ref() else {
            return false;
        };
        match (
            sampler.latest().map(|cursor| cursor.position()),
            self.recording_last_cursor,
        ) {
            (Some(now), Some(last)) => now != last,
            (Some(_), None) => true,
            (None, _) => false,
        }
    }

    /// Time until the next recording frame, or `None` when not recording. The
    /// event loop sleeps on this so a static desktop still gets captured at the
    /// configured rate without polling for it.
    pub(crate) fn recording_frame_deadline(&self) -> Option<std::time::Duration> {
        if !self.recording_active {
            return None;
        }
        let Some(last) = self.recording_last_frame else {
            return Some(std::time::Duration::ZERO);
        };
        Some(
            self.recording_frame_interval()
                .saturating_sub(std::time::Instant::now().duration_since(last)),
        )
    }

    pub(super) fn capture_recording_frame(&mut self) {
        if !self.recording_active {
            return;
        }
        // A broken encoder pipe is reported asynchronously by the writer
        // thread; stop feeding it as soon as we notice.
        if self
            .recording_sink
            .as_ref()
            .is_some_and(|sink| sink.is_broken())
        {
            log::warn!("compositor: recording encoder pipe closed; stopping capture");
            self.recording_active = false;
            return;
        }

        // The render gate uses the same deadline, so a frame that reached here
        // for some other reason (client damage, an animation) still only
        // captures at the recording rate.
        if !self.recording_frame_due() {
            return;
        }
        self.recording_last_frame = Some(advance_recording_deadline(
            self.recording_last_frame,
            self.recording_frame_interval(),
            std::time::Instant::now(),
        ));

        let (w, h) = self.recording_output_size;
        let Some((recording_fbo, _)) = self.recording_fbo else {
            return;
        };
        // Overlap the current GPU readback with sending the preceding PBO to
        // ffmpeg. This avoids a GPU/CPU round-trip on every frame.
        let written_pbo = self.recording_current_pbo;
        let Some(pbo) = self.recording_pbo[written_pbo] else {
            return;
        };
        let region = self.recording_region;
        // Sampled off-thread, so this never waits on the X server.
        let cursor = self
            .recording_cursor_sampler
            .as_ref()
            .and_then(RecordingCursorSampler::latest);
        self.recording_last_cursor = cursor.as_ref().map(RecordingCursor::position);

        unsafe {
            let (x, y, region_width, region_height) = region;
            let source_bottom = self.screen_h as i32 - (y + region_height as i32);
            self.gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
            self.gl
                .bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(recording_fbo));
            self.gl.blit_framebuffer(
                x,
                source_bottom,
                x + region_width as i32,
                source_bottom + region_height as i32,
                0,
                0,
                w as i32,
                h as i32,
                glow::COLOR_BUFFER_BIT,
                glow::LINEAR,
            );
            self.gl
                .bind_framebuffer(glow::FRAMEBUFFER, Some(recording_fbo));
            self.gl.viewport(0, 0, w as i32, h as i32);
        }
        // The pointer is a server-side sprite that compositing never sees, so
        // it is drawn in here — on the GPU, into the capture target, before the
        // frame is packed. Doing it after the readback is what the CPU path used
        // to do, and that is incompatible with a subsampled pixel format.
        if let Some(cursor) = cursor.as_ref() {
            self.draw_recording_cursor(cursor, region, (w, h));
        }

        // Convert to NV12 on the GPU so the readback, the copy out of mapped
        // memory, the pipe and ffmpeg's read all carry 1.5 bytes per pixel
        // instead of 4, and the encoder needs no conversion pass at all.
        let (read_w, read_h) = if self.recording_nv12 {
            if !self.pack_recording_nv12((w, h)) {
                // Same restoration as the success path below: bailing out must
                // not leave the frame with a capture-sized viewport or the
                // cursor pass's blend state.
                unsafe {
                    self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                    self.gl
                        .viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
                    self.gl.enable(glow::BLEND);
                    self.gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
                }
                return;
            }
            nv12_packed_target_size(w, h)
        } else {
            (w, h)
        };
        unsafe {
            self.gl.bind_buffer(glow::PIXEL_PACK_BUFFER, Some(pbo));
            self.gl.read_pixels(
                0,
                0,
                read_w as i32,
                read_h as i32,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::BufferOffset(0),
            );
            self.gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            // Put back the GL state the rest of the frame is entitled to
            // assume. The compositor sets blending up once at startup and never
            // re-establishes it per draw, so the packing pass turning it off
            // would have left whatever draws next — the recording region
            // overlay is right behind us — compositing without alpha.
            self.gl
                .viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
            self.gl.enable(glow::BLEND);
            self.gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
        }

        self.recording_current_pbo ^= 1;
        if self.recording_captured_frames > 0 {
            self.write_recording_pbo(written_pbo ^ 1);
        }
        self.recording_captured_frames += 1;
    }

    fn write_recording_pbo(&mut self, pbo_index: usize) {
        let Some(pbo) = self.recording_pbo[pbo_index] else {
            return;
        };
        let Some(mut sink) = self.recording_sink.take() else {
            return;
        };
        let (width, height) = self.recording_output_size;
        let buf_size = if self.recording_nv12 {
            nv12_frame_bytes(width, height)
        } else {
            (width as usize) * (height as usize) * 4
        };
        let mut frame = sink.take_buffer();
        let mut filled = false;
        unsafe {
            self.gl.bind_buffer(glow::PIXEL_PACK_BUFFER, Some(pbo));
            let ptr = self.gl.map_buffer_range(
                glow::PIXEL_PACK_BUFFER,
                0,
                buf_size as i32,
                glow::MAP_READ_BIT,
            );
            if ptr.is_null() {
                log::warn!("compositor: recording PBO map returned null");
            } else {
                // Copy straight out and unmap. Mapped pixel-buffer memory is
                // frequently uncached or write-combined, where reads run an
                // order of magnitude slower than against the heap, so one bulk
                // copy is the only access this path makes to it.
                // The sink was sized from the same `recording_output_size` that
                // sized the PBO, so these always agree; clamp anyway rather than
                // let a future divergence turn into an out-of-bounds copy.
                debug_assert_eq!(frame.len(), buf_size);
                let copied = buf_size.min(frame.len());
                std::ptr::copy_nonoverlapping(ptr as *const u8, frame.as_mut_ptr(), copied);
                self.gl.unmap_buffer(glow::PIXEL_PACK_BUFFER);
                filled = copied == buf_size;
            }
            self.gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
        }

        if filled {
            // The cursor was drawn on the GPU before packing, so the bytes here
            // are already a finished NV12 frame.
            // Non-blocking: a frame the encoder has no room for is dropped, and
            // ffmpeg's wall-clock input timestamps plus the constant output rate
            // keep the result correctly paced.
            sink.submit(frame);
        } else {
            sink.return_buffer(frame);
        }
        if sink.is_broken() {
            log::warn!("compositor: recording encoder pipe closed; stopping capture");
            self.recording_active = false;
        }
        self.recording_sink = Some(sink);
    }

    /// P6A: Process deferred X11 operations
    /// Called at start of render_frame to batch operations
    pub(super) fn process_deferred_x11_ops(&mut self) {
        while let Some(op) = self.deferred_ops_queue.pop() {
            match op.op_type.as_str() {
                "name_pixmap" => {
                    // Deferred NameWindowPixmap operation
                    // This was originally in event handler, now batched in render thread
                    log::debug!(
                        "compositor: processing deferred name_pixmap for window 0x{:x}",
                        op.window_id
                    );
                    // Implementation would go here (currently placeholder)
                }
                "destroy_pixmap" => {
                    // Deferred pixmap destruction
                    log::debug!(
                        "compositor: processing deferred destroy_pixmap for window 0x{:x}",
                        op.window_id
                    );
                }
                _ => {
                    log::warn!("compositor: unknown deferred op type: {}", op.op_type);
                }
            }
        }
    }
}

#[cfg(test)]
mod recording_pacing_tests {
    use super::{
        advance_recording_deadline, recording_capture_warranted, recording_idle_capture_due,
    };
    use std::time::{Duration, Instant};

    const THIRTY_FPS: Duration = Duration::from_nanos(33_333_333);
    const HEARTBEAT: Duration = Duration::from_millis(500);

    #[test]
    fn an_unchanged_screen_is_captured_only_on_the_heartbeat() {
        // The whole point: a still desktop must not drive 30 full-screen
        // recomposites a second to hand the encoder 30 identical frames.
        assert!(!recording_capture_warranted(true, false, false));
        assert!(recording_capture_warranted(true, true, false));
    }

    #[test]
    fn a_moving_cursor_alone_warrants_a_capture() {
        // The cursor is a server sprite drawn in after compositing, so it moves
        // without producing any damage the render gate would otherwise see.
        assert!(recording_capture_warranted(true, false, true));
    }

    #[test]
    fn nothing_is_captured_before_its_frame_is_due() {
        // Neither reason may outrun the configured frame rate.
        assert!(!recording_capture_warranted(false, true, true));
    }

    #[test]
    fn a_still_screen_still_owes_a_frame_within_the_heartbeat() {
        // Guards the file's duration: ffmpeg's constant-rate output only fills
        // up to the last frame it received, so an unbounded idle gap would end
        // the recording early.
        let last = Instant::now();
        assert!(!recording_idle_capture_due(
            Some(last),
            last + Duration::from_millis(100),
            HEARTBEAT
        ));
        assert!(recording_idle_capture_due(
            Some(last),
            last + Duration::from_millis(500),
            HEARTBEAT
        ));
        // The very first frame of a recording is always owed.
        assert!(recording_idle_capture_due(None, Instant::now(), HEARTBEAT));
    }

    #[test]
    fn the_first_capture_anchors_on_the_present() {
        let now = Instant::now();
        assert_eq!(advance_recording_deadline(None, THIRTY_FPS, now), now);
    }

    #[test]
    fn a_late_capture_still_advances_by_one_whole_interval() {
        // The frame took 40 ms on a 33.3 ms budget. Restarting the clock at
        // `now` would push every later capture 6.7 ms further out, which is how
        // a 30 fps recording degraded toward a 20 fps sampling cadence.
        let last = Instant::now();
        let now = last + Duration::from_millis(40);
        assert_eq!(
            advance_recording_deadline(Some(last), THIRTY_FPS, now),
            last + THIRTY_FPS
        );
    }

    #[test]
    fn falling_far_behind_resynchronizes_instead_of_bursting() {
        // Half a second of stall is 15 missed frames; catching up would submit
        // them back to back and flood the encoder we are trying to protect.
        let last = Instant::now();
        let now = last + Duration::from_millis(500);
        assert_eq!(advance_recording_deadline(Some(last), THIRTY_FPS, now), now);
    }
}

#[cfg(test)]
mod minimized_recapture_tests {
    use super::{
        PreviewSourcePlan, StaticMinimizedCapturePlan, durable_minimized_scene_owner,
        minimized_preview_source_is_drawable, overview_request_allowed, preview_source_plan,
        scene_window_is_tracked, static_minimized_capture_plan,
    };

    fn service_preview_renewal(
        cached: &mut bool,
        pending: &mut bool,
        import_attempts: &mut usize,
        import_succeeds: bool,
    ) -> StaticMinimizedCapturePlan {
        let plan =
            static_minimized_capture_plan(true, true, *cached, false, false, *pending, false);
        if plan != StaticMinimizedCapturePlan::Ignore {
            *pending = true;
            *import_attempts += 1;
            if import_succeeds {
                *cached = true;
                *pending = false;
            }
        }
        plan
    }

    #[test]
    fn failed_first_import_retries_on_renewal_then_stops_after_capture() {
        // Eviction leaves the authoritative minimized lifecycle and Dock
        // addressability intact, but no cached/animated source. The first
        // hover/ensure request therefore arms a static import.
        let mut cached = false;
        let mut pending = false;
        let mut import_attempts = 0;
        assert_eq!(
            service_preview_renewal(&mut cached, &mut pending, &mut import_attempts, false,),
            StaticMinimizedCapturePlan::ArmAndImport,
        );
        assert!(pending);
        assert!(!cached);
        // Model a failed get_geometry/add_window: pending remains but there is
        // still no cached source. The next lease renewal must retry rather
        // than leaving the preview permanently blank.
        assert_eq!(
            service_preview_renewal(&mut cached, &mut pending, &mut import_attempts, true,),
            StaticMinimizedCapturePlan::RetryImport,
        );
        assert!(!pending);
        assert!(cached);
        // Successful static settlement installs the cache and clears pending;
        // every later renewal is import-free.
        assert_eq!(
            service_preview_renewal(&mut cached, &mut pending, &mut import_attempts, true,),
            StaticMinimizedCapturePlan::Ignore,
        );
        assert_eq!(import_attempts, 2);
        assert_eq!(
            static_minimized_capture_plan(true, true, false, true, false, false, false),
            StaticMinimizedCapturePlan::Ignore,
            "an active Genie owns the pixels and must never be duplicated"
        );
        assert_eq!(
            static_minimized_capture_plan(false, true, false, false, false, false, false),
            StaticMinimizedCapturePlan::Ignore
        );
        assert_eq!(
            static_minimized_capture_plan(true, false, false, false, false, false, false),
            StaticMinimizedCapturePlan::Ignore
        );
        assert_eq!(
            static_minimized_capture_plan(true, true, false, false, true, false, false),
            StaticMinimizedCapturePlan::Ignore
        );
    }

    #[test]
    fn genie_and_settled_minimize_own_scene_xid_until_restore() {
        assert!(!scene_window_is_tracked(false, false, false));
        assert!(scene_window_is_tracked(true, false, false));
        assert!(scene_window_is_tracked(false, true, false));
        assert!(scene_window_is_tracked(false, false, true));
        assert!(scene_window_is_tracked(true, true, true));

        assert!(durable_minimized_scene_owner(true, false));
        assert!(!durable_minimized_scene_owner(true, true));
        assert!(!durable_minimized_scene_owner(false, false));
    }

    #[test]
    fn disabled_overview_blocks_entry_but_never_blocks_exit() {
        assert!(overview_request_allowed(true, true));
        assert!(overview_request_allowed(true, false));
        assert!(!overview_request_allowed(false, true));
        assert!(overview_request_allowed(false, false));
    }

    #[test]
    fn explicit_iconic_demand_recaptures_even_with_a_full_retained_owner() {
        assert_eq!(
            static_minimized_capture_plan(true, true, true, false, false, false, true),
            StaticMinimizedCapturePlan::RecaptureRetained
        );
        assert_eq!(
            static_minimized_capture_plan(true, true, false, true, false, false, true),
            StaticMinimizedCapturePlan::RecaptureRetained
        );
        assert_eq!(
            static_minimized_capture_plan(true, true, true, false, false, false, false),
            StaticMinimizedCapturePlan::Ignore,
            "retained pixels alone must not cause per-frame CPU readback"
        );
    }

    #[test]
    fn low_resolution_hover_draws_now_while_requesting_full_pixels() {
        assert_eq!(
            preview_source_plan(true, false),
            PreviewSourcePlan {
                awaiting_source: false,
                request_full_source: true,
            }
        );
        assert_eq!(
            preview_source_plan(false, false),
            PreviewSourcePlan {
                awaiting_source: true,
                request_full_source: true,
            }
        );
        assert_eq!(
            preview_source_plan(true, true),
            PreviewSourcePlan {
                awaiting_source: false,
                request_full_source: false,
            }
        );
    }

    #[test]
    fn cpu_only_snapshot_is_not_a_drawable_preview_source() {
        assert!(!minimized_preview_source_is_drawable(false, false, true));
        assert!(minimized_preview_source_is_drawable(false, true, true));
        assert!(minimized_preview_source_is_drawable(true, false, true));
    }
}
