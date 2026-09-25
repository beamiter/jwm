// src/core/maximize.rs

//! Platform-neutral maximize decisions.
//!
//! `Jwm::set_client_maximized` owns the transaction (publish, commit,
//! geometry, rollback); everything it decides first lives here as pure
//! functions so the resolution, admission and geometry rules can be pinned by
//! unit tests without a backend.
//!
//! Rectangles follow the client geometry convention: outer origin, content
//! (inner) size, border drawn outside the content.

use crate::backend::api::{MaximizeAxes, NetWmAction};
use crate::backend::common_define::ConfigWindowBits;
use crate::core::types::Rect;

/// Who asked for a maximize change; decides whether a layout-managed window may leave the layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaximizeOrigin {
    /// EWMH ClientMessage, xdg-shell, XWayland, wlr-foreign-toplevel.
    Client,
    /// JWM command/keybinding/IPC, snap_window, top-edge drop zone, session restore.
    User,
    /// Manage-time adoption of pre-set protocol state.
    Adopt,
}

/// Outcome of [`admit_maximize`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaximizeAdmission {
    /// Change maximize state (client rests floating, or only loses axes).
    Apply,
    /// Pull a layout-managed client out of the layout (records `maximize_restore_tiled`).
    Promote,
    /// Refuse: republish current state and reply with the authoritative geometry.
    Reject,
}

/// Client and monitor facts that admission depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MaximizeFacts {
    /// `old_state` while fullscreen or PiP, otherwise `is_floating`.
    pub resting_floating: bool,
    /// The client's monitor currently shows `LayoutEnum::FLOAT` (`monitor.lt.is_float()`).
    pub float_layout: bool,
    /// `is_fullscreen || is_pip`.
    pub suspended: bool,
    pub is_fixed: bool,
    pub is_dock: bool,
    /// `Jwm::maximize_work_area(client.mon)` is `Some`.
    pub has_work_area: bool,
}

/// The rectangle a client occupies once transient modes end; maximize reads/writes only this slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestingSlot {
    /// Live `x/y/w/h`.
    Live,
    /// `hidden_restore_rect` (minimized, or parked on an invisible tag).
    HiddenRestore,
    /// `old_*` (fullscreen return rectangle; border = `old_border_w`).
    FullscreenReturn,
    /// `floating_*` (PiP return rectangle).
    PipReturn,
}

/// Everything [`plan_maximize`] needs, read from one client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaximizeInput {
    pub current: MaximizeAxes,
    /// `ClientGeometry::maximize_restore_rect`.
    pub restore: Option<Rect>,
    /// Restore rectangle to use when entering from `NONE` (adoption: V1 floating_rect).
    pub restore_hint: Option<Rect>,
    /// Content rectangle of the resting slot.
    pub resting: Rect,
    /// `Jwm::maximize_work_area(mon)`, or `resting` when the client has no monitor.
    pub area: Rect,
    /// `old_border_w` for `FullscreenReturn`, otherwise `border_w`.
    pub border_w: i32,
}

/// The state and resting geometry one maximize transition commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaximizePlan {
    pub axes: MaximizeAxes,
    /// New `maximize_restore_rect` (`Some` iff `axes.any()`).
    pub restore_rect: Option<Rect>,
    /// New content rectangle of the resting slot (before size hints).
    pub resting_rect: Rect,
}

/// Maximize state captured when a pointer drag is armed; reinstated on cancel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MaximizeSnapshot {
    pub axes: MaximizeAxes,
    pub restore_rect: Option<Rect>,
    pub restore_tiled: bool,
    /// `floating_*` when the drag was armed. A promoted client
    /// (`restore_tiled`) keeps its independent pre-promotion floating rect
    /// there, which the drag overwrites, so the cancel needs this copy; a
    /// plain maximized client's floating slot mirrors `restore_rect`.
    pub floating_rect: Rect,
}

/// named == NONE -> current. Add -> current.union(named). Remove -> current.without(named).
/// Toggle with named.both() -> NONE if current.both() else BOTH. Toggle naming one axis -> flip that axis.
pub fn requested_axes(
    current: MaximizeAxes,
    action: NetWmAction,
    named: MaximizeAxes,
) -> MaximizeAxes {
    if !named.any() {
        return current;
    }
    match action {
        NetWmAction::Add => current.union(named),
        NetWmAction::Remove => current.without(named),
        // A paired toggle is what pagers and the maximize button mean: fill
        // the area unless the window already fills it. Flipping each axis
        // independently would turn a half-maximized window into the other
        // half instead.
        NetWmAction::Toggle if named.both() => {
            if current.both() {
                MaximizeAxes::NONE
            } else {
                MaximizeAxes::BOTH
            }
        }
        NetWmAction::Toggle => {
            MaximizeAxes::new(current.vert != named.vert, current.horz != named.horz)
        }
    }
}

