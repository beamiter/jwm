// Monitor management operations: output handling, geometry, and client distribution

use crate::Jwm;
use crate::backend::api::Backend;
use crate::backend::common_define::OutputId;
use crate::config::CONFIG;
use crate::core::models::{ClientKey, MonitorKey, WMClient, WMMonitor};
use crate::core::state::WMState;
use crate::core::types::Rect;
use crate::jwm::visibility::hidden_x_left_of_desktop;
use log::{info, warn};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

const HIDDEN_CLIENT_PARK_RETRY_INITIAL: Duration = Duration::from_millis(50);
const HIDDEN_CLIENT_PARK_RETRY_MAX: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug)]
struct HiddenClientParkRetry {
    deadline: Instant,
    backoff: Duration,
}

/// Durable, per-incarnation X11 parking retries. `ClientKey` includes its
/// slotmap generation, so a late deadline cannot target a reused window slot.
#[derive(Debug, Default)]
pub(crate) struct HiddenClientParkRetries {
    pending: HashMap<ClientKey, HiddenClientParkRetry>,
}

impl HiddenClientParkRetries {
    fn schedule(&mut self, client_key: ClientKey, now: Instant) {
        self.pending
            .entry(client_key)
            .or_insert(HiddenClientParkRetry {
                deadline: now + HIDDEN_CLIENT_PARK_RETRY_INITIAL,
                backoff: HIDDEN_CLIENT_PARK_RETRY_INITIAL,
            });
    }

    fn clear(&mut self, client_key: ClientKey) {
        self.pending.remove(&client_key);
    }

    fn next_wakeup(&self, now: Instant) -> Option<Duration> {
        self.pending
            .values()
            .map(|retry| retry.deadline.saturating_duration_since(now))
            .min()
    }

    fn due_keys(&self, now: Instant) -> Vec<ClientKey> {
        self.pending
            .iter()
            .filter_map(|(&client_key, retry)| (retry.deadline <= now).then_some(client_key))
            .collect()
    }

    fn reschedule_after_failure(
        &mut self,
        client_key: ClientKey,
        mut retry: HiddenClientParkRetry,
        now: Instant,
    ) {
        retry.backoff = retry
            .backoff
            .saturating_mul(2)
            .min(HIDDEN_CLIENT_PARK_RETRY_MAX);
        retry.deadline = now + retry.backoff;
        self.pending.insert(client_key, retry);
    }
}

fn monitor_rect(monitor: &WMMonitor) -> Rect {
    Rect::new(
        monitor.geometry.m_x,
        monitor.geometry.m_y,
        monitor.geometry.m_w.max(1),
        monitor.geometry.m_h.max(1),
    )
}

fn monitor_work_rect(monitor: &WMMonitor) -> Rect {
    Rect::new(
        monitor.geometry.w_x,
        monitor.geometry.w_y,
        monitor.geometry.w_w.max(1),
        monitor.geometry.w_h.max(1),
    )
}

fn valid_rect(rect: Rect) -> Option<Rect> {
    (rect.w > 0 && rect.h > 0).then_some(rect)
}

/// Preserve a work area's edge reservations while its containing output moves
/// or changes size.  The dynamic workarea calculator may still see the old bar
/// coordinates during `OutputChanged`; carrying the four insets explicitly
/// avoids briefly restoring minimized clients under a stale bar.
fn rebase_work_area(old_monitor: Rect, old_work: Rect, new_monitor: Rect) -> Rect {
    let raw_left = old_work.x.saturating_sub(old_monitor.x).max(0);
    let raw_top = old_work.y.saturating_sub(old_monitor.y).max(0);
    let old_monitor_right = old_monitor.x.saturating_add(old_monitor.w);
    let old_monitor_bottom = old_monitor.y.saturating_add(old_monitor.h);
    let old_work_right = old_work.x.saturating_add(old_work.w);
    let old_work_bottom = old_work.y.saturating_add(old_work.h);
    let raw_right = old_monitor_right.saturating_sub(old_work_right).max(0);
    let raw_bottom = old_monitor_bottom.saturating_sub(old_work_bottom).max(0);

    // A stale or malformed strut can describe a workarea outside its output.
    // Preserve as much inset as the new output can represent, but always
    // leave at least one logical pixel inside the monitor.
    let left = raw_left.min(new_monitor.w.saturating_sub(1).max(0));
    let top = raw_top.min(new_monitor.h.saturating_sub(1).max(0));
    let right = raw_right.min(new_monitor.w.saturating_sub(left).saturating_sub(1));
    let bottom = raw_bottom.min(new_monitor.h.saturating_sub(top).saturating_sub(1));

    let width = new_monitor
        .w
        .saturating_sub(left)
        .saturating_sub(right)
        .max(1);
    let height = new_monitor
        .h
        .saturating_sub(top)
        .saturating_sub(bottom)
        .max(1);
    Rect::new(
        new_monitor.x.saturating_add(left),
        new_monitor.y.saturating_add(top),
        width,
        height,
    )
}

/// Translate a client-content rectangle by its offset from the source
/// workarea, then make the complete decorated window fit the destination.
/// Saturating arithmetic keeps negative-origin and extreme RandR coordinates
/// deterministic instead of wrapping through the opposite side of the desk.
fn translate_and_clamp_restore_rect(
    rect: Rect,
    source_work: Option<Rect>,
    target_work: Rect,
    border_width: i32,
) -> Rect {
    let border2 = border_width.max(0).saturating_mul(2);
    let available_w = target_work.w.max(1);
    let available_h = target_work.h.max(1);
    let width = rect
        .w
        .max(1)
        .min(available_w.saturating_sub(border2).max(1));
    let height = rect
        .h
        .max(1)
        .min(available_h.saturating_sub(border2).max(1));

    let (mut x, mut y) = if let Some(source) = source_work {
        (
            target_work
                .x
                .saturating_add(rect.x.saturating_sub(source.x)),
            target_work
                .y
                .saturating_add(rect.y.saturating_sub(source.y)),
        )
    } else {
        (rect.x, rect.y)
    };
    let max_x = target_work
        .x
        .saturating_add(available_w)
        .saturating_sub(width.saturating_add(border2))
        .max(target_work.x);
    let max_y = target_work
        .y
        .saturating_add(available_h)
        .saturating_sub(height.saturating_add(border2))
        .max(target_work.y);
    x = x.clamp(target_work.x, max_x);
    y = y.clamp(target_work.y, max_y);
    Rect::new(x, y, width, height)
}

fn legacy_hidden_restore_rect(client: &WMClient, fallback: Rect) -> Rect {
    let floating = Rect::new(
        client.geometry.floating_x,
        client.geometry.floating_y,
        client.geometry.floating_w,
        client.geometry.floating_h,
    );
    if (client.state.is_floating || client.state.is_pip)
        && let Some(floating) = valid_rect(floating)
    {
        return floating;
    }

    let old = Rect::new(
        client.geometry.old_x,
        client.geometry.old_y,
        client.geometry.old_w,
        client.geometry.old_h,
    );
    if let Some(old) = valid_rect(old) {
        return old;
    }

    Rect::new(
        fallback.x,
        fallback.y,
        client.geometry.w.max(1),
        client.geometry.h.max(1),
    )
}

