//! Touchpad policy keeps visual previews separate from committed WM state.

use crate::backend::api::Backend;
use crate::backend::compositor_common::transitions::WORKSPACE_SWIPE_SETTLE;
use crate::backend::gesture_swipe::{GestureSwipeTracker, SwipeAxis, SwipeInput};
use crate::config::{ArgumentConfig, CONFIG};
use crate::core::models::{ClientKey, MonitorKey};
use crate::jwm::Jwm;
use std::time::Instant;

type SceneEntry = (u64, i32, i32, u32, u32);

#[derive(Debug)]
struct SwipeSession {
    id: u64,
    monitor: MonitorKey,
    client: Option<ClientKey>,
    source_tags: u32,
    config_signature: String,
    tracker: GestureSwipeTracker,
}

#[derive(Debug)]
struct WorkspacePreview {
    monitor: MonitorKey,
    target_tags: u32,
    windows: Vec<SceneEntry>,
    expires: Option<Instant>,
}

#[derive(Debug, Default)]
pub struct TouchpadGestures {
    active: Option<SwipeSession>,
    preview: Option<WorkspacePreview>,
    cancelled_through: u64,
}

impl Jwm {
    pub(crate) fn touchpad_modal_active(&self) -> bool {
        self.features.system_ui.is_active()
            || self.features.screenshot.active
            || self.features.recording.selecting_region
            || self.features.overview.active
            || self.features.expose_active
            || self.features.annotation_active
    }

    fn swipe_owner_valid(&self, session: &SwipeSession) -> bool {
        self.state.sel_mon == Some(session.monitor)
            && self
                .state
                .monitors
                .get(session.monitor)
                .is_some_and(|m| m.get_active_tags() == session.source_tags)
            && self.get_selected_client_key() == session.client
            && session
                .client
                .is_none_or(|key| self.state.clients.contains_key(key))
            && CONFIG.load().gesture_swipe_signature() == session.config_signature
    }

    /// Invalidating input never gets replayed into the new owner. Ordinary
    /// release cancellation may spring back; modal/lock/hotplug cancellation
    /// immediately restores the live scene before the new owner is drawn.
    pub(crate) fn cancel_touchpad_gesture(&mut self, backend: &mut dyn Backend, animate: bool) {
        if !animate {
            self.features.touchpad.cancelled_through =
                crate::backend::gesture_swipe::latest_swipe_id();
        }
        if self.features.touchpad.active.is_none()
            && self.features.touchpad.preview.is_none()
            && !backend.compositor_workspace_swipe_active()
        {
            return;
        }
        self.features.touchpad.active = None;
        if let Some(preview) = &mut self.features.touchpad.preview {
            if animate {
                preview.expires = Some(Instant::now() + WORKSPACE_SWIPE_SETTLE);
            } else {
                self.features.touchpad.preview = None;
            }
        }
        backend.compositor_end_workspace_swipe(false, animate);
        backend.request_render();
    }

    pub(crate) fn tick_touchpad_gesture(&mut self, backend: &mut dyn Backend) {
        let invalid = self.touchpad_modal_active()
            || backend.compositor_session_locked()
            || self
                .features
                .touchpad
                .active
                .as_ref()
                .is_some_and(|s| !self.swipe_owner_valid(s))
            || (self.features.touchpad.preview.is_some()
                && !backend.compositor_workspace_swipe_active());
        if invalid
            && (self.features.touchpad.active.is_some() || self.features.touchpad.preview.is_some())
        {
            self.cancel_touchpad_gesture(backend, false);
        } else if self
            .features
            .touchpad
            .preview
            .as_ref()
            .is_some_and(|p| p.expires.is_some_and(|end| Instant::now() >= end))
        {
            self.features.touchpad.preview = None;
            backend.request_render();
        }
    }

