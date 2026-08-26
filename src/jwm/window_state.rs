use crate::backend::api::Backend;
use crate::backend::api::{
    Geometry, MAX_MINIMIZED_RESTORE_ORDER, MinimizedRestoreRect, MinimizedRestoreState, NetWmState,
    StackMode, WindowChanges, WindowType,
};
use crate::backend::common_define::{SchemeType, WindowId};
use crate::config::CONFIG;
use crate::core::models::{ClientKey, WMClient};
use crate::core::types::Rect;
use std::sync::atomic::{AtomicU64, Ordering};
use xbar_core::shared_structures::MAX_MINIMIZED_WINDOWS;

use super::Jwm;
use super::statusbar::StatusBarBuilder;
use super::types::wm_state_for_minimized;
use super::visibility::hidden_x_left_of_desktop;

static NEXT_MINIMIZED_ORDER: AtomicU64 = AtomicU64::new(1);
const EXHAUSTED_MINIMIZED_ORDER: u64 = MAX_MINIMIZED_RESTORE_ORDER + 1;
/// Restart properties live on client-owned X11 windows and therefore are
/// untrusted input.  A natural JWM sequence cannot plausibly reach 2^48, while
/// accepting an advertised `i64::MAX` would move the process-wide allocator
/// straight to its exhausted sentinel and let one client disable every future
/// minimize.  The wire codec remains forward-compatible up to its i64 bound;
/// adoption simply rebases implausibly large values onto a fresh local order.
pub(super) const MAX_RECOVERED_MINIMIZED_ORDER: u64 = (1_u64 << 48) - 1;

pub(super) const fn minimized_order_is_safe_to_recover(order: u64) -> bool {
    order >= 1 && order <= MAX_RECOVERED_MINIMIZED_ORDER
}

pub(super) const fn client_decoration_scheme(
    is_focused: bool,
    is_urgent: bool,
    attention_enabled: bool,
) -> SchemeType {
    if is_focused {
        SchemeType::Sel
    } else if is_urgent && attention_enabled {
        SchemeType::Urgent
    } else {
        SchemeType::Norm
    }
}

fn minimized_order_transition(current: u64) -> Option<(u64, u64)> {
    let allocated = current.max(1);
    if allocated < EXHAUSTED_MINIMIZED_ORDER {
        Some((allocated, allocated + 1))
    } else {
        None
    }
}

pub(super) fn next_minimized_order() -> Option<u64> {
    let mut current = NEXT_MINIMIZED_ORDER.load(Ordering::Relaxed);
    loop {
        let (allocated, next) = minimized_order_transition(current)?;
        match NEXT_MINIMIZED_ORDER.compare_exchange_weak(
            current,
            next,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return Some(allocated),
            Err(observed) => current = observed,
        }
    }
}

/// Advance the process-local allocator past a Dock order recovered from the
/// previous JWM process. Without this, the first post-restart minimize could
/// reuse an adopted order and make insertion ordering ambiguous.
pub(super) fn observe_minimized_order(order: u64) -> bool {
    if !minimized_order_is_safe_to_recover(order) {
        return false;
    }
    // `MAX + 1` is an explicit exhausted sentinel. It fits in u64 and makes
    // allocation fail closed instead of wrapping through zero and silently
    // reusing an existing Dock order.
    NEXT_MINIMIZED_ORDER.fetch_max(order + 1, Ordering::Relaxed);
    true
}

pub(super) fn x11_geometry_fully_left_of_desktop(geometry: Geometry, desktop_left: i32) -> bool {
    i64::from(geometry.x)
        .saturating_add(i64::from(geometry.w))
        .saturating_add(i64::from(geometry.border).saturating_mul(2))
        <= i64::from(desktop_left)
}

fn valid_restore_rect(x: i32, y: i32, w: i32, h: i32) -> Option<MinimizedRestoreRect> {
    (w > 0 && h > 0).then_some(MinimizedRestoreRect { x, y, w, h })
}

fn minimized_restore_snapshot(
    client: &WMClient,
    monitor_num: Option<i32>,
    minimized_order: u64,
) -> Option<MinimizedRestoreState> {
    if minimized_order == 0 {
        return None;
    }
    // A client can already be parked because its tag is not visible while it
    // is still semantically non-minimized. Persist the dedicated visible
    // restore slot in that case, never the real off-screen X11 coordinate.
    let visible = client.geometry.hidden_restore_rect.unwrap_or(Rect::new(
        client.geometry.x,
        client.geometry.y,
        client.geometry.w,
        client.geometry.h,
    ));
    let visible_rect = valid_restore_rect(visible.x, visible.y, visible.w, visible.h)?;
    let floating_rect = valid_restore_rect(
        client.geometry.floating_x,
        client.geometry.floating_y,
        client.geometry.floating_w,
        client.geometry.floating_h,
    );
    if client.state.is_pip && floating_rect.is_none() {
        return None;
    }
    let fullscreen_restore_rect = if client.state.is_fullscreen {
        Some(valid_restore_rect(
            client.geometry.old_x,
            client.geometry.old_y,
            client.geometry.old_w,
            client.geometry.old_h,
        )?)
    } else {
        None
    };

    Some(MinimizedRestoreState {
        tags: client.state.tags,
        monitor_num: monitor_num.unwrap_or(-1),
        visible_rect,
        is_floating: client.state.is_floating,
        is_drag_floating: client.state.is_drag_floating,
        floating_rect,
        is_pip: client.state.is_pip,
        pip_restore_sticky: client.state.pip_restore_sticky,
        old_state: client.state.old_state,
        fullscreen_restore_rect,
        minimized_order,
    })
}

fn wm_hint_urgency_policy(
    hinted_urgent: bool,
    is_focused: bool,
    do_not_disturb: bool,
) -> (bool, bool) {
    let suppress_hint = hinted_urgent && (is_focused || do_not_disturb);
    (hinted_urgent && !suppress_hint, suppress_hint)
}

impl Jwm {
    /// Reinstall the complete pre-transition mode state after an operation
    /// failed part-way through.  This deliberately does not call either mode
    /// setter: a backend failure may still be active, and recursively trying
    /// the inverse transition would otherwise leave the client in the neutral
    /// state between fullscreen and PiP.
    fn restore_failed_mode_transition(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        previous_client: &WMClient,
        previous_animation: Option<crate::core::animation::ClientAnimation>,
        previous_monitor_order: Option<Vec<ClientKey>>,
    ) {
        let monitor = previous_client.mon;
        let win = previous_client.win;

        let install_internal_snapshot = |jwm: &mut Jwm| {
            if let Some(client) = jwm.state.clients.get_mut(client_key) {
                *client = previous_client.clone();
            }
            if let (Some(monitor), Some(order)) = (monitor, previous_monitor_order.as_ref())
                && let Some(clients) = jwm.state.monitor_clients.get_mut(monitor)
            {
                *clients = order.clone();
            }
            if let Some(animation) = previous_animation.as_ref() {
                jwm.animations.active.insert(client_key, animation.clone());
            } else {
                jwm.animations.remove(client_key);
            }
        };

        // Leaving fullscreen can arrange the other tiled clients before the
        // following PiP operation fails.  Re-run the layout with the original
        // mode visible to the arranger, then reinstall the exact client and
        // animation snapshots (arrange is allowed to retarget both).
        install_internal_snapshot(self);
        self.arrange(backend, monitor);
        install_internal_snapshot(self);

        if let Err(error) = backend
            .property_ops()
            .set_fullscreen_state(win, previous_client.state.is_fullscreen)
        {
            log::warn!(
                "could not restore fullscreen property after failed cross-mode transition for {win:?}: {error}"
            );
        }
        if let Err(error) = backend.property_ops().set_net_wm_state_flag(
            win,
            NetWmState::Sticky,
            previous_client.state.is_sticky,
        ) {
            log::warn!(
                "could not restore Sticky after failed cross-mode transition for {win:?}: {error}"
            );
        }

        let x11_border = if backend.has_compositor() {
            0
        } else {
            previous_client.geometry.border_w.max(0) as u32
        };
        if let Err(error) = backend.window_ops().configure(
            win,
            previous_client.geometry.x,
            previous_client.geometry.y,
            previous_client.geometry.w.max(1) as u32,
            previous_client.geometry.h.max(1) as u32,
            x11_border,
        ) {
            log::warn!(
                "could not restore geometry after failed cross-mode transition for {win:?}: {error}"
            );
        }

        // Reconcile compositor state even when the real-window configure is
        // still failing.  For hidden PiP this restores the retained texture's
        // presentation while the exact parked geometry remains in JWM/V1.
        backend.compositor_set_window_pip(win, previous_client.state.is_pip);
        if let Err(error) = self.restack(backend, monitor) {
            log::warn!(
                "could not restore stacking after failed cross-mode transition for {win:?}: {error}"
            );
        }
        if previous_client.state.is_hidden
            && let Err(error) = self.persist_minimized_restore_state(backend, client_key)
        {
            log::warn!(
                "could not restore minimized mode snapshot after failed cross-mode transition for {win:?}: {error}"
            );
        }
    }

    /// Rewrite one hidden client's restart snapshot from JWM's current
    /// semantic state. This is used when adopting legacy Iconic clients and
    /// immediately before a seamless exec, so monitor/tag/restore-slot
    /// changes made since the original minimize cannot leave stale restart
    /// metadata behind.
    pub(super) fn persist_minimized_restore_state(
        &self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let Some(snapshot) = self.expected_minimized_restore_state(client_key) else {
            return Ok(false);
        };
        let win = self
            .state
            .clients
            .get(client_key)
            .map(|client| client.win)
            .ok_or("client disappeared while persisting minimized restore state")?;
        backend
            .property_ops()
            .set_minimized_restore_state(win, snapshot)?;
        Ok(true)
    }

    /// Build the exact private restart property expected for one currently
    /// hidden client. Keeping this derivation shared with the write path lets
    /// restart preflight compare a synchronous X11 readback with the semantic
    /// state that will otherwise disappear at exec.
    pub(super) fn expected_minimized_restore_state(
        &self,
        client_key: ClientKey,
    ) -> Option<MinimizedRestoreState> {
        let client = self.state.clients.get(client_key)?;
        if !client.state.is_hidden {
            return None;
        }
        let monitor_num = client
            .mon
            .and_then(|key| self.state.monitors.get(key))
            .map(|monitor| monitor.num);
        minimized_restore_snapshot(client, monitor_num, client.state.minimized_order)
    }

    /// Repair restart metadata for a client that is already semantically
    /// minimized without replaying its compositor transition.
    ///
    /// Legacy/incomplete in-process state can carry the pre-Dock order zero,
    /// and an earlier best-effort adoption write may have left the private V1
    /// property missing or stale. Commit the internal order first so any
    /// later backend failure cannot make repeated requests consume the global
    /// allocator forever, then converge the property only when it differs.
    fn reconcile_existing_minimized_snapshot(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some((win, monitor_num, is_hidden, order)) =
            self.state.clients.get(client_key).map(|client| {
                (
                    client.win,
                    client
                        .mon
                        .and_then(|key| self.state.monitors.get(key))
                        .map(|monitor| monitor.num),
                    client.state.is_hidden,
                    client.state.minimized_order,
                )
            })
        else {
            return Ok(());
        };
        if !is_hidden {
            return Ok(());
        }

        if order == 0 {
            let repaired = next_minimized_order().ok_or("minimized Dock order space exhausted")?;
            if let Some(client) = self.state.clients.get_mut(client_key) {
                client.state.minimized_order = repaired;
            }
            self.mark_bar_update_needed_if_visible(monitor_num);
        }

        let Some(desired) = self.state.clients.get(client_key).and_then(|client| {
            minimized_restore_snapshot(client, monitor_num, client.state.minimized_order)
        }) else {
            log::warn!(
                "could not derive minimized restore state while repairing existing client {win:?}"
            );
            return Ok(());
        };
        let current = match backend.property_ops().get_minimized_restore_state(win) {
            Ok(current) => current,
            Err(error) => {
                log::warn!(
                    "could not read minimized restore state while repairing {win:?}: {error}"
                );
                None
            }
        };
        if current != Some(desired)
            && let Err(error) = backend
                .property_ops()
                .set_minimized_restore_state(win, desired)
        {
            // The hidden state and its stable insertion order remain valid.
            // A later idempotent minimize/restore request retries this write.
            log::warn!("could not repair minimized restore state for {win:?}: {error}");
        }
        Ok(())
    }

    /// Put a restore back into its retryable minimized state after a later
    /// stage failed. Public state is changed before the real X11 unpark, and
    /// focus/restack happen after it, so every one of those stages must share
    /// the same rollback boundary or a repeated external restore can become a
    /// visible no-op that never focuses the requested window again.
    fn rollback_failed_minimized_restore(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        previous_minimized_order: u64,
        previous_urgent: bool,
        previous_selected_client: Option<ClientKey>,
        previous_selected_monitor: Option<crate::core::models::MonitorKey>,
        previous_target_selection: Option<ClientKey>,
        previous_monitor_stack: Option<&[ClientKey]>,
        repair_focus: bool,
    ) {
        let desktop_left = self.desktop_left_edge();
        let Some((win, monitor, monitor_num, retry_target)) =
            self.state.clients.get(client_key).map(|client| {
                let retry_target = client.geometry.hidden_restore_rect.unwrap_or(Rect::new(
                    client.geometry.x,
                    client.geometry.y,
                    client.geometry.w,
                    client.geometry.h,
                ));
                let monitor = client.mon;
                let monitor_num = monitor
                    .and_then(|key| self.state.monitors.get(key))
                    .map(|monitor| monitor.num);
                (client.win, monitor, monitor_num, retry_target)
            })
        else {
            return;
        };

        if let Some(client) = self.state.clients.get_mut(client_key) {
            client.state.is_hidden = true;
            client.state.minimized_order = previous_minimized_order;
            let total_width = retry_target
                .w
                .saturating_add(client.geometry.border_w.saturating_mul(2));
            let hidden_x = hidden_x_left_of_desktop(desktop_left, total_width);
            client.geometry.hidden_restore_rect = Some(retry_target);
            client.geometry.hidden_x = Some(hidden_x);
            client.geometry.x = hidden_x;
            client.geometry.y = retry_target.y;
            client.geometry.w = retry_target.w;
            client.geometry.h = retry_target.h;
        }
        if let Err(error) = self.persist_minimized_restore_state(backend, client_key) {
            log::warn!(
                "could not retain minimized restore state after failed restore for {win:?}: {error}"
            );
        }
        if let Err(error) =
            backend
                .property_ops()
                .set_net_wm_state_flag(win, NetWmState::Hidden, true)
        {
            log::warn!("could not roll back EWMH Hidden for {win:?}: {error}");
        }
        if let Err(error) =
            self.setclientstate(backend, win, i64::from(wm_state_for_minimized(true)))
        {
            log::warn!("could not roll back WM_STATE for {win:?}: {error}");
        }
        self.mark_bar_update_needed_if_visible(monitor_num);
        self.arrange(backend, monitor);

        if repair_focus {
            // `focus(Some)` mutates decoration, button grabs, urgency and the
            // logical focus stack before the backend focus request itself.
            // Undo those partial commits explicitly; the ordinary fallback
            // focus path cannot see a now-hidden target as the current
            // selection and therefore cannot unfocus it for us.
            if previous_urgent && let Err(error) = self.seturgent(backend, client_key, true) {
                log::warn!("could not restore urgency after failed restore for {win:?}: {error}");
            }
            if let Err(error) = self.update_client_decoration(backend, client_key, false) {
                log::warn!(
                    "could not restore inactive decoration after failed restore for {win:?}: {error}"
                );
            }
            self.grabbuttons(backend, client_key, false);
            if let (Some(monitor), Some(previous_stack)) = (monitor, previous_monitor_stack)
                && let Some(stack) = self.state.monitor_stack.get_mut(monitor)
            {
                *stack = previous_stack.to_vec();
            }
            if let Some(monitor) = monitor
                && let Some(monitor) = self.state.monitors.get_mut(monitor)
            {
                monitor.set_selected_client_for_current_tag(previous_target_selection);
            }
            self.state.sel_mon = previous_selected_monitor;
            let previous_selected_client = previous_selected_client
                .filter(|client_key| self.is_client_visible_by_key(*client_key));
            if let Err(error) = self.focus(backend, previous_selected_client) {
                log::warn!("could not repair focus after failed restore for {win:?}: {error}");
            }
            if let Err(error) = self.restack(backend, monitor) {
                log::warn!("could not repair stacking after failed restore for {win:?}: {error}");
            }
        }

        // A true-Iconic restore maps the client before changing JWM's hidden
        // state.  Once any later geometry/focus/restack stage rolls back, the
        // window is parked and owns the same durable snapshot again, so re-arm
        // the backend coordinator. Failure is safe: the client remains mapped
        // outside the desktop and a repeated restore/minimize can retry.
        if let Err(error) = self.request_iconify_for_hidden_dock_client(backend, client_key) {
            log::warn!("could not re-iconify {win:?} after failed minimized restore: {error}");
        }
    }

    /// Request physical ICCCM IconicState only after JWM has a hidden,
    /// addressable Dock client and the real X11 window is either already
    /// unmapped or safely parked outside the desktop. Non-X11 backends keep a
    /// no-op implementation of the compositor hook.
    pub(super) fn request_iconify_for_hidden_dock_client(
        &self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some((win, hidden, eligible)) = self.state.clients.get(client_key).map(|client| {
            (
                client.win,
                client.state.is_hidden,
                StatusBarBuilder::is_minimized_dock_eligible(client),
            )
        }) else {
            return Ok(());
        };
        if !hidden || !eligible {
            return Ok(());
        }
        self.verify_x11_minimized_client_parked(backend, client_key)?;
        backend.compositor_request_window_iconify(win)?;
        Ok(())
    }

    fn verify_x11_minimized_client_parked(
        &self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if !backend.capabilities().supports_client_list {
            return Ok(());
        }
        // Without a compositor, a configured Hide animation intentionally
        // moves the real X11 window over several ticks. It is not required to
        // be fully parked until that animation is consumed; once no Hide is
        // active, an idempotent minimize below verifies/repairs the endpoint.
        if !backend.has_compositor()
            && crate::config::CONFIG.load().animation_enabled()
            && self
                .animations
                .active
                .get(&client_key)
                .is_some_and(|animation| {
                    animation.kind == crate::core::animation::AnimationKind::Hide
                })
        {
            return Ok(());
        }
        let win = self
            .state
            .clients
            .get(client_key)
            .map(|client| client.win)
            .ok_or("client disappeared while verifying minimized geometry")?;
        // A successfully iconified client is genuinely unmapped. Its stored X
        // geometry need not remain left of a later topology, so map state is
        // authoritative before consulting the parked mapped fallback.
        if !backend
            .window_ops()
            .get_window_attributes(win)?
            .map_state_viewable
        {
            return Ok(());
        }
        let geometry = backend.window_ops().get_geometry(win)?;
        if x11_geometry_fully_left_of_desktop(geometry, self.desktop_left_edge()) {
            Ok(())
        } else {
            Err(format!("minimized window {win:?} remained inside the desktop").into())
        }
    }