/// Move the *semantic* visible geometry of a minimized client to a new
/// output. The live window stays parked at `hidden_x`; only `show_client`
/// consumes the restore slot later.
fn migrate_hidden_restore_geometry(
    client: &mut WMClient,
    source_work: Option<Rect>,
    target_monitor: Rect,
    target_work: Rect,
    desktop_left: i32,
) -> bool {
    if !client.state.is_hidden {
        return false;
    }

    let border_width = client.geometry.border_w;
    let fallback_area = source_work.unwrap_or(target_work);
    let previous_visible = client
        .geometry
        .hidden_restore_rect
        .and_then(valid_rect)
        .unwrap_or_else(|| legacy_hidden_restore_rect(client, fallback_area));

    let visible = if client.state.is_fullscreen {
        target_monitor
    } else {
        translate_and_clamp_restore_rect(previous_visible, source_work, target_work, border_width)
    };

    if client.state.is_fullscreen {
        // `old_*` is the pre-fullscreen semantic rectangle. It must follow the
        // output as well, otherwise restoring and then leaving fullscreen
        // jumps back to a disconnected output.
        let old = Rect::new(
            client.geometry.old_x,
            client.geometry.old_y,
            client.geometry.old_w,
            client.geometry.old_h,
        );
        let old = translate_and_clamp_restore_rect(
            valid_rect(old).unwrap_or(previous_visible),
            source_work,
            target_work,
            border_width,
        );
        client.geometry.old_x = old.x;
        client.geometry.old_y = old.y;
        client.geometry.old_w = old.w;
        client.geometry.old_h = old.h;

        let floating = Rect::new(
            client.geometry.floating_x,
            client.geometry.floating_y,
            client.geometry.floating_w,
            client.geometry.floating_h,
        );
        if let Some(floating) = valid_rect(floating) {
            let floating =
                translate_and_clamp_restore_rect(floating, source_work, target_work, border_width);
            client.geometry.floating_x = floating.x;
            client.geometry.floating_y = floating.y;
            client.geometry.floating_w = floating.w;
            client.geometry.floating_h = floating.h;
        }
    } else if client.state.is_floating || client.state.is_pip {
        // A minimized floating client has one user-visible position. Keeping
        // the floating slot identical prevents toggle-float after restore
        // from resurrecting coordinates from the source output.
        client.geometry.floating_x = visible.x;
        client.geometry.floating_y = visible.y;
        client.geometry.floating_w = visible.w;
        client.geometry.floating_h = visible.h;
    }

    let total_width = visible
        .w
        .saturating_add(border_width.max(0).saturating_mul(2));
    let hidden_x = hidden_x_left_of_desktop(desktop_left, total_width);
    client.geometry.hidden_restore_rect = Some(visible);
    client.geometry.hidden_x = Some(hidden_x);
    client.geometry.x = hidden_x;
    client.geometry.y = visible.y;
    client.geometry.w = visible.w;
    client.geometry.h = visible.h;
    true
}

fn lowest_unused_monitor_num<'a>(monitor_nums: impl Iterator<Item = &'a i32>) -> i32 {
    let mut used: Vec<i32> = monitor_nums.copied().filter(|num| *num >= 0).collect();
    used.sort_unstable();
    used.dedup();

    let mut candidate = 0;
    for num in used {
        if num == candidate {
            candidate = candidate.saturating_add(1);
        } else if num > candidate {
            break;
        }
    }
    candidate
}

/// Return every live client whose authoritative monitor pointer still names
/// `monitor`, preserving the monitor list's layout order where possible.
///
/// The registry fallback is intentional: a partially-built Wayland client can
/// have acquired `client.mon` before it was inserted into the monitor vectors.
/// Hot-unplug must not leave that client pointing at a removed slotmap key.
fn clients_owned_by_monitor(state: &WMState, monitor: MonitorKey) -> Vec<ClientKey> {
    let mut seen = HashSet::new();
    let mut owned = Vec::new();

    if let Some(clients) = state.monitor_clients.get(monitor) {
        for &client_key in clients {
            if state
                .clients
                .get(client_key)
                .is_some_and(|client| client.mon == Some(monitor))
                && seen.insert(client_key)
            {
                owned.push(client_key);
            }
        }
    }

    for (client_key, client) in &state.clients {
        if client.mon == Some(monitor) && seen.insert(client_key) {
            owned.push(client_key);
        }
    }

    owned
}

/// Move clients off a monitor before its slotmap key is deleted. With no
/// surviving target they become intentional output orphans (`mon=None`) and
/// keep their tags/minimized metadata until a later OutputAdded reattaches
/// them. `non_reassignable` contains a retired bar client, if any: its stale
/// ownership is cleared but it must never migrate onto another output.
fn transfer_or_orphan_monitor_clients(
    state: &mut WMState,
    source: MonitorKey,
    target: Option<MonitorKey>,
    non_reassignable: Option<ClientKey>,
    parked_scratchpads: &HashSet<ClientKey>,
) -> Vec<ClientKey> {
    let target = target.filter(|&monitor| state.monitors.contains_key(monitor));
    let fallback_tags = target
        .and_then(|monitor| state.monitors.get(monitor))
        .map(|monitor| monitor.get_active_tags())
        .unwrap_or(1);
    let owned = clients_owned_by_monitor(state, source);
    let mut reassigned = Vec::with_capacity(owned.len());

    for client_key in owned {
        if let Some(clients) = state.monitor_clients.get_mut(source) {
            clients.retain(|&key| key != client_key);
        }
        if let Some(stack) = state.monitor_stack.get_mut(source) {
            stack.retain(|&key| key != client_key);
        }
        if let Some(monitor) = state.monitors.get_mut(source) {
            monitor.clear_selection_of(client_key);
        }

        // Clear the old key first even when a bar's unmanage path failed. That
        // prevents a deleted MonitorKey from escaping into later focus/layout
        // code; retired bars are deliberately not attached to the target.
        if let Some(client) = state.clients.get_mut(client_key) {
            client.mon = None;
        }
        if non_reassignable == Some(client_key) {
            continue;
        }

        let Some(target) = target else {
            reassigned.push(client_key);
            continue;
        };
        if let Some(client) = state.clients.get_mut(client_key) {
            client.mon = Some(target);
            // Moving output ownership must not itself reveal a parked
            // scratchpad. Only an explicit scratchpad toggle assigns that
            // client the destination's active tag.
            if client.state.tags == 0 && !parked_scratchpads.contains(&client_key) {
                client.state.tags = fallback_tags;
            }
        }
        if let Some(clients) = state.monitor_clients.get_mut(target)
            && !clients.contains(&client_key)
        {
            clients.push(client_key);
        }
        if let Some(stack) = state.monitor_stack.get_mut(target) {
            stack.retain(|&key| key != client_key);
            stack.insert(0, client_key);
        }
        reassigned.push(client_key);
    }

    reassigned
}

