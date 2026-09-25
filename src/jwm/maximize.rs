//! Maximize policy executor.
//!
//! Every maximize change (protocol requests, the `togglemaximize` command,
//! snap/drop, manage-time adoption, drags) runs through one transaction
//! shaped like `setfullscreen`: snapshot, publish the accepted state, commit,
//! apply geometry, and reinstall the snapshot through
//! `restore_failed_mode_transition` when a backend step fails. The pure
//! decisions (request resolution, admission, geometry plans) live in
//! `crate::core::maximize`.
//!
//! This module talks to platforms only through the `Backend` capability
//! traits.

use log::{debug, error, warn};

use crate::backend::api::{Backend, MaximizeAxes, NetWmAction};
use crate::backend::common_define::WindowId;
use crate::core::maximize::{
    MaximizeAdmission, MaximizeFacts, MaximizeInput, MaximizeOrigin, MaximizeSnapshot, RestingSlot,
    admit_maximize, maximize_target, mirror_free_axes, plan_maximize, requested_axes, resting_slot,
};
use crate::core::models::{ClientKey, MonitorKey, WMClient};
use crate::core::state::WMState;
use crate::core::types::Rect;
use crate::jwm::Jwm;
use crate::jwm::types::WMArgEnum;

use super::visibility::hidden_x_left_of_desktop;

type MaximizeResult<T> = Result<T, Box<dyn std::error::Error>>;

impl Jwm {
    /// `self.monitor_migration_areas(mon).map(|(_, work)| work)`: monitor_work_area
    /// (bar, docks, tab bar excluded) with the w_* fallback. Never m_*.
    pub(crate) fn maximize_work_area(&self, mon_key: MonitorKey) -> Option<Rect> {
        self.monitor_migration_areas(mon_key).map(|(_, work)| work)
    }

    /// Entry for every protocol request (WindowMaximizeRequest and the legacy per-axis
    /// WindowStateRequest). Unknown windows are ignored. next = requested_axes(current,
    /// action, named); set_client_maximized(.., MaximizeOrigin::Client); errors logged with error!.
    pub(crate) fn handle_maximize_request(
        &mut self,
        backend: &mut dyn Backend,
        win: WindowId,
        action: NetWmAction,
        named: MaximizeAxes,
    ) {
        let Some(client_key) = self.wintoclient(win) else {
            return;
        };
        let Some(current) = self
            .state
            .clients
            .get(client_key)
            .map(|client| client.state.maximized_axes())
        else {
            return;
        };
        let next = requested_axes(current, action, named);
        if let Err(e) = self.set_client_maximized(backend, client_key, next, MaximizeOrigin::Client)
        {
            error!("Could not apply maximize request for {win:?}: {e}");
        }
    }

    /// Outer transaction (snapshot -> inner -> restore_failed_mode_transition on Err).
    /// `axes` is the REQUESTED next state. Ok(true) when state changed; Ok(false) for
    /// Reject/no-op (both still republish current and send the configure_client reply).
    pub(crate) fn set_client_maximized(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        axes: MaximizeAxes,
        origin: MaximizeOrigin,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        self.maximize_transaction(backend, client_key, axes, origin, None)
    }

    /// Manage-time adoption: same transaction with origin Adopt and `restore_hint`
    /// (V1 floating_rect) used when entering from NONE.
    pub(crate) fn adopt_client_maximized(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        axes: MaximizeAxes,
        restore_hint: Option<Rect>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        self.maximize_transaction(
            backend,
            client_key,
            axes,
            MaximizeOrigin::Adopt,
            restore_hint,
        )
    }

    /// Drag start / non-maximize snap / legacy release: no-op Ok(false) unless realized.
    /// Transaction: publish NONE; axes NONE; restore None; promoted -> maximize_restore_tiled
    /// = false and is_drag_floating = true; floating_* = live; live unchanged; configure_client.
    pub(crate) fn unmaximize_in_place(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let realized = self
            .state
            .clients
            .get(client_key)
            .is_some_and(|client| client.state.is_maximize_realized());
        if !realized {
            return Ok(false);
        }
        self.with_maximize_rollback(backend, client_key, |jwm, backend| {
            jwm.unmaximize_in_place_inner(backend, client_key)
        })
    }

    /// `MaximizeSnapshot { axes, restore_rect, restore_tiled, floating_rect }`; Default when the
    /// client is missing. `floating_rect` is the live `floating_*` slot, which a promoted client
    /// needs back on a cancelled drag (every drag step rewrites it).
    pub(crate) fn maximize_snapshot(&self, client_key: ClientKey) -> MaximizeSnapshot {
        self.state
            .clients
            .get(client_key)
            .map(|client| MaximizeSnapshot {
                axes: client.state.maximized_axes(),
                restore_rect: client.geometry.maximize_restore_rect,
                restore_tiled: client.state.maximize_restore_tiled,
                floating_rect: floating_rect(client),
            })
            .unwrap_or_default()
    }

    /// Drag cancel: no-op unless snapshot.axes.any() and the client is floating, not fullscreen/PiP.
    /// Transaction: publish snapshot.axes; commit axes/restore_rect/restore_tiled; is_drag_floating
    /// = false for a promoted client (other clients keep theirs); floating_* = the snapshot's
    /// floating_rect for a promoted client (its pre-promotion floating rect), otherwise
    /// restore_rect (M4); configure_client.
    pub(crate) fn reinstate_maximize_snapshot(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        snapshot: MaximizeSnapshot,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if !snapshot.axes.any() {
            return Ok(());
        }
        let eligible = self.state.clients.get(client_key).is_some_and(|client| {
            client.state.is_floating && !client.state.is_fullscreen && !client.state.is_pip
        });
        if !eligible {
            return Ok(());
        }
        self.with_maximize_rollback(backend, client_key, |jwm, backend| {
            jwm.reinstate_maximize_snapshot_inner(backend, client_key, snapshot)
        })
    }

    /// Called by arrange() after arrangemon(mon). Idempotent, no protocol writes, no info!.
    ///
    /// Re-fits every realized maximized client on `mon_key` to the current
    /// work area and re-syncs its restore rect (free axes from the live rect).
    /// A client already at its target is not touched, so repeated arranges
    /// cost one filter pass per client.
    pub(crate) fn refit_maximized_clients(
        &mut self,
        backend: &mut dyn Backend,
        mon_key: MonitorKey,
    ) {
        let Some(clients) = self.state.monitor_clients.get(mon_key) else {
            return;
        };
        let candidates: Vec<ClientKey> = clients
            .iter()
            .copied()
            .filter(|&client_key| {
                self.state.clients.get(client_key).is_some_and(|client| {
                    client.state.is_maximize_realized()
                        && !client.state.is_hidden
                        && client.geometry.hidden_x.is_none()
                        && !client.state.is_dock
                })
            })
            .filter(|&client_key| self.is_client_visible_on_monitor(client_key, mon_key))
            .collect();
        if candidates.is_empty() {
            return;
        }
        // The work area walks the monitor's clients (docks, struts), so it is
        // only computed once a maximized client actually needs it.
        let Some(area) = self.maximize_work_area(mon_key) else {
            return;
        };

        for client_key in candidates {
            let Some((win, live, restore, axes, border_w, promoted)) =
                self.state.clients.get(client_key).map(|client| {
                    (
                        client.win,
                        Rect::new(
                            client.geometry.x,
                            client.geometry.y,
                            client.geometry.w,
                            client.geometry.h,
                        ),
                        client.geometry.maximize_restore_rect,
                        client.state.maximized_axes(),
                        client.geometry.border_w,
                        client.state.maximize_restore_tiled,
                    )
                })
            else {
                continue;
            };
            let base = mirror_free_axes(restore.unwrap_or(live), live, axes);
            let mut target = maximize_target(base, area, axes, border_w);
            if let Err(error) = self.applysizehints(
                backend,
                client_key,
                &mut target.x,
                &mut target.y,
                &mut target.w,
                &mut target.h,
                false,
            ) {
                warn!("could not refit maximized window {win:?}: {error}");
                continue;
            }
            if let Some(client) = self.state.clients.get_mut(client_key) {
                client.geometry.maximize_restore_rect = Some(base);
                if !promoted {
                    set_floating_rect(client, base);
                }
            }
            if target != live {
                debug!("[refit_maximized_clients] {win:?} -> {target:?}");
                // `old_*` may still hold a fullscreen or layout return rect
                // that this refit must not clobber.
                if let Err(error) = self.refit_keeping_restore_slot(backend, client_key, target) {
                    warn!("could not refit maximized window {win:?}: {error}");
                }
            }
        }
    }