/// In order: !next.gains_over(current) -> Apply; !has_work_area || is_dock || is_fixed -> Reject;
/// resting_floating -> Apply; suspended -> Reject; float_layout || origin == User -> Promote; else Reject.
pub fn admit_maximize(
    current: MaximizeAxes,
    next: MaximizeAxes,
    facts: MaximizeFacts,
    origin: MaximizeOrigin,
) -> MaximizeAdmission {
    // Giving up axes is always allowed: it can only return geometry to the
    // client, and refusing it would leave a stale published state behind.
    if !next.gains_over(current) {
        return MaximizeAdmission::Apply;
    }
    if !facts.has_work_area || facts.is_dock || facts.is_fixed {
        return MaximizeAdmission::Reject;
    }
    if facts.resting_floating {
        return MaximizeAdmission::Apply;
    }
    // A tiled client under fullscreen/PiP has no floating slot to maximize
    // into, and promoting it underneath the transient mode would silently
    // change where it returns to.
    if facts.suspended {
        return MaximizeAdmission::Reject;
    }
    // Clients, taskbars and adoption must not un-tile a layout-managed
    // window in a tiling layout; only the user may, and the FLOAT layout has
    // no tiles to protect.
    if facts.float_layout || origin == MaximizeOrigin::User {
        MaximizeAdmission::Promote
    } else {
        MaximizeAdmission::Reject
    }
}

/// FullscreenReturn if is_fullscreen; else PipReturn if is_pip; else HiddenRestore if parked; else Live.
pub fn resting_slot(is_fullscreen: bool, is_pip: bool, parked: bool) -> RestingSlot {
    if is_fullscreen {
        RestingSlot::FullscreenReturn
    } else if is_pip {
        RestingSlot::PipReturn
    } else if parked {
        RestingSlot::HiddenRestore
    } else {
        RestingSlot::Live
    }
}

/// Content size that leaves room for a border on both sides of `extent`.
fn inner_extent(extent: i32, border_w: i32) -> i32 {
    extent.saturating_sub(border_w.saturating_mul(2)).max(1)
}

/// Content rect (border outside, monocle convention, no gaps): horz -> x = area.x,
/// w = max(area.w - 2*bw, 1); vert -> y = area.y, h = max(area.h - 2*bw, 1);
/// other components from `restore`; NONE -> `restore`.
pub fn maximize_target(restore: Rect, area: Rect, axes: MaximizeAxes, border_w: i32) -> Rect {
    let mut rect = restore;
    if axes.horz {
        rect.x = area.x;
        rect.w = inner_extent(area.w, border_w);
    }
    if axes.vert {
        rect.y = area.y;
        rect.h = inner_extent(area.h, border_w);
    }
    rect
}

/// `restore` with its NON-maximized components taken from `live`:
/// x/w from live unless axes.horz; y/h from live unless axes.vert.
pub fn mirror_free_axes(restore: Rect, live: Rect, axes: MaximizeAxes) -> Rect {
    let mut rect = restore;
    if !axes.horz {
        rect.x = live.x;
        rect.w = live.w;
    }
    if !axes.vert {
        rect.y = live.y;
        rect.h = live.h;
    }
    rect
}

/// True when `outer` covers at least 90% of `available`.
fn covers_nine_tenths(outer: i32, available: i32) -> bool {
    i64::from(outer) * 10 >= i64::from(available) * 9
}

/// Two thirds of `extent`, computed without overflow.
fn two_thirds(extent: i32) -> i32 {
    // |extent| * 2 / 3 always fits back into i32.
    i32::try_from(i64::from(extent) * 2 / 3).unwrap_or(extent)
}