/// Attach clients that genuinely need an output while preserving the special
/// `mon=None` state of parked scratchpads and not adopting managed bar clients.
fn attachable_unassigned_clients(
    state: &WMState,
    parked_scratchpads: &HashSet<ClientKey>,
    bar_clients: &HashSet<ClientKey>,
) -> Vec<ClientKey> {
    let is_attachable = |client_key: ClientKey| {
        state.clients.get(client_key).is_some_and(|client| {
            client.mon.is_none()
                // A minimized scratchpad still needs output ownership so it
                // remains in that monitor's Dock projection. `tags=0` keeps
                // it parked; attaching the monitor does not reveal it. An
                // ordinary (non-minimized) parked scratchpad retains the
                // historical `mon=None` state until an explicit toggle.
                && (!parked_scratchpads.contains(&client_key) || client.state.is_hidden)
                && !bar_clients.contains(&client_key)
        })
    };
    let mut seen = HashSet::new();
    let mut clients = Vec::new();
    for &client_key in &state.client_order {
        if is_attachable(client_key) && seen.insert(client_key) {
            clients.push(client_key);
        }
    }
    for (client_key, _) in &state.clients {
        if is_attachable(client_key) && seen.insert(client_key) {
            clients.push(client_key);
        }
    }
    clients
}

fn attach_clients_to_monitor(
    state: &mut WMState,
    monitor: MonitorKey,
    client_keys: &[ClientKey],
    preserve_zero_tags: &HashSet<ClientKey>,
) -> Vec<ClientKey> {
    let Some(target_tags) = state
        .monitors
        .get(monitor)
        .map(|target| target.get_active_tags())
    else {
        return Vec::new();
    };
    let mut attached = Vec::with_capacity(client_keys.len());

    for &client_key in client_keys {
        let Some(client) = state.clients.get_mut(client_key) else {
            continue;
        };
        if client.mon.is_some() {
            continue;
        }
        client.mon = Some(monitor);
        if client.state.tags == 0 && !preserve_zero_tags.contains(&client_key) {
            client.state.tags = target_tags;
        }

        if let Some(clients) = state.monitor_clients.get_mut(monitor)
            && !clients.contains(&client_key)
        {
            clients.push(client_key);
        }
        if let Some(stack) = state.monitor_stack.get_mut(monitor) {
            stack.retain(|&key| key != client_key);
            stack.insert(0, client_key);
        }
        attached.push(client_key);
    }

    attached
}

fn remove_monitor_state(state: &mut WMState, monitor: MonitorKey) {
    state.monitors.remove(monitor);
    state.output_map.remove(monitor);
    state.monitor_clients.remove(monitor);
    state.monitor_stack.remove(monitor);
    state.monitor_order.retain(|&key| key != monitor);

    if state.sel_mon == Some(monitor) {
        state.sel_mon = state.monitor_order.first().copied();
    }
    if state.motion_mon == Some(monitor) {
        state.motion_mon = None;
    }
}

impl Jwm {
    pub(crate) fn schedule_hidden_client_park_retry(
        &mut self,
        client_key: ClientKey,
        now: Instant,
    ) {
        if self
            .state
            .clients
            .get(client_key)
            .is_some_and(|client| client.state.is_hidden)
        {
            self.hidden_client_park_retries.schedule(client_key, now);
        }
    }

    pub(crate) fn clear_hidden_client_park_retry(&mut self, client_key: ClientKey) {
        self.hidden_client_park_retries.clear(client_key);
    }

    pub(crate) fn hidden_client_park_retry_next_wakeup(&self, now: Instant) -> Option<Duration> {
        self.hidden_client_park_retries.next_wakeup(now)
    }

