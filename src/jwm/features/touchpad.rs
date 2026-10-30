//! Touchpad policy keeps visual previews separate from committed WM state.

use crate::backend::api::{Backend, ExposeNavDirection};
use crate::backend::common_define::Mods;
use crate::backend::compositor_common::transitions::WORKSPACE_SWIPE_SETTLE;
use crate::backend::gesture_swipe::{GestureSwipeTracker, SwipeAxis, SwipeInput};
use crate::config::{ArgumentConfig, CONFIG};
use crate::core::models::{ClientKey, MonitorKey};
use crate::jwm::Jwm;
use crate::jwm::features::system_ui::{ListRow, RowData, SystemUiState};
use crate::jwm::features::{expose_plan, switcher};
use crate::jwm::types::WMArgEnum;
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
    expose: bool,
    switcher_failed: bool,
}

#[derive(Debug)]
struct SwitcherPreview {
    id: u64,
    windows: Vec<ClientKey>,
    origin: usize,
    selected: usize,
}

#[derive(Debug)]
struct DesktopWindow {
    client: ClientKey,
    tags: u32,
}

#[derive(Debug)]
struct DesktopSnapshot {
    monitor: MonitorKey,
    tags: u32,
    windows: Vec<DesktopWindow>,
    focused: Option<ClientKey>,
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
    switcher: Option<SwitcherPreview>,
    desktops: Vec<DesktopSnapshot>,
    cancelled_through: u64,
}

impl TouchpadGestures {
    pub(crate) fn detach_switcher(&mut self) {
        self.switcher = None;
    }

    /// A manual restore ends this snapshot's ownership of the window, even
    /// when it is minimized again before the next frame tick.
    pub(crate) fn forget_desktop_window(&mut self, client: ClientKey) {
        for desktop in &mut self.desktops {
            desktop.windows.retain(|window| window.client != client);
        }
        self.desktops.retain(|desktop| !desktop.windows.is_empty());
    }
}

impl Jwm {
    pub(crate) fn touchpad_switcher_active(&self) -> bool {
        self.features.touchpad.switcher.is_some() && self.features.system_ui.is_window_switcher()
    }

    pub(crate) fn touchpad_switcher_id(&self) -> Option<u64> {
        self.features
            .touchpad
            .switcher
            .as_ref()
            .map(|preview| preview.id)
    }

    pub(crate) fn showing_desktop(&self) -> bool {
        self.state.sel_mon.is_some_and(|monitor| {
            self.state.monitors.get(monitor).is_some_and(|mon| {
                self.features.touchpad.desktops.iter().any(|desktop| {
                    desktop.monitor == monitor && desktop.tags == mon.get_active_tags()
                })
            })
        })
    }

    /// Hide only this view's visible applications, preserving the windows
    /// already minimized. Returning restores only our still-owned clients.
    pub fn show_desktop(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.touchpad_modal_active() || backend.compositor_session_locked() {
            return Ok(());
        }
        let Some(monitor) = self.state.sel_mon else {
            return Ok(());
        };
        let Some(mon) = self.state.monitors.get(monitor) else {
            return Ok(());
        };
        let tags = mon.get_active_tags();
        if let Some(index) = self
            .features
            .touchpad
            .desktops
            .iter()
            .position(|desktop| desktop.monitor == monitor && desktop.tags == tags)
        {
            let mut desktop = self.features.touchpad.desktops.remove(index);
            let mut restored = Vec::new();
            let mut failed = Vec::new();
            for window in desktop.windows {
                let eligible = self.state.clients.get(window.client).is_some_and(|client| {
                    client.mon == Some(monitor)
                        && client.state.tags == window.tags
                        && client.state.is_hidden
                        && !client.state.is_swallowed
                        && (client.state.is_sticky || client.state.tags & tags != 0)
                });
                if !eligible {
                    continue;
                }
                match self.set_client_minimized(backend, window.client, false) {
                    Ok(_) => restored.push(window.client),
                    Err(error) => {
                        log::warn!("[show_desktop] could not restore window: {error}");
                        failed.push(window);
                    }
                }
            }
            let focus = desktop.focused.filter(|key| restored.contains(key));
            if !failed.is_empty() {
                desktop.windows = failed;
                self.features.touchpad.desktops.push(desktop);
            }
            if let Some(focus) = focus {
                self.focus(backend, Some(focus))?;
            }
        } else {
            let focused = self.get_selected_client_key();
            let windows: Vec<_> = self
                .state
                .clients
                .iter()
                .filter_map(|(key, client)| {
                    (client.mon == Some(monitor)
                        && !client.state.is_dock
                        && !client.state.is_desktop
                        && !client.state.is_swallowed
                        && self.is_client_visible_by_key(key))
                    .then_some(DesktopWindow {
                        client: key,
                        tags: client.state.tags,
                    })
                })
                .collect();
            let mut hidden = Vec::new();
            for window in windows {
                let result = self.set_client_minimized(backend, window.client, true);
                if self
                    .state
                    .clients
                    .get(window.client)
                    .is_some_and(|client| client.state.is_hidden)
                {
                    hidden.push(window);
                }
                if let Err(error) = result {
                    log::warn!("[show_desktop] could not minimize window: {error}");
                }
            }
            if !hidden.is_empty() {
                self.features.touchpad.desktops.push(DesktopSnapshot {
                    monitor,
                    tags,
                    windows: hidden,
                    focused,
                });
            }
        }
        self.suppress_mouse_focus_until =
            Some(Instant::now() + std::time::Duration::from_millis(300));
        let _ = self.restack(backend, Some(monitor));
        self.broadcast_monitor_bar_ipc(backend, monitor);
        Ok(())
    }