    /// Retry only the real-window parking side effect for an already-hidden
    /// X11 client. This deliberately does not arrange or call the compositor:
    /// the semantic state, V1 snapshot, insertion order and Genie capture all
    /// belong to the original minimize incarnation.
    ///
    /// Configure even a physically unmapped true-Iconic window. Output
    /// hotplug can make its last server geometry overlap the new desktop, and
    /// MapWindow would otherwise expose that stale input surface before JWM
    /// gets another chance to park it.
    pub(super) fn retry_x11_minimized_client_park(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if !backend.capabilities().supports_client_list {
            return Ok(());
        }
        if !backend.has_compositor()
            && crate::config::CONFIG.load().animation_enabled()
            && self
                .animations
                .active
                .get(&client_key)
                .is_some_and(|animation| {
                    animation.kind == crate::core::animation::AnimationKind::Hide
                })
        {
            return Ok(());
        }
        let desktop_left = self.desktop_left_edge();
        let (win, y, w, h, border_w) = self
            .state
            .clients
            .get(client_key)
            .map(|client| {
                (
                    client.win,
                    client.geometry.y,
                    client.geometry.w,
                    client.geometry.h,
                    client.geometry.border_w,
                )
            })
            .ok_or("client disappeared while retrying minimized geometry")?;
        // Always derive parking from the current desktop topology. A hidden
        // coordinate that was safe before a new left-hand output appeared can
        // now overlap that output; reusing it would make every idempotent
        // minimize retry fail verification at the same stale coordinate.
        let total_width = w.saturating_add(border_w.saturating_mul(2));
        let hidden_x = hidden_x_left_of_desktop(desktop_left, total_width);
        if let Some(client) = self.state.clients.get_mut(client_key) {
            client.geometry.x = hidden_x;
            client.geometry.hidden_x = Some(hidden_x);
        }
        let x11_border = if backend.has_compositor() {
            0
        } else {
            border_w.max(0) as u32
        };
        backend.window_ops().configure(
            win,
            hidden_x,
            y,
            w.max(1) as u32,
            h.max(1) as u32,
            x11_border,
        )?;
        let geometry = backend.window_ops().get_geometry(win)?;
        if x11_geometry_fully_left_of_desktop(geometry, desktop_left) {
            Ok(())
        } else {
            Err(format!("minimized window {win:?} remained inside the desktop").into())
        }
    }