/// `resting` unless (resting.w + 2*bw) * 10 >= area.w * 9 && (resting.h + 2*bw) * 10 >= area.h * 9;
/// then the centered 2/3 rect: w = max(area.w * 2 / 3 - 2*bw, 1), h = max(area.h * 2 / 3 - 2*bw, 1),
/// x = area.x + (area.w - (w + 2*bw)) / 2, y = area.y + (area.h - (h + 2*bw)) / 2.
pub fn initial_restore_rect(resting: Rect, area: Rect, border_w: i32) -> Rect {
    let border = border_w.saturating_mul(2);
    let near_full = covers_nine_tenths(resting.w.saturating_add(border), area.w)
        && covers_nine_tenths(resting.h.saturating_add(border), area.h);
    if !near_full {
        return resting;
    }
    // A window that already fills the area would "restore" to the same
    // rectangle, making unmaximize look like a no-op. Give it a visible,
    // centered slot instead.
    let w = two_thirds(area.w).saturating_sub(border).max(1);
    let h = two_thirds(area.h).saturating_sub(border).max(1);
    Rect::new(
        area.x
            .saturating_add(area.w.saturating_sub(w.saturating_add(border)) / 2),
        area.y
            .saturating_add(area.h.saturating_sub(h.saturating_add(border)) / 2),
        w,
        h,
    )
}

/// next == input.current -> { axes: current, restore_rect: input.restore, resting_rect: input.resting }.
/// base = if current.any() { restore.map(|r| mirror_free_axes(r, resting, current))
///            .unwrap_or_else(|| initial_restore_rect(resting, area, bw)) }
///        else { restore_hint.unwrap_or_else(|| initial_restore_rect(resting, area, bw)) };
/// next.any() -> { axes: next, restore_rect: Some(base), resting_rect: maximize_target(base, area, next, bw) };
/// next == NONE -> { axes: NONE, restore_rect: None, resting_rect: base }.
pub fn plan_maximize(input: MaximizeInput, next: MaximizeAxes) -> MaximizePlan {
    if next == input.current {
        return MaximizePlan {
            axes: input.current,
            restore_rect: input.restore,
            resting_rect: input.resting,
        };
    }
    let fallback = || initial_restore_rect(input.resting, input.area, input.border_w);
    // While maximized, the free axes of the resting rect are the user's own
    // (a VERT window can still be moved sideways), so they win over the
    // recorded restore rect.
    let base = if input.current.any() {
        input
            .restore
            .map(|restore| mirror_free_axes(restore, input.resting, input.current))
            .unwrap_or_else(fallback)
    } else {
        input.restore_hint.unwrap_or_else(fallback)
    };
    if next.any() {
        MaximizePlan {
            axes: next,
            restore_rect: Some(base),
            resting_rect: maximize_target(base, input.area, next, input.border_w),
        }
    } else {
        MaximizePlan {
            axes: MaximizeAxes::NONE,
            restore_rect: None,
            resting_rect: base,
        }
    }
}

/// Remove X|WIDTH when axes.horz and Y|HEIGHT when axes.vert; every other bit is kept.
pub fn strip_maximized_configure_bits(mask_bits: u16, axes: MaximizeAxes) -> u16 {
    let mut owned = ConfigWindowBits::empty();
    if axes.horz {
        owned |= ConfigWindowBits::X | ConfigWindowBits::WIDTH;
    }
    if axes.vert {
        owned |= ConfigWindowBits::Y | ConfigWindowBits::HEIGHT;
    }
    // Plain bit arithmetic keeps bits `ConfigWindowBits` does not name.
    mask_bits & !owned.bits()
}