    /// WMFuncType command: selected client; no-op when none/fullscreen/PiP;
    /// set_client_maximized(requested_axes(current, Toggle, BOTH), User)?.
    pub fn togglemaximize(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(sel_mon_key) = self.state.sel_mon else {
            return Ok(());
        };
        let Some(client_key) = self.state.monitors.get(sel_mon_key).and_then(|m| m.sel) else {
            return Ok(());
        };
        let Some(current) = self
            .state
            .clients
            .get(client_key)
            .filter(|client| !client.state.is_fullscreen && !client.state.is_pip)
            .map(|client| client.state.maximized_axes())
        else {
            return Ok(());
        };
        let next = requested_axes(current, NetWmAction::Toggle, MaximizeAxes::BOTH);
        self.set_client_maximized(backend, client_key, next, MaximizeOrigin::User)?;
        Ok(())
    }

    /// Run one maximize transaction step with the same snapshot/rollback
    /// contract as `setfullscreen`: on any error the complete pre-transition
    /// client, animation and monitor order are reinstalled and the previous
    /// maximize state is republished before the error is returned.
    fn with_maximize_rollback<T>(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        step: impl FnOnce(&mut Jwm, &mut dyn Backend) -> MaximizeResult<T>,
    ) -> MaximizeResult<T> {
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

        match step(self, backend) {
            Ok(value) => Ok(value),
            Err(error) => {
                self.restore_failed_mode_transition(
                    backend,
                    client_key,
                    &previous_client,
                    previous_animation,
                    previous_monitor_order,
                );
                Err(error)
            }
        }
    }

    fn maximize_transaction(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        next: MaximizeAxes,
        origin: MaximizeOrigin,
        restore_hint: Option<Rect>,
    ) -> MaximizeResult<bool> {
        self.with_maximize_rollback(backend, client_key, |jwm, backend| {
            jwm.set_client_maximized_inner(backend, client_key, next, origin, restore_hint)
        })
    }