    /// Re-establish the real-window parking invariant before an active X11
    /// compositor is removed.  The backend's true-Iconic teardown maps every
    /// retained client, so a geometry left stale by an earlier hotplug failure
    /// would otherwise become visible for at least one frame.
    ///
    /// Keep this as a barrier: visit every hidden client so one bad window does
    /// not prevent the others from being repaired, but report failure if any
    /// repair or verification failed.  Callers must not disable the compositor
    /// unless this returns `Ok(())`.
    pub(super) fn park_hidden_clients_before_compositor_disable(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let hidden_clients: Vec<ClientKey> = self
            .state
            .clients
            .iter()
            .filter_map(|(client_key, client)| client.state.is_hidden.then_some(client_key))
            .collect();
        let mut failures = Vec::new();

        for client_key in hidden_clients {
            if let Err(error) = self.retry_x11_minimized_client_park(backend, client_key) {
                let window = self
                    .state
                    .clients
                    .get(client_key)
                    .map(|client| format!("{:?}", client.win))
                    .unwrap_or_else(|| "<removed>".to_string());
                failures.push(format!("{window}: {error}"));
            }
        }

        if failures.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "could not safely park {} hidden client(s): {}",
                failures.len(),
                failures.join("; ")
            )
            .into())
        }
    }

    /// Keep the bar projection and compositor texture lifecycle aligned when
    /// a hidden client's task-switcher eligibility changes in place (for
    /// example SKIP_TASKBAR or WINDOW_TYPE_DOCK). Re-entry is a static
    /// adoption: publish any known shelf first, then let the backend import
    /// pixels without replaying a Genie from JWM's hidden parking geometry.
    pub(super) fn reconcile_minimized_dock_eligibility(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        was_eligible: bool,
    ) {
        let Some((win, monitor, is_hidden, is_eligible)) =
            self.state.clients.get(client_key).map(|client| {
                (
                    client.win,
                    client.mon,
                    client.state.is_hidden,
                    StatusBarBuilder::is_minimized_dock_eligible(client),
                )
            })
        else {
            return;
        };
        if !is_hidden || (is_eligible && was_eligible) {
            return;
        }

        let monitor_num = monitor
            .and_then(|key| self.state.monitors.get(key))
            .map(|monitor| monitor.num);
        if is_eligible {
            let target = monitor_num
                .and_then(|monitor_num| self.minimized_dock_shelves.get(&monitor_num))
                .copied();
            backend.compositor_set_window_dock_geometry(win, target);
            backend.compositor_ensure_minimized_window_visual(win);
            if let Err(error) = self.request_iconify_for_hidden_dock_client(backend, client_key) {
                log::warn!("could not iconify newly Dock-eligible client {win:?}: {error}");
            }
        } else {
            // Withdraw every addressable Dock surface before attempting the
            // checked map.  Mapping may fail or its second policy-level
            // attributes query may be inconclusive, but an ineligible client
            // must never retain an interactive target/preview in the bar.
            backend.compositor_set_window_dock_geometry(win, None);
            if let Some((preview_monitor_num, preview_window)) = self.active_minimized_preview
                && preview_window == win
            {
                self.clear_minimized_preview_for(backend, preview_monitor_num, Some(win));
            }
            if let Err(error) = self.retry_x11_minimized_client_park(backend, client_key) {
                // Do not map or release the only retained pixels until the
                // real window's hidden server geometry is known safe. The
                // target stays withdrawn and the repeatable ineligible path
                // retries this parking barrier on the next reconciliation.
                log::warn!(
                    "could not safely park Iconic client {win:?} before Dock eligibility withdrawal: {error}"
                );
                self.mark_bar_update_needed_if_visible(monitor_num);
                return;
            }
            // A genuinely Iconic window has no server-side pixels once its
            // pinned snapshot is released. Map it first; if that fails, keep
            // the targetless pin/visual rather than manufacturing an
            // unrecoverable hidden client.  The ineligible path deliberately
            // remains repeatable even when `was_eligible` is already false,
            // so a later property reconciliation can finish this retirement.
            match backend.compositor_cancel_window_iconify(win) {
                Ok(()) => {
                    let mapped = !backend.capabilities().supports_client_list
                        || backend
                            .window_ops()
                            .get_window_attributes(win)
                            .is_ok_and(|attributes| attributes.map_state_viewable);
                    if mapped {
                        backend.compositor_forget_minimized_window_visual(win);
                    } else {
                        log::warn!(
                            "could not confirm mapped state before forgetting minimized visual for {win:?}"
                        );
                        // The client is now intentionally ineligible, so the
                        // ordinary Dock admission helper must not be used: its
                        // eligibility guard would turn this rollback into a
                        // false success. Re-arm true Iconic ownership by XID;
                        // the retained visual/pin stays intact until a later
                        // reconcile confirms the mapped fallback and forgets
                        // it.
                        if let Err(error) = backend.compositor_request_window_iconify(win) {
                            log::warn!(
                                "could not retain Iconic ownership for ineligible {win:?}: {error}"
                            );
                        }
                    }
                }
                Err(error) => {
                    log::warn!(
                        "could not map Iconic client {win:?} before Dock eligibility withdrawal: {error}"
                    );
                }
            }
        }
        self.mark_bar_update_needed_if_visible(monitor_num);
    }

    pub(super) fn update_client_decoration(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        is_focused: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (win, border_w, is_urgent) = if let Some(client) = self.state.clients.get(client_key) {
            (client.win, client.geometry.border_w, client.state.is_urgent)
        } else {
            return Err("Client not found".into());
        };

        let x11_bw = if backend.has_compositor() {
            0
        } else {
            border_w as u32
        };

        let scheme = client_decoration_scheme(
            is_focused,
            is_urgent,
            CONFIG.load().behavior().attention_animation,
        );
        if let Ok(pixel) = backend.color_allocator().get_border_pixel_of(scheme) {
            backend
                .window_ops()
                .set_decoration_style(win, x11_bw, pixel)?;
        }
        Ok(())
    }

    pub(super) fn setfullscreen(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        fullscreen: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let previous_client = self
            .state
            .clients
            .get(client_key)
            .cloned()
            .ok_or("Client not found")?;
        let previous_animation = self.animations.active.get(&client_key).cloned();
        let previous_monitor_order = previous_client
            .mon
            .and_then(|monitor| self.state.monitor_clients.get(monitor).cloned());

        if let Err(error) = self.setfullscreen_inner(backend, client_key, fullscreen) {
            self.restore_failed_mode_transition(
                backend,
                client_key,
                &previous_client,
                previous_animation,
                previous_monitor_order,
            );
            return Err(error);
        }
        Ok(())
    }

    fn setfullscreen_inner(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        fullscreen: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (win, mut is_fullscreen, is_pip) =
            if let Some(client) = self.state.clients.get(client_key) {
                (client.win, client.state.is_fullscreen, client.state.is_pip)
            } else {
                return Err("Client not found".into());
            };

        // PiP and fullscreen both temporarily own `old_state`, floating
        // geometry and compositor presentation. Entering fullscreen must
        // therefore finish a PiP restore first. The defensive first branch
        // also normalizes a client inherited from an older process that had
        // both bits set.
        if fullscreen && is_pip {
            if is_fullscreen {
                // The outer `setfullscreen` owns the complete pre-transition
                // snapshot, so stay inside the raw transition here and avoid
                // running two best-effort rollback passes for one failure.
                self.setfullscreen_inner(backend, client_key, false)?;
            }
            self.set_client_pip_inner(backend, client_key, false)?;
            is_fullscreen = false;
        }

        if fullscreen && !is_fullscreen {
            backend.property_ops().set_fullscreen_state(win, true)?;

            let hidden = self
                .state
                .clients
                .get(client_key)
                .is_some_and(|client| client.state.is_hidden);
            let fullscreen_rect = self
                .state
                .clients
                .get(client_key)
                .and_then(|client| client.mon)
                .and_then(|mon_key| self.state.monitors.get(mon_key))
                .map(|monitor| {
                    Rect::new(
                        monitor.geometry.m_x,
                        monitor.geometry.m_y,
                        monitor.geometry.m_w,
                        monitor.geometry.m_h,
                    )
                });
            let desktop_left = self.desktop_left_edge();
            if let Some(client) = self.state.clients.get_mut(client_key) {
                // `resizeclient` normally fills this fullscreen return slot
                // from the live rectangle. A minimized client's live x is its
                // parking coordinate, so use the semantic visible rectangle
                // instead. A restart snapshot may refine it after window-type
                // adoption.
                if hidden {
                    let visible = client.geometry.hidden_restore_rect.unwrap_or(Rect::new(
                        client.geometry.x,
                        client.geometry.y,
                        client.geometry.w,
                        client.geometry.h,
                    ));
                    client.geometry.old_x = visible.x;
                    client.geometry.old_y = visible.y;
                    client.geometry.old_w = visible.w;
                    client.geometry.old_h = visible.h;
                }
                client.state.is_fullscreen = true;
                client.state.old_state = client.state.is_floating;
                client.geometry.old_border_w = client.geometry.border_w;
                client.geometry.border_w = 0;
                client.state.is_floating = true;
            }
            self.reorder_client_in_monitor_groups(client_key);
            if let Some(target) = fullscreen_rect {
                if hidden {
                    let hidden_x = hidden_x_left_of_desktop(desktop_left, target.w);
                    if let Some(client) = self.state.clients.get_mut(client_key) {
                        client.geometry.hidden_restore_rect = Some(target);
                        client.geometry.hidden_x = Some(hidden_x);
                        client.geometry.x = hidden_x;
                        client.geometry.y = target.y;
                        client.geometry.w = target.w;
                        client.geometry.h = target.h;
                    }
                    // Keep the real/input window fully outside every output.
                    // In particular, never route initial IconicState adoption
                    // through `resizeclient`, whose target is on-screen.
                    backend.window_ops().configure(
                        win,
                        hidden_x,
                        target.y,
                        target.w.max(1) as u32,
                        target.h.max(1) as u32,
                        0,
                    )?;
                } else {
                    self.resizeclient(backend, client_key, target.x, target.y, target.w, target.h)?;
                }
            }
            let changes = WindowChanges {
                stack_mode: Some(StackMode::Above),
                ..Default::default()
            };
            backend.window_ops().apply_window_changes(win, changes)?;
        } else if !fullscreen && is_fullscreen {
            backend.property_ops().set_fullscreen_state(win, false)?;

            let hidden = self
                .state
                .clients
                .get(client_key)
                .is_some_and(|client| client.state.is_hidden);
            let desktop_left = self.desktop_left_edge();
            if let Some(client) = self.state.clients.get_mut(client_key) {
                client.state.is_fullscreen = false;
                client.state.is_floating = client.state.old_state;
                client.geometry.border_w = client.geometry.old_border_w;
                let target = Rect::new(
                    client.geometry.old_x,
                    client.geometry.old_y,
                    client.geometry.old_w,
                    client.geometry.old_h,
                );
                if hidden {
                    let total_width = target
                        .w
                        .saturating_add(client.geometry.border_w.saturating_mul(2));
                    let hidden_x = hidden_x_left_of_desktop(desktop_left, total_width);
                    client.geometry.hidden_restore_rect = Some(target);
                    client.geometry.hidden_x = Some(hidden_x);
                    client.geometry.x = hidden_x;
                    client.geometry.y = target.y;
                    client.geometry.w = target.w;
                    client.geometry.h = target.h;
                } else {
                    client.geometry.x = target.x;
                    client.geometry.y = target.y;
                    client.geometry.w = target.w;
                    client.geometry.h = target.h;
                }
            }
            self.reorder_client_in_monitor_groups(client_key);
            let (x, y, w, h) = if let Some(client) = self.state.clients.get(client_key) {
                (
                    client.geometry.x,
                    client.geometry.y,
                    client.geometry.w,
                    client.geometry.h,
                )
            } else {
                return Ok(());
            };
            if hidden {
                let x11_bw = self.state.clients.get(client_key).map_or(0, |client| {
                    if backend.has_compositor() {
                        0
                    } else {
                        client.geometry.border_w.max(0) as u32
                    }
                });
                backend.window_ops().configure(
                    win,
                    x,
                    y,
                    w.max(1) as u32,
                    h.max(1) as u32,
                    x11_bw,
                )?;
            } else {
                self.resizeclient(backend, client_key, x, y, w, h)?;
            }
            if let Some(mon_key) = self.state.clients.get(client_key).and_then(|c| c.mon) {
                self.arrange(backend, Some(mon_key));
            }
        }
        let hidden_attached_transition = fullscreen != is_fullscreen
            && self.state.clients.get(client_key).is_some_and(|client| {
                client.state.is_hidden
                    && client.mon.is_some_and(|monitor| {
                        self.state
                            .monitor_clients
                            .get(monitor)
                            .is_some_and(|clients| clients.contains(&client_key))
                    })
            });
        if hidden_attached_transition
            && let Err(error) = self.persist_minimized_restore_state(backend, client_key)
        {
            // The mode transition itself is already complete. Keep running
            // and let seamless-exec refresh retry; a write failure must not
            // roll the live fullscreen state back.
            log::warn!(
                "could not refresh minimized restore state after fullscreen transition for {win:?}: {error}"
            );
        }
        Ok(())
    }

    /// Enter or leave Picture-in-Picture through one state/geometry owner.
    ///
    /// Fullscreen and PiP both borrow `old_state` and a semantic return
    /// rectangle. They must never be nested: entering either mode completes
    /// the other mode's restore first. Hidden clients keep their real/input
    /// window parked throughout the transition; only
    /// `hidden_restore_rect` moves between the visible semantic rectangles.
    pub(super) fn set_client_pip(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        pip: bool,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let previous_client = self
            .state
            .clients
            .get(client_key)
            .cloned()
            .ok_or("Client not found")?;
        let previous_animation = self.animations.active.get(&client_key).cloned();
        let previous_monitor_order = previous_client
            .mon
            .and_then(|monitor| self.state.monitor_clients.get(monitor).cloned());
        let crossing_from_fullscreen = pip && previous_client.state.is_fullscreen;

        match self.set_client_pip_inner(backend, client_key, pip) {
            Ok(changed) => Ok(changed),
            Err(error) => {
                if crossing_from_fullscreen {
                    self.restore_failed_mode_transition(
                        backend,
                        client_key,
                        &previous_client,
                        previous_animation,
                        previous_monitor_order,
                    );
                }
                Err(error)
            }
        }
    }

    fn set_client_pip_inner(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        pip: bool,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let (win, mut is_pip, is_fullscreen) = self
            .state
            .clients
            .get(client_key)
            .map(|client| (client.win, client.state.is_pip, client.state.is_fullscreen))
            .ok_or("Client not found")?;

        if pip && is_fullscreen {
            // Cross-mode atomicity is owned by the outer `set_client_pip`
            // snapshot. Calling the wrapped setter here would reconcile the
            // same failed fullscreen exit twice.
            self.setfullscreen_inner(backend, client_key, false)?;
            is_pip = self
                .state
                .clients
                .get(client_key)
                .is_some_and(|client| client.state.is_pip);
        }
        if pip == is_pip {
            return Ok(false);
        }

        let monitor = self
            .state
            .clients
            .get(client_key)
            .and_then(|client| client.mon);
        let hidden = self
            .state
            .clients
            .get(client_key)
            .is_some_and(|client| client.state.is_hidden);
        let pip_source = if pip {
            let client = self
                .state
                .clients
                .get(client_key)
                .ok_or("Client not found")?;
            let visible = client.geometry.hidden_restore_rect.unwrap_or(Rect::new(
                client.geometry.x,
                client.geometry.y,
                client.geometry.w,
                client.geometry.h,
            ));
            if visible.w <= 0 || visible.h <= 0 {
                return Err("PiP client has no valid source geometry".into());
            }
            Some(visible)
        } else {
            None
        };

        let mut target = if pip {
            let monitor = monitor.ok_or("PiP client has no monitor")?;
            let area = self
                .monitor_work_area(monitor)
                .ok_or("PiP monitor has no work area")?;
            let width = (area.w / 4).max(1);
            let height = (area.h / 4).max(1);
            Rect::new(
                area.x
                    .saturating_add(area.w)
                    .saturating_sub(width)
                    .saturating_sub(10),
                area.y
                    .saturating_add(area.h)
                    .saturating_sub(height)
                    .saturating_sub(10),
                width,
                height,
            )
        } else {
            let client = self
                .state
                .clients
                .get(client_key)
                .ok_or("Client not found")?;
            valid_restore_rect(
                client.geometry.floating_x,
                client.geometry.floating_y,
                client.geometry.floating_w,
                client.geometry.floating_h,
            )
            .map(|rect| Rect::new(rect.x, rect.y, rect.w, rect.h))
            .ok_or("PiP client has no valid restore geometry")?
        };

        let previous_client = self
            .state
            .clients
            .get(client_key)
            .cloned()
            .ok_or("Client not found")?;
        let previous_animation = self.animations.active.get(&client_key).cloned();
        let rollback_client = |jwm: &mut Jwm| {
            if let Some(client) = jwm.state.clients.get_mut(client_key) {
                *client = previous_client.clone();
            }
            if let Some(animation) = previous_animation.clone() {
                jwm.animations.active.insert(client_key, animation);
            } else {
                jwm.animations.remove(client_key);
            }
        };
        let restored_sticky = if pip {
            true
        } else {
            previous_client.state.pip_restore_sticky
        };
        if let Some(client) = self.state.clients.get_mut(client_key) {
            if pip {
                let visible = pip_source.expect("PiP source was validated");
                client.state.old_state = client.state.is_floating;
                client.state.pip_restore_sticky = client.state.is_sticky;
                client.geometry.floating_x = visible.x;
                client.geometry.floating_y = visible.y;
                client.geometry.floating_w = visible.w;
                client.geometry.floating_h = visible.h;
                client.state.is_pip = true;
                client.state.is_floating = true;
                client.state.is_sticky = true;
            } else {
                client.state.is_pip = false;
                client.state.is_floating = client.state.old_state;
                client.state.is_sticky = client.state.pip_restore_sticky;
                client.state.pip_restore_sticky = false;
            }
        }

        // Apply the exact same size/boundary policy before either the visible
        // or parked configure. Besides making failures observable, this keeps
        // the semantic hidden restore slot and the V1 snapshot equal to the
        // geometry that was actually requested from the backend.
        if let Err(error) = self.applysizehints(
            backend,
            client_key,
            &mut target.x,
            &mut target.y,
            &mut target.w,
            &mut target.h,
            false,
        ) {
            rollback_client(self);
            return Err(error);
        }
        if let Err(error) =
            backend
                .property_ops()
                .set_net_wm_state_flag(win, NetWmState::Sticky, restored_sticky)
        {
            rollback_client(self);
            if let Err(rollback_error) = backend.property_ops().set_net_wm_state_flag(
                win,
                NetWmState::Sticky,
                previous_client.state.is_sticky,
            ) {
                log::warn!(
                    "could not roll back Sticky after failed PiP protocol transition for {win:?}: {rollback_error}"
                );
            }
            return Err(error.into());
        }

        let configure_result = if hidden {
            let desktop_left = self.desktop_left_edge();
            let total_width = target
                .w
                .saturating_add(previous_client.geometry.border_w.saturating_mul(2));
            let hidden_x = hidden_x_left_of_desktop(desktop_left, total_width);
            let x11_border = if backend.has_compositor() {
                0
            } else {
                previous_client.geometry.border_w.max(0) as u32
            };
            let result = backend.window_ops().configure(
                win,
                hidden_x,
                target.y,
                target.w.max(1) as u32,
                target.h.max(1) as u32,
                x11_border,
            );
            if result.is_ok()
                && let Some(client) = self.state.clients.get_mut(client_key)
            {
                client.geometry.hidden_restore_rect = Some(target);
                client.geometry.hidden_x = Some(hidden_x);
                client.geometry.x = hidden_x;
                client.geometry.y = target.y;
                client.geometry.w = target.w;
                client.geometry.h = target.h;
            }
            result.map_err(Into::into)
        } else {
            self.resizeclient(backend, client_key, target.x, target.y, target.w, target.h)
        };
        if let Err(error) = configure_result {
            rollback_client(self);
            if let Err(rollback_error) = backend.property_ops().set_net_wm_state_flag(
                win,
                NetWmState::Sticky,
                previous_client.state.is_sticky,
            ) {
                log::warn!(
                    "could not roll back Sticky after failed PiP configure for {win:?}: {rollback_error}"
                );
            }
            return Err(error);
        }
        self.reorder_client_in_monitor_groups(client_key);

        // This callback is deliberately unconditional: non-compositing
        // backends implement it as a no-op, while compositor backends observe
        // a deterministic PiP-off-before-fullscreen-on ordering.
        backend.compositor_set_window_pip(win, pip);
        self.arrange(backend, monitor);

        let hidden_attached_transition = self.state.clients.get(client_key).is_some_and(|client| {
            client.state.is_hidden
                && client.mon.is_some_and(|monitor| {
                    self.state
                        .monitor_clients
                        .get(monitor)
                        .is_some_and(|clients| clients.contains(&client_key))
                })
        });
        if hidden_attached_transition
            && let Err(error) = self.persist_minimized_restore_state(backend, client_key)
        {
            log::warn!(
                "could not refresh minimized restore state after PiP transition for {win:?}: {error}"
            );
        }
        Ok(true)
    }

    pub(super) fn seturgent(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        urgent: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let win = self.sync_client_urgent_state(backend, client_key, urgent)?;
        Ok(backend.property_ops().set_urgent_hint(win, urgent)?)
    }

    /// Update the authoritative client state and compositor without writing
    /// `WM_HINTS`. `PropertyNotify` handling uses this to avoid rewriting the
    /// property that triggered it and entering a notification loop.
    fn sync_client_urgent_state(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        urgent: bool,
    ) -> Result<WindowId, Box<dyn std::error::Error>> {
        let win = if let Some(client) = self.state.clients.get_mut(client_key) {
            client.state.is_urgent = urgent;
            client.win
        } else {
            return Err("Client not found".into());
        };

        if backend.has_compositor() {
            backend.compositor_set_window_urgent(win, urgent);
        } else {
            let is_focused = self.get_selected_client_key() == Some(client_key);
            self.update_client_decoration(backend, client_key, is_focused)?;
        }
        Ok(win)
    }

    pub(super) fn setclientstate(
        &self,
        backend: &mut dyn Backend,
        win: WindowId,
        state: i64,
    ) -> Result<(), Box<dyn std::error::Error>> {
        Ok(backend.property_ops().set_wm_state(win, state)?)
    }

    /// Float a client whose size hints pin it to a single size.
    ///
    /// dwm decides this in `manage()` (`c->isfloating = trans != None ||
    /// c->isfixed`), but jwm learns `is_fixed` only in `updatesizehints`,
    /// which runs after `applyrules_by_key` has already forced
    /// `is_floating = false`. Without this pass a min==max window is tiled: it
    /// occupies a layout slot it cannot fill, so `applysizehints` clamps it
    /// back to its own size and parks it at the tile origin while the rest of
    /// the layout is laid out around a rectangle nothing ever covers.
    /// Feishu's 780x659 "飞书会议" pre-join window is exactly this shape.
    pub(super) fn float_if_fixed_size(&mut self, client_key: ClientKey) {
        let Some(client) = self.state.clients.get(client_key) else {
            return;
        };
        if !client.state.is_fixed || client.state.is_floating {
            return;
        }
        log::info!(
            "[float_if_fixed_size] {:?} has min==max size hints ({}x{}); floating it",
            client.win,
            client.size_hints.min_w,
            client.size_hints.min_h,
        );
        if let Some(client) = self.state.clients.get_mut(client_key) {
            client.state.is_floating = true;
        }
        self.reorder_client_in_monitor_groups(client_key);
    }

    pub(super) fn updatewindowtype(&mut self, backend: &mut dyn Backend, client_key: ClientKey) {
        let (win, is_popup_like) = if let Some(client) = self.state.clients.get(client_key) {
            (client.win, self.is_popup_like(backend, client_key))
        } else {
            return;
        };

        let was_floating = self
            .state
            .clients
            .get(client_key)
            .map(|client| client.state.is_floating)
            .unwrap_or(false);

        if backend.property_ops().is_fullscreen(win) {
            let _ = self.setfullscreen(backend, client_key, true);
        }

        let types = backend.property_ops().get_window_types(win);
        let is_desktop = types.contains(&WindowType::Desktop);
        let is_dock = types.contains(&WindowType::Dock);
        let is_transient = backend.property_ops().transient_for(win).is_some();

        let layer_info = backend.property_ops().get_layer_surface_info(win);

        if let Some(c) = self.state.clients.get_mut(client_key) {
            c.state.is_dock = is_dock;
            c.state.dock_layer_info = if is_dock { layer_info } else { None };

            if is_popup_like || is_desktop {
                c.state.is_floating = true;

                if types.contains(&WindowType::Notification)
                    || types.contains(&WindowType::Tooltip)
                    || types.contains(&WindowType::Dock)
                    || types.contains(&WindowType::Desktop)
                {
                    if !is_transient {
                        c.state.tags = crate::config::CONFIG.load().tagmask();
                        c.state.never_focus = true;
                    }
                }
            }
        }

        let is_floating_now = self
            .state
            .clients
            .get(client_key)
            .map(|client| client.state.is_floating)
            .unwrap_or(was_floating);
        if is_floating_now != was_floating {
            self.reorder_client_in_monitor_groups(client_key);
        }
    }

    pub(super) fn updatewmhints(&mut self, backend: &mut dyn Backend, client_key: ClientKey) {
        let win = match self.state.clients.get(client_key) {
            Some(c) => c.win,
            None => return,
        };
        if let Some(hints) = backend.property_ops().get_wm_hints(win) {
            let is_focused = self.is_client_selected(client_key);
            // Under DND, suppress urgency on unfocused clients to silence
            // taskbar/tag highlights and prevent focus-stealing chains.
            let (urgent, clear_hint) =
                wm_hint_urgency_policy(hints.urgent, is_focused, self.do_not_disturb);
            if clear_hint {
                // This is the sole PropertyNotify path that writes WM_HINTS:
                // an active policy suppression must clear the source flag.
                let _ = self.seturgent(backend, client_key, false);
            } else {
                let _ = self.sync_client_urgent_state(backend, client_key, urgent);
            }
            if let Some(input_ok) = hints.input {
                if let Some(c) = self.state.clients.get_mut(client_key) {
                    c.state.never_focus = !input_ok;
                }
            } else {
                if let Some(c) = self.state.clients.get_mut(client_key) {
                    c.state.never_focus = false;
                }
            }
        }
    }

    /// Minimise or restore a client, whoever asked. Every route into this —
    /// the `minimize` command, an ICCCM `WM_CHANGE_STATE` from a toolkit's own
    /// minimise button, a pager's `_NET_WM_STATE_HIDDEN`, a Wayland taskbar's
    /// foreign-toplevel request — has to run the same steps in the same order,
    /// so they all run these.
    ///
    /// The order is the part that matters. Minimising must detach the still
    /// visible compositor texture *before* `arrange` moves the X window off
    /// screen, or the genie animation has nothing left to animate; restoring
    /// is the exact inverse, with `arrange` re-establishing live geometry
    /// before the compositor rebuilds its entry.
    ///
    /// Returns `Ok(false)` when the client was already in that state after
    /// repairing its public ICCCM/EWMH properties. An X11 parking error is a
    /// deliberately post-commit error: Hidden/Iconic, the Dock order and the
    /// retained visual stay intact so the same request can retry only the
    /// failed real-window move without replaying the animation.
    pub(crate) fn set_client_minimized(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        minimized: bool,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        if !minimized
            && self
                .state
                .clients
                .get(client_key)
                .is_some_and(|client| client.state.is_hidden)
        {
            // This is the last failure point before the public Hidden/WM_STATE
            // transition and the checked MapWindow transaction begin. A true
            // Iconic client's X11 geometry can have become unsafe while it
            // was unmapped after an output topology change.
            self.retry_x11_minimized_client_park(backend, client_key)?;
        }
        if self
            .state
            .clients
            .get(client_key)
            .is_some_and(|client| client.state.is_hidden)
        {
            self.reconcile_existing_minimized_snapshot(backend, client_key)?;
        }
        let Some(client) = self.state.clients.get(client_key) else {
            return Ok(false);
        };
        let state_changed = client.state.is_hidden != minimized;
        let previous_internal_hidden = client.state.is_hidden;
        let previous_minimized_order = client.state.minimized_order;
        let previous_urgent = client.state.is_urgent;
        let win = client.win;
        let monitor = client.mon;
        let was_selected = self.is_client_selected(client_key);
        let previous_selected_monitor = self.state.sel_mon;
        let previous_target_selection = monitor
            .and_then(|monitor| self.state.monitors.get(monitor))
            .and_then(|monitor| {
                monitor
                    .get_selected_client_for_current_tag()
                    .or(monitor.sel)
            });
        let previous_selected_client = (!minimized)
            .then(|| self.get_selected_client_key())
            .flatten();
        let previous_monitor_stack = if minimized {
            None
        } else {
            monitor
                .and_then(|monitor| self.state.monitor_stack.get(monitor))
                .cloned()
        };
        let monitor_num = monitor
            .and_then(|key| self.state.monitors.get(key))
            .map(|monitor| monitor.num);
        // The bar and compositor must agree on which hidden clients are Dock
        // items.  In particular, minimizing a SKIP_TASKBAR helper (or an
        // internal Dock/swallowed surface) must not retain a ghost texture
        // that no bar can ever address or release.
        let dock_eligible = StatusBarBuilder::is_minimized_dock_eligible(client);

        // Persist protocol state before committing the internal transition. If
        // ICCCM fails after EWMH succeeds, restore the previous EWMH bit so a
        // transient backend error cannot leave two public state machines in
        // disagreement with an unchanged JWM client.
        let proposed_minimized_order = if minimized && state_changed {
            Some(next_minimized_order().ok_or("minimized Dock order space exhausted")?)
        } else {
            None
        };
        let restore_snapshot = if let Some(order) = proposed_minimized_order {
            Some(
                minimized_restore_snapshot(client, monitor_num, order)
                    .ok_or("client has no valid minimized restore geometry")?,
            )
        } else {
            None
        };
        if let Some(snapshot) = restore_snapshot {
            backend
                .property_ops()
                .set_minimized_restore_state(win, snapshot)?;
        }

        let previous_ewmh_hidden = backend
            .property_ops()
            .has_net_wm_state_flag(win, crate::backend::api::NetWmState::Hidden)
            .unwrap_or(previous_internal_hidden);
        if let Err(error) = backend.property_ops().set_net_wm_state_flag(
            win,
            crate::backend::api::NetWmState::Hidden,
            minimized,
        ) {
            if restore_snapshot.is_some() {
                let _ = backend.property_ops().clear_minimized_restore_state(win);
            }
            return Err(error.into());
        }
        if let Err(error) =
            self.setclientstate(backend, win, i64::from(wm_state_for_minimized(minimized)))
        {
            let _ = backend.property_ops().set_net_wm_state_flag(
                win,
                crate::backend::api::NetWmState::Hidden,
                previous_ewmh_hidden,
            );
            if restore_snapshot.is_some() {
                let _ = backend.property_ops().clear_minimized_restore_state(win);
            }
            return Err(error);
        }

        // ICCCM deiconification is a physical MapWindow transaction. Do it
        // only after both public state machines say Normal, but before JWM
        // exposes the client to arrange/focus. The X11 backend verifies the
        // checked request; policy repeats the ordered attribute query so a
        // default/non-X11 implementation cannot accidentally commit an
        // unmapped restore.
        if !minimized {
            let cancel_result = backend.compositor_cancel_window_iconify(win);
            let mapped_result = cancel_result.and_then(|()| {
                if !backend.capabilities().supports_client_list {
                    return Ok(());
                }
                match backend.window_ops().get_window_attributes(win) {
                    Ok(attributes) if attributes.map_state_viewable => Ok(()),
                    Ok(_) => Err(crate::backend::error::BackendError::Message(format!(
                        "restored X11 window {win:?} is not viewable"
                    ))),
                    Err(error) => Err(error),
                }
            });
            if let Err(error) = mapped_result {
                if let Err(rollback_error) = backend.property_ops().set_net_wm_state_flag(
                    win,
                    NetWmState::Hidden,
                    previous_ewmh_hidden,
                ) {
                    log::warn!(
                        "could not roll back EWMH Hidden after failed deiconify for {win:?}: {rollback_error}"
                    );
                }
                if let Err(rollback_error) = self.setclientstate(
                    backend,
                    win,
                    i64::from(wm_state_for_minimized(previous_internal_hidden)),
                ) {
                    log::warn!(
                        "could not roll back WM_STATE after failed deiconify for {win:?}: {rollback_error}"
                    );
                }
                if let Err(rearm_error) =
                    self.request_iconify_for_hidden_dock_client(backend, client_key)
                {
                    log::warn!(
                        "could not retain Iconic ownership after failed deiconify for {win:?}: {rearm_error}"
                    );
                }
                return Err(error.into());
            }
        }

        // Repeated protocol requests still repair ICCCM/EWMH above. Avoid
        // replaying compositor animations or changing insertion order when
        // the internal state was already correct.
        if !state_changed {
            if !minimized
                && let Err(error) = backend.property_ops().clear_minimized_restore_state(win)
            {
                log::warn!("could not clear stale minimized restore state for {win:?}: {error}");
            }
            if !minimized {
                self.clear_hidden_client_park_retry(client_key);
            }
            if minimized_window_relinquishes_focus(minimized, was_selected) {
                self.focus(backend, None)?;
            }
            if minimized {
                self.retry_x11_minimized_client_park(backend, client_key)?;
                self.request_iconify_for_hidden_dock_client(backend, client_key)?;
            }
            return Ok(false);
        }

        if let Some(client) = self.state.clients.get_mut(client_key) {
            client.state.is_hidden = minimized;
            client.state.minimized_order = proposed_minimized_order.unwrap_or(0);
        }

        // A minimized client is represented in the bar's Dock even when it
        // was not focused.  Focus changes used to refresh the bar only for
        // the selected-window path, which left foreign-toplevel and EWMH
        // minimization of background windows invisible until an unrelated
        // status update happened.
        self.mark_bar_update_needed_if_visible(monitor_num);

        if minimized && let (Some(monitor_key), Some(monitor_num)) = (monitor, monitor_num) {
            let monitor_clients = self
                .state
                .monitor_clients
                .get(monitor_key)
                .map_or(&[][..], Vec::as_slice);
            let windows = StatusBarBuilder::get_minimized_windows(
                &self.state.clients,
                monitor_clients,
                monitor_num,
            );
            let omitted = windows.len().saturating_sub(MAX_MINIMIZED_WINDOWS);
            for window in windows.into_iter().take(omitted) {
                let window = WindowId::from_raw(window.window_id);
                self.clear_minimized_preview_for(backend, monitor_num, Some(window));
                backend.compositor_set_window_dock_geometry(window, None);
            }
        }
        if minimized {
            let target = if dock_eligible {
                monitor_num
                    .and_then(|monitor_num| self.minimized_dock_shelves.get(&monitor_num))
                    .copied()
            } else {
                None
            };
            // An explicit None also withdraws any stale target left by a
            // previous classification or a bar that disappeared.
            backend.compositor_set_window_dock_geometry(win, target);
            if dock_eligible {
                backend.compositor_set_window_minimized(win, true);
            }
        }
        let relinquish_focus = minimized_window_relinquishes_focus(minimized, was_selected);
        self.arrange(backend, monitor);
        if minimized
            && let Err(error) = self.verify_x11_minimized_client_parked(backend, client_key)
        {
            // Keep the already-committed Hidden/Iconic state, Dock order, V1
            // snapshot and retained compositor capture. A repeated minimize
            // retries only the failed real-window park above.
            if relinquish_focus && let Err(focus_error) = self.focus(backend, None) {
                log::warn!(
                    "could not relinquish focus after failed minimize parking for {win:?}: {focus_error}"
                );
            }
            return Err(error);
        }
        if !minimized && backend.capabilities().supports_client_list {
            let desktop_left = self.desktop_left_edge();
            let restore_failure = match backend.window_ops().get_window_attributes(win) {
                Ok(attributes) if !attributes.map_state_viewable => {
                    Some("window remained physically unmapped".to_string())
                }
                Err(error) => Some(format!("could not verify restored X11 map state: {error}")),
                Ok(_) => match backend.window_ops().get_geometry(win) {
                    Ok(geometry) if !x11_geometry_fully_left_of_desktop(geometry, desktop_left) => {
                        None
                    }
                    Ok(_) => Some("window remained in JWM's hidden parking region".to_string()),
                    Err(error) => Some(format!("could not verify restored X11 geometry: {error}")),
                },
            };
            if let Some(reason) = restore_failure {
                // `show_client` updates semantic geometry before issuing the
                // X move. If that move was ignored or failed, re-stage the
                // now-visible semantic rectangle, put the internal/public
                // state back to minimized, and leave the V1 snapshot intact
                // for a retry or crash recovery. The reverse compositor
                // transition and focus handoff have not started yet.
                self.rollback_failed_minimized_restore(
                    backend,
                    client_key,
                    previous_minimized_order,
                    previous_urgent,
                    previous_selected_client,
                    previous_selected_monitor,
                    previous_target_selection,
                    previous_monitor_stack.as_deref(),
                    false,
                );
                return Err(format!("could not restore {win:?}: {reason}").into());
            }
        }
        if relinquish_focus {
            // Arrange first so the newly selected client is chosen from the
            // final visible layout, not while the minimized client is still
            // waiting to be parked by show/hide.
            self.focus(backend, None)?;
        }
        if minimized {
            // The compositor has already detached/cached the live pixels and
            // the real client is now safely parked with focus relinquished.
            // A capacity/admission failure intentionally leaves the window in
            // that mapped fallback state and makes this request retryable.
            self.request_iconify_for_hidden_dock_client(backend, client_key)?;
        }
        if !minimized {
            // Restoring is always someone asking for *this* window back, so it
            // takes the focus. `focusin` cannot do that job — it is the
            // FocusIn handler, and re-asserts focus on whatever is already
            // selected, which left a restored window unfocused behind the
            // window that replaced it.
            if let Err(error) = self.focus(backend, Some(client_key)) {
                self.rollback_failed_minimized_restore(
                    backend,
                    client_key,
                    previous_minimized_order,
                    previous_urgent,
                    previous_selected_client,
                    previous_selected_monitor,
                    previous_target_selection,
                    previous_monitor_stack.as_deref(),
                    true,
                );
                return Err(error);
            }
            if let Err(error) = self.restack(backend, self.state.sel_mon) {
                self.rollback_failed_minimized_restore(
                    backend,
                    client_key,
                    previous_minimized_order,
                    previous_urgent,
                    previous_selected_client,
                    previous_selected_monitor,
                    previous_target_selection,
                    previous_monitor_stack.as_deref(),
                    true,
                );
                return Err(error);
            }
            if let Some(monitor_num) = monitor_num {
                self.clear_minimized_preview_for(backend, monitor_num, Some(win));
            }
            backend.compositor_set_window_minimized(win, false);
            if let Err(error) = backend.property_ops().clear_minimized_restore_state(win) {
                // Public/internal state is already restored. A stale private
                // property is ignored while WM_STATE is Normal and will be
                // retried by idempotent restore, live unmanage, or normal exit.
                log::warn!("could not clear minimized restore state for {win:?}: {error}");
            }
            self.clear_hidden_client_park_retry(client_key);
        }
        Ok(true)
    }
}