    pub(crate) fn touchpad_modal_active(&self) -> bool {
        self.features.system_ui.is_active()
            || self.features.screenshot.active
            || self.features.recording.selecting_region
            || self.features.overview.active
            || self.features.expose_active
            || self.features.annotation_active
    }

    fn touchpad_session_blocked(&self, expose: bool) -> bool {
        (self.features.system_ui.is_active() && !self.touchpad_switcher_active())
            || self.features.screenshot.active
            || self.features.recording.selecting_region
            || self.features.overview.active
            || (self.features.expose_active && !expose)
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
            && self.features.expose_active == session.expose
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
            && self.features.touchpad.switcher.is_none()
            && !backend.compositor_workspace_swipe_active()
        {
            return;
        }
        self.features.touchpad.active = None;
        if self.features.touchpad.switcher.take().is_some()
            && self.features.system_ui.is_window_switcher()
        {
            self.cancel_window_switcher(backend);
        }
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
        self.features.touchpad.desktops.retain_mut(|desktop| {
            desktop.windows.retain(|window| {
                self.state.clients.get(window.client).is_some_and(|client| {
                    client.mon == Some(desktop.monitor)
                        && client.state.tags == window.tags
                        && client.state.is_hidden
                })
            });
            self.state.monitors.contains_key(desktop.monitor) && !desktop.windows.is_empty()
        });
        let expose = self
            .features
            .touchpad
            .active
            .as_ref()
            .is_some_and(|session| session.expose);
        let invalid = self.touchpad_session_blocked(expose)
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
                let expose = self.features.expose_active && fingers == 4;
                if self.touchpad_session_blocked(expose)
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
                    expose,
                    switcher_failed: false,
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
                if self.touchpad_session_blocked(session.expose)
                    || backend.compositor_session_locked()
                    || !self.swipe_owner_valid(&session)
                {
                    self.features.touchpad.active = Some(session);
                    self.cancel_touchpad_gesture(backend, false);
                    return;
                }
                session.tracker.update_timed(dx, dy, time);
                let cfg = CONFIG.load();
                let mut preview_updated = false;
                let switcher_direction = (session.tracker.axis == Some(SwipeAxis::Horizontal))
                    .then(|| session.tracker.direction())
                    .flatten()
                    .and_then(|direction| {
                        cfg.behavior().gesture_swipe.iter().find(|binding| {
                            binding.fingers == session.tracker.fingers
                                && binding.direction.eq_ignore_ascii_case(direction)
                        })
                    })
                    .and_then(|binding| match (&*binding.function, &binding.argument) {
                        ("window_switcher" | "switcher", ArgumentConfig::Int(step @ (-1 | 1))) => {
                            Some(*step)
                        }
                        _ => None,
                    });
                if self.features.touchpad.switcher.is_some() && switcher_direction.is_none() {
                    if session.tracker.direction().is_none() {
                        let _ = self.update_touchpad_switcher(
                            backend,
                            session.id,
                            session.client,
                            1,
                            0.0,
                            cfg.behavior().gesture_swipe_threshold,
                        );
                    } else {
                        self.finish_touchpad_switcher(backend, false);
                    }
                }
                if !session.expose
                    && !session.switcher_failed
                    && let Some(step) = switcher_direction
                    && let Err(error) = self.update_touchpad_switcher(
                        backend,
                        session.id,
                        session.client,
                        step,
                        session.tracker.dx.abs(),
                        cfg.behavior().gesture_swipe_threshold,
                    )
                {
                    log::warn!("[gesture] window preview: {error}");
                    session.switcher_failed = true;
                }
                if !session.expose
                    && session.tracker.axis == Some(SwipeAxis::Horizontal)
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
                let safe = !self.touchpad_session_blocked(session.expose)
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
                    if session.expose {
                        self.handle_expose_touchpad_swipe(backend, fingers, direction);
                        return;
                    }
                    if self.features.touchpad.switcher.is_some() {
                        let window_action = cfg
                            .behavior()
                            .gesture_swipe
                            .iter()
                            .find(|binding| {
                                binding.fingers == fingers
                                    && binding.direction.eq_ignore_ascii_case(direction)
                            })
                            .is_some_and(|binding| {
                                matches!(binding.function.as_str(), "window_switcher" | "switcher")
                            });
                        self.finish_touchpad_switcher(
                            backend,
                            safe && !session.switcher_failed && window_action,
                        );
                        return;
                    }
                    if session.switcher_failed {
                        self.cancel_touchpad_gesture(backend, false);
                        return;
                    }
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

    fn update_touchpad_switcher(
        &mut self,
        backend: &mut dyn Backend,
        id: u64,
        focused: Option<ClientKey>,
        direction: i32,
        distance: f64,
        threshold: f64,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.features.touchpad.switcher.is_none() {
            if !backend.has_compositor() {
                return Ok(());
            }
            let entries = self.window_switcher_snapshot();
            let windows: Vec<_> = entries
                .iter()
                .filter_map(|entry| {
                    self.wintoclient(crate::backend::common_define::WindowId::from_raw(
                        entry.window,
                    ))
                })
                .collect();
            if windows.is_empty() || windows.len() != entries.len() {
                return Ok(());
            }
            let origin = focused
                .and_then(|key| windows.iter().position(|&window| window == key))
                .unwrap_or(if direction > 0 { windows.len() - 1 } else { 0 });
            // Replacing the pointer grab or invoking the ordinary modal
            // handoff cancels the native XI/libinput swipe. This preview is
            // owned by that sequence; only take the keyboard for Escape.
            self.cancel_pointer_drag(backend);
            if let Some(root) = backend.root_window() {
                backend.key_ops().grab_keyboard(root)?;
            }
            self.features.window_switcher_mods = Mods::empty();
            let rows = entries
                .iter()
                .map(|entry| ListRow {
                    key: entry.window.to_string(),
                    text: switcher::switcher_row(entry),
                    data: RowData::WindowSwitcher {
                        window: entry.window,
                    },
                })
                .collect();
            let icons = entries.iter().map(switcher::switcher_row_icon).collect();
            self.features.system_ui =
                SystemUiState::window_switcher_with_icons(rows, icons, origin);
            self.features.touchpad.switcher = Some(SwitcherPreview {
                id,
                windows,
                origin,
                selected: origin,
            });
        }
        let preview = self.features.touchpad.switcher.as_mut().unwrap();
        // Never wrap back onto the original window after a very long swipe.
        // Returning near the origin restores its highlight and cancels at End.
        let steps = if distance < 8.0 {
            0
        } else {
            ((distance / threshold).floor() as usize)
                .max(1)
                .min(preview.windows.len() - 1)
        };
        let selected = (preview.origin as isize + direction as isize * steps as isize)
            .rem_euclid(preview.windows.len() as isize) as usize;
        preview.selected = selected;
        if let SystemUiState::ListPanel {
            selected: current, ..
        } = &mut self.features.system_ui
        {
            *current = selected;
        }
        self.sync_system_ui(backend);
        Ok(())
    }

    fn finish_touchpad_switcher(&mut self, backend: &mut dyn Backend, commit: bool) {
        let Some(preview) = self.features.touchpad.switcher.take() else {
            return;
        };
        if !self.features.system_ui.is_window_switcher() {
            return;
        }
        let valid = preview
            .windows
            .get(preview.selected)
            .and_then(|&key| self.state.clients.get(key))
            .is_some_and(|client| {
                self.features.system_ui.selected_switcher_window() == Some(client.win.raw())
            });
        if commit && valid {
            if let Err(error) = self.commit_window_switcher(backend) {
                log::warn!("[gesture] could not switch window: {error}");
                self.cancel_window_switcher(backend);
            }
        } else {
            self.cancel_window_switcher(backend);
        }
    }

    fn handle_expose_touchpad_swipe(
        &mut self,
        backend: &mut dyn Backend,
        fingers: u32,
        direction: &str,
    ) {
        let cfg = CONFIG.load();
        let Some(binding) = cfg.behavior().gesture_swipe.iter().find(|binding| {
            binding.fingers == fingers && binding.direction.eq_ignore_ascii_case(direction)
        }) else {
            return;
        };
        let action = match binding.function.as_str() {
            "window_switcher" | "switcher" => {
                if let ArgumentConfig::Int(step @ (-1 | 1)) = binding.argument {
                    backend.compositor_expose_move(if step < 0 {
                        ExposeNavDirection::Left
                    } else {
                        ExposeNavDirection::Right
                    });
                }
                return;
            }
            "toggle_expose" | "expose" => expose_plan::plan_escape(),
            "show_desktop" => expose_plan::plan_click(backend.compositor_expose_selected()),
            _ => return,
        };
        if let Err(error) = self.apply_expose_action(backend, action) {
            log::warn!("[gesture] expose: {error}");
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