    fn set_client_maximized_inner(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        next: MaximizeAxes,
        origin: MaximizeOrigin,
        restore_hint: Option<Rect>,
    ) -> MaximizeResult<bool> {
        // (a) Facts.
        let mon = self
            .state
            .clients
            .get(client_key)
            .ok_or("Client not found")?
            .mon;
        let work_area = mon.and_then(|mon_key| self.maximize_work_area(mon_key));
        let float_layout = mon
            .and_then(|mon_key| self.state.monitors.get(mon_key))
            .is_some_and(|monitor| monitor.lt.is_float());
        let client = self
            .state
            .clients
            .get(client_key)
            .ok_or("Client not found")?;
        let win = client.win;
        let current = client.state.maximized_axes();
        let suspended = client.state.is_fullscreen || client.state.is_pip;
        let was_floating = client.state.is_floating;
        let was_promoted = client.state.maximize_restore_tiled;
        let facts = MaximizeFacts {
            resting_floating: if suspended {
                client.state.old_state
            } else {
                client.state.is_floating
            },
            float_layout,
            suspended,
            is_fixed: client.state.is_fixed,
            is_dock: client.state.is_dock,
            has_work_area: work_area.is_some(),
        };

        // (b) Admission; refusals and no-ops still owe the client a reply.
        let admission = admit_maximize(current, next, facts, origin);
        if admission == MaximizeAdmission::Reject || next == current {
            debug!("[set_client_maximized] {win:?} {origin:?} {next:?}: {admission:?}, replying");
            if let Err(error) = backend.property_ops().set_maximized_state(win, current) {
                warn!("could not republish maximize state for {win:?}: {error}");
            }
            self.configure_client(backend, client_key)?;
            return Ok(false);
        }

        // (c) Slot.
        let slot = resting_slot(
            client.state.is_fullscreen,
            client.state.is_pip,
            client.state.is_hidden || client.geometry.hidden_x.is_some(),
        );
        // (d) Resting rect of that slot and the border it is measured with.
        let live = Rect::new(
            client.geometry.x,
            client.geometry.y,
            client.geometry.w,
            client.geometry.h,
        );
        let (resting, border_w) = match slot {
            RestingSlot::Live => (live, client.geometry.border_w),
            RestingSlot::HiddenRestore => (
                client.geometry.hidden_restore_rect.unwrap_or(live),
                client.geometry.border_w,
            ),
            RestingSlot::FullscreenReturn => (
                Rect::new(
                    client.geometry.old_x,
                    client.geometry.old_y,
                    client.geometry.old_w,
                    client.geometry.old_h,
                ),
                client.geometry.old_border_w,
            ),
            // A promoted client does not lend `floating_*` to PiP: that slot
            // keeps its pre-promotion floating rect, and leaving PiP rebuilds
            // the maximized rect from the restore rect, so that is the rect
            // it rests in.
            RestingSlot::PipReturn if was_promoted => (
                client
                    .geometry
                    .maximize_restore_rect
                    .unwrap_or_else(|| floating_rect(client)),
                client.geometry.border_w,
            ),
            RestingSlot::PipReturn => (floating_rect(client), client.geometry.border_w),
        };
        let live_border_w = client.geometry.border_w;
        let restore = client.geometry.maximize_restore_rect;

        // (e) Plan.
        let plan = plan_maximize(
            MaximizeInput {
                current,
                restore,
                restore_hint,
                resting,
                area: work_area.unwrap_or(resting),
                border_w,
            },
            next,
        );
        debug!(
            "[set_client_maximized] {win:?} {origin:?} {current:?} -> {:?} slot={slot:?}",
            plan.axes
        );

        // (f) Publish first: a failed protocol write must leave JWM's state
        // untouched (the rollback republishes `current`).
        backend.property_ops().set_maximized_state(win, plan.axes)?;

        // Whether the layout already holds the client. Manage-time adoption
        // runs before `attach_new_client`: an unattached client is neither
        // regrouped here (the attach places it in its group) nor arranged
        // half-managed.
        let attached = mon.is_some_and(|mon_key| {
            self.state
                .monitor_clients
                .get(mon_key)
                .is_some_and(|clients| clients.contains(&client_key))
        });

        // (g) Commit.
        let promote = admission == MaximizeAdmission::Promote;
        let exiting = !plan.axes.any();
        let retile = exiting && was_promoted;
        // Promotion moves the client to the floating tail; remember what it
        // preceded so the retile can put it back into the same slot.
        let anchor = if promote && attached {
            mon.and_then(|mon_key| self.restore_anchor_after(mon_key, client_key))
        } else {
            None
        };
        let mut restore_anchor = None;
        {
            let client = self
                .state
                .clients
                .get_mut(client_key)
                .ok_or("Client not found")?;
            client.state.set_maximized_axes(plan.axes);
            client.geometry.maximize_restore_rect = plan.restore_rect;
            if promote {
                client.state.maximize_restore_tiled = true;
                client.state.maximize_restore_anchor = anchor;
                client.state.is_floating = true;
                client.state.is_drag_floating = false;
            }
            if retile {
                if suspended {
                    // Fullscreen/PiP still own `is_floating`; the retile
                    // lands when that mode exits.
                    client.state.old_state = false;
                } else {
                    client.state.is_floating = false;
                    client.state.is_drag_floating = false;
                }
            }
            if exiting {
                client.state.maximize_restore_tiled = false;
                restore_anchor = client.state.maximize_restore_anchor.take();
            }
            // M4: a plain floating client's floating slot mirrors the restore
            // rect. PiP owns `floating_*` as its return slot, and a promoted
            // client keeps its pre-promotion floating rect there.
            if slot != RestingSlot::PipReturn && !client.state.maximize_restore_tiled {
                if let Some(restore_rect) = plan.restore_rect {
                    set_floating_rect(client, restore_rect);
                } else if !retile {
                    set_floating_rect(client, plan.resting_rect);
                }
            }
        }
        let floating_changed = self
            .state
            .clients
            .get(client_key)
            .is_some_and(|client| client.state.is_floating != was_floating);
        if floating_changed && attached {
            // A deferred retile (fullscreen/PiP still active) regroups when
            // that mode exits; only a retile landing now can use the anchor.
            let back_in_slot = retile
                && !suspended
                && mon.is_some_and(|mon_key| {
                    self.retile_before_anchor(mon_key, client_key, restore_anchor)
                });
            if !back_in_slot {
                self.reorder_client_in_monitor_groups(client_key);
            }
        }

        // (h) Geometry of the resting slot.
        let mut configured = false;
        match slot {
            RestingSlot::Live if retile => {
                if float_layout {
                    let r = plan.resting_rect;
                    self.resizeclient(backend, client_key, r.x, r.y, r.w, r.h)?;
                    configured = true;
                } else {
                    // The arrange below places the tile and is the reply.
                    configured = mon.is_some_and(|mon_key| {
                        self.is_client_visible_on_monitor(client_key, mon_key)
                    });
                }
            }
            RestingSlot::Live => {
                let mut r = plan.resting_rect;
                self.applysizehints(
                    backend, client_key, &mut r.x, &mut r.y, &mut r.w, &mut r.h, false,
                )?;
                if r != live {
                    self.resizeclient(backend, client_key, r.x, r.y, r.w, r.h)?;
                    configured = true;
                }
            }
            RestingSlot::HiddenRestore => {
                let mut r = plan.resting_rect;
                if !retile {
                    self.applysizehints(
                        backend, client_key, &mut r.x, &mut r.y, &mut r.w, &mut r.h, false,
                    )?;
                }
                // A parked client is never configured on-screen: stage the
                // new visible rect and move the real window at its parking x.
                let hidden_x = hidden_x_left_of_desktop(
                    self.desktop_left_edge(),
                    r.w.saturating_add(live_border_w.saturating_mul(2)),
                );
                if let Some(client) = self.state.clients.get_mut(client_key) {
                    client.geometry.hidden_restore_rect = Some(r);
                    client.geometry.hidden_x = Some(hidden_x);
                    client.geometry.x = hidden_x;
                    client.geometry.y = r.y;
                    client.geometry.w = r.w;
                    client.geometry.h = r.h;
                }
                let x11_border = if backend.has_compositor() {
                    0
                } else {
                    live_border_w.max(0) as u32
                };
                backend.window_ops().configure(
                    win,
                    hidden_x,
                    r.y,
                    r.w.max(1) as u32,
                    r.h.max(1) as u32,
                    x11_border,
                )?;
                configured = true;
            }
            RestingSlot::FullscreenReturn => {
                if let Some(client) = self.state.clients.get_mut(client_key) {
                    client.geometry.old_x = plan.resting_rect.x;
                    client.geometry.old_y = plan.resting_rect.y;
                    client.geometry.old_w = plan.resting_rect.w;
                    client.geometry.old_h = plan.resting_rect.h;
                }
            }
            // A promoted client's `floating_*` is its pre-promotion floating
            // rect, not PiP's return slot; leaving PiP re-derives its rect.
            RestingSlot::PipReturn => {
                if !was_promoted && let Some(client) = self.state.clients.get_mut(client_key) {
                    set_floating_rect(client, plan.resting_rect);
                }
            }
        }

        // (i) Layout membership changed: let the layout take or release it.
        if floating_changed && attached {
            self.arrange(backend, mon);
        }

        // (j) Keep the restart snapshot of a minimized client current.
        let hidden = self
            .state
            .clients
            .get(client_key)
            .is_some_and(|client| client.state.is_hidden);
        if hidden
            && attached
            && let Err(error) = self.persist_minimized_restore_state(backend, client_key)
        {
            warn!("could not refresh minimized restore state after maximize for {win:?}: {error}");
        }

        // (k) Every accepted change owes the client exactly one configure;
        // this is only the reply, so a failure is not worth a rollback.
        if !configured && let Err(error) = self.configure_client(backend, client_key) {
            warn!("could not send the maximize reply configure for {win:?}: {error}");
        }
        Ok(true)
    }