    /// Retry only due real-window park operations. Every failure is scheduled
    /// relative to this tick with a capped exponential delay, so an overdue
    /// entry is attempted once rather than spinning to catch up.
    pub(crate) fn tick_hidden_client_park_retries(
        &mut self,
        backend: &mut dyn Backend,
        now: Instant,
    ) {
        let due = self.hidden_client_park_retries.due_keys(now);
        for client_key in due {
            let Some(retry) = self.hidden_client_park_retries.pending.remove(&client_key) else {
                continue;
            };
            let Some((win, true)) = self
                .state
                .clients
                .get(client_key)
                .map(|client| (client.win, client.state.is_hidden))
            else {
                // A removed or restored incarnation is terminal for this
                // side-effect-only retry.
                continue;
            };

            // The non-composited Hide animation owns intermediate X geometry.
            // Keep the durable entry until that owner is gone; otherwise the
            // shared helper's intentional animation no-op would look like a
            // verified parking success and prematurely clear the retry.
            let hide_animation_owns_geometry = !backend.has_compositor()
                && CONFIG.load().animation_enabled()
                && self
                    .animations
                    .active
                    .get(&client_key)
                    .is_some_and(|animation| {
                        animation.kind == crate::core::animation::AnimationKind::Hide
                    });
            if hide_animation_owns_geometry {
                self.hidden_client_park_retries
                    .reschedule_after_failure(client_key, retry, now);
                continue;
            }

            if let Err(error) = self.retry_x11_minimized_client_park(backend, client_key) {
                self.hidden_client_park_retries
                    .reschedule_after_failure(client_key, retry, now);
                warn!(
                    "could not retry minimized X11 parking for {:?}: {error}",
                    win
                );
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn has_hidden_client_park_retry(&self, client_key: ClientKey) -> bool {
        self.hidden_client_park_retries
            .pending
            .contains_key(&client_key)
    }

    #[cfg(test)]
    pub(crate) fn force_hidden_client_park_retry_due(&mut self, client_key: ClientKey) {
        if let Some(retry) = self.hidden_client_park_retries.pending.get_mut(&client_key) {
            retry.deadline = Instant::now();
        }
    }

    #[cfg(test)]
    pub(crate) fn defer_hidden_client_park_retry_for_test(
        &mut self,
        client_key: ClientKey,
        delay: Duration,
    ) {
        if let Some(retry) = self.hidden_client_park_retries.pending.get_mut(&client_key) {
            retry.deadline = Instant::now() + delay;
        }
    }

    pub(super) fn monitor_migration_areas(&self, monitor: MonitorKey) -> Option<(Rect, Rect)> {
        let monitor_ref = self.state.monitors.get(monitor)?;
        let output = monitor_rect(monitor_ref);
        let work = self
            .monitor_work_area(monitor)
            .and_then(valid_rect)
            .unwrap_or_else(|| monitor_work_rect(monitor_ref));
        Some((output, work))
    }

    /// Apply one hidden-client migration and immediately move/resize its real
    /// input window at the new parking coordinate. Cancelling a still-running
    /// Hide animation is essential: its old completion target would otherwise
    /// overwrite the freshly migrated restore state on the next frame.
    pub(super) fn migrate_hidden_client_restore(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        source_work: Option<Rect>,
        target_monitor: Rect,
        target_work: Rect,
    ) -> bool {
        let desktop_left = self.desktop_left_edge();
        let migrated = self
            .state
            .clients
            .get_mut(client_key)
            .is_some_and(|client| {
                migrate_hidden_restore_geometry(
                    client,
                    source_work,
                    target_monitor,
                    target_work,
                    desktop_left,
                )
            });
        if !migrated {
            return false;
        }

        self.animations.remove(client_key);
        let Some((win, x, y, w, h, border_w)) = self.state.clients.get(client_key).map(|client| {
            (
                client.win,
                client.geometry.x,
                client.geometry.y,
                client.geometry.w,
                client.geometry.h,
                client.geometry.border_w,
            )
        }) else {
            return false;
        };
        let border = if backend.has_compositor() {
            0
        } else {
            border_w.max(0) as u32
        };
        match backend
            .window_ops()
            .configure(win, x, y, w.max(1) as u32, h.max(1) as u32, border)
        {
            Ok(()) => self.clear_hidden_client_park_retry(client_key),
            Err(error) => {
                if backend.capabilities().supports_client_list {
                    self.schedule_hidden_client_park_retry(client_key, Instant::now());
                }
                warn!("could not repark hidden client {win:?} after output migration: {error}");
            }
        }
        true
    }

    fn refresh_migrated_client_properties(
        &mut self,
        backend: &mut dyn Backend,
        client_keys: &[ClientKey],
    ) {
        for &client_key in client_keys {
            if let Err(error) = self.setclienttagprop(backend, client_key) {
                warn!("could not refresh client metadata after output migration: {error}");
            }
            if let Err(error) = self.persist_minimized_restore_state(backend, client_key) {
                warn!("could not refresh minimized restore state after output migration: {error}");
            }
        }
    }

    /// A topology-only left-edge change can expose the live X11 parking
    /// coordinate even when a client's owning output did not move (for
    /// example, adding a new monitor to the far left). Repark every minimized
    /// client without touching its semantic visible rectangle.
    pub(super) fn repark_all_hidden_clients(&mut self, backend: &mut dyn Backend) {
        let desktop_left = self.desktop_left_edge();
        let now = Instant::now();
        let durable_x11_retry = backend.capabilities().supports_client_list;
        let client_keys: Vec<ClientKey> = self
            .state
            .clients
            .iter()
            .filter_map(|(client_key, client)| client.state.is_hidden.then_some(client_key))
            .collect();

        for client_key in client_keys {
            let Some((win, hidden_x, y)) = self.state.clients.get_mut(client_key).map(|client| {
                let restore_width = client
                    .geometry
                    .hidden_restore_rect
                    .and_then(valid_rect)
                    .map(|rect| {
                        rect.w
                            .saturating_add(client.geometry.border_w.max(0).saturating_mul(2))
                    })
                    .unwrap_or(0);
                let live_width = client
                    .geometry
                    .w
                    .max(1)
                    .saturating_add(client.geometry.border_w.max(0).saturating_mul(2));
                let total_width = live_width.max(restore_width).max(1);
                let hidden_x = hidden_x_left_of_desktop(desktop_left, total_width);
                client.geometry.x = hidden_x;
                client.geometry.hidden_x = Some(hidden_x);
                (client.win, hidden_x, client.geometry.y)
            }) else {
                continue;
            };
            self.animations.remove(client_key);
            match backend.window_ops().set_position(win, hidden_x, y) {
                Ok(()) => self.clear_hidden_client_park_retry(client_key),
                Err(error) => {
                    if durable_x11_retry {
                        self.schedule_hidden_client_park_retry(client_key, now);
                    }
                    warn!(
                        "could not repark minimized client {:?} after topology change: {error}",
                        win
                    );
                }
            }
        }
    }

    pub(crate) fn add_monitor(&mut self, info: crate::backend::api::OutputInfo) {
        info!("[add_monitor] Adding output: {:?}", info);
        let mut m = self.createmon(CONFIG.load().show_bar());

        // 设置 Monitor 几何属性
        m.geometry.m_x = info.x;
        m.geometry.m_y = info.y;
        m.geometry.m_w = info.width;
        m.geometry.m_h = info.height;
        // 工作区通常等于屏幕区，减去 Bar 的计算在 layout 中动态进行
        m.geometry.w_x = info.x;
        m.geometry.w_y = info.y;
        m.geometry.w_w = info.width;
        m.geometry.w_h = info.height;
        // Monitor numbers are protocol identities (bar shm key, Dock command
        // source), not the current slotmap length. Reusing `len()` after a
        // non-tail hot-unplug can collide with a surviving monitor.
        m.num = lowest_unused_monitor_num(self.state.monitors.values().map(|monitor| &monitor.num));

        let key = self.state.monitors.insert(m);
        self.state.monitor_order.push(key);
        self.state.output_map.insert(key, info.id);
        self.state.monitor_clients.insert(key, Vec::new());
        self.state.monitor_stack.insert(key, Vec::new());

        if self.state.sel_mon.is_none() {
            self.state.sel_mon = Some(key);
        }
    }

    pub(crate) fn handle_output_added(
        &mut self,
        backend: &mut dyn Backend,
        info: crate::backend::api::OutputInfo,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Jwm::new() already calls add_monitor for every output returned by
        // enumerate_outputs().  The udev backend then fires OutputAdded for
        // the same outputs when the event loop starts.  Skip the duplicate.
        if self.state.output_map.values().any(|&id| id == info.id) {
            return Ok(());
        }
        self.add_monitor(info);
        self.repark_all_hidden_clients(backend);

        // Wayland clients can appear before outputs are fully initialized.
        // Those clients end up with `mon=None`, meaning JWM will treat them as invisible:
        // - click-to-focus won't stick (focus() falls back to visible clients)
        // - arrange() won't resize them
        // The udev backend still renders them, so they look "stuck" at their initial size.
        self.attach_unassigned_clients_to_selected_monitor(backend);

        self.arrange(backend, None);
        Ok(())
    }

    pub(crate) fn attach_unassigned_clients_to_selected_monitor(
        &mut self,
        backend: &mut dyn Backend,
    ) {
        let target_mon_key = self
            .state
            .sel_mon
            .or_else(|| self.state.monitor_order.first().copied());

        let Some(mon_key) = target_mon_key else {
            return;
        };

        let tagmask = CONFIG.load().tagmask();
        let parked_scratchpads: HashSet<ClientKey> = self
            .scratchpads
            .values()
            .copied()
            .filter(|&client_key| {
                self.state
                    .clients
                    .get(client_key)
                    .is_some_and(|client| client.state.tags & tagmask == 0)
            })
            .collect();
        let bar_clients: HashSet<ClientKey> = self
            .secondary_bars
            .values()
            .filter_map(|bar| bar.client_key)
            .collect();
        let unassigned =
            attachable_unassigned_clients(&self.state, &parked_scratchpads, &bar_clients);
        let attached =
            attach_clients_to_monitor(&mut self.state, mon_key, &unassigned, &parked_scratchpads);
        let target_areas = self.monitor_migration_areas(mon_key);

        for &client_key in &attached {
            if let Some((target_monitor, target_work)) = target_areas {
                self.migrate_hidden_client_restore(
                    backend,
                    client_key,
                    None,
                    target_monitor,
                    target_work,
                );
            }
            self.reorder_client_in_monitor_groups(client_key);
        }
        self.refresh_migrated_client_properties(backend, &attached);
    }

    pub(crate) fn handle_output_removed(
        &mut self,
        backend: &mut dyn Backend,
        id: OutputId,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("[handle_output_removed] Removing output {:?}", id);

        // 查找对应的 MonitorKey
        let mon_key_opt = self
            .state
            .output_map
            .iter()
            .find(|&(_, &oid)| oid == id)
            .map(|(k, _)| k);

        if let Some(mon_key) = mon_key_opt {
            let retired_bar_client = if let Some(monitor_num) =
                self.state.monitors.get(mon_key).map(|monitor| monitor.num)
            {
                // Withdraw compositor-owned Dock overlays while the source
                // monitor and its hidden-client list are still addressable.
                // Waiting until after removal loses both pieces of lookup
                // state and leaves stale thumbnails at the unplugged output.
                self.retire_secondary_bar(backend, monitor_num)
            } else {
                None
            };
            self.move_clients_to_first_monitor(backend, mon_key, retired_bar_client);

            let removed_was_selected = self.state.sel_mon == Some(mon_key);
            remove_monitor_state(&mut self.state, mon_key);
            self.last_stacking.remove(mon_key);
            let dropped_scrolling_states = self.drop_scrolling_states_for_monitor(mon_key);
            self.repark_all_hidden_clients(backend);

            // 如果删除了当前选中的 Monitor，重置选中
            if removed_was_selected {
                self.focus(backend, None)?;
            }

            self.arrange(backend, None);
            self.mark_bar_update_needed_if_visible(None);
            if dropped_scrolling_states > 0 {
                info!(
                    "[handle_output_removed] Dropped {} scrolling states for removed monitor {:?}",
                    dropped_scrolling_states, mon_key
                );
            }
        }
        Ok(())
    }

    pub(crate) fn handle_output_changed(
        &mut self,
        backend: &mut dyn Backend,
        info: crate::backend::api::OutputInfo,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mon_key_opt = self
            .state
            .output_map
            .iter()
            .find(|&(_, &oid)| oid == info.id)
            .map(|(k, _)| k);
        if let Some(mon_key) = mon_key_opt {
            let Some((old_monitor, old_work)) = self.monitor_migration_areas(mon_key) else {
                return Ok(());
            };
            let new_monitor = Rect::new(info.x, info.y, info.width.max(1), info.height.max(1));
            let new_work = rebase_work_area(old_monitor, old_work, new_monitor);
            let hidden_clients: Vec<ClientKey> = clients_owned_by_monitor(&self.state, mon_key)
                .into_iter()
                .filter(|&client_key| {
                    self.state
                        .clients
                        .get(client_key)
                        .is_some_and(|client| client.state.is_hidden)
                })
                .collect();
            // OutputChanged also carries scale changes whose logical rectangle
            // may be unchanged. Every Dock target is in global physical pixels,
            // so withdraw the old coordinate space before mutating geometry and
            // let the bar publish a fresh scene for this output.
            let monitor_num = self.state.monitors.get(mon_key).map(|monitor| monitor.num);
            if let Some(monitor_num) = monitor_num {
                self.clear_minimized_dock_for_monitor(backend, monitor_num);
            }
            if let Some(m) = self.state.monitors.get_mut(mon_key) {
                m.geometry.m_x = info.x;
                m.geometry.m_y = info.y;
                m.geometry.m_w = info.width;
                m.geometry.m_h = info.height;
                m.geometry.w_x = info.x;
                m.geometry.w_y = info.y;
                m.geometry.w_w = info.width;
                m.geometry.w_h = info.height;
            }
            let mut migrated = Vec::with_capacity(hidden_clients.len());
            for client_key in hidden_clients {
                if self.migrate_hidden_client_restore(
                    backend,
                    client_key,
                    Some(old_work),
                    new_monitor,
                    new_work,
                ) {
                    migrated.push(client_key);
                }
            }
            self.repark_all_hidden_clients(backend);
            self.arrange(backend, Some(mon_key));
            self.refresh_migrated_client_properties(backend, &migrated);
            self.mark_bar_update_needed_if_visible(monitor_num);
        }
        Ok(())
    }
    pub(crate) fn updategeom(&mut self, backend: &mut dyn Backend) -> bool {
        info!("[updategeom]");
        let outputs = backend.output_ops().enumerate_outputs();

        let dirty = if outputs.len() <= 1 {
            self.setup_single_monitor(backend)
        } else {
            let mons: Vec<(i32, i32, i32, i32)> = outputs
                .iter()
                .map(|o| (o.x, o.y, o.width, o.height))
                .collect();
            self.setup_multiple_monitors(backend, mons)
        };

        if dirty {
            let root_window = backend.root_window();
            self.state.sel_mon = self.wintomon(backend, root_window);
            if self.state.sel_mon.is_none() && !self.state.monitor_order.is_empty() {
                self.state.sel_mon = self.state.monitor_order.first().copied();
            }
        }

        // Update compositor with current monitor geometries (for per-monitor wallpaper)
        self.refresh_compositor_monitors(backend);

        dirty
    }

    /// Push the current monitor list (geometry + active tag mask) down to the
    /// compositor. Called whenever monitors change, and also after tag-switch
    /// commands so per-tag wallpapers can be resolved.
    pub(crate) fn refresh_compositor_monitors(&self, backend: &mut dyn Backend) {
        let mon_list: Vec<(u32, i32, i32, u32, u32, u32)> = self
            .state
            .monitor_order
            .iter()
            .enumerate()
            .filter_map(|(idx, &mk)| {
                self.state.monitors.get(mk).map(|m| {
                    (
                        idx as u32,
                        m.geometry.m_x,
                        m.geometry.m_y,
                        m.geometry.m_w.max(1) as u32,
                        m.geometry.m_h.max(1) as u32,
                        m.get_active_tags(),
                    )
                })
            })
            .collect();
        backend.compositor_set_monitors(&mon_list);
    }

    pub(crate) fn setup_single_monitor(&mut self, backend: &mut dyn Backend) -> bool {
        let mut dirty = false;

        if self.state.monitor_order.is_empty() {
            let new_monitor = self.createmon(CONFIG.load().show_bar());
            let mon_key = self.insert_monitor(new_monitor);
            self.state.sel_mon = Some(mon_key);
            dirty = true;
        }

        if let Some(&mon_key) = self.state.monitor_order.first() {
            let geometry_changed = self.state.monitors.get(mon_key).is_some_and(|monitor| {
                monitor.geometry.m_x != 0
                    || monitor.geometry.m_y != 0
                    || monitor.geometry.m_w != self.s_w
                    || monitor.geometry.m_h != self.s_h
            });
            if geometry_changed {
                let old_areas = self.monitor_migration_areas(mon_key);
                let hidden_clients: Vec<ClientKey> = clients_owned_by_monitor(&self.state, mon_key)
                    .into_iter()
                    .filter(|&client_key| {
                        self.state
                            .clients
                            .get(client_key)
                            .is_some_and(|client| client.state.is_hidden)
                    })
                    .collect();
                if let Some(monitor_num) =
                    self.state.monitors.get(mon_key).map(|monitor| monitor.num)
                {
                    self.clear_minimized_dock_for_monitor(backend, monitor_num);
                }
                if let Some(monitor) = self.state.monitors.get_mut(mon_key) {
                    monitor.num = 0;
                    monitor.geometry.m_x = 0;
                    monitor.geometry.w_x = 0;
                    monitor.geometry.m_y = 0;
                    monitor.geometry.w_y = 0;
                    monitor.geometry.m_w = self.s_w;
                    monitor.geometry.w_w = self.s_w;
                    monitor.geometry.m_h = self.s_h;
                    monitor.geometry.w_h = self.s_h;
                }
                let new_monitor = Rect::new(0, 0, self.s_w.max(1), self.s_h.max(1));
                let mut migrated = Vec::with_capacity(hidden_clients.len());
                if let Some((old_monitor, old_work)) = old_areas {
                    let new_work = rebase_work_area(old_monitor, old_work, new_monitor);
                    for client_key in hidden_clients {
                        if self.migrate_hidden_client_restore(
                            backend,
                            client_key,
                            Some(old_work),
                            new_monitor,
                            new_work,
                        ) {
                            migrated.push(client_key);
                        }
                    }
                }
                self.refresh_migrated_client_properties(backend, &migrated);
                let monitor_num = self.state.monitors.get(mon_key).map(|monitor| monitor.num);
                self.mark_bar_update_needed_if_visible(monitor_num);
                dirty = true;
            }
        }

        if self.state.monitor_order.len() > 1 {
            self.remove_excess_monitors(backend, 1);
            dirty = true;
        }

        if dirty {
            self.repark_all_hidden_clients(backend);
        }

        dirty
    }

    pub(crate) fn setup_multiple_monitors(
        &mut self,
        backend: &mut dyn Backend,
        monitors: Vec<(i32, i32, i32, i32)>,
    ) -> bool {
        let mut dirty = false;
        let num_detected_monitors = monitors.len();
        let current_num_monitors = self.state.monitor_order.len();

        if num_detected_monitors > current_num_monitors {
            dirty = true;
            for _ in current_num_monitors..num_detected_monitors {
                let new_monitor = self.createmon(CONFIG.load().show_bar());
                let mon_key = self.insert_monitor(new_monitor);
                info!(
                    "[setup_multiple_monitors] Created new monitor {:?}",
                    mon_key
                );
            }
        }

        for (i, &(x, y, w, h)) in monitors.iter().enumerate() {
            if let Some(&mon_key) = self.state.monitor_order.get(i) {
                let geometry_changed = self.state.monitors.get(mon_key).is_some_and(|monitor| {
                    monitor.geometry.m_x != x
                        || monitor.geometry.m_y != y
                        || monitor.geometry.m_w != w
                        || monitor.geometry.m_h != h
                });
                if geometry_changed {
                    let old_areas = self.monitor_migration_areas(mon_key);
                    let hidden_clients: Vec<ClientKey> =
                        clients_owned_by_monitor(&self.state, mon_key)
                            .into_iter()
                            .filter(|&client_key| {
                                self.state
                                    .clients
                                    .get(client_key)
                                    .is_some_and(|client| client.state.is_hidden)
                            })
                            .collect();
                    if let Some(monitor_num) =
                        self.state.monitors.get(mon_key).map(|monitor| monitor.num)
                    {
                        self.clear_minimized_dock_for_monitor(backend, monitor_num);
                    }
                    if let Some(monitor) = self.state.monitors.get_mut(mon_key) {
                        monitor.num = i as i32;
                        monitor.geometry.m_x = x;
                        monitor.geometry.w_x = x;
                        monitor.geometry.m_y = y;
                        monitor.geometry.w_y = y;
                        monitor.geometry.m_w = w;
                        monitor.geometry.w_w = w;
                        monitor.geometry.m_h = h;
                        monitor.geometry.w_h = h;
                    }
                    let new_monitor = Rect::new(x, y, w.max(1), h.max(1));
                    let mut migrated = Vec::with_capacity(hidden_clients.len());
                    if let Some((old_monitor, old_work)) = old_areas {
                        let new_work = rebase_work_area(old_monitor, old_work, new_monitor);
                        for client_key in hidden_clients {
                            if self.migrate_hidden_client_restore(
                                backend,
                                client_key,
                                Some(old_work),
                                new_monitor,
                                new_work,
                            ) {
                                migrated.push(client_key);
                            }
                        }
                    }
                    self.refresh_migrated_client_properties(backend, &migrated);
                    let monitor_num = self.state.monitors.get(mon_key).map(|monitor| monitor.num);
                    self.mark_bar_update_needed_if_visible(monitor_num);
                    dirty = true;
                }
            }
        }

        if num_detected_monitors < current_num_monitors {
            dirty = true;
            self.remove_excess_monitors(backend, num_detected_monitors);
        }

        if dirty {
            // Geometry updates above are intentionally applied in monitor
            // order. A later output may establish a new global left edge, so
            // finish with one topology-wide parking pass.
            self.repark_all_hidden_clients(backend);
        }

        dirty
    }

    pub(crate) fn remove_excess_monitors(
        &mut self,
        backend: &mut dyn Backend,
        target_count: usize,
    ) {
        while self.state.monitor_order.len() > target_count {
            if let Some(mon_key_to_remove) = self.state.monitor_order.pop() {
                let retired_bar_client = if let Some(monitor_num) = self
                    .state
                    .monitors
                    .get(mon_key_to_remove)
                    .map(|monitor| monitor.num)
                {
                    self.retire_secondary_bar(backend, monitor_num)
                } else {
                    None
                };
                self.move_clients_to_first_monitor(backend, mon_key_to_remove, retired_bar_client);

                remove_monitor_state(&mut self.state, mon_key_to_remove);
                self.last_stacking.remove(mon_key_to_remove);
                let dropped_scrolling_states =
                    self.drop_scrolling_states_for_monitor(mon_key_to_remove);
                self.repark_all_hidden_clients(backend);

                info!(
                    "[remove_excess_monitors] Removed monitor {:?}, dropped {} scrolling states",
                    mon_key_to_remove, dropped_scrolling_states
                );
            }
        }
        self.mark_bar_update_needed_if_visible(None);
    }

    pub(crate) fn move_clients_to_first_monitor(
        &mut self,
        backend: &mut dyn Backend,
        from_monitor_key: MonitorKey,
        retired_bar_client: Option<ClientKey>,
    ) {
        // 必须排除即将被移除的 from_monitor_key，否则当它恰好是 monitor_order[0]
        // 时 target==from，client 会被 detach 后又 attach 回这个随即删除的 monitor，
        // 导致 client.mon 指向已删 key 且不在任何列表中——永久孤立。
        let target_monitor_key = self
            .state
            .monitor_order
            .iter()
            .copied()
            .find(|&key| key != from_monitor_key);
        let source_work = self
            .monitor_migration_areas(from_monitor_key)
            .map(|(_, work)| work);
        let target_areas =
            target_monitor_key.and_then(|monitor| self.monitor_migration_areas(monitor));
        let tagmask = CONFIG.load().tagmask();
        let parked_scratchpads: HashSet<ClientKey> = self
            .scratchpads
            .values()
            .copied()
            .filter(|&client_key| {
                self.state
                    .clients
                    .get(client_key)
                    .is_some_and(|client| client.state.tags & tagmask == 0)
            })
            .collect();

        let reassigned = transfer_or_orphan_monitor_clients(
            &mut self.state,
            from_monitor_key,
            target_monitor_key,
            retired_bar_client,
            &parked_scratchpads,
        );

        if let Some(target_monitor_key) = target_monitor_key {
            for &client_key in &reassigned {
                if let Some((target_monitor, target_work)) = target_areas {
                    self.migrate_hidden_client_restore(
                        backend,
                        client_key,
                        source_work,
                        target_monitor,
                        target_work,
                    );
                }
                self.reorder_client_in_monitor_groups(client_key);
                info!(
                    "[move_clients_to_first_monitor] Moved client {:?} from monitor {:?} to {:?}",
                    client_key, from_monitor_key, target_monitor_key
                );
            }
        } else {
            warn!(
                "[move_clients_to_first_monitor] No target monitor available; orphaned {} clients until the next output is added",
                reassigned.len()
            );
        }
        // Refresh both migrated clients and deliberate `mon=None` orphans.
        // The latter records monitor=-1 now, then OutputAdded replaces it with
        // the new monitor identity after geometry convergence.
        self.refresh_migrated_client_properties(backend, &reassigned);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        attach_clients_to_monitor, attachable_unassigned_clients, lowest_unused_monitor_num,
        migrate_hidden_restore_geometry, rebase_work_area, remove_monitor_state,
        transfer_or_orphan_monitor_clients, translate_and_clamp_restore_rect,
    };
    use crate::backend::common_define::{OutputId, WindowId};
    use crate::core::models::{ClientKey, MonitorKey, WMClient, WMMonitor};
    use crate::core::state::WMState;
    use std::collections::HashSet;

    fn insert_monitor(state: &mut WMState, output: OutputId, tags: u32) -> MonitorKey {
        let mut monitor = WMMonitor::new();
        monitor.tag_set = [tags, tags];
        let monitor_key = state.monitors.insert(monitor);
        state.monitor_order.push(monitor_key);
        state.output_map.insert(monitor_key, output);
        state.monitor_clients.insert(monitor_key, Vec::new());
        state.monitor_stack.insert(monitor_key, Vec::new());
        monitor_key
    }

    fn insert_client(
        state: &mut WMState,
        window: u64,
        monitor: Option<MonitorKey>,
        attach_to_vectors: bool,
    ) -> ClientKey {
        let mut client = WMClient::new(WindowId::from_raw(window));
        client.mon = monitor;
        let client_key = state.clients.insert(client);
        state.client_order.push(client_key);
        state
            .win_to_client
            .insert(WindowId::from_raw(window), client_key);
        if attach_to_vectors && let Some(monitor) = monitor {
            state.monitor_clients[monitor].push(client_key);
            state.monitor_stack[monitor].push(client_key);
        }
        client_key
    }

    #[test]
    fn hotplug_monitor_number_fills_a_hole_without_colliding() {
        assert_eq!(lowest_unused_monitor_num([].iter()), 0);
        assert_eq!(lowest_unused_monitor_num([0, 1].iter()), 2);
        assert_eq!(lowest_unused_monitor_num([1].iter()), 0);
        assert_eq!(lowest_unused_monitor_num([0, 2, 3].iter()), 1);
        assert_eq!(lowest_unused_monitor_num([-1, 0, 0, 2].iter()), 1);
    }

    #[test]
    fn restore_translation_handles_negative_origins_and_destination_clamping() {
        let source = crate::core::types::Rect::new(-1920, 40, 1920, 1040);
        let target = crate::core::types::Rect::new(320, -300, 1000, 700);

        let translated = translate_and_clamp_restore_rect(
            crate::core::types::Rect::new(-1700, 140, 500, 300),
            Some(source),
            target,
            2,
        );
        assert_eq!(
            translated,
            crate::core::types::Rect::new(540, -200, 500, 300)
        );

        // A rectangle that was near the source's bottom-right remains fully
        // inside a smaller destination, including its two-pixel border.
        let clamped = translate_and_clamp_restore_rect(
            crate::core::types::Rect::new(-200, 900, 1400, 900),
            Some(source),
            target,
            2,
        );
        assert_eq!(clamped.x, target.x);
        assert_eq!(clamped.y, target.y);
        assert_eq!((clamped.w, clamped.h), (996, 696));
    }

    #[test]
    fn hidden_fullscreen_migration_uses_target_output_and_moves_exit_geometry() {
        let source_work = crate::core::types::Rect::new(0, 32, 1920, 1048);
        let target_monitor = crate::core::types::Rect::new(-1280, -120, 1280, 800);
        let target_work = crate::core::types::Rect::new(-1280, -88, 1280, 768);
        let mut client = WMClient::new(WindowId::from_raw(0x55));
        client.state.is_hidden = true;
        client.state.is_fullscreen = true;
        client.state.is_floating = true;
        client.state.old_state = true;
        client.geometry.hidden_restore_rect = Some(crate::core::types::Rect::new(0, 0, 1920, 1080));
        client.geometry.old_x = 220;
        client.geometry.old_y = 132;
        client.geometry.old_w = 800;
        client.geometry.old_h = 560;
        client.geometry.floating_x = 220;
        client.geometry.floating_y = 132;
        client.geometry.floating_w = 800;
        client.geometry.floating_h = 560;

        assert!(migrate_hidden_restore_geometry(
            &mut client,
            Some(source_work),
            target_monitor,
            target_work,
            -1280,
        ));

        assert_eq!(client.geometry.hidden_restore_rect, Some(target_monitor));
        assert_eq!(
            (
                client.geometry.old_x,
                client.geometry.old_y,
                client.geometry.old_w,
                client.geometry.old_h,
            ),
            (-1060, 12, 800, 560)
        );
        assert_eq!(
            (
                client.geometry.floating_x,
                client.geometry.floating_y,
                client.geometry.floating_w,
                client.geometry.floating_h,
            ),
            (-1060, 12, 800, 560)
        );
        assert!(client.geometry.x.saturating_add(client.total_width()) <= -1280);
    }

    #[test]
    fn output_resize_rebases_workarea_insets_before_restore_migration() {
        let old_monitor = crate::core::types::Rect::new(100, 20, 1600, 900);
        let old_work = crate::core::types::Rect::new(110, 60, 1570, 840);
        let new_monitor = crate::core::types::Rect::new(-900, -200, 1000, 640);
        assert_eq!(
            rebase_work_area(old_monitor, old_work, new_monitor),
            crate::core::types::Rect::new(-890, -160, 970, 580)
        );

        let stale_work = crate::core::types::Rect::new(50_000, 40_000, 20, 20);
        let bounded = rebase_work_area(old_monitor, stale_work, new_monitor);
        assert!(bounded.x >= new_monitor.x);
        assert!(bounded.y >= new_monitor.y);
        assert!(bounded.x + bounded.w <= new_monitor.x + new_monitor.w);
        assert!(bounded.y + bounded.h <= new_monitor.y + new_monitor.h);
    }

    #[test]
    fn removing_the_only_output_clears_every_old_monitor_owner() {
        let mut state = WMState::new();
        let removed = insert_monitor(&mut state, OutputId(7), 0b0001);
        state.sel_mon = Some(removed);
        state.motion_mon = Some(removed);

        let minimized = insert_client(&mut state, 0x101, Some(removed), true);
        state.clients[minimized].state.tags = 0b0100;
        state.clients[minimized].state.is_hidden = true;
        state.clients[minimized].state.minimized_order = 23;

        // Registry-only ownership models a client that received `mon` before
        // the Wayland monitor vectors were fully populated.
        let partially_attached = insert_client(&mut state, 0x102, Some(removed), false);
        state.clients[partially_attached].state.tags = 0b0010;

        // A bar that survived a late unmanage error must have its stale key
        // cleared, but it is not a client to reassign on the next output.
        let retired_bar = insert_client(&mut state, 0x103, Some(removed), true);
        let parked_scratchpad = insert_client(&mut state, 0x104, None, false);
        state.clients[parked_scratchpad].state.tags = 0;

        let orphaned = transfer_or_orphan_monitor_clients(
            &mut state,
            removed,
            None,
            Some(retired_bar),
            &HashSet::from([parked_scratchpad]),
        );
        assert_eq!(orphaned, vec![minimized, partially_attached]);
        assert!(state.clients[minimized].mon.is_none());
        assert!(state.clients[partially_attached].mon.is_none());
        assert!(state.clients[retired_bar].mon.is_none());
        assert!(state.clients[parked_scratchpad].mon.is_none());
        assert!(state.clients[minimized].state.is_hidden);
        assert_eq!(state.clients[minimized].state.minimized_order, 23);
        assert_eq!(state.clients[minimized].state.tags, 0b0100);

        remove_monitor_state(&mut state, removed);

        assert!(state.monitors.is_empty());
        assert!(state.monitor_order.is_empty());
        assert!(state.output_map.get(removed).is_none());
        assert!(state.monitor_clients.get(removed).is_none());
        assert!(state.monitor_stack.get(removed).is_none());
        assert!(state.sel_mon.is_none());
        assert!(state.motion_mon.is_none());
        assert!(
            state
                .clients
                .values()
                .all(|client| client.mon != Some(removed))
        );
    }

    #[test]
    fn output_readd_adopts_orphans_and_restores_the_minimized_projection() {
        let mut state = WMState::new();
        let removed = insert_monitor(&mut state, OutputId(11), 0b0001);
        state.sel_mon = Some(removed);

        let minimized = insert_client(&mut state, 0x201, Some(removed), true);
        state.clients[minimized].state.tags = 0b0100;
        state.clients[minimized].state.is_hidden = true;
        state.clients[minimized].state.minimized_order = 41;
        let untagged = insert_client(&mut state, 0x202, Some(removed), true);
        state.clients[untagged].state.tags = 0;

        let minimized_scratchpad = insert_client(&mut state, 0x205, Some(removed), true);
        state.clients[minimized_scratchpad].state.tags = 0;
        state.clients[minimized_scratchpad].state.is_hidden = true;
        state.clients[minimized_scratchpad].state.minimized_order = 42;

        let parked_scratchpad = insert_client(&mut state, 0x203, None, false);
        state.clients[parked_scratchpad].state.tags = 0;
        let bar_client = insert_client(&mut state, 0x204, None, false);

        let orphaned = transfer_or_orphan_monitor_clients(
            &mut state,
            removed,
            None,
            None,
            &HashSet::from([parked_scratchpad, minimized_scratchpad]),
        );
        assert_eq!(orphaned, vec![minimized, untagged, minimized_scratchpad]);
        remove_monitor_state(&mut state, removed);

        let replacement = insert_monitor(&mut state, OutputId(12), 0b0010);
        state.sel_mon = Some(replacement);
        let parked_scratchpads = HashSet::from([parked_scratchpad, minimized_scratchpad]);
        let bar_clients = HashSet::from([bar_client]);
        let attachable = attachable_unassigned_clients(&state, &parked_scratchpads, &bar_clients);
        assert_eq!(attachable, vec![minimized, untagged, minimized_scratchpad]);
        let attached =
            attach_clients_to_monitor(&mut state, replacement, &attachable, &parked_scratchpads);
        assert_eq!(attached, vec![minimized, untagged, minimized_scratchpad]);

        assert_eq!(state.clients[minimized].mon, Some(replacement));
        assert_eq!(state.clients[minimized].state.tags, 0b0100);
        assert!(state.clients[minimized].state.is_hidden);
        assert_eq!(state.clients[minimized].state.minimized_order, 41);
        assert_eq!(state.clients[untagged].mon, Some(replacement));
        assert_eq!(state.clients[untagged].state.tags, 0b0010);
        assert_eq!(state.clients[minimized_scratchpad].mon, Some(replacement));
        assert_eq!(state.clients[minimized_scratchpad].state.tags, 0);
        assert!(state.clients[minimized_scratchpad].state.is_hidden);
        assert!(state.clients[parked_scratchpad].mon.is_none());
        assert!(state.clients[bar_client].mon.is_none());

        let minimized_projection: Vec<ClientKey> = state.monitor_clients[replacement]
            .iter()
            .copied()
            .filter(|&client_key| {
                let client = &state.clients[client_key];
                client.state.is_hidden && client.state.minimized_order != 0
            })
            .collect();
        assert_eq!(minimized_projection, vec![minimized, minimized_scratchpad]);
        assert!(state.monitor_stack[replacement].contains(&minimized));
        assert!(
            state
                .clients
                .values()
                .all(|client| client.mon != Some(removed))
        );
    }

    #[test]
    fn monitor_migration_keeps_a_source_owned_scratchpad_parked() {
        let mut state = WMState::new();
        let source = insert_monitor(&mut state, OutputId(21), 0b0001);
        let target = insert_monitor(&mut state, OutputId(22), 0b0100);
        let scratchpad = insert_client(&mut state, 0x301, Some(source), true);
        state.clients[scratchpad].state.tags = 0;
        let ordinary_untagged = insert_client(&mut state, 0x302, Some(source), true);
        state.clients[ordinary_untagged].state.tags = 0;

        let migrated = transfer_or_orphan_monitor_clients(
            &mut state,
            source,
            Some(target),
            None,
            &HashSet::from([scratchpad]),
        );

        assert_eq!(migrated, vec![scratchpad, ordinary_untagged]);
        assert_eq!(state.clients[scratchpad].mon, Some(target));
        assert_eq!(state.clients[scratchpad].state.tags, 0);
        assert_eq!(state.clients[ordinary_untagged].mon, Some(target));
        assert_eq!(state.clients[ordinary_untagged].state.tags, 0b0100);
    }
}
