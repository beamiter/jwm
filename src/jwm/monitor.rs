// Monitor management operations: output handling, geometry, and client distribution

use crate::Jwm;
use crate::backend::api::Backend;
use crate::backend::common_define::OutputId;
use crate::config::{BackendFamily, CONFIG, get_backend_family};
use crate::core::maximize::maximize_target;
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

/// What [`migrate_visible_geometry`] did to a client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VisibleMigration {
    /// Nothing to carry: minimized (the hidden path owns it), the bar (its
    /// own placement owns it), a tiled window (the next `arrange` places
    /// it), or a window already on the target output — a drag released
    /// there, whose position is exactly what the user chose, or a fullscreen
    /// window that already fills it.
    Unchanged,
    /// Parked off-screen because its tag is not shown: only the rectangle it
    /// comes back to moved; the real window stays where it is.
    Parked,
    /// On screen: the live rectangle moved and must be applied.
    Live,
}

/// Move a non-minimized client's geometry to a new output. A fullscreen
/// window fills the target output; a maximized window fills the target work
/// area on its maximized axes; a floating or PiP window keeps its offset
/// within the work area, clamped to fit. The floating, pre-fullscreen and
/// pre-maximize slots follow too, so toggling floating, leaving fullscreen or
/// unmaximizing later does not jump back to the source.
fn migrate_visible_geometry(
    client: &mut WMClient,
    source_work: Option<Rect>,
    target_monitor: Rect,
    target_work: Rect,
) -> VisibleMigration {
    if client.state.is_hidden || client.state.is_dock {
        return VisibleMigration::Unchanged;
    }
    let parked = client.geometry.hidden_x.is_some();
    let live = Rect::new(
        client.geometry.x,
        client.geometry.y,
        client.geometry.w,
        client.geometry.h,
    );
    let border_width = client.geometry.border_w;
    let translate =
        |rect: Rect| translate_and_clamp_restore_rect(rect, source_work, target_work, border_width);

    // The pre-maximize rectangle is relative to the work area, not to the
    // live window: it is rebased even when the window itself stays put (a
    // same-output resize, or a drop already on the target), so unmaximizing
    // lands inside the new work area.
    let axes = client.state.maximized_axes();
    let maximize_restore = if axes.any() {
        client.geometry.maximize_restore_rect.map(translate)
    } else {
        client.geometry.maximize_restore_rect
    };
    client.geometry.maximize_restore_rect = maximize_restore;

    // A window already on the target stays where it is. A fullscreen window
    // is not "somewhere on" its output but the whole of it, so it stays only
    // when it already fills the target exactly: after a same-output mode or
    // scale change the old rectangle's centre is still inside, and the window
    // must be refit rather than left at the old size.
    let already_placed = if client.state.is_fullscreen {
        live == target_monitor
    } else {
        rect_center_inside(live, target_monitor)
    };
    if !parked && already_placed {
        return VisibleMigration::Unchanged;
    }

    let floating = Rect::new(
        client.geometry.floating_x,
        client.geometry.floating_y,
        client.geometry.floating_w,
        client.geometry.floating_h,
    );
    if let Some(floating) = valid_rect(floating) {
        let floating = translate(floating);
        client.geometry.floating_x = floating.x;
        client.geometry.floating_y = floating.y;
        client.geometry.floating_w = floating.w;
        client.geometry.floating_h = floating.h;
    }

    // Where the window is (or, parked, comes back to) on the target.
    let current = if parked {
        client.geometry.hidden_restore_rect.and_then(valid_rect)
    } else {
        valid_rect(live)
    };
    let moved = if client.state.is_fullscreen {
        let old = Rect::new(
            client.geometry.old_x,
            client.geometry.old_y,
            client.geometry.old_w,
            client.geometry.old_h,
        );
        if let Some(old) = valid_rect(old) {
            let old = translate(old);
            client.geometry.old_x = old.x;
            client.geometry.old_y = old.y;
            client.geometry.old_w = old.w;
            client.geometry.old_h = old.h;
        }
        target_monitor
    } else if client.state.is_maximize_realized() {
        // Maximize owns the geometry: refill the target work area on the
        // maximized axes, keeping the free axes from the translated restore.
        let Some(restore) = maximize_restore.or_else(|| current.map(translate)) else {
            return VisibleMigration::Unchanged;
        };
        client.geometry.maximize_restore_rect = Some(restore);
        // M4: a non-promoted client's floating slot is the restore rect. A
        // promoted client's floating slot is its independent pre-promotion
        // rect, already translated above.
        if !client.state.maximize_restore_tiled {
            client.geometry.floating_x = restore.x;
            client.geometry.floating_y = restore.y;
            client.geometry.floating_w = restore.w;
            client.geometry.floating_h = restore.h;
        }
        maximize_target(restore, target_work, axes, border_width)
    } else if client.state.is_floating || client.state.is_pip {
        let Some(current) = current else {
            return VisibleMigration::Unchanged;
        };
        let moved = translate(current);
        // PiP owns `floating_*` as its return slot (the pre-PiP rect),
        // already translated above; the corner rect must not replace it.
        if !client.state.is_pip {
            client.geometry.floating_x = moved.x;
            client.geometry.floating_y = moved.y;
            client.geometry.floating_w = moved.w;
            client.geometry.floating_h = moved.h;
        }
        moved
    } else {
        return VisibleMigration::Unchanged;
    };

    if parked {
        client.geometry.hidden_restore_rect = Some(moved);
        return VisibleMigration::Parked;
    }
    client.geometry.x = moved.x;
    client.geometry.y = moved.y;
    client.geometry.w = moved.w;
    client.geometry.h = moved.h;
    VisibleMigration::Live
}