    fn unmaximize_in_place_inner(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> MaximizeResult<bool> {
        let win = self
            .state
            .clients
            .get(client_key)
            .ok_or("Client not found")?
            .win;
        backend
            .property_ops()
            .set_maximized_state(win, MaximizeAxes::NONE)?;
        let client = self
            .state
            .clients
            .get_mut(client_key)
            .ok_or("Client not found")?;
        client.state.set_maximized_axes(MaximizeAxes::NONE);
        client.geometry.maximize_restore_rect = None;
        if client.state.maximize_restore_tiled {
            // The drag now owns the window; a later layout re-apply may pull
            // it back into the tiles like any other drag-floated window.
            // `maximize_restore_anchor` stays: nothing reads it until a
            // promotion replaces it, or a cancelled drag reinstates this
            // promotion (the snapshot carries only the flag) and needs it.
            client.state.maximize_restore_tiled = false;
            client.state.is_drag_floating = true;
        }
        let live = Rect::new(
            client.geometry.x,
            client.geometry.y,
            client.geometry.w,
            client.geometry.h,
        );
        set_floating_rect(client, live);
        self.configure_client(backend, client_key)?;
        Ok(true)
    }

    fn reinstate_maximize_snapshot_inner(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        snapshot: MaximizeSnapshot,
    ) -> MaximizeResult<()> {
        let win = self
            .state
            .clients
            .get(client_key)
            .ok_or("Client not found")?
            .win;
        backend
            .property_ops()
            .set_maximized_state(win, snapshot.axes)?;
        let client = self
            .state
            .clients
            .get_mut(client_key)
            .ok_or("Client not found")?;
        client.state.set_maximized_axes(snapshot.axes);
        client.geometry.maximize_restore_rect = snapshot.restore_rect;
        client.state.maximize_restore_tiled = snapshot.restore_tiled;
        // `unmaximize_in_place` kept a promoted client's anchor for this;
        // any other client has none.
        if !snapshot.restore_tiled {
            client.state.maximize_restore_anchor = None;
        }
        // Only a promoted window's flag was the drag's doing: promotion
        // cleared it and `unmaximize_in_place` set it. A plain floating
        // window keeps the flag it had when the drag found it, so a window
        // dragged out of the tiles and then maximized can still be pulled
        // back into them once it is unmaximized.
        if snapshot.restore_tiled {
            client.state.is_drag_floating = false;
        }
        // `cancel_pointer_drag` runs this last, after its own geometry
        // restore, so this write outlasts every `floating_*` update the
        // drag made. A promoted client gets its pre-promotion floating rect
        // back; `unmaximize_in_place` had replaced it with the maximized rect.
        if snapshot.restore_tiled {
            set_floating_rect(client, snapshot.floating_rect);
        } else if let Some(restore_rect) = snapshot.restore_rect {
            set_floating_rect(client, restore_rect);
        }
        self.configure_client(backend, client_key)?;
        Ok(())
    }

    /// The anchor `client_key` records when maximize promotes it: the first
    /// tile after it in `mon_key`'s list or, while windows promoted from
    /// between it and that tile are still out of the layout, the first of
    /// those (the one none of the others names as its anchor). Each then
    /// re-tiles into its own slot whichever returns first.
    fn restore_anchor_after(
        &self,
        mon_key: MonitorKey,
        client_key: ClientKey,
    ) -> Option<ClientKey> {
        let clients = self.state.monitor_clients.get(mon_key)?;
        let position = clients.iter().position(|&key| key == client_key)?;
        let tile = clients.iter().skip(position + 1).copied().find(|&key| {
            self.state
                .clients
                .get(key)
                .is_some_and(|client| !client.state.is_floating)
        });
        let promoted_anchor = |key: ClientKey| {
            self.state
                .clients
                .get(key)
                .filter(|client| client.state.maximize_restore_tiled && client.state.is_floating)
                .map(|client| client.state.maximize_restore_anchor)
        };
        let resting_before_tile: Vec<ClientKey> = clients
            .iter()
            .copied()
            .filter(|&key| {
                key != client_key
                    && promoted_anchor(key).is_some_and(|anchor| {
                        resting_anchor(&self.state, clients, key, anchor) == tile
                    })
            })
            .collect();
        resting_before_tile
            .iter()
            .copied()
            .find(|&key| {
                !resting_before_tile
                    .iter()
                    .any(|&other| promoted_anchor(other) == Some(Some(key)))
            })
            .or(tile)
    }

    /// Re-point every anchor on `mon_key` that names `client_key`. Call it
    /// before `client_key` leaves `mon_key`'s list, as `detach` and
    /// `detach_from_monitor` do for every close, sendmon and scratchpad
    /// reveal, and [`move_to_front`](Self::move_to_front) does for a tile's
    /// pop: [`resting_anchor`] gives up on an anchor that is no longer
    /// listed, and a promoted window anchored to a window that left re-tiled
    /// at the end of the tiled group. A promoted `client_key` hands on its
    /// own anchor, so the chain resolves as it did; a tile hands on the
    /// anchor a promotion of it would record (see
    /// [`restore_anchor_after`](Self::restore_anchor_after)). Any other
    /// floating window (fullscreen, PiP or floated by the user) hands on
    /// none: [`resting_anchor`] already stops at it, so the windows naming
    /// it keep resting at the end of the tiled group. Its slot in the
    /// floating tail says nothing about the tiles, and the promoted window
    /// resting last there may be one that names them back.
    pub(crate) fn splice_restore_anchors(&mut self, mon_key: MonitorKey, client_key: ClientKey) {
        let Some(departing) = self.state.clients.get(client_key) else {
            return;
        };
        let floating = departing.state.is_floating;
        let promoted = departing.state.maximize_restore_tiled && floating;
        let own_anchor = departing.state.maximize_restore_anchor;
        let anchored: Vec<ClientKey> = self
            .state
            .monitor_clients
            .get(mon_key)
            .into_iter()
            .flatten()
            .copied()
            .filter(|&key| {
                key != client_key
                    && self.state.clients.get(key).is_some_and(|client| {
                        client.state.maximize_restore_anchor == Some(client_key)
                    })
            })
            .collect();
        if anchored.is_empty() {
            return;
        }
        let successor = if promoted {
            own_anchor
        } else if floating {
            None
        } else {
            self.restore_anchor_after(mon_key, client_key)
        };
        for key in anchored {
            if let Some(client) = self.state.clients.get_mut(key) {
                client.state.maximize_restore_anchor = successor.filter(|&anchor| anchor != key);
            }
        }
    }

    /// Move a client to the front of its group in its monitor's client
    /// list, as zoom's pop, the overview confirm and leaving VSTACK do to
    /// make it master. A tile goes to index 0 and becomes master. A floating
    /// window cannot be master: it goes to the front of the floating tail,
    /// behind every tile, since `reorder_client_in_monitor_groups`,
    /// `attach_new_client` and drag-attach take the first floating window
    /// for the end of the tiled group. At index 0 it made the next window
    /// to re-tile there the master.
    ///
    /// Unlike a `detach` followed by an insert at index 0, only a tile has
    /// its anchors re-pointed (see [`Self::splice_restore_anchors`]): it
    /// leaves its slot. A floating window stays listed, so the anchors
    /// naming it stay valid: a promoted one's own anchor still carries the
    /// chain through it, and [`resting_anchor`] gives up on any other
    /// floating anchor anyway. Handing a promoted window's anchor on here
    /// would leave it and the windows resting in front of it naming the
    /// same tile, and they would re-tile in the order they were unmaximized.
    pub fn move_to_front(&mut self, client_key: ClientKey) {
        let Some(client) = self.state.clients.get(client_key) else {
            return;
        };
        let Some(mon_key) = client.mon else {
            return;
        };
        let floating = client.state.is_floating;
        if !floating {
            self.splice_restore_anchors(mon_key, client_key);
        }
        let state = &mut self.state;
        let Some(clients) = state.monitor_clients.get_mut(mon_key) else {
            return;
        };
        if let Some(position) = clients.iter().position(|&key| key == client_key) {
            clients.remove(position);
        }
        let index = if floating {
            clients
                .iter()
                .position(|&key| {
                    state
                        .clients
                        .get(key)
                        .is_some_and(|client| client.state.is_floating)
                })
                .unwrap_or(clients.len())
        } else {
            0
        };
        clients.insert(index, client_key);
    }

    /// Re-tile `client_key` in front of the tile it preceded when maximize
    /// promoted it (see [`resting_anchor`]), and report whether it did.
    /// Otherwise `reorder_client_in_monitor_groups` has to regroup it.
    fn retile_before_anchor(
        &mut self,
        mon_key: MonitorKey,
        client_key: ClientKey,
        anchor: Option<ClientKey>,
    ) -> bool {
        let Some(target) = self
            .state
            .monitor_clients
            .get(mon_key)
            .and_then(|clients| resting_anchor(&self.state, clients, client_key, anchor))
        else {
            return false;
        };
        let Some(clients) = self.state.monitor_clients.get_mut(mon_key) else {
            return false;
        };
        let (Some(from), Some(to)) = (
            clients.iter().position(|&key| key == client_key),
            clients.iter().position(|&key| key == target),
        ) else {
            return false;
        };
        let moved = clients.remove(from);
        clients.insert(if from < to { to - 1 } else { to }, moved);
        true
    }
}

/// The tile in `list` that a promoted `client_key` rests in front of, given
/// its `maximize_restore_anchor`: the anchor while it is still a tile in the
/// list. An anchor promoted since sits in the floating tail itself, and its
/// own anchor is where both return, so the chain is followed (at most once
/// around the list). `None` when the chain ends, leaves the list, or reaches
/// a floating window maximize did not promote; the client then rests at the
/// end of the tiled group.
pub(crate) fn resting_anchor(
    state: &WMState,
    list: &[ClientKey],
    client_key: ClientKey,
    anchor: Option<ClientKey>,
) -> Option<ClientKey> {
    let mut anchor = anchor;
    for _ in 0..list.len() {
        let key = anchor.filter(|&key| key != client_key && list.contains(&key))?;
        let client = state.clients.get(key)?;
        if !client.state.is_floating {
            return Some(key);
        }
        if !client.state.maximize_restore_tiled {
            return None;
        }
        anchor = client.state.maximize_restore_anchor;
    }
    None
}

/// `list` (a monitor's client list) in the order its windows rest in once
/// nothing is maximized.
///
/// Maximize moves a tile it promotes out of the layout to the floating tail
/// and records the window it preceded; leaving maximize re-tiles it in front
/// of that window again ([`resting_anchor`]). A saved session records the
/// window tiled, so it records it in that slot too: from the floating tail
/// the restore would re-tile it after every other tile. A promoted window
/// whose anchor was promoted as well rests directly in front of it, however
/// many neighbours are out and whichever was promoted first; one with no
/// tile to rest in front of rests at the end of the tiled group. It lives
/// here, next to the live retile, so the two cannot drift apart.
pub(crate) fn resting_order(state: &WMState, list: &[ClientKey]) -> Vec<ClientKey> {
    let promoted = |key: &ClientKey| {
        state.clients.get(*key).is_some_and(|c| {
            c.state.maximize_restore_tiled && c.state.is_floating && c.state.maximized_axes().any()
        })
    };
    let (promoted_keys, mut order): (Vec<ClientKey>, Vec<ClientKey>) =
        list.iter().copied().partition(promoted);
    let mut pending = promoted_keys.clone();
    let place = |order: &mut Vec<ClientKey>, key: ClientKey, before: Option<ClientKey>| {
        let index = before
            .and_then(|before| order.iter().position(|&other| other == before))
            .or_else(|| {
                // The end of the tiled group: the first floating window
                // maximize did not pull out of the layout.
                order.iter().position(|other| {
                    !promoted_keys.contains(other)
                        && state
                            .clients
                            .get(*other)
                            .is_some_and(|c| c.state.is_floating)
                })
            })
            .unwrap_or(order.len());
        order.insert(index, key);
    };
    // Every pass places at least one window, or gives up on the rest.
    while !pending.is_empty() {
        let before = pending.len();
        let mut index = 0;
        while index < pending.len() {
            let key = pending[index];
            let anchor = state
                .clients
                .get(key)
                .and_then(|c| c.state.maximize_restore_anchor);
            let promoted_anchor =
                anchor.filter(|&anchor| anchor != key && promoted_keys.contains(&anchor));
            let target = match promoted_anchor {
                // An anchor out of the layout itself: wait until it is back.
                Some(anchor) if !order.contains(&anchor) => {
                    index += 1;
                    continue;
                }
                Some(anchor) => Some(anchor),
                // A tile, or no tile to rest in front of.
                None => resting_anchor(state, list, key, anchor),
            };
            pending.remove(index);
            place(&mut order, key, target);
        }
        if pending.len() == before {
            // Anchors naming each other in a cycle: no tile to rest before.
            for key in std::mem::take(&mut pending) {
                place(&mut order, key, None);
            }
        }
    }
    order
}

fn floating_rect(client: &WMClient) -> Rect {
    Rect::new(
        client.geometry.floating_x,
        client.geometry.floating_y,
        client.geometry.floating_w,
        client.geometry.floating_h,
    )
}

fn set_floating_rect(client: &mut WMClient, rect: Rect) {
    client.geometry.floating_x = rect.x;
    client.geometry.floating_y = rect.y;
    client.geometry.floating_w = rect.w;
    client.geometry.floating_h = rect.h;
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use crate::backend::api::MaximizeAxes;
    use crate::backend::common_define::WindowId;
    use crate::core::layout::LayoutEnum;
    use crate::core::models::{ClientKey, WMClient};
    use crate::jwm::Jwm;
    use crate::jwm::monitor::test_support::{DisplaySpyBackend, output};
    use crate::jwm::types::WMArgEnum;

    /// Three tiled windows attached in order on a TILE monitor and
    /// arranged, with the first (the master) selected.
    fn three_tiles(backend: &mut DisplaySpyBackend, raw: u64) -> (Jwm, [ClientKey; 3]) {
        let mut jwm =
            Jwm::new_with_runtime_backend(backend, "test").expect("a spy backend builds a JWM");
        let monitor = jwm.state.monitor_order[0];
        jwm.state.monitors[monitor].lt = Rc::new(LayoutEnum::TILE);
        let tags = jwm.state.monitors[monitor].get_active_tags();
        let keys = [0, 1, 2].map(|index| {
            let mut client = WMClient::new(WindowId::from_raw(raw + index));
            client.mon = Some(monitor);
            client.state.tags = tags;
            client.geometry.border_w = 2;
            let key = jwm.insert_client(client);
            jwm.attach_to_monitor(key, monitor);
            key
        });
        jwm.state.monitors[monitor].set_selected_client_for_current_tag(Some(keys[0]));
        jwm.arrange(backend, Some(monitor));
        assert_eq!(jwm.state.monitor_clients[monitor], keys);
        (jwm, keys)
    }

    fn toggle_maximize_of(jwm: &mut Jwm, backend: &mut DisplaySpyBackend, key: ClientKey) {
        let monitor = jwm.state.monitor_order[0];
        jwm.state.monitors[monitor].set_selected_client_for_current_tag(Some(key));
        jwm.togglemaximize(backend, &WMArgEnum::Int(0))
            .expect("togglemaximize");
    }

    /// Regression: promotion moved the window to the floating tail and the
    /// retile put it back at the end of the tiled group, so maximizing and
    /// restoring the master made the next tile master and rearranged the
    /// whole layout.
    #[test]
    fn retiling_the_promoted_master_puts_it_back_in_its_slot() {
        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
        let (mut jwm, [x, y, z]) = three_tiles(&mut backend, 0x5b30);
        let monitor = jwm.state.monitor_order[0];
        let slots = [x, y, z].map(|key| jwm.state.clients[key].rect());

        toggle_maximize_of(&mut jwm, &mut backend, x);
        let client = &jwm.state.clients[x];
        assert!(client.state.maximize_restore_tiled);
        assert_eq!(client.state.maximize_restore_anchor, Some(y));
        assert_eq!(jwm.state.monitor_clients[monitor], vec![y, z, x]);

        toggle_maximize_of(&mut jwm, &mut backend, x);
        let client = &jwm.state.clients[x];
        assert!(!client.state.is_floating);
        assert!(!client.state.maximize_restore_tiled);
        assert_eq!(client.state.maximize_restore_anchor, None);
        assert_eq!(jwm.state.monitor_clients[monitor], vec![x, y, z]);
        assert_eq!([x, y, z].map(|key| jwm.state.clients[key].rect()), slots);
    }

    /// Two neighbours promoted one after the other, in either order: the
    /// later one's anchor may be a window already out of the layout, and
    /// following the anchors puts both back in their slots whichever
    /// returns first.
    #[test]
    fn promoted_neighbours_retile_into_their_slots_in_any_order() {
        for (promote_first, return_first) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
            let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
            let (mut jwm, [x, y, z]) = three_tiles(&mut backend, 0x5b40);
            let monitor = jwm.state.monitor_order[0];
            let slots = [x, y, z].map(|key| jwm.state.clients[key].rect());
            let pair = |first: usize| if first == 0 { [x, y] } else { [y, x] };

            for key in pair(promote_first) {
                toggle_maximize_of(&mut jwm, &mut backend, key);
            }
            // x rests in front of y and y in front of z, promoted or not.
            assert_eq!(jwm.state.clients[x].state.maximize_restore_anchor, Some(y));
            assert_eq!(jwm.state.clients[y].state.maximize_restore_anchor, Some(z));

            for key in pair(return_first) {
                toggle_maximize_of(&mut jwm, &mut backend, key);
            }
            let case = format!("promoted first: {promote_first}, back first: {return_first}");
            assert_eq!(jwm.state.monitor_clients[monitor], vec![x, y, z], "{case}");
            assert_eq!(
                [x, y, z].map(|key| jwm.state.clients[key].rect()),
                slots,
                "{case}"
            );
        }
    }