/// True when any of X|Y|WIDTH|HEIGHT is set.
pub fn has_configure_geometry_bits(mask_bits: u16) -> bool {
    let geometry = ConfigWindowBits::X
        | ConfigWindowBits::Y
        | ConfigWindowBits::WIDTH
        | ConfigWindowBits::HEIGHT;
    mask_bits & geometry.bits() != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::api::NetWmState;

    const NONE: MaximizeAxes = MaximizeAxes::NONE;
    const VERT: MaximizeAxes = MaximizeAxes::VERT;
    const HORZ: MaximizeAxes = MaximizeAxes::HORZ;
    const BOTH: MaximizeAxes = MaximizeAxes::BOTH;

    fn area() -> Rect {
        Rect::new(0, 30, 1920, 1050)
    }

    #[test]
    fn requested_axes_resolves_add_remove_and_single_axis_toggle() {
        assert_eq!(requested_axes(NONE, NetWmAction::Add, VERT), VERT);
        assert_eq!(requested_axes(VERT, NetWmAction::Add, HORZ), BOTH);
        assert_eq!(requested_axes(BOTH, NetWmAction::Remove, VERT), HORZ);
        assert_eq!(requested_axes(HORZ, NetWmAction::Toggle, VERT), BOTH);
        assert_eq!(requested_axes(BOTH, NetWmAction::Toggle, HORZ), VERT);
        for action in [NetWmAction::Add, NetWmAction::Remove, NetWmAction::Toggle] {
            for current in [NONE, VERT, HORZ, BOTH] {
                assert_eq!(
                    requested_axes(current, action, NONE),
                    current,
                    "{action:?} naming nothing must leave {current:?} alone"
                );
            }
        }
    }

    #[test]
    fn paired_toggle_maximizes_unless_already_fully_maximized() {
        for current in [NONE, VERT, HORZ] {
            assert_eq!(
                requested_axes(current, NetWmAction::Toggle, BOTH),
                BOTH,
                "from {current:?}"
            );
        }
        assert_eq!(requested_axes(BOTH, NetWmAction::Toggle, BOTH), NONE);
    }

    #[test]
    fn maximize_axes_helpers_round_trip_net_wm_state() {
        assert_eq!(
            MaximizeAxes::from_net_wm_state(NetWmState::MaximizedVert),
            Some(VERT)
        );
        assert_eq!(
            MaximizeAxes::from_net_wm_state(NetWmState::MaximizedHorz),
            Some(HORZ)
        );
        assert_eq!(MaximizeAxes::from_net_wm_state(NetWmState::Above), None);

        assert_eq!(
            NONE.with_net_wm_state(NetWmState::MaximizedVert, true),
            VERT
        );
        assert_eq!(
            HORZ.with_net_wm_state(NetWmState::MaximizedVert, true),
            BOTH
        );
        assert_eq!(
            BOTH.with_net_wm_state(NetWmState::MaximizedHorz, false),
            VERT
        );
        assert_eq!(
            VERT.with_net_wm_state(NetWmState::MaximizedHorz, false),
            VERT
        );
        for axes in [NONE, VERT, HORZ, BOTH] {
            assert_eq!(axes.with_net_wm_state(NetWmState::Above, true), axes);
            assert_eq!(axes.with_net_wm_state(NetWmState::Fullscreen, false), axes);
        }

        // (next, current, gains_over)
        let gains = [
            (NONE, NONE, false),
            (VERT, NONE, true),
            (BOTH, VERT, true),
            (VERT, BOTH, false),
            (HORZ, VERT, true),
            (BOTH, BOTH, false),
        ];
        for (next, current, expected) in gains {
            assert_eq!(
                next.gains_over(current),
                expected,
                "{next:?} over {current:?}"
            );
        }
        assert_eq!(VERT.union(HORZ), BOTH);
        assert_eq!(NONE.union(VERT), VERT);
        assert_eq!(BOTH.union(NONE), BOTH);
        assert_eq!(BOTH.without(VERT), HORZ);
        assert_eq!(BOTH.without(BOTH), NONE);
        assert_eq!(VERT.without(HORZ), VERT);
        assert!(!NONE.any() && VERT.any() && HORZ.any() && BOTH.any());
        assert!(!VERT.both() && !HORZ.both() && BOTH.both());
        assert_eq!(MaximizeAxes::default(), NONE);
        assert_eq!(MaximizeAxes::new(true, false), VERT);
    }

    #[test]
    fn admission_keeps_layout_managed_clients_tiled() {
        let origins = [
            MaximizeOrigin::Client,
            MaximizeOrigin::User,
            MaximizeOrigin::Adopt,
        ];
        let hostile = MaximizeFacts {
            is_fixed: true,
            has_work_area: false,
            ..MaximizeFacts::default()
        };
        for origin in origins {
            // Losing axes is always allowed.
            assert_eq!(
                admit_maximize(BOTH, NONE, hostile, origin),
                MaximizeAdmission::Apply
            );
            assert_eq!(
                admit_maximize(BOTH, HORZ, hostile, origin),
                MaximizeAdmission::Apply
            );
        }

        let floating = MaximizeFacts {
            resting_floating: true,
            has_work_area: true,
            ..MaximizeFacts::default()
        };
        let refusals = [
            MaximizeFacts {
                has_work_area: false,
                ..floating
            },
            MaximizeFacts {
                is_dock: true,
                ..floating
            },
            MaximizeFacts {
                is_fixed: true,
                ..floating
            },
        ];
        for facts in refusals {
            for origin in origins {
                assert_eq!(
                    admit_maximize(NONE, BOTH, facts, origin),
                    MaximizeAdmission::Reject,
                    "{facts:?} {origin:?}"
                );
            }
        }
        for origin in origins {
            assert_eq!(
                admit_maximize(NONE, BOTH, floating, origin),
                MaximizeAdmission::Apply
            );
        }

        let tiled = MaximizeFacts {
            resting_floating: false,
            has_work_area: true,
            ..MaximizeFacts::default()
        };
        let suspended_tiled = MaximizeFacts {
            suspended: true,
            float_layout: true,
            ..tiled
        };
        for origin in origins {
            assert_eq!(
                admit_maximize(NONE, BOTH, suspended_tiled, origin),
                MaximizeAdmission::Reject,
                "{origin:?}"
            );
        }
        assert_eq!(
            admit_maximize(NONE, BOTH, tiled, MaximizeOrigin::Client),
            MaximizeAdmission::Reject
        );
        assert_eq!(
            admit_maximize(NONE, BOTH, tiled, MaximizeOrigin::Adopt),
            MaximizeAdmission::Reject
        );
        assert_eq!(
            admit_maximize(NONE, BOTH, tiled, MaximizeOrigin::User),
            MaximizeAdmission::Promote
        );
        let float_layout = MaximizeFacts {
            float_layout: true,
            ..tiled
        };
        assert_eq!(
            admit_maximize(NONE, BOTH, float_layout, MaximizeOrigin::Client),
            MaximizeAdmission::Promote
        );
    }

    #[test]
    fn resting_slot_prefers_fullscreen_then_pip_then_parking() {
        assert_eq!(
            resting_slot(true, true, true),
            RestingSlot::FullscreenReturn
        );
        assert_eq!(resting_slot(false, true, true), RestingSlot::PipReturn);
        assert_eq!(resting_slot(false, false, true), RestingSlot::HiddenRestore);
        assert_eq!(resting_slot(false, false, false), RestingSlot::Live);
    }

    #[test]
    fn maximize_target_fills_named_axes_of_the_work_area_minus_borders() {
        let restore = Rect::new(100, 120, 400, 300);
        assert_eq!(
            maximize_target(restore, area(), BOTH, 2),
            Rect::new(0, 30, 1916, 1046)
        );
        assert_eq!(
            maximize_target(restore, area(), VERT, 2),
            Rect::new(100, 30, 400, 1046)
        );
        assert_eq!(
            maximize_target(restore, area(), HORZ, 2),
            Rect::new(0, 120, 1916, 300)
        );
        assert_eq!(maximize_target(restore, area(), NONE, 2), restore);
        let tiny = maximize_target(restore, Rect::new(5, 6, 3, 3), BOTH, 2);
        assert_eq!(tiny, Rect::new(5, 6, 1, 1));
    }

    #[test]
    fn mirror_free_axes_takes_only_unmaximized_components_from_live() {
        let restore = Rect::new(10, 20, 300, 200);
        let live = Rect::new(50, 30, 400, 1046);
        assert_eq!(
            mirror_free_axes(restore, live, VERT),
            Rect::new(50, 20, 400, 200)
        );
        assert_eq!(
            mirror_free_axes(restore, live, HORZ),
            Rect::new(10, 30, 300, 1046)
        );
        assert_eq!(mirror_free_axes(restore, live, BOTH), restore);
        assert_eq!(mirror_free_axes(restore, live, NONE), live);
    }

    #[test]
    fn initial_restore_rect_falls_back_only_for_near_full_size_windows() {
        assert_eq!(
            initial_restore_rect(Rect::new(0, 30, 1916, 1046), area(), 2),
            Rect::new(320, 205, 1276, 696)
        );
        let small = Rect::new(200, 150, 640, 480);
        assert_eq!(initial_restore_rect(small, area(), 2), small);
        let tall = Rect::new(40, 30, 300, 1046);
        assert_eq!(initial_restore_rect(tall, area(), 2), tall);
    }

    #[test]
    fn plan_enter_uses_hint_then_resting_and_repeat_keeps_the_first_restore() {
        let resting = Rect::new(200, 150, 640, 480);
        let hint = Rect::new(333, 222, 555, 444);
        let input = MaximizeInput {
            current: NONE,
            restore: None,
            restore_hint: Some(hint),
            resting,
            area: area(),
            border_w: 2,
        };
        let hinted = plan_maximize(input, BOTH);
        assert_eq!(hinted.axes, BOTH);
        assert_eq!(hinted.restore_rect, Some(hint));
        assert_eq!(hinted.resting_rect, maximize_target(hint, area(), BOTH, 2));

        let plain = plan_maximize(
            MaximizeInput {
                restore_hint: None,
                ..input
            },
            BOTH,
        );
        assert_eq!(plain.restore_rect, Some(resting));
        assert_eq!(
            plain.resting_rect,
            maximize_target(resting, area(), BOTH, 2)
        );

        let r0 = Rect::new(111, 99, 700, 500);
        let target = maximize_target(r0, area(), BOTH, 2);
        let repeat = plan_maximize(
            MaximizeInput {
                current: BOTH,
                restore: Some(r0),
                restore_hint: Some(hint),
                resting: target,
                area: area(),
                border_w: 2,
            },
            BOTH,
        );
        assert_eq!(
            repeat,
            MaximizePlan {
                axes: BOTH,
                restore_rect: Some(r0),
                resting_rect: target,
            }
        );
    }

    #[test]
    fn plan_partial_and_full_exit_restore_axes_from_the_slot() {
        let r0 = Rect::new(111, 99, 700, 500);
        let both = plan_maximize(
            MaximizeInput {
                current: BOTH,
                restore: Some(r0),
                restore_hint: None,
                resting: maximize_target(r0, area(), BOTH, 2),
                area: area(),
                border_w: 2,
            },
            HORZ,
        );
        assert_eq!(both.axes, HORZ);
        assert_eq!(both.restore_rect, Some(r0));
        assert_eq!(both.resting_rect, Rect::new(0, 99, 1916, 500));

        // The user moved the HORZ window vertically: its free axis (y/h)
        // comes from the resting rect, the maximized axis from R0.
        let resting = Rect::new(0, 140, 1916, 520);
        let exit = plan_maximize(
            MaximizeInput {
                current: HORZ,
                restore: Some(r0),
                restore_hint: None,
                resting,
                area: area(),
                border_w: 2,
            },
            NONE,
        );
        assert_eq!(exit.axes, NONE);
        assert_eq!(exit.restore_rect, None);
        assert_eq!(exit.resting_rect, Rect::new(111, 140, 700, 520));
    }

    #[test]
    fn plan_mirrors_free_axis_edits_on_axis_change() {
        let r0 = Rect::new(111, 99, 700, 500);
        let mut resting = maximize_target(r0, area(), VERT, 2);
        resting.x = 260;
        resting.w = 810;
        let plan = plan_maximize(
            MaximizeInput {
                current: VERT,
                restore: Some(r0),
                restore_hint: None,
                resting,
                area: area(),
                border_w: 2,
            },
            BOTH,
        );
        assert_eq!(plan.restore_rect, Some(Rect::new(260, 99, 810, 500)));
        assert_eq!(plan.resting_rect, Rect::new(0, 30, 1916, 1046));
    }

    #[test]
    fn plan_without_a_recorded_restore_falls_back_to_a_visible_rect() {
        let full = Rect::new(0, 30, 1916, 1046);
        let plan = plan_maximize(
            MaximizeInput {
                current: BOTH,
                restore: None,
                restore_hint: None,
                resting: full,
                area: area(),
                border_w: 2,
            },
            NONE,
        );
        assert_eq!(plan.resting_rect, Rect::new(320, 205, 1276, 696));
    }

    #[test]
    fn strip_maximized_configure_bits_removes_only_owned_axes() {
        let geometry = (ConfigWindowBits::X
            | ConfigWindowBits::Y
            | ConfigWindowBits::WIDTH
            | ConfigWindowBits::HEIGHT)
            .bits();
        let other = (ConfigWindowBits::BORDER_WIDTH
            | ConfigWindowBits::SIBLING
            | ConfigWindowBits::STACK_MODE)
            .bits();
        assert_eq!(
            strip_maximized_configure_bits(geometry | other, BOTH),
            other
        );
        assert_eq!(
            strip_maximized_configure_bits(geometry, VERT),
            (ConfigWindowBits::X | ConfigWindowBits::WIDTH).bits()
        );
        assert_eq!(
            strip_maximized_configure_bits(geometry, HORZ),
            (ConfigWindowBits::Y | ConfigWindowBits::HEIGHT).bits()
        );
        assert_eq!(strip_maximized_configure_bits(geometry, NONE), geometry);
        // Bits outside the named set survive untouched.
        assert_eq!(strip_maximized_configure_bits(0x8000, BOTH), 0x8000);

        assert!(has_configure_geometry_bits(geometry));
        assert!(has_configure_geometry_bits(ConfigWindowBits::HEIGHT.bits()));
        assert!(!has_configure_geometry_bits(other));
        assert!(!has_configure_geometry_bits(
            strip_maximized_configure_bits(geometry, BOTH)
        ));
    }
}