fn rect_center_inside(rect: Rect, area: Rect) -> bool {
    let cx = i64::from(rect.x) + i64::from(rect.w.max(0)) / 2;
    let cy = i64::from(rect.y) + i64::from(rect.h.max(0)) / 2;
    cx >= i64::from(area.x)
        && cy >= i64::from(area.y)
        && cx < i64::from(area.x) + i64::from(area.w)
        && cy < i64::from(area.y) + i64::from(area.h)
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

    // The pre-maximize rectangle follows the work area first; a realized
    // maximized window comes back filling the target work area around it.
    let axes = client.state.maximized_axes();
    let maximize_restore = if axes.any() {
        client.geometry.maximize_restore_rect.map(|rect| {
            translate_and_clamp_restore_rect(rect, source_work, target_work, border_width)
        })
    } else {
        client.geometry.maximize_restore_rect
    };
    client.geometry.maximize_restore_rect = maximize_restore;
    let realized_restore = maximize_restore.filter(|_| client.state.is_maximize_realized());

    let visible = if client.state.is_fullscreen {
        target_monitor
    } else if let Some(restore) = realized_restore {
        maximize_target(restore, target_work, axes, border_width)
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
        // from resurrecting coordinates from the source output. A maximized
        // one keeps the pre-maximize rect there instead (M4), and a window
        // maximize promoted out of the tiling keeps its own pre-promotion
        // floating rect, translated like the visible path does. So does a
        // PiP window: its floating slot is the pre-PiP rect it returns to,
        // not the corner rect it is minimized from.
        let own_slot = || {
            valid_rect(Rect::new(
                client.geometry.floating_x,
                client.geometry.floating_y,
                client.geometry.floating_w,
                client.geometry.floating_h,
            ))
            .map(|rect| {
                translate_and_clamp_restore_rect(rect, source_work, target_work, border_width)
            })
        };
        let floating = match realized_restore {
            Some(restore) if !client.state.maximize_restore_tiled => Some(restore),
            Some(_) => own_slot(),
            None if client.state.is_pip => own_slot(),
            None => Some(visible),
        };
        if let Some(floating) = floating {
            client.geometry.floating_x = floating.x;
            client.geometry.floating_y = floating.y;
            client.geometry.floating_w = floating.w;
            client.geometry.floating_h = floating.h;
        }
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

/// The output rectangles for `setup_multiple_monitors`, reordered so that
/// position `i` (the monitor at `monitor_order[i]`, or a monitor about to be
/// created there) receives the output that monitor already stands for.
///
/// `setup_multiple_monitors` hands rectangles out by position. On Wayland a
/// non-tail unplug and re-plug leaves `monitor_order` in a different order
/// than `enumerate_outputs`, and handing out by position then swaps geometry
/// between two monitors that both still have their output. Here a surviving
/// position (one below the output count; `remove_excess_monitors` drops the
/// tail) whose mapped output is still enumerated keeps it; every other
/// position takes the next unclaimed output in enumeration order, which is
/// exactly the positional result when nothing moved. The result has one
/// rectangle per output.
fn plan_monitor_rects_by_output(
    monitor_outputs: &[Option<OutputId>],
    outputs: &[(OutputId, Rect)],
) -> Vec<Rect> {
    let survivors = monitor_outputs.len().min(outputs.len());
    let mut claimed = vec![false; outputs.len()];
    let mut slots: Vec<Option<usize>> = vec![None; outputs.len()];
    for (position, mapped) in monitor_outputs.iter().take(survivors).enumerate() {
        let Some(mapped) = mapped else {
            continue;
        };
        if let Some(index) = outputs.iter().position(|(id, _)| id == mapped)
            && !claimed[index]
        {
            claimed[index] = true;
            slots[position] = Some(index);
        }
    }
    // Exactly as many open slots as unclaimed outputs: each claim filled one
    // of each.
    let mut unclaimed = (0..outputs.len()).filter(|&index| !claimed[index]);
    slots
        .into_iter()
        .filter_map(|slot| slot.or_else(|| unclaimed.next()))
        .map(|index| outputs[index].1)
        .collect()
}

/// Which output each monitor stands for after a display refresh, as
/// `(monitor, output)` pairs in `monitors` order.
///
/// X11 reports every display change as "the layout changed": `updategeom`
/// hands the enumerated rectangles to the monitors by position and creates or
/// drops monitors at the tail, so the output id a monitor was created with no
/// longer says which output it covers, and a monitor created there has none.
/// Pointer lookups (`recttomon`) resolve through these ids, and a monitor
/// without the right one can never be selected by the pointer.
///
/// A monitor keeps its id while that output still has the monitor's
/// rectangle. Otherwise it takes an output with its rectangle, preferring the
/// one at its own position (clones share a rectangle), and failing that the
/// output at its position, which is where its geometry came from. Each output
/// is claimed once; a monitor left without one gets no entry.
fn plan_output_map(
    monitors: &[(MonitorKey, Rect, Option<OutputId>)],
    outputs: &[(OutputId, Rect)],
) -> Vec<(MonitorKey, OutputId)> {
    let mut claimed: HashSet<OutputId> = HashSet::new();
    let mut assigned: Vec<Option<OutputId>> = vec![None; monitors.len()];

    for (slot, &(_, rect, current)) in assigned.iter_mut().zip(monitors) {
        if let Some(id) = current
            && outputs
                .iter()
                .any(|&(output, output_rect)| output == id && output_rect == rect)
            && claimed.insert(id)
        {
            *slot = Some(id);
        }
    }

    // An output with the monitor's rectangle, the one at its position first.
    for (index, (slot, &(_, rect, _))) in assigned.iter_mut().zip(monitors).enumerate() {
        if slot.is_some() {
            continue;
        }
        let fits = |&&(output, output_rect): &&(OutputId, Rect)| {
            output_rect == rect && !claimed.contains(&output)
        };
        let matched = outputs
            .get(index)
            .filter(fits)
            .or_else(|| outputs.iter().find(fits));
        if let Some(&(output, _)) = matched {
            claimed.insert(output);
            *slot = Some(output);
        }
    }

    // The output at the monitor's position, which its geometry came from.
    for (index, slot) in assigned.iter_mut().enumerate() {
        if slot.is_none()
            && let Some(&(output, _)) = outputs.get(index)
            && claimed.insert(output)
        {
            *slot = Some(output);
        }
    }

    monitors
        .iter()
        .zip(assigned)
        .filter_map(|(&(monitor, _, _), id)| id.map(|id| (monitor, id)))
        .collect()
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
    /// Carry a visible fullscreen, floating or PiP client to its new output
    /// and apply the geometry; see [`migrate_visible_geometry`].
    pub(super) fn migrate_visible_client(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        source_work: Option<Rect>,
        target_monitor: Rect,
        target_work: Rect,
    ) -> bool {
        let outcome = self.state.clients.get_mut(client_key).map(|client| {
            migrate_visible_geometry(client, source_work, target_monitor, target_work)
        });
        match outcome {
            Some(VisibleMigration::Live) => {}
            Some(VisibleMigration::Parked) => return true,
            Some(VisibleMigration::Unchanged) | None => return false,
        }
        let Some((live, fullscreen)) = self.state.clients.get(client_key).map(|client| {
            (
                Rect::new(
                    client.geometry.x,
                    client.geometry.y,
                    client.geometry.w,
                    client.geometry.h,
                ),
                client.state.is_fullscreen,
            )
        }) else {
            return false;
        };
        let _ = if fullscreen {
            // The translated pre-fullscreen rectangle is the return slot.
            self.refit_keeping_restore_slot(backend, client_key, live)
        } else {
            self.resizeclient(backend, client_key, live.x, live.y, live.w, live.h)
        };
        true
    }

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
        // Monitor numbers are protocol identities (bar shm key, Dock command
        // source, saved per-tag layouts), not the current slotmap length.
        // Reusing `len()` after a non-tail hot-unplug can collide with a
        // surviving monitor.
        let num = lowest_unused_monitor_num(self.state.monitors.values().map(|monitor| &monitor.num));
        let mut m = self.createmon_numbered(CONFIG.load().show_bar(), num);

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
        // Every display change re-validates the monitor locks, as
        // `updategeom` does for X11: a shade must never outlive the output
        // rectangle it was cut for.
        self.prune_monitor_locks(backend);
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
            // An orphan the last output left on screen still carries that
            // output's coordinates: a fullscreen one refills this output and
            // a floating one is pulled onto it, as the unplug would have done
            // had a monitor survived. A window mapped before any output
            // existed is already here and stays where it is.
            if let Some((target_monitor, target_work)) = target_areas
                && !self.migrate_hidden_client_restore(
                    backend,
                    client_key,
                    None,
                    target_monitor,
                    target_work,
                )
            {
                self.migrate_visible_client(backend, client_key, None, target_monitor, target_work);
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
            // Before any focus decision: the lock on a vanished output comes
            // off (its number may be handed to the next output plugged in),
            // the last unlocked output going away lifts the oldest lock, and
            // a selection that fell back onto a shaded monitor moves off it,
            // so focus never lands on a window nobody can see.
            self.prune_monitor_locks(backend);

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
            let owned_clients = clients_owned_by_monitor(&self.state, mon_key);
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
            let migrated = self.migrate_monitor_clients(
                backend,
                owned_clients,
                old_work,
                new_monitor,
                new_work,
            );
            self.repark_all_hidden_clients(backend);
            self.arrange(backend, Some(mon_key));
            self.refresh_migrated_client_properties(backend, &migrated);
            self.mark_bar_update_needed_if_visible(monitor_num);
            // A mode or scale change resizes the output under its shade.
            self.prune_monitor_locks(backend);
        }
        Ok(())
    }

    /// Carry every client a monitor owns across a change of that monitor's
    /// rectangle, from `old_work` to `new_work`. Minimized clients get their
    /// restore slot rebased; visible fullscreen windows refit the changed
    /// output, and floating or PiP ones keep their offset within the rebased
    /// work area; tiled ones are left to the next `arrange`. Returns the
    /// minimized clients that moved, whose published restore state must be
    /// refreshed once the monitor's geometry is final.
    ///
    /// The one migration for Wayland's `OutputChanged` and X11's positional
    /// refresh alike: a floating window left at its old absolute coordinates
    /// ends up drawn on a neighbouring output while its monitor still owns it.
    fn migrate_monitor_clients(
        &mut self,
        backend: &mut dyn Backend,
        clients: Vec<ClientKey>,
        old_work: Rect,
        new_monitor: Rect,
        new_work: Rect,
    ) -> Vec<ClientKey> {
        let mut migrated = Vec::new();
        for client_key in clients {
            if self.migrate_hidden_client_restore(
                backend,
                client_key,
                Some(old_work),
                new_monitor,
                new_work,
            ) {
                migrated.push(client_key);
            } else {
                self.migrate_visible_client(
                    backend,
                    client_key,
                    Some(old_work),
                    new_monitor,
                    new_work,
                );
            }
        }
        migrated
    }

    pub(crate) fn updategeom(&mut self, backend: &mut dyn Backend) -> bool {
        info!("[updategeom]");
        let outputs = backend.output_ops().enumerate_outputs();

        let dirty = if outputs.len() <= 1 {
            let output = outputs
                .first()
                .map(|output| Rect::new(output.x, output.y, output.width, output.height));
            self.setup_single_monitor(backend, output)
        } else {
            let mons: Vec<(i32, i32, i32, i32)> = if get_backend_family() == BackendFamily::Wayland
            {
                // Wayland outputs carry stable ids that `output_map` tracks
                // through hotplug, so each monitor keeps its own output. X11
                // keeps the positional hand-out: RandR's order is what the
                // monitors are numbered by.
                let monitor_outputs: Vec<Option<OutputId>> = self
                    .state
                    .monitor_order
                    .iter()
                    .map(|key| self.state.output_map.get(*key).copied())
                    .collect();
                let outputs: Vec<(OutputId, Rect)> = outputs
                    .iter()
                    .map(|o| (o.id, Rect::new(o.x, o.y, o.width, o.height)))
                    .collect();
                plan_monitor_rects_by_output(&monitor_outputs, &outputs)
                    .into_iter()
                    .map(|rect| (rect.x, rect.y, rect.w, rect.h))
                    .collect()
            } else {
                outputs
                    .iter()
                    .map(|o| (o.x, o.y, o.width, o.height))
                    .collect()
            };
            self.setup_multiple_monitors(backend, mons)
        };
        // Before anything resolves a point to a monitor, the selection
        // re-pick below included.
        self.reconcile_output_map(&outputs);

        if dirty {
            let root_window = backend.root_window();
            self.state.sel_mon = self.wintomon(backend, root_window);
            if self.state.sel_mon.is_none() && !self.state.monitor_order.is_empty() {
                self.state.sel_mon = self.state.monitor_order.first().copied();
            }
        }

        // Update compositor with current monitor geometries (for per-monitor wallpaper)
        self.refresh_compositor_monitors(backend);
        // Outputs just moved, arrived or went away, so the rectangles the
        // lock shades were cut for may no longer describe anything.
        self.prune_monitor_locks(backend);

        dirty
    }

    /// Re-point `output_map` at the outputs the monitors now cover; see
    /// [`plan_output_map`]. An empty enumeration says nothing about which
    /// output is where, so it leaves the map alone.
    fn reconcile_output_map(&mut self, outputs: &[crate::backend::api::OutputInfo]) {
        if outputs.is_empty() {
            return;
        }
        let monitors: Vec<(MonitorKey, Rect, Option<OutputId>)> = self
            .state
            .monitor_order
            .iter()
            .filter_map(|&key| {
                let monitor = self.state.monitors.get(key)?;
                let rect = Rect::new(
                    monitor.geometry.m_x,
                    monitor.geometry.m_y,
                    monitor.geometry.m_w,
                    monitor.geometry.m_h,
                );
                Some((key, rect, self.state.output_map.get(key).copied()))
            })
            .collect();
        let outputs: Vec<(OutputId, Rect)> = outputs
            .iter()
            .map(|output| {
                (
                    output.id,
                    Rect::new(output.x, output.y, output.width, output.height),
                )
            })
            .collect();
        let planned = plan_output_map(&monitors, &outputs);
        self.state.output_map.clear();
        for (monitor, output) in planned {
            self.state.output_map.insert(monitor, output);
        }
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

    /// Converge on one monitor covering `output`, the single enumerated
    /// output's rectangle, or the whole screen (`s_w` x `s_h`) when the
    /// backend enumerated none.
    ///
    /// The output's own rectangle, not the screen size: `s_w`/`s_h` change
    /// only at startup and on an X11 root ConfigureNotify, so on Wayland a
    /// mode or scale change (or a nested host resize) that `OutputChanged`
    /// already applied would be reverted here to the startup size.
    pub(crate) fn setup_single_monitor(
        &mut self,
        backend: &mut dyn Backend,
        output: Option<Rect>,
    ) -> bool {
        let mut dirty = false;
        let target = output.unwrap_or_else(|| Rect::new(0, 0, self.s_w, self.s_h));

        if self.state.monitor_order.is_empty() {
            let new_monitor = self.createmon(CONFIG.load().show_bar());
            let mon_key = self.insert_monitor(new_monitor);
            self.state.sel_mon = Some(mon_key);
            dirty = true;
        }

        if let Some(&mon_key) = self.state.monitor_order.first() {
            let geometry_changed = self.state.monitors.get(mon_key).is_some_and(|monitor| {
                monitor.geometry.m_x != target.x
                    || monitor.geometry.m_y != target.y
                    || monitor.geometry.m_w != target.w
                    || monitor.geometry.m_h != target.h
            });
            if geometry_changed {
                let old_areas = self.monitor_migration_areas(mon_key);
                let owned_clients = clients_owned_by_monitor(&self.state, mon_key);
                if let Some(monitor_num) =
                    self.state.monitors.get(mon_key).map(|monitor| monitor.num)
                {
                    self.clear_minimized_dock_for_monitor(backend, monitor_num);
                }
                if let Some(monitor) = self.state.monitors.get_mut(mon_key) {
                    monitor.num = 0;
                    monitor.geometry.m_x = target.x;
                    monitor.geometry.w_x = target.x;
                    monitor.geometry.m_y = target.y;
                    monitor.geometry.w_y = target.y;
                    monitor.geometry.m_w = target.w;
                    monitor.geometry.w_w = target.w;
                    monitor.geometry.m_h = target.h;
                    monitor.geometry.w_h = target.h;
                }
                let new_monitor = Rect::new(target.x, target.y, target.w.max(1), target.h.max(1));
                let migrated = match old_areas {
                    Some((old_monitor, old_work)) => {
                        let new_work = rebase_work_area(old_monitor, old_work, new_monitor);
                        self.migrate_monitor_clients(
                            backend,
                            owned_clients,
                            old_work,
                            new_monitor,
                            new_work,
                        )
                    }
                    None => Vec::new(),
                };
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
                    let owned_clients = clients_owned_by_monitor(&self.state, mon_key);
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
                    let migrated = match old_areas {
                        Some((old_monitor, old_work)) => {
                            let new_work = rebase_work_area(old_monitor, old_work, new_monitor);
                            self.migrate_monitor_clients(
                                backend,
                                owned_clients,
                                old_work,
                                new_monitor,
                                new_work,
                            )
                        }
                        None => Vec::new(),
                    };
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
        //
        // A shaded survivor would hide the windows behind its lock shade, so
        // the first unlocked one takes them; only when every survivor is
        // locked do they go to a shaded one (and stay unfocusable there).
        let survivors = || {
            self.state
                .monitor_order
                .iter()
                .copied()
                .filter(|&key| key != from_monitor_key)
        };
        let target_monitor_key = survivors()
            .find(|&key| !self.monitor_key_is_locked(key))
            .or_else(|| survivors().next());
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
                    if !self.migrate_hidden_client_restore(
                        backend,
                        client_key,
                        source_work,
                        target_monitor,
                        target_work,
                    ) {
                        self.migrate_visible_client(
                            backend,
                            client_key,
                            source_work,
                            target_monitor,
                            target_work,
                        );
                    }
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

/// A backend for display-change tests, here and in the session restore's:
/// dummy ops, a running compositor, outputs a test rearranges between calls
/// the way RandR or a DRM hotplug would, and the lock shades pushed.
#[cfg(test)]
pub(crate) mod test_support {
    use crate::backend::api::{
        Backend, BackendDiagnostics, Capabilities, ColorAllocator, CompositorAnnotation,
        CompositorBenchmark, CompositorControl, CompositorMedia, CompositorWindowEffects,
        CompositorWorkspaceEffects, CursorProvider, DisplayControl, EventHandler, InputOps, KeyOps,
        MonitorShade, OutputIdentity, OutputInfo, OutputOps, PropertyOps, RenderScheduler,
        ScreenInfo, WindowOps,
    };
    use crate::backend::common_define::{OutputId, WindowId};
    use crate::backend::error::BackendError;
    use crate::backend::wayland_dummy_ops::{
        DummyColorAllocator, DummyCursorProvider, DummyInputOps, DummyKeyOps, DummyPropertyOps,
        DummyWindowOps,
    };

    /// An output of `width` x `height` at (`x`, `y`).
    pub(crate) fn output(id: u64, x: i32, y: i32, width: i32, height: i32) -> OutputInfo {
        let name = format!("Spy-{id}");
        OutputInfo {
            id: OutputId(id),
            name: name.clone(),
            x,
            y,
            width,
            height,
            scale: 1.0,
            refresh_rate: 60_000,
            hdr_capable: false,
            hdr_metadata: None,
            identity: OutputIdentity::connector_only(name),
        }
    }

    /// The outputs the backend reports; a test replaces them to change the
    /// display layout.
    pub(crate) struct SpyOutputOps {
        pub(crate) outputs: Vec<OutputInfo>,
    }

    impl OutputOps for SpyOutputOps {
        fn enumerate_outputs(&self) -> Vec<OutputInfo> {
            self.outputs.clone()
        }

        fn screen_info(&self) -> ScreenInfo {
            let right = self
                .outputs
                .iter()
                .map(|output| output.x.saturating_add(output.width))
                .max()
                .unwrap_or(1);
            let bottom = self
                .outputs
                .iter()
                .map(|output| output.y.saturating_add(output.height))
                .max()
                .unwrap_or(1);
            ScreenInfo {
                width: right.max(1),
                height: bottom.max(1),
            }
        }

        fn output_at(&self, x: i32, y: i32) -> Option<OutputId> {
            self.outputs
                .iter()
                .find(|output| {
                    x >= output.x
                        && y >= output.y
                        && x < output.x.saturating_add(output.width)
                        && y < output.y.saturating_add(output.height)
                })
                .map(|output| output.id)
        }
    }

    pub(crate) struct DisplaySpyBackend {
        window_ops: DummyWindowOps,
        input_ops: DummyInputOps,
        property_ops: DummyPropertyOps,
        pub(crate) output_ops: SpyOutputOps,
        key_ops: DummyKeyOps,
        cursor_provider: DummyCursorProvider,
        color_allocator: DummyColorAllocator,
        /// Every lock-shade payload pushed, newest last.
        pub(crate) shade_pushes: Vec<Vec<MonitorShade>>,
    }

    impl DisplaySpyBackend {
        pub(crate) fn new(outputs: Vec<OutputInfo>) -> Self {
            Self {
                window_ops: DummyWindowOps,
                input_ops: DummyInputOps,
                property_ops: DummyPropertyOps,
                output_ops: SpyOutputOps { outputs },
                key_ops: DummyKeyOps,
                cursor_provider: DummyCursorProvider,
                color_allocator: DummyColorAllocator,
                shade_pushes: Vec::new(),
            }
        }
    }

    impl CompositorBenchmark for DisplaySpyBackend {}
    impl BackendDiagnostics for DisplaySpyBackend {}
    impl CompositorControl for DisplaySpyBackend {}
    impl CompositorMedia for DisplaySpyBackend {}
    impl CompositorWorkspaceEffects for DisplaySpyBackend {
        fn compositor_set_monitor_shades(&mut self, shades: &[MonitorShade]) {
            self.shade_pushes.push(shades.to_vec());
        }
    }
    impl CompositorWindowEffects for DisplaySpyBackend {}
    impl CompositorAnnotation for DisplaySpyBackend {}
    impl DisplayControl for DisplaySpyBackend {}
    impl RenderScheduler for DisplaySpyBackend {
        fn has_compositor(&self) -> bool {
            true
        }
    }

    impl Backend for DisplaySpyBackend {
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
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{DisplaySpyBackend, output};
    use super::{
        VisibleMigration, attach_clients_to_monitor, attachable_unassigned_clients,
        lowest_unused_monitor_num, migrate_hidden_restore_geometry, migrate_visible_geometry,
        monitor_rect, plan_monitor_rects_by_output, plan_output_map, rebase_work_area,
        remove_monitor_state, transfer_or_orphan_monitor_clients, translate_and_clamp_restore_rect,
    };
    use crate::backend::api::MaximizeAxes;
    use crate::backend::common_define::{OutputId, WindowId};
    use crate::core::maximize::maximize_target;
    use crate::core::models::{ClientKey, MonitorKey, WMClient, WMMonitor};
    use crate::core::state::WMState;
    use crate::core::types::Rect;
    use crate::jwm::Jwm;
    use crate::jwm::types::WMArgEnum;
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

    // Output-migration geometry of maximized clients. The source output is
    // a 1080p primary with a 32 px bar; the target sits to its right with a
    // 30 px bar.
    const MAXIMIZE_SOURCE_WORK: Rect = Rect {
        x: 0,
        y: 32,
        w: 1920,
        h: 1048,
    };
    const MAXIMIZE_TARGET_MONITOR: Rect = Rect {
        x: 1920,
        y: 0,
        w: 1280,
        h: 720,
    };
    const MAXIMIZE_TARGET_WORK: Rect = Rect {
        x: 1920,
        y: 30,
        w: 1280,
        h: 690,
    };

    /// A floating client maximized on both axes over the source work area,
    /// whose pre-maximize rect is `restore`.
    fn maximized_floating_client(restore: Rect, border_w: i32) -> WMClient {
        let mut client = WMClient::new(WindowId::from_raw(0x3a0));
        client.state.is_floating = true;
        client.state.set_maximized_axes(MaximizeAxes::BOTH);
        client.geometry.border_w = border_w;
        client.geometry.maximize_restore_rect = Some(restore);
        let live = maximize_target(restore, MAXIMIZE_SOURCE_WORK, MaximizeAxes::BOTH, border_w);
        set_live(&mut client, live);
        set_floating(&mut client, restore);
        client
    }

    fn set_live(client: &mut WMClient, rect: Rect) {
        client.geometry.x = rect.x;
        client.geometry.y = rect.y;
        client.geometry.w = rect.w;
        client.geometry.h = rect.h;
    }

    fn set_floating(client: &mut WMClient, rect: Rect) {
        client.geometry.floating_x = rect.x;
        client.geometry.floating_y = rect.y;
        client.geometry.floating_w = rect.w;
        client.geometry.floating_h = rect.h;
    }

    fn live_of(client: &WMClient) -> Rect {
        Rect::new(
            client.geometry.x,
            client.geometry.y,
            client.geometry.w,
            client.geometry.h,
        )
    }

    fn floating_of(client: &WMClient) -> Rect {
        Rect::new(
            client.geometry.floating_x,
            client.geometry.floating_y,
            client.geometry.floating_w,
            client.geometry.floating_h,
        )
    }

    fn inside(outer: Rect, inner: Rect) -> bool {
        inner.x >= outer.x
            && inner.y >= outer.y
            && inner.x + inner.w <= outer.x + outer.w
            && inner.y + inner.h <= outer.y + outer.h
    }

    #[test]
    fn maximized_floating_migration_refits_the_target_work_area_and_translates_the_restore_rect() {
        let restore = Rect::new(200, 132, 600, 400);
        let mut client = maximized_floating_client(restore, 2);

        let outcome = migrate_visible_geometry(
            &mut client,
            Some(MAXIMIZE_SOURCE_WORK),
            MAXIMIZE_TARGET_MONITOR,
            MAXIMIZE_TARGET_WORK,
        );

        assert_eq!(outcome, VisibleMigration::Live);
        let translated = Rect::new(2120, 130, 600, 400);
        assert_eq!(
            translated,
            translate_and_clamp_restore_rect(
                restore,
                Some(MAXIMIZE_SOURCE_WORK),
                MAXIMIZE_TARGET_WORK,
                2
            )
        );
        assert_eq!(
            live_of(&client),
            maximize_target(translated, MAXIMIZE_TARGET_WORK, MaximizeAxes::BOTH, 2)
        );
        assert_eq!(live_of(&client), Rect::new(1920, 30, 1276, 686));
        assert_eq!(client.geometry.maximize_restore_rect, Some(translated));
        assert_eq!(floating_of(&client), translated);
    }

    #[test]
    fn promoted_maximized_migration_keeps_the_independent_floating_slot() {
        let restore = Rect::new(200, 132, 600, 400);
        let pre_promotion = Rect::new(300, 232, 500, 300);
        let mut client = maximized_floating_client(restore, 2);
        client.state.maximize_restore_tiled = true;
        set_floating(&mut client, pre_promotion);

        let outcome = migrate_visible_geometry(
            &mut client,
            Some(MAXIMIZE_SOURCE_WORK),
            MAXIMIZE_TARGET_MONITOR,
            MAXIMIZE_TARGET_WORK,
        );

        assert_eq!(outcome, VisibleMigration::Live);
        assert_eq!(
            floating_of(&client),
            translate_and_clamp_restore_rect(
                pre_promotion,
                Some(MAXIMIZE_SOURCE_WORK),
                MAXIMIZE_TARGET_WORK,
                2
            ),
            "the pre-promotion floating rect follows on its own"
        );
        assert_eq!(floating_of(&client), Rect::new(2220, 230, 500, 300));
        let translated = Rect::new(2120, 130, 600, 400);
        assert_eq!(client.geometry.maximize_restore_rect, Some(translated));
        assert_eq!(
            live_of(&client),
            maximize_target(translated, MAXIMIZE_TARGET_WORK, MaximizeAxes::BOTH, 2)
        );
    }

    #[test]
    fn same_output_resize_translates_the_restore_rect_despite_the_early_return() {
        let restore = Rect::new(200, 132, 600, 400);
        let mut client = maximized_floating_client(restore, 2);
        let live = live_of(&client);
        let grown_monitor = Rect::new(0, 0, 2560, 1440);
        let grown_work = Rect::new(0, 40, 2560, 1400);

        let outcome = migrate_visible_geometry(
            &mut client,
            Some(MAXIMIZE_SOURCE_WORK),
            grown_monitor,
            grown_work,
        );

        assert_eq!(
            outcome,
            VisibleMigration::Unchanged,
            "the window is still on its output; the next arrange refits it"
        );
        assert_eq!(live_of(&client), live);
        assert_eq!(
            client.geometry.maximize_restore_rect,
            Some(Rect::new(200, 140, 600, 400)),
            "the restore rect is rebased onto the new work area anyway"
        );
    }

    #[test]
    fn hidden_maximized_migration_restages_the_target_and_keeps_the_restore() {
        let restore = Rect::new(200, 132, 600, 400);
        let mut client = maximized_floating_client(restore, 2);
        let visible = live_of(&client);
        client.state.is_hidden = true;
        client.geometry.hidden_restore_rect = Some(visible);
        client.geometry.hidden_x = Some(-5000);
        client.geometry.x = -5000;

        assert!(migrate_hidden_restore_geometry(
            &mut client,
            Some(MAXIMIZE_SOURCE_WORK),
            MAXIMIZE_TARGET_MONITOR,
            MAXIMIZE_TARGET_WORK,
            0,
        ));

        let translated = Rect::new(2120, 130, 600, 400);
        assert_eq!(
            client.geometry.hidden_restore_rect,
            Some(maximize_target(
                translated,
                MAXIMIZE_TARGET_WORK,
                MaximizeAxes::BOTH,
                2
            )),
            "it comes back filling the target work area"
        );
        assert_eq!(floating_of(&client), translated);
        assert_eq!(client.geometry.maximize_restore_rect, Some(translated));
        assert_eq!(client.geometry.hidden_x, Some(client.geometry.x));
        assert!(client.geometry.x.saturating_add(client.total_width()) <= 0);
    }

    /// A floating window shrunk into PiP in the source output's corner,
    /// returning to `pre_pip` (its floating slot) when it leaves PiP.
    fn pip_client(pre_pip: Rect) -> WMClient {
        let mut client = WMClient::new(WindowId::from_raw(0x3a1));
        client.state.is_floating = true;
        client.state.is_pip = true;
        client.state.old_state = true;
        client.geometry.border_w = 0;
        set_live(&mut client, Rect::new(1430, 800, 480, 270));
        set_floating(&mut client, pre_pip);
        client
    }

    /// Regression: the floating/PiP arm copied the translated PiP corner
    /// rect into `floating_*`, so leaving PiP after a monitor move brought
    /// the window back at PiP size.
    #[test]
    fn pip_migration_carries_the_pre_pip_return_slot_not_the_corner_rect() {
        let pre_pip = Rect::new(100, 132, 800, 600);
        let mut client = pip_client(pre_pip);
        let pip = live_of(&client);

        let outcome = migrate_visible_geometry(
            &mut client,
            Some(MAXIMIZE_SOURCE_WORK),
            MAXIMIZE_TARGET_MONITOR,
            MAXIMIZE_TARGET_WORK,
        );

        assert_eq!(outcome, VisibleMigration::Live);
        assert_eq!(
            live_of(&client),
            translate_and_clamp_restore_rect(
                pip,
                Some(MAXIMIZE_SOURCE_WORK),
                MAXIMIZE_TARGET_WORK,
                0
            ),
            "the PiP window itself moves"
        );
        let slot = floating_of(&client);
        assert_eq!(
            slot,
            translate_and_clamp_restore_rect(
                pre_pip,
                Some(MAXIMIZE_SOURCE_WORK),
                MAXIMIZE_TARGET_WORK,
                0
            ),
            "the return slot follows the window at its pre-PiP size"
        );
        assert_eq!((slot.w, slot.h), (800, 600));
        assert!(inside(MAXIMIZE_TARGET_WORK, slot), "{slot:?}");
    }

    /// The minimized twin of the test above: the hidden path copied the
    /// staged PiP rect into the return slot.
    #[test]
    fn hidden_pip_migration_carries_the_pre_pip_return_slot_not_the_corner_rect() {
        let pre_pip = Rect::new(100, 132, 800, 600);
        let mut client = pip_client(pre_pip);
        let pip = live_of(&client);
        client.state.is_hidden = true;
        client.geometry.hidden_restore_rect = Some(pip);
        client.geometry.hidden_x = Some(-5000);
        client.geometry.x = -5000;

        assert!(migrate_hidden_restore_geometry(
            &mut client,
            Some(MAXIMIZE_SOURCE_WORK),
            MAXIMIZE_TARGET_MONITOR,
            MAXIMIZE_TARGET_WORK,
            0,
        ));

        assert_eq!(
            client.geometry.hidden_restore_rect,
            Some(translate_and_clamp_restore_rect(
                pip,
                Some(MAXIMIZE_SOURCE_WORK),
                MAXIMIZE_TARGET_WORK,
                0
            )),
            "it comes back as PiP on the target"
        );
        let slot = floating_of(&client);
        assert_eq!(
            slot,
            translate_and_clamp_restore_rect(
                pre_pip,
                Some(MAXIMIZE_SOURCE_WORK),
                MAXIMIZE_TARGET_WORK,
                0
            )
        );
        assert_eq!((slot.w, slot.h), (800, 600));
        assert!(inside(MAXIMIZE_TARGET_WORK, slot), "{slot:?}");
    }

    #[test]
    fn fullscreen_migration_also_translates_the_maximize_restore_rect() {
        let restore = Rect::new(200, 132, 600, 400);
        let mut client = maximized_floating_client(restore, 2);
        let maximized = live_of(&client);
        client.state.is_fullscreen = true;
        client.state.old_state = true;
        client.geometry.old_border_w = 2;
        client.geometry.border_w = 0;
        client.geometry.old_x = maximized.x;
        client.geometry.old_y = maximized.y;
        client.geometry.old_w = maximized.w;
        client.geometry.old_h = maximized.h;
        set_live(&mut client, Rect::new(0, 0, 1920, 1080));

        let outcome = migrate_visible_geometry(
            &mut client,
            Some(MAXIMIZE_SOURCE_WORK),
            MAXIMIZE_TARGET_MONITOR,
            MAXIMIZE_TARGET_WORK,
        );

        assert_eq!(outcome, VisibleMigration::Live);
        assert_eq!(live_of(&client), MAXIMIZE_TARGET_MONITOR);
        let old = Rect::new(
            client.geometry.old_x,
            client.geometry.old_y,
            client.geometry.old_w,
            client.geometry.old_h,
        );
        assert!(inside(MAXIMIZE_TARGET_WORK, old), "{old:?}");
        let moved_restore = client.geometry.maximize_restore_rect.unwrap();
        assert!(
            inside(MAXIMIZE_TARGET_WORK, moved_restore),
            "{moved_restore:?}"
        );
        assert_eq!(client.state.maximized_axes(), MaximizeAxes::BOTH);
    }

    // Display changes on a whole JWM, driven through the handlers the
    // backends' output events reach.

    fn jwm_on(backend: &mut DisplaySpyBackend) -> Jwm {
        Jwm::new_with_runtime_backend(backend, "test").expect("a spy backend builds a JWM")
    }

    /// A shown floating window on `monitor` at `rect`.
    fn floating_client_on(jwm: &mut Jwm, raw: u64, monitor: MonitorKey, rect: Rect) -> ClientKey {
        let mut client = WMClient::new(WindowId::from_raw(raw));
        client.mon = Some(monitor);
        client.state.tags = jwm.state.monitors[monitor].get_active_tags();
        client.state.is_floating = true;
        client.geometry.border_w = 0;
        set_live(&mut client, rect);
        set_floating(&mut client, rect);
        let key = jwm.insert_client(client);
        jwm.attach_to_monitor(key, monitor);
        key
    }

    /// A shown fullscreen window on `monitor`, filling `output` and returning
    /// to `restore` when it leaves fullscreen.
    fn fullscreen_client_on(
        jwm: &mut Jwm,
        raw: u64,
        monitor: MonitorKey,
        output: Rect,
        restore: Rect,
    ) -> ClientKey {
        let key = floating_client_on(jwm, raw, monitor, output);
        let client = &mut jwm.state.clients[key];
        client.state.is_fullscreen = true;
        client.state.old_state = true;
        set_floating(client, restore);
        client.geometry.old_x = restore.x;
        client.geometry.old_y = restore.y;
        client.geometry.old_w = restore.w;
        client.geometry.old_h = restore.h;
        key
    }

    fn work_of(jwm: &Jwm, monitor: MonitorKey) -> Rect {
        jwm.monitor_migration_areas(monitor)
            .expect("a live monitor has migration areas")
            .1
    }

    #[test]
    fn a_fullscreen_window_refits_a_resized_output_its_centre_is_still_on() {
        let old_output = Rect::new(0, 0, 1920, 1080);
        let new_output = Rect::new(0, 0, 2560, 1440);
        let mut client = WMClient::new(WindowId::from_raw(0x3b0));
        client.state.is_fullscreen = true;
        client.state.is_floating = true;
        set_live(&mut client, old_output);

        let outcome =
            migrate_visible_geometry(&mut client, Some(old_output), new_output, new_output);

        assert_eq!(outcome, VisibleMigration::Live);
        assert_eq!(live_of(&client), new_output);

        // One that already fills the target has nothing to carry.
        assert_eq!(
            migrate_visible_geometry(&mut client, Some(new_output), new_output, new_output),
            VisibleMigration::Unchanged
        );
    }

    #[test]
    fn output_change_refits_a_fullscreen_window_to_the_new_mode() {
        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
        let mut jwm = jwm_on(&mut backend);
        let monitor = jwm.state.monitor_order[0];
        let restore = Rect::new(300, 200, 800, 500);
        let player = fullscreen_client_on(
            &mut jwm,
            0x3b1,
            monitor,
            Rect::new(0, 0, 1920, 1080),
            restore,
        );

        jwm.handle_output_changed(&mut backend, output(1, 0, 0, 2560, 1440))
            .expect("the output changes");

        let client = &jwm.state.clients[player];
        assert_eq!(live_of(client), Rect::new(0, 0, 2560, 1440));
        assert_eq!(
            Rect::new(
                client.geometry.old_x,
                client.geometry.old_y,
                client.geometry.old_w,
                client.geometry.old_h,
            ),
            restore,
            "leaving fullscreen still returns to the same place"
        );
    }

    #[test]
    fn a_replacement_output_takes_in_orphans_the_last_one_left_on_screen() {
        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 2560, 1440)]);
        let mut jwm = jwm_on(&mut backend);
        let external = jwm.state.monitor_order[0];
        let floating =
            floating_client_on(&mut jwm, 0x3c0, external, Rect::new(1800, 1000, 600, 300));
        let player = fullscreen_client_on(
            &mut jwm,
            0x3c1,
            external,
            Rect::new(0, 0, 2560, 1440),
            Rect::new(400, 300, 900, 600),
        );

        jwm.handle_output_removed(&mut backend, OutputId(1))
            .expect("the external output goes away");
        assert_eq!(jwm.state.clients[floating].mon, None);
        assert_eq!(jwm.state.clients[player].mon, None);

        backend.output_ops.outputs = vec![output(2, 0, 0, 1920, 1080)];
        jwm.handle_output_added(&mut backend, output(2, 0, 0, 1920, 1080))
            .expect("the panel comes up");

        let panel = jwm.state.monitor_order[0];
        let work = work_of(&jwm, panel);
        let floating = &jwm.state.clients[floating];
        assert_eq!(floating.mon, Some(panel));
        assert!(inside(work, live_of(floating)), "{:?}", live_of(floating));
        assert!(
            inside(work, floating_of(floating)),
            "{:?}",
            floating_of(floating)
        );
        let player = &jwm.state.clients[player];
        assert_eq!(player.mon, Some(panel));
        assert_eq!(live_of(player), Rect::new(0, 0, 1920, 1080));
    }

    #[test]
    fn an_x11_layout_change_carries_shown_floating_windows_with_their_monitor() {
        let mut backend = DisplaySpyBackend::new(vec![
            output(1, 0, 0, 1920, 1080),
            output(2, 1920, 0, 1920, 1080),
        ]);
        let mut jwm = jwm_on(&mut backend);
        let right = jwm.state.monitor_order[1];
        let window = floating_client_on(&mut jwm, 0x3d0, right, Rect::new(2000, 100, 800, 600));
        let old_work = work_of(&jwm, right);

        // The left output grows, pushing the right one along.
        backend.output_ops.outputs =
            vec![output(1, 0, 0, 2560, 1440), output(2, 2560, 0, 1920, 1080)];
        assert!(jwm.updategeom(&mut backend));

        let new_work = work_of(&jwm, right);
        let expected = Rect::new(
            new_work.x + (2000 - old_work.x),
            new_work.y + (100 - old_work.y),
            800,
            600,
        );
        assert!(expected.x >= 2560, "{expected:?} is on the right output");
        let client = &jwm.state.clients[window];
        assert_eq!(client.mon, Some(right));
        assert_eq!(live_of(client), expected);
        assert_eq!(floating_of(client), expected);
    }

    #[test]
    fn x11_display_changes_keep_output_ids_on_the_monitors_that_cover_them() {
        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
        let mut jwm = jwm_on(&mut backend);
        let left = jwm.state.monitor_order[0];

        // Hotplug: the monitor RandR's refresh creates answers the pointer.
        backend.output_ops.outputs =
            vec![output(1, 0, 0, 1920, 1080), output(2, 1920, 0, 1920, 1080)];
        assert!(jwm.updategeom(&mut backend));
        let right = jwm.state.monitor_order[1];
        assert_eq!(jwm.state.output_map.get(right), Some(&OutputId(2)));
        assert_eq!(jwm.recttomon(&mut backend, 2500, 500), Some(right));

        // The outputs trade places: each id follows its rectangle, not the
        // monitor it used to be.
        backend.output_ops.outputs =
            vec![output(2, 0, 0, 1920, 1080), output(1, 1920, 0, 1920, 1080)];
        jwm.updategeom(&mut backend);
        assert_eq!(jwm.state.output_map.get(left), Some(&OutputId(2)));
        assert_eq!(jwm.state.output_map.get(right), Some(&OutputId(1)));
        assert_eq!(jwm.recttomon(&mut backend, 2500, 500), Some(right));

        // Unplugging one leaves the survivor mapped to the output that is
        // still there, not to the one that went away.
        backend.output_ops.outputs = vec![output(1, 1920, 0, 1920, 1080)];
        assert!(jwm.updategeom(&mut backend));
        assert_eq!(jwm.state.monitor_order, vec![left]);
        assert_eq!(jwm.state.output_map.get(left), Some(&OutputId(1)));
        assert_eq!(jwm.state.output_map.len(), 1);
    }

    #[test]
    fn output_ids_are_planned_by_rectangle_and_claimed_once() {
        let mut state = WMState::new();
        let first = insert_monitor(&mut state, OutputId(1), 1);
        let second = insert_monitor(&mut state, OutputId(2), 1);
        let third = insert_monitor(&mut state, OutputId(9), 1);
        let left = Rect::new(0, 0, 1920, 1080);
        let right = Rect::new(1920, 0, 1920, 1080);

        // Clones share a rectangle: each monitor keeps the id it had.
        assert_eq!(
            plan_output_map(
                &[
                    (first, left, Some(OutputId(2))),
                    (second, left, Some(OutputId(1)))
                ],
                &[(OutputId(1), left), (OutputId(2), left)],
            ),
            vec![(first, OutputId(2)), (second, OutputId(1))]
        );

        // A new monitor takes the output with its rectangle; a stale id
        // gives way to the output at its position; one monitor more than
        // there are outputs gets nothing.
        assert_eq!(
            plan_output_map(
                &[
                    (first, left, Some(OutputId(7))),
                    (second, right, None),
                    (third, Rect::new(0, 0, 1, 1), Some(OutputId(9))),
                ],
                &[
                    (OutputId(3), Rect::new(0, 0, 1280, 720)),
                    (OutputId(4), right)
                ],
            ),
            vec![(first, OutputId(3)), (second, OutputId(4))]
        );
    }

    /// Three side-by-side 1920x1080 outputs with ids 1..=3, numbered 0..=2,
    /// the selection on the first.
    fn jwm_on_three_outputs(backend: &mut DisplaySpyBackend) -> Jwm {
        backend.output_ops.outputs = vec![
            output(1, 0, 0, 1920, 1080),
            output(2, 1920, 0, 1920, 1080),
            output(3, 3840, 0, 1920, 1080),
        ];
        let jwm = jwm_on(backend);
        assert_eq!(jwm.state.monitor_order.len(), 3);
        jwm
    }

    #[test]
    fn unplugging_the_selected_output_never_selects_a_shaded_monitor() {
        let mut backend = DisplaySpyBackend::new(Vec::new());
        let mut jwm = jwm_on_three_outputs(&mut backend);
        let shaded = jwm.state.monitor_order[1];
        let clear = jwm.state.monitor_order[2];
        jwm.lock_monitor(&mut backend, &WMArgEnum::Int(1))
            .expect("monitor 1 locks");
        let behind_shade =
            floating_client_on(&mut jwm, 0x3e0, shaded, Rect::new(2100, 100, 600, 400));
        let on_show = floating_client_on(&mut jwm, 0x3e1, clear, Rect::new(4000, 100, 600, 400));

        backend.output_ops.outputs.remove(0);
        jwm.handle_output_removed(&mut backend, OutputId(1))
            .expect("the selected output goes away");

        assert!(jwm.monitor_is_locked(1), "the shaded output is still there");
        assert_eq!(jwm.state.sel_mon, Some(clear));
        let focused = jwm.get_selected_client_key();
        assert_ne!(focused, Some(behind_shade), "no focus behind the shade");
        assert_eq!(focused, Some(on_show));
    }

    #[test]
    fn unplugging_the_last_unlocked_output_lifts_the_lock() {
        let mut backend = DisplaySpyBackend::new(vec![
            output(1, 0, 0, 1920, 1080),
            output(2, 1920, 0, 1920, 1080),
        ]);
        let mut jwm = jwm_on(&mut backend);
        let shaded = jwm.state.monitor_order[1];
        jwm.lock_monitor(&mut backend, &WMArgEnum::Int(1))
            .expect("monitor 1 locks");

        backend.output_ops.outputs.remove(0);
        jwm.handle_output_removed(&mut backend, OutputId(1))
            .expect("the unlocked output goes away");

        assert!(!jwm.monitor_is_locked(1));
        assert_eq!(jwm.state.sel_mon, Some(shaded));
        assert_eq!(backend.shade_pushes.last(), Some(&Vec::new()));
    }

    #[test]
    fn a_shade_never_outlives_the_output_it_was_cut_for() {
        let mut backend = DisplaySpyBackend::new(Vec::new());
        let mut jwm = jwm_on_three_outputs(&mut backend);
        jwm.lock_monitor(&mut backend, &WMArgEnum::Int(1))
            .expect("monitor 1 locks");

        // A mode change grows the locked output past its shade.
        backend.output_ops.outputs[1] = output(2, 1920, 0, 2560, 1440);
        jwm.handle_output_changed(&mut backend, output(2, 1920, 0, 2560, 1440))
            .expect("the output changes");
        assert!(!jwm.monitor_is_locked(1));
        assert_eq!(backend.shade_pushes.last(), Some(&Vec::new()));

        // A locked output unplugged: the next one plugged in reuses its
        // number, and must not come up behind the old shade.
        jwm.lock_monitor(&mut backend, &WMArgEnum::Int(1))
            .expect("monitor 1 locks again");
        backend.output_ops.outputs.remove(1);
        jwm.handle_output_removed(&mut backend, OutputId(2))
            .expect("the locked output goes away");
        assert!(!jwm.monitor_is_locked(1));

        let replacement = output(4, 1920, 0, 1920, 1080);
        backend.output_ops.outputs.push(replacement.clone());
        jwm.handle_output_added(&mut backend, replacement)
            .expect("another output comes up");
        let added = *jwm.state.monitor_order.last().expect("the new monitor");
        assert_eq!(jwm.state.monitors[added].num, 1);
        assert!(!jwm.monitor_key_is_locked(added));
        assert_eq!(backend.shade_pushes.last(), Some(&Vec::new()));
    }

    /// Regression: `s_w`/`s_h` are refreshed only at startup and by an X11
    /// root ConfigureNotify. A single-output Wayland mode change arrives as
    /// `OutputChanged` then `ScreenLayoutChanged`, and the layout refresh
    /// used to reset the monitor to the stale startup size, undoing the new
    /// mode (and the fullscreen refit that went with it).
    #[test]
    fn a_single_output_layout_refresh_keeps_the_new_mode() {
        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
        let mut jwm = jwm_on(&mut backend);
        let monitor = jwm.state.monitor_order[0];
        let player = fullscreen_client_on(
            &mut jwm,
            0x3f0,
            monitor,
            Rect::new(0, 0, 1920, 1080),
            Rect::new(300, 200, 800, 500),
        );
        assert_eq!((jwm.s_w, jwm.s_h), (1920, 1080));

        backend.output_ops.outputs = vec![output(1, 0, 0, 2560, 1440)];
        jwm.handle_output_changed(&mut backend, output(1, 0, 0, 2560, 1440))
            .expect("the output changes");
        jwm.updategeom(&mut backend);

        assert_eq!(
            monitor_rect(&jwm.state.monitors[monitor]),
            Rect::new(0, 0, 2560, 1440)
        );
        assert_eq!(
            live_of(&jwm.state.clients[player]),
            Rect::new(0, 0, 2560, 1440)
        );

        // The single output's own origin, too, not the screen's.
        backend.output_ops.outputs = vec![output(1, 1920, 0, 1280, 1024)];
        assert!(jwm.updategeom(&mut backend));
        assert_eq!(
            monitor_rect(&jwm.state.monitors[monitor]),
            Rect::new(1920, 0, 1280, 1024)
        );

        // With nothing enumerated, the screen size is all there is.
        backend.output_ops.outputs.clear();
        assert!(jwm.updategeom(&mut backend));
        assert_eq!(
            monitor_rect(&jwm.state.monitors[monitor]),
            Rect::new(0, 0, jwm.s_w, jwm.s_h)
        );
    }

    #[test]
    fn monitor_rects_follow_output_ids_and_fall_back_to_position() {
        let a = Rect::new(0, 0, 1920, 1080);
        let b = Rect::new(1920, 0, 2560, 1440);
        let c = Rect::new(4480, 0, 1280, 1024);
        let outputs = [(OutputId(1), a), (OutputId(2), b), (OutputId(3), c)];

        // Nothing moved: the positional hand-out.
        assert_eq!(
            plan_monitor_rects_by_output(
                &[Some(OutputId(1)), Some(OutputId(2)), Some(OutputId(3))],
                &outputs
            ),
            vec![a, b, c]
        );
        // A non-tail re-plug left the monitors in another order than the
        // outputs: each keeps its own instead of trading geometry.
        assert_eq!(
            plan_monitor_rects_by_output(
                &[Some(OutputId(1)), Some(OutputId(3)), Some(OutputId(2))],
                &outputs
            ),
            vec![a, c, b]
        );
        // A monitor without an id (or with a vanished one) and a monitor
        // still to be created take what is left, in enumeration order.
        assert_eq!(
            plan_monitor_rects_by_output(&[Some(OutputId(9)), Some(OutputId(3))], &outputs),
            vec![a, c, b]
        );
        assert_eq!(
            plan_monitor_rects_by_output(&[None, Some(OutputId(2))], &outputs),
            vec![a, b, c]
        );
        // A monitor past the output count is removed from the tail; it does
        // not claim its output away from a surviving position.
        assert_eq!(
            plan_monitor_rects_by_output(
                &[Some(OutputId(7)), Some(OutputId(1))],
                &[(OutputId(2), b)]
            ),
            vec![b]
        );
        // Two monitors naming the same output: the first keeps it.
        assert_eq!(
            plan_monitor_rects_by_output(
                &[Some(OutputId(2)), Some(OutputId(2))],
                &[(OutputId(1), a), (OutputId(2), b)]
            ),
            vec![b, a]
        );
    }

    #[test]
    fn windows_of_an_unplugged_output_skip_a_shaded_survivor() {
        let mut backend = DisplaySpyBackend::new(Vec::new());
        let mut jwm = jwm_on_three_outputs(&mut backend);
        let unplugged = jwm.state.monitor_order[0];
        let shaded = jwm.state.monitor_order[1];
        let clear = jwm.state.monitor_order[2];
        jwm.lock_monitor(&mut backend, &WMArgEnum::Int(1))
            .expect("monitor 1 locks");
        let orphan = floating_client_on(&mut jwm, 0x3f1, unplugged, Rect::new(100, 100, 600, 400));

        backend.output_ops.outputs.remove(0);
        jwm.handle_output_removed(&mut backend, OutputId(1))
            .expect("the first output goes away");

        assert!(jwm.monitor_key_is_locked(shaded));
        assert_eq!(jwm.state.clients[orphan].mon, Some(clear));
        assert!(!jwm.state.monitor_clients[shaded].contains(&orphan));
        assert!(jwm.state.monitor_clients[clear].contains(&orphan));
    }
}