    /// A drag of a promoted window unmaximizes it in place; cancelling the
    /// drag reinstates the promotion, and the later retile still finds the
    /// window's slot.
    #[test]
    fn a_cancelled_drag_keeps_the_promoted_masters_slot() {
        use crate::jwm::mouse_handler::{DragCtl, DragMode};

        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
        let (mut jwm, [x, y, z]) = three_tiles(&mut backend, 0x5b50);
        let monitor = jwm.state.monitor_order[0];
        let slots = [x, y, z].map(|key| jwm.state.clients[key].rect());

        toggle_maximize_of(&mut jwm, &mut backend, x);
        let maximized = jwm.state.clients[x].rect();
        jwm.drag_ctl = Some(DragCtl {
            client: x,
            win: jwm.state.clients[x].win,
            mode: DragMode::MoveFloat,
            start_root: (0.0, 0.0),
            activated: false,
            was_floating: true,
            orig_geom: maximized,
            orig_index: jwm.state.monitor_clients[monitor]
                .iter()
                .position(|&k| k == x),
            mon: Some(monitor),
            orig_maximize: jwm.maximize_snapshot(x),
        });
        jwm.activate_pointer_drag(&mut backend)
            .expect("the drag activates");
        assert!(!jwm.state.clients[x].state.maximize_restore_tiled);
        jwm.cancel_pointer_drag(&mut backend);
        let client = &jwm.state.clients[x];
        assert!(client.state.maximize_restore_tiled);
        assert_eq!(client.state.maximized_axes(), MaximizeAxes::BOTH);

        toggle_maximize_of(&mut jwm, &mut backend, x);
        assert_eq!(jwm.state.monitor_clients[monitor], vec![x, y, z]);
        assert_eq!([x, y, z].map(|key| jwm.state.clients[key].rect()), slots);
    }