    pub(crate) fn handle_touchpad_input(&mut self, backend: &mut dyn Backend, event: SwipeInput) {
        match event {
            SwipeInput::Begin {
                id,
                fingers,
                config_signature,
            } => {
                if id <= self.features.touchpad.cancelled_through
                    || self.features.touchpad.active.is_some()
                {
                    return;
                }
                // A fresh gesture can interrupt the previous spring without
                // treating this already-queued Begin as cancelled input.
                if self.features.touchpad.preview.take().is_some()
                    || backend.compositor_workspace_swipe_active()
                {
                    backend.compositor_end_workspace_swipe(false, false);
                }
                if self.touchpad_modal_active()
                    || backend.compositor_session_locked()
                    || config_signature != CONFIG.load().gesture_swipe_signature()
                {
                    return;
                }
                let Some(monitor) = self.state.sel_mon else {
                    return;
                };
                let Some(mon) = self.state.monitors.get(monitor) else {
                    return;
                };
                let mut tracker = GestureSwipeTracker::default();
                tracker.begin(fingers, true);
                self.features.touchpad.active = Some(SwipeSession {
                    id,
                    monitor,
                    client: self.get_selected_client_key(),
                    source_tags: mon.get_active_tags(),
                    config_signature,
                    tracker,
                });
            }
            SwipeInput::Update { id, dx, dy, time } => {
                let Some(mut session) = self.features.touchpad.active.take() else {
                    return;
                };
                if session.id != id {
                    self.features.touchpad.active = Some(session);
                    return;
                }
                if self.touchpad_modal_active()
                    || backend.compositor_session_locked()
                    || !self.swipe_owner_valid(&session)
                {
                    self.cancel_touchpad_gesture(backend, false);
                    return;
                }
                session.tracker.update_timed(dx, dy, time);
                let cfg = CONFIG.load();
                let mut preview_updated = false;
                if session.tracker.axis == Some(SwipeAxis::Horizontal)
                    && let Some(direction) = session.tracker.direction()
                    && let Some(binding) = cfg.behavior().gesture_swipe.iter().find(|b| {
                        b.fingers == session.tracker.fingers
                            && b.direction.eq_ignore_ascii_case(direction)
                    })
                    && matches!(binding.function.as_str(), "loopview" | "loop_view" | "loop")
                    && let ArgumentConfig::Int(step @ (-1 | 1)) = binding.argument
                {
                    let target = self.calculate_next_tag(step);
                    if target != session.source_tags && cfg.motion_enabled() {
                        if !backend.compositor_workspace_swipe_active()
                            && let Some(rect) = self.monitor_work_area(session.monitor)
                            && rect.w > 0
                            && rect.h > 0
                        {
                            backend.compositor_begin_workspace_swipe((
                                rect.x,
                                rect.y,
                                rect.w as u32,
                                rect.h as u32,
                            ));
                        }
                        if backend.compositor_workspace_swipe_active() {
                            if self
                                .features
                                .touchpad
                                .preview
                                .as_ref()
                                .is_none_or(|p| p.target_tags != target)
                            {
                                let windows = self.swipe_destination_scene(session.monitor, target);
                                self.features.touchpad.preview = Some(WorkspacePreview {
                                    monitor: session.monitor,
                                    target_tags: target,
                                    windows,
                                    expires: None,
                                });
                            }
                            let threshold = cfg.behavior().gesture_swipe_threshold;
                            let progress = (session.tracker.dx.abs() / (2.0 * threshold)) as f32;
                            backend.compositor_update_workspace_swipe(
                                progress,
                                if direction == "left" { 1 } else { -1 },
                            );
                            preview_updated = true;
                        }
                    }
                }
                if !preview_updated && self.features.touchpad.preview.is_some() {
                    // Reversing through the origin, or toward an unbound
                    // direction, returns the captured source to its resting
                    // position while keeping the sequence reversible.
                    backend.compositor_update_workspace_swipe(0.0, 1);
                }
                self.features.touchpad.active = Some(session);
            }
            SwipeInput::End {
                id,
                cancelled,
                time,
            } => {
                let Some(mut session) = self.features.touchpad.active.take() else {
                    return;
                };
                if session.id != id {
                    self.features.touchpad.active = Some(session);
                    return;
                }
                let safe = !self.touchpad_modal_active()
                    && !backend.compositor_session_locked()
                    && self.swipe_owner_valid(&session);
                let cfg = CONFIG.load();
                let action = session
                    .tracker
                    .finish_timed(
                        cancelled || !safe,
                        cfg.behavior().gesture_swipe_threshold,
                        time,
                    )
                    .filter(|(fingers, direction)| {
                        cfg.behavior().gesture_swipe.iter().any(|binding| {
                            binding.fingers == *fingers
                                && binding.direction.eq_ignore_ascii_case(direction)
                        })
                    });
                if let Some((fingers, direction)) = action {
                    let preview_command = cfg
                        .behavior()
                        .gesture_swipe
                        .iter()
                        .find(|binding| {
                            binding.fingers == fingers
                                && binding.direction.eq_ignore_ascii_case(direction)
                        })
                        .is_some_and(|binding| {
                            matches!(binding.function.as_str(), "loopview" | "loop_view" | "loop")
                                && matches!(binding.argument, ArgumentConfig::Int(-1 | 1))
                        });
                    if !preview_command {
                        self.features.touchpad.preview = None;
                        backend.compositor_end_workspace_swipe(false, false);
                    }
                    // The stable owner is still current. view/minimize/custom
                    // dispatch is invoked exactly once and retains all normal
                    // IPC, EWMH, per-tag layout and focus side effects.
                    self.handle_gesture_swipe(backend, fingers, direction);
                    self.features.touchpad.preview = None;
                    backend.compositor_end_workspace_swipe(true, true);
                } else {
                    self.cancel_touchpad_gesture(backend, safe);
                }
            }
        }
    }