#[cfg(test)]
mod urgency_tests {
    use super::{
        EXHAUSTED_MINIMIZED_ORDER, MAX_MINIMIZED_RESTORE_ORDER, MAX_RECOVERED_MINIMIZED_ORDER,
        client_decoration_scheme, minimized_order_is_safe_to_recover, minimized_order_transition,
        wm_hint_urgency_policy,
    };
    use crate::backend::common_define::SchemeType;

    #[test]
    fn native_decoration_prioritizes_focus_then_urgency() {
        assert_eq!(client_decoration_scheme(true, true, true), SchemeType::Sel);
        assert_eq!(client_decoration_scheme(true, false, true), SchemeType::Sel);
        assert_eq!(
            client_decoration_scheme(false, true, true),
            SchemeType::Urgent
        );
        assert_eq!(
            client_decoration_scheme(false, false, true),
            SchemeType::Norm
        );
        assert_eq!(
            client_decoration_scheme(false, true, false),
            SchemeType::Norm
        );
    }

    #[test]
    fn wm_hint_policy_only_writes_when_an_urgent_hint_is_suppressed() {
        assert_eq!(wm_hint_urgency_policy(false, false, false), (false, false));
        assert_eq!(wm_hint_urgency_policy(false, true, true), (false, false));
        assert_eq!(wm_hint_urgency_policy(true, false, false), (true, false));
        assert_eq!(wm_hint_urgency_policy(true, true, false), (false, true));
        assert_eq!(wm_hint_urgency_policy(true, false, true), (false, true));
    }

    #[test]
    fn minimized_order_allocator_exhausts_without_wrapping() {
        assert_eq!(minimized_order_transition(0), Some((1, 2)));
        assert_eq!(
            minimized_order_transition(MAX_MINIMIZED_RESTORE_ORDER - 1),
            Some((MAX_MINIMIZED_RESTORE_ORDER - 1, MAX_MINIMIZED_RESTORE_ORDER))
        );
        assert_eq!(
            minimized_order_transition(MAX_MINIMIZED_RESTORE_ORDER),
            Some((MAX_MINIMIZED_RESTORE_ORDER, EXHAUSTED_MINIMIZED_ORDER))
        );
        assert_eq!(minimized_order_transition(EXHAUSTED_MINIMIZED_ORDER), None);
        assert_eq!(minimized_order_transition(u64::MAX), None);
    }

    #[test]
    fn recovered_order_cannot_exhaust_the_process_allocator() {
        assert!(!minimized_order_is_safe_to_recover(0));
        assert!(minimized_order_is_safe_to_recover(1));
        assert!(minimized_order_is_safe_to_recover(
            MAX_RECOVERED_MINIMIZED_ORDER
        ));
        assert!(!minimized_order_is_safe_to_recover(
            MAX_RECOVERED_MINIMIZED_ORDER + 1
        ));
        assert!(!minimized_order_is_safe_to_recover(
            MAX_MINIMIZED_RESTORE_ORDER
        ));
    }
}