    /// Regression: an anchor that closed left the monitor list and nothing
    /// re-pointed the anchors naming it, so the promoted master re-tiled at
    /// the end of the tiled group and the next tile stayed master. With
    /// `chained`, the closed window is promoted itself (as a maximized
    /// window on another tag often is) and never was in the layout.
    #[test]
    fn closing_the_anchor_keeps_the_promoted_masters_slot() {
        for chained in [false, true] {
            let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
            let (mut jwm, [x, y, z]) = three_tiles(&mut backend, 0x5b60);
            let monitor = jwm.state.monitor_order[0];
            if chained {
                toggle_maximize_of(&mut jwm, &mut backend, y);
            }
            toggle_maximize_of(&mut jwm, &mut backend, x);
            assert_eq!(jwm.state.clients[x].state.maximize_restore_anchor, Some(y));

            jwm.unmanage_regular_client(&mut backend, y, true)
                .expect("y closes");
            assert_eq!(
                jwm.state.clients[x].state.maximize_restore_anchor,
                Some(z),
                "chained: {chained}"
            );

            toggle_maximize_of(&mut jwm, &mut backend, x);
            assert!(!jwm.state.clients[x].state.is_floating);
            assert_eq!(
                jwm.state.monitor_clients[monitor],
                vec![x, z],
                "chained: {chained}"
            );
        }
    }

    /// The window after the promoted master closes while another promoted
    /// window rests in front of the next tile: the master's anchor moves to
    /// that window, so both return to their own slots in either order.
    #[test]
    fn closing_the_anchor_keeps_promoted_neighbours_in_order() {
        for return_first in [0, 1] {
            let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
            let (mut jwm, [x, y, z]) = three_tiles(&mut backend, 0x5b70);
            let monitor = jwm.state.monitor_order[0];
            let mut w = WMClient::new(WindowId::from_raw(0x5b7f));
            w.mon = Some(monitor);
            w.state.tags = jwm.state.monitors[monitor].get_active_tags();
            w.geometry.border_w = 2;
            let w = jwm.insert_client(w);
            jwm.attach_to_monitor(w, monitor);
            jwm.arrange(&mut backend, Some(monitor));
            assert_eq!(jwm.state.monitor_clients[monitor], vec![x, y, z, w]);

            toggle_maximize_of(&mut jwm, &mut backend, z);
            toggle_maximize_of(&mut jwm, &mut backend, x);
            jwm.unmanage_regular_client(&mut backend, y, true)
                .expect("y closes");
            assert_eq!(jwm.state.clients[x].state.maximize_restore_anchor, Some(z));

            let order = if return_first == 0 { [x, z] } else { [z, x] };
            for key in order {
                toggle_maximize_of(&mut jwm, &mut backend, key);
            }
            assert_eq!(
                jwm.state.monitor_clients[monitor],
                vec![x, z, w],
                "back first: {return_first}"
            );
        }
    }

    /// Regression: only a close re-pointed the anchors naming a window.
    /// Sent to another monitor, the anchor left the list all the same, and
    /// the promoted master re-tiled at the end of the tiled group. With
    /// `chained`, the window sent is promoted itself.
    #[test]
    fn sending_the_anchor_to_another_monitor_keeps_the_promoted_masters_slot() {
        for chained in [false, true] {
            let mut backend = DisplaySpyBackend::new(vec![
                output(1, 0, 0, 1920, 1080),
                output(2, 1920, 0, 1920, 1080),
            ]);
            let (mut jwm, [x, y, z]) = three_tiles(&mut backend, 0x5b80);
            let [monitor, other] = [jwm.state.monitor_order[0], jwm.state.monitor_order[1]];
            if chained {
                toggle_maximize_of(&mut jwm, &mut backend, y);
            }
            toggle_maximize_of(&mut jwm, &mut backend, x);
            assert_eq!(jwm.state.clients[x].state.maximize_restore_anchor, Some(y));

            jwm.sendmon(&mut backend, Some(y), Some(other));
            assert_eq!(jwm.state.clients[y].mon, Some(other), "chained: {chained}");
            assert_eq!(
                jwm.state.clients[x].state.maximize_restore_anchor,
                Some(z),
                "chained: {chained}"
            );

            jwm.state.sel_mon = Some(monitor);
            toggle_maximize_of(&mut jwm, &mut backend, x);
            assert!(!jwm.state.clients[x].state.is_floating);
            assert_eq!(
                jwm.state.monitor_clients[monitor],
                vec![x, z],
                "chained: {chained}"
            );
        }
    }

    /// Regression: zoom pops the tile after the master to the front, and a
    /// promoted window anchored to it followed it there, taking master from
    /// the window the user had just zoomed. The anchor now moves on to the
    /// window after the popped one's old slot, as if the promoted window had
    /// stayed tiled through the zoom.
    #[test]
    fn zooming_the_anchor_keeps_the_zoomed_window_master() {
        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
        let (mut jwm, [x, y, z]) = three_tiles(&mut backend, 0x5b90);
        let monitor = jwm.state.monitor_order[0];
        toggle_maximize_of(&mut jwm, &mut backend, y);
        assert_eq!(jwm.state.clients[y].state.maximize_restore_anchor, Some(z));
        assert_eq!(jwm.state.monitor_clients[monitor], vec![x, z, y]);

        jwm.state.monitors[monitor].set_selected_client_for_current_tag(Some(x));
        jwm.zoom(&mut backend, &WMArgEnum::Int(0)).expect("zoom");
        assert_eq!(jwm.state.monitor_clients[monitor], vec![z, x, y]);
        assert_eq!(jwm.state.clients[y].state.maximize_restore_anchor, None);

        toggle_maximize_of(&mut jwm, &mut backend, y);
        assert!(!jwm.state.clients[y].state.is_floating);
        assert_eq!(jwm.state.monitor_clients[monitor], vec![z, x, y]);
    }