    fn swipe_destination_scene(&self, monitor: MonitorKey, tags: u32) -> Vec<SceneEntry> {
        let desktop_left = self.desktop_left_edge();
        let fallback_x = self
            .monitor_work_area(monitor)
            .map_or(desktop_left, |r| r.x);
        self.monitor_stacking_order(monitor, |key| {
            self.state
                .clients
                .get(key)
                .is_some_and(|client| client.state.is_sticky || client.state.tags & tags != 0)
        })
        .into_iter()
        .filter_map(|window| {
            let key = self.state.win_to_client.get(&window)?;
            let client = self.state.clients.get(*key)?;
            if client.mon != Some(monitor)
                || client.state.is_hidden
                || client.state.is_swallowed
                || client.state.is_dock
                || (!client.state.is_sticky && client.state.tags & tags == 0)
            {
                return None;
            }
            // Hidden tags keep mapped buffers, but their real geometry is
            // parked off-screen. Restore only this visual copy; consuming the
            // client's semantic restore slot here would change input/layout.
            let mut g = client.geometry.clone();
            crate::jwm::visibility::restore_hidden_geometry(&mut g, desktop_left, fallback_x);
            (g.w > 0 && g.h > 0).then_some((client.win.raw(), g.x, g.y, g.w as u32, g.h as u32))
        })
        .collect()
    }

    /// Replace only visual scene entries. Window mapping, tags, geometry,
    /// focus, input hit-testing and client protocols stay on the source tag.
    pub(crate) fn apply_touchpad_preview_scene(&self, scene: &mut Vec<SceneEntry>) {
        let Some(preview) = &self.features.touchpad.preview else {
            return;
        };
        if preview.expires.is_some_and(|end| Instant::now() >= end) {
            return;
        }
        let belongs = |id: u64| {
            self.state
                .win_to_client
                .get(&crate::backend::common_define::WindowId::from_raw(id))
                .and_then(|key| self.state.clients.get(*key))
                .is_some_and(|c| c.mon == Some(preview.monitor) && !c.state.is_dock)
        };
        let insertion = scene.iter().position(|entry| belongs(entry.0)).unwrap_or(0);
        scene.retain(|entry| !belongs(entry.0));
        scene.splice(
            insertion.min(scene.len())..insertion.min(scene.len()),
            preview.windows.iter().copied(),
        );
    }
}