/// A minimised window cannot stay focused — nothing on screen would show where
/// the keyboard is going — but minimising some *other* window must not steal
/// focus away from whatever has it.
fn minimized_window_relinquishes_focus(minimized: bool, is_selected: bool) -> bool {
    minimized && is_selected
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::api::{
        BackendDiagnostics, Capabilities, CloseResult, ColorAllocator, CompositorAnnotation,
        CompositorBenchmark, CompositorControl, CompositorMedia, CompositorRect,
        CompositorWindowEffects, CompositorWorkspaceEffects, CursorProvider, DisplayControl,
        EventHandler, InputOps, KeyOps, NetWmState, NormalHints, OutputOps, PropertyOps,
        RenderScheduler, WindowAttributes, WindowOps, WindowType, WmHints,
    };
    use crate::backend::common_define::Pixel;
    use crate::backend::error::BackendError;
    use crate::backend::wayland_dummy_ops::{
        DummyColorAllocator, DummyCursorProvider, DummyInputOps, DummyKeyOps, DummyOutputOps,
    };
    use crate::core::models::WMClient;
    use std::any::Any;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum ModeEvent {
        Fullscreen(bool),
        Pip(bool),
        Sticky(bool),
    }

    #[derive(Default)]
    struct MinimizePropertyOps {
        ewmh_hidden: AtomicBool,
        fullscreen: AtomicBool,
        sticky: AtomicBool,
        wm_state: AtomicI64,
        fail_wm_state: AtomicBool,
        fail_ewmh: AtomicBool,
        fail_ewmh_query: AtomicBool,
        fail_restore_set: AtomicBool,
        fail_fullscreen_on: AtomicBool,
        fail_fullscreen_off: AtomicBool,
        fail_sticky_on: AtomicBool,
        fail_next_normal_hints: AtomicBool,
        normal_hints: Mutex<Option<NormalHints>>,
        urgent_writes: Mutex<Vec<(WindowId, bool)>>,
        minimized_restore: Mutex<Option<MinimizedRestoreState>>,
        minimized_restore_writes: Mutex<Vec<Option<MinimizedRestoreState>>>,
        mode_events: Mutex<Vec<ModeEvent>>,
    }

    impl PropertyOps for MinimizePropertyOps {
        fn get_title(&self, _win: WindowId) -> String {
            String::new()
        }

        fn get_class(&self, _win: WindowId) -> (String, String) {
            (String::new(), String::new())
        }

        fn get_window_types(&self, _win: WindowId) -> Vec<WindowType> {
            Vec::new()
        }

        fn is_fullscreen(&self, _win: WindowId) -> bool {
            self.fullscreen.load(Ordering::Relaxed)
        }

        fn set_fullscreen_state(&self, _win: WindowId, on: bool) -> Result<(), BackendError> {
            if (on && self.fail_fullscreen_on.load(Ordering::Relaxed))
                || (!on && self.fail_fullscreen_off.load(Ordering::Relaxed))
            {
                return Err(BackendError::Message(
                    "injected fullscreen property failure".into(),
                ));
            }
            self.fullscreen.store(on, Ordering::Relaxed);
            self.mode_events
                .lock()
                .expect("mode events lock")
                .push(ModeEvent::Fullscreen(on));
            Ok(())
        }

        fn transient_for(&self, _win: WindowId) -> Option<WindowId> {
            None
        }

        fn get_wm_hints(&self, _win: WindowId) -> Option<WmHints> {
            None
        }

        fn set_urgent_hint(&self, win: WindowId, urgent: bool) -> Result<(), BackendError> {
            self.urgent_writes
                .lock()
                .expect("urgent writes lock")
                .push((win, urgent));
            Ok(())
        }

        fn fetch_normal_hints(&self, _win: WindowId) -> Result<Option<NormalHints>, BackendError> {
            if self.fail_next_normal_hints.swap(false, Ordering::Relaxed) {
                return Err(BackendError::Message(
                    "injected normal-hints failure".into(),
                ));
            }
            Ok(*self.normal_hints.lock().expect("normal hints lock"))
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
            Ok(self.wm_state.load(Ordering::Relaxed))
        }

        fn set_wm_state(&self, _win: WindowId, state: i64) -> Result<(), BackendError> {
            if self.fail_wm_state.load(Ordering::Relaxed) {
                return Err(BackendError::Message("injected WM_STATE failure".into()));
            }
            self.wm_state.store(state, Ordering::Relaxed);
            Ok(())
        }

        fn get_minimized_restore_state(
            &self,
            _win: WindowId,
        ) -> Result<Option<MinimizedRestoreState>, BackendError> {
            Ok(*self
                .minimized_restore
                .lock()
                .expect("minimized restore lock"))
        }

        fn set_minimized_restore_state(
            &self,
            _win: WindowId,
            state: MinimizedRestoreState,
        ) -> Result<(), BackendError> {
            if self.fail_restore_set.load(Ordering::Relaxed) {
                return Err(BackendError::Message(
                    "injected minimized-restore property failure".into(),
                ));
            }
            *self
                .minimized_restore
                .lock()
                .expect("minimized restore lock") = Some(state);
            self.minimized_restore_writes
                .lock()
                .expect("minimized restore writes lock")
                .push(Some(state));
            Ok(())
        }

        fn clear_minimized_restore_state(&self, _win: WindowId) -> Result<(), BackendError> {
            *self
                .minimized_restore
                .lock()
                .expect("minimized restore lock") = None;
            self.minimized_restore_writes
                .lock()
                .expect("minimized restore writes lock")
                .push(None);
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

        fn set_net_wm_state_flag(
            &self,
            _win: WindowId,
            state: NetWmState,
            on: bool,
        ) -> Result<(), BackendError> {
            if state == NetWmState::Hidden {
                if self.fail_ewmh.load(Ordering::Relaxed) {
                    return Err(BackendError::Message("injected EWMH failure".into()));
                }
                self.ewmh_hidden.store(on, Ordering::Relaxed);
            } else if state == NetWmState::Sticky {
                if on && self.fail_sticky_on.load(Ordering::Relaxed) {
                    return Err(BackendError::Message(
                        "injected Sticky property failure".into(),
                    ));
                }
                self.sticky.store(on, Ordering::Relaxed);
                self.mode_events
                    .lock()
                    .expect("mode events lock")
                    .push(ModeEvent::Sticky(on));
            }
            Ok(())
        }

        fn has_net_wm_state_flag(
            &self,
            _win: WindowId,
            state: NetWmState,
        ) -> Result<bool, BackendError> {
            if self.fail_ewmh_query.load(Ordering::Relaxed) {
                return Err(BackendError::Message("injected EWMH query failure".into()));
            }
            Ok(state == NetWmState::Hidden && self.ewmh_hidden.load(Ordering::Relaxed))
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq)]
    enum DockLifecycleEvent {
        Geometry(WindowId, Option<CompositorRect>),
        Preview(Option<WindowId>, Option<CompositorRect>),
        EnsureStatic(WindowId),
        RequestIconify(WindowId),
        CancelIconify(WindowId),
        ForgetStatic(WindowId),
    }

    #[derive(Default)]
    struct MinimizeWindowOps {
        configures: Mutex<Vec<(WindowId, i32, i32, u32, u32, u32)>>,
        decoration_updates: Mutex<Vec<WindowId>>,
        button_ungrabs: Mutex<Vec<WindowId>>,
        passive_button_grabs: Mutex<Vec<WindowId>>,
        input_focus_attempts: Mutex<Vec<WindowId>>,
        geometries: Mutex<std::collections::HashMap<WindowId, Geometry>>,
        scanned_windows: Mutex<Vec<WindowId>>,
        mapped_windows: Mutex<Vec<WindowId>>,
        fail_scan: AtomicBool,
        fail_next_position: AtomicBool,
        fail_restack_countdown: AtomicI64,
        fail_configure: AtomicBool,
        fail_next_configure: AtomicBool,
        fail_configure_countdown: AtomicI64,
        fail_configure_size: Mutex<Option<(u32, u32)>>,
        fail_apply_window_changes: AtomicBool,
        fail_next_input_focus: AtomicBool,
        fail_next_attributes: AtomicBool,
        force_unmapped: AtomicBool,
    }

    impl WindowOps for MinimizeWindowOps {
        fn set_position(&self, win: WindowId, x: i32, y: i32) -> Result<(), BackendError> {
            if self.fail_next_position.swap(false, Ordering::Relaxed) {
                return Err(BackendError::Message(
                    "injected set-position failure".into(),
                ));
            }
            let mut geometries = self.geometries.lock().expect("window geometries lock");
            let geometry = geometries.entry(win).or_default();
            geometry.x = x;
            geometry.y = y;
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
            let countdown = self.fail_configure_countdown.load(Ordering::Relaxed);
            let countdown_failed = countdown > 0
                && self
                    .fail_configure_countdown
                    .fetch_sub(1, Ordering::Relaxed)
                    == 1;
            let size_failed = {
                let mut fail_size = self
                    .fail_configure_size
                    .lock()
                    .expect("configure failure size lock");
                if *fail_size == Some((w, h)) {
                    *fail_size = None;
                    true
                } else {
                    false
                }
            };
            if self.fail_configure.load(Ordering::Relaxed)
                || self.fail_next_configure.swap(false, Ordering::Relaxed)
                || countdown_failed
                || size_failed
            {
                return Err(BackendError::Message("injected configure failure".into()));
            }
            self.configures
                .lock()
                .expect("window configures lock")
                .push((win, x, y, w, h, border));
            self.geometries
                .lock()
                .expect("window geometries lock")
                .insert(win, Geometry { x, y, w, h, border });
            Ok(())
        }

        fn set_decoration_style(
            &self,
            win: WindowId,
            _border_width: u32,
            _border_color: Pixel,
        ) -> Result<(), BackendError> {
            self.decoration_updates
                .lock()
                .expect("decoration updates lock")
                .push(win);
            Ok(())
        }

        fn raise_window(&self, _win: WindowId) -> Result<(), BackendError> {
            Ok(())
        }

        fn restack_windows(&self, _windows: &[WindowId]) -> Result<(), BackendError> {
            let countdown = self.fail_restack_countdown.load(Ordering::Relaxed);
            if countdown > 0 && self.fail_restack_countdown.fetch_sub(1, Ordering::Relaxed) == 1 {
                return Err(BackendError::Message("injected restack failure".into()));
            }
            Ok(())
        }

        fn map_window(&self, win: WindowId) -> Result<(), BackendError> {
            self.mapped_windows
                .lock()
                .expect("mapped windows lock")
                .push(win);
            self.force_unmapped.store(false, Ordering::Relaxed);
            Ok(())
        }

        fn unmap_window(&self, _win: WindowId) -> Result<(), BackendError> {
            Ok(())
        }

        fn close_window(&self, _win: WindowId) -> Result<CloseResult, BackendError> {
            Ok(CloseResult::Graceful)
        }

        fn set_input_focus(&self, win: WindowId) -> Result<(), BackendError> {
            self.input_focus_attempts
                .lock()
                .expect("input focus attempts lock")
                .push(win);
            if self.fail_next_input_focus.swap(false, Ordering::Relaxed) {
                return Err(BackendError::Message(
                    "injected set-input-focus failure".into(),
                ));
            }
            Ok(())
        }

        fn set_input_focus_root(&self) -> Result<(), BackendError> {
            Ok(())
        }

        fn get_window_attributes(&self, _win: WindowId) -> Result<WindowAttributes, BackendError> {
            if self.fail_next_attributes.swap(false, Ordering::Relaxed) {
                return Err(BackendError::Message(
                    "injected window-attributes failure".into(),
                ));
            }
            Ok(WindowAttributes {
                override_redirect: false,
                map_state_viewable: !self.force_unmapped.load(Ordering::Relaxed),
            })
        }

        fn get_geometry(&self, win: WindowId) -> Result<Geometry, BackendError> {
            Ok(self
                .geometries
                .lock()
                .expect("window geometries lock")
                .get(&win)
                .copied()
                .unwrap_or_default())
        }

        fn scan_windows(&self) -> Result<Vec<WindowId>, BackendError> {
            if self.fail_scan.load(Ordering::Relaxed) {
                return Err(BackendError::Message("injected root scan failure".into()));
            }
            Ok(self
                .scanned_windows
                .lock()
                .expect("scanned windows lock")
                .clone())
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
            if self.fail_apply_window_changes.load(Ordering::Relaxed) {
                return Err(BackendError::Message(
                    "injected apply-window-changes failure".into(),
                ));
            }
            Ok(())
        }

        fn ungrab_all_buttons(&self, win: WindowId) -> Result<(), BackendError> {
            self.button_ungrabs
                .lock()
                .expect("button ungrabs lock")
                .push(win);
            Ok(())
        }

        fn grab_button_any_anymod(&self, win: WindowId, _mask: u32) -> Result<(), BackendError> {
            self.passive_button_grabs
                .lock()
                .expect("passive button grabs lock")
                .push(win);
            Ok(())
        }
    }

    struct MinimizeSpyBackend {
        window_ops: MinimizeWindowOps,
        input_ops: DummyInputOps,
        property_ops: MinimizePropertyOps,
        output_ops: DummyOutputOps,
        key_ops: DummyKeyOps,
        cursor_provider: DummyCursorProvider,
        color_allocator: DummyColorAllocator,
        minimized: Vec<(WindowId, bool)>,
        dock_targets: Vec<(WindowId, Option<CompositorRect>)>,
        dock_lifecycle: Vec<DockLifecycleEvent>,
        focused: Vec<Option<WindowId>>,
        fail_next_focus: AtomicBool,
        iconify_requests: Vec<WindowId>,
        iconify_cancels: Vec<WindowId>,
        iconify_cancel_observations: Vec<(WindowId, Geometry)>,
        iconify_observations: Vec<(WindowId, Geometry, Option<Option<WindowId>>)>,
        fail_next_iconify: AtomicBool,
        fail_next_iconify_cancel: AtomicBool,
        iconify_cancel_leaves_unmapped: AtomicBool,
        supports_client_list: bool,
    }

    impl MinimizeSpyBackend {
        fn new() -> Self {
            Self {
                window_ops: MinimizeWindowOps::default(),
                input_ops: DummyInputOps,
                property_ops: MinimizePropertyOps::default(),
                output_ops: DummyOutputOps,
                key_ops: DummyKeyOps,
                cursor_provider: DummyCursorProvider,
                color_allocator: DummyColorAllocator,
                minimized: Vec::new(),
                dock_targets: Vec::new(),
                dock_lifecycle: Vec::new(),
                focused: Vec::new(),
                fail_next_focus: AtomicBool::new(false),
                iconify_requests: Vec::new(),
                iconify_cancels: Vec::new(),
                iconify_cancel_observations: Vec::new(),
                iconify_observations: Vec::new(),
                fail_next_iconify: AtomicBool::new(false),
                fail_next_iconify_cancel: AtomicBool::new(false),
                iconify_cancel_leaves_unmapped: AtomicBool::new(false),
                supports_client_list: false,
            }
        }
    }

    impl CompositorBenchmark for MinimizeSpyBackend {}
    impl BackendDiagnostics for MinimizeSpyBackend {}
    impl CompositorControl for MinimizeSpyBackend {}
    impl CompositorMedia for MinimizeSpyBackend {}
    impl CompositorWorkspaceEffects for MinimizeSpyBackend {}
    impl CompositorWindowEffects for MinimizeSpyBackend {
        fn compositor_request_window_iconify(
            &mut self,
            window: WindowId,
        ) -> Result<(), BackendError> {
            self.dock_lifecycle
                .push(DockLifecycleEvent::RequestIconify(window));
            self.iconify_requests.push(window);
            self.iconify_observations.push((
                window,
                self.window_ops.get_geometry(window).unwrap_or_default(),
                self.focused.last().copied(),
            ));
            if self.fail_next_iconify.swap(false, Ordering::Relaxed) {
                return Err(BackendError::Message(
                    "injected Iconic admission failure".into(),
                ));
            }
            self.window_ops
                .force_unmapped
                .store(true, Ordering::Relaxed);
            Ok(())
        }

        fn compositor_cancel_window_iconify(
            &mut self,
            window: WindowId,
        ) -> Result<(), BackendError> {
            self.dock_lifecycle
                .push(DockLifecycleEvent::CancelIconify(window));
            self.iconify_cancels.push(window);
            self.iconify_cancel_observations.push((
                window,
                self.window_ops.get_geometry(window).unwrap_or_default(),
            ));
            if self.fail_next_iconify_cancel.swap(false, Ordering::Relaxed) {
                return Err(BackendError::Message("injected Iconic map failure".into()));
            }
            if !self
                .iconify_cancel_leaves_unmapped
                .swap(false, Ordering::Relaxed)
            {
                self.window_ops
                    .force_unmapped
                    .store(false, Ordering::Relaxed);
            }
            Ok(())
        }

        fn compositor_set_window_pip(&mut self, _window: WindowId, pip: bool) {
            self.property_ops
                .mode_events
                .lock()
                .expect("mode events lock")
                .push(ModeEvent::Pip(pip));
        }

        fn compositor_set_window_minimized(&mut self, window: WindowId, minimized: bool) {
            self.minimized.push((window, minimized));
        }

        fn compositor_ensure_minimized_window_visual(&mut self, window: WindowId) {
            self.dock_lifecycle
                .push(DockLifecycleEvent::EnsureStatic(window));
        }

        fn compositor_forget_minimized_window_visual(&mut self, window: WindowId) {
            self.dock_lifecycle
                .push(DockLifecycleEvent::ForgetStatic(window));
        }

        fn compositor_set_window_dock_geometry(
            &mut self,
            window: WindowId,
            target: Option<CompositorRect>,
        ) {
            self.dock_targets.push((window, target));
            self.dock_lifecycle
                .push(DockLifecycleEvent::Geometry(window, target));
        }

        fn compositor_set_minimized_window_preview(
            &mut self,
            window: Option<WindowId>,
            anchor: Option<CompositorRect>,
        ) {
            self.dock_lifecycle
                .push(DockLifecycleEvent::Preview(window, anchor));
        }
    }
    impl CompositorAnnotation for MinimizeSpyBackend {}
    impl DisplayControl for MinimizeSpyBackend {}
    impl RenderScheduler for MinimizeSpyBackend {
        fn has_compositor(&self) -> bool {
            true
        }
    }

    impl Backend for MinimizeSpyBackend {
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                supports_client_list: self.supports_client_list,
                ..Capabilities::default()
            }
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

        fn on_focused_client_changed(&mut self, win: Option<WindowId>) -> Result<(), BackendError> {
            if let Some(win) = win {
                self.window_ops.set_input_focus(win)?;
            } else {
                self.window_ops.set_input_focus_root()?;
            }
            if self.fail_next_focus.swap(false, Ordering::Relaxed) {
                return Err(BackendError::Message("injected focus failure".into()));
            }
            self.focused.push(win);
            Ok(())
        }
    }

    fn take_mode_events(backend: &MinimizeSpyBackend) -> Vec<ModeEvent> {
        std::mem::take(
            &mut *backend
                .property_ops
                .mode_events
                .lock()
                .expect("mode events lock"),
        )
    }

    fn client_rect(jwm: &Jwm, client_key: ClientKey) -> Rect {
        let client = &jwm.state.clients[client_key];
        Rect::new(
            client.geometry.x,
            client.geometry.y,
            client.geometry.w,
            client.geometry.h,
        )
    }

    fn add_mode_client(
        jwm: &mut Jwm,
        window: WindowId,
        geometry: Rect,
        is_floating: bool,
        is_sticky: bool,
    ) -> (crate::core::models::MonitorKey, ClientKey) {
        let monitor = jwm.state.monitor_order[0];
        let mut client = WMClient::new(window);
        client.mon = Some(monitor);
        client.state.tags = jwm.state.monitors[monitor].get_active_tags();
        client.state.is_floating = is_floating;
        client.state.is_sticky = is_sticky;
        client.geometry.x = geometry.x;
        client.geometry.y = geometry.y;
        client.geometry.w = geometry.w;
        client.geometry.h = geometry.h;
        client.geometry.border_w = 2;
        client.geometry.floating_x = geometry.x;
        client.geometry.floating_y = geometry.y;
        client.geometry.floating_w = geometry.w;
        client.geometry.floating_h = geometry.h;
        let client_key = jwm.insert_client(client);
        jwm.attach_to_monitor(client_key, monitor);
        jwm.state.monitors[monitor].set_selected_client_for_current_tag(Some(client_key));
        (monitor, client_key)
    }

    fn add_restart_hidden_client(
        jwm: &mut Jwm,
        backend: &MinimizeSpyBackend,
        window: WindowId,
    ) -> ClientKey {
        let monitor = jwm.state.monitor_order[0];
        let visible = Rect::new(320, 180, 720, 500);
        let hidden_x = hidden_x_left_of_desktop(jwm.desktop_left_edge(), visible.w);
        let mut client = WMClient::new(window);
        client.mon = Some(monitor);
        client.state.tags = jwm.state.monitors[monitor].get_active_tags();
        client.state.is_hidden = true;
        client.state.minimized_order = 91;
        client.geometry.x = hidden_x;
        client.geometry.y = visible.y;
        client.geometry.w = visible.w;
        client.geometry.h = visible.h;
        client.geometry.hidden_x = Some(hidden_x);
        client.geometry.hidden_restore_rect = Some(visible);
        let client_key = jwm.insert_client(client);
        jwm.attach_to_monitor(client_key, monitor);
        backend
            .window_ops
            .geometries
            .lock()
            .expect("window geometries lock")
            .insert(
                window,
                Geometry {
                    x: hidden_x,
                    y: visible.y,
                    w: visible.w as u32,
                    h: visible.h as u32,
                    border: 0,
                },
            );
        backend
            .window_ops
            .scanned_windows
            .lock()
            .expect("scanned windows lock")
            .push(window);
        backend
            .window_ops
            .force_unmapped
            .store(true, Ordering::Relaxed);
        client_key
    }

    #[test]
    fn setup_initial_windows_propagates_root_scan_failure() {
        let mut backend = MinimizeSpyBackend::new();
        backend.supports_client_list = true;
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        backend.window_ops.fail_scan.store(true, Ordering::Relaxed);

        let error = jwm
            .setup_initial_windows(&mut backend)
            .expect_err("a failed QueryTree must cancel adoption");
        assert!(error.to_string().contains("injected root scan failure"));
    }

    #[test]
    fn restart_preflight_fails_closed_when_v1_checked_write_fails() {
        let mut backend = MinimizeSpyBackend::new();
        backend.supports_client_list = true;
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let window = WindowId::from_raw(0x4f01);
        add_restart_hidden_client(&mut jwm, &backend, window);
        backend
            .property_ops
            .fail_restore_set
            .store(true, Ordering::Relaxed);

        let error = jwm
            .prepare_restart_clients(&mut backend)
            .expect_err("missing exact V1 must cancel restart");
        assert!(error.to_string().contains("V1 readback"));
        assert!(backend.iconify_cancels.is_empty());
        assert!(backend.window_ops.force_unmapped.load(Ordering::Relaxed));
    }

    #[test]
    fn restart_preflight_maps_only_an_ewmh_proven_unmapped_client() {
        let mut backend = MinimizeSpyBackend::new();
        backend.supports_client_list = true;
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let window = WindowId::from_raw(0x4f02);
        add_restart_hidden_client(&mut jwm, &backend, window);
        backend
            .property_ops
            .fail_wm_state
            .store(true, Ordering::Relaxed);
        backend
            .iconify_cancel_leaves_unmapped
            .store(true, Ordering::Relaxed);

        jwm.prepare_restart_clients(&mut backend)
            .expect("exact EWMH + V1 may use the mapped discoverability fallback");

        assert_eq!(backend.iconify_cancels, vec![window]);
        assert_eq!(
            *backend
                .window_ops
                .mapped_windows
                .lock()
                .expect("mapped windows lock"),
            vec![window],
            "a coordinator no-op must fall back to checked MapWindow"
        );
        assert!(backend.property_ops.ewmh_hidden.load(Ordering::Relaxed));
        assert!(
            backend
                .property_ops
                .minimized_restore
                .lock()
                .expect("minimized restore lock")
                .is_some()
        );
        assert!(!backend.window_ops.force_unmapped.load(Ordering::Relaxed));
        assert!(x11_geometry_fully_left_of_desktop(
            backend.window_ops.get_geometry(window).unwrap(),
            jwm.desktop_left_edge()
        ));
    }

    #[test]
    fn fullscreen_to_pip_exits_fullscreen_first_and_restores_floating_sticky_state() {
        let mut backend = MinimizeSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let area = jwm.monitor_work_area(monitor).expect("monitor work area");
        let source = Rect::new(area.x + 120, area.y + 90, 820, 610);
        let window = WindowId::from_raw(0x5040);
        let (_, client_key) = add_mode_client(&mut jwm, window, source, true, true);

        jwm.setfullscreen(&mut backend, client_key, true).unwrap();
        take_mode_events(&backend);

        assert!(jwm.set_client_pip(&mut backend, client_key, true).unwrap());
        let events = take_mode_events(&backend);
        let fullscreen_off = events
            .iter()
            .position(|event| *event == ModeEvent::Fullscreen(false))
            .expect("fullscreen-off event");
        let pip_on = events
            .iter()
            .position(|event| *event == ModeEvent::Pip(true))
            .expect("PiP-on event");
        assert!(fullscreen_off < pip_on, "events: {events:?}");
        let client = &jwm.state.clients[client_key];
        assert!(client.state.is_pip);
        assert!(!client.state.is_fullscreen);
        assert!(client.state.is_floating);
        assert!(client.state.is_sticky);
        assert!(client.state.pip_restore_sticky);

        assert!(jwm.set_client_pip(&mut backend, client_key, false).unwrap());
        assert_eq!(
            take_mode_events(&backend),
            vec![ModeEvent::Sticky(true), ModeEvent::Pip(false)]
        );
        let client = &jwm.state.clients[client_key];
        assert!(!client.state.is_pip);
        assert!(!client.state.is_fullscreen);
        assert!(client.state.is_floating);
        assert!(client.state.is_sticky);
        assert!(!client.state.pip_restore_sticky);
        assert_eq!(client_rect(&jwm, client_key), source);
    }

    #[test]
    fn pip_to_fullscreen_exits_pip_first_and_restores_tiled_nonsticky_state() {
        let mut backend = MinimizeSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let area = jwm.monitor_work_area(monitor).expect("monitor work area");
        let source = Rect::new(area.x + 80, area.y + 70, 760, 540);
        let window = WindowId::from_raw(0x5041);
        let (_, client_key) = add_mode_client(&mut jwm, window, source, false, false);

        assert!(jwm.set_client_pip(&mut backend, client_key, true).unwrap());
        take_mode_events(&backend);

        jwm.setfullscreen(&mut backend, client_key, true).unwrap();
        let events = take_mode_events(&backend);
        let pip_off = events
            .iter()
            .position(|event| *event == ModeEvent::Pip(false))
            .expect("PiP-off event");
        let fullscreen_on = events
            .iter()
            .position(|event| *event == ModeEvent::Fullscreen(true))
            .expect("fullscreen-on event");
        assert!(pip_off < fullscreen_on, "events: {events:?}");
        let client = &jwm.state.clients[client_key];
        assert!(!client.state.is_pip);
        assert!(client.state.is_fullscreen);
        assert!(client.state.is_floating);
        assert!(!client.state.pip_restore_sticky);

        jwm.setfullscreen(&mut backend, client_key, false).unwrap();
        let client = &jwm.state.clients[client_key];
        assert!(!client.state.is_pip);
        assert!(!client.state.is_fullscreen);
        assert!(!client.state.is_floating);
        assert!(!client.state.is_sticky);
        assert!(!client.state.pip_restore_sticky);
        assert_eq!(
            Rect::new(
                client.geometry.floating_x,
                client.geometry.floating_y,
                client.geometry.floating_w,
                client.geometry.floating_h,
            ),
            source,
            "PiP's semantic return slot must survive the intervening fullscreen cycle"
        );
    }

    #[test]
    fn pip_to_fullscreen_target_failures_restore_the_exact_original_mode() {
        for hidden in [false, true] {
            for failure in ["property", "configure", "stack"] {
                let mut backend = MinimizeSpyBackend::new();
                let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
                let monitor = jwm.state.monitor_order[0];
                let area = jwm.monitor_work_area(monitor).expect("monitor work area");
                let source = Rect::new(area.x + 91, area.y + 73, 777, 555);
                let window = WindowId::from_raw(0x5080 + u64::from(hidden));
                let (_, client_key) = add_mode_client(&mut jwm, window, source, false, false);
                assert!(jwm.set_client_pip(&mut backend, client_key, true).unwrap());
                if hidden {
                    assert!(
                        jwm.set_client_minimized(&mut backend, client_key, true)
                            .unwrap()
                    );
                }
                let before = jwm.state.clients[client_key].clone();
                let before_order = jwm.state.monitor_clients[monitor].clone();
                let before_v1 = *backend
                    .property_ops
                    .minimized_restore
                    .lock()
                    .expect("restore snapshot lock");
                take_mode_events(&backend);
                match failure {
                    "property" => backend
                        .property_ops
                        .fail_fullscreen_on
                        .store(true, Ordering::Relaxed),
                    "configure" => {
                        let monitor_geometry = &jwm.state.monitors[monitor].geometry;
                        *backend
                            .window_ops
                            .fail_configure_size
                            .lock()
                            .expect("configure failure size lock") =
                            Some((monitor_geometry.m_w as u32, monitor_geometry.m_h as u32));
                    }
                    "stack" => backend
                        .window_ops
                        .fail_apply_window_changes
                        .store(true, Ordering::Relaxed),
                    _ => unreachable!(),
                }

                assert!(
                    jwm.setfullscreen(&mut backend, client_key, true).is_err(),
                    "hidden={hidden} {failure}"
                );
                assert_eq!(
                    jwm.state.clients[client_key], before,
                    "hidden={hidden} {failure}"
                );
                assert_eq!(jwm.state.monitor_clients[monitor], before_order);
                assert!(!backend.property_ops.fullscreen.load(Ordering::Relaxed));
                assert!(backend.property_ops.sticky.load(Ordering::Relaxed));
                assert_eq!(
                    *backend
                        .property_ops
                        .minimized_restore
                        .lock()
                        .expect("restore snapshot lock"),
                    before_v1
                );
                let events = take_mode_events(&backend);
                assert!(events.contains(&ModeEvent::Pip(false)), "{events:?}");
                assert_eq!(events.last(), Some(&ModeEvent::Pip(true)), "{events:?}");
            }
        }
    }

    #[test]
    fn fullscreen_to_pip_target_failures_restore_the_exact_original_mode() {
        for hidden in [false, true] {
            for failure in ["property", "configure"] {
                let mut backend = MinimizeSpyBackend::new();
                let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
                let monitor = jwm.state.monitor_order[0];
                let area = jwm.monitor_work_area(monitor).expect("monitor work area");
                let source = Rect::new(area.x + 87, area.y + 69, 789, 567);
                let window = WindowId::from_raw(0x5090 + u64::from(hidden));
                let (_, client_key) = add_mode_client(&mut jwm, window, source, true, false);
                jwm.setfullscreen(&mut backend, client_key, true).unwrap();
                if hidden {
                    assert!(
                        jwm.set_client_minimized(&mut backend, client_key, true)
                            .unwrap()
                    );
                }
                let before = jwm.state.clients[client_key].clone();
                let before_order = jwm.state.monitor_clients[monitor].clone();
                let before_v1 = *backend
                    .property_ops
                    .minimized_restore
                    .lock()
                    .expect("restore snapshot lock");
                take_mode_events(&backend);
                match failure {
                    "property" => backend
                        .property_ops
                        .fail_sticky_on
                        .store(true, Ordering::Relaxed),
                    "configure" => {
                        let work = jwm.monitor_work_area(monitor).expect("monitor work area");
                        *backend
                            .window_ops
                            .fail_configure_size
                            .lock()
                            .expect("configure failure size lock") =
                            Some(((work.w / 4).max(1) as u32, (work.h / 4).max(1) as u32));
                    }
                    _ => unreachable!(),
                }

                assert!(
                    jwm.set_client_pip(&mut backend, client_key, true).is_err(),
                    "hidden={hidden} {failure}"
                );
                assert_eq!(
                    jwm.state.clients[client_key], before,
                    "hidden={hidden} {failure}"
                );
                assert_eq!(jwm.state.monitor_clients[monitor], before_order);
                assert!(backend.property_ops.fullscreen.load(Ordering::Relaxed));
                assert!(!backend.property_ops.sticky.load(Ordering::Relaxed));
                assert_eq!(
                    *backend
                        .property_ops
                        .minimized_restore
                        .lock()
                        .expect("restore snapshot lock"),
                    before_v1
                );
                let events = take_mode_events(&backend);
                assert!(events.contains(&ModeEvent::Fullscreen(false)), "{events:?}");
                assert_eq!(events.last(), Some(&ModeEvent::Pip(false)), "{events:?}");
            }
        }
    }

    #[test]
    fn hidden_pip_to_fullscreen_never_configures_onscreen_and_persists_fullscreen_semantics() {
        let mut backend = MinimizeSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let area = jwm.monitor_work_area(monitor).expect("monitor work area");
        let source = Rect::new(area.x + 140, area.y + 100, 840, 620);
        let fullscreen = Rect::new(
            jwm.state.monitors[monitor].geometry.m_x,
            jwm.state.monitors[monitor].geometry.m_y,
            jwm.state.monitors[monitor].geometry.m_w,
            jwm.state.monitors[monitor].geometry.m_h,
        );
        let desktop_left = jwm.desktop_left_edge();
        let window = WindowId::from_raw(0x5042);
        let (_, client_key) = add_mode_client(&mut jwm, window, source, true, false);

        assert!(jwm.set_client_pip(&mut backend, client_key, true).unwrap());
        assert!(
            jwm.set_client_minimized(&mut backend, client_key, true)
                .unwrap()
        );
        take_mode_events(&backend);
        backend
            .window_ops
            .configures
            .lock()
            .expect("window configures lock")
            .clear();

        jwm.setfullscreen(&mut backend, client_key, true).unwrap();

        let events = take_mode_events(&backend);
        let pip_off = events
            .iter()
            .position(|event| *event == ModeEvent::Pip(false))
            .expect("PiP-off event");
        let fullscreen_on = events
            .iter()
            .position(|event| *event == ModeEvent::Fullscreen(true))
            .expect("fullscreen-on event");
        assert!(pip_off < fullscreen_on, "events: {events:?}");
        let configures = backend
            .window_ops
            .configures
            .lock()
            .expect("window configures lock")
            .clone();
        assert!(!configures.is_empty());
        assert!(configures.iter().all(|(_, x, _, width, _, border)| {
            i64::from(*x) + i64::from(*width) + i64::from(*border).saturating_mul(2)
                <= i64::from(desktop_left)
        }));

        let client = &jwm.state.clients[client_key];
        assert!(client.state.is_hidden);
        assert!(client.state.is_fullscreen);
        assert!(!client.state.is_pip);
        assert_eq!(client.geometry.hidden_restore_rect, Some(fullscreen));
        assert_eq!(
            Rect::new(
                client.geometry.old_x,
                client.geometry.old_y,
                client.geometry.old_w,
                client.geometry.old_h,
            ),
            source
        );
        let snapshot = backend
            .property_ops
            .minimized_restore
            .lock()
            .expect("minimized restore lock")
            .expect("hidden fullscreen snapshot");
        assert!(!snapshot.is_pip);
        assert!(!snapshot.pip_restore_sticky);
        assert_eq!(
            snapshot.visible_rect,
            MinimizedRestoreRect {
                x: fullscreen.x,
                y: fullscreen.y,
                w: fullscreen.w,
                h: fullscreen.h,
            }
        );
        assert_eq!(
            snapshot.fullscreen_restore_rect,
            Some(MinimizedRestoreRect {
                x: source.x,
                y: source.y,
                w: source.w,
                h: source.h,
            })
        );

        assert!(
            jwm.set_client_minimized(&mut backend, client_key, false)
                .unwrap()
        );
        jwm.setfullscreen(&mut backend, client_key, false).unwrap();
        let client = &jwm.state.clients[client_key];
        assert!(!client.state.is_hidden);
        assert!(!client.state.is_fullscreen);
        assert!(!client.state.is_pip);
        assert!(client.state.is_floating);
        assert!(!client.state.is_sticky);
        assert_eq!(client_rect(&jwm, client_key), source);
    }

    #[test]
    fn pip_configure_failure_rolls_back_visible_and_hidden_enter_and_exit() {
        for hidden in [false, true] {
            let mut backend = MinimizeSpyBackend::new();
            let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
            let monitor = jwm.state.monitor_order[0];
            let area = jwm.monitor_work_area(monitor).expect("monitor work area");
            let source = Rect::new(area.x + 130, area.y + 95, 800, 590);
            let window = WindowId::from_raw(if hidden { 0x5044 } else { 0x5043 });
            let (_, client_key) = add_mode_client(&mut jwm, window, source, true, false);
            if hidden {
                assert!(
                    jwm.set_client_minimized(&mut backend, client_key, true)
                        .unwrap()
                );
            }

            let before_enter = jwm.state.clients[client_key].clone();
            let before_enter_order = jwm.state.monitor_clients[monitor].clone();
            let before_enter_animation = jwm.animations.active.contains_key(&client_key);
            take_mode_events(&backend);
            backend
                .window_ops
                .fail_configure
                .store(true, Ordering::Relaxed);

            assert!(jwm.set_client_pip(&mut backend, client_key, true).is_err());
            assert_eq!(jwm.state.clients[client_key], before_enter);
            assert_eq!(jwm.state.monitor_clients[monitor], before_enter_order);
            assert_eq!(
                jwm.animations.active.contains_key(&client_key),
                before_enter_animation
            );
            assert_eq!(
                take_mode_events(&backend),
                vec![ModeEvent::Sticky(true), ModeEvent::Sticky(false)]
            );

            backend
                .window_ops
                .fail_configure
                .store(false, Ordering::Relaxed);
            assert!(jwm.set_client_pip(&mut backend, client_key, true).unwrap());
            let before_exit = jwm.state.clients[client_key].clone();
            let before_exit_order = jwm.state.monitor_clients[monitor].clone();
            let before_exit_animation = jwm.animations.active.contains_key(&client_key);
            take_mode_events(&backend);
            backend
                .window_ops
                .fail_configure
                .store(true, Ordering::Relaxed);

            assert!(jwm.set_client_pip(&mut backend, client_key, false).is_err());
            assert_eq!(jwm.state.clients[client_key], before_exit);
            assert_eq!(jwm.state.monitor_clients[monitor], before_exit_order);
            assert_eq!(
                jwm.animations.active.contains_key(&client_key),
                before_exit_animation
            );
            assert_eq!(
                take_mode_events(&backend),
                vec![ModeEvent::Sticky(false), ModeEvent::Sticky(true)]
            );
        }
    }

    #[test]
    fn hidden_pip_snapshot_uses_the_size_hint_adjusted_configure_target() {
        let mut backend = MinimizeSpyBackend::new();
        *backend
            .property_ops
            .normal_hints
            .lock()
            .expect("normal hints lock") = Some(NormalHints {
            min_w: 700,
            min_h: 500,
            ..Default::default()
        });
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let area = jwm.monitor_work_area(monitor).expect("monitor work area");
        let source = Rect::new(area.x + 100, area.y + 80, 900, 650);
        let window = WindowId::from_raw(0x5045);
        let (_, client_key) = add_mode_client(&mut jwm, window, source, true, false);
        assert!(
            jwm.set_client_minimized(&mut backend, client_key, true)
                .unwrap()
        );
        backend
            .window_ops
            .configures
            .lock()
            .expect("window configures lock")
            .clear();

        assert!(jwm.set_client_pip(&mut backend, client_key, true).unwrap());

        let semantic = jwm.state.clients[client_key]
            .geometry
            .hidden_restore_rect
            .expect("PiP semantic target");
        assert_eq!((semantic.w, semantic.h), (700, 500));
        let configured = backend
            .window_ops
            .configures
            .lock()
            .expect("window configures lock")
            .last()
            .copied()
            .expect("PiP configure");
        assert_eq!(
            (configured.2, configured.3, configured.4),
            (semantic.y, semantic.w as u32, semantic.h as u32)
        );
        let snapshot = backend
            .property_ops
            .minimized_restore
            .lock()
            .expect("minimized restore lock")
            .expect("PiP snapshot");
        assert_eq!(
            snapshot.visible_rect,
            MinimizedRestoreRect {
                x: semantic.x,
                y: semantic.y,
                w: semantic.w,
                h: semantic.h,
            }
        );
    }

    #[test]
    fn only_the_selected_window_relinquishes_focus_when_minimised() {
        assert!(minimized_window_relinquishes_focus(true, true));
        assert!(!minimized_window_relinquishes_focus(true, false));
        assert!(!minimized_window_relinquishes_focus(false, true));
    }

    #[test]
    fn true_iconify_is_requested_only_after_parking_and_focus_relinquish() {
        let mut backend = MinimizeSpyBackend::new();
        backend.supports_client_list = true;
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let window = WindowId::from_raw(0x504c);
        let visible = Rect::new(260, 150, 740, 520);
        let (_, client_key) = add_mode_client(&mut jwm, window, visible, true, false);
        backend
            .window_ops
            .geometries
            .lock()
            .expect("window geometries lock")
            .insert(
                window,
                Geometry {
                    x: visible.x,
                    y: visible.y,
                    w: visible.w as u32,
                    h: visible.h as u32,
                    border: 0,
                },
            );

        assert!(
            jwm.set_client_minimized(&mut backend, client_key, true)
                .unwrap()
        );

        assert_eq!(backend.iconify_requests, vec![window]);
        let (_, geometry_at_request, focus_at_request) = backend.iconify_observations[0];
        assert!(x11_geometry_fully_left_of_desktop(
            geometry_at_request,
            jwm.desktop_left_edge()
        ));
        assert_eq!(
            focus_at_request,
            Some(None),
            "the selected client must relinquish focus before UnmapWindow"
        );
        assert!(
            backend.window_ops.force_unmapped.load(Ordering::Relaxed),
            "the spy models a physically Iconic client after admission"
        );
    }

    #[test]
    fn restore_reparks_unmapped_stale_geometry_before_public_deiconify() {
        let mut backend = MinimizeSpyBackend::new();
        backend.supports_client_list = true;
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let window = WindowId::from_raw(0x504a);
        let visible = Rect::new(280, 160, 720, 500);
        let (_, client_key) = add_mode_client(&mut jwm, window, visible, true, false);
        backend
            .window_ops
            .geometries
            .lock()
            .expect("window geometries lock")
            .insert(
                window,
                Geometry {
                    x: visible.x,
                    y: visible.y,
                    w: visible.w as u32,
                    h: visible.h as u32,
                    border: 0,
                },
            );
        assert!(
            jwm.set_client_minimized(&mut backend, client_key, true)
                .unwrap()
        );
        let order = jwm.state.clients[client_key].state.minimized_order;
        let snapshot = *backend
            .property_ops
            .minimized_restore
            .lock()
            .expect("minimized restore lock");

        // Model a hotplug-era stale server rectangle: JWM has already staged
        // a safe hidden target, but the physically unmapped X11 window still
        // carries an on-screen geometry because the first repark was lost.
        let stale = Geometry {
            x: visible.x,
            y: visible.y,
            w: visible.w as u32,
            h: visible.h as u32,
            border: 0,
        };
        backend
            .window_ops
            .geometries
            .lock()
            .expect("window geometries lock")
            .insert(window, stale);
        backend
            .window_ops
            .configures
            .lock()
            .expect("window configures lock")
            .clear();
        backend
            .window_ops
            .fail_next_configure
            .store(true, Ordering::Relaxed);

        assert!(
            jwm.set_client_minimized(&mut backend, client_key, false)
                .is_err(),
            "restore must stop before changing protocol state or mapping"
        );
        assert!(jwm.state.clients[client_key].state.is_hidden);
        assert_eq!(jwm.state.clients[client_key].state.minimized_order, order);
        assert!(backend.property_ops.ewmh_hidden.load(Ordering::Relaxed));
        assert_eq!(
            backend.property_ops.wm_state.load(Ordering::Relaxed),
            i64::from(crate::backend::api::ICCCM_ICONIC_STATE)
        );
        assert_eq!(
            *backend
                .property_ops
                .minimized_restore
                .lock()
                .expect("minimized restore lock"),
            snapshot
        );
        assert!(backend.iconify_cancels.is_empty());
        assert!(backend.iconify_cancel_observations.is_empty());
        assert!(backend.window_ops.force_unmapped.load(Ordering::Relaxed));
        let actual = backend.window_ops.get_geometry(window).unwrap();
        assert_eq!(
            (actual.x, actual.y, actual.w, actual.h, actual.border),
            (stale.x, stale.y, stale.w, stale.h, stale.border)
        );

        assert!(
            jwm.set_client_minimized(&mut backend, client_key, false)
                .unwrap()
        );
        assert!(!jwm.state.clients[client_key].state.is_hidden);
        assert!(!backend.property_ops.ewmh_hidden.load(Ordering::Relaxed));
        assert_eq!(backend.iconify_cancels, vec![window]);
        let (observed_window, geometry_before_map) = backend.iconify_cancel_observations[0];
        assert_eq!(observed_window, window);
        assert!(x11_geometry_fully_left_of_desktop(
            geometry_before_map,
            jwm.desktop_left_edge()
        ));
        assert_eq!(
            (geometry_before_map.w, geometry_before_map.h),
            (visible.w as u32, visible.h as u32)
        );
    }

    #[test]
    fn failed_true_deiconify_keeps_the_same_retryable_dock_incarnation() {
        let mut backend = MinimizeSpyBackend::new();
        backend.supports_client_list = true;
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let window = WindowId::from_raw(0x504d);
        let visible = Rect::new(300, 180, 700, 500);
        let (_, client_key) = add_mode_client(&mut jwm, window, visible, true, false);
        backend
            .window_ops
            .geometries
            .lock()
            .expect("window geometries lock")
            .insert(
                window,
                Geometry {
                    x: visible.x,
                    y: visible.y,
                    w: visible.w as u32,
                    h: visible.h as u32,
                    border: 0,
                },
            );
        assert!(
            jwm.set_client_minimized(&mut backend, client_key, true)
                .unwrap()
        );
        let order = jwm.state.clients[client_key].state.minimized_order;
        let snapshot = *backend
            .property_ops
            .minimized_restore
            .lock()
            .expect("minimized restore lock");

        backend
            .fail_next_iconify_cancel
            .store(true, Ordering::Relaxed);
        assert!(
            jwm.set_client_minimized(&mut backend, client_key, false)
                .is_err()
        );
        assert!(jwm.state.clients[client_key].state.is_hidden);
        assert_eq!(jwm.state.clients[client_key].state.minimized_order, order);
        assert!(backend.property_ops.ewmh_hidden.load(Ordering::Relaxed));
        assert_eq!(
            backend.property_ops.wm_state.load(Ordering::Relaxed),
            i64::from(crate::backend::api::ICCCM_ICONIC_STATE)
        );
        assert_eq!(
            *backend
                .property_ops
                .minimized_restore
                .lock()
                .expect("minimized restore lock"),
            snapshot
        );
        assert_eq!(backend.iconify_cancels, vec![window]);
        assert_eq!(backend.iconify_requests, vec![window, window]);
        assert_eq!(backend.minimized, vec![(window, true)]);

        assert!(
            jwm.set_client_minimized(&mut backend, client_key, false)
                .unwrap()
        );
        assert!(!jwm.state.clients[client_key].state.is_hidden);
        assert_eq!(backend.iconify_cancels, vec![window, window]);
        assert_eq!(backend.minimized, vec![(window, true), (window, false)]);
    }

    #[test]
    fn deiconify_does_not_commit_until_the_client_is_viewable() {
        let mut backend = MinimizeSpyBackend::new();
        backend.supports_client_list = true;
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let window = WindowId::from_raw(0x504b);
        let visible = Rect::new(220, 140, 680, 480);
        let (_, client_key) = add_mode_client(&mut jwm, window, visible, true, false);
        backend
            .window_ops
            .geometries
            .lock()
            .expect("window geometries lock")
            .insert(
                window,
                Geometry {
                    x: visible.x,
                    y: visible.y,
                    w: visible.w as u32,
                    h: visible.h as u32,
                    border: 0,
                },
            );
        assert!(
            jwm.set_client_minimized(&mut backend, client_key, true)
                .unwrap()
        );
        let order = jwm.state.clients[client_key].state.minimized_order;

        backend
            .iconify_cancel_leaves_unmapped
            .store(true, Ordering::Relaxed);
        assert!(
            jwm.set_client_minimized(&mut backend, client_key, false)
                .is_err()
        );
        assert!(jwm.state.clients[client_key].state.is_hidden);
        assert_eq!(jwm.state.clients[client_key].state.minimized_order, order);
        assert!(backend.property_ops.ewmh_hidden.load(Ordering::Relaxed));
        assert_eq!(backend.iconify_requests, vec![window, window]);
        assert_eq!(backend.minimized, vec![(window, true)]);

        assert!(
            jwm.set_client_minimized(&mut backend, client_key, false)
                .unwrap()
        );
        assert!(!jwm.state.clients[client_key].state.is_hidden);
    }

    #[test]
    fn restore_focus_failure_rolls_back_to_a_retryable_dock_item() {
        let mut backend = MinimizeSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let window = WindowId::from_raw(0x504f);
        let fallback_window = WindowId::from_raw(0x504e);
        let visible = Rect::new(280, 170, 760, 540);
        let mut fallback = WMClient::new(fallback_window);
        fallback.mon = Some(monitor);
        fallback.state.tags = jwm.state.monitors[monitor].get_active_tags();
        fallback.state.is_floating = true;
        fallback.geometry.x = 80;
        fallback.geometry.y = 90;
        fallback.geometry.w = 500;
        fallback.geometry.h = 360;
        let fallback_key = jwm.insert_client(fallback);
        jwm.attach_to_monitor(fallback_key, monitor);
        let mut client = WMClient::new(window);
        client.mon = Some(monitor);
        client.state.tags = jwm.state.monitors[monitor].get_active_tags();
        client.state.is_floating = true;
        client.state.is_urgent = true;
        client.geometry.x = visible.x;
        client.geometry.y = visible.y;
        client.geometry.w = visible.w;
        client.geometry.h = visible.h;
        client.geometry.floating_x = visible.x;
        client.geometry.floating_y = visible.y;
        client.geometry.floating_w = visible.w;
        client.geometry.floating_h = visible.h;
        let client_key = jwm.insert_client(client);
        jwm.attach_to_monitor(client_key, monitor);
        jwm.state.monitors[monitor].set_selected_client_for_current_tag(Some(client_key));

        assert!(
            jwm.set_client_minimized(&mut backend, client_key, true)
                .unwrap()
        );
        assert_eq!(jwm.get_selected_client_key(), Some(fallback_key));
        let order = jwm.state.clients[client_key].state.minimized_order;
        let snapshot = backend
            .property_ops
            .minimized_restore
            .lock()
            .expect("minimized restore lock")
            .expect("minimized snapshot");
        let previous_monitor_stack = jwm.state.monitor_stack[monitor].clone();
        let previous_selected_monitor = jwm.state.sel_mon;
        backend
            .window_ops
            .decoration_updates
            .lock()
            .expect("decoration updates lock")
            .clear();
        backend
            .window_ops
            .button_ungrabs
            .lock()
            .expect("button ungrabs lock")
            .clear();
        backend
            .window_ops
            .passive_button_grabs
            .lock()
            .expect("passive button grabs lock")
            .clear();
        backend
            .window_ops
            .input_focus_attempts
            .lock()
            .expect("input focus attempts lock")
            .clear();
        backend
            .property_ops
            .urgent_writes
            .lock()
            .expect("urgent writes lock")
            .clear();

        backend
            .window_ops
            .fail_next_input_focus
            .store(true, Ordering::Relaxed);
        assert!(
            jwm.set_client_minimized(&mut backend, client_key, false)
                .is_err(),
            "a failed activation must be observable to the external caller"
        );
        let client = &jwm.state.clients[client_key];
        assert!(client.state.is_hidden);
        assert!(client.state.is_urgent);
        assert_eq!(client.state.minimized_order, order);
        assert_eq!(client.geometry.hidden_restore_rect, Some(visible));
        assert_eq!(jwm.get_selected_client_key(), Some(fallback_key));
        assert_eq!(jwm.state.sel_mon, previous_selected_monitor);
        assert_eq!(jwm.state.monitor_stack[monitor], previous_monitor_stack);
        assert_eq!(
            jwm.state.monitors[monitor].get_selected_client_for_current_tag(),
            Some(fallback_key)
        );
        assert_eq!(
            backend
                .property_ops
                .urgent_writes
                .lock()
                .expect("urgent writes lock")
                .as_slice(),
            &[(window, false), (window, true)],
            "failed focus must restore both JWM urgency and the WM_HINTS source bit"
        );
        assert_eq!(
            backend
                .window_ops
                .decoration_updates
                .lock()
                .expect("decoration updates lock")
                .as_slice(),
            &[fallback_window, window, window, fallback_window],
            "the partial selected decoration must be normalized before the fallback is refocused"
        );
        assert_eq!(
            backend
                .window_ops
                .button_ungrabs
                .lock()
                .expect("button ungrabs lock")
                .as_slice(),
            &[fallback_window, window, window, fallback_window]
        );
        assert_eq!(
            backend
                .window_ops
                .passive_button_grabs
                .lock()
                .expect("passive button grabs lock")
                .as_slice(),
            &[fallback_window, window],
            "rollback must leave the hidden target passive and restore focused grabs on the fallback"
        );
        assert_eq!(
            backend
                .window_ops
                .input_focus_attempts
                .lock()
                .expect("input focus attempts lock")
                .as_slice(),
            &[window, fallback_window],
            "the failed target SetInputFocus must be repaired by refocusing the previous client"
        );
        assert!(backend.property_ops.ewmh_hidden.load(Ordering::Relaxed));
        assert_eq!(
            backend.property_ops.wm_state.load(Ordering::Relaxed),
            i64::from(crate::backend::api::ICCCM_ICONIC_STATE)
        );
        assert_eq!(
            *backend
                .property_ops
                .minimized_restore
                .lock()
                .expect("minimized restore lock"),
            Some(snapshot)
        );
        assert_eq!(
            backend.minimized,
            vec![(window, true)],
            "reverse Genie must not start before focus/restack commits"
        );

        assert!(
            jwm.set_client_minimized(&mut backend, client_key, false)
                .unwrap()
        );
        let client = &jwm.state.clients[client_key];
        assert!(!client.state.is_hidden);
        assert!(!client.state.is_urgent);
        assert_eq!(client.state.minimized_order, 0);
        assert_eq!(jwm.get_selected_client_key(), Some(client_key));
        assert_eq!(backend.minimized, vec![(window, true), (window, false)]);
        assert!(!backend.property_ops.ewmh_hidden.load(Ordering::Relaxed));
        assert_eq!(
            backend.property_ops.wm_state.load(Ordering::Relaxed),
            i64::from(crate::backend::api::ICCCM_NORMAL_STATE)
        );
        assert!(
            backend
                .property_ops
                .minimized_restore
                .lock()
                .expect("minimized restore lock")
                .is_none()
        );
    }

    #[test]
    fn restore_restack_failure_rolls_back_after_focus_and_retries() {
        let mut backend = MinimizeSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let window = WindowId::from_raw(0x5052);
        let fallback_window = WindowId::from_raw(0x5053);
        let visible = Rect::new(260, 150, 740, 520);

        let mut fallback = WMClient::new(fallback_window);
        fallback.mon = Some(monitor);
        fallback.state.tags = jwm.state.monitors[monitor].get_active_tags();
        fallback.state.is_floating = true;
        fallback.geometry.x = 70;
        fallback.geometry.y = 80;
        fallback.geometry.w = 480;
        fallback.geometry.h = 340;
        let fallback_key = jwm.insert_client(fallback);
        jwm.attach_to_monitor(fallback_key, monitor);

        let mut client = WMClient::new(window);
        client.mon = Some(monitor);
        client.state.tags = jwm.state.monitors[monitor].get_active_tags();
        client.state.is_floating = true;
        client.state.is_urgent = true;
        client.geometry.x = visible.x;
        client.geometry.y = visible.y;
        client.geometry.w = visible.w;
        client.geometry.h = visible.h;
        client.geometry.floating_x = visible.x;
        client.geometry.floating_y = visible.y;
        client.geometry.floating_w = visible.w;
        client.geometry.floating_h = visible.h;
        let client_key = jwm.insert_client(client);
        jwm.attach_to_monitor(client_key, monitor);
        jwm.state.monitors[monitor].set_selected_client_for_current_tag(Some(client_key));

        assert!(
            jwm.set_client_minimized(&mut backend, client_key, true)
                .unwrap()
        );
        let order = jwm.state.clients[client_key].state.minimized_order;
        let snapshot = backend
            .property_ops
            .minimized_restore
            .lock()
            .expect("minimized restore lock")
            .expect("minimized snapshot");

        // Restore first restacks the newly visible scene from arrange, then
        // restacks again after selecting the requested window. Fail the
        // latter to exercise rollback after focus has fully committed.
        backend
            .window_ops
            .fail_restack_countdown
            .store(2, Ordering::Relaxed);
        assert!(
            jwm.set_client_minimized(&mut backend, client_key, false)
                .is_err(),
            "a failed final restack must keep the Dock action retryable"
        );

        let client = &jwm.state.clients[client_key];
        assert!(client.state.is_hidden);
        assert!(client.state.is_urgent);
        assert_eq!(client.state.minimized_order, order);
        assert_eq!(client.geometry.hidden_restore_rect, Some(visible));
        assert_eq!(jwm.get_selected_client_key(), Some(fallback_key));
        assert!(backend.property_ops.ewmh_hidden.load(Ordering::Relaxed));
        assert_eq!(
            backend.property_ops.wm_state.load(Ordering::Relaxed),
            i64::from(crate::backend::api::ICCCM_ICONIC_STATE)
        );
        assert_eq!(
            *backend
                .property_ops
                .minimized_restore
                .lock()
                .expect("minimized restore lock"),
            Some(snapshot)
        );
        assert_eq!(
            backend.minimized,
            vec![(window, true)],
            "a failed restack must not release the retained Dock visual"
        );

        assert!(
            jwm.set_client_minimized(&mut backend, client_key, false)
                .unwrap()
        );
        assert!(!jwm.state.clients[client_key].state.is_hidden);
        assert_eq!(jwm.get_selected_client_key(), Some(client_key));
        assert_eq!(backend.minimized, vec![(window, true), (window, false)]);
    }

    #[test]
    fn failed_x11_minimize_parking_retries_without_replaying_the_genie() {
        let mut backend = MinimizeSpyBackend::new();
        backend.supports_client_list = true;
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let window = WindowId::from_raw(0x5051);
        let visible = Rect::new(330, 190, 720, 510);
        let mut client = WMClient::new(window);
        client.mon = Some(monitor);
        client.state.tags = jwm.state.monitors[monitor].get_active_tags();
        client.state.is_floating = true;
        client.geometry.x = visible.x;
        client.geometry.y = visible.y;
        client.geometry.w = visible.w;
        client.geometry.h = visible.h;
        let client_key = jwm.insert_client(client);
        jwm.attach_to_monitor(client_key, monitor);
        jwm.state.monitors[monitor].set_selected_client_for_current_tag(Some(client_key));
        backend
            .window_ops
            .geometries
            .lock()
            .expect("window geometries lock")
            .insert(
                window,
                Geometry {
                    x: visible.x,
                    y: visible.y,
                    w: visible.w.max(1) as u32,
                    h: visible.h.max(1) as u32,
                    border: 0,
                },
            );
        backend
            .window_ops
            .fail_next_position
            .store(true, Ordering::Relaxed);

        assert!(
            jwm.set_client_minimized(&mut backend, client_key, true)
                .is_err(),
            "a Hidden client that still owns an on-screen X11 input region must not report success"
        );
        let order = jwm.state.clients[client_key].state.minimized_order;
        assert_ne!(order, 0);
        assert!(jwm.state.clients[client_key].state.is_hidden);
        assert_eq!(backend.minimized, vec![(window, true)]);
        assert!(
            !x11_geometry_fully_left_of_desktop(
                backend.window_ops.get_geometry(window).unwrap(),
                jwm.desktop_left_edge()
            ),
            "the injected transport failure must leave the real window visible"
        );

        backend
            .window_ops
            .fail_next_configure
            .store(true, Ordering::Relaxed);
        assert!(
            jwm.set_client_minimized(&mut backend, client_key, true)
                .is_err(),
            "a failed checked parking retry must remain observable"
        );
        assert_eq!(jwm.state.clients[client_key].state.minimized_order, order);
        assert_eq!(
            backend.minimized,
            vec![(window, true)],
            "parking retry must not replay the forward Genie"
        );
        assert!(
            !jwm.set_client_minimized(&mut backend, client_key, true)
                .unwrap(),
            "the retry repairs a side effect, not the semantic Hidden transition"
        );
        assert_eq!(jwm.state.clients[client_key].state.minimized_order, order);
        assert_eq!(backend.minimized, vec![(window, true)]);
        assert!(x11_geometry_fully_left_of_desktop(
            backend.window_ops.get_geometry(window).unwrap(),
            jwm.desktop_left_edge()
        ));
    }

    #[test]
    fn idempotent_minimize_reparks_left_of_an_expanded_desktop_without_a_new_order_or_genie() {
        let mut backend = MinimizeSpyBackend::new();
        backend.supports_client_list = true;
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let window = WindowId::from_raw(0x5054);
        let visible = Rect::new(310, 180, 700, 500);
        let (_, client_key) = add_mode_client(&mut jwm, window, visible, true, false);
        backend
            .window_ops
            .geometries
            .lock()
            .expect("window geometries lock")
            .insert(
                window,
                Geometry {
                    x: visible.x,
                    y: visible.y,
                    w: visible.w as u32,
                    h: visible.h as u32,
                    border: 0,
                },
            );

        assert!(
            jwm.set_client_minimized(&mut backend, client_key, true)
                .unwrap()
        );
        let order = jwm.state.clients[client_key].state.minimized_order;
        let old_hidden_x = jwm.state.clients[client_key]
            .geometry
            .hidden_x
            .expect("initial hidden x");

        // Model a new output appearing to the left of the old parking area.
        // The stored hidden coordinate is now inside the desktop and must not
        // be reused by the idempotent parking repair.
        jwm.state.monitors[monitor].geometry.m_x = old_hidden_x.saturating_sub(100);
        assert!(!x11_geometry_fully_left_of_desktop(
            backend.window_ops.get_geometry(window).unwrap(),
            jwm.desktop_left_edge()
        ));

        assert!(
            !jwm.set_client_minimized(&mut backend, client_key, true)
                .unwrap(),
            "repairing physical parking is not a new semantic minimize"
        );
        assert_eq!(jwm.state.clients[client_key].state.minimized_order, order);
        assert_eq!(backend.minimized, vec![(window, true)]);
        assert!(x11_geometry_fully_left_of_desktop(
            backend.window_ops.get_geometry(window).unwrap(),
            jwm.desktop_left_edge()
        ));
    }

    #[test]
    fn fullscreen_minimize_restore_then_exit_keeps_the_pre_fullscreen_geometry() {
        let mut backend = MinimizeSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let window = WindowId::from_raw(0x5050);
        let pre_fullscreen = crate::core::types::Rect::new(240, 120, 960, 720);
        let mut client = WMClient::new(window);
        client.mon = Some(monitor);
        client.state.tags = jwm.state.monitors[monitor].get_active_tags();
        client.state.is_floating = true;
        client.geometry.x = pre_fullscreen.x;
        client.geometry.y = pre_fullscreen.y;
        client.geometry.w = pre_fullscreen.w;
        client.geometry.h = pre_fullscreen.h;
        client.geometry.border_w = 2;
        let client_key = jwm.insert_client(client);
        jwm.attach_to_monitor(client_key, monitor);
        jwm.state.monitors[monitor].set_selected_client_for_current_tag(Some(client_key));

        jwm.setfullscreen(&mut backend, client_key, true).unwrap();
        assert!(
            jwm.set_client_minimized(&mut backend, client_key, true)
                .unwrap()
        );

        let persisted = backend
            .property_ops
            .minimized_restore
            .lock()
            .expect("minimized restore lock")
            .expect("fullscreen minimize snapshot");
        assert_eq!(
            persisted.visible_rect,
            MinimizedRestoreRect {
                x: jwm.state.monitors[monitor].geometry.m_x,
                y: jwm.state.monitors[monitor].geometry.m_y,
                w: jwm.state.monitors[monitor].geometry.m_w,
                h: jwm.state.monitors[monitor].geometry.m_h,
            }
        );
        assert_eq!(
            persisted.fullscreen_restore_rect,
            Some(MinimizedRestoreRect {
                x: pre_fullscreen.x,
                y: pre_fullscreen.y,
                w: pre_fullscreen.w,
                h: pre_fullscreen.h,
            })
        );

        let fullscreen_rect = jwm.state.clients[client_key]
            .geometry
            .hidden_restore_rect
            .expect("minimize must retain the visible fullscreen rectangle");
        assert_eq!(
            crate::core::types::Rect::new(
                jwm.state.clients[client_key].geometry.old_x,
                jwm.state.clients[client_key].geometry.old_y,
                jwm.state.clients[client_key].geometry.old_w,
                jwm.state.clients[client_key].geometry.old_h,
            ),
            pre_fullscreen,
            "hiding must not borrow fullscreen's return slot"
        );

        assert!(
            jwm.set_client_minimized(&mut backend, client_key, false)
                .unwrap()
        );
        assert_eq!(
            crate::core::types::Rect::new(
                jwm.state.clients[client_key].geometry.x,
                jwm.state.clients[client_key].geometry.y,
                jwm.state.clients[client_key].geometry.w,
                jwm.state.clients[client_key].geometry.h,
            ),
            fullscreen_rect
        );

        jwm.setfullscreen(&mut backend, client_key, false).unwrap();
        assert_eq!(
            crate::core::types::Rect::new(
                jwm.state.clients[client_key].geometry.x,
                jwm.state.clients[client_key].geometry.y,
                jwm.state.clients[client_key].geometry.w,
                jwm.state.clients[client_key].geometry.h,
            ),
            pre_fullscreen
        );
    }

    #[test]
    fn hidden_fullscreen_exit_refreshes_the_restart_snapshot_immediately() {
        let mut backend = MinimizeSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let window = WindowId::from_raw(0x5051);
        let before_fullscreen = Rect::new(260, 145, 920, 680);
        let mut client = WMClient::new(window);
        client.mon = Some(monitor);
        client.state.tags = jwm.state.monitors[monitor].get_active_tags();
        client.state.is_floating = true;
        client.geometry.x = before_fullscreen.x;
        client.geometry.y = before_fullscreen.y;
        client.geometry.w = before_fullscreen.w;
        client.geometry.h = before_fullscreen.h;
        client.geometry.floating_x = before_fullscreen.x;
        client.geometry.floating_y = before_fullscreen.y;
        client.geometry.floating_w = before_fullscreen.w;
        client.geometry.floating_h = before_fullscreen.h;
        let client_key = jwm.insert_client(client);
        jwm.attach_to_monitor(client_key, monitor);

        jwm.setfullscreen(&mut backend, client_key, true).unwrap();
        assert!(
            jwm.set_client_minimized(&mut backend, client_key, true)
                .unwrap()
        );
        let order = jwm.state.clients[client_key].state.minimized_order;
        let fullscreen_snapshot = backend
            .property_ops
            .minimized_restore
            .lock()
            .expect("minimized restore lock")
            .expect("fullscreen snapshot");
        assert!(fullscreen_snapshot.fullscreen_restore_rect.is_some());

        jwm.setfullscreen(&mut backend, client_key, false).unwrap();

        let refreshed = backend
            .property_ops
            .minimized_restore
            .lock()
            .expect("minimized restore lock")
            .expect("refreshed hidden snapshot");
        assert_eq!(refreshed.minimized_order, order);
        assert_eq!(
            refreshed.visible_rect,
            MinimizedRestoreRect {
                x: before_fullscreen.x,
                y: before_fullscreen.y,
                w: before_fullscreen.w,
                h: before_fullscreen.h,
            }
        );
        assert!(refreshed.is_floating);
        assert_eq!(refreshed.fullscreen_restore_rect, None);

        assert!(
            jwm.set_client_minimized(&mut backend, client_key, false)
                .unwrap()
        );
        let client = &jwm.state.clients[client_key];
        assert_eq!(
            Rect::new(
                client.geometry.x,
                client.geometry.y,
                client.geometry.w,
                client.geometry.h,
            ),
            before_fullscreen
        );
    }

    #[test]
    fn skip_taskbar_before_minimize_hides_without_creating_a_dock_ghost() {
        let mut backend = MinimizeSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let monitor_num = jwm.state.monitors[monitor].num;
        let shelf = CompositorRect::new(100.0, 200.0, 80.0, 40.0);
        jwm.minimized_dock_shelves.insert(monitor_num, shelf);

        let window = WindowId::from_raw(0x5150);
        let mut client = WMClient::new(window);
        client.mon = Some(monitor);
        client.state.tags = 1;
        client.state.skip_taskbar = true;
        client.geometry.x = 80;
        client.geometry.y = 90;
        client.geometry.w = 640;
        client.geometry.h = 480;
        let client_key = jwm.insert_client(client);
        jwm.attach_to_monitor(client_key, monitor);

        assert!(
            jwm.set_client_minimized(&mut backend, client_key, true)
                .unwrap()
        );

        assert!(jwm.state.clients[client_key].state.is_hidden);
        assert_eq!(backend.dock_targets, vec![(window, None)]);
        assert!(backend.minimized.is_empty());
        assert!(StatusBarBuilder::get_minimized_windows(
            &jwm.state.clients,
            &[client_key],
            monitor_num,
        )
        .is_empty());
    }

    #[test]
    fn first_minimize_persists_semantic_state_once_and_restore_clears_it() {
        let mut backend = MinimizeSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let monitor_num = jwm.state.monitors[monitor].num;
        let window = WindowId::from_raw(0x6060);
        let mut client = WMClient::new(window);
        client.mon = Some(monitor);
        client.state.tags = 0b101;
        client.state.is_floating = true;
        client.state.is_drag_floating = true;
        client.state.old_state = true;
        client.geometry.x = -320;
        client.geometry.y = 140;
        client.geometry.w = 900;
        client.geometry.h = 650;
        client.geometry.floating_x = -280;
        client.geometry.floating_y = 160;
        client.geometry.floating_w = 860;
        client.geometry.floating_h = 610;
        let client_key = jwm.insert_client(client);
        jwm.attach_to_monitor(client_key, monitor);

        assert!(
            jwm.set_client_minimized(&mut backend, client_key, true)
                .unwrap()
        );
        let order = jwm.state.clients[client_key].state.minimized_order;
        assert_ne!(order, 0);
        let expected = MinimizedRestoreState {
            tags: 0b101,
            monitor_num,
            visible_rect: MinimizedRestoreRect {
                x: -320,
                y: 140,
                w: 900,
                h: 650,
            },
            is_floating: true,
            is_drag_floating: true,
            floating_rect: Some(MinimizedRestoreRect {
                x: -280,
                y: 160,
                w: 860,
                h: 610,
            }),
            is_pip: false,
            pip_restore_sticky: false,
            old_state: true,
            fullscreen_restore_rect: None,
            minimized_order: order,
        };
        assert_eq!(
            *backend
                .property_ops
                .minimized_restore
                .lock()
                .expect("minimized restore lock"),
            Some(expected)
        );
        assert_eq!(
            *backend
                .property_ops
                .minimized_restore_writes
                .lock()
                .expect("minimized restore writes lock"),
            vec![Some(expected)]
        );

        assert!(
            !jwm.set_client_minimized(&mut backend, client_key, true)
                .unwrap(),
            "an idempotent minimize must not replace the original snapshot"
        );
        assert_eq!(
            *backend
                .property_ops
                .minimized_restore_writes
                .lock()
                .expect("minimized restore writes lock"),
            vec![Some(expected)]
        );

        assert!(
            jwm.set_client_minimized(&mut backend, client_key, false)
                .unwrap()
        );
        assert_eq!(
            *backend
                .property_ops
                .minimized_restore
                .lock()
                .expect("minimized restore lock"),
            None
        );
        assert_eq!(
            *backend
                .property_ops
                .minimized_restore_writes
                .lock()
                .expect("minimized restore writes lock"),
            vec![Some(expected), None]
        );
    }

    #[test]
    fn idempotent_legacy_hidden_minimize_repairs_zero_order_once() {
        let mut backend = MinimizeSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let monitor_num = jwm.state.monitors[monitor].num;
        let window = WindowId::from_raw(0x6060_0001);
        let visible = Rect::new(260, 180, 700, 500);
        let mut client = WMClient::new(window);
        client.mon = Some(monitor);
        client.state.tags = 1;
        client.state.is_hidden = true;
        client.state.minimized_order = 0;
        client.geometry.x = hidden_x_left_of_desktop(jwm.desktop_left_edge(), visible.w);
        client.geometry.y = visible.y;
        client.geometry.w = visible.w;
        client.geometry.h = visible.h;
        client.geometry.hidden_x = Some(client.geometry.x);
        client.geometry.hidden_restore_rect = Some(visible);
        let client_key = jwm.insert_client(client);
        jwm.attach_to_monitor(client_key, monitor);

        assert!(
            !jwm.set_client_minimized(&mut backend, client_key, true)
                .unwrap(),
            "the hidden bit is already correct"
        );
        let repaired_order = jwm.state.clients[client_key].state.minimized_order;
        assert_ne!(repaired_order, 0);
        let expected = MinimizedRestoreState {
            tags: 1,
            monitor_num,
            visible_rect: MinimizedRestoreRect {
                x: visible.x,
                y: visible.y,
                w: visible.w,
                h: visible.h,
            },
            is_floating: false,
            is_drag_floating: false,
            floating_rect: None,
            is_pip: false,
            pip_restore_sticky: false,
            old_state: false,
            fullscreen_restore_rect: None,
            minimized_order: repaired_order,
        };
        assert_eq!(
            backend
                .property_ops
                .minimized_restore_writes
                .lock()
                .expect("minimized restore writes lock")
                .as_slice(),
            &[Some(expected)]
        );
        assert!(jwm.pending_bar_updates.contains(&monitor_num));

        assert!(
            !jwm.set_client_minimized(&mut backend, client_key, true)
                .unwrap()
        );
        assert_eq!(
            jwm.state.clients[client_key].state.minimized_order, repaired_order,
            "the repaired incarnation must remain stable"
        );
        assert_eq!(
            backend
                .property_ops
                .minimized_restore_writes
                .lock()
                .expect("minimized restore writes lock")
                .as_slice(),
            &[Some(expected)],
            "repeated idempotent requests must not consume another order"
        );

        // Adoption and hotplug persistence are intentionally best-effort. If
        // such a write was lost, the next ordinary idempotent request must
        // restore V1 without changing the Dock incarnation or replaying the
        // compositor transition.
        *backend
            .property_ops
            .minimized_restore
            .lock()
            .expect("minimized restore lock") = None;
        assert!(
            !jwm.set_client_minimized(&mut backend, client_key, true)
                .unwrap()
        );
        assert_eq!(
            jwm.state.clients[client_key].state.minimized_order,
            repaired_order
        );
        assert_eq!(
            backend
                .property_ops
                .minimized_restore_writes
                .lock()
                .expect("minimized restore writes lock")
                .as_slice(),
            &[Some(expected), Some(expected)]
        );
    }

    #[test]
    fn zero_order_repair_survives_a_later_protocol_failure() {
        let mut backend = MinimizeSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let window = WindowId::from_raw(0x6060_0002);
        let visible = Rect::new(310, 220, 640, 460);
        let mut client = WMClient::new(window);
        client.mon = Some(monitor);
        client.state.tags = 1;
        client.state.is_hidden = true;
        client.geometry.x = hidden_x_left_of_desktop(jwm.desktop_left_edge(), visible.w);
        client.geometry.y = visible.y;
        client.geometry.w = visible.w;
        client.geometry.h = visible.h;
        client.geometry.hidden_restore_rect = Some(visible);
        let client_key = jwm.insert_client(client);
        jwm.attach_to_monitor(client_key, monitor);

        backend
            .property_ops
            .fail_ewmh
            .store(true, Ordering::Relaxed);
        assert!(
            jwm.set_client_minimized(&mut backend, client_key, true)
                .is_err()
        );
        let repaired_order = jwm.state.clients[client_key].state.minimized_order;
        assert_ne!(repaired_order, 0);
        assert_eq!(
            backend
                .property_ops
                .minimized_restore_writes
                .lock()
                .expect("minimized restore writes lock")
                .len(),
            1
        );

        backend
            .property_ops
            .fail_ewmh
            .store(false, Ordering::Relaxed);
        assert!(
            !jwm.set_client_minimized(&mut backend, client_key, true)
                .unwrap()
        );
        assert_eq!(
            jwm.state.clients[client_key].state.minimized_order,
            repaired_order
        );
        assert_eq!(
            backend
                .property_ops
                .minimized_restore_writes
                .lock()
                .expect("minimized restore writes lock")
                .len(),
            1,
            "retry must reuse the repaired order and snapshot"
        );
    }

    #[test]
    fn zero_order_repair_survives_snapshot_write_failure_and_retries_in_place() {
        let mut backend = MinimizeSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let window = WindowId::from_raw(0x6060_0003);
        let visible = Rect::new(350, 240, 620, 440);
        let mut client = WMClient::new(window);
        client.mon = Some(monitor);
        client.state.tags = 1;
        client.state.is_hidden = true;
        client.geometry.x = hidden_x_left_of_desktop(jwm.desktop_left_edge(), visible.w);
        client.geometry.y = visible.y;
        client.geometry.w = visible.w;
        client.geometry.h = visible.h;
        client.geometry.hidden_restore_rect = Some(visible);
        let client_key = jwm.insert_client(client);
        jwm.attach_to_monitor(client_key, monitor);

        backend
            .property_ops
            .fail_restore_set
            .store(true, Ordering::Relaxed);
        assert!(
            !jwm.set_client_minimized(&mut backend, client_key, true)
                .unwrap()
        );
        let repaired_order = jwm.state.clients[client_key].state.minimized_order;
        assert_ne!(repaired_order, 0);
        assert!(
            backend
                .property_ops
                .minimized_restore
                .lock()
                .expect("minimized restore lock")
                .is_none()
        );

        backend
            .property_ops
            .fail_restore_set
            .store(false, Ordering::Relaxed);
        assert!(
            !jwm.set_client_minimized(&mut backend, client_key, true)
                .unwrap()
        );
        let snapshot = backend
            .property_ops
            .minimized_restore
            .lock()
            .expect("minimized restore lock")
            .expect("retry must persist the repaired snapshot");
        assert_eq!(snapshot.minimized_order, repaired_order);
        assert_eq!(
            jwm.state.clients[client_key].state.minimized_order, repaired_order,
            "a property retry must not allocate a new Dock incarnation"
        );
        assert_eq!(
            backend
                .property_ops
                .minimized_restore_writes
                .lock()
                .expect("minimized restore writes lock")
                .as_slice(),
            &[Some(snapshot)]
        );

        assert!(
            !jwm.set_client_minimized(&mut backend, client_key, true)
                .unwrap()
        );
        assert_eq!(
            backend
                .property_ops
                .minimized_restore_writes
                .lock()
                .expect("minimized restore writes lock")
                .as_slice(),
            &[Some(snapshot)],
            "once repaired, an idempotent minimize must not rewrite V1"
        );
    }

    #[test]
    fn minimizing_an_already_parked_tag_persists_its_visible_restore_slot() {
        let mut backend = MinimizeSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let monitor_num = jwm.state.monitors[monitor].num;
        let desktop_left = jwm.desktop_left_edge();
        let window = WindowId::from_raw(0x6061);
        let visible = Rect::new(475, 210, 720, 520);
        let mut client = WMClient::new(window);
        client.mon = Some(monitor);
        client.state.tags = 1 << 4;
        client.geometry.x = hidden_x_left_of_desktop(desktop_left, visible.w);
        client.geometry.y = visible.y;
        client.geometry.w = visible.w;
        client.geometry.h = visible.h;
        client.geometry.hidden_x = Some(client.geometry.x);
        client.geometry.hidden_restore_rect = Some(visible);
        let client_key = jwm.insert_client(client);
        jwm.attach_to_monitor(client_key, monitor);

        assert!(
            jwm.set_client_minimized(&mut backend, client_key, true)
                .unwrap()
        );
        let snapshot = backend
            .property_ops
            .minimized_restore
            .lock()
            .expect("minimized restore lock")
            .expect("parked minimize snapshot");
        assert_eq!(snapshot.monitor_num, monitor_num);
        assert_eq!(
            snapshot.visible_rect,
            MinimizedRestoreRect {
                x: visible.x,
                y: visible.y,
                w: visible.w,
                h: visible.h,
            }
        );
        assert_ne!(
            snapshot.visible_rect.x,
            jwm.state.clients[client_key].geometry.x
        );

        assert!(
            !jwm.set_client_minimized(&mut backend, client_key, true)
                .unwrap()
        );
        assert_eq!(
            backend
                .property_ops
                .minimized_restore_writes
                .lock()
                .expect("minimized restore writes lock")
                .as_slice(),
            &[Some(snapshot)]
        );
    }

    #[test]
    fn ewmh_precommit_failure_removes_the_new_snapshot() {
        let mut backend = MinimizeSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let window = WindowId::from_raw(0x6062);
        let mut client = WMClient::new(window);
        client.mon = Some(monitor);
        client.state.tags = 1;
        client.geometry.x = 100;
        client.geometry.y = 120;
        client.geometry.w = 640;
        client.geometry.h = 480;
        let client_key = jwm.insert_client(client);
        jwm.attach_to_monitor(client_key, monitor);

        backend
            .property_ops
            .fail_ewmh
            .store(true, Ordering::Relaxed);
        assert!(
            jwm.set_client_minimized(&mut backend, client_key, true)
                .is_err()
        );
        assert!(!jwm.state.clients[client_key].state.is_hidden);
        assert_eq!(jwm.state.clients[client_key].state.minimized_order, 0);
        assert_eq!(
            *backend
                .property_ops
                .minimized_restore
                .lock()
                .expect("minimized restore lock"),
            None
        );
        assert_eq!(
            backend
                .property_ops
                .minimized_restore_writes
                .lock()
                .expect("minimized restore writes lock")
                .len(),
            2,
            "snapshot write must be followed by rollback clear"
        );
        assert!(!backend.property_ops.ewmh_hidden.load(Ordering::Relaxed));
    }

    #[test]
    fn failed_ewmh_query_uses_internal_state_for_every_wm_state_rollback() {
        for (case, initially_hidden, requested_hidden) in [
            (0_u64, false, true),
            (1, true, false),
            (2, true, true),
            (3, false, false),
        ] {
            let mut backend = MinimizeSpyBackend::new();
            let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
            let monitor = jwm.state.monitor_order[0];
            let window = WindowId::from_raw(0x6070 + case);
            let mut client = WMClient::new(window);
            client.mon = Some(monitor);
            client.state.tags = 1;
            client.state.is_hidden = initially_hidden;
            client.state.minimized_order = if initially_hidden { 77 } else { 0 };
            client.geometry.x = 100;
            client.geometry.y = 120;
            client.geometry.w = 640;
            client.geometry.h = 480;
            let client_key = jwm.insert_client(client);
            jwm.attach_to_monitor(client_key, monitor);

            backend
                .property_ops
                .ewmh_hidden
                .store(initially_hidden, Ordering::Relaxed);
            backend.property_ops.wm_state.store(
                i64::from(wm_state_for_minimized(initially_hidden)),
                Ordering::Relaxed,
            );
            backend
                .property_ops
                .fail_ewmh_query
                .store(true, Ordering::Relaxed);
            backend
                .property_ops
                .fail_wm_state
                .store(true, Ordering::Relaxed);

            assert!(
                jwm.set_client_minimized(&mut backend, client_key, requested_hidden)
                    .is_err()
            );
            assert_eq!(
                jwm.state.clients[client_key].state.is_hidden, initially_hidden,
                "case {case}: internal state must not commit"
            );
            assert_eq!(
                backend.property_ops.ewmh_hidden.load(Ordering::Relaxed),
                initially_hidden,
                "case {case}: rollback must use pre-transition internal state"
            );
        }
    }

    #[test]
    fn hidden_client_reentering_dock_is_statically_adopted_after_geometry() {
        let mut backend = MinimizeSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let monitor_num = jwm.state.monitors[monitor].num;
        let shelf = CompositorRect::new(100.0, 200.0, 80.0, 40.0);
        jwm.minimized_dock_shelves.insert(monitor_num, shelf);

        let window = WindowId::from_raw(0x5250);
        let mut client = WMClient::new(window);
        client.mon = Some(monitor);
        client.state.tags = 1;
        client.state.is_hidden = true;
        client.state.skip_taskbar = true;
        let client_key = jwm.insert_client(client);
        jwm.attach_to_monitor(client_key, monitor);

        jwm.state.clients[client_key].state.skip_taskbar = false;
        jwm.reconcile_minimized_dock_eligibility(&mut backend, client_key, false);

        assert_eq!(
            backend.dock_lifecycle,
            vec![
                DockLifecycleEvent::Geometry(window, Some(shelf)),
                DockLifecycleEvent::EnsureStatic(window),
                DockLifecycleEvent::RequestIconify(window),
            ]
        );
        assert_eq!(backend.iconify_requests, vec![window]);

        // A repeated EWMH Remove while the bit is already clear is not a new
        // eligibility transition and must not restart capture or animation.
        backend.dock_lifecycle.clear();
        jwm.reconcile_minimized_dock_eligibility(&mut backend, client_key, true);
        assert!(backend.dock_lifecycle.is_empty());
    }

    #[test]
    fn hidden_client_changing_to_and_from_dock_type_reuses_reconciliation() {
        let mut backend = MinimizeSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let monitor_num = jwm.state.monitors[monitor].num;
        let shelf = CompositorRect::new(300.0, 400.0, 64.0, 48.0);
        jwm.minimized_dock_shelves.insert(monitor_num, shelf);

        let window = WindowId::from_raw(0x5350);
        let mut client = WMClient::new(window);
        client.mon = Some(monitor);
        client.state.tags = 1;
        client.state.is_hidden = true;
        let client_key = jwm.insert_client(client);
        jwm.attach_to_monitor(client_key, monitor);

        jwm.state.clients[client_key].state.is_dock = true;
        jwm.reconcile_minimized_dock_eligibility(&mut backend, client_key, true);
        assert_eq!(
            backend.dock_lifecycle,
            vec![
                DockLifecycleEvent::Geometry(window, None),
                DockLifecycleEvent::CancelIconify(window),
                DockLifecycleEvent::ForgetStatic(window),
            ]
        );
        assert_eq!(backend.iconify_cancels, vec![window]);

        backend.dock_lifecycle.clear();
        jwm.state.clients[client_key].state.is_dock = false;
        jwm.reconcile_minimized_dock_eligibility(&mut backend, client_key, false);
        assert_eq!(
            backend.dock_lifecycle,
            vec![
                DockLifecycleEvent::Geometry(window, Some(shelf)),
                DockLifecycleEvent::EnsureStatic(window),
                DockLifecycleEvent::RequestIconify(window),
            ]
        );
        assert_eq!(backend.iconify_requests, vec![window]);
    }

    #[test]
    fn ineligible_iconic_client_rearms_after_unconfirmed_map_then_forgets_on_retry() {
        let mut backend = MinimizeSpyBackend::new();
        backend.supports_client_list = true;
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let monitor_num = jwm.state.monitors[monitor].num;

        let window = WindowId::from_raw(0x5351);
        let mut client = WMClient::new(window);
        client.mon = Some(monitor);
        client.state.tags = 1;
        client.state.is_hidden = true;
        let client_key = jwm.insert_client(client);
        jwm.attach_to_monitor(client_key, monitor);
        jwm.active_minimized_preview = Some((monitor_num, window));
        jwm.active_minimized_preview_generation = Some(91);
        backend
            .window_ops
            .force_unmapped
            .store(true, Ordering::Relaxed);

        // The eligibility change first withdraws the Dock target/preview.
        // Simulate the X11 backend successfully mapping the Iconic client,
        // followed by JWM's ordered second attributes query failing.
        jwm.state.clients[client_key].state.skip_taskbar = true;
        backend
            .window_ops
            .fail_next_attributes
            .store(true, Ordering::Relaxed);
        jwm.reconcile_minimized_dock_eligibility(&mut backend, client_key, true);

        assert_eq!(jwm.active_minimized_preview, None);
        assert_eq!(jwm.active_minimized_preview_generation, None);
        assert_eq!(backend.dock_targets, vec![(window, None)]);
        assert_eq!(backend.iconify_cancels, vec![window]);
        assert_eq!(backend.iconify_requests, vec![window]);
        assert_eq!(
            backend.dock_lifecycle,
            vec![
                DockLifecycleEvent::Geometry(window, None),
                DockLifecycleEvent::Preview(None, None),
                DockLifecycleEvent::CancelIconify(window),
                DockLifecycleEvent::RequestIconify(window),
            ],
            "an unconfirmed map must retain the minimized visual"
        );
        assert!(
            backend.window_ops.force_unmapped.load(Ordering::Relaxed),
            "the ineligible client must be re-armed as true Iconic"
        );

        // No new eligibility edge is required to finish the pending cleanup.
        // A later reconciliation maps and confirms the client, then retires
        // the retained visual/pin.
        backend.dock_targets.clear();
        backend.dock_lifecycle.clear();
        jwm.reconcile_minimized_dock_eligibility(&mut backend, client_key, false);

        assert_eq!(backend.dock_targets, vec![(window, None)]);
        assert_eq!(backend.iconify_cancels, vec![window, window]);
        assert_eq!(backend.iconify_requests, vec![window]);
        assert_eq!(
            backend.dock_lifecycle,
            vec![
                DockLifecycleEvent::Geometry(window, None),
                DockLifecycleEvent::CancelIconify(window),
                DockLifecycleEvent::ForgetStatic(window),
            ]
        );
        assert!(!backend.window_ops.force_unmapped.load(Ordering::Relaxed));
    }

    #[test]
    fn ineligible_iconic_client_reparks_before_cancel_and_retries_configure_failure() {
        let mut backend = MinimizeSpyBackend::new();
        backend.supports_client_list = true;
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let monitor_num = jwm.state.monitors[monitor].num;
        let window = WindowId::from_raw(0x5352);
        let visible = Rect::new(340, 210, 680, 460);
        let (_, client_key) = add_mode_client(&mut jwm, window, visible, true, false);
        backend
            .window_ops
            .geometries
            .lock()
            .expect("window geometries lock")
            .insert(
                window,
                Geometry {
                    x: visible.x,
                    y: visible.y,
                    w: visible.w as u32,
                    h: visible.h as u32,
                    border: 0,
                },
            );
        assert!(
            jwm.set_client_minimized(&mut backend, client_key, true)
                .unwrap()
        );

        let stale = Geometry {
            x: visible.x,
            y: visible.y,
            w: visible.w as u32,
            h: visible.h as u32,
            border: 0,
        };
        backend
            .window_ops
            .geometries
            .lock()
            .expect("window geometries lock")
            .insert(window, stale);
        backend.dock_targets.clear();
        backend.dock_lifecycle.clear();
        backend.iconify_requests.clear();
        jwm.active_minimized_preview = Some((monitor_num, window));
        jwm.active_minimized_preview_generation = Some(92);
        jwm.state.clients[client_key].state.skip_taskbar = true;
        backend
            .window_ops
            .fail_next_configure
            .store(true, Ordering::Relaxed);

        jwm.reconcile_minimized_dock_eligibility(&mut backend, client_key, true);

        assert_eq!(jwm.active_minimized_preview, None);
        assert_eq!(jwm.active_minimized_preview_generation, None);
        assert_eq!(backend.dock_targets, vec![(window, None)]);
        assert_eq!(
            backend.dock_lifecycle,
            vec![
                DockLifecycleEvent::Geometry(window, None),
                DockLifecycleEvent::Preview(None, None),
            ],
            "parking failure must keep the targetless visual/pin without mapping"
        );
        assert!(backend.iconify_cancels.is_empty());
        assert!(backend.iconify_requests.is_empty());
        assert!(backend.iconify_cancel_observations.is_empty());
        assert!(jwm.state.clients[client_key].state.is_hidden);
        assert!(backend.property_ops.ewmh_hidden.load(Ordering::Relaxed));
        assert_eq!(
            backend.property_ops.wm_state.load(Ordering::Relaxed),
            i64::from(crate::backend::api::ICCCM_ICONIC_STATE)
        );
        assert!(backend.window_ops.force_unmapped.load(Ordering::Relaxed));
        let actual = backend.window_ops.get_geometry(window).unwrap();
        assert_eq!(
            (actual.x, actual.y, actual.w, actual.h, actual.border),
            (stale.x, stale.y, stale.w, stale.h, stale.border)
        );

        backend.dock_targets.clear();
        backend.dock_lifecycle.clear();
        jwm.reconcile_minimized_dock_eligibility(&mut backend, client_key, false);

        assert_eq!(backend.dock_targets, vec![(window, None)]);
        assert_eq!(backend.iconify_cancels, vec![window]);
        assert_eq!(
            backend.dock_lifecycle,
            vec![
                DockLifecycleEvent::Geometry(window, None),
                DockLifecycleEvent::CancelIconify(window),
                DockLifecycleEvent::ForgetStatic(window),
            ]
        );
        let (_, geometry_before_map) = backend.iconify_cancel_observations[0];
        assert!(x11_geometry_fully_left_of_desktop(
            geometry_before_map,
            jwm.desktop_left_edge()
        ));
        assert!(!backend.window_ops.force_unmapped.load(Ordering::Relaxed));
    }

    #[test]
    fn wm_state_failure_rolls_back_ewmh_without_committing_internal_minimize() {
        let mut backend = MinimizeSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let window = WindowId::from_raw(0x6160);
        let mut client = WMClient::new(window);
        client.mon = Some(monitor);
        client.state.tags = 1;
        client.geometry.x = 100;
        client.geometry.y = 120;
        client.geometry.w = 640;
        client.geometry.h = 480;
        let client_key = jwm.insert_client(client);
        jwm.attach_to_monitor(client_key, monitor);

        backend
            .property_ops
            .fail_wm_state
            .store(true, Ordering::Relaxed);
        assert!(
            jwm.set_client_minimized(&mut backend, client_key, true)
                .is_err()
        );
        assert!(!jwm.state.clients[client_key].state.is_hidden);
        assert_eq!(jwm.state.clients[client_key].state.minimized_order, 0);
        assert!(!backend.property_ops.ewmh_hidden.load(Ordering::Relaxed));
        assert!(backend.minimized.is_empty());
        assert_eq!(
            *backend
                .property_ops
                .minimized_restore
                .lock()
                .expect("minimized restore lock"),
            None
        );
        assert_eq!(
            backend
                .property_ops
                .minimized_restore_writes
                .lock()
                .expect("minimized restore writes lock")
                .len(),
            2,
            "snapshot write must be followed by rollback clear"
        );

        backend
            .property_ops
            .fail_wm_state
            .store(false, Ordering::Relaxed);
        assert!(
            jwm.set_client_minimized(&mut backend, client_key, true)
                .unwrap()
        );
        assert!(jwm.state.clients[client_key].state.is_hidden);
        assert!(backend.property_ops.ewmh_hidden.load(Ordering::Relaxed));
        assert_eq!(
            backend.property_ops.wm_state.load(Ordering::Relaxed),
            i64::from(crate::jwm::types::ICONIC_STATE)
        );

        // An idempotent request is also a protocol reconciliation point.
        backend
            .property_ops
            .ewmh_hidden
            .store(false, Ordering::Relaxed);
        backend.property_ops.wm_state.store(
            i64::from(crate::jwm::types::NORMAL_STATE),
            Ordering::Relaxed,
        );
        assert!(
            !jwm.set_client_minimized(&mut backend, client_key, true)
                .unwrap()
        );
        assert!(backend.property_ops.ewmh_hidden.load(Ordering::Relaxed));
        assert_eq!(
            backend.property_ops.wm_state.load(Ordering::Relaxed),
            i64::from(crate::jwm::types::ICONIC_STATE)
        );
    }
}