    /// A pop that moves a window no anchor names leaves the promoted
    /// master's anchor alone: it returns in front of the tile it preceded.
    #[test]
    fn zooming_another_tile_keeps_the_promoted_masters_anchor() {
        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
        let (mut jwm, [x, y, z]) = three_tiles(&mut backend, 0x5ba0);
        let monitor = jwm.state.monitor_order[0];
        toggle_maximize_of(&mut jwm, &mut backend, x);

        jwm.state.monitors[monitor].set_selected_client_for_current_tag(Some(y));
        jwm.zoom(&mut backend, &WMArgEnum::Int(0)).expect("zoom");
        assert_eq!(jwm.state.monitor_clients[monitor], vec![z, y, x]);
        assert_eq!(jwm.state.clients[x].state.maximize_restore_anchor, Some(y));

        toggle_maximize_of(&mut jwm, &mut backend, x);
        assert_eq!(jwm.state.monitor_clients[monitor], vec![z, x, y]);
    }

    /// Regression: `detach_from_monitor` took the anchor out of the list
    /// without re-pointing the anchors naming it.
    #[test]
    fn detaching_the_anchor_from_its_monitor_keeps_the_promoted_masters_slot() {
        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
        let (mut jwm, [x, y, z]) = three_tiles(&mut backend, 0x5bb0);
        let monitor = jwm.state.monitor_order[0];
        toggle_maximize_of(&mut jwm, &mut backend, x);

        jwm.detach_from_monitor(y, monitor);
        jwm.state.clients[y].mon = None;
        assert_eq!(jwm.state.clients[x].state.maximize_restore_anchor, Some(z));

        toggle_maximize_of(&mut jwm, &mut backend, x);
        assert_eq!(jwm.state.monitor_clients[monitor], vec![x, z]);
    }

    /// Regression: the overview confirm and leaving VSTACK moved the
    /// selected window to the front by detaching it and reinserting it at
    /// index 0. The detach handed a promoted window's anchor on to the
    /// windows resting in front of it although it stayed listed. Both then
    /// named the same tile and came back in the order they were unmaximized,
    /// so returning `b` first left the old master `a` second.
    #[test]
    fn moving_a_promoted_window_to_the_front_keeps_promoted_neighbours_in_order() {
        for return_first in [0, 1] {
            let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
            let (mut jwm, [a, b, c]) = three_tiles(&mut backend, 0x5bc0);
            let monitor = jwm.state.monitor_order[0];
            let slots = [a, b, c].map(|key| jwm.state.clients[key].rect());
            toggle_maximize_of(&mut jwm, &mut backend, b);
            toggle_maximize_of(&mut jwm, &mut backend, a);
            assert_eq!(jwm.state.clients[a].state.maximize_restore_anchor, Some(b));
            assert_eq!(jwm.state.monitor_clients[monitor], vec![c, b, a]);

            // `b` floats: it leads the floating tail, behind the tile `c`.
            jwm.move_to_front(b);
            assert_eq!(jwm.state.monitor_clients[monitor], vec![c, b, a]);
            assert_eq!(jwm.state.clients[a].state.maximize_restore_anchor, Some(b));

            let order = if return_first == 0 { [a, b] } else { [b, a] };
            for key in order {
                toggle_maximize_of(&mut jwm, &mut backend, key);
            }
            let case = format!("back first: {return_first}");
            assert_eq!(jwm.state.monitor_clients[monitor], vec![a, b, c], "{case}");
            assert_eq!(
                [a, b, c].map(|key| jwm.state.clients[key].rect()),
                slots,
                "{case}"
            );
        }
    }

    /// A tile moved to the front leaves its slot: the promoted master
    /// anchored to it moves on to the next tile and returns as if it had
    /// stayed tiled through the pop.
    #[test]
    fn moving_a_tile_to_the_front_moves_its_anchors_on() {
        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
        let (mut jwm, [x, y, z]) = three_tiles(&mut backend, 0x5bd0);
        let monitor = jwm.state.monitor_order[0];
        toggle_maximize_of(&mut jwm, &mut backend, x);
        assert_eq!(jwm.state.monitor_clients[monitor], vec![y, z, x]);

        jwm.move_to_front(y);
        assert_eq!(jwm.state.monitor_clients[monitor], vec![y, z, x]);
        assert_eq!(jwm.state.clients[x].state.maximize_restore_anchor, Some(z));

        toggle_maximize_of(&mut jwm, &mut backend, x);
        assert_eq!(jwm.state.monitor_clients[monitor], vec![y, x, z]);
    }

    /// Regression: moving a floating window to the front put it at index 0,
    /// ahead of every tile. A window re-tiling without a slot goes in front
    /// of the first floating window, the end of the tiled group, so `c`
    /// came back as master ahead of `a`, and `b` followed it there: the
    /// layout ended `[b, c, a]` or `[a, b, c]` by the order they returned.
    #[test]
    fn moving_a_floating_window_to_the_front_keeps_it_behind_the_tiles() {
        for return_first in [0, 1] {
            let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
            let (mut jwm, [a, b, c]) = three_tiles(&mut backend, 0x5c00);
            let monitor = jwm.state.monitor_order[0];
            let slots = [a, b, c].map(|key| jwm.state.clients[key].rect());
            toggle_maximize_of(&mut jwm, &mut backend, c);
            toggle_maximize_of(&mut jwm, &mut backend, b);
            assert_eq!(jwm.state.clients[c].state.maximize_restore_anchor, None);
            assert_eq!(jwm.state.clients[b].state.maximize_restore_anchor, Some(c));
            assert_eq!(jwm.state.monitor_clients[monitor], vec![a, c, b]);

            // As the overview confirm and leaving VSTACK do with `b` picked.
            jwm.move_to_front(b);
            assert_eq!(jwm.state.monitor_clients[monitor], vec![a, b, c]);

            let order = if return_first == 0 { [c, b] } else { [b, c] };
            for key in order {
                toggle_maximize_of(&mut jwm, &mut backend, key);
            }
            let case = format!("back first: {return_first}");
            assert_eq!(jwm.state.monitor_clients[monitor], vec![a, b, c], "{case}");
            assert_eq!(
                [a, b, c].map(|key| jwm.state.clients[key].rect()),
                slots,
                "{case}"
            );
        }
    }

    /// Regression: leaving VSTACK made the selection master by detaching it
    /// and reinserting it at index 0. A promoted selection stays listed, yet
    /// the detach handed its anchor on to the window resting in front of it,
    /// so both named the same tile and returning `b` first left the old
    /// master `a` second.
    #[test]
    fn leaving_vstack_with_a_promoted_selection_keeps_neighbours_in_order() {
        for return_first in [0, 1] {
            let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
            let (mut jwm, [a, b, c]) = three_tiles(&mut backend, 0x5be0);
            let monitor = jwm.state.monitor_order[0];
            toggle_maximize_of(&mut jwm, &mut backend, b);
            toggle_maximize_of(&mut jwm, &mut backend, a);
            jwm.setlayout(
                &mut backend,
                &WMArgEnum::Layout(Rc::new(LayoutEnum::VSTACK)),
            )
            .expect("vstack");
            jwm.state.monitors[monitor].set_selected_client_for_current_tag(Some(b));
            jwm.setlayout(&mut backend, &WMArgEnum::Layout(Rc::new(LayoutEnum::TILE)))
                .expect("tile");
            // A floating selection cannot be master: `b` stays behind `c`.
            assert_eq!(jwm.state.monitor_clients[monitor], vec![c, b, a]);

            let order = if return_first == 0 { [a, b] } else { [b, a] };
            for key in order {
                toggle_maximize_of(&mut jwm, &mut backend, key);
            }
            assert_eq!(
                jwm.state.monitor_clients[monitor],
                vec![a, b, c],
                "back first: {return_first}"
            );
        }
    }

    /// Regression: a window that left the tiles without maximize (it went
    /// fullscreen, or the user floated it) sits in the floating tail, where
    /// [`resting_anchor`] already gives up on it. Closing it handed the
    /// anchors naming it the promoted window resting at the end of the
    /// tiled group, which could be one naming them back: `p` and `q` named
    /// each other and came back in the order they were unmaximized.
    #[test]
    fn closing_a_floated_anchor_leaves_no_anchor_cycle() {
        for (fullscreen, return_first) in [(true, 0), (true, 1), (false, 0), (false, 1)] {
            let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
            let (mut jwm, [q, p, t]) = three_tiles(&mut backend, 0x5bf0);
            let monitor = jwm.state.monitor_order[0];
            let case = format!("fullscreen: {fullscreen}, back first: {return_first}");
            toggle_maximize_of(&mut jwm, &mut backend, p);
            toggle_maximize_of(&mut jwm, &mut backend, q);
            assert_eq!(jwm.state.clients[p].state.maximize_restore_anchor, Some(t));
            assert_eq!(jwm.state.clients[q].state.maximize_restore_anchor, Some(p));
            if fullscreen {
                jwm.setfullscreen(&mut backend, t, true)
                    .expect("fullscreen");
            } else {
                jwm.state.monitors[monitor].set_selected_client_for_current_tag(Some(t));
                jwm.togglefloating(&mut backend, &WMArgEnum::Int(0))
                    .expect("float");
            }
            assert!(jwm.state.clients[t].state.is_floating, "{case}");
            assert_eq!(jwm.state.monitor_clients[monitor], vec![p, q, t], "{case}");

            jwm.unmanage_regular_client(&mut backend, t, true)
                .expect("t closes");
            // `q` still rests in front of `p`, and `p` names nothing back.
            assert_eq!(
                jwm.state.clients[p].state.maximize_restore_anchor, None,
                "{case}"
            );
            assert_eq!(
                jwm.state.clients[q].state.maximize_restore_anchor,
                Some(p),
                "{case}"
            );

            let order = if return_first == 0 { [q, p] } else { [p, q] };
            for key in order {
                toggle_maximize_of(&mut jwm, &mut backend, key);
            }
            assert_eq!(jwm.state.monitor_clients[monitor], vec![q, p], "{case}");
        }
    }

    /// Regression: manage adopts a pre-mapped maximize before
    /// `attach_new_client`. Under the FLOAT layout admission promotes the
    /// tiled window, and the promotion's regrouping used to push the still
    /// unattached key into the monitor list, so the attach that follows
    /// added it a second time: the tiling later counted the window twice
    /// and its close left a stale key behind.
    #[test]
    fn manage_time_promotion_leaves_attaching_to_the_attach() {
        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test")
            .expect("a spy backend builds a JWM");
        let monitor = jwm.state.monitor_order[0];
        jwm.state.monitors[monitor].lt = Rc::new(LayoutEnum::FLOAT);

        let mut client = WMClient::new(WindowId::from_raw(0x5b10));
        client.mon = Some(monitor);
        client.state.tags = jwm.state.monitors[monitor].get_active_tags();
        client.geometry.x = 120;
        client.geometry.y = 90;
        client.geometry.w = 800;
        client.geometry.h = 600;
        client.geometry.border_w = 2;
        let key = jwm.insert_client(client);

        assert!(
            jwm.adopt_client_maximized(&mut backend, key, MaximizeAxes::BOTH, None)
                .expect("adoption succeeds")
        );
        let client = &jwm.state.clients[key];
        assert!(client.state.maximize_restore_tiled, "admission promoted it");
        assert!(client.state.is_floating);
        assert!(
            !jwm.state.monitor_clients[monitor].contains(&key),
            "adoption must not attach a client manage has not attached yet"
        );

        jwm.attach_new_client(key);
        let occurrences = jwm.state.monitor_clients[monitor]
            .iter()
            .filter(|&&k| k == key)
            .count();
        assert_eq!(occurrences, 1, "the monitor list holds the window once");
    }

    /// Regression: a cancelled drag reinstated the maximize and cleared
    /// `is_drag_floating` on every window. Only a promoted window's flag
    /// is the drag's doing; a window dragged out of the tiles and then
    /// maximized is a plain floating maximize that keeps the flag, and
    /// lost it on Esc, so a later layout re-apply no longer pulled it back
    /// into the tiles.
    #[test]
    fn a_cancelled_drag_keeps_a_drag_floated_maximized_window_reclaimable() {
        use crate::jwm::mouse_handler::{DragCtl, DragMode};
        use crate::jwm::types::WMArgEnum;

        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test")
            .expect("a spy backend builds a JWM");
        let monitor = jwm.state.monitor_order[0];
        jwm.state.monitors[monitor].lt = Rc::new(LayoutEnum::TILE);

        // Dragged out of the tiles: floating because of the drag.
        let window = WindowId::from_raw(0x5b20);
        let mut client = WMClient::new(window);
        client.mon = Some(monitor);
        client.state.tags = jwm.state.monitors[monitor].get_active_tags();
        client.state.is_floating = true;
        client.state.is_drag_floating = true;
        (client.geometry.x, client.geometry.y) = (300, 200);
        (client.geometry.w, client.geometry.h) = (640, 480);
        (client.geometry.floating_x, client.geometry.floating_y) = (300, 200);
        (client.geometry.floating_w, client.geometry.floating_h) = (640, 480);
        client.geometry.border_w = 2;
        let key = jwm.insert_client(client);
        jwm.attach_to_monitor(key, monitor);
        jwm.state.monitors[monitor].set_selected_client_for_current_tag(Some(key));

        jwm.togglemaximize(&mut backend, &WMArgEnum::Int(0))
            .expect("togglemaximize");
        let client = &jwm.state.clients[key];
        assert_eq!(client.state.maximized_axes(), MaximizeAxes::BOTH);
        assert!(
            !client.state.maximize_restore_tiled,
            "a floating window is maximized in place, not promoted"
        );
        assert!(client.state.is_drag_floating);
        let maximized = client.rect();
        let snapshot = jwm.maximize_snapshot(key);

        jwm.drag_ctl = Some(DragCtl {
            client: key,
            win: window,
            mode: DragMode::MoveFloat,
            start_root: (0.0, 0.0),
            activated: false,
            was_floating: true,
            orig_geom: maximized,
            orig_index: jwm.state.monitor_clients[monitor]
                .iter()
                .position(|&k| k == key),
            mon: Some(monitor),
            orig_maximize: snapshot,
        });
        jwm.activate_pointer_drag(&mut backend)
            .expect("the drag activates");
        assert_eq!(
            jwm.state.clients[key].state.maximized_axes(),
            MaximizeAxes::NONE
        );

        jwm.cancel_pointer_drag(&mut backend);
        let client = &jwm.state.clients[key];
        assert_eq!(client.state.maximized_axes(), MaximizeAxes::BOTH);
        assert!(!client.state.maximize_restore_tiled);
        assert!(client.state.is_floating);
        assert!(
            client.state.is_drag_floating,
            "the cancelled drag must leave the window as it found it"
        );
        assert_eq!(client.rect(), maximized);
        assert_eq!(jwm.maximize_snapshot(key), snapshot);
    }
}
